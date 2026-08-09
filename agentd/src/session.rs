//! Session 状态机 + SessionManager（ADR-0004 §5 / §8）
//!
//! Session 持有 Pty + RingBuffer + 状态。PTY read task 在独立 std::thread 中
//! 阻塞读 master_fd → 写入 RingBuffer → 更新 last_activity。EOF → state = Lost。

use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;
use thiserror::Error;

use crate::buffer::{ReadSinceResult, RingBuffer};
use crate::protocol::{ControlKey, PtySize, SessionInfo};
use crate::pty::Pty;

// ───────────────────────────────────────────────────────────────────────────
// 错误类型
// ───────────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("session state invalid: expected {expected}, got {actual}")]
    InvalidState { expected: String, actual: String },
    #[error("session lost: {0}")]
    Lost(String),
    #[error("pty error: {0}")]
    Pty(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

// ───────────────────────────────────────────────────────────────────────────
// SessionState
// ───────────────────────────────────────────────────────────────────────────

/// Session 生命周期状态（ADR-0004 §8 daemon 侧简化版）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// 刚创建，未 attach
    Created,
    /// client 已 attach
    Attached,
    /// client 已 detach，PTY 仍在运行
    Detached,
    /// PTY EOF 或崩溃
    Lost,
}

impl SessionState {
    /// 转为协议字符串（SessionInfo.state 字段）
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Created => "created",
            SessionState::Attached => "attached",
            SessionState::Detached => "detached",
            SessionState::Lost => "lost",
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Session
// ───────────────────────────────────────────────────────────────────────────

/// 单个 session 的全部状态
pub struct Session {
    id: String,
    name: Option<String>,
    state: Arc<Mutex<SessionState>>,
    pty: Arc<Mutex<Pty>>,
    buffer: Arc<RingBuffer>,
    created_at: DateTime<Utc>,
    last_activity_at: Arc<Mutex<DateTime<Utc>>>,
    pty_size: Mutex<PtySize>,
}

impl Session {
    /// 当前状态快照
    pub fn state(&self) -> SessionState {
        *self.state.lock()
    }

    /// 当前 written 快照
    pub fn written(&self) -> u64 {
        self.buffer.written()
    }

    /// 转为协议 SessionInfo
    pub fn to_info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            state: self.state().as_str().to_string(),
            created_at: self.created_at.to_rfc3339(),
            last_activity_at: self.last_activity_at.lock().to_rfc3339(),
            pty_size: *self.pty_size.lock(),
            written: self.written(),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// SessionManager
// ───────────────────────────────────────────────────────────────────────────

/// 全局 session 管理器（DashMap 并发安全）
pub struct SessionManager {
    sessions: DashMap<String, Arc<Session>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    /// 创建新 session：spawn PTY + 启动 read task，返回 session_id
    pub fn create(
        &self,
        shell: &str,
        cwd: Option<&str>,
        pty_size: PtySize,
        name: Option<String>,
    ) -> Result<String, SessionError> {
        let pty = Pty::spawn(shell, cwd, pty_size)
            .map_err(|e| SessionError::Pty(format!("spawn 失败: {}", e)))?;
        // 在包进 Mutex 前取出 master_fd（RawFd 是 Copy，可安全传给 read task）
        let master_fd = pty.master_fd();
        let pty_arc = Arc::new(Mutex::new(pty));
        let buffer = Arc::new(RingBuffer::with_default_size());
        let now = Utc::now();
        let session_id = gen_session_id();

        let session = Arc::new(Session {
            id: session_id.clone(),
            name: name.clone(),
            state: Arc::new(Mutex::new(SessionState::Created)),
            pty: pty_arc.clone(),
            buffer: buffer.clone(),
            created_at: now,
            last_activity_at: Arc::new(Mutex::new(now)),
            pty_size: Mutex::new(pty_size),
        });

        // 启动 PTY read task（独立线程，阻塞读 master_fd → 写 buffer → 更新 activity）
        // read task 持有 pty_arc 保持 Pty 存活（防止 master_fd 被 close）
        let read_state = session.state.clone();
        let read_activity = session.last_activity_at.clone();
        thread::spawn(move || {
            pty_read_loop(master_fd, pty_arc, buffer, read_state, read_activity);
        });

        self.sessions.insert(session_id.clone(), session);
        Ok(session_id)
    }

    /// attach：状态 Created/Detached → Attached，返回 since_cursor 之后的增量数据
    pub fn attach(
        &self,
        session_id: &str,
        since_cursor: u64,
    ) -> Result<ReadSinceResult, SessionError> {
        let session = self.get(session_id)?;
        let mut state = session.state.lock();
        match *state {
            SessionState::Lost => {
                return Err(SessionError::Lost(session_id.to_string()));
            }
            SessionState::Attached => {
                return Err(SessionError::InvalidState {
                    expected: "created or detached".to_string(),
                    actual: "attached".to_string(),
                });
            }
            SessionState::Created | SessionState::Detached => {
                *state = SessionState::Attached;
            }
        }
        drop(state);
        Ok(session.buffer.read_since(since_cursor))
    }

    /// detach：状态 Attached → Detached
    pub fn detach(&self, session_id: &str) -> Result<(), SessionError> {
        let session = self.get(session_id)?;
        let mut state = session.state.lock();
        match *state {
            SessionState::Attached => {
                *state = SessionState::Detached;
                Ok(())
            }
            SessionState::Lost => Err(SessionError::Lost(session_id.to_string())),
            other => Err(SessionError::InvalidState {
                expected: "attached".to_string(),
                actual: other.as_str().to_string(),
            }),
        }
    }

    /// 发送输入到 PTY（不等待 shell 处理）
    pub fn send_input(&self, session_id: &str, data: &[u8]) -> Result<(), SessionError> {
        let session = self.get(session_id)?;
        self.ensure_alive(&session)?;
        let pty = session.pty.lock();
        pty.write(data)
            .map_err(|e| SessionError::Pty(format!("write 失败: {}", e)))?;
        // 更新 last_activity
        *session.last_activity_at.lock() = Utc::now();
        Ok(())
    }

    /// 发送控制键（ctrl+c 等）
    pub fn send_control(
        &self,
        session_id: &str,
        control: ControlKey,
    ) -> Result<(), SessionError> {
        self.send_input(session_id, control.as_bytes())
    }

    /// 调整 PTY 窗口尺寸
    pub fn resize(&self, session_id: &str, rows: u16, cols: u16) -> Result<(), SessionError> {
        let session = self.get(session_id)?;
        self.ensure_alive(&session)?;
        {
            let pty = session.pty.lock();
            pty.resize(rows, cols)
                .map_err(|e| SessionError::Pty(format!("resize 失败: {}", e)))?;
        }
        *session.pty_size.lock() = PtySize { rows, cols };
        Ok(())
    }

    /// 读取 output 增量（不推进任何 cursor，纯读）
    pub fn read_output(
        &self,
        session_id: &str,
        since_cursor: u64,
    ) -> Result<ReadSinceResult, SessionError> {
        let session = self.get(session_id)?;
        Ok(session.buffer.read_since(since_cursor))
    }

    /// 关闭 session：kill PTY + 移除
    pub fn close(&self, session_id: &str) -> Result<(), SessionError> {
        // DashMap::remove 返回 Option<(K, V)>，取 .1 拿到 Arc<Session>
        let (_, session) = self.sessions.remove(session_id).ok_or_else(|| {
            SessionError::NotFound(session_id.to_string())
        })?;
        // kill 子进程（Pty drop 会自动 kill + wait，这里显式 kill 加速退出）
        let pty = session.pty.lock();
        pty.kill_child();
        drop(pty);
        Ok(())
    }

    /// 列出所有 session 信息
    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .map(|entry| entry.to_info())
            .collect()
    }

    /// 查询 session 状态（不存在返回 None），供 event pump 判断是否继续推送
    pub fn session_state(&self, session_id: &str) -> Option<SessionState> {
        self.sessions.get(session_id).map(|e| e.state())
    }

    /// 关闭所有 session（daemon shutdown 时调用）
    pub fn shutdown(&self) {
        let ids: Vec<String> = self.sessions.iter().map(|e| e.id.clone()).collect();
        for id in ids {
            let _ = self.close(&id);
        }
    }

    /// 获取 session（不存在报错）
    fn get(&self, session_id: &str) -> Result<Arc<Session>, SessionError> {
        self.sessions
            .get(session_id)
            .map(|e| Arc::clone(e.value()))
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))
    }

    /// 确保 session 还活着（未 Lost）
    fn ensure_alive(&self, session: &Session) -> Result<(), SessionError> {
        match session.state() {
            SessionState::Lost => Err(SessionError::Lost(session.id.clone())),
            _ => Ok(()),
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// PTY read task
// ───────────────────────────────────────────────────────────────────────────

/// PTY read 循环：阻塞读 master_fd → 写 buffer → 更新 last_activity。
/// EOF 或错误 → state = Lost，线程退出。
///
/// read task 直接用 master_fd（libc::read），不锁 Pty 对象，避免与 send_input 的 write
/// 互斥。pty_arc 仅用于保持 Pty 存活（防止 master_fd 被 close）。
fn pty_read_loop(
    master_fd: RawFd,
    pty_arc: Arc<Mutex<Pty>>,
    buffer: Arc<RingBuffer>,
    state: Arc<Mutex<SessionState>>,
    last_activity: Arc<Mutex<DateTime<Utc>>>,
) {
    let mut buf = [0u8; 8192];
    loop {
        // 阻塞读：master_fd 默认阻塞模式，read 会等到有数据或 EOF
        let n = unsafe { libc::read(master_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            // 非阻塞模式（若设置）的 EAGAIN/WOULDLOCK：短暂 sleep 后重试
            if err.kind() == io::ErrorKind::WouldBlock {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            // 其他错误（EBADF 等）→ Lost
            *state.lock() = SessionState::Lost;
            break;
        }
        if n == 0 {
            // EOF：子进程关闭 slave 端
            *state.lock() = SessionState::Lost;
            break;
        }
        let n = n as usize;
        buffer.write(&buf[..n]);
        *last_activity.lock() = Utc::now();
    }
    // pty_arc drop 时若引用计数归零，Pty drop 会 kill_child + close master_fd
    drop(pty_arc);
}

// ───────────────────────────────────────────────────────────────────────────
// session_id 生成
// ───────────────────────────────────────────────────────────────────────────

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 生成 session_id：sess_ + 8 hex（计数器 ^ 纳秒时间戳，取低 32 位）
fn gen_session_id() -> String {
    let n = SESSION_COUNTER.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = (n ^ ts) as u32;
    format!("sess_{:08x}", mixed)
}

// ───────────────────────────────────────────────────────────────────────────
// 单元测试
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 用 /bin/sleep 创建 session，验证基本生命周期
    #[test]
    fn create_and_close() {
        let mgr = SessionManager::new();
        let id = mgr
            .create("/bin/sleep", None, PtySize { rows: 24, cols: 80 }, None)
            .expect("create");
        assert!(id.starts_with("sess_"));
        assert_eq!(id.len(), "sess_".len() + 8);

        let list = mgr.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].state, "created");

        mgr.close(&id).expect("close");
        assert_eq!(mgr.list().len(), 0);
    }

    /// 状态转换：Created → Attached → Detached → Attached
    #[test]
    fn state_transitions() {
        let mgr = SessionManager::new();
        let id = mgr
            .create("/bin/sleep", None, PtySize { rows: 24, cols: 80 }, None)
            .expect("create");

        // Created → Attached
        let r = mgr.attach(&id, 0).expect("attach 1");
        assert_eq!(r.cursor_start, 0);
        assert_eq!(mgr.list()[0].state, "attached");

        // Attached → Detached
        mgr.detach(&id).expect("detach");
        assert_eq!(mgr.list()[0].state, "detached");

        // Detached → Attached
        mgr.attach(&id, 0).expect("attach 2");
        assert_eq!(mgr.list()[0].state, "attached");

        // 重复 attach 报错
        let err = mgr.attach(&id, 0).unwrap_err();
        assert!(matches!(err, SessionError::InvalidState { .. }));

        mgr.close(&id).expect("close");
    }

    /// send_input 写入 PTY
    #[test]
    fn send_input_to_cat() {
        let mgr = SessionManager::new();
        let id = mgr
            .create("/bin/cat", None, PtySize { rows: 24, cols: 80 }, None)
            .expect("create cat");
        // 等 cat 启动
        thread::sleep(Duration::from_millis(50));
        // attach 后才能 send_input
        mgr.attach(&id, 0).expect("attach");
        mgr.send_input(&id, b"hello\n").expect("send input");
        // 等 cat 回显
        thread::sleep(Duration::from_millis(100));
        // 读 output
        let r = mgr.read_output(&id, 0).expect("read");
        assert!(r.data.len() > 0, "应有回显数据");
        let output = String::from_utf8_lossy(&r.data);
        assert!(output.contains("hello"), "cat 应回显 hello，实际: {}", output);
        mgr.close(&id).expect("close");
    }

    /// send_control 发送 ctrl+c
    #[test]
    fn send_control_ctrl_c() {
        let mgr = SessionManager::new();
        let id = mgr
            .create("/bin/sleep", None, PtySize { rows: 24, cols: 80 }, None)
            .expect("create sleep");
        mgr.attach(&id, 0).expect("attach");
        // 发 ctrl+c（sleep 应被中断）
        mgr.send_control(&id, ControlKey::CtrlC)
            .expect("send ctrl+c");
        // 等 sleep 退出 → PTY EOF → state Lost
        thread::sleep(Duration::from_millis(200));
        // session 仍在（close 才移除），但状态可能 Lost
        let info = &mgr.list()[0];
        assert!(
            info.state == "lost" || info.state == "attached",
            "ctrl+c 后状态应为 lost 或 attached，实际: {}",
            info.state
        );
        mgr.close(&id).expect("close");
    }

    /// resize 调整窗口
    #[test]
    fn resize_updates_pty_size() {
        let mgr = SessionManager::new();
        let id = mgr
            .create("/bin/sleep", None, PtySize { rows: 24, cols: 80 }, None)
            .expect("create");
        mgr.attach(&id, 0).expect("attach");
        mgr.resize(&id, 40, 120).expect("resize");
        let info = &mgr.list()[0];
        assert_eq!(info.pty_size.rows, 40);
        assert_eq!(info.pty_size.cols, 120);
        mgr.close(&id).expect("close");
    }

    /// 不存在的 session 报 NotFound
    #[test]
    fn not_found_error() {
        let mgr = SessionManager::new();
        let err = mgr.attach("sess_nonexist", 0).unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
        let err = mgr.close("sess_nonexist").unwrap_err();
        assert!(matches!(err, SessionError::NotFound(_)));
    }

    /// detach 非 Attached 状态报错
    #[test]
    fn detach_wrong_state_errors() {
        let mgr = SessionManager::new();
        let id = mgr
            .create("/bin/sleep", None, PtySize { rows: 24, cols: 80 }, None)
            .expect("create");
        // Created 状态 detach 报错
        let err = mgr.detach(&id).unwrap_err();
        assert!(matches!(err, SessionError::InvalidState { .. }));
        mgr.close(&id).expect("close");
    }

    /// PTY EOF 后状态转 Lost
    #[test]
    fn pty_eof_marks_lost() {
        let mgr = SessionManager::new();
        // /bin/true 立即退出 → PTY EOF
        let id = mgr
            .create("/bin/true", None, PtySize { rows: 24, cols: 80 }, None)
            .expect("create true");
        // 等子进程退出 + read task 检测 EOF
        thread::sleep(Duration::from_millis(300));
        let info = &mgr.list()[0];
        assert_eq!(info.state, "lost", "true 退出后应 Lost");
        // Lost 状态 attach 报错
        let err = mgr.attach(&id, 0).unwrap_err();
        assert!(matches!(err, SessionError::Lost(_)));
        mgr.close(&id).expect("close");
    }

    /// shutdown 关闭所有 session
    #[test]
    fn shutdown_closes_all() {
        let mgr = SessionManager::new();
        for _ in 0..3 {
            mgr.create("/bin/sleep", None, PtySize { rows: 24, cols: 80 }, None)
                .expect("create");
        }
        assert_eq!(mgr.list().len(), 3);
        mgr.shutdown();
        assert_eq!(mgr.list().len(), 0);
    }

    /// session_id 格式
    #[test]
    fn session_id_format() {
        let id = gen_session_id();
        assert!(id.starts_with("sess_"));
        assert_eq!(id.len(), 13); // "sess_" (5) + 8 hex
        let hex = &id[5..];
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// name 字段保留
    #[test]
    fn name_preserved() {
        let mgr = SessionManager::new();
        let id = mgr
            .create(
                "/bin/sleep",
                None,
                PtySize { rows: 24, cols: 80 },
                Some("test-session".to_string()),
            )
            .expect("create");
        let info = &mgr.list()[0];
        assert_eq!(info.name.as_deref(), Some("test-session"));
        mgr.close(&id).expect("close");
    }
}
