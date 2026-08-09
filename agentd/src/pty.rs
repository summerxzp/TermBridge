//! Linux PTY 创建（ADR-0004 §1）
//!
//! 用 nix crate 创建 master/slave PTY，fork 子进程执行 shell。
//!
//! ⚠️ fork 在多线程进程里是 unsafe（POSIX 仅允许 async-signal-safe 函数在 fork 后 exec 前
//! 调用）。子进程仅调 setsid / dup2 / close / chdir / execvp，均为 async-signal-safe。

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};

use anyhow::{Context, Result};
use nix::pty::{openpty, Winsize};
use nix::sys::signal::Signal;
use nix::sys::wait::WaitStatus;
use nix::unistd::{close, fork, setsid, ForkResult, Pid};

use crate::protocol::PtySize;

// ───────────────────────────────────────────────────────────────────────────
// Pty
// ───────────────────────────────────────────────────────────────────────────

/// PTY 句柄：master_fd + 子进程 pid
///
/// Drop 时会 kill 子进程并关闭 master_fd。
pub struct Pty {
    master_fd: RawFd,
    child_pid: Pid,
}

impl Pty {
    /// 创建 PTY 并 fork 子进程执行 shell。
    ///
    /// - `shell`：shell 路径（如 /bin/bash）
    /// - `cwd`：子进程工作目录（None 则继承父进程）
    /// - `pty_size`：初始 PTY 窗口尺寸
    pub fn spawn(shell: &str, cwd: Option<&str>, pty_size: PtySize) -> Result<Self> {
        let winsize = Winsize {
            ws_row: pty_size.rows,
            ws_col: pty_size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // openpty 创建 master/slave PTY 对
        let pty = openpty(Some(&winsize), None).context("openpty 失败")?;

        // fork 前消耗 OwnedFd 为 RawFd，防止 fork 后子进程触发 OwnedFd::drop
        // （Rust Drop 不是 async-signal-safe，在多线程 fork 后可能死锁/UB）
        let master_fd = pty.master.into_raw_fd();
        let slave_fd = pty.slave.into_raw_fd();

        // fork 子进程执行 shell
        // SAFETY: 子进程仅调 async-signal-safe 的 libc 函数（close/setsid/dup2/chdir/execvp/_exit）。
        // 不触发任何 Rust Drop 逻辑。父进程继续。
        let fork_result = unsafe { fork() }.context("fork 失败")?;

        match fork_result {
            ForkResult::Child => {
                // —— 子进程 ——
                // 仅调 async-signal-safe 函数，不用 Rust Drop
                unsafe {
                    libc::close(master_fd); // 子进程不需要 master
                    libc::dup2(slave_fd, 0);
                    libc::dup2(slave_fd, 1);
                    libc::dup2(slave_fd, 2);
                    libc::close(slave_fd); // 关闭原 slave（0/1/2 是副本）
                }
                let _ = setsid();
                // chdir 到指定工作目录
                if let Some(dir) = cwd {
                    if let Ok(c_dir) = CString::new(dir) {
                        let _ = nix::unistd::chdir(c_dir.as_c_str());
                    }
                }
                // execvp shell（成功不返回）
                let shell_c = match CString::new(shell) {
                    Ok(c) => c,
                    Err(_) => {
                        // _exit 是 async-signal-safe，exit 不是
                        unsafe { libc::_exit(127); }
                    }
                };
                let argv: Vec<CString> = vec![shell_c.clone()];
                match nix::unistd::execvp(&shell_c, &argv) {
                    Ok(infallible) => match infallible {},
                    Err(_) => {
                        unsafe { libc::_exit(127); }
                    }
                }
            }
            ForkResult::Parent { child } => {
                // —— 父进程 ——
                unsafe { libc::close(slave_fd); } // 父进程不需要 slave
                Ok(Pty {
                    master_fd,
                    child_pid: child,
                })
            }
        }
    }

    /// 读取 PTY output（阻塞模式：阻塞直到有数据；非阻塞模式：EAGAIN 返回 WouldBlock）。
    ///
    /// 返回 0 表示 EOF（子进程关闭 slave 端）。
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        // 用 libc 直接调用，避免 nix 版本 AsFd 差异
        let n = unsafe { libc::read(self.master_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    /// 写入 PTY input（发送给子进程 stdin）
    pub fn write(&self, data: &[u8]) -> io::Result<usize> {
        let n = unsafe { libc::write(self.master_fd, data.as_ptr() as *const _, data.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    /// 调整 PTY 窗口尺寸（ioctl TIOCSWINSZ）
    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let ret = unsafe { libc::ioctl(self.master_fd, libc::TIOCSWINSZ, &ws) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// kill 子进程（SIGKILL）
    pub fn kill_child(&self) {
        let _ = nix::sys::signal::kill(self.child_pid, Signal::SIGKILL);
    }

    /// 等待子进程退出（阻塞），返回 exit code
    pub fn wait_child(&self) -> io::Result<i32> {
        loop {
            match nix::sys::wait::waitpid(self.child_pid, None) {
                Ok(WaitStatus::Exited(_, code)) => return Ok(code),
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    return Ok(128 + sig as i32);
                }
                Ok(_) => continue, // 其他状态继续等
                Err(e) => return Err(io::Error::from_raw_os_error(e as i32)),
            }
        }
    }

    /// master fd（供 PTY read task 用）
    pub fn master_fd(&self) -> RawFd {
        self.master_fd
    }

    /// 子进程 pid
    pub fn child_pid(&self) -> Pid {
        self.child_pid
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // kill 子进程
        self.kill_child();
        // 尝试回收僵尸（不阻塞，失败无所谓）
        let _ = nix::sys::wait::waitpid(self.child_pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
        // 关闭 master fd
        let _ = close(self.master_fd);
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 单元测试（仅 Linux）
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use std::thread;

    /// 测试 spawn + read + write：用 /bin/echo 验证
    #[test]
    fn spawn_echo_and_read() {
        // /bin/echo 不是 shell，但可以 exec，输出后立即退出 → PTY EOF
        let pty = Pty::spawn(
            "/bin/echo",
            None,
            PtySize { rows: 24, cols: 80 },
        )
        .expect("spawn PTY");
        // 等 echo 输出
        thread::sleep(Duration::from_millis(100));
        let mut buf = [0u8; 256];
        let n = pty.read(&mut buf).expect("read PTY");
        assert!(n > 0, "应读到 echo 输出");
        let output = String::from_utf8_lossy(&buf[..n]);
        assert!(output.contains("\n"), "echo 输出应含换行");
    }

    /// 测试 spawn 失败：不存在的 shell
    #[test]
    fn spawn_nonexistent_shell_fails_or_exits_127() {
        // spawn 本身会成功（fork 成功），但子进程 execvp 失败 exit(127)
        let pty = Pty::spawn(
            "/nonexistent/shell",
            None,
            PtySize { rows: 24, cols: 80 },
        )
        .expect("fork 本身应成功");
        // 等子进程退出
        let code = pty.wait_child().expect("wait child");
        assert_eq!(code, 127, "execvp 失败应 exit(127)");
    }

    /// 测试 write：用 /bin/cat 验证回显
    #[test]
    fn write_to_cat_and_read_echo() {
        // /bin/cat 从 stdin 读并写到 stdout
        let pty = Pty::spawn(
            "/bin/cat",
            None,
            PtySize { rows: 24, cols: 80 },
        )
        .expect("spawn cat");
        // 写入数据
        let input = b"hello-pty\n";
        pty.write(input).expect("write PTY");
        // 等 cat 回显
        thread::sleep(Duration::from_millis(100));
        let mut buf = [0u8; 256];
        let n = pty.read(&mut buf).expect("read echo");
        assert!(n > 0);
        let output = String::from_utf8_lossy(&buf[..n]);
        assert!(output.contains("hello-pty"), "cat 应回显输入，实际: {}", output);
        // 关闭（kill cat）
        pty.kill_child();
    }

    /// 测试 resize 不报错
    #[test]
    fn resize_no_error() {
        let pty = Pty::spawn(
            "/bin/sleep",
            None,
            PtySize { rows: 24, cols: 80 },
        )
        .expect("spawn sleep");
        pty.resize(40, 120).expect("resize 应成功");
        pty.kill_child();
    }

    /// 测试 cwd 参数
    #[test]
    fn spawn_with_cwd() {
        // /bin/pwd 输出当前目录
        let pty = Pty::spawn(
            "/bin/pwd",
            Some("/tmp"),
            PtySize { rows: 24, cols: 80 },
        )
        .expect("spawn pwd");
        thread::sleep(Duration::from_millis(100));
        let mut buf = [0u8; 256];
        let n = pty.read(&mut buf).expect("read pwd");
        let output = String::from_utf8_lossy(&buf[..n]);
        assert!(output.contains("/tmp"), "pwd 应输出 /tmp，实际: {}", output);
    }
}
