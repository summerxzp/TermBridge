//! Local Control Plane —— 人类授权/管理面 IPC（ADR-0018）。
//!
//! 与 MCP stdio（Agent 数据面）互补：
//! - MCP stdio = Agent plane（open_session / send_input / read_output / sftp_*）
//! - Control IPC = Human plane（session.list / session.get / session.set_approval_mode）
//!
//! 传输：
//! - Linux/macOS: Unix Domain Socket（$XDG_RUNTIME_DIR/termbridge/mcp-<instance>.sock, 0600）
//! - Windows: Named Pipe（\\.\pipe\termbridge-mcp-<instance>）
//!
//! 协议：简化 JSON-RPC（newline-delimited JSON）
//! - 请求：{"id": <u64>, "method": "<string>", "params": {...}}
//! - 成功响应：{"id": <u64>, "ok": true, "result": ...}
//! - 错误响应：{"id": <u64>, "ok": false, "error": {"code": "<string>", "message": "<string>"}}
//!
//! 安全：
//! - 仅本机（Unix socket / Named Pipe 不暴露网络端口）
//! - owner-only 权限（Unix 0600 / Named Pipe 当前用户 SID）
//! - instance token：CLI 连接时需提供 HELLO token 认证
//!
//! 第一版方法：
//! - session.list → 返回所有 session 的 ControlInfo
//! - session.get → 返回单个 session 的 ControlInfo
//! - session.set_approval_mode → 设置 approval_mode（standard / unrestricted）

pub mod handler;
pub mod proto;
pub mod server;
pub mod instance;

pub use handler::{ControlHandler, SessionControlInfo};
pub use proto::{ControlRequest, ControlResponse, ControlError};
pub use server::ControlServer;
pub use instance::{InstanceInfo, InstanceRegistry};
