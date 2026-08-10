//! Timeline —— Session 执行事件时间线（Phase 4-A）
//!
//! 与 OutputEngine 并行的附加结构：记录命令/输出/控制/状态事件的元数据时间线，
//! 供 AI 结构化理解"发了什么→返回什么"并用于排障。
//!
//! 设计要点：
//! - 只记元数据，不双份存储输出内容（输出内容在 RingBuffer，Timeline 只记 cursor 范围 + bytes）
//! - 环形淘汰，默认保留 1000 条
//! - 线程安全：`Arc<Mutex<Vec<_>>>` + `AtomicU64`，read task 和 send_input 可并发访问
//! - Timeline 是 Clone（内部 Arc），Session 持有原件，read task 持有 clone

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;

/// Timeline 事件。只记元数据，不存输出内容（内容在 RingBuffer）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimelineEvent {
    /// 用户发送的命令（send_input）
    Command {
        timestamp: u64,
        command_id: u64,
        input: String,
        cursor_before: u64,
    },
    /// PTY 输出（read task 每次 write 后记录）
    Output {
        timestamp: u64,
        cursor_start: u64,
        cursor_end: u64,
        bytes: usize,
    },
    /// 控制键（send_control）
    Control {
        timestamp: u64,
        control: String,
    },
    /// 状态转换
    StateChange {
        timestamp: u64,
        from: String,
        to: String,
    },
}

/// Session 的事件时间线。环形淘汰，默认保留 1000 条。
///
/// Clone（内部 Arc）：Session 持有原件，PTY read task 持有 clone 以记录 output 事件。
#[derive(Clone)]
pub struct Timeline {
    events: Arc<Mutex<Vec<TimelineEvent>>>,
    next_command_id: Arc<AtomicU64>,
    max_events: usize,
}

impl Timeline {
    pub fn new() -> Self {
        Self::with_capacity(1000)
    }

    pub fn with_capacity(max_events: usize) -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::with_capacity(max_events.min(1024)))),
            next_command_id: Arc::new(AtomicU64::new(1)),
            max_events,
        }
    }

    /// 记录命令事件。input 限长 4KB（超出截断 + "..." 后缀）。返回分配的 command_id。
    pub fn record_command(&self, input: &[u8], cursor_before: u64) -> u64 {
        let command_id = self.next_command_id.fetch_add(1, Ordering::Relaxed);
        let input_str = truncate_utf8(input, 4096);
        self.push(TimelineEvent::Command {
            timestamp: now_millis(),
            command_id,
            input: input_str,
            cursor_before,
        });
        command_id
    }

    /// 记录输出事件（PTY read task 每次 buffer.write 后调用）。bytes=0 时跳过。
    pub fn record_output(&self, cursor_start: u64, cursor_end: u64, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.push(TimelineEvent::Output {
            timestamp: now_millis(),
            cursor_start,
            cursor_end,
            bytes,
        });
    }

    /// 记录控制键事件
    pub fn record_control(&self, control: &str) {
        self.push(TimelineEvent::Control {
            timestamp: now_millis(),
            control: control.to_string(),
        });
    }

    /// 记录状态转换事件
    pub fn record_state_change(&self, from: &str, to: &str) {
        self.push(TimelineEvent::StateChange {
            timestamp: now_millis(),
            from: from.to_string(),
            to: to.to_string(),
        });
    }

    /// 返回最近 limit 条事件（None 返回全部）。最旧的在前，最新的在后。
    pub fn events(&self, limit: Option<usize>) -> Vec<TimelineEvent> {
        let events = self.events.lock();
        match limit {
            Some(n) if n < events.len() => events[events.len() - n..].to_vec(),
            _ => events.clone(),
        }
    }

    fn push(&self, event: TimelineEvent) {
        let mut events = self.events.lock();
        if events.len() >= self.max_events {
            events.remove(0); // 环形淘汰最旧
        }
        events.push(event);
    }
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// UTF-8 lossy 截断，超长加 "..." 后缀
fn truncate_utf8(bytes: &[u8], max_len: usize) -> String {
    if bytes.len() <= max_len {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut truncated = String::from_utf8_lossy(&bytes[..max_len]).into_owned();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_command_returns_incrementing_id() {
        let tl = Timeline::new();
        let id1 = tl.record_command(b"ls\n", 0);
        let id2 = tl.record_command(b"pwd\n", 10);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn record_output_skips_zero_bytes() {
        let tl = Timeline::new();
        tl.record_output(0, 0, 0);
        assert!(tl.events(None).is_empty());
        tl.record_output(0, 5, 5);
        assert_eq!(tl.events(None).len(), 1);
    }

    #[test]
    fn events_returns_most_recent_n() {
        let tl = Timeline::new();
        for i in 0..10 {
            tl.record_control(&format!("ctrl+{i}"));
        }
        let last3 = tl.events(Some(3));
        assert_eq!(last3.len(), 3);
        // 最旧的在前
        assert!(matches!(&last3[0], TimelineEvent::Control { control, .. } if control == "ctrl+7"));
        assert!(matches!(&last3[2], TimelineEvent::Control { control, .. } if control == "ctrl+9"));
    }

    #[test]
    fn ring_eviction_drops_oldest() {
        let tl = Timeline::with_capacity(3);
        tl.record_control("a");
        tl.record_control("b");
        tl.record_control("c");
        tl.record_control("d"); // 应淘汰 a
        let events = tl.events(None);
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], TimelineEvent::Control { control, .. } if control == "b"));
    }

    #[test]
    fn truncate_utf8_long_input() {
        let tl = Timeline::new();
        let big = vec![b'x'; 5000];
        let id = tl.record_command(&big, 0);
        let events = tl.events(None);
        let cmd = events.iter().find_map(|e| match e {
            TimelineEvent::Command { command_id, input, .. } if *command_id == id => Some(input),
            _ => None,
        }).unwrap();
        assert!(cmd.ends_with("..."));
        // 4096 + "..." = 4099
        assert_eq!(cmd.len(), 4096 + 3);
    }

    #[test]
    fn state_change_recorded() {
        let tl = Timeline::new();
        tl.record_state_change("creating", "ready");
        let events = tl.events(None);
        assert!(matches!(&events[0], TimelineEvent::StateChange { from, to, .. } if from == "creating" && to == "ready"));
    }

    #[test]
    fn clone_shares_state() {
        let tl = Timeline::new();
        let clone = tl.clone();
        tl.record_control("ctrl+c");
        // clone 应能看到 tl 记录的事件（共享 Arc）
        assert_eq!(clone.events(None).len(), 1);
    }
}
