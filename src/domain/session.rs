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

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use super::output::{OutputEngine, ReadOutputParams, ReadOutputResult, DEFAULT_BUFFER_SIZE};
use super::provider::{ControlKey, HostName, PtySize, TerminalHandle, TermError};

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
pub struct Session {
    id: SessionId,
    host: HostName,
    state: Arc<Mutex<SessionState>>,
    pty_size: PtySize,
    output: OutputEngine,
    handle: Arc<dyn TerminalHandle>,
    read_task: Mutex<Option<JoinHandle<()>>>,
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

        // spawn PTY read task：handle.read() → output.buffer().write()
        let read_handle = Arc::clone(&handle);
        let read_state = Arc::clone(&state);
        let read_id = id.clone();
        let read_task = tokio::spawn(async move {
            loop {
                match read_handle.read().await {
                    Ok(Some(data)) => buffer.write(&data),
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
        Ok(self.output.read_output(params).await)
    }

    /// 写输入到 PTY（§4.6 契约 7：立即返回，不等命令完成）。
    /// 仅 `Ready` 允许写；Lost/Closed 拒绝（连接已断）。
    pub async fn send_input(&self, data: &[u8]) -> Result<(), TermError> {
        if !self.state().is_usable() {
            return Err(TermError::SessionClosed(self.id.clone()));
        }
        self.handle.write(data).await
    }

    /// 发控制字符（§4.6 契约 8）。仅 `Ready` 允许。
    pub async fn send_control(&self, key: ControlKey) -> Result<(), TermError> {
        if !self.state().is_usable() {
            return Err(TermError::SessionClosed(self.id.clone()));
        }
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
}

impl Session {
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            host: self.host.clone(),
            state: self.state(),
            pty_size: self.pty_size,
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
}
