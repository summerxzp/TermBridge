//! OutputEngine —— TermBridge 核心模块（PLAN.md §5.2 / §5.3 / §4.6 契约 12 条）
//!
//! 架构（借鉴 pty-mcp `internal/buffer/ring.go` + `pty/helper.go`）：
//! ```text
//! ┌──────────────────────────────────────────────┐
//! │ OutputEngine                                  │
//! │   PTY → MultiWriter ──┬──→ RingBuffer (bounded)│
//! │                       └──→ 滚动日志文件 (兜底)  │
//! │                                            │
//! │   RingBuffer 游标机制：                       │
//! │     ┌────────────┬─────────────┐            │
//! │     ▼            ▼             ▼            │
//! │  mark_cursor  since_cursor   tail (peek)    │
//! │  (内部消费)   (外部自管)      (不推进)        │
//! │   settle /    多 consumer                   │
//! │   wait_for    增量读                        │
//! │                                            │
//! │   Waiter: wait_for 正则 + watch 唤醒        │
//! └──────────────────────────────────────────────┘
//! ```
//!
//! 行为契约（§4.6，12 条）——本模块所有方法都必须满足：
//! 1. Session 是唯一 terminal state owner
//! 2. PTY output 永久进入 bounded buffer，满则丢最旧
//! 3. read_output 默认 drain（settle 语义），推进 mark_cursor
//! 4. tail_lines 不推进任何 cursor
//! 5. wait_for 先扫已有 unread + 等未来；命中推进 mark，超时不推进；正则失败回退 contains
//! 6. timeout 只约束本次调用
//! 7. send_input 不等待（与本模块无关，由 Session 保证）
//! 8. Ctrl+C 是 control（与本模块无关）
//! 9. Session close 才结束 shell（与本模块无关）
//! 10. Connection disconnect 不销毁 Session（与本模块无关）
//! 11. since_cursor 不推进 mark_cursor，支持多 consumer
//! 12. RingBuffer 用"物理环形 buf[] + 逻辑单调 written 计数器"

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::time::timeout;

use crate::domain::ansi_strip::{strip_control_sequences, strip_page};

// ───────────────────────────────────────────────────────────────────────────
// OutputRingBuffer —— 物理环形 buf[] + 逻辑单调 written 计数器
// ───────────────────────────────────────────────────────────────────────────

/// 默认容量（§5.2：默认 1MB，min 64KB，max 32MB）
pub const DEFAULT_BUFFER_SIZE: usize = 1024 * 1024;
pub const MIN_BUFFER_SIZE: usize = 64 * 1024;
pub const MAX_BUFFER_SIZE: usize = 32 * 1024 * 1024;

/// RingBuffer 内部可变状态
struct BufferInner {
    buf: Vec<u8>,
    size: usize,
    head: usize, // 下一个写入位置（环形）
    written: u64, // 单调递增总写入字节数（永不回绕，作绝对游标）
}

/// 物理环形 buffer + 单调 written 计数器 + watch 唤醒
///
/// 线程安全：内部 `parking_lot::Mutex`，临界区极短（仅 buf 操作）。
/// `watch<u64>`（written 游标）在锁外，唤醒语义见 `subscribe_written`。
#[derive(Clone)]
pub struct OutputRingBuffer {
    inner: Arc<Mutex<BufferInner>>,
    /// written 游标变更通知（修复 P3-1：watch 永不丢唤醒，
    /// 替代原 `Notify::notify_one` 在多 waiter 下互相抢唤醒的问题）
    written_tx: watch::Sender<u64>,
}

/// `ReadSinceMax` 返回结果（§5.3 since_cursor 模式）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadSinceResult {
    pub output: Vec<u8>,
    pub new_cursor: u64,
    pub has_more: bool,
    pub is_truncated: bool,
}

impl OutputRingBuffer {
    /// 创建指定容量的 RingBuffer（容量 clamp 到 [MIN, MAX]）
    pub fn new(size: usize) -> Self {
        let size = size.clamp(MIN_BUFFER_SIZE, MAX_BUFFER_SIZE);
        Self::new_raw(size)
    }

    /// 创建指定容量的 RingBuffer，不 clamp（测试用，但也被 new() 调用）
    pub(crate) fn new_raw(size: usize) -> Self {
        let (written_tx, _) = watch::channel(0u64);
        Self {
            inner: Arc::new(Mutex::new(BufferInner {
                buf: vec![0u8; size],
                size,
                head: 0,
                written: 0,
            })),
            written_tx,
        }
    }

    /// 写入数据（PTY output 入口）。推进 written + 通知等待者。
    /// 满则丢最旧（环形覆盖）。
    pub fn write(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let new_written = {
            let mut g = self.inner.lock();
            let len = data.len();
            // 单次写入超过容量：只保留最后 size 字节
            if len >= g.size {
                let start = len - g.size;
                g.buf.copy_from_slice(&data[start..]);
                g.head = 0;
                g.written += len as u64;
            } else {
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
            g.written
        };
        // 锁外通知。watch 语义：每个 Receiver 独立记录"已见"值，任何一次 write
        // 都会让所有等待中的 `changed()` 返回——不会像 notify_one 那样只唤醒一个
        // 等待者、使其他并发 wait_for 白等到超时（修复 P3-1）。
        // 无 receiver 时 send 返回 Err，属正常情况（无人等待），忽略。
        let _ = self.written_tx.send(new_written);
    }

    /// 当前 written 快照（绝对游标，单调递增）
    pub fn written(&self) -> u64 {
        self.inner.lock().written
    }

    /// cursor 是否已被覆盖（buffer 满后丢最旧）
    pub fn is_truncated(&self, cursor: u64) -> bool {
        let g = self.inner.lock();
        // 数据量未超过容量：任何 cursor <= written 都有效
        // 数据量超过容量：有效区间是 [written - size, written]
        if g.written <= g.size as u64 {
            cursor > g.written
        } else {
            cursor < g.written - g.size as u64 || cursor > g.written
        }
    }

    /// 读取 cursor 之后的所有数据（纯读，无副作用）。
    /// 若 cursor 已被截断，返回当前 buffer 全部。
    pub fn read_since(&self, cursor: u64) -> Vec<u8> {
        let g = self.inner.lock();
        read_since_inner(&g, cursor, None).0
    }

    /// 读取 cursor 之后的数据，最多 max_bytes（纯读，无副作用）。
    /// 返回 (output, new_cursor, has_more, is_truncated)。
    pub fn read_since_max(&self, cursor: u64, max_bytes: usize) -> ReadSinceResult {
        let g = self.inner.lock();
        let (output, new_cursor) = read_since_inner(&g, cursor, Some(max_bytes));
        let available = g.written.saturating_sub(new_cursor);
        ReadSinceResult {
            output,
            new_cursor,
            has_more: available > 0,
            is_truncated: Self::is_truncated_inner(&g, cursor),
        }
    }

    /// 读取最后 n 行（不推进任何 cursor，peek 语义）。
    /// 按 `\n` 切分，去尾空行。n=0 返回空。
    pub fn tail(&self, n_lines: usize) -> Vec<u8> {
        if n_lines == 0 {
            return Vec::new();
        }
        let g = self.inner.lock();
        // 取整个 buffer 的有效内容
        let (full, _) = read_since_inner(&g, 0, None);
        // 按行切分，取最后 n 行
        let lines: Vec<&[u8]> = full.split(|&b| b == b'\n').collect();
        let total = lines.len();
        // 去尾空行（最后一段如果为空，是末尾 \n 导致）
        let effective = if total > 0 && lines[total - 1].is_empty() {
            total - 1
        } else {
            total
        };
        if effective == 0 {
            return Vec::new();
        }
        let start = effective.saturating_sub(n_lines);
        let mut out = Vec::new();
        for (i, line) in lines[start..effective].iter().enumerate() {
            if i > 0 {
                out.push(b'\n');
            }
            out.extend_from_slice(line);
        }
        out
    }

    /// 等待新 output（阻塞直到下一次 write 或超时）。
    ///
    /// watch 语义：Receiver 以订阅时刻的 written 为"已见"，之后的每次 write
    /// 都会让 `changed()` 返回——不会漏掉任何一次写入。
    pub async fn wait_for_notify(&self, dur: Duration) -> bool {
        let mut rx = self.written_tx.subscribe();
        timeout(dur, rx.changed()).await.is_ok()
    }

    /// 新建一个 written 游标 watch Receiver（wait_for 循环用，修复 P3-1）。
    ///
    /// 典型用法（无丢唤醒竞态）：
    /// 1. 先 `subscribe()`（记录当前 written 为已见）
    /// 2. 再扫描已有数据并检查谓词（覆盖订阅前已到达的数据）
    /// 3. 最后 await `changed()`（覆盖 scan 与 await 之间到达的数据）
    pub fn subscribe_written(&self) -> watch::Receiver<u64> {
        self.written_tx.subscribe()
    }

    fn is_truncated_inner(g: &BufferInner, cursor: u64) -> bool {
        if g.written <= g.size as u64 {
            cursor > g.written
        } else {
            cursor < g.written - g.size as u64 || cursor > g.written
        }
    }
}

/// 内部辅助：从 cursor 读取，可选 max_bytes 限制
fn read_since_inner(
    g: &BufferInner,
    cursor: u64,
    max_bytes: Option<usize>,
) -> (Vec<u8>, u64) {
    if cursor >= g.written {
        return (Vec::new(), g.written);
    }
    // 有效起点（clamp 到 buffer 实际拥有的最早数据）
    let earliest = g.written.saturating_sub(g.size as u64);
    let start = cursor.max(earliest);
    let available = (g.written - start) as usize;
    let to_read = max_bytes.map_or(available, |m| available.min(m));
    if to_read == 0 {
        return (Vec::new(), start);
    }
    // 物理位置计算（分两种情况）：
    //   未满（written <= size）：数据在 buf[0..written]，earliest=0 对应 phys=0
    //   已满+溢出（written > size）：数据环形，earliest 对应 phys=head
    let start_phys = if g.written <= g.size as u64 {
        start as usize
    } else {
        (g.head + (start - earliest) as usize) % g.size
    };
    let mut out = Vec::with_capacity(to_read);
    let mut read = 0usize;
    let mut phys = start_phys;
    while read < to_read {
        let n = (g.size - phys).min(to_read - read);
        out.extend_from_slice(&g.buf[phys..phys + n]);
        phys = (phys + n) % g.size;
        read += n;
    }
    (out, start + to_read as u64)
}

// ───────────────────────────────────────────────────────────────────────────
// ReadOutputParams / ReadOutputResult —— §5.3 三模式参数与返回
// ───────────────────────────────────────────────────────────────────────────

/// read_output 入参（§5.3）
#[derive(Debug, Clone, Default)]
pub struct ReadOutputParams {
    /// 阻塞匹配模式：等待此正则出现
    pub wait_for: Option<String>,
    /// 超时秒数，默认 5s，上限 60s
    pub timeout_secs: Option<u64>,
    /// peek 模式：看最后 N 行，不推进 cursor
    pub tail_lines: Option<usize>,
    /// 增量读模式：从此 cursor 读取，不推进 mark_cursor
    pub since_cursor: Option<u64>,
    /// since_cursor 模式单次最大字节
    pub max_bytes: Option<usize>,
    /// wait_for 命中行前后上下文行数，上限 50
    pub context_lines: Option<usize>,
    /// Phase 8：剥离终端控制序列（CSI/OSC/DCS/APC 等），只影响返回数据，
    /// 不改 RingBuffer。默认 false（保持 raw byte 透明性）。
    pub strip_ansi: bool,
}

/// read_output 返回（三模式共用）
#[derive(Debug, Clone)]
pub struct ReadOutputResult {
    pub output: Vec<u8>,
    pub cursor: u64,
    pub has_more: bool,
    pub is_truncated: bool,
    pub matched: bool,
    pub timed_out: bool,
    pub mode: ReadMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    SinceCursor,
    Tail,
    WaitFor,
    Settle,
}

// ───────────────────────────────────────────────────────────────────────────
// OutputEngine —— 整合 RingBuffer + mark_cursor + 三模式 read_output
// ───────────────────────────────────────────────────────────────────────────

/// settle 检测参数（§5.3 默认模式）
const SETTLE_POLL_MS: u64 = 50;
const SETTLE_THRESHOLD_MS: u64 = 300;
const DEFAULT_TIMEOUT_SECS: u64 = 5;
const MAX_TIMEOUT_SECS: u64 = 60;
const MAX_TAIL_LINES: usize = 100;
const MAX_CONTEXT_LINES: usize = 50;

/// since_cursor 模式单次读取的默认最大字节（64KB）
pub const DEFAULT_READ_MAX_BYTES: usize = 64 * 1024;

/// strip_ansi 跨页续接状态（修复 P1-11b）。
///
/// 记录上次 since_cursor 剥离的结束游标与该页尾部不完整的转义序列字节；
/// 下次读取起点与结束游标完全一致（顺序分页）时拼接遗留字节再剥离。
#[derive(Default)]
struct StripCarry {
    /// 是否已有续接点（首次剥离前为 false，任意起点都可开始状态化剥离）
    armed: bool,
    /// 上一页剥离结束的绝对游标
    next_cursor: u64,
    /// 上一页尾部不完整的转义序列字节
    pending: Vec<u8>,
}

/// OutputEngine = RingBuffer + mark_cursor（内部消费游标）
///
/// 一个 Session 持有一个 OutputEngine。
/// PTY read task 调 `write()` 灌数据；
/// Agent 调 `read_output()` 读取。
pub struct OutputEngine {
    buffer: OutputRingBuffer,
    mark_cursor: Mutex<u64>,
    /// strip_ansi 跨页续接状态（见 StripCarry）
    strip_carry: Mutex<StripCarry>,
}

impl OutputEngine {
    pub fn new(buffer_size: usize) -> Self {
        Self {
            buffer: OutputRingBuffer::new(buffer_size),
            mark_cursor: Mutex::new(0),
            strip_carry: Mutex::new(StripCarry::default()),
        }
    }

    /// 从已有 buffer 构造（测试用）
    #[cfg(test)]
    pub(crate) fn from_buffer(buffer: OutputRingBuffer) -> Self {
        Self {
            buffer,
            mark_cursor: Mutex::new(0),
            strip_carry: Mutex::new(StripCarry::default()),
        }
    }

    /// 暴露 buffer 给 PTY read task 写入
    pub fn buffer(&self) -> &OutputRingBuffer {
        &self.buffer
    }

    /// 当前 mark_cursor 快照
    pub fn mark_cursor(&self) -> u64 {
        *self.mark_cursor.lock()
    }

    /// 跳到最新（放弃未读）
    pub fn mark_to_latest(&self) {
        let latest = self.buffer.written();
        *self.mark_cursor.lock() = latest;
    }

    // ── read_output 三模式调度（§5.3）──────────────────────────────────

    /// 读取输出。模式优先级：since_cursor > tail_lines > wait_for > 默认 settle
    ///
    /// `strip_ansi = true` 时剥离返回数据中的终端控制序列（不改 RingBuffer）：
    /// - since_cursor 模式：状态化剥离（修复 P1-11b）——顺序连续分页时跨页
    ///   截断的转义序列在下一页续接剥离；非连续读退化为无状态剥离（best-effort）
    /// - tail / wait_for / settle 模式：无状态剥离（这些模式无"顺序分页"语义）
    pub async fn read_output(&self, params: ReadOutputParams) -> ReadOutputResult {
        let timeout_secs = params
            .timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        let dur = Duration::from_secs(timeout_secs);

        // 模式 1：since_cursor（增量读，不推进 mark_cursor）
        if let Some(cursor) = params.since_cursor {
            let max = params.max_bytes.unwrap_or(DEFAULT_READ_MAX_BYTES);
            let r = self.buffer.read_since_max(cursor, max);
            let output = if params.strip_ansi {
                self.strip_since_page(cursor, r.new_cursor, r.output)
            } else {
                r.output
            };
            return ReadOutputResult {
                output,
                cursor: r.new_cursor,
                has_more: r.has_more,
                is_truncated: r.is_truncated,
                matched: false,
                timed_out: false,
                mode: ReadMode::SinceCursor,
            };
        }

        // 模式 2：tail_lines（peek，不推进任何 cursor）
        if let Some(n) = params.tail_lines {
            let n = n.min(MAX_TAIL_LINES);
            let output = strip_stateless(params.strip_ansi, self.buffer.tail(n));
            return ReadOutputResult {
                output,
                cursor: self.mark_cursor(),
                has_more: false,
                is_truncated: false,
                matched: false,
                timed_out: false,
                mode: ReadMode::Tail,
            };
        }

        // 模式 3：wait_for（阻塞匹配）
        if let Some(pattern) = params.wait_for {
            let mut result = self
                .read_with_wait_for(pattern, params.context_lines, dur)
                .await;
            result.output = strip_stateless(params.strip_ansi, result.output);
            return result;
        }

        // 模式 4：默认 settle
        let mut result = self.read_with_settle(dur).await;
        result.output = strip_stateless(params.strip_ansi, result.output);
        result
    }

    // ── strip_ansi 状态化剥离（修复 P1-11b）──────────────────────────

    /// since_cursor 页的状态化 ANSI 剥离。
    ///
    /// 仅当本次读取起点与上次剥离结束位置完全一致（顺序分页）且起点未被环形
    /// 覆盖时，拼接上次遗留的不完整序列前缀；否则（任意 since / 并发 peek /
    /// 数据已被环形覆盖）退化为无状态剥离（best-effort），且**不更新**续接状态，
    /// 保证原分页流之后仍可正确续接。
    fn strip_since_page(&self, read_start: u64, read_end: u64, page: Vec<u8>) -> Vec<u8> {
        let mut carry = self.strip_carry.lock();
        let contiguous =
            !self.buffer.is_truncated(read_start) && (!carry.armed || carry.next_cursor == read_start);
        if !contiguous {
            return strip_control_sequences(&page);
        }
        let pending = std::mem::take(&mut carry.pending);
        let res = strip_page(&page, &pending);
        *carry = StripCarry {
            armed: true,
            next_cursor: read_end,
            pending: res.trailing,
        };
        res.text
    }

    // ── wait_for 模式（§5.3 + 契约 5）──────────────────────────────────

    async fn read_with_wait_for(
        &self,
        pattern: String,
        context_lines: Option<usize>,
        dur: Duration,
    ) -> ReadOutputResult {
        let context_lines = context_lines.unwrap_or(0).min(MAX_CONTEXT_LINES);
        let mark = self.mark_cursor();

        // 正则编译，失败回退纯文本 contains（契约 5）
        let re = regex::Regex::new(&pattern).ok();
        let plain = pattern.as_bytes();

        // ① 先订阅 written watch（记录当前 written 为"已见"），再扫已有 unread。
        //    顺序保证不丢唤醒（修复 P3-1）：订阅前的写入由 ② 的 scan 覆盖，
        //    订阅后、scan 前的写入由 changed() 捕获（watch 每个接收者独立记录
        //    已见值，并发多个 waiter 不会互相抢走唤醒）。
        let mut written_rx = self.buffer.subscribe_written();

        // ② 扫已有 unread（mark → written）
        let existing = self.buffer.read_since(mark);
        if let Some(m) = match_pattern(&existing, re.as_ref(), plain) {
            // 命中 → 推进 mark 到最新（契约 5：命中推进）
            let latest = self.buffer.written();
            *self.mark_cursor.lock() = latest;
            let context = extract_context(&existing, m, context_lines);
            return ReadOutputResult {
                output: context,
                cursor: latest,
                has_more: false,
                is_truncated: self.buffer.is_truncated(mark),
                matched: true,
                timed_out: false,
                mode: ReadMode::WaitFor,
            };
        }

        // ② 未命中 → 等待新 output，重新扫
        // last_mark 始终保持为初始 mark，每次全量扫（mark → written）
        // 这样不会漏掉跨 chunk 的匹配；命中才推进 mark，超时不推进（契约 5）
        let deadline = tokio::time::sleep(dur);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    // 超时 → 不推进 mark（契约 5：超时不推进，留作后续 read_output 的未读数据）
                    // 返回当前已扫 unread 供调用方观察，但 mark_cursor 保持原位
                    let unread = self.buffer.read_since(mark);
                    let has_more = !unread.is_empty();
                    return ReadOutputResult {
                        output: unread,
                        cursor: mark,
                        has_more,
                        is_truncated: self.buffer.is_truncated(mark),
                        matched: false,
                        timed_out: true,
                        mode: ReadMode::WaitFor,
                    };
                }
                res = written_rx.changed() => {
                    // 新数据到达（watch 永不丢更新），全量重扫（mark → written）。
                    // 发送端随 OutputRingBuffer 存活，Err（发送端已 drop）实际不可达；
                    // 保险起见按超时返回（不推进 mark，同时避免 Err 分支忙转）。
                    if res.is_err() {
                        let unread = self.buffer.read_since(mark);
                        let has_more = !unread.is_empty();
                        return ReadOutputResult {
                            output: unread,
                            cursor: mark,
                            has_more,
                            is_truncated: self.buffer.is_truncated(mark),
                            matched: false,
                            timed_out: true,
                            mode: ReadMode::WaitFor,
                        };
                    }
                    let unread = self.buffer.read_since(mark);
                    if let Some(m) = match_pattern(&unread, re.as_ref(), plain) {
                        // 命中 → 推进 mark 到最新（契约 5：命中推进）
                        let latest = self.buffer.written();
                        *self.mark_cursor.lock() = latest;
                        let context = extract_context(&unread, m, context_lines);
                        return ReadOutputResult {
                            output: context,
                            cursor: latest,
                            has_more: false,
                            is_truncated: self.buffer.is_truncated(mark),
                            matched: true,
                            timed_out: false,
                            mode: ReadMode::WaitFor,
                        };
                    }
                    // 未命中，继续等下一批数据
                }
            }
        }
    }

    // ── settle 模式（§5.3 默认 + 契约 3）───────────────────────────────

    async fn read_with_settle(&self, dur: Duration) -> ReadOutputResult {
        let mark = self.mark_cursor();
        let mut last_output = self.buffer.read_since(mark);
        let mut last_change = tokio::time::Instant::now();

        let poll = Duration::from_millis(SETTLE_POLL_MS);
        let threshold = Duration::from_millis(SETTLE_THRESHOLD_MS);
        let deadline = tokio::time::sleep(dur);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                _ = &mut deadline => {
                    // 超时 → 推进 mark，返回当前 output
                    let out = self.buffer.read_since(mark);
                    let len = out.len() as u64;
                    *self.mark_cursor.lock() = mark + len;
                    return ReadOutputResult {
                        output: out,
                        cursor: mark + len,
                        has_more: false,
                        is_truncated: self.buffer.is_truncated(mark),
                        matched: false,
                        timed_out: true,
                        mode: ReadMode::Settle,
                    };
                }
                _ = tokio::time::sleep(poll) => {
                    // 50ms 轮询
                    let current = self.buffer.read_since(mark);
                    if current.is_empty() {
                        // 空输出永不 settled（契约：避免命令还没产出就提前返回）
                        continue;
                    }
                    let len = current.len() as u64;
                    let is_prompt = has_prompt_suffix(&current);
                    let settled = if current != last_output {
                        // 输出变化 → 更新 last_change，clone 避免后续借用冲突
                        last_output = current.clone();
                        last_change = tokio::time::Instant::now();
                        false
                    } else {
                        // 输出未变 → 检查是否稳定 ≥ 300ms
                        last_change.elapsed() >= threshold
                    };
                    if settled || is_prompt {
                        *self.mark_cursor.lock() = mark + len;
                        return ReadOutputResult {
                            output: current,
                            cursor: mark + len,
                            has_more: false,
                            is_truncated: self.buffer.is_truncated(mark),
                            matched: false,
                            timed_out: false,
                            mode: ReadMode::Settle,
                        };
                    }
                }
            }
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 辅助函数
// ───────────────────────────────────────────────────────────────────────────

/// 无状态 ANSI 剥离（tail / wait_for / settle 模式用）。
///
/// 这些模式没有"顺序分页"语义（wait_for 扫描的是 mark → written 全量、
/// tail 是 peek、settle 依赖 poll），跨页截断的序列按 best-effort 处理，
/// 与旧 server.rs 的行为一致。
fn strip_stateless(strip: bool, data: Vec<u8>) -> Vec<u8> {
    if strip {
        strip_control_sequences(&data)
    } else {
        data
    }
}

/// 匹配结果（字节区间）
struct Match {
    start: usize,
    end: usize,
}

/// 在 data 中匹配正则或纯文本。返回第一个匹配的字节区间。
fn match_pattern(data: &[u8], re: Option<&regex::Regex>, plain: &[u8]) -> Option<Match> {
    /// 把字节位置包成 Match 区间
    fn pos_to_match(pos: usize, plain_len: usize) -> Match {
        Match {
            start: pos,
            end: pos + plain_len,
        }
    }

    if let Some(re) = re {
        // 正则匹配（需要 str）
        if let Ok(s) = std::str::from_utf8(data) {
            if let Some(m) = re.find(s) {
                return Some(Match {
                    start: m.start(),
                    end: m.end(),
                });
            }
        }
        // 非 UTF-8 数据，正则无法匹配，回退 contains
        find_subslice(data, plain).map(|pos| pos_to_match(pos, plain.len()))
    } else {
        // 纯文本 contains
        find_subslice(data, plain).map(|pos| pos_to_match(pos, plain.len()))
    }
}

/// 提取匹配行 + 前后 context_lines 行
fn extract_context(data: &[u8], m: Match, context_lines: usize) -> Vec<u8> {
    if context_lines == 0 {
        // 无上下文：返回匹配所在行
        let line_start = data[..m.start].iter().rposition(|&b| b == b'\n').map(|p| p + 1).unwrap_or(0);
        let line_end = data[m.end..].iter().position(|&b| b == b'\n').map(|p| m.end + p + 1).unwrap_or(data.len());
        return data[line_start..line_end].to_vec();
    }
    // 有上下文：前 context_lines 行 + 匹配所在行（含匹配本身，修复 P1-10：
    // 旧实现只取 m.start 前 / m.end 后的行，匹配文本被整体丢弃，且因为
    // "after" 的第一个元素是匹配行剩余部分，实际还多丢一行前文）+ 后 context_lines 行
    let line_start = data[..m.start].iter().rposition(|&b| b == b'\n').map(|p| p + 1).unwrap_or(0);
    // 匹配行行尾（含换行符）之后的下一行行首；无换行则为数据末尾
    let match_line_end = data[m.end..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| m.end + p + 1)
        .unwrap_or(data.len());

    let mut out = Vec::new();
    fn push_line(out: &mut Vec<u8>, line: &[u8]) {
        if !out.is_empty() {
            out.push(b'\n');
        }
        out.extend_from_slice(line);
    }

    // 前文：line_start 之前的完整行（line_start 总是 0 或紧跟 '\n'，
    // 因此非空时以 '\n' 结尾——split 产生的末尾空段是换行符伪影，丢弃）
    let before = &data[..line_start];
    let before_lines: Vec<&[u8]> = if before.is_empty() {
        Vec::new()
    } else {
        let mut lines: Vec<&[u8]> = before.split(|&b| b == b'\n').collect();
        lines.pop();
        lines
    };
    let before_start = before_lines.len().saturating_sub(context_lines);
    for line in &before_lines[before_start..] {
        push_line(&mut out, line);
    }

    // 匹配所在行（去掉行尾换行符；跨行匹配时覆盖 m.start 与 m.end 所在行区间）
    let match_line = &data[line_start..match_line_end];
    let match_line = match_line.strip_suffix(b"\n").unwrap_or(match_line);
    push_line(&mut out, match_line);

    // 后文：匹配行下一行起的 context_lines 行（同样丢弃末尾换行伪影段）
    let after = &data[match_line_end..];
    let after_lines: Vec<&[u8]> = if after.is_empty() {
        Vec::new()
    } else {
        let mut lines: Vec<&[u8]> = after.split(|&b| b == b'\n').collect();
        if after.ends_with(b"\n") {
            lines.pop();
        }
        lines
    };
    for line in after_lines.iter().take(context_lines) {
        push_line(&mut out, line);
    }
    out
}

/// 简单 prompt 检测：以 `$ ` / `# ` / `> ` 结尾（settle 提前返回）
fn has_prompt_suffix(data: &[u8]) -> bool {
    let n = data.len();
    if n >= 2 {
        let tail = &data[n - 2..];
        tail == b"$ " || tail == b"# " || tail == b"> "
    } else {
        false
    }
}

/// 在 data 中查找 sub 子序列
fn find_subslice(data: &[u8], sub: &[u8]) -> Option<usize> {
    if sub.is_empty() {
        return Some(0);
    }
    data.windows(sub.len()).position(|w| w == sub)
}

// ───────────────────────────────────────────────────────────────────────────
// 单元测试 —— 验证 §4.6 契约 12 条
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_engine(size: usize) -> OutputEngine {
        OutputEngine::new(size)
    }

    /// 测试用：不 clamp 的 engine
    fn mk_engine_raw(size: usize) -> OutputEngine {
        OutputEngine::from_buffer(OutputRingBuffer::new_raw(size))
    }

    // ── 契约 2：PTY output 永久进入 bounded buffer，满则丢最旧 ────────

    #[test]
    fn test_buffer_overflow_drops_oldest() {
        let buf = OutputRingBuffer::new(64 * 1024);
        // 写满 + 溢出
        let big = vec![b'A'; 100_000];
        buf.write(&big);
        // written 应该是 100_000，但实际 buffer 只有 64KB
        assert_eq!(buf.written(), 100_000);
        // 最早的数据被丢弃
        let earliest = buf.read_since(0);
        // 最早可读位置应该是 100_000 - 64*1024
        let expected_earliest = 100_000 - 64 * 1024;
        assert_eq!(earliest.len(), 64 * 1024);
        assert!(earliest.iter().all(|&b| b == b'A'));
        // cursor=0 已被截断
        assert!(buf.is_truncated(0));
        assert!(!buf.is_truncated(expected_earliest));
    }

    #[test]
    fn test_written_monotonic() {
        // 契约 12：written 单调递增，永不回绕
        let buf = OutputRingBuffer::new(1024);
        buf.write(b"hello");
        assert_eq!(buf.written(), 5);
        buf.write(b" world");
        assert_eq!(buf.written(), 11);
        // 写超过容量
        let big = vec![b'X'; 2000];
        buf.write(&big);
        assert_eq!(buf.written(), 11 + 2000);
        // written 持续单调
        buf.write(b"end");
        assert_eq!(buf.written(), 11 + 2000 + 3);
    }

    // ── 契约 3：read_output 默认 drain（settle 语义），推进 mark_cursor ──

    #[tokio::test]
    async fn test_settle_advances_mark_cursor() {
        let engine = mk_engine(64 * 1024);
        // 写入数据
        engine.buffer().write(b"$ ls\nfile1 file2\n$ ");
        // settle 读取
        let r = engine.read_output(ReadOutputParams::default()).await;
        assert_eq!(r.mode, ReadMode::Settle);
        assert!(!r.timed_out || !r.output.is_empty()); // prompt 检测或 settle
        // mark_cursor 应推进
        assert!(engine.mark_cursor() > 0);
    }

    // ── 契约 4：tail_lines 不推进任何 cursor ──────────────────────────

    #[tokio::test]
    async fn test_tail_lines_does_not_advance_cursor() {
        let engine = mk_engine(64 * 1024);
        engine.buffer().write(b"line1\nline2\nline3\nline4\n$ ");
        let mark_before = engine.mark_cursor();
        let r = engine
            .read_output(ReadOutputParams {
                tail_lines: Some(2),
                ..Default::default()
            })
            .await;
        assert_eq!(r.mode, ReadMode::Tail);
        assert_eq!(r.cursor, mark_before); // cursor 不变
        assert_eq!(engine.mark_cursor(), mark_before); // mark_cursor 不变
        // 返回最后 2 行
        assert!(r.output.windows(5).any(|w| w == b"line4"));
    }

    // ── 契约 5：wait_for 先扫已有 + 等未来；命中推进，超时不推进 ──────

    #[tokio::test]
    async fn test_wait_for_matches_existing() {
        let engine = mk_engine(64 * 1024);
        engine.buffer().write(b"loading...\nServer started on :8080\n$ ");
        let r = engine
            .read_output(ReadOutputParams {
                wait_for: Some("Server started".to_string()),
                timeout_secs: Some(2),
                ..Default::default()
            })
            .await;
        assert_eq!(r.mode, ReadMode::WaitFor);
        assert!(r.matched);
        assert!(!r.timed_out);
        assert!(engine.mark_cursor() > 0); // 命中推进
    }

    #[tokio::test]
    async fn test_wait_for_timeout_does_not_advance() {
        // 契约 5：超时不推进 mark（留作后续 read_output 的未读数据）
        let engine = mk_engine(64 * 1024);
        engine.buffer().write(b"loading...\n");
        let mark_before = engine.mark_cursor();
        let r = engine
            .read_output(ReadOutputParams {
                wait_for: Some("never_appears".to_string()),
                timeout_secs: Some(1),
                ..Default::default()
            })
            .await;
        assert_eq!(r.mode, ReadMode::WaitFor);
        assert!(!r.matched);
        assert!(r.timed_out);
        // 关键：mark_cursor 不推进，数据留作未读
        assert_eq!(engine.mark_cursor(), mark_before);
        assert_eq!(r.cursor, mark_before);
        // 超时返回的 output 是已扫 unread（供调用方观察）
        assert!(r.output.windows(10).any(|w| w == b"loading..."));
        // 后续 read_output 仍能读到这批数据
        let r2 = engine
            .read_output(ReadOutputParams {
                timeout_secs: Some(1),
                ..Default::default()
            })
            .await;
        assert!(r2.output.windows(10).any(|w| w == b"loading..."));
    }

    #[tokio::test]
    async fn test_wait_for_waits_for_future_output() {
        let engine = mk_engine(64 * 1024);
        engine.buffer().write(b"loading...\n");
        let engine_clone = engine.buffer().clone();
        let engine_ref = std::sync::Arc::new(engine);
        let engine_for_task = engine_ref.clone();

        // 200ms 后写入匹配数据
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            engine_for_task.buffer().write(b"Server ready\n$ ");
        });

        let r = engine_ref
            .read_output(ReadOutputParams {
                wait_for: Some("Server ready".to_string()),
                timeout_secs: Some(3),
                ..Default::default()
            })
            .await;
        assert!(r.matched);
        assert!(!r.timed_out);
        let _ = engine_clone;
    }

    #[tokio::test]
    async fn test_wait_for_regex_fallback_to_contains() {
        // 契约 5：正则编译失败回退纯文本 contains
        let engine = mk_engine(64 * 1024);
        engine.buffer().write(b"password: hello\n");
        let r = engine
            .read_output(ReadOutputParams {
                wait_for: Some("password:".to_string()), // 合法正则也是文本
                timeout_secs: Some(1),
                ..Default::default()
            })
            .await;
        assert!(r.matched);

        // 非法正则回退
        let engine2 = mk_engine(64 * 1024);
        engine2.buffer().write(b"[error] something\n");
        let r2 = engine2
            .read_output(ReadOutputParams {
                wait_for: Some("[error".to_string()), // 非法正则
                timeout_secs: Some(1),
                ..Default::default()
            })
            .await;
        assert!(r2.matched); // 回退 contains 匹配
    }

    // ── 契约 6：timeout 只约束本次调用 ────────────────────────────────

    #[tokio::test]
    async fn test_timeout_does_not_close_session() {
        let engine = mk_engine(64 * 1024);
        // 第一次 read timeout
        let _r1 = engine
            .read_output(ReadOutputParams {
                timeout_secs: Some(1),
                ..Default::default()
            })
            .await;
        // engine 仍然可用
        engine.buffer().write(b"data after timeout\n$ ");
        let r2 = engine
            .read_output(ReadOutputParams {
                timeout_secs: Some(2),
                ..Default::default()
            })
            .await;
        assert!(!r2.output.is_empty() || r2.timed_out);
    }

    // ── 契约 11：since_cursor 不推进 mark_cursor，支持多 consumer ──────

    #[tokio::test]
    async fn test_since_cursor_multi_consumer() {
        let engine = mk_engine(64 * 1024);
        engine.buffer().write(b"chunk1\n");
        let mark_before = engine.mark_cursor();

        // consumer A 用 since_cursor=0 读取
        let r_a = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(0),
                max_bytes: Some(1024),
                ..Default::default()
            })
            .await;
        assert_eq!(r_a.mode, ReadMode::SinceCursor);
        assert!(r_a.output.windows(7).any(|w| w == b"chunk1\n"));
        // mark_cursor 不变
        assert_eq!(engine.mark_cursor(), mark_before);

        // 写入新数据
        engine.buffer().write(b"chunk2\n");

        // consumer B 用不同的 cursor 读取
        let r_b = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(r_a.cursor), // 从 A 停下的位置继续
                max_bytes: Some(1024),
                ..Default::default()
            })
            .await;
        assert!(r_b.output.windows(7).any(|w| w == b"chunk2\n"));
        assert_eq!(engine.mark_cursor(), mark_before); // mark 仍不变
    }

    #[tokio::test]
    async fn test_since_cursor_truncation() {
        let engine = mk_engine_raw(1024); // 小 buffer 测截断
        let big = vec![b'X'; 2000];
        engine.buffer().write(&big);
        let r = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(0), // cursor=0 已被覆盖
                max_bytes: Some(1024),
                ..Default::default()
            })
            .await;
        assert!(r.is_truncated); // 应报告截断
    }

    // ── 契约 12：RingBuffer 物理环形 + 逻辑单调 written ────────────────

    #[test]
    fn test_ring_buffer_wrap_around() {
        let buf = OutputRingBuffer::new_raw(16);
        // 写入 10 字节
        buf.write(b"0123456789");
        assert_eq!(buf.written(), 10);
        // 读 cursor=0
        let r = buf.read_since(0);
        assert_eq!(r, b"0123456789");
        // 再写 10 字节（会回绕）
        buf.write(b"abcdefghij");
        assert_eq!(buf.written(), 20);
        // 读 cursor=0（应该有截断，buffer 只有 16）
        let r = buf.read_since(0);
        // 最早可读是 written - size = 20 - 16 = 4
        // 但 cursor=0 < 4，所以从 4 开始读
        assert_eq!(r.len(), 16);
        // 读取 cursor=10（之前的末尾）
        let r = buf.read_since(10);
        assert_eq!(r, b"abcdefghij");
    }

    #[test]
    fn test_tail_basic() {
        let buf = OutputRingBuffer::new_raw(1024);
        buf.write(b"line1\nline2\nline3\nline4\nline5\n");
        let t = buf.tail(2);
        let s = String::from_utf8_lossy(&t);
        assert!(s.contains("line4"));
        assert!(s.contains("line5"));
        assert!(!s.contains("line1"));
    }

    #[test]
    fn test_tail_zero() {
        let buf = OutputRingBuffer::new_raw(1024);
        buf.write(b"data\n");
        assert_eq!(buf.tail(0), Vec::<u8>::new());
    }

    #[test]
    fn test_empty_buffer() {
        let buf = OutputRingBuffer::new_raw(1024);
        assert_eq!(buf.written(), 0);
        assert_eq!(buf.read_since(0), Vec::<u8>::new());
        assert!(!buf.is_truncated(0));
        assert_eq!(buf.tail(10), Vec::<u8>::new());
    }

    #[test]
    fn test_write_empty_noop() {
        let buf = OutputRingBuffer::new_raw(1024);
        buf.write(b"");
        assert_eq!(buf.written(), 0);
    }

    // ── settle 模式细节 ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_settle_empty_never_settled() {
        // 空输出永不 settled
        let engine = mk_engine(64 * 1024);
        let start = tokio::time::Instant::now();
        let r = engine
            .read_output(ReadOutputParams {
                timeout_secs: Some(1),
                ..Default::default()
            })
            .await;
        // 应该 timeout（因为没有数据）
        assert!(r.timed_out);
        assert!(start.elapsed() >= Duration::from_millis(900));
    }

    #[tokio::test]
    async fn test_settle_prompt_detection() {
        let engine = mk_engine(64 * 1024);
        engine.buffer().write(b"output\n$ ");
        let r = engine
            .read_output(ReadOutputParams {
                timeout_secs: Some(2),
                ..Default::default()
            })
            .await;
        // prompt 检测应立即返回（不用等 300ms）
        assert!(!r.timed_out);
        assert_eq!(r.mode, ReadMode::Settle);
    }

    // ── extract_context（修复 P1-10：匹配文本不得被丢弃）──────────────

    /// 在 data 中定位 pattern 的字节区间
    fn find_match(data: &[u8], pattern: &str) -> Match {
        let pos = find_subslice(data, pattern.as_bytes()).expect("pattern must be present");
        Match {
            start: pos,
            end: pos + pattern.len(),
        }
    }

    #[test]
    fn extract_context_includes_match_mid_line() {
        // 匹配在行中间：前 1 行 + 匹配行 + 后 1 行
        let data = b"l1\nl2\nxxMATCHyy\nl4\nl5\n";
        let m = find_match(data, "MATCH");
        assert_eq!(extract_context(data, m, 1), b"l2\nxxMATCHyy\nl4");
    }

    #[test]
    fn extract_context_repro_from_review() {
        // review 原始复现：旧实现返回 "l2\n\n"（匹配与 l4 都被丢）
        let data = b"l1\nl2\nMATCH\nl4\nl5\n";
        assert_eq!(extract_context(data, find_match(data, "MATCH"), 1), b"l2\nMATCH\nl4");
        assert_eq!(
            extract_context(data, find_match(data, "MATCH"), 2),
            b"l1\nl2\nMATCH\nl4\nl5"
        );
    }

    #[test]
    fn extract_context_match_at_line_start_and_buffer_edges() {
        // 匹配在行首
        let data = b"l1\nMATCH\nl4\nl5\n";
        let m = find_match(data, "MATCH");
        assert_eq!(extract_context(data, m, 1), b"l1\nMATCH\nl4");

        // 匹配在 buffer 开头（无前文）
        let data = b"MATCH\nl4\nl5\n";
        let m = find_match(data, "MATCH");
        assert_eq!(extract_context(data, m, 1), b"MATCH\nl4");

        // 匹配在 buffer 末尾（无后文、无行尾换行）
        let data = b"l1\nl2\nMATCH";
        let m = find_match(data, "MATCH");
        assert_eq!(extract_context(data, m, 1), b"l2\nMATCH");
    }

    #[test]
    fn extract_context_no_trailing_newline_in_data() {
        // 数据末尾无换行：后文取到数据末尾为止
        let data = b"l1\nl2\nMATCH\nl4\nl5";
        let m = find_match(data, "MATCH");
        assert_eq!(extract_context(data, m, 1), b"l2\nMATCH\nl4");
    }

    #[test]
    fn extract_context_zero_returns_match_line_only() {
        // context_lines=0 行为不变：返回匹配所在行（含行尾换行）
        let data = b"l1\nMATCH\nl4\n";
        let m = find_match(data, "MATCH");
        assert_eq!(extract_context(data, m, 0), b"MATCH\n");
    }

    // ── strip_ansi 状态化剥离（修复 P1-11b）──────────────────────────

    #[tokio::test]
    async fn test_strip_ansi_csi_split_across_contiguous_pages() {
        // max_bytes=8 分页时 CSI \x1b[?2004l 恰好被拦腰截断：
        // 页 1 尾丢弃半个 CSI（不泄漏 "200"），页 2 续接后正常剥离
        let engine = mk_engine_raw(1024);
        engine.buffer().write(b"ab\x1b[?2004lhello");
        let r1 = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(0),
                max_bytes: Some(8),
                strip_ansi: true,
                ..Default::default()
            })
            .await;
        assert_eq!(r1.output, b"ab");
        assert_eq!(r1.cursor, 8);
        let r2 = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(r1.cursor),
                max_bytes: Some(8),
                strip_ansi: true,
                ..Default::default()
            })
            .await;
        assert_eq!(r2.output, b"hello");
        assert_eq!(r2.cursor, 15);
    }

    #[tokio::test]
    async fn test_strip_ansi_osc_split_across_contiguous_pages() {
        // 未终止的 OSC 吞掉页尾：页 1 只剩 'x'（而非把 "0;tit" 泄漏成正文），
        // 页 2 拼接遗留后 BEL 终止，只剩 'y'
        let engine = mk_engine_raw(1024);
        engine.buffer().write(b"x\x1b]0;title\x07y");
        let r1 = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(0),
                max_bytes: Some(8),
                strip_ansi: true,
                ..Default::default()
            })
            .await;
        assert_eq!(r1.output, b"x");
        let r2 = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(r1.cursor),
                max_bytes: Some(8),
                strip_ansi: true,
                ..Default::default()
            })
            .await;
        assert_eq!(r2.output, b"y");
    }

    #[tokio::test]
    async fn test_strip_ansi_non_contiguous_read_falls_back_stateless() {
        // 非连续读（任意 since）：退化为无状态剥离（best-effort，跨页序列的
        // 尾部按正文返回——与旧 server.rs 行为一致），且不破坏续接状态：
        // 之后的连续分页读取仍能正确拼接遗留字节
        let engine = mk_engine_raw(1024);
        engine.buffer().write(b"ab\x1b[?2004lhello");
        let r1 = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(0),
                max_bytes: Some(8),
                strip_ansi: true,
                ..Default::default()
            })
            .await;
        assert_eq!(r1.output, b"ab");

        // cursor=3 的非连续读：无状态剥离页内容 "[?2004lh"（bytes 3..11）
        let r2 = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(3),
                max_bytes: Some(8),
                strip_ansi: true,
                ..Default::default()
            })
            .await;
        assert_eq!(r2.output, b"[?2004lh");

        // 连续分页从上次剥离点恢复：遗留 "\x1b[?200" 正确拼接
        let r3 = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(8),
                max_bytes: Some(8),
                strip_ansi: true,
                ..Default::default()
            })
            .await;
        assert_eq!(r3.output, b"hello");
    }

    #[tokio::test]
    async fn test_strip_ansi_empty_page_keeps_pending() {
        // 空页（cursor 已追平 written，无新数据）：遗留字节原样保留，
        // 新数据到达后从同一续接点仍可拼接
        let engine = mk_engine_raw(1024);
        // 页 1 恰好结束在半个 CSI 处（written=8）
        engine.buffer().write(b"ab\x1b[?200");
        let r1 = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(0),
                max_bytes: Some(8),
                strip_ansi: true,
                ..Default::default()
            })
            .await;
        assert_eq!(r1.output, b"ab");
        assert_eq!(r1.cursor, 8);

        // 空页：无新数据
        let r2 = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(r1.cursor),
                strip_ansi: true,
                ..Default::default()
            })
            .await;
        assert_eq!(r2.output, b"");
        assert_eq!(r2.cursor, 8);

        // 新数据到达后续接成功
        engine.buffer().write(b"4lhello");
        let r3 = engine
            .read_output(ReadOutputParams {
                since_cursor: Some(r2.cursor),
                strip_ansi: true,
                ..Default::default()
            })
            .await;
        assert_eq!(r3.output, b"hello");
        assert_eq!(r3.cursor, 15);
    }

    #[test]
    fn default_read_max_bytes_is_64kib() {
        // since_cursor 分页默认单次 64KB（review P1-11b 的假设核实）
        assert_eq!(DEFAULT_READ_MAX_BYTES, 64 * 1024);
    }

    // ── wait_for 并发唤醒（修复 P3-1）────────────────────────────────

    #[tokio::test]
    async fn test_wait_for_two_concurrent_waiters_both_wake() {
        // notify_one 语义下两个并发 wait_for 会互相抢唤醒（只有一个被唤醒，
        // 另一个白等到超时）；watch 语义下两者都应在同一次 write 后命中
        let engine = Arc::new(mk_engine(64 * 1024));
        engine.buffer().write(b"booting...\n");

        let e1 = engine.clone();
        let e2 = engine.clone();
        let t1 = tokio::spawn(async move {
            e1.read_output(ReadOutputParams {
                wait_for: Some("READY".to_string()),
                timeout_secs: Some(5),
                ..Default::default()
            })
            .await
        });
        let t2 = tokio::spawn(async move {
            e2.read_output(ReadOutputParams {
                wait_for: Some("READY".to_string()),
                timeout_secs: Some(5),
                ..Default::default()
            })
            .await
        });
        // 让两个 waiter 都进入等待状态
        tokio::time::sleep(Duration::from_millis(100)).await;
        engine.buffer().write(b"READY\n$ ");

        let r1 = t1.await.unwrap();
        let r2 = t2.await.unwrap();
        assert!(r1.matched, "waiter 1 应命中");
        assert!(r2.matched, "waiter 2 应命中");
    }
}
