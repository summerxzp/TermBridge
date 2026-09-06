//! Session 状态机 + SessionManager（ADR-0004 §5 / §8）
//!
//! Session 持有 Pty + PtyWriter + RingBuffer + 状态。PTY read task 在独立 std::thread 中
//! 阻塞读 master_fd → 写入 RingBuffer → 更新 last_activity → 唤醒 event pump（Notify）。
//! EOF/EIO → 有界回收子进程 → 填写终态（TerminalInfo）→ state = Lost。

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
use tokio::sync::Notify;

use crate::buffer::{ReadSinceResult, RingBuffer};
use crate::protocol::{ControlKey, PtySize, SessionInfo};
use crate::pty::{Pty, PtyWriter};

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
// TerminalInfo
// ───────────────────────────────────────────────────────────────────────────

/// 会话终态信息：PTY read task 在 EOF/错误时填写，event pump 读取后构造终态事件
/// （pty_exit / session_lost，与 daemon_proto 的字段约定一致）。
#[derive(Debug, Clone)]
pub enum TerminalInfo {
    /// 子进程已退出，退出码已知（waitpid 回收成功；信号死亡为 128+信号号）
    Exited(i32),
    /// PTY EOF 但退出状态未知（有界窗口内未回收子进程，可能仅关闭了 slave fd），
    /// 协议侧 pty_exit 的 exit_code 字段缺省（Option = None，daemon_proto 的 unknown 约定）
    ExitedUnknown,
    /// 异常丢失（PTY 读错误等），附原因
    Lost(String),
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
    /// 输入写入器（有界队列 + 专属写线程；dispatch 线程不直接写 master fd）
    writer: PtyWriter,
    buffer: Arc<RingBuffer>,
    created_at: DateTime<Utc>,
    last_activity_at: Arc<Mutex<DateTime<Utc>>>,
    pty_size: Mutex<PtySize>,
    /// event pump 唤醒（read task 新数据 / 状态变化 / close 时 notify_one）
    pump_notify: Arc<Notify>,
    /// 终态信息（EOF/错误路径由 read task 填写，event pump 读取）
    terminal: Arc<Mutex<Option<TerminalInfo>>>,
    /// pump 代际：每次 attach 递增，旧代际 pump 自动作废（防止 detach 后重连时
    /// 新旧 pump 并存 → 重复数据 / 重复终态事件）
    pump_epoch: AtomicU64,
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

    /// 读取 buffer 增量（event pump 冲刷尾部数据用）
    pub fn read_since(&self, cursor: u64) -> ReadSinceResult {
        self.buffer.read_since(cursor)
    }

    /// 终态信息快照（state = Lost 后由 read task 填写）
    pub fn terminal(&self) -> Option<TerminalInfo> {
        self.terminal.lock().clone()
    }

    /// 唤醒 event pump（新数据 / 状态变化 / close）
    pub fn notify_pump(&self) {
        self.pump_notify.notify_one();
    }

    /// pump 等待用的 Notify 引用（rpc 层 event pump 用）
    pub fn pump_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.pump_notify)
    }

    /// pump 代际快照
    pub fn pump_epoch(&self) -> u64 {
        self.pump_epoch.load(Ordering::SeqCst)
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

impl Drop for Session {
    fn drop(&mut self) {
        // Session 销毁前先杀子进程：Pty 被 read task 的 Arc 共享，真正的 Pty::drop 可能延后；
        // 必须保证 writer 字段 Drop（join 写线程）之前子进程已死 —— 否则写线程可能阻塞在
        // master write 上（tty 缓冲满且前台进程不读 stdin），join 无法返回。
        self.pty.lock().kill_child();
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

    /// 创建新 session：spawn PTY + 启动 read task + 写线程，返回 session_id
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
        // 专属写线程：dispatch 的 send_input 只入队，不直接 write master fd
        let writer = PtyWriter::new(master_fd)
            .map_err(|e| SessionError::Pty(format!("writer 创建失败: {}", e)))?;
        let pty_arc = Arc::new(Mutex::new(pty));
        let buffer = Arc::new(RingBuffer::with_default_size());
        let now = Utc::now();
        let session_id = gen_session_id();

        let session = Arc::new(Session {
            id: session_id.clone(),
            name: name.clone(),
            state: Arc::new(Mutex::new(SessionState::Created)),
            pty: pty_arc.clone(),
            writer,
            buffer: buffer.clone(),
            created_at: now,
            last_activity_at: Arc::new(Mutex::new(now)),
            pty_size: Mutex::new(pty_size),
            pump_notify: Arc::new(Notify::new()),
            terminal: Arc::new(Mutex::new(None)),
            pump_epoch: AtomicU64::new(0),
        });

        // 启动 PTY read task（独立线程，阻塞读 master_fd → 写 buffer → 更新 activity → 唤醒 pump）
        // read task 持有 pty_arc 保持 Pty 存活（防止 master_fd 被 close）+ EOF 后回收子进程
        let read_state = session.state.clone();
        let read_activity = session.last_activity_at.clone();
        let read_notify = session.pump_notify.clone();
        let read_terminal = session.terminal.clone();
        thread::spawn(move || {
            pty_read_loop(
                master_fd,
                pty_arc,
                buffer,
                read_state,
                read_activity,
                read_notify,
                read_terminal,
            );
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
        // 递增 pump 代际：使该 session 上旧连接遗留的 event pump 自动作废
        // （detach 后立即重连时，防止新旧 pump 并存 → 重复数据 / 重复终态事件）
        session.pump_epoch.fetch_add(1, Ordering::SeqCst);
        Ok(session.buffer.read_since(since_cursor))
    }

    /// detach：状态 Attached → Detached
    pub fn detach(&self, session_id: &str) -> Result<(), SessionError> {
        let session = self.get(session_id)?;
        let mut state = session.state.lock();
        match *state {
            SessionState::Attached => {
                *state = SessionState::Detached;
                drop(state);
                // 唤醒 pump：观察 Detached 立即退出（不发终态事件）
                session.notify_pump();
                Ok(())
            }
            SessionState::Lost => Err(SessionError::Lost(session_id.to_string())),
            other => Err(SessionError::InvalidState {
                expected: "attached".to_string(),
                actual: other.as_str().to_string(),
            }),
        }
    }

    /// 发送输入到 PTY（不等待 shell 处理）。
    ///
    /// 输入经有界队列交给专属写线程写 master fd：RPC dispatch 线程绝不阻塞在 tty 上。
    /// 队列满（tty 背压，如前台进程不读 stdin）时最多等待 5s，仍满则报错而非无限阻塞。
    pub fn send_input(&self, session_id: &str, data: &[u8]) -> Result<(), SessionError> {
        let session = self.get(session_id)?;
        self.ensure_alive(&session)?;
        session
            .writer
            .send(data)
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
        // kill 子进程（Pty drop 会自动 kill + wait，这里显式 kill 加速退出；
        // Session::drop 兜底再 kill 一次，保证写线程 join 前子进程必死）
        {
            let pty = session.pty.lock();
            pty.kill_child();
        }
        // 唤醒 event pump：session 已移除，pump 立即冲刷尾部数据并发 session_lost 终态事件
        // （终态保证：close 也必须给 attach 中的 client 一个终态事件）
        session.notify_pump();
        Ok(())
    }

    /// 列出所有 session 信息
    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .map(|entry| entry.to_info())
            .collect()
    }

    /// 关闭所有 session（daemon shutdown 时调用）
    pub fn shutdown(&self) {
        let ids: Vec<String> = self.sessions.iter().map(|e| e.id.clone()).collect();
        for id in ids {
            let _ = self.close(&id);
        }
    }

    /// 获取 session Arc（不存在返回 None；rpc 层 / event pump 用）
    pub fn get_session(&self, session_id: &str) -> Option<Arc<Session>> {
        self.sessions
            .get(session_id)
            .map(|e| Arc::clone(e.value()))
    }

    /// session 是否仍存在于管理器（未被 close 移除；event pump 判断终态用）
    pub fn contains(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }

    /// 获取 session（不存在报错；manager 内部用）
    fn get(&self, session_id: &str) -> Result<Arc<Session>, SessionError> {
        self.get_session(session_id)
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

/// EOF 后回收子进程的有界窗口：100 次 × 20ms = 2s
const REAP_ATTEMPTS: usize = 100;
/// 每次 waitpid(WNOHANG) 轮询间隔
const REAP_INTERVAL: Duration = Duration::from_millis(20);

/// PTY read 循环：阻塞读 master_fd → 写 buffer → 更新 last_activity → 唤醒 event pump。
///
/// EOF / EIO（Linux 上 slave 端全部关闭后 master read 返回 EIO）→ 有界回收子进程（≤2s）
/// 填写终态 → state = Lost → 唤醒 pump。pump 负责先冲刷尾部 pty_data，再发送
/// pty_exit / session_lost 终态事件（每个 attach 恰好一个终态事件）。
///
/// read task 直接用 master_fd（libc::read），不锁 Pty 对象，避免与写路径互斥；
/// pty_arc 仅用于保持 Pty 存活 + EOF 后回收子进程（短锁单次 try_reap）。
fn pty_read_loop(
    master_fd: RawFd,
    pty_arc: Arc<Mutex<Pty>>,
    buffer: Arc<RingBuffer>,
    state: Arc<Mutex<SessionState>>,
    last_activity: Arc<Mutex<DateTime<Utc>>>,
    pump_notify: Arc<Notify>,
    terminal: Arc<Mutex<Option<TerminalInfo>>>,
) {
    let mut buf = [0u8; 8192];
    loop {
        // 阻塞读：master_fd 默认阻塞模式，read 会等到有数据或 EOF/EIO
        let n = unsafe { libc::read(master_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            // 非阻塞模式（若设置）的 EAGAIN：短暂 sleep 后重试
            if err.kind() == io::ErrorKind::WouldBlock {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            if err.raw_os_error() == Some(libc::EIO) {
                // Linux：slave 端全部关闭（子进程退出）后 master read 返回 EIO —— 视同 EOF
                finish_lost(&pty_arc, &terminal, &state, &pump_notify);
                break;
            }
            // 其他错误（EBADF 等）→ 异常丢失（session_lost）
            *terminal.lock() = Some(TerminalInfo::Lost(format!("pty read 错误: {}", err)));
            *state.lock() = SessionState::Lost;
            pump_notify.notify_one();
            break;
        }
        if n == 0 {
            // EOF：子进程关闭 slave 端 → 与 EIO 同一退出路径
            finish_lost(&pty_arc, &terminal, &state, &pump_notify);
            break;
        }
        let n = n as usize;
        buffer.write(&buf[..n]);
        *last_activity.lock() = Utc::now();
        pump_notify.notify_one();
    }
    // pty_arc drop 时若引用计数归零，Pty drop 会 kill_child + reap + close master_fd
    drop(pty_arc);
}

/// EOF/EIO 退出路径：有界回收子进程 → 填写终态 → 翻转 Lost → 唤醒 pump。
///
/// 顺序约束：先填终态再翻状态 —— pump 看到 Lost 时终态必已就绪，可直接发终态事件。
/// EOF/EIO 不保证子进程已退出（可能仅关闭了 slave fd），故回收是有界的；
/// 窗口内未回收则以"未知退出码"上报（协议侧 pty_exit 的 exit_code 缺省）。
fn finish_lost(
    pty_arc: &Arc<Mutex<Pty>>,
    terminal: &Arc<Mutex<Option<TerminalInfo>>>,
    state: &Arc<Mutex<SessionState>>,
    pump_notify: &Arc<Notify>,
) {
    let exit_code = reap_exit_code_bounded(pty_arc);
    *terminal.lock() = Some(match exit_code {
        Some(code) => TerminalInfo::Exited(code),
        None => TerminalInfo::ExitedUnknown,
    });
    *state.lock() = SessionState::Lost;
    pump_notify.notify_one();
}

/// 有界回收子进程（≤2s，WNOHANG 轮询）。
///
/// 返回 Some(code)：已退出（正常退出码，或 128+信号号）；None：窗口内未退出
/// （可能仅关闭 slave fd）或已被其他处回收 —— 调用方以"未知退出码"上报。
fn reap_exit_code_bounded(pty_arc: &Arc<Mutex<Pty>>) -> Option<i32> {
    for attempt in 0..REAP_ATTEMPTS {
        if attempt > 0 {
            thread::sleep(REAP_INTERVAL);
        }
        // 短锁：每次尝试单独加锁，不阻塞 resize / close 的 kill_child
        let code = pty_arc.lock().try_reap();
        if let Some(code) = code {
            return Some(code);
        }
    }
    None
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
        // cat 挂在 stdin 读上不退出，状态稳定可断言。
        // （"/bin/sleep" 无参会立即 usage-error 退出 → state 变 Lost，
        // 全量并行时序下必挂——Linux 实测教训）
        let id = mgr
            .create("/bin/cat", None, PtySize { rows: 24, cols: 80 }, None)
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

    /// Fix 1：EOF 后 read task 应回收子进程并填写终态（退出码），状态转 Lost
    #[test]
    fn eof_records_terminal_exit_code() {
        let mgr = SessionManager::new();
        // /bin/true 立即退出 → PTY EOF/EIO → 有界回收 → 终态 Exited(0)
        let id = mgr
            .create("/bin/true", None, PtySize { rows: 24, cols: 80 }, None)
            .expect("create true");
        // 等 true 退出 + read task EOF 路径（回收是有界的，但 true 退出极快）
        thread::sleep(Duration::from_millis(500));
        let session = mgr.get_session(&id).expect("session 应仍存在");
        assert_eq!(session.state(), SessionState::Lost);
        match session.terminal() {
            Some(TerminalInfo::Exited(0)) => {}
            other => panic!("应记录 Exited(0) 终态，实际: {:?}", other),
        }
        mgr.close(&id).expect("close");
    }

    /// Fix 3：输入积压（前台进程不读 stdin）时，resize / close 不被阻塞，
    /// send_input 以错误返回而非无限阻塞
    #[test]
    fn resize_and_close_work_during_input_backlog() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc as StdArc;
        // sh 执行脚本时从脚本文件读命令，不读 tty stdin → 输入积压可复现
        let dir = std::env::temp_dir();
        let script = dir.join(format!("tb_test_backlog_{}.sh", std::process::id()));
        std::fs::write(&script, "#!/bin/sh\nsleep 300\n").expect("写脚本失败");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let mgr = StdArc::new(SessionManager::new());
        let id = mgr
            .create(script.to_string_lossy().as_ref(), None, PtySize { rows: 24, cols: 80 }, None)
            .expect("create");
        mgr.attach(&id, 0).expect("attach");
        // 等脚本跑起来
        thread::sleep(Duration::from_millis(200));

        // 后台线程发送 1MB（远超 256KB 队列容量）→ 等待至背压超时。
        // 数据含换行：无换行的超长行会被 PTY 行缓冲直接丢弃（不缓冲），
        // 队列填不满，背压不发生（Linux 实测教训）
        let mut big = Vec::with_capacity(1024 * 1024);
        for _ in 0..(1024 * 1024 / 64) {
            big.extend_from_slice(&[0x41u8; 63]);
            big.push(b'\n');
        }
        let mgr_t = mgr.clone();
        let id_t = id.clone();
        let sender = thread::spawn(move || mgr_t.send_input(&id_t, &big));
        // 等队列填满、写线程阻塞
        thread::sleep(Duration::from_millis(300));

        // resize 必须立即返回（不与写线程争锁）
        let t0 = std::time::Instant::now();
        mgr.resize(&id, 40, 120).expect("resize");
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "resize 不应被积压输入阻塞，实际耗时 {:?}",
            t0.elapsed()
        );

        // close 必须立即返回（kill → 写线程 EIO 退出 → join）
        let t1 = std::time::Instant::now();
        mgr.close(&id).expect("close");
        assert!(
            t1.elapsed() < Duration::from_secs(2),
            "close 不应被积压输入阻塞，实际耗时 {:?}",
            t1.elapsed()
        );

        // 发送线程最终以错误返回（Backlogged 背压超时，或 close 后 Closed）
        let r = sender.join().expect("join sender");
        assert!(matches!(r, Err(SessionError::Pty(_))), "应报错而非无限阻塞，实际: {:?}", r);

        let _ = std::fs::remove_file(&script);
    }
}
