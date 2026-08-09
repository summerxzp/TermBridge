//! termbridge-agentd：远端 daemon 入口（ADR-0004 §3）
//!
//! 三模式 CLI：
//! - bootstrap：fork daemon，stdio 返回握手信息（SSH channel 启动用）
//! - proxy：stdio ↔ socket 字节流透传（client 经 SSH channel 接入 daemon）
//! - serve：直接跑 daemon（前台，调试用）

mod buffer;
mod daemonize;
mod protocol;
mod pty;
mod rpc;
mod session;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{info, warn};

use crate::daemonize::{daemonize, DaemonizeResult};
use crate::protocol::{BUILD_VERSION, PROTOCOL_VERSION};
use crate::rpc::{gen_daemon_id, RpcServer};
use crate::session::SessionManager;

// ───────────────────────────────────────────────────────────────────────────
// CLI
// ───────────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "termbridge-agentd", about = "TermBridge 远端 daemon")]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand)]
enum Mode {
    /// Bootstrap 模式：fork daemon，stdio 返回握手信息
    Bootstrap {
        /// Unix socket 路径（默认 $XDG_RUNTIME_DIR/termbridge.sock 或 ~/.local/share/termbridge/termbridge.sock）
        #[arg(long)]
        sock: Option<PathBuf>,
    },
    /// Proxy 模式：stdio ↔ socket 字节流透传
    Proxy {
        #[arg(long)]
        sock: Option<PathBuf>,
    },
    /// Serve 模式：直接跑 daemon（前台，调试用）
    Serve {
        #[arg(long)]
        sock: Option<PathBuf>,
    },
}

// ───────────────────────────────────────────────────────────────────────────
// main
// ───────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.mode {
        Mode::Bootstrap { sock } => bootstrap(sock).await,
        Mode::Proxy { sock } => proxy(sock).await,
        Mode::Serve { sock } => serve(sock).await,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// bootstrap
// ───────────────────────────────────────────────────────────────────────────

/// bootstrap 模式：检查 daemon 是否已运行 → 否则 fork + serve
async fn bootstrap(sock: Option<PathBuf>) -> Result<()> {
    let socket_path = sock.unwrap_or_else(default_socket_path);
    let pid_path = default_pid_path();

    // 确保 runtime 目录存在
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // 检查 socket 是否已活跃（connect 试一下）
    if tokio::net::UnixStream::connect(&socket_path).await.is_ok() {
        // daemon 已运行，直接返回现有信息
        // MVP：不通过 hello 获取 daemon_id（避免开 RPC 会话），返回 "existing"
        let resp = serde_json::json!({
            "daemon_id": "existing",
            "socket": socket_path,
            "protocol_version": PROTOCOL_VERSION,
            "build": BUILD_VERSION,
        });
        println!("{}", resp);
        return Ok(());
    }

    // daemon 未运行，fork + setsid
    let daemon_id = gen_daemon_id();
    match daemonize(&pid_path)? {
        DaemonizeResult::Parent { child_pid: _ } => {
            // 父进程：写 stdout 握手响应后 exit（SSH channel 关闭）
            let resp = serde_json::json!({
                "daemon_id": daemon_id,
                "socket": socket_path,
                "protocol_version": PROTOCOL_VERSION,
                "build": BUILD_VERSION,
            });
            println!("{}", resp);
            std::process::exit(0);
        }
        DaemonizeResult::Child => {
            // 子进程：继续 serve（已 setsid + 写 pid 文件）
            serve_inner(socket_path, daemon_id).await
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// proxy
// ───────────────────────────────────────────────────────────────────────────

/// proxy 模式：stdio ↔ socket 双向字节流透传
async fn proxy(sock: Option<PathBuf>) -> Result<()> {
    let socket_path = sock.unwrap_or_else(default_socket_path);
    let mut socket = tokio::net::UnixStream::connect(&socket_path)
        .await
        .with_context(|| format!("connect socket 失败: {:?}", socket_path))?;

    // split socket + stdin/stdout
    let (mut socket_read, mut socket_write) = socket.split();
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (mut stdin, mut stdout) = (stdin, stdout);

    // stdin → socket_write（client → daemon）
    let to_socket = async {
        tokio::io::copy(&mut stdin, &mut socket_write).await
    };
    // socket_read → stdout（daemon → client）
    let to_stdout = async {
        tokio::io::copy(&mut socket_read, &mut stdout).await
    };

    // 任一端 EOF 退出
    tokio::select! {
        res = to_socket => {
            if let Err(e) = res {
                warn!("stdin → socket 透传错误: {}", e);
            }
        }
        res = to_stdout => {
            if let Err(e) = res {
                warn!("socket → stdout 透传错误: {}", e);
            }
        }
    }
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// serve
// ───────────────────────────────────────────────────────────────────────────

/// serve 模式：前台直接跑 daemon（调试用，不 fork）
async fn serve(sock: Option<PathBuf>) -> Result<()> {
    let socket_path = sock.unwrap_or_else(default_socket_path);
    // 确保 runtime 目录存在
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let session_mgr = Arc::new(SessionManager::new());
    let server = RpcServer::new(session_mgr);
    info!(?socket_path, "serve 模式启动（前台）");
    server.serve(socket_path).await
}

/// 内部 serve 逻辑（bootstrap 子进程用）
async fn serve_inner(socket_path: PathBuf, daemon_id: String) -> Result<()> {
    let session_mgr = Arc::new(SessionManager::new());
    let server = RpcServer::new_with_id(session_mgr, daemon_id);
    server.serve(socket_path).await
}

// ───────────────────────────────────────────────────────────────────────────
// 路径解析
// ───────────────────────────────────────────────────────────────────────────

/// 默认 socket 路径：优先 $XDG_RUNTIME_DIR/termbridge.sock，回退 ~/.local/share/termbridge/termbridge.sock
fn default_socket_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(xdg).join("termbridge.sock");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local/share/termbridge/termbridge.sock")
}

/// 默认 PID 文件路径：~/.local/share/termbridge/agentd.pid
fn default_pid_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local/share/termbridge/agentd.pid")
}
