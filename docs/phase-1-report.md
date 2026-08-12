# Phase 1 报告：MVP 核心（7 工具 + 安全 baseline + 连接健壮性）

- **Date**: 2026-08-09
- **Status**: ✅ Complete
- **PLAN.md**: v0.4 §7.4
- **前置**: Phase 0-C（vertical slice 6 工具打通）

## 目标

在 Phase 0-C vertical slice 之上交付生产可用 MVP：补齐 **SFTP 文件传输**、**安全 baseline**（known_hosts 校验 + SSH Agent 认证 + 日志脱敏）、**连接健壮性**（keepalive + idleReaper + 并发会话），并锁定 Output 缓冲策略与安全模型两条 ADR。

## 交付物

### 代码（4 层架构）

| 层 | 文件 | 职责 |
|---|---|---|
| domain | `src/domain/provider.rs` | `TerminalProvider` / `TerminalHandle` trait（加 `as_any()` 下转）+ `Host`（新增 `user_known_hosts_file` / `strict_host_key_checking` / `identity_files: Vec`）+ `ControlKey` / `PtySize` / `TransferDirection` / `SftpCanonicalize` trait + `TermError`（新增 `HostKeyRejected` / `SftpError` / `LocalPathNotAllowed` / `RemotePathNotAllowed`）|
| domain | `src/domain/session.rs` | `Session` 实体 + PTY read task + 状态机（Ready/Closing/Closed/Lost）+ `is_idle` / `last_activity` |
| domain | `src/domain/output.rs` | OutputEngine（Phase 0-B 完成，4 模式 read_output：settle / wait_for / tail_lines / since_cursor）|
| application | `src/application/hosts.rs` | `HostManager`（list_hosts 扫描 ~/.ssh/config）|
| application | `src/application/sessions.rs` | `SessionManager`（`DashMap` 并发 + `sftp_transfer` 下转 SFTP + Lost session 清理 + `idle_reaper_loop`）|
| application | `src/application/path_policy.rs` | `PathPolicy`（`allowed_local_paths` canonicalize + `allowed_remote_paths` realpath + null 字节拒绝）|
| infrastructure | `src/infrastructure/ssh.rs` | `SshProvider` + `SshTerminalHandle`（`Channel::split()` 读写分离 + keepalive task + ssh-agent 降级 + `open_sftp_provider`）+ `SshClientHandler`（known_hosts 严格校验）|
| infrastructure | `src/infrastructure/sftp.rs` | `SftpProvider`（upload / download 原子写 / canonicalize）|
| infrastructure | `src/infrastructure/redact.rs` | `RedactingWriter` + `RedactingMakeWriter`（三类正则脱敏 + 行缓冲）|
| infrastructure | `src/infrastructure/sshconfig.rs` | `ssh -G` 解析（ADR-0006）|
| transport | `src/transport/mcp/server.rs` | `TermBridgeServer`（rmcp 7 工具 + `ToolError` 结构化错误）|
| bin | `src/main.rs` | tracing → stderr 经 `RedactingMakeWriter` 脱敏 + 组装依赖 + `serve_stdio()` |

### 7 个 MCP 工具

| 工具 | 映射 | 说明 |
|---|---|---|
| `list_hosts` | HostManager::list_hosts | 扫描 ~/.ssh/config，返回别名列表 |
| `open_session` | SessionManager::open_session | ssh -G → SshProvider::open → Session::new，返回 session_id |
| `send_input` | SessionManager::send_input | 写 PTY stdin（立即返回）|
| `read_output` | SessionManager::read_output | 4 模式：settle / wait_for / tail_lines / since_cursor |
| `send_control` | SessionManager::send_control | ctrl+c / ctrl+d / ctrl+z / tab / enter / escape |
| `close_session` | SessionManager::close_session | EOF + disconnect，幂等 |
| `sftp_transfer` | SessionManager::sftp_transfer | upload / download，路径策略 + 原子写 |

### ADR

- [ADR-0003](adr/0003-output-buffer-strategy.md)：Output 缓冲策略（1MB 默认 / 双游标语义 / settle 阈值 / 截断检测）
- [ADR-0005](adr/0005-security-model.md)：安全模型（凭据优先级 / known_hosts / 日志脱敏 / SFTP 路径策略 / 下载原子写）

### 测试统计

- **单元测试**：100 个全通过（`cargo test`：100 passed; 0 failed; 1.32s）
  - ssh 模块：known_hosts 校验 9 个（match / mismatch / unknown / ask / no / 非 22 端口 / 大写归一化 / 无路径拒绝）+ ssh-agent 降级 1 个 + keepalive 常量 1 个
  - sftp 模块：原子写 rename / 失败清理 / tmp 路径生成 3 个
  - path_policy 模块：远端 9 个 + 本地 5 个 + `is_under_remote` 5 个
  - redact 模块：8 个（三类正则 + 大小写 + 多行 + 行内多凭证）
  - sessions 模块：SESSION_NOT_FOUND / list / idleReaper / DashMap 并发 / Lost 清理 11 个
  - domain output / session / provider / sshconfig 余下补足

## 关键实现与决策

### 1. known_hosts 严格校验（`SshClientHandler::check_server_key`）

复用 `ssh -G` 的 `userknownhostsfile` + `stricthostkeychecking`：调 `russh::keys::check_known_hosts_path` 比对 server public key。`yes` / `ask` 严格模式：key 不匹配（`KeyChanged`，疑似 MITM）→ `error!` + 拒绝；host 未知 → `ask` 视为 `yes`（MVP 无 HITL）拒绝；`no` 仅 WARN 接受。拒绝原因通过 `Arc<Mutex<Option<String>>>` 共享给 `SshProvider::open`，区分 `HOST_KEY_REJECTED` 与普通 `CONNECT_FAILED`。

### 2. ssh-agent 降级链（凭据优先级：Agent > IdentityFile > HITL）

`authenticate_with_agent` 连接 agent（Unix `SSH_AUTH_SOCK` UDS / Windows `\\.\pipe\openssh-ssh-agent` named pipe）→ `request_identities` → 遍历 `PublicKey` / `Certificate` 调 `authenticate_publickey_with` / `authenticate_certificate_with`，任一成功即返回。agent 不可用 / 无 identities / 全部失败 → `Ok(false)`，降级到 `authenticate_with_identity_files` 遍历 `host.identity_files` 逐个尝试。两者均失败 → `AUTH_FAILED`。**降级非报错**，单一来源失败不影响后续尝试。

### 3. `Channel::split()` 读写分离（Phase 0-C 遗留，Phase 1 保持）

`SshTerminalHandle` 用 `Channel::split()` 拆 `ChannelReadHalf`（read task 独占 `wait()`，`tokio::sync::Mutex`）+ `ChannelWriteHalf`（`&self` 方法，write/control/resize/close 无锁并发）。Phase 1 因 SFTP 与 keepalive 需要在持锁状态下 `await`，`session` 字段从 `parking_lot::Mutex` 改为 `Arc<tokio::sync::Mutex<Option<Handle<...>>>>`，并让 keepalive task 持 `Arc` clone。

### 4. keepalive + idleReaper 协作

- **keepalive**：`KEEPALIVE_INTERVAL_SECS=10s` + `KEEPALIVE_MAX_MISSES=3`，每轮 `send_ping`（带 `INTERVAL` timeout）；连续 3 次无响应 → take `session` 并 `disconnect` → PTY read task 检测 EOF → Session 置 Lost。
- **idleReaper**：`IDLE_REAPER_INTERVAL_SECS=30s` tick + `IDLE_TIMEOUT_SECS=1800s` 超时。关键：**先释放读锁再 close**——`iter` 收集 idle id → 释放 → 逐个 `remove`（写锁瞬间）→ `close`（无锁）。借鉴 pty-mcp `session.go` 避免在持锁状态下 close 同一 map 触发死锁。
- 两者独立：keepalive 处理半开 socket（网络层），idleReaper 处理僵尸 session（应用层）。

### 5. SFTP `as_any` 下转 vs trait 污染

`TerminalHandle` trait 加 `Any` supertrait + `as_any()` 方法，`SessionManager::sftp_transfer` 通过 `handle.as_any().downcast_ref::<SshTerminalHandle>()` 下转访问 `open_sftp_provider`。**取舍**：不把 SFTP 能力塞进 `TerminalHandle` trait（避免 LocalProvider / DockerProvider 强制实现无意义的 SFTP 方法）。下转失败 → `INVALID_ARGUMENT`（"session does not support SFTP"）。

### 6. DashMap 并发模型

`SessionManager.sessions: Arc<DashMap<SessionId, Arc<Session>>>` 替代 Phase 0-C 的 `Mutex<HashMap>`。读（get / iter）与写（insert / remove）分片无锁并发，idleReaper 持 `Arc` clone。并发测试（4 线程 × 20 insert/remove + 100 轮 iter + 50 轮读写）5s timeout 验证无死锁。Lost session 在 send_input / read_output / send_control 返回 `SessionClosed` 时由 `cleanup_detached_session` 移除，防泄漏。

## 端到端验证

e2e vertical slice 持续通过（Phase 0-C 既有用例 + Phase 1 增强）：

```
>>> open_session host=203.0.113.200       # ssh-agent 降级到 IdentityFile + known_hosts 校验通过
<<< session_id = sess_0                    # keepalive task 后台 spawn，不影响 open 延迟
>>> read_output (settle)                   # MOTD + prompt
>>> send_input "echo HELLO_TERMBRIDGE\n"
>>> read_output (wait_for="HELLO_TERMBRIDGE")   # 命中推进 mark
>>> send_control ctrl+c                    # 长任务中断
>>> close_session                          # abort keepalive + eof + disconnect

# SFTP 切片
>>> sftp_transfer direction=upload  local=./file.txt remote=/tmp/file.txt   # 路径策略放行
>>> sftp_transfer direction=download remote=/tmp/file.txt local=./out.bin   # tmp + fsync + rename
<<< ok                                     # 目标文件原子出现，无半写

# host key 校验
>>> open_session host=unknown-host         # 不在 known_hosts → HOST_KEY_REJECTED
<<< { code: "HOST_KEY_REJECTED", retriable: false }
```

100 个单元测试全通过（含 known_hosts 9 例、ssh-agent 降级、keepalive / idleReaper 常量、DashMap 并发、Lost 清理、原子写、路径策略 14 例、脱敏 8 例）。

## 已知限制（Phase 1 范围内）

1. **`strict=ask` 等同拒绝新主机**：MVP 无 HITL UI，用户必须先手动 `ssh <host>` 一次写入 known_hosts。Phase 6 加 HITL 后改为弹窗确认。
2. **ssh-agent Windows named pipe 依赖 OpenSSH 服务**：用户需启动 `ssh-agent` 服务并 `ssh-add`；文档需说明。Unix 依赖 `SSH_AUTH_SOCK` 环境变量。
3. **SFTP channel 不池化**：每次 `sftp_transfer` 开新 channel（`channel_open_session` + `request_subsystem("sftp")`），调用结束 best-effort close。频繁小文件传输有开销，Phase 2 评估池化。
4. **无自动重连**：Connection 断 → Session Lost，不自动重连。Phase 3 Persistent Session + Phase 4 Connection pool 再做。
5. **远端路径默认全放行**：`allowedRemotePaths=["/"]` 等于不限制，启动期 `WARN` 提醒用户收紧；安全收益依赖用户主动配置白名单。
6. **日志 tee 未实现**：ADR-0003 §6 预留，RingBuffer 满后旧数据丢失，Phase 2+ 加 MultiWriter + 滚动日志兜底。
7. **`wait_for` 全量重扫**：每次 Notify 唤醒扫 mark→written 全段，高频 `tail -f` + 长 pattern 有 CPU 成本，MVP 串行可接受。

## 下一步（Phase 2）

按 PLAN.md §7.5：

- **ProxyJump / Bastion / SOCKS** 支持（russh 嵌套 connect）
- **SFTP 增强**：mkdir / 目录递归 / 权限 / channel 池化评估
- **known_hosts 完整处理**：`ask` 模式 HITL 询问写入（依赖 Phase 6 UI 雏形）、`hash_known_hosts`、多 known_hosts 文件
- **Policy 接口 + DefaultPolicy**：命令 blocklist / dangerous command confirm（§8），在 Application 层拦截，不侵入 SSH 层
- **日志 tee（MultiWriter）**：buffer 溢出数据滚动日志兜底

> Phase 1 不碰：GUI、数据库、Workspace、Persistent Session、自动重连。
