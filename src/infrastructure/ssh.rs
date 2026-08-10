//! ssh —— russh 封装：SshProvider + SshTerminalHandle（§4.4 / §5.1）
//!
//! ```text
//! SshProvider.open(OpenTerminalRequest)
//!   → connect_session(host, depth=0)
//!       ├─ 直连：client::connect + auth
//!       └─ ProxyJump：递归 connect_session(bastion) → channel_open_direct_tcpip
//!                    → channel.into_stream() → client::connect_stream + auth
//!   → channel_open_session + request_pty + request_shell
//!   → SshTerminalHandle { channel, session, bastion_sessions }
//! ```
//!
//! SshTerminalHandle.read() = channel.wait() → ChannelMsg::Data / ExtendedData / Eof
//! SshTerminalHandle.write() = channel.data()
//! SshTerminalHandle.send_control() = channel.data(ctrl_bytes)
//! SshTerminalHandle.resize() = channel.window_change()
//! SshTerminalHandle.close() = channel.eof() + session.disconnect() + bastions.disconnect()
//!
//! # Phase 2 ProxyJump 研究结论（russh 0.62.5）
//!
//! **russh 0.62 无内置 ProxyJump / proxy 连接 API**。lib.rs 文档建议用
//! `russh-config` crate 的 `Stream::tcp_connect` / `Stream::proxy_command`，
//! 但那仅是 ProxyCommand（exec 子进程）的薄封装，**不是** SSH-over-SSH 的原生 ProxyJump。
//!
//! russh 0.62 提供的可用原语：
//! - `Handle::channel_open_direct_tcpip(host, port, originator_addr, originator_port)`
//!   —— RFC4254 §7 direct-tcpip channel，可在已认证 session 上开 TCP 转发 channel。
//! - `Channel::into_stream()` —— 将 channel 转为 `AsyncRead + AsyncWrite + Unpin + Send`。
//! - `client::connect_stream(config, stream, handler)` —— 在任意 AsyncRead+AsyncWrite
//!   之上跑 SSH client（kex + auth + channel），返回 `Handle`。
//!
//! **实现方案**（手动 SSH-over-SSH 隧道）：
//! 1. `ssh -G` 解析目标 Host（含 `proxyjump` 字段，Phase 0-C 已实现）。
//! 2. 解析 `proxyjump` 为 `ProxyJumpTarget { user, host, port }`（本模块 sshconfig.rs）。
//! 3. 递归 `connect_session` 连跳板机（跳板机本身经 `ssh -G` 解析，可再嵌套）。
//! 4. 在跳板机 session 上 `channel_open_direct_tcpip(target_host, target_port, ...)`。
//! 5. `channel.into_stream()` → `client::connect_stream` 在隧道上建目标 SSH session。
//! 6. 在目标 session 上认证（ssh-agent / IdentityFile）。
//! 7. 返回目标 Handle + 跳板机 Handle 链（必须同生命周期，否则隧道断开）。
//!
//! **SOCKS 支持延期至 Phase 5**：russh 不原生支持 SOCKS，需类似手动实现
//! （SOCKS5 协商 → direct-tcpip）。企业场景 ProxyJump 已覆盖 90%+ 需求，
//! SOCKS 留待 Phase 5（多目标路由 / 审计代理）一并处理。

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
    ControlKey, Host, OpenTerminalRequest, PtySize, TerminalHandle, TerminalProvider, TermError,
};
use crate::infrastructure::sshconfig;

// ───────────────────────────────────────────────────────────────────────────
// keepalive 配置常量（§7.4 Phase 1）
// ───────────────────────────────────────────────────────────────────────────

/// keepalive 间隔（秒）。借鉴 classfang 默认值（PLAN §7.4）。
pub const KEEPALIVE_INTERVAL_SECS: u64 = 10;
/// 连续无响应上限。达到后断开 session，PTY read task 检测到 EOF → Session::Lost。
pub const KEEPALIVE_MAX_MISSES: u32 = 3;

// ───────────────────────────────────────────────────────────────────────────
// ProxyJump 配置常量（§7.4 Phase 2）
// ───────────────────────────────────────────────────────────────────────────

/// ProxyJump 递归深度上限（§7.4 Phase 2）。
///
/// 防止循环引用（A 的 proxyjump 是 B，B 的 proxyjump 又是 A）导致无限递归。
/// 单跳 bastion 最常见；允许 3 跳覆盖 "堡垒→中间跳板→目标" 罕见场景。
/// 超限返回 `InvalidArgument` 错误。
pub const MAX_PROXY_DEPTH: usize = 3;

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

/// russh client Handler：Phase 1 实现 known_hosts 严格校验（§5.5），Phase 2 扩展 TOFU + 多文件。
///
/// 持有从 `ssh -G` 解析的 known_hosts 路径列表与 StrictHostKeyChecking 模式，
/// 在 `check_server_key` 中按以下策略决策：
/// - `strict == "no"`：接受任意 key（仅 WARN，不推荐）
/// - `strict == "yes"`：key 不匹配 / host 未知 → 拒绝（返回 false → 连接失败）
/// - `strict == "ask"`：MVP 阶段无 HITL UI，等同 "yes"（拒绝未知，WARN 提示）
/// - `strict == "accept-new"`（Phase 2 TOFU）：host 未知 → 自动添加 key 到首个 known_hosts 文件 → 接受；
///   host 已知 → 正常校验（匹配则接受，不匹配仍拒绝）
///
/// Phase 2 多文件支持：`known_hosts_paths` 为 `Vec<PathBuf>`（OpenSSH 默认
/// `~/.ssh/known_hosts ~/.ssh/known_hosts2`），校验时遍历所有文件查找；
/// TOFU 添加时写入首个路径。空 Vec 表示 ssh -G 未输出 `userknownhostsfile`，
/// strict 模式下拒绝。
///
/// 拒绝原因通过 `rejection` 共享给 `SshProvider::open`，用于映射为
/// `TermError::HostKeyRejected(String)`（而非普通 ConnectFailed）。
pub struct SshClientHandler {
    /// `UserKnownHostsFile` 路径列表（已展开 ~，保留全部空格分隔路径）。
    /// 空 Vec 表示 ssh -G 未输出该字段，strict 模式下拒绝。
    known_hosts_paths: Vec<PathBuf>,
    /// StrictHostKeyChecking：ask / yes / no / accept-new（小写）。
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
        known_hosts_paths: Vec<PathBuf>,
        strict: String,
        host: String,
        port: u16,
    ) -> (Self, Arc<Mutex<Option<String>>>) {
        let rejection = Arc::new(Mutex::new(None));
        let handler = Self {
            known_hosts_paths,
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

        // 空 known_hosts 路径列表 → 无法校验，strict 模式下拒绝
        if self.known_hosts_paths.is_empty() {
            let reason = format!(
                "无 known_hosts 路径，无法校验 host key (host={})",
                self.host
            );
            tracing::warn!("{}", reason);
            *self.rejection.lock() = Some(reason);
            return Ok(false);
        }

        // Phase 2 多文件遍历：依次查每个 known_hosts 文件，聚合结果。
        // - 任一文件 Ok(true) → 匹配，接受
        // - 任一文件 Err(KeyChanged) → 同算法 key 不匹配（疑似 MITM），记录行号，
        //   遍历完无匹配则拒绝（KeyChanged 优先级高于 "未知"）
        // - 全部 Ok(false) → host 在所有文件中都未知
        // - 单文件读取/解析错误 → WARN 并跳过该文件（不影响其他文件结果）
        let mut found_match = false;
        let mut key_changed_line: Option<usize> = None;

        for path in &self.known_hosts_paths {
            match russh::keys::check_known_hosts_path(
                &self.host,
                self.port,
                server_public_key,
                path,
            ) {
                Ok(true) => {
                    found_match = true;
                    break; // 任一匹配即可接受，无需继续
                }
                Ok(false) => {
                    // 本文件未找到该 host（或仅有不同算法 key），继续查下一个文件
                }
                Err(russh::keys::Error::KeyChanged { line }) => {
                    // 同算法但 key 不同 → 强烈怀疑 MITM，记录行号（遍历完无匹配则拒绝）
                    key_changed_line = Some(line);
                    break; // KeyChanged 优先级最高，无需继续查其他文件
                }
                Err(e) => {
                    // 本文件读取/解析错误：WARN 后跳过，继续查其他文件
                    tracing::warn!(
                        ?path,
                        host = %self.host,
                        error = %e,
                        "known_hosts 文件读取错误，跳过该文件"
                    );
                }
            }
        }

        if found_match {
            tracing::info!(host = %self.host, "host key 校验通过");
            return Ok(true);
        }

        // 同算法 key 不匹配（疑似 MITM）→ 一律拒绝（accept-new 也不自动覆盖）
        if let Some(line) = key_changed_line {
            let reason = format!(
                "host key 不匹配 (host={}, known_hosts line={})，可能遭受中间人攻击",
                self.host, line
            );
            tracing::error!("{}", reason);
            *self.rejection.lock() = Some(reason);
            return Ok(false);
        }

        // host 在所有文件中都未知
        if strict == "accept-new" {
            // Phase 2 TOFU：自动添加 host key 到首个 known_hosts 文件后接受
            let target_path = &self.known_hosts_paths[0];
            tracing::info!(
                host = %self.host,
                ?target_path,
                "StrictHostKeyChecking=accept-new: host 未知，TOFU 自动添加 host key"
            );
            match add_host_key_to_known_hosts(target_path, &self.host, self.port, server_public_key)
            {
                Ok(()) => {
                    tracing::info!(
                        host = %self.host,
                        ?target_path,
                        "TOFU: host key 已写入 known_hosts，接受本次连接"
                    );
                    return Ok(true);
                }
                Err(e) => {
                    let reason = format!(
                        "TOFU 添加 host key 失败 (host={}, path={:?}): {}",
                        self.host, target_path, e
                    );
                    tracing::warn!("{}", reason);
                    *self.rejection.lock() = Some(reason);
                    return Ok(false);
                }
            }
        }

        // strict == yes / ask：host 未知 → 拒绝
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
}

// ───────────────────────────────────────────────────────────────────────────
// known_hosts 写入（Phase 2 TOFU）
// ───────────────────────────────────────────────────────────────────────────

/// 将 host key 追加写入 known_hosts 文件（Phase 2 TOFU）。
///
/// 复用 `russh::keys::known_hosts::learn_known_hosts_path` 完成实际写入：
/// - 格式：`host ssh-ed25519 AAAA...`（标准端口 22）或 `[host]:port ssh-ed25519 AAAA...`（非标准端口）
/// - 自动创建文件与父目录（若不存在）
/// - 追加模式（POSIX append 原子写，无需 flock）
/// - 自动补齐末尾换行
///
/// 本函数额外负责：
/// - 新建文件时设置 0600 权限（Unix，SSH 安全约定，防止其他用户读取 host key 列表）
/// - INFO 日志记录添加行为
///
/// **hashed known_hosts**（`|1|<salt>|<hmac-sha1>`）：Phase 2 MVP 写入明文主机名，
/// 读取时由 `check_known_hosts_path` 原生支持 hash 匹配（已验证）。
/// hash 写入留配置项（Phase 3+）。
///
/// 失败映射为 `TermError::HostKeyRejected`（TOFU 写失败时连接应失败而非接受未持久化的 key）。
pub(crate) fn add_host_key_to_known_hosts(
    path: &Path,
    host: &str,
    port: u16,
    key: &ssh_key::PublicKey,
) -> Result<(), TermError> {
    // 记录写入前文件是否已存在（用于决定是否设置 0600 权限）
    let existed = path.exists();

    // 复用 russh 写入逻辑：格式化 + 创建父目录 + 追加 + 换行补齐
    russh::keys::known_hosts::learn_known_hosts_path(host, port, key, path).map_err(|e| {
        TermError::HostKeyRejected(format!(
            "写入 known_hosts 失败 (host={}, path={:?}): {}",
            host, path, e
        ))
    })?;

    // 新建文件时设置 0600 权限（SSH 安全约定）。Windows 无 Unix 权限位，跳过。
    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            // 权限设置失败不致命（文件已写入且可读），仅 WARN
            tracing::warn!(?path, error = %e, "设置 known_hosts 文件权限 0600 失败（非致命）");
        }
    }

    tracing::info!(
        ?path,
        host = %host,
        port,
        new_file = !existed,
        "TOFU: host key 已写入 known_hosts"
    );
    Ok(())
}

#[async_trait]
impl TerminalProvider for SshProvider {
    async fn open(
        &self,
        request: OpenTerminalRequest,
    ) -> Result<Arc<dyn TerminalHandle>, TermError> {
        let host = &request.host;
        tracing::info!(
            host = %host.name,
            hostname = %host.hostname,
            port = host.port,
            user = %host.user,
            proxy_jump = ?host.proxy_jump,
            "ssh connecting"
        );

        // 连接 + 认证（直连或 ProxyJump 路径，由 connect_session 内部决策）
        let connected = connect_session(host, 0).await?;
        let session = connected.handle;

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

        tracing::info!(host = %host.name, "pty + shell requested");

        Ok(Arc::new(SshTerminalHandle::new(channel, session, connected.bastions))
            as Arc<dyn TerminalHandle>)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ───────────────────────────────────────────────────────────────────────────
// SshProvider —— exec / exec_stream（Phase 3：远端 daemon 探测与 proxy 透传）
// ───────────────────────────────────────────────────────────────────────────

impl SshProvider {
    /// 执行一条 SSH 命令，收集 stdout 并返回。用于 check_remote_runtime / bootstrap。
    ///
    /// Phase 3 用途：
    /// - `check_remote_runtime`：执行
    ///   `test -x ~/.local/share/termbridge/termbridge-agentd` 等探测命令
    /// - `bootstrap_daemon`：执行
    ///   `termbridge-agentd bootstrap --sock <path>`，收集 stdout JSON 响应
    ///
    /// 行为：
    /// - 复用 `connect_session`（不开 PTY、不发 shell）建立已认证 session
    /// - `channel.exec(command)` 触发远端执行
    /// - 循环收 `ChannelMsg::Data`（stdout）拼接为 String
    /// - `ChannelMsg::ExtendedData`（stderr）记录到 tracing::debug 后丢弃（不阻塞）
    /// - 收 `ChannelMsg::ExitStatus` 记录退出码，等 `Eof`/`Close` 退出循环
    /// - 命令完成后主动 `session.disconnect` 关闭连接（exec 是短连接）
    ///
    /// 错误映射：
    /// - SSH 连接失败 → `TermError::ConnectFailed` / `HostKeyRejected` / `AuthFailed`
    /// - channel / exec 操作失败 → `TermError::ChannelError`
    /// - 命令退出码非 0 → `TermError::ChannelError("command exited with code {code}")`
    pub async fn exec(&self, host: &Host, command: &str) -> Result<String, TermError> {
        tracing::info!(
            host = %host.name,
            hostname = %host.hostname,
            command,
            "ssh exec"
        );

        // 连接 + 认证（复用 connect_session，支持直连与 ProxyJump）
        let connected = connect_session(host, 0).await?;
        let session = connected.handle;
        let bastions = connected.bastions;

        // 开 session channel + exec（不开 PTY）
        // want_reply=true：要求 server 回 success/failure，便于及早发现 exec 被拒
        let mut channel = session
            .channel_open_session()
            .await
            .map_err(|e| TermError::ChannelError(format!("channel_open_session: {e}")))?;
        channel
            .exec(true, command)
            .await
            .map_err(|e| TermError::ChannelError(format!("channel.exec: {e}")))?;

        // 循环收消息：Data → stdout；ExtendedData → stderr 丢弃；ExitStatus → 退出码
        let mut stdout = Vec::new();
        let mut exit_code: Option<u32> = None;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    stdout.extend_from_slice(&data);
                }
                Some(ChannelMsg::ExtendedData { data, ext }) => {
                    // stderr 不阻塞 stdout 收集，仅 debug 记录后丢弃
                    tracing::debug!(
                        host = %host.name,
                        ext,
                        len = data.len(),
                        "ssh exec stderr (discarded)"
                    );
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit_code = Some(exit_status);
                    // 不 break：等 Eof/Close 确保所有 stdout 数据已收到
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                Some(_) => continue,
            }
        }

        // 清理：channel eof + session/bastions disconnect（exec 是短连接）
        let _ = channel.eof().await;
        let _ = session
            .disconnect(Disconnect::ByApplication, "termbridge exec done", "en")
            .await;
        for bs in bastions.into_iter().rev() {
            let _ = bs
                .disconnect(
                    Disconnect::ByApplication,
                    "termbridge exec done bastion",
                    "en",
                )
                .await;
        }

        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        match exit_code {
            Some(0) => {
                tracing::info!(
                    host = %host.name,
                    stdout_len = stdout.len(),
                    "ssh exec completed (exit 0)"
                );
                Ok(stdout)
            }
            Some(code) => {
                tracing::warn!(
                    host = %host.name,
                    code,
                    stdout_len = stdout.len(),
                    "ssh exec failed (non-zero exit)"
                );
                Err(TermError::ChannelError(format!(
                    "command exited with code {code}"
                )))
            }
            None => {
                // channel 关闭但未收到 ExitStatus（部分 server 实现不发送）
                tracing::warn!(
                    host = %host.name,
                    "ssh exec: channel closed without ExitStatus, treating as success"
                );
                Ok(stdout)
            }
        }
    }

    /// 执行一条 SSH 命令，返回 (read_half, write_half) 双向字节流。
    ///
    /// Phase 3 用途：`ssh_proxy_connect` —— 执行
    /// `termbridge-agentd proxy --sock <path>`，获取双向字节流透传 daemon RPC。
    /// stdin ↔ Unix socket 双向透传。
    ///
    /// 行为：
    /// - 复用 `connect_session`（不开 PTY、不发 shell）建立已认证 session
    /// - `channel.exec(command)` 触发远端执行
    /// - `channel.split()` 拆为读/写两半，返回给调用方
    /// - **不等待 ExitStatus**：proxy 是长连接，channel 关闭时自然结束
    ///
    /// 返回：
    /// - `read_half: ChannelReadHalf` —— 收 daemon → stdout（调用方读数据）
    /// - `write_half: ChannelWriteHalf<client::Msg>` —— 发 stdin → daemon（调用方写数据）
    ///
    /// 生命周期：
    /// - `session` Handle 与 `bastion_sessions` 在本函数返回时被 drop。
    ///   russh 的 `Handle::drop` 不主动 disconnect，session loop 由 `write_half`
    ///   持有的 sender 维持存活。
    /// - 调用方 drop `write_half` 后，所有 sender 被释放，session loop 自然退出，
    ///   SSH 连接随之关闭。
    /// - ProxyJump 场景下，bastion session 由 direct-tcpip channel 维持存活
    ///   （target session 通过 stream 读取），同样在 `write_half` drop 后级联退出。
    ///
    /// 错误映射：
    /// - SSH 连接失败 → `TermError::ConnectFailed` / `HostKeyRejected` / `AuthFailed`
    /// - channel / exec 操作失败 → `TermError::ChannelError`
    pub async fn exec_stream(
        &self,
        host: &Host,
        command: &str,
    ) -> Result<(russh::ChannelReadHalf, russh::ChannelWriteHalf<client::Msg>), TermError> {
        tracing::info!(
            host = %host.name,
            hostname = %host.hostname,
            command,
            "ssh exec_stream (proxy)"
        );

        // 连接 + 认证（复用 connect_session，支持直连与 ProxyJump）
        let connected = connect_session(host, 0).await?;
        let session = connected.handle;
        // bastions 在函数返回时 drop；SSH 连接不会断（见 docstring 生命周期说明）
        let _bastions = connected.bastions;

        // 开 session channel + exec（不开 PTY）
        // want_reply=true：要求 server 回 success/failure，便于及早发现 exec 被拒
        let channel = session
            .channel_open_session()
            .await
            .map_err(|e| TermError::ChannelError(format!("channel_open_session: {e}")))?;
        channel
            .exec(true, command)
            .await
            .map_err(|e| TermError::ChannelError(format!("channel.exec: {e}")))?;

        // 拆分 channel 为读/写两半，返回给调用方。
        // session 与 bastions 在此函数返回时 drop，但 SSH 连接不会断：
        // - write_half 持有 session loop 的 sender，维持 loop 存活
        // - 调用方 drop write_half 后，loop 退出，连接关闭
        let (read_half, write_half) = channel.split();
        Ok((read_half, write_half))
    }
}

// ───────────────────────────────────────────────────────────────────────────
// connect_session —— connect + auth（直连 / ProxyJump，§7.4 Phase 2）
// ───────────────────────────────────────────────────────────────────────────

/// `connect_session` 的返回结果。
///
/// `handle` 是目标主机的已认证 SSH session（未开 PTY/shell）。
/// `bastions` 是跳板机 session 链（从外层到内层），直连时为空。
/// **必须保持 bastions 存活**到 session 关闭 —— drop 会断开隧道导致目标 session 失活。
pub struct ConnectResult {
    pub(crate) handle: Handle<SshClientHandler>,
    pub(crate) bastions: Vec<Handle<SshClientHandler>>,
}

/// 连接 SSH session 并认证（不开 PTY/shell）。
///
/// **直连路径**（`host.proxy_jump` 为 None）：
/// `client::connect(addr, handler)` → 认证 → `(handle, vec![])`。
///
/// **ProxyJump 路径**（`host.proxy_jump` 为 Some，depth < MAX_PROXY_DEPTH）：
/// 1. `parse_proxy_jump` 解析跳板机 `[user@]host[:port]`。
/// 2. `sshconfig::resolve(bastion)` 获取跳板机完整配置（IdentityFile/known_hosts 等）。
/// 3. 递归 `connect_session(bastion, depth+1)` 连跳板机（支持链式嵌套）。
/// 4. 在跳板机 session 上 `channel_open_direct_tcpip` 到目标主机。
/// 5. `channel.into_stream()` → `client::connect_stream` 在隧道上建目标 SSH session。
/// 6. 在目标 session 上认证。
/// 7. 返回 `(target_handle, bastions_chain)`。
///
/// **深度限制**：`depth >= MAX_PROXY_DEPTH` 时返回 `InvalidArgument`（防循环配置）。
///
/// host key 校验：目标与跳板机各自独立校验（用各自的 known_hosts/strict/host/port）。
/// 认证：目标与跳板机各自独立认证（ssh-agent / IdentityFile）。
async fn connect_session(host: &Host, depth: usize) -> Result<ConnectResult, TermError> {
    // ── ProxyJump 路径 ──
    if let Some(pj_str) = host.proxy_jump.as_deref() {
        if depth >= MAX_PROXY_DEPTH {
            return Err(TermError::InvalidArgument(format!(
                "proxyjump 递归深度超限 (MAX_PROXY_DEPTH={MAX_PROXY_DEPTH})，疑似循环配置 (host={})",
                host.name
            )));
        }

        let target = sshconfig::parse_proxy_jump(pj_str)?;

        // 解析跳板机完整配置（ssh -G <bastion>，复用 OpenSSH config：IdentityFile/known_hosts 等）
        let mut bastion_host = sshconfig::resolve(&target.host).await?;
        // ProxyJumpTarget 显式指定的 user/port 覆盖 ssh config 默认值（与 OpenSSH 语义一致）
        if let Some(u) = &target.user {
            bastion_host.user = u.clone();
        }
        if let Some(p) = target.port {
            bastion_host.port = p;
        }

        tracing::info!(
            target_host = %host.hostname,
            target_port = host.port,
            bastion_host = %bastion_host.hostname,
            bastion_port = bastion_host.port,
            depth,
            "proxyjump: connecting via bastion"
        );

        // 递归连接跳板机（跳板机本身可能也有 proxy_jump，支持链式）
        // Box::pin 必须：async fn 递归会导致 Future 大小无限增长，Rust 要求 box 化
        let bastion_result = Box::pin(connect_session(&bastion_host, depth + 1)).await?;
        let bastion_handle = bastion_result.handle;
        let mut all_bastions = bastion_result.bastions;

        // 在跳板机 session 上开 direct-tcpip channel 到目标主机（RFC4254 §7）
        let channel = bastion_handle
            .channel_open_direct_tcpip(
                host.hostname.clone(),
                host.port as u32,
                "127.0.0.1",
                0,
            )
            .await
            .map_err(|e| {
                TermError::ConnectFailed(format!(
                    "proxyjump channel_open_direct_tcpip to {}:{} via {}: {e}",
                    host.hostname, host.port, bastion_host.hostname
                ))
            })?;

        // channel → stream → connect_stream（在隧道上建目标 SSH session）
        let stream = channel.into_stream();
        let config = Arc::new(client::Config::default());
        // 目标 host key 校验：用目标的 hostname/port/known_hosts/strict
        let (target_handler, rejection) = SshClientHandler::new(
            host.user_known_hosts_files.clone(),
            host.strict_host_key_checking.clone(),
            host.hostname.clone(),
            host.port,
        );
        let mut target_session = client::connect_stream(config, stream, target_handler)
            .await
            .map_err(|e| {
                if let Some(reason) = rejection.lock().take() {
                    tracing::warn!(host = %host.name, reason = %reason, "target host key rejected");
                    TermError::HostKeyRejected(reason)
                } else {
                    map_connect_err(e, &host.name)
                }
            })?;

        // 在目标 session 上认证
        authenticate_session(&mut target_session, &host.user, &host.identity_files).await?;

        // 跳板机 handle 加入链（必须与 target_session 同生命周期）
        all_bastions.push(bastion_handle);
        tracing::info!(
            target_host = %host.hostname,
            bastion_host = %bastion_host.hostname,
            bastion_chain_len = all_bastions.len(),
            "proxyjump: target authenticated via bastion chain"
        );

        return Ok(ConnectResult {
            handle: target_session,
            bastions: all_bastions,
        });
    }

    // ── 直连路径 ──
    let addr = (host.hostname.as_str(), host.port);
    let config = Arc::new(client::Config::default());
    // 构造 handler：传入 known_hosts 路径列表 + strict 模式 + host 信息。
    // 用 host.hostname（纯 IP/域名）而非 host.name（可能含 "root@" 前缀），
    // 因为 known_hosts 条目按纯主机名存储（如 "192.168.88.200" 而非 "root@192.168.88.200"）。
    let (handler, rejection) = SshClientHandler::new(
        host.user_known_hosts_files.clone(),
        host.strict_host_key_checking.clone(),
        host.hostname.clone(),
        host.port,
    );
    let mut session = client::connect(config, addr, handler)
        .await
        .map_err(|e| {
            // 若 check_server_key 拒绝，rejection 槽有原因 → 映射为 HostKeyRejected
            if let Some(reason) = rejection.lock().take() {
                tracing::warn!(host = %host.name, reason = %reason, "host key rejected");
                TermError::HostKeyRejected(reason)
            } else {
                map_connect_err(e, &host.name)
            }
        })?;

    authenticate_session(&mut session, &host.user, &host.identity_files).await?;

    Ok(ConnectResult {
        handle: session,
        bastions: vec![],
    })
}

/// 建立 SSH 连接但不认证（供 bootstrap_host 使用，ADR-0009）。
///
/// 与 `connect_session` 的区别：不调用 `authenticate_session`，仅完成 TCP 连接 + host key 校验。
/// 调用方负责后续认证（可先尝试 key，失败再密码）。
///
/// **不支持 ProxyJump**：bootstrap 场景假设直连目标主机。
/// 如果 host 配置了 proxy_jump，返回 `TermError::InvalidArgument`。
pub async fn connect_unauthenticated(host: &Host) -> Result<ConnectResult, TermError> {
    if host.proxy_jump.is_some() {
        return Err(TermError::InvalidArgument(
            "bootstrap does not support ProxyJump".to_string(),
        ));
    }

    let addr = (host.hostname.as_str(), host.port);
    let config = Arc::new(client::Config::default());
    let (handler, rejection) = SshClientHandler::new(
        host.user_known_hosts_files.clone(),
        host.strict_host_key_checking.clone(),
        host.hostname.clone(),
        host.port,
    );
    let session = client::connect(config, addr, handler)
        .await
        .map_err(|e| {
            if let Some(reason) = rejection.lock().take() {
                tracing::warn!(host = %host.name, reason = %reason, "host key rejected");
                TermError::HostKeyRejected(reason)
            } else {
                map_connect_err(e, &host.name)
            }
        })?;

    Ok(ConnectResult {
        handle: session,
        bastions: vec![],
    })
}

/// 在已连接的 session 上认证（凭据优先级 SSH Agent > IdentityFile > HITL(Phase 6)）。
///
/// 1. 尝试 ssh-agent → 成功则跳过 IdentityFile
/// 2. ssh-agent 失败/不可用 → 遍历 identity_files
/// 3. 都失败 → Err(AuthFailed)
///
/// 返回认证方式（"ssh-agent" / "identity_file"）用于日志。
pub(crate) async fn authenticate_session(
    session: &mut Handle<SshClientHandler>,
    user: &str,
    identity_files: &[PathBuf],
) -> Result<&'static str, TermError> {
    let authed = authenticate_with_agent(session, user).await?;
    let via = if authed {
        "ssh-agent"
    } else {
        let ok = authenticate_with_identity_files(session, user, identity_files).await?;
        if !ok {
            tracing::warn!(user = %user, "ssh 认证失败：ssh-agent 与 identity_files 均未通过");
            return Err(TermError::AuthFailed);
        }
        "identity_file"
    };
    tracing::info!(user = %user, auth_via = %via, "ssh authenticated");
    Ok(via)
}

/// 用密码认证（仅 bootstrap_host 使用，ADR-0009）。
///
/// 正常 SSH 认证流程不调用此函数。bootstrap_host 在 key 认证失败后，
/// 通过 CredentialProvider 获取密码，调用此函数完成一次性密码登录。
///
/// 返回 true 表示认证成功，false 表示密码错误（认证拒绝）。
pub async fn authenticate_with_password(
    session: &mut Handle<SshClientHandler>,
    user: &str,
    password: &str,
) -> Result<bool, TermError> {
    let auth_res = session
        .authenticate_password(user, password)
        .await
        .map_err(|e| TermError::ChannelError(format!("authenticate_password: {e}")))?;
    Ok(auth_res.success())
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
///
/// Phase 2 ProxyJump：新增 `bastion_sessions` 字段持有跳板机 session 链。
/// 这些 session 是目标 SSH 隧道的底层 transport，必须与 `session` 同生命周期 ——
/// drop 会断开隧道导致目标 session 立即失活。`close()` 逆序 disconnect 全部跳板机。
/// 直连时为 None。
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
    /// 跳板机 session 链（Phase 2 ProxyJump）。从外层到内层。
    /// `close()` 时逆序 disconnect（先断内层再断外层，与建立顺序相反）。
    /// 直连时为 None；ProxyJump 时为 `Some(vec![bastion_outer, ..., bastion_inner])`。
    /// **不为跳板机跑 keepalive**：目标 session 的 keepalive 间接监测隧道活性
    /// （隧道断 → 目标 stream EOF → 目标 keepalive 失败 → Session::Lost）。
    bastion_sessions: Arc<tokio::sync::Mutex<Option<Vec<Handle<SshClientHandler>>>>>,
}

impl SshTerminalHandle {
    fn new(
        channel: russh::Channel<client::Msg>,
        session: Handle<SshClientHandler>,
        bastions: Vec<Handle<SshClientHandler>>,
    ) -> Self {
        let (reader, writer) = channel.split();
        let session = Arc::new(tokio::sync::Mutex::new(Some(session)));
        let bastion_sessions = Arc::new(tokio::sync::Mutex::new(
            if bastions.is_empty() { None } else { Some(bastions) },
        ));

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
            bastion_sessions,
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

    /// 在已建立的 SSH session 上执行一条命令（Phase 5-B）。
    ///
    /// 开新 channel（独立于 PTY channel）执行 `command`，收集 stdout 返回。
    /// 不通过 PTY，不污染 session 输出。channel 执行完后关闭（session 保持存活）。
    ///
    /// 锁 `session` 仅在 `channel_open_session` 期间持有，exec 收数据阶段释放锁，
    /// 不阻塞 PTY 写与 SFTP 操作。
    pub async fn exec(&self, command: &str) -> Result<String, TermError> {
        tracing::debug!(command, "ssh handle exec: starting");

        let mut channel = {
            let guard = self.session.lock().await;
            let session = guard.as_ref().ok_or_else(|| {
                TermError::SessionClosed("ssh session handle already taken".into())
            })?;
            session
                .channel_open_session()
                .await
                .map_err(|e| TermError::ChannelError(format!("channel_open_session: {e}")))?
        };

        channel
            .exec(true, command)
            .await
            .map_err(|e| TermError::ChannelError(format!("channel.exec: {e}")))?;

        let mut stdout = Vec::new();
        let mut exit_code: Option<u32> = None;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    stdout.extend_from_slice(&data);
                }
                Some(ChannelMsg::ExtendedData { data, ext }) => {
                    tracing::debug!(ext, len = data.len(), "ssh exec stderr (discarded)");
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit_code = Some(exit_status);
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                Some(_) => continue,
            }
        }

        let _ = channel.eof().await;

        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        match exit_code {
            Some(0) => {
                tracing::debug!(stdout_len = stdout.len(), "ssh handle exec: complete (exit 0)");
                Ok(stdout)
            }
            Some(code) => Err(TermError::ChannelError(format!(
                "command exited with code {code}"
            ))),
            None => {
                tracing::warn!("ssh handle exec: channel closed without ExitStatus, treating as success");
                Ok(stdout)
            }
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

        // 3. 跳板机 session 链 disconnect（Phase 2 ProxyJump）
        // 逆序 disconnect：先断内层（离目标最近的跳板机）再断外层，
        // 与建立顺序相反，确保每一层断开时其外层隧道仍可用以传递 disconnect 报文。
        let bastions = self.bastion_sessions.lock().await.take();
        if let Some(bastions) = bastions {
            for bs in bastions.into_iter().rev() {
                let _ = bs
                    .disconnect(Disconnect::ByApplication, "termbridge close bastion", "en")
                    .await;
            }
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
            vec![path.clone()],
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
            vec![path.clone()],
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
            vec![path.clone()],
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
            vec![path.clone()],
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
            vec![path.clone()],
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
            vec![path.clone()],
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
    async fn check_server_key_rejects_when_known_hosts_path_empty() {
        // strict 模式但无 known_hosts 路径（空 Vec）→ 无法校验，拒绝
        let (mut handler, rejection) =
            SshClientHandler::new(vec![], "yes".to_string(), "myhost".to_string(), 22);

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();

        assert!(!accepted, "empty known_hosts paths in yes mode must reject");
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
            vec![path.clone()],
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
            vec![path.clone()],
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

    // ── Phase 2：ProxyJump 配置常量测试 ──────────────────────────────

    #[test]
    fn proxy_jump_depth_constant_has_expected_value() {
        // §7.4 Phase 2：MAX_PROXY_DEPTH=3 覆盖 "堡垒→中间跳板→目标" 罕见场景
        assert_eq!(MAX_PROXY_DEPTH, 3, "proxyjump 递归深度上限应为 3");
        // 防循环：A→B→A 会因 depth 超限返回 InvalidArgument
        assert!(
            MAX_PROXY_DEPTH >= 1,
            "至少允许 1 跳（最常见的单 bastion 场景）"
        );
    }

    // ── Phase 2：TOFU（accept-new）测试 ──────────────────────────────

    #[tokio::test]
    async fn tofu_accept_new_adds_key_for_unknown_host() {
        // accept-new + host 不在 known_hosts → 自动添加 key + 接受，
        // 且写入后再次校验（用 yes 模式）应能匹配同一 key。
        let content = format!("otherhost ssh-ed25519 {TEST_KEY_A}\n");
        let path = write_known_hosts(&content, "tofu_unknown");
        let (mut handler, rejection) = SshClientHandler::new(
            vec![path.clone()],
            "accept-new".to_string(),
            "tofuhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_B).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();

        assert!(accepted, "accept-new + 未知主机应 TOFU 自动添加并接受");
        assert!(
            rejection.lock().is_none(),
            "TOFU 成功不应设置拒绝原因"
        );

        // 文件中应新增一行：tofuhost ssh-ed25519 <KEY_B>
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains(&format!("tofuhost ssh-ed25519 {TEST_KEY_B}")),
            "known_hosts 应包含新添加的 host key 行，实际内容: {after}"
        );

        // 再次校验（换 yes 模式，模拟第二次连接）：同一 key 应匹配接受
        let (mut handler2, _) =
            SshClientHandler::new(vec![path.clone()], "yes".to_string(), "tofuhost".to_string(), 22);
        let accepted2 = handler2.check_server_key(&pubkey).await.unwrap();
        assert!(accepted2, "TOFU 写入后第二次连接（yes 模式）应匹配接受");

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn tofu_accept_new_rejects_key_mismatch() {
        // accept-new + host 已知但 key 变更（KeyChanged）→ 仍拒绝（TOFU 仅对未知主机自动添加）
        let content = format!("myhost ssh-ed25519 {TEST_KEY_A}\n");
        let path = write_known_hosts(&content, "tofu_mismatch");
        let (mut handler, rejection) = SshClientHandler::new(
            vec![path.clone()],
            "accept-new".to_string(),
            "myhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_B).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();

        assert!(!accepted, "accept-new + key 不匹配必须拒绝（疑似 MITM）");
        let reason = rejection.lock().take().expect("rejection reason should be set");
        assert!(
            reason.contains("不匹配"),
            "rejection reason 应提及不匹配，实际: {reason}"
        );

        // 文件不应被修改（TOFU 不覆盖已有 key）
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains(&format!("myhost ssh-ed25519 {TEST_KEY_A}")),
            "key 不匹配时不应改写 known_hosts"
        );
        assert!(
            !after.contains(TEST_KEY_B),
            "key 不匹配时不应写入新 key"
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn tofu_accept_new_accepts_matching_key_without_write() {
        // accept-new + host 已知且 key 匹配 → 接受，且不重复写入
        let content = format!("myhost ssh-ed25519 {TEST_KEY_A}\n");
        let path = write_known_hosts(&content, "tofu_match");
        let before = std::fs::read_to_string(&path).unwrap();

        let (mut handler, rejection) = SshClientHandler::new(
            vec![path.clone()],
            "accept-new".to_string(),
            "myhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();
        assert!(accepted, "accept-new + 匹配 key 应接受");
        assert!(rejection.lock().is_none(), "匹配成功不应设置拒绝原因");

        // 已知主机匹配时不触发 TOFU 写入，文件内容不变
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "匹配 key 时不应改写 known_hosts");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn tofu_accept_new_creates_missing_known_hosts_file() {
        // accept-new + known_hosts 文件不存在 → 自动创建并写入
        let path = std::env::temp_dir()
            .join(format!("termbridge_ssh_test_tofu_create_{}", std::process::id()));
        let _ = std::fs::remove_file(&path); // 确保起点干净
        assert!(!path.exists(), "测试前置：文件不应存在");

        let (mut handler, _) = SshClientHandler::new(
            vec![path.clone()],
            "accept-new".to_string(),
            "newhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();
        assert!(accepted, "accept-new + 文件不存在应自动创建并接受");

        assert!(path.exists(), "known_hosts 文件应被创建");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains(&format!("newhost ssh-ed25519 {TEST_KEY_A}")),
            "新文件应包含写入的 host key，实际: {content}"
        );
        std::fs::remove_file(&path).ok();
    }

    // ── Phase 2：add_host_key_to_known_hosts 写入格式测试 ──────────────

    #[test]
    fn add_host_key_writes_standard_port_format() {
        // 标准端口 22：格式 `host ssh-ed25519 AAAA...`
        let path = std::env::temp_dir()
            .join(format!("termbridge_ssh_test_add_std_{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        add_host_key_to_known_hosts(&path, "myhost", 22, &pubkey).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains(&format!("myhost ssh-ed25519 {TEST_KEY_A}")),
            "标准端口应写 `host ssh-ed25519 <key>`，实际: {content}"
        );
        assert!(
            !content.contains("[myhost]:22"),
            "标准端口 22 不应写 [host]:port 格式"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn add_host_key_writes_nonstandard_port_format() {
        // 非标准端口：格式 `[host]:port ssh-ed25519 AAAA...`
        let path = std::env::temp_dir()
            .join(format!("termbridge_ssh_test_add_port_{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        add_host_key_to_known_hosts(&path, "myhost", 2222, &pubkey).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains(&format!("[myhost]:2222 ssh-ed25519 {TEST_KEY_A}")),
            "非标准端口应写 `[host]:port ssh-ed25519 <key>`，实际: {content}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn add_host_key_appends_to_existing_without_corrupting() {
        // 追加模式：已有内容应保留，新 key 追加在后
        let path = std::env::temp_dir()
            .join(format!("termbridge_ssh_test_add_append_{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, format!("existinghost ssh-ed25519 {TEST_KEY_A}\n")).unwrap();

        let pubkey = parse_public_key_base64(TEST_KEY_B).unwrap();
        add_host_key_to_known_hosts(&path, "newhost", 22, &pubkey).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains(&format!("existinghost ssh-ed25519 {TEST_KEY_A}")),
            "追加写入不应破坏已有条目"
        );
        assert!(
            content.contains(&format!("newhost ssh-ed25519 {TEST_KEY_B}")),
            "应追加新条目"
        );
        std::fs::remove_file(&path).ok();
    }

    // ── Phase 2：多 known_hosts 文件遍历测试 ──────────────────────────

    #[tokio::test]
    async fn check_server_key_finds_key_in_second_known_hosts_file() {
        // 两个 known_hosts 文件：第一个无目标 host，第二个有 → 遍历到第二个应匹配接受
        let content1 = format!("otherhost ssh-ed25519 {TEST_KEY_A}\n");
        let content2 = format!("myhost ssh-ed25519 {TEST_KEY_A}\n");
        let path1 = write_known_hosts(&content1, "multi_file_1");
        let path2 = write_known_hosts(&content2, "multi_file_2");

        let (mut handler, rejection) = SshClientHandler::new(
            vec![path1.clone(), path2.clone()],
            "yes".to_string(),
            "myhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_A).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();
        assert!(accepted, "应在第二个 known_hosts 文件中找到匹配 key 并接受");
        assert!(rejection.lock().is_none(), "匹配成功不应设置拒绝原因");

        std::fs::remove_file(&path1).ok();
        std::fs::remove_file(&path2).ok();
    }

    #[tokio::test]
    async fn tofu_accept_new_writes_to_first_path_when_multiple() {
        // accept-new + 多文件 + host 未知 → 写入首个文件
        let path1 = std::env::temp_dir()
            .join(format!("termbridge_ssh_test_tofu_multi_1_{}", std::process::id()));
        let path2 = std::env::temp_dir()
            .join(format!("termbridge_ssh_test_tofu_multi_2_{}", std::process::id()));
        let _ = std::fs::remove_file(&path1);
        let _ = std::fs::remove_file(&path2);
        // 两个文件都存在但都不含目标 host
        std::fs::write(&path1, format!("otherhost ssh-ed25519 {TEST_KEY_A}\n")).unwrap();
        std::fs::write(&path2, format!("otherhost2 ssh-ed25519 {TEST_KEY_A}\n")).unwrap();

        let (mut handler, _) = SshClientHandler::new(
            vec![path1.clone(), path2.clone()],
            "accept-new".to_string(),
            "tofuhost".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_B).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();
        assert!(accepted, "accept-new + 多文件应 TOFU 接受");

        // key 应写入首个文件，第二个文件不变
        let c1 = std::fs::read_to_string(&path1).unwrap();
        let c2 = std::fs::read_to_string(&path2).unwrap();
        assert!(
            c1.contains(&format!("tofuhost ssh-ed25519 {TEST_KEY_B}")),
            "应写入首个 known_hosts 文件，实际: {c1}"
        );
        assert!(
            !c2.contains("tofuhost"),
            "不应写入第二个文件，实际: {c2}"
        );
        std::fs::remove_file(&path1).ok();
        std::fs::remove_file(&path2).ok();
    }

    // ── Phase 2：hashed known_hosts 读取测试 ──────────────────────────
    // 验证 `check_known_hosts_path` 原生支持 `|1|<salt>|<hmac-sha1>` hashed 主机名条目。
    // hashed 条目取自 russh 0.62.5 测试 fixture（known_hosts.rs::test），
    // 对应明文主机名 "example.com" + 端口 22 + 测试用 KEY_HASHED。
    // HMAC-SHA1 计算已用 PowerShell 独立验证：HMAC-SHA1(salt, "example.com") == 预期 hash。

    /// 测试用 ed25519 公钥（对应 hashed fixture）。
    const TEST_KEY_HASHED: &str =
        "AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF";

    /// hashed known_hosts 测试行（example.com:22 对应的 |1|salt|hash 条目）。
    /// 单行字面量避免行续接歧义。
    const HASHED_LINE: &str = "|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF";

    #[tokio::test]
    async fn check_server_key_accepts_hashed_known_hosts_entry() {
        // hashed known_hosts：`|1|<salt>|<hash> ssh-ed25519 <key>`
        // 此 hash 对应 example.com:22。russh 的 match_hostname 用 HMAC-SHA1 验证主机名。
        let content = format!("{HASHED_LINE}\n");
        let path = write_known_hosts(&content, "hashed");
        let (mut handler, rejection) = SshClientHandler::new(
            vec![path.clone()],
            "yes".to_string(),
            "example.com".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_HASHED).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();
        assert!(accepted, "应匹配 hashed known_hosts 条目（HMAC-SHA1 验证主机名）");
        assert!(rejection.lock().is_none(), "匹配成功不应设置拒绝原因");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn check_server_key_rejects_wrong_host_for_hashed_entry() {
        // 同一 hashed 条目，但用错误主机名查询 → 不应匹配 → host 未知 → yes 模式拒绝
        let content = format!("{HASHED_LINE}\n");
        let path = write_known_hosts(&content, "hashed_wrong");
        let (mut handler, rejection) = SshClientHandler::new(
            vec![path.clone()],
            "yes".to_string(),
            "wronghost.com".to_string(),
            22,
        );

        let pubkey = parse_public_key_base64(TEST_KEY_HASHED).unwrap();
        let accepted = handler.check_server_key(&pubkey).await.unwrap();
        assert!(!accepted, "错误主机名不应匹配 hashed 条目");
        let reason = rejection.lock().take().expect("rejection reason should be set");
        assert!(
            reason.contains("未知"),
            "hash 不匹配应视为 host 未知，实际: {reason}"
        );
        std::fs::remove_file(&path).ok();
    }
}
