# Changelog

All notable changes to TermBridge are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
- Credential isolation via platform-native `termbridge-auth-helper` (Windows CredUI / Linux tty / macOS Keychain)
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
