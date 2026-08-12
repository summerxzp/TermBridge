# Phase 0-A 技术验证报告

> 日期：2026-08-09 · 状态：✅ 全部通过

## 验证目标

在写业务代码前，锁定 4 个核心 crate 在 Windows + 真实 Linux SSH 主机上的可用性。

## 环境

- Windows + Rust 1.96 + OpenSSH 9.5p1
- 远端：Debian (203.0.113.140:22)

## 验证结果

### 1. rmcp 3.1.2 — ✅ 通过

- **编译**：`server` + `macros` + `transport-io` + `schemars` features 正常
- **stdio transport**：`rmcp::transport::stdio()` + `ServiceExt::serve()` + `waiting()` 模式工作
- **JSON-RPC 实测**：`initialize` / `notifications/initialized` / `tools/list` / `tools/call` 全部正确响应
- **`#[tool]` 宏模式**：`#[tool_router]` impl + `#[tool_handler]` impl ServerHandler + `ServerInfo::new(capabilities).with_instructions(...)` + `Parameters<T>` 参数包装

**关键 API 确认**：
- `Implementation::from_build_env()` 无参，自动读 `CARGO_PKG_NAME/VERSION`
- `ServerInfo` 是 non-exhaustive，必须用 `ServerInfo::new(...)` builder，不能用 struct expression
- `serve()` 方法来自 `ServiceExt` trait，需 `use rmcp::ServiceExt`
- `schemars` 由 rmcp re-export，不需单独依赖

### 2. russh 0.62 + ring backend — ✅ 通过

- **ring backend**：`default-features = false, features = ["ring", "flate2", "rsa", "async-trait"]`，纯 Rust 无需 NASM
- **密码认证**：`authenticate_password()` 成功
- **PTY 链路**：`channel_open_session()` → `request_pty()` → `request_shell(true)` → 交互式 shell（非 exec）
- **read/write**：`channel.data(&[u8])` + `channel.wait()` 读 `ChannelMsg::Data`/`ExtendedData`
- **Ctrl+C**：`channel.data(b"\x03")` 生效，shell 回到 prompt
- **resize**：`channel.window_change(rows, cols, 0, 0)` 生效，shell 继续可用
- **优雅关闭**：`channel.eof()` + `session.disconnect()`

**关键发现**：`channel.data()` 参数需 `AsyncRead`，`&[u8; N]` 不实现但 `&[u8]` 实现，用 `&b"..."[..]` 或 `b"\x03" as &[u8]`。`request_shell`（非 `exec`）是持久交互式 shell 的正确方式。

### 3. ssh -G 策略 — ✅ 通过

- **`ssh -G <host>` 子进程**：Windows OpenSSH 9.5p1 正常输出，4063 bytes / 74 单值 + 2 多值字段
- **格式**：`key value` 每行一个，key 全小写
- **关键字段全部可解析**：`hostname` / `port` / `user` / `identityfile`（多值，7 个）/ `proxyjump` / `stricthostkeychecking` / `userknownhostsfile` / `identitiesonly`
- **多值字段**：`identityfile` 等多行字段用 `HashMap<String, Vec<String>>` 收集

**结论**：`ssh -G` 让 OpenSSH 负责 Include/Match/ProxyJump/Host*/Canonicalize 全部复杂逻辑，TermBridge 只消费最终结果。**不需要自己实现 SSH config parser**（ADR-0006 策略确认）。

### 4. russh-sftp 2.4.0 — ✅ 通过

- **SFTP 会话建立**：`channel.request_subsystem(true, "sftp")` → `SftpSession::new(channel.into_stream())`
- **upload**：`open_with_flags(path, CREATE|TRUNCATE|WRITE|READ)` + `write_all` + `flush` + `shutdown`
- **download**：`open_with_flags(path, READ)` + `read_to_end` + `shutdown`
- **验证一致**：upload 44 bytes → download 44 bytes，内容完全匹配
- **清理**：`sftp.remove_file(path)`

## 核心依赖选型确认（→ ADR-0001）

| 领域 | crate | 版本 | 状态 |
|------|-------|------|------|
| MCP SDK | `rmcp` | 3.1.2 | ✅ 官方，MCP 2026-07-28 spec |
| SSH | `russh` | 0.62.5 | ✅ ring backend，PTY/shell/Ctrl+C/resize 全通过 |
| SFTP | `russh-sftp` | 2.4.0 | ✅ upload/download 验证一致 |
| SSH Config | `ssh -G` 子进程 | OpenSSH 9.5p1 | ✅ 74 字段解析 |
| 异步运行时 | `tokio` | 1.53 | ✅ |
| 日志 | `tracing` | 0.1 | ✅ |

## 关键决策（→ ADR 初稿）

1. **ADR-0001**：核心 crate 选型如上表。russh 用 ring backend（非默认 aws-lc-rs），避免 Windows NASM 依赖。`portable-pty` MVP 不需要（远端 PTY 走 SSH `request_pty`）。
2. **ADR-0002**：MCP transport = **stdio only**。rmcp 的 `transport-io` feature + `stdio()` 函数已验证。SSE/HTTP 后续按需。
3. **ADR-0006**：SSH Config 兼容策略 = **`ssh -G <host>` 子进程**，复用 OpenSSH 完整解析能力（Include/Match/ProxyJump/Host*）。不自己实现 parser，不用 `ssh2-config` crate。

## 已知限制（Phase 1 需补）

- **host key verification**：Phase 0 原型 `check_server_key` 接受任意 key（`Ok(true)`）。Phase 1 必须改 `known_hosts` 校验。
- **密码认证**：Phase 0 用密码测试。Phase 1 主走 SSH Agent / IdentityFile，密码仅 HITL（Phase 6）。
- **russh ProxyJump**：MVP 不涉及，Phase 2 验证。`ssh -G` 已能解析 proxyjump 字段，连接层待验证。

## 原型文件

| 文件 | 验证内容 |
|------|---------|
| [examples/p0_echo_mcp.rs](file:///e:/Code/TermBridge/examples/p0_echo_mcp.rs) | rmcp stdio MCP server + #[tool] 宏 |
| [examples/p0_ssh_pty.rs](file:///e:/Code/TermBridge/examples/p0_ssh_pty.rs) | russh PTY + shell + read/write + Ctrl+C + resize + EOF |
| [examples/p0_ssh_config.rs](file:///e:/Code/TermBridge/examples/p0_ssh_config.rs) | ssh -G 输出解析 |
| [examples/p0_sftp.rs](file:///e:/Code/TermBridge/examples/p0_sftp.rs) | russh-sftp upload/download |

## 结论

**Phase 0-A 全部通过，可进入 Phase 0-B。**

Phase 0-B 目标：独立实现 `OutputRingBuffer` + `ReadCursor` + `Waiter` + `SessionState`，用 fake PTY output 单测 §4.6 行为契约 10 条，不接 MCP/SSH。
