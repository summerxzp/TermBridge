//! TermBridge client 端 daemon 协议定义（ADR-0004 §4）
//!
//! length-prefixed JSON：`[4 bytes big-endian length] [JSON payload UTF-8]`
//!
//! 这是 agentd crate `protocol.rs` 的 client 端副本，与远端 daemon 通信使用。
//! Phase 3-A W3 暂用复制策略（ADR-0004 调研风险 1），未来抽 `termbridge-protocol`
//! 共享 crate 时删除此文件，改 `use termbridge_protocol::*`。
//!
//! 不依赖任何 Unix-only 库，纯 serde + tokio，Windows 可编译。

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

/// PTY 尺寸（daemon 协议侧类型，与 domain::provider::PtySize 字段一致但独立）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
}

impl PtySize {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self { rows, cols }
    }
}

/// 从 domain::provider::PtySize 转换
impl From<crate::domain::provider::PtySize> for PtySize {
    fn from(s: crate::domain::provider::PtySize) -> Self {
        Self {
            rows: s.rows,
            cols: s.cols,
        }
    }
}

/// 转回 domain::provider::PtySize
impl From<PtySize> for crate::domain::provider::PtySize {
    fn from(s: PtySize) -> Self {
        Self {
            rows: s.rows,
            cols: s.cols,
        }
    }
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
///
/// daemon 协议侧类型，与 domain::provider::ControlKey 变体一致但独立。
/// PersistentTerminalHandle 内部做转换。
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

/// 从 domain::provider::ControlKey 转换
impl From<crate::domain::provider::ControlKey> for ControlKey {
    fn from(c: crate::domain::provider::ControlKey) -> Self {
        match c {
            crate::domain::provider::ControlKey::CtrlC => ControlKey::CtrlC,
            crate::domain::provider::ControlKey::CtrlD => ControlKey::CtrlD,
            crate::domain::provider::ControlKey::CtrlZ => ControlKey::CtrlZ,
            crate::domain::provider::ControlKey::Tab => ControlKey::Tab,
            crate::domain::provider::ControlKey::Enter => ControlKey::Enter,
            crate::domain::provider::ControlKey::Escape => ControlKey::Escape,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 方法名 / 事件名常量
// ───────────────────────────────────────────────────────────────────────────

/// 方法名常量（与 agentd rpc.rs dispatch 一致）
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
        assert!(bytes.len() > 4);
        let len = u32::from_be_bytes(bytes[..4].try_into().unwrap());
        assert_eq!(len as usize, bytes.len() - 4);
        let decoded: Request = serde_json::from_slice(&bytes[4..]).unwrap();
        assert_eq!(decoded.id, 42);
        assert_eq!(decoded.method, "session.create");
    }

    #[test]
    fn pty_size_conversion() {
        let domain = crate::domain::provider::PtySize { rows: 30, cols: 100 };
        let proto: PtySize = domain.into();
        assert_eq!(proto.rows, 30);
        assert_eq!(proto.cols, 100);
        let back: crate::domain::provider::PtySize = proto.into();
        assert_eq!(back.rows, 30);
        assert_eq!(back.cols, 100);
    }

    #[test]
    fn control_key_conversion() {
        let domain = crate::domain::provider::ControlKey::CtrlC;
        let proto: ControlKey = domain.into();
        assert_eq!(proto.as_bytes(), b"\x03");
    }

    #[test]
    fn event_pty_data_serialization() {
        let ev = Event::pty_data("sess_abc", 100, 200, false, "aGVsbG8=".to_string());
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["event"], "pty_data");
        assert_eq!(json["cursor_start"], 100);
        assert!(json.get("exit_code").is_none());
    }

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
    }

    #[tokio::test]
    async fn read_msg_oversized_errors() {
        use tokio::io::duplex;
        let (mut client, mut server) = duplex(64);
        client.write_all(&[0xFFu8, 0, 0, 0]).await.unwrap();
        let err = read_msg(&mut server).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
