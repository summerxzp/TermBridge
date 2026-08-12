<div align="center">

# TermBridge

**Remote Terminal Runtime for AI Agents**

为 AI Agent 提供持久、可恢复、具有明确执行语义的远程终端运行时

[![release](https://img.shields.io/github/v/release/summerxzp/TermBridge?label=release&colorB=blue)](https://github.com/summerxzp/TermBridge/releases/latest)
[![license](https://img.shields.io/badge/license-MIT-green)](LICENSE)
[![platform](https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-blue)](https://github.com/summerxzp/TermBridge)

**简体中文** | [English](README.en.md)

</div>

---

TermBridge 是一个 MCP（Model Context Protocol）server，通过 stdio 与 AI 客户端（TraeCode / Claude Code / Codex / OpenCode 等）通信。它不是 Ansible、不是 playbook 引擎，而是给 Agent 提供一个**稳定、可编程、安全**的远程 Terminal Runtime。

## 核心特性

- **纯 SSH 优先**：默认通过系统 `~/.ssh/config` + SSH Agent / IdentityFile 连接，远端零安装
- **首次连接引导**：`bootstrap_host` 工具通过平台原生凭据对话框一次性输入密码，自动部署公钥，后续全部免密（密码永不进入 LLM context）
- **完整 Terminal 语义**：PTY byte stream + cursor + `wait_for` + 4 种读取模式，不是屏幕快照
- **SFTP 文件/目录传输**：单文件原子写、目录递归、路径策略防越界
- **Persistent Runtime（可选）**：opt-in 部署远端 daemon，支持 session 跨 MCP 重启保活、detach/attach
- **Agent Terminal Protocol**：ADR-0013 定义的 7 条规则，确保 Agent 正确处理 completion / timeout / disconnect / TUI
- **跨平台**：Windows / Linux / macOS 预构建二进制（macOS Apple Silicon 原生，Intel Mac 可走 Rosetta 2），CLI + GUI + MCP 三种消费者
- **安全边界严格**：host key 严格校验、密码脱敏日志、SFTP 路径策略、凭据不进 MCP schema

## 安装

### 方式一：下载预构建二进制（推荐）

从 [GitHub Release](https://github.com/summerxzp/TermBridge/releases/latest) 下载对应平台的压缩包：

| 平台 | 文件 |
|------|------|
| Windows x86_64 | `termbridge-windows-x86_64.zip` |
| Linux x86_64 | `termbridge-linux-x86_64.tar.gz` |
| macOS Apple Silicon (M1/M2/M3...) | `termbridge-macos-arm64.tar.gz` |

> macOS Intel 用户：暂无预构建包，可通过 Rosetta 2 运行 arm64 版本，或自行 `cargo build --release`。

解压后包含：

```
termbridge-v<version>-<platform>-<arch>/
├── termbridge-mcp            MCP server 主入口
├── termbridge                CLI（人类管理员工具，可选）
├── termbridge-auth-helper    凭据 helper（必须与 mcp 同目录）
├── mcp-config.json           MCP 配置模板
├── SKILL.md                  Agent Skill
├── README.txt                快速开始
└── resources/agentd/
    └── linux-x86_64/         远端 daemon（bootstrap_host 自动部署，无需手动操作）
```

### 方式二：从源码构建

**前置要求**：
- Rust toolchain（stable，建议 1.75+）
- Windows / Linux / macOS 均可编译（agentd crate 仅 Linux）
- 系统已安装 OpenSSH 并配置 `~/.ssh/config`

```powershell
git clone https://github.com/summerxzp/TermBridge.git
cd TermBridge
cargo build --release -p termbridge -p termbridge-auth-helper --bins
```

## 快速开始

### 第 1 步：配置 SSH Host

在 `~/.ssh/config` 中添加目标服务器：

```sshconfig
Host my-server
    HostName 192.0.2.10
    User root
    Port 22
    IdentityFile ~/.ssh/id_ed25519
```

### 第 2 步：接入 MCP 客户端

将 `mcp-config.json` 导入你的 AI 客户端，或手动配置：

```json
{
  "mcpServers": {
    "termbridge": {
      "command": "/path/to/termbridge-mcp",
      "args": []
    }
  }
}
```

> `termbridge-auth-helper` 必须与 `termbridge-mcp` 在同一目录，否则首次认证会 fallback 到不支持状态。

### 第 3 步：安装 Agent Skill

将 [SKILL.md](skills/termbridge/SKILL.md) 安装到你的 AI Agent 的 skill 目录，确保 Agent 遵守 Agent Terminal Protocol（ADR-0013）。

### 第 4 步：首次连接新服务器

如果目标服务器尚未部署你的 SSH 公钥，让 Agent 调用 `bootstrap_host`：

```
Agent: 我需要连接 my-server，请先 bootstrap。
 工具调用: bootstrap_host({ "host": "my-server" })
 桌面弹出原生凭据输入框
 用户输入密码
 TermBridge 部署公钥 + 验证 key 认证
← 返回: { "status": "bootstrapped", "authentication": "public_key" }
```

之后所有 `open_session` 调用均使用 SSH key 免密连接。

> 完整流程见 [docs/getting-started.md](docs/getting-started.md)。

## 工具列表（20 个）

TermBridge 向 MCP 客户端暴露 20 个工具，按功能分类：

### Host 管理

| 工具 | 说明 |
|---|---|
| `list_hosts` | 列出 `~/.ssh/config` 中所有 Host 别名及 hostname |

### Session 生命周期

| 工具 | 参数 | 说明 |
|---|---|---|
| `open_session` | `host`, `persistent?` | 建立 SSH + PTY session，返回 `session_id`。`persistent=true` 启用远端 daemon 跨重启保活 |
| `send_input` | `session_id`, `data` | 发送文本到 PTY stdin（`\n` 为回车，立即发送不等命令完成） |
| `read_output` | `session_id`, `wait_for?`/`tail_lines?`/`since_cursor?`, `timeout_secs?`, `strip_ansi?` | 读取 PTY 输出，支持 4 种模式：settle / wait_for / tail_lines / since_cursor。`strip_ansi=true` 剥离终端控制序列（CSI/OSC/DCS），RingBuffer 保留 raw bytes 不变。返回含 `session_state` 字段（`ready`/`lost`/`closing`/`closed`），Agent 据此感知断线 |
| `send_control` | `session_id`, `control_key` | 发送控制键：`ctrl+c` / `ctrl+d` / `ctrl+z` / `tab` / `enter` / `escape` |
| `close_session` | `session_id` | 关闭 session（幂等），释放 SSH channel |
| `reconnect_session` | `session_id` | 重连 Lost 状态的 session：重建 SSH + PTY，复用原 session_id。buffer 历史不保留。仅交互式 session 支持（persistent session 用 attach_remote_session） |
| `resize` | `session_id`, `cols`, `rows` | 调整 PTY 尺寸（window_change），支持 TUI 程序随窗口重绘 |

### SFTP 文件操作

| 工具 | 说明 |
|---|---|
| `sftp_transfer` | 单文件 upload / download（download 用原子写：temp + fsync + rename） |
| `sftp_mkdir` | 创建远端目录（mode 为八进制字符串如 `"755"`） |
| `sftp_list` | 列出远端目录内容（名称/类型/大小/权限） |
| `sftp_remove` | 删除远端文件或目录（`recursive=true` 删目录树，系统目录受保护） |
| `sftp_chmod` | 修改远端文件/目录权限（mode 为八进制字符串如 `"644"`） |
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
| `bootstrap_host` | 部署 SSH 公钥到远端 `authorized_keys`（详见下方专章） |

## bootstrap_host 详解

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
弹出凭据输入（Windows CredUI / POSIX tty）
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
| **密码经独立通道** | 凭据对话框由 `termbridge-auth-helper` 独立进程处理，IPC 与 MCP stdio 完全隔离 |
| **密码不持久化** | 认证后立即 `Zeroize`，不写入文件/日志/环境变量 |
| **host key 严格校验** | 复用 ADR-0005 known_hosts 机制，不自动接受变更 |
| **公钥部署幂等** | 部署前检查公钥是否已存在，避免重复写入 |
| **重连验证** | 部署公钥后必须重新用 key 认证成功，才返回 `bootstrapped` |

### 返回状态

| status | 含义 |
|---|---|
| `already_configured` | 已有可用 key 认证（SSH Agent 或 IdentityFile），无需 bootstrap |
| `bootstrapped` | 密码认证 + 公钥部署 + key 验证均成功 |
| `cancelled` | 用户取消密码输入 |
| `authentication_failed` | 密码错误 |
| `bootstrap_failed` | 公钥已部署但 key 重连验证失败（检查 sshd 配置 / 权限 / SELinux） |

## SSH Config 配置建议

TermBridge 通过 `ssh -G` 解析系统 `~/.ssh/config`，支持常用指令：

```sshconfig
# 基本配置
Host prod-server
    HostName 192.0.2.10
    User root
    Port 22
    IdentityFile ~/.ssh/id_ed25519

# 通过跳板机
Host bastion-prod
    HostName 203.0.113.50
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
| **SFTP 路径策略** | 本地路径白名单 `[cwd, $TEMP/termbridge]` + 环境变量 `TERMBRIDGE_ALLOWED_LOCAL_PATHS` 追加；远端路径 realpath 解析防 `../` 越界（ADR-0005 §4） |
| **下载原子写** | 临时文件 + fsync + rename，避免半写文件被误读 |

## 消费者

TermBridge Runtime 支持三种消费者，全部遵守 Agent Terminal Protocol（ADR-0013）：

| 消费者 | 入口 | 适用场景 |
|--------|------|----------|
| **MCP** | `termbridge-mcp` | AI Agent（TraeCode / Claude Code / Codex / OpenCode）|
| **CLI** | `termbridge` | 人类管理员，raw mode PTY，支持 vim/top/htop |
| **GUI** | Tauri v2 + React + xterm.js | 可视化终端，Host/Session 管理 |

## 架构决策记录（ADR）

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
| [0010](docs/adr/0010-session-reconnect.md) | Session 断线感知 + 手动重连 | Accepted |
| [0011](docs/adr/0011-input-semantics-and-execution-safety.md) | send_input 语义 + 执行安全 | Accepted |
| [0012](docs/adr/0012-execution-state-and-completion-protocol.md) | 执行语义契约（9 大契约） | Accepted |
| [0013](docs/adr/0013-agent-terminal-protocol.md) | Agent Terminal Protocol（7 条规则） | Accepted |
| [0014](docs/adr/0014-phase7-consumer-roadmap.md) | Phase 7 消费者路线图 | Accepted |
| [0015](docs/adr/0015-provider-api-freeze.md) | Provider API 冻结 | Accepted |
| [0016](docs/adr/0016-runtime-freeze.md) | Runtime Freeze | Accepted |

## 项目结构

```text
TermBridge/
├── src/                              # 主 crate
│   ├── domain/                       # 领域抽象（CredentialProvider / Provider / Session / Timeline）
│   ├── application/                  # 业务逻辑（BootstrapHost / Sessions / Hosts）
│   ├── infrastructure/               # 基础设施（SSH / SFTP / Credential / DaemonProto）
│   ├── transport/mcp/                # MCP server（rmcp）
│   └── bin/termbridge.rs             # 人类管理员 CLI
├── crates/
│   └── termbridge-auth-helper/       # 独立凭据 helper（跨平台）
├── agentd/                           # 远端 daemon（Linux only）
├── gui/                              # Tauri v2 + React + xterm.js
├── skills/termbridge/SKILL.md        # Agent Skill
├── examples/mcp/                     # MCP 配置模板
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
| Phase 6 | Execution State + Reconnect + Agent Terminal Protocol | ✅ 完成 |
| Phase 7 | CLI + 跨平台 + GUI + Provider API Freeze | ✅ 完成 |
| Phase 8 | Adoption / Bootstrap（Skill + 开箱即用 + Dogfooding） | ✅ 完成 |
| [ADR-0016](docs/adr/0016-runtime-freeze.md) | **Runtime Freeze** | ✅ 冻结 |
| 未来 | Local / Docker / WSL Provider、Playbook、高级 GUI | 规划中 |

> **边界声明**（ADR-0008）：TermBridge 是 Remote Terminal Runtime，不是 AI Ops Platform。不负责 config validation / playbook / service orchestration / desired state。编排层属未来独立项目。

## 验证矩阵

| 测试套件 | 结果 |
|---|---|
| P0 执行语义（ADR-0012） | 33/33 ✅ |
| T17 attach/cursor 边界 | 8/8 ✅ |
| Cross-restart E2E | 5/5 ✅ |
| T16 resize | 6/6 ✅ |
| 单元测试 | 256/256 ✅ |

## 许可证

[MIT](LICENSE)
