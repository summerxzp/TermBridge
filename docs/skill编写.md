我会把你现在的状态定义为：**TermBridge 已经从“开发验证阶段”进入“产品化收尾 / dogfooding 阶段”**。

从你给的结果看，下一步继续堆底层功能的收益已经明显下降。尤其是：

- Runtime Contract：已冻结
- Agent Terminal Protocol：已冻结
- Provider API：已冻结
- Execution State / recovery：33/33
- Attach / cursor：8/8
- Cross-restart：5/5
- CLI + T16：完成
- Windows / Linux / macOS：构建验证
- GUI：完成
- MCP resize：完成

所以我赞同你的判断：**现在最值得做的不是继续开发功能，而是把“怎么让 Agent 正确使用 TermBridge”以及“安装后马上能用”做好。**

------

# 下一阶段我建议：Phase 8 = Adoption / Bootstrap

可以非常克制地分成三件事：

```text
Phase 8
│
├── 8-A  Agent Skill
│
├── 8-B  Bootstrap / 开箱即用配置
│
└── 8-C  Dogfooding + 小范围修 bug
```

而不是马上继续做 Playbook、复杂 GUI 功能之类。

------

## 1. Skill 是现在最重要的东西

因为你已经把：

> “TermBridge 能做什么”

解决了。

现在真正的问题变成：

> **Claude Code / Codex / OpenCode 怎么正确使用它？**

尤其你已经有 ADR-0013，这实际上已经是 Skill 的技术基础。

我建议 Skill 不要把 ADR-0012/0013 原封不动塞进去。

应该把它变成一个**非常短的 Agent 操作手册**。

例如 Agent 看到的核心逻辑应该类似：

```text
# TermBridge

Use TermBridge when working on a remote Linux host.

## Basic workflow

1. list_hosts
2. open/connect session
3. send_input
4. read_output
5. use completion marker for non-interactive commands
6. use send_control for interactive programs
7. reconnect after disconnect
8. never blindly retry an unknown execution

## Command completion

For commands that must report completion:

command; printf '\n__TB_DONE__:%s:%s\n' "$REQID" "$?"

Do not use:
command && echo DONE

## Timeout

timeout != failure.

After timeout:
- continue waiting, or
- explicitly interrupt

Never automatically retry.

## Interactive programs

Do not use completion markers with:
- vim
- top
- htop
- tail -f
- interactive shells

Use interactive input/control instead.
```

也就是说：

**ADR 是给开发者看的，Skill 是给 Agent 行为约束看的。**

这个区别很重要。

------

# 2. Skill 最好不要过度依赖具体 Agent

你现在支持的消费者可能有：

- Claude Code
- Codex
- OpenCode
- 其他 MCP Agent

所以我建议：

```text
docs/
    adr/
        ADR-0012...
        ADR-0013...

skills/
    termbridge/
        SKILL.md
```

`SKILL.md` 是：

> **Agent Terminal Protocol 的 operational version**

而不是：

> TermBridge 项目的开发文档。

这样以后换 Agent 不需要重新设计协议。

------

# 3. 我尤其建议 Skill 加一个“决策表”

这是实际使用中非常有价值的。

例如：

| 情况                    | Agent 应该怎么做                 |
| ----------------------- | -------------------------------- |
| 普通命令                | `send_input` + completion marker |
| 命令需要实时观察        | `send_input` + `read_output`     |
| 长时间运行              | 不因 timeout 重试                |
| timeout                 | 继续等待 / Ctrl+C                |
| sudo                    | 遵循 confirmation policy         |
| vim/top                 | 不使用 completion marker         |
| SSH 断线                | 状态视为 UNKNOWN                 |
| 重新执行有副作用        | 先 reconnect + 幂等检查          |
| 已有 persistent session | attach                           |
| 需要完整输出            | `since_cursor`                   |
| 只等事件                | `wait_for`                       |

这比几十页说明文字更有用。

------

# 4. “开箱即用”其实比继续开发 GUI 更值得做

你现在已经有：

```text
Windows
Linux
macOS

       ↓

termbridge
termbridge-auth-helper
```

但真正的开箱即用体验应该是：

```text
安装
 ↓
termbridge hosts
 ↓
发现/读取 ~/.ssh/config
 ↓
connect xxx
 ↓
需要密码
 ↓
auth-helper
 ↓
bootstrap_host
 ↓
后续直接 key authentication
```

这条链路应该成为**官方 happy path**。

------

# 5. 我建议做一个 Bootstrap Bundle

甚至可以把周边配置统一成：

```text
TermBridge
├── Binary
├── Auth Helper
├── MCP config
├── Skill
└── SSH bootstrap
```

用户最终只需要：

```text
安装 TermBridge
+
配置一个 MCP server
+
安装 Skill
```

然后 Agent 就能：

```text
“帮我在 ubuntu-test 上部署 xxx”
```

直接开始工作。

这才是这个项目真正的价值闭环。

------

# 6. MCP 配置也应该标准化

建议最终提供一个官方模板，例如：

```json
{
  "mcpServers": {
    "termbridge": {
      "command": "termbridge",
      "args": ["mcp"]
    }
  }
}
```

当然具体字段以你现在实际 CLI/MCP 入口为准。

重点是：

**不要让用户自己研究 Rust binary、路径、helper、环境变量。**

可以直接提供：

```text
examples/
    mcp/
        claude-code.json
        codex.json
        opencode.json
```

以及：

```text
docs/
    getting-started.md
```

------

# 7. 我甚至建议提供一个 `termbridge doctor`

这个东西对正式使用价值很高，而且实现成本不大。

例如：

```bash
termbridge doctor
```

输出：

```text
TermBridge Doctor

✓ TermBridge runtime
✓ SSH config
✓ SSH agent
✓ Credential helper
✓ MCP server
✓ Host configuration
✓ PTY support
✓ Terminal size
✓ Provider: Persistent
```

如果：

```text
✗ termbridge-auth-helper
```

直接告诉用户：

```text
Credential helper is not executable.
Run: ...
```

这会显著降低你以后自己排查问题的成本。

------

# 8. GUI 暂时真的可以停了

你现在 GUI MVP 已经完成，我反而建议**不要马上继续加 GUI 功能**。

目前：

```text
Hosts
Sessions
Terminal
Attach
Detach
Resize
```

已经足够。

等自己真正用了几天/几周，再记录：

> “这个操作每次都很烦。”

然后才加功能。

否则很容易进入：

```text
GUI
 ↓
tab
 ↓
file browser
 ↓
search
 ↓
command history
 ↓
notification
 ↓
settings
 ↓
...
```

最后变成一个完全不同的产品。

------

# 9. 正式使用前，我建议只再补一轮“真实使用测试”

不是再做几十个 P0。

而是直接拿 TermBridge 做你原来真实的工作：

```text
Windows Agent
       ↓
TermBridge
       ↓
Linux
       ↓
拉代码
       ↓
修改
       ↓
编译
       ↓
运行
       ↓
观察日志
       ↓
调试
       ↓
重启
       ↓
SSH断线
       ↓
reconnect
       ↓
继续工作
```

尤其测试这些：

### A. 长任务

```bash
cargo build
npm run build
docker compose up
```

### B. 交互

```bash
sudo
vim
top
```

### C. 长时间运行

```bash
journalctl -f
tail -f
server
```

### D. Agent 最容易犯错的地方

```text
timeout
disconnect
unknown execution
重复执行
session attach
```

这轮不需要追求“测试数量”。

追求的是：

> **我自己连续使用一周，会不会觉得它烦。**

------

# 10. 我会把 Phase 8 定成这样

```text
Phase 8 — Production Readiness

8-A Agent Skill
    ├── Agent Terminal Protocol operational guide
    ├── command completion rules
    ├── timeout / disconnect rules
    ├── interactive program rules
    ├── cursor usage
    └── idempotency patterns

8-B Bootstrap
    ├── MCP config templates
    ├── Skill installation
    ├── SSH bootstrap happy path
    ├── auth-helper packaging
    ├── doctor
    └── getting-started

8-C Dogfooding
    ├── 真实开发任务
    ├── 长任务
    ├── 交互程序
    ├── disconnect/reconnect
    └── 收集问题

8-D Freeze
    ├── runtime API freeze
    ├── MCP schema freeze
    ├── Skill v1
    └── release
```

然后才是：

```text
Phase 9+
    Playbook
    高级 GUI
    更多 Provider
    自动化
    ...
```

------

## 一个很重要的判断

你现在其实已经完成了一个比较容易被低估的事情：

**TermBridge 的核心已经不是“SSH MCP server”了。**

它现在更接近：

> **一个给 AI Agent 提供持久、可恢复、具有明确执行语义的远程 Terminal Runtime。**

MCP 只是其中一个入口。

所以现在最应该保护的，恰恰是你已经冻结的东西：

```text
Runtime Contract
       ↓
Provider API
       ↓
Agent Terminal Protocol
       ↓
Skill
       ↓
CLI / GUI / MCP
```

**不要为了做 Skill 又去修改 Runtime。**

如果实际使用发现 Skill 不够好，优先修改 Skill；只有发现 Skill 无法表达某个真实需求，才回头修改 Protocol。

这会让 TermBridge 从“一个开发了很多功能的项目”真正变成“一个可以稳定投入日常使用的工具”。