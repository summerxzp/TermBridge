# ADR-0018：Local Control Plane for Human Authorization

- **Status**: Accepted
- **Date**: 2026-08-14
- **Phase**: 0.2.1
- **Supersedes**: —
- **Depends on**: [ADR-0008](0008-scope-boundary.md)（Scope Boundary）、[ADR-0009](0009-bootstrap-host-and-credential-provider.md)（Credential Provider）、[ADR-0011](0011-input-semantics-and-execution-safety.md)（Input Semantics / sudo confirm）、[ADR-0017](0017-host-connection-policy.md)（Host Connection Policy）
- **Amends**: —

## 1. Context

### 1.1 当前架构的两个平面（缺一个）

TermBridge 当前的进程拓扑：

```text
AI Agent  ──MCP/stdio──▶  TermBridge MCP Server（SessionManager / PolicyManager）  ──SSH/PTY──▶  Remote Host
                                ▲
                                │
                          （没有人类通道）
```

事实：

1. **MCP server 是纯 stdio JSON-RPC 进程**。Agent 通过 MCP tools（`open_session` / `send_input` / `read_output` / `sftp_*`）操作 session，这是 **Agent Data Plane**。
2. **CLI（`termbridge.exe`）是独立进程**，有自己的 `SessionManager`，**无法**操作已经在 MCP server 内部运行中的 session。CLI 与 MCP server 之间没有共享通道。
3. **sudo 一刀切拦截阻断自动化**。ADR-0011 的 PolicyManager 对 `sudo` 命令统一 confirm，导致非交互场景（CI 排障、Agent 批量运维）也被卡住。
4. **`unrestricted` approval mode 不能由 Agent 自己批准**。这是安全边界：若 Agent 能自行调用 `set_approval_mode(unrestricted)`，等于 Agent 自己批准自己的权限提升，PolicyManager 形同虚设。

### 1.2 缺失的"Human Control Plane"

当前 TermBridge 架构里只有 Agent 数据面，没有人类授权/管理面：

```text
Agent Data Plane（MCP stdio）  ✓  已有
Human Control Plane            ✗  缺失
```

人类要操作 MCP server 内的 session（例如把某个 session 临时切到 unrestricted 以放行 sudo -n），目前**没有任何通道**：

- Agent 不能调用 `set_approval_mode`（安全边界，§1.1 第 4 点）
- CLI 与 MCP server 不共享 SessionManager（§1.1 第 2 点）
- hosts.toml 是 host-level 静态策略（ADR-0017），不是 session-scoped 动态授权

这就形成了一个明确的架构缺口：**需要一个仅人类可访问、不暴露给 Agent 的本地通道，让人类授权 session 级别的策略提升**。

### 1.3 为什么是 Session-scoped，不是 Host-scoped

ADR-0017 的 Host Policy 是 **持久化的用户意图**（auth / session），不可被操作副作用污染。而本 ADR 解决的是 **一次性的运行时授权**（"我现在信任这个 session 跑 sudo -n"），两者必须严格分离：

| 维度 | Host Policy（ADR-0017） | Approval Mode（本 ADR） |
|---|---|---|
| 粒度 | per-host | per-session |
| 持久化 | hosts.toml | 不持久化 |
| 修改者 | 用户显式编辑 toml | 人类经 Control IPC 授权 |
| 生命周期 | 配置文件生命周期 | Session 关闭即重置 |
| 写入 hosts.toml? | 是 | **否**（§2.7） |

### 1.4 为什么需要 sudo -n 的"窄口子"

ADR-0011 把 `sudo` 全部 confirm 是对的（默认安全）。但生产场景中，CI / Agent 自动化经常使用 `sudo -n` / `--non-interactive`，这是**显式的"禁止密码交互"信号**——调用方已经声明"我不要弹密码，能跑就跑，不能跑就失败"。

一刀切 confirm 会把这种本应自动化的场景也卡住。需要给 `sudo -n` 一个**保守、窄口子**的放行规则，作为 unrestricted 的补充（§2.9）。

## 2. Decision

### 2.1 两条平面的职责划分

```text
Agent Data Plane            Human Control Plane
─────────────────           ─────────────────────
传输：MCP / stdio            传输：Unix Socket / Named Pipe
调用者：AI Agent             调用者：CLI / GUI（人类）
操作：open_session           操作：session.list / session.get
       send_input                   session.set_approval_mode
       read_output
       sftp_*
       bootstrap_host
```

**核心原则**：Human Control Plane 上的操作（尤其 `set_approval_mode`）**永远不暴露为 MCP tool**。Agent 不能通过 MCP stdio 触发 approval mode 变更。

### 2.2 七条决策

1. **MCP stdio remains agent-only transport**。MCP tools（`open_session` / `send_input` / `read_output` / `sftp_*`）是 Agent 数据面，不暴露人类授权操作。
2. **Human approval is never exposed as an MCP tool**。不增加 `set_approval_mode` MCP tool，Agent 不可自行批准权限提升。
3. **MCP server exposes a local-only control IPC**。MCP server 启动时同时监听本地 IPC（Unix socket / Named Pipe），作为 Human Control Plane。
4. **CLI and GUI use the control IPC**。CLI（`termbridge session approve`）和未来 GUI 通过 Control IPC 操作 MCP server 内的 session，而不是各自维护独立 SessionManager。
5. **Control IPC uses Unix socket / Named Pipe**。Linux/macOS 用 Unix Domain Socket（`0600`）；Windows 第一版用 TCP loopback（`127.0.0.1:随机端口`），未来切 Named Pipe。
6. **Session approval is ephemeral and session-scoped**。`approval_mode` 绑定 Session，不持久化，Session 关闭即重置。
7. **Approval state is never persisted into host policy**。`hosts.toml` 仍只有 `auth` / `session`（ADR-0017），不包含 `approval_mode`。

### 2.3 架构图

```text
                         ┌─────────────┐
                         │ AI Agent    │
                         └──────┬──────┘
                                │
                           MCP / stdio
                                │
                                ▼
                     ┌───────────────────┐
                     │  TermBridge MCP   │
                     │                   │
                     │  SessionManager   │
                     │  PolicyManager    │
                     └─────────┬─────────┘
                               │
             ┌─────────────────┴─────────────────┐
             │                                   │
        Agent Data Plane                    Human Control Plane
             │                                   │
       MCP stdio                        Unix Socket / Named Pipe
             │                                   │
             │                            ┌──────┴──────┐
             │                            │             │
             │                          CLI           GUI
             │
             ▼
        Remote SSH/PTY
```

### 2.4 Control IPC 协议

- **传输层**：简化 JSON-RPC over newline-delimited JSON（每条消息一行 JSON，以 `\n` 分隔）。
- **握手**：连接后第一条消息必须是 `HELLO` + token 认证（§2.6）。
- **第一版方法**：

| 方法 | 入参 | 返回 |
|---|---|---|
| `session.list` | — | 所有 session 的 `ControlInfo` 数组（id / host / state / approval_mode） |
| `session.get` | `session_id` | 单个 session 的 `ControlInfo` |
| `session.set_approval_mode` | `session_id`, `mode`（standard / unrestricted） | 更新后的 `ControlInfo` |

`ControlInfo` 字段：

```json
{
  "id": "sess_a1b2c3",
  "host": "prod-web-01",
  "state": "attached",
  "approval_mode": "standard"
}
```

> `session.list` / `session.get` 是**只读**操作，对安全边界无影响；只有 `session.set_approval_mode` 是状态变更，必须经过 HELLO + token 认证。

### 2.5 Instance 发现机制

MCP Server 启动时在本地目录写入一个 instance 描述文件，CLI 扫描该目录发现运行中的 MCP Server：

| 平台 | 目录 | 文件名 |
|---|---|---|
| Linux | `$XDG_RUNTIME_DIR/termbridge/`（fallback `/tmp/termbridge/`） | `mcp-<instance>.json` |
| macOS | `$TMPDIR/termbridge/`（fallback `/tmp/termbridge/`） | `mcp-<instance>.json` |
| Windows | `%TEMP%/termbridge/` | `mcp-<instance>.json` |

文件内容：

```json
{
  "pid": 12345,
  "transport": "unix_socket",
  "endpoint": "/run/user/1000/termbridge/mcp-7f3a.sock",
  "token": "tok_<32-char-hex>",
  "started_at": "2026-08-14T03:21:08Z",
  "protocol_version": "0.2.1"
}
```

清理规则：

- **正常退出**：MCP Server 的 Drop 实现删除该文件。
- **异常退出**（kill -9 / 崩溃）：文件残留。CLI 扫描时检测 `pid` 是否仍存活，**stale instance 自动清理**。
- **同用户多实例**：允许多个 MCP Server 并存（不同 IDE / 不同项目），CLI 默认连接最近启动的（按 `started_at` 排序），`--instance <id>` 可显式选择。

### 2.6 安全要求

| 层 | 措施 |
|---|---|
| 网络 | 仅本机访问。Unix socket / TCP loopback（127.0.0.1），**不暴露网络端口** |
| 文件权限 | owner-only。Unix socket 文件 `0600`；instance json `0600`；Named Pipe 限制当前用户 SID |
| 认证 | instance token（CLI 连接时 `HELLO` 必须提供与 instance json 一致的 token） |
| 防冒充 | 即使同用户的其他本地进程拿到 instance json 路径，仍需 token 才能通过握手；token 仅在 instance json（`0600`）中可见 |

> **威胁模型**：本 ADR 不防御 root 级攻击者（root 可读任何文件、可 ptrace）。防御的是**同用户其他进程的意外/低强度冒充**（例如浏览器进程无意连上 IPC）。这是与"人类通过 CLI 显式授权"匹配的最小安全模型。

### 2.7 Approval Mode

#### 定位：defense-in-depth guardrail，不是 Primary HITL

TermBridge Policy 是 **secondary guardrail / defense-in-depth**，不是主要的人类审批系统。三层职责严格分离：

```text
Primary Approval        → Coding Agent / Host UI 负责（用户是否批准本次操作）
Secondary Guardrail     → TermBridge Policy 负责（hard deny + confirm 类风险）
Secret Handling         → CredentialProvider 负责（密码 / passphrase 不进 LLM context）
```

**不做 per-command HITL 弹窗**。命令执行的"是否需要用户批准"由 Coding Agent 负责（如 OpenCode 的 allow/ask/deny）。TermBridge 再造一套审批 UI 会导致双重审批 + 两套权限模型。TermBridge 只保留最低限度的 hard safety boundary（blocklist Deny）+ defense-in-depth confirm guardrail。

#### 两个值（第一版）

| `approval_mode` | 语义 |
|---|---|
| `Standard`（默认） | PolicyManager 正常执行 blocklist / confirm 检查（ADR-0011 全部生效） |
| `Unrestricted`（用户经 Control IPC 授权） | **只跳过 confirm 类 guardrail**，blocklist / hard deny **仍生效** |

> **`unrestricted` only disables TermBridge confirmation guardrails for the current session; it does not bypass hard-deny rules, SSH security checks, credential isolation, or protocol invariants.**

评估顺序：

```text
1. blocklist 检查 → 若命中 → Deny（unrestricted 也不绕过）
2. 若 unrestricted → Allow（跳过 confirm）
3. 若 standard → confirm 检查 → Confirm / Allow
```

即：

| 命令 | Standard | Unrestricted |
|---|---|---|
| 普通命令 | Allow | Allow |
| `sudo ls`（confirm） | Confirm | Allow |
| `rm -rf /tmp/x`（confirm） | Confirm | Allow |
| `rm -rf /`（blocklist） | **Deny** | **Deny** |
| `mkfs`（blocklist） | **Deny** | **Deny** |

> **Unrestricted 不是万能钥匙**。以下安全层**仍然生效**：
> - blocklist / hard deny（`rm -rf /`、`mkfs`、`dd of=/dev/` 等）
> - SSH / PTY 认证（ADR-0009 CredentialProvider）
> - path safety（sftp 路径校验）
> - credential 隔离（ADR-0005）
> - Session 生命周期（不会因 unrestricted 而绕过 attach / detach 校验）

#### 三个层次（明确分离）

```text
Host Policy (hosts.toml)   ← 默认行为（auth / session）       ADR-0017
       ↓
Session State              ← 当前运行状态（attached / detached）ADR-0010
       ↓
Approval Mode              ← 当前人工授权状态（session-scoped，不持久化）本 ADR
```

三层互不污染：Host Policy 仍由用户显式编辑 toml 修改；Session State 仍由 MCP 协议驱动；Approval Mode 由人类经 Control IPC 设置且不落盘。

### 2.8 PolicyManager 的接入点

引入 `ApprovalMode` 后，`PolicyManager` 的检查流程：

```text
send_input(command)
    ↓
PolicyManager.authorize_with_approval(cmd, session.approval_mode)
    ↓
┌──────────────────────────────────────┐
│  1. blocklist 检查（永远生效）        │
│     若命中 → Deny（unrestricted 也不绕过）│
│                                      │
│  2. if approval_mode == Unrestricted: │
│         → Allow（跳过 confirm）       │  ← 仅跳过 confirm 类 guardrail
│     else (Standard):                  │
│         run confirm 检查              │
│           - sudo confirm              │
│           - sudo -n 保守放行（§2.9）  │
│           - rm -rf / kill -9 / etc.  │
│         → Confirm / Allow             │
└──────────────────────────────────────┘
    ↓
approve / reject
```

`authorize_with_approval` 是 `PolicyManager.authorize` 之上的薄包装：先跑完整 policy 链，若返回 `Deny` 则直接 `Deny`（hard safety boundary），若返回 `Confirm` 且 `unrestricted` 则升为 `Allow`（跳过 confirm），否则原样返回。

### 2.9 sudo -n 保守放行（本 ADR 关联优化）

#### 设计原则

`sudo -n` / `--non-interactive` 作为**"禁止密码交互"的安全信号**，豁免 sudo confirm。但放行必须保守，避免被构造性绕过。

#### 五条放行条件（全部命中才放行）

| 条件 | 说明 |
|---|---|
| 1. 行内仅一次 sudo | `sudo ... sudo ...` 直接拒绝（避免嵌套混淆） |
| 2. 行首命令 | `sudo` 必须是管道/复合命令的第一个 token，否则 confirm |
| 3. 紧跟 `-n` 或 `--non-interactive` | `sudo` 后第一个参数必须是 `-n` / `--non-interactive` |
| 4. 无 shell 复合构造 | 不含 `&&` / `\|\|` / `;` / `>` / 子 shell `$()` 等，否则 confirm |
| 5. 不命中 blocklist / 其他 confirm | 即使 sudo -n，若命令本身在 blocklist（如 `rm -rf /`）或其他 confirm 触发，仍走 confirm |

任一条件不满足 → 走 ADR-0011 默认 sudo confirm。

#### 与 unrestricted 的互补关系

```text
                ┌─────────────────────┐
                │   PolicyManager     │
                └──────────┬──────────┘
                           │
            ┌──────────────┴───────────────┐
            │                              │
     approval_mode ==                approval_mode ==
       Standard                         Unrestricted
            │                              │
            ▼                              ▼
   ADR-0011 完整 pipeline          skip blocklist + confirm
            │                     （但 SSH/cred/path 仍校验）
            │
   命中 sudo -n 五条 → 放行
   否则 → sudo confirm
```

- **sudo -n 是零交互窄口子**：单条命令级别，5 条严格匹配，不改变 session 状态。
- **unrestricted 是用户授权宽口子**：session 级别，由人类经 Control IPC 显式设置，session 关闭即失效。

两者互补，不互相替代：用户可以只依赖 sudo -n 保守放行（保持 standard），也可以临时把 session 切到 unrestricted（批量放行）。

### 2.10 不做的事

- ❌ **不增加 Agent 可调用的 `set_approval_mode` MCP tool**。Agent 不可自行批准权限提升（§2.2 第 2 条）。
- ❌ **不做永久 host whitelist**。`approval_mode` 不持久化到 hosts.toml（§2.2 第 7 条）。
- ❌ **不做 `elevated` 中间档**。第一版只有 `standard` / `unrestricted`，避免组合爆炸（参考 ADR-0017 §5 A 的否决理由）。
- ❌ **不做完整 HITL Policy UI**。P2 计划，过渡期用 CLI `session approve` 命令。本 ADR 只定义 Control IPC 协议 + CLI 入口。

## 3. Consequences

### 3.1 正面

- **补上 Human Control Plane 架构缺口**：TermBridge 从单平面（Agent 数据面）变为双平面（Agent 数据面 + 人类控制面），职责清晰
- **Agent 不能自批准权限提升**：`set_approval_mode` 不在 MCP tool 列表，安全边界明确
- **CLI 与 MCP server 共享 session 视图**：CLI 通过 Control IPC 操作 MCP server 内 session，不再各自独立
- **Session-scoped 授权不污染 host policy**：hosts.toml 保持纯净（ADR-0017 §2.2 不可变原则不破）
- **sudo -n 自动化友好**：CI / Agent 批量运维场景不再被一刀切 confirm 卡住
- **Windows 第一版可落地**：TCP loopback 实现，未来再切 Named Pipe，不阻塞功能上线

### 3.2 代价

- **MCP server 启动复杂度增加**：需同时起 stdio JSON-RPC + Control IPC listener
- **instance 文件管理**：需处理 stale instance 清理、多实例并存、token 生成与校验
- **CLI 新增子命令**：`termbridge session approve` 需要发现 + 连接 + 认证 + 调用四步
- **跨平台传输差异**：Unix socket 与 TCP loopback 的 API 不一致，需抽象 transport 层
- **ApprovalMode 串入 PolicyManager**：`authorize_with_approval` 引入新分支，需充分测试 standard / unrestricted 两条路径

### 3.3 Skill 必须跟进

引入 Approval Mode 后，Skill 必须新增规则：

1. **Agent 不能调用 `set_approval_mode`**。若 Agent 判断需要 unrestricted，应**告诉用户**："请在 CLI 运行 `termbridge session approve <session_id> --mode unrestricted`"，而不是尝试自己调用。
2. **sudo -n 自动放行 ≠ unrestricted**。Agent 看到 sudo -n 命令在 standard 模式下也能跑，是 ADR-0018 §2.9 的保守放行，不是 session 进入了 unrestricted。
3. **Approval Mode 是 session-scoped**。Session 关闭后 unrestricted 失效，新 session 默认 standard。Agent 不应假设 unrestricted 是持久状态。

## 4. Implementation Plan

分步落地，每步独立可验证：

| 步骤 | 内容 | 验证 |
|---|---|---|
| 1 | ADR-0018（本文档） | 本文档过审 |
| 2 | `ApprovalMode` enum + `Session.approval_mode` 字段 | 单测：新建 session 默认 standard；set 后变更；close 后字段销毁 |
| 3 | `PolicyManager.authorize_with_approval` + sudo -n 五条保守放行 | 单测：standard 模式下 sudo -n 命中 5 条放行；命中 4 条 confirm；unrestricted 模式下 blocklist 跳过但 path safety 仍校验 |
| 4 | `src/transport/control/`：proto / handler / server / instance | 单测：HELLO 认证成功/失败；session.list/get/set_approval_mode round-trip |
| 5 | Instance 发现：写 / 读 / stale 清理 | 单测：MCP 启动写文件；CLI 扫描发现；kill -9 后 CLI 检测 stale 并清理 |
| 6 | MCP server 启动 Control IPC listener（main.rs） | e2e：MCP server 启动后 instance 文件出现，Control IPC 可连接 |
| 7 | CLI `termbridge session approve` 子命令 | e2e：CLI 发现 MCP → HELLO → set_approval_mode → MCP 内 session.approval_mode 变更 |
| 8 | Skill 更新（§3.3 三条规则） | Skill 文档审查 |

### 关键：第 2-3 步零架构改动

第 2-3 步只加 enum 字段和 PolicyManager 包装，不动 MCP schema / Runtime Core trait，**零 Runtime Freeze 风险**。第 4-7 步才是 Control IPC 真正的功能新增。

## 5. Alternatives Considered

### A. 把 `set_approval_mode` 暴露为 MCP tool

**否决**：Agent 可自行批准权限提升，安全边界消失。`unrestricted` 的全部意义就是"人类显式授权"，让 Agent 自己调用等于 PolicyManager 自我作废。

### B. CLI 与 MCP server 共享 SessionManager（同进程）

**否决**：MCP server 是 stdio JSON-RPC 进程，stdin/stdout 被 Agent 占用；CLI 是独立交互进程，无法共用 stdio。强行合并会破坏 ADR-0002 的 stdio-only 决策。Control IPC 是同进程内 SessionManager 的"对外窗口"，更合适。

### C. 持久化 `approval_mode` 到 hosts.toml

**否决**：违反 ADR-0017 §2.2 不可变原则。Host Policy 是用户意图，不应被运行时授权状态污染。Approval Mode 是 session-scoped 一次性授权，Session 关闭即失效。

### D. 引入 `elevated` 中间档（standard / elevated / unrestricted）

**否决**：组合爆炸。三档之间的差异（"哪些 confirm 跳过、哪些保留"）难以清晰定义，且用户难以理解。第一版只做 standard / unrestricted 两档，若未来证明需要中间档再走新 ADR。

### E. Control IPC 走 TCP 网络端口（非 loopback）

**否决**：暴露网络攻击面。本机其他用户 / 局域网设备可能扫描到端口。Unix socket / TCP loopback 是严格本机访问，符合最小暴露原则。

### F. sudo -n 全量放行（不做 5 条保守匹配）

**否决**：构造性绕过风险。`sudo -n ls && rm -rf /` 这类复合命令若全量放行，等于把 sudo -n 当万能钥匙。5 条保守匹配（§2.9）确保只放行"单条、行首、紧跟 -n、无复合、不命中 blocklist"的窄场景。

### G. 用环境变量传递 token 而非 instance json 文件

**否决**：环境变量在 `/proc/<pid>/environ` 可被同用户进程读取（Linux），且 CLI 启动时需要知道连哪个 MCP server，env 无法承载"发现"语义。instance json 文件（`0600`）同时解决发现 + token 传递两个问题。

### H. Windows 第一版就上 Named Pipe

**否决（第一版）**：Named Pipe 的 ACL / SID 配置在 Rust 生态中实现复杂度高于 TCP loopback。第一版用 TCP loopback（127.0.0.1:随机端口）快速落地，未来再切 Named Pipe。功能行为一致，仅传输层差异。

## 6. Relationships

- **Depends on ADR-0008**：TermBridge 定位 = Remote Terminal Runtime。Control Plane 不引入超出 Runtime 范畴的能力（不做 RBAC / audit log）
- **Depends on ADR-0009**：Approval Mode 不绕过 CredentialProvider；`unrestricted` 跳过 command-level policy 但保留 SSH / credential 安全层
- **Depends on ADR-0011**：`ApprovalMode` 是 ADR-0011 PolicyManager 之上的包装层；sudo -n 保守放行是 ADR-0011 sudo confirm 的窄口子优化
- **Depends on ADR-0017**：`approval_mode` 不持久化到 hosts.toml，Host Policy 字段保持 `auth` / `session` 不变
- **不修改 ADR-0002**：MCP stdio 仍是 Agent-only 传输，Control IPC 是独立的 Human Control Plane 通道
- **不修改 ADR-0013**：Agent Terminal Protocol 七条规则不变；Agent 不能调用 `set_approval_mode`，无需扩展 protocol
- **不修改 ADR-0016**：Runtime Freeze 不动；`ApprovalMode` 是 Application 层枚举，不进 Core trait / contract / schema
