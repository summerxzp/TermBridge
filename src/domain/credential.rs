//! CredentialProvider —— 平台无关的凭据请求抽象（ADR-0009）
//!
//! TermBridge Core 只依赖此 trait，不直接依赖任何平台 UI API。
//! MVP 实现：`HelperCredentialProvider`（spawn 独立 helper process，见
//! `infrastructure/credential/helper.rs`）。
//!
//! ```text
//! BootstrapHost（application 层）
//!     │
//!     │ CredentialProvider::request_password()
//!     ▼
//! HelperCredentialProvider（infrastructure 层）
//!     │
//!     │ spawn + private IPC pipe
//!     ▼
//! termbridge-credential-prompt（workspace member crate）
//!     │
//!     ├── Windows native dialog
//!     ├── macOS native dialog（后续）
//!     └── Linux dialog / TTY fallback（后续）
//! ```
//!
//! 关键约束（ADR-0009）：
//! - `Secret` 内部用 `Zeroizing<String>`，drop 自动清零
//! - Core 只依赖 trait，不 import 任何 `windows` / `cocoa` / `x11` crate
//! - 平台实现通过依赖注入传入 `SessionManager` / `BootstrapHost`
//! - 密码永不进 MCP tool arguments / LLM context

use async_trait::async_trait;
use zeroize::Zeroizing;

// ───────────────────────────────────────────────────────────────────────────
// Secret —— 一次性密码包装，drop 自动清零
// ───────────────────────────────────────────────────────────────────────────

/// 一次性密码包装。drop 时自动清零内存（`Zeroizing<String>`）。
///
/// 调用方拿到 `Secret` 后，仅在 SSH 认证瞬间 `reveal()` 暴露明文，
/// 认证完成立即 drop（作用域结束自动 Zeroize）。
///
/// **禁止**：
/// - 把 `reveal()` 的返回值存入任何长生命周期结构
/// - 写入日志 / 文件 / MCP 返回
/// - 经过 MCP transport / LLM context
#[derive(Debug)]
pub struct Secret {
    inner: Zeroizing<String>,
}

impl Secret {
    /// 从明文构造（仅 CredentialProvider 实现内部使用）。
    pub fn new(value: String) -> Self {
        Self {
            inner: Zeroizing::new(value),
        }
    }

    /// 暴露明文给调用方（仅 SSH 认证瞬间使用）。
    pub fn reveal(&self) -> &str {
        &self.inner
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Request 类型
// ───────────────────────────────────────────────────────────────────────────

/// 密码请求参数。
#[derive(Debug, Clone)]
pub struct PasswordRequest {
    /// 目标主机（hostname / IP / ssh config alias，用于 UI 展示）
    pub host: String,
    /// 登录用户名
    pub user: String,
    /// 请求原因（如 "bootstrap: deploy public key to authorized_keys"）
    pub reason: String,
}

/// private key passphrase 请求参数（MVP 可不实现，优先走 SSH Agent）。
#[derive(Debug, Clone)]
pub struct PassphraseRequest {
    /// key 文件路径（用于 UI 展示）
    pub key_path: String,
    /// 请求原因
    pub reason: String,
}

// ───────────────────────────────────────────────────────────────────────────
// CredentialError
// ───────────────────────────────────────────────────────────────────────────

/// CredentialProvider 错误。
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// 用户取消输入
    #[error("credential prompt cancelled by user")]
    Cancelled,

    /// helper 进程启动 / IPC 失败
    #[error("credential helper error: {0}")]
    HelperFailed(String),

    /// 平台不支持（如 Linux 无 GUI 且无 TTY）
    #[error("credential prompt not supported on this platform: {0}")]
    Unsupported(String),
}

// ───────────────────────────────────────────────────────────────────────────
// CredentialProvider trait
// ───────────────────────────────────────────────────────────────────────────

/// 平台无关的凭据请求抽象（Core 层，ADR-0009）。
///
/// TermBridge Core 只依赖此 trait。MVP 实现：
/// `HelperCredentialProvider`（spawn `termbridge-credential-prompt` helper）。
///
/// 实现要点：
/// - 密码经独立 IPC 通道传递，不经过 MCP transport
/// - helper stdout 只被 TermBridge 捕获
/// - 不写日志 / 文件 / 环境变量
/// - 返回的 `Secret` 由调用方负责尽快 drop（Zeroize）
#[async_trait]
pub trait CredentialProvider: Send + Sync {
    /// 请求密码（一次性使用，调用方负责 drop 后 Zeroize）。
    async fn request_password(&self, request: PasswordRequest) -> Result<Secret, CredentialError>;

    /// 请求 private key passphrase（MVP 可返回 `Unsupported`，优先走 SSH Agent）。
    async fn request_passphrase(
        &self,
        request: PassphraseRequest,
    ) -> Result<Secret, CredentialError>;
}

// ───────────────────────────────────────────────────────────────────────────
// NoopCredentialProvider —— 测试 / 未配置平台用
// ───────────────────────────────────────────────────────────────────────────

/// 不支持凭据请求的 stub 实现（测试 / 未配置平台用）。
///
/// 所有请求返回 `Unsupported`。生产环境应注入 `HelperCredentialProvider`。
#[derive(Debug, Clone, Default)]
pub struct NoopCredentialProvider;

#[async_trait]
impl CredentialProvider for NoopCredentialProvider {
    async fn request_password(
        &self,
        _request: PasswordRequest,
    ) -> Result<Secret, CredentialError> {
        Err(CredentialError::Unsupported(
            "NoopCredentialProvider: no credential provider configured".into(),
        ))
    }

    async fn request_passphrase(
        &self,
        _request: PassphraseRequest,
    ) -> Result<Secret, CredentialError> {
        Err(CredentialError::Unsupported(
            "NoopCredentialProvider: no credential provider configured".into(),
        ))
    }
}
