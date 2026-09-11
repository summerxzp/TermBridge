# ADR-0019：Unix Credential Prompt 多级 fallback 与协议 v2

- **Status**: Accepted
- **Date**: 2026-09-10
- **Phase**: 0.3.2
- **Supersedes**: —
- **Depends on**: [ADR-0009](0009-bootstrap-host-and-credential-provider.md)（Credential Provider / helper 进程架构）、[ADR-0017](0017-host-connection-policy.md)（auth=password 路径）
- **Amends**: ADR-0009 §「macOS/Linux prompt 暂未实现」约束（本 ADR 补全 Unix 实现）

## 1. Context

### 1.1 现状：Linux/macOS 只有 `/dev/tty` 一条路

ADR-0009 落地时 Unix 平台的 helper 实现是直接 `open("/dev/tty")` 读密码。
这在两类真实场景下失败或体验劣化：

1. **GUI 客户端（VSCode / Trae / Cursor）spawn MCP server 时没有控制终端**：
   `/dev/tty` 打开失败 → `Unsupported` → 更糟的是旧协议把所有平台错误
   折叠成 `cancelled`，Agent 告诉用户「你取消了」，而用户根本没见过任何
   输入框。
2. **CLI 客户端（终端里的 Claude Code 等 TUI）**：宿主 TUI 自己在读终端
   stdin，helper 同时去读 `/dev/tty`——两者是同一个终端设备的输入源，
   不存在天然隔离。密码字符可能被 TUI 吃掉，helper 的 prompt 文本会
   破坏 TUI 画面。

### 1.2 约束（继承 ADR-0009）

- MCP server 的 stdin/stdout 是 JSON-RPC 通道，stderr 不可靠，均不能用于交互
- 密码不能进 MCP tool arguments / LLM context
- Core 不 import 任何平台 UI crate（`windows` / `cocoa` / `x11`）
- 凭据输入必须隔离在 `termbridge-auth-helper` 独立进程中

### 1.3 评审收敛的决策（两轮外部评审）

本 ADR 的方案经两轮评审收敛，关键裁定：

| 开放问题 | 裁定 |
|---|---|
| `SSH_ASKPASS` 是否直接采用 | **否**。OpenSSH 契约是「有 TTY 就不用 askpass」，而 TermBridge 因宿主 TUI 占着 TTY 恰恰要 askpass 优先——同名变量、相反触发条件会给设置过 ksshaskpass 的用户带来与其 ssh 经验相反的行为。v1 只读自有变量 `TERMBRIDGE_ASKPASS`，语义自洽；`SSH_ASKPASS` 留待有真实用户诉求时带显式偏差说明再加入 |
| 级联信号如何区分「环境失败」与「用户取消」 | 见 §2.2。防止「用户点一次取消 → 连弹三个对话框」 |
| GUI 检测能否信任 `$DISPLAY` | 否。DISPLAY 存在 ≠ GUI 可用（X forwarding 失效 / 残留 env）。DISPLAY 只作启发式预检，真正判定靠执行后的级联信号 |
| zenity/kdialog/yad 是否打包 | 不打包、不安装、不声明依赖。PATH 找到才用（软依赖），TermBridge 保持单二进制 |
| Core 层是否拆 provider trait 层级（SecureStore / InteractivePrompt） | 否。fallback 链整体收敛在 helper 进程内部，Core 仍只见 `CredentialProvider` trait + 四态 JSON 响应。Core 层 trait 拆分是 YAGNI |

## 2. Decision

### 2.1 原则：优先选择不干扰宿主 Agent 交互通道的输入方式

不是「GUI 优先于 TTY」，而是按**干扰风险**排序：

```text
独立 GUI（zenity/kdialog/yad/osascript）   ✅ 不碰宿主终端
独立 askpass 程序                          ✅ 自带 UI 或通道
闲置 TTY（headless 服务器）                 ⚠️ 接受残余风险（见 §2.6）
被宿主 TUI 占用的 TTY                      ❌ 从 helper 视角不可检测
```

Unix fallback 链（Linux；macOS 的 GUI 层为 osascript）：

```text
1. TERMBRIDGE_ASKPASS（显式配置；终态语义，失败不级联）
2. GUI：zenity → kdialog → yad（PATH 软依赖，逐个尝试）
3. TTY：/dev/tty（headless 兜底）
4. Unsupported（可行动错误，见 §2.5）
```

**askpass 契约**：prompt 文本作为 `$1`，密码写 stdout，exit 0 = 成功，
非零 + 空 stderr = 用户取消（x11-ssh-askpass 等惯例），非零 + 非空
stderr = 程序损坏 → `failed`（用户显式配置的程序出问题必须上报，
静默级联等于无视用户意图）。

**GUI 工具调用**（均实测或按官方文档核实）：

| 工具 | 调用 | 输出 | 用户取消 |
|---|---|---|---|
| zenity | `--password --username --text <p> --title <t>` | `user\|password`（`\|` 分隔，官方示例 `cut -d'|'`） | exit 1 |
| kdialog | `--password <p> --title <t>` | 密码（stdout 单行） | exit 1 |
| yad | `--entry --hide-text --text <p> --title <t>` | 密码（stdout 单行） | exit 1；Esc/关窗 = 252 |
| osascript (macOS) | `-e 'return text returned of (display dialog … with hidden answer)'` | 密码（stdout） | error -128 → exit 1 + stderr "User canceled" |

zenity 是唯一支持用户名编辑的（`--username`），解析后回传 `user` 字段
（与 Windows CredUI 语义对齐）；其余 provider 回 `null`，Core 回退
ssh config 预填用户名。

### 2.2 级联信号：环境失败 vs 用户取消

防止「用户点一次取消 → zenity/kdialog/yad 连弹三个框」的核心规则：

```text
级联到下一 provider，当且仅当对话框从未展示给用户：
  - spawn ENOENT（二进制不在 PATH）
  - stderr 含环境失败特征（cannot open display / could not connect
    to display / no protocol specified …）
  - 无 stdout、无环境特征、且 <2s 快速退出（对话框来不及展示）

视为用户取消（停止级联）：
  - exit 0 + 空 stdout（空提交）
  - 任何「对话框已展示后」的非零退出（含用户取消）
  - stderr 含用户取消特征（user canceled，优先于快速退出启发——
    osascript 快速点取消时不能被误判为环境失败）
  - yad 的 Esc 关窗码 252（无条件）
  - 非零 + stdout 非空（异常状态，输出可能含密码，保守丢弃不解析）
```

2s 阈值的依据：GTK/Qt 初始化 ~0.5s，坏 DISPLAY 实测 44ms 退出；
人类阅读 + 点击取消不可能 <2s 完成。

`$DISPLAY` / `$WAYLAND_DISPLAY` 仅作**启发式预检**（都未设置时跳过
GUI 层直接落 TTY，省 3 次 spawn）；真正的可用性判定靠上述执行后信号。

### 2.3 协议 v2：helper 响应四态

```json
{"type":"password","value":"…","user":"alice"|null}
{"type":"cancelled"}
{"type":"unsupported","message":"…(tried: …). Options: (1)…(2)…(3)…"}
{"type":"failed","message":"TERMBRIDGE_ASKPASS program '…' failed: …"}
```

- `unsupported` ≠ `cancelled`：前者是环境问题（Agent 应转述三条出路），
  后者是用户意志（Agent 应询问是否重试）
- `failed` 仅用于显式配置的 provider 损坏（askpass 程序无法执行 / 非零
  退出且带 stderr）
- v1 响应（`password` / `cancelled`）不变；旧 TermBridge 读到新 tag 报
  `HelperFailed`（可接受：helper 与 mcp 同目录同版本发布）
- Windows CredUI 同步区分 `ERROR_CANCELLED`（用户取消）与其它失败码
  （无交互桌面会话 → `unsupported`）

### 2.4 超时：父进程侧执行，SIGTERM → 宽限 → SIGKILL

- 默认 **5 分钟**（用户可能去找密码 / 切窗口），`TERMBRIDGE_PROMPT_TIMEOUT`
  环境变量可配（秒，`0` = 禁用回退旧行为，非法值回退默认）
- **在 TermBridge 父进程侧计时**，不在 helper 内自杀——helper 可能卡在
  不可中断的内核调用上
- 超时后：`SIGTERM`（Windows: TerminateProcess）→ 2s 宽限 → `SIGKILL`
- helper 安装 **SIGTERM/SIGINT 守卫**（`signal_guard.rs`）：
  - 杀掉正在运行的 prompt 子进程（防孤儿对话框留在屏幕上）
  - 恢复 TTY termios（防用户终端停在无回显 raw 模式「打字无反应」）
  - 恢复默认信号处理并重新触发（保持退出码语义）
  - handler 内只调用 async-signal-safe 的 `kill` / `tcsetattr` / `raise`
- 错误映射：`bootstrap_host` 返回新终态 `timed_out`（正常结果，非错误，
  Agent 应提示用户在场时重试）；`open_session`（auth=password）映射
  `AUTH_FAILED`

### 2.5 可行动错误

全链不可用时，错误信息包含**尝试轨迹**与**三条出路**：

```text
no interactive password prompt available for root@192.0.2.10
(tried: TERMBRIDGE_ASKPASS not set; zenity: cannot open display: :99;
 kdialog: not found in PATH; yad: not found in PATH;
 /dev/tty: No such device or address (os error 6)).
Options: (1) install zenity, kdialog or yad on a desktop session;
(2) set TERMBRIDGE_ASKPASS to an askpass-compatible program;
(3) run from an interactive terminal.
```

### 2.6 已知残余风险（接受）

- **TTY 与宿主 TUI 抢输入**：helper 视角原理上无法检测「终端是否被宿主
  TUI 占用」。缓解 = 链路顺序把 TTY 排在所有非干扰通道之后；文档声明。
- **DISPLAY 指向无人观看的显示器**：对话框挂到超时才返回（级联救不了，
  timeout 兜底）。
- **密码经 askpass 第三方程**：external askpass 属于用户信任边界外
  （它可能自己记日志 / 集成 secret store / 缓存）。TermBridge 的
  secret-handling 承诺（zeroize / 不落日志）不延伸到第三方程序内部。

### 2.7 边界：密码提示 ≠ 命令审批

`CredentialProvider` 只负责「拿到凭据」。命令危险审批（sudo confirm /
policy）属于 ADR-0018 的 Human Control Plane 与 ADR-0011 的 PolicyManager，
**不得**因 helper 具备通用 GUI 对话框能力就合并两者——密码输入是
「用户向 TermBridge 证明身份」，命令审批是「用户向 Agent 授权动作」，
合并会把两个不同的信任决策耦合进一个 HITL 系统。

### 2.8 不做的事

- 不读 `SSH_ASKPASS` / `SSH_ASKPASS_REQUIRE`（§1.3）
- 不打包 / 安装 zenity/kdialog/yad（软依赖）
- 不在 Core 层拆 provider trait 层级（helper 内部用函数 + enum 足够，
  内部 trait 仅在需要 mock 时才有价值）
- 不做 MCP elicitation 主链路（密码会经过 MCP client UI/日志层，
  与 ADR-0009「不信任下游不记录」立场冲突；留作未来可选 backend）
- 不做 macOS 原生 AppKit dialog（osascript 已满足核心诉求，
  留待有真实诉求时再做）

## 3. 实现落点

| 文件 | 变更 |
|---|---|
| `crates/termbridge-auth-helper/src/main.rs` | 协议 v2 四态响应 |
| `crates/termbridge-auth-helper/src/platform/mod.rs` | `PromptOutcome` 四态 enum |
| `crates/termbridge-auth-helper/src/platform/linux.rs` | fallback 链 resolver + zenity 输出解析 + 可行动错误 |
| `crates/termbridge-auth-helper/src/platform/macos.rs` | askpass → osascript → TTY + AppleScript 转义 |
| `crates/termbridge-auth-helper/src/platform/prompt_cmd.rs` | 命令运行器 + 级联信号分类（纯函数 `classify_dialog`） |
| `crates/termbridge-auth-helper/src/platform/tty.rs` | POSIX tty prompt（自 Linux/macOS 实现抽取共享） |
| `crates/termbridge-auth-helper/src/signal_guard.rs` | SIGTERM/SIGINT 守卫（子进程清理 + termios 恢复） |
| `crates/termbridge-auth-helper/src/platform/windows.rs` | CredUI 失败码区分 cancelled / unsupported |
| `src/domain/credential.rs` | `CredentialError::Timeout(u64)` |
| `src/infrastructure/credential/helper.rs` | 协议 v2 解析 + 父进程侧超时（SIGTERM→SIGKILL）+ `TERMBRIDGE_PROMPT_TIMEOUT` |
| `src/application/bootstrap.rs` | `BootstrapResult::TimedOut` 终态 |
| `src/application/sessions.rs` | Timeout → `AUTH_FAILED` 映射 |
| `src/transport/mcp/server.rs` | `bootstrap_host` 工具描述补 `timed_out` |

## 4. 测试

- **helper crate**：16 单测（级联分类纯函数 / zenity 解析 / AppleScript
  转义 / signal guard 状态机）+ 8 集成测试（stub askpass 四态 / 坏
  DISPLAY 级联 / setsid 无 TTY 的 unsupported / script(1) PTY 真实输入）
- **Core**：协议 v2 解析 / 超时 env 解析 / 挂起 helper 的超时 E2E
  （1s 超时 + 断言及时 kill）/ `TimedOut` 序列化 / sessions 映射
- **实测**：本机（GNOME + zenity）验证 8 个 E2E 场景——坏 DISPLAY +
  无 TTY → unsupported 带轨迹；askpass 成功/取消/损坏；TTY 真实输入；
  zenity 真实弹框存活 + 杀进程模拟取消
- **Windows**：`cargo check --target x86_64-pc-windows-msvc` 通过
  （CredUI 路径无法在 Linux 上运行时验证，靠编译检查 + 码位语义审查）

## 5. Consequences

- Linux/macOS 的 GUI 客户端用户首次连接时能真实看到密码输入框
  （zenity/kdialog/yad/osascript），不再收到误导性的「用户取消」
- headless 用户保留 TTY 路径；无任何通道时得到可行动指引而非黑盒错误
- 凭据输入有界等待（默认 5 分钟），`bootstrap_host` 不再可能永久挂起
- 用户可用 `TERMBRIDGE_ASKPASS` 接入任意 askpass 生态程序
  （ksshaskpass / x11-ssh-askpass / secret-tool 包装等）
- 代价：helper 复杂度上升（~600 行含注释与测试）；`timed_out` 是
  `bootstrap_host` 新增返回状态，SKILL.md 已同步 Agent 应对话术
