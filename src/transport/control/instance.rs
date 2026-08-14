//! Instance 发现机制（ADR-0018）。
//!
//! MCP Server 启动时创建 instance 文件，CLI 通过扫描发现运行中的 MCP Server。
//!
//! 文件位置：
//! - Linux/macOS: $XDG_RUNTIME_DIR/termbridge/mcp-<instance>.json（未设置时回退系统临时目录）
//! - Windows: %TEMP%/termbridge/mcp-<instance>.json
//!
//! 文件内容：pid / endpoint / token / started_at / protocol_version
//! MCP Server 退出时删除文件（Drop 时尽力清理）。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Instance 信息（写入 discovery 文件）。
#[derive(Debug, Serialize, Deserialize)]
pub struct InstanceInfo {
    /// MCP Server 进程 PID
    pub pid: u32,
    /// 传输类型："unix_socket" | "named_pipe"
    pub transport: String,
    /// IPC 端点路径
    pub endpoint: String,
    /// 认证 token（随机生成，CLI 连接时需提供）
    pub token: String,
    /// 启动时间（Unix 毫秒）
    pub started_at: u64,
    /// 协议版本
    pub protocol_version: u32,
}

/// Instance 注册器：管理 discovery 文件的生命周期。
///
/// 创建时写文件，Drop 时删文件。
pub struct InstanceRegistry {
    info: InstanceInfo,
    file_path: PathBuf,
    cleaned_up: bool,
}

impl InstanceRegistry {
    /// 创建新 instance 并写入 discovery 文件。
    ///
    /// - `endpoint`: IPC 端点路径
    /// - `token`: 随机认证 token
    pub fn register(endpoint: String, token: String) -> std::io::Result<Self> {
        let instance_id = generate_instance_id();
        let file_path = instance_file_path(&instance_id);

        // 确保父目录存在
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let info = InstanceInfo {
            pid: std::process::id(),
            transport: transport_name().to_string(),
            endpoint,
            token,
            started_at: now_millis(),
            protocol_version: 1,
        };

        let json = serde_json::to_string_pretty(&info)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(&file_path, json)?;

        tracing::info!(
            file = %file_path.display(),
            pid = info.pid,
            "instance registered"
        );

        Ok(Self {
            info,
            file_path,
            cleaned_up: false,
        })
    }

    /// 获取 instance 信息。
    pub fn info(&self) -> &InstanceInfo {
        &self.info
    }

    /// 扫描所有运行中的 instance（静态方法，CLI 用）。
    ///
    /// 返回 discovery 目录下所有 instance 文件的信息。
    /// 自动过滤 PID 已不存在的 stale instance（尽力清理）。
    pub fn list_instances() -> Vec<InstanceInfo> {
        let dir = match instance_dir() {
            Some(d) => d,
            None => return Vec::new(),
        };

        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };

        let mut instances = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(info) = serde_json::from_str::<InstanceInfo>(&content) {
                    // 检查 PID 是否还活着
                    if is_pid_alive(info.pid) {
                        instances.push(info);
                    } else {
                        // 清理 stale instance 文件
                        tracing::debug!(file = %path.display(), pid = info.pid, "cleaning stale instance");
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
        instances
    }
}

impl Drop for InstanceRegistry {
    fn drop(&mut self) {
        if !self.cleaned_up {
            let _ = std::fs::remove_file(&self.file_path);
            tracing::debug!(file = %self.file_path.display(), "instance file cleaned up");
        }
    }
}

/// 生成 6 字符 hex instance ID。
fn generate_instance_id() -> String {
    // 用 PID + 时间戳生成简单唯一 ID（不需要密码学安全）
    let pid = std::process::id();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:06x}", (pid as u64 ^ ts as u64) & 0xFFFFFF)
}

/// Instance discovery 目录。
fn instance_dir() -> Option<PathBuf> {
    // Linux/macOS: $XDG_RUNTIME_DIR/termbridge/，未设置时回退系统临时目录，
    // 保证 register 与 list_instances 读写一致（XDG_RUNTIME_DIR 常缺省，如 CI）。
    if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
        std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .map(|d| PathBuf::from(d).join("termbridge"))
            .or_else(|| Some(std::env::temp_dir().join("termbridge")))
    } else {
        // Windows 及其他：用 TEMP
        std::env::var("TEMP")
            .ok()
            .map(|d| PathBuf::from(d).join("termbridge"))
            .or_else(|| Some(std::env::temp_dir().join("termbridge")))
    }
}

/// Instance discovery 文件路径。
fn instance_file_path(instance_id: &str) -> PathBuf {
    instance_dir()
        .expect("instance dir always resolvable (temp_dir fallback)")
        .join(format!("mcp-{instance_id}.json"))
}

/// 传输类型名称。
fn transport_name() -> &'static str {
    if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
        "unix_socket"
    } else {
        "named_pipe"
    }
}

/// 检查 PID 是否还活着（尽力，平台差异）。
fn is_pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        // Windows: 尝试 OpenProcess，失败则认为已死
        use std::ffi::c_void;
        extern "system" {
            fn OpenProcess(access: u32, inherit: bool, pid: u32) -> *mut c_void;
            fn CloseHandle(h: *mut c_void) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
            if h.is_null() {
                return false;
            }
            CloseHandle(h);
            true
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Unix: kill(pid, 0) 返回 0 表示进程存在
        unsafe { libc_kill(pid as i32, 0) == 0 }
    }
}

/// 当前时间（Unix 毫秒）。
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// Unix 的 kill syscall（避免引入 libc crate 依赖）
#[cfg(not(target_os = "windows"))]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_register_and_list() {
        // 注册一个 instance
        let registry = InstanceRegistry::register(
            "/tmp/test-termbridge.sock".into(),
            "test-token-123".into(),
        )
        .expect("register failed");

        // list 应包含刚注册的
        let instances = InstanceRegistry::list_instances();
        let found = instances.iter().find(|i| i.token == "test-token-123");
        assert!(found.is_some(), "应能发现刚注册的 instance");
        assert_eq!(found.unwrap().pid, std::process::id());

        // Drop 后应清理
        drop(registry);
        let instances = InstanceRegistry::list_instances();
        let found = instances.iter().find(|i| i.token == "test-token-123");
        assert!(found.is_none(), "Drop 后应清理 instance 文件");
    }

    #[test]
    fn instance_info_serialization() {
        let info = InstanceInfo {
            pid: 12345,
            transport: "unix_socket".into(),
            endpoint: "/tmp/test.sock".into(),
            token: "abc".into(),
            started_at: 1700000000000,
            protocol_version: 1,
        };
        let json = serde_json::to_string(&info).unwrap();
        let deserialized: InstanceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.pid, 12345);
        assert_eq!(deserialized.token, "abc");
    }
}
