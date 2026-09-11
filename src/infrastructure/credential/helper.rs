//! HelperCredentialProvider —— ADR-0009 阶段 C1 + ADR-0019 协议 v2 / 超时。
//!
//! Spawn `termbridge-auth-helper` helper process，经 stdin/stdout
//! JSON IPC 请求密码。helper stdout 只被 TermBridge 捕获，不经过 MCP transport。
//!
//! 安全约束（ADR-0009）：
//! - 密码经独立 IPC 通道传递，不经过 MCP transport / LLM context
//! - 不写日志记录密码内容（只记录 "password requested" / "password received"）
//! - 返回的 `Secret` 由调用方负责尽快 drop（Zeroize）
//!
//! 协议 v2（ADR-0019）：helper 响应区分 cancelled / unsupported / failed /
//! password 四态。旧 helper 二进制只回 password / cancelled，`user` 字段
//! 缺省兼容（None → 调用方回退预填用户名）。
//!
//! 超时（ADR-0019）：父进程侧计时（默认 5 分钟，`TERMBRIDGE_PROMPT_TIMEOUT`
//! 可配，秒，0 = 禁用），超时先 SIGTERM（helper 的 signal_guard 恢复 termios /
//! 清理子 dialog）、宽限 2s、再 SIGKILL。helper 可能卡在不可中断的内核
//! 调用上，自杀式超时不可靠，必须由父进程执行。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

use crate::domain::credential::{
    CredentialError, CredentialProvider, PassphraseRequest, PasswordCredential, PasswordRequest,
    Secret,
};

/// 凭据输入等待超时（ADR-0019 默认 5 分钟：用户可能去找密码 / 切窗口）。
const DEFAULT_PROMPT_TIMEOUT_SECS: u64 = 300;

/// 超时后 SIGTERM 的宽限期，超时升级为 SIGKILL（helper 的 signal_guard
/// 在 SIGTERM 时恢复 termios / 清理子 dialog，正常情况下立即退出）。
const KILL_GRACE: Duration = Duration::from_secs(2);

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
/// 镜像 helper 的 `Response` enum（`#[serde(tag = "type")]`）：
/// - `{"type":"password","value":"...","user":"..."}` → `Password`
/// - `{"type":"password","value":"..."}` → `Password`（旧版 helper 无 user，
///   `#[serde(default)]` 兼容 → user=None，调用方回退 request.user）
/// - `{"type":"cancelled"}` → `Cancelled`
/// - `{"type":"unsupported","message":"..."}` → `Unsupported`（协议 v2）
/// - `{"type":"failed","message":"..."}` → `Failed`（协议 v2）
///
/// serde 兼容性：本 enum 未开 `deny_unknown_fields`——未知字段被忽略，
/// 旧 TermBridge 读新 helper 的 `user` 字段同样安全。
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HelperResponse {
    Password {
        value: String,
        /// 对话框中实际确认/编辑的用户名（旧 helper 省略此字段 → None）
        #[serde(default)]
        user: Option<String>,
    },
    Cancelled,
    /// 环境无可用输入通道（message 含可行动指引，ADR-0019）
    Unsupported {
        message: String,
    },
    /// provider 执行失败（如 TERMBRIDGE_ASKPASS 程序损坏，ADR-0019）
    Failed {
        message: String,
    },
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
    ) -> Result<PasswordCredential, CredentialError> {
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

        // 4. 读 helper stdout 一行（带父进程侧超时：用户可能永远不响应）
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CredentialError::HelperFailed("helper stdout not captured".into()))?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();

        let prompt_timeout = prompt_timeout();
        let read_line = reader.read_line(&mut line);
        let n_read = match prompt_timeout {
            None => read_line
                .await
                .map_err(|e| CredentialError::HelperFailed(format!("read helper stdout: {e}")))?,
            Some(secs) => match timeout(secs, read_line).await {
                Ok(res) => res.map_err(|e| {
                    CredentialError::HelperFailed(format!("read helper stdout: {e}"))
                })?,
                Err(_) => {
                    // 超时：SIGTERM（helper signal_guard 恢复 termios / 清理子
                    // dialog）→ 宽限 → SIGKILL 兜底
                    tracing::warn!(
                        timeout_secs = secs.as_secs(),
                        "credential helper: prompt timed out, killing helper"
                    );
                    kill_helper(&mut child).await;
                    return Err(CredentialError::Timeout(secs.as_secs()));
                }
            },
        };
        if n_read == 0 {
            return Err(CredentialError::HelperFailed(
                "helper exited without response".into(),
            ));
        }

        // 5. 等待 helper 进程退出（best-effort，不阻塞过久）
        let _ = child.wait().await;

        // 6. 解析响应
        let response: HelperResponse = serde_json::from_str(line.trim())
            .map_err(|e| CredentialError::HelperFailed(format!("parse helper response: {e}")))?;

        match response {
            HelperResponse::Password { value, user } => {
                tracing::debug!("credential helper: password received");
                Ok(PasswordCredential {
                    user,
                    secret: Secret::new(value),
                })
            }
            HelperResponse::Cancelled => {
                tracing::debug!("credential helper: cancelled by user");
                Err(CredentialError::Cancelled)
            }
            HelperResponse::Unsupported { message } => {
                tracing::debug!("credential helper: no interactive channel available");
                Err(CredentialError::Unsupported(message))
            }
            HelperResponse::Failed { message } => {
                tracing::warn!("credential helper: provider failed: {}", message);
                Err(CredentialError::HelperFailed(message))
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
// 超时（ADR-0019）
// ───────────────────────────────────────────────────────────────────────────

/// 解析凭据输入超时：`TERMBRIDGE_PROMPT_TIMEOUT`（秒），缺省 300（5 分钟），
/// 0 = 禁用（无限等待，兼容旧行为）。非法值（非数字）回退默认。
fn prompt_timeout() -> Option<Duration> {
    let raw = match std::env::var("TERMBRIDGE_PROMPT_TIMEOUT") {
        Ok(v) => v,
        Err(_) => return Some(Duration::from_secs(DEFAULT_PROMPT_TIMEOUT_SECS)), // 未设置 → 默认
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => None, // 显式禁用
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(_) => Some(Duration::from_secs(DEFAULT_PROMPT_TIMEOUT_SECS)), // 非法值回退默认
    }
}

/// 超时杀 helper：SIGTERM → 宽限 → SIGKILL。
async fn kill_helper(child: &mut tokio::process::Child) {
    let _ = child.start_kill(); // SIGTERM (Unix) / TerminateProcess (Windows)
    match tokio::time::timeout(KILL_GRACE, child.wait()).await {
        Ok(_) => {} // SIGTERM 生效，helper 已清理退出
        Err(_) => {
            let _ = child.kill().await; // 宽限超时，强杀
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── helper 响应协议：user 字段向后/向前兼容 ────────────────────────

    #[test]
    fn parse_response_with_user() {
        // 新 helper：回传对话框中确认/编辑后的用户名
        let resp: HelperResponse =
            serde_json::from_str(r#"{"type":"password","value":"pw","user":"alice"}"#).unwrap();
        match resp {
            HelperResponse::Password { value, user } => {
                assert_eq!(value, "pw");
                assert_eq!(user.as_deref(), Some("alice"));
            }
            other => panic!("期望 Password，实际: {other:?}"),
        }
    }

    #[test]
    fn parse_response_without_user() {
        // 旧 helper 二进制：只回密码，无 user 字段 → None（调用方回退 request.user）
        let resp: HelperResponse =
            serde_json::from_str(r#"{"type":"password","value":"pw"}"#).unwrap();
        match resp {
            HelperResponse::Password { value, user } => {
                assert_eq!(value, "pw");
                assert_eq!(user, None, "旧 helper 无 user 字段应为 None");
            }
            other => panic!("期望 Password，实际: {other:?}"),
        }
    }

    #[test]
    fn parse_response_cancelled() {
        let resp: HelperResponse = serde_json::from_str(r#"{"type":"cancelled"}"#).unwrap();
        assert!(matches!(resp, HelperResponse::Cancelled));
    }

    // ── 协议 v2：unsupported / failed（ADR-0019）─────────────────────

    #[test]
    fn parse_response_unsupported() {
        let resp: HelperResponse = serde_json::from_str(
            r#"{"type":"unsupported","message":"no channel (tried: zenity: cannot open display)"}"#,
        )
        .unwrap();
        match resp {
            HelperResponse::Unsupported { message } => {
                assert!(message.contains("zenity"));
            }
            other => panic!("期望 Unsupported，实际: {other:?}"),
        }
    }

    #[test]
    fn parse_response_failed() {
        let resp: HelperResponse =
            serde_json::from_str(r#"{"type":"failed","message":"askpass program crashed"}"#)
                .unwrap();
        match resp {
            HelperResponse::Failed { message } => {
                assert_eq!(message, "askpass program crashed");
            }
            other => panic!("期望 Failed，实际: {other:?}"),
        }
    }

    // ── 超时配置解析（ADR-0019）──────────────────────────────────────

    /// 环境变量测试串行锁：两个测试（parsing / hanging helper）共享同一把，
    /// 防止 TERMBRIDGE_PROMPT_TIMEOUT 读写交错。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn prompt_timeout_env_parsing() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // 未设置 → 默认 5 分钟
        std::env::remove_var("TERMBRIDGE_PROMPT_TIMEOUT");
        assert_eq!(prompt_timeout(), Some(Duration::from_secs(300)));

        // 显式秒数
        std::env::set_var("TERMBRIDGE_PROMPT_TIMEOUT", "30");
        assert_eq!(prompt_timeout(), Some(Duration::from_secs(30)));

        // 0 = 禁用（无限等待）
        std::env::set_var("TERMBRIDGE_PROMPT_TIMEOUT", "0");
        assert_eq!(prompt_timeout(), None);

        // 非法值 → 回退默认
        std::env::set_var("TERMBRIDGE_PROMPT_TIMEOUT", "abc");
        assert_eq!(prompt_timeout(), Some(Duration::from_secs(300)));

        std::env::remove_var("TERMBRIDGE_PROMPT_TIMEOUT");
    }

    // ── 超时 E2E：挂起 helper + 短超时（ADR-0019）────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn prompt_timeout_kills_hanging_helper() {
        use std::os::unix::fs::PermissionsExt;

        // stub helper：读 stdin 后挂起（模拟用户不在场、对话框无人响应）
        let script = "#!/bin/sh\nread line\nsleep 600\n";
        let path =
            std::env::temp_dir().join(format!("termbridge-hang-helper-{}.sh", std::process::id()));
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        // 串行锁：与 prompt_timeout_env_parsing 共享，防 env 读写交错
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("TERMBRIDGE_PROMPT_TIMEOUT", "1");

        let provider = HelperCredentialProvider::with_path(path.clone());
        let start = std::time::Instant::now();
        let err = provider
            .request_password(PasswordRequest {
                host: "h".into(),
                user: "u".into(),
                reason: "r".into(),
            })
            .await
            .unwrap_err();

        std::env::remove_var("TERMBRIDGE_PROMPT_TIMEOUT");
        let _ = std::fs::remove_file(&path);

        // 超时错误 + 总耗时 ≈ 1s 超时（+ 最多 2s SIGTERM 宽限）
        assert!(matches!(err, CredentialError::Timeout(1)), "实际: {err:?}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "超时应及时杀掉 helper"
        );
    }
}
