//! Linux PTY 创建（ADR-0004 §1）
//!
//! 用 nix crate 创建 master/slave PTY，fork 子进程执行 shell。
//!
//! ⚠️ fork 在多线程进程里是 unsafe（POSIX 仅允许 async-signal-safe 函数在 fork 后 exec 前
//! 调用）。所有 CString / argv 指针数组均在 fork 前 预构造；子进程仅调 setsid / dup2 /
//! close / chdir / execvp / _exit 等 async-signal-safe 的 libc 函数，无任何 Rust 分配。
//!
//! PTY 输入走 [`PtyWriter`]：有界队列 + 专属写线程，dispatch 线程绝不直接写 master fd
//! （阻塞 write 在前台进程不读 stdin 时会永久占住线程 / 锁，拖死 close/resize）。

use std::collections::VecDeque;
use std::ffi::CString;
use std::fmt;
use std::io;
use std::os::fd::{IntoRawFd, RawFd};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::pty::{openpty, Winsize};
use nix::sys::signal::Signal;
use nix::sys::wait::WaitStatus;
use nix::unistd::{close, fork, ForkResult, Pid};
use parking_lot::{Condvar, Mutex};

use crate::protocol::PtySize;

// ───────────────────────────────────────────────────────────────────────────
// Pty
// ───────────────────────────────────────────────────────────────────────────

/// PTY 句柄：master_fd + 子进程 pid
///
/// Drop 时会 kill 子进程并关闭 master_fd。
/// 写入请走 [`PtyWriter`]（独立结构，与读 / resize 无锁竞争）；本结构的 master_fd
/// 供 PTY read task 与 resize（ioctl）使用。
pub struct Pty {
    master_fd: RawFd,
    child_pid: Pid,
}

/// 给 fd 设置 FD_CLOEXEC（exec 时自动关闭，防止 fd 泄漏给 fork 出的 shell 子进程）
fn set_cloexec(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

impl Pty {
    /// 创建 PTY 并 fork 子进程执行 shell。
    ///
    /// - `shell`：shell 路径（如 /bin/bash）
    /// - `cwd`：子进程工作目录（None 则继承父进程；非法路径跳过 chdir）
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

        // 立即给 master/slave 设置 FD_CLOEXEC（nix 0.29 的 openpty 不暴露 open 标志）。
        // 否则 fork+exec 后 shell 子进程会继承本 fd 以及 daemon 的所有其他 fd
        // （unix listener、client socket、其他 session 的 pty master），导致：
        //   1) 其他 session 的 master 被无关进程持有 → master 永不 EOF → session Lost 检测失效；
        //   2) fd 泄漏随 session 数量放大。
        // 子进程随后的 dup2(slave_fd, 0/1/2) 会清除 0/1/2 副本上的 CLOEXEC（dup2 语义），
        // 因此 shell 的 stdin/stdout/stderr 在 exec 后存活。
        set_cloexec(master_fd);
        set_cloexec(slave_fd);

        // —— fork 前预构造全部 CString / argv 指针数组 ——
        // fork 与 exec 之间只允许 async-signal-safe 函数；CString::new / Vec 分配都可能
        // 触发 malloc，若其他线程恰持 malloc arena 锁，子进程会死锁在 exec 之前。
        let shell_c = CString::new(shell)
            .with_context(|| format!("shell 路径含 NUL 字节: {:?}", shell))?;
        // 预构建 execvp 参数指针数组（含 null 结尾），子进程内不再分配。
        // 直接用 libc::execvp：nix 0.29 的 execvp 内部仍会 Vec 分配（to_exec_array）。
        let exec_argv: Vec<*const libc::c_char> = vec![shell_c.as_ptr(), std::ptr::null()];
        // cwd 非法（含 NUL）时保持旧行为：跳过 chdir
        let cwd_c = cwd.and_then(|d| CString::new(d).ok());

        // fork 子进程执行 shell
        // SAFETY: fork 后子进程仅调用 async-signal-safe 的 libc 函数
        // （close/dup2/setsid/chdir/execvp/_exit）。所有 CString / argv 指针数组已在
        // fork 前构造完毕，子进程内无任何 Rust 分配 / Drop / 锁操作。
        let fork_result = unsafe { fork() }.context("fork 失败")?;

        match fork_result {
            ForkResult::Child => {
                // —— 子进程 —— 仅 async-signal-safe libc 调用
                unsafe {
                    libc::close(master_fd); // 子进程不需要 master（且已带 CLOEXEC，exec 时亦会关闭）
                    libc::dup2(slave_fd, 0); // dup2 清除副本的 CLOEXEC → 0/1/2 在 exec 后存活
                    libc::dup2(slave_fd, 1);
                    libc::dup2(slave_fd, 2);
                    libc::close(slave_fd); // 关闭原 slave（0/1/2 是副本）
                    let _ = libc::setsid();
                    if let Some(ref dir) = cwd_c {
                        let _ = libc::chdir(dir.as_ptr());
                    }
                    libc::execvp(shell_c.as_ptr(), exec_argv.as_ptr());
                    // execvp 成功不返回；失败则 _exit（async-signal-safe；exit 不是）
                    libc::_exit(127);
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
    /// 返回 0 表示 EOF（子进程关闭 slave 端）。注意：Linux 上 slave 全部关闭后
    /// master read 也会返回 EIO（见 read task 对 EIO 的处理）。
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        // 用 libc 直接调用，避免 nix 版本 AsFd 差异
        let n = unsafe { libc::read(self.master_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    /// 调整 PTY 窗口尺寸（ioctl TIOCSWINSZ）
    ///
    /// resize 只短暂持有 Pty 锁做 ioctl，与 writer 线程（独占 dup fd）无锁竞争，
    /// 大输入积压时依然可以立即执行。
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

    /// kill 子进程（SIGKILL 整个进程组）。
    ///
    /// 子进程 spawn 时已 setsid() 自成进程组长，只杀 leader pid 会把组内孙进程留成孤儿
    /// （继续持有 slave fd → master 永不 EOF → Lost 检测失效）。故用 kill(-pid) 杀全组；
    /// 组不存在（ESRCH，如子进程已退出）时回退杀 leader 本身。
    pub fn kill_child(&self) {
        let neg_pid = Pid::from_raw(-self.child_pid.as_raw());
        if let Err(nix::errno::Errno::ESRCH) =
            nix::sys::signal::kill(neg_pid, Signal::SIGKILL)
        {
            let _ = nix::sys::signal::kill(self.child_pid, Signal::SIGKILL);
        }
    }

    /// 尝试非阻塞回收子进程（waitpid WNOHANG 单次尝试）。
    ///
    /// 返回 Some(code)：子进程已退出（正常退出码，或 128+信号号）；
    /// None：仍存活 / 已在其他处被回收（ECHILD）/ 出错。
    pub fn try_reap(&self) -> Option<i32> {
        match nix::sys::wait::waitpid(self.child_pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, code)) => Some(code as i32),
            Ok(WaitStatus::Signaled(_, sig, _)) => Some(128 + sig as i32),
            Ok(_) => None,
            Err(_) => None,
        }
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
        // kill 子进程（整个进程组）
        self.kill_child();
        // 回收僵尸兜底（read task 在 EOF 路径已回收；这里做有界重试，SIGKILL 后通常几 ms 退出）。
        // ECHILD（已被回收）则直接跳过，不空转。
        match nix::sys::wait::waitpid(self.child_pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {
                let deadline = Instant::now() + Duration::from_millis(200);
                while Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                    match nix::sys::wait::waitpid(
                        self.child_pid,
                        Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                    ) {
                        Ok(WaitStatus::StillAlive) => continue,
                        _ => break,
                    }
                }
            }
            _ => {} // 刚回收 / 已被回收（ECHILD）
        }
        // 关闭 master fd
        let _ = close(self.master_fd);
    }
}

// ───────────────────────────────────────────────────────────────────────────
// PtyWriter：有界队列 + 专属写线程
// ───────────────────────────────────────────────────────────────────────────

/// 输入队列容量（字节）。超过即对 send_input 背压（等待/报错），防止内存无限增长。
const WRITER_QUEUE_BYTES: usize = 256 * 1024;
/// 单个队列 chunk 上限（大输入切块入队）
const WRITER_CHUNK: usize = 64 * 1024;
/// 队列满时 send_input 的最长等待（超时报错，绝不无限阻塞 RPC dispatch 线程）
const SEND_WAIT: Duration = Duration::from_secs(5);
/// Drop 时 join 写线程的上限。正常路径 Session 销毁前已 kill 子进程，slave
/// 挂断使阻塞的 write 以 EIO 失败、写线程立即退出；但 panic/独立使用路径下
/// 无人 kill 子进程，join 会永久挂死调用方（测试实测复现）。超时后放弃 join、
/// 泄漏写线程——fd 关闭后其 write 最终以 EBADF 失败退出，不会永久驻留。
const DROP_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// send_input 入队失败原因
#[derive(Debug, PartialEq, Eq)]
pub enum PtyWriteError {
    /// writer 线程已关闭（session 正在销毁）
    Closed,
    /// 队列持续背压超时（tty 缓冲满且前台进程未读 stdin）
    Backlogged,
}

impl fmt::Display for PtyWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PtyWriteError::Closed => write!(f, "writer 已关闭（session 正在销毁）"),
            PtyWriteError::Backlogged => {
                write!(f, "输入队列背压超时（{}s），前台进程可能未读取 stdin", SEND_WAIT.as_secs())
            }
        }
    }
}

impl std::error::Error for PtyWriteError {}

/// 队列内部状态
struct WriterQueue {
    chunks: VecDeque<Vec<u8>>,
    /// 已入队字节总数（字节容量计账）
    queued: usize,
    /// 关闭标志：置位后写线程丢弃积压退出，send 拒绝新输入
    closed: bool,
}

/// writer 线程共享状态
struct WriterShared {
    queue: Mutex<WriterQueue>,
    cv: Condvar,
}

/// PTY 输入写入器：有界队列 + 专属写线程。
///
/// - dispatch 线程只做入队（有界等待 ≤5s），绝不直接 write master fd；
/// - 写线程独占一份 master fd 的 dup 副本，循环 write 处理短写直到整块写完；
/// - 读（read task）与 resize（ioctl）走原 master fd，与写线程无锁竞争。
pub struct PtyWriter {
    inner: Arc<WriterShared>,
    /// 写线程句柄（Drop 时 shutdown + join）
    handle: Option<thread::JoinHandle<()>>,
}

impl PtyWriter {
    /// 创建 writer：dup 一份 master fd 交给写线程独占使用
    pub fn new(master_fd: RawFd) -> io::Result<Self> {
        let fd = unsafe { libc::dup(master_fd) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // dup 不继承 CLOEXEC（fd 描述符标志 per-fd），必须显式设置，
        // 否则该副本会泄漏给后续 fork 出的 shell 子进程
        set_cloexec(fd);
        let inner = Arc::new(WriterShared {
            queue: Mutex::new(WriterQueue {
                chunks: VecDeque::new(),
                queued: 0,
                closed: false,
            }),
            cv: Condvar::new(),
        });
        let thread_inner = Arc::clone(&inner);
        let handle = thread::Builder::new()
            .name("pty-writer".to_string())
            .spawn(move || writer_loop(fd, thread_inner))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(Self {
            inner,
            handle: Some(handle),
        })
    }

    /// 入队输入（切块 + 字节容量计账）。
    ///
    /// 队列满时等待最多 [`SEND_WAIT`]，仍满则返回 [`PtyWriteError::Backlogged`]，
    /// 绝不无限阻塞调用线程。数据入队即返回 Ok；后续由写线程保证整块写完（短写循环），
    /// 仅在 slave 端已关闭（子进程退出，输入无处投递）时才丢弃剩余数据。
    pub fn send(&self, data: &[u8]) -> Result<(), PtyWriteError> {
        if data.is_empty() {
            return Ok(());
        }
        let deadline = Instant::now() + SEND_WAIT;
        for chunk in data.chunks(WRITER_CHUNK) {
            let mut q = self.inner.queue.lock();
            loop {
                if q.closed {
                    return Err(PtyWriteError::Closed);
                }
                if q.queued + chunk.len() <= WRITER_QUEUE_BYTES {
                    break;
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(PtyWriteError::Backlogged);
                }
                // 有界等待容量（循环头会重查 closed / deadline）
                self.inner.cv.wait_for(&mut q, deadline - now);
            }
            q.queued += chunk.len();
            q.chunks.push_back(chunk.to_vec());
            drop(q);
            self.inner.cv.notify_one();
        }
        Ok(())
    }
}

impl Drop for PtyWriter {
    fn drop(&mut self) {
        // 关闭队列 + 唤醒写线程并 join。
        // 正常路径前置：Session 销毁时已先 kill 子进程 —— 写线程若阻塞在 master
        // write 上，slave 挂断（SIGKILL → slave fd 全关）会使其以 EIO 失败退出。
        // 但 panic / 独立使用路径无人保证该前置，join 必须有界：超时放弃 join、
        // 泄漏写线程（其 fd 关闭后 write 以 EBADF 失败自然退出），绝不挂死
        // 正在 unwind 的线程。
        {
            let mut q = self.inner.queue.lock();
            q.closed = true;
        }
        self.inner.cv.notify_all();
        if let Some(handle) = self.handle.take() {
            let timeout = DROP_JOIN_TIMEOUT;
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let _ = thread::spawn(move || {
                let _ = handle.join();
                let _ = tx.send(());
            });
            if rx.recv_timeout(timeout).is_err() {
                tracing::warn!(
                    "pty-writer 线程在 {}s 内未退出（子进程未被 kill?），放弃 join",
                    timeout.as_secs()
                );
            }
        }
    }
}

/// 写线程主循环：出队 → 循环 write 处理短写，直到整块写完。
fn writer_loop(fd: RawFd, inner: Arc<WriterShared>) {
    loop {
        // 出队一块（无数据且未关闭则等待）
        let chunk = {
            let mut q = inner.queue.lock();
            loop {
                if q.closed {
                    // session 正在销毁：剩余积压输入已无意义，丢弃并退出
                    q.chunks.clear();
                    q.queued = 0;
                    inner.cv.notify_all();
                    break None;
                }
                if let Some(c) = q.chunks.pop_front() {
                    q.queued -= c.len();
                    inner.cv.notify_all(); // 唤醒等待容量的 send_input
                    break Some(c);
                }
                inner.cv.wait(&mut q);
            }
        };
        let Some(chunk) = chunk else { break };
        // 写整块（短写循环）。EIO/EBADF：slave 端已全部关闭（子进程退出），
        // 剩余输入无处投递，丢弃后继续处理队列（或等待关闭）。
        let mut off = 0usize;
        while off < chunk.len() {
            let n = unsafe {
                libc::write(
                    fd,
                    chunk[off..].as_ptr() as *const libc::c_void,
                    chunk.len() - off,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::Interrupted => continue,
                    // fd 为阻塞模式，WouldBlock 理论上不出现，兜底短暂重试
                    io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    _ => break,
                }
            }
            off += n as usize;
        }
    }
    let _ = unsafe { libc::close(fd) };
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

    /// 测试 PtyWriter：用 /bin/cat 验证回显
    #[test]
    fn write_to_cat_and_read_echo() {
        // /bin/cat 从 stdin 读并写到 stdout
        let pty = Pty::spawn(
            "/bin/cat",
            None,
            PtySize { rows: 24, cols: 80 },
        )
        .expect("spawn cat");
        let writer = PtyWriter::new(pty.master_fd()).expect("create writer");
        // 写入数据
        writer.send(b"hello-pty\n").expect("send input");
        // 等 cat 回显
        thread::sleep(Duration::from_millis(100));
        let mut buf = [0u8; 256];
        let n = pty.read(&mut buf).expect("read echo");
        assert!(n > 0);
        let output = String::from_utf8_lossy(&buf[..n]);
        assert!(output.contains("hello-pty"), "cat 应回显输入，实际: {}", output);
        // 关闭（kill cat；先杀再 drop writer，避免 join 等待阻塞 write）
        pty.kill_child();
        drop(writer);
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

    /// Fix 4：shell 子进程不应继承任何多余 fd（openpty master/slave 均已设 CLOEXEC，
    /// dup2 后仅剩 0/1/2）。若未设 CLOEXEC，子进程会多持有 master（fd 3）。
    #[test]
    fn child_has_no_leaked_fds() {
        let pty = Pty::spawn(
            "/bin/cat",
            None,
            PtySize { rows: 24, cols: 80 },
        )
        .expect("spawn cat");
        // 等 exec 完成再检查：fork→exec 之间子进程短暂持有 master/slave 原始
        // fd 是预期（随后显式关闭，exec 时 CLOEXEC 原子兜底）。并行负载下
        // 子进程可能迟迟未被调度，固定 sleep 会误报（Linux 实测 flaky）。
        // comm 变为 "cat" 即 exec 已完成，此后 fd > 2 才是真泄漏。
        let pid = pty.child_pid().as_raw();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            if comm.trim() == "cat" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "子进程 5s 内未完成 exec（comm: {comm:?}）"
            );
            thread::sleep(Duration::from_millis(20));
        }
        let fd_dir = format!("/proc/{pid}/fd");
        let entries = std::fs::read_dir(&fd_dir).expect("读 /proc/<pid>/fd 失败");
        for entry in entries {
            let name = entry.expect("entry").file_name();
            let fd: i32 = name.to_string_lossy().parse().expect("fd 编号");
            assert!(fd <= 2, "子进程泄漏了 fd {}（应仅有 0/1/2）", fd);
        }
        pty.kill_child();
    }

    /// Fix 7：kill_child 应杀掉整个进程组（setsid 后 leader + 孙进程全灭）
    #[test]
    fn kill_child_kills_process_group() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static MARKER: AtomicU64 = AtomicU64::new(0);
        let unique = MARKER.fetch_add(1, Ordering::SeqCst);

        // 用临时脚本作为 shell：后台起一个 sleep 孙进程并写下其 pid，然后 wait
        let dir = std::env::temp_dir();
        let script = dir.join(format!("tb_test_group_{}_{}.sh", std::process::id(), unique));
        let pidfile = dir.join(format!("tb_test_group_pid_{}_{}", std::process::id(), unique));
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nsleep 300 & echo $! > {}\nwait\n",
                pidfile.to_string_lossy()
            ),
        )
        .expect("写脚本失败");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let pty = Pty::spawn(
            script.to_string_lossy().as_ref(),
            None,
            PtySize { rows: 24, cols: 80 },
        )
        .expect("spawn script");

        // 等孙进程 pid 文件出现（最多 3s）
        let mut grandchild: Option<i32> = None;
        for _ in 0..60 {
            if let Ok(s) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = s.trim().parse::<i32>() {
                    grandchild = Some(pid);
                    break;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        let grandchild = grandchild.expect("应读到孙进程 pid");

        // 杀进程组
        pty.kill_child();
        thread::sleep(Duration::from_millis(300));

        // 孙进程应已死亡（kill(pid, 0) 返回 ESRCH）
        let ret = unsafe { libc::kill(grandchild, 0) };
        let errno = io::Error::last_os_error().raw_os_error();
        assert!(
            ret == -1 && errno == Some(libc::ESRCH),
            "孙进程 {} 应随进程组一起被杀，kill 返回 {:?} errno {:?}",
            grandchild,
            ret,
            errno
        );

        // 清理
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&pidfile);
    }

    /// Fix 3：PtyWriter 走队列写入，cat 全量回显（短写不丢字节）
    #[test]
    fn writer_delivers_full_input() {
        let pty = Pty::spawn(
            "/bin/cat",
            None,
            PtySize { rows: 24, cols: 80 },
        )
        .expect("spawn cat");
        let writer = PtyWriter::new(pty.master_fd()).expect("create writer");

        // 关 ECHO（回显由 cat 输出提供，避免 tty 双重回显）与 ONLCR
        // （输出 \n → \r\n 改写，否则回显内容 ≠ 输入，无法精确比对）
        let fd = pty.master_fd();
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(fd, &mut t), 0, "tcgetattr");
            t.c_lflag &= !libc::ECHO;
            t.c_oflag &= !libc::ONLCR;
            assert_eq!(libc::tcsetattr(fd, libc::TCSANOW, &t), 0, "tcsetattr");
        }

        // 后台线程分批发送 512KB；主线程**并发读取**回显——先发后读会撑满
        // tty 输出缓冲 → cat 停止读取 → 输入队列背压死锁（Linux 实测教训）。
        // 数据只用字母+换行：canonical 模式下控制字符有特殊语义
        // （0x03=SIGINT 杀前台进程、0x7f=删除符…），会破坏回显一致性
        let total = 512 * 1024;
        let input: Vec<u8> = (0..total)
            .map(|i| if i % 64 == 63 { b'\n' } else { b'a' + (i % 26) as u8 })
            .collect();
        let writer_t = std::sync::Arc::new(writer);
        let sender = {
            let writer_t = writer_t.clone();
            let input = input.clone();
            thread::spawn(move || {
                for chunk in input.chunks(WRITER_CHUNK) {
                    // 背压容忍：cat 读取慢时 send 可能 Backlogged，重试到成功
                    let mut off = 0;
                    while off < chunk.len() {
                        match writer_t.send(&chunk[off..]) {
                            Ok(()) => off = chunk.len(),
                            Err(PtyWriteError::Backlogged) => {
                                thread::sleep(Duration::from_millis(50));
                            }
                            Err(e) => panic!("send chunk 失败: {e}"),
                        }
                    }
                }
            })
        };

        // 轮询读回显直到收满（最多 10s）
        let mut received: Vec<u8> = Vec::with_capacity(total);
        let deadline = Instant::now() + Duration::from_secs(10);
        while received.len() < total && Instant::now() < deadline {
            let mut buf = [0u8; 65536];
            match pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        sender.join().expect("join sender");
        assert_eq!(received.len(), total, "cat 应回显全部输入（短写不得丢字节）");
        assert_eq!(received, input, "回显内容应一致");

        // 先杀子进程再 drop writer（避免 join 等待阻塞 write）
        pty.kill_child();
        drop(writer_t);
    }

    /// Fix 3：前台进程不读 stdin 时，队列背压应超时报错而非无限阻塞
    #[test]
    fn writer_backpressure_times_out() {
        let pty = Pty::spawn(
            "/bin/sleep",
            None,
            PtySize { rows: 24, cols: 80 },
        )
        .expect("spawn sleep");
        let writer = PtyWriter::new(pty.master_fd()).expect("create writer");

        // sleep 永不读 stdin：写入远超队列容量（256KB）的数据 → 5s 后 Backlogged。
        // 数据必须含换行：PTY 行缓冲（canonical 模式）对无换行的超长行直接
        // 丢弃（不缓冲），队列永远填不满，背压不会发生（Linux 实测教训）。
        let mut big = Vec::with_capacity(1024 * 1024);
        for _ in 0..(1024 * 1024 / 64) {
            big.extend_from_slice(&[0x42u8; 63]);
            big.push(b'\n');
        }
        let start = Instant::now();
        let err = writer.send(&big).expect_err("应背压超时");
        assert_eq!(err, PtyWriteError::Backlogged);
        assert!(start.elapsed() >= SEND_WAIT, "应等待到超时");

        // 先杀子进程（解除写线程阻塞）再 drop writer，join 应快速返回
        pty.kill_child();
        drop(writer);
    }
}
