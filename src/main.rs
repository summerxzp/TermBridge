// TermBridge — remote terminal bridge for AI agents.
// Phase 0-C：vertical slice（rmcp stdio server + SSH PTY）。
//
// 启动：
//   cargo run                          # 默认 stdio MCP server
//   RUST_LOG=info cargo run            # 带日志（输出到 stderr）
//
// 日志走 stderr（不污染 stdio MCP 通道）。

use std::sync::Arc;

use termbridge::application::hosts::HostManager;
use termbridge::application::sessions::SessionManager;
use termbridge::infrastructure::ssh::SshProvider;
use termbridge::transport::mcp::server::TermBridgeServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // tracing → stderr（stdio 留给 MCP JSON-RPC）
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("termbridge: Phase 0-C vertical slice starting");

    // 组装依赖链：SshProvider → SessionManager, HostManager → Server
    let provider = Arc::new(SshProvider::new()) as Arc<dyn termbridge::domain::provider::TerminalProvider>;
    let session_manager = Arc::new(SessionManager::new(provider));
    let host_manager = Arc::new(HostManager::new());

    let server = TermBridgeServer::new(host_manager, session_manager);
    server.serve_stdio().await?;

    tracing::info!("termbridge: server stopped");
    Ok(())
}
