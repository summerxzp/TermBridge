//! ControlHandler trait —— Control IPC 的业务逻辑接口（ADR-0018）。
//!
//! MCP Server 实现此 trait，委托给 SessionManager。
//! ControlServer 通过此 trait 调用业务逻辑，与传输层解耦。

pub use super::proto::{ControlError, SessionControlInfo};

/// Control IPC 业务逻辑接口。
///
/// 实现方（SessionManager 的包装）提供：
/// - session 查询（list / get）
/// - approval_mode 设置（人类授权操作）
pub trait ControlHandler: Send + Sync {
    /// 列出所有 session 的控制信息。
    fn list_sessions(&self) -> Vec<SessionControlInfo>;

    /// 获取单个 session 的控制信息。
    fn get_session(&self, session_id: &str) -> Option<SessionControlInfo>;

    /// 设置 session 的审批模式。
    fn set_approval_mode(
        &self,
        session_id: &str,
        mode: &str,
    ) -> Result<(), ControlError>;
}
