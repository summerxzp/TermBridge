//! MCP transport（rmcp server，stdio only）。
//!
//! Phase 0-C：6 个工具（list_hosts / open_session / send_input / read_output /
//! send_control / close_session），映射到 application 层的 HostManager + SessionManager。

pub mod server;
