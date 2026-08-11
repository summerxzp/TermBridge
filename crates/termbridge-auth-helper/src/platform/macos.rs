// macOS native 凭据输入：通过 /dev/tty 直接读写终端（与 Linux 同为 POSIX termios），
// 关闭 ECHO 隐藏输入，Ctrl+C 返回 Cancelled（关闭 ISIG，不触发 SIGINT 退出）。
// GUI 集成（Security framework / Keychain）留待后续阶段。

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;

pub enum PromptError {
    Cancelled,
    Unsupported,
}

pub fn prompt_password(host: &str, user: &str, reason: &str) -> Result<String, PromptError> {
    // 1. 打开 /dev/tty（MCP 进程的 stdin/stdout 被 JSON-RPC 占用，必须直连终端）
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|_| PromptError::Unsupported)?;

    // 2. 写 prompt
    let prompt = format!("Password for {}@{} ({}): ", user, host, reason);
    let _ = tty.write_all(prompt.as_bytes());
    let _ = tty.flush();

    // 3. 关闭 ECHO / ECHONL / ICANON / ISIG：隐藏输入、字节级读取、Ctrl+C 不触发 SIGINT
    let fd = tty.as_raw_fd();
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        let _ = tty.write_all(b"\n");
        return Err(PromptError::Unsupported);
    }
    let original = termios;
    termios.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
    termios.c_cc[libc::VMIN] = 1;
    termios.c_cc[libc::VTIME] = 0;
    let disabled = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } == 0;

    // 4. 读密码（字节级）；若无法关闭 ECHO 则不读取，避免密码回显暴露
    let mut bytes: Vec<u8> = Vec::new();
    let read_result: Result<(), PromptError> = if !disabled {
        Err(PromptError::Unsupported)
    } else {
        (|| {
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
                            return Err(PromptError::Cancelled);
                        }
                        bytes.push(buf[0]);
                    }
                    Err(_) => break,
                }
            }
            Ok(())
        })()
    };

    // 5. 恢复 ECHO（无论成功失败都恢复，避免终端紊乱）
    let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
    let _ = tty.write_all(b"\n");

    read_result?;
    if bytes.is_empty() {
        Err(PromptError::Cancelled)
    } else {
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}
