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

use termbridge::application::host_policy::{default_config_path, HostPolicyResolver};
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
    /// 显示 host policy（ADR-0017：hosts.toml 中的 per-host 策略）
    Policy {
        /// 可选：只显示某个 host 的策略（省略则显示全部已配置 host）
        host: Option<String>,
    },
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
    /// 列出运行中的 MCP server instance（ADR-0018）
    McpList,
    /// 列出 MCP server 上的 session（ADR-0018）
    SessionList {
        /// 可选：指定 MCP instance ID（默认连第一个）
        #[arg(long)]
        instance: Option<String>,
    },
    /// 批准 session 进入 unrestricted 模式（ADR-0018）
    SessionApprove {
        /// session ID
        session_id: String,
        /// 可选：指定 MCP instance ID
        #[arg(long)]
        instance: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // 启动时检查是否有新版本（与 termbridge-mcp 一致：本地缓存 + 24h 限频 +
    // 后台刷新，仅提示不自动安装；TERMBRIDGE_NO_UPDATE_CHECK=1 可关闭）
    termbridge::infrastructure::update_check::check_for_updates();

    let cli = Cli::parse();
    match cli.command {
        Command::Hosts => cmd_hosts(),
        Command::Policy { host } => cmd_policy(host.as_deref()),
        Command::Connect { host } => cmd_connect(&host).await,
        Command::Sessions { host } => cmd_sessions(&host).await,
        Command::Attach { host, session_id } => cmd_attach(&host, &session_id).await,
        Command::Detach { session_id } => cmd_detach(&session_id),
        Command::McpList => cmd_mcp_list(),
        Command::SessionList { instance } => cmd_session_list(instance.as_deref()).await,
        Command::SessionApprove { session_id, instance } => {
            cmd_session_approve(&session_id, instance.as_deref()).await
        }
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
// policy（ADR-0017 §4 第 6 步：展示 host policy）
// ───────────────────────────────────────────────────────────────────────────

/// 显示 hosts.toml 中的 host policy（ADR-0017）。
///
/// - `termbridge policy`：列出全部已配置 host 的**有效策略**（host policy > system default）
/// - `termbridge policy <host>`：单 host 视图——已配置值 + 有效值 + 修改提示
///
/// 只读展示，不修改配置（ADR-0017 §2.2 不可变原则）。配置文件由用户手动编辑。
fn cmd_policy(host: Option<&str>) -> Result<()> {
    let resolver = HostPolicyResolver::load_default();
    let path = default_config_path();

    match host {
        // 单 host 视图：已配置值（可能有省略）+ 有效值（已合并优先级）
        Some(alias) => {
            println!("host: {alias}");
            println!("config: {}", path.display());
            match resolver.get_host_policy(alias) {
                Some(policy) => {
                    let auth = policy
                        .auth
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| "-".into());
                    let session = policy
                        .session
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "-".into());
                    println!("configured: auth = {auth}, session = {session} (- = 省略，走 system default)");
                }
                None => {
                    println!("configured: (none — 未在 hosts.toml 配置，走 system default)");
                }
            }
            let effective = resolver.resolve(alias, None, None);
            println!("effective: auth = {}, session = {}", effective.auth, effective.session);
            println!("hint: edit {} to change this host's policy", path.display());
        }
        // 全部视图：已配置 host 的有效策略表
        None => {
            println!("config: {}", path.display());
            let mut hosts = resolver.list_configured_hosts();
            if hosts.is_empty() {
                println!("(no hosts.toml — all hosts use system defaults: auth = auto, session = standard)");
                println!(
                    "hint: create {} to configure per-host policies: [hosts.<alias>] auth = \"key\" | \"password\" | \"auto\", session = \"standard\" | \"persistent\" (IP 类别名必须加引号: [hosts.\"192.168.1.180\"])",
                    path.display()
                );
                return Ok(());
            }
            hosts.sort_unstable();
            println!("{:<20} {:<10} {:<10}", "HOST", "AUTH", "SESSION");
            for alias in hosts {
                let p = resolver.resolve(alias, None, None);
                println!("{:<20} {:<10} {:<10}", alias, p.auth, p.session);
            }
            println!("hint: hosts not listed above use system defaults (auth = auto, session = standard)");
        }
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
            // CLI connect 命令语义 = 显式开 persistent session（ADR-0017 §2.4：
            // explicit > host policy——用户选择 connect 即显式要求 persistent）
            Some(true),
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

// ───────────────────────────────────────────────────────────────────────────
// Control IPC 客户端（ADR-0018）
// ───────────────────────────────────────────────────────────────────────────

/// 连接到 MCP server 的 Control IPC，发送 HELLO + 请求。
///
/// 流程：发现 instance → 连接 endpoint → HELLO token → 发请求 → 读响应
async fn control_ipc_call(
    instance_id: Option<&str>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // 1. 发现 instance
    let instances = termbridge::transport::control::InstanceRegistry::list_instances();
    if instances.is_empty() {
        anyhow::bail!(
            "no running TermBridge MCP server found.\n\
             Start one first (e.g. via your MCP client), then retry."
        );
    }

    // 选择 instance：指定 ID（按 endpoint 子串匹配）或取第一个
    let info = if let Some(id) = instance_id {
        instances
            .iter()
            .find(|i| i.endpoint.contains(id))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "instance '{}' not found among {} running instances",
                    id,
                    instances.len()
                )
            })?
    } else {
        if instances.len() > 1 {
            eprintln!(
                "[提示] 发现 {} 个运行中的 MCP server，使用第一个。用 --instance 指定。",
                instances.len()
            );
        }
        &instances[0]
    };

    // 2. 连接 endpoint（用 Box<dyn AsyncRead/AsyncWrite> 抽象平台差异）
    let (reader, mut writer): (
        Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    ) = connect_endpoint_split(&info.endpoint).await?;

    // 3. HELLO token 认证
    let hello = serde_json::json!({"token": &info.token});
    writer.write_all(format!("{hello}\n").as_bytes()).await?;
    writer.flush().await?;

    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let hello_resp: serde_json::Value = serde_json::from_str(line.trim())?;
    if hello_resp.get("ok") != Some(&serde_json::Value::Bool(true)) {
        anyhow::bail!("HELLO authentication failed: {}", hello_resp);
    }

    // 4. 发请求
    line.clear();
    let req = serde_json::json!({"id": 1, "method": method, "params": params});
    writer.write_all(format!("{req}\n").as_bytes()).await?;
    writer.flush().await?;
    reader.read_line(&mut line).await?;
    let resp: serde_json::Value = serde_json::from_str(line.trim())?;
    if resp.get("ok") == Some(&serde_json::Value::Bool(true)) {
        Ok(resp["result"].clone())
    } else {
        let err = &resp["error"];
        anyhow::bail!(
            "control IPC error: {} - {}",
            err["code"].as_str().unwrap_or("?"),
            err["message"].as_str().unwrap_or("?")
        )
    }
}

/// 连接到 endpoint，返回分离的 reader/writer（平台抽象）。
///
/// - Linux/macOS：endpoint 以 `/` 开头时走 Unix socket，否则按 TCP 处理
/// - Windows：恒走 TCP loopback（endpoint 形如 "tcp://127.0.0.1:<port>"）
async fn connect_endpoint_split(
    endpoint: &str,
) -> Result<(
    Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
)> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if endpoint.starts_with('/') {
            let stream = tokio::net::UnixStream::connect(endpoint)
                .await
                .map_err(|e| anyhow::anyhow!("connect Unix socket {} failed: {}", endpoint, e))?;
            let (r, w) = tokio::io::split(stream);
            return Ok((Box::new(r), Box::new(w)));
        }
    }

    // TCP loopback
    let addr = endpoint.strip_prefix("tcp://").unwrap_or(endpoint);
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| anyhow::anyhow!("connect to {} failed: {}", endpoint, e))?;
    let (r, w) = tokio::io::split(stream);
    Ok((Box::new(r), Box::new(w)))
}

/// termbridge mcp list —— 列出运行中的 MCP server instance
fn cmd_mcp_list() -> Result<()> {
    let instances = termbridge::transport::control::InstanceRegistry::list_instances();
    if instances.is_empty() {
        println!("(no running TermBridge MCP server)");
        println!("hint: start an MCP client (Claude / Codex / OpenCode) to launch termbridge-mcp");
        return Ok(());
    }
    println!(
        "{:<10} {:<8} {:<15} {:<20}",
        "INSTANCE", "PID", "TRANSPORT", "ENDPOINT"
    );
    println!("{:-<60}", "");
    for i in &instances {
        // 从 endpoint 提取末尾段作为 instance 标识（如 mcp-abc123.sock）
        let id = i
            .endpoint
            .rsplit(|c| c == '/' || c == '\\')
            .next()
            .unwrap_or(&i.endpoint);
        let id_short = &id[..id.len().min(8)];
        println!(
            "{:<10} {:<8} {:<15} {:<20}",
            id_short, i.pid, i.transport, i.endpoint
        );
    }
    Ok(())
}

/// termbridge session list —— 列出 MCP server 上的 session
async fn cmd_session_list(instance: Option<&str>) -> Result<()> {
    let result = control_ipc_call(instance, "session.list", serde_json::json!({})).await?;
    let sessions = result
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("unexpected response: not an array"))?;
    if sessions.is_empty() {
        println!("(no sessions)");
        return Ok(());
    }
    println!(
        "{:<20} {:<20} {:<10} {:<15}",
        "ID", "HOST", "STATE", "APPROVAL"
    );
    println!("{:-<70}", "");
    for s in sessions {
        let id = s["id"].as_str().unwrap_or("?");
        let id_short = &id[..id.len().min(20)];
        println!(
            "{:<20} {:<20} {:<10} {:<15}",
            id_short,
            s["host"].as_str().unwrap_or("-"),
            s["state"].as_str().unwrap_or("?"),
            s["approval_mode"].as_str().unwrap_or("?"),
        );
    }
    Ok(())
}

/// termbridge session approve <session_id> —— 批准 session 进入 unrestricted 模式
async fn cmd_session_approve(session_id: &str, instance: Option<&str>) -> Result<()> {
    let result = control_ipc_call(
        instance,
        "session.set_approval_mode",
        serde_json::json!({"session_id": session_id, "mode": "unrestricted"}),
    )
    .await?;
    println!(
        "✓ session {} approved: {}",
        session_id,
        result["approval_mode"].as_str().unwrap_or("?")
    );
    println!("  TermBridge will skip command-level policy (sudo, rm -rf, etc.) for this session.");
    println!("  This does NOT bypass SSH credentials or remote server permissions.");
    println!("  Approval resets when the session closes.");
    Ok(())
}
