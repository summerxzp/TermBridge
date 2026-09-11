// 共享：POSIX TTY 凭据输入（ADR-0019 从 Linux/macOS 实现抽取）。
//
// 通过 /dev/tty 直接读写终端（不依赖被 JSON-RPC 占用的 stdin/stdout），
// 关闭 ECHO 隐藏输入，Ctrl+C 返回 Cancelled（关闭 ISIG，不触发 SIGINT 退出）。
//
// 定位：fallback 链最后一环（askpass / GUI 之后）。已知风险（ADR-0019 §2.6）：
// 宿主 Agent 若是 TUI（如终端里的 Claude Code），它也在读同一个终端——
// 从 helper 视角无法检测「终端是否被宿主 TUI 占用」，只能靠链路顺序把
// TTY 排在非干扰通道之后，并在文档中声明该残余风险。

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;

use crate::signal_guard;

pub enum TtyError {
    /// 用户取消（Ctrl+C / EOF / 空提交）。
    Cancelled,
    /// 终端不可用（无控制终端 / termios 控制失败）。
    Unavailable(String),
}

/// 在控制终端上读一行密码（无回显）。成功返回密码（用户名不可编辑，由
/// 调用方回退预填值）。
pub fn prompt(prompt_text: &str) -> Result<String, TtyError> {
    // 1. 打开 /dev/tty（MCP 进程的 stdin/stdout 被 JSON-RPC 占用，必须直连终端）
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| TtyError::Unavailable(format!("/dev/tty: {e}")))?;

    // 2. 写 prompt
    let _ = tty.write_all(prompt_text.as_bytes());
    let _ = tty.flush();

    // 3. 关闭 ECHO / ECHONL / ICANON / ISIG：隐藏输入、字节级读取、Ctrl+C 不触发 SIGINT
    let fd = tty.as_raw_fd();
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        let _ = tty.write_all(b"\n");
        return Err(TtyError::Unavailable("tcgetattr failed".into()));
    }
    let original = termios;
    termios.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
    termios.c_cc[libc::VMIN] = 1;
    termios.c_cc[libc::VTIME] = 0;
    let disabled = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } == 0;

    // 4. 读密码（字节级）；若无法关闭 ECHO 则不读取，避免密码回显暴露
    let mut bytes: Vec<u8> = Vec::new();
    let read_result: Result<(), TtyError> = if !disabled {
        Err(TtyError::Unavailable("cannot disable ECHO".into()))
    } else {
        // raw 模式生效：登记 SIGTERM 守卫（父进程超时杀 helper 时恢复 termios）
        signal_guard::tty_begin(fd, &original);
        let r = (|| {
            let mut buf = [0u8; 1];
            loop {
                match tty.read(&mut buf) {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        if buf[0] == b'\n' || buf[0] == b'\r' {
                            break;
                        }
                        if buf[0] == 0x03 {
                            // Ctrl+C
                            return Err(TtyError::Cancelled);
                        }
                        bytes.push(buf[0]);
                    }
                    Err(_) => break,
                }
            }
            Ok(())
        })();
        signal_guard::tty_end();
        r
    };

    // 5. 恢复 ECHO（无论成功失败都恢复，避免终端紊乱）
    let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
    let _ = tty.write_all(b"\n");

    read_result?;
    if bytes.is_empty() {
        Err(TtyError::Cancelled)
    } else {
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}
