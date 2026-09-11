// macOS 凭据输入 resolver（ADR-0019）：多级 fallback 链。
//
//   1. TERMBRIDGE_ASKPASS（显式配置，askpass 兼容程序；失败不级联）
//   2. GUI：osascript `display dialog ... with hidden answer`（系统自带，
//      无第三方依赖；无 Aqua 会话如 SSH headless 时失败 → 级联）
//   3. TTY：/dev/tty（headless 兜底）
//   4. Unsupported：带可行动指引
//
// 原生 AppKit dialog（Security framework / SwiftUI helper）留待后续阶段，
// osascript MVP 已满足「非干扰 + 系统自带 + 密码掩码」三个核心诉求。

use std::process::Command;

use super::prompt_cmd::{askpass_program, run_askpass, run_dialog, DialogResult};
use super::tty::{self, TtyError};
use super::PromptOutcome;

pub fn prompt_password(host: &str, user: &str, reason: &str) -> PromptOutcome {
    let prompt = format!("Password for {user}@{host} ({reason}): ");

    let mut trace: Vec<String> = Vec::new();

    // 1. 显式 askpass：终态语义（Password / Cancelled / Failed），不级联
    if let Some(program) = askpass_program() {
        return run_askpass(&program, &prompt);
    }
    trace.push("TERMBRIDGE_ASKPASS not set".into());

    // 2. osascript GUI：stderr 明确区分用户取消（error -128 "User canceled"）
    //    与环境失败（无 WindowServer 会话等）——后者级联到 TTY
    match osascript(&prompt) {
        DialogResult::Success { stdout } => {
            return PromptOutcome::Password {
                user: None,
                password: stdout,
            }
        }
        DialogResult::Cancelled => return PromptOutcome::Cancelled,
        DialogResult::Unavailable { detail } => trace.push(format!("osascript: {detail}")),
    }

    // 3. TTY 兜底
    match tty::prompt(&prompt) {
        Ok(password) => {
            return PromptOutcome::Password {
                user: None,
                password,
            }
        }
        Err(TtyError::Cancelled) => return PromptOutcome::Cancelled,
        Err(TtyError::Unavailable(detail)) => trace.push(detail),
    }

    // 4. 全部不可用：可行动指引
    PromptOutcome::Unsupported {
        message: unsupported_message(user, host, &trace),
    }
}

/// `osascript -e 'return text returned of (display dialog ... with hidden answer)'`
///
/// - OK：stdout = 原始密码文本（human-readable 格式不加引号不转义）
/// - 取消：AppleScript error -128 → osascript 退出码 1，stderr 含
///   "User canceled" → Cancelled
/// - 其它失败（headless 无 GUI 会话 / Apple Events 未授权等）→ Unavailable
fn osascript(prompt: &str) -> DialogResult {
    let script = format!(
        "return text returned of (display dialog \"{}\" default answer \"\" \
with hidden answer with title \"TermBridge Credential\")",
        escape_applescript(prompt)
    );
    let mut cmd = Command::new("osascript");
    cmd.arg("-e").arg(&script);
    // 取消信号（stderr "User canceled"）由共享分类器的 USER_CANCEL_PATTERNS
    // 识别，优先于快速退出启发——无需 always_cancel 码
    run_dialog(&mut cmd, &[])
}

/// AppleScript 字符串字面量转义（prompt 中的 host/user 来自 ssh config，
/// 用户可控，必须转义防止注入脚本）。
fn escape_applescript(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

fn unsupported_message(user: &str, host: &str, trace: &[String]) -> String {
    format!(
        "no interactive password prompt available for {user}@{host} \
(tried: {}). Options: (1) run from a desktop (Aqua) session; (2) set \
TERMBRIDGE_ASKPASS to an askpass-compatible program; (3) run from an \
interactive terminal.",
        trace.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_escaping() {
        assert_eq!(escape_applescript("plain"), "plain");
        assert_eq!(escape_applescript("a\"b"), "a\\\"b");
        assert_eq!(escape_applescript("a\\b"), "a\\\\b");
        // 注入尝试：引号闭合被转义
        assert_eq!(
            escape_applescript("\") & do shell script \"rm -rf /"),
            "\\\") & do shell script \\\"rm -rf /"
        );
    }

    #[test]
    fn unsupported_message_is_actionable() {
        let msg = unsupported_message(
            "root",
            "mac.example",
            &["osascript: No user interaction allowed".into()],
        );
        assert!(msg.contains("root@mac.example"));
        assert!(msg.contains("TERMBRIDGE_ASKPASS"));
        assert!(msg.contains("desktop"));
    }
}
