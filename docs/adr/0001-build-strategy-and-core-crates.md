# ADR-0001：构建策略 + 核心 crate 选型

- **Status**: Accepted
- **Date**: 2026-08-09
- **Phase**: 0-C
- **Supersedes**: —

## Context

TermBridge 需要 Rust 异步生态下稳定可维护的 SSH PTY + MCP 协议栈。Phase 0-A 对候选 crate 做了实际验证（见 `docs/phase-0a-report.md`），需要在进入 Phase 0-C 实现前锁定选型，避免后期换底座。

候选维度：
1. MCP 协议（Server SDK）
2. SSH 客户端（连接 / 认证 / PTY / channel）
3. SFTP（文件传输，Phase 1+ 用）
4. SSH config 解析
5. 异步运行时 + 并发原语
6. 日志 / 错误 / 序列化

## Decision

| 维度 | 选型 | 版本（锁定） | 理由 |
|---|---|---|---|
| MCP SDK | `rmcp` | 3.1.2 | 官方 Rust SDK，`#[tool]` 宏 + `tool_router` 声明式工具；stdio transport 经 Phase 0-A 验证；`CallToolResult::structured` / `structured_error` 原生支持 §6.1 错误格式 |
| SSH 客户端 | `russh` | 0.62.5 | 纯 Rust（无 libssh 依赖），async/tokio 原生；`Channel::request_pty` + `request_shell` 满足 PTY 需求；`client::Handle` 提供 `best_supported_rsa_hash` / `authenticate_publickey` |
| SFTP | `russh-sftp` | 2.4.0 | 与 russh 同源，channel 复用，避免二次连接 |
| SSH config | `ssh -G` 子进程 | OpenSSH 自带 | 见 ADR-0006 |
| 运行时 | `tokio` | 1.x | rmcp / russh 均基于 tokio |
| 并发锁 | `parking_lot` | 0.12 | 非 async 的 `Mutex`（SessionManager / SshTerminalHandle 内部状态），避免 tokio Mutex 的跨 await 持锁问题；`OutputEngine` 用 `parking_lot::Mutex` 保护 RingBuffer |
| 日志 | `tracing` + `tracing-subscriber` | 0.1 / 0.3 | 结构化日志，span/field 支持 session_id 关联；stderr 输出避免污染 stdio MCP 通道 |
| 错误 | `anyhow`（bin）/ 自定义 `TermError`（lib） | 1.x | lib 层用 `thiserror` 派生 `TermError`，带 `code()` / `retriable()` 供 §6.1 ToolError 映射；bin 层用 `anyhow` 聚合 |
| 序列化 | `serde` + `serde_json` | 1.x / 1.x | rmcp 工具参数/返回 JSON schema 由 `schemars` 自动生成 |

### 关键约束

- **russh 0.62 API 变更**：`authenticate_publickey` 签名从 `Arc<PrivateKey>` 改为 `PrivateKeyWithHashAlg`，需配合 `best_supported_rsa_hash()` 协商 RSA hash（ed25519 传 `None`）。已在 `infrastructure/ssh.rs` 适配。
- **`Arc<dyn TerminalHandle>: Send + Sync`**：trait 必须加 `Send + Sync` 约束，PTY read task 才能跨线程持有 `Arc<handle>` clone。实现方（`SshTerminalHandle`）内部用 `parking_lot::Mutex` 包非 Sync 的 channel/session 状态。
- **Channel 跨 await 持锁**：`russh::Channel::wait()` 是 async，不能在 `MutexGuard` 跨 await。用 `Mutex<Option<Channel>>` 的 take/put 模式：take 出来 await，再 put 回。

## Consequences

- ✅ 全 Rust 栈，Windows 上无需 libssh/libssh2 动态库，交叉编译友好
- ✅ rmcp 宏减少样板代码，6 个工具约 200 行 server.rs
- ✅ russh 纯 Rust 的代价：少数 OpenSSH 高级特性（如 ControlMaster）不原生支持，但 TermBridge 自管 Session 生命周期，不需要 ControlMaster
- ⚠️ russh 0.62 API 仍在演进（PrivateKeyWithHashAlg 是 0.62 引入），升级时需重新验证认证路径
- ⚠️ parking_lot::Mutex 不是 async-aware，锁内不能 `.await`——已在 SshTerminalHandle 用 take/put 规避；若未来 SessionManager 并发量上升（Phase 1+），可能换 DashMap
