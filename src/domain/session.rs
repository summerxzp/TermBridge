//! Session —— 运行实体（§4.3 / §5.4）
//!
//! ```text
//! Session
//!   ├── id: SessionId
//!   ├── host: HostName              // 归属 Host，不归属 Connection
//!   ├── state: SessionState
//!   ├── pty_size: PtySize
//!   ├── output: OutputEngine        // RingBuffer + 双游标 + Waiter
//!   ├── handle: Arc<dyn TerminalHandle>  // 当前 attachment（Phase 0-C：始终 attached）
//!   └── read_task: JoinHandle       // PTY → OutputEngine 灌数据
//! ```
//!
//! PTY read task：`while let Some(data) = handle.read() { output.buffer().write(&data) }`
//! None / Err → 退出，置 SessionState::Lost（§4.6 契约 10：disconnect 不销毁 Session，
//! 但状态反映断开）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use super::output::{OutputEngine, ReadOutputParams, ReadOutputResult, DEFAULT_BUFFER_SIZE};
use super::policy::ApprovalMode;
use super::provider::{ControlKey, HostName, PtySize, TerminalHandle, TermError};
use super::timeline::Timeline;

/// 获取当前 Unix 时间戳（秒）。系统时钟异常时返回 0。
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 获取当前 Unix 时间戳（毫秒）。系统时钟异常时返回 0。
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ───────────────────────────────────────────────────────────────────────────
// SessionId / SessionState
// ───────────────────────────────────────────────────────────────────────────

pub type SessionId = String;

/// Session 状态机（§5.4，Phase 0-C 最小子集）。
///
/// 完整状态机（RUNNING/IDLE/RECONNECTING/DETACHED）留给 Phase 1+。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// 正在创建（Provider.open 进行中）
    Creating,
    /// PTY + shell 就绪，read task 运行中（覆盖 §5.4 READY/RUNNING/IDLE）
    Ready,
    /// 正在关闭（close 进行中）
    Closing,
    /// 已关闭（shell + channel 已释放）
    Closed,
    /// 连接丢失（read task 异常退出，shell 可能仍在远端）
    Lost,
}

impl SessionState {
    /// 已显式关闭（拒绝一切操作）
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Closed)
    }

    /// read task 已退出（Closed 或 Lost）。Lost 时 buffer 仍可读。
    pub fn is_detached(self) -> bool {
        matches!(self, Self::Closed | Self::Lost)
    }

    /// PTY 可写（仅 Ready）
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Ready)
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Session
// ───────────────────────────────────────────────────────────────────────────

/// Session 运行实体（§4.3）。
///
/// 一个 Session = 一个 OutputEngine + 一个 TerminalHandle + 一个 PTY read task。
/// Agent 通过 `read_output` / `send_input` / `send_control` / `close` 操作。
///
/// Phase 1：新增 `last_activity`（Unix 秒）供 idleReaper 判定空闲超时。
pub struct Session {
    id: SessionId,
    host: HostName,
    state: Arc<Mutex<SessionState>>,
    pty_size: PtySize,
    output: OutputEngine,
    handle: Arc<dyn TerminalHandle>,
    read_task: Mutex<Option<JoinHandle<()>>>,
    /// 最近一次活动时间（Unix 秒）。send_input / read_output / send_control 调用时更新。
    /// idleReaper 据此判定是否空闲超时（§7.4 Phase 1）。
    last_activity: AtomicU64,
    /// 执行事件时间线（Phase 4-A）。记录命令/输出/控制/状态事件元数据。
    timeline: Timeline,
    /// Session 创建时间（Unix 毫秒，Phase 4-B）。
    created_at: u64,
    /// 审批模式（ADR-0018）。session-scoped，不持久化。
    approval_mode: parking_lot::Mutex<ApprovalMode>,
}

impl Session {
    /// 创建并启动 PTY read task。
    ///
    /// 调用方（SessionManager）先 `provider.open()` 拿到 handle，再 `Session::new(...)`。
    pub fn new(
        id: SessionId,
        host: HostName,
        pty_size: PtySize,
        handle: Arc<dyn TerminalHandle>,
    ) -> Self {
        let output = OutputEngine::new(DEFAULT_BUFFER_SIZE);
        let state = Arc::new(Mutex::new(SessionState::Ready));
        let buffer = output.buffer().clone();

        // Phase 4-A：初始化 timeline，记录 Creating→Ready 转换
        let timeline = Timeline::new();
        timeline.record_state_change("creating", "ready");
        let read_timeline = timeline.clone();

        // spawn PTY read task：handle.read() → output.buffer().write()
        let read_handle = Arc::clone(&handle);
        let read_state = Arc::clone(&state);
        let read_id = id.clone();
        let read_task = tokio::spawn(async move {
            loop {
                match read_handle.read().await {
                    Ok(Some(data)) => {
                        // Phase 4-A：记录 output 事件（write 前后 cursor + bytes）
                        let cursor_start = buffer.written();
                        buffer.write(&data);
                        let cursor_end = buffer.written();
                        read_timeline.record_output(cursor_start, cursor_end, data.len());
                    }
                    Ok(None) => {
                        tracing::info!(session=%read_id, "pty read task: EOF");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(session=%read_id, error=?e, "pty read task error, marking LOST");
                        break;
                    }
                }
            }
            // read task 退出 = 连接断开。置 Lost（§4.6 契约 10）。
            // 只在仍 Ready 时置 Lost（close 流程会先置 Closing/Closed）。
            let mut g = read_state.lock();
            if *g == SessionState::Ready {
                *g = SessionState::Lost;
            }
        });

        Self {
            id,
            host,
            state,
            pty_size,
            output,
            handle,
            read_task: Mutex::new(Some(read_task)),
            last_activity: AtomicU64::new(now_secs()),
            timeline,
            created_at: now_millis(),
            approval_mode: parking_lot::Mutex::new(ApprovalMode::Standard),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn state(&self) -> SessionState {
        *self.state.lock()
    }

    pub fn pty_size(&self) -> PtySize {
        self.pty_size
    }

    /// 最近一次活动时间（Unix 秒）。
    pub fn last_activity(&self) -> u64 {
        self.last_activity.load(Ordering::Relaxed)
    }

    /// Session 创建时间（Unix 毫秒，Phase 4-B）。
    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    /// 当前审批模式（ADR-0018）。
    pub fn approval_mode(&self) -> ApprovalMode {
        *self.approval_mode.lock()
    }

    /// 设置审批模式（仅 Control IPC 调用，ADR-0018）。
    ///
    /// 注：从已持有的锁内读取旧值（避免再次获取锁导致死锁），ApprovalMode 为 Copy。
    pub fn set_approval_mode(&self, mode: ApprovalMode) {
        let mut g = self.approval_mode.lock();
        let prev = *g;
        tracing::warn!(
            session = %self.id,
            from = %prev.as_str(),
            to = %mode.as_str(),
            "approval_mode changed"
        );
        *g = mode;
    }

    /// 是否空闲超过 `threshold_secs` 秒。供 idleReaper 判定（§7.4 Phase 1）。
    pub fn is_idle(&self, threshold_secs: u64) -> bool {
        let last = self.last_activity.load(Ordering::Relaxed);
        now_secs().saturating_sub(last) >= threshold_secs
    }

    /// 刷新 last_activity 为当前时间。send_input / read_output / send_control 调用前触发。
    fn touch_last_activity(&self) {
        self.last_activity.store(now_secs(), Ordering::Relaxed);
    }

    /// 测试辅助：直接设置 last_activity（模拟"很久没活动"）。
    #[cfg(test)]
    pub fn set_last_activity_for_test(&self, ts: u64) {
        self.last_activity.store(ts, Ordering::Relaxed);
    }

    /// 返回底层 TerminalHandle 引用（供 SessionManager 下转到具体 SshTerminalHandle 调 SFTP）。
    ///
    /// Phase 1：SFTP 能力通过 `handle.as_any().downcast_ref::<SshTerminalHandle>()` 访问。
    pub fn handle(&self) -> &Arc<dyn TerminalHandle> {
        &self.handle
    }

    /// 中止内部 PTY read task（供 CLI raw mode 接管读取循环）。
    ///
    /// 调用后 OutputEngine buffer 不再更新，但 `handle.read()` 可被外部独占调用，
    /// 不会与内部 read_task 竞争。Session 状态不变（仍为 Ready）。
    pub fn abort_read_task(&self) {
        if let Some(task) = self.read_task.lock().take() {
            task.abort();
        }
    }

    /// 返回执行事件时间线引用（Phase 4-A）。
    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// 读取输出（§5.3 三模式 + §4.6 契约 3/4/5/11）。
    ///
    /// 仅 `Closed` 拒绝；`Lost` 仍可读 buffer 剩余数据（§4.6 契约 10：disconnect 不销毁 Session）。
    pub async fn read_output(
        &self,
        params: ReadOutputParams,
    ) -> Result<ReadOutputResult, TermError> {
        if self.state().is_closed() {
            return Err(TermError::SessionClosed(self.id.clone()));
        }
        self.touch_last_activity();
        Ok(self.output.read_output(params).await)
    }

    /// 写输入到 PTY（§4.6 契约 7：立即返回，不等命令完成）。
    /// 仅 `Ready` 允许写；Lost/Closed 拒绝（连接已断）。
    pub async fn send_input(&self, data: &[u8]) -> Result<(), TermError> {
        if !self.state().is_usable() {
            return Err(TermError::SessionClosed(self.id.clone()));
        }
        self.touch_last_activity();
        // Phase 4-A：记录命令事件（发送前的 written cursor 作为 cursor_before）
        let cursor_before = self.output.buffer().written();
        self.timeline.record_command(data, cursor_before);
        self.handle.write(data).await
    }

    /// 发控制字符（§4.6 契约 8）。仅 `Ready` 允许。
    pub async fn send_control(&self, key: ControlKey) -> Result<(), TermError> {
        if !self.state().is_usable() {
            return Err(TermError::SessionClosed(self.id.clone()));
        }
        self.touch_last_activity();
        // Phase 4-A：记录控制键事件
        self.timeline.record_control(key.as_name());
        self.handle.send_control(key).await
    }

    /// 调整 PTY 尺寸。仅 `Ready` 允许。
    pub async fn resize(&self, size: PtySize) -> Result<(), TermError> {
        if !self.state().is_usable() {
            return Err(TermError::SessionClosed(self.id.clone()));
        }
        self.handle.resize(size).await
    }

    /// 关闭 Session（§4.6 契约 9：Session close 才结束远端 shell）。
    ///
    /// 幂等：已 Closed 直接返回 Ok。Lost 也会转为 Closed（清理本地资源）。
    pub async fn close(&self) -> Result<(), TermError> {
        // 已 Closed：幂等返回
        if self.state().is_closed() {
            return Ok(());
        }
        // Phase 4-A：记录 Ready/Lost → Closing 转换
        let from_state = format!("{:?}", self.state()).to_ascii_lowercase();
        self.timeline.record_state_change(&from_state, "closing");
        // 标记 Closing，阻止并发写
        *self.state.lock() = SessionState::Closing;

        // 1. abort read task（Lost 时已为 None，take 无副作用）
        if let Some(task) = self.read_task.lock().take() {
            task.abort();
        }

        // 2. close handle（发 EOF + disconnect；Lost 时 channel 可能已死，best-effort）
        let res = self.handle.close().await;

        // 3. 置 Closed（无论 handle.close 是否成功，Session 视角已关闭）
        *self.state.lock() = SessionState::Closed;
        // Phase 4-A：记录 Closing → Closed 转换
        self.timeline.record_state_change("closing", "closed");

        res
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // 兜底：drop 时若 read task 还在，abort 掉防泄漏
        if let Some(task) = self.read_task.lock().take() {
            task.abort();
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Session 摘要（给 list_sessions 工具用）
// ───────────────────────────────────────────────────────────────────────────

/// Session 摘要（list_sessions 返回）。
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: SessionId,
    pub host: HostName,
    pub state: SessionState,
    pub pty_size: PtySize,
    /// Session 创建时间（Unix 毫秒，Phase 4-B）。
    pub created_at: u64,
    /// 最近一次活动时间（Unix 秒，Phase 4-B 复用 last_activity 字段）。
    pub last_activity: u64,
    /// OutputEngine 当前 written cursor（Phase 4-B）。
    pub written: u64,
    /// Session 名称（Phase 4-B：client 侧 Session 暂无 name 字段，恒为 None）。
    pub name: Option<String>,
    /// 审批模式（ADR-0018）。
    pub approval_mode: ApprovalMode,
}

impl Session {
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            host: self.host.clone(),
            state: self.state(),
            pty_size: self.pty_size,
            created_at: self.created_at,
            last_activity: self.last_activity.load(Ordering::Relaxed),
            written: self.output.buffer().written(),
            name: None,
            approval_mode: self.approval_mode(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::time::Duration;

    /// 假 TerminalHandle：read 返回几条预设数据后 None(EOF)
    struct FakeHandle {
        chunks: Mutex<Vec<Bytes>>,
    }

    #[async_trait]
    impl TerminalHandle for FakeHandle {
        async fn read(&self) -> Result<Option<Bytes>, TermError> {
            let mut g = self.chunks.lock();
            Ok(g.pop())
        }
        async fn write(&self, _data: &[u8]) -> Result<(), TermError> {
            Ok(())
        }
        async fn send_control(&self, _c: ControlKey) -> Result<(), TermError> {
            Ok(())
        }
        async fn resize(&self, _size: PtySize) -> Result<(), TermError> {
            Ok(())
        }
        async fn close(&self) -> Result<(), TermError> {
            Ok(())
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[tokio::test]
    async fn session_read_task_feeds_buffer() {
        // FakeHandle 先返回 "world\n" 再 "hello\n"（pop 顺序），再 None
        // 注：pop 从尾部，所以写入顺序是 hello 然后 world
        let handle = Arc::new(FakeHandle {
            chunks: Mutex::new(vec![Bytes::from_static(b"hello\n"), Bytes::from_static(b"world\n")]),
        }) as Arc<dyn TerminalHandle>;

        let session = Session::new(
            "sess_test".into(),
            "testhost".into(),
            PtySize::default(),
            handle,
        );

        // 等 read task 把数据灌进 buffer
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 用 since_cursor=0 读，应拿到 hello + world
        let r = session
            .read_output(ReadOutputParams {
                since_cursor: Some(0),
                max_bytes: Some(4096),
                ..Default::default()
            })
            .await
            .unwrap();
        let s = String::from_utf8_lossy(&r.output);
        assert!(s.contains("hello"));
        assert!(s.contains("world"));

        // read task EOF 后 state 应置 Lost
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(session.state(), SessionState::Lost);
    }

    #[tokio::test]
    async fn session_close_sets_closed() {
        let handle = Arc::new(FakeHandle {
            chunks: Mutex::new(vec![]), // 立即 EOF
        }) as Arc<dyn TerminalHandle>;

        let session = Session::new(
            "sess_close".into(),
            "h".into(),
            PtySize::default(),
            handle,
        );
        tokio::time::sleep(Duration::from_millis(50)).await;

        session.close().await.unwrap();
        assert_eq!(session.state(), SessionState::Closed);

        // close 后 read_output 返回 SessionClosed
        let err = session
            .read_output(ReadOutputParams::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_CLOSED");
    }

    // ── Phase 1：last_activity / is_idle 测试 ──────────────────────────

    #[tokio::test]
    async fn last_activity_initialized_to_now() {
        let handle = Arc::new(FakeHandle {
            chunks: Mutex::new(vec![]),
        }) as Arc<dyn TerminalHandle>;
        let before = now_secs();
        let session = Session::new("s1".into(), "h".into(), PtySize::default(), handle);
        let after = now_secs();

        let la = session.last_activity();
        assert!(la >= before && la <= after, "last_activity 应为当前时间");
    }

    #[tokio::test]
    async fn is_idle_true_when_last_activity_old() {
        let handle = Arc::new(FakeHandle {
            chunks: Mutex::new(vec![]),
        }) as Arc<dyn TerminalHandle>;
        let session = Session::new("s1".into(), "h".into(), PtySize::default(), handle);

        // 设为 100 秒前 → 超过 1 秒阈值 → idle
        session.set_last_activity_for_test(now_secs().saturating_sub(100));
        assert!(session.is_idle(1));
        assert!(session.is_idle(60));
        assert!(!session.is_idle(200));
    }

    #[tokio::test]
    async fn is_idle_false_when_recently_active() {
        let handle = Arc::new(FakeHandle {
            chunks: Mutex::new(vec![]),
        }) as Arc<dyn TerminalHandle>;
        let session = Session::new("s1".into(), "h".into(), PtySize::default(), handle);

        // 刚创建，last_activity 为当前 → 不 idle
        assert!(!session.is_idle(1800));
    }

    #[tokio::test]
    async fn send_input_updates_last_activity() {
        let handle = Arc::new(FakeHandle {
            chunks: Mutex::new(vec![]),
        }) as Arc<dyn TerminalHandle>;
        let session = Session::new("s1".into(), "h".into(), PtySize::default(), handle);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 设为很久以前
        session.set_last_activity_for_test(0);
        assert!(session.is_idle(1));

        // read task EOF → state Lost → send_input 返回 SessionClosed，不会 touch
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(session.state(), SessionState::Lost);

        // Lost 状态下 send_input 不更新 last_activity（is_usable 为 false，提前返回）
        let _ = session.send_input(b"ls\n").await;
        assert_eq!(session.last_activity(), 0);
    }

    // ── SessionSummary 字段变化测试（Phase 4-B） ───────────────────────

    /// 辅助：打印 SessionSummary 各字段。
    fn print_summary(label: &str, s: &SessionSummary) {
        println!("=== {label} ===");
        println!("  id:            {}", s.id);
        println!("  host:          {}", s.host);
        println!("  state:         {:?}", s.state);
        println!("  pty_size:      {}x{}", s.pty_size.rows, s.pty_size.cols);
        println!("  created_at:    {} (Unix ms)", s.created_at);
        println!("  last_activity: {} (Unix s)", s.last_activity);
        println!("  written:       {} bytes", s.written);
        println!("  name:          {:?}", s.name);
    }

    /// 验证 SessionSummary 在 session 生命周期各阶段的字段变化。
    ///
    /// 覆盖 8 个字段：
    /// - 不变字段：id / host / pty_size / created_at / name（恒 None）
    /// - 变化字段：state（Ready→Lost→Closed）/ written（read task 灌入增长）/ last_activity（read_output 更新）
    #[tokio::test]
    async fn session_summary_fields_across_lifecycle() {
        // FakeHandle 有 2 条数据（pop 顺序：chunk2 先，chunk1 后），然后 EOF → Lost
        let handle = Arc::new(FakeHandle {
            chunks: Mutex::new(vec![
                Bytes::from_static(b"chunk1\n"), // 7 bytes
                Bytes::from_static(b"chunk2\n"), // 7 bytes
            ]),
        }) as Arc<dyn TerminalHandle>;

        let pty = PtySize { rows: 24, cols: 80 };
        let session = Session::new(
            "sess_mock_001".into(),
            "192.0.2.171".into(),
            pty,
            handle,
        );

        // ── 阶段 1: 初始创建（read task 刚 spawn，可能还没写入） ──
        let s1 = session.summary();
        print_summary("阶段 1: 初始创建", &s1);

        assert_eq!(s1.id, "sess_mock_001");
        assert_eq!(s1.host, "192.0.2.171");
        assert_eq!(s1.state, SessionState::Ready);
        assert_eq!(s1.pty_size.rows, 24);
        assert_eq!(s1.pty_size.cols, 80);
        assert_eq!(s1.name, None);
        let created_at = s1.created_at;
        let initial_last_activity = s1.last_activity;

        // ── 阶段 2: read task 灌入数据后 ──
        // read task 把 chunk1 + chunk2 写入 buffer（共 16 bytes），然后 EOF → Lost
        tokio::time::sleep(Duration::from_millis(150)).await;
        let s2 = session.summary();
        print_summary("阶段 2: read task 灌入数据后", &s2);

        // written 应增长（read task 写入了 14 bytes: "chunk1\n" + "chunk2\n"）
        assert!(
            s2.written >= 14,
            "written 应 >= 14 bytes, 实际: {}",
            s2.written
        );
        assert!(s2.written >= s1.written, "written 应单调增长");
        // read task 不更新 last_activity
        assert_eq!(
            s2.last_activity, initial_last_activity,
            "read task 不应更新 last_activity"
        );
        // read task EOF 后 state 应为 Lost
        assert_eq!(s2.state, SessionState::Lost);

        // ── 阶段 3: read_output 更新 last_activity ──
        // Lost 状态下 read_output 仍允许（§4.6 契约 10：disconnect 不销毁 Session）
        let before_read = now_secs();
        let _ = session
            .read_output(ReadOutputParams {
                since_cursor: Some(0),
                max_bytes: Some(4096),
                ..Default::default()
            })
            .await
            .unwrap();
        // 确保 before_read <= last_activity（touch 在 read_output 内部）
        tokio::time::sleep(Duration::from_millis(10)).await;
        let s3 = session.summary();
        print_summary("阶段 3: read_output 后", &s3);

        // read_output 不改变 written（written 是 buffer 写入游标，只在 read task 写时增长）
        assert_eq!(s3.written, s2.written, "read_output 不应改变 written");
        // read_output 更新 last_activity
        assert!(
            s3.last_activity >= before_read,
            "read_output 应更新 last_activity, before={}, actual={}",
            before_read,
            s3.last_activity
        );

        // ── 阶段 4: close → Closed ──
        session.close().await.unwrap();
        let s4 = session.summary();
        print_summary("阶段 4: close 后", &s4);

        assert_eq!(s4.state, SessionState::Closed);
        // close 不改变 written / last_activity
        assert_eq!(s4.written, s3.written);
        assert_eq!(s4.last_activity, s3.last_activity);

        // ── 不变量校验：跨阶段不变的字段 ──
        println!("\n=== 不变量校验 ===");
        assert_eq!(s4.id, s1.id, "id 跨阶段不变");
        assert_eq!(s4.host, s1.host, "host 跨阶段不变");
        assert_eq!(s4.pty_size.rows, s1.pty_size.rows, "pty_size 跨阶段不变");
        assert_eq!(s4.pty_size.cols, s1.pty_size.cols, "pty_size 跨阶段不变");
        assert_eq!(s4.created_at, created_at, "created_at 跨阶段不变");
        assert_eq!(s4.name, None, "name 恒为 None");

        println!("\n=== SessionSummary 字段变化总结 ===");
        println!("  id:            不变 \"{}\"", s1.id);
        println!("  host:          不变 \"{}\"", s1.host);
        println!(
            "  state:         {:?} → {:?} → {:?} → {:?}",
            s1.state, s2.state, s3.state, s4.state
        );
        println!(
            "  pty_size:      不变 {}x{}",
            s1.pty_size.rows, s1.pty_size.cols
        );
        println!("  created_at:    不变 {} ms", created_at);
        println!(
            "  last_activity: {} → {} → {} → {}",
            s1.last_activity, s2.last_activity, s3.last_activity, s4.last_activity
        );
        println!(
            "  written:       {} → {} → {} → {}",
            s1.written, s2.written, s3.written, s4.written
        );
        println!("  name:          不变 {:?}", s1.name);
    }
}
