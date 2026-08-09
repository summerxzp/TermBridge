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
#[derive(Debug, Clone)]
pub struct Host {
    pub name: HostName,
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub identity_file: Option<PathBuf>,
    pub proxy_jump: Option<HostName>,
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

/// 创建 Terminal Backend 的请求（§4.4）。
///
/// `host` 已由 `ssh -G` 解析完成；Provider 不感知 ssh config。
#[derive(Debug, Clone)]
pub struct OpenTerminalRequest {
    pub host: Host,
    pub pty_size: PtySize,
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
}

/// Terminal Backend 句柄（§4.4）。
///
/// 生命周期与 PTY channel 绑定。`close()` 释放远端 shell + channel。
///
/// `Send + Sync`：让 `Arc<dyn TerminalHandle>` 可跨线程（PTY read task 持有 clone）。
/// 实现方（如 SshTerminalHandle）内部用 Mutex 包非 Sync 的 channel 状态。
#[async_trait]
pub trait TerminalHandle: Send + Sync {
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

    #[error("host key rejected")]
    HostKeyRejected,

    #[error("SSH channel error: {0}")]
    ChannelError(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

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
            Self::HostKeyRejected => "HOST_KEY_REJECTED",
            Self::InvalidArgument(_) => "INVALID_ARGUMENT",
            Self::Io(_) => "CONNECT_FAILED",
        }
    }

    /// Agent 是否应重试（§6.1）
    pub fn retriable(&self) -> bool {
        match self {
            Self::AuthFailed
            | Self::SessionNotFound(_)
            | Self::SessionClosed(_)
            | Self::HostKeyRejected
            | Self::InvalidArgument(_) => false,
            Self::ConnectFailed(_)
            | Self::OperationTimeout
            | Self::ChannelError(_)
            | Self::Io(_) => true,
        }
    }
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
}
