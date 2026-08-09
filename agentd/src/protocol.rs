//! termbridge-agentd 协议层（ADR-0004 §4）
//!
//! length-prefixed JSON：`[4 bytes big-endian length] [JSON payload UTF-8]`
//!
//! 不用 JSON-RPC 2.0：daemon 是 TermBridge 内部组件，最简协议即可。
//! 保留 `id` 字段以支持未来异步 pipeline；事件无 `id`，靠 `event` 字段区分。

use std::io;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// ───────────────────────────────────────────────────────────────────────────
// 常量
// ───────────────────────────────────────────────────────────────────────────

/// 协议版本（仅在不向后兼容的协议变更时 bump，独立于 build 版本）
pub const PROTOCOL_VERSION: u32 = 1;

/// Build 版本字符串（握手用）
pub const BUILD_VERSION: &str = "0.1.0";

/// 单条消息最大长度（128MB，防止恶意长度前缀导致 OOM）
pub const MAX_MSG_LEN: u32 = 128 * 1024 * 1024;

// ───────────────────────────────────────────────────────────────────────────
// 请求 / 响应 / 事件
// ───────────────────────────────────────────────────────────────────────────

/// client → daemon 请求
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Request {
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

/// daemon → client 同步响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorDetail>,
}

/// 错误详情
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

impl ErrorDetail {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// daemon → client 异步推送事件（无 id，靠 event 字段区分）
///
/// 扁平化设计：所有可能字段都放顶层，按 event 类型选择性序列化（skip_serializing_if）。
/// 简单优先，避免 serde tag + flatten 的复杂组合。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Event {
    /// 事件类型："pty_data" / "pty_exit" / "session_lost"
    pub event: String,
    /// 所属 session id
    pub session_id: String,
    // —— pty_data 专用字段 ——
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_start: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_end: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_truncated: Option<bool>,
    /// base64 编码的 PTY output（pty_data 专用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    // —— pty_exit 专用字段 ——
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    // —— session_lost 专用字段 ——
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Event {
    /// 构造 pty_data 事件
    pub fn pty_data(
        session_id: &str,
        cursor_start: u64,
        cursor_end: u64,
        is_truncated: bool,
        data: String,
    ) -> Self {
        Self {
            event: "pty_data".to_string(),
            session_id: session_id.to_string(),
            cursor_start: Some(cursor_start),
            cursor_end: Some(cursor_end),
            is_truncated: Some(is_truncated),
            data: Some(data),
            exit_code: None,
            reason: None,
        }
    }

    /// 构造 pty_exit 事件
    pub fn pty_exit(session_id: &str, exit_code: i32) -> Self {
        Self {
            event: "pty_exit".to_string(),
            session_id: session_id.to_string(),
            cursor_start: None,
            cursor_end: None,
            is_truncated: None,
            data: None,
            exit_code: Some(exit_code),
            reason: None,
        }
    }

    /// 构造 session_lost 事件
    pub fn session_lost(session_id: &str, reason: impl Into<String>) -> Self {
        Self {
            event: "session_lost".to_string(),
            session_id: session_id.to_string(),
            cursor_start: None,
            cursor_end: None,
            is_truncated: None,
            data: None,
            exit_code: None,
            reason: Some(reason.into()),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 复合参数 / 返回结构
// ───────────────────────────────────────────────────────────────────────────

/// PTY 尺寸
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

/// session.list 返回的单个 session 信息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionInfo {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// "created" / "attached" / "detached" / "lost"
    pub state: String,
    /// ISO8601 时间戳
    pub created_at: String,
    pub last_activity_at: String,
    pub pty_size: PtySize,
    /// RingBuffer 当前 written 计数
    pub written: u64,
}

/// session.attach / session.read_output 返回的增量数据
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReadResult {
    pub cursor_start: u64,
    pub cursor_end: u64,
    pub is_truncated: bool,
    /// base64 编码
    pub data: String,
}

/// control 键枚举（session.send_control 参数）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKey {
    CtrlC,
    CtrlD,
    CtrlZ,
    Tab,
    Enter,
    Escape,
}

impl ControlKey {
    /// 转换为 PTY 输入字节
    pub fn as_bytes(self) -> &'static [u8] {
        match self {
            ControlKey::CtrlC => b"\x03",
            ControlKey::CtrlD => b"\x04",
            ControlKey::CtrlZ => b"\x1a",
            ControlKey::Tab => b"\t",
            ControlKey::Enter => b"\r",
            ControlKey::Escape => b"\x1b",
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 编解码
// ───────────────────────────────────────────────────────────────────────────

/// 编码消息：4 字节 big-endian 长度前缀 + JSON payload
pub fn encode(msg: &impl Serialize) -> Vec<u8> {
    let json = serde_json::to_vec(msg).expect("序列化消息不应失败");
    let len = json.len() as u32;
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&json);
    out
}

/// 异步写入消息
pub async fn write_msg<W: AsyncWrite + Unpin>(w: &mut W, msg: &impl Serialize) -> io::Result<()> {
    let buf = encode(msg);
    w.write_all(&buf).await
}

/// 异步读取消息：读 4B 长度 + JSON，返回 Value 让调用方按需 deserialize
pub async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<serde_json::Value> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "消息长度为 0"));
    }
    if len > MAX_MSG_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("消息长度 {} 超过上限 {}", len, MAX_MSG_LEN),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    let value: serde_json::Value =
        serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(value)
}

/// 辅助：从 Value 反序列化为具体类型
pub fn from_value<T: DeserializeOwned>(v: &serde_json::Value) -> Result<T, serde_json::Error> {
    serde_json::from_value(v.clone())
}

/// 构造成功响应
pub fn ok_response(id: u64, result: serde_json::Value) -> Response {
    Response {
        id,
        ok: true,
        result: Some(result),
        error: None,
    }
}

/// 构造失败响应
pub fn err_response(id: u64, code: &str, message: impl Into<String>) -> Response {
    Response {
        id,
        ok: false,
        result: None,
        error: Some(ErrorDetail::new(code, message)),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 单元测试
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// encode → 手动 decode 往返
    #[test]
    fn encode_decode_roundtrip() {
        let req = Request {
            id: 42,
            method: "session.create".to_string(),
            params: serde_json::json!({
                "shell": "/bin/bash",
                "cwd": null,
                "pty_size": { "rows": 40, "cols": 120 },
                "name": "test"
            }),
        };
        let bytes = encode(&req);
        // 前 4 字节是长度
        assert!(bytes.len() > 4);
        let len = u32::from_be_bytes(bytes[..4].try_into().unwrap());
        let json_len = bytes.len() - 4;
        assert_eq!(len as usize, json_len);
        // 反序列化 JSON 部分
        let decoded: Request = serde_json::from_slice(&bytes[4..]).unwrap();
        assert_eq!(decoded.id, 42);
        assert_eq!(decoded.method, "session.create");
        assert_eq!(decoded.params["pty_size"]["rows"], 40);
    }

    /// 长度前缀正确性
    #[test]
    fn length_prefix_is_big_endian() {
        let resp = Response {
            id: 1,
            ok: true,
            result: Some(serde_json::json!({"written": 0})),
            error: None,
        };
        let bytes = encode(&resp);
        let len = u32::from_be_bytes(bytes[..4].try_into().unwrap());
        assert_eq!(len as usize, bytes.len() - 4);
    }

    /// 空 payload（params 为空对象）
    #[test]
    fn empty_payload_roundtrip() {
        let req = Request {
            id: 5,
            method: "session.list".to_string(),
            params: serde_json::json!({}),
        };
        let bytes = encode(&req);
        let decoded: Request = serde_json::from_slice(&bytes[4..]).unwrap();
        assert_eq!(decoded.method, "session.list");
        assert!(decoded.params.is_object());
    }

    /// 大 payload（>64KB）往返
    #[test]
    fn large_payload_roundtrip() {
        // 构造 100KB 的 data 字符串
        let big_data = "x".repeat(100 * 1024);
        let req = Request {
            id: 99,
            method: "session.send_input".to_string(),
            params: serde_json::json!({
                "session_id": "sess_abc",
                "data": big_data,
            }),
        };
        let bytes = encode(&req);
        // 长度前缀应大于 64KB
        let len = u32::from_be_bytes(bytes[..4].try_into().unwrap());
        assert!(len > 64 * 1024);
        let decoded: Request = serde_json::from_slice(&bytes[4..]).unwrap();
        assert_eq!(decoded.params["data"].as_str().unwrap().len(), 100 * 1024);
    }

    /// Event 序列化只包含相关字段
    #[test]
    fn event_pty_data_serialization() {
        let ev = Event::pty_data("sess_abc", 100, 200, false, "aGVsbG8=".to_string());
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["event"], "pty_data");
        assert_eq!(json["session_id"], "sess_abc");
        assert_eq!(json["cursor_start"], 100);
        assert_eq!(json["cursor_end"], 200);
        assert_eq!(json["is_truncated"], false);
        assert_eq!(json["data"], "aGVsbG8=");
        // pty_data 事件不应有 exit_code / reason
        assert!(json.get("exit_code").is_none());
        assert!(json.get("reason").is_none());
    }

    /// pty_exit 事件只包含 exit_code
    #[test]
    fn event_pty_exit_serialization() {
        let ev = Event::pty_exit("sess_abc", 0);
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["event"], "pty_exit");
        assert_eq!(json["exit_code"], 0);
        assert!(json.get("cursor_start").is_none());
        assert!(json.get("data").is_none());
    }

    /// session_lost 事件只包含 reason
    #[test]
    fn event_session_lost_serialization() {
        let ev = Event::session_lost("sess_abc", "pty eof");
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["event"], "session_lost");
        assert_eq!(json["reason"], "pty eof");
        assert!(json.get("exit_code").is_none());
        assert!(json.get("data").is_none());
    }

    /// ControlKey 字节映射
    #[test]
    fn control_key_bytes() {
        assert_eq!(ControlKey::CtrlC.as_bytes(), b"\x03");
        assert_eq!(ControlKey::CtrlD.as_bytes(), b"\x04");
        assert_eq!(ControlKey::CtrlZ.as_bytes(), b"\x1a");
        assert_eq!(ControlKey::Tab.as_bytes(), b"\t");
        assert_eq!(ControlKey::Enter.as_bytes(), b"\r");
        assert_eq!(ControlKey::Escape.as_bytes(), b"\x1b");
    }

    /// Response skip_serializing_if 行为
    #[test]
    fn response_skip_none_fields() {
        let ok = ok_response(1, serde_json::json!({"written": 0}));
        let json = serde_json::to_value(&ok).unwrap();
        assert!(json.get("error").is_none());

        let err = err_response(1, "NOT_FOUND", "session missing");
        let json = serde_json::to_value(&err).unwrap();
        assert!(json.get("result").is_none());
        assert_eq!(json["error"]["code"], "NOT_FOUND");
    }

    /// 异步 read_msg 往返
    #[tokio::test]
    async fn async_write_read_roundtrip() {
        use tokio::io::duplex;
        let (mut client, mut server) = duplex(64 * 1024);
        let req = Request {
            id: 7,
            method: "hello".to_string(),
            params: serde_json::json!({"client_protocol_version": 1}),
        };
        write_msg(&mut client, &req).await.unwrap();
        let value = read_msg(&mut server).await.unwrap();
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "hello");
    }

    /// 长度为 0 的消息应报错
    #[tokio::test]
    async fn read_msg_zero_length_errors() {
        use tokio::io::duplex;
        let (mut client, mut server) = duplex(64);
        client.write_all(&[0u8, 0, 0, 0]).await.unwrap();
        let err = read_msg(&mut server).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// 超长消息应报错
    #[tokio::test]
    async fn read_msg_oversized_errors() {
        use tokio::io::duplex;
        let (mut client, mut server) = duplex(64);
        // 声称 256MB
        client.write_all(&[0xFFu8, 0, 0, 0]).await.unwrap();
        let err = read_msg(&mut server).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
