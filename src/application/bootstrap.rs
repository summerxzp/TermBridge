//! BootstrapHost —— 把 Host 初始化为长期可 key 认证的 SSH 主机（ADR-0009）。
//!
//! ```text
//! BootstrapHost::bootstrap(host_alias)
//!   ├── sshconfig::resolve(alias) → Host
//!   ├── connect_unauthenticated → 尝试 key 认证
//!   │   └── 成功 → AlreadyConfigured
//!   ├── ensure_identity_file（生成 ed25519 keypair 如需）
//!   ├── CredentialProvider::request_password
//!   │   └── 用户取消 → Cancelled
//!   ├── authenticate_with_password
//!   │   └── 失败 → AuthenticationFailed
//!   ├── deploy_public_key（幂等写入 authorized_keys）
//!   ├── 重连 + key 认证验证
//!   │   └── 失败 → BootstrapFailed
//!   └── Bootstrapped（含 hint：建议用户手动改 hosts.toml 的 auth=key，ADR-0017 §2.8）
//! ```
//!
//! 关键约束（ADR-0009 / ADR-0017 §2.2）：
//! - 密码经 `CredentialProvider` 获取，仅在 SSH 认证瞬间 `reveal()`，用完立即 drop（Zeroize）
//! - 公钥部署幂等（`grep -qF` 检查已存在，避免重复写入）
//! - **不修改 hosts.toml**：bootstrap 只改变 Remote State（authorized_keys），
//!   Host Policy 是用户意图，配置修改由用户显式完成（hint 仅为建议）
//! - 不修改 `ssh.rs` 逻辑，仅复用 `connect_unauthenticated` / `authenticate_session` /
//!   `authenticate_with_password` 三个公开函数

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client::Handle;
use russh::ChannelMsg;
use serde::Serialize;

use crate::domain::credential::{CredentialError, CredentialProvider, PasswordRequest};
use crate::domain::provider::{Host, TermError};
use crate::infrastructure::ssh::{self, SshClientHandler};
use crate::infrastructure::sshconfig;

/// bootstrap_host 结果（ADR-0009 §3 / ADR-0017 §2.8）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BootstrapResult {
    /// 已有可用 key 认证，无需 bootstrap。
    AlreadyConfigured {
        host: String,
        authentication: String,  // "public_key"
        identity_source: String, // "ssh_agent" / "identity_file"
    },
    /// bootstrap 成功。
    Bootstrapped {
        host: String,
        authentication: String,  // "public_key"
        identity_source: String, // "identity_file"
        /// 非阻塞提示（ADR-0017 §2.8）：host 现已支持 key 认证，建议用户手动
        /// 把 hosts.toml 的 `auth` 改为 `key`。TermBridge **不**自动修改配置
        /// （§2.2 不可变原则）。用 `hint` 而非 `auth` 字段承载——避免调用方
        /// 误以为 TermBridge 已修改 host policy。
        hint: String,
    },
    /// 用户取消密码输入。
    Cancelled { host: String },
    /// 密码错误。
    AuthenticationFailed { host: String },
    /// 公钥部署成功但 key 重连验证失败。
    BootstrapFailed { host: String, reason: String },
}

/// bootstrap 成功后的 hint 文本（ADR-0017 §2.8）。
///
/// bootstrap 只改变 Remote State（authorized_keys 增加了公钥），**不修改**
/// Host Policy（hosts.toml 是用户意图，§2.2 不可变原则）。配置修改必须由
/// 用户显式完成——hint 只是非阻塞建议。
pub const BOOTSTRAP_HINT: &str =
    "Host now supports key authentication; consider changing host policy to auth=key in hosts.toml.";

/// BootstrapHost：把 Host 初始化为长期可 key 认证的 SSH 主机（ADR-0009）。
///
/// 持有 `CredentialProvider` trait 对象，通过依赖注入获取密码，
/// Core 层不直接依赖任何平台 UI API。
pub struct BootstrapHost {
    credential_provider: Arc<dyn CredentialProvider>,
}

impl BootstrapHost {
    pub fn new(credential_provider: Arc<dyn CredentialProvider>) -> Self {
        Self { credential_provider }
    }

    /// 执行 ADR-0009 §4 bootstrap 流程。
    ///
    /// 返回 `BootstrapResult` 描述最终状态；仅在不可预期错误（连接失败、
    /// host key 拒绝等）时返回 `Err(TermError)`。
    pub async fn bootstrap(&self, host_alias: &str) -> Result<BootstrapResult, TermError> {
        // 步骤 1：解析 SSH config（ssh -G）
        let host = sshconfig::resolve(host_alias).await?;

        // 步骤 2-3：连接（不认证，内部校验 host key）+ 尝试 key 认证
        let connected = ssh::connect_unauthenticated(&host).await?;
        let mut session = connected.handle;

        let key_auth_result =
            ssh::authenticate_session(&mut session, &host.user, &host.identity_files).await;

        match key_auth_result {
            Ok(via) => {
                // 已有可用 key 认证，无需 bootstrap
                return Ok(BootstrapResult::AlreadyConfigured {
                    host: host_alias.to_string(),
                    authentication: "public_key".into(),
                    identity_source: via.to_string(),
                });
            }
            Err(TermError::AuthFailed) => {
                // key 认证失败，继续 bootstrap 流程
            }
            Err(e) => return Err(e),
        }
        // session 认证失败后可能不可用，drop 它
        drop(session);

        // 步骤 4：确保有 IdentityFile（无则生成 ed25519 keypair）
        let key_path = ensure_identity_file(&host).await?;

        // 步骤 5：请求密码
        let password = match self
            .credential_provider
            .request_password(PasswordRequest {
                host: host.hostname.clone(),
                user: host.user.clone(),
                reason: "bootstrap: deploy public key to authorized_keys".into(),
            })
            .await
        {
            Ok(p) => p,
            Err(CredentialError::Cancelled) => {
                return Ok(BootstrapResult::Cancelled {
                    host: host_alias.into(),
                });
            }
            Err(CredentialError::HelperFailed(msg)) => {
                return Err(TermError::InvalidArgument(msg));
            }
            Err(CredentialError::Unsupported(msg)) => {
                return Err(TermError::InvalidArgument(msg));
            }
        };

        // 步骤 6：重连 + 密码认证
        let connected = ssh::connect_unauthenticated(&host).await?;
        let mut session = connected.handle;
        let ok =
            ssh::authenticate_with_password(&mut session, &host.user, password.reveal()).await?;
        drop(password); // 立即 Zeroize

        if !ok {
            return Ok(BootstrapResult::AuthenticationFailed {
                host: host_alias.into(),
            });
        }

        // 步骤 7：部署公钥到远端 authorized_keys（幂等）
        let public_key = read_public_key(&key_path).await?;
        deploy_public_key(&mut session, &public_key).await?;

        // 步骤 8：关闭密码连接
        drop(session);

        // 步骤 9：重连 + key 认证验证
        let connected = ssh::connect_unauthenticated(&host).await?;
        let mut session = connected.handle;
        // host.identity_files 可能为空（刚生成的 key），用实际 key_path
        let identity_files = vec![key_path];
        let via =
            match ssh::authenticate_session(&mut session, &host.user, &identity_files).await {
                Ok(v) => v,
                Err(TermError::AuthFailed) => {
                    return Ok(BootstrapResult::BootstrapFailed {
                        host: host_alias.into(),
                        reason: "key auth verification failed after install".into(),
                    });
                }
                Err(e) => return Err(e),
            };

        // 步骤 10：Bootstrapped
        Ok(BootstrapResult::Bootstrapped {
            host: host_alias.into(),
            authentication: "public_key".into(),
            identity_source: via.to_string(),
            // ADR-0017 §2.8：hint 非阻塞提示，不修改 hosts.toml
            hint: BOOTSTRAP_HINT.into(),
        })
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 辅助函数
// ───────────────────────────────────────────────────────────────────────────

/// 确保 Host 有可用的 IdentityFile。
///
/// - `host.identity_files` 非空 → 返回第一个
/// - 为空且 `~/.ssh/id_ed25519` 已存在 → 返回该路径
/// - 为空且不存在 → 调用 `ssh-keygen -t ed25519` 生成 keypair
async fn ensure_identity_file(host: &Host) -> Result<PathBuf, TermError> {
    if let Some(first) = host.identity_files.first() {
        return Ok(first.clone());
    }

    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| TermError::InvalidArgument("cannot determine home dir".into()))?;
    let ssh_dir = PathBuf::from(home).join(".ssh");
    let key_path = ssh_dir.join("id_ed25519");

    if key_path.exists() {
        return Ok(key_path);
    }

    tokio::fs::create_dir_all(&ssh_dir).await.ok();
    let output = tokio::process::Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            key_path.to_str().unwrap(),
            "-N",
            "",
            "-q",
        ])
        .output()
        .await
        .map_err(|e| TermError::InvalidArgument(format!("ssh-keygen failed: {e}")))?;
    if !output.status.success() {
        return Err(TermError::InvalidArgument(format!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(key_path)
}

/// 读取公钥文件内容（`<key_path>.pub`），trim 尾部换行。
async fn read_public_key(key_path: &Path) -> Result<String, TermError> {
    let content = tokio::fs::read_to_string(key_path.with_extension("pub"))
        .await
        .map_err(|e| TermError::InvalidArgument(format!("read public key: {e}")))?;
    Ok(content.trim().to_string())
}

/// 在已认证的 session 上开 channel 执行公钥部署命令（幂等）。
///
/// 命令：确保 `~/.ssh` 存在且权限正确 + 公钥未重复写入。
/// `grep -qF` 检查公钥是否已在 authorized_keys 中，是则跳过追加。
async fn deploy_public_key(
    session: &mut Handle<SshClientHandler>,
    public_key: &str,
) -> Result<(), TermError> {
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|e| TermError::ChannelError(format!("open channel: {e}")))?;

    let cmd = format!(
        r#"mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && grep -qF '{}' ~/.ssh/authorized_keys || echo '{}' >> ~/.ssh/authorized_keys"#,
        public_key, public_key
    );

    channel
        .exec(true, cmd.as_str())
        .await
        .map_err(|e| TermError::ChannelError(format!("exec: {e}")))?;

    let mut exit_code: Option<u32> = None;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                exit_code = Some(exit_status);
            }
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
            Some(_) => continue,
        }
    }

    let _ = channel.eof().await;

    match exit_code {
        Some(0) | None => Ok(()),
        Some(code) => Err(TermError::ChannelError(format!(
            "deploy public key failed: exit code {code}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ADR-0017 §2.8：bootstrap 成功后的 hint 合同 ────────────────────

    #[test]
    fn bootstrapped_serializes_with_hint_and_without_auth() {
        // §2.8：hint 承载"考虑改 host policy"建议；用 hint 而非 auth 字段，
        // 避免调用方误以为 TermBridge 已修改 hosts.toml（§2.2 不可变原则）
        let result = BootstrapResult::Bootstrapped {
            host: "prod".into(),
            authentication: "public_key".into(),
            identity_source: "identity_file".into(),
            hint: BOOTSTRAP_HINT.into(),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["status"], "bootstrapped");
        assert_eq!(json["authentication"], "public_key");
        let hint = json["hint"].as_str().unwrap();
        assert!(hint.contains("auth=key"), "hint 应建议改 auth=key: {hint}");
        assert!(hint.contains("hosts.toml"), "hint 应指向 hosts.toml: {hint}");
        // 无 auth 字段——TermBridge 没有修改 host policy，只是给了建议
        assert!(json.get("auth").is_none(), "hint 不得出现在 auth 字段: {json}");
    }

    #[test]
    fn bootstrapped_hint_is_advisory_not_a_config_change_statement() {
        // §2.8：hint 是"考虑修改"的非阻塞建议，不是"已修改"的陈述
        assert!(
            BOOTSTRAP_HINT.starts_with("Host now supports key authentication; consider"),
            "hint 应以建议语气开头: {BOOTSTRAP_HINT}"
        );
        assert!(
            !BOOTSTRAP_HINT.contains("changed host policy"),
            "hint 不得宣称已修改 host policy: {BOOTSTRAP_HINT}"
        );
    }
}
