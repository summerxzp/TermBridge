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
use crate::domain::policy::{Action, Decision};
use crate::domain::provider::{
    ControlKey, HostName, OpenTerminalRequest, PtySize, TerminalProvider, TermError,
    TransferDirection,
};
use crate::domain::session::{Session, SessionId, SessionSummary};
use crate::domain::timeline::TimelineEvent;
use crate::infrastructure::daemon_proto::SessionInfo;
use crate::infrastructure::persistent::{PersistentProvider, PersistentTerminalHandle};
use crate::infrastructure::ssh::SshTerminalHandle;
use crate::infrastructure::sshconfig;
use crate::application::path_policy::PathPolicy;
use crate::application::policy::PolicyManager;

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
    /// 危险动作拦截策略链（PLAN.md §8，Phase 2）。默认 [DefaultPolicy]。
    policy: Arc<PolicyManager>,
    /// idleReaper task 句柄。Drop 时 abort 防泄漏。
    idle_reaper_task: Mutex<Option<JoinHandle<()>>>,
}

impl SessionManager {
    pub fn new(provider: Arc<dyn TerminalProvider>) -> Self {
        Self::build(
            provider,
            PathPolicy::default_from_cwd(),
            Arc::new(PolicyManager::with_default()),
        )
    }

    /// 用自定义路径策略构造（测试 / 配置覆盖用）。
    pub fn with_path_policy(
        provider: Arc<dyn TerminalProvider>,
        path_policy: PathPolicy,
    ) -> Self {
        Self::build(
            provider,
            path_policy,
            Arc::new(PolicyManager::with_default()),
        )
    }

    /// 用自定义路径策略 + Policy 链构造（Phase 2，测试 / 配置覆盖用）。
    pub fn with_policy(
        provider: Arc<dyn TerminalProvider>,
        path_policy: PathPolicy,
        policy: Arc<PolicyManager>,
    ) -> Self {
        Self::build(provider, path_policy, policy)
    }

    fn build(
        provider: Arc<dyn TerminalProvider>,
        path_policy: PathPolicy,
        policy: Arc<PolicyManager>,
    ) -> Self {
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
            policy,
            idle_reaper_task: Mutex::new(Some(idle_reaper_task)),
        }
    }

    /// 打开一个新 Session（§6 open_session 工具的后端）。
    ///
    /// 流程：`ssh -G <alias>` 解析 → `provider.open()` → `Session::new()` → 存入 map。
    /// 返回 session_id 供后续操作引用。
    ///
    /// - `persistent=false`：Interactive Session（Phase 1/2 路径，SSH 直连 PTY）
    /// - `persistent=true`：Persistent Session（Phase 3 路径，远端 daemon 托管 PTY，ADR-0004）
    ///   `name` 仅 persistent 模式有效，作为远端 session 可读标签
    pub async fn open_session(
        &self,
        host_alias: &HostName,
        pty_size: Option<PtySize>,
        persistent: bool,
        name: Option<String>,
    ) -> Result<SessionId, TermError> {
        tracing::info!(host = %host_alias, persistent, "open_session: resolving ssh config");

        // 1. ssh -G 解析（ADR-0006：复用 OpenSSH 完整 config 解析）
        let host = sshconfig::resolve(host_alias).await?;
        tracing::info!(
            host = %host.name,
            hostname = %host.hostname,
            port = host.port,
            user = %host.user,
            persistent,
            "open_session: resolved, connecting"
        );

        // 2. Provider 创建 Terminal Backend
        //    persistent=false → SshProvider 路径（SSH connect + auth + PTY + shell）
        //    persistent=true  → PersistentProvider 路径（check/deploy/bootstrap daemon + session.create）
        let pty_size = pty_size.unwrap_or_default();
        let handle = self
            .provider
            .open(OpenTerminalRequest {
                host: host.clone(),
                pty_size,
                persistent,
                name,
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
    /// Phase 2：调用 Policy 拦截危险命令（PLAN.md §8）。
    /// - Allow → 继续发送
    /// - Deny → 返回 `TermError::PolicyDenied`
    /// - Confirm → 返回 `TermError::PolicyNeedsConfirm`（Phase 2 MVP 等同 Deny）
    ///
    /// Policy 检查在 session 查找前——拒绝危险命令不泄漏 session 存在性。
    ///
    /// Phase 1：若返回 SessionClosed 且 session 已 Lost/Closed，从 map 移除防泄漏。
    pub async fn send_input(
        &self,
        session_id: &str,
        data: &[u8],
    ) -> Result<(), TermError> {
        // Phase 2：Policy 拦截（PLAN.md §8）
        // 字节按 UTF-8 lossy 转字符串供 Policy 文本检查（best-effort）
        let data_str = String::from_utf8_lossy(data).into_owned();
        self.check_policy(&Action::SendInput {
            session_id: session_id.to_string(),
            data: data_str,
        })?;

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
        // Phase 2：Policy 拦截（PLAN.md §8）
        self.check_policy(&Action::SftpTransfer {
            direction,
            local: local.to_string_lossy().into_owned(),
            remote: remote.clone(),
        })?;

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

    // ── Phase 2：SFTP 目录 / 权限 / 列表 / 删除 ──────────────────

    /// SFTP 创建远端目录（Phase 2）。
    ///
    /// 流程与 sftp_transfer 一致：get session → 开 SFTP channel → 路径策略校验
    /// → mkdir → close。
    /// mkdir 的 remote_path 可能尚不存在，用 `check_remote_allow_new` 校验
    /// （先尝试 realpath，失败则校验父目录）。
    pub async fn sftp_mkdir(
        &self,
        session_id: &str,
        remote: String,
        mode: u32,
    ) -> Result<(), TermError> {
        let sftp = self.open_sftp_for_session(session_id).await?;
        tracing::info!(
            session = %session_id,
            remote = %remote,
            mode = format!("{mode:o}"),
            "sftp_mkdir: starting"
        );

        // 路径策略校验：mkdir 目标可能不存在，用 allow_new 变体
        let result = async {
            self.path_policy.check_remote_allow_new(&remote, &sftp).await?;
            sftp.mkdir(&remote, mode).await
        }
        .await;

        let _ = sftp.close().await;
        result
    }

    /// SFTP 列远端目录（Phase 2）。
    ///
    /// 返回 `Vec<RemoteEntry>`。remote_path 必须存在（check_remote）。
    pub async fn sftp_list(
        &self,
        session_id: &str,
        remote: String,
    ) -> Result<Vec<crate::infrastructure::sftp::RemoteEntry>, TermError> {
        let sftp = self.open_sftp_for_session(session_id).await?;
        tracing::info!(
            session = %session_id,
            remote = %remote,
            "sftp_list: starting"
        );

        let result = async {
            self.path_policy.check_remote(&remote, &sftp).await?;
            sftp.list_dir(&remote).await
        }
        .await;

        let _ = sftp.close().await;
        result
    }

    /// SFTP 删除远端文件/目录（Phase 2）。
    ///
    /// - `recursive=false`：删除文件（用 remove_file）；若目标是目录会失败。
    /// - `recursive=true`：删除目录树（递归 list_dir → remove 子项 → rmdir）。
    ///
    /// Phase 2：Policy 拦截（PLAN.md §8）——递归删除系统目录 → Deny，
    /// 其他递归/非递归删除 → Confirm。
    pub async fn sftp_remove(
        &self,
        session_id: &str,
        remote: String,
        recursive: bool,
    ) -> Result<(), TermError> {
        // Phase 2：Policy 拦截（PLAN.md §8）
        self.check_policy(&Action::SftpRemove {
            remote: remote.clone(),
            recursive,
        })?;

        let sftp = self.open_sftp_for_session(session_id).await?;
        tracing::info!(
            session = %session_id,
            remote = %remote,
            recursive,
            "sftp_remove: starting"
        );

        let result = async {
            self.path_policy.check_remote(&remote, &sftp).await?;
            if recursive {
                sftp_remove_recursive(&sftp, &remote, 0).await
            } else {
                sftp.remove(&remote).await
            }
        }
        .await;

        let _ = sftp.close().await;
        result
    }

    /// SFTP 修改远端文件/目录权限（Phase 2）。
    ///
    /// `mode` 为 POSIX 权限位（如 `0o755`）。
    ///
    /// Phase 2：Policy 拦截（PLAN.md §8）——chmod 777 系统目录 → Confirm。
    pub async fn sftp_chmod(
        &self,
        session_id: &str,
        remote: String,
        mode: u32,
    ) -> Result<(), TermError> {
        // Phase 2：Policy 拦截（PLAN.md §8）
        self.check_policy(&Action::SftpChmod {
            remote: remote.clone(),
            mode: format!("{mode:o}"),
        })?;

        let sftp = self.open_sftp_for_session(session_id).await?;
        tracing::info!(
            session = %session_id,
            remote = %remote,
            mode = format!("{mode:o}"),
            "sftp_chmod: starting"
        );

        let result = async {
            self.path_policy.check_remote(&remote, &sftp).await?;
            sftp.chmod(&remote, mode).await
        }
        .await;

        let _ = sftp.close().await;
        result
    }

    /// 列出所有 Session 摘要。
    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        self.sessions.iter().map(|r| r.value().summary()).collect()
    }

    /// 返回 session 的 timeline 事件（最近 limit 条，Phase 4-A）。
    ///
    /// 用于排障和 AI 上下文：结构化展示"发了什么命令→返回什么输出→发了什么控制键"。
    /// `limit=None` 返回全部（受 timeline 环形淘汰上限约束，默认 1000 条）。
    pub fn get_session_timeline(
        &self,
        session_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<TimelineEvent>, TermError> {
        let session = self.get_session(session_id)?;
        Ok(session.timeline().events(limit))
    }

    // ── Phase 3-B：跨 MCP 重启重连 ────────────────────────────────

    /// 列出远端 daemon 上的所有 session（含 detached 的）。
    ///
    /// 用于跨 MCP 重启后发现之前创建的 persistent session。
    /// 需要底层 provider 为 PersistentProvider。
    pub async fn list_remote_sessions(
        &self,
        host_alias: &HostName,
    ) -> Result<Vec<SessionInfo>, TermError> {
        let host = sshconfig::resolve(host_alias).await?;
        let provider = self.provider.as_any().downcast_ref::<PersistentProvider>()
            .ok_or_else(|| TermError::InvalidArgument(
                "list_remote_sessions requires persistent provider".into()
            ))?;
        provider.list_remote_sessions(&host).await
    }

    /// attach 到远端已有 session，返回新的 client session_id。
    ///
    /// 用于跨 MCP 重启后重连到之前创建（可能已 detached）的 persistent session。
    /// 需要底层 provider 为 PersistentProvider。
    pub async fn attach_remote_session(
        &self,
        host_alias: &HostName,
        remote_session_id: &str,
        _name: Option<String>,
    ) -> Result<SessionId, TermError> {
        let host = sshconfig::resolve(host_alias).await?;
        let provider = self.provider.as_any().downcast_ref::<PersistentProvider>()
            .ok_or_else(|| TermError::InvalidArgument(
                "attach_remote_session requires persistent provider".into()
            ))?;

        let handle = provider.attach_remote_session(&host, remote_session_id).await?;

        let id = self.next_session_id();
        let session = Arc::new(Session::new(
            id.clone(),
            host.name.clone(),
            PtySize::default(),
            handle,
        ));
        self.sessions.insert(id.clone(), session);
        tracing::info!(
            session = %id,
            remote_session_id,
            host = %host.name,
            "attach_remote_session: ready"
        );
        Ok(id)
    }

    /// detach session（远端 PTY 保活，本地释放连接，供后续 attach 重连）。
    ///
    /// 仅 persistent session 支持 detach；非 persistent handle 返回 InvalidArgument。
    /// 调用后 session 从本地 map 移除，远端 session 转 Detached。
    pub async fn detach_session(&self, session_id: &str) -> Result<(), TermError> {
        let session = self
            .sessions
            .remove(session_id)
            .map(|(_, v)| v)
            .ok_or_else(|| TermError::SessionNotFound(session_id.to_string()))?;

        let handle = session.handle();
        let persistent = handle
            .as_any()
            .downcast_ref::<PersistentTerminalHandle>()
            .ok_or_else(|| TermError::InvalidArgument(
                "session does not support detach (non-persistent handle)".into()
            ))?;

        persistent.detach().await?;
        tracing::info!(session = %session_id, "detach_session: detached, remote PTY kept alive");
        // session drop 时 read_task 被 abort，handle drop 时 daemon 连接关闭
        Ok(())
    }

    // ── 内部辅助 ──────────────────────────────────────────────────

    /// 打开 SFTP channel 并返回 SftpProvider（Phase 2 提取的公共逻辑）。
    ///
    /// 流程：get session → 下转 SshTerminalHandle → open_sftp_provider。
    /// 调用方负责在操作完成后 `sftp.close()`。
    async fn open_sftp_for_session(
        &self,
        session_id: &str,
    ) -> Result<crate::infrastructure::sftp::SftpProvider, TermError> {
        let session = self.get_session(session_id)?;
        let handle = session.handle();
        let ssh_handle = handle
            .as_any()
            .downcast_ref::<SshTerminalHandle>()
            .ok_or_else(|| {
                TermError::InvalidArgument(format!(
                    "session '{session_id}' does not support SFTP (non-SSH handle)"
                ))
            })?;
        ssh_handle.open_sftp_provider().await
    }

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

    /// Phase 2：调用 Policy 链拦截动作（PLAN.md §8）。
    ///
    /// - Allow → Ok(())
    /// - Deny → Err(PolicyDenied)
    /// - Confirm → Err(PolicyNeedsConfirm)（Phase 2 MVP 等同 Deny）
    fn check_policy(&self, action: &Action) -> Result<(), TermError> {
        let decision = self.policy.authorize(action);
        match decision {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(TermError::PolicyDenied(policy_reason(action, "blocked"))),
            Decision::Confirm => Err(TermError::PolicyNeedsConfirm(policy_reason(
                action,
                "needs confirmation",
            ))),
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

// ───────────────────────────────────────────────────────────────────────────
// Phase 2：递归删除目录树 + Policy reason 辅助
// ───────────────────────────────────────────────────────────────────────────

/// 递归删除远端目录树（Phase 2，sftp_remove recursive=true 用）。
///
/// 流程：list_dir → 对每个条目：目录则递归，文件则 remove → 最后 rmdir 空目录。
/// 深度限制 `MAX_DEPTH` 防止 symlink 环导致无限递归。
async fn sftp_remove_recursive(
    sftp: &crate::infrastructure::sftp::SftpProvider,
    remote: &str,
    depth: usize,
) -> Result<(), TermError> {
    const MAX_DEPTH: usize = 20;
    if depth > MAX_DEPTH {
        return Err(TermError::InvalidArgument(format!(
            "sftp recursive remove exceeded max depth {MAX_DEPTH} at '{remote}' \
             (possible symlink loop)"
        )));
    }

    let entries = sftp.list_dir(remote).await?;
    for entry in entries {
        let child_path = if remote.ends_with('/') {
            format!("{}{}", remote, entry.name)
        } else {
            format!("{}/{}", remote, entry.name)
        };
        if entry.is_dir {
            Box::pin(sftp_remove_recursive(sftp, &child_path, depth + 1)).await?;
        } else {
            sftp.remove(&child_path).await?;
        }
    }
    sftp.rmdir(remote).await
}

/// 从 Action + 状态描述构造 Policy 拒绝/需确认的原因字符串。
fn policy_reason(action: &Action, status: &str) -> String {
    match action {
        Action::SendInput { data, .. } => {
            let snippet = data
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            let snippet: String = if snippet.chars().count() > 80 {
                format!("{}...", snippet.chars().take(77).collect::<String>())
            } else {
                snippet.to_string()
            };
            format!("command {status}: {snippet}")
        }
        Action::SftpTransfer { direction, remote, .. } => {
            format!("sftp {} {status}: {remote}", direction.as_str())
        }
        Action::SftpRemove { remote, recursive } => {
            format!(
                "sftp remove{} {status}: {remote}",
                if *recursive { " (recursive)" } else { "" }
            )
        }
        Action::SftpChmod { remote, mode } => {
            format!("sftp chmod {mode} {status}: {remote}")
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

        fn as_any(&self) -> &dyn std::any::Any {
            self
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

    // ── Phase 2：Policy 集成测试 ─────────────────────────────────────

    #[tokio::test]
    async fn send_input_dangerous_command_returns_policy_denied() {
        // rm -rf / 命中 blocklist → POLICY_DENIED
        // Policy 检查在 session 查找前，无需真实 session
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr.send_input("any_session", b"rm -rf /\n").await.unwrap_err();
        assert_eq!(err.code(), "POLICY_DENIED");
        assert!(!err.retriable());
    }

    #[tokio::test]
    async fn send_input_mkfs_returns_policy_denied() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr
            .send_input("any", b"mkfs.ext4 /dev/sda1\n")
            .await
            .unwrap_err();
        assert_eq!(err.code(), "POLICY_DENIED");
    }

    #[tokio::test]
    async fn send_input_sudo_returns_policy_needs_confirm() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr.send_input("any", b"sudo apt update\n").await.unwrap_err();
        assert_eq!(err.code(), "POLICY_NEEDS_CONFIRM");
    }

    #[tokio::test]
    async fn send_input_normal_command_to_nonexistent_returns_session_not_found() {
        // Policy Allow → 继续查找 session → SESSION_NOT_FOUND
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr.send_input("nonexistent", b"ls\n").await.unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn send_input_multiline_dangerous_denies_all() {
        // 多行输入，第二行命中 blocklist → POLICY_DENIED
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let input = b"ls -la\nrm -rf /\necho done\n";
        let err = mgr.send_input("any", input).await.unwrap_err();
        assert_eq!(err.code(), "POLICY_DENIED");
    }

    #[tokio::test]
    async fn sftp_transfer_to_dev_returns_policy_needs_confirm() {
        // upload 到 /dev/ → Confirm → POLICY_NEEDS_CONFIRM
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr
            .sftp_transfer(
                "any",
                TransferDirection::Upload,
                PathBuf::from("/tmp/file"),
                "/dev/sda".to_string(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "POLICY_NEEDS_CONFIRM");
    }

    #[tokio::test]
    async fn sftp_remove_recursive_system_dir_returns_policy_denied() {
        // 递归删除 /etc → Deny → POLICY_DENIED
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr
            .sftp_remove("any", "/etc".to_string(), true)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "POLICY_DENIED");
    }

    #[tokio::test]
    async fn sftp_remove_non_recursive_returns_policy_needs_confirm() {
        // 非递归删除 → Confirm → POLICY_NEEDS_CONFIRM
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr
            .sftp_remove("any", "/tmp/file".to_string(), false)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "POLICY_NEEDS_CONFIRM");
    }

    #[tokio::test]
    async fn sftp_chmod_777_system_dir_returns_policy_needs_confirm() {
        // chmod 777 /etc → Confirm → POLICY_NEEDS_CONFIRM
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr
            .sftp_chmod("any", "/etc/passwd".to_string(), 0o777)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "POLICY_NEEDS_CONFIRM");
    }

    #[tokio::test]
    async fn sftp_chmod_normal_returns_session_not_found() {
        // chmod 644 普通路径 → Policy Allow → SESSION_NOT_FOUND
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr
            .sftp_chmod("nonexistent", "/home/user/file".to_string(), 0o644)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn sftp_mkdir_nonexistent_session_returns_session_not_found() {
        // mkdir 普通路径 → 无 Policy 拦截 → SESSION_NOT_FOUND
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr
            .sftp_mkdir("nonexistent", "/home/user/newdir".to_string(), 0o755)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn sftp_list_nonexistent_session_returns_session_not_found() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let err = mgr
            .sftp_list("nonexistent", "/home/user".to_string())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn custom_policy_manager_empty_chain_allows_dangerous() {
        // 空 PolicyManager（无 Policy）→ 所有命令 Allow
        let empty_policy = Arc::new(PolicyManager::new());
        let mgr = SessionManager::with_policy(
            Arc::new(FakeProvider) as Arc<dyn TerminalProvider>,
            PathPolicy::default_from_cwd(),
            empty_policy,
        );
        // 无 Policy 拦截，rm -rf / 不会被拒（但 session 不存在 → SESSION_NOT_FOUND）
        let err = mgr.send_input("any", b"rm -rf /\n").await.unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND", "空 Policy 链不拦截");
    }
}
