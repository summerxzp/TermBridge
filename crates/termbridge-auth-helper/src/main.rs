// ADR-0009 阶段 B1：termbridge-credential-prompt helper process。
//
// 职责：从 stdin 读一行 JSON 请求（password_request），弹出平台原生
// 凭据对话框获取密码，向 stdout 写一行 JSON 响应。
//
// 保守策略：任何解析错误 / 平台错误 / 用户取消，统一回 cancelled，
// 不向 TermBridge 暴露 helper 内部错误细节。

mod platform;

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
    Password { value: String },
    Cancelled,
}

fn main() {
    let stdin = io::stdin();
    let mut line = String::new();
    let _ = stdin.lock().read_line(&mut line);

    let response = (|| -> Option<Response> {
        let req: PasswordRequest = serde_json::from_str(line.trim()).ok()?;
        if req.msg_type != "password_request" {
            return None;
        }
        match platform::prompt_password(&req.host, &req.user, &req.reason) {
            Ok(password) => Some(Response::Password { value: password }),
            Err(_) => None,
        }
    })()
    .map_or(Response::Cancelled, |r| r);

    let json = serde_json::to_string(&response)
        .unwrap_or_else(|_| r#"{"type":"cancelled"}"#.to_string());

    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let _ = writeln!(handle, "{}", json);
    let _ = handle.flush();
}
