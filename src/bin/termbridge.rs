//! termbridge —— 人类管理员 CLI
//!
//! 与 `termbridge-mcp`（AI Agent MCP server）互补，提供交互式终端供管理员直接操作。
//! 命令：hosts / connect / sessions / attach / detach
//!
//! 交互式终端（connect/attach）流程：
//! 1. SessionManager.open_session / attach_remote_session → 拿到本地 session_id
//! 2. prepare_for_raw_mode → 中止 Session 内部 read_task（否则会与 read_raw 竞争 handle.read）
//! 3. crossterm enable_raw_mode → 终端进入 raw mode
//! 4. 事件线程：crossterm poll+read 同时处理键盘输入和窗口 resize
//!    - 键盘 → key_event_to_bytes → write_raw → 远端 PTY
//!    - resize → resize → 远端 PTY window_change
//! 5. 主循环：read_raw → stdout（PTY 输出实时显示）
//! 6. 退出（PTY EOF / 错误）：停事件线程 → disable_raw_mode → close/detach

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use termbridge::application::hosts::HostManager;
use termbridge::application::sessions::SessionManager;
use termbridge::domain::provider::{PtySize, TerminalProvider};
use termbridge::infrastructure::persistent::PersistentProvider;

// ───────────────────────────────────────────────────────────────────────────
// CLI 定义
// ───────────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "termbridge", about = "TermBridge CLI — 人类管理员工具")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 列出 ~/.ssh/config 中已配置的主机
    Hosts,
    /// 连接到主机（开新 persistent session，进入交互式终端）
    Connect {
        /// SSH config 中的 Host 别名
        host: String,
    },
    /// 列出远端 daemon 上的 persistent session
    Sessions {
        /// SSH config 中的 Host 别名
        host: String,
    },
    /// attach 到远端已有 session（进入交互式终端，退出时 detach 保留远端 session）
    Attach {
        /// SSH config 中的 Host 别名
        host: String,
        /// 远端 session ID（可用 sessions 命令查看）
        session_id: String,
    },
    /// detach 命令（交互式终端退出时自动处理，此命令暂不支持独立 detach）
    Detach {
        /// 本地 session ID
        session_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Hosts => cmd_hosts(),
        Command::Connect { host } => cmd_connect(&host).await,
        Command::Sessions { host } => cmd_sessions(&host).await,
        Command::Attach { host, session_id } => cmd_attach(&host, &session_id).await,
        Command::Detach { session_id } => cmd_detach(&session_id),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// hosts
// ───────────────────────────────────────────────────────────────────────────

fn cmd_hosts() -> Result<()> {
    let mgr = HostManager::new();
    let hosts = mgr.list_hosts();
    if hosts.is_empty() {
        println!("(no hosts in ~/.ssh/config)");
        return Ok(());
    }
    println!("{:<20} {}", "NAME", "HOSTNAME");
    for h in hosts {
        println!(
            "{:<20} {}",
            h.alias,
            h.hostname.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// sessions
// ───────────────────────────────────────────────────────────────────────────

async fn cmd_sessions(host: &str) -> Result<()> {
    let provider = Arc::new(PersistentProvider::default()) as Arc<dyn TerminalProvider>;
    let mgr = SessionManager::new(provider);
    let sessions = mgr
        .list_remote_sessions(&host.to_string())
        .await
        .context("list_remote_sessions failed")?;
    if sessions.is_empty() {
        println!("(no remote sessions)");
        return Ok(());
    }
    println!("{:<10} {:<20} {:<10}", "ID", "NAME", "STATE");
    for s in sessions {
        let id_short = &s.id[..s.id.len().min(8)];
        println!(
            "{:<10} {:<20} {:<10}",
            id_short,
            s.name.as_deref().unwrap_or("-"),
            s.state
        );
    }
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// connect
// ───────────────────────────────────────────────────────────────────────────

async fn cmd_connect(host: &str) -> Result<()> {
    let provider = Arc::new(PersistentProvider::default()) as Arc<dyn TerminalProvider>;
    let mgr = Arc::new(SessionManager::new(provider));

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let session_id = mgr
        .open_session(
            &host.to_string(),
            Some(PtySize { rows, cols }),
            true,
            Some(format!("cli-{ts}")),
        )
        .await
        .context("open_session failed")?;

    let id_short = &session_id[..session_id.len().min(8)];
    println!("Connected to {host} (session: {id_short})");

    let result = interactive_terminal(Arc::clone(&mgr), &session_id).await;

    // connect 退出时 close（新 session，用户拥有生命周期）
    let _ = mgr.close_session(&session_id).await;
    if result.is_ok() {
        println!("\nDisconnected.");
    }
    result
}

// ───────────────────────────────────────────────────────────────────────────
// attach
// ───────────────────────────────────────────────────────────────────────────

async fn cmd_attach(host: &str, remote_session_id: &str) -> Result<()> {
    let provider = Arc::new(PersistentProvider::default()) as Arc<dyn TerminalProvider>;
    let mgr = Arc::new(SessionManager::new(provider));

    let session_id = mgr
        .attach_remote_session(
            &host.to_string(),
            remote_session_id,
            Some("cli-reattach".to_string()),
        )
        .await
        .context("attach_remote_session failed")?;

    let id_short = &session_id[..session_id.len().min(8)];
    println!("Attached to {remote_session_id} (local session: {id_short})");

    let result = interactive_terminal(Arc::clone(&mgr), &session_id).await;

    // attach 退出时 detach（保留远端 session 供后续 re-attach）
    let _ = mgr.detach_session(&session_id).await;
    if result.is_ok() {
        println!("\nDisconnected (remote session preserved).");
    }
    result
}

// ───────────────────────────────────────────────────────────────────────────
// detach
// ───────────────────────────────────────────────────────────────────────────

fn cmd_detach(_session_id: &str) -> Result<()> {
    // CLI 脚本模式不支持独立 detach（本地 session 不跨进程存活）。
    // 交互式终端内关闭终端窗口或 Ctrl+D 退出远端 shell 即可断开。
    println!("detach is only available in interactive mode.");
    println!("  connect 退出时自动 close 远端 session。");
    println!("  attach 退出时自动 detach 保留远端 session。");
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// 交互式终端（connect/attach 共用）
// ───────────────────────────────────────────────────────────────────────────

async fn interactive_terminal(
    mgr: Arc<SessionManager>,
    session_id: &str,
) -> Result<()> {
    // 1. 中止 Session 内部 read_task，让 read_raw 独占 handle.read()
    //    （否则 read_task 和 read_raw 都是单消费者，会交替拿到数据导致输出丢失）
    mgr.prepare_for_raw_mode(session_id)
        .context("prepare_for_raw_mode failed")?;

    // 2. 进入 raw mode
    crossterm::terminal::enable_raw_mode().context("enable_raw_mode failed")?;

    // 3. 同步当前终端尺寸到远端 PTY
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let _ = mgr.resize(session_id, cols, rows).await;

    // 4. 创建 input/resize 事件 channel
    let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(64);
    let (resize_tx, mut resize_rx) = mpsc::channel::<(u16, u16)>(16);
    let running = Arc::new(AtomicBool::new(true));

    // 5. spawn 事件读取线程（blocking: crossterm poll + read）
    //    用 crossterm event 系统同时处理键盘和 resize，避免与 tokio::stdin 竞争
    //    （在 raw mode 下，crossterm event 和 tokio stdin 都从同一终端读取，会互相抢数据）
    let running_clone = Arc::clone(&running);
    let event_thread = std::thread::spawn(move || {
        while running_clone.load(Ordering::Relaxed) {
            if let Ok(true) = crossterm::event::poll(std::time::Duration::from_millis(100)) {
                match crossterm::event::read() {
                    Ok(crossterm::event::Event::Key(key)) => {
                        // Windows 会产生 Press/Repeat/Release，只处理 Press 和 Repeat
                        use crossterm::event::KeyEventKind;
                        if matches!(
                            key.kind,
                            KeyEventKind::Press | KeyEventKind::Repeat
                        ) {
                            if let Some(bytes) = key_event_to_bytes(key) {
                                let _ = input_tx.blocking_send(bytes);
                            }
                        }
                    }
                    Ok(crossterm::event::Event::Resize(cols, rows)) => {
                        let _ = resize_tx.blocking_send((cols, rows));
                    }
                    _ => {}
                }
            }
        }
    });

    // 6. spawn write task: 键盘输入 → write_raw → 远端 PTY
    //    input_rx 关闭（事件线程退出 / stdin EOF）时，write_task 结束，
    //    通过 select! 让主循环感知并退出（否则主循环会卡在 read_raw 等 PTY EOF）
    let mgr_write = Arc::clone(&mgr);
    let sid_write = session_id.to_string();
    let mut write_task = tokio::spawn(async move {
        while let Some(bytes) = input_rx.recv().await {
            if let Err(e) = mgr_write.write_raw(&sid_write, &bytes).await {
                eprintln!("write_raw error: {e}");
                break;
            }
        }
    });

    // 7. spawn resize task: resize 事件 → resize → 远端 PTY
    let mgr_resize = Arc::clone(&mgr);
    let sid_resize = session_id.to_string();
    let resize_task = tokio::spawn(async move {
        while let Some((cols, rows)) = resize_rx.recv().await {
            if let Err(e) = mgr_resize.resize(&sid_resize, cols, rows).await {
                eprintln!("resize error: {e}");
            }
        }
    });

    // 8. 主循环: read_raw → stdout（PTY 输出实时显示）
    //    用 select! 同时等 PTY 输出和 write_task 结束
    //    write_task 结束（stdin EOF / 事件线程退出）→ 发 Ctrl+D 给远端 shell → 等 PTY EOF
    let mut stdout = tokio::io::stdout();
    let main_result = loop {
        tokio::select! {
            // PTY → stdout
            read_res = mgr.read_raw(session_id) => {
                match read_res {
                    Ok(Some(data)) => {
                        if let Err(e) = stdout.write_all(&data).await {
                            break Err(anyhow::anyhow!("stdout write error: {e}"));
                        }
                        if let Err(e) = stdout.flush().await {
                            break Err(anyhow::anyhow!("stdout flush error: {e}"));
                        }
                    }
                    Ok(None) => break Ok(()), // PTY EOF（远端 shell 退出 / 连接断开）
                    Err(e) => break Err(anyhow::anyhow!("read_raw error: {e}")),
                }
            }
            // write_task 结束 → stdin EOF，发 Ctrl+D 让远端 shell 退出
            _ = &mut write_task => {
                // 发 Ctrl+D 触发远端 shell 退出（空行 EOF）
                let _ = mgr.send_control(session_id, termbridge::domain::provider::ControlKey::CtrlD).await;
                // 继续读 PTY 直到 EOF（shell 退出后的最后输出）
                loop {
                    match mgr.read_raw(session_id).await {
                        Ok(Some(data)) => {
                            let _ = stdout.write_all(&data).await;
                            let _ = stdout.flush().await;
                        }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
                break Ok(());
            }
        }
    };

    // 9. 清理：停事件线程 → 禁用 raw mode → abort resize task
    running.store(false, Ordering::Relaxed);
    let _ = event_thread.join(); // 等待事件线程退出（最多 100ms）
    crossterm::terminal::disable_raw_mode().ok();
    resize_task.abort();

    main_result
}

// ───────────────────────────────────────────────────────────────────────────
// 键盘事件 → PTY 字节序列
// ───────────────────────────────────────────────────────────────────────────

/// 将 crossterm KeyEvent 转为 PTY 字节序列。
///
/// 处理：
/// - Ctrl+letter → 0x01-0x1a（终端控制字符，如 Ctrl+C=0x03, Ctrl+D=0x04）
/// - Alt+key → ESC + key（Emacs 风格 Meta 键）
/// - 普通字符 → UTF-8
/// - 方向键 / Home / End / PageUp / PageDown / Delete / Insert → ANSI escape 序列
fn key_event_to_bytes(key: crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    use crossterm::event::{KeyCode, KeyModifiers};

    let code = key.code;
    let mods = key.modifiers;

    // Ctrl+letter → 0x01-0x1a
    if mods.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = code {
            let lower = c.to_ascii_lowercase();
            if ('a'..='z').contains(&lower) {
                return Some(vec![(lower as u8) - b'a' + 1]);
            }
        }
    }

    // Alt+key → ESC + key bytes
    if mods.contains(KeyModifiers::ALT) {
        if let Some(mut bytes) = key_code_to_bytes(code) {
            let mut result = vec![0x1b];
            result.append(&mut bytes);
            return Some(result);
        }
    }

    // 普通键
    key_code_to_bytes(code)
}

fn key_code_to_bytes(code: crossterm::event::KeyCode) -> Option<Vec<u8>> {
    use crossterm::event::KeyCode;
    match code {
        KeyCode::Char(c) => Some(c.to_string().into_bytes()),
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Tab => Some(b"\t".to_vec()),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Backspace => Some(b"\x7f".to_vec()),
        KeyCode::Esc => Some(b"\x1b".to_vec()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        _ => None, // F-keys 等暂不处理
    }
}
