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
use termbridge::application::host_policy::HostPolicyResolver;
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

    // ADR-0009：CredentialProvider —— 优先 HelperCredentialProvider（spawn 独立 helper
    // process 弹 Windows native dialog），找不到 helper exe 时 fallback NoopCredentialProvider
    //（bootstrap_host / auth=password 的 open_session 调用时返回 Unsupported 错误）
    let credential_provider: Arc<dyn CredentialProvider> =
        match HelperCredentialProvider::new() {
            Ok(helper) => {
                tracing::info!("credential provider: HelperCredentialProvider (helper resolved)");
                Arc::new(helper)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "credential provider: HelperCredentialProvider unavailable, falling back to Noop (bootstrap_host / auth=password will fail with Unsupported)"
                );
                Arc::new(NoopCredentialProvider)
            }
        };

    // ADR-0017：启动时加载 hosts.toml（不存在 → 空配置，行为与之前完全一致）；
    // CredentialProvider 与 BootstrapHost 共享同一 Arc（ADR-0017 §2.3 password 路径）
    let session_manager = Arc::new(SessionManager::with_host_policy(
        provider,
        HostPolicyResolver::load_default(),
        Arc::clone(&credential_provider),
    ));
    let host_manager = Arc::new(HostManager::new());

    let bootstrap_host = Arc::new(BootstrapHost::new(Arc::clone(&credential_provider)));

    // ADR-0018：clone session_manager 用于 Control IPC（必须在 move 进
    // TermBridgeServer 之前完成 clone——之后 binding 不可用）
    let control_handler: Arc<dyn termbridge::transport::control::ControlHandler> =
        Arc::clone(&session_manager) as Arc<dyn termbridge::transport::control::ControlHandler>;

    let server = TermBridgeServer::new(host_manager, session_manager, bootstrap_host);

    // ADR-0018：启动 Local Control IPC（Human Control Plane）
    // MCP stdio = Agent 数据面；Control IPC = 人类授权/管理面
    // Agent 不可调用 set_approval_mode，仅 CLI/GUI 通过本地 IPC 操作
    let _control_server = match termbridge::transport::control::ControlServer::start(control_handler).await {
        Ok(s) => {
            let info = s.instance_info();
            tracing::info!(
                endpoint = %info.endpoint,
                transport = %info.transport,
                "Control IPC: listening (human control plane, ADR-0018)"
            );
            Some(s)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Control IPC: failed to start (session approve via CLI unavailable); MCP server continues"
            );
            None
        }
    };

    server.serve_stdio().await?;

    tracing::info!("termbridge: server stopped");
    Ok(())
}
