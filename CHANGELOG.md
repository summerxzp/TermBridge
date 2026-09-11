# Changelog

All notable changes to TermBridge are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.4] - 2026-09-11

三平台发布二进制兼容性基线（ADR-0020）。修复 v0.3.1 起 Linux 产物无法在 Ubuntu 22.04 / RHEL 9 / Debian 12 等系统运行的 GLIBC_2.39 问题。

### Fixed

- **Linux 产物 GLIBC_2.39 依赖（v0.3.1 起）**：CI 在 `ubuntu-latest`（24.04，glibc 2.39）构建，rustix 0.38.44 的 `pidfd_spawnp` 弱符号在链接时写死 GLIBC_2.39——Ubuntu 20.04/22.04、Debian 11/12、RHEL/Rocky/Alma 8/9 全部无法运行（`npx` 实测复现）。**修复：Linux 全产物线（含 agentd）切换 musl 静态链接**，零 libc 依赖，任意现代 x86_64 Linux 内核可运行；agentd 部署到远端服务器不再依赖目标机发行版
- **Windows 产物 VCRUNTIME140.dll 依赖**：动态链接 MSVC CRT 导致缺 VC++ Redistributable 的机器（portable VSCode / Server Core / 精简镜像）上 MCP server 静默启动失败。**修复：静态 CRT（`crt-static`）**，产物自包含；代价为包体积 +3%~16%（npm 包 27.4MB 基线实测）

### Added

- CI 静态性验证门禁：Linux 产物断言 0 个 GLIBC 引用、Windows 断言无 VCRUNTIME140/ucrtbase 导入、macOS 断言 deployment target 11.0——防止未来依赖或 runner 漂移无声破坏基线
- CI 增加 musl target 的 agentd 测试（46/46）与 auth-helper 集成测试，与发布产物线同参数
- macOS 显式锚定 `MACOSX_DEPLOYMENT_TARGET=11.0`（Rust target 默认值，显式写入防漂移）
- README 平台兼容性表：三端基线（Win10+ 静态 CRT / Linux musl 静态 / macOS 11+）+ musl DNS/NSS 边界说明

### 已知限制（ADR-0020 §5，文档化接受）

- musl 无 NSS 扩展：mDNS（`.local`）/ SSSD/LDAP 企业主机名场景需用标准 DNS 域名或 IP 连接（agentd 零影响——源码无主机名解析；本地侧唯一影响点是 SSH 连接的目标解析）
- musl malloc 多线程分配性能弱于 glibc：本项目 I/O 型负载，影响可忽略

## [0.3.3] - 2026-09-11

0.3.2 的重发行：内容与 0.3.2 完全一致（ADR-0019 + 发版修复），仅版本号顺延。

> 0.3.2 在 npm 上为残缺状态（mcp/linux/darwin 三包已上，win32-x64 缺失）：
> win32-x64@0.3.2 曾在 v0.3.1 发版事故中发布过，unpublish 后按 npm 政策
> 「package@version 一旦用过永不能复用」无法重发，导致主包 0.3.2 的
> Windows 用户 npx 安装时平台二进制被 optional 静默跳过。latest 已被
> 0.3.3 覆盖，`npx @summerxzp/termbridge-mcp@latest` 不受影响；请勿
> 显式安装 @0.3.2。

### Added

- **Linux/macOS 凭据输入多级 fallback（ADR-0019）**：`bootstrap_host` / `auth=password` 的密码输入从「仅 `/dev/tty`」升级为 `TERMBRIDGE_ASKPASS`（显式配置，askpass 兼容）→ GUI 对话框（zenity / kdialog / yad / osascript，PATH 软依赖，不打包）→ TTY 兜底。GUI 客户端（VSCode / Trae / Cursor 等）首次连接现在能真实弹出密码框；设计原则是「优先选择不干扰宿主 Agent 终端的输入通道」，TTY 排最后（宿主 TUI 占用终端的风险见 ADR-0019 §2.6）
- **凭据输入超时**：默认 5 分钟（`TERMBRIDGE_PROMPT_TIMEOUT` 秒可配，0 = 禁用），TermBridge 父进程侧执行（SIGTERM → 2s 宽限 → SIGKILL），`bootstrap_host` 不再可能永久挂起。helper 侧安装 SIGTERM 守卫：超时被杀时恢复 termios（防终端停在无回显模式）+ 清理子对话框进程（防孤儿窗口）
- `bootstrap_host` 新增 `timed_out` 返回状态（正常终态非错误，含超时秒数）；Agent 应提示用户在场时重试

### Fixed

- **凭据错误语义折叠（协议 v2）**：helper 响应区分 `cancelled`（用户取消）/ `unsupported`（环境无输入通道，含尝试轨迹与三条出路）/ `failed`（显式配置的 askpass 程序损坏）。此前 Linux/macOS 所有平台错误统一折叠成 `cancelled`，GUI 客户端用户会被告知「你取消了」而实际从未见过输入框
- **级联信号防误判**：GUI provider 仅在「对话框从未展示」（ENOENT / stderr 环境失败特征 / <2s 快速退出）时级联到下一 provider；对话框展示后的任何非零退出（含用户取消、yad Esc 关窗码 252、osascript "User canceled" stderr）视为取消——用户点一次取消不会连弹三个框
- Windows CredUI 失败码不再全部折叠为取消：`ERROR_CANCELLED` = 用户取消，其余（无交互桌面会话等）= unsupported 带指引
- CI：npm publish「版本已存在」兜底 grep 与实际错误消息不匹配（大小写/单复数），残留版本撞车时 job 直接失败而非跳过——模式改 `grep -iE "cannot publish over (the )?previously published versions?|EPUBLISHCONFLICT"`；发版规范补「npm 版本号不可复用」预检条款（§2.2.1）
- CI：macOS flaky——`update_check` 两个测试共享同一 pid 命名的缓存路径，并发执行时互相污染，改为每测试独立目录（0.3.0 引入的既有问题）

## [0.3.2] - 2026-09-11

> 残缺版本（见 0.3.3 段落说明）：ADR-0019 首发载体，GitHub Release 六资产完整，
> npm 仅三包。内容与 0.3.3 完全一致。

### Added

- **Linux/macOS 凭据输入多级 fallback（ADR-0019）**：`bootstrap_host` / `auth=password` 的密码输入从「仅 `/dev/tty`」升级为 `TERMBRIDGE_ASKPASS`（显式配置，askpass 兼容）→ GUI 对话框（zenity / kdialog / yad / osascript，PATH 软依赖，不打包）→ TTY 兜底。GUI 客户端（VSCode / Trae / Cursor 等）首次连接现在能真实弹出密码框；设计原则是「优先选择不干扰宿主 Agent 终端的输入通道」，TTY 排最后（宿主 TUI 占用终端的风险见 ADR-0019 §2.6）
- **凭据输入超时**：默认 5 分钟（`TERMBRIDGE_PROMPT_TIMEOUT` 秒可配，0 = 禁用），TermBridge 父进程侧执行（SIGTERM → 2s 宽限 → SIGKILL），`bootstrap_host` 不再可能永久挂起。helper 侧安装 SIGTERM 守卫：超时被杀时恢复 termios（防终端停在无回显模式）+ 清理子对话框进程（防孤儿窗口）
- `bootstrap_host` 新增 `timed_out` 返回状态（正常终态非错误，含超时秒数）；Agent 应提示用户在场时重试

### Fixed

- **凭据错误语义折叠（协议 v2）**：helper 响应区分 `cancelled`（用户取消）/ `unsupported`（环境无输入通道，含尝试轨迹与三条出路）/ `failed`（显式配置的 askpass 程序损坏）。此前 Linux/macOS 所有平台错误统一折叠成 `cancelled`，GUI 客户端用户会被告知「你取消了」而实际从未见过输入框
- **级联信号防误判**：GUI provider 仅在「对话框从未展示」（ENOENT / stderr 环境失败特征 / <2s 快速退出）时级联到下一 provider；对话框展示后的任何非零退出（含用户取消、yad Esc 关窗码 252、osascript "User canceled" stderr）视为取消——用户点一次取消不会连弹三个框
- Windows CredUI 失败码不再全部折叠为取消：`ERROR_CANCELLED` = 用户取消，其余（无交互桌面会话等）= unsupported 带指引

## [0.3.1] - 2026-09-07

全量 code review（`docs/code-review-2026-08-30.md`）修复批次 + 真实 Linux 环境实测验证。

### Fixed

**SSH / PTY**
- PTY rows/cols 与 russh 参数顺序对齐（russh 为 col-first），修正初始窗口尺寸颠倒（远端 `stty size` 实测 24×80 正确，resize 后 30×100 正确）
- SSH connect / channel_open / PTY write / send_control / `SshProvider::exec` 收集循环（120s）/ `ssh -G` 全部补齐超时，半开连接不再永久挂起
- `open_session` host 别名拒绝 `-` 前缀，堵 `ssh -G` 参数注入；`userknownhostsfile` 支持带引号含空格路径；Git Bash（MSYS）下 `ssh -G` POSIX 路径（`/c/Users/...`）归一化，Windows 侧 host key 不再永远未知

**agentd（远端 daemon；Linux 实测 46/46 三轮全绿）**
- 子进程退出：冲刷尾部输出 + 发送 `pty_exit`/`session_lost` 终态事件（此前 attached 客户端永远挂等，实测 exit 后立即转 lost）
- 客户端断连自动 detach，重连直接 attach（实测 detach→attach 游标续读正常）
- `send_input` 改专属写线程（短写补全 + 256KB 背压 5s 超时），RPC dispatch 不再被阻塞写楔死；PtyWriter Drop 有界 join（panic 路径不再挂死 unwind）
- FD_CLOEXEC 全覆盖（shell 子进程不再继承 listener/连接/其他 session 的 pty master）；fork 前完成全部内存分配；kill 打进程组 + EOF 路径 reap；日志改 stderr 不污染协议流

**策略 / 安全**
- 封堵 `authorized_keys`/`authorized_keys2` 经 SFTP create 的绕过（新建文件此前不检查目标路径）；策略层敏感路径词法归一化（`//`、`..` 变体不再绕过 Confirm）；hosts.toml 损坏 fail-closed；bootstrap 公钥部署注入安全（base64 append）；`sftp_chmod` 拒绝 mode=0（防 chmod 0000）
- 控制面：IPC token 改 CSPRNG（原时间戳^pid 零熵）、discovery 文件 0600、HELLO 失败限流、endpoint 唯一化；**Windows 控制面切换 Named Pipe + 当前用户 SID DACL**（protected DACL 无 Everyone/Anonymous ACE，跨用户连接在传输层被拒；`FILE_FLAG_FIRST_PIPE_INSTANCE` 防管道名抢注）

**输出 / SFTP**
- `extract_context`（wait_for context_lines≥1）补回匹配文本本身；ANSI strip 支持带 intermediate byte 序列（`ESC ( B` 等）+ 跨页状态机（分页不再截断序列）；wait_for 唤醒改 watch（并发 waiter 不丢唤醒）
- SFTP 上传原子化（tmp + 尺寸校验 + rename）；download 临时文件 pid+毫秒命名；`sftp_transfer_dir` 返回 `skipped` 列表（symlink/非常规/本地不安全文件名带原因，Agent 可感知静默跳过）；`download_dir` 校验远端文件名防 Windows 路径意外
- Secret 手写 Debug 输出 `[REDACTED]`；日志脱敏补 URL userinfo

**其他**
- agentd 升级路径：版本比对触发重部署 + 原子部署（tmp+chmod+mv）+ 部署后同流程自动重启远端 daemon；新增 MCP 工具 `restart_remote_daemon`
- 凭据弹窗（CredUI）用户名可编辑且真正生效（此前回读被丢弃），按主机记忆用户名（最新优先），ssh config User 可为空
- GUI 修复 React StrictMode 双挂载读循环瓜分 PTY 输出
- `RUNTIME_MISSING` 错误信息改为 agent 可自助排查的完整指引（agentd 是什么/为何缺失/三条修复路径）；SKILL.md 新增「Architecture At A Glance」章节（agentd 知识前置，用户 dogfooding 反馈）
- CI 补 agentd 测试步骤（46 个 Linux-only 测试此前从未在 CI 运行）；agentd fd 泄漏测试改「不新增泄漏」语义（CI runner 环境继承 fd 不算泄漏）
- npm 平台包/主包 package.json 补 `repository.url`（OIDC provenance 校验硬性要求）；Windows 平台包 bin 值不允许 `.exe` 后缀（npm 静默删除条目），改为无扩展名 Node shim 转发

### Known Issues

- npm 平台包 `bin` 字段修复（Linux/macOS 二进制 0644 → EACCES）包含在本版本，`npx @summerxzp/termbridge-mcp@0.3.1` 为首个 Linux/macOS 可用的 npm 版本
- russh-sftp 无 posix-rename 扩展，SFTP 覆盖上传存在极短的 remove→rename 空窗

## [0.3.0] - 2026-08-28

### Added

- **更新检查**（借鉴 chrome-devtools-mcp）：`termbridge-mcp` 与 `termbridge` 启动时检查 GitHub Releases 是否有新版本，有则 stderr 提示下载（仅提示，不自动安装）
  - 本地缓存 `dirs::cache_dir()/termbridge/update-check.json`，24h 内不重复联网检查；网络/解析失败静默，24h 后重试
  - 版本刷新在后台线程异步完成，不阻塞启动；可用 `TERMBRIDGE_NO_UPDATE_CHECK=1` 关闭
- **agentd 自动自举**（修复 `persistent=true` 全新安装报 `RuntimeMissing`）：首次部署时若本地缓存缺失，自动从发布包同目录的 `resources/agentd/linux-x86_64/termbridge-agentd` 复制到 `%LOCALAPPDATA%\TermBridge\agentd\`（POSIX 自动补执行位），不再需要手动放置；新增 3 个单元测试
- **npm 平台包方案（长期主渠道，`packaging/npm-platform`）**：薄主包 `@summerxzp/termbridge-mcp` + 平台包 `@summerxzp/termbridge-win32-x64|linux-x64|darwin-arm64`（每包含完整 release 目录，保持 exe 同目录 / `current_exe()` / agentd 布局语义）
  - `scripts/build-platform-packages.mjs`：从 release staging 生成 3 个平台包并同步主包版本与 optionalDependencies（版本取自 git tag，单源）
  - release.yml 的 `npm-packages` job：从 release staging 生成平台包 + `npm pack` 校验 + 经 **npm Trusted Publishing（OIDC）** 发布（各 npm 包已配置 `summerxzp/TermBridge` `release.yml` 为受信发布者，无需 NPM_TOKEN）
  - release.yml 发布矩阵归档统一为**扁平结构**（Windows zip 与 Unix tar.gz 解压层级一致）
- **npm 壳过渡方案修正（`packaging/npm`）**：
  - 版本严格绑定：下载的二进制版本 = npm 包版本（`npx termbridge-mcp@0.2.1` 精确运行 v0.2.1），不再拉 GitHub latest；移除后台 24h 自动升级（更新交给 npm）
  - 兼容带顶层目录的旧归档（`findBinary` 两级探测）
  - 下载失败给出明确提示（确认 tag 已发布 / `TERMBRIDGE_NPM_MIRROR` 镜像兜底）

### Security

- **SFTP 远端路径策略重做（ADR-0005 §4）**：从"整目录封死"改为**范围(scope) + 操作(operation) 分离**模型
  - 新增 `RemoteOperation` 分级（Read / Write / Create / Delete / Chmod），hard safety 规则只拦高风险操作：
    - `~/.ssh/authorized_keys`：写/建/删/改权限一律硬拒（公钥部署唯一通道是 `bootstrap_host`）
    - `/proc`、`/sys`：写/建/删硬拒（内核接口）；读不受限
    - `/etc` 等系统目录的正常读/写仍放行（改 nginx.conf、部署应用等运维场景不受影响）
  - `hosts.toml` 新增 per-host `allowed_remote_paths`（ADR-0017 Host Policy 扩展）：按主机声明可触及范围，未配置回退全局 `TERMBRIDGE_ALLOWED_REMOTE_PATHS`（默认 `["/"]`，不缩小 SSH 账号已具备权限）
  - `~` / `~/...` 远端路径经 `realpath("~")`（通道级缓存）正确展开；`Create` 目标不存在时校验父目录；null 字节路径一律拒绝

## [0.2.1] - 2026-08-14

### Fixed

- macOS：`XDG_RUNTIME_DIR` 未设置时 instance discovery 读写目录不一致（register 走 temp_dir 兜底而 list 返回空），以及 Unix socket bind 前未创建父目录导致 `ENOENT` —— 统一目录兜底 + bind 前 `create_dir_all`

### Added

**Local Control Plane（ADR-0018）**
- 新增 Human Control Plane：MCP server 启动时监听本地 IPC（Linux/macOS Unix socket 0600 / Windows TCP loopback），与 MCP stdio（Agent 数据面）互补
- Agent 不可调用权限提升接口——`set_approval_mode` 仅通过 Control IPC 由人类经 CLI/GUI 操作
- Instance 发现机制：MCP server 启动时写 `$XDG_RUNTIME_DIR/termbridge/mcp-<instance>.json`（Windows: `%TEMP%/termbridge/`），含 pid/endpoint/token；退出自动清理 + stale instance 自动回收
- 新 CLI 子命令：`termbridge mcp list`（列运行中 MCP server）、`termbridge session list`（列 MCP server 上的 session）、`termbridge session approve <session_id>`（批准 session 进入 unrestricted 模式）

**Session Approval Mode**
- `ApprovalMode` enum（`Standard` / `Unrestricted`），session-scoped 不持久化，Session 关闭即重置
- `Unrestricted` 模式**只跳过 confirm 类 guardrail**（sudo / rm -rf /tmp 等），blocklist / hard deny（`rm -rf /`、`mkfs` 等）**仍生效**；不绕过 SSH host key / credential / path safety / protocol invariants 等系统级安全边界
- `list_sessions` / `SessionSummary` 暴露 `approval_mode` 字段

**sudo -n 保守放行**
- `sudo -n` / `--non-interactive` 命令豁免 sudo confirm（auto-passthrough），仅当：行内仅一次 sudo、行首命令、紧跟 `-n`、无 shell 复合构造（`;` `&&` `||` `|` `$(` 等）
- blocklist（rm -rf /、mkfs、dd of=/dev/ 等）和其他 confirm 规则仍独立生效，安全性不降级
- 防止 policy bypass：`sudo rm /tmp/foo; echo "sudo -n"` 等子串注入正确拦截

**Policy 错误信息改进**
- `POLICY_NEEDS_CONFIRM` 错误附 3 条可操作建议：用 `sudo -n` / 请求 `termbridge session approve` / 手动执行

**Skill 强化**
- 新增 `Input Semantics` 小节：区分 shell command input（必加 LF）vs interactive input（不加 LF），附 BAD/GOOD 对比
- Decision Table 更新 sudo 行为：`sudo -n` auto-passthrough + unrestricted session 选项
- Anti-Patterns 新增 sudo 反模式示例

### Changed

- `Session` struct 新增 `approval_mode` 字段（`parking_lot::Mutex<ApprovalMode>`，默认 Standard）
- `PolicyManager` 新增 `authorize_with_approval(action, approval_mode)` 方法（Unrestricted 短路）
- `check_policy` 改造：仅 `SendInput` 在 Deny/Confirm 时检查 session approval_mode（SFTP 有独立 PathPolicy，不参与短路）

## [0.2.0] - 2026-08-13

### Added

**Host Connection Policy（ADR-0017）**
- Per-host 连接策略配置文件 `hosts.toml`：`auth`（key / password / auto）+ `session`（standard / persistent）双维度
- 优先级：显式参数 > host policy > system default；无 hosts.toml 时行为与 0.1.x 完全一致（向后兼容）
- `auth=password` 认证路径：`open_session` 经 credential helper 弹窗请求密码（不持久化、不部署 key）；`password + persistent` 组合在弹密码**之前**明确拒绝（不做静默降级）
- `bootstrap_host` 成功返回 `hint`（建议手动更新 hosts.toml），**永不自动修改配置**（ADR-0017 §2.2 不可变原则）
- 新 CLI 子命令 `termbridge policy`：查看 hosts.toml 策略（全览 / 单 host 有效值 + 修改提示）
- Skill 新增 password-policy host 两条规则（禁止主动 bootstrap_host + open_session 可能触发用户弹窗）

### Fixed

- `ssh -G` 在 Git for Windows 下阻塞等待 stdin EOF，导致 open_session 永久挂起（stdin 置空）
- TOML 点号别名（IP host）静默失效：`[hosts.192.168.1.180]` 被解析为嵌套表、策略不生效 — 加载时 WARN 并提示正确写法 `[hosts."192.168.1.180"]`
- macOS 配置路径遵循 XDG（`~/.config/termbridge/`，而非 Apple 的 `~/Library/Application Support`）；Windows 统一为 `%APPDATA%\TermBridge\`（与 agentd 本地路径一致）

## [0.1.1] - 2026-08-12

### Changed

**Release pipeline**
- New `release.yml` workflow: automated 4-target build on `v*` tag push, packages with templates + sha256, uploads to GitHub Release
- Add macOS Apple Silicon (`aarch64-apple-darwin`) pre-built binary — macOS users no longer need to build from source
- Embed `termbridge-agentd` (Linux x86_64) into all host packages under `resources/agentd/linux-x86_64/`; `bootstrap_host` auto-deploys it, users no longer need to manually download the remote daemon
- New `packaging/` directory: `README.txt` + `mcp-config.json` templates tracked in git (previously in gitignored `release-artifacts/`, unavailable to CI)

### Fixed

- `CHANGELOG.md`: correct stale "macOS Keychain" credential description to "POSIX tty" (matches actual `termbridge-auth-helper` implementation on macOS)

### Notes

- macOS Intel (x86_64) pre-built binary intentionally not published — Intel Mac users can run the arm64 build via Rosetta 2 or build from source. Rationale: prioritize 95% user coverage over 100% arch coverage (see `docs/internal/打包建议.md`).
- Release matrix reduced to 3 host packages: `windows-x86_64`, `linux-x86_64`, `macos-arm64`.

## [0.1.0] - 2026-08-12

First public release. TermBridge Core is frozen (ADR-0016).

### Added

**Terminal Runtime**
- SSH PTY sessions with cursor-based output buffer and `wait_for` pattern matching
- Persistent daemon sessions (detach/attach, cross-restart recovery)
- Session reconnect after SSH disconnect (ADR-0010)
- PTY resize support (`resize` tool)
- 20 MCP tools: session lifecycle, SFTP, persistent sessions, timeline, bootstrap, reconnect
- `strip_ansi` option on `read_output` for clean text output

**Agent Terminal Protocol** (ADR-0013)
- 7 rules for AI Agent consumers: completion markers, timeout handling, disconnect recovery, idempotency, TUI mode, cursor usage, persistent sessions

**Bootstrap & Security** (ADR-0009)
- `bootstrap_host` one-time SSH key deployment
- Credential isolation via platform-native `termbridge-auth-helper` (Windows CredUI / POSIX tty)
- Password never enters LLM context

**Consumers**
- CLI (`termbridge` binary) with crossterm raw mode, WINCH resize, Ctrl+C/D/Z passthrough
- GUI (Tauri v2 + React + xterm.js) with 10 Tauri commands
- Agent Skill (`skills/termbridge/SKILL.md`) with decision table and anti-patterns

**Cross-platform**
- Windows, Linux, macOS build support
- GitHub Actions CI matrix (3 platforms)

**Documentation**
- 16 ADRs covering architecture decisions from Phase 0 to Phase 8
- Agent Skill with operational workflow and decision table
- MCP config templates for Claude Code, Codex, OpenCode
- Getting started guide

### Verified

- 33/33 P0 tests (ADR-0012 execution semantics)
- 8/8 T17 attach/cursor boundary tests
- 5/5 cross-restart E2E tests
- 6/6 T16 resize tests
- 256/256 unit tests

### Frozen

- Runtime Contract (ADR-0012): 9 contracts
- Agent Terminal Protocol (ADR-0013): 7 rules
- Provider API (ADR-0015): 2+6 trait methods

## Phase History

- **Phase 0**: Prototype validation (MCP / SSH PTY / ssh config)
- **Phase 1**: Interactive Session (SSH + PTY + SFTP basics)
- **Phase 2**: SFTP extensions (mkdir / list / remove / chmod) + ProxyJump
- **Phase 3**: Remote Persistent Runtime (daemon + detach/attach)
- **Phase 4**: Observability (Timeline + SessionSummary)
- **Phase 5**: Remote Workspace (SFTP dir recursive + env detection)
- **Phase 6**: Execution State + Reconnect + Agent Terminal Protocol
- **Phase 7**: CLI + Cross-platform + GUI + Provider API Freeze
- **Phase 8**: Adoption (Skill + Bootstrap + Dogfooding + Runtime Freeze)
