# ADR-0004：Remote Persistent Runtime Architecture

- **Status**: Accepted
- **Date**: 2026-08-09
- **Phase**: 3
- **Supersedes**: —

## Context

Phase 1/2 交付的是 **Interactive Session**：SSH 直连 PTY，PTY read task 持续把 output 写入本地 OutputEngine 的 RingBuffer。只要 `termbridge.exe` 进程活着，Session 就在；进程退出或 SSH 断开 → PTY read task 退出 → Session 进入 `Lost` / `Closed`，远端 shell 也随之被 SIGHUP 终止。

Phase 3 要解决的是 **跨 MCP 重启保活**：用户主动 `detach` 或 `termbridge.exe` 重启后，远端 shell 进程与 PTY output 不丢失，新的 `termbridge.exe` 可以 `attach` 回去继续读 / 写。这与 PLAN §2 R2（远端零安装）冲突，必须 **opt-in**：默认仍是 Interactive Session，仅当用户显式请求 persistent 时才部署远端 runtime。

### 备选方案

1. **tmux backend**：复用远端 `tmux`，`tmux new-session -d` 创建 detached session，`tmux send-keys` 输入，`tmux capture-pane` 读输出，`tmux attach` 接管。
2. **自研 Rust runtime（单二进制 daemon）**：远端跑 `termbridge-agentd`，通过 Unix socket 暴露 `create_session` / `attach` / `detach` / `read` / `write` 等 RPC，PTY 与 OutputBuffer 完全由 daemon 管理。
3. **混合**：daemon 为主，tmux 作为 fallback/debug backend。

### 为何不选 tmux 主路线

表面上 tmux 已解决 session 生命周期 / detach-attach / PTY / resize / buffer / daemon 化全部问题，诱惑很大。但它与 TermBridge 的核心抽象存在根本冲突：

- **tmux 暴露的是渲染后的 screen buffer，不是 PTY byte stream。** `tmux capture-pane` 返回的是「第 N 行第 M 列显示什么」，而不是 stdout / stderr / 控制事件的原始字节流。TermBridge 的目标不是「给人类一个远程终端」，而是「给 Agent 一个可编程 Terminal」—— Agent 需要的是 byte stream + cursor 状态，不是屏幕快照。
- **破坏 Phase 0-B 已验证的 OutputEngine 契约。** Phase 0-B（ADR-0003）设计了 RingBuffer + 双游标（mark_cursor / since_cursor）+ Waiter + settle 检测，全部基于 byte stream 语义。tmux 中间插了一层 terminal emulator，`wait_for("Server started")` 将退化成 `capture-pane` + 屏幕解析，立刻面临换行 / ANSI color / resize / alternate screen / vim / top / curses 等边界问题，所有 Phase 1 的 read_output / wait_for / tail_lines 契约都要重写。
- **tmux 是用户工具，不是 infrastructure。** Phase 0-C 建立的 `TerminalProvider` / `TerminalHandle` 抽象刻意把 SSH 隐藏在 Infrastructure 层，未来要扩展到 Local / Docker / Serial / WSL Provider。如果 Phase 3 引入 `TmuxProvider`，实际是在做 tmux 自动化层，方向会从「Persistent Terminal Backend」歪到「tmux wrapper」，未来 Local / Docker Provider 无法复用同一抽象。
- **附加问题**：依赖远端装 tmux（违反 R2 的精神，虽然 tmux 常见但不是标配）；`tmux attach` 是 TTY 模式，与 MCP 的 byte stream 接口不匹配；`capture-pane` 是 pull 模型，无法做 PTY read task 那样的流式 push。

### tmux 作为 fallback 的定位

完全不碰 tmux 也太极端。某些受限环境（远端无法 scp 二进制 / 内核 ABI 不匹配 / 用户偏好）下，daemon 部署失败时退到 tmux 仍有价值。因此 **tmux 作为可选 fallback/debug backend 保留接口**，但 Phase 3 不实现 —— 仅在 `PersistentProvider` trait 上预留 `TmuxBackend` 位置，待 Phase 5 再评估。

## Decision

**采用方案 2：自研 Rust native remote runtime（单二进制 daemon）为主，tmux 仅作预留 fallback 接口（Phase 3 不实现）。**

### 1. Backend：Rust native runtime

`termbridge-agentd` 是一个独立的 Rust 单二进制，跑在远端 Linux 主机上，职责：

```
termbridge-agentd
    │
    ├── PTY manager        ← 创建/销毁 PTY（portable-pty 或 nix::pty）
    ├── Session manager    ← 远端 session 生命周期（CREATED/ATTACHED/DETACHED/LOST）
    ├── Output buffer      ← 每个 session 一个 RingBuffer（默认 10MB）
    └── RPC server         ← Unix socket 上跑 length-prefixed JSON 协议
```

PTY 与 OutputBuffer 由 daemon 管理，client（`termbridge.exe`）通过 RPC 读写。**OutputEngine 的 byte-stream 语义在 daemon 内部完整保留**，client 端的 OutputEngine 退化为「attach 期间的一个 cache 视图」，detach 后清空，attach 时从 daemon 拉增量重建。

### 2. Daemon 形态：单二进制 + 单 Unix socket

```
~/.local/share/termbridge/
    termbridge-agentd          ← 单二进制
    termbridge.sock            ← Unix domain socket（runtime 通道）
    agentd.pid                 ← PID 文件（discovery 用）
    agentd.version             ← 版本文件（deploy handshake 用）
    sessions/                  ← （未来 audit/replay 用，Phase 3 不落盘）
```

**不设计成大服务**：不开 HTTP / REST / WebSocket / TCP / 数据库。一个二进制 + 一个 socket + 一个 session runtime，符合「remote zero install」精神（部署 = scp 一个文件 + 起一个进程）。

路径选 `~/.local/share/termbridge/`（XDG Base Directory 标准）而非 `~/.termbridge/`，与 `~/.local/bin` / `~/.local/share` 的 Linux 惯例一致。socket 路径优先 `$XDG_RUNTIME_DIR/termbridge.sock`（`/run/user/$UID/`，systemd user runtime），回退 `~/.local/share/termbridge/termbridge.sock`。

### 3. Transport：三阶段生命周期（关键修正）

> ⚠️ **关键决策**：SSH channel stdin/stdout **不是** daemon 的生命周期 owner。stdio 仅作 bootstrap，Unix socket 才是 runtime 通道。daemon 通过 fork+setsid 脱离 SSH channel 独立存活，persistent 语义由此保证。

#### 阶段 1：Bootstrap（SSH channel stdio）

首次 `open_session(host, persistent=true)` 触发 `check_remote_runtime(host)`，daemon 未运行时通过 SSH exec 启动：

```
termbridge.exe
    │ SSH exec: ~/.local/share/termbridge/termbridge-agentd bootstrap --sock <path>
    ▼
SSH channel (stdin/stdout) ──► agentd bootstrap 模式
    │
    │  daemon 执行：
    │    1. 检查 socket 是否已存在且活跃（另一个 client 已 bootstrap 过）
    │    2. 若否：fork + setsid 脱离 SSH channel，父进程写 socket path + daemon_id 到 stdout 后退出
    │    3. 子进程以 daemon 模式监听 Unix socket
    │
    ▼
stdout 返回（bootstrap 完成）：
    { "daemon_id": "daed_xxx", "socket": "/run/user/1000/termbridge.sock",
      "protocol_version": 1, "build": "0.1.0" }
```

**stdio 仅在 bootstrap 阶段使用**，daemon fork 后立即关闭 stdin/stdout，SSH channel 随父进程退出而关闭，子进程作为 orphan 由 init/systemd-user 收养。

#### 阶段 2：Daemonize（fork + setsid）

```
agentd bootstrap
    │
    ├── 父进程：写 stdout 握手响应 → exit 0（SSH channel 关闭）
    │
    └── 子进程：setsid() → 脱离控制终端 → close(0,1,2) → umask 0077
                    │
                    ├── 写 PID 文件 agentd.pid
                    ├── bind Unix socket（0600 权限）
                    └── 进入 RPC 事件循环（accept + 多路复用）
```

socket 权限 0600 + 父目录 0700，仅当前 Linux 用户可连。daemon 不实现任何应用层鉴权 —— **谁能连 socket = 谁能 SSH 到这台机器 = 谁已通过 SSH 鉴权**。

#### 阶段 3：Runtime（Unix socket via SSH channel 透传）

后续每次 client 操作（attach / send_input / read_output / detach 等），client 通过 SSH exec 开一个临时 channel 透传 socket：

```
termbridge.exe
    │ SSH exec: ~/.local/share/termbridge/termbridge-agentd proxy --sock <path>
    ▼
SSH channel (stdin/stdout) ◄──► agentd proxy 模式
    │                              │
    │  proxy 子进程：               │
    │    connect(socket)            │
    │    STDIN → socket             │
    │    socket → STDOUT            │
    │    双向字节流透传              │
    │                              │
    ▼                              ▼
client length-prefixed JSON ◄──► daemon RPC handler
```

**proxy 子进程是临时透传进程**，不持有状态，每次 client 操作开一个 SSH channel + 一个 proxy 进程，操作结束 channel 关闭，proxy 进程退出。daemon 本身长生命周期，不受 channel 开关影响。

> 备选实现：SSH `direct-streamlocal@openssh.com`（Unix domain socket forwarding），russh 0.62 未原生支持，需自实现。MVP 走 `agentd proxy` 子进程方案，简单且不依赖 SSH 扩展。Phase 5 评估 streamlocal 优化（少一次进程 fork）。

#### 为何不直接让 client 长连 socket

Windows client 无法直接连远端 Unix socket，必须经 SSH。两种 SSH 接入方式：

- **A. 每次 RPC 开 channel**（本 ADR 方案）：无状态，client 崩溃/重启无需清理，daemon 侧连接自然 EOF。代价：每次操作一次 SSH exec + proxy fork（开销 < 50ms，可接受）。
- **B. 长持 channel + 端口转发**（`ssh -L`）：性能更好，但 channel 断开需重连逻辑，且 `ssh -L` 是 CLI 子进程模式，与 russh 集成复杂。Phase 4 评估自动重连时再切换。

MVP 选 A，Phase 3-B 的「跨 MCP 重启」天然成立 —— termbridge.exe 重启后开新 channel 即可，daemon 完全不感知。

### 4. Protocol：length-prefixed JSON + request id

**不用 JSON-RPC 2.0**。daemon 是 TermBridge 内部组件，不是开放 API，不需要 jsonrpc / error code 等正式字段。MVP 用最简的 length-prefixed JSON + request id：

```
[4 bytes big-endian length] [JSON payload UTF-8]
```

#### 请求（client → daemon）

```json
{ "id": 1, "method": "session.create", "params": {
    "shell": "/bin/bash", "cwd": null,
    "pty_size": { "rows": 40, "cols": 120 },
    "name": "python server"
} }
```

#### 响应（daemon → client，同步）

```json
{ "id": 1, "ok": true, "result": { "session_id": "sess_abc123", "written": 0 } }
```

```json
{ "id": 1, "ok": false, "error": { "code": "INVALID_ARGUMENT", "message": "shell not found" } }
```

#### 事件（daemon → client，异步推送）

```json
{ "event": "pty_data", "session_id": "sess_abc123",
  "cursor_start": 15000, "cursor_end": 15234, "data": "<base64>" }
```

```json
{ "event": "pty_exit", "session_id": "sess_abc123", "exit_code": 0 }
```

```json
{ "event": "session_lost", "session_id": "sess_abc123", "reason": "pty eof" }
```

**保留 `id` 字段**：虽然 MVP 多数请求是同步等待响应，但 `id` 让未来异步 pipeline（批量发请求 + 乱序响应）成为可能；事件无 `id`，靠 `event` 字段区分。client 实现按 `id` 匹配请求/响应，无 `id` 的消息当事件派发。

选 JSON 而非 MessagePack / protobuf 的理由：**调试友好**（开发期可手工 `agentd proxy | jq` 看流），daemon 协议非高频路径，序列化开销可忽略。未来若 hot path 性能不足（PTY output 大流量下 JSON 解析成为瓶颈），可单独把 `pty_data` 事件改为 length-prefixed binary frame，其他控制消息保留 JSON。Phase 3 不做这个优化。

#### 版本握手（handshake）

client 连上 socket 后第一条消息必须是 `hello`：

```json
{ "id": 0, "method": "hello", "params": {
    "client_protocol_version": 1, "client_build": "0.1.0"
} }
```

daemon 响应：

```json
{ "id": 0, "ok": true, "result": {
    "daemon_protocol_version": 1, "daemon_build": "0.1.0", "daemon_id": "daed_xxx"
} }
```

`client_protocol_version != daemon_protocol_version` → daemon 返回 `ok:false` + `code: PROTOCOL_MISMATCH`，client 提示用户 `upgrade_runtime(host)`。**协议版本号独立于 build 版本**，仅在不向后兼容的协议变更时 bump。

### 5. Buffer 归属：detach 期间存远端 daemon 内存 RingBuffer

**必须远端保存**，否则 persistent session 没意义。架构：

```
Remote daemon
    │
    Session
        │
        PTY → RingBuffer (内存, 默认 10MB, 满则丢最旧)
        │
        （磁盘落盘 Phase 3 不开, 留 Phase 5 audit/replay）
```

#### Cursor 协议字段

每次 `pty_data` 事件和 `read_output` 响应都带三个 cursor 字段：

```json
{
  "cursor_start": 15000,   // 本批数据的起始绝对字节偏移
  "cursor_end": 15234,     // 本批数据的结束绝对字节偏移（= 下批 cursor_start）
  "is_truncated": false,   // cursor_start 之前的数据是否已被 RingBuffer 丢弃
  "data": "<base64>"
}
```

#### attach 时增量重建

client attach 时声明 `since_cursor`（client 最后读到的字节位置），daemon 返回 `since_cursor → written` 之间的增量：

```json
{ "id": 2, "method": "session.attach", "params": {
    "session_id": "sess_abc123", "since_cursor": 15000
} }
```

daemon 响应：

```json
{ "id": 2, "ok": true, "result": {
    "cursor_start": 15000, "cursor_end": 20000, "is_truncated": false,
    "data": "<base64 of 15000..20000>"
} }
```

若 `since_cursor` 已被 RingBuffer 截断（`since_cursor < buffer.cursor_start`）：

```json
{ "id": 2, "ok": true, "result": {
    "cursor_start": 17000, "cursor_end": 20000, "is_truncated": true,
    "data": "<base64 of 17000..20000>"
} }
```

client 看到 `is_truncated=true`，自行决定是否 `tail_lines` 兜底（拉 RingBuffer 当前全部内容的尾部 N 行）。

#### detach 时

client 关闭 SSH channel，daemon 把 session 状态从 `ATTACHED` 改为 `DETACHED`，PTY read task 继续往 RingBuffer 写。RingBuffer 满则丢最旧数据（与 Phase 0-B 契约 2 一致）。

#### 不落盘

Phase 3 不写磁盘。Phase 5 做 audit / replay 时再开滚动日志（与本地 OutputEngine 的 tee 机制对称）。

### 6. Deploy：opt-in scp 单二进制 + 自动检查 + 版本协议

首次 `open_session(host, persistent=true)` 触发 `check_remote_runtime(host)`：

```
check_remote_runtime(host)
    │
    ├── SSH exec: test -x ~/.local/share/termbridge/termbridge-agentd
    │       │
    │       ├── 存在 + 可执行 → 读取 agentd.version，与本地 protocol_version 比对
    │       │       │
    │       │       ├── 匹配 → bootstrap daemon（见 §3 阶段 1）
    │       │       │
    │       │       └── 不匹配 → 提示用户 upgrade_runtime（Phase 3 不自动升级）
    │       │
    │       └── 不存在 → deploy_runtime(host)
    │                       │
    │                       ├── SFTP upload 本地预编译的 agentd → 远端 ~/.local/share/termbridge/
    │                       ├── chmod +x
    │                       ├── 写 agentd.version（protocol_version + build）
    │                       └── bootstrap daemon
    │
    └── daemon 已运行 → 直接 attach
```

**不每次上传**。daemon 二进制预编译为 `x86_64-unknown-linux-gnu`（MVP 只支持这一目标，覆盖 90% Linux 主机；aarch64 / musl 留 Phase 5 cross-compile matrix）。二进制与 `termbridge.exe` 同源码仓库，CI 产物，本机缓存于 `%LOCALAPPDATA%\TermBridge\agentd\`。

daemon 自身不自动升级（避免每次 attach 都检查版本），由用户手动触发 `upgrade_runtime(host)`（Phase 3-C 不暴露为 MCP 工具，留作 CLI / 配置项）。

### 7. Discovery：探测 + 启动 + 部署三态

```
RemoteRuntimeState
    │
    ├── Missing      → 部署 + 启动
    ├── Stopped      → 启动（bootstrap）
    └── Running      → 直接 attach
```

探测通过 SSH exec `pgrep -f termbridge-agentd`（MVP）。Stopped 状态由 daemon 写 PID 文件 `~/.local/share/termbridge/agentd.pid` 区分（运行中存在且 PID 存活；停止则文件残留但 PID 不存在）。daemon 启动时检查 stale socket 并清理。

### 8. Session 模型：Attachment（位置）+ State（生命周期）分离

> ⚠️ **关键修正**：原草案把 `Detached` 混入 `SessionAttachment` 枚举，逻辑上不对 —— Detached 是状态（PTY 在远端但无 client attach），不是 attachment 类型。拆成两个正交维度。

#### Attachment（位置）：Session 通过什么后端跑

```rust
enum SessionAttachment {
    /// 本地 PTY handle（Interactive Session，Phase 1/2 路径）
    Local(Arc<dyn TerminalHandle>),
    /// 远端 daemon client（Persistent Session，attach 状态）
    Remote(PersistentClient),
    /// 无后端连接（Persistent Session detached 状态，PTY 在远端 daemon 内存中）
    None { remote_session_id: String, remote_host: HostName },
}
```

#### State（生命周期）：Session 处于什么阶段

```rust
enum SessionState {
    Creating,
    Ready,        // Interactive: Local attachment; Persistent: Remote attachment
    Detached,     // 仅 Persistent Session: PTY 在远端 daemon，本地无 attachment（attachment == None）
    Closing,
    Closed,
    Lost,         // PTY EOF 或 daemon 崩溃
}
```

#### 状态机

```
普通 Session（Interactive）：       Persistent Session：

    Creating                            Creating
       │                                   │
       ▼                                   ▼
      Ready                              Ready (attachment=Remote)
       │                                   │
       │                                   │ detach → attachment=None
       │                                   ▼
       │                                Detached
       │                                   │ attach → attachment=Remote
       │                                   ▼
       │                                Ready (attachment=Remote)
       ▼                                   │
      Lost                                ▼
                                       Lost
```

`Session` 持有 `attachment: SessionAttachment` + `state: SessionState`。`read_output` / `send_input` 等方法根据 attachment 分发到本地 handle 或远端 client；`Detached` 状态下这些方法返回 `TermError::SessionDetached`（client 必须先 attach）。**Session 不感知自己在哪** —— Local / Remote / None 只是 attachment 的变体，业务逻辑（OutputEngine cursor 推进、Policy 检查、idleReaper）对所有 attachment 类型一致。

### 9. MCP 工具：3 个新工具

| 工具 | 参数 | 返回 | 说明 |
|---|---|---|---|
| `list_remote_sessions` | `host: HostName` | `Vec<RemoteSessionInfo>` | 列出远端 daemon 上当前 Linux 用户的所有 session（含 detached） |
| `attach_remote_session` | `host: HostName`, `session_id: String` | `local_session_id: String` | attach 到远端 session，返回本地 session_id |
| `detach_session` | `session_id: String`（本地 id） | — | detach 当前 session，远端 PTY 继续运行 |

`attach_remote_session` 必须带 `host` 参数 —— 未来多机器场景下，`session_id` 是 daemon 内部 ID，跨主机不唯一，必须靠 `host` 消歧。

`list_remote_sessions` 返回结构化字段而非裸 id：

```json
[
  {
    "id": "sess_abc123",
    "name": "python server",       // 可选，create_session 时指定
    "state": "detached",            // created / attached / detached / lost
    "created_at": "2026-08-09T...",
    "last_activity_at": "2026-08-09T...",
    "pty_size": { "rows": 40, "cols": 120 },
    "written": 23456                // RingBuffer 当前 written 计数
  }
]
```

`open_session(host, persistent=true, name="...")` 是创建远端 session 的入口（Phase 1 `open_session` 加 `persistent?: bool` 和 `name?: String` 可选参数），不新增 `create_remote_session` 工具。

#### 权限边界（Phase 3 限定）

daemon 只管 **当前 Linux 用户** 的 session，socket 路径在 `$XDG_RUNTIME_DIR/` 或 `~/.local/share/termbridge/`，0600 权限仅当前用户可连。**Phase 3 不考虑多用户共享 daemon**（一个 Linux 用户 = 一个 daemon 实例 = 一组 session）。多用户场景（如团队共享 bastion）留 Phase 5 评估，需加 socket 权限模型 + per-user session 隔离。

### 10. Phase 3 拆分

```
Phase 3-A：远端 daemon + Linux CLI client
    W1: ADR-0004（本文档）
    W2: daemon 原型（单二进制 + 协议层 + PTY 管理 + RingBuffer）
        + Linux CLI client（termbridge-cli，纯测试用，不进 MCP）
        → verify: 本地集成测试 daemon 可 create/list/attach/detach，
                  detach 后 PTY 继续跑，kill cli 后 daemon 仍活
    W3: TermBridge Windows client（PersistentProvider + check_remote_runtime + deploy + SSH proxy）
        → verify: 单测覆盖 deploy/discover/attach/detach/增量读

Phase 3-B：跨 MCP 重启重连
    W4: restart termbridge.exe → attach 远端 session 仍可读
        → verify: e2e，daemon 进程不依赖 termbridge.exe 生命周期

Phase 3-C：MCP 工具 + e2e
    W5: list_remote_sessions / attach_remote_session / detach_session + open_session(persistent=true)
    W6: e2e（open persistent → detach → kill termbridge → restart → list → attach → 续读）
        + Phase 3 报告 + git 提交
```

**W2 先在 Linux 主机（`203.0.113.200`）原生编译 daemon + CLI client 跑通本地集成测试**，验证 PTY lifecycle / RPC / detach-attach / daemon 独立存活。这一步不需要 Windows client，最大限度降低风险。W3 再做 Windows cross-compile + SSH proxy 接入。

### 11. Daemon crash 语义（Phase 3 不恢复）

**daemon 进程崩溃 = 所有 detached session 丢失**。Phase 3 不做 PTY state 恢复 / 磁盘持久化，原因：

- PTY state 恢复极复杂（需 snapshot PTY 内核状态 + 子进程树 + 环境变量 + cwd），Rust 生态无成熟方案
- Phase 3 目标是「跨 MCP 重启保活」，不是「跨 daemon 崩溃保活」—— 前者通过 daemon 独立进程解决，后者需 systemd unit + 持久化，Phase 4/5 再评估
- 与 tmux server 崩溃同等后果，但 daemon 进程逻辑简单（无插件 / 无脚本），崩溃概率远低于 tmux

daemon 崩溃后，client 侧 session 检测到 socket EOF → 状态转 `Lost`，client 提示用户重新 `open_session(persistent=true)` 重建。Phase 5 加 audit 日志后可做 command 回放辅助恢复。

## Consequences

- ✅ **保持 byte-stream 语义**：OutputEngine 契约（ADR-0003）在 daemon 内部完整保留，client 端 OutputEngine 退化为 attach 期间的 cache 视图，read_output / wait_for / tail_lines 契约不变。
- ✅ **与 TerminalProvider 抽象一致**：daemon 协议是「Terminal over RPC」，与 `TerminalHandle` trait 语义等价，未来 Local / Docker / Serial Provider 可复用同一协议。
- ✅ **persistent 语义成立**：daemon 通过 fork+setsid 脱离 SSH channel 独立存活，termbridge.exe 重启 / SSH channel 断开均不影响 daemon 与 PTY。client 重启后开新 SSH channel + proxy 连 socket 即可 attach。
- ✅ **安全模型简单**：不开 TCP，daemon 不实现鉴权，鉴权完全继承 SSH（known_hosts + ssh-agent + IdentityFile + Phase 2 TOFU）。socket 0600 权限 + 父目录 0700，仅当前 Linux 用户可连。
- ✅ **opt-in 部署**：默认 Interactive Session 不变，仅 `persistent=true` 才触发部署，符合 R2。
- ✅ **tmux 接口预留**：未来受限环境可加 `TmuxBackend` 作为 `PersistentProvider` 的另一个实现，不影响主路线。
- ✅ **版本协议防漂移**：`hello` handshake + `agentd.version` 文件双重校验，client/daemon 协议版本不匹配时明确拒绝并提示升级。
- ✅ **Session 模型正交**：Attachment（位置）与 State（生命周期）分离，`Detached` 是状态不是 attachment 类型，未来扩展（多 backend / 多 attachment 切换）不污染状态机。
- ⚠️ **需 cross-compile**：daemon 二进制要为远端目标编译（MVP 只 `x86_64-unknown-linux-gnu`），CI 要加 build matrix。Phase 3-A W2 先在 Linux 主机上原生编译验证，W3 再做 Windows 主机 cross-compile。
- ⚠️ **daemon 进程崩溃丢全部远端 session**：单 daemon 进程无 supervisor，崩溃 = 所有 detached session 丢失（见 §11）。Phase 4 加 systemd unit / supervisor；Phase 5 加磁盘持久化与 audit 回放。Phase 3 接受这个限制。
- ⚠️ **每次 RPC 操作有 SSH exec + proxy fork 开销**：MVP 方案 A（每次开 channel），单次操作开销 < 50ms，可接受。高频小操作（如 `read_output` 轮询）可能累积延迟，Phase 4 评估长持 channel + 自动重连（方案 B）。
- ⚠️ **PTY output 大流量下 JSON 解析可能成为瓶颈**：`tail -f` 大文件 / `find /` 等场景下 `pty_data` 事件高频，JSON 解析有开销。Phase 3 不优化，必要时单独把 `pty_data` 改为 length-prefixed binary frame，其他控制消息保留 JSON。
- ⚠️ **daemon 版本与 client 不匹配需手动升级**：协议不匹配时拒绝 attach 并提示 `upgrade_runtime`，Phase 3 不自动升级。Phase 5 评估自动升级策略（按 protocol_version 滚动）。
- ⚠️ **Phase 3 不实现 tmux fallback**：`TmuxBackend` 仅在 ADR 中预留，受限环境用户暂只能用 Interactive Session。Phase 5 再评估。
- ⚠️ **stdin/stdout bootstrap 有 PID race 风险**：两个 client 同时 bootstrap 同一 host 可能 fork 两个 daemon，第二个 fork 时检测到 socket 已存在应直接退出并返回现有 daemon 信息。bootstrap 实现需加文件锁（`flock(agentd.pid)`）序列化。
