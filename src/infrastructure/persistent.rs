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
//!         2. Missing → deploy_runtime(host)（SFTP 上传 agentd 二进制 + version 文件）；
//!            NeedsUpgrade → deploy_runtime(host) + stop_remote_daemon(host)：
//!            升级部署后旧 daemon 仍在运行旧二进制，必须先停掉，随后的
//!            bootstrap_daemon 才会以新二进制 spawn 全新 serve 进程
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
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
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
    PtySize as ProtoPtySize, ReadResult, Response, SessionInfo, BUILD_VERSION, PROTOCOL_VERSION,
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
/// - `NeedsUpgrade`：二进制已部署但 version 文件的 build 与客户端 BUILD_VERSION
///   不一致（或内容无法解析）——远端 agentd 需随客户端升级重新部署
/// - `Stopped`：二进制已部署且版本一致，但 daemon 进程未运行（`pgrep` 无输出）
/// - `Running`：daemon 进程已在运行（`pgrep` 有输出）
#[derive(Debug)]
enum RemoteRuntimeState {
    Missing,
    NeedsUpgrade,
    Stopped,
    Running,
}

/// check_remote_runtime 的探测结果：状态 + version 文件中记录的旧 build。
struct RemoteRuntimeProbe {
    state: RemoteRuntimeState,
    /// version 文件中的远端 build（Missing / 内容无法解析时为 None）。
    /// 升级路径用它打 "agentd 已升级 x→y" 日志 / 填充 restart 报告的 version_before。
    remote_build: Option<String>,
}

// ───────────────────────────────────────────────────────────────────────────
// 远端 daemon 停止 / 重启结果类型
// ───────────────────────────────────────────────────────────────────────────

/// stop_remote_daemon 的结果。
///
/// 用 `status` 标签区分"daemon 本来就没在运行"（`not_running`，未执行任何
/// kill——包括 pid 文件缺失、内容非法、pid 进程已消失、pid 被其他进程复用
/// 等安全分支）与"daemon 已停止"（`stopped`），调用方不应把前者当失败。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RemoteDaemonStopResult {
    /// daemon 未在运行，未执行 kill。`reason` 为诊断信息（中文，进日志）。
    NotRunning { reason: String },
    /// daemon 进程已停止。
    Stopped {
        /// 被停止的 daemon pid（来自 pid 文件且 comm 校验通过）
        pid: u32,
        /// SIGTERM 宽限期内未退出、动用了 kill -9 兜底
        forced: bool,
    },
}

/// restart_remote_daemon 的结构化报告（MCP 工具返回体）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RemoteDaemonRestartReport {
    /// 本次是否停止了正在运行的旧 daemon
    pub stopped: bool,
    /// 重启前 daemon 是否在运行（= stopped，成功路径下二者同值）
    pub was_running: bool,
    /// 本次调用是否部署了 agentd 二进制（Missing / NeedsUpgrade 触发）
    pub deployed: bool,
    /// 部署前 version 文件记录的远端 build（缺失 / 无法解析时省略）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_before: Option<String>,
    /// 重启后 hello 握手返回的 daemon build（未真正重启时即旧 daemon 的 build）
    pub version_after: String,
    /// 是否 spawn 了全新 daemon 进程；false = bootstrap 复用了仍在运行的旧 daemon
    /// （如 pid 文件丢失导致 stop 判定"未运行"），此时升级尚未生效
    pub restarted: bool,
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
    /// hello 握手返回的 daemon build（agentd 编译期 BUILD_VERSION）。
    ///
    /// reader task 持有 inner 的 clone，hello 又只能在 connect 后期完成，
    /// 用 OnceLock 允许握手后一次性写入（restart_remote_daemon 据此报告
    /// version_after，判断升级是否真正生效）。
    daemon_build: OnceLock<String>,
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
            daemon_build: OnceLock::new(),
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
        // 记录 daemon build（restart_remote_daemon 的 version_after 数据源）
        let _ = client
            .inner
            .daemon_build
            .set(result.get("daemon_build").and_then(|v| v.as_str()).unwrap_or("?").to_string());
        tracing::info!(daemon_id, daemon_version, "daemon hello ok");
        Ok(client)
    }

    /// daemon hello 握手返回的 build（未握手/字段缺失时为 "?"）。
    pub fn daemon_build(&self) -> &str {
        self.inner.daemon_build.get().map(|s| s.as_str()).unwrap_or("?")
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
                return Err(TermError::OperationTimeout {
                    operation: method.to_string(),
                    session_id: None,
                });
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

    /// session.list → 返回 daemon 侧所有 session 的信息（含 detached 的）
    pub async fn session_list(&self) -> Result<Vec<SessionInfo>, TermError> {
        let result = self.call(methods::SESSION_LIST, serde_json::json!({})).await?;
        let sessions = result.get("sessions").ok_or_else(|| {
            TermError::ChannelError("session.list: missing sessions field".into())
        })?;
        from_value(sessions)
            .map_err(|e| TermError::ChannelError(format!("session.list parse: {e}")))
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

    /// detach：调 daemon.session_detach（远端 PTY 保活），不调 session_close。
    /// 调用后 handle 应被 drop，daemon 侧 session 转 Detached，供后续 attach 重连。
    /// 幂等：已 closed/detached 直接返回。
    pub async fn detach(&self) -> Result<(), TermError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.daemon
            .session_detach(&self.remote_session_id)
            .await
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

        // ADR-0017 §2.3：password + persistent 不支持。persistent runtime 的
        // check/deploy/bootstrap/exec 每一步都开新 SSH 连接，依赖 key-based
        // unattended auth；SessionManager 已在弹密码前校验，这里是双保险。
        if request.password.is_some() {
            return Err(TermError::InvalidArgument(
                "auth=password with session=persistent is not supported: password \
                 authentication is supported for standard SSH sessions; persistent \
                 sessions require key-based unattended SSH authentication. Use \
                 auth=password + session=standard, or auth=key + session=persistent \
                 (ADR-0017 §2.3)"
                    .into(),
            ));
        }

        let host = &request.host;
        tracing::info!(
            host = %host.name,
            pty_size = ?request.pty_size,
            name = ?request.name,
            "opening persistent session"
        );

        // 1. 检查远端 runtime 状态
        let probe = self.check_remote_runtime(host).await?;
        if matches!(
            probe.state,
            RemoteRuntimeState::Missing | RemoteRuntimeState::NeedsUpgrade
        ) {
            let old_build = probe.remote_build.clone().unwrap_or_else(|| "?".into());
            tracing::info!(host = %host.name, "remote runtime missing or outdated, deploying");
            self.deploy_runtime(host).await?;

            // 升级部署（NeedsUpgrade）语义：mv -f 原子替换只换磁盘上的二进制，
            // 旧 daemon 进程仍在内存里跑旧版本、继续服务——必须先停掉它，下面的
            // bootstrap_daemon 才会以新二进制 spawn 全新 serve 进程，升级在同一次
            // open 流程内生效。state == Missing（首次部署）时远端从未有过 daemon，
            // 无需 stop。
            //
            // 注意：stop 失败会让本次 open 显式失败（宁可报错也不静默跑旧版本）。
            // 此时 version 文件已写入新 build，后续 open 不会再触发升级路径，
            // 须调用 restart_remote_daemon 工具显式重启使升级生效。
            // 另：停 daemon 会让本 MCP 进程内所有连到该 daemon 的 session 变
            // Lost（proxy channel 断开）——升级语义本身即如此。
            if should_stop_daemon_after_deploy(&probe.state) {
                match self.stop_remote_daemon(host).await? {
                    RemoteDaemonStopResult::Stopped { pid, forced } => {
                        tracing::info!(
                            host = %host.name,
                            from = %old_build,
                            to = BUILD_VERSION,
                            pid,
                            forced,
                            "agentd 已升级 {old_build}→{BUILD_VERSION}，旧 daemon（pid {pid}）已停止，bootstrap 将启动新 daemon"
                        );
                    }
                    RemoteDaemonStopResult::NotRunning { reason } => {
                        tracing::info!(
                            host = %host.name,
                            from = %old_build,
                            to = BUILD_VERSION,
                            reason,
                            "agentd 已升级 {old_build}→{BUILD_VERSION}，旧 daemon 未在运行，跳过停止"
                        );
                    }
                }
            }
        }

        // 2. bootstrap daemon（幂等：已运行则返回现有 socket）
        let socket_path = self.bootstrap_daemon(host).await?.socket;
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

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ───────────────────────────────────────────────────────────────────────────
// PersistentProvider 辅助方法
// ───────────────────────────────────────────────────────────────────────────

impl PersistentProvider {
    /// 远端路径约定（ADR-0004 §2）
    const REMOTE_BIN: &'static str = "~/.local/share/termbridge/termbridge-agentd";
    const REMOTE_VERSION: &'static str = "~/.local/share/termbridge/agentd.version";
    /// 远端 daemon pid 文件：agentd/src/main.rs `default_pid_path()` 固定写
    /// `$HOME/.local/share/termbridge/agentd.pid`（与 socket 不同，不受
    /// XDG_RUNTIME_DIR 影响）。bootstrap spawn serve 时写入。
    const REMOTE_PID_FILE: &'static str = "~/.local/share/termbridge/agentd.pid";
    /// kill（SIGTERM）后的宽限秒数：远端 shell 每 1s 轮询一次 `kill -0`，
    /// 超过仍未退出 → 升级 `kill -9`。agentd 未注册任何信号处理，SIGTERM
    /// 即默认终止，正常情况下远小于该上限。
    const DAEMON_TERM_GRACE_SECS: u64 = 5;

    /// 列出远端 daemon 上的所有 session（含 detached 的，用于跨 MCP 重启重连）。
    ///
    /// 流程：bootstrap_daemon（幂等）→ connect → session.list。
    /// daemon drop 时 SSH proxy channel 关闭。
    pub async fn list_remote_sessions(
        &self,
        host: &Host,
    ) -> Result<Vec<SessionInfo>, TermError> {
        let socket_path = self.bootstrap_daemon(host).await?.socket;
        let daemon = DaemonClient::connect(&self.ssh, host, &socket_path).await?;
        daemon.session_list().await
    }

    /// attach 到远端已有的 session（跨 MCP 重启重连）。
    ///
    /// 流程（复用 open 的 handle 构造逻辑）：
    /// 1. bootstrap_daemon → connect
    /// 2. subscribe_pty_data（attach 前订阅，避免 event_pump 启动后漏增量）
    /// 3. session_attach(remote_session_id, 0) → 初始 buffer 快照
    /// 4. PersistentTerminalHandle::new
    ///
    /// 远端 session 必须已存在（由之前 open_session persistent=true 创建，可能已 detached）。
    pub async fn attach_remote_session(
        &self,
        host: &Host,
        remote_session_id: &str,
    ) -> Result<Arc<dyn TerminalHandle>, TermError> {
        let socket_path = self.bootstrap_daemon(host).await?.socket;
        let daemon = DaemonClient::connect(&self.ssh, host, &socket_path).await?;

        // 订阅 pty_data（attach 前订阅，确保 event_pump 启动后不漏增量推送）
        let pty_data_rx = daemon.subscribe_pty_data();

        // attach：启动 daemon 侧 event_pump + 取初始 buffer 快照 [0, cursor_end]
        let initial = daemon.session_attach(remote_session_id, 0).await?;
        let initial_data = match BASE64_STANDARD.decode(&initial.data) {
            Ok(bytes) if !bytes.is_empty() => Some(Bytes::from(bytes)),
            _ => None,
        };
        tracing::info!(
            remote_session_id,
            cursor_end = initial.cursor_end,
            initial_bytes = initial_data.as_ref().map(|b| b.len()).unwrap_or(0),
            "daemon session re-attached"
        );

        let handle = Arc::new(PersistentTerminalHandle::new(
            daemon,
            remote_session_id.to_string(),
            pty_data_rx,
            initial_data,
        )) as Arc<dyn TerminalHandle>;
        Ok(handle)
    }

    /// 探测远端 runtime 状态。
    ///
    /// 1. `test -x <bin> && cat <version>` → 失败 = Missing；成功后比对 version
    ///    文件的 build 字段与客户端 BUILD_VERSION，不一致/无法解析 = NeedsUpgrade
    /// 2. `pgrep -f termbridge-agentd` → 有输出 = Running；无输出/失败 = Stopped
    ///
    /// 返回 `RemoteRuntimeProbe`：状态 + version 文件中的旧 build（升级日志 /
    /// restart 报告的 version_before 用；Missing 或内容无法解析时为 None）。
    async fn check_remote_runtime(&self, host: &Host) -> Result<RemoteRuntimeProbe, TermError> {
        // 1. 检查二进制 + version 文件
        let version_out = match self
            .ssh
            .exec(
                host,
                &format!(
                    "test -x {} && cat {}",
                    Self::REMOTE_BIN,
                    Self::REMOTE_VERSION
                ),
            )
            .await
        {
            Ok(out) => out,
            Err(_) => {
                tracing::info!(host = %host.name, "remote runtime: missing (binary/version not found)");
                return Ok(RemoteRuntimeProbe {
                    state: RemoteRuntimeState::Missing,
                    remote_build: None,
                });
            }
        };

        // 2. 版本比对：远端 build 与客户端 BUILD_VERSION 不一致（或 version 文件
        //    内容无法解析）→ NeedsUpgrade，由调用方重新部署。早期实现只检查文件
        //    存在性，远端 agentd 版本落后时永远不会升级。
        let remote_build = parse_remote_build(&version_out);
        if remote_build.as_deref() != Some(BUILD_VERSION) {
            tracing::info!(
                host = %host.name,
                remote_build = ?remote_build,
                "remote runtime: version mismatch/unknown, needs upgrade"
            );
            return Ok(RemoteRuntimeProbe {
                state: RemoteRuntimeState::NeedsUpgrade,
                remote_build,
            });
        }

        // 3. 检查 daemon 进程是否运行
        let pgrep = self.ssh.exec(host, "pgrep -f termbridge-agentd").await;
        let running = matches!(pgrep, Ok(out) if !out.trim().is_empty());
        if running {
            tracing::info!(host = %host.name, "remote runtime: running");
            Ok(RemoteRuntimeProbe {
                state: RemoteRuntimeState::Running,
                remote_build,
            })
        } else {
            tracing::info!(host = %host.name, "remote runtime: stopped");
            Ok(RemoteRuntimeProbe {
                state: RemoteRuntimeState::Stopped,
                remote_build,
            })
        }
    }

    /// 部署远端 runtime：SFTP 上传 agentd 二进制 + 写 version 文件（原子替换）。
    ///
    /// 流程：
    /// 1. 检查本地 agentd 二进制存在（`local_agentd_path()`），不存在 → `RuntimeMissing`
    /// 2. SSH exec `mkdir -p <remote_dir>`
    /// 3. SFTP upload 本地二进制 → 远端 `<remote_bin>.termbridge-tmp`
    ///    （同目录临时文件；通过临时 SSH session + `SshTerminalHandle::open_sftp_provider`）
    /// 4. SSH exec `chmod 0755 <tmp>` + `mv -f <tmp> <remote_bin>`（同目录 → 同一
    ///    文件系统 → rename(2) 原子替换；直接上传到最终路径被中断会留下"可执行
    ///    但截断"的二进制，且 check_remote_runtime 不再报 Missing → 主机永久损坏
    ///    需手工清理）
    /// 5. SSH exec 写 version 文件（在成功替换之后写，保证 version 与实际二进制一致）
    /// 6. 任一步失败 → `RuntimeDeployFailed`（尽力清理 tmp 残留）
    async fn deploy_runtime(&self, host: &Host) -> Result<(), TermError> {
        // 首次使用自动从发布包内置 resources/agentd 自举到本地缓存（wrapper/平台包
        // 均保持 exe 同目录布局，故可稳定解析）；都不可用才报 RuntimeMissing。
        // 错误信息必须自包含（What/Why/Fix）：消费方是无 agentd 记忆的 agent 会话，
        // 不能假设它知道 agentd 是什么、装在哪、怎么补（用户反馈：新会话无法部署持久会话）
        let Some(local_path) = Self::ensure_local_agentd() else {
            // current_exe() 失败的极端情况下回退到相对描述，保证信息仍可读
            let bundled = Self::bundled_agentd_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| {
                    "<exe dir>/resources/agentd/linux-x86_64/termbridge-agentd".to_string()
                });
            return Err(TermError::RuntimeMissing(format!(
                "persistent sessions (open_session with persistent=true) require the TermBridge \
                 agentd daemon binary (Linux x86_64) to be available locally so it can be \
                 auto-deployed to the remote host, but it was not found. Checked locations: \
                 bundled copy at {bundled} and local cache at {}. This usually means termbridge \
                 was installed by a method that does not bundle agentd (npm package without \
                 resources/, cargo install, or a dev build). Fix (pick one): (1) download a \
                 release archive from https://github.com/summerxzp/TermBridge/releases, which \
                 bundles agentd under resources/; (2) if installed via npm \
                 @summerxzp/termbridge-mcp, reinstall it and verify the platform package \
                 contains resources/agentd; (3) for dev builds, build on/for Linux with \
                 `cargo build --release -p termbridge-agentd` and copy the binary to the \
                 local cache path above. Standard sessions (persistent=false) work without \
                 agentd.",
                Self::local_agentd_path().display()
            )));
        };

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
        // 原子部署的临时文件：与最终路径同目录（同一文件系统，mv 才是 rename(2)）
        let remote_tmp = format!("{remote_bin}.termbridge-tmp");

        // mkdir
        self.ssh
            .exec(host, &format!("mkdir -p {remote_dir}"))
            .await?;

        // SFTP upload → tmp 文件（开临时 SSH session，复用 SshTerminalHandle::open_sftp_provider）
        let temp_req = OpenTerminalRequest {
            host: host.clone(),
            pty_size: PtySize::default(),
            persistent: false,
            name: None,
            password: None,
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
            sftp.upload(&local_path, &remote_tmp).await?;
            sftp.close().await.ok();
            Ok(())
        }
        .await;
        // 无论上传成功与否都 close 临时 session
        let _ = handle.close().await;
        if let Err(e) = deploy_result {
            // 上传失败：清理半成品 tmp（尽力而为）
            let _ = self.ssh.exec(host, &format!("rm -f {remote_tmp}")).await;
            return Err(e);
        }

        // chmod 0755 + 原子替换（同目录 mv → rename(2)）：最终路径要么是完整的
        // 旧版本，要么是完整的新版本，不会出现截断的可执行文件
        let finalize = async {
            self.ssh
                .exec(host, &format!("chmod 0755 {remote_tmp}"))
                .await?;
            self.ssh
                .exec(host, &format!("mv -f {remote_tmp} {remote_bin}"))
                .await?;
            Ok::<(), TermError>(())
        }
        .await;
        if let Err(e) = finalize {
            // 改名失败：清理 tmp（尽力而为，避免遗留垃圾文件）
            let _ = self.ssh.exec(host, &format!("rm -f {remote_tmp}")).await;
            return Err(e);
        }

        // 写 version 文件（成功替换之后，保证 version 与实际二进制一致）
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

    /// 计算远端默认 socket 路径（与 agentd/src/main.rs `default_socket_path`
    /// 的解析规则一致，远端 shell 求值）。
    async fn remote_socket_path(&self, host: &Host) -> Result<String, TermError> {
        Ok(self
            .ssh
            .exec(
                host,
                "echo ${XDG_RUNTIME_DIR:-$HOME/.local/share/termbridge}/termbridge.sock",
            )
            .await?
            .trim()
            .to_string())
    }

    /// 启动 daemon（幂等），返回 socket 路径 + daemon_id。
    ///
    /// 1. 计算默认 socket 路径：`${XDG_RUNTIME_DIR:-$HOME/.local/share/termbridge}/termbridge.sock`
    /// 2. 执行 `termbridge-agentd bootstrap --sock <path>`（幂等：已运行则返回现有信息）
    /// 3. 解析 stdout JSON：`{ daemon_id, socket, protocol_version, build }`
    ///
    /// `daemon_id` 供调用方判断是否真的 spawn 了新 daemon：daemon 已在运行时
    /// agentd bootstrap 返回字面量 `"existing"`，否则为新生成的 daemon id。
    async fn bootstrap_daemon(&self, host: &Host) -> Result<DaemonBootstrapInfo, TermError> {
        // 计算默认 socket 路径
        let default_socket = self.remote_socket_path(host).await?;

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
        Ok(DaemonBootstrapInfo { socket, daemon_id })
    }

    /// 停止远端 daemon（SSH exec，与 check_remote_runtime 同管道；restart_remote_daemon
    /// 工具与 open() 升级路径共用）。
    ///
    /// 绝不盲杀——pid 文件会过期、pid 会被复用，kill 前必须校验进程身份：
    /// 1. 读 pid 文件（`~/.local/share/termbridge/agentd.pid`）——缺失/内容非法
    ///    → `NotRunning`（daemon 从未启动，或从未记录 pid）
    /// 2. 校验 pid 身份：`/proc/<pid>/comm`（回退 `ps -p <pid> -o comm=`）必须是
    ///    agentd —— 进程已消失 → `NotRunning`（pid 文件过期）；comm 不符（pid 被
    ///    其他进程复用）→ `NotRunning` 并拒绝 kill
    /// 3. `kill <pid>`（SIGTERM），远端 shell 每 1s 轮询 `kill -0`，至多
    ///    `DAEMON_TERM_GRACE_SECS` 秒；仍未退出 → `kill -9` 兜底并复核
    /// 4. `rm -f` 残留 socket 文件（尽力而为；新 daemon 的 serve 本也会 unlink
    ///    stale socket，见 agentd/src/rpc.rs `RpcServer::serve`）
    ///
    /// 错误语义：步骤 1/2 中 SSH 连接级故障与"文件/进程不存在"无法区分，统一按
    /// `NotRunning` 处理（安全方向：不做任何 kill），随后的 bootstrap 会以连接
    /// 错误显式失败；kill 阶段的故障（SSH 失败 / SIGKILL 后进程仍存活 / 输出缺
    /// 标记）→ `Err(TermError)`，调用方不应假定 daemon 已停止。
    pub async fn stop_remote_daemon(
        &self,
        host: &Host,
    ) -> Result<RemoteDaemonStopResult, TermError> {
        tracing::info!(host = %host.name, "stopping remote daemon");

        // 1. 读 pid 文件
        let pid = match self
            .ssh
            .exec(
                host,
                &format!("cat {} 2>/dev/null", Self::REMOTE_PID_FILE),
            )
            .await
        {
            Ok(out) => match parse_pid_file(&out) {
                Some(pid) => pid,
                None => {
                    let reason = "pid 文件内容为空或非法".to_string();
                    tracing::info!(host = %host.name, reason, "remote daemon not running");
                    return Ok(RemoteDaemonStopResult::NotRunning { reason });
                }
            },
            Err(_) => {
                // cat 失败 = pid 文件不存在（含 SSH 连接级故障，见函数注释）
                let reason = "pid 文件不存在（daemon 从未启动或已被清理）".to_string();
                tracing::info!(host = %host.name, reason, "remote daemon not running");
                return Ok(RemoteDaemonStopResult::NotRunning { reason });
            }
        };

        // 2. 校验 pid 身份：comm 必须是 agentd
        let comm_out = self
            .ssh
            .exec(
                host,
                &format!(
                    "cat /proc/{pid}/comm 2>/dev/null || ps -p {pid} -o comm= 2>/dev/null"
                ),
            )
            .await;
        match comm_out {
            Ok(out) if is_agentd_comm(&out) => {}
            Ok(out) => {
                // pid 被其他进程复用：绝不能 kill
                let comm = out.trim().to_string();
                let reason = format!("pid {pid} 已被其他进程复用（comm={comm:?}），拒绝 kill");
                tracing::warn!(host = %host.name, pid, comm, "remote daemon: pid reused, refusing to kill");
                return Ok(RemoteDaemonStopResult::NotRunning { reason });
            }
            Err(_) => {
                // 进程不存在（或 SSH 故障，安全方向按未运行处理）
                let reason = format!("pid {pid} 对应进程不存在（pid 文件过期）");
                tracing::info!(host = %host.name, pid, "remote daemon not running (stale pid file)");
                return Ok(RemoteDaemonStopResult::NotRunning { reason });
            }
        }

        // 3. SIGTERM + 有界等待。用 stdout 标记（GONE/ALIVE）区分结果，避免解析
        //    exec 的 exit-code 错误文本；命令整体 exit 0，输出必带标记。
        let grace_seq: Vec<String> = (1..=Self::DAEMON_TERM_GRACE_SECS)
            .map(|i| i.to_string())
            .collect();
        let term_cmd = format!(
            "kill {pid} 2>/dev/null; for i in {seq}; do sleep 1; kill -0 {pid} 2>/dev/null || {{ echo GONE; exit 0; }}; done; echo ALIVE",
            seq = grace_seq.join(" "),
        );
        let forced = match self.ssh.exec(host, &term_cmd).await {
            Ok(out) if out.contains("GONE") => false,
            Ok(out) if out.contains("ALIVE") => {
                tracing::warn!(
                    host = %host.name,
                    pid,
                    grace_secs = Self::DAEMON_TERM_GRACE_SECS,
                    "daemon 未在 SIGTERM 宽限期内退出，升级 kill -9"
                );
                let kill9_cmd = format!(
                    "kill -9 {pid} 2>/dev/null; sleep 1; kill -0 {pid} 2>/dev/null || echo GONE"
                );
                let out9 = self.ssh.exec(host, &kill9_cmd).await?;
                if !out9.contains("GONE") {
                    return Err(TermError::ChannelError(format!(
                        "daemon (pid {pid}) 在 SIGKILL 后依然存活"
                    )));
                }
                true
            }
            Ok(_) => {
                // 无标记输出（异常 shell 行为 / channel 未回 ExitStatus）：按失败
                // 处理，绝不假定已停止
                return Err(TermError::ChannelError(format!(
                    "kill daemon (pid {pid}): 远端输出缺少 GONE/ALIVE 标记"
                )));
            }
            Err(e) => return Err(e),
        };

        // 4. 清理残留 socket 文件（尽力而为，失败不影响"已停止"结论）
        match self.remote_socket_path(host).await {
            Ok(socket) => {
                if let Err(e) = self.ssh.exec(host, &format!("rm -f {socket}")).await {
                    tracing::warn!(host = %host.name, socket, error = %e, "清理残留 socket 失败（尽力而为）");
                }
            }
            Err(e) => {
                tracing::warn!(host = %host.name, error = %e, "计算 socket 路径失败，跳过残留 socket 清理");
            }
        }

        tracing::info!(host = %host.name, pid, forced, "remote daemon stopped");
        Ok(RemoteDaemonStopResult::Stopped { pid, forced })
    }

    /// 重启远端 daemon（restart_remote_daemon MCP 工具后端）。
    ///
    /// 流程：确保 runtime 就绪（Missing / NeedsUpgrade → deploy）→ 停止运行中的
    /// daemon → bootstrap → hello 握手验证版本。
    ///
    /// 报告字段语义：
    /// - `was_running` / `stopped`：重启前 daemon 是否在运行、是否被本次停止
    ///   （kill 失败会直接 Err，不会以"未停止"成功返回）
    /// - `restarted`：bootstrap 是否 spawn 了全新 daemon。false = bootstrap 复用
    ///   了仍在运行的旧 daemon（如 pid 文件丢失导致 stop 判定"未运行"），此时
    ///   部署的新二进制尚未生效，`version_after` 即旧 daemon 的 build，调用方
    ///   （Agent）可据此重试或排查
    pub async fn restart_remote_daemon(
        &self,
        host: &Host,
    ) -> Result<RemoteDaemonRestartReport, TermError> {
        tracing::info!(host = %host.name, "restart_remote_daemon: 开始");

        // 1. 确保 runtime 就绪（缺失 / 版本不匹配 → 部署）
        let probe = self.check_remote_runtime(host).await?;
        let deployed = matches!(
            probe.state,
            RemoteRuntimeState::Missing | RemoteRuntimeState::NeedsUpgrade
        );
        if deployed {
            tracing::info!(host = %host.name, "restart_remote_daemon: 部署 runtime");
            self.deploy_runtime(host).await?;
        }
        let version_before = probe.remote_build;

        // 2. 停止运行中的 daemon（未运行 → NotRunning，直接进入 bootstrap）
        let (was_running, stopped) = match self.stop_remote_daemon(host).await? {
            RemoteDaemonStopResult::Stopped { .. } => (true, true),
            RemoteDaemonStopResult::NotRunning { reason } => {
                tracing::info!(host = %host.name, reason, "restart_remote_daemon: daemon 未在运行");
                (false, false)
            }
        };

        // 3. bootstrap（幂等）：daemon 已停止 → spawn 全新进程
        let info = self.bootstrap_daemon(host).await?;
        let restarted = info.daemon_id != "existing";

        // 4. hello 握手验证：connect 内校验协议版本（不匹配 → DaemonProtocolMismatch）
        let daemon = DaemonClient::connect(&self.ssh, host, &info.socket).await?;
        let version_after = daemon.daemon_build().to_string();
        if deployed && !restarted {
            tracing::warn!(
                host = %host.name,
                version_after,
                "restart_remote_daemon: 已部署新二进制但 bootstrap 复用了旧 daemon，升级未生效"
            );
        }
        tracing::info!(
            host = %host.name,
            was_running,
            stopped,
            deployed,
            restarted,
            version_before = ?version_before,
            version_after,
            "restart_remote_daemon: 完成"
        );

        Ok(RemoteDaemonRestartReport {
            stopped,
            was_running,
            deployed,
            version_before,
            version_after,
            restarted,
        })
    }

    /// 本地 agentd 二进制路径：`<data_local_dir>/TermBridge/agentd/termbridge-agentd`。
    ///
    /// 基目录用 `dirs::data_local_dir()`：Windows = `%LOCALAPPDATA%`（路径与早期
    /// 实现完全一致）、Linux = `~/.local/share`、macOS = `~/Library/Application
    /// Support`。早期实现只读 LOCALAPPDATA 环境变量，Unix 上基目录为空 → 相对
    /// 路径跟随 MCP 进程 cwd（IDE 拉起时不可控）。data_local_dir() 不可用时回退
    /// 旧行为（LOCALAPPDATA / 空基路径，后续 exists() 检查会失败）并告警。
    fn local_agentd_path() -> PathBuf {
        let base = dirs::data_local_dir().unwrap_or_else(|| {
            tracing::warn!(
                "dirs::data_local_dir() unavailable, falling back to LOCALAPPDATA (cwd-relative on Unix)"
            );
            std::env::var("LOCALAPPDATA").unwrap_or_default().into()
        });
        base.join("TermBridge")
            .join("agentd")
            .join("termbridge-agentd")
    }

    /// 确保本地 agentd 就绪：本地缓存存在则直接用；否则从当前可执行文件同目录的
    /// `resources/agentd/linux-x86_64/termbridge-agentd`（发布包内嵌，wrapper/平台包
    /// 保持同目录布局）自动复制到 `local_agentd_path()`。都不可用 → None。
    fn ensure_local_agentd() -> Option<PathBuf> {
        let local = Self::local_agentd_path();
        let bundled = Self::bundled_agentd_path()?; // 无发布包布局（如开发机）→ None
        ensure_agentd_copy(&local, &bundled)
    }

    /// 发布包内嵌 agentd 的路径：`<exe 同目录>/resources/agentd/linux-x86_64/termbridge-agentd`
    fn bundled_agentd_path() -> Option<PathBuf> {
        let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        Some(
            dir.join("resources")
                .join("agentd")
                .join("linux-x86_64")
                .join("termbridge-agentd"),
        )
    }
}

/// 纯函数：目标已存在 → 直接返回；bundled 存在 → 复制（含父目录创建 + POSIX 执行位）；
/// 否则 None。（可单测）
fn ensure_agentd_copy(local: &Path, bundled: &Path) -> Option<PathBuf> {
    if local.is_file() {
        return Some(local.to_path_buf());
    }
    let copied = bundled.is_file()
        && local.parent().map(|p| fs::create_dir_all(p).is_ok()).unwrap_or(false)
        && fs::copy(bundled, local).is_ok();
    if !copied {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(local, fs::Permissions::from_mode(0o755));
    }
    tracing::info!(
        bundled = %bundled.display(),
        local = %local.display(),
        "agentd: copied bundled binary into local cache"
    );
    Some(local.to_path_buf())
}

/// bootstrap_daemon 的返回信息（模块内部使用）。
struct DaemonBootstrapInfo {
    /// socket 路径（优先 bootstrap JSON 的 socket 字段，回退远端计算路径）
    socket: String,
    /// daemon_id：daemon 已在运行时 agentd bootstrap 返回字面量 `"existing"`，
    /// spawn 了全新进程时为新生成的 daemon id（restart 据此判定 restarted）
    daemon_id: String,
}

/// 纯函数：解析 pid 文件内容为 daemon pid。
///
/// 要求：trim 后非空、全为十进制数字、可解析为非 0 的 u32。pid 文件由 agentd
/// bootstrap 写入（`fs::write(path, format!("{pid}"))`），内容为一行十进制数。
/// 空文件 / 损坏内容 / 负数 / 溢出 → None（调用方按"daemon 未运行"处理，绝不
/// 盲杀）。（可单测）
fn parse_pid_file(content: &str) -> Option<u32> {
    let trimmed = content.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u32>().ok().filter(|&pid| pid != 0)
}

/// 纯函数：判断 `/proc/<pid>/comm`（或 `ps -p <pid> -o comm=`）输出是否为
/// agentd 进程，用于 kill 前的身份校验。
///
/// Linux 内核把 comm 截断为 15 字符（TASK_COMM_LEN=16 含结尾 NUL），二进制名
/// `termbridge-agentd`（17 字符）在 comm 里实际是 `termbridge-agen`；因此接受
/// 完整名与恰好 15 字符的前缀截断两种形态。其余（bash、更短的前缀如
/// `termbridge`、空输出等）一律拒绝——pid 被无关进程复用时绝不能 kill。（可单测）
fn is_agentd_comm(comm_output: &str) -> bool {
    const AGENTD_BIN: &str = "termbridge-agentd";
    let comm = comm_output.trim();
    if comm.is_empty() {
        return false;
    }
    comm == AGENTD_BIN || (comm.len() == 15 && AGENTD_BIN.starts_with(comm))
}

/// 纯函数：从远端 version 文件内容提取 build 字段。
///
/// version 文件由 deploy_runtime 写入：`{"protocol_version":1,"build":"0.1.0"}`。
/// 内容为空 / JSON 解析失败 / build 字段缺失或类型不符 → None（调用方视为
/// NeedsUpgrade 重新部署；升级日志中显示为 "?"）。（可单测）
fn parse_remote_build(version_file_stdout: &str) -> Option<String> {
    let trimmed = version_file_stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|v| {
            v.get("build")
                .and_then(|b| b.as_str())
                .map(|b| b.to_string())
        })
}

/// 纯函数：部署 runtime 后是否需要停止旧 daemon（open() 升级路径的决策点）。
///
/// 仅 `NeedsUpgrade`（升级部署：磁盘上的二进制已被 mv -f 替换，但旧 daemon
/// 进程仍在内存里跑旧版本）→ true；`Missing`（首次部署：远端从未有过 daemon，
/// 没有东西可停）→ false；`Stopped` / `Running` 不走 deploy 分支，防御性返回
/// false。（可单测）
fn should_stop_daemon_after_deploy(state: &RemoteRuntimeState) -> bool {
    matches!(state, RemoteRuntimeState::NeedsUpgrade)
}

// ───────────────────────────────────────────────────────────────────────────
// 单元测试
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::credential::Secret;

    #[test]
    fn local_agentd_path_layout() {
        // 路径布局固定：基目录（dirs::data_local_dir()，Windows = %LOCALAPPDATA%）
        // + TermBridge/agentd/termbridge-agentd；早期实现只读 LOCALAPPDATA 环境变量，
        // Unix 上基目录为空 → cwd 相对路径，已改为 dirs::data_local_dir()
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
    }

    #[test]
    fn parse_remote_build_extracts_build_field() {
        // 与 BUILD_VERSION 一致 → Some(BUILD_VERSION)（check_remote_runtime 视为版本匹配）
        let ok = format!(
            "{{\"protocol_version\":{},\"build\":\"{}\"}}",
            PROTOCOL_VERSION, BUILD_VERSION
        );
        assert_eq!(
            parse_remote_build(&ok),
            Some(BUILD_VERSION.to_string()),
            "相同 build 应解析出来: {ok}"
        );
        // cat 输出带尾部换行 → 仍解析成功
        assert_eq!(parse_remote_build(&format!("{ok}\n")), Some(BUILD_VERSION.to_string()));

        // build 不一致 → Some(旧版本)，调用方判 NeedsUpgrade（升级日志显示 x→y）
        assert_eq!(
            parse_remote_build(r#"{"protocol_version":1,"build":"0.0.9"}"#),
            Some("0.0.9".to_string())
        );
        // 内容为空 / 纯空白（version 文件缺失但 cat 未报错的兜底）→ None
        assert_eq!(parse_remote_build(""), None);
        assert_eq!(parse_remote_build("   \n"), None);
        // JSON 解析失败 / build 字段缺失或类型不符 → None
        assert_eq!(parse_remote_build("garbage"), None);
        assert_eq!(parse_remote_build(r#"{"protocol_version":1}"#), None);
        assert_eq!(parse_remote_build(r#"{"build":123}"#), None, "build 非字符串");
    }

    // ── stop_remote_daemon / restart_remote_daemon 纯逻辑 ────────────────

    #[test]
    fn parse_pid_file_accepts_plain_decimal_pid() {
        // agentd bootstrap 写入格式：一行十进制 pid（fs::write(path, format!("{pid}"))）
        assert_eq!(parse_pid_file("1234\n"), Some(1234));
        assert_eq!(parse_pid_file("  42  "), Some(42));
        assert_eq!(parse_pid_file("4294967295"), Some(u32::MAX));
    }

    #[test]
    fn parse_pid_file_rejects_invalid_content() {
        // 空文件 / 纯空白 / 非数字 → daemon 视为未运行，绝不盲杀
        assert_eq!(parse_pid_file(""), None);
        assert_eq!(parse_pid_file("   \n"), None);
        assert_eq!(parse_pid_file("abc"), None);
        assert_eq!(parse_pid_file("0"), None, "pid 0 非法（kill 0 会打进程组）");
        assert_eq!(parse_pid_file("-1"), None, "负号不是数字");
        assert_eq!(parse_pid_file("99999999999"), None, "u32 溢出");
        assert_eq!(parse_pid_file("12 34"), None, "中间空白 → 非全数字");
    }

    #[test]
    fn is_agentd_comm_accepts_full_and_truncated_names() {
        // /proc/<pid>/comm 截断为 15 字符（TASK_COMM_LEN=16 含 NUL）：
        // termbridge-agentd（17 字符）→ "termbridge-agen"
        assert!(is_agentd_comm("termbridge-agen\n"));
        assert!(is_agentd_comm("termbridge-agentd"));
        assert!(is_agentd_comm("  termbridge-agentd  \n"), "ps 输出前后空白应容忍");
    }

    #[test]
    fn is_agentd_comm_rejects_other_processes() {
        // kill 前身份校验：pid 被无关进程复用时必须拒绝
        assert!(!is_agentd_comm("bash"));
        assert!(!is_agentd_comm("sshd\n"));
        assert!(!is_agentd_comm("termbridge"), "更短的前缀不算（防止误杀同名前缀进程）");
        assert!(!is_agentd_comm("termbridge-agent"), "16 字符截断形态不存在且非精确匹配");
        assert!(!is_agentd_comm(""));
        assert!(!is_agentd_comm("\n"));
        assert!(!is_agentd_comm("termbridge-agentdd"), "比完整名更长不算");
    }

    #[test]
    fn should_stop_daemon_after_deploy_only_for_upgrade() {
        // 升级部署（NeedsUpgrade）：旧 daemon 进程仍在跑旧二进制 → 需要先 stop，
        // 随后的 bootstrap 才会以新二进制 spawn
        assert!(should_stop_daemon_after_deploy(&RemoteRuntimeState::NeedsUpgrade));
        // 首次部署（Missing）：远端从未有过 daemon → 不 stop
        assert!(!should_stop_daemon_after_deploy(&RemoteRuntimeState::Missing));
        // 不走 deploy 分支的状态：防御性 false
        assert!(!should_stop_daemon_after_deploy(&RemoteRuntimeState::Stopped));
        assert!(!should_stop_daemon_after_deploy(&RemoteRuntimeState::Running));
    }

    #[test]
    fn remote_daemon_stop_result_serializes_with_status_tag() {
        // tag = status：调用方可区分"没在跑"（非失败）与"已停止"
        let stopped = serde_json::to_value(RemoteDaemonStopResult::Stopped {
            pid: 4321,
            forced: true,
        })
        .unwrap();
        assert_eq!(stopped["status"], "stopped");
        assert_eq!(stopped["pid"], 4321);
        assert_eq!(stopped["forced"], true);

        let not_running = serde_json::to_value(RemoteDaemonStopResult::NotRunning {
            reason: "pid 文件不存在（daemon 从未启动或已被清理）".into(),
        })
        .unwrap();
        assert_eq!(not_running["status"], "not_running");
        assert_eq!(not_running["reason"], "pid 文件不存在（daemon 从未启动或已被清理）");
    }

    #[test]
    fn restart_report_serialization_contract() {
        let report = RemoteDaemonRestartReport {
            stopped: true,
            was_running: true,
            deployed: true,
            version_before: Some("0.0.9".into()),
            version_after: BUILD_VERSION.to_string(),
            restarted: true,
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["stopped"], true);
        assert_eq!(json["was_running"], true);
        assert_eq!(json["deployed"], true);
        assert_eq!(json["version_before"], "0.0.9");
        assert_eq!(json["version_after"], BUILD_VERSION);
        assert_eq!(json["restarted"], true);

        // version_before 未知（Missing / version 文件无法解析）→ 字段整体省略
        let unknown = RemoteDaemonRestartReport {
            version_before: None,
            ..report
        };
        let json = serde_json::to_value(&unknown).unwrap();
        assert!(
            json.get("version_before").is_none(),
            "未知 version_before 应省略字段: {json}"
        );
    }

    #[test]
    fn ensure_agentd_copy_copies_bundled_into_cache() {
        let dir = std::env::temp_dir().join(format!("tb-agentd-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let bundled = dir.join("resources/agentd/linux-x86_64/termbridge-agentd");
        fs::create_dir_all(bundled.parent().unwrap()).unwrap();
        fs::write(&bundled, b"agentd-bin").unwrap();
        let local = dir.join("LocalAppData/TermBridge/agentd/termbridge-agentd");

        let result = ensure_agentd_copy(&local, &bundled).expect("copy should succeed");
        assert_eq!(result, local);
        assert!(local.is_file());
        assert_eq!(fs::read(&local).unwrap(), b"agentd-bin");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_agentd_copy_keeps_existing_cache() {
        let dir = std::env::temp_dir().join(format!("tb-agentd-test2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let bundled = dir.join("resources/agentd/linux-x86_64/termbridge-agentd");
        fs::create_dir_all(bundled.parent().unwrap()).unwrap();
        fs::write(&bundled, b"new").unwrap();
        let local = dir.join("LocalAppData/TermBridge/agentd/termbridge-agentd");
        fs::create_dir_all(local.parent().unwrap()).unwrap();
        fs::write(&local, b"existing").unwrap();

        let result = ensure_agentd_copy(&local, &bundled).unwrap();
        assert_eq!(result, local);
        assert_eq!(fs::read(&local).unwrap(), b"existing", "已有缓存不应被覆盖");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_agentd_copy_missing_bundled_returns_none() {
        let dir = std::env::temp_dir().join(format!("tb-agentd-test3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let bundled = dir.join("missing/termbridge-agentd");
        let local = dir.join("LocalAppData/TermBridge/agentd/termbridge-agentd");
        assert!(ensure_agentd_copy(&local, &bundled).is_none());
        let _ = fs::remove_dir_all(&dir);
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

    // ── ADR-0017 §2.3：password + persistent 组合校验 ──────────────────

    #[tokio::test]
    async fn open_rejects_password_with_persistent_session() {
        // password + persistent 不支持：必须在不做任何 SSH 操作前拒绝
        //（SessionManager 已在弹密码前校验，此处为 Provider 边界双保险）
        let provider = PersistentProvider::default();
        let host = Host {
            name: "prod".into(),
            hostname: "192.0.2.1".into(),
            user: "root".into(),
            port: 22,
            identity_files: vec![],
            proxy_jump: None,
            user_known_hosts_files: vec![],
            strict_host_key_checking: "yes".into(),
        };
        let err = match provider
            .open(OpenTerminalRequest {
                host,
                pty_size: PtySize::default(),
                persistent: true,
                name: None,
                password: Some(Secret::new("hunter2".into())),
            })
            .await
        {
            Err(e) => e,
            // dyn TerminalHandle 不实现 Debug，不能用 unwrap_err
            Ok(_) => panic!("expected InvalidArgument for password+persistent"),
        };
        assert_eq!(err.code(), "INVALID_ARGUMENT");
        let msg = format!("{err}");
        assert!(msg.contains("auth=password with session=persistent"));
        assert!(msg.contains("ADR-0017"));
    }
}
