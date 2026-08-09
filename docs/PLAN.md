# TermBridge 方案文档

> 状态：Draft v0.4 · 日期：2026-08-09
>
> 本文档是 TermBridge 的总体方案，权威设计决策以独立 ADR 形式沉淀（`docs/adr/`），本文档随 ADR 更新而演进。
>
> v0.4 变更：合并 pty-mcp + classfang 源码研读的 10 条借鉴点（详见 [docs/reference-analysis.md](file:///e:/Code/TermBridge/docs/reference-analysis.md)）。核心——OutputEngine 结构明确化（§5.2：物理环形 + 单调 written 计数器 + 双游标 + Notify + 日志 tee）；read_output 加 since_cursor 参数（§5.3）；§4.6 契约补充（5 细化 + 新增 11/12）；drain 模式改 settle 语义（§5.3）；审计脱敏引用 pty-mcp 正则（§5.5）；工具错误格式统一 ToolError（§6）；Phase 1 SFTP 加路径安全 + 原子写（§9）；Phase 1 加 keepalive + idleReaper + known_hosts（§9）。
>
> v0.3 变更：收敛语义而非加功能。核心——钉死 Session/Output 行为契约（§4.6）；重构 Connection/Session 关系（§4.3）；TerminalProvider 拆 Provider/Handle 两层（§4.4）；read_output 重定义 drain/tail/wait_for 语义（§5.3）；区分 Interactive vs Persistent Session（§5.6）；Phase 1 纳入 SFTP+Security baseline；Phase 0 拆 0-A/0-B/0-C vertical slice；portable-pty 降优先级；SSH Config 优先 `ssh -G` 复用 OpenSSH；MCP stdio only；工具压到 6 个。
>
> v0.3 原则：**不再往上加功能。把 Session + Connection + PTY + OutputBuffer + read_output/wait_for 的行为契约彻底定义清楚，然后用 Rust 做最小 vertical slice。**

---

## 1. 定位

> **A remote terminal bridge for AI agents.**
>
> Connect your AI agent to any SSH-accessible machine and work with it like a local terminal.

TermBridge 是一个面向 AI Agent 的**远程终端 MCP Server**。Agent 通过统一的 Session 抽象操作多台远端 Linux 主机，支持持久 PTY、实时输出、交互式输入、文件传输。

**一句话原则**：给 Agent 一个真正的 Terminal，而不是一个 `exec` 接口。SSH 只是实现细节。

**三条不可动摇的原则**：
1. **Agent 面向 Terminal，不面向 SSH。** 工具是 `open/send/read/control`，不是 `ssh_connect/ssh_exec`。
2. **Session 是一等公民。** Host / Connection / Session 严格分离；MCP 只是接口之一。
3. **默认零远端安装，可选持久化。** 核心 SSH+SFTP 零安装；跨重启保活才按需部署远端 daemon。

**项目真正的核心 IP**（其余都是 Infrastructure 或 Transport）：

```
                 TermBridge
                     │
        ┌────────────┼────────────┐
        │            │            │
        ▼            ▼            ▼
   HostResolver   Session     Output Engine
                     │            │
                     │            ├── RingBuffer
                     │            ├── Cursor
                     │            └── Waiter
                     │
                     ├── PTY
                     ├── Input
                     ├── Control
                     └── Lifecycle
```

SSH/SFTP 是 Infrastructure，MCP 是 Transport。**真正值得设计好的就是 Session + Output Engine。** 这部分跑顺，后面 SFTP/ProxyJump/persistent daemon 都是工程问题；这部分没设计好，功能越多越难救。

---

## 2. 核心需求

| #   | 需求                          | 优先级 | 说明                                |
| --- | --------------------------- | --- | --------------------------------- |
| R1  | Agent 只装在主用 Windows         | 必须  | 单点部署，Windows 侧承载 MCP Server       |
| R2  | 远端 Linux 尽量零安装              | 必须  | 核心功能不依赖远端任何额外组件                   |
| R3  | 管理多个 SSH 主机                 | 必须  | Host 别名 / 元数据管理                   |
| R4  | Agent 可随时连接任意主机             | 必须  | 无需重启即可切换/复用连接                     |
| R5  | 持久 shell / 交互式终端            | 必须  | 一次连接，多次输入输出；非一次性 `exec`           |
| R6  | 实时获得输出                      | 必须  | 流式读取，`wait_for` 正则阻塞等待            |
| R7  | Ctrl+C / 密码 / REPL / 长任务    | 必须  | 控制字符、交互式输入、长任务 attach             |
| R8  | 支持 `~/.ssh/config`          | 必须  | 自动发现 Host、IdentityFile、ProxyJump 等 |
| R9  | 方便扩展成"远程工作台"                | 长期  | 端口转发、审计、Workspace 等可插拔             |

**MVP（v0.1）覆盖**：Hosts(OpenSSH config) + Terminal(SSH/PTY/Interactive/Ctrl+C/resize/realtime/wait_for/bounded buffer) + Sessions(multiple/lifecycle/reuse) + Files(upload/download) + Security baseline(host key/SSH Agent/log redaction) + MCP(stdio)。

> ⚠️ **取舍**：跨 MCP 重启保活（远端 daemon）与 R2 冲突。采用 opt-in 策略——核心零安装，仅"跨重启保活"时按需部署。Phase 3。

---

## 3. 技术选型：Rust

### 3.1 决策

**实现语言：Rust**（用户确认，借鉴 pty-mcp 原理而非代码）。

> 事实纠正：pty-mcp 实际是 **Go** 实现（`creack/pty` + `x/crypto/ssh`），非 TS。本方案借鉴其 Session/PTY/ring buffer/wait_for 的**设计与原理**，用 Rust 重新实现。

### 3.2 Rust 可行性分析

| pty-mcp (Go) 能力          | Rust 对等方案                          | 可行性 | 风险                          |
| ------------------------- | ---------------------------------- | --- | --------------------------- |
| `x/crypto/ssh` PTY+shell  | `russh`（纯 Rust，tokio 异步）            | ✅ 高 | ProxyJump 嵌套、MFA 成熟度稍弱      |
| `creack/pty` 本地 PTY       | `portable-pty`（wezterm 出品）         | ✅ 高 | **MVP 不需要**（远端 PTY 走 SSH RequestPty），仅未来 LocalProvider 用 |
| ring buffer + wait_for    | 纯应用层逻辑，`tokio::sync` + 环形缓冲         | ✅ 高 | 与语言无关                       |
| SSH Config 解析             | `ssh2-config` **或 `ssh -G` 子进程**   | ⚠️ 中 | 见 §3.4 ADR-0006，优先复用 OpenSSH |
| SFTP                      | `russh-sftp`                       | ✅ 高 | 基本 upload/download 覆盖        |
| MCP 协议                    | `rmcp`（官方 Rust SDK，tokio）           | ✅ 高 | 0.x 版本，API 可能变动             |
| persistent 远端 daemon      | 自研（参考 ai-tmux 原理）                  | ⚠️ 中 | Phase 3 opt-in，不阻塞 MVP      |

**结论**：Rust 可行。MVP 全部能力有成熟 crate 覆盖。主要不确定性：`ssh2-config` Match 子句（用 `ssh -G` 规避）、`rmcp` 0.x API（Phase 0-A 验证）。

### 3.3 核心依赖（Phase 0-A 最终确认）

| 领域        | 候选                              | 备选                  | 决策点                  |
| --------- | ------------------------------- | ------------------- | -------------------- |
| MCP SDK   | `rmcp`（官方）                      | 自研薄协议层              | API 稳定性（stdio only）  |
| SSH       | `russh`（纯 Rust，异步）              | `ssh2`（libssh2 绑定）  | ProxyJump/SFTP 支持完整度  |
| SSH Config| **`ssh -G <host>` 子进程**（优先）     | `ssh2-config` 解析    | 见 ADR-0006           |
| SFTP      | `russh-sftp`                    | `ssh2::Sftp`        | 与 SSH crate 对齐       |
| 异步运行时     | `tokio`                         | -                   | -                    |
| 日志        | `tracing`                       | -                   | 结构化日志 + redaction    |
| PTY（本地）   | `portable-pty`                  | -                   | **Phase 5+ LocalProvider 才需要** |

### 3.4 参考项目（借鉴原理，不 fork 不拼代码）

| 项目                       | 借鉴点                                       | 借鉴方式       |
| ------------------------ | ---------------------------------------- | ---------- |
| `raychao-oao/pty-mcp`    | Session/PTY 抽象、ring buffer、wait_for、attach/detach、secret redaction | 读源码学原理，Rust 重写 |
| `classfang/ssh-mcp-server` | 多主机、SSH Config、SFTP、SOCKS/Bastion、Policy 接口 | 读源码学 SSH infra 设计 |
| `mingyang91/ssh-mcp`     | Rust + Session Manager + 端口转发结构          | 架构参考       |

---

## 4. 架构设计

### 4.1 模块化单体（不做微服务）

一个进程，一个二进制：`termbridge.exe`。内部分层模块化。**无网络端口、无数据库、无 Redis、无 Docker、无后台服务依赖。**

### 4.2 分层架构

```
                  ┌──────────────────┐
                  │   MCP Transport  │  (stdio only, MVP)
                  └────────┬─────────┘
                           │
                           ▼
                 ┌──────────────────┐
                 │   Application     │
                 │                  │
                 │ HostManager      │
                 │ SessionManager   │  (核心)
                 │ TransferManager  │
                 └────────┬─────────┘
                          │
                 interfaces / traits
                          │
             ┌────────────┴────────────┐
             ▼                         ▼
      ┌──────────────┐          ┌──────────────┐
      │ SSH Provider │          │ SFTP Provider│
      └──────┬───────┘          └──────┬───────┘
             │                         │
             └────────────┬────────────┘
                          ▼
                     SSH Connection
                          │
                 ┌────────┴────────┐
                 ▼                 ▼
              PTY shell          SFTP
                 │
                 ▼
              Session
```

**Domain 层只放**：ID / State / Value types / Policy trait / Provider trait。**不塞业务逻辑。**

**关键解耦**：Application 层定义 trait 接口，MCP transport 只是调用方之一。未来 GUI / CLI 可直接复用 Application 层。

### 4.3 核心抽象：Host ≠ Connection ≠ Session

#### Host（配置实体）—— "去哪台机器？"
```rust
struct Host {
    name: HostName,
    hostname: String,
    user: String,
    port: u16,
    identity_file: Option<PathBuf>,
    proxy_jump: Option<HostName>,
}
```

#### Connection（传输实体）—— "我和它建立了一条 SSH 连接"
```rust
struct Connection {
    id: ConnectionId,
    host: HostName,
    state: ConnectionState,  // CONNECTED / DISCONNECTED / RECONNECTING
    // SSH transport + auth + channels (shell / sftp / forward)
}
```
Connection 可断、可重连。**Channel 是 Infrastructure 层概念**（shell channel / sftp channel / forward channel），Application 层不感知。

#### Session（运行实体）—— "这个 Terminal 正在跑什么"

> ⚠️ v0.3 关键修正：Session **不永久绑定一个 ConnectionId**。SSH 断了重连可能换 Connection，Session 通过 attachment 间接持有当前 channel。

```rust
struct Session {
    id: SessionId,
    host: HostName,                  // Session 归属 Host，不归属 Connection
    state: SessionState,
    pty_size: PtySize,
    cwd: Option<String>,             // MVP: 始终 None（见 §5.7）
    output: OutputEngine,            // RingBuffer + Cursor + Waiter
    attachment: Option<Attachment>,  // 当前绑定的 Connection + channel（可空=DETACHED）
}

struct Attachment {
    connection_id: ConnectionId,
    channel: ChannelHandle,          // Infra 层 handle
}
```

**关系图**：
```
Host
  │
  ├── Connection A
  │     ├── shell channel ── Session 1
  │     ├── shell channel ── Session 2
  │     └── sftp channel
  │
  └── Connection B（重连后）
        └── shell channel ── Session 1（reattached）
```

**关键意义**：
- Connection 断掉 ≠ Session 消失
- Session 可跨 Connection reattach（Phase 4 自动重连；Phase 3 跨 MCP 重启）
- SFTP 与 PTY 是同一 Connection 上的不同 channel，独立但复用传输

### 4.4 TerminalProvider：Provider / Handle 两层

> ⚠️ v0.3 修正：v0.2 的 trait 太像 Session 本身，职责重复。拆成 Provider（创建 backend）+ Handle（read/write/resize/close）。

```rust
#[async_trait]
trait TerminalProvider {
    /// 创建一个 Terminal Backend，返回 Handle
    async fn open(
        &self,
        request: OpenTerminalRequest,
    ) -> Result<Box<dyn TerminalHandle>>;
}

#[async_trait]
trait TerminalHandle {
    async fn read(&self) -> Result<Bytes>;
    async fn write(&self, data: Bytes) -> Result<()>;
    async fn send_control(&self, c: ControlKey) -> Result<()>;
    async fn resize(&self, size: PtySize) -> Result<()>;
    async fn close(&self) -> Result<()>;
}
```

```
TerminalProvider
        │
        ├── SshProvider    → SshTerminalHandle    (russh + RequestPty)
        ├── LocalProvider  → LocalTerminalHandle  (portable-pty, Phase 5+)
        └── DockerProvider → DockerTerminalHandle (Phase 5+)
```

**MVP 只实现 `SshProvider`。** SessionManager 持有 `Box<dyn TerminalProvider>`，不关心是 SSH 还是 Local。

### 4.5 远端零安装策略

- **核心路径**：纯 SSH + SFTP，远端只要有标准 shell 即可工作（R2 满足）。
- **持久化路径**：仅"跨 MCP 重启保活"时部署轻量远端 daemon（单二进制）。默认不启用。

### 4.6 Session / Output Semantics（v0.3 核心——行为契约）

> 这 12 条是 TermBridge 的行为契约，**比 Policy/Workspace/SSE/100 sessions 都重要**。任何实现都必须满足。

1. **Session 是唯一 terminal state owner。** Connection、MCP、Agent 都不是。
2. **PTY output 永久进入 bounded buffer。** RingBuffer 满则丢弃最旧数据，绝不无限增长。同时 tee 到滚动日志文件，溢出数据仍有兜底（§5.2）。
3. **`read_output` 默认读取 unread output（drain 语义），推进内部 mark cursor。** 默认走 settle 检测，非"有数据即返回"（§5.3）。
4. **`tail_lines` 不推进任何 cursor，只看历史尾部。** 用于"看一眼最近发生了什么"。
5. **`wait_for` 等待未来 output，同时检查已有 unread output。** 不是"在 buffer 里搜索一遍"。**命中才推进 mark cursor，超时不推进**（留作后续 read_output 的未读数据）。正则编译失败回退纯文本 `contains` 匹配。
6. **`timeout` 只约束本次 `read_output` 调用，不代表 Session timeout。** Session 不会因 read timeout 而关闭。
7. **`send_input` 不等待命令完成。** 写入 PTY 立即返回。
8. **Ctrl+C 是 input/control，不是 exec。** 走 `send_control`，不影响 Session 生命周期。
9. **Session close 才会结束远端 shell。** `read_output` 超时、Connection 断开都不自动 close。
10. **Connection disconnect 不自动销毁 Session。** 普通 Session → LOST；Persistent Session → DETACHED（见 §5.6）。
11. **`since_cursor` 模式不推进内部 mark cursor，调用方自管 cursor，支持多 consumer 增量读。** 返回 cursor 之后的数据 + 新 cursor + `has_more` + `is_truncated`。
12. **RingBuffer 用"物理环形 buf[] + 逻辑单调 written 计数器"作绝对游标。** written 永不回绕，`IsTruncated(cursor)` 判断简单（§5.2）。

---

## 5. Session 内部设计（借鉴 pty-mcp）

### 5.1 PTY 链路

```
SSH Connection → SSH Channel → RequestPty() → shell → PTY I/O
```
**不是** `ssh exec "command"`。PTY 是默认 Terminal 模式。远端 PTY 由 SSH `RequestPty` 提供，**不需要本地 portable-pty**。

### 5.2 Output Engine：RingBuffer + 双游标 + Waiter + 日志 tee

> v0.4 明确化：借鉴 pty-mcp `internal/buffer/ring.go`——物理环形 buf[] + 逻辑单调 `written: u64` 计数器（永不回绕，作绝对游标）+ 双游标机制 + Notify 唤醒 + 日志 tee。

```rust
struct OutputRingBuffer {
    buf: Vec<u8>,               // 物理环形
    size: usize,                // 容量（默认 1MB，min 64KB，max 32MB）
    head: usize,                // 下一个写入位置（环形）
    written: u64,               // 单调递增总写入字节数（永不回绕，作绝对游标）
    notify: tokio::sync::Notify, // 新 output 唤醒（非阻塞，丢一次无所谓）
    // 由 Session 持有，不在 buffer 内：
    //   mark_cursor: u64  —— 内部消费游标（settle/wait_for 共享）
    //   调用方自管 since_cursor: u64（多 consumer 各自追踪）
}
```

```
┌──────────────────────────────────────────────┐
│ OutputEngine                                  │
│   PTY → MultiWriter ──┬──→ RingBuffer (bounded)│
│                       └──→ 滚动日志文件 (兜底)  │
│                                            │
│   RingBuffer 游标机制：                       │
│     ┌────────────┬─────────────┐            │
│     ▼            ▼             ▼            │
│  mark_cursor  since_cursor   tail (peek)    │
│  (内部消费)   (外部自管)      (不推进)        │
│   settle /    多 consumer                   │
│   wait_for    增量读                        │
│                                            │
│   Waiter: wait_for 正则 + Notify 唤醒        │
└──────────────────────────────────────────────┘
```

- **RingBuffer**：bounded，满则丢最旧（pty-mcp 已验证设计，防 `tail -f` 内存爆炸）。`written` 是单调计数器，物理环形只在 `buf[]` 上发生
- **双游标机制**（关键）：
  - `mark_cursor`：内部消费游标，settle/wait_for 共用。`Mark()` 跳到最新，`AdvanceMarkBy(n)` 精确前进 n
  - `since_cursor`：调用方自管，通过 `ReadSinceMax(cursor, max_bytes)` 读取，**不触碰 mark_cursor**，支持多 consumer
- **Notify**：`tokio::sync::Notify` 唤醒等待者。pty-mcp 用 buffered=1 channel 非阻塞 send，Rust 用 Notify 等价
- **日志 tee**：PTY output 同时写 RingBuffer + 滚动日志文件（按大小滚动）。buffer 溢出数据仍有日志兜底
- **Waiter**：`wait_for` 注册的正则 + timeout，PTY 有新 output 时 Notify 唤醒匹配

### 5.3 `read_output` 语义（v0.4 三模式 + settle）

> v0.4 修正：加 `since_cursor` 参数（多 consumer 增量读）；drain 模式从"有数据即返回"改为 settle 语义（借鉴 pty-mcp `WaitForSettle`）。

```json
{
  "session_id": "sess_123",
  "wait_for": "Server started",   // optional, 正则
  "timeout": 5,                   // optional, 默认 5s, 上限 60s
  "tail_lines": 20,               // optional, 不推进任何 cursor, 上限 100
  "since_cursor": 1234,           // optional, 增量读游标, 不推进 mark_cursor
  "max_bytes": 65536,             // optional, since_cursor 模式单次上限
  "context_lines": 5              // optional, wait_for 命中行前后上下文, 上限 50
}
```

**三种模式**（互斥优先级：since_cursor > wait_for > 默认 settle）：

```
read_output()
    ↓
if since_cursor 指定 (增量读模式):
    return ReadSinceMax(cursor, max_bytes)
       → { output, new_cursor, has_more, is_truncated }
    // 不推进 mark_cursor，调用方自管 cursor
    ↓
if tail_lines 指定 (peek 模式):
    return RingBuffer.tail(tail_lines)   // 不动任何 cursor
    ↓
if wait_for 指定 (阻塞匹配模式):
    1. 先扫 unread output（mark_cursor → written）
       - 正则编译失败 → 回退纯文本 contains
    2. 命中 → Mark() 推进到最新 → return (match_line + context_lines)
    3. 未命中 → Notify 阻塞等新 output → append 后重新扫
    4. timeout 到 → 返回 tail_lines（不推进 mark_cursor，留作后续未读）
    ↓
if 都未指定 (默认 settle 模式):
    WaitForSettle(getOutput=Since(mark_cursor), settle=300ms, timeout):
      - 50ms 轮询 Since(mark_cursor)
      - 输出变化 → 更新 lastChange
      - 输出稳定 ≥ 300ms 且 hasOutput → 返回，AdvanceMarkBy(len)
      - 检测到 prompt → 立即返回
      - 空输出永不 settled（避免命令还没产出就提前返回）
      - timeout → 返回当前 output，AdvanceMarkBy(len)
```

**关键**：
- `wait_for` 不是"在已有 buffer 搜索"，而是"先扫已有 unread，未命中才等未来 output"。命中推进 mark，超时不推进
- 默认 settle 模式不是"有数据即返回"，而是等输出稳定 300ms 或检测到 prompt——避免碎片化返回
- `since_cursor` 是唯一支持多 consumer 的路径，mark_cursor 路径假定单 consumer（MCP 串行调用）

### 5.4 Session 状态机

```
正常：CREATING → CONNECTING → READY → RUNNING → IDLE → CLOSING → CLOSED

异常：READY/RUNNING → DISCONNECTED → RECONNECTING → READY
                                           ↓ (失败)
                                         LOST

Persistent：DETACHED（独立状态，Connection 可断）
```

Connection 与 Session 状态分离：
- SSH 断：Connection=DISCONNECTED，Session=LOST（普通）或 DETACHED（persistent）
- 重连后：Connection=CONNECTED，Session=RUNNING（reattached）

### 5.5 Secret 隔离（Security baseline，Phase 1 就做）

**密码绝不进 LLM context。** 借鉴 pty-mcp `send_secret` 思路：

```
Agent: read_output(wait_for="Password:")
        ↓
TermBridge 检测 password prompt
        ↓
Human-in-the-loop UI: "请输入服务器密码"  (Phase 6)
        ↓ MVP 阶段：直接报错要求用户在 ssh-agent / IdentityFile 配置好
密码直接写入 PTY（不经过 Agent）
        ↓
Agent 只得到：success
```

凭据来源优先级：
1. **SSH Agent**（首选）
2. **IdentityFile**
3. **interactive secret**（HITL，Phase 6）

**不**把 password/privateKey/passphrase 放 MCP 配置（与 classfang 的关键安全差异——classfang 把密码放 mcp.json args 暴露在进程列表）。

日志侧继承 pty-mcp `audit/redact.go` 的三类脱敏正则，**Phase 1 就接入**：

```rust
// 1. key=value / key: value 凭证（行尾脱敏）
r"(?i)((?:password|passwd|secret|token|api[_-]?key|access[_-]?key|auth[_-]?token)\s*[=:]\s*)[^\n]+"
    => "${1}[REDACTED]"

// 2. HTTP Authorization header
r"(?i)(Authorization:\s*(?:Bearer|Basic|Token)\s+)\S+"
    => "${1}[REDACTED]"

// 3. PEM 私钥块
r"-----BEGIN (?:[A-Z ]+ )?PRIVATE KEY-----[\s\S]*?-----END (?:[A-Z ]+ )?PRIVATE KEY-----"
    => "[PRIVATE KEY REDACTED]"
```

审计 snippet 截前 2048 字节再脱敏（pty-mcp 设计）。`send_secret` / `prepare_secret` 永不入审计日志。

### 5.6 Interactive Session vs Persistent Session（v0.3 概念澄清）

> ⚠️ v0.2 把"持久 PTY"和"跨 MCP 重启持久化"都叫 persistent，混淆。v0.3 明确区分。

#### Level 1：Interactive Session（MVP 默认）
```
MCP Server
    ↓ SSH
    PTY
    ↓
    bash
```
只要 MCP Server 活着，Session 就在。**这已经是 persistent shell session。**

#### Level 2：Persistent Session（Phase 3 opt-in）
```
MCP Server
    ↓ SSH
    remote daemon / tmux
    ↓
    PTY
```
即使 MCP Server 重启、SSH 断开，远端任务依旧存在。这是 **persistent across connection/server lifecycle**。

文档统一术语：Interactive Session / Persistent Session，不再混用 persistent。

### 5.7 cwd（MVP 不跟踪）

> v0.3 修正：v0.2 写"Session.cwd 跟踪"，但 `cd / su - sudo -i source xxx alias` 都会让 cwd 推断很麻烦，不要尝试解析 PS1。

**MVP：`Session.cwd` 始终 None。** Terminal 是真实 shell，cwd 是 shell 自己的状态。Agent 用 `pwd` 查询。等 Phase 5 Workspace 再做 cwd abstraction。

### 5.8 Input 模型（内部区分，MCP 层保持简单）

> v0.4 简化：借鉴 pty-mcp `aitx/server.go` 的统一 map——MVP 只做 ControlKey（Ctrl+C/D/Z + Tab/Enter/Escape），Key（方向键/Home/End）后置到 Phase 5 GUI。

```rust
enum InputAction {
    Bytes(Vec<u8>),         // send_input
    Control(ControlKey),    // send_control: MVP 支持 Ctrl+C/D/Z + Tab/Enter/Escape
    Key(Key),               // Phase 5+ GUI: ArrowUp/Down, Home/End
}

// pty-mcp 风格的统一控制键映射（MVP 子集）
const CONTROL_KEYS: &[(&str, &[u8])] = &[
    ("ctrl+c", b"\x03"), ("ctrl+d", b"\x04"), ("ctrl+z", b"\x1a"),
    ("tab", b"\t"), ("enter", b"\r"), ("escape", b"\x1b"),
];
```

MCP 层只暴露 `send_input` / `send_control` 两个工具。`Key` 留给 Phase 5+ GUI，MVP 不暴露。`send_input(raw=true)` 走 `WriteRaw`（不追加 `\r`），用于交互式菜单单字符输入。

---

## 6. MCP 工具接口（MVP 6 个）

> v0.3 修正：v0.2 列 8 个工具含 `connect` 和 `sftp_transfer`。`connect` 对 Agent 繁琐——`open_session(host)` 内部自动 `get_or_connect`。SFTP 移到 Phase 1 但工具单独列。MVP（Phase 1）压到 **6 个核心 + 1 个 SFTP**。

### Phase 1 工具（7 个）

| 工具              | 说明                                       |
| --------------- | ---------------------------------------- |
| `list_hosts`    | 列出 ssh config 发现的主机                       |
| `open_session`  | **`open_session(host)` 自动 get_or_connect**，返回 session_id |
| `send_input`    | 向会话输入                                    |
| `read_output`   | 读取会话输出，支持 `wait_for` + `tail_lines` + `timeout`（见 §5.3） |
| `send_control`  | 发送控制字符（Ctrl+C / Ctrl+D / Tab 等）          |
| `close_session` | 关闭会话                                     |
| `sftp_transfer` | upload / download（方向参数化）                |

`connect` **不作为 MCP Tool**，仅作 Application 层 `ConnectionManager.get_or_connect(host)` 内部方法。这更符合"Agent 面向 Terminal，不面向 SSH"。

> Phase 3 引入 `attach_session` / `detach_session` / `list_remote_sessions`。Policy/HITL 不暴露为工具，由内部拦截。

### 6.1 工具错误格式（v0.4 统一）

> v0.4 新增：借鉴 classfang `utils/tool-error.ts`——结构化错误对 Agent 重试逻辑友好。

所有工具失败统一返回 `{code, message, retriable}` JSON + `isError: true`：

```rust
struct ToolError {
    code: String,       // 稳定错误码，如 "AUTH_FAILED" / "SESSION_NOT_FOUND" / "SFTP_ERROR" / "OPERATION_TIMEOUT"
    message: String,    // 人类可读描述
    retriable: bool,    // Agent 是否应重试（如网络超时=true，认证失败=false）
}
```

错误码枚举（初版）：
- `AUTH_FAILED`（retriable=false）—— SSH 认证失败
- `SESSION_NOT_FOUND`（retriable=false）—— session_id 不存在
- `SESSION_CLOSED`（retriable=false）—— session 已关闭
- `CONNECT_FAILED`（retriable=true）—— TCP/SSH 连接失败
- `OPERATION_TIMEOUT`（retriable=true）—— 命令/传输超时
- `SFTP_ERROR`（retriable=true）—— SFTP 操作失败
- `LOCAL_PATH_NOT_ALLOWED`（retriable=false）—— 路径策略拒绝
- `REMOTE_PATH_NOT_ALLOWED`（retriable=false）—— 远端路径策略拒绝
- `HOST_KEY_REJECTED`（retriable=false）—— host key 校验失败

成功返回工具特定结构（如 `read_output` 返回 `{output, cursor, is_truncated, ...}`）。

---

## 7. 项目结构（Rust）

```
termbridge/
├── Cargo.toml
├── src/
│   ├── main.rs
│   ├── lib.rs
│   ├── domain/
│   │   ├── host.rs
│   │   ├── connection.rs
│   │   ├── session.rs          # Session + 状态机
│   │   ├── output.rs           # RingBuffer + Cursor + Waiter
│   │   ├── policy.rs           # Policy trait + Decision
│   │   └── provider.rs         # TerminalProvider + TerminalHandle trait
│   ├── application/
│   │   ├── hosts.rs            # HostManager
│   │   ├── connections.rs      # ConnectionManager (get_or_connect)
│   │   ├── sessions.rs         # SessionManager (核心)
│   │   ├── transfer.rs         # TransferManager (SFTP)
│   │   └── policy.rs           # PolicyManager + DefaultPolicy
│   ├── infrastructure/
│   │   ├── ssh/                # russh 封装 + SshProvider + SshTerminalHandle
│   │   ├── sftp/               # russh-sftp 封装
│   │   ├── sshconfig/          # ssh -G 子进程 or ssh2-config
│   │   ├── security/           # host key verification + log redaction
│   │   └── persistence/        # opt-in 远端 daemon 客户端 (Phase 3)
│   └── transport/
│       └── mcp/
│           ├── server.rs       # rmcp server 入口 (stdio only)
│           ├── tools_session.rs
│           └── tools_transfer.rs
├── docs/
│   ├── PLAN.md
│   └── adr/
├── tests/                      # 集成测试（真实 SSH Docker 容器）
└── README.md
```

**不做**：`pkg/` `services/` `repositories/` `factories/` `adapters/` 全套 DDD。

---

## 8. Policy 接口（简单，但留接口）

第一版简单分类，**不做企业级 RBAC/ABAC/OPA**：

```rust
trait Policy {
    fn authorize(&self, action: &Action) -> Decision;
}

enum Decision { Allow, Confirm, Deny }
```

第一版实现 `DefaultPolicy`（Phase 2），未来加 `RulePolicy`。Policy 在 Application 层拦截，不侵入 SSH 层。

> v0.3 调整：**Security baseline 与 Policy 分开**。Security baseline（host key verification / credential isolation / log redaction）Phase 1 必做；Policy（dangerous command confirmation / blocklist）Phase 2。

---

## 9. 分阶段实施

### Phase 0：调研评估 + Vertical Slice

> v0.3 重构：拆成 0-A / 0-B / 0-C，结束时拥有一个**真实工作的 vertical slice**。

#### Phase 0-A：技术验证
- [ ] 验证 `rmcp`：跑通最小 stdio echo 工具
- [ ] 验证 `russh`：PTY request / shell / write / read / Ctrl+C / resize / EOF / disconnect
- [ ] 验证 OpenSSH config 策略：`ssh -G <host>` 输出格式与可用性（ADR-0006）
- [ ] 验证 `russh-sftp`：基本 upload/download
- [ ] 原型：Windows → Ubuntu SSH PTY，能 `ls` 读输出、Ctrl+C

#### Phase 0-B：Session 原型（先不接 MCP）
独立实现并单测：
- [ ] `OutputRingBuffer`（边界、溢出、tail_lines、drain 语义）
- [ ] `ReadCursor`（推进、不推进语义）
- [ ] `Waiter`（wait_for 等待未来 output + 检查已有 + timeout）
- [ ] `SessionState` 状态机迁移
- [ ] fake PTY output → buffer → read → wait_for 测试通过

**目的**：把最核心的并发问题提前解决，不依赖 SSH/MCP。

#### Phase 0-C：MCP Vertical Slice
- [ ] `rmcp` → `SessionManager` 接通
- [ ] Agent: `open_session(host)` → `send_input("ls\n")` → `read_output` → `send_control(Ctrl+C)` → `close_session`
- [ ] **输出 ADR-0001**：构建策略 + 核心 crate 选型
- [ ] **输出 ADR-0002**：MCP transport = stdio only
- [ ] **输出 ADR-0006**：OpenSSH config 兼容策略（`ssh -G` vs crate）

**Phase 0 验证标准**：Rust vertical slice 在 Windows 上连一台 Linux，6 个工具全跑通，行为符合 §4.6 契约。

### Phase 1：MVP 核心

**7 个工具**：`list_hosts` / `open_session` / `send_input` / `read_output` / `send_control` / `close_session` / `sftp_transfer`。

> v0.3 修正：v0.2 把 SFTP 完全排除出 Phase 1，但 R2 说"核心 SSH+SFTP 零安装"，逻辑冲突。Phase 1 纳入 SFTP（**只 upload/download**，不做 mkdir/rename/chmod/recursive sync/watch）。

- [ ] HostManager：ssh config 解析（`ssh -G`）+ `list_hosts` + `HashMap<HostName, Host>` + default host 概念
- [ ] ConnectionManager：`get_or_connect`、连接复用
- [ ] SessionManager：PTY 会话 + OutputEngine（§5.2 双游标 + Notify + 日志 tee）+ 状态机（§4.6 契约 12 条）
- [ ] SFTP：`sftp_transfer`（upload/download only）+ **路径策略**（allowedLocalPaths 默认 cwd + allowedRemotePaths + realpath 防穿越 + 未配置启动告警）+ **下载原子写**（temp + rename + 失败清理）
- [ ] **Security baseline**：
  - [ ] **known_hosts 校验**（classfang 的前车之鉴——绝不 accept_any_host_key）—— 解析 `ssh -G` 的 `userknownhostsfile` + `stricthostkeychecking`，russh `check_server_key` 严格校验
  - [ ] SSH Agent / IdentityFile 认证（密码仅 HITL，Phase 6）
  - [ ] log redaction（§5.5 三类正则，Phase 1 接入）
  - [ ] **凭据不进 MCP 配置 args**（避免进程列表暴露，与 classfang 关键差异）
- [ ] **SSH keepalive**（10s 间隔 + 3 次上限，借鉴 classfang 默认值）
- [ ] **idleReaper**（30s tick，收集超时 session 后**先释放锁再 Close**避免死锁，借鉴 pty-mcp `session.go`；idle timeout 1800s）
- [ ] **超时即 invalidateConnection 重连**（借鉴 classfang commit 14484ed——解决半开 socket 问题）
- [ ] 多会话并发（DashMap 或 actor 模型，不抄 classfang 单例 + 可变 Map）
- [ ] 基础日志（连接/断开/重连，含 redaction，走 stderr 不污染 stdio MCP）
- [ ] **输出 ADR-0003**：Output 缓冲策略（ring buffer 容量、双游标语义）
- [ ] **输出 ADR-0005**：安全模型（凭据存储、secret 隔离、host key、log redaction、路径策略）

**验证标准**：Agent 连多台主机，跑长任务（`python -m http.server`），`read_output(wait_for="...")`，Ctrl+C 中断后重启。Windows 项目 upload → 远端测试 → download 日志。host key 校验生效（未知主机拒绝连接）。

> **Phase 1 不碰**：GUI、数据库、Workspace、Policy、ProxyJump、ai-tmux、Persistent Session。

### Phase 2：ProxyJump + SFTP polish + Policy

- [ ] ProxyJump / Bastion / SOCKS 支持
- [ ] SFTP 增强：mkdir、目录递归、权限
- [ ] known_hosts 完整处理
- [ ] Policy 接口 + DefaultPolicy（命令 blocklist / dangerous command confirm）

### Phase 3：Persistent Session（opt-in）

- [ ] 远端 persistent daemon（参考 ai-tmux 原理，Rust 重写）
- [ ] `attach_session` / `detach_session` / `list_remote_sessions`
- [ ] 跨 MCP 重启会话保活验证
- [ ] **输出 ADR-0004**：持久化协议与远端 daemon 形态

### Phase 4：可靠性

- [ ] Connection pool / 自动重连（Session 跨 Connection reattach）
- [ ] timeout / backpressure / heartbeat
- [ ] output buffer 优化与 session cleanup

### Phase 5：高级 SSH + 扩展

- [ ] MFA / SSH Agent forwarding
- [ ] 端口转发（`ssh_forward`）
- [ ] Workspace 抽象（Host + RemotePath + Session 编组 + cwd abstraction）
- [ ] TerminalProvider 扩展：Local（用 `portable-pty`）/ WSL / Docker / Serial
- [ ] 审计日志 / 会话回放
- [ ] 配置热更新

### Phase 6：Human-in-the-loop

- [ ] password prompt 检测 + HITL UI（secret 不进 LLM）
- [ ] `send_secret` 机制
- [ ] dangerous command 确认流
- [ ] 完整 audit log

---

## 10. 测试策略

最容易出 bug 的不是 MCP，而是**长连接 + PTY + 并发 + 断线**。

### Unit
- `OutputRingBuffer`（边界、溢出、tail_lines、drain vs peek、`written` 单调性、`IsTruncated`）
- **双游标语义**（§4.6 契约 3/4/5/11/12）：
  - `mark_cursor`：settle 推进、wait_for 命中推进、wait_for 超时不推进、tail_lines 不推进
  - `since_cursor`：不推进 mark_cursor、多 consumer 各自追踪、`has_more`/`is_truncated` 正确
- **Waiter**（wait_for 先扫已有 + 等未来 + 正则编译失败回退 contains + timeout，§5.3）
- **settle 检测**（50ms 轮询、300ms 阈值、空输出永不 settled、prompt 检测立即返回，§5.3）
- `SessionState` 状态机迁移（含 DETACHED）
- `HostResolver`（ssh -G 解析 + 多 identityfile）
- **log redaction**（§5.5 三类正则：key=value 凭证、Authorization、PEM）
- **ToolError**（§6.1：code/retriable 字段、isError=true）
- **路径策略**（allowedLocalPaths/allowedRemotePaths、realpath 防穿越、null 字节拒绝）

### Integration（真实 SSH）
Docker 容器跑 Linux sshd，TermBridge 连接测试：
- connect / PTY / Ctrl+C / Ctrl+D / Tab
- long-running / large output / unicode
- resize / disconnect / reconnect
- upload / download

### Stress（真实场景，非为完整而完整）
> v0.3 修正：删掉"1GB output / 100 sessions"（不是真实瓶颈）。真实场景是 3~10 Linux × 1~5 sessions。

- `tail -f` 持续输出（10MB/s × 10min）
- 大量日志滚动
- Unicode / ANSI escape
- Ctrl+C 中断长任务
- 断网恢复
- SSH server kill -9 后重连
- **MCP restart 后 Session 状态正确**（Interactive→LOST；Persistent→DETACHED）
- **重点**：Agent 反复 `read_output()` 不造成内存增长（RingBuffer 验证）

---

## 11. Out of Scope（明确不做，防止范围蔓延）

```
❌ Server monitoring dashboard / CPU-memory dashboard
❌ Docker manager / Kubernetes manager
❌ Deployment platform / CI-CD
❌ Server inventory SaaS / Web management panel
❌ User/RBAC/ABAC/OPA/JWT 系统
❌ Cloud server API
❌ Remote Agent（远端绝不装 Agent，除 opt-in persistent daemon）
❌ MCP SSE / HTTP transport（MVP stdio only，未来按需）
❌ 100 sessions / 1GB output 压测目标（非真实场景）
```

这些会把项目从"Remote Terminal for Agents"带偏成"AI Server Management Platform"。

---

## 12. 风险与权衡

| 风险                          | 影响  | 缓解                       |
| --------------------------- | --- | ------------------------ |
| `rmcp` 0.x API 变动           | 中   | Phase 0-A 验证；必要时自研薄协议层   |
| `russh` ProxyJump/MFA 成熟度   | 中   | MVP 不涉及；Phase 2/5 验证，按需切 `ssh2` |
| SSH Config Match/Include 不完整 | 中   | **优先 `ssh -G` 复用 OpenSSH**（ADR-0006） |
| Windows ConPTY 与 SSH 桥接     | 低   | MVP 远端 PTY 走 SSH RequestPty，不需本地 PTY |
| tmux 持久化与 R2 冲突              | 中   | opt-in，默认不启用             |
| Output 语义实现复杂               | 高   | Phase 0-B 独立原型 + 单测，先不接 MCP/SSH |
| 工具数量膨胀                      | 中   | MVP 压到 6+1，能力参数化而非拆工具    |
| 远端零安装边界（如需 jq/python）       | 低   | 仅依赖 POSIX shell，扩展能力按需降级 |

---

## 13. 待决策项（→ ADR）

- **ADR-0001**：构建策略 + 核心 crate 选型 — Phase 0-C 输出（Phase 0-A 已验证 rmcp 3.1.2 / russh 0.62 ring / russh-sftp 2.4.0 / ssh -G 全通过）
- **ADR-0002**：MCP transport = **stdio only**（MVP）— Phase 0-C 输出（Phase 0-A 已验证 rmcp stdio）
- **ADR-0003**：Output 缓冲策略（ring buffer 容量默认 1MB/max 32MB、双游标 mark_cursor/since_cursor 语义、settle 阈值 300ms、日志 tee）— Phase 1 输出
- **ADR-0004**：持久化协议与远端 daemon 形态 — Phase 3 输出
- **ADR-0005**：安全模型（凭据存储、secret 隔离、known_hosts 校验、log redaction 三类正则、SFTP 路径策略、下载原子写）— Phase 1 输出
- **ADR-0006**：OpenSSH config 兼容策略（`ssh -G` vs `ssh2-config` crate）— Phase 0-C 输出（Phase 0-A 已验证 `ssh -G` 74 字段解析可行）

---

## 14. 下一步

1. 用户审阅 v0.4，确认 §4.6 契约 12 条 + §5.2 OutputEngine 结构 + §5.3 三模式 + §6.1 ToolError + Phase 1 安全清单。
2. ✅ Phase 0-A 已完成（见 [docs/phase-0a-report.md](file:///e:/Code/TermBridge/docs/phase-0a-report.md)）。
3. 进入 Phase 0-B：OutputEngine 原型 + 单测（不接 MCP/SSH）—— 实现 RingBuffer + 双游标 + Waiter + settle，用 fake PTY output 验证 §4.6 契约 12 条。
4. Phase 0-C：vertical slice + ADR-0001/0002/0006。
5. ADR 落定后启动 Phase 1 MVP。
