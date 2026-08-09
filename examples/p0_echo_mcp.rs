// Phase 0-A 验证 1：rmcp 最小 stdio MCP server
//
// 目标：验证 rmcp 3.1.2 在 Windows 上能跑通 stdio transport + #[tool] 宏 + JSON Schema。
//
// 运行（MCP inspector 或直接对接 Agent）：
//   cargo run --example p0_echo_mcp
//
// 验证点：
//   1. 编译通过（rmcp + macros + transport-io + schemars features 正常）
//   2. stdio 启动后能响应 initialize / tools/list / tools/call
//   3. echo 工具能正确返回输入

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;

#[derive(Clone)]
struct EchoServer;

/// echo 工具的输入参数。
#[derive(Deserialize, schemars::JsonSchema)]
struct EchoParams {
    /// 要回显的文本
    text: String,
}

#[tool_router]
impl EchoServer {
    /// 原样返回输入文本，用于验证 MCP tool 调用链路。
    #[tool(description = "Echo back the input text")]
    fn echo(&self, Parameters(EchoParams { text }): Parameters<EchoParams>) -> String {
        format!("echo: {text}")
    }
}

#[tool_handler]
impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("TermBridge Phase 0-A echo server")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("p0_echo_mcp: starting stdio MCP server");

    let service = EchoServer.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
