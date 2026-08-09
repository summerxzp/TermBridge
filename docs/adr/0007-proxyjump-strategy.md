# ADR-0007：ProxyJump 策略 —— 手动 SSH-over-SSH 隧道

- **Status**: Accepted
- **Date**: 2026-08-09
- **Phase**: 2
- **Supersedes**: —

## Context

企业场景普遍通过堡垒机 / 跳板机（bastion）访问内网主机，`~/.ssh/config` 中以 `ProxyJump bastion` 声明。Phase 0-A 已验证 `ssh -G` 能完整解析 `proxyjump` 字段（ADR-0006），Phase 1 在 `Host` 实体上保留了 `proxy_jump: Option<HostName>`，但直连路径未触及跳板机逻辑。

`russh 0.62` 不提供原生 ProxyJump API：`russh-config` crate 的 `Stream::proxy_command` 仅是 ProxyCommand（exec 子进程）薄封装，并非 SSH-over-SSH 隧道。可选路线：

1. **`ssh` 子进程作为 transport**：放弃 russh，用系统 `ssh -W` 转发。代价：丢失 known_hosts 校验、ssh-agent 认证、keepalive 等Phase 1 能力，且依赖外部进程生命周期管理。
2. **手动 SSH-over-SSH 隧道**：复用 russh 的 `channel_open_direct_tcpip` + `into_stream()` + `connect_stream()` 三个原语，在跳板机 session 上开 direct-tcpip channel，将其作为目标 SSH 的底层 stream。
3. **SOCKS 代理**：russh 不原生支持 SOCKS，需自行实现 SOCKS5 协商再走 direct-tcpip。

## Decision

**采用方案 2：手动 SSH-over-SSH 隧道。**

### 1. 递归 `connect_session` + direct-tcpip + `connect_stream`

`SshProvider::open` 调 `connect_session(host, depth=0)`，函数返回 `ConnectResult { handle, bastions }`：

- **直连**（`proxy_jump` 为 None）：`client::connect(addr, handler)` + 认证 → `ConnectResult { handle, bastions: vec![] }`。
- **ProxyJump**：`parse_proxy_jump` 解析 `[user@]host[:port]` → `sshconfig::resolve(bastion)` 取跳板机完整配置（`ProxyJumpTarget` 显式 user/port 覆盖 ssh config 默认值，与 OpenSSH 语义一致）→ 递归 `connect_session(bastion, depth+1)` → 在跳板机 handle 上 `channel_open_direct_tcpip(target_host, target_port, "127.0.0.1", 0)` → `channel.into_stream()` → `client::connect_stream(config, stream, target_handler)` 在隧道上建目标 SSH session → 在目标 session 上独立认证 → 跳板机 handle append 到 `bastions` 链尾。

递归调用 `Box::pin` 包裹，避免 async fn 递归导致 Future 大小无限增长。跳板机 host key 与目标 host key 各自独立校验（用各自的 `user_known_hosts_file` / `strict_host_key_checking` / `hostname` / `port`），认证也各自独立（ssh-agent / IdentityFile）。`SshProvider::open` 拿到 `ConnectResult` 后统一开 `channel_open_session` + `request_pty` + `request_shell`，构造 `SshTerminalHandle::new(channel, session, bastions)`。

### 2. `MAX_PROXY_DEPTH = 3` 防循环

常量 `MAX_PROXY_DEPTH=3`（`src/infrastructure/ssh.rs`）：覆盖「堡垒 → 中间跳板 → 目标」罕见场景；`depth >= MAX_PROXY_DEPTH` 返回 `InvalidArgument`，防止 `A.proxyjump=B; B.proxyjump=A` 类循环配置导致无限递归。单跳 bastion（企业 90%+ 场景）由 `depth=0 → 1` 直接通过。

### 3. 跳板机 session 同生命周期持有

`SshTerminalHandle` 新增 `bastion_sessions: Arc<tokio::sync::Mutex<Option<Vec<Handle<SshClientHandler>>>>>` 字段，持有跳板机 session 链（从外层到内层）。**必须与目标 session 同生命周期** —— drop 会断开底层隧道，目标 session 立即失活。直连时为 `None`。

### 4. `close()` 逆序 disconnect

`SshTerminalHandle::close()` 顺序：abort keepalive task → writer eof → take 目标 session 并 `disconnect` → take 跳板机链并 `into_iter().rev()` 逆序 `disconnect`。逆序确保每一层断开时其外层隧道仍可用以传递 disconnect 报文（与建立顺序相反）。

### 5. SOCKS 延期 Phase 5

russh 不原生支持 SOCKS，需自行实现 SOCKS5 协商 + direct-tcpip 复用。企业场景 ProxyJump 已覆盖 90%+ 需求，SOCKS 留待 Phase 5（多目标路由 / 审计代理）一并处理。

## Consequences

- ✅ **多跳支持**：递归 `connect_session` 天然支持链式 bastion（A→B→C），仅受 `MAX_PROXY_DEPTH` 约束。
- ✅ **复用 russh**：known_hosts 校验、ssh-agent 认证、keepalive 等Phase 1 能力对跳板机与目标均生效，无需重写。
- ✅ **隧道透明**：目标 session 的 PTY / SFTP / keepalive 完全不感知底层是直连还是经隧道，`SshTerminalHandle` 接口不变。
- ✅ **安全默认保留**：跳板机与目标各自独立做 host key 校验与认证，不因隧道而放宽。
- ⚠️ **跳板机无独立 keepalive**：仅目标 session 跑 keepalive task。隧道断开 → 目标 stream EOF → 目标 keepalive 失败 → Session::Lost，间接监测。若需跳板机层独立活性检测，Phase 4 可扩展（当前对单 bastion 场景足够）。
- ⚠️ **不支持 IPv6 字面量**：`parse_proxy_jump` 以 `rfind(':')` 拆分 host:port，IPv6 字面量（`[::1]:22`）会误判。企业跳板机通常为域名/IPv4，MVP 可接受；Phase 5 若有需求再加 bracket 解析。
- ⚠️ **递归 Future 经 `Box::pin`**：每跳一次堆分配一次 Future，深度 3 时开销可忽略，但理论上比迭代式略慢。
- ⚠️ **不支持 ProxyCommand / ProxyJump 逗号多跳语法**：`parse_proxy_jump` 显式拒绝含逗号的输入（`bastion1,bastion2`），多跳由 ssh config 的嵌套 `ProxyJump` + 递归 `connect_session` 表达，与 OpenSSH 语义等价但语法不同。
