//! daemon client —— 封装与 termbridge-agentd 的 RPC 调用
//!
//! 一对一请求响应模型：每个方法自增 id → 构造 Request → write_msg → read_msg → 解析 Response。
//! 事件（pty_data / pty_exit / session_lost）在 attach 交互模式下由 main.rs 直接处理。

use std::path::Path;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Deserialize;
use tokio::net::UnixStream;

use crate::protocol::{
    methods, read_msg, write_msg, ErrorDetail, PtySize, Request, Response, BUILD_VERSION,
    PROTOCOL_VERSION,
};

/// daemon 错误类型
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    /// 协议层错误（响应 id 不匹配、缺少字段等）
    #[error("协议错误: {0}")]
    Protocol(String),
    /// 协议版本不匹配
    #[error("协议版本不匹配: client={client} daemon={daemon}")]
    ProtocolMismatch { client: u32, daemon: u32 },
    /// daemon 返回 ok=false
    #[error("RPC 错误: {code}: {message}")]
    Rpc { code: String, message: String },
    /// base64 解码失败
    #[error("base64 解码失败: {0}")]
    Base64(String),
}

pub type Result<T> = std::result::Result<T, DaemonError>;

/// hello 响应
#[derive(Debug, Clone, Deserialize)]
pub struct HelloInfo {
    pub daemon_protocol_version: u32,
    pub daemon_build: String,
    pub daemon_id: String,
}

/// session.create 响应
#[derive(Debug, Clone, Deserialize)]
pub struct CreateResult {
    pub session_id: String,
    pub written: u64,
}

/// session.attach / session.read_output 响应的 wire 格式（data 为 base64 字符串）
#[derive(Debug, Clone, Deserialize)]
struct ReadSinceResultWire {
    cursor_start: u64,
    cursor_end: u64,
    is_truncated: bool,
    #[serde(default)]
    data: Option<String>,
}

/// session.attach / session.read_output 响应（data 已 base64 解码为原始字节）
#[derive(Debug, Clone)]
pub struct ReadSinceResult {
    pub cursor_start: u64,
    pub cursor_end: u64,
    pub is_truncated: bool,
    pub data: Vec<u8>,
}

/// session.list 返回的 session 信息
#[derive(Debug, Clone, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub state: String,
    pub created_at: String,
    pub last_activity_at: String,
    pub pty_size: PtySize,
    pub written: u64,
}

/// daemon client
pub struct DaemonClient {
    /// 底层 Unix socket（attach 交互模式时取出手动管理）
    pub(crate) stream: UnixStream,
    /// 下一个请求 id（自增）
    pub(crate) next_id: u64,
    /// daemon 实例 id（hello 后填充）
    pub(crate) daemon_id: String,
}

impl DaemonClient {
    /// 连接 daemon 并完成 hello 握手，校验协议版本
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket_path).await?;
        let mut client = Self {
            stream,
            next_id: 1,
            daemon_id: String::new(),
        };
        let info = client.hello().await?;
        client.daemon_id = info.daemon_id;
        Ok(client)
    }

    /// hello 握手：发送 client 版本，校验 daemon 协议版本
    pub async fn hello(&mut self) -> Result<HelloInfo> {
        let params = serde_json::json!({
            "client_protocol_version": PROTOCOL_VERSION,
            "client_build": BUILD_VERSION,
        });
        let result = self.call(methods::HELLO, params).await?;
        let info: HelloInfo = serde_json::from_value(result)?;
        if info.daemon_protocol_version != PROTOCOL_VERSION {
            return Err(DaemonError::ProtocolMismatch {
                client: PROTOCOL_VERSION,
                daemon: info.daemon_protocol_version,
            });
        }
        self.daemon_id = info.daemon_id.clone();
        Ok(info)
    }

    /// 创建 session
    pub async fn create_session(
        &mut self,
        shell: &str,
        cwd: Option<&str>,
        pty_size: PtySize,
        name: Option<&str>,
    ) -> Result<CreateResult> {
        let params = serde_json::json!({
            "shell": shell,
            "cwd": cwd,
            "pty_size": { "rows": pty_size.rows, "cols": pty_size.cols },
            "name": name,
        });
        let result = self.call(methods::SESSION_CREATE, params).await?;
        Ok(serde_json::from_value(result)?)
    }

    /// attach session，拉取 since_cursor 之后的增量输出
    pub async fn attach_session(
        &mut self,
        session_id: &str,
        since_cursor: u64,
    ) -> Result<ReadSinceResult> {
        let params = serde_json::json!({
            "session_id": session_id,
            "since_cursor": since_cursor,
        });
        let result = self.call(methods::SESSION_ATTACH, params).await?;
        parse_read_since(result)
    }

    /// detach session
    pub async fn detach_session(&mut self, session_id: &str) -> Result<()> {
        let params = serde_json::json!({ "session_id": session_id });
        self.call(methods::SESSION_DETACH, params).await?;
        Ok(())
    }

    /// 列出 daemon 上所有 session
    pub async fn list_sessions(&mut self) -> Result<Vec<SessionInfo>> {
        let result = self.call(methods::SESSION_LIST, serde_json::json!({})).await?;
        Ok(serde_json::from_value(result)?)
    }

    /// 发送输入字节到 session 的 PTY
    pub async fn send_input(&mut self, session_id: &str, data: &[u8]) -> Result<()> {
        let b64 = B64.encode(data);
        let params = serde_json::json!({ "session_id": session_id, "data": b64 });
        self.call(methods::SESSION_SEND_INPUT, params).await?;
        Ok(())
    }

    /// 发送控制字符（如 "C-c"）
    pub async fn send_control(&mut self, session_id: &str, control: &str) -> Result<()> {
        let params = serde_json::json!({ "session_id": session_id, "control": control });
        self.call(methods::SESSION_SEND_CONTROL, params).await?;
        Ok(())
    }

    /// 调整 PTY 尺寸
    pub async fn resize(&mut self, session_id: &str, rows: u16, cols: u16) -> Result<()> {
        let params = serde_json::json!({
            "session_id": session_id,
            "pty_size": { "rows": rows, "cols": cols }
        });
        self.call(methods::SESSION_RESIZE, params).await?;
        Ok(())
    }

    /// 读取 session 输出（since_cursor 之后的增量）
    pub async fn read_output(
        &mut self,
        session_id: &str,
        since_cursor: u64,
    ) -> Result<ReadSinceResult> {
        let params = serde_json::json!({
            "session_id": session_id,
            "since_cursor": since_cursor,
        });
        let result = self.call(methods::SESSION_READ_OUTPUT, params).await?;
        parse_read_since(result)
    }

    /// 关闭 session
    pub async fn close_session(&mut self, session_id: &str) -> Result<()> {
        let params = serde_json::json!({ "session_id": session_id });
        self.call(methods::SESSION_CLOSE, params).await?;
        Ok(())
    }

    /// 关闭 daemon
    pub async fn shutdown_daemon(&mut self) -> Result<()> {
        self.call(methods::DAEMON_SHUTDOWN, serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// 取出底层 stream 与当前 next_id（attach 交互模式用）
    pub(crate) fn into_parts(self) -> (UnixStream, u64) {
        (self.stream, self.next_id)
    }

    /// 核心 RPC 调用：自增 id → 发请求 → 读响应 → 校验 → 返回 result
    async fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request {
            id,
            method: method.to_string(),
            params,
        };
        write_msg(&mut self.stream, &req).await?;
        let value = read_msg(&mut self.stream).await?;
        let resp: Response = serde_json::from_value(value)?;
        if resp.id != id {
            return Err(DaemonError::Protocol(format!(
                "响应 id 不匹配: 期望 {id} 实际 {}",
                resp.id
            )));
        }
        if !resp.ok {
            let err: ErrorDetail = resp
                .error
                .ok_or_else(|| DaemonError::Protocol("ok=false 但缺少 error 字段".into()))?;
            return Err(DaemonError::Rpc {
                code: err.code,
                message: err.message,
            });
        }
        resp.result
            .ok_or_else(|| DaemonError::Protocol("ok=true 但缺少 result 字段".into()))
    }
}

/// 把 ReadSinceResultWire 转成 ReadSinceResult（base64 解码 data）
fn parse_read_since(value: serde_json::Value) -> Result<ReadSinceResult> {
    let wire: ReadSinceResultWire = serde_json::from_value(value)?;
    let data = match wire.data {
        Some(s) if !s.is_empty() => B64
            .decode(&s)
            .map_err(|e| DaemonError::Base64(e.to_string()))?,
        _ => Vec::new(),
    };
    Ok(ReadSinceResult {
        cursor_start: wire.cursor_start,
        cursor_end: wire.cursor_end,
        is_truncated: wire.is_truncated,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_read_since_with_data() {
        let value = serde_json::json!({
            "cursor_start": 15000,
            "cursor_end": 15234,
            "is_truncated": false,
            "data": "aGVsbG8=" // "hello"
        });
        let result = parse_read_since(value).unwrap();
        assert_eq!(result.cursor_start, 15000);
        assert_eq!(result.cursor_end, 15234);
        assert!(!result.is_truncated);
        assert_eq!(result.data, b"hello");
    }

    #[test]
    fn test_parse_read_since_no_data() {
        let value = serde_json::json!({
            "cursor_start": 0,
            "cursor_end": 0,
            "is_truncated": false,
        });
        let result = parse_read_since(value).unwrap();
        assert_eq!(result.cursor_start, 0);
        assert!(result.data.is_empty());
    }

    #[test]
    fn test_parse_read_since_truncated() {
        let value = serde_json::json!({
            "cursor_start": 17000,
            "cursor_end": 20000,
            "is_truncated": true,
            "data": ""
        });
        let result = parse_read_since(value).unwrap();
        assert!(result.is_truncated);
        assert!(result.data.is_empty());
    }

    #[test]
    fn test_parse_read_since_invalid_base64() {
        let value = serde_json::json!({
            "cursor_start": 0,
            "cursor_end": 1,
            "is_truncated": false,
            "data": "!!!not base64!!!"
        });
        let result = parse_read_since(value);
        assert!(matches!(result, Err(DaemonError::Base64(_))));
    }

    #[test]
    fn test_session_info_deserialize() {
        let value = serde_json::json!({
            "id": "sess_abc123",
            "name": "python server",
            "state": "detached",
            "created_at": "2026-08-09T10:00:00Z",
            "last_activity_at": "2026-08-09T10:05:00Z",
            "pty_size": { "rows": 40, "cols": 120 },
            "written": 23456
        });
        let info: SessionInfo = serde_json::from_value(value).unwrap();
        assert_eq!(info.id, "sess_abc123");
        assert_eq!(info.name.as_deref(), Some("python server"));
        assert_eq!(info.state, "detached");
        assert_eq!(info.pty_size.rows, 40);
        assert_eq!(info.pty_size.cols, 120);
        assert_eq!(info.written, 23456);
    }

    #[test]
    fn test_session_info_deserialize_no_name() {
        let value = serde_json::json!({
            "id": "sess_xyz",
            "state": "attached",
            "created_at": "2026-08-09T10:00:00Z",
            "last_activity_at": "2026-08-09T10:05:00Z",
            "pty_size": { "rows": 24, "cols": 80 },
            "written": 0
        });
        let info: SessionInfo = serde_json::from_value(value).unwrap();
        assert!(info.name.is_none());
    }

    #[test]
    fn test_hello_info_deserialize() {
        let value = serde_json::json!({
            "daemon_protocol_version": 1,
            "daemon_build": "0.1.0",
            "daemon_id": "daed_abc"
        });
        let info: HelloInfo = serde_json::from_value(value).unwrap();
        assert_eq!(info.daemon_protocol_version, 1);
        assert_eq!(info.daemon_build, "0.1.0");
        assert_eq!(info.daemon_id, "daed_abc");
    }

    #[test]
    fn test_create_result_deserialize() {
        let value = serde_json::json!({"session_id": "sess_new", "written": 0});
        let cr: CreateResult = serde_json::from_value(value).unwrap();
        assert_eq!(cr.session_id, "sess_new");
        assert_eq!(cr.written, 0);
    }

    #[test]
    fn test_daemon_error_display() {
        let e = DaemonError::Rpc {
            code: "INVALID_ARGUMENT".into(),
            message: "shell not found".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("INVALID_ARGUMENT"));
        assert!(s.contains("shell not found"));
    }
}
