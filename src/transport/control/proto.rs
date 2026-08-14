//! Control IPC 协议数据结构（ADR-0018）。
//!
//! 简化 JSON-RPC over newline-delimited JSON。
//! 不复用 rmcp（那是 Agent 数据面），Control IPC 是独立的 Human plane 协议。

use serde::{Deserialize, Serialize};

/// 请求（CLI → MCP Server）。
#[derive(Debug, Deserialize, Serialize)]
pub struct ControlRequest {
    /// 请求 ID（用于匹配响应）
    pub id: u64,
    /// 方法名（如 "session.list" / "session.set_approval_mode"）
    pub method: String,
    /// 参数（方法特定）
    #[serde(default)]
    pub params: serde_json::Value,
}

/// 响应（MCP Server → CLI）。
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ControlResponse {
    /// 成功
    Ok {
        id: u64,
        ok: bool, // 恒为 true
        result: serde_json::Value,
    },
    /// 错误
    Err {
        id: u64,
        ok: bool, // 恒为 false
        error: ControlError,
    },
}

/// 错误信息。
#[derive(Debug, Serialize)]
pub struct ControlError {
    pub code: String,
    pub message: String,
}

impl ControlError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// HELLO 认证请求（连接后第一条消息）。
#[derive(Debug, Deserialize, Serialize)]
pub struct HelloRequest {
    pub token: String,
}

/// HELLO 认证响应。
#[derive(Debug, Serialize, Deserialize)]
pub struct HelloResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// session.list / session.get 返回的 session 信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionControlInfo {
    pub id: String,
    pub host: String,
    pub state: String,
    pub approval_mode: String,
}

/// session.set_approval_mode 参数。
#[derive(Debug, Deserialize)]
pub struct SetApprovalModeParams {
    pub session_id: String,
    pub mode: String, // "standard" | "unrestricted"
}

impl SetApprovalModeParams {
    /// 解析 mode 字符串，返回标准化值或错误。
    pub fn parse_mode(&self) -> Result<&str, ControlError> {
        match self.mode.as_str() {
            "standard" | "unrestricted" => Ok(&self.mode),
            _ => Err(ControlError::new(
                "INVALID_ARGUMENT",
                format!(
                    "invalid mode '{}'; expected 'standard' or 'unrestricted'",
                    self.mode
                ),
            )),
        }
    }
}
