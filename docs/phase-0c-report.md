# Phase 0-C 报告：Vertical Slice 端到端打通

- **Date**: 2026-08-09
- **Status**: ✅ Complete
- **PLAN.md**: v0.4 §7.3
- **前置**: Phase 0-A（crate 验证）+ Phase 0-B（OutputEngine 12 契约）

## 目标

构建 Windows → Linux 的端到端 vertical slice：rmcp stdio server + SSH PTY + OutputEngine，6 个 MCP 工具完整链路验证。

## 交付物

### 代码（4 层架构）

| 层 | 文件 | 职责 |
|---|---|---|
| domain | `src/domain/provider.rs` | `TerminalProvider` / `TerminalHandle` trait + `Host` / `PtySize` / `ControlKey` / `TermError` |
| domain | `src/domain/session.rs` | `Session` 实体 + PTY read task + 状态机（Ready/Closing/Closed/Lost） |
| domain | `src/domain/output.rs` | OutputEngine（Phase 0-B 完成，4 模式 read_output） |
| infrastructure | `src/infrastructure/sshconfig.rs` | `ssh -G <alias>` 解析为 `Host`（ADR-0006） |
| infrastructure | `src/infrastructure/ssh.rs` | `SshProvider` + `SshTerminalHandle`（russh 封装，`Channel::split()` 读写分离） |
| application | `src/application/hosts.rs` | `HostManager`（list_hosts 扫描 ~/.ssh/config） |
| application | `src/application/sessions.rs` | `SessionManager`（open/send/read/control/close + list_sessions） |
| transport | `src/transport/mcp/server.rs` | `TermBridgeServer`（rmcp 6 工具 + ToolError 格式） |
| bin | `src/main.rs` | tracing→stderr + 组装依赖 + `serve_stdio()` |

### 6 个 MCP 工具

| 工具 | 映射 | 说明 |
|---|---|---|
| `list_hosts` | HostManager::list_hosts | 扫描 ~/.ssh/config，返回别名列表（不调 ssh -G，快速） |
| `open_session` | SessionManager::open_session | ssh -G 解析 → SshProvider::open → Session::new → 返回 session_id |
| `send_input` | SessionManager::send_input | 写 PTY stdin（立即返回，不等命令完成） |
| `read_output` | SessionManager::read_output | 4 模式：settle（默认）/ wait_for / tail_lines / since_cursor |
| `send_control` | SessionManager::send_control | ctrl+c / ctrl+d / ctrl+z / tab / enter / escape |
| `close_session` | SessionManager::close_session | EOF + disconnect，幂等 |

### ADR

- [ADR-0001](adr/0001-build-strategy-and-core-crates.md)：构建策略 + 核心 crate 选型
- [ADR-0002](adr/0002-mcp-transport-stdio-only.md)：MCP transport = stdio only（MVP）
- [ADR-0006](adr/0006-openssh-config-via-ssh-g.md)：OpenSSH config 兼容策略（ssh -G 子进程）

### 测试

- **单元测试**：36 个全通过（output 18 + sshconfig 5 + sessions 3 + hosts 4 + 其他 6）
- **MCP smoke test**：stdin 喂 JSON-RPC，验证 initialize / tools/list / list_hosts 协议链路
- **端到端 vertical slice**（`examples/e2e_mcp.ps1`）：
  - 目标：Windows → Debian 12 (192.168.88.200, root, ed25519 免密)
  - 流程：open_session → settle read（MOTD+prompt）→ send_input(echo) → wait_for 匹配 → send_control(ctrl+c) → tail read → close_session
  - 结果：**ALL PASSED**，6 工具完整链路验证通过

## 端到端测试结果

```
>>> open_session host=192.168.88.200
<<< session_id = sess_0                          (~200ms: ssh -G + connect + auth + pty + shell)

>>> read_output (settle, timeout=3s)
<<< 462 bytes: Linux debian 6.1.0-52-amd64 ... root@debian:~#

>>> send_input "echo HELLO_TERMBRIDGE\n"
<<< OK

>>> read_output (wait_for="HELLO_TERMBRIDGE", timeout=5s)
<<< matched=True, 27 bytes                       (阻塞匹配成功)

>>> send_control ctrl+c
<<< OK

>>> read_output (tail_lines=5)
<<< 208 bytes: ... ^C ... root@debian:~#

>>> close_session
<<< OK
```

## 关键决策与问题修复

### 1. russh 0.62 API 适配

`authenticate_publickey` 签名从 `Arc<PrivateKey>` 改为 `PrivateKeyWithHashAlg`，需配合 `best_supported_rsa_hash()` 协商 RSA hash（ed25519 传 None）。见 ADR-0001。

### 2. Channel read/write 竞争（关键修复）

**问题**：初版用 `Mutex<Option<Channel>>` 的 take/put 模式。read task 长期 `wait()` 阻塞期间持有 channel，write 无法获取——`send_input` 静默失败（或返回 Err），`wait_for` 超时 0 bytes。

**修复**：用 `Channel::split()` 拆分为 `ChannelReadHalf`（read task 独占）+ `ChannelWriteHalf`（`&self` 方法，无锁并发）。read 用 `tokio::sync::Mutex`（async-aware，guard 是 Send，可跨 await）。

**验证**：修复后 wait_for 匹配成功（matched=True），read/write 真正并发。

### 3. `Arc<dyn TerminalHandle>: Send + Sync`

trait 加 `Send + Sync` 约束，PTY read task 才能跨线程持有 `Arc<handle>` clone。`SshTerminalHandle` 内部用 Mutex 包非 Sync 状态。

### 4. Session Lost 状态可读

read_output 只拒 Closed（Lost 仍可读），close() 能将 Lost 转为 Closed——符合契约 10（disconnect 不销毁 Session，buffer 仍可读）。

## 性能数据

- open_session 端到端：~200ms（ssh -G 50ms + SSH connect/auth 60ms + PTY+shell 90ms）
- read_output settle：3s timeout 内完成（MOTD 即时到达）
- wait_for 匹配：echo 输出即时返回（<100ms）

## 已知限制（Phase 0-C 范围内）

1. **Host key 校验跳过**：`SshClientHandler::check_server_key` 接受任意 key（Phase 1 改 known_hosts 校验，ADR-0005）
2. **无 ssh-agent 支持**：仅 IdentityFile 公钥认证（Phase 1 加 ssh-agent fallback）
3. **identityfile 取第一个存在文件**：多 key 场景可能选错（Phase 1 改遍历尝试）
4. **无日志 redaction**：tracing 日志可能含敏感信息（Phase 1 加 redaction，ADR-0005）
5. **SessionManager 用 HashMap**：并发量低时足够，Phase 1+ 换 DashMap

## 下一步（Phase 1）

按 PLAN.md §7.4：
- ADR-0003：Output 缓冲策略（ring buffer 容量/双游标语义正式化）
- ADR-0005：安全模型（known_hosts + log redaction + SFTP 路径策略）
- SFTP 文件传输工具（upload/download/list）
- ssh-agent 认证支持
- 多 key 遍历尝试
- 集成测试套件（自动化的 e2e，不依赖手动 ssh 环境）
