//! TerminalProvider / TerminalHandle —— §4.4 两层抽象
//!
//! ```text
//! TerminalProvider（创建 backend）
//!        │
//!        ├── SshProvider    → SshTerminalHandle    (russh + RequestPty)
//!        ├── LocalProvider  → LocalTerminalHandle  (portable-pty, Phase 5+)
//!        └── DockerProvider → DockerTerminalHandle (Phase 5+)
//! ```
//!
//! 关键：`TerminalHandle.read()` 是给 Session 内部 PTY read task 用的——
//! 读原始字节灌进 `OutputEngine.buffer().write()`。Agent 不直接调 handle.read()，
//! 而是走 `OutputEngine.read_output()`（§4.6 契约 1/2）。
//!
//! 因此 read task 形如：
//! ```ignore
//! while let Some(data) = handle.read().await? {
//!     output.buffer().write(&data);
//! }
//! // None = PTY EOF / channel closed，read task 退出
//! ```

use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;

// ───────────────────────────────────────────────────────────────────────────
// Host（配置实体，§4.3）—— "去哪台机器？"
// ───────────────────────────────────────────────────────────────────────────

/// Host 别名（ssh config 里的 Host 名）
pub type HostName = String;

/// 解析后的 Host 配置实体（§4.3）。
///
/// 由 `ssh -G <alias>` 解析得到（ADR-0006），已展开 Include/Match/ProxyJump。
/// Phase 0-C：proxy_jump 只解析不连接（MVP 不涉及跳板）。
/// Phase 1：新增 user_known_hosts_file / strict_host_key_checking（§5.5 host key 校验）。
/// Phase 2：user_known_hosts_file 改为 Vec（支持 known_hosts2 多文件），
/// strict_host_key_checking 新增 "accept-new"（TOFU 自动添加）。
#[derive(Debug, Clone)]
pub struct Host {
    pub name: HostName,
    pub hostname: String,
    pub user: String,
    pub port: u16,
    /// `IdentityFile` 列表（已展开 ~，仅含存在的文件，保持 ssh -G 输出顺序）。空 Vec 表示无。
    /// Phase 1：支持多 IdentityFile 遍历（凭据优先级 SSH Agent > IdentityFile > HITL）。
    pub identity_files: Vec<PathBuf>,
    pub proxy_jump: Option<HostName>,
    /// `UserKnownHostsFile` 路径列表（已展开 ~，保留全部空格分隔路径）。
    /// OpenSSH 默认 `~/.ssh/known_hosts ~/.ssh/known_hosts2`，Phase 2 改为收集全部路径：
    /// 校验时遍历所有文件查找 host key；TOFU 添加时写入首个路径。
    /// 不做 is_file 过滤——known_hosts 缺失本身是有意义状态（host 未知 / TOFU 首次写入）。
    /// 空 Vec 表示 ssh -G 未输出该字段（极少见，strict 模式下拒绝）。
    pub user_known_hosts_files: Vec<PathBuf>,
    /// `StrictHostKeyChecking`（小写）。缺省 ask（OpenSSH 默认）。
    /// - `yes`：严格校验，未知主机拒绝（Phase 1）
    /// - `ask`：MVP 无 HITL，等同 yes（Phase 1）
    /// - `accept-new`：TOFU——首次连接（host 不在 known_hosts）自动添加 key，已知主机正常校验（Phase 2）
    /// - `no`：接受任意 key（仅 WARN，不安全）
    pub strict_host_key_checking: String,
}

// ───────────────────────────────────────────────────────────────────────────
// PtySize / ControlKey —— 开 PTY 与发控制字符的参数
// ───────────────────────────────────────────────────────────────────────────

/// PTY 尺寸（行列）。MVP 不关心像素（0,0）。
#[derive(Debug, Clone, Copy)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

/// 控制键（§5.8 MVP 子集）。MCP 层 send_control 接字符串映射到这些字节序列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKey {
    CtrlC,
    CtrlD,
    CtrlZ,
    Tab,
    Enter,
    Escape,
}

impl ControlKey {
    /// 控制键 → PTY 字节序列（§5.8 pty-mcp 风格映射）
    pub fn as_bytes(&self) -> &'static [u8] {
        match self {
            Self::CtrlC => b"\x03",
            Self::CtrlD => b"\x04",
            Self::CtrlZ => b"\x1a",
            Self::Tab => b"\t",
            Self::Enter => b"\r",
            Self::Escape => b"\x1b",
        }
    }

    /// 字符串名 → ControlKey（MCP send_control 参数解析用）
    /// 接受 "ctrl+c" / "ctrl+c" 等大小写无关形式。
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "ctrl+c" | "c" => Some(Self::CtrlC),
            "ctrl+d" | "d" => Some(Self::CtrlD),
            "ctrl+z" | "z" => Some(Self::CtrlZ),
            "tab" => Some(Self::Tab),
            "enter" => Some(Self::Enter),
            "escape" | "esc" => Some(Self::Escape),
            _ => None,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// OpenTerminalRequest
// ───────────────────────────────────────────────────────────────────────────

/// 创建 Terminal Backend 的请求（§4.4 / ADR-0004 §8）。
///
/// `host` 已由 `ssh -G` 解析完成；Provider 不感知 ssh config。
///
/// - `persistent = false`：Interactive Session（Phase 1/2 路径，SSH 直连 PTY）
/// - `persistent = true`：Persistent Session（Phase 3 路径，远端 daemon 托管 PTY）
///   `name` 仅 persistent 模式有效，作为远端 session 的可读标签（list_remote_sessions 返回）
#[derive(Debug, Clone)]
pub struct OpenTerminalRequest {
    pub host: Host,
    pub pty_size: PtySize,
    /// 是否走远端 daemon persistent 路径（ADR-0004 §8）
    pub persistent: bool,
    /// 远端 session 可读标签（仅 persistent=true 时使用）
    pub name: Option<String>,
}

// ───────────────────────────────────────────────────────────────────────────
// TerminalProvider / TerminalHandle（§4.4）
// ───────────────────────────────────────────────────────────────────────────

/// 创建 Terminal Backend 的工厂（§4.4）。
///
/// SessionManager 持有 `Box<dyn TerminalProvider>`，不关心是 SSH 还是 Local。
/// MVP 只实现 `SshProvider`（infrastructure/ssh）。
///
/// 返回 `Arc<dyn TerminalHandle>` 而非 `Box`：PTY read task 需要共享 handle 调 `read()`，
/// Arc 让 Session（write/control/close）与 read task（read）各持一份。
#[async_trait]
pub trait TerminalProvider: Send + Sync {
    /// 创建一个 Terminal Backend，返回 Handle。
    ///
    /// 失败映射为 TermError（如 AuthFailed / ConnectFailed / HostKeyRejected）。
    async fn open(
        &self,
        request: OpenTerminalRequest,
    ) -> Result<Arc<dyn TerminalHandle>, TermError>;

    /// 下转 `&self` 为 `&dyn Any`，供 SessionManager 下转到具体 Provider
    /// 以访问 persistent 特有能力（list_remote_sessions / attach_remote_session）。
    fn as_any(&self) -> &dyn Any;
}

/// Terminal Backend 句柄（§4.4）。
///
/// 生命周期与 PTY channel 绑定。`close()` 释放远端 shell + channel。
///
/// `Send + Sync`：让 `Arc<dyn TerminalHandle>` 可跨线程（PTY read task 持有 clone）。
/// 实现方（如 SshTerminalHandle）内部用 Mutex 包非 Sync 的 channel 状态。
///
/// Phase 1：trait 加 `Any` supertrait + `as_any()` 默认方法，让 SessionManager 可下转
/// 到具体 `SshTerminalHandle` 以访问 SFTP 能力（upload/download）。
#[async_trait]
pub trait TerminalHandle: Send + Sync + Any {
    /// 读一批原始 PTY output。`Ok(None)` = PTY EOF / channel closed（read task 应退出）。
    async fn read(&self) -> Result<Option<Bytes>, TermError>;

    /// 写输入到 PTY（send_input）。立即返回，不等命令完成（§4.6 契约 7）。
    async fn write(&self, data: &[u8]) -> Result<(), TermError>;

    /// 发控制字符（send_control，§4.6 契约 8）。
    async fn send_control(&self, c: ControlKey) -> Result<(), TermError>;

    /// 调整 PTY 尺寸（window_change）。
    async fn resize(&self, size: PtySize) -> Result<(), TermError>;

    /// 关闭 PTY + channel + 远端 shell（§4.6 契约 9：Session close 才结束 shell）。
    async fn close(&self) -> Result<(), TermError>;

    /// 下转 `&self` 为 `&dyn Any`，供 SessionManager 下转到具体 `SshTerminalHandle`
    /// 以访问 SFTP 能力（upload/download）。
    ///
    /// 实现方写 `fn as_any(&self) -> &dyn Any { self }` 即可（依赖 `Self: Any`）。
    /// 不放默认实现：默认 `self` 在 trait 对象上下文中无法编译（`Self` unsized）。
    fn as_any(&self) -> &dyn Any;
}

// ───────────────────────────────────────────────────────────────────────────
// TermError —— 领域错误（§6.1 ToolError 的来源）
// ───────────────────────────────────────────────────────────────────────────

/// 领域层错误。MCP transport 层映射为 ToolError { code, message, retriable }（§6.1）。
#[derive(Debug, Error)]
pub enum TermError {
    #[error("SSH authentication failed")]
    AuthFailed,

    #[error("SSH connection failed: {0}")]
    ConnectFailed(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("session already closed: {0}")]
    SessionClosed(String),

    #[error("operation timed out")]
    OperationTimeout,

    /// Host key 校验失败（Phase 1，§5.5）。
    /// 携带拒绝原因（key 不匹配 / host 未知 / known_hosts 读取失败等）。
    #[error("host key rejected: {0}")]
    HostKeyRejected(String),

    #[error("SSH channel error: {0}")]
    ChannelError(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// SFTP 操作失败（§6.1：SFTP_ERROR，retriable=true）。
    /// Phase 1：upload/download/canonicalize 等远端 SFTP 失败统一映射到此。
    #[error("SFTP error: {0}")]
    SftpError(String),

    /// SFTP 目标不存在（Phase 2，§6.1：SFTP_NO_SUCH_FILE，retriable=false）。
    /// 远端路径不存在（mkdir 父目录缺失、remove/list/chmod 目标不存在等）。
    #[error("SFTP no such file: {0}")]
    SftpNoSuchFile(String),

    /// SFTP 权限不足（Phase 2，§6.1：SFTP_PERMISSION_DENIED，retriable=false）。
    /// 远端拒绝执行操作（目标属主非当前用户且无权限等）。
    #[error("SFTP permission denied: {0}")]
    SftpPermissionDenied(String),

    /// 本地路径策略拒绝（§5.5 / §6.1：LOCAL_PATH_NOT_ALLOWED，retriable=false）。
    /// 路径不在 `allowedLocalPaths` 白名单内，或含 `..` 穿越等。
    #[error("local path not allowed: {0}")]
    LocalPathNotAllowed(String),

    /// 远端路径策略拒绝（§5.5 / §6.1：REMOTE_PATH_NOT_ALLOWED，retriable=false）。
    /// realpath 解析后不在 `allowedRemotePaths` 白名单内，或含 null 字节等。
    #[error("remote path not allowed: {0}")]
    RemotePathNotAllowed(String),

    /// Policy 拒绝（Phase 2，§8 / §6.1：POLICY_DENIED，retriable=false）。
    /// 命中 blocklist（如 `rm -rf /`、`mkfs`、`dd of=/dev/...`）。
    /// 携带拒绝原因（命中的规则描述）。
    #[error("policy denied: {0}")]
    PolicyDenied(String),

    /// Policy 需确认（Phase 2，§8 / §6.1：POLICY_NEEDS_CONFIRM，retriable=false）。
    /// 命中 confirm 列表（如 `sudo`、`rm -rf <非根>`、`kill -9`）。
    ///
    /// Phase 2 MVP：无 HITL UI，等同 Deny；Agent 应提示用户手动执行。
    /// Phase 6 实现 HITL UI 后才真正交互确认。
    /// 携带需确认的原因（命中的规则描述）。
    #[error("policy needs confirm: {0}")]
    PolicyNeedsConfirm(String),

    // —— Phase 3：远端 daemon persistent session 错误（ADR-0004）——

    /// Session 处于 Detached 状态，读写被拒绝（ADR-0004 §8）。
    /// client 必须先 attach 才能继续操作。retriable=false。
    #[error("session detached: {0}")]
    SessionDetached(String),

    /// daemon 协议版本不匹配（ADR-0004 §4 handshake）。
    /// client 需 upgrade_runtime。retriable=false。
    #[error("daemon protocol mismatch: client={client}, daemon={daemon}")]
    DaemonProtocolMismatch { client: u32, daemon: u32 },

    /// 远端 daemon runtime 缺失（二进制未部署，ADR-0004 §6/§7）。
    /// deploy 后可重试。retriable=true。
    #[error("remote runtime missing: {0}")]
    RuntimeMissing(String),

    /// 远端 daemon 部署失败（SFTP 上传 / chmod / version 写入失败，ADR-0004 §6）。
    /// retriable=false（需人工排查原因）。
    #[error("remote runtime deploy failed: {0}")]
    RuntimeDeployFailed(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl TermError {
    /// 错误码（§6.1，稳定字符串，供 Agent 重试逻辑判断）
    pub fn code(&self) -> &'static str {
        match self {
            Self::AuthFailed => "AUTH_FAILED",
            Self::SessionNotFound(_) => "SESSION_NOT_FOUND",
            Self::SessionClosed(_) => "SESSION_CLOSED",
            Self::ConnectFailed(_) => "CONNECT_FAILED",
            Self::OperationTimeout => "OPERATION_TIMEOUT",
            Self::ChannelError(_) => "CONNECT_FAILED",
            Self::HostKeyRejected(_) => "HOST_KEY_REJECTED",
            Self::InvalidArgument(_) => "INVALID_ARGUMENT",
            Self::SftpError(_) => "SFTP_ERROR",
            Self::SftpNoSuchFile(_) => "SFTP_NO_SUCH_FILE",
            Self::SftpPermissionDenied(_) => "SFTP_PERMISSION_DENIED",
            Self::LocalPathNotAllowed(_) => "LOCAL_PATH_NOT_ALLOWED",
            Self::RemotePathNotAllowed(_) => "REMOTE_PATH_NOT_ALLOWED",
            Self::PolicyDenied(_) => "POLICY_DENIED",
            Self::PolicyNeedsConfirm(_) => "POLICY_NEEDS_CONFIRM",
            Self::SessionDetached(_) => "SESSION_DETACHED",
            Self::DaemonProtocolMismatch { .. } => "PROTOCOL_MISMATCH",
            Self::RuntimeMissing(_) => "RUNTIME_MISSING",
            Self::RuntimeDeployFailed(_) => "RUNTIME_DEPLOY_FAILED",
            Self::Io(_) => "CONNECT_FAILED",
        }
    }

    /// Agent 是否应重试（§6.1）
    pub fn retriable(&self) -> bool {
        match self {
            Self::AuthFailed
            | Self::SessionNotFound(_)
            | Self::SessionClosed(_)
            | Self::HostKeyRejected(_)
            | Self::InvalidArgument(_)
            | Self::LocalPathNotAllowed(_)
            | Self::RemotePathNotAllowed(_)
            | Self::PolicyDenied(_)
            | Self::PolicyNeedsConfirm(_)
            | Self::SftpNoSuchFile(_)
            | Self::SftpPermissionDenied(_)
            | Self::SessionDetached(_)
            | Self::DaemonProtocolMismatch { .. }
            | Self::RuntimeDeployFailed(_) => false,
            Self::ConnectFailed(_)
            | Self::OperationTimeout
            | Self::ChannelError(_)
            | Self::SftpError(_)
            | Self::RuntimeMissing(_)
            | Self::Io(_) => true,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// TransferDirection —— SFTP 传输方向（§5.5 / §7.4 Phase 1）
// ───────────────────────────────────────────────────────────────────────────

/// SFTP 传输方向。Phase 1 仅支持 upload（local→remote）与 download（remote→local）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    /// 本地 → 远端
    Upload,
    /// 远端 → 本地
    Download,
}

impl TransferDirection {
    /// 字符串名 → TransferDirection（MCP sftp_transfer 参数解析用）。
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "upload" => Some(Self::Upload),
            "download" => Some(Self::Download),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Download => "download",
        }
    }
}

/// 路径策略检查所需 SFTP 能力（domain 层接口）。
///
/// PathPolicy::check_remote 需要调远端 `realpath` 解析路径，避免 `..` 与 symlink 逃逸。
/// 此 trait 由 infrastructure 层 SFTP 实现（`SftpProvider`）实现，application 层
/// PathPolicy 通过此 trait 解耦具体 SFTP 库（russh-sftp）。
#[async_trait]
pub trait SftpCanonicalize: Send + Sync {
    /// 解析远端路径为绝对路径（等价 SFTP `realpath`）。
    async fn canonicalize(&self, path: &str) -> Result<String, TermError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_key_from_name_case_insensitive() {
        assert_eq!(ControlKey::from_name("Ctrl+C"), Some(ControlKey::CtrlC));
        assert_eq!(ControlKey::from_name("ctrl+c"), Some(ControlKey::CtrlC));
        assert_eq!(ControlKey::from_name("C"), Some(ControlKey::CtrlC));
        assert_eq!(ControlKey::from_name("enter"), Some(ControlKey::Enter));
        assert_eq!(ControlKey::from_name("esc"), Some(ControlKey::Escape));
        assert_eq!(ControlKey::from_name("unknown"), None);
    }

    #[test]
    fn control_key_bytes() {
        assert_eq!(ControlKey::CtrlC.as_bytes(), b"\x03");
        assert_eq!(ControlKey::Enter.as_bytes(), b"\r");
        assert_eq!(ControlKey::Tab.as_bytes(), b"\t");
    }

    #[test]
    fn term_error_code_and_retriable() {
        assert_eq!(TermError::AuthFailed.code(), "AUTH_FAILED");
        assert!(!TermError::AuthFailed.retriable());

        assert_eq!(
            TermError::ConnectFailed("timeout".into()).code(),
            "CONNECT_FAILED"
        );
        assert!(TermError::ConnectFailed("timeout".into()).retriable());

        assert_eq!(TermError::SessionNotFound("s1".into()).code(), "SESSION_NOT_FOUND");
        assert!(!TermError::SessionNotFound("s1".into()).retriable());
    }

    #[test]
    fn host_key_rejected_error_code_and_retriable() {
        // Phase 1：HOST_KEY_REJECTED 不可重试（key 不匹配通常是配置/攻击问题，重试无意义）。
        let err = TermError::HostKeyRejected("key mismatch at line 3".into());
        assert_eq!(err.code(), "HOST_KEY_REJECTED");
        assert!(!err.retriable());
        assert!(format!("{err}").contains("key mismatch at line 3"));
    }

    #[test]
    fn sftp_error_code_and_retriable() {
        // Phase 1：SFTP_ERROR 可重试（网络抖动等导致，重试可能成功）
        let err = TermError::SftpError("channel closed".into());
        assert_eq!(err.code(), "SFTP_ERROR");
        assert!(err.retriable());
        assert!(format!("{err}").contains("channel closed"));
    }

    #[test]
    fn path_not_allowed_errors_are_not_retriable() {
        // Phase 1：路径策略拒绝不可重试（路径不会因重试而变合法）
        let local = TermError::LocalPathNotAllowed("/etc/passwd not in allowed roots".into());
        assert_eq!(local.code(), "LOCAL_PATH_NOT_ALLOWED");
        assert!(!local.retriable());

        let remote = TermError::RemotePathNotAllowed("/root not allowed".into());
        assert_eq!(remote.code(), "REMOTE_PATH_NOT_ALLOWED");
        assert!(!remote.retriable());
    }

    #[test]
    fn sftp_no_such_file_error_code_and_retriable() {
        // Phase 2：SFTP_NO_SUCH_FILE 不可重试（远端路径不存在，重试无意义）
        let err = TermError::SftpNoSuchFile("mkdir parent missing: /no/such/path".into());
        assert_eq!(err.code(), "SFTP_NO_SUCH_FILE");
        assert!(!err.retriable());
        assert!(format!("{err}").contains("/no/such/path"));
    }

    #[test]
    fn sftp_permission_denied_error_code_and_retriable() {
        // Phase 2：SFTP_PERMISSION_DENIED 不可重试（权限不足，需用户介入）
        let err = TermError::SftpPermissionDenied("chmod /root/.ssh: denied".into());
        assert_eq!(err.code(), "SFTP_PERMISSION_DENIED");
        assert!(!err.retriable());
        assert!(format!("{err}").contains("denied"));
    }

    #[test]
    fn policy_denied_error_code_and_retriable() {
        // Phase 2：POLICY_DENIED 不可重试（命中 blocklist，重试仍会被拒）
        let err = TermError::PolicyDenied("command blocked: rm -rf /".into());
        assert_eq!(err.code(), "POLICY_DENIED");
        assert!(!err.retriable());
        assert!(format!("{err}").contains("rm -rf /"));
    }

    #[test]
    fn policy_needs_confirm_error_code_and_retriable() {
        // Phase 2：POLICY_NEEDS_CONFIRM 不可重试（需用户确认，Agent 应提示用户）
        let err = TermError::PolicyNeedsConfirm("sftp remove (recursive) needs confirmation: /tmp/foo".into());
        assert_eq!(err.code(), "POLICY_NEEDS_CONFIRM");
        assert!(!err.retriable());
        assert!(format!("{err}").contains("needs confirmation"));
    }

    #[test]
    fn transfer_direction_from_name_case_insensitive() {
        assert_eq!(
            TransferDirection::from_name("upload"),
            Some(TransferDirection::Upload)
        );
        assert_eq!(
            TransferDirection::from_name("DOWNLOAD"),
            Some(TransferDirection::Download)
        );
        assert_eq!(
            TransferDirection::from_name("Download"),
            Some(TransferDirection::Download)
        );
        assert_eq!(TransferDirection::from_name("sync"), None);
    }

    #[test]
    fn transfer_direction_as_str() {
        assert_eq!(TransferDirection::Upload.as_str(), "upload");
        assert_eq!(TransferDirection::Download.as_str(), "download");
    }
}
