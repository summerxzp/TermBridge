# TermBridge MCP 配置模板

TermBridge MCP server 通过 stdio 通信，所有 MCP 客户端配置格式一致。

## 前置条件

- `termbridge-mcp` 和 `termbridge-auth-helper` 在同一目录，且该目录在 `PATH` 中
- 或者使用绝对路径（见下方"绝对路径配置"）

## 标准配置（推荐，PATH 方式）

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

## 绝对路径配置（开发 / 测试用）

```json
{
  "mcpServers": {
    "termbridge": {
      "command": "e:\\Code\\TermBridge\\target\\release\\termbridge-mcp.exe",
      "args": []
    }
  }
}
```

> 注意：使用绝对路径时，`termbridge-auth-helper` 仍需在 `termbridge-mcp` 同目录，
> 或通过环境变量 `TERMBRIDGE_AUTH_HELPER` 指定路径。

## 客户端特定配置文件位置

| 客户端 | 配置位置 |
|--------|---------|
| Claude Code | `~/.claude/claude_desktop_config.json` 或项目 `.mcp.json` |
| Codex | 项目 `.mcp.json` |
| OpenCode | 项目 `.mcp.json` 或 `~/.config/opencode/mcp.json` |

## 验证

配置完成后重启客户端，应能看到 TermBridge 工具列表（`list_hosts`、`open_session`、`send_input` 等）。

首次连接某主机时：
1. Agent 调用 `list_hosts` → 确认主机可见
2. Agent 调用 `bootstrap_host(host)` → 弹出 `termbridge-auth-helper` 凭据窗口输入密码（一次性）
3. 后续连接直接 `open_session(host)` → SSH key 认证，无需密码

## 环境变量

| 变量 | 作用 | 默认 |
|------|------|------|
| `TERMBRIDGE_AUTH_HELPER` | 自定义 auth-helper 路径 | 与 termbridge-mcp 同目录 |
| `TERMBRIDGE_ALLOWED_LOCAL_PATHS` | SFTP 本地路径白名单（`;` 或 `:` 分隔） | cwd + `$TEMP/termbridge` |
| `RUST_LOG` | 日志级别（stderr） | `info,termbridge=debug` |

**`TERMBRIDGE_ALLOWED_LOCAL_PATHS`**：让宿主把 workspace 显式传入 SFTP 白名单。
默认 `[cwd, $TEMP/termbridge]`，但 MCP server 作为 IDE 子进程启动时 cwd 不可控，
建议在 MCP 配置中显式设置 workspace 路径：

```json
{
  "mcpServers": {
    "termbridge": {
      "command": "termbridge-mcp",
      "args": [],
      "env": {
        "TERMBRIDGE_ALLOWED_LOCAL_PATHS": "e:\\Code\\TermBridge;C:\\Users\\SUMMER\\tmp"
      }
    }
  }
}
```
