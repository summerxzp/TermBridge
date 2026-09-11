// SIGTERM / SIGINT 安全退出守卫（ADR-0019）。
//
// TermBridge 父进程在凭据输入超时后先发 SIGTERM、宽限 2s、再 SIGKILL。helper
// 的默认信号行为是立即死亡，会留下两个烂摊子：
//  1. TTY prompt 进行中：termios 停在无回显 raw 模式，用户终端「打字无反应」
//  2. GUI dialog 子进程：成为孤儿，密码框永远留在屏幕上
//
// 本模块在 main 启动时安装 handler：杀子进程 → 恢复 termios → 恢复默认处理
// 并重新触发信号。helper 是单线程进程，static 状态用 Release/Acquire 发布，
// handler 内只调用 async-signal-safe 的 kill / tcsetattr / raise。

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// 正在运行的 prompt 子进程 pid（0 = 无）。
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

/// TTY raw 模式进行中的 fd（-1 = 无）。
static TTY_FD: AtomicI32 = AtomicI32::new(-1);

/// SAVED_TERMIOS 是否已写入完整副本（handler 仅在 true 时恢复）。
static TTY_READY: AtomicBool = AtomicBool::new(false);

/// 保存的 termios 字节副本。仅单线程写（TTY_READY=false 期间），handler 仅在
/// TTY_READY=true 后读——Release/Acquire 保证可见性，无数据竞争。
///
/// 用字节数组而非 `UnsafeCell::termios>`：`mem::zeroed` 非 const，
/// 数组全零字面量可直接初始化 static。
const TERMIOS_BUF: usize = 128;

/// 保存的 termios 字节副本 wrapper。
///
/// 安全论证（单线程进程内）：`tty_begin` 在 TTY_READY=false 时写入，
/// `tty_end` 置 false 前不再有写；handler 仅在 TTY_READY=true（Release）
/// 后读。写者与读者由 Release/Acquire 定序，无并发写。信号 handler 与
/// 主流程可能并发（handler 打断主流程），但两者不同时访问：handler 读的
/// 前提 TTY_READY=true 意味着主流程正处于 raw 模式区间，该区间内主流程
/// 不触碰 SAVED_TERMIOS。`tty_end` 与 handler 的竞争由 TTY_READY 的
/// Acquire/Release 原子序解决——最坏情况是 handler 恢复了一个即将被主
/// 流程恢复的相同副本（幂等）。
struct SavedTermios(UnsafeCell<[u8; TERMIOS_BUF]>);

// SAFETY: 访问完全由 TTY_READY 的 Release/Acquire 定序（见上），且进程
// 内只有一条主线程会调用 tty_begin/tty_end。
unsafe impl Sync for SavedTermios {}

static SAVED_TERMIOS: SavedTermios = SavedTermios(UnsafeCell::new([0u8; TERMIOS_BUF]));

// 编译期保证：任何支持平台的 termios 都装得进缓冲区
const _: () = assert!(std::mem::size_of::<libc::termios>() <= TERMIOS_BUF);

extern "C" fn handle_signal(sig: libc::c_int) {
    unsafe {
        // 1. 清理 prompt 子进程（防孤儿对话框）
        let pid = CHILD_PID.load(Ordering::SeqCst);
        if pid > 0 {
            libc::kill(pid, libc::SIGTERM);
        }
        // 2. 恢复 termios（TTY prompt 进行中时终端停在无回显模式）
        if TTY_READY.load(Ordering::Acquire) {
            let fd = TTY_FD.load(Ordering::SeqCst);
            if fd >= 0 {
                let mut saved: libc::termios = std::mem::zeroed();
                let src = (*SAVED_TERMIOS.0.get()).as_ptr();
                std::ptr::copy_nonoverlapping(
                    src,
                    &mut saved as *mut libc::termios as *mut u8,
                    std::mem::size_of::<libc::termios>(),
                );
                libc::tcsetattr(fd, libc::TCSANOW, &saved);
            }
        }
        // 3. 恢复默认处理并重新触发，保持退出码 / core dump 语义不变
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// 安装 SIGTERM / SIGINT 守卫。main 启动时调用一次。
pub fn install() {
    unsafe {
        libc::signal(libc::SIGTERM, handle_signal as *const () as usize);
        libc::signal(libc::SIGINT, handle_signal as *const () as usize);
    }
}

// ── prompt 子进程跟踪（prompt_cmd.rs 调用）─────────────────────────────

/// 记录正在运行的 prompt 子进程（spawn 成功后、wait 前调用）。
pub fn set_child(pid: u32) {
    CHILD_PID.store(pid as libc::pid_t, Ordering::SeqCst);
}

/// 清除子进程跟踪（wait 返回后调用）。
pub fn clear_child() {
    CHILD_PID.store(0, Ordering::SeqCst);
}

// ── TTY 状态跟踪（tty.rs 调用）────────────────────────────────────────

/// 记录进入 raw 模式的 fd + 原始 termios 副本（tcsetattr 成功后调用）。
pub fn tty_begin(fd: std::os::unix::io::RawFd, original: &libc::termios) {
    unsafe {
        std::ptr::copy_nonoverlapping(
            original as *const libc::termios as *const u8,
            (*SAVED_TERMIOS.0.get()).as_mut_ptr(),
            std::mem::size_of::<libc::termios>(),
        );
    }
    TTY_FD.store(fd, Ordering::SeqCst);
    TTY_READY.store(true, Ordering::Release);
}

/// 撤销 TTY 跟踪（恢复 termios 前调用；handler 与本地恢复幂等）。
pub fn tty_end() {
    TTY_READY.store(false, Ordering::Release);
    TTY_FD.store(-1, Ordering::SeqCst);
}

/// 测试辅助：当前 TTY 守卫是否激活。
#[cfg(test)]
pub fn tty_active() -> bool {
    TTY_READY.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_tracking_roundtrip() {
        set_child(12345);
        assert_eq!(CHILD_PID.load(Ordering::SeqCst), 12345);
        clear_child();
        assert_eq!(CHILD_PID.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn tty_guard_toggles() {
        let original: libc::termios = unsafe { std::mem::zeroed() };
        assert!(!tty_active());
        tty_begin(7, &original);
        assert!(tty_active());
        assert_eq!(TTY_FD.load(Ordering::SeqCst), 7);
        tty_end();
        assert!(!tty_active());
    }
}
