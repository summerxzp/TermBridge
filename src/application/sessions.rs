//! SessionManager —— 编排 domain Session + TerminalProvider（§4.3 / §5.4 / §6）
//!
//! 职责：
//! - `open_session(host)`：`ssh -G` 解析 → `provider.open()` → `Session::new()` → 存入 map
//! - `send_input` / `read_output` / `send_control` / `close_session`：委托给 Session
//! - `list_sessions`：返回所有 Session 摘要
//!
//! Phase 0-C：`parking_lot::Mutex<HashMap>` 足够（并发量低）。Phase 1 换 DashMap。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::domain::output::{ReadOutputParams, ReadOutputResult};
use crate::domain::provider::{
    ControlKey, HostName, OpenTerminalRequest, PtySize, TerminalProvider, TermError,
};
use crate::domain::session::{Session, SessionId, SessionSummary};
use crate::infrastructure::sshconfig;

/// SessionManager：管理所有活跃 Session。
///
/// 持有 `Arc<dyn TerminalProvider>`（通常是 `SshProvider`），
/// open_session 时创建 Session，close_session 时移除。
pub struct SessionManager {
    sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
    provider: Arc<dyn TerminalProvider>,
    counter: AtomicU64,
}

impl SessionManager {
    pub fn new(provider: Arc<dyn TerminalProvider>) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            provider,
            counter: AtomicU64::new(0),
        }
    }

    /// 打开一个新 Session（§6 open_session 工具的后端）。
    ///
    /// 流程：`ssh -G <alias>` 解析 → `provider.open()` → `Session::new()` → 存入 map。
    /// 返回 session_id 供后续操作引用。
    pub async fn open_session(
        &self,
        host_alias: &HostName,
        pty_size: Option<PtySize>,
    ) -> Result<SessionId, TermError> {
        tracing::info!(host = %host_alias, "open_session: resolving ssh config");

        // 1. ssh -G 解析（ADR-0006：复用 OpenSSH 完整 config 解析）
        let host = sshconfig::resolve(host_alias).await?;
        tracing::info!(
            host = %host.name,
            hostname = %host.hostname,
            port = host.port,
            user = %host.user,
            "open_session: resolved, connecting"
        );

        // 2. Provider 创建 Terminal Backend（SSH connect + auth + PTY + shell）
        let pty_size = pty_size.unwrap_or_default();
        let handle = self
            .provider
            .open(OpenTerminalRequest {
                host: host.clone(),
                pty_size,
            })
            .await?;

        // 3. 创建 Session（内部 spawn PTY read task）
        let id = self.next_session_id();
        let session = Arc::new(Session::new(
            id.clone(),
            host.name.clone(),
            pty_size,
            handle,
        ));

        self.sessions.lock().insert(id.clone(), session);
        tracing::info!(session = %id, host = %host.name, "open_session: ready");
        Ok(id)
    }

    /// 写输入到 PTY（§4.6 契约 7：立即返回，不等命令完成）。
    pub async fn send_input(
        &self,
        session_id: &str,
        data: &[u8],
    ) -> Result<(), TermError> {
        let session = self.get_session(session_id)?;
        session.send_input(data).await
    }

    /// 读取输出（§5.3 三模式 + §4.6 契约 3/4/5/11）。
    pub async fn read_output(
        &self,
        session_id: &str,
        params: ReadOutputParams,
    ) -> Result<ReadOutputResult, TermError> {
        let session = self.get_session(session_id)?;
        session.read_output(params).await
    }

    /// 发控制字符（§4.6 契约 8）。
    pub async fn send_control(
        &self,
        session_id: &str,
        key: ControlKey,
    ) -> Result<(), TermError> {
        let session = self.get_session(session_id)?;
        session.send_control(key).await
    }

    /// 关闭 Session（§4.6 契约 9：Session close 才结束远端 shell）。
    /// 幂等：已关闭的 session 直接移除并返回 Ok。
    pub async fn close_session(&self, session_id: &str) -> Result<(), TermError> {
        let session = self.sessions.lock().remove(session_id).ok_or_else(|| {
            TermError::SessionNotFound(session_id.to_string())
        })?;
        session.close().await
    }

    /// 列出所有 Session 摘要。
    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        self.sessions
            .lock()
            .values()
            .map(|s| s.summary())
            .collect()
    }

    // ── 内部辅助 ──────────────────────────────────────────────────

    fn get_session(&self, id: &str) -> Result<Arc<Session>, TermError> {
        self.sessions
            .lock()
            .get(id)
            .cloned()
            .ok_or_else(|| TermError::SessionNotFound(id.to_string()))
    }

    fn next_session_id(&self) -> SessionId {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("sess_{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provider::{OpenTerminalRequest, TerminalHandle};
    use async_trait::async_trait;
    use bytes::Bytes;
    use parking_lot::Mutex as PLMutex;

    /// 假 Provider：open 返回 FakeHandle（不真正连接 SSH）
    struct FakeProvider;

    #[async_trait]
    impl TerminalProvider for FakeProvider {
        async fn open(
            &self,
            request: OpenTerminalRequest,
        ) -> Result<Arc<dyn TerminalHandle>, TermError> {
            let host = &request.host;
            tracing::info!(host = %host.name, "FakeProvider: open");
            Ok(Arc::new(FakeHandle {
                chunks: PLMutex::new(vec![Bytes::from_static(b"$ ")]),
            }) as Arc<dyn TerminalHandle>)
        }
    }

    struct FakeHandle {
        chunks: PLMutex<Vec<Bytes>>,
    }

    #[async_trait]
    impl TerminalHandle for FakeHandle {
        async fn read(&self) -> Result<Option<Bytes>, TermError> {
            Ok(self.chunks.lock().pop())
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

    // 注意：open_session 依赖 sshconfig::resolve（调 `ssh -G` 子进程），
    // 这需要真实 ssh 环境。这里只测不依赖 SSH 的逻辑。

    #[tokio::test]
    async fn session_not_found_errors() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr
            .send_input("nonexistent", b"ls\n")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");

        let err = mgr
            .read_output("nonexistent", ReadOutputParams::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");

        let err = mgr
            .close_session("nonexistent")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn list_sessions_empty() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        assert!(mgr.list_sessions().is_empty());
    }

    #[tokio::test]
    async fn next_session_id_is_unique() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let id1 = mgr.next_session_id();
        let id2 = mgr.next_session_id();
        assert_ne!(id1, id2);
        assert!(id1.starts_with("sess_"));
        assert!(id2.starts_with("sess_"));
    }
}
