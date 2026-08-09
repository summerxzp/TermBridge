//! 简化版 RingBuffer（ADR-0004 §5）
//!
//! daemon 侧只需 write + read_since，不需要 Phase 0-B 的 Waiter / settle / tail。
//! PTY read task 调 `write()` 灌数据；client attach / read_output 调 `read_since()` 拉增量。
//!
//! 物理环形 buf[] + 逻辑单调 written 计数器（与 Phase 0-B OutputRingBuffer 同语义，
//! 但去掉 Notify / mark_cursor / tail / read_since_max，仅保留 daemon 必需的最小接口）。

use parking_lot::Mutex;

// ───────────────────────────────────────────────────────────────────────────
// 常量
// ───────────────────────────────────────────────────────────────────────────

/// 默认容量 10MB（ADR-0004 §5：每个 session 一个 RingBuffer，默认 10MB）
pub const DEFAULT_BUFFER_SIZE: usize = 10 * 1024 * 1024;

/// 最小容量 64KB
pub const MIN_BUFFER_SIZE: usize = 64 * 1024;

// ───────────────────────────────────────────────────────────────────────────
// RingBuffer
// ───────────────────────────────────────────────────────────────────────────

/// RingBuffer 内部可变状态
struct BufferInner {
    buf: Vec<u8>,
    size: usize,
    head: usize,   // 下一个写入位置（环形）
    written: u64,  // 单调递增总写入字节数（永不回绕，作绝对游标）
}

/// 物理环形 buffer + 单调 written 计数器
///
/// 线程安全：内部 `parking_lot::Mutex`，临界区极短（仅 buf 操作）。
pub struct RingBuffer {
    inner: Mutex<BufferInner>,
}

/// `read_since` 返回结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadSinceResult {
    /// 本批数据起始绝对字节偏移
    pub cursor_start: u64,
    /// 本批数据结束绝对字节偏移（= 下批 cursor_start）
    pub cursor_end: u64,
    /// cursor_start 之前的数据是否已被 RingBuffer 丢弃
    pub is_truncated: bool,
    /// 原始字节（rpc 层负责 base64 编码）
    pub data: Vec<u8>,
}

impl RingBuffer {
    /// 创建指定容量的 RingBuffer（容量 clamp 到 [MIN, ∞)，0 视为 DEFAULT）
    pub fn new(size: usize) -> Self {
        let size = if size == 0 {
            DEFAULT_BUFFER_SIZE
        } else {
            size.max(MIN_BUFFER_SIZE)
        };
        Self {
            inner: Mutex::new(BufferInner {
                buf: vec![0u8; size],
                size,
                head: 0,
                written: 0,
            }),
        }
    }

    /// 用默认容量创建
    pub fn with_default_size() -> Self {
        Self::new(DEFAULT_BUFFER_SIZE)
    }

    /// 写入数据（PTY output 入口）。推进 written + 环形覆盖（满则丢最旧）。
    pub fn write(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut g = self.inner.lock();
        let len = data.len();
        // 单次写入超过容量：只保留最后 size 字节
        if len >= g.size {
            let start = len - g.size;
            g.buf.copy_from_slice(&data[start..]);
            g.head = 0;
            g.written += len as u64;
            return;
        }
        // 分段拷贝（可能回绕）
        let mut remaining = len;
        let mut src = 0;
        while remaining > 0 {
            let head = g.head;
            let space = g.size - head;
            let n = remaining.min(space);
            g.buf[head..head + n].copy_from_slice(&data[src..src + n]);
            g.head = (head + n) % g.size;
            src += n;
            remaining -= n;
        }
        g.written += len as u64;
    }

    /// 读取 since 之后的所有数据。
    ///
    /// - 若 since >= written：返回空 data，cursor_end = written
    /// - 若 since < cursor_start（已被覆盖）：is_truncated=true，cursor_start = 当前最旧游标
    /// - 否则：正常返回 [since, written) 区间数据
    pub fn read_since(&self, since: u64) -> ReadSinceResult {
        let g = self.inner.lock();
        let cursor_end = g.written;
        // 无新数据
        if since >= g.written {
            return ReadSinceResult {
                cursor_start: since,
                cursor_end,
                is_truncated: false,
                data: Vec::new(),
            };
        }
        // 当前 buffer 拥有的最早数据游标
        let earliest = g.written.saturating_sub(g.size as u64);
        let is_truncated = since < earliest;
        let start = since.max(earliest);
        let available = (g.written - start) as usize;
        let data = read_range(&g, start, available);
        ReadSinceResult {
            cursor_start: start,
            cursor_end,
            is_truncated,
            data,
        }
    }

    /// 当前 written 快照（绝对游标，单调递增）
    pub fn written(&self) -> u64 {
        self.inner.lock().written
    }

    /// 当前最旧数据游标 = written.saturating_sub(size)
    pub fn cursor_start(&self) -> u64 {
        let g = self.inner.lock();
        g.written.saturating_sub(g.size as u64)
    }

    /// buffer 容量
    pub fn size(&self) -> usize {
        self.inner.lock().size
    }
}

/// 内部辅助：从逻辑游标 start 读取 n 字节（已持有锁）
fn read_range(g: &BufferInner, start: u64, n: usize) -> Vec<u8> {
    if n == 0 {
        return Vec::new();
    }
    let earliest = g.written.saturating_sub(g.size as u64);
    // 物理位置计算：
    //   未满（written <= size）：数据在 buf[0..written]，earliest=0 对应 phys=0
    //   已满+溢出（written > size）：数据环形，earliest 对应 phys=head
    let start_phys = if g.written <= g.size as u64 {
        start as usize
    } else {
        (g.head + (start - earliest) as usize) % g.size
    };
    let mut out = Vec::with_capacity(n);
    let mut read = 0usize;
    let mut phys = start_phys;
    while read < n {
        let chunk = (g.size - phys).min(n - read);
        out.extend_from_slice(&g.buf[phys..phys + chunk]);
        phys = (phys + chunk) % g.size;
        read += chunk;
    }
    out
}

// ───────────────────────────────────────────────────────────────────────────
// 单元测试
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 基本 write / read_since
    #[test]
    fn basic_write_read() {
        let buf = RingBuffer::new(0); // 用默认 10MB
        buf.write(b"hello");
        assert_eq!(buf.written(), 5);
        let r = buf.read_since(0);
        assert_eq!(r.cursor_start, 0);
        assert_eq!(r.cursor_end, 5);
        assert!(!r.is_truncated);
        assert_eq!(r.data, b"hello");
    }

    /// 增量读：since > 0
    #[test]
    fn incremental_read() {
        let buf = RingBuffer::new(0);
        buf.write(b"hello ");
        buf.write(b"world");
        // 读 [3, 12)
        let r = buf.read_since(3);
        assert_eq!(r.cursor_start, 3);
        assert_eq!(r.cursor_end, 11);
        assert_eq!(r.data, b"lo world");
    }

    /// since >= written 返回空
    #[test]
    fn since_at_or_beyond_written() {
        let buf = RingBuffer::new(0);
        buf.write(b"abc");
        let r = buf.read_since(3);
        assert_eq!(r.data, Vec::new());
        assert_eq!(r.cursor_end, 3);
        let r = buf.read_since(100);
        assert_eq!(r.data, Vec::new());
        assert_eq!(r.cursor_end, 3);
    }

    /// 环形覆盖：写入超过容量，最旧数据被丢弃
    #[test]
    fn ring_overwrite() {
        // 用最小容量 64KB 测试
        let size = MIN_BUFFER_SIZE;
        let buf = RingBuffer::new(size);
        // 写满
        let first = vec![0xAAu8; size];
        buf.write(&first);
        assert_eq!(buf.written(), size as u64);
        assert_eq!(buf.cursor_start(), 0);
        // 再写一半，前一半被覆盖
        let second = vec![0xBBu8; size / 2];
        buf.write(&second);
        assert_eq!(buf.written(), (size + size / 2) as u64);
        // 最旧游标推进到 size/2
        assert_eq!(buf.cursor_start(), (size / 2) as u64);
        // 读全部当前内容，应是 [0xBB; size/2] + [0xAA; size/2]
        let r = buf.read_since(buf.cursor_start());
        assert!(!r.is_truncated);
        assert_eq!(r.data.len(), size);
        // 后半是 0xAA（旧的剩余）
        assert!(r.data[size / 2..].iter().all(|&b| b == 0xAA));
        // 前半是 0xBB（新写入）
        assert!(r.data[..size / 2].iter().all(|&b| b == 0xBB));
    }

    /// truncation 检测：since 早于已被覆盖的数据
    #[test]
    fn truncation_detection() {
        let size = MIN_BUFFER_SIZE;
        let buf = RingBuffer::new(size);
        // 写满 + 多 100 字节
        let data = vec![0x11u8; size + 100];
        buf.write(&data);
        // 最早游标 = 100
        assert_eq!(buf.cursor_start(), 100);
        // since=0 已被截断
        let r = buf.read_since(0);
        assert!(r.is_truncated);
        assert_eq!(r.cursor_start, 100); // clamp 到 earliest
        assert_eq!(r.cursor_end, (size + 100) as u64);
        assert_eq!(r.data.len(), size);
    }

    /// since=0 全量读（未溢出时）
    #[test]
    fn read_all_from_zero() {
        let buf = RingBuffer::new(0);
        for i in 0..5 {
            buf.write(&[i]);
        }
        let r = buf.read_since(0);
        assert_eq!(r.data, vec![0, 1, 2, 3, 4]);
        assert!(!r.is_truncated);
    }

    /// 空写入无副作用
    #[test]
    fn empty_write_noop() {
        let buf = RingBuffer::new(0);
        buf.write(b"");
        assert_eq!(buf.written(), 0);
    }

    /// 单次写入超过容量：只保留尾部
    #[test]
    fn write_larger_than_capacity() {
        let buf = RingBuffer::new(MIN_BUFFER_SIZE);
        let big = vec![0x42u8; MIN_BUFFER_SIZE + 500];
        buf.write(&big);
        assert_eq!(buf.written(), (MIN_BUFFER_SIZE + 500) as u64);
        // 最早游标 = 500
        assert_eq!(buf.cursor_start(), 500);
        let r = buf.read_since(500);
        assert!(!r.is_truncated);
        assert_eq!(r.data.len(), MIN_BUFFER_SIZE);
        assert!(r.data.iter().all(|&b| b == 0x42));
    }

    /// 多次小写入累积
    #[test]
    fn multiple_small_writes() {
        let buf = RingBuffer::new(0);
        for _ in 0..100 {
            buf.write(b"abcd");
        }
        assert_eq!(buf.written(), 400);
        let r = buf.read_since(0);
        assert_eq!(r.data.len(), 400);
        assert!(r.data.windows(4).all(|w| w == b"abcd"));
    }
}
