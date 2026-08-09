//! Unix socket RPC server（ADR-0004 §3 阶段 3 / §4）
//!
//! 监听 Unix socket（0600 权限），accept 每个连接后 spawn handler：
//! 1. hello 握手（校验 protocol_version）
//! 2. 请求循环：read_msg → dispatch → write Response
//! 3. attach 成功后启动 event pump：轮询 buffer 增量 → 推送 pty_data 事件

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::protocol::{
    err_response, from_value, ok_response, read_msg, write_msg, Event, PtySize,
    Request, Response, BUILD_VERSION, PROTOCOL_VERSION,
};
use crate::session::{SessionError, SessionManager, SessionState};

// ───────────────────────────────────────────────────────────────────────────
// RpcServer
// ───────────────────────────────────────────────────────────────────────────

/// daemon RPC server
pub struct RpcServer {
    session_mgr: Arc<SessionManager>,
    daemon_id: String,
}

impl RpcServer {
    pub fn new(session_mgr: Arc<SessionManager>) -> Self {
        Self {
            session_mgr,
            daemon_id: gen_daemon_id(),
        }
    }

    /// 用外部指定的 daemon_id 构造（bootstrap 模式：父进程生成 id，子进程继承）
    pub fn new_with_id(session_mgr: Arc<SessionManager>, daemon_id: String) -> Self {
        Self {
            session_mgr,
            daemon_id,
        }
    }

    /// 监听 Unix socket 并 serve（前台运行，直到 daemon.shutdown 或进程被 kill）
    pub async fn serve(self, socket_path: PathBuf) -> Result<()> {
        // 清理 stale socket 文件
        if socket_path.exists() {
            std::fs::remove_file(&socket_path).ok();
        }
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind socket 失败: {:?}", socket_path))?;
        // 权限 0600：仅当前 Linux 用户可连（ADR-0004 §3 阶段 2）
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
        info!(?socket_path, daemon_id = %self.daemon_id, "daemon 监听中");

        let session_mgr = self.session_mgr.clone();
        let daemon_id = self.daemon_id.clone();

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let mgr = session_mgr.clone();
                    let did = daemon_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, mgr, did).await {
                            warn!("连接处理错误: {}", e);
                        }
                    });
                }
                Err(e) => {
                    warn!("accept 失败: {}", e);
                }
            }
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// 连接处理
// ───────────────────────────────────────────────────────────────────────────

/// 单个连接的完整生命周期：握手 → 请求循环 → 事件推送
async fn handle_connection(
    stream: UnixStream,
    session_mgr: Arc<SessionManager>,
    daemon_id: String,
) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    let write_half = Arc::new(Mutex::new(write_half));
    let mut read_half = read_half;

    // —— 阶段 1：hello 握手 ——
    let value = read_msg(&mut read_half).await?;
    let req: Request = from_value(&value).context("解析 hello 请求失败")?;
    if req.method != "hello" {
        let resp = err_response(
            req.id,
            "PROTOCOL_MISMATCH",
            format!("首条消息必须是 hello，收到: {}", req.method),
        );
        let mut w = write_half.lock().await;
        write_msg(&mut *w, &resp).await?;
        return Ok(());
    }
    // 校验 protocol_version
    let client_version = req.params.get("client_protocol_version").and_then(|v| v.as_u64());
    match client_version {
        Some(v) if v as u32 == PROTOCOL_VERSION => {
            let resp = ok_response(
                req.id,
                serde_json::json!({
                    "daemon_protocol_version": PROTOCOL_VERSION,
                    "daemon_build": BUILD_VERSION,
                    "daemon_id": daemon_id,
                }),
            );
            let mut w = write_half.lock().await;
            write_msg(&mut *w, &resp).await?;
        }
        _ => {
            let resp = err_response(
                req.id,
                "PROTOCOL_MISMATCH",
                format!(
                    "协议版本不匹配：client={:?} daemon={}",
                    client_version, PROTOCOL_VERSION
                ),
            );
            let mut w = write_half.lock().await;
            write_msg(&mut *w, &resp).await?;
            return Ok(());
        }
    }

    // —— 阶段 2：请求循环 ——
    loop {
        let value = match read_msg(&mut read_half).await {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // client 关闭连接
                break;
            }
            Err(e) => {
                warn!("读取请求失败: {}", e);
                break;
            }
        };
        let req: Request = match from_value(&value) {
            Ok(r) => r,
            Err(e) => {
                warn!("解析请求失败: {}", e);
                continue;
            }
        };
        let id = req.id;
        let (resp, pump_action) = dispatch(&req, &session_mgr).await;
        // 写响应
        {
            let mut w = write_half.lock().await;
            if let Err(e) = write_msg(&mut *w, &resp).await {
                warn!("写响应失败: {}", e);
                break;
            }
        }
        // 如果是 attach 成功，启动 event pump
        if let Some(PumpAction::Start { session_id, cursor_end }) = pump_action {
            let wh = write_half.clone();
            let mgr = session_mgr.clone();
            tokio::spawn(async move {
                event_pump(wh, mgr, session_id, cursor_end).await;
            });
        }
        // daemon.shutdown：写完响应后退出进程
        if req.method == "daemon.shutdown" {
            session_mgr.shutdown();
            info!("daemon.shutdown 收到，退出进程");
            std::process::exit(0);
        }
    }
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// dispatch
// ───────────────────────────────────────────────────────────────────────────

/// dispatch 返回 (Response, 可选的 event pump 启动动作)
enum PumpAction {
    Start { session_id: String, cursor_end: u64 },
}

async fn dispatch(
    req: &Request,
    mgr: &SessionManager,
) -> (Response, Option<PumpAction>) {
    let id = req.id;
    match req.method.as_str() {
        "session.create" => {
            let shell = req.params.get("shell").and_then(|v| v.as_str());
            let cwd = req.params.get("cwd").and_then(|v| v.as_str());
            let pty_size = req.params.get("pty_size");
            let name = req.params.get("name").and_then(|v| v.as_str());
            let (Some(shell), Some(pty_size)) = (shell, pty_size) else {
                return (err_response(id, "INVALID_ARGUMENT", "缺少 shell 或 pty_size"), None);
            };
            let rows = pty_size.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
            let cols = pty_size.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
            match mgr.create(shell, cwd, PtySize { rows, cols }, name.map(|s| s.to_string())) {
                Ok(session_id) => (
                    ok_response(
                        id,
                        serde_json::json!({ "session_id": session_id, "written": 0 }),
                    ),
                    None,
                ),
                Err(e) => (err_response(id, "INTERNAL", format!("{}", e)), None),
            }
        }
        "session.attach" => {
            let session_id = req.params.get("session_id").and_then(|v| v.as_str());
            let since_cursor = req.params.get("since_cursor").and_then(|v| v.as_u64()).unwrap_or(0);
            let Some(session_id) = session_id else {
                return (err_response(id, "INVALID_ARGUMENT", "缺少 session_id"), None);
            };
            match mgr.attach(session_id, since_cursor) {
                Ok(r) => {
                    let data_b64 = general_purpose::STANDARD.encode(&r.data);
                    let result = serde_json::json!({
                        "cursor_start": r.cursor_start,
                        "cursor_end": r.cursor_end,
                        "is_truncated": r.is_truncated,
                        "data": data_b64,
                    });
                    (
                        ok_response(id, result),
                        Some(PumpAction::Start {
                            session_id: session_id.to_string(),
                            cursor_end: r.cursor_end,
                        }),
                    )
                }
                Err(e) => (session_err_to_response(id, e), None),
            }
        }
        "session.detach" => {
            let session_id = req.params.get("session_id").and_then(|v| v.as_str());
            let Some(session_id) = session_id else {
                return (err_response(id, "INVALID_ARGUMENT", "缺少 session_id"), None);
            };
            match mgr.detach(session_id) {
                Ok(()) => (ok_response(id, serde_json::json!({})), None),
                Err(e) => (session_err_to_response(id, e), None),
            }
        }
        "session.list" => {
            let sessions = mgr.list();
            (
                ok_response(id, serde_json::json!({ "sessions": sessions })),
                None,
            )
        }
        "session.send_input" => {
            let session_id = req.params.get("session_id").and_then(|v| v.as_str());
            let data_b64 = req.params.get("data").and_then(|v| v.as_str());
            let (Some(session_id), Some(data_b64)) = (session_id, data_b64) else {
                return (err_response(id, "INVALID_ARGUMENT", "缺少 session_id 或 data"), None);
            };
            match general_purpose::STANDARD.decode(data_b64) {
                Ok(data) => match mgr.send_input(session_id, &data) {
                    Ok(()) => (ok_response(id, serde_json::json!({})), None),
                    Err(e) => (session_err_to_response(id, e), None),
                },
                Err(e) => (err_response(id, "INVALID_ARGUMENT", format!("base64 解码失败: {}", e)), None),
            }
        }
        "session.send_control" => {
            let session_id = req.params.get("session_id").and_then(|v| v.as_str());
            let control = req.params.get("control").and_then(|v| v.as_str());
            let (Some(session_id), Some(control)) = (session_id, control) else {
                return (err_response(id, "INVALID_ARGUMENT", "缺少 session_id 或 control"), None);
            };
            // 解析 control 字符串
            let ctrl = match control {
                "ctrl+c" => Some(crate::protocol::ControlKey::CtrlC),
                "ctrl+d" => Some(crate::protocol::ControlKey::CtrlD),
                "ctrl+z" => Some(crate::protocol::ControlKey::CtrlZ),
                "tab" => Some(crate::protocol::ControlKey::Tab),
                "enter" => Some(crate::protocol::ControlKey::Enter),
                "escape" => Some(crate::protocol::ControlKey::Escape),
                _ => None,
            };
            let Some(ctrl) = ctrl else {
                return (err_response(id, "INVALID_ARGUMENT", format!("未知 control: {}", control)), None);
            };
            match mgr.send_control(session_id, ctrl) {
                Ok(()) => (ok_response(id, serde_json::json!({})), None),
                Err(e) => (session_err_to_response(id, e), None),
            }
        }
        "session.resize" => {
            let session_id = req.params.get("session_id").and_then(|v| v.as_str());
            let rows = req.params.get("rows").and_then(|v| v.as_u64());
            let cols = req.params.get("cols").and_then(|v| v.as_u64());
            let (Some(session_id), Some(rows), Some(cols)) = (session_id, rows, cols) else {
                return (err_response(id, "INVALID_ARGUMENT", "缺少 session_id/rows/cols"), None);
            };
            match mgr.resize(session_id, rows as u16, cols as u16) {
                Ok(()) => (ok_response(id, serde_json::json!({})), None),
                Err(e) => (session_err_to_response(id, e), None),
            }
        }
        "session.read_output" => {
            let session_id = req.params.get("session_id").and_then(|v| v.as_str());
            let since_cursor = req.params.get("since_cursor").and_then(|v| v.as_u64()).unwrap_or(0);
            let Some(session_id) = session_id else {
                return (err_response(id, "INVALID_ARGUMENT", "缺少 session_id"), None);
            };
            match mgr.read_output(session_id, since_cursor) {
                Ok(r) => {
                    let data_b64 = general_purpose::STANDARD.encode(&r.data);
                    let result = serde_json::json!({
                        "cursor_start": r.cursor_start,
                        "cursor_end": r.cursor_end,
                        "is_truncated": r.is_truncated,
                        "data": data_b64,
                    });
                    (ok_response(id, result), None)
                }
                Err(e) => (session_err_to_response(id, e), None),
            }
        }
        "session.close" => {
            let session_id = req.params.get("session_id").and_then(|v| v.as_str());
            let Some(session_id) = session_id else {
                return (err_response(id, "INVALID_ARGUMENT", "缺少 session_id"), None);
            };
            match mgr.close(session_id) {
                Ok(()) => (ok_response(id, serde_json::json!({})), None),
                Err(e) => (session_err_to_response(id, e), None),
            }
        }
        "daemon.shutdown" => {
            // 响应空对象，handler 在写完响应后退出进程
            (ok_response(id, serde_json::json!({})), None)
        }
        other => (
            err_response(id, "NOT_FOUND", format!("未知 method: {}", other)),
            None,
        ),
    }
}

/// SessionError → 协议 Response
fn session_err_to_response(id: u64, e: SessionError) -> Response {
    let (code, msg) = match e {
        SessionError::NotFound(_) => ("NOT_FOUND", format!("{}", e)),
        SessionError::InvalidState { .. } => ("INVALID_STATE", format!("{}", e)),
        SessionError::Lost(_) => ("SESSION_LOST", format!("{}", e)),
        SessionError::Pty(_) => ("INTERNAL", format!("{}", e)),
        SessionError::InvalidArgument(_) => ("INVALID_ARGUMENT", format!("{}", e)),
    };
    err_response(id, code, msg)
}

// ───────────────────────────────────────────────────────────────────────────
// event pump
// ───────────────────────────────────────────────────────────────────────────

/// 事件推送 task：轮询 session buffer 增量，有新数据时发 pty_data 事件。
///
/// MVP 用轮询（每 10ms 检查 written 是否增长）。Phase 4 优化为 Notify 唤醒。
/// 退出条件：session 不存在 / state != Attached / 写失败
async fn event_pump(
    write_half: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    mgr: Arc<SessionManager>,
    session_id: String,
    mut last_sent: u64,
) {
    loop {
        tokio::time::sleep(Duration::from_millis(10)).await;
        // 检查 session 是否还 Attached
        match mgr.session_state(&session_id) {
            Some(SessionState::Attached) => {}
            _ => break, // session 不存在 / detach / lost
        }
        // 读增量
        match mgr.read_output(&session_id, last_sent) {
            Ok(r) => {
                if r.data.is_empty() {
                    continue;
                }
                let data_b64 = general_purpose::STANDARD.encode(&r.data);
                let ev = Event::pty_data(
                    &session_id,
                    r.cursor_start,
                    r.cursor_end,
                    r.is_truncated,
                    data_b64,
                );
                let mut w = write_half.lock().await;
                if write_msg(&mut *w, &ev).await.is_err() {
                    break; // 写失败（连接关闭）
                }
                last_sent = r.cursor_end;
            }
            Err(_) => break,
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// daemon_id 生成
// ───────────────────────────────────────────────────────────────────────────

static DAEMON_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 生成 daemon_id：daed_ + 8 hex（计数器 ^ 纳秒时间戳，取低 32 位）
pub(crate) fn gen_daemon_id() -> String {
    let n = DAEMON_COUNTER.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = (n ^ ts) as u32;
    format!("daed_{:08x}", mixed)
}
