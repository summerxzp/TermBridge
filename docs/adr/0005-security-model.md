# ADR-0005：安全模型 —— 凭据隔离 + host key + 日志脱敏 + SFTP 路径策略

- **Status**: Accepted
- **Date**: 2026-08-09
- **Phase**: 1
- **Supersedes**: —

## Context

Phase 0-C vertical slice 为快速跑通行为契约，留了三笔安全欠账：

1. **host key 跳过**——`src/infrastructure/ssh.rs` 的 `SshClientHandler::check_server_key` 直接 `Ok(true)` 并 `tracing::warn!("host key verification SKIPPED")`，存在中间人攻击风险。
2. **无日志脱敏**——`tracing` 直写 stderr，PTY output / 错误信息中可能含凭证、Authorization header、PEM 私钥块。
3. **单一 IdentityFile**——`authenticate_with_key` 只读 `host.identity_file`，无 ssh-agent 路径，无 IdentityFile 时直接报错（"ssh-agent auth not implemented in Phase 0-C"）。

**参考前车之鉴**：classfang `ssh-mcp-server` 把密码作为 `mcp.json` 的 args 字段传递，导致凭据出现在 MCP server 进程的命令行参数中，任何能列进程列表（`ps` / task manager）的本机用户即可读出。TermBridge 必须从设计上杜绝此路径。

PLAN §5.5 / §9 已定下 Phase 1 安全 baseline：known_hosts 校验、SSH Agent / IdentityFile 认证、log redaction 三类正则、凭据不进 MCP 配置 args、SFTP 路径策略 + 下载原子写。本 ADR 锁定具体实现策略。

## Decision

### 1. 凭据来源优先级与隔离

```
SSH Agent（首选）> IdentityFile > HITL（Phase 6）
```

| 来源 | Phase 1 | 进程可见性 | 说明 |
|---|---|---|---|
| SSH Agent | 实现 | 仅 agent socket fd | `russh` 走 `agent::connect` + `authenticate_future_with_key`；本机 `SSH_AUTH_SOCK` |
| IdentityFile | 实现 | 文件路径（无内容） | 读 `~/.ssh/id_*`，私钥只存在于 TermBridge 进程内存，不写入日志 / args / MCP 返回 |
| 密码 / passphrase | **不**做 | — | Phase 6 走 HITL UI，secret 直接写 PTY，不经 LLM context |
| MCP 配置 args | **禁** | — | `mcp.json` 只放 `command` / `args`（如 `["termbridge"]`），不放任何凭据字段 |

**与 classfang 的关键差异**：TermBridge 的 MCP 配置永不包含 `password` / `privateKey` / `passphrase` 字段。即使本机被列进程，也读不到凭据。

### 2. known_hosts 校验

复用 ADR-0006 的 `ssh -G` 输出，消费两个字段：

| `ssh -G` 字段 | 用途 |
|---|---|
| `userknownhostsfile` | known_hosts 文件路径（展开 `~`） |
| `stricthostkeychecking` | 校验策略 |

`SshClientHandler::check_server_key(server_public_key)` 实现：

| `stricthostkeychecking` | 行为 |
|---|---|
| `yes` / `ask` | 严格校验：known_hosts 中无此 host key → **拒绝**（返回 `Err` → `HOST_KEY_REJECTED`）；存在但 mismatch → 拒绝 |
| `no` | 接受任意 key，但 `tracing::warn!` 标记（便于审计） |

`ask` 在 MVP 无 HITL 时等同于 `yes`（无法弹窗询问用户）——**用户必须先手动 `ssh <host>` 一次将 host key 写入 known_hosts**，否则 TermBridge 连接会被拒。这是有意的安全默认。

### 3. 日志脱敏（tracing layer）

新增 `infrastructure/security/redact.rs`，实现一个 `tracing_subscriber::Layer`，对所有 `tracing` 事件（含 PTY output snippet / 错误信息）在落盘前应用三类正则：

| # | 正则 | 替换 | 命中场景 |
|---|---|---|---|
| 1 | `(?i)((?:password\|passwd\|secret\|token\|api[_-]?key\|access[_-]?key\|auth[_-]?token)\s*[=:]\s*)[^\n]+` | `${1}[REDACTED]` | `password: hello`、`TOKEN=abc123` |
| 2 | `(?i)(Authorization:\s*(?:Bearer\|Basic\|Token)\s+)\S+` | `${1}[REDACTED]` | HTTP header |
| 3 | `-----BEGIN (?:[A-Z ]+ )?PRIVATE KEY-----[\s\S]*?-----END (?:[A-Z ]+ )?PRIVATE KEY-----` | `[PRIVATE KEY REDACTED]` | PEM 块 |

审计 snippet 截前 2048 字节再脱敏。`send_secret` / `prepare_secret`（Phase 6）路径永不入审计日志。`Layer` 在 `main.rs` 初始化时挂到 `tracing_subscriber::registry()`。

### 4. SFTP 路径策略

| 配置 | 默认 | 说明 |
|---|---|---|
| `allowedLocalPaths` | `[cwd]` | 本地可读写的根；upload 源 / download 目标必须在其下 |
| `allowedRemotePaths` | `["/"]`（全放行，可收紧为白名单） | 远端可读写的根 |

**防穿越**：

- 本地路径：`canonicalize()` 后检查 `starts_with(allowed)`。
- 远端路径：调 SFTP `realpath` 解析后检查 `starts_with(allowed)`；拒绝含 `..` 的输入、拒绝 symlink 逃逸（`realpath` 已解析）、拒绝 null 字节。
- 不满足 → `LOCAL_PATH_NOT_ALLOWED` / `REMOTE_PATH_NOT_ALLOWED`（`retriable=false`）。

未配置 `allowedRemotePaths` 白名单时启动期 `tracing::warn!` 提醒。

### 5. 下载原子写

download 流程：

```
open temp = target + ".termbridge.<pid>.<seq>.tmp"
write chunks → temp
fsync(temp)
rename(temp, target)        # POSIX 原子
失败/中断 → 删除 temp
```

避免半写文件被 Agent 误读为目标完整产物。upload 不需要原子写（远端若中断，目标文件本就是不完整的，由调用方决定是否清理）。

## Consequences

- ✅ **进程列表不暴露凭据**：MCP 配置 args 无凭据字段，本机 `ps` / 任务管理器读不到。
- ✅ **host key 攻击防护**：`strict=yes/ask` 拒绝未知主机；MITM 的伪造 key 会被拒。
- ✅ **日志不含敏感信息**：三类正则覆盖常见凭证形态；`send_secret` 永不入审计。
- ✅ **SFTP 路径受控**：本地 / 远端均有 allowed roots，防 `../` 与 symlink 逃逸。
- ✅ **下载不产生半写文件**：temp + rename 原子切换。
- ⚠️ **`strict=ask` 在 MVP 等同拒绝新主机**：无 HITL，用户必须先手动 `ssh <host>` 一次添加 known_hosts。Phase 6 HITL 后可改为弹窗确认。
- ⚠️ **路径白名单需用户配置**：默认 `allowedRemotePaths=["/"]` 等于不限制，安全收益依赖用户主动收紧；启动 warn 提醒。
- ⚠️ **SSH Agent 依赖本机 `SSH_AUTH_SOCK`**：Windows 下需用户启动 ssh-agent 服务并 `ssh-add`；文档需说明。
- ⚠️ **正则脱敏有漏网可能**：非标准凭证格式（如 base64 内嵌）不会被命中；脱敏是 best-effort 兜底，根本防线仍是凭据不进 LLM context（Phase 6 HITL）。
