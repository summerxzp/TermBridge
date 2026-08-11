//! HelperCredentialProvider —— ADR-0009 阶段 C1。
//!
//! Spawn `termbridge-auth-helper` helper process，经 stdin/stdout
//! JSON IPC 请求密码。helper stdout 只被 TermBridge 捕获，不经过 MCP transport。
//!
//! 安全约束（ADR-0009）：
//! - 密码经独立 IPC 通道传递，不经过 MCP transport / LLM context
//! - 不写日志记录密码内容（只记录 "password requested" / "password received"）
//! - 返回的 `Secret` 由调用方负责尽快 drop（Zeroize）

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::domain::credential::{
    CredentialError, CredentialProvider, PassphraseRequest, PasswordRequest, Secret,
};

// ───────────────────────────────────────────────────────────────────────────
// IPC 消息类型
// ───────────────────────────────────────────────────────────────────────────

/// TermBridge → helper 请求（stdin，单行 JSON）。
#[derive(Serialize)]
struct PasswordRequestMsg {
    #[serde(rename = "type")]
    msg_type: &'static str,
    host: String,
    user: String,
    reason: String,
}

/// helper → TermBridge 响应（stdout，单行 JSON）。
///
/// 镜像 B1 helper 的 `Response` enum（`#[serde(tag = "type")]`）：
/// - `{"type":"password","value":"..."}` → `Password`
/// - `{"type":"cancelled"}` → `Cancelled`
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HelperResponse {
    Password { value: String },
    Cancelled,
}

// ───────────────────────────────────────────────────────────────────────────
// HelperCredentialProvider
// ───────────────────────────────────────────────────────────────────────────

/// 通过 spawn `termbridge-auth-helper` helper process 请求凭据。
///
/// Helper 可执行文件路径解析策略：
/// 1. 优先环境变量 `TERMBRIDGE_AUTH_HELPER`（测试 / 自定义路径用）
/// 2. 回退：与 termbridge.exe 同目录的 `termbridge-auth-helper.exe`（Windows）
///    或 `termbridge-auth-helper`（Unix）
pub struct HelperCredentialProvider {
    helper_path: PathBuf,
}

impl HelperCredentialProvider {
    /// 用默认路径策略构造（与 termbridge.exe 同目录）。
    pub fn new() -> Result<Self, CredentialError> {
        let helper_path = resolve_helper_path()?;
        Ok(Self { helper_path })
    }

    /// 用显式路径构造（测试用）。
    pub fn with_path(helper_path: PathBuf) -> Self {
        Self { helper_path }
    }
}

#[async_trait]
impl CredentialProvider for HelperCredentialProvider {
    async fn request_password(
        &self,
        request: PasswordRequest,
    ) -> Result<Secret, CredentialError> {
        // 1. 构造 IPC 请求 JSON
        let msg = PasswordRequestMsg {
            msg_type: "password_request",
            host: request.host,
            user: request.user,
            reason: request.reason,
        };
        let request_json = serde_json::to_string(&msg)
            .map_err(|e| CredentialError::HelperFailed(format!("serialize request: {e}")))?;

        tracing::debug!("credential helper: password requested");

        // 2. spawn helper process（stdin piped, stdout piped, stderr null）
        let mut child = Command::new(&self.helper_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| CredentialError::HelperFailed(format!("spawn helper: {e}")))?;

        // 3. 写请求到 helper stdin + close stdin
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| CredentialError::HelperFailed("helper stdin not captured".into()))?;
        stdin
            .write_all(request_json.as_bytes())
            .await
            .map_err(|e| CredentialError::HelperFailed(format!("write helper stdin: {e}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| CredentialError::HelperFailed(format!("write helper stdin: {e}")))?;
        drop(stdin); // 关闭 stdin 让 helper 知道请求结束

        // 4. 读 helper stdout 一行
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CredentialError::HelperFailed("helper stdout not captured".into()))?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| CredentialError::HelperFailed(format!("read helper stdout: {e}")))?;

        // 5. 等待 helper 进程退出（best-effort，不阻塞过久）
        let _ = child.wait().await;

        // 6. 解析响应
        let response: HelperResponse = serde_json::from_str(line.trim())
            .map_err(|e| CredentialError::HelperFailed(format!("parse helper response: {e}")))?;

        match response {
            HelperResponse::Password { value } => {
                tracing::debug!("credential helper: password received");
                Ok(Secret::new(value))
            }
            HelperResponse::Cancelled => {
                tracing::debug!("credential helper: cancelled by user");
                Err(CredentialError::Cancelled)
            }
        }
    }

    async fn request_passphrase(
        &self,
        _request: PassphraseRequest,
    ) -> Result<Secret, CredentialError> {
        // MVP: B1 的 helper 只处理 password_request，passphrase 直接返回 Unsupported
        Err(CredentialError::Unsupported(
            "passphrase prompt not implemented in MVP".into(),
        ))
    }
}

// ───────────────────────────────────────────────────────────────────────────
// helper 路径解析
// ───────────────────────────────────────────────────────────────────────────

/// 解析 helper 可执行文件路径。
///
/// 1. 环境变量 `TERMBRIDGE_AUTH_HELPER`
/// 2. 当前可执行文件同目录（`termbridge-auth-helper.exe` / `termbridge-auth-helper`）
/// 3. 都找不到 → `Err(HelperFailed)`
fn resolve_helper_path() -> Result<PathBuf, CredentialError> {
    // 1. 环境变量
    if let Ok(path) = std::env::var("TERMBRIDGE_AUTH_HELPER") {
        return Ok(PathBuf::from(path));
    }

    // 2. 当前可执行文件同目录
    let exe = std::env::current_exe()
        .map_err(|e| CredentialError::HelperFailed(format!("resolve current_exe: {e}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| CredentialError::HelperFailed("current_exe has no parent dir".into()))?;

    let helper_name = if cfg!(windows) {
        "termbridge-auth-helper.exe"
    } else {
        "termbridge-auth-helper"
    };

    let helper_path = dir.join(helper_name);
    if helper_path.is_file() {
        return Ok(helper_path);
    }

    Err(CredentialError::HelperFailed(format!(
        "credential helper not found: {}",
        helper_path.display()
    )))
}
