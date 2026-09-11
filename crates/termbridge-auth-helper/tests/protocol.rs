// helper 协议 v2 stub 集成测试（ADR-0019）。
//
// 用 stub 程序（shell 脚本 / 不存在的路径）替换真实平台实现的可控部分：
// - TERMBRIDGE_ASKPASS 路径：askpass 是纯外部程序，stub 完全可控
// - 坏 DISPLAY + setsid（无控制终端）：GUI 链与 TTY 全部不可用 → unsupported
// - TTY 路径：script(1) 提供真实 PTY
//
// GUI dialog 成功路径无法 headless 自动化（需要真实桌面交互），其管道语义
// 与 askpass 成功路径同构（同为 wait_with_output + stdout 解析），由
// askpass 测试 + prompt_cmd 单测覆盖。

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn helper_bin() -> PathBuf {
    // tests/ 在 crate 根，target/ 在 workspace 根
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    path.push("target");
    path.push("debug");
    path.push("termbridge-auth-helper");
    path
}

fn run_helper(env: &[(&str, &str)], detach_tty: bool) -> (String, i32) {
    let mut cmd = Command::new(helper_bin());
    cmd.env_remove("TERMBRIDGE_ASKPASS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    if detach_tty {
        // setsid 脱离控制终端：模拟 GUI 客户端（VSCode 等）spawn MCP server
        // 的环境——/dev/tty 不可用
        cmd.env_remove("DISPLAY");
        cmd.env_remove("WAYLAND_DISPLAY");
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().expect("spawn helper");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            br#"{"type":"password_request","host":"192.0.2.10","user":"root","reason":"integration test"}"#,
        )
        .unwrap();
    let out = child.wait_with_output().expect("wait helper");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        out.status.code().unwrap_or(-1),
    )
}

fn write_script(name: &str, body: &str) -> String {
    let path =
        std::env::temp_dir().join(format!("termbridge-test-{name}-{}.sh", std::process::id()));
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

// ── 协议 v2：四态响应 ───────────────────────────────────────────────────

#[test]
fn askpass_success_returns_password_with_null_user() {
    let script = write_script("ok", "#!/bin/sh\necho stub-password\n");
    let (stdout, code) = run_helper(&[("TERMBRIDGE_ASKPASS", &script)], true);
    assert_eq!(code, 0);
    assert_eq!(
        stdout,
        r#"{"type":"password","value":"stub-password","user":null}"#
    );
}

#[test]
fn askpass_cancel_returns_cancelled() {
    // askpass 惯例：非零 + 空 stderr = 用户取消
    let script = write_script("cancel", "#!/bin/sh\nexit 1\n");
    let (stdout, _) = run_helper(&[("TERMBRIDGE_ASKPASS", &script)], true);
    assert_eq!(stdout, r#"{"type":"cancelled"}"#);
}

#[test]
fn askpass_failure_returns_failed_not_cancelled() {
    // 协议 v2 核心区分：显式配置的 provider 损坏 → failed（不级联不折叠）
    let script = write_script(
        "broken",
        "#!/bin/sh\necho 'stub askpass crashed' >&2\nexit 127\n",
    );
    let (stdout, _) = run_helper(&[("TERMBRIDGE_ASKPASS", &script)], true);
    assert!(stdout.starts_with(r#"{"type":"failed","message":"#));
    assert!(
        stdout.contains("stub askpass crashed"),
        "应含失败原因: {stdout}"
    );
    assert!(
        stdout.contains("TERMBRIDGE_ASKPASS"),
        "应指明是哪个 provider: {stdout}"
    );
}

#[test]
fn askpass_missing_program_returns_failed() {
    let (stdout, _) = run_helper(&[("TERMBRIDGE_ASKPASS", "/nonexistent/askpass")], true);
    assert!(stdout.starts_with(r#"{"type":"failed""#));
    assert!(stdout.contains("cannot be executed"), "{stdout}");
}

#[test]
fn no_channel_returns_actionable_unsupported() {
    // GUI 客户端典型场景：无 DISPLAY + 无控制终端。
    // 旧协议这里返回 cancelled（误导）；v2 必须是 unsupported + 三条出路
    let (stdout, _) = run_helper(&[], true);
    assert!(
        stdout.starts_with(r#"{"type":"unsupported","message":""#),
        "{stdout}"
    );
    assert!(
        stdout.contains("root@192.0.2.10"),
        "应含 host/user: {stdout}"
    );
    assert!(
        stdout.contains("zenity"),
        "应建议安装 GUI provider: {stdout}"
    );
    assert!(
        stdout.contains("TERMBRIDGE_ASKPASS"),
        "应建议 askpass: {stdout}"
    );
    assert!(
        stdout.contains("interactive terminal"),
        "应建议 TTY: {stdout}"
    );
    assert!(stdout.contains("tried:"), "应含尝试轨迹: {stdout}");
}

#[test]
fn garbage_request_returns_cancelled() {
    // 保守默认：无法解析的请求不泄漏内部细节
    let mut cmd = Command::new(helper_bin());
    cmd.env_remove("TERMBRIDGE_ASKPASS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"not json\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        r#"{"type":"cancelled"}"#
    );
}

// ── 级联行为：坏 DISPLAY 不误判取消 ────────────────────────────────────

#[test]
fn broken_display_cascades_to_unsupported_not_cancelled() {
    // DISPLAY 指向不存在的 X server（SSH X forwarding 失效等场景）：
    // zenity 快速失败 + stderr 特征 → 级联，最终 unsupported
    let (stdout, _) = run_helper(&[("DISPLAY", ":99")], true);
    assert!(stdout.starts_with(r#"{"type":"unsupported""#), "{stdout}");
    // 若 zenity 存在，轨迹应含其失败细节；若不存在则含 not found
    assert!(
        stdout.contains("zenity") || stdout.contains("not found"),
        "{stdout}"
    );
}

// ── TTY 路径：script(1) 提供真实 PTY ───────────────────────────────────

#[test]
fn tty_prompt_reads_password() {
    use std::io::Read;
    // script(1) 分配 PTY，stdin 的内容成为终端输入；无 DISPLAY → 直接落 TTY
    let mut child = Command::new("script")
        .arg("-qec")
        .arg(format!("{:?}", helper_bin().to_string_lossy()))
        .arg("/dev/null")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("TERMBRIDGE_ASKPASS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("script(1) required for PTY test");

    // 请求 + 密码 + 回车（PTY 内密码不回显）
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            b"{\"type\":\"password_request\",\"host\":\"h\",\"user\":\"u\",\"reason\":\"r\"}\ntty-password-42\n",
        )
        .unwrap();
    let mut out = String::new();
    child
        .wait_with_output()
        .unwrap()
        .stdout
        .as_slice()
        .read_to_string(&mut out)
        .unwrap();
    assert!(
        out.contains(r#""type":"password""#),
        "TTY 路径应返回 password: {out}"
    );
    assert!(out.contains("tty-password-42"), "应含输入的密码: {out}");
}
