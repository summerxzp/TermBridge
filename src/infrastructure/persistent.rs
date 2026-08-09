//! persistent —— Phase 3-A W3：PersistentProvider + PersistentTerminalHandle + DaemonClient
//!
//! 远端 daemon persistent session 能力（ADR-0004）：
//! - `DaemonClient`：daemon RPC client，长持 SSH proxy 连接，reader task 处理 Response/Event
//! - `PersistentTerminalHandle`：实现 `TerminalHandle`，通过 DaemonClient 读写远端 PTY
//! - `PersistentProvider`：实现 `TerminalProvider`，persistent=true 走 daemon 路径，
//!   persistent=false 委托 `SshProvider`（Phase 1/2 Interactive 路径）
//!
//! ```text
//! PersistentProvider.open(request)
//!   ├── request.persistent == false → 委托 SshProvider.open（Phase 1/2 路径）
//!   └── request.persistent == true  → 走 daemon 路径：
//!         1. check_remote_runtime(host) → RemoteRuntimeState
//!         2. Missing → deploy_runtime(host)（SFTP 上传 agentd 二进制 + version 文件）
//!         3. bootstrap_daemon(host) → socket_path（幂等：已运行则返回现有 socket）
//!         4. DaemonClient::connect(ssh, host, socket_path) → hello 握手 + 协议版本校验
//!         5. daemon.session_create(...) → remote_session_id
//!         6. daemon.subscribe_pty_data() → pty_data_rx
//!         7. PersistentTerminalHandle::new(daemon, remote_session_id, pty_data_rx)
//! ```
//!
//! DaemonClient 内部架构：
//! ```text
//! DaemonClient (Arc<DaemonClientInner>)
//!   ├── write: TokioMutex<ChannelWriteHalf>  — call() 写 Request
//!   ├── pending: ParkingMutex<HashMap<id, oneshot::Sender>>  — 等待 Response
//!   ├── pty_data_subscribers: ParkingMutex<Vec<mpsc::Sender<Bytes>>>  — pty_data 推送
//!   ├── event_tx: broadcast::Sender<Event>  — pty_exit/session_lost 推送
//!   └── reader task: 独占 read_half，循环 read_msg
//!         ├── Response（有 id）→ 匹配 pending，oneshot 发送
//!         └── Event（有 event）→ pty_data 解码推 mpsc；pty_exit/session_lost 推 broadcast
//! ```

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::prelude::*;
use bytes::Bytes;
use parking_lot::Mutex as ParkingMutex;
use russh::client;
use russh::ChannelMsg;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex as TokioMutex};

use crate::domain::provider::{
    ControlKey, Host, OpenTerminalRequest, PtySize, TerminalHandle, TerminalProvider, TermError,
};
use crate::infrastructure::daemon_proto::{
    self, events, from_value, methods, ControlKey as ProtoControlKey, ErrorDetail, Event,
    PtySize as ProtoPtySize, ReadResult, Response, BUILD_VERSION, PROTOCOL_VERSION,
};
use crate::infrastructure::ssh::{SshProvider, SshTerminalHandle};

// ───────────────────────────────────────────────────────────────────────────
// 类型别名：SSH channel 读写半（与 ssh.rs exec_stream 返回类型一致）
// ───────────────────────────────────────────────────────────────────────────

type WriteHalf = russh::ChannelWriteHalf<client::Msg>;
type ReadHalf = russh::ChannelReadHalf;

// ───────────────────────────────────────────────────────────────────────────
// 远端 runtime 状态（ADR-0004 §6/§7）
// ───────────────────────────────────────────────────────────────────────────

/// 远端 daemon runtime 探测结果。
///
/// - `Missing`：agentd 二进制未部署（`test -x` 失败或 version 文件不存在）
/// - `Stopped`：二进制已部署但 daemon 进程未运行（`pgrep` 无输出）
/// - `Running`：daemon 进程已在运行（`pgrep` 有输出）
enum RemoteRuntimeState {
    Missing,
    Stopped,
    Running,
}

// ───────────────────────────────────────────────────────────────────────────
// DaemonClientInner —— reader task 与 call() 共享的状态
// ───────────────────────────────────────────────────────────────────────────

/// DaemonClient 的共享内部状态。
///
/// `write` 用 `tokio::sync::Mutex`：`write_msg` 是 async，需跨 await 持锁；
/// guard 是 Send。
///
/// `pending` / `pty_data_subscribers` 用 `parking_lot::Mutex`：仅同步操作
/// （insert/remove/push/try_send），不跨 await，同步锁更高效且 `subscribe_pty_data`
/// 可在同步上下文调用。
struct DaemonClientInner {
    /// SSH proxy 写半。call() 写 Request 到此。
    write: TokioMutex<WriteHalf>,
    /// 等待响应的请求表：id → oneshot::Sender。reader task 收 Response 后取出发送。
    pending: ParkingMutex<HashMap<u64, oneshot::Sender<Response>>>,
    /// pty_data 订阅者列表。reader task 解码 pty_data 后推入所有 subscriber。
    pty_data_subscribers: ParkingMutex<Vec<mpsc::Sender<Bytes>>>,
    /// 事件广播：pty_exit / session_lost。PersistentTerminalHandle::read 监听以返回 EOF。
    event_tx: broadcast::Sender<Event>,
    /// 连接是否已关闭（reader task EOF / write 失败）。call() 前检查。
    closed: AtomicBool,
}

// ───────────────────────────────────────────────────────────────────────────
// DaemonClient —— daemon RPC client
// ───────────────────────────────────────────────────────────────────────────

/// daemon RPC client（长持 SSH proxy 连接）。
///
/// 通过 `SshProvider::exec_stream("agentd proxy --sock <path>")` 获取双向字节流，
/// 在其上跑 length-prefixed JSON RPC 协议（daemon_proto）。
///
/// reader task 独占 read_half，循环 `read_msg` 分发 Response（按 id 匹配 pending）
/// 与 Event（pty_data → mpsc，pty_exit/session_lost → broadcast）。
///
/// call() 写 Request 到 write_half（`Arc<TokioMutex<WriteHalf>>` 共享），await oneshot。
pub struct DaemonClient {
    inner: Arc<DaemonClientInner>,
    next_id: AtomicU64,
}

impl DaemonClient {
    /// 连接 daemon：exec_stream 开 proxy → spawn reader task → hello 握手 + 协议版本校验。
    ///
    /// hello 请求：`{method:"hello", params:{client_protocol_version, client_build}}`
    /// hello 响应：`{daemon_protocol_version, daemon_id, daemon_build}`
    /// 校验 `daemon_protocol_version == PROTOCOL_VERSION`，不匹配返回 `DaemonProtocolMismatch`。
    pub async fn connect(
        ssh: &SshProvider,
        host: &Host,
        socket_path: &str,
    ) -> Result<Arc<Self>, TermError> {
        let cmd = format!(
            "~/.local/share/termbridge/termbridge-agentd proxy --sock {}",
            socket_path
        );
        tracing::info!(host = %host.name, socket_path, "daemon proxy connecting");

        let (read_half, write_half) = ssh.exec_stream(host, &cmd).await?;

        let (event_tx, _) = broadcast::channel(64);
        let inner = Arc::new(DaemonClientInner {
            write: TokioMutex::new(write_half),
            pending: ParkingMutex::new(HashMap::new()),
            pty_data_subscribers: ParkingMutex::new(Vec::new()),
            event_tx,
            closed: AtomicBool::new(false),
        });

        // spawn reader task：独占 read_half，分发 Response / Event
        let reader_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            reader_loop(read_half, reader_inner).await;
        });

        let client = Arc::new(Self {
            inner,
            next_id: AtomicU64::new(1),
        });

        // hello 握手
        let hello_params = serde_json::json!({
            "client_protocol_version": PROTOCOL_VERSION,
            "client_build": BUILD_VERSION,
        });
        let result = client.call(methods::HELLO, hello_params).await?;

        let daemon_version = result
            .get("daemon_protocol_version")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                TermError::ChannelError("hello: missing daemon_protocol_version".into())
            })? as u32;

        if daemon_version != PROTOCOL_VERSION {
            tracing::warn!(
                client = PROTOCOL_VERSION,
                daemon = daemon_version,
                "daemon protocol mismatch"
            );
            return Err(TermError::DaemonProtocolMismatch {
                client: PROTOCOL_VERSION,
                daemon: daemon_version,
            });
        }

        let daemon_id = result
            .get("daemon_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        tracing::info!(daemon_id, daemon_version, "daemon hello ok");
        Ok(client)
    }

    /// 发送 Request 并等待 Response（30s 超时）。
    ///
    /// - 生成 id，构造 Request，写入 write_half
    /// - 在 pending 注册 oneshot::Sender
    /// - await oneshot::Receiver（带超时）
    /// - ok=true 返回 result；ok=false 返回 `ChannelError(error.message)`
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, TermError> {
        if self.inner.closed.load(Ordering::Relaxed) {
            return Err(TermError::SessionClosed(
                "daemon connection closed".into(),
            ));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = daemon_proto::Request {
            id,
            method: method.to_string(),
            params,
        };

        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().insert(id, tx);

        // 写入 Request（失败则清理 pending）
        // ChannelWriteHalf 不实现 AsyncWrite，用 data_bytes 发送 encode 后的字节
        {
            let w = self.inner.write.lock().await;
            let buf = daemon_proto::encode(&request);
            if let Err(e) = w.data_bytes(Bytes::from(buf)).await {
                self.inner.pending.lock().remove(&id);
                self.inner.closed.store(true, Ordering::Relaxed);
                return Err(TermError::ChannelError(format!("write: {e}")));
            }
        }

        // 等待 Response（30s 超时）
        let response = match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => {
                self.inner.pending.lock().remove(&id);
                return Err(TermError::ChannelError(
                    "response channel dropped (reader task exited?)".into(),
                ));
            }
            Err(_) => {
                self.inner.pending.lock().remove(&id);
                return Err(TermError::OperationTimeout);
            }
        };

        if response.ok {
            response.result.ok_or_else(|| {
                TermError::ChannelError("ok response missing result".into())
            })
        } else {
            let msg = response
                .error
                .map(|e| e.message)
                .unwrap_or_else(|| "unknown daemon error".into());
            Err(TermError::ChannelError(msg))
        }
    }

    /// session.create → 返回 session_id
    pub async fn session_create(
        &self,
        shell: &str,
        cwd: Option<&str>,
        pty_size: ProtoPtySize,
        name: Option<&str>,
    ) -> Result<String, TermError> {
        let params = serde_json::json!({
            "shell": shell,
            "cwd": cwd,
            "pty_size": { "rows": pty_size.rows, "cols": pty_size.cols },
            "name": name,
        });
        let result = self.call(methods::SESSION_CREATE, params).await?;
        let session_id = result
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TermError::ChannelError("session.create: missing session_id".into())
            })?
            .to_string();
        tracing::info!(session_id, "daemon session created");
        Ok(session_id)
    }

    /// session.attach → 启动 daemon 侧 event_pump + 返回 since_cursor 后的 buffer 快照。
    ///
    /// attach 成功后 daemon 开始向此连接推送 pty_data 事件（增量，自 cursor_end 起）。
    /// 返回的 `ReadResult.data` 为 attach 时刻 buffer 中 `[since_cursor, cursor_end]` 的
    /// base64 快照，调用方应优先消费此数据再读 pty_data 事件流，避免与 event_pump 推送竞态。
    pub async fn session_attach(
        &self,
        session_id: &str,
        since_cursor: u64,
    ) -> Result<ReadResult, TermError> {
        let params = serde_json::json!({
            "session_id": session_id,
            "since_cursor": since_cursor,
        });
        let result = self.call(methods::SESSION_ATTACH, params).await?;
        from_value(&result)
            .map_err(|e| TermError::ChannelError(format!("session.attach parse: {e}")))
    }

    /// session.send_input：base64 编码 data 后发送
    pub async fn session_send_input(
        &self,
        session_id: &str,
        data: &[u8],
    ) -> Result<(), TermError> {
        let data_b64 = BASE64_STANDARD.encode(data);
        let params = serde_json::json!({
            "session_id": session_id,
            "data": data_b64,
        });
        self.call(methods::SESSION_SEND_INPUT, params).await?;
        Ok(())
    }

    /// session.send_control
    pub async fn session_send_control(
        &self,
        session_id: &str,
        control: ProtoControlKey,
    ) -> Result<(), TermError> {
        let params = serde_json::json!({
            "session_id": session_id,
            "control": control,
        });
        self.call(methods::SESSION_SEND_CONTROL, params).await?;
        Ok(())
    }

    /// session.resize
    pub async fn session_resize(
        &self,
        session_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), TermError> {
        let params = serde_json::json!({
            "session_id": session_id,
            "rows": rows,
            "cols": cols,
        });
        self.call(methods::SESSION_RESIZE, params).await?;
        Ok(())
    }

    /// session.read_output → ReadResult（data 字段为 base64）
    pub async fn session_read_output(
        &self,
        session_id: &str,
        since_cursor: u64,
    ) -> Result<ReadResult, TermError> {
        let params = serde_json::json!({
            "session_id": session_id,
            "since_cursor": since_cursor,
        });
        let result = self.call(methods::SESSION_READ_OUTPUT, params).await?;
        from_value(&result)
            .map_err(|e| TermError::ChannelError(format!("session.read_output parse: {e}")))
    }

    /// session.close
    pub async fn session_close(&self, session_id: &str) -> Result<(), TermError> {
        let params = serde_json::json!({ "session_id": session_id });
        self.call(methods::SESSION_CLOSE, params).await?;
        tracing::info!(session_id, "daemon session closed");
        Ok(())
    }

    /// session.detach
    pub async fn session_detach(&self, session_id: &str) -> Result<(), TermError> {
        let params = serde_json::json!({ "session_id": session_id });
        self.call(methods::SESSION_DETACH, params).await?;
        tracing::info!(session_id, "daemon session detached");
        Ok(())
    }

    /// 订阅 pty_data 流。reader task 解码 pty_data 事件的 base64 data 后推入此 channel。
    ///
    /// 返回 `mpsc::Receiver<Bytes>`。daemon 断开时 reader task 清空 subscriber 列表，
    /// Receiver 收到 None（EOF 语义）。
    pub fn subscribe_pty_data(&self) -> mpsc::Receiver<Bytes> {
        let (tx, rx) = mpsc::channel(256);
        self.inner.pty_data_subscribers.lock().push(tx);
        rx
    }

    /// 订阅事件流（pty_exit / session_lost）。用于 PersistentTerminalHandle::read 检测 EOF。
    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.inner.event_tx.subscribe()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// reader task
// ───────────────────────────────────────────────────────────────────────────

/// reader task 主循环：独占 read_half，分发 Response / Event。
///
/// `ChannelReadHalf` 不实现 `AsyncRead`，用 `wait()` 读 `ChannelMsg::Data` 累积到缓冲区，
/// 再按 length-prefixed 协议解析完整消息。
///
/// - Response（有 "id" 字段）→ 从 pending 取 oneshot::Sender 发送
/// - Event（有 "event" 字段）：
///   - pty_data → base64 decode → 推入所有 pty_data subscriber
///   - pty_exit / session_lost → 推入 broadcast
/// - channel 关闭（Eof/Close/None）→ 标记 closed，清理所有 pending（发错误响应），
///   清空 subscriber（让 Receiver 收到 None）
async fn reader_loop(mut read_half: ReadHalf, inner: Arc<DaemonClientInner>) {
    tracing::debug!("daemon reader task started");
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        // 尝试从 buffer 解析所有完整消息
        loop {
            match try_parse_msg(&mut buffer) {
                Ok(Some(value)) => {
                    dispatch_msg(value, &inner).await;
                }
                Ok(None) => break, // buffer 不够，需要读更多
                Err(e) => {
                    tracing::warn!(error=%e, "reader: parse error, exiting");
                    handle_disconnect(&inner, format!("parse error: {e}"));
                    return;
                }
            }
        }

        // 读更多数据
        match read_half.wait().await {
            Some(ChannelMsg::Data { data }) => {
                buffer.extend_from_slice(&data);
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                // stderr 丢弃（proxy 进程的诊断输出）
                tracing::debug!(len = data.len(), "proxy stderr (discarded)");
            }
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                tracing::info!("daemon reader: proxy channel closed");
                handle_disconnect(&inner, "proxy channel closed");
                return;
            }
            Some(_) => continue,
        }
    }
}

/// 从缓冲区解析一条 length-prefixed JSON 消息。
///
/// - `Ok(Some(value))`：缓冲区有完整消息，已 drain 并返回
/// - `Ok(None)`：缓冲区数据不足，需要读更多
/// - `Err`：长度非法 / JSON 解析失败
fn try_parse_msg(buffer: &mut Vec<u8>) -> io::Result<Option<serde_json::Value>> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if len == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "消息长度为 0"));
    }
    if len > daemon_proto::MAX_MSG_LEN as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("消息长度 {} 超过上限 {}", len, daemon_proto::MAX_MSG_LEN),
        ));
    }
    if buffer.len() < 4 + len {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_slice(&buffer[4..4 + len])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    buffer.drain(..4 + len);
    Ok(Some(value))
}

/// 分发一条解析好的消息：Response（有 id）或 Event（有 event）。
async fn dispatch_msg(value: serde_json::Value, inner: &Arc<DaemonClientInner>) {
    if value.get("id").is_some() {
        // Response
        match serde_json::from_value::<Response>(value) {
            Ok(resp) => {
                if let Some(tx) = inner.pending.lock().remove(&resp.id) {
                    let _ = tx.send(resp);
                }
            }
            Err(e) => {
                tracing::warn!(error=%e, "reader: failed to parse Response");
            }
        }
    } else if value.get("event").is_some() {
        // Event
        match serde_json::from_value::<Event>(value) {
            Ok(ev) => handle_event(&ev, inner).await,
            Err(e) => {
                tracing::warn!(error=%e, "reader: failed to parse Event");
            }
        }
    } else {
        tracing::warn!("reader: message has neither id nor event field");
    }
}

/// reader task 断开时的清理：标记 closed，清理 pending（发错误响应），清空 subscriber。
fn handle_disconnect(inner: &Arc<DaemonClientInner>, reason: impl Into<String>) {
    inner.closed.store(true, Ordering::Relaxed);
    let reason = reason.into();

    // 清理所有 pending：发送错误响应让 call() 返回错误
    let pending = std::mem::take(&mut *inner.pending.lock());
    for (_, tx) in pending {
        let _ = tx.send(Response {
            id: 0,
            ok: false,
            result: None,
            error: Some(ErrorDetail::new("CONNECTION_CLOSED", reason.clone())),
        });
    }

    // 清空 subscriber：让 pty_data Receiver 收到 None（EOF）
    let subs = std::mem::take(&mut *inner.pty_data_subscribers.lock());
    drop(subs); // Sender drop → Receiver recv() 返回 None

    // broadcast 自动通知所有 receiver（Closed）
}

/// 处理 daemon 推送事件。
///
/// - pty_data：base64 decode data → 推入所有 pty_data subscriber（try_send 不阻塞）
/// - pty_exit / session_lost：推入 broadcast
async fn handle_event(ev: &Event, inner: &Arc<DaemonClientInner>) {
    match ev.event.as_str() {
        events::PTY_DATA => {
            if let Some(data_b64) = &ev.data {
                match BASE64_STANDARD.decode(data_b64) {
                    Ok(bytes) => {
                        let bytes = Bytes::from(bytes);
                        let subs = inner.pty_data_subscribers.lock();
                        for tx in subs.iter() {
                            // try_send：channel 满时丢弃，不阻塞 reader task
                            let _ = tx.try_send(bytes.clone());
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error=%e, "pty_data base64 decode failed");
                    }
                }
            }
        }
        events::PTY_EXIT | events::SESSION_LOST => {
            tracing::info!(
                event = %ev.event,
                session_id = %ev.session_id,
                exit_code = ?ev.exit_code,
                "daemon event"
            );
            let _ = inner.event_tx.send(ev.clone());
        }
        other => {
            tracing::debug!(event = other, "unknown daemon event, ignoring");
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// PersistentTerminalHandle —— TerminalHandle 实现
// ───────────────────────────────────────────────────────────────────────────

/// persistent session 句柄，通过 DaemonClient 读写远端 PTY。
///
/// `pty_data_rx` 和 `event_rx` 拆为独立 `TokioMutex`，让 `read()` 中 `select!`
/// 可同时持有两个 guard（同一 Mutex 不允许两个可变借用）。
///
/// - `read()`：select! 等 pty_data（返回 Some）或 pty_exit/session_lost/广播关闭（返回 None=EOF）
/// - `write()`：base64 编码 → `daemon.session_send_input`
/// - `send_control()`：domain ControlKey → proto ControlKey → `daemon.session_send_control`
/// - `resize()`：`daemon.session_resize`
/// - `close()`：幂等（AtomicBool），`daemon.session_close`
pub struct PersistentTerminalHandle {
    daemon: Arc<DaemonClient>,
    remote_session_id: String,
    pty_data_rx: TokioMutex<mpsc::Receiver<Bytes>>,
    event_rx: TokioMutex<broadcast::Receiver<Event>>,
    closed: AtomicBool,
    /// attach 时 buffer 的初始快照（`[0, cursor_end]`），`read()` 优先返回。
    ///
    /// 避免 attach 响应内联数据与 event_pump 推送的 pty_data 事件之间的竞态：
    /// 初始快照在 handle 内同步消费，event_pump 增量（`[cursor_end, ...]`）随后入队。
    initial_data: TokioMutex<Option<Bytes>>,
}

impl PersistentTerminalHandle {
    pub fn new(
        daemon: Arc<DaemonClient>,
        remote_session_id: String,
        pty_data_rx: mpsc::Receiver<Bytes>,
        initial_data: Option<Bytes>,
    ) -> Self {
        let event_rx = daemon.subscribe_events();
        Self {
            daemon,
            remote_session_id,
            pty_data_rx: TokioMutex::new(pty_data_rx),
            event_rx: TokioMutex::new(event_rx),
            closed: AtomicBool::new(false),
            initial_data: TokioMutex::new(initial_data),
        }
    }
}

#[async_trait]
impl TerminalHandle for PersistentTerminalHandle {
    /// 读 PTY output。None = PTY EOF（pty_exit / session_lost / daemon 断开）。
    async fn read(&self) -> Result<Option<Bytes>, TermError> {
        // 优先返回 attach 时的初始 buffer 快照（[0, cursor_end]）。
        // 先消费完 initial_data 再读 pty_data_rx，保证顺序：初始快照 → event_pump 增量。
        {
            let mut init = self.initial_data.lock().await;
            if let Some(data) = init.take() {
                if !data.is_empty() {
                    return Ok(Some(data));
                }
            }
        }
        let mut pty_rx = self.pty_data_rx.lock().await;
        let mut ev_rx = self.event_rx.lock().await;
        loop {
            tokio::select! {
                msg = pty_rx.recv() => {
                    match msg {
                        Some(bytes) => return Ok(Some(bytes)),
                        None => return Ok(None), // daemon 断开，subscriber 被清空
                    }
                }
                ev = ev_rx.recv() => {
                    match ev {
                        Ok(e) => {
                            if e.event == events::PTY_EXIT
                                || e.event == events::SESSION_LOST
                            {
                                return Ok(None);
                            }
                            // 其他事件继续等待
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(lag = n, "event broadcast lagged, continuing");
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // broadcast 关闭（DaemonClient 已 drop）→ EOF
                            return Ok(None);
                        }
                    }
                }
            }
        }
    }

    async fn write(&self, data: &[u8]) -> Result<(), TermError> {
        self.daemon
            .session_send_input(&self.remote_session_id, data)
            .await
    }

    async fn send_control(&self, c: ControlKey) -> Result<(), TermError> {
        let proto: ProtoControlKey = c.into();
        self.daemon
            .session_send_control(&self.remote_session_id, proto)
            .await
    }

    async fn resize(&self, size: PtySize) -> Result<(), TermError> {
        self.daemon
            .session_resize(&self.remote_session_id, size.rows, size.cols)
            .await
    }

    async fn close(&self) -> Result<(), TermError> {
        // 幂等：已关闭直接返回
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.daemon
            .session_close(&self.remote_session_id)
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ───────────────────────────────────────────────────────────────────────────
// PersistentProvider —— TerminalProvider 实现
// ───────────────────────────────────────────────────────────────────────────

/// persistent session provider。
///
/// - `persistent=false`：委托 `SshProvider::open`（Phase 1/2 Interactive 路径）
/// - `persistent=true`：走 daemon 路径（check_remote_runtime → deploy_runtime →
///   bootstrap_daemon → DaemonClient::connect → session_create）
pub struct PersistentProvider {
    ssh: SshProvider,
}

impl PersistentProvider {
    pub fn new(ssh: SshProvider) -> Self {
        Self { ssh }
    }
}

impl Default for PersistentProvider {
    fn default() -> Self {
        Self::new(SshProvider::new())
    }
}

#[async_trait]
impl TerminalProvider for PersistentProvider {
    async fn open(
        &self,
        request: OpenTerminalRequest,
    ) -> Result<Arc<dyn TerminalHandle>, TermError> {
        if !request.persistent {
            // 非 persistent：委托 SshProvider（Phase 1/2 Interactive 路径）
            return self.ssh.open(request).await;
        }

        let host = &request.host;
        tracing::info!(
            host = %host.name,
            pty_size = ?request.pty_size,
            name = ?request.name,
            "opening persistent session"
        );

        // 1. 检查远端 runtime 状态
        let state = self.check_remote_runtime(host).await?;
        if matches!(state, RemoteRuntimeState::Missing) {
            tracing::info!(host = %host.name, "remote runtime missing, deploying");
            self.deploy_runtime(host).await?;
        }

        // 2. bootstrap daemon（幂等：已运行则返回现有 socket）
        let socket_path = self.bootstrap_daemon(host).await?;
        tracing::info!(host = %host.name, socket_path, "daemon bootstrapped");

        // 3. 连接 daemon + hello 握手
        let daemon = DaemonClient::connect(&self.ssh, host, &socket_path).await?;

        // 4. 创建 session
        let remote_session_id = daemon
            .session_create(
                "/bin/bash",
                None,
                request.pty_size.into(),
                request.name.as_deref(),
            )
            .await?;

        // 5. 订阅 pty_data（attach 前订阅，确保 event_pump 启动后不漏增量推送）
        let pty_data_rx = daemon.subscribe_pty_data();

        // 6. attach：启动 daemon 侧 event_pump + 取初始 buffer 快照 [0, cursor_end]。
        //    未 attach 时 daemon 不会推送 pty_data 事件，read_output 会永久阻塞。
        let initial = daemon.session_attach(&remote_session_id, 0).await?;
        let initial_data = match BASE64_STANDARD.decode(&initial.data) {
            Ok(bytes) if !bytes.is_empty() => Some(Bytes::from(bytes)),
            _ => None,
        };
        tracing::info!(
            session_id = %remote_session_id,
            cursor_end = initial.cursor_end,
            initial_bytes = initial_data.as_ref().map(|b| b.len()).unwrap_or(0),
            "daemon session attached"
        );

        // 7. 返回 handle（initial_data 优先于 pty_data_rx 被消费）
        let handle = Arc::new(PersistentTerminalHandle::new(
            daemon,
            remote_session_id,
            pty_data_rx,
            initial_data,
        )) as Arc<dyn TerminalHandle>;
        Ok(handle)
    }
}

// ───────────────────────────────────────────────────────────────────────────
// PersistentProvider 辅助方法
// ───────────────────────────────────────────────────────────────────────────

impl PersistentProvider {
    /// 远端路径约定（ADR-0004 §2）
    const REMOTE_BIN: &'static str = "~/.local/share/termbridge/termbridge-agentd";
    const REMOTE_VERSION: &'static str = "~/.local/share/termbridge/agentd.version";

    /// 探测远端 runtime 状态。
    ///
    /// 1. `test -x <bin> && cat <version>` → 失败 = Missing；成功 = 二进制存在
    /// 2. `pgrep -f termbridge-agentd` → 有输出 = Running；无输出/失败 = Stopped
    async fn check_remote_runtime(&self, host: &Host) -> Result<RemoteRuntimeState, TermError> {
        // 1. 检查二进制 + version 文件
        let bin_check = self
            .ssh
            .exec(
                host,
                &format!(
                    "test -x {} && cat {}",
                    Self::REMOTE_BIN,
                    Self::REMOTE_VERSION
                ),
            )
            .await;
        if bin_check.is_err() {
            tracing::info!(host = %host.name, "remote runtime: missing (binary/version not found)");
            return Ok(RemoteRuntimeState::Missing);
        }

        // 2. 检查 daemon 进程是否运行
        let pgrep = self.ssh.exec(host, "pgrep -f termbridge-agentd").await;
        let running = matches!(pgrep, Ok(out) if !out.trim().is_empty());
        if running {
            tracing::info!(host = %host.name, "remote runtime: running");
            Ok(RemoteRuntimeState::Running)
        } else {
            tracing::info!(host = %host.name, "remote runtime: stopped");
            Ok(RemoteRuntimeState::Stopped)
        }
    }

    /// 部署远端 runtime：SFTP 上传 agentd 二进制 + 写 version 文件。
    ///
    /// 流程：
    /// 1. 检查本地 agentd 二进制存在（`local_agentd_path()`），不存在 → `RuntimeMissing`
    /// 2. SSH exec `mkdir -p <remote_dir>`
    /// 3. SFTP upload 本地二进制 → 远端 `<remote_bin>`
    ///    （通过临时 SSH session + `SshTerminalHandle::open_sftp_provider`）
    /// 4. SSH exec `chmod +x <remote_bin>`
    /// 5. SSH exec 写 version 文件
    /// 6. 任一步失败 → `RuntimeDeployFailed`
    async fn deploy_runtime(&self, host: &Host) -> Result<(), TermError> {
        let local_path = Self::local_agentd_path();
        if !local_path.exists() {
            return Err(TermError::RuntimeMissing(format!(
                "local agentd binary not found: {}",
                local_path.display()
            )));
        }

        tracing::info!(
            host = %host.name,
            local = %local_path.display(),
            "deploying remote runtime"
        );

        // 获取远端 home 目录（SFTP 路径需绝对路径，不展开 ~）
        let home = self
            .ssh
            .exec(host, "echo $HOME")
            .await?
            .trim()
            .to_string();
        let remote_dir = format!("{home}/.local/share/termbridge");
        let remote_bin = format!("{remote_dir}/termbridge-agentd");
        let remote_version = format!("{remote_dir}/agentd.version");

        // mkdir
        self.ssh
            .exec(host, &format!("mkdir -p {remote_dir}"))
            .await?;

        // SFTP upload（开临时 SSH session，复用 SshTerminalHandle::open_sftp_provider）
        let temp_req = OpenTerminalRequest {
            host: host.clone(),
            pty_size: PtySize::default(),
            persistent: false,
            name: None,
        };
        let handle = self.ssh.open(temp_req).await?;
        let deploy_result: Result<(), TermError> = async {
            let any = handle.as_any();
            let ssh_handle = any
                .downcast_ref::<SshTerminalHandle>()
                .ok_or_else(|| {
                    TermError::ChannelError(
                        "downcast to SshTerminalHandle failed for SFTP upload".into(),
                    )
                })?;
            let sftp = ssh_handle.open_sftp_provider().await?;
            sftp.upload(&local_path, &remote_bin).await?;
            sftp.close().await.ok();
            Ok(())
        }
        .await;
        // 无论上传成功与否都 close 临时 session
        let _ = handle.close().await;
        deploy_result?;

        // chmod +x
        self.ssh
            .exec(host, &format!("chmod +x {remote_bin}"))
            .await?;

        // 写 version 文件
        self.ssh
            .exec(
                host,
                &format!(
                    "echo '{{\"protocol_version\":{},\"build\":\"{}\"}}' > {}",
                    PROTOCOL_VERSION, BUILD_VERSION, remote_version
                ),
            )
            .await?;

        tracing::info!(host = %host.name, "remote runtime deployed");
        Ok(())
    }

    /// 启动 daemon（幂等），返回 socket_path。
    ///
    /// 1. 计算默认 socket 路径：`${XDG_RUNTIME_DIR:-$HOME/.local/share/termbridge}/termbridge.sock`
    /// 2. 执行 `termbridge-agentd bootstrap --sock <path>`（幂等：已运行则返回现有信息）
    /// 3. 解析 stdout JSON：`{ daemon_id, socket, protocol_version, build }`
    /// 4. 返回 socket path（优先 JSON 中的 socket 字段，回退到计算的路径）
    async fn bootstrap_daemon(&self, host: &Host) -> Result<String, TermError> {
        // 计算默认 socket 路径
        let default_socket = self
            .ssh
            .exec(
                host,
                "echo ${XDG_RUNTIME_DIR:-$HOME/.local/share/termbridge}/termbridge.sock",
            )
            .await?
            .trim()
            .to_string();

        // 执行 bootstrap
        let stdout = self
            .ssh
            .exec(
                host,
                &format!("{} bootstrap --sock {}", Self::REMOTE_BIN, default_socket),
            )
            .await?;

        // 解析 JSON 响应
        let json: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|e| {
            TermError::ChannelError(format!("parse bootstrap response: {e}"))
        })?;

        let socket = json
            .get("socket")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or(default_socket);

        let daemon_id = json
            .get("daemon_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        tracing::info!(host = %host.name, daemon_id, socket = %socket, "daemon bootstrapped");
        Ok(socket)
    }

    /// 本地 agentd 二进制路径：`%LOCALAPPDATA%\TermBridge\agentd\termbridge-agentd`
    ///
    /// Windows 环境变量 LOCALAPPDATA。不存在时返回空基路径（后续 exists() 检查会失败）。
    fn local_agentd_path() -> PathBuf {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_default();
        PathBuf::from(base)
            .join("TermBridge")
            .join("agentd")
            .join("termbridge-agentd")
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 单元测试
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_agentd_path_uses_localappdata() {
        // 设置临时 LOCALAPPDATA，验证路径拼接
        let saved = std::env::var_os("LOCALAPPDATA");
        std::env::set_var("LOCALAPPDATA", "/tmp/fake_localappdata");

        let path = PersistentProvider::local_agentd_path();
        assert!(
            path.to_string_lossy().contains("TermBridge"),
            "path should contain TermBridge, got: {}",
            path.display()
        );
        assert!(
            path.to_string_lossy().contains("agentd"),
            "path should contain agentd, got: {}",
            path.display()
        );
        assert!(
            path.to_string_lossy()
                .ends_with("termbridge-agentd"),
            "path should end with termbridge-agentd, got: {}",
            path.display()
        );

        match saved {
            Some(v) => std::env::set_var("LOCALAPPDATA", v),
            None => std::env::remove_var("LOCALAPPDATA"),
        }
    }

    #[test]
    fn local_agentd_path_handles_missing_env() {
        // LOCALAPPDATA 不存在时返回相对路径（exists() 会 false → RuntimeMissing）
        let saved = std::env::var_os("LOCALAPPDATA");
        std::env::remove_var("LOCALAPPDATA");

        let path = PersistentProvider::local_agentd_path();
        assert!(
            path.to_string_lossy().ends_with("termbridge-agentd"),
            "path should still end with termbridge-agentd, got: {}",
            path.display()
        );

        match saved {
            Some(v) => std::env::set_var("LOCALAPPDATA", v),
            None => std::env::remove_var("LOCALAPPDATA"),
        }
    }

    #[test]
    fn base64_encode_decode_roundtrip_for_pty_data() {
        // 验证 session_send_input / pty_data 使用的 base64 编解码
        let input = b"hello \x00 world \xff\xe9";
        let encoded = BASE64_STANDARD.encode(input);
        let decoded = BASE64_STANDARD.decode(&encoded).unwrap();
        assert_eq!(decoded, input);
    }

    #[test]
    fn control_key_domain_to_proto_conversion() {
        // 验证 PersistentTerminalHandle::send_control 的类型转换
        let cases = [
            (ControlKey::CtrlC, ProtoControlKey::CtrlC),
            (ControlKey::CtrlD, ProtoControlKey::CtrlD),
            (ControlKey::CtrlZ, ProtoControlKey::CtrlZ),
            (ControlKey::Tab, ProtoControlKey::Tab),
            (ControlKey::Enter, ProtoControlKey::Enter),
            (ControlKey::Escape, ProtoControlKey::Escape),
        ];
        for (domain, expected) in cases {
            let proto: ProtoControlKey = domain.into();
            assert_eq!(proto.as_bytes(), expected.as_bytes());
        }
    }

    #[test]
    fn pty_size_domain_to_proto_conversion() {
        // 验证 session_create 的 pty_size 转换
        let domain = PtySize { rows: 40, cols: 120 };
        let proto: ProtoPtySize = domain.into();
        assert_eq!(proto.rows, 40);
        assert_eq!(proto.cols, 120);
    }

    #[test]
    fn persistent_provider_default_constructs() {
        // Default trait 应能构造（内部 new SshProvider::default）
        let _provider = PersistentProvider::default();
    }
}
