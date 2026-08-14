//! TermBridge transport 层：MCP 协议适配。
//!
//! Phase 0-C：`mcp`（rmcp server + 6 工具，stdio only）。
//! 未来可扩展 WebSocket / HTTP transport。

pub mod mcp;
pub mod control;
