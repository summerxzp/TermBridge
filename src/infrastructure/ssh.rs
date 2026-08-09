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

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use russh::client::{self, Handle};
use russh::keys::ssh_key;
use russh::keys::PrivateKeyWithHashAlg;
use russh::{ChannelMsg, Disconnect};

use crate::domain::provider::{
    ControlKey, OpenTerminalRequest, PtySize, TerminalHandle, TerminalProvider, TermError,
};

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

/// russh client Handler：Phase 0-C 接受任意 host key（Phase 1 改 known_hosts 校验，§5.5）。
pub(crate) struct SshClientHandler;

impl client::Handler for SshClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // ⚠️ Phase 0-C：原型接受任意 host key。Phase 1 必须改 known_hosts 校验。
        tracing::warn!("host key verification SKIPPED (Phase 0-C prototype)");
        Ok(true)
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
        let mut session = client::connect(config, addr, SshClientHandler)
            .await
            .map_err(|e| map_connect_err(e, &host.name))?;

        // 认证：优先 IdentityFile，失败回退 ssh-agent（NoneAuth）。
        // Phase 0-C：IdentityFile 路径，公钥认证。
        let authed = if let Some(key_path) = &host.identity_file {
            authenticate_with_key(&mut session, &host.user, key_path).await?
        } else {
            // 无 IdentityFile：尝试 ssh-agent（publickey_offering）。
            // Phase 0-C 简化：直接报错要求配置 IdentityFile。
            tracing::warn!(host=%host.name, "no IdentityFile resolved; ssh-agent auth not implemented in Phase 0-C");
            return Err(TermError::AuthFailed);
        };

        if !authed {
            return Err(TermError::AuthFailed);
        }
        tracing::info!(host=%host.name, "ssh authenticated");

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
pub struct SshTerminalHandle {
    /// 读半：read task 独占 `wait()`。用 tokio::sync::Mutex（async-aware，guard 是 Send），
    /// 允许跨 await 持锁。实际只有 PTY read task 调用，无竞争。
    reader: tokio::sync::Mutex<russh::ChannelReadHalf>,
    /// 写半：`data_bytes` / `eof` / `window_change` 都是 `&self`，无需 Mutex。
    writer: russh::ChannelWriteHalf<client::Msg>,
    /// SSH session handle。close() 时 disconnect。
    session: Mutex<Option<Handle<SshClientHandler>>>,
}

impl SshTerminalHandle {
    fn new(channel: russh::Channel<client::Msg>, session: Handle<SshClientHandler>) -> Self {
        let (reader, writer) = channel.split();
        Self {
            reader: tokio::sync::Mutex::new(reader),
            writer,
            session: Mutex::new(Some(session)),
        }
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
        // 1. channel eof（writer 的 &self 方法，无需 take）
        let _ = self.writer.eof().await;

        // 2. session disconnect（take 出来再 await，避免跨 await 持锁）
        let session = self.session.lock().take();
        if let Some(session) = session {
            let _ = session
                .disconnect(Disconnect::ByApplication, "termbridge close", "en")
                .await;
        }
        Ok(())
    }
}
