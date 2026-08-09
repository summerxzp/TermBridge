//! ssh —— russh 封装：SshProvider + SshTerminalHandle（§4.4 / §5.1）
//!
//! ```text
//! SshProvider.open(OpenTerminalRequest)
//!   → russh connect + auth (IdentityFile/ssh-agent)
//!   → channel_open_session + request_pty + request_shell
//!   → SshTerminalHandle { channel: Mutex<Channel>, session: Mutex<Handle> }
//! ```
//!
//! SshTerminalHandle.read() = channel.wait() → ChannelMsg::Data / ExtendedData / Eof
//! SshTerminalHandle.write() = channel.data()
//! SshTerminalHandle.send_control() = channel.data(ctrl_bytes)
//! SshTerminalHandle.resize() = channel.window_change()
//! SshTerminalHandle.close() = channel.eof() + session.disconnect()

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use russh::client::{self, Handle};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::agent::AgentIdentity;
use russh::keys::ssh_key;
use russh::keys::PrivateKeyWithHashAlg;
use russh::{ChannelMsg, Disconnect};
use tokio::task::JoinHandle;

use crate::domain::provider::{
    ControlKey, OpenTerminalRequest, PtySize, TerminalHandle, TerminalProvider, TermError,
};

// ───────────────────────────────────────────────────────────────────────────
// keepalive 配置常量（§7.4 Phase 1）
// ───────────────────────────────────────────────────────────────────────────

/// keepalive 间隔（秒）。借鉴 classfang 默认值（PLAN §7.4）。
pub const KEEPALIVE_INTERVAL_SECS: u64 = 10;
/// 连续无响应上限。达到后断开 session，PTY read task 检测到 EOF → Session::Lost。
pub const KEEPALIVE_MAX_MISSES: u32 = 3;

// ───────────────────────────────────────────────────────────────────────────
// SshProvider
// ───────────────────────────────────────────────────────────────────────────

/// SSH TerminalProvider（§4.4）。
///
/// 通过 russh 连接 + 认证 + 开 PTY + shell，返回 SshTerminalHandle。
pub struct SshProvider;

impl SshProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SshProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// russh client Handler：Phase 1 实现 known_hosts 严格校验（§5.5）。
///
/// 持有从 `ssh -G` 解析的 known_hosts 路径与 StrictHostKeyChecking 模式，
/// 在 `check_server_key` 中按以下策略决策：
/// - `strict == "no"`：接受任意 key（仅 WARN，不推荐）
/// - `strict == "yes"`：key 不匹配 / host 未知 → 拒绝（返回 false → 连接失败）
/// - `strict == "ask"`：MVP 阶段无 HITL UI，等同 "yes"（拒绝未知，WARN 提示）
///
/// 拒绝原因通过 `rejection` 共享给 `SshProvider::open`，用于映射为
/// `TermError::HostKeyRejected(String)`（而非普通 ConnectFailed）。
pub(crate) struct SshClientHandler {
    /// `UserKnownHostsFile` 路径（None 表示 ssh -G 未输出，无法校验）。
    known_hosts_path: Option<PathBuf>,
    /// StrictHostKeyChecking：ask / yes / no（小写）。
    strict: String,
    /// 目标主机名（known_hosts 查找 key）。
    host: String,
    /// 目标端口（影响 known_hosts 行格式 `[host]:port`）。
    port: u16,
    /// 拒绝原因槽（handler 与 provider 共享，connect 失败后读取）。
    rejection: Arc<Mutex<Option<String>>>,
}

impl SshClientHandler {
    /// 构造 handler，返回 `(handler, rejection_arc)`。
    /// `rejection_arc` 供调用方在 `client::connect` 失败后读取拒绝原因。
    pub(crate) fn new(
        known_hosts_path: Option<PathBuf>,
        strict: String,
        host: String,
        port: u16,
    ) -> (Self, Arc<Mutex<Option<String>>>) {
        let rejection = Arc::new(Mutex::new(None));
        let handler = Self {
            known_hosts_path,
            strict,
            host,
            port,
            rejection: rejection.clone(),
        };
        (handler, rejection)
    }
}

impl client::Handler for SshClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let strict = self.strict.to_ascii_lowercase();

        // "no"：跳过校验，接受任意 key（不安全，仅 WARN）
        if strict == "no" {
            tracing::warn!(
                host = %self.host,
                "StrictHostKeyChecking=no: 跳过 host key 校验（不安全）"
            );
            return Ok(true);
        }

        // 取 known_hosts 路径；None 则无法校验，strict 模式下拒绝
        let path: &Path = match self.known_hosts_path.as_deref() {
            Some(p) => p,
            None => {
                let reason = format!(
                    "无 known_hosts 路径，无法校验 host key (host={})",
                    self.host
                );
                tracing::warn!("{}", reason);
                *self.rejection.lock() = Some(reason);
                return Ok(false);
            }
        };

        // 查 known_hosts：返回 Ok(true)=匹配 / Ok(false)=host 未知或仅不同算法 /
        // Err(KeyChanged)=同算法 key 不匹配 / Err(其他)=文件读取等错误
        let check = russh::keys::check_known_hosts_path(
            &self.host,
            self.port,
            server_public_key,
            path,
        );

        match check {
            Ok(true) => {
                tracing::info!(host = %self.host, "host key 校验通过");
                Ok(true)
            }
            Ok(false) => {
                // host 不在 known_hosts（或仅有不同算法的 key 记录）
                let reason = if strict == "ask" {
                    format!(
                        "host key 未知 (host={})，StrictHostKeyChecking=ask 在 MVP 阶段视为拒绝（无 HITL UI）",
                        self.host
                    )
                } else {
                    format!(
                        "host key 未知 (host={})，StrictHostKeyChecking=yes 拒绝",
                        self.host
                    )
                };
                tracing::warn!("{}", reason);
                *self.rejection.lock() = Some(reason);
                Ok(false)
            }
            Err(russh::keys::Error::KeyChanged { line }) => {
                // 同算法但 key 不同 → 强烈怀疑 MITM，必须拒绝
                let reason = format!(
                    "host key 不匹配 (host={}, known_hosts line={})，可能遭受中间人攻击",
                    self.host, line
                );
                tracing::error!("{}", reason);
                *self.rejection.lock() = Some(reason);
                Ok(false)
            }
            Err(e) => {
                // known_hosts 文件读取/解析错误等
                let reason = format!(
                    "known_hosts 校验出错 (host={}): {}",
                    self.host, e
                );
                tracing::warn!("{}", reason);
                *self.rejection.lock() = Some(reason);
                Ok(false)
            }
        }
    }
}

#[async_trait]
impl TerminalProvider for SshProvider {
    async fn open(
        &self,
        request: OpenTerminalRequest,
    ) -> Result<Arc<dyn TerminalHandle>, TermError> {
        let host = &request.host;
        let addr = (host.hostname.as_str(), host.port);

        tracing::info!(host=%host.name, addr=?addr, user=%host.user, "ssh connecting");

        let config = Arc::new(client::Config::default());
        // 构造 handler：传入 known_hosts 路径 + strict 模式 + host 信息。
        // 用 host.hostname（纯 IP/域名）而非 host.name（可能含 "root@" 前缀），
        // 因为 known_hosts 条目按纯主机名存储（如 "192.168.88.200" 而非 "root@192.168.88.200"）。
        let (handler, rejection) = SshClientHandler::new(
            host.user_known_hosts_file.clone(),
            host.strict_host_key_checking.clone(),
            host.hostname.clone(),
            host.port,
        );
        let mut session = client::connect(config, addr, handler)
            .await
            .map_err(|e| {
                // 若 check_server_key 拒绝，rejection 槽有原因 → 映射为 HostKeyRejected
                if let Some(reason) = rejection.lock().take() {
                    tracing::warn!(host=%host.name, reason=%reason, "host key rejected");
                    TermError::HostKeyRejected(reason)
                } else {
                    map_connect_err(e, &host.name)
                }
            })?;

        // 认证：凭据优先级 SSH Agent > IdentityFile > HITL(Phase 6)
        // 1. 尝试 ssh-agent → 成功则跳过 IdentityFile
        // 2. ssh-agent 失败/不可用 → 遍历 identity_files
        // 3. 都失败 → Err(AuthFailed)
        let authed = authenticate_with_agent(&mut session, &host.user).await?;
        let via = if authed {
            "ssh-agent"
        } else {
            let ok = authenticate_with_identity_files(
                &mut session,
                &host.user,
                &host.identity_files,
            )
            .await?;
            if !ok {
                tracing::warn!(
                    host = %host.name,
                    "ssh 认证失败：ssh-agent 与 identity_files 均未通过"
                );
                return Err(TermError::AuthFailed);
            }
            "identity_file"
        };
        tracing::info!(host = %host.name, auth_via = %via, "ssh authenticated");

        // 开 channel + request PTY + request shell（§5.1）
        let channel = session
            .channel_open_session()
            .await
            .map_err(|e| TermError::ChannelError(format!("channel_open_session: {e}")))?;

        channel
            .request_pty(
                false,
                "xterm-256color",
                request.pty_size.rows as u32,
                request.pty_size.cols as u32,
                0,
                0,
                &[],
            )
            .await
            .map_err(|e| TermError::ChannelError(format!("request_pty: {e}")))?;

        channel
            .request_shell(true)
            .await
            .map_err(|e| TermError::ChannelError(format!("request_shell: {e}")))?;

        tracing::info!(host=%host.name, "pty + shell requested");

        Ok(Arc::new(SshTerminalHandle::new(channel, session)) as Arc<dyn TerminalHandle>)
    }
}

/// 用 IdentityFile 公钥认证。
async fn authenticate_with_key(
    session: &mut Handle<SshClientHandler>,
    user: &str,
    key_path: &std::path::Path,
) -> Result<bool, TermError> {
    let key_pair = russh::keys::PrivateKey::read_openssh_file(key_path)
        .map_err(|e| {
            tracing::error!(?key_path, error=%e, "failed to read identity file");
            TermError::AuthFailed
        })?;

    // russh 0.62：authenticate_publickey 需要 PrivateKeyWithHashAlg。
    // 对 RSA key 需要协商 hash algorithm；对 ed25519 等非 RSA key 传 None。
    // best_supported_rsa_hash() → Result<Option<Option<HashAlg>>>，双重 flatten 得 Option<HashAlg>。
    let hash_alg = session
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();
    let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash_alg);

    let auth_res = session
        .authenticate_publickey(user, key_with_alg)
        .await
        .map_err(|e| TermError::ChannelError(format!("authenticate_publickey: {e}")))?;
    Ok(auth_res.success())
}

/// ssh-agent 认证：连接 agent → 遍历其 identities 逐个尝试。
///
/// agent 不可用（Unix `SSH_AUTH_SOCK` 未设 / socket 不存在；Windows named pipe 不存在）
/// → 返回 `Ok(false)`，由调用方降级到 IdentityFile（不报错）。
async fn authenticate_with_agent(
    session: &mut Handle<SshClientHandler>,
    user: &str,
) -> Result<bool, TermError> {
    let mut agent = match connect_agent().await {
        Some(a) => a,
        None => return Ok(false),
    };
    agent_auth_loop(session, user, &mut agent).await
}

/// 用已连接的 ssh-agent 持有的 identities 逐个尝试公钥/证书认证。
/// 任一成功返回 true；全部失败/出错返回 false（降级到 IdentityFile）。
async fn agent_auth_loop<S>(
    session: &mut Handle<SshClientHandler>,
    user: &str,
    agent: &mut AgentClient<S>,
) -> Result<bool, TermError>
where
    S: AgentStream + Send + Unpin,
{
    let identities = match agent.request_identities().await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error=%e, "ssh-agent request_identities 失败，降级到 IdentityFile");
            return Ok(false);
        }
    };

    if identities.is_empty() {
        tracing::info!("ssh-agent 无 identities，降级到 IdentityFile");
        return Ok(false);
    }

    // 协商 RSA hash（对非 RSA key 传 None 无影响）
    let hash_alg = session
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();

    for identity in identities {
        let comment = identity.comment().to_string();
        let result = match identity {
            AgentIdentity::PublicKey { key, .. } => {
                tracing::debug!(comment=%comment, "尝试 agent 公钥认证");
                session
                    .authenticate_publickey_with(user, key, hash_alg, agent)
                    .await
            }
            AgentIdentity::Certificate { certificate, .. } => {
                tracing::debug!(comment=%comment, "尝试 agent 证书认证");
                session
                    .authenticate_certificate_with(user, certificate, hash_alg, agent)
                    .await
            }
        };
        match result {
            Ok(auth) if auth.success() => {
                tracing::info!(comment=%comment, "ssh-agent 认证成功");
                return Ok(true);
            }
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(
                    comment=%comment,
                    error=%e,
                    "agent 认证调用出错，继续下一个 identity"
                );
                continue;
            }
        }
    }

    tracing::info!("ssh-agent 全部 identities 认证失败，降级到 IdentityFile");
    Ok(false)
}

/// 连接 ssh-agent（Unix 走 `SSH_AUTH_SOCK` UDS）。
/// 不可用 → None（降级到 IdentityFile）。
#[cfg(unix)]
async fn connect_agent() -> Option<AgentClient<tokio::net::UnixStream>> {
    let sock = match std::env::var("SSH_AUTH_SOCK") {
        Ok(s) => s,
        Err(_) => {
            tracing::info!("ssh-agent 不可用（SSH_AUTH_SOCK 未设），降级到 IdentityFile");
            return None;
        }
    };
    match AgentClient::connect_uds(&sock).await {
        Ok(agent) => Some(agent),
        Err(e) => {
            tracing::info!(error=%e, "ssh-agent 不可用（UDS 连接失败），降级到 IdentityFile");
            None
        }
    }
}

/// 连接 ssh-agent（Windows 走 named pipe，默认 `\\.\pipe\openssh-ssh-agent`）。
/// 不可用 → None（降级到 IdentityFile）。
#[cfg(windows)]
async fn connect_agent() -> Option<AgentClient<tokio::net::windows::named_pipe::NamedPipeClient>> {
    connect_agent_pipe(r"\\.\pipe\openssh-ssh-agent").await
}

/// 连接指定 named pipe 的 ssh-agent（Windows）。不可用 → None。
#[cfg(windows)]
async fn connect_agent_pipe(
    pipe: &str,
) -> Option<AgentClient<tokio::net::windows::named_pipe::NamedPipeClient>> {
    match AgentClient::connect_named_pipe(pipe).await {
        Ok(agent) => Some(agent),
        Err(e) => {
            tracing::info!(error=%e, "ssh-agent 不可用（named pipe 连接失败），降级到 IdentityFile");
            None
        }
    }
}

/// 遍历多个 IdentityFile 公钥认证。任一成功返回 true；全部失败返回 false。
async fn authenticate_with_identity_files(
    session: &mut Handle<SshClientHandler>,
    user: &str,
    keys: &[PathBuf],
) -> Result<bool, TermError> {
    for key_path in keys {
        match authenticate_with_key(session, user, key_path).await {
            Ok(true) => {
                tracing::info!(?key_path, "IdentityFile 认证成功");
                return Ok(true);
            }
            Ok(false) => {
                tracing::debug!(?key_path, "IdentityFile 认证失败，尝试下一个");
                continue;
            }
            Err(e) => {
                // 读 key 文件 / 调用出错：记录后继续尝试下一个（全部失败再统一报错）
                tracing::warn!(?key_path, error=%e, "IdentityFile 认证出错，尝试下一个");
                continue;
            }
        }
    }
    Ok(false)
}

/// 把 russh 连接错误映射为 TermError。
fn map_connect_err(e: russh::Error, host: &str) -> TermError {
    let msg = format!("{e}");
    tracing::warn!(host, error=%msg, "ssh connect failed");
    match e {
        russh::Error::IO(_) => TermError::ConnectFailed(msg),
        _ => TermError::ConnectFailed(msg),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// SshTerminalHandle
// ───────────────────────────────────────────────────────────────────────────

/// SSH Terminal Backend 句柄。封装 russh Channel（split 为读/写两半）+ Session Handle。
///
/// 用 `Channel::split()` 拆分：
/// - `reader: Mutex<ChannelReadHalf>` —— 只 read task 用，无写竞争
/// - `writer: ChannelWriteHalf` —— `&self` 方法，read/write/resize/close 并发无锁
///
/// 这解决了 read task 长期 `wait()` 阻塞期间 write 无法获取 channel 的问题。
///
/// Phase 1：`session` 字段从 `parking_lot::Mutex` 换为 `tokio::sync::Mutex`，
/// 因为 SFTP 操作需要在持锁状态下 `await`（`channel_open_session` 是 async）。
/// `parking_lot::Mutex` 的 guard 非 Send，无法跨 await 持有。
///
/// Phase 1 keepalive：`session` 包一层 `Arc`，让 keepalive task 持有 clone
/// 定期发 `send_ping`（等 reply）。连续 `KEEPALIVE_MAX_MISSES` 次无响应 →
/// `disconnect`，PTY read task 检测到 EOF → Session 置 Lost。
pub struct SshTerminalHandle {
    /// 读半：read task 独占 `wait()`。用 tokio::sync::Mutex（async-aware，guard 是 Send），
    /// 允许跨 await 持锁。实际只有 PTY read task 调用，无竞争。
    reader: tokio::sync::Mutex<russh::ChannelReadHalf>,
    /// 写半：`data_bytes` / `eof` / `window_change` 都是 `&self`，无需 Mutex。
    writer: russh::ChannelWriteHalf<client::Msg>,
    /// SSH session handle。close() 时 disconnect；SFTP 操作时复用开新 channel。
    /// 用 Arc 包裹让 keepalive task 持有 clone（Handle 不实现 Clone）。
    session: Arc<tokio::sync::Mutex<Option<Handle<SshClientHandler>>>>,
    /// keepalive task 句柄。close() / Drop 时 abort 防泄漏。
    keepalive_task: Mutex<Option<JoinHandle<()>>>,
}

impl SshTerminalHandle {
    fn new(channel: russh::Channel<client::Msg>, session: Handle<SshClientHandler>) -> Self {
        let (reader, writer) = channel.split();
        let session = Arc::new(tokio::sync::Mutex::new(Some(session)));

        // spawn keepalive task：定期 send_ping 检测连接活性（§7.4 Phase 1）
        let ka_session = Arc::clone(&session);
        let keepalive_task = tokio::spawn(async move {
            keepalive_loop(ka_session).await;
        });

        Self {
            reader: tokio::sync::Mutex::new(reader),
            writer,
            session,
            keepalive_task: Mutex::new(Some(keepalive_task)),
        }
    }

    /// 在当前 SSH session 上开新 SFTP channel，返回 `SftpProvider`。
    ///
    /// 实现：锁 `session` → `channel_open_session` → `request_subsystem("sftp")` →
    /// `SftpSession::new`。SFTP channel 独立于 PTY channel，互不影响。
    ///
    /// Phase 1 不做 channel 池：每次调用都开新 channel，调用方负责 drop（关闭）。
    /// 持锁期间不能并发 SFTP（同一 session 串行），但可并发 PTY 写（writer 无锁）。
    pub async fn open_sftp_provider(
        &self,
    ) -> Result<crate::infrastructure::sftp::SftpProvider, TermError> {
        let guard = self.session.lock().await;
        let session = guard.as_ref().ok_or_else(|| {
            // session 已 take（close 过）→ SessionClosed
            TermError::SessionClosed("ssh session handle already taken".into())
        })?;
        crate::infrastructure::sftp::SftpProvider::open(session).await
    }
}

#[async_trait]
impl TerminalHandle for SshTerminalHandle {
    async fn read(&self) -> Result<Option<Bytes>, TermError> {
        // 只有 PTY read task 调用，无竞争。wait() 阻塞期间 writer 可并发写。
        let mut reader = self.reader.lock().await;
        loop {
            match reader.wait().await {
                Some(ChannelMsg::Data { data }) => break Ok(Some(data)),
                Some(ChannelMsg::ExtendedData { data, .. }) => break Ok(Some(data)),
                Some(ChannelMsg::Eof) => break Ok(None),
                Some(ChannelMsg::ExitStatus { .. }) => continue,
                Some(_) => continue,
                None => break Ok(None),
            }
        }
    }

    async fn write(&self, data: &[u8]) -> Result<(), TermError> {
        self.writer
            .data_bytes(Bytes::copy_from_slice(data))
            .await
            .map_err(|e| TermError::ChannelError(format!("write: {e}")))
    }

    async fn send_control(&self, c: ControlKey) -> Result<(), TermError> {
        self.writer
            .data_bytes(Bytes::copy_from_slice(c.as_bytes()))
            .await
            .map_err(|e| TermError::ChannelError(format!("send_control: {e}")))
    }

    async fn resize(&self, size: PtySize) -> Result<(), TermError> {
        self.writer
            .window_change(size.rows as u32, size.cols as u32, 0, 0)
            .await
            .map_err(|e| TermError::ChannelError(format!("resize: {e}")))
    }

    async fn close(&self) -> Result<(), TermError> {
        // 0. abort keepalive task（防泄漏）
        if let Some(task) = self.keepalive_task.lock().take() {
            task.abort();
        }

        // 1. channel eof（writer 的 &self 方法，无需 take）
        let _ = self.writer.eof().await;

        // 2. session disconnect（take 出来再 await，避免跨 await 持锁）
        // Phase 1：session 改 tokio::sync::Mutex，需 .await 取锁。
        let session = self.session.lock().await.take();
        if let Some(session) = session {
            let _ = session
                .disconnect(Disconnect::ByApplication, "termbridge close", "en")
                .await;
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl Drop for SshTerminalHandle {
    fn drop(&mut self) {
        // 兜底：drop 时若 keepalive task 还在，abort 掉防泄漏
        if let Some(task) = self.keepalive_task.lock().take() {
            task.abort();
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// keepalive task（§7.4 Phase 1）
// ───────────────────────────────────────────────────────────────────────────

/// keepalive 循环：每 `KEEPALIVE_INTERVAL_SECS` 秒发 `send_ping`（等 reply）。
///
/// - ping 在 `KEEPALIVE_INTERVAL_SECS` 内返回 Ok → 连接健康，重置 miss 计数
/// - ping 超时 / 返回 Err → miss +1
/// - 连续 `KEEPALIVE_MAX_MISSES` 次 miss → take session 并 disconnect，
///   PTY read task 随后检测到 EOF → Session 置 Lost
///
/// 借鉴 classfang 默认值（10s 间隔 + 3 次上限）解决半开 socket / NAT 超时问题。
async fn keepalive_loop(session: Arc<tokio::sync::Mutex<Option<Handle<SshClientHandler>>>>) {
    let mut misses: u32 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(KEEPALIVE_INTERVAL_SECS)).await;

        // 锁 session 取引用，发 ping（持锁期间 await；ping 正常时毫秒级返回）
        let result = {
            let guard = session.lock().await;
            match guard.as_ref() {
                Some(s) => tokio::time::timeout(
                    Duration::from_secs(KEEPALIVE_INTERVAL_SECS),
                    s.send_ping(),
                )
                .await,
                None => break, // session 已 take（close 过），退出
            }
        }; // guard 在此释放

        let ok = match result {
            Ok(Ok(_)) => true,
            Ok(Err(_)) => false,
            Err(_) => false, // timeout
        };

        if ok {
            misses = 0;
        } else {
            misses += 1;
            tracing::warn!(misses, max = KEEPALIVE_MAX_MISSES, "keepalive ping 失败/超时");
            if misses >= KEEPALIVE_MAX_MISSES {
                tracing::warn!(
                    misses,
                    max = KEEPALIVE_MAX_MISSES,
                    "keepalive 连续无响应达到上限，断开 session"
                );
                // take session 并 disconnect，触发 PTY read task 检测 EOF → Session::Lost
                if let Some(s) = session.lock().await.take() {
                    let _ = s
                        .disconnect(Disconnect::ByApplication, "keepalive timeout", "en")
                        .await;
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::client::Handler;
    use russh::keys::parse_public_key_base64;

    /// 测试用 ed25519 公钥 A（来自 russh keys 测试 fixtures）。
    const TEST_KEY_A: &str =
        "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    /// 测试用 ed25519 公钥 B（与 A 同算法但不同 key，用于 mismatch 测试）。
    const TEST_KEY_B: &str =
        "AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";

    /// 写测试用 known_hosts 文件到 temp_dir，返回路径。调用方负责清理。
    fn write_known_hosts(content: &str, suffix: &str) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("termbridge_ssh_test_known_hosts_{suffix}"));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[tokio::test]
    async fn check_server_key_accepts_matching_key_in_yes_mode() {
        // known_hosts 中记录了 myhost 的 key A，server 也返回 key A → 匹配，接受
        let content = format!("myhost ssh-ed25519 {TEST_KEY_A}\n");
        let path = write_known_hosts(&content, "match_yes");
        let (mut handler, rejection) = SshClientHandler::new(
            Some(path.clone()),
            "yes".to_string(),
            "myhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();

        assert!(accepted, "matching key in yes mode must be accepted");
        assert!(
            rejection.lock().is_none(),
            "no rejection reason on success"
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn check_server_key_rejects_mismatched_key_in_yes_mode() {
        // known_hosts 记录 key A，server 返回 key B（同算法不同 key）→ KeyChanged → 拒绝
        let content = format!("myhost ssh-ed25519 {TEST_KEY_A}\n");
        let path = write_known_hosts(&content, "mismatch_yes");
        let (mut handler, rejection) = SshClientHandler::new(
            Some(path.clone()),
            "yes".to_string(),
            "myhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_B).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();

        assert!(!accepted, "mismatched key must be rejected");
        let reason = rejection.lock().take().expect("rejection reason should be set");
        assert!(
            reason.contains("不匹配"),
            "rejection reason should mention mismatch, got: {reason}"
        );
        assert!(
            reason.contains("myhost"),
            "rejection reason should mention host, got: {reason}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn check_server_key_rejects_unknown_host_in_yes_mode() {
        // known_hosts 中没有 unknownhost 的条目 → host 未知 → yes 模式拒绝
        let content = format!("otherhost ssh-ed25519 {TEST_KEY_A}\n");
        let path = write_known_hosts(&content, "unknown_yes");
        let (mut handler, rejection) = SshClientHandler::new(
            Some(path.clone()),
            "yes".to_string(),
            "unknownhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();

        assert!(!accepted, "unknown host in yes mode must be rejected");
        let reason = rejection.lock().take().expect("rejection reason should be set");
        assert!(
            reason.contains("未知"),
            "rejection reason should mention unknown, got: {reason}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn check_server_key_rejects_unknown_host_in_ask_mode() {
        // ask 模式：host 未知 → MVP 视为拒绝（无 HITL UI）
        let content = format!("otherhost ssh-ed25519 {TEST_KEY_A}\n");
        let path = write_known_hosts(&content, "unknown_ask");
        let (mut handler, rejection) = SshClientHandler::new(
            Some(path.clone()),
            "ask".to_string(),
            "unknownhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();

        assert!(!accepted, "unknown host in ask mode must be rejected (MVP)");
        let reason = rejection.lock().take().expect("rejection reason should be set");
        assert!(
            reason.contains("ask"),
            "rejection reason should mention ask mode, got: {reason}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn check_server_key_accepts_matching_key_in_ask_mode() {
        // ask 模式：匹配的 key 仍然接受
        let content = format!("myhost ssh-ed25519 {TEST_KEY_A}\n");
        let path = write_known_hosts(&content, "match_ask");
        let (mut handler, rejection) = SshClientHandler::new(
            Some(path.clone()),
            "ask".to_string(),
            "myhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();

        assert!(accepted, "matching key in ask mode must be accepted");
        assert!(rejection.lock().is_none(), "no rejection on match");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn check_server_key_accepts_any_key_in_no_mode() {
        // no 模式：即使 host 未知、key 不匹配也接受（仅 WARN）
        let content = format!("otherhost ssh-ed25519 {TEST_KEY_A}\n");
        let path = write_known_hosts(&content, "no_mode");
        let (mut handler, rejection) = SshClientHandler::new(
            Some(path.clone()),
            "no".to_string(),
            "unknownhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_B).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();

        assert!(accepted, "no mode must accept any key");
        assert!(
            rejection.lock().is_none(),
            "no rejection reason in no mode"
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn check_server_key_rejects_when_known_hosts_path_none() {
        // strict 模式但无 known_hosts 路径 → 无法校验，拒绝
        let (mut handler, rejection) =
            SshClientHandler::new(None, "yes".to_string(), "myhost".to_string(), 22);

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();

        assert!(!accepted, "missing known_hosts path in yes mode must reject");
        let reason = rejection.lock().take().expect("rejection reason should be set");
        assert!(
            reason.contains("无 known_hosts 路径"),
            "rejection reason should mention missing path, got: {reason}"
        );
    }

    #[tokio::test]
    async fn check_server_key_accepts_matching_key_nonstandard_port() {
        // 非 22 端口：known_hosts 行格式为 [host]:port
        let content = format!("[myhost]:2222 ssh-ed25519 {TEST_KEY_A}\n");
        let path = write_known_hosts(&content, "port2222");
        let (mut handler, rejection) = SshClientHandler::new(
            Some(path.clone()),
            "yes".to_string(),
            "myhost".to_string(),
            2222,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();

        assert!(accepted, "matching key on non-standard port must be accepted");
        assert!(rejection.lock().is_none(), "no rejection on match");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn check_server_key_uppercase_strict_value_normalized() {
        // STRICTHOSTKEYCHECKING 大写值应被 to_ascii_lowercase 归一化
        let content = format!("myhost ssh-ed25519 {TEST_KEY_A}\n");
        let path = write_known_hosts(&content, "upper_strict");
        let (mut handler, _rejection) = SshClientHandler::new(
            Some(path.clone()),
            "YES".to_string(),
            "myhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();
        assert!(accepted, "uppercase YES should be treated as yes");
        std::fs::remove_file(&path).ok();
    }

    // ── ssh-agent 降级测试 ───────────────────────────────────────
    // 验证凭据优先级的关键触发点：agent 不可用时 connect_agent 返回 None，
    // 从而让 SshProvider::open 降级到 IdentityFile 遍历。
    // （完整 open 流程的优先级验证属集成测试范畴，需真实/mock SSH server。）

    #[cfg(unix)]
    #[tokio::test]
    async fn connect_agent_returns_none_when_sock_bogus() {
        // SSH_AUTH_SOCK 指向不存在的 socket → connect_uds 失败 → None（降级到 IdentityFile）
        let saved = std::env::var_os("SSH_AUTH_SOCK");
        std::env::set_var("SSH_AUTH_SOCK", "/tmp/termbridge_nonexistent_agent_sock");
        let agent = connect_agent().await;
        assert!(agent.is_none(), "bogus SSH_AUTH_SOCK 应返回 None 以降级");
        match saved {
            Some(v) => std::env::set_var("SSH_AUTH_SOCK", v),
            None => std::env::remove_var("SSH_AUTH_SOCK"),
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn connect_agent_pipe_returns_none_when_pipe_missing() {
        // 不存在的 named pipe → connect_named_pipe 失败 → None（降级到 IdentityFile）
        let agent = connect_agent_pipe(r"\\.\pipe\termbridge-nonexistent-agent-test").await;
        assert!(agent.is_none(), "不存在的 named pipe 应返回 None 以降级");
    }

    // ── Phase 1：keepalive 配置常量测试 ──────────────────────────────

    #[test]
    fn keepalive_constants_have_expected_values() {
        // §7.4 Phase 1：10s 间隔 + 3 次上限（借鉴 classfang 默认值）
        assert_eq!(KEEPALIVE_INTERVAL_SECS, 10, "keepalive 间隔应为 10 秒");
        assert_eq!(KEEPALIVE_MAX_MISSES, 3, "keepalive 最大 miss 次数应为 3");
    }
}
