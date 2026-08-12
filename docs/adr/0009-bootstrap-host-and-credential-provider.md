# ADR-0009：bootstrap_host + CredentialProvider —— 首次 SSH 认证引导

- **Status**: Accepted
- **Date**: 2026-08-10
- **Phase**: 5（在 Phase 5 Remote Workspace 之后确立，解决多服务器接入首次认证问题）
- **Amends**: ADR-0005 §1（把 "Phase 6 HITL" 具体化为 CredentialProvider + bootstrap_host）
- **Supersedes**: —

## Context

Phase 5 完成后，TermBridge 已具备 Remote Workspace 能力（SFTP 递归 / 环境检测），但在切换到新目标服务器（192.0.2.171）测试时暴露了一个长期欠账：**首次连接无可用 SSH key 时无法登录**。

ADR-0005 §1 已确立凭据优先级 `SSH Agent > IdentityFile > HITL(Phase 6)`，其中密码 / passphrase 标记为 "Phase 6 HITL UI，secret 直接写 PTY，不经 LLM context"。但实战发现这个方向需要更具体的架构设计，而非笼统的"HITL"。

### 问题分解

**1. 首次登录的鸡生蛋问题**

```text
新服务器 → ~/.ssh/authorized_keys 无本机公钥 → key 认证失败 → 无法连接
                                              ↓
                                     没有机制部署公钥（因为连不上）
```

需要密码认证作为 bootstrap 凭据，认证成功后部署公钥，后续全部走 key 认证。

**2. MCP stdio 不能用于密码交互**

```text
Agent ──MCP──► termbridge.exe
                 stdin/stdout = MCP transport（被占用）
                 stderr       = tracing log（不可靠交互通道）
```

MCP server 的 stdin/stdout 是协议通道，不能混用为人机交互。stderr 也不能依赖 IDE/Agent 一定把交互呈现给用户。

**3. 密码不能进 MCP tool arguments**

即使标记 "bootstrap-only，不持久化"，密码作为 MCP 参数仍会进入：

```text
LLM context → tool call → MCP host logs → conversation transcript → telemetry
```

TermBridge 无法保证下游不记录。redaction 是事后补救，**从 schema 层禁止才是根本方案**。

### 调研中否决的方案

| 方案 | 否决理由 |
|---|---|
| `open_session(password=...)` MCP 参数 | 密码进 LLM context / transcript，违反 ADR-0005 §1 |
| `bootstrap_host(password=...)` MCP 参数 | 同上，且 bootstrap_host 名称暗示"接收密码"是误导 |
| termbridge.exe 内部 spawn GUI thread | UI 崩溃拖垮 MCP 进程；Core 被平台 UI API 污染 |
| termbridge.exe 从 stderr 读密码 | MCP 客户端不一定把 stderr 呈现给用户，不可靠 |
| TermBridge 持久化保存密码 | 违反"密码是 bootstrap 凭据，key 才是长期凭据"原则 |
| 完整 Credential Service（Keychain/Secret Service） | 过度设计，Phase 5 不需要 |

## Decision

### 1. CredentialProvider：平台无关的 Core 抽象

```rust
/// 平台无关的凭据请求抽象（Core 层）。
///
/// TermBridge Core 只依赖此 trait，不直接依赖任何平台 UI API。
/// MVP 实现：HelperCredentialProvider（spawn 独立 helper process）。
pub trait CredentialProvider: Send + Sync {
    /// 请求密码（一次性使用，调用方负责 Zeroize）。
    async fn request_password(&self, request: PasswordRequest) -> Result<Secret, CredentialError>;

    /// 请求 private key passphrase（MVP 可不实现，优先走 SSH Agent）。
    async fn request_passphrase(&self, request: PassphraseRequest) -> Result<Secret, CredentialError>;
}

pub struct PasswordRequest {
    pub host: String,
    pub user: String,
    pub reason: String, // 如 "bootstrap: deploy public key to authorized_keys"
}

pub struct Secret {
    inner: zeroize::Zeroizing<String>,
}

impl Secret {
    /// 暴露明文给调用方（仅 SSH 认证瞬间使用）。
    pub fn reveal(&self) -> &str { &self.inner }
    /// 用完立即释放（drop 时 Zeroizing 自动清零内存）。
}
```

**关键约束**：
- `Secret` 内部用 `Zeroizing<String>`，drop 自动清零
- Core 只依赖 trait，不 import 任何 `windows` / `cocoa` / `x11` crate
- 平台实现通过依赖注入传入 `SessionManager` / `BootstrapHost`

### 2. HelperCredentialProvider：MVP 实现

```text
┌──────────────────────────────────────┐
│             AI Agent                 │
└──────────────────┬───────────────────┘
                   │ MCP (stdio)
                   ▼
┌──────────────────────────────────────┐
│          termbridge.exe              │
│                                      │
│ BootstrapHost                        │
│ CredentialProvider (trait)           │
│   └─ HelperCredentialProvider        │
└──────────────────┬───────────────────┘
                   │ spawn + private IPC pipe
                   │ (helper stdin/stdout 与 MCP 完全隔离)
                   ▼
┌──────────────────────────────────────┐
│   termbridge-credential-prompt       │
│   (workspace member crate, 跨平台)    │
│                                      │
│  ┌──────────┐ ┌────────┐ ┌────────┐ │
│  │ Windows  │ │ macOS  │ │ Linux  │ │
│  │ native   │ │ native │ │ dialog │ │
│  │ dialog   │ │ (后续) │ │/TTY    │ │
│  └──────────┘ └────────┘ └────────┘ │
└──────────────────────────────────────┘
```

**同一份 Rust 源码，三个平台构建产物**（非三个项目）：

```text
cargo build --target x86_64-pc-windows-msvc  → termbridge-credential-prompt.exe
cargo build --target aarch64-apple-darwin    → termbridge-credential-prompt
cargo build --target x86_64-unknown-linux-gnu → termbridge-credential-prompt
```

**IPC 协议（简版 JSON over pipe）**：

```text
TermBridge → helper (stdin):
  { "type": "password_request", "host": "192.0.2.171", "user": "root", "reason": "..." }

helper → TermBridge (stdout):
  { "type": "password", "value": "..." }     // 成功
  { "type": "cancelled" }                     // 用户取消
```

**安全约束**：
- helper stdout 只被 TermBridge 捕获，不经过 MCP transport
- 不写日志、不写文件、不经过环境变量
- TermBridge 收到后立即用于 SSH 认证，认证完成 Zeroize

### 3. bootstrap_host MCP 工具

**暴露给 Agent**，职责：把 Host 初始化为长期可 key 认证的 SSH 主机。

```json
{
  "name": "bootstrap_host",
  "description": "Bootstrap SSH key authentication for a host. Triggers credential prompt if no key available. Does not accept password in arguments.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "host": { "type": "string", "description": "SSH host alias (from ~/.ssh/config) or direct hostname/IP" }
    },
    "required": ["host"]
  }
}
```

**关键：schema 中无 password / secret / passphrase 任何字段。** 这是从架构层杜绝凭据进入 LLM context。

**返回结构化状态**：

```json
// 已有可用 key 认证
{ "status": "already_configured", "host": "...", "authentication": "public_key", "identity_source": "ssh_agent" }

// bootstrap 成功
{ "status": "bootstrapped", "host": "...", "authentication": "public_key", "identity_source": "identity_file" }

// 用户取消密码输入
{ "status": "cancelled", "host": "..." }

// 密码错误
{ "status": "authentication_failed", "host": "..." }

// 公钥部署成功但 key 重连验证失败（权限/sshd 配置问题）
{ "status": "bootstrap_failed", "host": "...", "reason": "key auth verification failed after install" }
```

### 4. 完整 bootstrap 验证流程

```text
bootstrap_host(host)
    │
    ▼
解析 SSH config（ssh -G：user/port/identityfile/known_hosts/strict）
    │
    ▼
校验 host key（复用 ADR-0005 §2 known_hosts 机制）
    ├── mismatch → HOST_KEY_REJECTED（不自动接受，要求人工介入）
    │
    ▼
尝试 SSH Agent 认证
    ├── success → status: already_configured (identity_source: ssh_agent)
    │
    ▼
尝试已配置的 IdentityFile 认证
    ├── success → status: already_configured (identity_source: identity_file)
    │
    ▼
无 IdentityFile 或全部失败
    │
    ├── 无 IdentityFile → 自动生成 ed25519 keypair
    │     (~/.ssh/id_ed25519, 无 passphrase; 若文件已存在则跳过生成)
    │
    ▼
CredentialProvider.request_password()
    ├── 用户取消 → status: cancelled
    │
    ▼
密码 SSH 认证（一次性）
    ├── 失败 → status: authentication_failed
    │
    ▼
部署公钥到远端 ~/.ssh/authorized_keys
    │   - 幂等：读取现有 authorized_keys，公钥已存在则跳过写入
    │   - 确保 ~/.ssh 权限 700，authorized_keys 权限 600
    │
    ▼
关闭密码连接
    │
    ▼
新建连接 + key 认证验证（关键步骤，不能省）
    ├── 失败 → status: bootstrap_failed
    │          （可能原因：sshd PubkeyAuthentication no / SELinux context / home 权限）
    │
    ▼
status: bootstrapped (identity_source: identity_file)
```

**为何必须重连验证**：
- authorized_keys 权限错误不会在写入时报错
- sshd 配置可能 `PubkeyAuthentication no`
- SELinux context 可能不正确
- key 格式 / 算法可能不被服务器接受

只有重新用 key 认证成功，才能向 Agent 返回 `bootstrapped`。

### 5. 职责分层

```text
Host
 │
 ├── bootstrap_host    ← 解决身份问题（首次 key 部署）
 │
 └── open_session      ← 建立终端（纯连接，不做 bootstrap）
        │
        └── persistent=true ← 建立 Remote Runtime
```

`open_session` 保持纯连接语义，**不隐式触发 bootstrap**。Agent 显式调用 `bootstrap_host` 后再 `open_session`，职责清晰：

| 工具 | 职责 | 接收凭据 |
|---|---|---|
| `bootstrap_host` | 部署公钥 + 验证 key 认证 | ❌ 不接收（经 CredentialProvider out-of-band） |
| `open_session` | 用现有凭据建立终端 | ❌ 不接收 |
| `sftp_*` / `send_input` 等 | 在已建立 session 上操作 | ❌ 不接收 |

**所有 MCP 工具永久禁止 password / secret / passphrase 参数**（继承 ADR-0005 §1，本 ADR 强化）。

### 6. 公钥部署位置

部署到远端 `~/.ssh/authorized_keys`（OpenSSH 原生机制）。

**不搞自定义路径**（如 `~/.local/share/termbridge/authorized_keys`），避免改造 SSH 认证模型，遵循 OpenSSH 原生机制。

### 7. 自动生成 keypair（MVP 支持）

用户无任何 SSH key 时，`bootstrap_host` 自动生成：

```text
~/.ssh/id_ed25519      (0600, 无 passphrase)
~/.ssh/id_ed25519.pub  (0644)
```

- 算法：Ed25519（现代推荐，非 RSA）
- 无 passphrase（MVP 简化；passphrase 支持走后续 CredentialProvider.request_passphrase）
- 文件已存在则跳过生成，复用现有 key
- 生成后写入 `~/.ssh/config` 对应 Host 的 IdentityFile（若 config 中无此 Host 则跳过写入，仅依赖默认 key 名）

## Consequences

### 正面

- **密码永不进 LLM context**：从 MCP schema 层杜绝
- **Core 平台无关**：`CredentialProvider` trait 无平台依赖，Windows/macOS/Linux 通过同一 trait 接入
- **职责清晰**：`bootstrap_host`（身份）vs `open_session`（终端）分离
- **复用 OpenSSH 机制**：authorized_keys / known_hosts / IdentityFile 全部遵循原生机制
- **跨平台 helper 同源**：一份 Rust 代码三个平台构建产物，非三个项目

### 负面 / 代价

- **MVP 开发量增加**：需新建 workspace member crate `termbridge-credential-prompt`
- **Windows native dialog 需调 Win32 API**：`windows` crate 依赖，但隔离在 helper 内
- **macOS/Linux prompt 暂未实现**：MVP 只保证 Windows，其他平台编译通过但 prompt 可能 fallback TTY
- **多一个 binary 部署**：发布时需同时分发 `termbridge.exe` + `termbridge-credential-prompt.exe`

### 边界强化（避免范围蔓延）

本 ADR 顺便定死一条边界：

> **TermBridge 是 Remote Terminal Runtime，CredentialProvider 是它的平台适配器。不要因为首次密码登录，又慢慢把项目做成 Credential Manager。**

具体：
- ❌ 不做密码持久化保存（Windows Credential Manager / Keychain 等留作未来可选，不默认）
- ❌ 不做多凭据仓库管理
- ❌ 不做凭据轮换 / 过期提醒
- ✅ 只做"一次性密码 prompt → 部署公钥 → 后续全 key 认证"

## Implementation Plan

### 文件 / 组件清单

| 文件 | 操作 | 职责 |
|---|---|---|
| `docs/adr/0009-bootstrap-host-and-credential-provider.md` | 新增（本文件） | 决策记录 |
| `Cargo.toml` (workspace) | 修改 | 新增 `termbridge-credential-prompt` member |
| `crates/termbridge-credential-prompt/` | 新增 | 独立 helper binary crate |
| `crates/termbridge-credential-prompt/src/main.rs` | 新增 | IPC 读取 + 平台 prompt 分发 |
| `crates/termbridge-credential-prompt/src/platform/windows.rs` | 新增 | Win32 native dialog |
| `crates/termbridge-credential-prompt/src/platform/macos.rs` | 新增（stub） | 后续实现 |
| `crates/termbridge-credential-prompt/src/platform/linux.rs` | 新增（stub） | 后续实现 |
| `src/domain/credential.rs` | 新增 | `CredentialProvider` trait + `Secret` + `PasswordRequest` |
| `src/infrastructure/credential/helper.rs` | 新增 | `HelperCredentialProvider` 实现（spawn helper + IPC） |
| `src/infrastructure/ssh.rs` | 修改 | `authenticate_session` 支持密码降级（bootstrap 内部使用） |
| `src/application/bootstrap.rs` | 新增 | `BootstrapHost` 业务逻辑（解析 config / 尝试 key / 密码认证 / 部署公钥 / 重连验证） |
| `src/application/sessions.rs` | 修改 | 注入 `CredentialProvider`，注册 `bootstrap_host` 方法 |
| `src/transport/mcp/server.rs` | 修改 | 注册 `bootstrap_host` MCP 工具 |

### 实现顺序

1. **Core 抽象**：`CredentialProvider` trait + `Secret` + `PasswordRequest`（无平台依赖）
2. **Helper crate**：`termbridge-credential-prompt` workspace member + Windows native dialog
3. **HelperCredentialProvider**：spawn helper + IPC 协议
4. **SSH 密码认证**：`authenticate_session` 增加 password 降级路径（仅 bootstrap 调用）
5. **BootstrapHost 业务**：完整流程（解析 / key 尝试 / 生成 key / 密码认证 / 部署公钥 / 重连验证）
6. **MCP 工具**：注册 `bootstrap_host`
7. **e2e 验证**：用 192.0.2.171 测试完整 bootstrap 流程

## Alternatives Considered

### A. password 作为 MCP 参数（bootstrap-only）

**否决**：即使不持久化，密码仍进入 LLM context / transcript / telemetry。redaction 是事后补救，schema 层禁止才是根本。

### B. termbridge.exe 内部 spawn GUI thread

**否决**：UI 崩溃拖垮 MCP 进程；Core 被平台 UI API 污染；违反"Core 平台无关"原则。

### C. 完整 Credential Service（Keychain / Secret Service 集成）

**否决（Phase 5 不做）**：过度设计。MVP 只需"一次性 prompt + Zeroize"，不需要持久化凭据仓库。未来用户明确要求"记住密码"时再评估。

### D. 不做 bootstrap，要求用户手动 ssh-copy-id

**否决**：违反 TermBridge "自动化工作流，减少用户步骤"的产品定位（user_profile: "advocates for automated workflows that reduce user steps"）。bootstrap_host 让 Agent 一句话完成首次配置。

## Relationships

- **Amends ADR-0005 §1**：把 "Phase 6 HITL" 具体化为 `CredentialProvider + bootstrap_host`，明确密码不进 MCP schema
- **继承 ADR-0005 §2**：bootstrap 流程中 host key 校验复用现有 known_hosts 机制
- **符合 ADR-0008 边界**：bootstrap_host 属连接基础设施，在 Remote Terminal Runtime 范畴内
