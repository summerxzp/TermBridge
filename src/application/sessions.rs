//! SessionManager —— 编排 domain Session + TerminalProvider（§4.3 / §5.4 / §6）
//!
//! 职责：
//! - `open_session(host)`：`ssh -G` 解析 → `provider.open()` → `Session::new()` → 存入 map
//! - `send_input` / `read_output` / `send_control` / `close_session`：委托给 Session
//! - `sftp_transfer`（Phase 1）：下转 handle 到 `SshTerminalHandle`，开 SFTP channel，路径策略检查，传输
//! - `list_sessions`：返回所有 Session 摘要
//!
//! Phase 0-C：`parking_lot::Mutex<HashMap>` 足够（并发量低）。
//! Phase 1：换 `DashMap`（无锁并发读写），新增 idleReaper + Lost session 清理。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::domain::output::{ReadOutputParams, ReadOutputResult};
use crate::domain::provider::{
    ControlKey, HostName, OpenTerminalRequest, PtySize, TerminalProvider, TermError,
    TransferDirection,
};
use crate::domain::session::{Session, SessionId, SessionSummary};
use crate::infrastructure::ssh::SshTerminalHandle;
use crate::infrastructure::sshconfig;
use crate::application::path_policy::PathPolicy;

// ───────────────────────────────────────────────────────────────────────────
// idleReaper 配置常量（§7.4 Phase 1）
// ───────────────────────────────────────────────────────────────────────────

/// idleReaper 扫描间隔（秒）。借鉴 pty-mcp session.go 的 30s tick。
pub const IDLE_REAPER_INTERVAL_SECS: u64 = 30;
/// Session 空闲超时（秒）。超过此时间无活动 → idleReaper 关闭并移除。
/// 默认 1800 秒（30 分钟），借鉴 pty-mcp。
pub const IDLE_TIMEOUT_SECS: u64 = 1800;

/// SessionManager：管理所有活跃 Session。
///
/// 持有 `Arc<dyn TerminalProvider>`（通常是 `SshProvider`），
/// open_session 时创建 Session，close_session 时移除。
///
/// Phase 1：
/// - `sessions` 换 `Arc<DashMap>`（无锁并发读写，适合多 session 并发）
/// - 新增 idleReaper task（每 30s 扫描，关闭空闲超 30min 的 session）
/// - send_input / read_output 检测到 Lost session 时从 map 移除（防泄漏）
pub struct SessionManager {
    /// DashMap 支持并发读写无锁。Arc 包裹让 idleReaper task 持有 clone。
    sessions: Arc<DashMap<SessionId, Arc<Session>>>,
    provider: Arc<dyn TerminalProvider>,
    counter: AtomicU64,
    /// SFTP 路径策略（ADR-0005 §4）。默认 cwd + 全放行。
    path_policy: PathPolicy,
    /// idleReaper task 句柄。Drop 时 abort 防泄漏。
    idle_reaper_task: Mutex<Option<JoinHandle<()>>>,
}

impl SessionManager {
    pub fn new(provider: Arc<dyn TerminalProvider>) -> Self {
        Self::build(provider, PathPolicy::default_from_cwd())
    }

    /// 用自定义路径策略构造（测试 / 配置覆盖用）。
    pub fn with_path_policy(
        provider: Arc<dyn TerminalProvider>,
        path_policy: PathPolicy,
    ) -> Self {
        Self::build(provider, path_policy)
    }

    fn build(provider: Arc<dyn TerminalProvider>, path_policy: PathPolicy) -> Self {
        let sessions = Arc::new(DashMap::new());

        // spawn idleReaper task：每 30s 扫描，关闭空闲超 30min 的 session（§7.4 Phase 1）
        let reaper_sessions = Arc::clone(&sessions);
        let idle_reaper_task = tokio::spawn(async move {
            idle_reaper_loop(reaper_sessions).await;
        });

        Self {
            sessions,
            provider,
            counter: AtomicU64::new(0),
            path_policy,
            idle_reaper_task: Mutex::new(Some(idle_reaper_task)),
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

        self.sessions.insert(id.clone(), session);
        tracing::info!(session = %id, host = %host.name, "open_session: ready");
        Ok(id)
    }

    /// 写输入到 PTY（§4.6 契约 7：立即返回，不等命令完成）。
    ///
    /// Phase 1：若返回 SessionClosed 且 session 已 Lost/Closed，从 map 移除防泄漏。
    pub async fn send_input(
        &self,
        session_id: &str,
        data: &[u8],
    ) -> Result<(), TermError> {
        let session = self.get_session(session_id)?;
        match session.send_input(data).await {
            Ok(()) => Ok(()),
            Err(e) if matches!(e, TermError::SessionClosed(_)) => {
                self.cleanup_detached_session(session_id, &session);
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// 读取输出（§5.3 三模式 + §4.6 契约 3/4/5/11）。
    ///
    /// Phase 1：若返回 SessionClosed 且 session 已 Closed，从 map 移除防泄漏。
    pub async fn read_output(
        &self,
        session_id: &str,
        params: ReadOutputParams,
    ) -> Result<ReadOutputResult, TermError> {
        let session = self.get_session(session_id)?;
        match session.read_output(params).await {
            Ok(r) => Ok(r),
            Err(e) if matches!(e, TermError::SessionClosed(_)) => {
                self.cleanup_detached_session(session_id, &session);
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// 发控制字符（§4.6 契约 8）。
    pub async fn send_control(
        &self,
        session_id: &str,
        key: ControlKey,
    ) -> Result<(), TermError> {
        let session = self.get_session(session_id)?;
        match session.send_control(key).await {
            Ok(()) => Ok(()),
            Err(e) if matches!(e, TermError::SessionClosed(_)) => {
                self.cleanup_detached_session(session_id, &session);
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// 关闭 Session（§4.6 契约 9：Session close 才结束远端 shell）。
    /// 幂等：已关闭的 session 直接移除并返回 Ok。
    pub async fn close_session(&self, session_id: &str) -> Result<(), TermError> {
        // DashMap::remove 返回 Option<(K, V)>，取 value
        let session = self
            .sessions
            .remove(session_id)
            .map(|(_, v)| v)
            .ok_or_else(|| TermError::SessionNotFound(session_id.to_string()))?;
        session.close().await
    }

    /// SFTP 文件传输（Phase 1，§7.4 / ADR-0005 §4 / §5）。
    ///
    /// 流程：
    /// 1. get session → 下转 handle 到 `SshTerminalHandle`（不支持 SFTP 则报错）。
    /// 2. 开 SFTP channel（独立于 PTY channel）→ `SftpProvider`。
    /// 3. PathPolicy::check_local + check_remote（realpath 防穿越）。
    /// 4. 按 direction 调用 upload / download（download 内部原子写）。
    /// 5. drop SftpProvider（best-effort close channel）。
    ///
    /// Phase 1 约束：每次调用开新 SFTP channel（不做池化）。
    pub async fn sftp_transfer(
        &self,
        session_id: &str,
        direction: TransferDirection,
        local: PathBuf,
        remote: String,
    ) -> Result<(), TermError> {
        let session = self.get_session(session_id)?;

        // 1. 下转 handle 到 SshTerminalHandle（FakeHandle 等非 SSH handle 不支持 SFTP）
        let handle = session.handle();
        let ssh_handle = handle
            .as_any()
            .downcast_ref::<SshTerminalHandle>()
            .ok_or_else(|| {
                TermError::InvalidArgument(format!(
                    "session '{session_id}' does not support SFTP (non-SSH handle)"
                ))
            })?;

        tracing::info!(
            session = %session_id,
            direction = direction.as_str(),
            local = ?local,
            remote = %remote,
            "sftp_transfer: starting"
        );

        // 2. 开 SFTP channel（复用 SSH session，新 channel 独立于 PTY）
        let sftp = ssh_handle.open_sftp_provider().await?;

        // 3. 路径策略检查
        //    check_remote 需要调远端 realpath，传 &sftp（实现 SftpCanonicalize）
        self.path_policy.check_local(local.as_path())?;
        self.path_policy.check_remote(&remote, &sftp).await?;

        // 4. 执行传输
        let result = match direction {
            TransferDirection::Upload => sftp.upload(local.as_path(), &remote).await,
            TransferDirection::Download => sftp.download(&remote, local.as_path()).await,
        };

        // 5. 显式关闭 SFTP channel（best-effort，失败不影响传输结果）
        let _ = sftp.close().await;

        result
    }

    /// 列出所有 Session 摘要。
    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        self.sessions.iter().map(|r| r.value().summary()).collect()
    }

    // ── 内部辅助 ──────────────────────────────────────────────────

    fn get_session(&self, id: &str) -> Result<Arc<Session>, TermError> {
        self.sessions
            .get(id)
            .map(|r| r.value().clone())
            .ok_or_else(|| TermError::SessionNotFound(id.to_string()))
    }

    fn next_session_id(&self) -> SessionId {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("sess_{n}")
    }

    /// 当 send_input / read_output / send_control 返回 SessionClosed 时，
    /// 若 session 状态为 Lost/Closed（is_detached），从 map 移除防泄漏。
    ///
    /// Phase 1 MVP：不自动重连（Phase 3 Persistent Session 再做），
    /// 仅记录 WARN 日志提示 agent 应通过 open_session 重新打开。
    fn cleanup_detached_session(&self, session_id: &str, session: &Arc<Session>) {
        if session.state().is_detached() {
            tracing::warn!(
                session = %session_id,
                state = ?session.state(),
                "session lost/closed, removing from map; agent should reopen via open_session"
            );
            self.sessions.remove(session_id);
        }
    }
}

impl Drop for SessionManager {
    fn drop(&mut self) {
        // 兜底：drop 时若 idleReaper task 还在，abort 掉防泄漏
        if let Some(task) = self.idle_reaper_task.lock().take() {
            task.abort();
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// idleReaper（§7.4 Phase 1）
// ───────────────────────────────────────────────────────────────────────────

/// idleReaper 循环：每 `IDLE_REAPER_INTERVAL_SECS` 秒扫描所有 session，
/// 关闭空闲超 `IDLE_TIMEOUT_SECS` 秒的 session。
///
/// 关键：**先释放锁再 Close**（借鉴 pty-mcp session.go 避免死锁）。
/// 实现：DashMap::iter 持读锁收集 idle id → 释放锁 → 逐个 remove（写锁瞬间）→ close（无锁）。
async fn idle_reaper_loop(sessions: Arc<DashMap<SessionId, Arc<Session>>>) {
    loop {
        tokio::time::sleep(Duration::from_secs(IDLE_REAPER_INTERVAL_SECS)).await;
        reap_idle(&sessions, IDLE_TIMEOUT_SECS).await;
    }
}

/// 扫描并关闭空闲超时的 session（可单独调用供测试）。
///
/// 流程（"先释放锁再 Close"）：
/// 1. iter 收集 idle session id（持读锁，仅克隆 id）
/// 2. 释放读锁
/// 3. 对每个 id：再次检查 is_idle（防止收集期间变活跃）→ remove（写锁瞬间）→ close（无锁）
pub async fn reap_idle(sessions: &DashMap<SessionId, Arc<Session>>, idle_timeout_secs: u64) {
    // 1. 收集 idle session id（读锁在 Ref 生命周期内持有，collect 后释放）
    let idle_ids: Vec<SessionId> = sessions
        .iter()
        .filter(|entry| entry.value().is_idle(idle_timeout_secs))
        .map(|entry| entry.key().clone())
        .collect();

    // 2. 逐个 remove + close（不持读锁，避免死锁）
    for id in idle_ids {
        // 再次检查（可能在收集期间变活跃了）
        let to_close = sessions
            .get(&id)
            .map_or(false, |s| s.is_idle(idle_timeout_secs));
        if !to_close {
            continue;
        }
        // remove 取出 Arc clone（写锁瞬间释放），再无锁调 close
        if let Some((_, session)) = sessions.remove(&id) {
            tracing::info!(session = %id, "idle_reaper: 关闭空闲超时 session");
            let _ = session.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::provider::{OpenTerminalRequest, TerminalHandle};
    use crate::domain::session::SessionState;
    use async_trait::async_trait;
    use bytes::Bytes;
    use parking_lot::Mutex as PLMutex;
    use std::time::{SystemTime, UNIX_EPOCH};

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
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// 当前 Unix 时间戳（秒）。
    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
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

    /// Phase 1：sftp_transfer 在 session 不存在时返回 SESSION_NOT_FOUND。
    #[tokio::test]
    async fn sftp_transfer_session_not_found() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr
            .sftp_transfer(
                "nonexistent",
                TransferDirection::Upload,
                PathBuf::from("/tmp/local.txt"),
                "/tmp/remote.txt".to_string(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
    }

    // ── Phase 1：idleReaper 配置常量测试 ────────────────────────────

    #[test]
    fn idle_reaper_constants_have_expected_values() {
        // §7.4 Phase 1：30s tick + 1800s（30 分钟）idle timeout
        assert_eq!(IDLE_REAPER_INTERVAL_SECS, 30, "idleReaper 间隔应为 30 秒");
        assert_eq!(IDLE_TIMEOUT_SECS, 1800, "idle 超时应为 1800 秒（30 分钟）");
    }

    // ── Phase 1：idleReaper 逻辑测试 ────────────────────────────────

    /// 构造一个 FakeHandle（立即 EOF → Session 会变 Lost，但不影响 reaper 测试）。
    fn make_fake_session(id: &str) -> Arc<Session> {
        let handle = Arc::new(FakeHandle {
            chunks: PLMutex::new(vec![]), // 立即 EOF
        }) as Arc<dyn TerminalHandle>;
        Arc::new(Session::new(
            id.into(),
            "testhost".into(),
            PtySize::default(),
            handle,
        ))
    }

    #[tokio::test]
    async fn reap_idle_removes_idle_session() {
        let sessions: DashMap<SessionId, Arc<Session>> = DashMap::new();

        // 插入一个 session，设 last_activity 为 100 秒前
        let s = make_fake_session("sess_idle");
        s.set_last_activity_for_test(now_secs().saturating_sub(100));
        sessions.insert("sess_idle".into(), s);

        // 插入一个活跃 session
        let s2 = make_fake_session("sess_active");
        // s2 的 last_activity 默认为当前时间
        sessions.insert("sess_active".into(), s2);

        // 用 1 秒阈值 reap：sess_idle 应被关闭，sess_active 保留
        reap_idle(&sessions, 1).await;

        assert!(sessions.is_empty() == false, "应至少保留活跃 session");
        assert!(
            sessions.contains_key("sess_active"),
            "活跃 session 应保留"
        );
        assert!(
            !sessions.contains_key("sess_idle"),
            "空闲 session 应被 reaper 移除"
        );
    }

    #[tokio::test]
    async fn reap_idle_skips_active_session() {
        let sessions: DashMap<SessionId, Arc<Session>> = DashMap::new();
        let s = make_fake_session("sess_fresh");
        sessions.insert("sess_fresh".into(), s);

        // 刚创建的 session 不应被 reap（即使 1 秒阈值也需 last_activity 在 1 秒前）
        // 为确保不误杀，先 sleep 50ms 再 reap with 100s threshold
        tokio::time::sleep(Duration::from_millis(50)).await;
        reap_idle(&sessions, 100).await;

        assert!(
            sessions.contains_key("sess_fresh"),
            "活跃 session 不应被 reap"
        );
    }

    #[tokio::test]
    async fn reap_idle_empty_map_noop() {
        let sessions: DashMap<SessionId, Arc<Session>> = DashMap::new();
        // 空 map，reap 不应 panic
        reap_idle(&sessions, 1).await;
        assert!(sessions.is_empty());
    }

    // ── Phase 1：DashMap 并发测试 ───────────────────────────────────

    #[tokio::test]
    async fn dashmap_concurrent_insert_remove_no_deadlock() {
        // 多线程并发 insert + remove，验证 DashMap 无死锁
        let sessions: Arc<DashMap<SessionId, Arc<Session>>> = Arc::new(DashMap::new());

        let mut handles = Vec::new();
        for t in 0..4 {
            let s = Arc::clone(&sessions);
            handles.push(tokio::spawn(async move {
                for i in 0..20 {
                    let id = format!("sess_t{t}_i{i}");
                    let session = make_fake_session(&id);
                    s.insert(id.clone(), session);
                    // 立即移除部分
                    if i % 2 == 0 {
                        s.remove(&id);
                    }
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // 不死锁即通过；奇数 i 的 session 保留（每个线程 10 个）
        let remaining = sessions.len();
        assert_eq!(remaining, 40, "每线程 10 个奇数 i session 保留，4 线程共 40");
    }

    #[tokio::test]
    async fn dashmap_concurrent_read_write_no_deadlock() {
        // 并发读（iter / get）+ 写（insert / remove），验证 DashMap 无死锁
        let sessions: Arc<DashMap<SessionId, Arc<Session>>> = Arc::new(DashMap::new());

        // 预填充
        for i in 0..10 {
            let id = format!("sess_init_{i}");
            sessions.insert(id, make_fake_session(&format!("init{i}")));
        }

        let reader = Arc::clone(&sessions);
        let writer = Arc::clone(&sessions);

        let r = tokio::spawn(async move {
            for _ in 0..100 {
                let _ = reader.iter().count();
                let _ = reader.get("sess_init_0");
            }
        });
        let w = tokio::spawn(async move {
            for i in 0..50 {
                let id = format!("sess_w_{i}");
                writer.insert(id.clone(), make_fake_session(&id));
                writer.remove(&id);
            }
        });

        // 若有死锁，这里会超时
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            r.await.unwrap();
            w.await.unwrap();
        })
        .await
        .expect("并发读写不应死锁");
    }

    // ── Phase 1：Lost session 清理测试 ──────────────────────────────

    #[tokio::test]
    async fn cleanup_detached_session_removes_lost_from_map() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);

        // 手动构造一个 Lost session 插入 map（FakeHandle 立即 EOF → read task 退出 → Lost）
        let session = make_fake_session("sess_lost");
        mgr.sessions.insert("sess_lost".into(), session.clone());

        // 等 read task EOF → state Lost
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(session.state(), SessionState::Lost);

        // send_input 应返回 SessionClosed，且从 map 移除
        let err = mgr.send_input("sess_lost", b"ls\n").await.unwrap_err();
        assert_eq!(err.code(), "SESSION_CLOSED");

        // map 中应已移除
        assert!(
            !mgr.sessions.contains_key("sess_lost"),
            "Lost session 应从 map 移除"
        );
    }
}
