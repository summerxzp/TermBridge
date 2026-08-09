//! daemonize：fork + setsid 脱离控制终端（ADR-0004 §3 阶段 2）
//!
//! bootstrap 模式下，父进程 fork 后写 stdout 握手响应再 exit（SSH channel 关闭），
//! 子进程 setsid 脱离控制终端，作为 orphan 由 init/systemd-user 收养，独立存活。
//!
//! 不用 double-fork：Unix socket 无需完全脱离 controlling terminal，setsid 足够。

use std::path::Path;

use anyhow::{Context, Result};
use nix::unistd::{fork, getpid, setsid, ForkResult, Pid};

// ───────────────────────────────────────────────────────────────────────────
// DaemonizeResult
// ───────────────────────────────────────────────────────────────────────────

/// daemonize 返回值：区分父进程与子进程
pub enum DaemonizeResult {
    /// 父进程：应写 stdout 握手响应后 exit(0)
    Parent { child_pid: Pid },
    /// 子进程：已 setsid + 写 pid 文件，继续执行 serve 逻辑
    Child,
}

// ───────────────────────────────────────────────────────────────────────────
// daemonize
// ───────────────────────────────────────────────────────────────────────────

/// fork + setsid 脱离控制终端。
///
/// - 父进程返回 `DaemonizeResult::Parent`，调用方写 stdout 响应后 exit
/// - 子进程返回 `DaemonizeResult::Child`，已 setsid + umask + chdir / + 重定向 stdin 到 /dev/null
///   + 写 pid 文件，调用方继续 serve
///
/// `pid_path`：PID 文件路径（子进程写入自己的 PID）
pub fn daemonize(pid_path: &Path) -> Result<DaemonizeResult> {
    // SAFETY: fork 后子进程仅调 async-signal-safe 函数（setsid/umask/chdir/dup2）。
    // 父进程继续。多线程 fork 的风险由调用方承担（bootstrap 应在 runtime 启动前或单独线程调用）。
    let fork_result = unsafe { fork() }.context("fork 失败")?;

    match fork_result {
        ForkResult::Parent { child } => {
            // 父进程：直接返回，让调用方写 stdout 响应后 exit
            Ok(DaemonizeResult::Parent { child_pid: child })
        }
        ForkResult::Child => {
            // —— 子进程 ——
            // setsid：创建新会话，脱离控制终端
            setsid().context("setsid 失败")?;
            // umask 0o077：新文件仅当前用户可读写
            unsafe { libc::umask(0o077) };
            // chdir /：避免占用工作目录
            std::env::set_current_dir("/").context("chdir / 失败")?;
            // 重定向 stdin (0) 到 /dev/null（避免读取已关闭的 SSH channel）
            redirect_stdin_to_dev_null()?;
            // 写 pid 文件
            let pid = getpid();
            std::fs::write(pid_path, format!("{}", pid))
                .with_context(|| format!("写 pid 文件失败: {:?}", pid_path))?;
            Ok(DaemonizeResult::Child)
        }
    }
}

/// 重定向 stdin 到 /dev/null
fn redirect_stdin_to_dev_null() -> Result<()> {
    use std::os::fd::AsRawFd;
    let dev_null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .context("打开 /dev/null 失败")?;
    let null_fd = dev_null.as_raw_fd();
    // dup2 /dev/null 到 stdin (fd 0)
    let ret = unsafe { libc::dup2(null_fd, 0) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // dev_null drop 时关闭原 fd（stdin 现在是 /dev/null 的副本，独立）
    Ok(())
}
