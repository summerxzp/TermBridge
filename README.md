# TermBridge

**Remote Terminal Runtime for AI Agents** —— 把远端服务器的终端、文件系统、会话生命周期暴露为 MCP 工具，让 AI Agent 可编程地操作远程主机。

TermBridge 是一个 MCP（Model Context Protocol）server，通过 stdio 与 AI 客户端（Trae / Claude / Codex 等）通信。它不是 Ansible、不是 playbook 引擎，而是给 Agent 提供一个**稳定、可编程、安全**的远程 Terminal Runtime。

## 核心特性

- **纯 SSH 优先**：默认通过系统 `~/.ssh/config` + SSH Agent / IdentityFile 连接，远端零安装
- **首次连接引导**：`bootstrap_host` 工具通过 Windows 原生凭据对话框一次性输入密码，自动部署公钥，后续全部免密（密码永不进入 LLM context）
- **完整 Terminal 语义**：PTY byte stream + cursor + `wait_for` + settle 检测，不是屏幕快照
- **SFTP 文件/目录传输**：单文件原子写、目录递归、路径策略防越界
- **Persistent Runtime（可选）**：opt-in 部署远端 daemon，支持 session 跨 MCP 重启保活、detach/attach
- **执行时间线**：结构化记录命令/输出/控制/状态事件，供 Agent 排障
- **安全边界严格**：host key 严格校验、密码脱敏日志、SFTP 路径策略、凭据不进 MCP schema

## 安装

### 前置要求

- Rust toolchain（stable，建议 1.75+）
- Windows / macOS / Linux 均可编译（agentd crate 仅 Linux）
- 系统已安装 OpenSSH 并配置 `~/.ssh/config`

### 构建

```powershell
git clone <repo-url> TermBridge
cd TermBridge
cargo build --release
```

产物：

| 文件 | 路径 | 用途 |
|---|---|---|
| `termbridge.exe` | `target/release/termbridge.exe` | MCP server 主进程 |
| `termbridge-credential-prompt.exe` | `target/release/termbridge-credential-prompt.exe` | 首次认证密码输入 helper（需与主进程同目录） |

> macOS / Linux 下产物无 `.exe` 后缀，使用方式相同。

## 快速开始

### 第 1 步：配置 SSH Host

在 `~/.ssh/config` 中添加目标服务器：

```sshconfig
Host my-server
    HostName 192.168.1.171
    User root
    Port 22
    IdentityFile ~/.ssh/id_ed25519
```

### 第 2 步：接入 MCP 客户端

在你的 AI 客户端 MCP 配置中添加 TermBridge server。以 Trae / Claude Desktop 为例：

```json
{
  "mcpServers": {
    "termbridge": {
      "command": "C:\\path\\to\\termbridge.exe",
      "args": [],
      "env": {
        "RUST_LOG": "info"
      }
    }
  }
}
```

> `termbridge-credential-prompt.exe` 必须与 `termbridge.exe` 在同一目录，否则首次认证会 fallback 到不支持状态。

### 第 3 步：首次连接新服务器

如果目标服务器尚未部署你的 SSH 公钥，让 Agent 调用 `bootstrap_host`：

```
Agent: 我需要连接 my-server，请先 bootstrap。
→ 工具调用: bootstrap_host({ "host": "my-server" })
→ 桌面弹出 Windows 凭据输入框
→ 用户输入密码
→ TermBridge 部署公钥 + 验证 key 认证
← 返回: { "status": "bootstrapped", "authentication": "public_key" }
```

之后所有 `open_session` 调用均使用 SSH key 免密连接。

## 首次连接新服务器：bootstrap_host 详解

`bootstrap_host` 是 TermBridge 的安全设计核心，解决"新服务器首次登录的鸡生蛋问题"——没有 key 连不上，连不上没法部署 key。

### 工作流程

```text
bootstrap_host(host)
    │
    ▼
解析 ~/.ssh/config（ssh -G）
    │
    ▼
校验 host key（复用 known_hosts，不自动接受变更）
    ├── 变更 → HOST_KEY_REJECTED（要求人工介入）
    │
    ▼
尝试 SSH Agent 认证
    ├── 成功 → status: already_configured
    │
    ▼
尝试 IdentityFile 认证
    ├── 成功 → status: already_configured
    │
    ▼
无 IdentityFile → 自动生成 ed25519 keypair（~/.ssh/id_ed25519）
    │
    ▼
弹出 Windows 原生凭据对话框（CredUI）
    ├── 用户取消 → status: cancelled
    │
    ▼
密码 SSH 认证（一次性）
    ├── 失败 → status: authentication_failed
    │
    ▼
部署公钥到远端 ~/.ssh/authorized_keys（幂等：已存在则跳过）
    │
    ▼
关闭密码连接
    │
    ▼
新建连接 + key 认证验证（关键步骤，不能省）
    ├── 失败 → status: bootstrap_failed
    │          （可能原因：sshd PubkeyAuthentication no / SELinux / home 权限）
    │
    ▼
status: bootstrapped
```

### 安全保证

| 属性 | 实现 |
|---|---|
| **密码不进 LLM context** | MCP 工具 schema 中无 `password` / `secret` / `passphrase` 字段 |
| **密码经独立通道** | 弹窗由 `termbridge-credential-prompt.exe` 独立进程处理，IPC 与 MCP stdio 完全隔离 |
| **密码不持久化** | 认证后立即 `Zeroize`，不写入文件/日志/环境变量 |
| **host key 严格校验** | 复用 ADR-0005 known_hosts 机制，不自动接受变更 |
| **公钥部署幂等** | 部署前 `grep` 检查公钥是否已存在，避免重复写入 |
| **重连验证** | 部署公钥后必须重新用 key 认证成功，才返回 `bootstrapped` |

### 返回状态

| status | 含义 |
|---|---|
| `already_configured` | 已有可用 key 认证（SSH Agent 或 IdentityFile），无需 bootstrap |
| `bootstrapped` | 密码认证 + 公钥部署 + key 验证均成功 |
| `cancelled` | 用户取消密码输入 |
| `authentication_failed` | 密码错误 |
| `bootstrap_failed` | 公钥已部署但 key 重连验证失败（检查 sshd 配置 / 权限 / SELinux） |

## 工具列表（18 个）

TermBridge 向 MCP 客户端暴露 18 个工具，按功能分类：

### Host 管理

| 工具 | 说明 |
|---|---|
| `list_hosts` | 列出 `~/.ssh/config` 中所有 Host 别名及 hostname |

### Session 生命周期

| 工具 | 参数 | 说明 |
|---|---|---|
| `open_session` | `host`, `persistent?` | 建立 SSH + PTY session，返回 `session_id`。`persistent=true` 启用远端 daemon 跨重启保活 |
| `send_input` | `session_id`, `data` | 发送文本到 PTY stdin（`\n` 为回车，立即发送不等命令完成） |
| `read_output` | `session_id`, `wait_for?`/`tail_lines?`/`since_cursor?`, `timeout_secs?` | 读取 PTY 输出，支持 4 种模式：settle / wait_for / tail_lines / since_cursor |
| `send_control` | `session_id`, `control_key` | 发送控制键：`ctrl+c` / `ctrl+d` / `ctrl+z` / `tab` / `enter` / `escape` |
| `close_session` | `session_id` | 关闭 session（幂等），释放 SSH channel |

### SFTP 文件操作

| 工具 | 说明 |
|---|---|
| `sftp_transfer` | 单文件 upload / download（download 用原子写：temp + fsync + rename） |
| `sftp_mkdir` | 创建远端目录（mode 为八进制字符串如 `"755"`） |
| `sftp_list` | 列出远端目录内容（名称/类型/大小/权限） |
| `sftp_remove` | 删除远端文件或目录（`recursive=true` 删目录树，系统目录受保护） |
| `sftp_chmod` | 修改远端文件/目录权限（mode 为八进制字符串如 `"644"`） |

### SFTP 目录递归

| 工具 | 说明 |
|---|---|
| `sftp_transfer_dir` | 递归上传/下载目录，自动创建目标目录，跳过符号链接，返回传输文件数 |

### 远端环境检测

| 工具 | 说明 |
|---|---|
| `detect_remote_env` | 通过 SSH exec 检测远端 OS（uname）、shell、PATH、已装工具（python/node/rustc/go/docker/git 等），不污染 PTY session |

### Persistent Runtime（可选，opt-in）

| 工具 | 说明 |
|---|---|
| `list_remote_sessions` | 列出远端 daemon 上的所有 session（含已 detach 的） |
| `attach_remote_session` | attach 到远端已存在的 session（跨 MCP 重连） |
| `detach_session` | detach persistent session（保留远端 PTY，释放本地连接） |

### Observability

| 工具 | 说明 |
|---|---|
| `get_session_timeline` | 获取 session 执行时间线：命令/输出/控制/状态事件的有序列表（含 timestamp + cursor 元数据） |

### 首次认证

| 工具 | 说明 |
|---|---|
| `bootstrap_host` | 部署 SSH 公钥到远端 `authorized_keys`，详见上方专章 |

## SSH Config 配置建议

TermBridge 通过 `ssh -G` 解析系统 `~/.ssh/config`，支持常用指令：

```sshconfig
# 基本配置
Host prod-server
    HostName 192.168.1.171
    User root
    Port 22
    IdentityFile ~/.ssh/id_ed25519

# 通过跳板机
Host bastion-prod
    HostName 10.0.0.50
    User ops
    ProxyJump bastion.example.com

# 严格 host key 校验（推荐）
Host *
    StrictHostKeyChecking accept-new
    UserKnownHostsFile ~/.ssh/known_hosts
```

**支持**：HostName / User / Port / IdentityFile / ProxyJump / StrictHostKeyChecking / UserKnownHostsFile / IdentitiesOnly

**认证优先级**：SSH Agent > IdentityFile > （bootstrap_host 时的密码认证）

## Persistent Runtime（可选）

默认模式下 TermBridge 是纯 SSH：MCP server 退出 → session 丢失。如需 session 跨 MCP 重启保活，使用 `open_session(persistent=true)`：

```text
open_session(host, persistent=true)
    │
    ▼
首次：部署 termbridge-agentd 到远端 ~/.local/share/termbridge/
    │
    ▼
daemon 管理 PTY + OutputBuffer（Unix socket 通信）
    │
    ├── detach_session → 保留远端 PTY，释放本地连接
    │
    └── list_remote_sessions → attach_remote_session → 跨 MCP 重连
```

**约束**（Phase 3）：
- 远端 daemon 崩溃 = session 丢失（无 disk 持久化）
- daemon 单用户模式，socket 权限 0600
- 不开 TCP / HTTP，仅 Unix socket + SSH tunnel

## 安全模型

| 维度 | 策略 |
|---|---|
| **Host Key** | 严格校验 known_hosts，变更拒绝，不自动接受（ADR-0005 §2） |
| **SSH 认证** | 优先 SSH Agent / IdentityFile，密码仅 bootstrap 一次性使用（ADR-0009） |
| **密码隔离** | 密码经独立 helper process IPC，不进 MCP arguments / LLM context |
| **日志脱敏** | tracing 日志自动 redact 密码 / token / key 等敏感字段（ADR-0005 §3） |
| **SFTP 路径策略** | 本地路径限制在工作目录下，远端路径 realpath 解析防 `../` 越界（ADR-0005 §4） |
| **下载原子写** | 临时文件 + fsync + rename，避免半写文件被误读 |

## 开发者文档

> 本节为索引预留，后续完善。当前请参考 `docs/adr/` 下的架构决策记录。

### 架构决策记录（ADR）

| ADR | 主题 | 状态 |
|---|---|---|
| [0001](docs/adr/0001-build-strategy-and-core-crates.md) | Build Strategy & Core Crates | Accepted |
| [0002](docs/adr/0002-mcp-transport-stdio-only.md) | MCP Transport: stdio only | Accepted |
| [0003](docs/adr/0003-output-buffer-strategy.md) | Output Buffer Strategy | Accepted |
| [0004](docs/adr/0004-remote-persistent-runtime.md) | Remote Persistent Runtime Architecture | Accepted |
| [0005](docs/adr/0005-security-model.md) | Security Model | Accepted (Amended by 0009) |
| [0006](docs/adr/0006-openssh-config-via-ssh-g.md) | OpenSSH Config via `ssh -G` | Accepted |
| [0007](docs/adr/0007-proxyjump-strategy.md) | ProxyJump Strategy | Accepted |
| [0008](docs/adr/0008-scope-boundary.md) | Scope Boundary | Accepted |
| [0009](docs/adr/0009-bootstrap-host-and-credential-provider.md) | bootstrap_host + CredentialProvider | Accepted |

### 项目结构（概览）

```text
TermBridge/
├── src/                              # 主 crate
│   ├── domain/                       # 领域抽象（CredentialProvider / Provider / Session / Timeline）
│   ├── application/                  # 业务逻辑（BootstrapHost / Sessions / Hosts）
│   ├── infrastructure/               # 基础设施（SSH / SFTP / Credential / DaemonProto）
│   └── transport/mcp/                # MCP server（rmcp）
├── crates/
│   └── termbridge-credential-prompt/ # 独立密码 prompt helper（跨平台）
├── agentd/                           # 远端 daemon（Linux only）
├── cli/                              # 测试用 CLI client
└── docs/adr/                         # 架构决策记录
```

## 路线图

| Phase | 主题 | 状态 |
|---|---|---|
| Phase 0 | 原型验证（MCP / SSH PTY / ssh config） | ✅ 完成 |
| Phase 1 | Interactive Session（SSH + PTY + SFTP 基础） | ✅ 完成 |
| Phase 2 | SFTP 扩展（mkdir / list / remove / chmod） | ✅ 完成 |
| Phase 3 | Remote Persistent Runtime（daemon + detach/attach） | ✅ 完成 |
| Phase 4 | Observability（Timeline） | ✅ 完成 |
| Phase 5 | Remote Workspace（SFTP 目录递归 + 环境检测） | ✅ 完成 |
| ADR-0009 | bootstrap_host + CredentialProvider | ✅ 完成 |
| 未来 | Local / Docker / WSL Provider | 规划中 |

> **边界声明**（ADR-0008）：TermBridge 是 Remote Terminal Runtime，不是 AI Ops Platform。不负责 config validation / playbook / service orchestration / desired state。编排层属未来独立项目。

## 许可证

待定（项目当前 `publish = false`）。
