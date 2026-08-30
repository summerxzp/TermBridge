//! Unix socket RPC server（ADR-0004 §3 阶段 3 / §4）
//!
//! 监听 Unix socket（0600 权限），accept 每个连接后 spawn handler：
//! 1. hello 握手（校验 protocol_version）
//! 2. 请求循环：read_msg → dispatch → write Response
//! 3. attach 成功后启动 event pump：事件驱动（session Notify 唤醒）推送 pty_data 增量，
//!    session 结束时保证发送恰好一个终态事件（pty_exit / session_lost）
//! 4. 连接退出（client EOF / 写失败）时把本连接仍 Attached 的 session 复位为 Detached，
//!    保证重连后可重新 attach（PTY 进程继续运行 = 持久化契约）

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
use crate::session::{Session, SessionError, SessionManager, SessionState, TerminalInfo};

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
    // 本连接 attach 的 session 列表：连接退出时逐个复位为 Detached（仅状态，
    // PTY 进程继续运行），保证重连后 session.attach 不再永久 INVALID_STATE
    let mut attached_sessions: Vec<String> = Vec::new();
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
        let (resp, pump_action) = dispatch(&req, &session_mgr, &mut attached_sessions).await;
        // 写响应
        {
            let mut w = write_half.lock().await;
            if let Err(e) = write_msg(&mut *w, &resp).await {
                warn!("写响应失败: {}", e);
                break;
            }
        }
        // 如果是 attach 成功，启动 event pump
        if let Some(PumpAction { session, session_id, cursor_end, epoch }) = pump_action {
            let wh = write_half.clone();
            let mgr = session_mgr.clone();
            tokio::spawn(async move {
                event_pump(wh, mgr, session, session_id, epoch, cursor_end).await;
            });
        }
        // daemon.shutdown：写完响应后退出进程
        if req.method == "daemon.shutdown" {
            session_mgr.shutdown();
            info!("daemon.shutdown 收到，退出进程");
            std::process::exit(0);
        }
    }
    // —— 阶段 3：连接退出清理 ——
    // 本连接仍 Attached 的 session 复位为 Detached（PTY 及其进程继续运行，仅重置
    // attach 状态）；已 Lost / 已 Detached 的 session detach 报错，忽略即可。
    for session_id in &attached_sessions {
        let _ = session_mgr.detach(session_id);
    }
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// dispatch
// ───────────────────────────────────────────────────────────────────────────

/// dispatch 返回的 event pump 启动动作
struct PumpAction {
    session: Arc<Session>,
    session_id: String,
    /// attach 响应携带的 cursor_end（pump 起始游标）
    cursor_end: u64,
    /// pump 代际（attach 时递增；旧代际 pump 自动作废，防止重连后新旧并存）
    epoch: u64,
}

async fn dispatch(
    req: &Request,
    mgr: &SessionManager,
    attached: &mut Vec<String>,
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
                    let Some(session) = mgr.get_session(session_id) else {
                        return (err_response(id, "INTERNAL", "attach 后 session 消失"), None);
                    };
                    // attach 已递增 pump 代际，取当前值作为本 pump 的代际标记
                    let epoch = session.pump_epoch();
                    // 记录到本连接的 attach 列表：连接退出时复位为 Detached
                    attached.push(session_id.to_string());
                    let data_b64 = general_purpose::STANDARD.encode(&r.data);
                    let result = serde_json::json!({
                        "cursor_start": r.cursor_start,
                        "cursor_end": r.cursor_end,
                        "is_truncated": r.is_truncated,
                        "data": data_b64,
                    });
                    (
                        ok_response(id, result),
                        Some(PumpAction {
                            session,
                            session_id: session_id.to_string(),
                            cursor_end: r.cursor_end,
                            epoch,
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
                Ok(()) => {
                    attached.retain(|s| s != session_id);
                    (ok_response(id, serde_json::json!({})), None)
                }
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
                Ok(()) => {
                    attached.retain(|s| s != session_id);
                    (ok_response(id, serde_json::json!({})), None)
                }
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

/// pump 兜底轮询间隔（主路径为事件驱动：session 的 Notify 唤醒；此 tick 仅作
/// 极端情况下丢通知的保险，不承载数据吞吐）
const PUMP_TICK: Duration = Duration::from_millis(200);

/// 事件推送 task：事件驱动推送 session buffer 增量（pty_data 事件）。
///
/// 由 read task 在每次写入 buffer 后 `Notify::notify_one` 唤醒；`notify_one` 会存储
/// 唤醒许可，检查与等待之间的窗口不会丢通知。每次唤醒后全量重查状态 + 读增量
/// （condvar 语义），保证最终一致。
///
/// 终态保证：pump 退出前恰好发送一个终态事件 ——
/// - 子进程退出（state = Lost）：先冲刷尾部 pty_data，再发 pty_exit（退出码未知时
///   exit_code 字段缺省）或 session_lost（读错误等异常丢失，附 reason）；
/// - session.close()（管理器中已移除）：发 session_lost("session closed")；
/// - 客户端主动 detach / 被新连接的 attach 取代（epoch 不匹配）：不发终态。
async fn event_pump(
    write_half: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    mgr: Arc<SessionManager>,
    session: Arc<Session>,
    session_id: String,
    epoch: u64,
    mut last_sent: u64,
) {
    let notify = session.pump_notify();
    loop {
        // 被更新的 pump 取代（detach 后重连）→ 退出，不发终态
        if session.pump_epoch() != epoch {
            return;
        }
        // 1. 冲刷增量（Lost 后也要先把尾部数据发完，再发终态 —— 顺序保证）
        let r = session.read_since(last_sent);
        if !r.data.is_empty() {
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
                return; // 写失败（连接关闭）
            }
            drop(w);
            last_sent = r.cursor_end;
        }
        // 2. session 已被 close 移除 → session_lost 终态（恰好一次）
        if !mgr.contains(&session_id) {
            let ev = Event::session_lost(&session_id, "session closed");
            let mut w = write_half.lock().await;
            let _ = write_msg(&mut *w, &ev).await;
            return;
        }
        // 3. 状态检查（Lost 时 read task 已填好终态信息，见 session.rs finish_lost）
        match session.state() {
            SessionState::Attached => {}
            SessionState::Lost => {
                let ev = match session.terminal() {
                    Some(TerminalInfo::Exited(code)) => Event::pty_exit(&session_id, code),
                    // 退出码未知：与 daemon_proto 的 unknown 约定一致，exit_code 缺省
                    Some(TerminalInfo::ExitedUnknown) => Event::pty_exit_unknown(&session_id),
                    Some(TerminalInfo::Lost(reason)) => Event::session_lost(&session_id, reason),
                    None => Event::session_lost(&session_id, "pty eof"),
                };
                let mut w = write_half.lock().await;
                let _ = write_msg(&mut *w, &ev).await;
                return;
            }
            // Detached / Created：客户端主动离开，不发终态
            _ => return,
        }
        // 4. 等待唤醒：read task 的 Notify（新数据 / 状态变化）+ 周期兜底 tick
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(PUMP_TICK) => {}
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

// ───────────────────────────────────────────────────────────────────────────
// 单元测试（仅 Linux：依赖 Unix socket / PTY）
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Request, PROTOCOL_VERSION};
    use crate::protocol::{read_msg, write_msg};
    use crate::session::SessionManager;
    use std::time::Instant;

    /// 建立一条完成 hello 握手的连接（返回 client 端流）
    async fn connect_and_hello(
        mgr: Arc<SessionManager>,
    ) -> (UnixStream, tokio::task::JoinHandle<anyhow::Result<()>>) {
        let (client, server) = UnixStream::pair().expect("unix pair");
        let handle = tokio::spawn(handle_connection(server, mgr, "daed_test".to_string()));
        let mut client = client;
        let hello = Request {
            id: 1,
            method: "hello".to_string(),
            params: serde_json::json!({ "client_protocol_version": PROTOCOL_VERSION }),
        };
        write_msg(&mut client, &hello).await.expect("write hello");
        let resp = read_msg(&mut client).await.expect("read hello resp");
        assert_eq!(resp["ok"], true, "hello 应成功: {}", resp);
        (client, handle)
    }

    async fn send_attach(
        client: &mut UnixStream,
        session_id: &str,
        id: u64,
    ) -> serde_json::Value {
        let req = Request {
            id,
            method: "session.attach".to_string(),
            params: serde_json::json!({ "session_id": session_id, "since_cursor": 0 }),
        };
        write_msg(client, &req).await.expect("write attach");
        let resp = read_msg(client).await.expect("read attach resp");
        assert_eq!(resp["ok"], true, "attach 应成功: {}", resp);
        resp
    }

    /// Fix 2：client 断开后，仍 Attached 的 session 应复位为 Detached（PTY 继续运行），
    /// 重连后可重新 attach
    #[tokio::test]
    async fn client_disconnect_resets_attach_state() {
        let mgr = Arc::new(SessionManager::new());
        // /bin/cat 存活不退出（sleep 无参数会立即退出，attach 可能撞上 Lost）
        let sid = mgr
            .create("/bin/cat", None, PtySize { rows: 24, cols: 80 }, None)
            .expect("create");
        let (mut client, handle) = connect_and_hello(mgr.clone()).await;

        send_attach(&mut client, &sid, 2).await;
        assert_eq!(mgr.list()[0].state, "attached");

        // client 断开 → 连接循环退出 → 清理路径复位 attach 状态
        drop(client);
        let joined = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("handle_connection 应及时退出");
        assert!(joined.is_ok(), "handle_connection 不应 panic");

        assert_eq!(mgr.list().len(), 1, "session 不应被移除（PTY 继续运行）");
        assert_eq!(mgr.list()[0].state, "detached", "应复位为 detached");

        // 重连后应可重新 attach
        mgr.attach(&sid, 0).expect("重连后应可 attach");
        mgr.close(&sid).expect("close");
    }

    /// Fix 1：子进程退出后，pump 应先冲刷尾部 pty_data，再发恰好一个 pty_exit 终态事件
    #[tokio::test]
    async fn pump_emits_tail_then_single_pty_exit() {
        let mgr = Arc::new(SessionManager::new());
        let sid = mgr
            .create("/bin/cat", None, PtySize { rows: 24, cols: 80 }, None)
            .expect("create");
        let (mut client, handle) = connect_and_hello(mgr.clone()).await;
        send_attach(&mut client, &sid, 2).await;

        // 让 cat 退出：ctrl+d（EOF）→ PTY EOF → read task 回收 → 终态
        mgr.send_input(&sid, b"\x04").expect("send ctrl+d");

        // 读事件：可能先有 pty_data（回显 ^D），最后必须收到恰好一个 pty_exit(0)
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut terminal = None;
        while Instant::now() < deadline {
            let v = tokio::time::timeout(Duration::from_secs(2), read_msg(&mut client))
                .await
                .expect("读事件超时")
                .expect("读事件失败");
            let Some(ev) = v.get("event").and_then(|e| e.as_str()) else {
                continue; // attach 响应等非事件消息
            };
            if ev == "pty_exit" || ev == "session_lost" {
                terminal = Some(v);
                break;
            }
        }
        let terminal = terminal.expect("应收到终态事件");
        assert_eq!(terminal["event"], "pty_exit");
        assert_eq!(terminal["session_id"], sid);
        assert_eq!(terminal["exit_code"], 0, "cat 正常退出应为 exit_code 0");

        // 终态之后不应再有事件（恰好一个终态；500ms 内读不到任何消息）
        let extra = tokio::time::timeout(Duration::from_millis(500), read_msg(&mut client)).await;
        assert!(extra.is_err(), "终态后不应再有事件: {:?}", extra);

        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        mgr.close(&sid).ok();
    }

    /// Fix 1：session.close() 后，pump 应给 attach 中的 client 发恰好一个
    /// session_lost 终态事件
    #[tokio::test]
    async fn pump_emits_session_lost_on_close() {
        let mgr = Arc::new(SessionManager::new());
        let sid = mgr
            .create("/bin/cat", None, PtySize { rows: 24, cols: 80 }, None)
            .expect("create");
        let (mut client, handle) = connect_and_hello(mgr.clone()).await;
        send_attach(&mut client, &sid, 2).await;

        // session.close
        let req = Request {
            id: 3,
            method: "session.close".to_string(),
            params: serde_json::json!({ "session_id": sid }),
        };
        write_msg(&mut client, &req).await.expect("write close");
        let resp = read_msg(&mut client).await.expect("read close resp");
        assert_eq!(resp["ok"], true, "close 应成功: {}", resp);

        // 应收到 session_lost("session closed")
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut terminal = None;
        while Instant::now() < deadline {
            let v = tokio::time::timeout(Duration::from_secs(2), read_msg(&mut client))
                .await
                .expect("读事件超时")
                .expect("读事件失败");
            let Some(ev) = v.get("event").and_then(|e| e.as_str()) else {
                continue;
            };
            if ev == "pty_exit" || ev == "session_lost" {
                terminal = Some(v);
                break;
            }
        }
        let terminal = terminal.expect("应收到终态事件");
        assert_eq!(terminal["event"], "session_lost");
        assert_eq!(terminal["session_id"], sid);
        assert_eq!(terminal["reason"], "session closed");

        // 恰好一个：终态后不应再有事件
        let extra = tokio::time::timeout(Duration::from_millis(500), read_msg(&mut client)).await;
        assert!(extra.is_err(), "终态后不应再有事件: {:?}", extra);

        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }
}
