//! ControlServer —— 本地 IPC server（ADR-0018）。
//!
//! 监听 Unix socket / Named Pipe，接受 CLI/GUI 连接，
//! 验证 HELLO token，处理 JSON-RPC 请求。
//!
//! 传输层与业务逻辑通过 ControlHandler trait 解耦。

use std::sync::Arc;
use std::time::Duration;

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
        let endpoint = generate_endpoint();

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
        // 仅当残留 socket 已死（connect 失败）时才删除：无条件的 remove_file
        // 会在端点碰撞时把其他存活实例的 socket 一并删除（历史上端点 ID 仅
        // 24 bit，见 generate_endpoint）。若 connect 成功（存活实例占用），
        // 下方 bind 会以地址占用失败并向上返回错误。
        if std::path::Path::new(&endpoint).exists() {
            let stale = UnixStream::connect(&endpoint).await.is_err();
            if stale {
                let _ = std::fs::remove_file(&endpoint);
            }
        }

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

        // 1. HELLO 认证（第一条消息必须是 HELLO + token）。
        //
        //    威胁模型：Unix socket 为 0600，但 Windows TCP loopback 上任意本地
        //    用户都可连接，可对认证 token 做本地跨用户暴力枚举。纵深防御：
        //    - 每次失败响应前先 sleep HELLO_FAIL_DELAY，抬高枚举成本；
        //    - 每连接最多容忍 MAX_HELLO_ATTEMPTS 次失败后断开。
        //    token 本身为 128-bit CSPRNG（枚举空间不可行），限速是二道防线。
        const MAX_HELLO_ATTEMPTS: u32 = 5;
        const HELLO_FAIL_DELAY: Duration = Duration::from_millis(250);

        let mut authenticated = false;
        for attempt in 1..=MAX_HELLO_ATTEMPTS {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(()); // 连接立即关闭
            }

            // 解析失败与 token 错误同样计入失败次数（对恶意/失步客户端无差别限速）
            let parsed: Option<HelloRequest> = serde_json::from_str(line.trim()).ok();
            if parsed
                .as_ref()
                .map(|h| h.token == token)
                .unwrap_or(false)
            {
                authenticated = true;
                break;
            }

            tracing::warn!(attempt, "Control IPC: HELLO rejected");
            tokio::time::sleep(HELLO_FAIL_DELAY).await;
            let resp = HelloResponse {
                ok: false,
                error: Some(if parsed.is_none() {
                    "expected HELLO with token".into()
                } else {
                    "invalid token".into()
                }),
            };
            write_half
                .write_all(serde_json::to_string(&resp)?.as_bytes())
                .await?;
            write_half.write_all(b"\n").await?;
        }

        if !authenticated {
            tracing::warn!("Control IPC: HELLO failed too many times, closing connection");
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

/// 生成随机认证 token（32 字符 hex = 128 bit CSPRNG 熵）。
///
/// Control plane 的唯一认证手段（HELLO 通过后可 `session.set_approval_mode`
/// 等修改会话策略），token 必须不可预测。早期实现用 `时间戳 ^ pid`，熵为零，
/// 本地攻击者可直接推断，已改为 OS CSPRNG。
fn generate_token() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 生成 IPC 端点路径。
///
/// 端点 ID 含完整 pid + 随机后缀，保证跨进程唯一：早期实现取 token 前 6 个
/// hex 字符（仅 24 bit），pid ≥ 2^24 时不同进程可能碰撞，`start_unix` 的无条件
/// remove_file 会删掉其他存活实例的 socket（现已改为 liveness probe，双保险）。
fn generate_endpoint() -> String {
    use rand::Rng;
    let mut suffix = [0u8; 4];
    rand::rng().fill_bytes(&mut suffix);
    let id = format!(
        "{}-{}",
        std::process::id(),
        suffix.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
        // $XDG_RUNTIME_DIR/termbridge/mcp-<pid>-<rand>.sock
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

    #[test]
    fn generate_token_is_128bit_csprng_hex() {
        let t1 = generate_token();
        let t2 = generate_token();
        assert_eq!(t1.len(), 32, "token 应为 32 个 hex 字符（128 bit）");
        assert!(
            t1.chars().all(|c| c.is_ascii_hexdigit()),
            "token 应全部为 hex 字符: {t1}"
        );
        assert_ne!(t1, t2, "CSPRNG token 不应重复");
    }

    #[test]
    fn generate_endpoint_is_unique_per_process() {
        // 端点 ID 含 pid + 随机后缀：同进程内两次生成不应碰撞（历史实现取
        // token 前 6 字符，仅 24 bit，跨进程碰撞会误删其他实例的 socket）
        let e1 = generate_endpoint();
        let e2 = generate_endpoint();
        assert_ne!(e1, e2);
        assert!(
            e1.contains(&format!("mcp-{}-", std::process::id())),
            "端点应包含 pid: {e1}"
        );
    }

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
