//! ControlServer —— 本地 IPC server（ADR-0018）。
//!
//! 监听 Unix socket / Named Pipe，接受 CLI/GUI 连接，
//! 验证 HELLO token，处理 JSON-RPC 请求。
//!
//! 传输层与业务逻辑通过 ControlHandler trait 解耦。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(target_os = "windows")]
use tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tokio::net::{UnixListener, UnixStream};

use super::handler::ControlHandler;
use super::instance::InstanceRegistry;
use super::proto::{
    ControlError, ControlRequest, ControlResponse, HelloRequest, HelloResponse,
    SetApprovalModeParams,
};

/// ControlServer。
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

        #[cfg(target_os = "linux")]
        {
            Self::start_unix(handler, endpoint, token).await
        }
        #[cfg(target_os = "macos")]
        {
            Self::start_unix(handler, endpoint, token).await
        }
        // Windows：Named Pipe + 当前用户 SID DACL（ADR-0018 修订，原 TCP loopback）
        #[cfg(target_os = "windows")]
        {
            Self::start_named_pipe(handler, endpoint, token).await
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

    /// Named Pipe 实现（Windows，ADR-0018 修订）。
    ///
    /// 端点 `\\.\pipe\termbridge-mcp-<pid>-<rand>` 由 `generate_endpoint()` 生成，
    /// 直接作为 pipe 名使用。安全核心：pipe 带 DACL（仅 SYSTEM / Administrators /
    /// 当前用户 SID 可连接），替代原 TCP loopback（任意本地用户可连）。
    /// 认证（HELLO token + 限速）保持不变，DACL 是新增的传输层防线。
    #[cfg(target_os = "windows")]
    async fn start_named_pipe(
        handler: Arc<dyn ControlHandler>,
        endpoint: String,
        token: String,
    ) -> std::io::Result<Self> {
        // 第一个实例带 FILE_FLAG_FIRST_PIPE_INSTANCE：若同名 pipe 已存在
        // （其他进程抢占/squat），CreateNamedPipeW 返回 ERROR_ACCESS_DENIED，
        // 防止客户端误连到他人实例
        let server = create_pipe_instance(&endpoint, true)?;

        tracing::info!(endpoint = %endpoint, "Control IPC: Named Pipe listening");

        let registry = InstanceRegistry::register(endpoint.clone(), token.clone())?;

        let listen_task = tokio::spawn(async move {
            Self::accept_loop_named_pipe(server, endpoint, handler, token).await;
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

    /// Named Pipe accept 循环（Windows）。
    ///
    /// 标准 named pipe 模式：`connect().await` 等待客户端连接当前实例；
    /// 连接建立后把当前实例交给连接处理 task，立即创建下一实例供后续
    /// 客户端连接（同一时刻只有一个实例处于"等待连接"状态）。
    #[cfg(target_os = "windows")]
    async fn accept_loop_named_pipe(
        mut server: NamedPipeServer,
        endpoint: String,
        handler: Arc<dyn ControlHandler>,
        token: String,
    ) {
        loop {
            // 等待客户端连接当前实例
            if let Err(e) = server.connect().await {
                tracing::warn!(error = %e, "Control IPC pipe connect error");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }

            // 当前实例已被客户端占用：先创建下一实例（后续实例不带
            // FILE_FLAG_FIRST_PIPE_INSTANCE，同名 pipe 由本进程持有），
            // 再把当前实例交给连接 task
            let next = match create_pipe_instance(&endpoint, false) {
                Ok(s) => s,
                Err(e) => {
                    // 无法创建新实例意味着后续客户端无法连接，监听终止
                    tracing::error!(
                        error = %e,
                        endpoint = %endpoint,
                        "Control IPC: create pipe instance failed, listener aborting"
                    );
                    return;
                }
            };

            let h = Arc::clone(&handler);
            let t = token.clone();
            tokio::spawn(async move {
                if let Err(e) = Self::handle_connection_pipe(server, h, t).await {
                    tracing::warn!(error = %e, "Control IPC connection error");
                }
            });
            server = next;
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

    /// 处理单个 Named Pipe 连接（Windows）。
    #[cfg(target_os = "windows")]
    async fn handle_connection_pipe(
        stream: NamedPipeServer,
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
        //    威胁模型：Unix socket 为 0600、Windows Named Pipe 带 DACL（仅当前
        //    用户/SYSTEM/Administrators 可连），但同用户的其他本地进程仍可连接
        //    并对认证 token 做暴力枚举。纵深防御：
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
        // Windows Named Pipe：\\.\pipe\termbridge-mcp-<pid>-<rand>（ADR-0018 修订后
        // 即真实端点，带当前用户 SID DACL；此前 TCP loopback 版本会丢弃此值）
        format!("\\\\.\\pipe\\termbridge-mcp-{id}")
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Windows Named Pipe 辅助（ADR-0018 修订：DACL 限制当前用户 SID）
// ───────────────────────────────────────────────────────────────────────────

/// Rust 字符串转以 NUL 结尾的 UTF-16（Win32 W 系列 API 入参）。
#[cfg(target_os = "windows")]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// NUL 结尾的 UTF-16 指针转 Rust String（Win32 API 出参，只读不接管内存）。
#[cfg(target_os = "windows")]
fn wide_ptr_to_string(ws: windows_sys::core::PWSTR) -> String {
    // SAFETY: ws 指向 NUL 结尾的 UTF-16 缓冲区（Win32 API 返回值约定），
    // 仅在拷贝出 String 期间读取
    unsafe {
        let mut len = 0usize;
        while *ws.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ws, len))
    }
}

/// 打开的进程 token 句柄 RAII（确保 CloseHandle 恰好一次）。
#[cfg(target_os = "windows")]
struct TokenHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
impl Drop for TokenHandle {
    fn drop(&mut self) {
        // SAFETY: self.0 是 OpenProcessToken 成功返回的有效句柄，未被其他对象
        // 拥有，Drop 时恰好关闭一次
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// 获取当前进程用户的 SID 字符串（如 "S-1-5-21-..."）。
///
/// OpenProcessToken(GetCurrentProcess) → GetTokenInformation(TokenUser) →
/// ConvertSidToStringSidW。用于构建 pipe DACL（ADR-0018 §2.6）。
#[cfg(target_os = "windows")]
fn current_user_sid() -> std::io::Result<String> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: GetCurrentProcess 仅返回当前进程伪句柄，无副作用，无需关闭
    let process = unsafe { GetCurrentProcess() };

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: process 为有效进程句柄；token 指向合法栈变量；成功返回的句柄
    // 立即交给 TokenHandle RAII，所有路径均会关闭
    let ok = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let _token_guard = TokenHandle(token);

    // 第一次调用探测所需缓冲区长度（失败 + ERROR_INSUFFICIENT_BUFFER 属预期路径）
    let mut len = 0u32;
    // SAFETY: token 有效；缓冲区为 null、长度为 0，仅取长度
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
    }
    if len == 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut buf = vec![0u8; len as usize];
    // SAFETY: token 有效；buf 长度由上一步探测得到，足以容纳 TOKEN_USER
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: buf 已被成功写入完整 TOKEN_USER；User.Sid 指向有效 SID，
    // 生命周期覆盖本次调用
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };

    let mut sid_w = std::ptr::null_mut();
    // SAFETY: sid 来自有效的 TOKEN_USER；sid_w 由 API 分配，需 LocalFree 释放
    let ok = unsafe {
        windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW(
            sid, &mut sid_w,
        )
    };
    if ok == 0 || sid_w.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let sid_str = wide_ptr_to_string(sid_w);
    // SAFETY: sid_w 由 ConvertSidToStringSidW 分配，按文档要求用 LocalFree
    // 释放（内容已拷贝，释放后不再使用）
    unsafe {
        windows_sys::Win32::Foundation::LocalFree(sid_w.cast());
    }
    Ok(sid_str)
}

/// 构建 pipe 的 SDDL 字符串。
///
/// `D:P` = protected DACL（不继承父对象 ACE，防止环境级 ACE 意外放宽）：
/// - `(A;;GA;;;SY)`：SYSTEM 完全访问
/// - `(A;;GA;;;BA)`：Administrators 完全访问
/// - `(A;;GA;;;<当前用户 SID>)`：实例所有者完全访问
///
/// 刻意不含 Everyone（WD）/ ANONYMOUS（AN）ACE：原 TCP loopback 任意本地用户
/// 可连的跨用户暴露面就此关闭（ADR-0018 §2.6 "Named Pipe 限制当前用户 SID"）。
#[cfg(target_os = "windows")]
fn build_pipe_sddl() -> std::io::Result<String> {
    let sid = current_user_sid()?;
    Ok(format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})"))
}

/// 带 RAII 释放的 SECURITY_ATTRIBUTES（SD 由 SDDL 转换而来，LocalFree 释放）。
#[cfg(target_os = "windows")]
struct PipeSecurityAttributes {
    attrs: windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
}

#[cfg(target_os = "windows")]
impl Drop for PipeSecurityAttributes {
    fn drop(&mut self) {
        // SAFETY: lpSecurityDescriptor 由 ConvertStringSecurityDescriptor...
        // 分配，按文档要求用 LocalFree 释放；Drop 恰好一次
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(
                self.attrs.lpSecurityDescriptor.cast(),
            );
        }
    }
}

/// 从 SDDL 字符串构建 SECURITY_ATTRIBUTES。
#[cfg(target_os = "windows")]
fn build_pipe_security_attributes(sddl: &str) -> std::io::Result<PipeSecurityAttributes> {
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    let sddl_w = to_wide(sddl);
    let mut sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: sddl_w 为 NUL 结尾 UTF-16；sd 由 API 分配（交给 RAII 释放）；
    // 不需要长度出参，传 null
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_w.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || sd.is_null() {
        return Err(std::io::Error::last_os_error());
    }

    Ok(PipeSecurityAttributes {
        attrs: SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        },
    })
}

/// 创建一个 Named Pipe server 实例（tokio 异步包装）。
///
/// tokio 的 `ServerOptions` 不暴露 SECURITY_ATTRIBUTES，无法带 DACL 创建，
/// 因此直接调用 CreateNamedPipeW 再用 `NamedPipeServer::from_raw_handle`
/// 包装（需在 tokio runtime 内调用）。
///
/// - `first = true`：附加 FILE_FLAG_FIRST_PIPE_INSTANCE，要求本实例是该
///   pipe 名的第一个实例；同名 pipe 已存在（含其他进程/用户抢占）时返回
///   ERROR_ACCESS_DENIED，防止客户端误连到他人实例（防 squat）
/// - `first = false`：accept loop 的后续实例（同名 pipe 已由本进程持有）
#[cfg(target_os = "windows")]
fn create_pipe_instance(endpoint: &str, first: bool) -> std::io::Result<NamedPipeServer> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PIPE_WAIT,
    };

    let sddl = build_pipe_sddl()?;
    let sa = build_pipe_security_attributes(&sddl)?;
    let name_w = to_wide(endpoint);

    // OVERLAPPED 是 tokio 异步 IO 的前提；DUPLEX = 双向（HELLO 响应 + 请求/响应）
    let open_mode = PIPE_ACCESS_DUPLEX
        | FILE_FLAG_OVERLAPPED
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    // BYTE 模式与 handle_connection 的行协议（newline-delimited JSON）匹配
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT;

    // SAFETY: name_w / sa 为合法栈数据且在调用期间存活（SD 内容由系统复制）；
    // 返回句柄为新建 pipe 实例的所有权句柄，失败时为 INVALID_HANDLE_VALUE
    let handle = unsafe {
        CreateNamedPipeW(
            name_w.as_ptr(),
            open_mode,
            pipe_mode,
            // accept loop 需并发持有"已连接 + 待连接"两个实例，不能限制为 1
            PIPE_UNLIMITED_INSTANCES,
            4096, // nOutBufferSize
            4096, // nInBufferSize
            0,    // nDefaultTimeOut（0 = 默认 50ms 等待，仅 WAIT 模式有意义）
            &sa.attrs,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: handle 是 CreateNamedPipeW 新建、未被其他对象拥有的所有权句柄，
    // 且已置 FILE_FLAG_OVERLAPPED（tokio 注册 IOCP 的要求）；所有权转移给
    // NamedPipeServer，由其负责关闭
    unsafe { NamedPipeServer::from_raw_handle(handle) }
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

        // Windows Named Pipe 测试（ADR-0018 修订后传输层）
        #[cfg(target_os = "windows")]
        {
            use tokio::net::windows::named_pipe::ClientOptions;

            // endpoint 即 pipe 名（\\.\pipe\termbridge-mcp-<pid>-<rand>）
            let stream =
                ClientOptions::new().open(endpoint).expect("open pipe failed");
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

    /// HELLO token 错误应被拒绝（Windows Named Pipe 路径）。
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn control_server_hello_reject_windows_pipe() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::windows::named_pipe::ClientOptions;

        let handler = Arc::new(StubHandler { sessions: vec![] });
        let server = ControlServer::start(handler)
            .await
            .expect("start failed");
        let endpoint = server.instance_info().endpoint.clone();

        let stream = ClientOptions::new().open(&endpoint).expect("open failed");
        let (read_half, mut write_half) = tokio::io::split(stream);

        // 错误 token 的 HELLO
        let hello = serde_json::json!({"token": "wrong-token"});
        write_half
            .write_all(format!("{hello}\n").as_bytes())
            .await
            .unwrap();

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let resp: HelloResponse = serde_json::from_str(line.trim()).unwrap();
        assert!(!resp.ok, "错误 token 的 HELLO 应被拒绝");
        assert!(resp.error.is_some());

        drop(server);
    }

    /// SDDL 应包含当前用户 SID，且不含 Everyone/Anonymous ACE（安全核心）。
    #[cfg(target_os = "windows")]
    #[test]
    fn pipe_sddl_contains_current_user_sid() {
        let sddl = build_pipe_sddl().expect("build sddl failed");
        let sid = current_user_sid().expect("get sid failed");
        assert!(
            sddl.contains(&sid),
            "SDDL 应包含当前用户 SID: sddl={sddl} sid={sid}"
        );
        // 显式 ACE 白名单：SY / BA / 当前用户，不允许 Everyone(WD)/AN
        assert!(sddl.contains("(A;;GA;;;SY)"), "SYSTEM ACE 缺失: {sddl}");
        assert!(sddl.contains("(A;;GA;;;BA)"), "Administrators ACE 缺失: {sddl}");
        assert!(!sddl.contains(";;WD"), "SDDL 不应含 Everyone ACE: {sddl}");
        assert!(!sddl.contains(";;AN"), "SDDL 不应含 Anonymous ACE: {sddl}");
        assert!(sddl.starts_with("D:P"), "DACL 应为 protected（不继承）: {sddl}");
    }

    /// Windows pipe 端点格式：\\.\pipe\termbridge-mcp-<pid>-<rand>。
    #[cfg(target_os = "windows")]
    #[test]
    fn generate_endpoint_windows_pipe_format() {
        let e = generate_endpoint();
        assert!(
            e.starts_with(&format!("\\\\.\\pipe\\termbridge-mcp-{}-", std::process::id())),
            "端点应为 pipe 名且含 pid: {e}"
        );
    }

    /// 同名 pipe 的第二个 first-instance 创建应失败（ERROR_ACCESS_DENIED）。
    ///
    /// 验证 FILE_FLAG_FIRST_PIPE_INSTANCE 语义：防其他进程抢占同名 pipe
    /// （squat）。注意：同进程内 CreateNamedPipeW 同名 second-instance 是
    /// 允许的，但带 FIRST_PIPE_INSTANCE 标志的重复创建会被拒绝。
    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn pipe_first_instance_conflict_rejected() {
        let name = format!(
            "\\\\.\\pipe\\termbridge-mcp-test-{}-{:x}",
            std::process::id(),
            rand_for_test()
        );
        let _first = create_pipe_instance(&name, true).expect("first instance failed");

        // 同名 + FIRST_PIPE_INSTANCE：应 ERROR_ACCESS_DENIED (5)
        let err = create_pipe_instance(&name, true)
            .expect_err("second first-instance should fail");
        assert_eq!(
            err.raw_os_error(),
            Some(5), // ERROR_ACCESS_DENIED
            "期望 ERROR_ACCESS_DENIED，实际: {err}"
        );
    }

    /// 测试用随机后缀（避免与其他测试的 pipe 名碰撞）。
    #[cfg(target_os = "windows")]
    fn rand_for_test() -> u32 {
        // 与 generate_endpoint 同款写法（fill_bytes，rand 0.10）
        use rand::Rng;
        let mut b = [0u8; 4];
        rand::rng().fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }
}
