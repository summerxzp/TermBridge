// ADR-0009 阶段 B1 + ADR-0019 协议 v2：termbridge-auth-helper helper process。
//
// 职责：从 stdin 读一行 JSON 请求（password_request），弹出平台原生
// 凭据输入（Windows CredUI / Linux+macOS 多级 fallback：askpass → GUI →
// TTY），向 stdout 写一行 JSON 响应。
//
// 协议 v2（ADR-0019）：区分 cancelled / unsupported / failed / password。
// 旧协议把所有平台错误折叠成 cancelled，Agent 无法区分「用户取消」与
// 「环境无输入通道」，误导排障。v1 响应（password / cancelled）保持不变，
// v1 TermBridge 读到新 tag 会报 HelperFailed（可接受：helper 与 mcp 同
// 目录同版本发布）。

mod platform;
#[cfg(unix)]
mod signal_guard;

use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};

#[derive(Deserialize)]
struct PasswordRequest {
    #[serde(rename = "type")]
    msg_type: String,
    host: String,
    user: String,
    reason: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response {
    /// value = 密码；user = 对话框中实际编辑后的用户名（None = provider
    /// 不支持用户名编辑，调用方回退请求预填值）。
    Password { value: String, user: Option<String> },
    /// 用户取消（对话框 / 终端已展示给用户后的取消动作）。
    Cancelled,
    /// 当前环境没有任何可用的输入通道（message 含可行动指引）。
    Unsupported { message: String },
    /// provider 执行失败（如 TERMBRIDGE_ASKPASS 指向的程序损坏）。
    Failed { message: String },
}

fn main() {
    // SIGTERM / SIGINT 守卫：父进程超时 kill 时恢复 termios、清理子进程
    #[cfg(unix)]
    signal_guard::install();

    let stdin = io::stdin();
    let mut line = String::new();
    let _ = stdin.lock().read_line(&mut line);

    let response = (|| -> Option<Response> {
        let req: PasswordRequest = serde_json::from_str(line.trim()).ok()?;
        if req.msg_type != "password_request" {
            return None;
        }
        Some(
            match platform::prompt_password(&req.host, &req.user, &req.reason) {
                platform::PromptOutcome::Password { user, password } => Response::Password {
                    value: password,
                    user,
                },
                platform::PromptOutcome::Cancelled => Response::Cancelled,
                platform::PromptOutcome::Unsupported { message } => {
                    Response::Unsupported { message }
                }
                platform::PromptOutcome::Failed { message } => Response::Failed { message },
            },
        )
    })()
    .unwrap_or(Response::Cancelled);

    let json =
        serde_json::to_string(&response).unwrap_or_else(|_| r#"{"type":"cancelled"}"#.to_string());

    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let _ = writeln!(handle, "{}", json);
    let _ = handle.flush();
}
