//! termbridge-cli —— ADR-0004 Phase 3-A W2 测试用 Linux CLI client
//!
//! 纯测试工具，不进 MCP。用于在 Linux 主机上手动测试 termbridge-agentd daemon。

mod client;
mod protocol;

use std::path::{Path, PathBuf};

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use client::DaemonClient;
use protocol::{events, methods, read_msg, write_msg, PtySize, Request};

#[derive(Parser)]
#[command(
    name = "termbridge-cli",
    about = "termbridge-agentd 测试 client（ADR-0004 Phase 3-A W2）"
)]
struct Cli {
    /// daemon Unix socket 路径
    #[arg(long, default_value = "/run/user/1000/termbridge.sock")]
    socket: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 列出远端 sessions
    List,
    /// 创建 session
    Create {
        /// shell 路径
        #[arg(long, default_value = "/bin/bash")]
        shell: String,
        /// 工作目录
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// PTY 行数
        #[arg(long, default_value = "40")]
        rows: u16,
        /// PTY 列数
        #[arg(long, default_value = "120")]
        cols: u16,
        /// session 名称
        #[arg(long)]
        name: Option<String>,
    },
    /// Attach 到 session（拉取增量 + 进入交互模式，Ctrl+C 退出自动 detach）
    Attach {
        /// session id
        session_id: String,
        /// 增量起点 cursor
        #[arg(long, default_value = "0")]
        since: u64,
    },
    /// Detach session
    Detach {
        session_id: String,
    },
    /// 发送输入（不进入交互模式）
    Send {
        session_id: String,
        /// 要发送的文本
        data: String,
    },
    /// 读输出（base64 解码后写 stdout）
    Read {
        session_id: String,
        #[arg(long, default_value = "0")]
        since: u64,
    },
    /// 发控制字符（如 C-c）
    Control {
        session_id: String,
        control: String,
    },
    /// 关闭 session
    Close {
        session_id: String,
    },
    /// 关闭 daemon
    Shutdown,
}

#[tokio::main]
async fn main() -> Result<()> {
    // 日志输出到 stderr，不污染 stdout 的 PTY 输出
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.cmd {
        Cmd::List => cmd_list(&cli.socket).await?,
        Cmd::Create {
            shell,
            cwd,
            rows,
            cols,
            name,
        } => {
            cmd_create(&cli.socket, &shell, cwd.as_deref(), rows, cols, name.as_deref())
                .await?
        }
        Cmd::Attach { session_id, since } => {
            cmd_attach(&cli.socket, &session_id, since).await?
        }
        Cmd::Detach { session_id } => cmd_detach(&cli.socket, &session_id).await?,
        Cmd::Send { session_id, data } => cmd_send(&cli.socket, &session_id, &data).await?,
        Cmd::Read { session_id, since } => cmd_read(&cli.socket, &session_id, since).await?,
        Cmd::Control {
            session_id,
            control,
        } => cmd_control(&cli.socket, &session_id, &control).await?,
        Cmd::Close { session_id } => cmd_close(&cli.socket, &session_id).await?,
        Cmd::Shutdown => cmd_shutdown(&cli.socket).await?,
    }

    Ok(())
}

/// 列出所有 session，表格打印
async fn cmd_list(socket: &Path) -> Result<()> {
    let mut client = DaemonClient::connect(socket).await?;
    let sessions = client.list_sessions().await?;
    if sessions.is_empty() {
        println!("（无 session）");
        return Ok(());
    }
    println!(
        "{:<20} {:<15} {:<10} {:>10}  {}",
        "ID", "NAME", "STATE", "WRITTEN", "CREATED_AT"
    );
    println!("{:-<90}", "");
    for s in &sessions {
        println!(
            "{:<20} {:<15} {:<10} {:>10}  {}",
            s.id,
            s.name.as_deref().unwrap_or("-"),
            s.state,
            s.written,
            s.created_at
        );
    }
    Ok(())
}

/// 创建 session，打印 session_id
async fn cmd_create(
    socket: &Path,
    shell: &str,
    cwd: Option<&Path>,
    rows: u16,
    cols: u16,
    name: Option<&str>,
) -> Result<()> {
    let mut client = DaemonClient::connect(socket).await?;
    let result = client
        .create_session(
            shell,
            cwd.and_then(|p| p.to_str()),
            PtySize::new(rows, cols),
            name,
        )
        .await?;
    println!("{}", result.session_id);
    tracing::info!(
        session_id = %result.session_id,
        written = result.written,
        "session 已创建"
    );
    Ok(())
}

/// Attach session：拉取增量输出 + 进入交互模式
async fn cmd_attach(socket: &Path, session_id: &str, since: u64) -> Result<()> {
    let mut client = DaemonClient::connect(socket).await?;
    let initial = client.attach_session(session_id, since).await?;

    if initial.is_truncated {
        eprintln!(
            "[警告] 输出已被截断，cursor {} 之前的数据已丢失",
            initial.cursor_start
        );
    }

    // 写初始增量到 stdout
    let mut stdout = tokio::io::stdout();
    stdout.write_all(&initial.data).await?;
    stdout.flush().await?;

    tracing::info!(
        session_id,
        cursor_end = initial.cursor_end,
        "已 attach，进入交互模式（Ctrl+C 退出并 detach）"
    );

    // 取出 stream 拆为读写两半，手动管理双向流
    let (stream, mut next_id) = client.into_parts();
    let (read_half, mut write_half) = stream.into_split();
    let session_id_owned = session_id.to_string();

    // 后台 task：读 daemon 推送的事件，pty_data 解码写 stdout，session_lost/pty_exit 退出
    let reader = tokio::spawn(async move {
        let mut reader = read_half;
        let mut stdout = tokio::io::stdout();
        loop {
            match read_msg(&mut reader).await {
                Ok(value) => {
                    // 事件有 "event" 字段，响应有 "ok" 字段（send_input 的响应丢弃）
                    if let Some(event) = value.get("event").and_then(|v| v.as_str()) {
                        match event {
                            events::PTY_DATA => {
                                if let Some(data) =
                                    value.get("data").and_then(|v| v.as_str())
                                {
                                    if let Ok(bytes) = B64.decode(data) {
                                        let _ = stdout.write_all(&bytes).await;
                                        let _ = stdout.flush().await;
                                    }
                                }
                            }
                            events::PTY_EXIT => {
                                let code =
                                    value.get("exit_code").and_then(|v| v.as_i64());
                                eprintln!("\n[pty_exit] exit_code={code:?}");
                                break;
                            }
                            events::SESSION_LOST => {
                                let reason =
                                    value.get("reason").and_then(|v| v.as_str());
                                eprintln!("\n[session_lost] reason={reason:?}");
                                break;
                            }
                            _ => {
                                tracing::debug!(event, "未知事件类型");
                            }
                        }
                    } else if value.get("ok").is_some() {
                        tracing::trace!("收到响应，丢弃");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "读取 daemon 事件流结束");
                    break;
                }
            }
        }
    });

    // 主循环：stdin → daemon（send_input），Ctrl+C → detach 退出
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 4096];
    let mut detached = false;

    loop {
        tokio::select! {
            n = stdin.read(&mut buf) => {
                match n {
                    Ok(0) => {
                        eprintln!("[stdin EOF]");
                        break;
                    }
                    Ok(n) => {
                        let b64 = B64.encode(&buf[..n]);
                        let id = next_id;
                        next_id += 1;
                        let req = Request {
                            id,
                            method: methods::SESSION_SEND_INPUT.into(),
                            params: serde_json::json!({
                                "session_id": session_id_owned,
                                "data": b64,
                            }),
                        };
                        if write_msg(&mut write_half, &req).await.is_err() {
                            eprintln!("[连接断开]");
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("[stdin 读错误] {e}");
                        break;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\n[Ctrl+C] detach session");
                let id = next_id;
                next_id += 1;
                let req = Request {
                    id,
                    method: methods::SESSION_DETACH.into(),
                    params: serde_json::json!({ "session_id": session_id_owned }),
                };
                let _ = write_msg(&mut write_half, &req).await;
                detached = true;
                break;
            }
        }
    }

    reader.abort();
    if !detached {
        eprintln!("[提示] 非 Ctrl+C 退出，session 仍在 daemon 端运行");
    }
    Ok(())
}

/// Detach session
async fn cmd_detach(socket: &Path, session_id: &str) -> Result<()> {
    let mut client = DaemonClient::connect(socket).await?;
    client.detach_session(session_id).await?;
    println!("detached: {session_id}");
    Ok(())
}

/// 发送输入（不进入交互模式）
async fn cmd_send(socket: &Path, session_id: &str, data: &str) -> Result<()> {
    let mut client = DaemonClient::connect(socket).await?;
    client.send_input(session_id, data.as_bytes()).await?;
    println!("sent {} bytes", data.len());
    Ok(())
}

/// 读输出（base64 解码后写 stdout）
async fn cmd_read(socket: &Path, session_id: &str, since: u64) -> Result<()> {
    let mut client = DaemonClient::connect(socket).await?;
    let result = client.read_output(session_id, since).await?;
    if result.is_truncated {
        eprintln!(
            "[警告] 输出已被截断，cursor {} 之前的数据已丢失",
            result.cursor_start
        );
    }
    let mut stdout = tokio::io::stdout();
    stdout.write_all(&result.data).await?;
    stdout.flush().await?;
    tracing::info!(
        cursor_start = result.cursor_start,
        cursor_end = result.cursor_end,
        bytes = result.data.len(),
        "read_output 完成"
    );
    Ok(())
}

/// 发控制字符
async fn cmd_control(socket: &Path, session_id: &str, control: &str) -> Result<()> {
    let mut client = DaemonClient::connect(socket).await?;
    client.send_control(session_id, control).await?;
    println!("control sent: {control}");
    Ok(())
}

/// 关闭 session
async fn cmd_close(socket: &Path, session_id: &str) -> Result<()> {
    let mut client = DaemonClient::connect(socket).await?;
    client.close_session(session_id).await?;
    println!("closed: {session_id}");
    Ok(())
}

/// 关闭 daemon
async fn cmd_shutdown(socket: &Path) -> Result<()> {
    let mut client = DaemonClient::connect(socket).await?;
    client.shutdown_daemon().await?;
    println!("daemon shutdown");
    Ok(())
}
