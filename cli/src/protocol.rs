//! termbridge-cli 协议定义（ADR-0004 §4）
//!
//! length-prefixed JSON：`[4 bytes big-endian length] [JSON payload UTF-8]`
//!
//! 这是 cli 内复制的协议定义，与 agentd/protocol.rs 字段保持一致。
//! Phase 3-A W3 会把 protocol 抽到共享 crate，届时删除此副本。

use serde::{Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 协议版本号（独立于 build 版本，仅在不向后兼容的协议变更时 bump）
pub const PROTOCOL_VERSION: u32 = 1;

/// CLI build 版本
pub const BUILD_VERSION: &str = "0.1.0";

/// 请求（client → daemon）。`id` 用于匹配请求/响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

/// 响应（daemon → client，同步）。`ok=true` 时 `result` 有值，`ok=false` 时 `error` 有值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    pub result: Option<serde_json::Value>,
    pub error: Option<ErrorDetail>,
}

/// 错误详情
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

/// 事件（daemon → client，异步推送）。无 `id`，靠 `event` 字段区分。
/// 字段按事件类型选择性序列化（未使用的字段为 None，序列化时由 serde 跳过）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_start: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_end: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>, // base64
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// PTY 尺寸
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

impl PtySize {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self { rows, cols }
    }
}

/// 方法名常量（与 agentd 一致）
pub mod methods {
    pub const HELLO: &str = "hello";
    pub const SESSION_CREATE: &str = "session.create";
    pub const SESSION_ATTACH: &str = "session.attach";
    pub const SESSION_DETACH: &str = "session.detach";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_SEND_INPUT: &str = "session.send_input";
    pub const SESSION_SEND_CONTROL: &str = "session.send_control";
    pub const SESSION_RESIZE: &str = "session.resize";
    pub const SESSION_READ_OUTPUT: &str = "session.read_output";
    pub const SESSION_CLOSE: &str = "session.close";
    pub const DAEMON_SHUTDOWN: &str = "daemon.shutdown";
}

/// 事件名常量
pub mod events {
    pub const PTY_DATA: &str = "pty_data";
    pub const PTY_EXIT: &str = "pty_exit";
    pub const SESSION_LOST: &str = "session_lost";
}

/// 编码消息为 length-prefixed JSON 字节序列：`[4B BE length][JSON UTF-8]`
pub fn encode(msg: &impl Serialize) -> Vec<u8> {
    let json = serde_json::to_vec(msg).expect("序列化不应失败");
    let len = json.len() as u32;
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&json);
    out
}

/// 异步写一条 length-prefixed JSON 消息
pub async fn write_msg<W: AsyncWrite + Unpin>(w: &mut W, msg: &impl Serialize) -> io::Result<()> {
    let bytes = encode(msg);
    w.write_all(&bytes).await
}

/// 异步读一条 length-prefixed JSON 消息，返回解析后的 JSON Value
pub async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<serde_json::Value> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_length_prefix() {
        let req = Request {
            id: 1,
            method: "hello".into(),
            params: serde_json::json!({"client_protocol_version": 1}),
        };
        let bytes = encode(&req);
        // 前 4 字节为大端长度
        assert!(bytes.len() > 4);
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(len, bytes.len() - 4);
        // 后半部分为合法 JSON
        let value: serde_json::Value = serde_json::from_slice(&bytes[4..]).unwrap();
        assert_eq!(value["id"], 1);
        assert_eq!(value["method"], "hello");
        assert_eq!(value["params"]["client_protocol_version"], 1);
    }

    #[test]
    fn test_request_roundtrip() {
        let req = Request {
            id: 42,
            method: "session.create".into(),
            params: serde_json::json!({
                "shell": "/bin/bash",
                "cwd": null,
                "pty_size": {"rows": 40, "cols": 120},
                "name": "python server"
            }),
        };
        let bytes = encode(&req);
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let req2: Request = serde_json::from_slice(&bytes[4..4 + len]).unwrap();
        assert_eq!(req2.id, req.id);
        assert_eq!(req2.method, req.method);
        assert_eq!(req2.params, req.params);
    }

    #[test]
    fn test_response_roundtrip_ok() {
        let resp = Response {
            id: 1,
            ok: true,
            result: Some(serde_json::json!({"session_id": "sess_abc", "written": 0})),
            error: None,
        };
        let bytes = encode(&resp);
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let resp2: Response = serde_json::from_slice(&bytes[4..4 + len]).unwrap();
        assert_eq!(resp2.id, 1);
        assert!(resp2.ok);
        assert_eq!(resp2.result.unwrap()["session_id"], "sess_abc");
        assert!(resp2.error.is_none());
    }

    #[test]
    fn test_response_roundtrip_err() {
        let resp = Response {
            id: 1,
            ok: false,
            result: None,
            error: Some(ErrorDetail {
                code: "INVALID_ARGUMENT".into(),
                message: "shell not found".into(),
            }),
        };
        let bytes = encode(&resp);
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let resp2: Response = serde_json::from_slice(&bytes[4..4 + len]).unwrap();
        assert!(!resp2.ok);
        let err = resp2.error.unwrap();
        assert_eq!(err.code, "INVALID_ARGUMENT");
        assert_eq!(err.message, "shell not found");
    }

    #[test]
    fn test_event_skip_none_fields() {
        let ev = Event {
            event: "pty_exit".into(),
            session_id: "sess_abc".into(),
            cursor_start: None,
            cursor_end: None,
            is_truncated: None,
            data: None,
            exit_code: Some(0),
            reason: None,
        };
        let value = serde_json::to_value(&ev).unwrap();
        // None 字段应被跳过
        assert!(value.get("cursor_start").is_none());
        assert!(value.get("data").is_none());
        assert_eq!(value["exit_code"], 0);
        // 往返
        let ev2: Event = serde_json::from_value(value).unwrap();
        assert_eq!(ev2.event, "pty_exit");
        assert_eq!(ev2.exit_code, Some(0));
    }

    #[test]
    fn test_event_pty_data() {
        let ev = Event {
            event: "pty_data".into(),
            session_id: "sess_abc".into(),
            cursor_start: Some(15000),
            cursor_end: Some(15234),
            is_truncated: Some(false),
            data: Some("aGVsbG8=".into()),
            exit_code: None,
            reason: None,
        };
        let bytes = encode(&ev);
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let ev2: Event = serde_json::from_slice(&bytes[4..4 + len]).unwrap();
        assert_eq!(ev2.cursor_start, Some(15000));
        assert_eq!(ev2.cursor_end, Some(15234));
        assert_eq!(ev2.is_truncated, Some(false));
        assert_eq!(ev2.data.as_deref(), Some("aGVsbG8="));
    }

    #[tokio::test]
    async fn test_write_read_roundtrip() {
        let (mut tx, mut rx) = tokio::io::duplex(4096);
        let msg = serde_json::json!({"id": 5, "method": "test", "params": {"x": 1}});
        write_msg(&mut tx, &msg).await.unwrap();
        let value = read_msg(&mut rx).await.unwrap();
        assert_eq!(value["id"], 5);
        assert_eq!(value["method"], "test");
        assert_eq!(value["params"]["x"], 1);
    }

    #[tokio::test]
    async fn test_write_read_multiple_messages() {
        let (mut tx, mut rx) = tokio::io::duplex(8192);
        for i in 0..5u64 {
            let msg = serde_json::json!({"id": i, "method": "ping"});
            write_msg(&mut tx, &msg).await.unwrap();
        }
        for i in 0..5u64 {
            let value = read_msg(&mut rx).await.unwrap();
            assert_eq!(value["id"], i);
        }
    }

    #[tokio::test]
    async fn test_read_msg_eof() {
        let buf: &[u8] = &[];
        let mut cursor = std::io::Cursor::new(buf);
        let result = read_msg(&mut cursor).await;
        assert!(result.is_err());
    }
}
