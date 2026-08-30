//! TermBridge GUI —— Tauri v2 后端
//!
//! Rust 后端直接调用 termbridge core API（不走 MCP），通过 Tauri commands + events
//! 与 React 前端通信。
//!
//! 数据流：
//! - 前端 → 后端：Tauri invoke（write_raw / resize / send_control / open / attach / close / detach）
//! - 后端 → 前端：Tauri event（pty_data / pty_eof），data 用 base64 编码

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use dashmap::DashMap;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::task::JoinHandle;

use termbridge::application::hosts::HostManager;
use termbridge::application::sessions::SessionManager;
use termbridge::domain::provider::{ControlKey, PtySize, TerminalProvider};
use termbridge::infrastructure::daemon_proto::SessionInfo;
use termbridge::infrastructure::persistent::PersistentProvider;

// ───────────────────────────────────────────────────────────────────────────
// AppState
// ───────────────────────────────────────────────────────────────────────────

/// GUI 全局状态：SessionManager + PTY read task 句柄
struct AppState {
    mgr: Arc<SessionManager>,
    /// PTY read loop 任务表：value 为 (task, generation)。generation 用于任务结束时
    /// 判断表中条目是否仍是自己（防止旧任务自清理时误删后继任务）；Arc 供任务
    /// 结束时自清理（spawn_read_loop 的 async move 拿不到 &self）
    read_tasks: Arc<DashMap<String, (JoinHandle<()>, u64)>>,
    next_read_task_gen: AtomicU64,
}

impl AppState {
    fn new() -> Self {
        let provider = Arc::new(PersistentProvider::default()) as Arc<dyn TerminalProvider>;
        Self {
            mgr: Arc::new(SessionManager::new(provider)),
            read_tasks: Arc::new(DashMap::new()),
            next_read_task_gen: AtomicU64::new(0),
        }
    }

    /// 启动 PTY read 循环：持续 read_raw → emit pty_data 事件
    fn spawn_read_loop(&self, app: AppHandle, session_id: String) {
        // React 18 StrictMode 双挂载 / 前端重复调用 start_read_loop 防护：
        // 同 session 已有 read loop 时必须先中止旧任务再重建，否则两个 tokio task
        // 并发 read_raw() 会把 PTY 字节流拆成两半（旧 task 对应的前端 listener 已
        // 注销，那一半输出丢失）
        if let Some((_, (old, _))) = self.read_tasks.remove(&session_id) {
            old.abort();
        }

        let mgr = self.mgr.clone();
        let sid = session_id.clone();
        let read_tasks = self.read_tasks.clone();
        let gen = self.next_read_task_gen.fetch_add(1, Ordering::Relaxed);

        let task = tokio::spawn(async move {
            loop {
                match mgr.read_raw(&sid).await {
                    Ok(Some(data)) => {
                        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
                        let payload = PtyDataEvent {
                            session_id: sid.clone(),
                            data: encoded,
                        };
                        // 单事件名 + session_id 字段，前端按 session_id 过滤
                        if app.emit("pty_data", &payload).is_err() {
                            break; // app 已关闭
                        }
                    }
                    Ok(None) => {
                        // PTY EOF（远端 shell 退出 / 连接断开）
                        let _ = app.emit("pty_eof", &PtyEofEvent {
                            session_id: sid.clone(),
                        });
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(session = %sid, error = %e, "read_raw error, stopping read loop");
                        let _ = app.emit("pty_eof", &PtyEofEvent {
                            session_id: sid.clone(),
                        });
                        break;
                    }
                }
            }
            // 循环自然结束（EOF / 错误 / app 关闭）：把本任务条目从 map 移除，防止
            // map 泄漏。仅当条目仍是自己（generation 匹配）才移除；detach/close 走
            // abort_read_task 的 remove（被 abort 的任务不会执行到这里），StrictMode
            // 快速重建出的后继任务 generation 更大，也不会被误删
            read_tasks.remove_if(&sid, |_, (_, g)| *g == gen);
        });

        self.read_tasks.insert(session_id, (task, gen));
    }

    /// 停止 PTY read 循环
    fn abort_read_task(&self, session_id: &str) {
        if let Some((_, (task, _))) = self.read_tasks.remove(session_id) {
            task.abort();
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Event payloads
// ───────────────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize)]
struct PtyDataEvent {
    session_id: String,
    /// base64 编码的 PTY 输出
    data: String,
}

#[derive(Clone, Serialize)]
struct PtyEofEvent {
    session_id: String,
}

// ───────────────────────────────────────────────────────────────────────────
// Tauri Commands
// ───────────────────────────────────────────────────────────────────────────

/// 列出 ~/.ssh/config 中的主机
#[tauri::command]
fn list_hosts() -> Vec<HostEntryDto> {
    HostManager::new()
        .list_hosts()
        .into_iter()
        .map(|h| HostEntryDto {
            alias: h.alias,
            hostname: h.hostname.unwrap_or_default(),
        })
        .collect()
}

#[derive(Serialize)]
struct HostEntryDto {
    alias: String,
    hostname: String,
}

/// 列出远端 daemon 上的 persistent session
#[tauri::command]
async fn list_remote_sessions(
    state: State<'_, AppState>,
    host: String,
) -> Result<Vec<SessionInfo>, String> {
    state
        .mgr
        .list_remote_sessions(&host)
        .await
        .map_err(|e| e.to_string())
}

/// 连接主机（开新 persistent session），返回 local session_id
///
/// 注意：此处不启动 read loop，需由前端注册 pty_data listener 后
/// 调用 start_read_loop 显式启动，避免事件丢失竞态。
#[tauri::command]
async fn open_session(
    state: State<'_, AppState>,
    host: String,
    cols: u16,
    rows: u16,
) -> Result<String, String> {
    let session_id = state
        .mgr
        .open_session(
            &host,
            Some(PtySize { rows, cols }),
            // GUI 连接语义 = 显式开 persistent session（ADR-0017 §2.4：
            // explicit > host policy——用户点了连接即显式要求 persistent）
            Some(true),
            Some("gui".to_string()),
        )
        .await
        .map_err(|e| e.to_string())?;

    // 中止内部 read_task，让 read_raw 独占 handle.read()
    state
        .mgr
        .prepare_for_raw_mode(&session_id)
        .map_err(|e| e.to_string())?;

    Ok(session_id)
}

/// attach 到远端已有 session，返回 local session_id
///
/// 注意：同 open_session，read loop 需由 start_read_loop 显式启动。
#[tauri::command]
async fn attach_session(
    state: State<'_, AppState>,
    host: String,
    remote_session_id: String,
) -> Result<String, String> {
    let session_id = state
        .mgr
        .attach_remote_session(&host, &remote_session_id, Some("gui-reattach".to_string()))
        .await
        .map_err(|e| e.to_string())?;

    state
        .mgr
        .prepare_for_raw_mode(&session_id)
        .map_err(|e| e.to_string())?;

    Ok(session_id)
}

/// 启动 PTY read 循环（前端注册 listener 后调用，避免事件丢失）
#[tauri::command]
fn start_read_loop(
    state: State<'_, AppState>,
    app: AppHandle,
    session_id: String,
) -> Result<(), String> {
    state.spawn_read_loop(app, session_id);
    Ok(())
}

/// detach（保留远端 session，停止本地 read 循环）
#[tauri::command]
async fn detach_session(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    state.abort_read_task(&session_id);
    state
        .mgr
        .detach_session(&session_id)
        .await
        .map_err(|e| e.to_string())
}

/// close（终止远端 session）
#[tauri::command]
async fn close_session(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    state.abort_read_task(&session_id);
    state
        .mgr
        .close_session(&session_id)
        .await
        .map_err(|e| e.to_string())
}

/// 写字节到 PTY（前端 xterm.js 输入 → base64 → 后端解码 → write_raw）
#[tauri::command]
async fn write_raw(
    state: State<'_, AppState>,
    session_id: String,
    data: String,
) -> Result<(), String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&data)
        .map_err(|e| e.to_string())?;
    state
        .mgr
        .write_raw(&session_id, &bytes)
        .await
        .map_err(|e| e.to_string())
}

/// 调整 PTY 尺寸
#[tauri::command]
async fn resize(
    state: State<'_, AppState>,
    session_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    state
        .mgr
        .resize(&session_id, cols, rows)
        .await
        .map_err(|e| e.to_string())
}

/// 发送控制字符（Ctrl+C / Ctrl+D / Ctrl+Z）
#[tauri::command]
async fn send_control(
    state: State<'_, AppState>,
    session_id: String,
    key: String,
) -> Result<(), String> {
    let ctrl = match key.as_str() {
        "ctrl_c" => ControlKey::CtrlC,
        "ctrl_d" => ControlKey::CtrlD,
        "ctrl_z" => ControlKey::CtrlZ,
        other => return Err(format!("unknown control key: {other}")),
    };
    state
        .mgr
        .send_control(&session_id, ctrl)
        .await
        .map_err(|e| e.to_string())
}

// ───────────────────────────────────────────────────────────────────────────
// App entry
// ───────────────────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,termbridge=debug")),
        )
        .init();

    tauri::Builder::default()
        .setup(|app| {
            // AppState::new() 内部 tokio::spawn idleReaper，必须在 runtime 上下文中调用
            let state = tauri::async_runtime::block_on(async { AppState::new() });
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_hosts,
            list_remote_sessions,
            open_session,
            attach_session,
            start_read_loop,
            detach_session,
            close_session,
            write_raw,
            resize,
            send_control,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn main() {
    run();
}
