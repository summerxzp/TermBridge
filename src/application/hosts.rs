//! HostManager —— 从 ~/.ssh/config 枚举 Host 别名（§6 list_hosts 工具的后端）
//!
//! 设计（ADR-0006）：
//! - `list_hosts`：快速解析 ~/.ssh/config 的 `Host` 行，返回别名列表（不展开 Include/Match）
//! - 实际连接参数由 `sshconfig::resolve(alias)`（`ssh -G`）在 open_session 时解析
//!
//! 这样 list_hosts 不需要为每个 Host 调一次 `ssh -G`（慢），只做轻量 config 扫描。

use std::path::PathBuf;

/// SSH config 中发现的主机条目。
#[derive(Debug, Clone, serde::Serialize)]
pub struct HostEntry {
    /// Host 别名（ssh config Host 行的 token）
    pub alias: String,
    /// HostName（若 config 中显式指定）
    pub hostname: Option<String>,
}

/// HostManager：管理 ~/.ssh/config 的 Host 列表。
pub struct HostManager {
    config_path: PathBuf,
}

impl HostManager {
    /// 用默认 ~/.ssh/config 路径创建。
    pub fn new() -> Self {
        Self {
            config_path: default_ssh_config_path(),
        }
    }

    /// 用指定 config 路径创建（测试用）。
    pub fn with_config_path(path: PathBuf) -> Self {
        Self { config_path: path }
    }

    /// 列出 config 中所有 Host 别名（过滤通配符模式）。
    pub fn list_hosts(&self) -> Vec<HostEntry> {
        let content = match std::fs::read_to_string(&self.config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %self.config_path.display(),
                    error = %e,
                    "failed to read ssh config, returning empty host list"
                );
                return Vec::new();
            }
        };
        parse_host_entries(&content)
    }
}

impl Default for HostManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 解析 ssh config 文本，提取 Host 条目。
///
/// 规则：
/// - 匹配 `Host <aliases...>` 行（大小写不敏感）
/// - 每个别名过滤通配符（含 `*` 或 `?`）
/// - 紧随其后的 `HostName <addr>` 行关联到最近的 Host（若存在）
fn parse_host_entries(content: &str) -> Vec<HostEntry> {
    let mut entries = Vec::new();
    let mut current_aliases: Vec<String> = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let key = parts.next().unwrap_or("").to_lowercase();
        let value = parts.next().unwrap_or("").trim().to_string();

        match key.as_str() {
            "host" => {
                // 每个 token 是一个别名
                current_aliases = value
                    .split_whitespace()
                    .filter(|a| !a.contains('*') && !a.contains('?'))
                    .map(String::from)
                    .collect();
                for alias in &current_aliases {
                    entries.push(HostEntry {
                        alias: alias.clone(),
                        hostname: None,
                    });
                }
            }
            "hostname" => {
                // 关联到最近一批 Host（填充 hostname）
                if !current_aliases.is_empty() && !value.is_empty() {
                    for alias in &current_aliases {
                        if let Some(entry) = entries.iter_mut().find(|e| &e.alias == alias) {
                            entry.hostname = Some(value.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    entries
}

/// 默认 ~/.ssh/config 路径。
fn default_ssh_config_path() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let home = PathBuf::from(home.unwrap_or_default());
    home.join(".ssh").join("config")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_config() {
        let config = r#"
Host 192.0.2.200
    HostName 192.0.2.200
    User root

Host gitee.com
    HostName gitee.com
    User git

Host *
    User default
"#;
        let entries = parse_host_entries(config);
        assert_eq!(entries.len(), 2); // 通配符 * 被过滤
        assert_eq!(entries[0].alias, "192.0.2.200");
        assert_eq!(entries[0].hostname.as_deref(), Some("192.0.2.200"));
        assert_eq!(entries[1].alias, "gitee.com");
    }

    #[test]
    fn parse_multi_alias_host() {
        let config = "Host dev staging backup\n    HostName 10.0.0.1\n";
        let entries = parse_host_entries(config);
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.hostname.as_deref() == Some("10.0.0.1")));
    }

    #[test]
    fn parse_empty_config() {
        let entries = parse_host_entries("");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_comments_and_blanks() {
        let config = "# comment\n\nHost foo\n  # indented comment\n  HostName 1.2.3.4\n";
        let entries = parse_host_entries(config);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].alias, "foo");
        assert_eq!(entries[0].hostname.as_deref(), Some("1.2.3.4"));
    }
}
