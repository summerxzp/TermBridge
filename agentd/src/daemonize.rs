//! daemon 启动：spawn 独立 serve 进程（ADR-0004 §3 阶段 2）
//!
//! 旧方案在 tokio runtime 内 fork，子进程继承父进程的 runtime 状态
//! （多线程、锁、tokio 内部状态），导致死锁。新方案用 `Command::spawn`
//! 启动全新的 serve 进程，子进程有干净的 tokio runtime。
//!
//! `pre_exec` 做 setsid 脱离控制终端；stdin/stdout/stderr 重定向到 /dev/null
//! 避免 SSH exec 等待 EOF。daemon_id 通过环境变量 `TERMBRIDGE_DAEMON_ID` 传递。

use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};

/// spawn 独立 serve 进程：setsid + 重定向 stdio + 重新执行自己（serve 模式）
///
/// - `pre_exec`: setsid 脱离控制终端（async-signal-safe 系统调用）
/// - stdin/stdout/stderr → /dev/null（避免 SSH exec 等待 EOF）
/// - 环境变量 `TERMBRIDGE_DAEMON_ID` 传递 daemon_id 给子进程
///
/// 返回子进程 `Child`，调用方应 `std::mem::forget(child)` 防止 Drop kill 子进程。
pub fn spawn_serve_process(exe: &Path, socket_path: &str, daemon_id: &str) -> Result<Child> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .arg("--sock")
        .arg(socket_path)
        .env("TERMBRIDGE_DAEMON_ID", daemon_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: pre_exec 闭包仅调用 async-signal-safe 的 libc::setsid（系统调用）
    // + std::io::Error::last_os_error（读 errno，fork 后安全）
    unsafe {
        cmd.pre_exec(|| {
            // setsid：创建新会话，脱离控制终端
            let ret = libc::setsid();
            if ret < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.spawn().context("spawn serve 进程失败")
}
