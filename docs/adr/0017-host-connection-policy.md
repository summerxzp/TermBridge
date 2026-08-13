# ADR-0017：Host Connection Policy

- **Status**: Accepted
- **Date**: 2026-08-13
- **Phase**: 9（Phase 8 Runtime Freeze 之后的 Application 层扩展）
- **Supersedes**: —
- **Depends on**: [ADR-0009](0009-bootstrap-host-and-credential-provider.md)（bootstrap_host / CredentialProvider）、[ADR-0016](0016-runtime-freeze.md)（Runtime Freeze）
- **Amends**: —

## 1. Context

### 1.1 Runtime Freeze 之后的扩展边界

ADR-0016 冻结了 Runtime Core（TerminalProvider / TerminalHandle / Runtime Contract / MCP schema / Agent Terminal Protocol）。Host Policy 属于 **Application 层的用户偏好配置**，不修改任何冻结的 trait / contract / schema 签名，是 Freeze 之后典型的正确扩展方向：

```text
Runtime Freeze（ADR-0016）
      ↓
Consumer / Policy Layer
      ↓
HostPolicy（本 ADR）
```

而非：

```text
Runtime Freeze
      ↓
偷偷改变 Session / Provider / Contract
```

### 1.2 现状的两个事实（基于实际代码校验）

1. **`open_session` 当前只有 key 认证路径**。`auth=password`（每次连接输密码、不保存 key）是一个**真实的功能缺口**，不是已有能力加开关。
2. **`open_session` 已有 `persistent: Option<bool>` 参数**（默认 `false` = standard）。Session 维度不需要重新设计，只需要 per-host 持久化默认值。

### 1.3 为什么要 Host Policy

真实运维场景中，不同 host 的约束差异很大：

| 场景 | 诉求 |
|---|---|
| 开发机 | key + persistent（最佳体验） |
| 测试机 | key + standard |
| 生产机 | key + standard，不允许额外 daemon |
| 临时/遗留设备 | password + standard，不部署 key |
| 受限环境 | password + standard，不允许留任何 TermBridge 文件 |

当前 TermBridge 没有表达这些差异的机制，Agent 每次调用都默认冲着"key 免密 + persistent session"去，与生产/受限环境诉求冲突。

## 2. Decision

### 2.1 Host Policy 定位

Host Policy 是 **Application 层的用户意图配置**，回答一个问题：

> 这台 Host 默认应该怎么使用 TermBridge？

它**不是**：
- SSH 连接参数（那是 OpenSSH config 的职责）
- 远端服务器状态缓存
- 强制约束（RBAC / enforcement policy）

```text
Host
  ├── SSH Config        ← OpenSSH：怎么找到并连接主机
  │
  └── Host Policy       ← TermBridge：用户希望怎么使用这台主机
        ├── auth        ← 认证方式偏好
        └── session     ← session 持久化偏好
```

### 2.2 不可变原则（核心）

> **TermBridge must never implicitly mutate Host Policy as a side effect of connection, authentication, bootstrap, or session operations. Host Policy changes require explicit user intent.**

Host Policy = 用户意图。远端状态（Remote State）可以暂时与用户意图不一致，这是**允许的**。例如：

```text
hosts.toml: auth = password
     ↓
用户显式调 bootstrap_host()
     ↓
远端 authorized_keys 增加了公钥
     ↓
hosts.toml: auth = password   ← 保持不变
```

用户可能今天为了排障临时 bootstrap 一次 key，但仍希望 TermBridge 下次继续走 password。自动修改配置反而违反用户意图。

三个概念必须严格分离，不可互相覆盖：

```text
SSH Config      = 怎么找到并连接主机
Host Policy     = 用户希望 TermBridge 怎么使用这台主机
Remote State    = 服务器当前实际上发生了什么
```

### 2.3 字段定义

```toml
[hosts.<alias>]
auth = "auto" | "key" | "password"
session = "standard" | "persistent"
```

两个字段均可省略，省略时用 system default（§2.5）。

#### auth

| 值 | 语义 |
|---|---|
| `key` | 仅 SSH Agent / IdentityFile 认证。key 失败 → `AuthFailed`，不弹密码 |
| `password` | 每次新连接通过 CredentialProvider 请求密码。**不持久化密码，不部署 key** |
| `auto` | **当前等价于 key-only**。使用系统现有 SSH key 认证机制，**不进行密码 fallback**。`auto` 不预留未来 fallback 语义；若未来引入 fallback，必须走显式行为变更 + 新 ADR |

#### session

| 值 | 语义 |
|---|---|
| `standard` | 不部署远端 TermBridge runtime。SSH 断开 → session 丢失 |
| `persistent` | 允许部署并管理远端 runtime（ADR-0004 路径） |

#### 组合语义

| auth | session | 用途 |
|---|---|---|
| key / auto | standard | 纯 SSH，普通开发/生产/安全环境 |
| key / auto | persistent | **TermBridge 最佳体验** |
| password | standard | 临时/遗留/受限机器 |
| password | persistent | **不支持**。Persistent runtime 的建立过程（check → deploy agentd → bootstrap daemon → exec）每一步都打开新的 SSH 连接，依赖**可复用、非交互的 key-based authentication**；password 模式每次连接需交互式密码，与 unattended runtime 管理冲突。`open_session` 在 credential prompt 之前返回 `InvalidArgument` |

> **不做静默降级**：TermBridge 不会把显式请求的 persistent session 静默降级为 standard session。用户请求的语义与实际执行语义必须一致（Runtime Contract 核心思想）。`auth=password + session=persistent` 组合在弹密码**之前**返回 `InvalidArgument`（附修复建议：改用 `session=standard` 或 `auth=key`），而不是打开一个用户没要求的 session 类型。

### 2.4 优先级解析

```text
explicit tool argument
    >
host policy
    >
system default
```

示例：

```toml
[hosts.prod]
session = "standard"
```

Agent 调用 `open_session(host="prod", persistent=true)` → 使用 `true`（显式参数优先）。

这对高级用户/Agent 很重要：host policy 只是默认值，不是约束。

### 2.5 默认值

| 字段 | system default |
|---|---|
| auth | `auto`（等价 key-only） |
| session | `standard` |

默认值保证向后兼容：无 `hosts.toml` 时，行为与当前完全一致。

### 2.6 Host Policy 不做 enforcement

> **Host policy is a default preference, not an enforcement policy.**

第一版**不**引入：
- `allow_remote_runtime`
- `allowed_session_modes`
- 任何"禁止某模式"的约束列表

`session = "standard"` 已经充分表达"不部署 agentd"。加 `allow_remote_runtime = false` 会引入 `session=persistent + allow_remote_runtime=false` 这种自相矛盾的配置状态，无收益。

若未来出现"prod 机被误装 agentd"事故，再评估是否加约束层，需新 ADR 论证。

### 2.7 `open_session` 的副作用声明

引入 `auth=password` 后，`open_session` 从"纯即时操作"变为"potentially interactive operation"：

```text
auth = password
    ↓
open_session()
    ↓
CredentialProvider.request_password()   ← 触发用户 prompt
    ↓
SSH password auth
    ↓
session
```

ADR-0013 Agent Terminal Protocol 的隐含契约（"open_session 即时返回"）在此场景下被显式扩展：

> `open_session` 在 host auth policy = password 时可能触发 CredentialProvider prompt。Agent 调用前无法预知是否需要用户介入。

这是允许的（ADR-0016 §2.2 "新增可选参数允许"），但 Skill 必须向 Agent 传达这一行为（§3.3）。

### 2.8 `bootstrap_host` 与 `auth=password` 是两条独立路径

| 路径 | 目标 | 改变 Host Policy? |
|---|---|---|
| `bootstrap_host` | 改变 Host 的长期认证方式（部署公钥） | ❌ 不改变 |
| `auth=password` 的 `open_session` | 每次用密码建立 session，不改变服务器认证配置 | ❌ 不改变 |

两者不互相隐式调用。`bootstrap_host` 成功后返回 `hint`（非阻塞提示），**不修改 hosts.toml**：

```json
{
  "status": "bootstrapped",
  "authentication": "public_key",
  "hint": "Host now supports key authentication; consider changing host policy to auth=key."
}
```

用 `hint` 字段而非 `auth` 字段，避免调用方误以为 TermBridge 已修改策略。

### 2.9 配置文件位置

| 平台 | 路径 |
|---|---|
| Linux / macOS | `~/.config/termbridge/hosts.toml`（XDG） |
| Windows | `%APPDATA%\TermBridge\hosts.toml` |

平台路径解析用 `dirs` crate，不在代码里硬编码。这是实现细节，不进 ADR 决策。

> **macOS 强制 XDG**：`dirs::config_dir()` 在 macOS 返回 `~/Library/Application Support`（Apple 原生惯例，适合 GUI 应用）。TermBridge 是 CLI/开发者工具，hosts.toml 是用户手写的 toml，应遵循 XDG 惯例（`~/.config`，尊重 `XDG_CONFIG_HOME`），与 git/vim/tmux 等 CLI 工具一致。
>
> **Windows 目录名 `TermBridge`**（与 agentd 本地路径一致）：`%APPDATA%\TermBridge\hosts.toml`；Unix 用小写 `termbridge`（XDG 惯例）。
>
> **IP / 点号别名必须加引号**：TOML 的 `[hosts.192.168.1.180]` 会把点号解析为嵌套表（静默产生垃圾条目，策略不生效）。正确写法 `[hosts."192.168.1.180"]`。加载器检测到非 `auth`/`session` 字段会 WARN 提示。

### 2.10 配置文件最小化

第一版 `hosts.toml` 只有两个字段（auth / session）。**不**加入：
- `allow_remote_runtime` / `allowed_session_modes`（见 §2.6）
- `credential_store` / `password_policy`（凭据管理已有 ADR-0009 边界强化）
- `known_hosts_policy` / `proxy_policy`（已有 OpenSSH config 负责）

Host Policy 只回答"怎么使用这台主机"，不重新发明 SSH config 或 RBAC。

## 3. Consequences

### 3.1 正面

- **尊重每台服务器的安全/运维约束**：生产机可选 standard，受限机可选 password，不再一刀切 key+persistent
- **不违反 Runtime Freeze**：纯 Application 层配置，不动 Core trait / contract / schema
- **配置极简**：两个字段，无组合爆炸，无自相矛盾状态
- **用户意图纯净**：Host Policy 不被操作副作用污染，三者（SSH Config / Host Policy / Remote State）职责清晰
- **向后兼容**：无 hosts.toml 时行为不变

### 3.2 代价

- **新增 `auth=password` 路径**：open_session 需接 CredentialProvider，功能新增
- **open_session 语义扩展**：从即时操作变为 potentially interactive，需 Skill 配合传达
- **新增配置文件**：首次使用需引导用户（或 GUI 弹窗）填写，不能纯靠 Agent 推断

### 3.3 Skill 必须跟进

引入 Host Policy 后，Skill 必须新增两条规则：

1. **不要在 password-policy host 上主动调 `bootstrap_host`**，除非用户明确要求切换/建立 key 认证。否则 Agent 会"自作主张优化"：在 password host 上偷偷 bootstrap key，违反用户意图。
2. **password host 每次 open_session 可能触发用户 prompt**，Agent 不应假设即时返回。

## 4. Implementation Plan

分步落地，每步独立可验证：

| 步骤 | 内容 | 验证 |
|---|---|---|
| 1 | ADR-0017（本文档） | 本文档过审 |
| 2 | HostPolicy resolver：读 hosts.toml + 优先级解析 | 单测：显式参数 > host policy > system default |
| 3 | open_session 接入 HostPolicy 默认值 | e2e：配 `session=standard` 的 host 不传 persistent → 走 standard；传 `persistent=true` → 走 persistent |
| 4 | `auth=password` 路径：CredentialProvider 注入 SessionManager + open_session password 分支 | e2e：配 `auth=password` 的 host 每次 open_session 弹密码，不部署 key；`auth=password + session=persistent` → 弹密码前返回 `InvalidArgument` |
| 5 | bootstrap_host 返回 hint（不修改 hosts.toml） | 单测：bootstrap 成功后 hosts.toml 内容不变 |
| 6 | CLI / GUI 展示 host policy | 手动验证 |
| 7 | Skill 更新（§3.3 两条规则） | Skill 文档审查 |

### 关键：第 2-3 步零功能改动

第 2-3 步只加配置层和默认值读取，不改任何认证逻辑，**零 Runtime Freeze 风险**。第 4 步才补 password 路径，是真正的功能新增。

## 5. Alternatives Considered

### A. Mode enum（`key-persistent` / `key-standard` / `password-persistent` / ...）

**否决**：组合爆炸。auth / session 解耦成两个独立维度更清晰。

### B. `auto` 预留 password fallback 语义

**否决**：会产生静默行为变更。今天 `auto` = key，未来 `auto` = key+password fallback，同一份配置升级后 `open_session` 副作用变化。`auto` 在本 ADR 中等价 key-only，未来若加 fallback 需显式行为变更 + 新 ADR。

### C. `allow_remote_runtime = false` 字段

**否决**：`session = "standard"` 已充分表达"不部署 agentd"。加该字段会引入 `session=persistent + allow_remote_runtime=false` 的自相矛盾状态，无收益。

### D. 把 hostname/user/port 也写进 hosts.toml

**否决**：重新发明 SSH config。OpenSSH config 管"怎么连上"，Host Policy 管"连上后怎么行为"，职责分离。

### E. bootstrap_host 成功后自动更新 hosts.toml 为 `auth=key`

**否决**：违反 §2.2 不可变原则。Host Policy 是用户意图，不是远端状态缓存。用户可能临时 bootstrap 一次但仍希望下次走 password。返回 `hint` 而非修改配置。

### F. Host Policy 做 enforcement（约束 Agent 不能用某模式）

**否决（第一版）**：Host Policy 是 default preference，不是 enforcement policy。约束层一旦加上，需在 open_session 里做 policy enforcement，污染 Application 层。第一版先把默认值做好；若未来出现"prod 被误装 agentd"事故再评估，需新 ADR。

### G. password 凭据贯穿 persistent runtime 内部连接

**否决**：PersistentProvider 的 check/deploy/bootstrap/exec 每一步都开新 SSH 连接，整个生命周期建立在 key-based unattended authentication 之上。把 password 线程化穿过这些内部连接，等于改变 ADR-0004 的核心运行模型，且仅服务于一个本身就自相矛盾的组合。以"明确拒绝"替代（§2.3），错误信息给出两种可行配置。

### H. `password + persistent` 静默降级为 standard

**否决**：silent semantic downgrade。用户显式配 `session=persistent`，若实际得到 SSH-bound session，Agent 会误以为 MCP 重启后 session 仍在。违反"用户请求的语义与实际执行语义必须一致"。宁可明确失败（§2.3），不偷偷降级。

## 6. Relationships

- **Depends on ADR-0009**：password 路径复用 CredentialProvider + `authenticate_with_password`
- **Depends on ADR-0016**：Host Policy 属于 Freeze 之后的 Application 层扩展，不动 Core
- **不修改 ADR-0013**：Agent Terminal Protocol 七条规则不变，但 open_session 副作用语义扩展（§2.7）需 Skill 传达
- **不修改 ADR-0004**：persistent/standard 维度已存在，本 ADR 只加 per-host 默认值
- **不修改 ADR-0005**：凭据隔离原则不变，password 模式仍不持久化密码、不进 MCP schema
