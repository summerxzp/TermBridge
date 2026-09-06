//! username_store —— per-host 用户名记忆（最新优先）。
//!
//! 用户在凭据对话框中填写过一次用户名后（对话框支持编辑用户名，Windows 为
//! CredUIPromptForCredentialsW 的 in/out 用户名缓冲），把它记住作为该 host 的
//! 默认用户名，下次连接时优先于 ssh config 的 `User`（优先级链见
//! `sshconfig::resolve` 文档）。这不是凭据：只存用户名（非机密），不存密码。
//!
//! 与 Host Policy（ADR-0017 §2.2 不可变原则）的区别：hosts.toml 是用户**显式**
//! 编辑的意图配置；username-memory.json 是 TermBridge 作为认证副作用的**记忆**
//! （用户在对话框中显式输入即视为授权记住），二者目录相同但文件分离。
//!
//! 存储：配置目录（复用 `host_policy::default_config_dir`）下的
//! `username-memory.json`，形如 `{ "hosts": { "<hostname>": "<username>" } }`。
//!
//! 容错策略：
//! - 读：文件缺失 / 解析失败 → 视为空记忆（warn），下次 `remember` 覆盖修复
//! - 写：原子写（tmp + rename），失败仅 warn，绝不影响调用方（认证已成功，
//!   记不住用户名只是退化为下次再填一次）

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::application::host_policy::default_config_dir;

/// 记忆文件名（位于配置目录下）。
const MEMORY_FILE: &str = "username-memory.json";

/// 记忆文件 JSON 结构：`{ "hosts": { "<hostname>": "<username>" } }`。
#[derive(Debug, Default, Serialize, Deserialize)]
struct UsernameMemory {
    /// hostname → 用户名（最新优先：remember 直接覆盖）
    hosts: HashMap<String, String>,
}

/// 查询 host 记忆的用户名（None = 未填写过，调用方回退 ssh config User）。
pub fn lookup(hostname: &str) -> Option<String> {
    let path = memory_path()?;
    load_from(&path).hosts.get(hostname).cloned()
}

/// 记住 host 的用户名（最新优先：直接覆盖旧值）。
///
/// 失败仅 warn——认证已成功，记忆失败只是下次需要再填一次，不应影响调用方。
pub fn remember(hostname: &str, user: &str) {
    let Some(path) = memory_path() else {
        tracing::warn!("username memory: no config dir available, cannot remember user");
        return;
    };
    let mut memory = load_from(&path);
    memory.hosts.insert(hostname.to_string(), user.to_string());
    if let Err(e) = save_to(&path, &memory) {
        tracing::warn!("username memory: write failed (ignored): {e}");
    }
}

/// 记忆文件完整路径（配置目录不可解析 → None，调用方 warn 降级）。
fn memory_path() -> Option<PathBuf> {
    Some(default_config_dir().join(MEMORY_FILE))
}

/// 从指定路径读取记忆（纯函数，供单测）。文件缺失 / 解析失败 → 空记忆。
fn load_from(path: &Path) -> UsernameMemory {
    match fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str(&raw) {
            Ok(memory) => memory,
            Err(e) => {
                // 损坏文件：warn + 视为空（下次 remember 覆盖修复）
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "username memory: parse failed, treating as empty"
                );
                UsernameMemory::default()
            }
        },
        Err(_) => UsernameMemory::default(), // 文件不存在：首次使用，正常
    }
}

/// 原子写记忆到指定路径（tmp + rename，供单测）。
fn save_to(path: &Path, memory: &UsernameMemory) -> std::io::Result<()> {
    let json = serde_json::to_vec(memory)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // 原子写：先写 tmp 再 rename，避免并发 / 中断留下半截文件
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独立的临时文件（进程 id + 测试名，避免并行竞争）。
    fn temp_memory_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "termbridge-username-memory-test-{}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn roundtrip_remember_and_lookup() {
        let path = temp_memory_path("roundtrip");
        let _ = fs::remove_file(&path);

        // 空 → None
        assert_eq!(load_from(&path).hosts.get("prod"), None);

        // remember → 落盘
        let mut memory = load_from(&path);
        memory.hosts.insert("prod".into(), "alice".into());
        save_to(&path, &memory).unwrap();

        // 重新读回 → Some
        let loaded = load_from(&path);
        assert_eq!(loaded.hosts.get("prod"), Some(&"alice".to_string()));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn latest_wins_overwrite() {
        let path = temp_memory_path("latest-wins");
        let _ = fs::remove_file(&path);

        // 两次 remember（模拟用户改了用户名）：最新值覆盖旧值
        let mut memory = load_from(&path);
        memory.hosts.insert("prod".into(), "root".into());
        save_to(&path, &memory).unwrap();
        let mut memory = load_from(&path);
        memory.hosts.insert("prod".into(), "alice".into());
        save_to(&path, &memory).unwrap();

        assert_eq!(load_from(&path).hosts.get("prod"), Some(&"alice".to_string()));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_treated_as_empty_and_repaired_by_save() {
        let path = temp_memory_path("corrupt");
        fs::write(&path, b"{ not valid json !!!").unwrap();

        // 损坏 → 空（不 panic）
        assert!(load_from(&path).hosts.is_empty());

        // 下一次 remember 覆盖修复
        let mut memory = load_from(&path);
        memory.hosts.insert("prod".into(), "alice".into());
        save_to(&path, &memory).unwrap();
        assert_eq!(load_from(&path).hosts.get("prod"), Some(&"alice".to_string()));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn hosts_are_isolated() {
        let path = temp_memory_path("isolated");
        let _ = fs::remove_file(&path);

        let mut memory = load_from(&path);
        memory.hosts.insert("host-a".into(), "alice".into());
        memory.hosts.insert("host-b".into(), "bob".into());
        save_to(&path, &memory).unwrap();

        let loaded = load_from(&path);
        assert_eq!(loaded.hosts.get("host-a"), Some(&"alice".to_string()));
        assert_eq!(loaded.hosts.get("host-b"), Some(&"bob".to_string()));
        assert_eq!(loaded.hosts.get("host-c"), None);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_empty_not_error() {
        // 不存在的路径 → 空记忆（首次使用是正常状态，非错误）
        let path = temp_memory_path("missing-never-written");
        let _ = fs::remove_file(&path);
        assert!(load_from(&path).hosts.is_empty());
    }
}
