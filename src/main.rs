// TermBridge — remote terminal bridge for AI agents.
// Phase 0-C：vertical slice（rmcp stdio server + SSH PTY）。
//
// 启动：
//   cargo run                          # 默认 stdio MCP server
//   RUST_LOG=info cargo run            # 带日志（输出到 stderr）
//
// 日志走 stderr（不污染 stdio MCP 通道）。

use std::sync::Arc;

use termbridge::application::bootstrap::BootstrapHost;
use termbridge::application::hosts::HostManager;
use termbridge::application::sessions::SessionManager;
use termbridge::domain::credential::{CredentialProvider, NoopCredentialProvider};
use termbridge::infrastructure::credential::HelperCredentialProvider;
use termbridge::infrastructure::redact::RedactingMakeWriter;
use termbridge::infrastructure::persistent::PersistentProvider;
use termbridge::transport::mcp::server::TermBridgeServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // tracing → stderr（stdio 留给 MCP JSON-RPC），经 RedactingMakeWriter 脱敏（§5.5）
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(RedactingMakeWriter::new())
        .init();

    tracing::info!("termbridge: Phase 3-A starting (persistent provider enabled)");

    // 组装依赖链：PersistentProvider（内化 SshProvider，persistent=false 委托 SSH 直连，
    // persistent=true 走远端 daemon 路径 ADR-0004）→ SessionManager, HostManager → Server
    let provider = Arc::new(PersistentProvider::default())
        as Arc<dyn termbridge::domain::provider::TerminalProvider>;
    let session_manager = Arc::new(SessionManager::new(provider));
    let host_manager = Arc::new(HostManager::new());

    // ADR-0009：CredentialProvider —— 优先 HelperCredentialProvider（spawn 独立 helper
    // process 弹 Windows native dialog），找不到 helper exe 时 fallback NoopCredentialProvider
    //（bootstrap_host 调用时返回 Unsupported 错误）
    let credential_provider: Arc<dyn CredentialProvider> =
        match HelperCredentialProvider::new() {
            Ok(helper) => {
                tracing::info!("credential provider: HelperCredentialProvider (helper resolved)");
                Arc::new(helper)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "credential provider: HelperCredentialProvider unavailable, falling back to Noop (bootstrap_host will return Unsupported)"
                );
                Arc::new(NoopCredentialProvider)
            }
        };
    let bootstrap_host = Arc::new(BootstrapHost::new(credential_provider));

    let server = TermBridgeServer::new(host_manager, session_manager, bootstrap_host);
    server.serve_stdio().await?;

    tracing::info!("termbridge: server stopped");
    Ok(())
}
