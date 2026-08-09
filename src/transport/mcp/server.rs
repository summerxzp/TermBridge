//! rmcp MCP server —— 7 工具映射到 application 层（§6 / §7.4 Phase 1）
//!
//! 工具：
//! 1. `list_hosts`       → HostManager::list_hosts
//! 2. `open_session`     → SessionManager::open_session
//! 3. `send_input`       → SessionManager::send_input
//! 4. `read_output`      → SessionManager::read_output
//! 5. `send_control`     → SessionManager::send_control
//! 6. `close_session`    → SessionManager::close_session
//! 7. `sftp_transfer`    → SessionManager::sftp_transfer（Phase 1，upload/download only）
//!
//! 错误格式（§6.1）：`CallToolResult::structured_error({code, message, retriable})`
//! 成功格式：`CallToolResult::structured({工具特定结构})`

use std::sync::Arc;

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::application::hosts::HostManager;
use crate::application::sessions::SessionManager;
use crate::domain::output::ReadOutputParams;
use crate::domain::provider::{ControlKey, PtySize, TermError, TransferDirection};

// ───────────────────────────────────────────────────────────────────────────
// ToolError —— §6.1 统一错误格式
// ───────────────────────────────────────────────────────────────────────────

/// 工具错误（§6.1：code / message / retriable，供 Agent 重试逻辑判断）。
#[derive(Debug, Serialize)]
struct ToolError {
    code: String,
    message: String,
    retriable: bool,
}

impl ToolError {
    fn from_term(e: &TermError) -> Self {
        Self {
            code: e.code().to_string(),
            message: e.to_string(),
            retriable: e.retriable(),
        }
    }
}

/// 构建成功结果（structured content）。
fn ok_result<T: Serialize>(result: T) -> CallToolResult {
    CallToolResult::structured(json!(result))
}

/// 构建错误结果（structured error，is_error=true）。
fn err_result(e: &TermError) -> CallToolResult {
    CallToolResult::structured_error(json!(ToolError::from_term(e)))
}

// ───────────────────────────────────────────────────────────────────────────
// 工具参数 / 返回类型
// ───────────────────────────────────────────────────────────────────────────

/// open_session 参数
#[derive(Deserialize, schemars::JsonSchema)]
pub struct OpenSessionParams {
    /// SSH host alias (from ~/.ssh/config) or direct hostname/IP
    pub host: String,
    /// PTY rows (default 24)
    pub rows: Option<u16>,
    /// PTY columns (default 80)
    pub cols: Option<u16>,
}

/// send_input 参数
#[derive(Deserialize, schemars::JsonSchema)]
pub struct SendInputParams {
    /// Session ID returned by open_session
    pub session_id: String,
    /// Text to send to the terminal (append \n for Enter)
    pub data: String,
}

/// read_output 参数
#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReadOutputParamsSchema {
    /// Session ID returned by open_session
    pub session_id: String,
    /// Wait for this regex/substring to appear in output (blocking mode)
    pub wait_for: Option<String>,
    /// Peek last N lines without advancing cursor (tail mode)
    pub tail_lines: Option<usize>,
    /// Read incrementally from this cursor position (since_cursor mode)
    pub since_cursor: Option<u64>,
    /// Timeout in seconds (default 5, max 60)
    pub timeout_secs: Option<u64>,
    /// Max bytes to read in since_cursor mode (default 64KB)
    pub max_bytes: Option<usize>,
    /// Context lines around wait_for match (default 0, max 50)
    pub context_lines: Option<usize>,
}

/// send_control 参数
#[derive(Deserialize, schemars::JsonSchema)]
pub struct SendControlParams {
    /// Session ID returned by open_session
    pub session_id: String,
    /// Control key: "ctrl+c" / "ctrl+d" / "ctrl+z" / "tab" / "enter" / "escape"
    pub control_key: String,
}

/// close_session 参数
#[derive(Deserialize, schemars::JsonSchema)]
pub struct CloseSessionParams {
    /// Session ID returned by open_session
    pub session_id: String,
}

/// sftp_transfer 参数（Phase 1，§7.4）
#[derive(Deserialize, schemars::JsonSchema)]
pub struct SftpTransferParams {
    /// Session ID returned by open_session
    pub session_id: String,
    /// Transfer direction: "upload" (local→remote) or "download" (remote→local)
    pub direction: String,
    /// Local file path. For upload: source (must exist). For download: destination (will be created/overwritten atomically).
    pub local_path: String,
    /// Remote file path. Path policy enforced (realpath resolution; rejects ../ and null bytes).
    pub remote_path: String,
}

// ── 返回类型 ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ListHostsResult {
    hosts: Vec<HostEntryDto>,
}

#[derive(Serialize)]
struct HostEntryDto {
    alias: String,
    hostname: Option<String>,
}

#[derive(Serialize)]
struct OpenSessionResult {
    session_id: String,
}

#[derive(Serialize)]
struct OkResult {
    ok: bool,
}

#[derive(Serialize)]
struct ReadOutputDto {
    output: String,
    cursor: u64,
    has_more: bool,
    is_truncated: bool,
    matched: bool,
    timed_out: bool,
    mode: String,
}

/// sftp_transfer 成功返回（Phase 1）
#[derive(Serialize)]
struct SftpTransferResult {
    direction: String,
    local_path: String,
    remote_path: String,
}

// ───────────────────────────────────────────────────────────────────────────
// TermBridgeServer —— MCP server 实体
// ───────────────────────────────────────────────────────────────────────────

/// TermBridge MCP Server。
///
/// 持有 HostManager + SessionManager（Arc 共享，Clone 供 rmcp serve）。
#[derive(Clone)]
pub struct TermBridgeServer {
    host_manager: Arc<HostManager>,
    session_manager: Arc<SessionManager>,
}

impl TermBridgeServer {
    pub fn new(host_manager: Arc<HostManager>, session_manager: Arc<SessionManager>) -> Self {
        Self { host_manager, session_manager }
    }

    /// 启动 stdio MCP server（阻塞直到连接关闭）。
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        tracing::info!("termbridge: starting stdio MCP server");
        let service = self.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        Ok(())
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 6 个 MCP 工具
// ───────────────────────────────────────────────────────────────────────────

#[tool_router]
impl TermBridgeServer {
    /// List all SSH hosts discovered from ~/.ssh/config.
    #[tool(description = "List all SSH hosts from ~/.ssh/config. Returns host aliases and their configured hostnames.")]
    fn list_hosts(&self) -> CallToolResult {
        let entries = self.host_manager.list_hosts();
        let hosts: Vec<HostEntryDto> = entries
            .into_iter()
            .map(|e| HostEntryDto {
                alias: e.alias,
                hostname: e.hostname,
            })
            .collect();
        ok_result(ListHostsResult { hosts })
    }

    /// Open a new terminal session to an SSH host.
    #[tool(description = "Open a new terminal session to an SSH host. Resolves host alias via `ssh -G`, connects via SSH, opens PTY + shell. Returns session_id for subsequent operations.")]
    async fn open_session(
        &self,
        Parameters(params): Parameters<OpenSessionParams>,
    ) -> CallToolResult {
        let pty_size = match (params.rows, params.cols) {
            (Some(r), Some(c)) => Some(PtySize { rows: r, cols: c }),
            _ => None,
        };
        match self.session_manager.open_session(&params.host, pty_size).await {
            Ok(id) => ok_result(OpenSessionResult { session_id: id }),
            Err(e) => err_result(&e),
        }
    }

    /// Send input text to a terminal session.
    #[tool(description = "Send input text to a terminal session. Appends to PTY stdin immediately without waiting for command completion. Use \\n for Enter.")]
    async fn send_input(
        &self,
        Parameters(params): Parameters<SendInputParams>,
    ) -> CallToolResult {
        match self.session_manager.send_input(&params.session_id, params.data.as_bytes()).await {
            Ok(()) => ok_result(OkResult { ok: true }),
            Err(e) => err_result(&e),
        }
    }

    /// Read output from a terminal session.
    #[tool(description = "Read output from a terminal session. Supports 4 modes: (1) default settle - drain output until stable; (2) wait_for - block until regex/substring appears; (3) tail_lines - peek last N lines; (4) since_cursor - incremental read from cursor. Only one mode active per call.")]
    async fn read_output(
        &self,
        Parameters(params): Parameters<ReadOutputParamsSchema>,
    ) -> CallToolResult {
        let domain_params = ReadOutputParams {
            wait_for: params.wait_for,
            timeout_secs: params.timeout_secs,
            tail_lines: params.tail_lines,
            since_cursor: params.since_cursor,
            max_bytes: params.max_bytes,
            context_lines: params.context_lines,
        };
        match self.session_manager.read_output(&params.session_id, domain_params).await {
            Ok(r) => {
                let mode = match r.mode {
                    crate::domain::output::ReadMode::SinceCursor => "since_cursor",
                    crate::domain::output::ReadMode::Tail => "tail",
                    crate::domain::output::ReadMode::WaitFor => "wait_for",
                    crate::domain::output::ReadMode::Settle => "settle",
                };
                ok_result(ReadOutputDto {
                    output: String::from_utf8_lossy(&r.output).into_owned(),
                    cursor: r.cursor,
                    has_more: r.has_more,
                    is_truncated: r.is_truncated,
                    matched: r.matched,
                    timed_out: r.timed_out,
                    mode: mode.to_string(),
                })
            }
            Err(e) => err_result(&e),
        }
    }

    /// Send a control character to a terminal session.
    #[tool(description = "Send a control character to a terminal session. Supported: ctrl+c, ctrl+d, ctrl+z, tab, enter, escape.")]
    async fn send_control(
        &self,
        Parameters(params): Parameters<SendControlParams>,
    ) -> CallToolResult {
        let key = match ControlKey::from_name(&params.control_key) {
            Some(k) => k,
            None => {
                return CallToolResult::structured_error(json!(ToolError {
                    code: "INVALID_ARGUMENT".to_string(),
                    message: format!(
                        "unknown control_key '{}'; supported: ctrl+c, ctrl+d, ctrl+z, tab, enter, escape",
                        params.control_key
                    ),
                    retriable: false,
                }));
            }
        };
        match self.session_manager.send_control(&params.session_id, key).await {
            Ok(()) => ok_result(OkResult { ok: true }),
            Err(e) => err_result(&e),
        }
    }

    /// Close a terminal session.
    #[tool(description = "Close a terminal session. Sends EOF + disconnect to SSH channel. Session resources are released. Idempotent.")]
    async fn close_session(
        &self,
        Parameters(params): Parameters<CloseSessionParams>,
    ) -> CallToolResult {
        match self.session_manager.close_session(&params.session_id).await {
            Ok(()) => ok_result(OkResult { ok: true }),
            Err(e) => err_result(&e),
        }
    }

    /// Transfer files via SFTP (Phase 1, upload/download only).
    #[tool(description = "Transfer files via SFTP. Supports upload (local->remote) and download (remote->local). Path policy enforced: local paths must be under allowedLocalPaths (default cwd); remote paths resolved via realpath to prevent ../ traversal and symlink escape. Download uses atomic write (temp + fsync + rename).")]
    async fn sftp_transfer(
        &self,
        Parameters(params): Parameters<SftpTransferParams>,
    ) -> CallToolResult {
        let direction = match TransferDirection::from_name(&params.direction) {
            Some(d) => d,
            None => {
                return CallToolResult::structured_error(json!(ToolError {
                    code: "INVALID_ARGUMENT".to_string(),
                    message: format!(
                        "unknown direction '{}'; supported: upload, download",
                        params.direction
                    ),
                    retriable: false,
                }));
            }
        };

        match self
            .session_manager
            .sftp_transfer(
                &params.session_id,
                direction,
                std::path::PathBuf::from(&params.local_path),
                params.remote_path.clone(),
            )
            .await
        {
            Ok(()) => ok_result(SftpTransferResult {
                direction: params.direction,
                local_path: params.local_path,
                remote_path: params.remote_path,
            }),
            Err(e) => err_result(&e),
        }
    }
}

#[tool_handler]
impl ServerHandler for TermBridgeServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("TermBridge: Remote terminal bridge for AI agents. Open a session to an SSH host, send commands, read output, send control characters (Ctrl+C etc.), transfer files via SFTP (upload/download), and close when done.")
    }
}
