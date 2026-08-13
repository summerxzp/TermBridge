//! HostPolicy —— per-host 连接偏好配置（ADR-0017）。
//!
//! Host Policy 是 Application 层的**用户意图配置**，回答：
//! > 这台 Host 默认应该怎么使用 TermBridge？
//!
//! 两个维度：
//! - `auth`：认证方式偏好（key / password / auto）
//! - `session`：session 持久化偏好（standard / persistent）
//!
//! 优先级解析：
//! ```text
//! explicit tool argument
//!     > host policy
//!     > system default
//! ```
//!
//! 不可变原则（ADR-0017 §2.2）：
//! Host Policy = 用户意图。TermBridge 永不作为连接/认证/bootstrap/session
//! 操作的副作用隐式修改 Host Policy。配置文件只由用户显式编辑或 GUI 显式写入。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ───────────────────────────────────────────────────────────────────────────
// 枚举
// ───────────────────────────────────────────────────────────────────────────

/// 认证方式偏好（ADR-0017 §2.3）。
///
/// - `Key`：仅 SSH Agent / IdentityFile 认证，失败不弹密码
/// - `Password`：每次连接通过 CredentialProvider 请求密码，不持久化、不部署 key
/// - `Auto`：当前等价于 `Key`，不预留 password fallback 语义
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// 仅 SSH Agent / IdentityFile 认证。
    Key,
    /// 每次连接通过 CredentialProvider 请求密码。
    Password,
    /// 当前等价于 `Key`（ADR-0017 §2.3：不预留 fallback 语义）。
    Auto,
}

/// Session 持久化偏好（ADR-0017 §2.3）。
///
/// - `Standard`：不部署远端 runtime，SSH 断开 → session 丢失
/// - `Persistent`：允许部署并管理远端 runtime（ADR-0004 路径）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    /// 不部署远端 TermBridge runtime。
    Standard,
    /// 允许部署并管理远端 runtime。
    Persistent,
}

/// `persistent: bool` → SessionMode 映射（open_session 显式参数，ADR-0017 §2.3）。
impl From<bool> for SessionMode {
    fn from(persistent: bool) -> Self {
        if persistent {
            SessionMode::Persistent
        } else {
            SessionMode::Standard
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Display（CLI / GUI 展示用，ADR-0017 §4 第 6 步）
// ───────────────────────────────────────────────────────────────────────────

impl std::fmt::Display for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AuthMode::Key => "key",
            AuthMode::Password => "password",
            AuthMode::Auto => "auto",
        })
    }
}

impl std::fmt::Display for SessionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SessionMode::Standard => "standard",
            SessionMode::Persistent => "persistent",
        })
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 配置结构
// ───────────────────────────────────────────────────────────────────────────

/// 单个 host 的策略（两字段均可省略，省略时用 system default）。
///
/// 对应 `hosts.toml`：
/// ```toml
/// [hosts.prod]
/// auth = "key"
/// session = "standard"
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostPolicy {
    /// 认证方式偏好。省略 → system default (Auto)。
    pub auth: Option<AuthMode>,
    /// session 持久化偏好。省略 → system default (Standard)。
    pub session: Option<SessionMode>,
}

/// 整个 hosts.toml 配置文件。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostPolicyConfig {
    /// per-host 策略表，key 为 host 别名（与 ~/.ssh/config 的 Host 别名一致）。
    #[serde(default)]
    pub hosts: std::collections::HashMap<String, HostPolicy>,
}

// ───────────────────────────────────────────────────────────────────────────
// ResolvedPolicy：resolver 输出（已合并优先级，字段非 None）
// ───────────────────────────────────────────────────────────────────────────

/// 解析后的最终策略（ADR-0017 §2.4 优先级合并结果）。
///
/// 字段非 None：已按 `explicit > host policy > system default` 合并。
/// 调用方直接使用即可，无需再处理 None。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPolicy {
    pub auth: AuthMode,
    pub session: SessionMode,
}

impl ResolvedPolicy {
    /// system default（ADR-0017 §2.5）：auth=Auto, session=Standard。
    pub const fn default_policy() -> Self {
        Self {
            auth: AuthMode::Auto,
            session: SessionMode::Standard,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// HostPolicyResolver
// ───────────────────────────────────────────────────────────────────────────

/// HostPolicy 解析器：加载 hosts.toml + 按优先级合并（ADR-0017 §2.4）。
///
/// 不可变原则（ADR-0017 §2.2）：本结构只读不写。配置文件由用户显式编辑，
/// TermBridge 永不作为操作副作用修改它。
pub struct HostPolicyResolver {
    config: HostPolicyConfig,
}

impl HostPolicyResolver {
    /// 从默认平台路径加载（ADR-0017 §2.9）。
    ///
    /// 容错策略（向后兼容）：
    /// - 文件不存在 → 返回空配置（所有 host 走 system default）
    /// - 解析失败 → 记录 WARN，返回空配置（不 panic，不阻断启动）
    pub fn load_default() -> Self {
        let path = default_config_path();
        Self::load_from(&path)
    }

    /// 从指定路径加载（测试用）。
    pub fn load_from(path: &Path) -> Self {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                // 文件不存在是正常情况（首次使用），不 WARN；其他错误 WARN
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to read host policy config, falling back to empty config"
                    );
                }
                return Self {
                    config: HostPolicyConfig::default(),
                };
            }
        };

        match toml::from_str::<HostPolicyConfig>(&content) {
            Ok(cfg) => {
                warn_on_nested_dotted_host_keys(&content);
                tracing::info!(
                    path = %path.display(),
                    host_count = cfg.hosts.len(),
                    "host policy config loaded"
                );
                Self { config: cfg }
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to parse host policy config, falling back to empty config"
                );
                Self {
                    config: HostPolicyConfig::default(),
                }
            }
        }
    }

    /// 用空配置创建（所有 host 走 system default，测试用）。
    pub fn empty() -> Self {
        Self {
            config: HostPolicyConfig::default(),
        }
    }

    /// 用预加载的配置创建（测试用）。
    pub fn with_config(config: HostPolicyConfig) -> Self {
        Self { config }
    }

    /// 解析某 host 的最终策略（ADR-0017 §2.4 优先级）。
    ///
    /// 优先级：`explicit > host policy > system default`
    ///
    /// - `explicit_auth` / `explicit_session`：调用方显式传入的参数（如
    ///   `open_session(persistent=true)` 中的 `Some(true)`）。None 表示未显式指定。
    /// - 返回的 `ResolvedPolicy` 字段非 None，调用方直接使用。
    pub fn resolve(
        &self,
        host_alias: &str,
        explicit_auth: Option<AuthMode>,
        explicit_session: Option<SessionMode>,
    ) -> ResolvedPolicy {
        let host_policy = self.config.hosts.get(host_alias);
        let default = ResolvedPolicy::default_policy();

        // 优先级合并：explicit > host policy > system default
        let auth = explicit_auth
            .or_else(|| host_policy.and_then(|p| p.auth))
            .unwrap_or(default.auth);

        let session = explicit_session
            .or_else(|| host_policy.and_then(|p| p.session))
            .unwrap_or(default.session);

        ResolvedPolicy { auth, session }
    }

    /// 查询某 host 的策略原值（未经优先级合并，字段可能为 None）。
    ///
    /// 用于 GUI / CLI 展示当前配置。host 未配置时返回 None。
    pub fn get_host_policy(&self, host_alias: &str) -> Option<&HostPolicy> {
        self.config.hosts.get(host_alias)
    }

    /// 列出所有已配置的 host 别名。
    pub fn list_configured_hosts(&self) -> Vec<&str> {
        self.config.hosts.keys().map(|s| s.as_str()).collect()
    }
}

/// 防静默失效：`[hosts.192.168.1.180]` 在 TOML 里会把点号解析为**嵌套表**
/// （hosts → 192 → 168 → 1 → 180），serde 忽略未知字段后得到垃圾条目
/// `hosts.192 = {}`，用户以为配了 `192.168.1.180` 实际没有 —— IP 别名 host
/// 是常见用法，必须显式警告。正确写法：`[hosts."192.168.1.180"]`（引号）。
fn warn_on_nested_dotted_host_keys(content: &str) {
    let Ok(v) = toml::from_str::<toml::Value>(content) else {
        return;
    };
    let Some(hosts) = v.get("hosts").and_then(|h| h.as_table()) else {
        return;
    };
    for (alias, entry) in hosts {
        let Some(table) = entry.as_table() else {
            continue;
        };
        let bad: Vec<&String> = table
            .keys()
            .filter(|k| k.as_str() != "auth" && k.as_str() != "session")
            .collect();
        if !bad.is_empty() {
            tracing::warn!(
                alias = %alias,
                fields = ?bad,
                "host entry contains non-policy fields: likely an unquoted dotted alias \
                 (e.g. [hosts.192.168.1.180] parses as nested tables). Use quoted key: \
                 [hosts.\"{alias}\"]"
            );
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 平台路径（ADR-0017 §2.9）
// ───────────────────────────────────────────────────────────────────────────

/// 默认 hosts.toml 平台路径（ADR-0017 §2.9）。
///
/// - Linux / macOS：`~/.config/termbridge/hosts.toml`（XDG）
/// - Windows：`%APPDATA%\TermBridge\hosts.toml`
///
/// macOS 特殊处理：`dirs::config_dir()` 在 macOS 返回 `~/Library/Application Support`
/// （Apple 原生惯例，适合 GUI 应用用 plist 管理配置）。但 TermBridge 是 CLI/开发者
/// 工具，hosts.toml 是用户手写的 toml，应遵循 XDG 惯例（`~/.config`），与 git/vim/
/// tmux 等所有 CLI 工具一致。参考：https://becca.ooo/blog/macos-dotfiles/
pub fn default_config_path() -> PathBuf {
    let base = if cfg!(target_os = "macos") {
        // macOS: 强制 XDG 风格（~/.config），尊重 XDG_CONFIG_HOME 环境变量
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".config")
            })
    } else {
        // Linux: dirs::config_dir() = ~/.config（尊重 XDG_CONFIG_HOME）
        // Windows: dirs::config_dir() = %APPDATA%
        dirs::config_dir().unwrap_or_else(|| {
            tracing::warn!("dirs::config_dir() returned None, falling back to home dir");
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
        })
    };
    // Windows 用 TermBridge 目录（与 agentd 本地路径、ADR-0017 §2.9 一致，
    // %APPDATA%\TermBridge\hosts.toml）；Unix 用 XDG 惯例小写 termbridge。
    let app_dir = if cfg!(windows) { "TermBridge" } else { "termbridge" };
    base.join(app_dir).join("hosts.toml")
}

// ───────────────────────────────────────────────────────────────────────────
// 测试
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ── 枚举序列化 ───────────────────────────────────────────────────

    #[test]
    fn auth_mode_serde_lowercase() {
        assert_eq!(
            serde_json::to_string(&AuthMode::Key).unwrap(),
            "\"key\""
        );
        assert_eq!(
            serde_json::to_string(&AuthMode::Password).unwrap(),
            "\"password\""
        );
        assert_eq!(
            serde_json::to_string(&AuthMode::Auto).unwrap(),
            "\"auto\""
        );

        assert_eq!(
            serde_json::from_str::<AuthMode>("\"key\"").unwrap(),
            AuthMode::Key
        );
        assert_eq!(
            serde_json::from_str::<AuthMode>("\"password\"").unwrap(),
            AuthMode::Password
        );
        assert_eq!(
            serde_json::from_str::<AuthMode>("\"auto\"").unwrap(),
            AuthMode::Auto
        );
    }

    #[test]
    fn session_mode_serde_lowercase() {
        assert_eq!(
            serde_json::to_string(&SessionMode::Standard).unwrap(),
            "\"standard\""
        );
        assert_eq!(
            serde_json::to_string(&SessionMode::Persistent).unwrap(),
            "\"persistent\""
        );

        assert_eq!(
            serde_json::from_str::<SessionMode>("\"standard\"").unwrap(),
            SessionMode::Standard
        );
        assert_eq!(
            serde_json::from_str::<SessionMode>("\"persistent\"").unwrap(),
            SessionMode::Persistent
        );
    }

    #[test]
    fn session_mode_from_bool() {
        // open_session 的 persistent 显式参数映射（ADR-0017 §2.3）
        assert_eq!(SessionMode::from(true), SessionMode::Persistent);
        assert_eq!(SessionMode::from(false), SessionMode::Standard);
    }

    #[test]
    fn auth_mode_display_lowercase() {
        // CLI / GUI 展示用（ADR-0017 §4 第 6 步）
        assert_eq!(AuthMode::Key.to_string(), "key");
        assert_eq!(AuthMode::Password.to_string(), "password");
        assert_eq!(AuthMode::Auto.to_string(), "auto");
    }

    #[test]
    fn session_mode_display_lowercase() {
        assert_eq!(SessionMode::Standard.to_string(), "standard");
        assert_eq!(SessionMode::Persistent.to_string(), "persistent");
    }

    #[test]
    fn invalid_auth_mode_rejected() {
        assert!(serde_json::from_str::<AuthMode>("\"unknown\"").is_err());
    }

    // ── 优先级解析（核心）────────────────────────────────────────────

    #[test]
    fn resolve_no_config_no_explicit_uses_system_default() {
        let resolver = HostPolicyResolver::empty();
        let p = resolver.resolve("prod", None, None);
        assert_eq!(p, ResolvedPolicy::default_policy());
        assert_eq!(p.auth, AuthMode::Auto);
        assert_eq!(p.session, SessionMode::Standard);
    }

    #[test]
    fn resolve_host_policy_used_when_no_explicit() {
        let mut config = HostPolicyConfig::default();
        config.hosts.insert(
            "prod".into(),
            HostPolicy {
                auth: Some(AuthMode::Key),
                session: Some(SessionMode::Persistent),
            },
        );
        let resolver = HostPolicyResolver::with_config(config);

        let p = resolver.resolve("prod", None, None);
        assert_eq!(p.auth, AuthMode::Key);
        assert_eq!(p.session, SessionMode::Persistent);
    }

    #[test]
    fn resolve_explicit_overrides_host_policy() {
        let mut config = HostPolicyConfig::default();
        config.hosts.insert(
            "prod".into(),
            HostPolicy {
                auth: Some(AuthMode::Key),
                session: Some(SessionMode::Persistent),
            },
        );
        let resolver = HostPolicyResolver::with_config(config);

        // 显式参数优先
        let p = resolver.resolve(
            "prod",
            Some(AuthMode::Password),
            Some(SessionMode::Standard),
        );
        assert_eq!(p.auth, AuthMode::Password);
        assert_eq!(p.session, SessionMode::Standard);
    }

    #[test]
    fn resolve_explicit_auth_only_session_falls_back_to_host_policy() {
        let mut config = HostPolicyConfig::default();
        config.hosts.insert(
            "prod".into(),
            HostPolicy {
                auth: Some(AuthMode::Key),
                session: Some(SessionMode::Persistent),
            },
        );
        let resolver = HostPolicyResolver::with_config(config);

        // 只显式传 auth，session 走 host policy
        let p = resolver.resolve("prod", Some(AuthMode::Password), None);
        assert_eq!(p.auth, AuthMode::Password);
        assert_eq!(p.session, SessionMode::Persistent);
    }

    #[test]
    fn resolve_explicit_session_only_auth_falls_back_to_host_policy() {
        let mut config = HostPolicyConfig::default();
        config.hosts.insert(
            "prod".into(),
            HostPolicy {
                auth: Some(AuthMode::Key),
                session: Some(SessionMode::Persistent),
            },
        );
        let resolver = HostPolicyResolver::with_config(config);

        // 只显式传 session，auth 走 host policy
        let p = resolver.resolve("prod", None, Some(SessionMode::Standard));
        assert_eq!(p.auth, AuthMode::Key);
        assert_eq!(p.session, SessionMode::Standard);
    }

    #[test]
    fn resolve_partial_host_policy_falls_back_to_system_default() {
        let mut config = HostPolicyConfig::default();
        config.hosts.insert(
            "prod".into(),
            HostPolicy {
                auth: Some(AuthMode::Key),
                session: None, // 省略 → system default
            },
        );
        let resolver = HostPolicyResolver::with_config(config);

        let p = resolver.resolve("prod", None, None);
        assert_eq!(p.auth, AuthMode::Key);
        assert_eq!(p.session, SessionMode::Standard); // system default
    }

    #[test]
    fn resolve_unknown_host_uses_system_default() {
        let mut config = HostPolicyConfig::default();
        config.hosts.insert(
            "dev".into(),
            HostPolicy {
                auth: Some(AuthMode::Key),
                session: Some(SessionMode::Persistent),
            },
        );
        let resolver = HostPolicyResolver::with_config(config);

        // 未配置的 host → system default
        let p = resolver.resolve("unknown", None, None);
        assert_eq!(p.auth, AuthMode::Auto);
        assert_eq!(p.session, SessionMode::Standard);
    }

    // ── 优先级组合矩阵 ───────────────────────────────────────────────

    #[test]
    fn resolve_priority_matrix() {
        // 构造矩阵：host policy = (Key, Persistent)，测试所有 explicit 组合
        let mut config = HostPolicyConfig::default();
        config.hosts.insert(
            "prod".into(),
            HostPolicy {
                auth: Some(AuthMode::Key),
                session: Some(SessionMode::Persistent),
            },
        );
        let resolver = HostPolicyResolver::with_config(config);

        // (explicit, host, expected) 三元组
        // auth: None → Key (host); Some(Password) → Password (explicit)
        // session: None → Persistent (host); Some(Standard) → Standard (explicit)
        let cases = [
            (None, None, AuthMode::Key, SessionMode::Persistent),
            (
                Some(AuthMode::Password),
                None,
                AuthMode::Password,
                SessionMode::Persistent,
            ),
            (
                None,
                Some(SessionMode::Standard),
                AuthMode::Key,
                SessionMode::Standard,
            ),
            (
                Some(AuthMode::Password),
                Some(SessionMode::Standard),
                AuthMode::Password,
                SessionMode::Standard,
            ),
        ];

        for (exp_auth, exp_session, want_auth, want_session) in cases {
            let p = resolver.resolve("prod", exp_auth, exp_session);
            assert_eq!(p.auth, want_auth, "auth mismatch for ({exp_auth:?}, {exp_session:?})");
            assert_eq!(
                p.session, want_session,
                "session mismatch for ({exp_auth:?}, {exp_session:?})"
            );
        }
    }

    // ── 配置文件加载与容错 ───────────────────────────────────────────

    #[test]
    fn load_from_nonexistent_file_returns_empty() {
        let resolver =
            HostPolicyResolver::load_from(&PathBuf::from("/nonexistent/hosts.toml"));
        let p = resolver.resolve("any", None, None);
        assert_eq!(p, ResolvedPolicy::default_policy());
        assert!(resolver.list_configured_hosts().is_empty());
    }

    #[test]
    fn load_from_valid_toml() {
        let toml = r#"
[hosts.prod]
auth = "key"
session = "standard"

[hosts.dev]
auth = "password"
session = "persistent"

[hosts.legacy]
session = "standard"
"#;
        let tmp = temp_file(toml);
        let resolver = HostPolicyResolver::load_from(&tmp);

        let p = resolver.resolve("prod", None, None);
        assert_eq!(p.auth, AuthMode::Key);
        assert_eq!(p.session, SessionMode::Standard);

        let p = resolver.resolve("dev", None, None);
        assert_eq!(p.auth, AuthMode::Password);
        assert_eq!(p.session, SessionMode::Persistent);

        // legacy 只配 session，auth 走 system default
        let p = resolver.resolve("legacy", None, None);
        assert_eq!(p.auth, AuthMode::Auto);
        assert_eq!(p.session, SessionMode::Standard);
    }

    #[test]
    fn load_from_invalid_toml_returns_empty() {
        let tmp = temp_file("this is not valid toml [[[[");
        let resolver = HostPolicyResolver::load_from(&tmp);

        // 解析失败 → 空配置 → system default
        let p = resolver.resolve("any", None, None);
        assert_eq!(p, ResolvedPolicy::default_policy());
    }

    #[test]
    fn load_from_empty_file_returns_empty() {
        let tmp = temp_file("");
        let resolver = HostPolicyResolver::load_from(&tmp);
        let p = resolver.resolve("any", None, None);
        assert_eq!(p, ResolvedPolicy::default_policy());
    }

    #[test]
    fn load_from_partial_host_entry() {
        let toml = r#"
[hosts.prod]
auth = "key"
# session 省略
"#;
        let tmp = temp_file(toml);
        let resolver = HostPolicyResolver::load_from(&tmp);

        let p = resolver.resolve("prod", None, None);
        assert_eq!(p.auth, AuthMode::Key);
        assert_eq!(p.session, SessionMode::Standard); // system default
    }

    #[test]
    fn load_from_invalid_auth_value_returns_empty() {
        // auth 值不在枚举内 → toml 解析失败 → 整个配置回退空
        let toml = r#"
[hosts.prod]
auth = "biometric"
"#;
        let tmp = temp_file(toml);
        let resolver = HostPolicyResolver::load_from(&tmp);

        let p = resolver.resolve("prod", None, None);
        assert_eq!(p, ResolvedPolicy::default_policy());
    }

    // ── TOML 点号别名防护（IP 别名必须加引号）────────────────────────

    #[test]
    fn unquoted_dotted_alias_parses_as_nested_junk() {
        // [hosts.192.168.1.180] → TOML 嵌套表 → hosts 表出现 "192" 垃圾条目。
        // 这是 silent no-op 陷阱，warn_on_nested_dotted_host_keys 应识别。
        let toml = r#"
[hosts.192.168.1.180]
auth = "key"
"#;
        let cfg = toml::from_str::<HostPolicyConfig>(toml).unwrap();
        // 垃圾条目:hosts.192 = {}（"168" 被当作未知字段忽略）
        assert!(cfg.hosts.contains_key("192"));
        assert!(!cfg.hosts.contains_key("192.168.1.180"));
    }

    #[test]
    fn quoted_dotted_alias_parses_correctly() {
        // 正确写法:[hosts."192.168.1.180"] → 单 key "192.168.1.180"
        let toml = r#"
[hosts."192.168.1.180"]
auth = "key"
"#;
        let cfg = toml::from_str::<HostPolicyConfig>(toml).unwrap();
        assert!(cfg.hosts.contains_key("192.168.1.180"));
        assert_eq!(cfg.hosts["192.168.1.180"].auth, Some(AuthMode::Key));
    }

    #[test]
    fn load_from_warns_on_unquoted_dotted_alias_but_keeps_valid_entries() {
        // 防护:脏条目 + 合法条目并存时,合法条目仍生效(WARN 不阻断)
        let toml = r#"
[hosts.192.168.1.180]
auth = "key"

[hosts.prod]
auth = "password"
"#;
        let tmp = temp_file(toml);
        let resolver = HostPolicyResolver::load_from(&tmp);
        // prod 正常生效
        assert_eq!(
            resolver.resolve("prod", None, None).auth,
            AuthMode::Password
        );
    }

    // ── 查询接口 ─────────────────────────────────────────────────────

    #[test]
    fn get_host_policy_returns_configured() {
        let mut config = HostPolicyConfig::default();
        config.hosts.insert(
            "prod".into(),
            HostPolicy {
                auth: Some(AuthMode::Key),
                session: Some(SessionMode::Standard),
            },
        );
        let resolver = HostPolicyResolver::with_config(config);

        let p = resolver.get_host_policy("prod").unwrap();
        assert_eq!(p.auth, Some(AuthMode::Key));
        assert_eq!(p.session, Some(SessionMode::Standard));
    }

    #[test]
    fn get_host_policy_returns_none_for_unknown() {
        let resolver = HostPolicyResolver::empty();
        assert!(resolver.get_host_policy("unknown").is_none());
    }

    #[test]
    fn list_configured_hosts() {
        let mut config = HostPolicyConfig::default();
        config.hosts.insert(
            "prod".into(),
            HostPolicy::default(),
        );
        config.hosts.insert(
            "dev".into(),
            HostPolicy::default(),
        );
        let resolver = HostPolicyResolver::with_config(config);

        let mut hosts = resolver.list_configured_hosts();
        hosts.sort();
        assert_eq!(hosts, vec!["dev", "prod"]);
    }

    // ── 平台路径 ─────────────────────────────────────────────────────

    #[test]
    fn default_config_path_returns_some_path() {
        let path = default_config_path();
        // 不断言具体路径（跨平台），只断言非空且以 hosts.toml 结尾
        assert!(path.ends_with("hosts.toml"));
        assert!(path.components().count() > 1);
        // 目录段与 ADR-0017 §2.9 一致：Windows TermBridge / Unix termbridge
        #[cfg(windows)]
        assert!(
            path.to_string_lossy().contains("TermBridge"),
            "Windows 路径应用 TermBridge 目录: {}",
            path.display()
        );
        #[cfg(not(windows))]
        assert!(
            path.to_string_lossy().contains("termbridge"),
            "Unix 路径应用小写 termbridge 目录: {}",
            path.display()
        );
    }

    // ── 辅助 ─────────────────────────────────────────────────────────

    /// 全局计数器，确保多测试并发时临时文件名唯一。
    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// 创建唯一临时 toml 文件，返回路径。
    ///
    /// cargo test 默认多线程，用 PID + 全局递增计数器避免并发冲突。
    /// 不自动清理（测试进程退出时 OS 临时目录会被清理），保持测试代码简单。
    fn temp_file(content: &str) -> PathBuf {
        let seq = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut tmp = std::env::temp_dir();
        tmp.push(format!(
            "termbridge_host_policy_test_{}_{seq}.toml",
            std::process::id()
        ));
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        tmp
    }
}
