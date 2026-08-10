//! rmcp MCP server —— 17 工具映射到 application 层（§6 / §7.4 Phase 1-2 / Phase 3-B / Phase 4-A / Phase 5-A-B）
//!
//! 工具：
//! 1. `list_hosts`            → HostManager::list_hosts
//! 2. `open_session`          → SessionManager::open_session
//! 3. `send_input`            → SessionManager::send_input
//! 4. `read_output`           → SessionManager::read_output
//! 5. `send_control`          → SessionManager::send_control
//! 6. `close_session`         → SessionManager::close_session
//! 7. `sftp_transfer`         → SessionManager::sftp_transfer（Phase 1，upload/download）
//! 8. `sftp_mkdir`            → SessionManager::sftp_mkdir（Phase 2）
//! 9. `sftp_list`             → SessionManager::sftp_list（Phase 2）
//! 10. `sftp_remove`          → SessionManager::sftp_remove（Phase 2）
//! 11. `sftp_chmod`           → SessionManager::sftp_chmod（Phase 2）
//! 12. `list_remote_sessions` → SessionManager::list_remote_sessions（Phase 3-B）
//! 13. `attach_remote_session` → SessionManager::attach_remote_session（Phase 3-B）
//! 14. `detach_session`       → SessionManager::detach_session（Phase 3-B）
//! 15. `get_session_timeline` → SessionManager::get_session_timeline（Phase 4-A）
//! 16. `sftp_transfer_dir`    → SessionManager::sftp_transfer_dir（Phase 5-A，目录递归）
//! 17. `detect_remote_env`    → SessionManager::detect_remote_env（Phase 5-B，远端环境检测）
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
use crate::application::sessions::{RemoteEnvInfo, SessionManager};
use crate::domain::output::ReadOutputParams;
use crate::domain::provider::{ControlKey, PtySize, TermError, TransferDirection};
use crate::domain::timeline::TimelineEvent;

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

/// 解析八进制权限模式字符串（Phase 2）。
///
/// 接受 "755"、"0o755"、"0" 等形式；None / 空串 → 0（服务器默认）。
/// 返回 `Ok(u32)`（如 0o755 = 493）或 `Err(错误描述)`。
fn parse_octal_mode(mode: Option<&str>) -> Result<u32, String> {
    let raw = match mode {
        None | Some("") => return Ok(0),
        Some(s) => s.trim(),
    };
    let stripped = raw.strip_prefix("0o").or_else(|| raw.strip_prefix("0O")).unwrap_or(raw);
    u32::from_str_radix(stripped, 8)
        .map_err(|_| format!("invalid octal mode '{raw}'; expected e.g. '755' or '0o755'"))
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
    /// Use persistent daemon session (ADR-0004). Default false (interactive session).
    /// When true, deploys and connects to remote termbridge-agentd daemon for cross-restart persistence.
    #[serde(default)]
    pub persistent: Option<bool>,
    /// Optional session name (only used when persistent=true, shown in list_remote_sessions)
    #[serde(default)]
    pub name: Option<String>,
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

/// sftp_mkdir 参数（Phase 2）
#[derive(Deserialize, schemars::JsonSchema)]
pub struct SftpMkdirParams {
    /// Session ID returned by open_session
    pub session_id: String,
    /// Remote directory path to create. Parent must exist. Path policy enforced.
    pub remote_path: String,
    /// POSIX permissions as octal string, e.g. "755" or "0o755". Use "0" for server default.
    pub mode: Option<String>,
}

/// sftp_list 参数（Phase 2）
#[derive(Deserialize, schemars::JsonSchema)]
pub struct SftpListParams {
    /// Session ID returned by open_session
    pub session_id: String,
    /// Remote directory path to list. Must exist. Path policy enforced.
    pub remote_path: String,
}

/// sftp_remove 参数（Phase 2）
#[derive(Deserialize, schemars::JsonSchema)]
pub struct SftpRemoveParams {
    /// Session ID returned by open_session
    pub session_id: String,
    /// Remote file or directory path to delete. Path policy enforced.
    pub remote_path: String,
    /// If true, recursively delete directory tree. Policy: recursive delete of system dirs is denied.
    pub recursive: Option<bool>,
}

/// sftp_chmod 参数（Phase 2）
#[derive(Deserialize, schemars::JsonSchema)]
pub struct SftpChmodParams {
    /// Session ID returned by open_session
    pub session_id: String,
    /// Remote file or directory path. Must exist. Path policy enforced.
    pub remote_path: String,
    /// POSIX permissions as octal string, e.g. "755" or "644".
    pub mode: String,
}

/// list_remote_sessions 参数（Phase 3-B）
#[derive(Deserialize, schemars::JsonSchema)]
pub struct ListRemoteSessionsParams {
    /// SSH host alias (from ~/.ssh/config) or direct hostname/IP
    pub host: String,
}

/// attach_remote_session 参数（Phase 3-B）
#[derive(Deserialize, schemars::JsonSchema)]
pub struct AttachRemoteSessionParams {
    /// SSH host alias (from ~/.ssh/config) or direct hostname/IP
    pub host: String,
    /// Remote session ID (from list_remote_sessions) to attach to
    pub remote_session_id: String,
    /// Optional local label for the session (not stored on remote)
    pub name: Option<String>,
}

/// detach_session 参数（Phase 3-B）
#[derive(Deserialize, schemars::JsonSchema)]
pub struct DetachSessionParams {
    /// Session ID returned by open_session or attach_remote_session
    pub session_id: String,
}

/// get_session_timeline 参数（Phase 4-A）
#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetSessionTimelineParams {
    /// Session ID returned by open_session
    pub session_id: String,
    /// Return only the most recent N events. Omit to return all (up to ring capacity, default 1000).
    pub limit: Option<usize>,
}

/// sftp_transfer_dir 参数（Phase 5-A）
#[derive(Deserialize, schemars::JsonSchema)]
pub struct SftpTransferDirParams {
    /// Session ID returned by open_session
    pub session_id: String,
    /// Transfer direction: "upload" (local→remote) or "download" (remote→local)
    pub direction: String,
    /// Local directory path. For upload: source (must exist). For download: destination (will be created).
    pub local_path: String,
    /// Remote directory path. Path policy enforced.
    pub remote_path: String,
}

/// detect_remote_env 参数（Phase 5-B）
#[derive(Deserialize, schemars::JsonSchema)]
pub struct DetectRemoteEnvParams {
    /// Session ID returned by open_session
    pub session_id: String,
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

/// sftp_list 单条目录条目（Phase 2）
#[derive(Serialize)]
struct RemoteEntryDto {
    name: String,
    is_dir: bool,
    is_file: bool,
    size: u64,
    permissions: Option<u32>,
}

/// sftp_list 成功返回（Phase 2）
#[derive(Serialize)]
struct SftpListResult {
    path: String,
    entries: Vec<RemoteEntryDto>,
}

/// sftp_mkdir / sftp_remove / sftp_chmod 成功返回（Phase 2）
#[derive(Serialize)]
struct SftpOkResult {
    ok: bool,
    remote_path: String,
}

/// attach_remote_session 成功返回（Phase 3-B）
#[derive(Serialize)]
struct AttachRemoteSessionResult {
    session_id: String,
}

/// get_session_timeline 成功返回（Phase 4-A）
#[derive(Serialize)]
struct GetSessionTimelineResult {
    events: Vec<TimelineEvent>,
}

/// sftp_transfer_dir 成功返回（Phase 5-A）
#[derive(Serialize)]
struct SftpTransferDirResult {
    direction: String,
    local_path: String,
    remote_path: String,
    files_transferred: usize,
}

/// detect_remote_env 成功返回（Phase 5-B）
#[derive(Serialize)]
struct DetectRemoteEnvResult {
    env: RemoteEnvInfo,
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
    #[tool(description = "Open a new terminal session to an SSH host. Resolves host alias via `ssh -G`, connects via SSH, opens PTY + shell. Set persistent=true to use remote daemon for cross-restart session persistence (deploys termbridge-agentd if needed). Returns session_id for subsequent operations.")]
    async fn open_session(
        &self,
        Parameters(params): Parameters<OpenSessionParams>,
    ) -> CallToolResult {
        let pty_size = match (params.rows, params.cols) {
            (Some(r), Some(c)) => Some(PtySize { rows: r, cols: c }),
            _ => None,
        };
        let persistent = params.persistent.unwrap_or(false);
        match self
            .session_manager
            .open_session(&params.host, pty_size, persistent, params.name)
            .await
        {
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

    /// Create a remote directory via SFTP (Phase 2).
    #[tool(description = "Create a remote directory via SFTP. Parent directory must exist. Path policy enforced. Mode is octal string like '755'; use '0' or omit for server default.")]
    async fn sftp_mkdir(
        &self,
        Parameters(params): Parameters<SftpMkdirParams>,
    ) -> CallToolResult {
        let mode = match parse_octal_mode(params.mode.as_deref()) {
            Ok(m) => m,
            Err(msg) => {
                return CallToolResult::structured_error(json!(ToolError {
                    code: "INVALID_ARGUMENT".to_string(),
                    message: msg,
                    retriable: false,
                }));
            }
        };
        match self
            .session_manager
            .sftp_mkdir(&params.session_id, params.remote_path.clone(), mode)
            .await
        {
            Ok(()) => ok_result(SftpOkResult {
                ok: true,
                remote_path: params.remote_path,
            }),
            Err(e) => err_result(&e),
        }
    }

    /// List remote directory contents via SFTP (Phase 2).
    #[tool(description = "List remote directory contents via SFTP. Returns entry names, types (file/dir), sizes, and permissions. Path must exist. Path policy enforced.")]
    async fn sftp_list(
        &self,
        Parameters(params): Parameters<SftpListParams>,
    ) -> CallToolResult {
        match self
            .session_manager
            .sftp_list(&params.session_id, params.remote_path.clone())
            .await
        {
            Ok(entries) => {
                let dtos: Vec<RemoteEntryDto> = entries
                    .into_iter()
                    .map(|e| RemoteEntryDto {
                        name: e.name,
                        is_dir: e.is_dir,
                        is_file: e.is_file,
                        size: e.size,
                        permissions: e.permissions,
                    })
                    .collect();
                ok_result(SftpListResult {
                    path: params.remote_path,
                    entries: dtos,
                })
            }
            Err(e) => err_result(&e),
        }
    }

    /// Delete a remote file or directory via SFTP (Phase 2).
    #[tool(description = "Delete a remote file or directory via SFTP. Set recursive=true to delete a directory tree. Policy: recursive delete of system directories (/etc, /usr, etc.) is denied; other deletes need confirmation.")]
    async fn sftp_remove(
        &self,
        Parameters(params): Parameters<SftpRemoveParams>,
    ) -> CallToolResult {
        let recursive = params.recursive.unwrap_or(false);
        match self
            .session_manager
            .sftp_remove(&params.session_id, params.remote_path.clone(), recursive)
            .await
        {
            Ok(()) => ok_result(SftpOkResult {
                ok: true,
                remote_path: params.remote_path,
            }),
            Err(e) => err_result(&e),
        }
    }

    /// Change remote file/directory permissions via SFTP (Phase 2).
    #[tool(description = "Change remote file/directory permissions via SFTP (chmod). Mode is octal string like '755' or '644'. Path must exist. Policy: chmod 777 on system directories needs confirmation.")]
    async fn sftp_chmod(
        &self,
        Parameters(params): Parameters<SftpChmodParams>,
    ) -> CallToolResult {
        let mode = match parse_octal_mode(Some(&params.mode)) {
            Ok(m) => m,
            Err(msg) => {
                return CallToolResult::structured_error(json!(ToolError {
                    code: "INVALID_ARGUMENT".to_string(),
                    message: msg,
                    retriable: false,
                }));
            }
        };
        match self
            .session_manager
            .sftp_chmod(&params.session_id, params.remote_path.clone(), mode)
            .await
        {
            Ok(()) => ok_result(SftpOkResult {
                ok: true,
                remote_path: params.remote_path,
            }),
            Err(e) => err_result(&e),
        }
    }

    /// List remote daemon sessions (Phase 3-B).
    #[tool(description = "List all sessions on the remote daemon (including detached ones). Used to discover persistent sessions across MCP restarts. Requires persistent provider.")]
    async fn list_remote_sessions(
        &self,
        Parameters(params): Parameters<ListRemoteSessionsParams>,
    ) -> CallToolResult {
        match self
            .session_manager
            .list_remote_sessions(&params.host)
            .await
        {
            Ok(sessions) => ok_result(json!({ "sessions": sessions })),
            Err(e) => err_result(&e),
        }
    }

    /// Attach to a remote session (Phase 3-B).
    #[tool(description = "Attach to an existing remote daemon session (for cross-restart reconnection). The remote session must have been created by a previous open_session with persistent=true. Returns a new local session_id.")]
    async fn attach_remote_session(
        &self,
        Parameters(params): Parameters<AttachRemoteSessionParams>,
    ) -> CallToolResult {
        match self
            .session_manager
            .attach_remote_session(
                &params.host,
                &params.remote_session_id,
                params.name,
            )
            .await
        {
            Ok(session_id) => ok_result(AttachRemoteSessionResult { session_id }),
            Err(e) => err_result(&e),
        }
    }

    /// Detach a session (Phase 3-B).
    #[tool(description = "Detach a persistent session: keeps the remote PTY alive but releases the local connection. The session can be reconnected later via attach_remote_session. Only persistent sessions support detach.")]
    async fn detach_session(
        &self,
        Parameters(params): Parameters<DetachSessionParams>,
    ) -> CallToolResult {
        match self
            .session_manager
            .detach_session(&params.session_id)
            .await
        {
            Ok(()) => ok_result(OkResult { ok: true }),
            Err(e) => err_result(&e),
        }
    }

    /// Get session execution timeline (Phase 4-A).
    #[tool(description = "Get session execution timeline: ordered list of command/output/control/state events with timestamps and cursor metadata. Used for debugging (what was sent, what came back) and AI context. Output content stays in RingBuffer; timeline only records byte ranges.")]
    async fn get_session_timeline(
        &self,
        Parameters(params): Parameters<GetSessionTimelineParams>,
    ) -> CallToolResult {
        match self
            .session_manager
            .get_session_timeline(&params.session_id, params.limit)
        {
            Ok(events) => ok_result(GetSessionTimelineResult { events }),
            Err(e) => err_result(&e),
        }
    }

    /// Transfer a directory recursively via SFTP (Phase 5-A).
    #[tool(description = "Transfer a directory recursively between local and remote via SFTP. Supports upload (local->remote) and download (remote->local). Creates target directories automatically. Symlinks are skipped. Returns files_transferred count. Path policy enforced.")]
    async fn sftp_transfer_dir(
        &self,
        Parameters(params): Parameters<SftpTransferDirParams>,
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
            .sftp_transfer_dir(
                &params.session_id,
                direction,
                std::path::PathBuf::from(&params.local_path),
                params.remote_path.clone(),
            )
            .await
        {
            Ok(count) => ok_result(SftpTransferDirResult {
                direction: params.direction,
                local_path: params.local_path,
                remote_path: params.remote_path,
                files_transferred: count,
            }),
            Err(e) => err_result(&e),
        }
    }

    /// Detect remote environment (Phase 5-B).
    #[tool(description = "Detect remote environment: OS (uname -a), default shell ($SHELL), PATH, and installed tools (python, node, rustc, go, docker, git, etc.). Uses SSH exec (not PTY) to avoid polluting session output.")]
    async fn detect_remote_env(
        &self,
        Parameters(params): Parameters<DetectRemoteEnvParams>,
    ) -> CallToolResult {
        match self
            .session_manager
            .detect_remote_env(&params.session_id)
            .await
        {
            Ok(env) => ok_result(DetectRemoteEnvResult { env }),
            Err(e) => err_result(&e),
        }
    }
}

#[tool_handler]
impl ServerHandler for TermBridgeServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("TermBridge: Remote terminal bridge for AI agents. Open a session to an SSH host, send commands, read output, send control characters (Ctrl+C etc.), transfer files via SFTP (upload/download), create/list/delete remote directories and files, change permissions (chmod), and close when done.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_octal_mode_plain() {
        assert_eq!(parse_octal_mode(Some("755")).unwrap(), 0o755);
        assert_eq!(parse_octal_mode(Some("644")).unwrap(), 0o644);
        assert_eq!(parse_octal_mode(Some("777")).unwrap(), 0o777);
    }

    #[test]
    fn parse_octal_mode_with_0o_prefix() {
        assert_eq!(parse_octal_mode(Some("0o755")).unwrap(), 0o755);
        assert_eq!(parse_octal_mode(Some("0O644")).unwrap(), 0o644);
    }

    #[test]
    fn parse_octal_mode_none_or_empty_returns_zero() {
        assert_eq!(parse_octal_mode(None).unwrap(), 0);
        assert_eq!(parse_octal_mode(Some("")).unwrap(), 0);
    }

    #[test]
    fn parse_octal_mode_whitespace_trimmed() {
        assert_eq!(parse_octal_mode(Some("  755  ")).unwrap(), 0o755);
    }

    #[test]
    fn parse_octal_mode_invalid_returns_error() {
        assert!(parse_octal_mode(Some("abc")).is_err());
        assert!(parse_octal_mode(Some("8")).is_err()); // 8 is not valid octal
        assert!(parse_octal_mode(Some("rwxr-xr-x")).is_err());
    }
}
