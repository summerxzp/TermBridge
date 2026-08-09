//! sshconfig —— `ssh -G <alias>` 解析为 domain::provider::Host（ADR-0006）
//!
//! 复用 OpenSSH 完整 config 解析能力（Include / Match / ProxyJump / Host *），
//! TermBridge 只消费 `ssh -G` 的最终输出，不自己实现 parser。
//!
//! ```text
//! resolve("testhost")
//!   → `ssh -G testhost` 子进程
//!   → 解析 `key value` 行
//!   → Host { name, hostname, user, port, identity_file, proxy_jump }
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use crate::domain::provider::Host;
use crate::domain::provider::TermError;

/// 调用 `ssh -G <alias>` 并解析为 Host。
///
/// `alias` 是 ssh config 里的 Host 别名（或直接 IP/hostname）。
pub async fn resolve(alias: &str) -> Result<Host, TermError> {
    // ssh -G 走 stdio，快速返回，用 tokio::process 异步等
    let output = tokio::process::Command::new("ssh")
        .arg("-G")
        .arg(alias)
        .output()
        .await
        .map_err(|e| TermError::ConnectFailed(format!("spawn `ssh -G {alias}`: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TermError::ConnectFailed(format!(
            "`ssh -G {alias}` failed: {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_ssh_g(alias, &stdout)
}

/// 解析 `ssh -G` 输出文本为 Host（纯函数，便于单测）。
fn parse_ssh_g(alias: &str, stdout: &str) -> Result<Host, TermError> {
    let mut single: HashMap<String, String> = HashMap::new();
    let mut multi: HashMap<String, Vec<String>> = HashMap::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 格式：`key value`（空格分隔，key 全小写）
        let mut parts = line.splitn(2, char::is_whitespace);
        let key = parts.next().unwrap_or("").to_lowercase();
        let value = parts.next().unwrap_or("").trim().to_string();
        if key.is_empty() {
            continue;
        }
        match key.as_str() {
            "identityfile" | "certificatefile" => {
                multi.entry(key).or_default().push(value);
            }
            _ => {
                single.insert(key, value);
            }
        }
    }

    let hostname = single
        .get("hostname")
        .cloned()
        .ok_or_else(|| TermError::ConnectFailed(format!("`ssh -G {alias}` 缺少 hostname")))?;
    let user = single.get("user").cloned().unwrap_or_default();
    let port: u16 = single
        .get("port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(22);
    let proxy_jump = single
        .get("proxyjump")
        .filter(|v| !v.is_empty() && *v != "none")
        .cloned();

    // identityfile：取第一个，展开 ~ 为 home dir
    let identity_file = multi
        .get("identityfile")
        .and_then(|files| files.first())
        .map(|s| expand_tilde(s))
        .filter(|p| p.is_file());

    Ok(Host {
        name: alias.to_string(),
        hostname,
        user,
        port,
        identity_file,
        proxy_jump,
    })
}

/// 展开 `~` 为用户 home 目录。非 `~` 开头原样返回。
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs_or_home() {
            return home.join(rest);
        }
    }
    if path == "~" {
        if let Some(home) = dirs_or_home() {
            return home;
        }
    }
    PathBuf::from(path)
}

/// 取 home 目录（不引入 dirs crate，用 std + 环境变量）。
fn dirs_or_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_G: &str = r#"user testuser
hostname 192.168.88.140
port 22
identityfile ~/.ssh/id_ed25519
identityfile ~/.ssh/id_rsa
userknownhostsfile ~/.ssh/known_hosts
stricthostkeychecking ask
identitiesonly yes
proxyjump none
"#;

    #[test]
    fn parse_basic_fields() {
        let host = parse_ssh_g("testhost", SAMPLE_G).unwrap();
        assert_eq!(host.name, "testhost");
        assert_eq!(host.hostname, "192.168.88.140");
        assert_eq!(host.user, "testuser");
        assert_eq!(host.port, 22);
        assert_eq!(host.proxy_jump, None);
    }

    #[test]
    fn parse_multi_identityfile_takes_first() {
        // 用 tempdir 模拟 identity file，避免依赖真实 ~/.ssh/id_ed25519
        let temp_dir = std::env::temp_dir();
        let temp_key = temp_dir.join("termbridge_test_id_ed25519");
        std::fs::write(&temp_key, "dummy_key_content").unwrap();

        let g = format!(
            "user testuser\nhostname 192.168.88.140\nport 22\nidentityfile {}\nidentityfile /nonexistent_key\n",
            temp_key.display()
        );
        let host = parse_ssh_g("testhost", &g).unwrap();
        assert!(host.identity_file.is_some());
        let p = host.identity_file.unwrap();
        assert!(p.ends_with("termbridge_test_id_ed25519"));

        // cleanup
        std::fs::remove_file(&temp_key).ok();
    }

    #[test]
    fn parse_proxyjump_when_set() {
        let g = r#"user u
hostname h
port 22
proxyjump bastion
"#;
        let host = parse_ssh_g("via", g).unwrap();
        assert_eq!(host.proxy_jump.as_deref(), Some("bastion"));
    }

    #[test]
    fn parse_missing_hostname_errors() {
        let g = r#"user u
port 22
"#;
        let err = parse_ssh_g("bad", g).unwrap_err();
        assert_eq!(err.code(), "CONNECT_FAILED");
    }

    #[test]
    fn expand_tilde_home() {
        // 保存并恢复 HOME，避免影响并行测试
        let saved_home = std::env::var_os("HOME");
        std::env::set_var("HOME", "/tmp/fakehome");

        let p = expand_tilde("~/foo/bar");
        // Windows 上 join 用 \ 分隔，比较 PathBuf 而非字符串
        let expected = PathBuf::from("/tmp/fakehome").join("foo").join("bar");
        assert_eq!(p, expected);
        assert!(!p.to_string_lossy().contains('~'));

        let p2 = expand_tilde("/abs/path");
        assert_eq!(p2, PathBuf::from("/abs/path"));

        // 恢复
        match saved_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn parse_custom_port() {
        let g = r#"user u
hostname h
port 2222
"#;
        let host = parse_ssh_g("custom", g).unwrap();
        assert_eq!(host.port, 2222);
    }
}
