# 参考项目源码研读对照

> 日期：2026-08-09 · 状态：对照 v0.3 PLAN，列出影响设计的借鉴点
>
> 研读方式：WebFetch 直接读 GitHub raw 文件，不克隆全仓。
> - pty-mcp：14 个 Go 文件 + README
> - classfang/ssh-mcp-server：全部 src/**/*.ts（connection-manager 1853 行通过单文件下载补全）

---

## 一、pty-mcp 借鉴点（TermBridge 用 Rust 重写）

### ✅ 强烈借鉴（影响 v0.3 设计）

| # | pty-mcp 设计 | 源码位置 | 对 v0.3 PLAN 的影响 |
|---|---|---|---|
| 1 | **RingBuffer + 单调 `written` 计数器作绝对游标** | `internal/buffer/ring.go` | §5.2 OutputEngine 已写"RingBuffer + Cursor"，但 pty-mcp 的 `written: int64` 单调计数器（非环形回绕）是关键——游标不会回绕，`IsTruncated(cursor)` 判断简单。**v0.3 §5.2 应明确：物理环形 buf[] + 逻辑单调 written 计数器。** |
| 2 | **双游标机制** | `ring.go` `markSnapshot` + `since_cursor` | v0.3 §4.6 契约 3/4 只提了"read cursor"和"tail_lines 不推进"。pty-mcp 证明需要**两条路径**：(a) 内部 markSnapshot（settle/wait_for 共享消费）；(b) 外部 since_cursor（调用方自管，支持多 consumer）。**v0.3 应在 §5.3 read_output 加 `since_cursor` 可选参数，支持增量读。** |
| 3 | **wait_for = 先扫已有 buffer + 再阻塞等未来** | `tools.go` `waitForPattern` L444-516 | v0.3 §5.3 已写"先扫 unread，未命中才等未来"。pty-mcp 确认了这个顺序正确，且**正则编译失败应回退纯文本 contains**（容错）。**v0.3 §5.3 加一条：正则编译失败回退 contains。** |
| 4 | **wait_for 命中才推进 mark，超时不消费** | `waitForPattern` | v0.3 §4.6 契约 5 只说"等待未来 output"。pty-mcp 明确：命中→Mark()推进；超时→不推进，留作后续 read_output 的未读数据。**v0.3 §4.6 契约 5 补充："命中推进 cursor，超时不推进"。** |
| 5 | **WaitForSettle 的 settle 检测** | `pty/helper.go` | v0.3 §5.3 drain 模式"有数据即返回"太粗。pty-mcp 用 50ms 轮询 + 300ms settle 阈值 + **空输出永不 settled** + prompt 检测立即返回。**v0.3 §5.3 drain 模式应改为 settle 语义，而非"有数据即返回"。** |
| 6 | **read goroutine + MultiWriter tee 到日志** | `aitx/ptysession.go` | v0.3 §5.2 只画了 PTY→Writer→RingBuffer。pty-mcp 用 `io.MultiWriter(rb, logFile)` tee 到滚动日志文件，buffer 溢出数据仍有日志兜底。**v0.3 §5.2 加：PTY output 同时 tee 到 bounded RingBuffer + 滚动日志文件。** |
| 7 | **控制键统一 map** | `aitx/server.go` | v0.3 §5.8 `InputAction::Control(ControlKey)` + `Key(Key)` 分两类。pty-mcp 用单 map（ctrl 系列单字节 + 方向键 VT 转义序列），简单够用。**v0.3 §5.8 可简化：MVP 只做 ControlKey（Ctrl+C/D/Z + Tab/Enter/Escape），Key（方向键）后置。** |
| 8 | **send_secret GUI 弹框 + 明文不返回 LLM** | `tools.go` + README | v0.3 §5.5 已写"密码绝不进 LLM context"。pty-mcp 用平台原生对话框（osascript/zenity/powershell）。**v0.3 §5.5 明确：Phase 6 用 `rfd` crate 或平台命令弹框；返回 `{success, length}` 而非明文。** |
| 9 | **审计脱敏三类正则** | `audit/redact.go` | v0.3 §5.5 只说"PEM/Authorization/password 清洗"。pty-mcp 的正则可直接复用：key=value 凭证、Authorization header、PEM 块。**v0.3 §5.5 引用 pty-mcp redact.go 的正则模式。** |
| 10 | **idleReaper 先收集再释放锁后 Close** | `session.go` | v0.3 未提 session 清理。pty-mcp 30s tick + 先收集超时 session + 释放锁后逐个 Close（避免死锁）。**v0.3 §5 加一条：idleReaper 机制，避免锁内 Close 死锁。** |

### ⚠️ 选择性借鉴

| # | pty-mcp 设计 | 决策 |
|---|---|---|
| A | **ai-tmux daemon 持久会话** | Phase 3 opt-in 保留。但 pty-mcp 用同步 `mu` 锁 RPC，TermBridge 用 async（tokio）支持并发读。 |
| B | **classifier 状态推断**（6 种状态正则分类） | v0.3 §5.4 状态机是显式的。pty-mcp 用只读推断（at_prompt/password_prompt/confirmation/pager/running）。**TermBridge 可在 read goroutine 实时分类缓存，但不作为状态机唯一来源。** |
| C | **send_input 内置 wait_for** | pty-mcp 把"发命令+等模式"合并成一次调用减少 round-trip。v0.3 §6 保持 `send_input` 和 `read_output(wait_for)` 分离。**不借鉴——分离更清晰，靠 LLM 编排。** |
| D | **cachedOut 优化**（send_input 响应缓存给后续 read） | RemoteSession 专用。Phase 3 persistent session 可考虑。**MVP 不需要。** |

### ❌ 不借鉴

| # | pty-mcp 设计 | 原因 |
|---|---|---|
| α | 手写 MCP JSON-RPC | TermBridge 用 rmcp 官方 SDK |
| β | cred-mcp HPKE 集成 | 复杂度高，场景窄，send_secret 已覆盖 |
| γ | 单 markSnapshot 共享游标（并发不安全） | TermBridge 支持 since_cursor 路径，多 consumer 各自管 cursor |
| δ | DSR 拦截 hack（PSReadLine `ESC[6n`） | pwsh 特定兼容，MVP 不强求 pwsh 支持 |
| ε | mapstructure WeaklyTypedInput | Rust 用 serde，不需此 workaround |

### 关键数据参考

| 参数 | pty-mcp 值 | TermBridge 建议初值 |
|------|-----------|-------------------|
| RingBuffer 默认大小 | 1MB | 1MB |
| RingBuffer 上限 | 32MB | 32MB |
| maxSessions | 50 | 50 |
| idle timeout | 1800s | 1800s |
| ReadScreen 默认 timeout | 5000ms | 5000ms |
| settle 阈值 | 300ms | 300ms |
| settle 轮询间隔 | 50ms | 50ms |
| read_output timeout 上限 | 600s | 60s（Agent 场景不需要 10 分钟） |
| tail_lines 上限 | 100 | 100 |
| context_lines 上限 | 50 | 50 |
| PTY 初始尺寸 | 40×120 | 24×80（更常规） |

---

## 二、classfang/ssh-mcp-server 借鉴点

### ✅ 强烈借鉴

| # | classfang 设计 | 源码位置 | 对 v0.3 PLAN 的影响 |
|---|---|---|---|
| 1 | **多主机配置模型 `Record<name, SSHConfig>` + 按 name 索引 + connectionName 缺省回退 default** | `models/types.ts` + `ssh-connection-manager.ts:147-153` | v0.3 §4.3 Host 是单实体。**应补：HostManager 内部 `HashMap<HostName, Host>` + 当前 default host 概念 + `list_hosts` 返回所有。** |
| 2 | **结构化 ToolError(code, message, retriable)** | `utils/tool-error.ts` | v0.3 §6 工具返回值未定义错误格式。**应补：工具错误统一 `{code, message, retriable}` JSON + `isError: true`，对 Agent 重试友好。** |
| 3 | **路径策略 allowedLocalPaths/allowedRemotePaths + realpath 防穿越 + 未配置启动告警** | `ssh-connection-manager.ts:324-433` | v0.3 未提 SFTP 路径安全。**Phase 1 SFTP 应加：allowedLocalPaths（默认 cwd）+ allowedRemotePaths + realpath 校验。** |
| 4 | **下载原子写（temp 文件 + rename + 失败清理）** | `ssh-connection-manager.ts:577-591` | v0.3 SFTP 未提原子性。**Phase 1 download 应加：先写 .tmp 再 rename。** |
| 5 | **超时全链路覆盖 + 超时即 invalidateConnection 重连** | `ssh-connection-manager.ts:1179-1202` | v0.3 §9 Phase 4 "自动重连"提到。classfang 的"半开 socket"问题（commit 14484ed）值得直接吸收——**超时即销毁 stale 连接，下次自动重连。** |
| 6 | **SSH keepalive 默认值（10s 间隔 / 3 次上限）** | `buildClientConfig` | v0.3 未提 keepalive。**Phase 1 应加：keepalive 10s + 3 次上限。** |
| 7 | **logger 走 stderr**（MCP stdio transport 必备） | `utils/logger.ts` | v0.3 §3.3 tracing 已用 stderr，确认正确。 |
| 8 | **2FA tryKeyboard 有序 auth method + maxAuthAttempts + 密码 prompt 识别** | `buildClientConfig:882-999` | v0.3 §9 Phase 5 MFA。**借鉴 auth method 顺序 + 防死循环。** |

### ⚠️ 选择性借鉴

| # | classfang 设计 | 决策 |
|---|---|---|
| A | **shell 模式 marker 框架**（begin/end marker + exit code + ANSI 剥离 + 串行队列） | Phase 2 bastion/网络设备时借鉴。MVP 不需要。 |
| B | **commandTemplate + shellQuote**（su/docker/jumphost 包裹） | Phase 2 借鉴。 |
| C | **list-servers 工具 + 预连接 connectAll** | v0.3 §6 已有 `list_hosts`。预连接不借鉴——按需连接更省资源。 |
| D | **status-collector 每次连接后跑系统命令** | 不借鉴——耦合 Linux + 加延迟。改为 opt-in/lazy。 |

### ❌ 不借鉴 / 必须改进

| # | classfang 设计 | 原因 / TermBridge 改进 |
|---|---|---|
| α | **Host key 校验缺失** | **重大安全缺口**。TermBridge Phase 1 必须做 known_hosts 校验。 |
| β | **SSH config 只解析 4 字段，无 ProxyJump/Match** | TermBridge 用 `ssh -G` 复用 OpenSSH 完整解析（ADR-0006 已定）。 |
| γ | **无真正 ProxyJump**（用 commandTemplate "ssh jumphost ..." 假装） | TermBridge Phase 2 做真正 SSH 多跳链式。 |
| δ | **正则命令白名单可绕过**（`^ls.*` 挡不住 `ls; rm`） | TermBridge Policy 至少做 shell 语法感知，或 deny-by-default + 显式校验。 |
| ε | **exec 模式 PTY 默认开启**（合并 stdout/stderr） | TermBridge PTY 是 Session 模式（不是 exec），stdout/stderr 本就合流是 PTY 特性；但 exec 场景应分离。 |
| ζ | **exec 一次性、无 stdin 交互** | 这是 classfang 的核心局限。TermBridge 定位"Terminal MCP"，持久 PTY 是差异化卖点。 |
| η | **SFTP 单文件、无递归** | TermBridge Phase 2 支持目录递归。 |
| θ | **单例 + 可变共享 Map**（JS 单线程才安全） | Rust 用 `DashMap<name, Arc<Mutex<Connection>>>` 或 actor 模型。 |
| ι | **凭据明文进 mcp.json args**（暴露进程列表） | TermBridge 优先 ssh-agent / 环境变量，避免密码进 args。 |
| κ | **download/upload 参数顺序不一致** | TermBridge 统一 `(local_path, remote_path)` 顺序。 |

---

## 三、对 v0.3 PLAN 的具体修订建议

### 修订 1：§5.2 OutputEngine 结构明确化

补充 pty-mcp 的"物理环形 + 逻辑单调 written 计数器"设计：

```rust
struct OutputRingBuffer {
    buf: Vec<u8>,           // 物理环形
    size: usize,            // 容量（默认 1MB，上限 32MB）
    head: usize,            // 下一个写入位置（环形）
    written: u64,           // 单调递增总写入字节数（永不回绕，作绝对游标）
    notify: tokio::sync::Notify,  // 新 output 唤醒
}
// 游标语义：
// - mark_snapshot: 内部消费游标（settle/wait_for 用）
// - since_cursor: 外部调用方自管（多 consumer 增量读）
```

### 修订 2：§5.3 read_output 加 since_cursor 参数

```json
{
  "session_id": "sess_123",
  "wait_for": "Server started",   // optional
  "timeout": 5,                   // optional, 默认 5s
  "tail_lines": 20,               // optional, 不推进 cursor
  "since_cursor": 1234            // optional, 增量读，不推进 mark_snapshot
}
```

新增 since_cursor 模式：返回 cursor 之后的数据 + 新 cursor + has_more + is_truncated。**不推进 mark_snapshot**，支持多 consumer。

### 修订 3：§4.6 契约补充

- 契约 5 补充："wait_for 命中推进 cursor，超时不推进（留作后续 read 未读数据）"
- 新增契约 11："since_cursor 模式不推进 mark_snapshot，调用方自管 cursor，支持多 consumer"

### 修订 4：§5.3 drain 模式改 settle 语义

原 v0.3："有数据即返回" → 改为：50ms 轮询 + 300ms settle 阈值 + 空输出永不 settled + prompt 检测立即返回。

### 修订 5：§5.2 加日志 tee

PTY output 同时 tee 到 bounded RingBuffer + 滚动日志文件（buffer 溢出数据仍有日志兜底）。

### 修订 6：§5.5 审计脱敏引用 pty-mcp 正则

key=value 凭证、Authorization header、PEM 块三类正则直接复用 pty-mcp `audit/redact.go`。

### 修订 7：§6 工具错误格式统一

`{code, message, retriable}` JSON + `isError: true`。

### 修订 8：§9 Phase 1 SFTP 加路径安全

allowedLocalPaths（默认 cwd）+ allowedRemotePaths + realpath 防穿越 + 下载原子写。

### 修订 9：§9 Phase 1 加 keepalive + idleReaper

keepalive 10s + 3 次上限；idleReaper 30s tick，先收集超时 session 再释放锁后 Close。

### 修订 10：§9 Phase 1 加 known_hosts 校验

host key verification 必须做（classfang 的前车之鉴）。
