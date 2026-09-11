// 共享：外部命令式凭据 prompt 的执行与级联信号分类（ADR-0019，Linux/macOS）。
//
// 级联规则（防止「用户点一次取消 → 连弹三个对话框」）：
// - spawn ENOENT（二进制不存在）/ 环境失败 stderr 特征 / 快速退出（<2s，
//   对话框来不及展示给任何用户）→ Unavailable，级联到下一 provider
// - 对话框已展示后的非零退出（含用户取消）→ Cancelled，停止级联
// - exit 0 + stdout → Success；exit 0 + 空 stdout → Cancelled（空提交）
// - 非零 + stdout 非空 → Cancelled（保守：stdout 可能含密码，直接丢弃不解析）

use std::io;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::platform::PromptOutcome;
use crate::signal_guard;

/// GUI dialog 命令的执行结果（分类后）。
#[derive(Debug)]
pub enum DialogResult {
    /// exit 0 且 stdout 非空：原始输出由调用方按 provider 格式解析。
    Success { stdout: String },
    /// 用户取消（对话框已展示后的非零退出）。
    Cancelled,
    /// 环境不可用（对话框从未展示）→ 级联到下一 provider。
    Unavailable { detail: String },
}

/// 运行 GUI dialog 命令并分类结果。
///
/// `always_cancel_codes`：provider 专属的「用户关窗」退出码（如 yad 的 252），
/// 命中时无条件视为 Cancelled（不参与快速退出启发）。
pub fn run_dialog(cmd: &mut Command, always_cancel_codes: &[i32]) -> DialogResult {
    let start = Instant::now();
    let spawned = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn();
    let child = match spawned {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return DialogResult::Unavailable {
                detail: "not found in PATH".into(),
            }
        }
        Err(e) => {
            return DialogResult::Unavailable {
                detail: format!("cannot execute: {e}"),
            }
        }
    };

    signal_guard::set_child(child.id());
    let output = child.wait_with_output();
    signal_guard::clear_child();
    let elapsed = start.elapsed();

    match output {
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            classify_dialog(code, elapsed, &stdout, &stderr, always_cancel_codes)
        }
        Err(e) => DialogResult::Unavailable {
            detail: format!("wait failed: {e}"),
        },
    }
}

/// 环境失败 stderr 特征（X / Wayland 不可达）。仅当非零退出且无 stdout 时
/// 检查——此时唯一合理的解释就是对话框没弹出来。
const ENV_FAILURE_PATTERNS: &[&str] = &[
    "cannot open display",          // GTK（zenity / yad）
    "could not connect to display", // Qt（kdialog）
    "cannot connect to display",
    "missing x server",
    "no protocol specified", // X authorization 失败
    "authorization required",
];

/// 用户取消 stderr 特征。优先于环境失败特征和快速退出启发检查：
/// osascript 取消 = 退出码 1 + stderr "User canceled. (-128)"，快速点取消
/// 时若无此检查会被误判为「环境不可用」而级联到下一 provider。
const USER_CANCEL_PATTERNS: &[&str] = &["user canceled", "user cancelled"];

/// 对话框来不及展示给任何用户的时间上限。GTK/Qt 初始化 ~0.5s，人类阅读 +
/// 点击取消不可能在 2s 内完成；反过来环境失败（坏 DISPLAY）实测 <0.1s。
const FAST_FAIL: Duration = Duration::from_secs(2);

/// dialog 结果分类（纯函数，单测覆盖）。
fn classify_dialog(
    code: i32,
    elapsed: Duration,
    stdout: &str,
    stderr: &str,
    always_cancel_codes: &[i32],
) -> DialogResult {
    let stdout_trimmed = trim_newlines(stdout);
    if code == 0 {
        if stdout_trimmed.is_empty() {
            DialogResult::Cancelled // 空提交（误触回车）按取消处理
        } else {
            DialogResult::Success {
                stdout: stdout_trimmed.to_string(),
            }
        }
    } else {
        // 非零 + stdout 非空：异常状态，保守当取消（输出可能含密码，丢弃）
        if !stdout_trimmed.is_empty() {
            return DialogResult::Cancelled;
        }
        let lower = stderr.to_ascii_lowercase();
        // 用户取消特征优先于环境失败 / 快速退出启发（见 USER_CANCEL_PATTERNS 注释）
        if USER_CANCEL_PATTERNS.iter().any(|p| lower.contains(p)) {
            return DialogResult::Cancelled;
        }
        if ENV_FAILURE_PATTERNS.iter().any(|p| lower.contains(p)) {
            return DialogResult::Unavailable {
                detail: first_line(stderr),
            };
        }
        if always_cancel_codes.contains(&code) {
            return DialogResult::Cancelled;
        }
        if elapsed < FAST_FAIL {
            return DialogResult::Unavailable {
                detail: "exited before showing dialog".into(),
            };
        }
        DialogResult::Cancelled
    }
}

// ── askpass（TERMBRIDGE_ASKPASS，显式配置：失败不级联）─────────────────

/// 运行 askpass 兼容程序：prompt 作为 $1（OpenSSH SSH_ASKPASS 契约），
/// 密码写 stdout，exit 0。
///
/// 失败语义（ADR-0019 §2.3）：
/// - spawn 失败 / 非零 + stderr 非空 → Failed：用户显式配置的程序损坏，
///   必须上报而不是静默级联（用户可能正是为了避开 GUI 才配置它）
/// - 非零 + stderr 空 → Cancelled：askpass 惯例（x11-ssh-askpass 等）用
///   非零 + 空输出表示用户取消
pub fn run_askpass(program: &str, prompt: &str) -> PromptOutcome {
    let mut cmd = Command::new(program);
    cmd.arg(prompt)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let spawned = cmd.spawn();
    let child = match spawned {
        Ok(c) => c,
        Err(e) => {
            return PromptOutcome::Failed {
                message: format!("TERMBRIDGE_ASKPASS program '{program}' cannot be executed: {e}"),
            }
        }
    };

    signal_guard::set_child(child.id());
    let output = child.wait_with_output();
    signal_guard::clear_child();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            let password = trim_newlines(&stdout);
            if out.status.success() {
                if password.is_empty() {
                    PromptOutcome::Cancelled
                } else {
                    // askpass 契约无用户名编辑
                    PromptOutcome::Password {
                        user: None,
                        password: password.to_string(),
                    }
                }
            } else if stderr.trim().is_empty() {
                PromptOutcome::Cancelled
            } else {
                PromptOutcome::Failed {
                    message: format!(
                        "TERMBRIDGE_ASKPASS program '{program}' failed: {}",
                        first_line(&stderr)
                    ),
                }
            }
        }
        Err(e) => PromptOutcome::Failed {
            message: format!("TERMBRIDGE_ASKPASS program '{program}' wait failed: {e}"),
        },
    }
}

// ── 小工具 ──────────────────────────────────────────────────────────────

/// 只去尾部换行（保留密码中合法的尾部空格）。
pub fn trim_newlines(s: &str) -> &str {
    s.trim_end_matches(['\n', '\r'])
}

fn first_line(s: &str) -> String {
    s.lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(200)
        .collect()
}

/// 读取 TERMBRIDGE_ASKPASS（空 / 纯空白视为未配置）。
pub fn askpass_program() -> Option<String> {
    std::env::var("TERMBRIDGE_ASKPASS")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unavailable(code: i32, elapsed_ms: u64, stdout: &str, stderr: &str) -> bool {
        matches!(
            classify_dialog(code, Duration::from_millis(elapsed_ms), stdout, stderr, &[]),
            DialogResult::Unavailable { .. }
        )
    }

    // ── 级联信号：环境失败 → Unavailable ─────────────────────────────

    #[test]
    fn env_failure_stderr_cascades() {
        // zenity 坏 DISPLAY 实测：44ms 退出码 1 + Gtk-WARNING（无论快慢都级联）
        assert!(unavailable(
            1,
            10_000,
            "",
            "(zenity:3456): Gtk-WARNING **: cannot open display: :99"
        ));
        assert!(unavailable(
            1,
            10_000,
            "",
            "qt.qpa.plugin: Could not connect to display"
        ));
        assert!(unavailable(1, 10_000, "", "No protocol specified"));
    }

    #[test]
    fn fast_exit_without_stderr_cascades() {
        // 未知环境错误（无 stderr 特征）但快速退出：对话框没展示过
        assert!(unavailable(1, 44, "", ""));
        assert!(unavailable(2, 1_999, "", "some unknown error"));
    }

    // ── 级联信号：用户取消 → Cancelled（不级联）──────────────────────

    #[test]
    fn slow_cancelled_does_not_cascade() {
        // 用户阅读对话框后取消：非零、无环境特征、耗时 > 2s
        assert!(!unavailable(1, 3_000, "", ""));
    }

    #[test]
    fn user_cancel_stderr_beats_fast_exit_heuristic() {
        // osascript 快速取消：退出码 1 + "User canceled" stderr，
        // 即使 <2s 也必须视为用户取消而非环境失败
        assert!(!unavailable(
            1,
            100,
            "",
            "execution error: User canceled. (-128)"
        ));
    }

    #[test]
    fn always_cancel_code_wins_even_when_fast() {
        // yad Esc 关窗 = 252：无条件 Cancelled，不参与快速退出启发
        let r = classify_dialog(252, Duration::from_millis(100), "", "", &[252]);
        assert!(matches!(r, DialogResult::Cancelled));
    }

    #[test]
    fn nonzero_with_stdout_is_cancelled() {
        // 异常带输出（可能含密码）：保守取消，不解析不级联
        let r = classify_dialog(1, Duration::from_secs(5), "leaked?", "", &[]);
        assert!(matches!(r, DialogResult::Cancelled));
    }

    // ── 成功路径 ──────────────────────────────────────────────────────

    #[test]
    fn exit_zero_with_stdout_is_success() {
        let r = classify_dialog(0, Duration::from_secs(5), "root|pw\n", "", &[]);
        match r {
            DialogResult::Success { stdout } => assert_eq!(stdout, "root|pw"),
            other => panic!("期望 Success，实际: {other:?}"),
        }
    }

    #[test]
    fn exit_zero_empty_stdout_is_cancelled() {
        let r = classify_dialog(0, Duration::from_secs(5), "\n", "", &[]);
        assert!(matches!(r, DialogResult::Cancelled));
    }

    // ── trim_newlines：保留尾部空格 ───────────────────────────────────

    #[test]
    fn trim_newlines_keeps_trailing_spaces() {
        assert_eq!(trim_newlines("pw \n"), "pw ");
        assert_eq!(trim_newlines("pw\r\n"), "pw");
        assert_eq!(trim_newlines("p w"), "p w");
    }
}
