//! ControlServer —— 本地 IPC server（ADR-0018）。
//!
//! 监听 Unix socket / Named Pipe，接受 CLI/GUI 连接，
//! 验证 HELLO token，处理 JSON-RPC 请求。
//!
//! 传输层与业务逻辑通过 ControlHandler trait 解耦。

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tokio::net::{UnixListener, UnixStream};

use super::handler::ControlHandler;
use super::instance::InstanceRegistry;
use super::proto::{
    ControlError, ControlRequest, ControlResponse, HelloRequest, HelloResponse,
    SetApprovalModeParams,
};

/// Control IPC server。
///
/// 启动后监听本地 IPC 端点，处理来自 CLI/GUI 的控制请求。
/// MCP Server 持有此实例，Drop 时自动清理。
pub struct ControlServer {
    /// instance 注册信息（Drop 时清理文件）
    registry: InstanceRegistry,
    /// 监听 task 句柄
    listen_task: tokio::task::JoinHandle<()>,
}

/// ControlServer 启动后的句柄。
impl ControlServer {
    /// 启动 Control IPC server。
    ///
    /// - 创建 IPC 端点（Unix socket / Named Pipe）
    /// - 写入 instance discovery 文件
    /// - spawn 监听 task
    pub async fn start(handler: Arc<dyn ControlHandler>) -> std::io::Result<Self> {
        let token = generate_token();
        let endpoint = generate_endpoint(&token);

        // 创建 Unix socket 监听（Windows Named Pipe 第一版暂用 Unix socket 的
        // tokio 支持；Windows 实现见 TODO 注释）
        #[cfg(target_os = "linux")]
        {
            Self::start_unix(handler, endpoint, token).await
        }
        #[cfg(target_os = "macos")]
        {
            Self::start_unix(handler, endpoint, token).await
        }
        #[cfg(target_os = "windows")]
        {
            // Windows: 第一版用 TCP loopback 作为 Named Pipe 的简化替代
            // TODO: 未来切换到 tokio::net::windows::named_pipe
            Self::start_tcp_loopback(handler, endpoint, token).await
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            Self::start_tcp_loopback(handler, endpoint, token).await
        }
    }

    /// Unix socket 实现（Linux/macOS）。
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn start_unix(
        handler: Arc<dyn ControlHandler>,
        endpoint: String,
        token: String,
    ) -> std::io::Result<Self> {
        // 确保旧 socket 文件不存在
        let _ = std::fs::remove_file(&endpoint);

        // 确保父目录存在（bind 不创建父目录；XDG_RUNTIME_DIR 未设置时
        // 兜底 /tmp 下的 termbridge 目录可能不存在）
        if let Some(parent) = std::path::Path::new(&endpoint).parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(&endpoint)?;

        // 设置 0600 权限
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&endpoint, perms)?;

        tracing::info!(endpoint = %endpoint, "Control IPC: Unix socket listening");

        let registry = InstanceRegistry::register(endpoint.clone(), token.clone())?;

        let listen_task = tokio::spawn(async move {
            Self::accept_loop_unix(listener, handler, token).await;
        });

        Ok(Self {
            registry,
            listen_task,
        })
    }

    /// TCP loopback 实现（Windows 及其他平台，第一版简化方案）。
    ///
    /// 绑定 127.0.0.1:0 随机端口，endpoint 记录为 "tcp://127.0.0.1:<port>"。
    /// 安全性靠 token 认证 + loopback-only（不暴露到网络）。
    /// TODO: 未来切换到 Named Pipe。
    #[cfg(any(target_os = "windows", not(any(target_os = "linux", target_os = "macos"))))]
    async fn start_tcp_loopback(
        handler: Arc<dyn ControlHandler>,
        _endpoint: String,
        token: String,
    ) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let local_addr = listener.local_addr()?;
        let endpoint = format!("tcp://127.0.0.1:{}", local_addr.port());

        tracing::info!(endpoint = %endpoint, "Control IPC: TCP loopback listening");

        let registry = InstanceRegistry::register(endpoint, token.clone())?;

        let listen_task = tokio::spawn(async move {
            Self::accept_loop_tcp(listener, handler, token).await;
        });

        Ok(Self {
            registry,
            listen_task,
        })
    }

    /// 获取 instance 信息（供日志/调试）。
    pub fn instance_info(&self) -> &super::instance::InstanceInfo {
        self.registry.info()
    }

    /// Unix socket accept 循环。
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn accept_loop_unix(
        listener: UnixListener,
        handler: Arc<dyn ControlHandler>,
        token: String,
    ) {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let h = Arc::clone(&handler);
                    let t = token.clone();
                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_connection_unix(stream, h, t).await {
                            tracing::warn!(error = %e, "Control IPC connection error");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Control IPC accept error");
                    // 短暂等待后继续（避免 busy loop）
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// TCP accept 循环。
    #[cfg(any(target_os = "windows", not(any(target_os = "linux", target_os = "macos"))))]
    async fn accept_loop_tcp(
        listener: tokio::net::TcpListener,
        handler: Arc<dyn ControlHandler>,
        token: String,
    ) {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let h = Arc::clone(&handler);
                    let t = token.clone();
                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_connection_tcp(stream, h, t).await {
                            tracing::warn!(error = %e, "Control IPC connection error");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Control IPC accept error");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// 处理单个 Unix socket 连接。
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn handle_connection_unix(
        stream: UnixStream,
        handler: Arc<dyn ControlHandler>,
        token: String,
    ) -> std::io::Result<()> {
        Self::handle_connection(stream, handler, token).await
    }

    /// 处理单个 TCP 连接。
    #[cfg(any(target_os = "windows", not(any(target_os = "linux", target_os = "macos"))))]
    async fn handle_connection_tcp(
        stream: tokio::net::TcpStream,
        handler: Arc<dyn ControlHandler>,
        token: String,
    ) -> std::io::Result<()> {
        Self::handle_connection(stream, handler, token).await
    }

    /// 通用连接处理（tokio AsyncRead + AsyncWrite）。
    async fn handle_connection<S>(
        stream: S,
        handler: Arc<dyn ControlHandler>,
        token: String,
    ) -> std::io::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // 1. HELLO 认证（第一条消息必须是 HELLO + token）
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // 连接立即关闭
        }

        let hello: HelloRequest = match serde_json::from_str(line.trim()) {
            Ok(h) => h,
            Err(_) => {
                let resp = HelloResponse {
                    ok: false,
                    error: Some("expected HELLO with token".into()),
                };
                write_half
                    .write_all(serde_json::to_string(&resp)?.as_bytes())
                    .await?;
                write_half.write_all(b"\n").await?;
                return Ok(());
            }
        };

        if hello.token != token {
            let resp = HelloResponse {
                ok: false,
                error: Some("invalid token".into()),
            };
            write_half
                .write_all(serde_json::to_string(&resp)?.as_bytes())
                .await?;
            write_half.write_all(b"\n").await?;
            tracing::warn!("Control IPC: HELLO token mismatch, rejecting");
            return Ok(());
        }

        // 认证成功
        let hello_resp = HelloResponse {
            ok: true,
            error: None,
        };
        write_half
            .write_all(serde_json::to_string(&hello_resp)?.as_bytes())
            .await?;
        write_half.write_all(b"\n").await?;

        // 2. 处理后续 JSON-RPC 请求
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                break; // EOF
            }

            let req: ControlRequest = match serde_json::from_str(line.trim()) {
                Ok(r) => r,
                Err(e) => {
                    let resp = ControlResponse::Err {
                        id: 0,
                        ok: false,
                        error: ControlError::new(
                            "PARSE_ERROR",
                            format!("invalid JSON: {e}"),
                        ),
                    };
                    write_half
                        .write_all(serde_json::to_string(&resp)?.as_bytes())
                        .await?;
                    write_half.write_all(b"\n").await?;
                    continue;
                }
            };

            let resp = Self::dispatch(&req, &handler).await;
            write_half
                .write_all(serde_json::to_string(&resp)?.as_bytes())
                .await?;
            write_half.write_all(b"\n").await?;
        }

        Ok(())
    }

    /// 分发请求到 handler。
    async fn dispatch(
        req: &ControlRequest,
        handler: &Arc<dyn ControlHandler>,
    ) -> ControlResponse {
        match req.method.as_str() {
            "session.list" => {
                let sessions = handler.list_sessions();
                ControlResponse::Ok {
                    id: req.id,
                    ok: true,
                    result: serde_json::json!(sessions),
                }
            }
            "session.get" => {
                let session_id = req
                    .params
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match handler.get_session(session_id) {
                    Some(info) => ControlResponse::Ok {
                        id: req.id,
                        ok: true,
                        result: serde_json::json!(info),
                    },
                    None => ControlResponse::Err {
                        id: req.id,
                        ok: false,
                        error: ControlError::new(
                            "NOT_FOUND",
                            format!("session not found: {session_id}"),
                        ),
                    },
                }
            }
            "session.set_approval_mode" => {
                match serde_json::from_value::<SetApprovalModeParams>(req.params.clone()) {
                    Ok(params) => match params.parse_mode() {
                        Ok(mode) => {
                            match handler.set_approval_mode(&params.session_id, mode) {
                                Ok(()) => ControlResponse::Ok {
                                    id: req.id,
                                    ok: true,
                                    result: serde_json::json!({"session_id": params.session_id, "approval_mode": mode}),
                                },
                                Err(e) => ControlResponse::Err {
                                    id: req.id,
                                    ok: false,
                                    error: e,
                                },
                            }
                        }
                        Err(e) => ControlResponse::Err {
                            id: req.id,
                            ok: false,
                            error: e,
                        },
                    },
                    Err(e) => ControlResponse::Err {
                        id: req.id,
                        ok: false,
                        error: ControlError::new(
                            "INVALID_ARGUMENT",
                            format!("invalid params: {e}"),
                        ),
                    },
                }
            }
            _ => ControlResponse::Err {
                id: req.id,
                ok: false,
                error: ControlError::new(
                    "METHOD_NOT_FOUND",
                    format!("unknown method: {}", req.method),
                ),
            },
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.listen_task.abort();
        // InstanceRegistry 的 Drop 会清理 discovery 文件
    }
}

/// 生成随机 token（16 字符 hex）。
fn generate_token() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    format!("{:016x}", ts ^ (pid << 64))
}

/// 生成 IPC 端点路径。
fn generate_endpoint(token: &str) -> String {
    let id = &token[..6.min(token.len())];
    if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
        // $XDG_RUNTIME_DIR/termbridge/mcp-<id>.sock
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        format!("{base}/termbridge/mcp-{id}.sock")
    } else {
        // Windows Named Pipe（第一版用 TCP loopback，endpoint 在 start_tcp_loopback 中重写）
        format!("\\\\.\\pipe\\termbridge-mcp-{id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::proto::SessionControlInfo;

    struct StubHandler {
        sessions: Vec<SessionControlInfo>,
    }

    impl ControlHandler for StubHandler {
        fn list_sessions(&self) -> Vec<SessionControlInfo> {
            self.sessions.clone()
        }
        fn get_session(&self, id: &str) -> Option<SessionControlInfo> {
            self.sessions.iter().find(|s| s.id == id).cloned()
        }
        fn set_approval_mode(
            &self,
            session_id: &str,
            mode: &str,
        ) -> Result<(), ControlError> {
            if self.sessions.iter().any(|s| s.id == session_id) {
                tracing::info!(session = session_id, mode = mode, "stub: set_approval_mode");
                Ok(())
            } else {
                Err(ControlError::new("NOT_FOUND", "session not found"))
            }
        }
    }

    #[tokio::test]
    async fn control_server_start_and_dispatch() {
        let handler = Arc::new(StubHandler {
            sessions: vec![SessionControlInfo {
                id: "sess_test".into(),
                host: "host1".into(),
                state: "ready".into(),
                approval_mode: "standard".into(),
            }],
        });

        let server = ControlServer::start(handler.clone())
            .await
            .expect("start failed");

        // 获取 endpoint 和 token
        let info = server.instance_info();
        let endpoint = &info.endpoint;
        let token = &info.token;

        // 连接并测试（Unix socket 路径）
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let stream = UnixStream::connect(endpoint)
                .await
                .expect("connect failed");
            let (read_half, mut write_half) = tokio::io::split(stream);

            // HELLO
            let hello = serde_json::json!({"token": token});
            write_half
                .write_all(format!("{hello}\n").as_bytes())
                .await
                .unwrap();

            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let resp: HelloResponse = serde_json::from_str(line.trim()).unwrap();
            assert!(resp.ok);

            // session.list
            line.clear();
            let req = serde_json::json!({"id": 1, "method": "session.list", "params": {}});
            write_half
                .write_all(format!("{req}\n").as_bytes())
                .await
                .unwrap();
            reader.read_line(&mut line).await.unwrap();
            let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(resp["ok"], true);
            assert_eq!(resp["result"][0]["id"], "sess_test");
        }

        // Windows TCP loopback 测试
        #[cfg(target_os = "windows")]
        {
            // 解析 tcp://127.0.0.1:<port>
            let addr: String = endpoint
                .strip_prefix("tcp://")
                .unwrap_or("127.0.0.1:0")
                .into();
            let stream = tokio::net::TcpStream::connect(&addr)
                .await
                .expect("connect failed");
            let (read_half, mut write_half) = tokio::io::split(stream);

            // HELLO
            let hello = serde_json::json!({"token": token});
            write_half
                .write_all(format!("{hello}\n").as_bytes())
                .await
                .unwrap();

            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let resp: HelloResponse = serde_json::from_str(line.trim()).unwrap();
            assert!(resp.ok);

            // session.list
            line.clear();
            let req = serde_json::json!({"id": 1, "method": "session.list", "params": {}});
            write_half
                .write_all(format!("{req}\n").as_bytes())
                .await
                .unwrap();
            reader.read_line(&mut line).await.unwrap();
            let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(resp["ok"], true);
            assert_eq!(resp["result"][0]["id"], "sess_test");
        }

        drop(server);
    }
}
