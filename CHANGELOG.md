# Changelog

All notable changes to TermBridge are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **更新检查**（借鉴 chrome-devtools-mcp）：`termbridge-mcp` 与 `termbridge` 启动时检查 GitHub Releases 是否有新版本，有则 stderr 提示下载（仅提示，不自动安装）
  - 本地缓存 `dirs::cache_dir()/termbridge/update-check.json`，24h 内不重复联网检查；网络/解析失败静默，24h 后重试
  - 版本刷新在后台线程异步完成，不阻塞启动；可用 `TERMBRIDGE_NO_UPDATE_CHECK=1` 关闭

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
