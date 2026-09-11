// Linux 凭据输入 resolver（ADR-0019）：多级 fallback 链。
//
//   1. TERMBRIDGE_ASKPASS（显式配置，askpass 兼容程序；失败不级联）
//   2. GUI：zenity → kdialog → yad（软依赖：PATH 找到才用，不打包不安装）
//   3. TTY：/dev/tty（headless 兜底；接受与宿主 TUI 抢输入的残余风险）
//   4. Unsupported：带可行动指引（三条出路）
//
// 设计原则是「优先选择不干扰宿主 Agent 交互通道的输入方式」（ADR-0019 §2.1），
// 而非简单的 GUI 优先：独立 GUI / 独立 askpass / 闲置 TTY 都不抢宿主的
// stdin；被宿主 TUI 占用的 TTY 才是问题，但 helper 视角无法检测占用，
// 只能靠顺序把 TTY 排最后。
//
// 注意（ADR-0019 §2.3）：v1 只读 TERMBRIDGE_ASKPASS，不读 SSH_ASKPASS。
// OpenSSH 的 askpass 契约是「有 TTY 就不用 askpass」，而 TermBridge 因为
// 宿主 TUI 占着 TTY 恰恰要 askpass 优先——同一个变量名、相反的触发条件
// 会给设置过 ksshaskpass 的用户带来与其 ssh 经验相反的行为。

use std::process::Command;

use super::prompt_cmd::{askpass_program, run_askpass, run_dialog, DialogResult};
use super::tty::{self, TtyError};
use super::PromptOutcome;

pub fn prompt_password(host: &str, user: &str, reason: &str) -> PromptOutcome {
    let prompt = format!("Password for {user}@{host} ({reason}): ");

    // 记录每级尝试的失败原因，最终 Unsupported 时给出完整排障上下文
    let mut trace: Vec<String> = Vec::new();

    // 1. 显式 askpass：终态语义（Password / Cancelled / Failed），不级联
    if let Some(program) = askpass_program() {
        return run_askpass(&program, &prompt);
    }
    trace.push("TERMBRIDGE_ASKPASS not set".into());

    // 2. GUI 链：仅在显示会话存在时尝试（DISPLAY / WAYLAND_DISPLAY 是启发式
    //    预检；真正的判定是执行后的级联信号分类）
    if has_display_session() {
        for (name, result) in [
            ("zenity", zenity(&prompt)),
            ("kdialog", kdialog(&prompt)),
            ("yad", yad(&prompt)),
        ] {
            match result {
                DialogResult::Success { stdout } => {
                    return match name {
                        // zenity --username 官方输出格式 "user|password"
                        "zenity" => parse_zenity_output(&stdout),
                        _ => PromptOutcome::Password {
                            user: None,
                            password: stdout,
                        },
                    };
                }
                DialogResult::Cancelled => return PromptOutcome::Cancelled,
                DialogResult::Unavailable { detail } => {
                    trace.push(format!("{name}: {detail}"));
                }
            }
        }
    } else {
        trace.push("no display session (DISPLAY/WAYLAND_DISPLAY unset)".into());
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

fn has_display_session() -> bool {
    fn non_empty(var: &str) -> bool {
        std::env::var_os(var)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }
    non_empty("DISPLAY") || non_empty("WAYLAND_DISPLAY")
}

/// zenity `--password --username` 输出 `user|password`（官方文档示例用
/// `cut -d'|'` 解析）。密码中的 `|` 由 split_once 天然保留；用户名为空
/// 视为未编辑（回退预填值）；密码为空视为取消（误触回车）。
fn parse_zenity_output(stdout: &str) -> PromptOutcome {
    match stdout.split_once('|') {
        Some((user, password)) if !password.is_empty() => PromptOutcome::Password {
            user: (!user.is_empty()).then(|| user.to_string()),
            password: password.to_string(),
        },
        _ => PromptOutcome::Cancelled,
    }
}

fn zenity(prompt: &str) -> DialogResult {
    let mut cmd = Command::new("zenity");
    cmd.arg("--title")
        .arg("TermBridge Credential")
        .arg("--text")
        .arg(prompt)
        .arg("--password")
        .arg("--username");
    // --text 实测被密码框接受（对话框正文显示 host/user/reason）。
    // 非法选项退出码 255 + stderr 提示（实测），会落入快速退出级联分支，
    // 不会误判为用户取消。
    run_dialog(&mut cmd, &[])
}

fn kdialog(prompt: &str) -> DialogResult {
    let mut cmd = Command::new("kdialog");
    cmd.arg("--title")
        .arg("TermBridge Credential")
        .arg("--password")
        .arg(prompt);
    run_dialog(&mut cmd, &[])
}

fn yad(prompt: &str) -> DialogResult {
    let mut cmd = Command::new("yad");
    cmd.arg("--title")
        .arg("TermBridge Credential")
        .arg("--text")
        .arg(prompt)
        .arg("--entry")
        .arg("--hide-text");
    // yad 关窗（Esc / WM close）= 252，是用户动作而非环境失败
    run_dialog(&mut cmd, &[252])
}

/// 全链不可用时的可行动错误（Agent 可直接转述给用户）。
fn unsupported_message(user: &str, host: &str, trace: &[String]) -> String {
    format!(
        "no interactive password prompt available for {user}@{host} \
(tried: {}). Options: (1) install zenity, kdialog or yad on a desktop session; \
(2) set TERMBRIDGE_ASKPASS to an askpass-compatible program; (3) run from an \
interactive terminal.",
        trace.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── zenity 输出解析 ───────────────────────────────────────────────

    #[test]
    fn zenity_output_with_username() {
        match parse_zenity_output("alice|s3cret") {
            PromptOutcome::Password { user, password } => {
                assert_eq!(user.as_deref(), Some("alice"));
                assert_eq!(password, "s3cret");
            }
            other => panic!("期望 Password，实际: {other:?}"),
        }
    }

    #[test]
    fn zenity_output_password_with_pipe() {
        // 密码含 |：split_once 只切第一个，密码保留剩余部分
        match parse_zenity_output("alice|p|w") {
            PromptOutcome::Password { user, password } => {
                assert_eq!(user.as_deref(), Some("alice"));
                assert_eq!(password, "p|w");
            }
            other => panic!("期望 Password，实际: {other:?}"),
        }
    }

    #[test]
    fn zenity_output_empty_username_falls_back_to_none() {
        match parse_zenity_output("|s3cret") {
            PromptOutcome::Password { user, password } => {
                assert_eq!(user, None, "空用户名应回退预填值");
                assert_eq!(password, "s3cret");
            }
            other => panic!("期望 Password，实际: {other:?}"),
        }
    }

    #[test]
    fn zenity_output_empty_password_is_cancelled() {
        assert!(matches!(
            parse_zenity_output("alice|"),
            PromptOutcome::Cancelled
        ));
    }

    // ── 可行动错误 ────────────────────────────────────────────────────

    #[test]
    fn unsupported_message_is_actionable() {
        let msg = unsupported_message(
            "root",
            "192.0.2.10",
            &[
                "TERMBRIDGE_ASKPASS not set".into(),
                "zenity: cannot open display".into(),
                "/dev/tty: No such device".into(),
            ],
        );
        assert!(msg.contains("root@192.0.2.10"));
        assert!(
            msg.contains("zenity: cannot open display"),
            "应含失败上下文"
        );
        assert!(msg.contains("TERMBRIDGE_ASKPASS"), "应给出出路 (2)");
        assert!(msg.contains("interactive terminal"), "应给出出路 (3)");
    }
}
