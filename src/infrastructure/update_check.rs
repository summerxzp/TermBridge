//! 更新检查（借鉴 chrome-devtools-mcp 的 update-check 机制）：
//! - 启动时同步读本地缓存，若缓存记录的最新版本 > 当前版本 → stderr 提示（仅提示，不自动安装）
//! - 缓存超过 24h 或缺失 → 先占位刷新检查时间（防并发重复请求），再后台线程异步查询
//!   GitHub Releases API 刷新缓存；网络/解析任何异常静默，24h 后重试
//! - 环境变量 `TERMBRIDGE_NO_UPDATE_CHECK` 可整体关闭
//!
//! 与 chrome-devtools-mcp 的对应关系：
//!   npm registry 发布版本  →  GitHub Releases tag（vX.Y.Z）
//!   ~/.cache/chrome-devtools-mcp/latest.json → dirs::cache_dir()/termbridge/update-check.json
//!   detached 子进程异步刷新 → 后台 std::thread（同进程 fire-and-forget）
//!   console.warn + CHROME_DEVTOOLS_MCP_NO_UPDATE_CHECKS → stderr 提示 + TERMBRIDGE_NO_UPDATE_CHECK

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 与 chrome-devtools-mcp 的 CHROME_DEVTOOLS_MCP_NO_UPDATE_CHECKS 对应
const UPDATE_CHECK_ENV: &str = "TERMBRIDGE_NO_UPDATE_CHECK";
const REPO_OWNER: &str = "summerxzp";
const REPO_NAME: &str = "TermBridge";
const CACHE_FILE: &str = "update-check.json";
/// 检查频率上限：24h 内不再发起网络请求（同 chrome-devtools-mcp）
const THROTTLE_SECS: u64 = 24 * 60 * 60;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize, Deserialize)]
struct UpdateCache {
    /// 已知的最新版本（来自 GitHub Releases tag，去掉前导 v）
    version: String,
    /// 上次成功（或占位）检查的 unix 时间戳，用于 24h 限频
    checked_at_unix: u64,
}

/// 启动时检查更新。非阻塞：仅同步读缓存并提示；网络查询在后台线程异步完成。
pub fn check_for_updates() {
    if std::env::var(UPDATE_CHECK_ENV).is_ok_and(|v| !v.is_empty()) {
        tracing::debug!("update check disabled by {UPDATE_CHECK_ENV}");
        return;
    }
    let Some(path) = cache_path() else {
        tracing::debug!("update check: no cache dir available");
        return;
    };
    let cache = read_cache(&path);

    // 1) 同步提示：缓存记录的最新版本 > 当前版本（仅提示，不自动安装）
    if let Some(c) = cache.as_ref() {
        if is_newer(&c.version, CURRENT_VERSION) {
            // stderr（MCP stdio 通道留 stdout；CLI 无 tracing subscriber 也可见）
            eprintln!(
                "\nTermBridge 更新可用：当前 v{CURRENT_VERSION} → v{latest}\n\
                 请到 GitHub Releases 下载：https://github.com/{REPO_OWNER}/{REPO_NAME}/releases\n\
                 禁用检查：{UPDATE_CHECK_ENV}=1\n",
                latest = c.version,
            );
        }
    }

    // 2) 限频：24h 内已检查过则跳过
    let stale = cache.as_ref().is_none_or(|c| {
        now_unix().saturating_sub(c.checked_at_unix) >= THROTTLE_SECS
    });
    if !stale {
        return;
    }

    // 3) 先占位刷新 checked_at（提交流程不阻塞）：即使刷新失败也保持 24h 限频，
    //    且多个进程并发启动时只有第一个会触发网络请求（同 chrome 先刷新 mtime 再 spawn）
    write_cache(&path, cache.map_or_else(|| CURRENT_VERSION.to_string(), |c| c.version));

    // 后台线程异步查询 GitHub，结果写回缓存 → 下次启动才会提示
    std::thread::spawn(move || match fetch_latest_version() {
        Ok(latest) => write_cache(&path, latest),
        Err(e) => tracing::debug!("update check refresh failed (retry in 24h): {e}"),
    });
}

// ───────────────────────────────────────────────────────────────────────────
// 实现细节（均可单测，不触网）
// ───────────────────────────────────────────────────────────────────────────

/// 解析 `vX.Y.Z` / `X.Y.Z` 为数值三元组；解析失败返回 None
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut it = s.split('.');
    let (major, minor, patch) = (it.next()?, it.next()?, it.next()?);
    Some((major.parse().ok()?, minor.parse().ok()?, patch.parse().ok()?))
}

/// `a` 是否严格新于 `b`（数值比较，非字典序）；任一解析失败视为不新
fn is_newer(a: &str, b: &str) -> bool {
    match (parse_version(a), parse_version(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("termbridge").join(CACHE_FILE))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_cache(path: &Path) -> Option<UpdateCache> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache(path: &Path, version: String) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let cache = UpdateCache { version, checked_at_unix: now_unix() };
    let _ = fs::write(path, serde_json::to_vec(&cache).unwrap_or_default());
}

/// 查询 GitHub Releases 最新版 tag（返回去掉前导 v 的版本号）
fn fetch_latest_version() -> anyhow::Result<String> {
    let url =
        format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest");
    let response = ureq::get(&url)
        .set("User-Agent", "termbridge-update-check")
        .set("Accept", "application/vnd.github+json")
        .timeout(FETCH_TIMEOUT)
        .call()?;
    let body: serde_json::Value = serde_json::from_str(&response.into_string()?)?;
    let tag = body
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("release payload missing tag_name"))?;
    Ok(tag.trim_start_matches('v').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("termbridge-update-check-test-{}", std::process::id()))
    }

    #[test]
    fn parse_version_handles_v_prefix_and_numbers() {
        assert_eq!(parse_version("0.2.1"), Some((0, 2, 1)));
        assert_eq!(parse_version("v1.10.3"), Some((1, 10, 3)));
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.x"), None);
    }

    #[test]
    fn is_newer_uses_numeric_not_lexicographic() {
        assert!(is_newer("0.3.0", "0.2.21"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("0.2.1", "0.2.1"));
        assert!(!is_newer("0.2.1", "0.3.0"));
        assert!(!is_newer("garbage", "0.2.1"));
    }

    #[test]
    fn cache_roundtrip_and_throttle() {
        let dir = temp_dir();
        let path = dir.join(CACHE_FILE);
        let _ = fs::remove_file(&path);

        // fresh: checked_at = now → not stale（24h 内）
        write_cache(&path, "0.3.0".to_string());
        let cache = read_cache(&path).expect("cache should exist");
        assert_eq!(cache.version, "0.3.0");
        assert!(!is_stale(cache.checked_at_unix, now_unix()));

        // stale: 25h 前检查 → stale
        let cache = UpdateCache { version: "0.3.0".into(), checked_at_unix: now_unix() - 25 * 3600 };
        fs::write(&path, serde_json::to_vec(&cache).unwrap()).unwrap();
        assert!(is_stale(
            read_cache(&path).unwrap().checked_at_unix,
            now_unix(),
        ));

        let _ = fs::remove_file(&path);
    }

    fn is_stale(checked_at_unix: u64, now: u64) -> bool {
        now.saturating_sub(checked_at_unix) >= THROTTLE_SECS
    }

    #[test]
    fn cache_missing_is_stale() {
        let dir = temp_dir();
        let path = dir.join(CACHE_FILE);
        let _ = fs::remove_file(&path);
        assert!(read_cache(&path).is_none());
    }
}