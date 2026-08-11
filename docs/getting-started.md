# Getting Started

从零到第一个远程命令，5 分钟完成。

## 1. 安装

### 方式 A：预构建二进制（推荐）

从 [Releases](../../releases) 下载对应平台压缩包，解压后得到三个二进制：

| 文件 | 用途 |
|------|------|
| `termbridge-mcp` | MCP server（AI Agent 入口） |
| `termbridge` | 人类管理员 CLI |
| `termbridge-auth-helper` | 首次认证密码输入 helper |

将三个二进制放到同一目录，并把该目录加入 `PATH`。

### 方式 B：从源码构建

```bash
git clone <repo-url> TermBridge
cd TermBridge
cargo build --release
```

产物在 `target/release/`，把该目录加入 `PATH`，或复制到已有 PATH 目录。

> macOS / Linux 产物无 `.exe` 后缀，使用方式相同。

## 2. 验证安装

```bash
termbridge-mcp --version
termbridge --version
```

两个命令都能输出版本号即安装成功。`termbridge-auth-helper` 不接受参数，无法直接验证，会在首次 bootstrap 时自动调用。

## 3. 配置 SSH Host

在 `~/.ssh/config` 中添加目标服务器：

```sshconfig
Host my-server
    HostName 192.168.1.171
    User root
    Port 22
    IdentityFile ~/.ssh/id_ed25519
```

验证 TermBridge 能识别：

```bash
termbridge hosts
# 应输出：my-server  192.168.1.171
```

> 如果没有 `~/.ssh/id_ed25519`，`bootstrap_host` 会自动生成。

## 4. 配置 MCP 客户端

将 TermBridge 注册到你的 AI 客户端。配置模板见 [`examples/mcp/`](../examples/mcp/)。

**标准配置**（PATH 方式，推荐）：

```json
{
  "mcpServers": {
    "termbridge": {
      "command": "termbridge-mcp",
      "args": []
    }
  }
}
```

**客户端配置文件位置**：

| 客户端 | 位置 |
|--------|------|
| Claude Code | `~/.claude/claude_desktop_config.json` 或项目 `.mcp.json` |
| Codex | 项目 `.mcp.json` |
| OpenCode | 项目 `.mcp.json` 或 `~/.config/opencode/mcp.json` |
| Trae | 项目 `.mcp.json` |

配置后重启客户端，应能看到 `list_hosts`、`open_session`、`send_input` 等工具。

## 5. 安装 Agent Skill（可选但推荐）

将 [`skills/termbridge/SKILL.md`](../skills/termbridge/SKILL.md) 注册到 AI 客户端的 skill 目录。Skill 包含 Agent 操作 TermBridge 的 7 条规则、决策表和反模式，能让 Agent 首次接触就正确使用。

具体注册方式取决于客户端：
- **Trae / Claude Code**：放到 `.trae/skills/termbridge/SKILL.md` 或客户端 skill 目录
- **其他**：参考客户端文档

## 6. 首次连接（Happy Path）

第一次连接某主机时，需要部署 SSH 公钥。让 Agent 调用 `bootstrap_host`：

```
你: 帮我连接 my-server

Agent: list_hosts → 确认 my-server 可见
Agent: bootstrap_host(host="my-server")
       → 桌面弹出 termbridge-auth-helper 凭据窗口
       → 用户输入密码
       → 公钥部署 + key 认证验证
       → 返回 bootstrapped

Agent: open_session(host="my-server", persistent=true, name="work")
       → 返回 session_id

Agent: send_input + read_output 执行命令
```

之后所有连接直接 `open_session`，SSH key 免密，不再需要密码。

## 7. 验证可用

让 Agent 执行一个简单命令：

```
你: 在 my-server 上执行 whoami

Agent: open_session(host="my-server")
       send_input(session_id, "whoami\n")
       read_output(session_id, wait_for="$")  # 等 prompt
       close_session(session_id)
```

输出 `root`（或对应用户名）即表示链路通畅。

## 常见问题

### `termbridge-auth-helper` 找不到

确保它与 `termbridge-mcp` 在同一目录，或设置环境变量：

```bash
# Windows PowerShell
$env:TERMBRIDGE_AUTH_HELPER = "C:\path\to\termbridge-auth-helper.exe"

# Linux / macOS
export TERMBRIDGE_AUTH_HELPER=/path/to/termbridge-auth-helper
```

### `bootstrap_host` 返回 `bootstrap_failed`

公钥已部署但 key 重连验证失败。检查远端：

- `sshd_config` 中 `PubkeyAuthentication` 是否为 `yes`
- `~/.ssh` 权限是否为 `700`，`~/.ssh/authorized_keys` 是否为 `600`
- SELinux / home 目录权限

### `list_hosts` 为空

检查 `~/.ssh/config` 是否存在且包含 `Host` 条目。TermBridge 通过 `ssh -G` 解析，不支持 Match 指令。

### MCP 客户端看不到工具

- 确认 `termbridge-mcp` 在 `PATH` 中（终端运行 `termbridge-mcp --version` 应成功）
- 或使用绝对路径配置（见 [`examples/mcp/README.md`](../examples/mcp/README.md)）
- 重启 MCP 客户端

## 下一步

- 阅读 [SKILL.md](../skills/termbridge/SKILL.md) 了解 Agent 操作规则
- 阅读 [ADR-0013](adr/0013-agent-terminal-protocol.md) 了解 Protocol 设计动机
- 使用 `persistent=true` 体验跨 MCP 重启的 session 保活
