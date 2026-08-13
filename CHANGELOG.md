# Changelog

All notable changes to TermBridge are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
