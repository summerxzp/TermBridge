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
use serde::Serialize;
use tokio::task::JoinHandle;
use bytes::Bytes;

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

// ───────────────────────────────────────────────────────────────────────────
// Phase 5-B：远端环境检测 DTO
// ───────────────────────────────────────────────────────────────────────────

/// 远端环境信息（Phase 5-B）。
///
/// 由 `detect_remote_env` 通过 SSH exec 探测命令收集，供 Agent 了解
/// 远端 OS / shell / PATH / 已装工具，决定后续操作策略。
#[derive(Debug, Clone, Serialize)]
pub struct RemoteEnvInfo {
    /// `uname -a` 输出（OS 内核架构等信息）
    pub os: String,
    /// `$SHELL` 环境变量（默认 shell 路径）
    pub shell: String,
    /// `$PATH` 环境变量
    pub path: String,
    /// 已装工具列表
    pub tools: Vec<ToolInfo>,
}

/// 远端已装工具信息（Phase 5-B）。
#[derive(Debug, Clone, Serialize)]
pub struct ToolInfo {
    /// 工具名（如 "python"、"node"）
    pub name: String,
    /// `which` 输出的绝对路径；未安装时为空串
    pub path: String,
    /// 是否已安装
    pub installed: bool,
}

/// 重连结果（Phase 6-A，ADR-0010）。
///
/// Agent 调用 `reconnect_session` 后根据 `status` 字段判断结果：
/// - `reconnected`：重连成功，session_id 可继续使用（buffer 从新开始）
/// - `not_lost`：session 非 Lost 状态，无需重连
/// - `failed`：重连失败（旧 session 已 close，需 open_session 新建）
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReconnectResult {
    /// 重连成功
    Reconnected {
        session_id: String,
        host: String,
        /// 是否恢复了工作目录（MVP 恒为 false，ADR-0010 §5）
        cwd_restored: bool,
    },
    /// session 非 Lost 状态
    NotLost {
        session_id: String,
        current_state: String,
    },
    /// 重连失败
    Failed {
        session_id: String,
        reason: String,
    },
}

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
        // Phase 4-B：tracing（debug 级别，截断长命令 + 转义换行，避免日志爆炸）
        let data_display = truncate_for_log(data, 200);
        tracing::debug!(session = %session_id, input = %data_display, "send_input");

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
        // Phase 4-B：tracing（debug 级别，记录读取模式）
        let mode = read_mode_name(&params);
        tracing::debug!(session = %session_id, mode = %mode, "read_output");
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
        // Phase 4-B：tracing（info 级别，低频重要操作）
        tracing::info!(session = %session_id, control = ?key, "send_control");
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

    // ── 人类管理员 CLI raw mode（直接读写 PTY handle，绕过 OutputEngine buffer） ──

    /// 让 session 进入 raw mode：中止内部 read task，让外部可独占调用 `read_raw`。
    ///
    /// 调用后 OutputEngine buffer 不再更新（read task 已中止），`handle.read()`
    /// 可被 `read_raw` 独占调用，不会与内部 read_task 竞争。
    pub fn prepare_for_raw_mode(&self, session_id: &str) -> Result<(), TermError> {
        let session = self.get_session(session_id)?;
        session.abort_read_task();
        Ok(())
    }

    /// 直接从 PTY handle 读一批原始字节（供 CLI raw mode）。
    ///
    /// **必须先调 `prepare_for_raw_mode`**，否则会与 Session 内部 read_task 竞争
    /// `handle.read()`（两者都是单消费者，会交替拿到数据导致输出丢失）。
    /// `Ok(None)` = PTY EOF（远端 shell 退出 / 连接断开）。
    pub async fn read_raw(&self, session_id: &str) -> Result<Option<Bytes>, TermError> {
        let session = self.get_session(session_id)?;
        session.handle().read().await
    }

    /// 直接写字节到 PTY handle（供 CLI raw mode，不经过 Policy 拦截）。
    ///
    /// 与 `send_input` 的区别：不经过 Policy 拦截 / 不更新 timeline / 不 touch
    /// last_activity。CLI 是人类管理员工具，管理员自行承担命令风险。
    pub async fn write_raw(&self, session_id: &str, data: &[u8]) -> Result<(), TermError> {
        let session = self.get_session(session_id)?;
        session.handle().write(data).await
    }

    /// 调整 PTY 尺寸（window_change）。
    pub async fn resize(
        &self,
        session_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), TermError> {
        // Phase 4-B：tracing（info 级别，低频重要操作）
        tracing::info!(session = %session_id, cols, rows, "resize");
        let session = self.get_session(session_id)?;
        match session.resize(PtySize { rows, cols }).await {
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
        // Phase 4-B：tracing（info 级别，低频重要操作）
        tracing::info!(session = %session_id, "close_session");
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

    // ── Phase 5-A：SFTP 目录递归传输 ──────────────────────────────

    /// SFTP 递归传输目录（Phase 5-A）。
    ///
    /// 复用 `sftp_transfer` 的 session 获取 + path policy 校验模式，
    /// 调用 `SftpProvider::upload_dir` 或 `download_dir`。
    /// 返回传输的文件数（不含目录）。
    pub async fn sftp_transfer_dir(
        &self,
        session_id: &str,
        direction: TransferDirection,
        local_path: PathBuf,
        remote_path: String,
    ) -> Result<usize, TermError> {
        self.check_policy(&Action::SftpTransfer {
            direction,
            local: local_path.to_string_lossy().into_owned(),
            remote: remote_path.clone(),
        })?;

        let sftp = self.open_sftp_for_session(session_id).await?;
        tracing::info!(
            session = %session_id,
            direction = direction.as_str(),
            local = ?local_path,
            remote = %remote_path,
            "sftp_transfer_dir: starting"
        );

        let result = async {
            match direction {
                TransferDirection::Upload => {
                    self.path_policy.check_local(local_path.as_path())?;
                    self.path_policy
                        .check_remote_allow_new(&remote_path, &sftp)
                        .await?;
                    sftp.upload_dir(local_path.as_path(), &remote_path).await
                }
                TransferDirection::Download => {
                    self.path_policy.check_remote(&remote_path, &sftp).await?;
                    self.path_policy.check_local(local_path.as_path())?;
                    sftp.download_dir(&remote_path, local_path.as_path()).await
                }
            }
        }
        .await;

        let _ = sftp.close().await;
        result
    }

    // ── Phase 5-B：远端环境检测 ───────────────────────────────────

    /// 检测远端环境（OS / shell / PATH / 已装工具，Phase 5-B）。
    ///
    /// 通过 session 的 SSH 连接 exec 一条探测命令（不开 PTY，不污染 session 输出），
    /// 解析输出提取结构化信息。
    pub async fn detect_remote_env(
        &self,
        session_id: &str,
    ) -> Result<RemoteEnvInfo, TermError> {
        let session = self.get_session(session_id)?;
        let handle = session.handle();
        let ssh_handle = handle
            .as_any()
            .downcast_ref::<SshTerminalHandle>()
            .ok_or_else(|| {
                TermError::InvalidArgument(format!(
                    "session '{session_id}' does not support exec (non-SSH handle)"
                ))
            })?;

        tracing::info!(session = %session_id, "detect_remote_env: starting");

        let probe = concat!(
            r#"echo "__ENV_START__"; "#,
            r#"uname -a; "#,
            r#"echo "SHELL=$SHELL"; "#,
            r#"echo "PATH=$PATH"; "#,
            r#"for cmd in python python3 node npm rustc cargo go docker git; do "#,
            r#"p=$(which $cmd 2>/dev/null); "#,
            r#"if [ -n "$p" ]; then echo "TOOL:$cmd:$p"; "#,
            r#"else echo "TOOL:$cmd:"; fi; "#,
            r#"done; "#,
            r#"echo "__ENV_END__""#,
        );

        let output = ssh_handle.exec(probe).await?;
        let info = parse_remote_env(&output)?;
        tracing::info!(
            session = %session_id,
            tools_count = info.tools.len(),
            "detect_remote_env: complete"
        );
        Ok(info)
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
        // Phase 4-B：tracing（info 级别，低频重要操作）
        tracing::info!(host = %host_alias, remote_session_id, "attach_remote_session");
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
        // Phase 4-B：tracing（info 级别，低频重要操作）
        tracing::info!(session = %session_id, "detach_session");
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

    // ── Phase 6-A：断线感知 + 手动重连 ────────────────────────────

    /// 查询 session 当前状态（ADR-0010）。
    ///
    /// 供 Agent 通过 read_output 返回的 `session_state` 字段外的独立查询。
    /// 返回小写字符串：`ready` / `closing` / `closed` / `lost`。
    pub fn session_state(&self, session_id: &str) -> Result<String, TermError> {
        let session = self.get_session(session_id)?;
        Ok(format!("{:?}", session.state()).to_lowercase())
    }

    /// 重连 Lost session（ADR-0010）。
    ///
    /// 流程：
    /// 1. 取出旧 session，检查 state == Lost
    /// 2. 记录 host + pty_size
    /// 3. close 旧 session（清理 handle + read task）
    /// 4. provider.open() 新建 handle
    /// 5. 新建 Session（新 buffer），复用旧 session_id
    /// 6. DashMap.insert 替换
    ///
    /// **约束**：
    /// - 仅交互式 session（persistent=false）支持重连
    /// - buffer 历史不保留（从新开始）
    /// - 不恢复 shell 状态（cwd/env/history）
    /// - 重连失败后旧 session 已 close，需 open_session 新建
    pub async fn reconnect_session(
        &self,
        session_id: &str,
    ) -> Result<ReconnectResult, TermError> {
        tracing::info!(session = %session_id, "reconnect_session: starting");

        // 1. 取出旧 session
        let old_session = self.get_session(session_id)?;

        // 2. 检查 state（仅 Lost 允许重连）
        let current_state = old_session.state();
        if current_state != crate::domain::session::SessionState::Lost {
            tracing::info!(
                session = %session_id,
                state = ?current_state,
                "reconnect_session: not lost, skipping"
            );
            return Ok(ReconnectResult::NotLost {
                session_id: session_id.to_string(),
                current_state: format!("{:?}", current_state).to_lowercase(),
            });
        }

        // 3. 记录 host + pty_size（close 前取出，close 后 session 不可用）
        let host_name = old_session.host().to_string();
        let pty_size = old_session.pty_size();
        let session_id_owned = session_id.to_string();

        // 4. close 旧 session（清理 handle + abort read task）
        //    注意：close 后旧 session 仍在 DashMap 中（state=Closed），
        //    我们用 insert 覆盖它。
        if let Err(e) = old_session.close().await {
            tracing::warn!(
                session = %session_id,
                error = %e,
                "reconnect_session: old session close failed (continuing)"
            );
        }

        // 5. 重新解析 ssh config
        let host = match sshconfig::resolve(&host_name).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    session = %session_id,
                    error = %e,
                    "reconnect_session: ssh config resolve failed"
                );
                return Ok(ReconnectResult::Failed {
                    session_id: session_id_owned,
                    reason: format!("ssh config resolve: {e}"),
                });
            }
        };

        // 6. provider.open 新建 handle（交互式 session，persistent=false）
        let handle = match self
            .provider
            .open(OpenTerminalRequest {
                host: host.clone(),
                pty_size,
                persistent: false,
                name: None,
            })
            .await
        {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(
                    session = %session_id,
                    error = %e,
                    "reconnect_session: provider.open failed"
                );
                return Ok(ReconnectResult::Failed {
                    session_id: session_id_owned,
                    reason: format!("{e}"),
                });
            }
        };

        // 7. 新建 Session，复用旧 session_id（新 buffer）
        let new_session = Arc::new(Session::new(
            session_id_owned.clone(),
            host.name.clone(),
            pty_size,
            handle,
        ));

        // 8. 替换 DashMap entry
        self.sessions.insert(session_id_owned.clone(), new_session);

        tracing::info!(
            session = %session_id,
            host = %host.name,
            "reconnect_session: reconnected"
        );

        Ok(ReconnectResult::Reconnected {
            session_id: session_id_owned,
            host: host.name,
            cwd_restored: false, // MVP 不恢复 cwd（ADR-0010 §5）
        })
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
    /// 若 session 状态为 Closed，从 map 移除防泄漏。
    ///
    /// **ADR-0010 修正**：Lost session 不在此移除——保留供 `reconnect_session`
    /// 恢复。Lost session 的兜底清理由 idleReaper（30min 超时）负责。
    /// 仅 Closed（已显式关闭，不可恢复）立即移除。
    fn cleanup_detached_session(&self, session_id: &str, session: &Arc<Session>) {
        if session.state().is_closed() {
            tracing::warn!(
                session = %session_id,
                state = ?session.state(),
                "session closed, removing from map"
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

// ───────────────────────────────────────────────────────────────────────────
// Phase 5-B：远端环境探测输出解析
// ───────────────────────────────────────────────────────────────────────────

/// 解析 `detect_remote_env` 探测命令的 stdout 输出为 `RemoteEnvInfo`。
///
/// 探测命令输出格式（由 `__ENV_START__` / `__ENV_END__` 标记包裹）：
/// ```text
/// __ENV_START__
/// Linux host 5.15.0-91-generic ... (uname -a)
/// SHELL=/bin/bash
/// PATH=/usr/local/sbin:...
/// TOOL:python:/usr/bin/python3
/// TOOL:node:
/// __ENV_END__
/// ```
fn parse_remote_env(output: &str) -> Result<RemoteEnvInfo, TermError> {
    const START_MARKER: &str = "__ENV_START__";
    const END_MARKER: &str = "__ENV_END__";

    let start = output
        .find(START_MARKER)
        .ok_or_else(|| TermError::ChannelError("detect_remote_env: start marker not found".into()))?;
    let end_rel = output[start..]
        .find(END_MARKER)
        .ok_or_else(|| TermError::ChannelError("detect_remote_env: end marker not found".into()))?;
    let body_start = start + START_MARKER.len();
    let body_end = start + end_rel;
    let body = &output[body_start..body_end];

    let mut lines = body.lines().skip_while(|l| l.trim().is_empty());
    let os = lines.next().unwrap_or("").trim().to_string();

    let mut shell = String::new();
    let mut path = String::new();
    let mut tools = Vec::new();

    for line in lines {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("SHELL=") {
            shell = val.to_string();
        } else if let Some(val) = line.strip_prefix("PATH=") {
            path = val.to_string();
        } else if let Some(rest) = line.strip_prefix("TOOL:") {
            if let Some((name, tool_path)) = rest.split_once(':') {
                let installed = !tool_path.is_empty();
                tools.push(ToolInfo {
                    name: name.to_string(),
                    path: tool_path.to_string(),
                    installed,
                });
            }
        }
    }

    Ok(RemoteEnvInfo {
        os,
        shell,
        path,
        tools,
    })
}

// ───────────────────────────────────────────────────────────────────────────
// Phase 4-B：tracing 辅助
// ───────────────────────────────────────────────────────────────────────────

/// 将输入字节按 UTF-8 lossy 转字符串，截断到 `max_chars` 字符并转义换行，
/// 供 send_input 的 debug tracing 使用（防止日志爆炸）。
fn truncate_for_log(data: &[u8], max_chars: usize) -> String {
    let s = String::from_utf8_lossy(data);
    let mut out: String = if s.chars().count() > max_chars {
        let mut t: String = s.chars().take(max_chars).collect();
        t.push_str("...");
        t
    } else {
        s.into_owned()
    };
    out = out.replace('\n', "\\n").replace('\r', "\\r");
    out
}

/// 从 ReadOutputParams 提取模式名（与 OutputEngine 优先级一致：
/// since_cursor > tail_lines > wait_for > 默认 settle）。
fn read_mode_name(params: &ReadOutputParams) -> &'static str {
    if params.since_cursor.is_some() {
        "since_cursor"
    } else if params.tail_lines.is_some() {
        "tail"
    } else if params.wait_for.is_some() {
        "wait_for"
    } else {
        "settle"
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

    // ── Phase 1 / Phase 6-A：session 清理测试 ──────────────────────

    /// ADR-0010：Lost session 不被 send_input 移除，保留供 reconnect_session 恢复。
    /// Lost session 的兜底清理由 idleReaper（30min 超时）负责。
    #[tokio::test]
    async fn cleanup_detached_session_preserves_lost_for_reconnect() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);

        // 手动构造一个 Lost session 插入 map（FakeHandle 立即 EOF → read task 退出 → Lost）
        let session = make_fake_session("sess_lost");
        mgr.sessions.insert("sess_lost".into(), session.clone());

        // 等 read task EOF → state Lost
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(session.state(), SessionState::Lost);

        // send_input 应返回 SessionClosed
        let err = mgr.send_input("sess_lost", b"ls\n").await.unwrap_err();
        assert_eq!(err.code(), "SESSION_CLOSED");

        // ADR-0010：Lost session 应保留在 map 中供 reconnect
        assert!(
            mgr.sessions.contains_key("sess_lost"),
            "Lost session 应保留在 map 供 reconnect_session 恢复"
        );
    }

    /// Closed session（已显式关闭，不可恢复）应被 send_input 移除防泄漏。
    #[tokio::test]
    async fn cleanup_detached_session_removes_closed_from_map() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);

        // 构造 session → 等 Lost → close() → Closed
        let session = make_fake_session("sess_closed");
        mgr.sessions.insert("sess_closed".into(), session.clone());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(session.state(), SessionState::Lost);
        session.close().await.unwrap();
        assert_eq!(session.state(), SessionState::Closed);

        // send_input 应返回 SessionClosed，且从 map 移除（Closed 不可恢复）
        let err = mgr.send_input("sess_closed", b"ls\n").await.unwrap_err();
        assert_eq!(err.code(), "SESSION_CLOSED");
        assert!(
            !mgr.sessions.contains_key("sess_closed"),
            "Closed session 应从 map 移除防泄漏"
        );
    }

    // ── Phase 6-B：Lost 状态边界行为测试（ADR-0010 §4.6 契约 10） ──────

    /// FakeHandle 变体：read 永不返回（read task 挂起，session 保持 Ready）。
    /// 用于测试非 Lost 状态下的行为（如 reconnect 返回 NotLost）。
    struct FakeHandleNeverEof;

    #[async_trait]
    impl TerminalHandle for FakeHandleNeverEof {
        async fn read(&self) -> Result<Option<Bytes>, TermError> {
            std::future::pending().await
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

    /// 构造一个已进入 Lost 状态的 session 并插入 mgr，返回 session_id。
    /// chunks 会被 read task 消费完，然后 read 返回 None → Lost（§4.6 契约 10）。
    async fn make_lost_session(mgr: &SessionManager, chunks: Vec<Bytes>) -> String {
        let id = mgr.next_session_id();
        let handle = Arc::new(FakeHandle {
            chunks: PLMutex::new(chunks),
        }) as Arc<dyn TerminalHandle>;
        let session = Arc::new(Session::new(
            id.clone(),
            "testhost".into(),
            PtySize::default(),
            handle,
        ));
        mgr.sessions.insert(id.clone(), session.clone());
        // 等 read task 消费完 chunks 并 EOF → Lost
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(session.state(), SessionState::Lost, "session 应进入 Lost 状态");
        id
    }

    /// 契约 10：Session 进入 Lost 后，buffer 仍可读（disconnect 不销毁 Session）。
    #[tokio::test]
    async fn lost_state_buffer_still_readable() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let id = make_lost_session(
            &mgr,
            vec![Bytes::from_static(b"hello lost session\n")],
        )
        .await;

        let result = mgr
            .read_output(
                &id,
                ReadOutputParams {
                    since_cursor: Some(0),
                    max_bytes: Some(4096),
                    ..Default::default()
                },
            )
            .await
            .expect("Lost 状态 read_output 应成功");
        assert!(
            !result.output.is_empty(),
            "Lost 状态 buffer 应仍有数据可读"
        );
    }

    /// Lost 状态下 send_input 返回 SESSION_CLOSED（连接已断，不可写）。
    #[tokio::test]
    async fn lost_state_send_input_returns_session_closed() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let id = make_lost_session(&mgr, vec![Bytes::from_static(b"$ ")]).await;

        let err = mgr.send_input(&id, b"ls\n").await.unwrap_err();
        assert_eq!(err.code(), "SESSION_CLOSED");
    }

    /// Lost 状态下 send_control 返回 SESSION_CLOSED（连接已断，不可写）。
    #[tokio::test]
    async fn lost_state_send_control_returns_session_closed() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let id = make_lost_session(&mgr, vec![Bytes::from_static(b"$ ")]).await;

        let err = mgr.send_control(&id, ControlKey::CtrlC).await.unwrap_err();
        assert_eq!(err.code(), "SESSION_CLOSED");
    }

    /// Lost 状态下 close_session 幂等：首次成功（Lost → Closed），再次 SESSION_NOT_FOUND。
    #[tokio::test]
    async fn close_session_on_lost_state_is_idempotent() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let id = make_lost_session(&mgr, vec![Bytes::from_static(b"$ ")]).await;

        // 首次 close：Lost → Closed，从 map 移除
        mgr.close_session(&id)
            .await
            .expect("Lost 状态 close_session 应成功");

        // 再次 close：已从 map 移除 → SESSION_NOT_FOUND
        let err = mgr.close_session(&id).await.unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
    }

    /// 非 Lost 状态调 reconnect_session 返回 NotLost（current_state="ready"）。
    #[tokio::test]
    async fn reconnect_on_non_lost_returns_not_lost() {
        let mgr = SessionManager::new(Arc::new(FakeProvider) as Arc<dyn TerminalProvider>);
        let id = mgr.next_session_id();
        let handle = Arc::new(FakeHandleNeverEof) as Arc<dyn TerminalHandle>;
        let session = Arc::new(Session::new(
            id.clone(),
            "testhost".into(),
            PtySize::default(),
            handle,
        ));
        mgr.sessions.insert(id.clone(), session.clone());
        assert_eq!(session.state(), SessionState::Ready);

        let result = mgr.reconnect_session(&id).await.unwrap();
        match result {
            ReconnectResult::NotLost {
                session_id,
                current_state,
            } => {
                assert_eq!(session_id, id);
                assert_eq!(current_state, "ready");
            }
            other => panic!("期望 NotLost，实际: {:?}", other),
        }
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

    // ── Phase 5-B：parse_remote_env 单元测试 ───────────────────────

    #[test]
    fn parse_remote_env_extracts_all_fields() {
        let output = concat!(
            "__ENV_START__\n",
            "Linux myhost 5.15.0-91-generic #101-Ubuntu SMP x86_64 GNU/Linux\n",
            "SHELL=/bin/bash\n",
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n",
            "TOOL:python:/usr/bin/python3\n",
            "TOOL:python3:/usr/bin/python3\n",
            "TOOL:node:/usr/bin/node\n",
            "TOOL:npm:/usr/bin/npm\n",
            "TOOL:rustc:\n",
            "TOOL:cargo:\n",
            "TOOL:go:/usr/local/go/bin/go\n",
            "TOOL:docker:/usr/bin/docker\n",
            "TOOL:git:/usr/bin/git\n",
            "__ENV_END__\n",
        );
        let info = parse_remote_env(output).unwrap();
        assert!(info.os.contains("Linux myhost"));
        assert_eq!(info.shell, "/bin/bash");
        assert!(info.path.contains("/usr/bin"));
        assert_eq!(info.tools.len(), 9);
        assert_eq!(info.tools[0].name, "python");
        assert_eq!(info.tools[0].path, "/usr/bin/python3");
        assert!(info.tools[0].installed);
        assert_eq!(info.tools[4].name, "rustc");
        assert_eq!(info.tools[4].path, "");
        assert!(!info.tools[4].installed);
    }

    #[test]
    fn parse_remote_env_missing_markers_returns_error() {
        assert!(parse_remote_env("no markers here").is_err());
        assert!(parse_remote_env("__ENV_START__\nbut no end").is_err());
    }

    #[test]
    fn parse_remote_env_empty_tools() {
        let output = concat!(
            "__ENV_START__\n",
            "Darwin host 23.0.0 Darwin Kernel arm64\n",
            "SHELL=/bin/zsh\n",
            "PATH=/usr/bin:/bin\n",
            "__ENV_END__\n",
        );
        let info = parse_remote_env(output).unwrap();
        assert!(info.os.contains("Darwin"));
        assert_eq!(info.shell, "/bin/zsh");
        assert_eq!(info.tools.len(), 0);
    }
}
