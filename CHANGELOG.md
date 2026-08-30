# Changelog

All notable changes to TermBridge are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

### Fixed

**SSH / PTY（termbridge core）**
- PTY rows/cols 与 russh 参数顺序对齐（russh 为 col-first），修正初始窗口尺寸颠倒
- SSH connect / exec / SFTP open / PTY write / `ssh -G` 全部补齐超时，无响应主机不再永久挂起；新增 host 别名注入防护与 known_hosts 引号路径解析
- `SshProvider::exec` 收集循环加 120s 总超时（channel_open 同步加界）；`exec_stream`（proxy 长生命通道）仅 channel_open 加界，数据循环有意保持无界
- SFTP 下载本地临时文件改 pid+毫秒命名（原固定 `.termbridge.tmp` 并发下载同一目标互覆）
- 输出链路：`extract_context` 返回匹配文本；`strip_ansi` 修复 CSI 中间字节剥离与跨页状态；RingBuffer 改 watch 唤醒，避免读取空转
- `sftp_chmod` 拒绝 mode=0（防止把文件权限清零）；SFTP 上传改原子写（temp + fsync + rename）；`sftp_transfer_dir` 下载校验目标目录名
- Timeline 缓冲改用 `VecDeque`，避免大 session 下的频繁内存搬移
- Unix 缓存路径统一走 `dirs` crate（遵循 XDG）；CLI 读消息增加长度边界校验；日志脱敏补全（Secret Debug redaction + URL userinfo redact）

**agentd（远端 daemon；新增单元测试需 Linux CI 跑通）**
- 新增 MCP 工具 `restart_remote_daemon`：pid 文件校验（/proc comm 防误杀）→ SIGTERM/5s 宽限/-9 兜底 → 清理 socket → bootstrap 新 daemon；升级部署（NeedsUpgrade）后同一流程内自动重启生效
- 补齐 pty_exit / session_lost 事件 + 通知驱动泵 + tail flush；修复 disconnect→detach 后 reconnect 的输出丢失
- `send_input` 改独立 writer 线程 + 背压，大输入不再阻塞 read loop
- 进程安全：FD_CLOEXEC、fork 前完成内存分配（fork-before-exec）、进程组 kill + reap；日志改走 stderr，不污染 PTY
- daemon 升级路径 + 原子部署（升级不再中断既有 session）

**策略 / 控制面安全**
- 封堵 authorized_keys / authorized_keys2 经 SFTP create 的绕过；敏感路径词法归一化（`..` / 重复分隔符）；hosts.toml 解析失败 fail-closed；bootstrap 公钥部署注入安全 + 非 UTF-8 home 兼容
- 控制面加固：IPC token 改用 CSPRNG、discovery 文件 0600、HELLO 限流、endpoint 唯一化

**GUI / npm 分发**
- 修复 React StrictMode 双挂载产生两条 PTY read loop（字节流被拆分、一半丢失）：后端重建前先 abort 同 session 旧任务 + 循环结束自清理（防 map 泄漏），前端 `startReadLoop` 补 disposed 检查
- npm 平台包 package.json 补 `bin` 字段：修复 Linux/macOS 发布的二进制被 `npm pack` 归一为 0644（运行 EACCES）的发布阻断问题；launcher 失败提示改为 scoped 包名；release.yml 移除 `!cancelled()` 误用、新增 tag/Cargo.toml 版本一致性门禁、删除死代码兼容块；README 与内部文档同步（npm 主渠道、扁平归档结构、legacy 下载器说明）

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
