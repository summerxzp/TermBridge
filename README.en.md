<div align="center">

# TermBridge

**Remote Terminal Runtime for AI Agents**

A persistent, recoverable remote terminal runtime with explicit execution semantics for AI Agents

[![release](https://img.shields.io/github/v/release/summerxzp/TermBridge?label=release)](https://github.com/summerxzp/TermBridge/releases/latest)
[![license](https://img.shields.io/badge/license-MIT-green)](LICENSE)
[![platform](https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-blue)](https://github.com/summerxzp/TermBridge)

[简体中文](README.md) | **English**

</div>

---

TermBridge is an MCP (Model Context Protocol) server that communicates with AI clients (TraeCode / Claude Code / Codex / OpenCode) via stdio. It is not Ansible, not a playbook engine — it provides Agents with a **stable, programmable, secure** remote Terminal Runtime.

## Key Features

- **SSH-First**: Connects via system `~/.ssh/config` + SSH Agent / IdentityFile, zero remote installation
- **Bootstrap on First Connect**: `bootstrap_host` tool deploys SSH public key via platform-native credential dialog (password never enters LLM context)
- **Full Terminal Semantics**: PTY byte stream + cursor + `wait_for` + 4 read modes, not screen snapshots
- **SFTP File/Directory Transfer**: Atomic single-file writes, recursive directories, path policy protection
- **Persistent Runtime (optional)**: Opt-in remote daemon for sessions surviving MCP restarts, detach/attach
- **Agent Terminal Protocol**: 7 rules defined in ADR-0013 ensuring Agents handle completion / timeout / disconnect / TUI correctly
- **Cross-Platform**: Windows / Linux / macOS pre-built binaries (macOS Apple Silicon native, Intel Mac via Rosetta 2), three consumers (CLI + GUI + MCP)
- **Strict Security Boundary**: Host key strict checking, password redaction in logs, SFTP path policy, credentials excluded from MCP schema

## Installation

### Option 1: Download Pre-built Binaries (Recommended)

Download the archive for your platform from [GitHub Release](https://github.com/summerxzp/TermBridge/releases/latest):

| Platform | File |
|----------|------|
| Windows x86_64 | `termbridge-v0.1.1-windows-x86_64.zip` |
| Linux x86_64 | `termbridge-v0.1.1-linux-x86_64.tar.gz` |
| macOS Apple Silicon (M1/M2/M3) | `termbridge-v0.1.1-macos-arm64.tar.gz` |

> macOS Intel users: no pre-built package. Run arm64 build via Rosetta 2, or build from source with `cargo build --release`.

Archive contents:

```
termbridge-v<version>-<platform>-<arch>/
├── termbridge-mcp            MCP server main entry
├── termbridge                CLI (human admin tool, optional)
├── termbridge-auth-helper    Credential helper (must be in same directory as mcp)
├── mcp-config.json           MCP config template
├── SKILL.md                  Agent Skill
├── README.txt                Quick start
└── resources/agentd/
    └── linux-x86_64/         Remote daemon (auto-deployed by bootstrap_host, no manual action needed)
```

### Option 2: Build from Source

**Prerequisites**:
- Rust toolchain (stable, 1.75+ recommended)
- Windows / Linux / macOS (agentd crate is Linux-only)
- OpenSSH installed with `~/.ssh/config` configured

```bash
git clone https://github.com/summerxzp/TermBridge.git
cd TermBridge
cargo build --release -p termbridge -p termbridge-auth-helper --bins
```

## Quick Start

### Step 1: Configure SSH Host

Add your target server to `~/.ssh/config`:

```sshconfig
Host my-server
    HostName 192.0.2.10
    User root
    Port 22
    IdentityFile ~/.ssh/id_ed25519
```

### Step 2: Connect to MCP Client

Import `mcp-config.json` into your AI client, or configure manually:

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

> `termbridge-auth-helper` must be in the same directory as `termbridge-mcp`, otherwise first-time authentication falls back to unsupported state.

### Step 3: Install Agent Skill

Install [SKILL.md](skills/termbridge/SKILL.md) into your AI Agent's skill directory to ensure Agent follows Agent Terminal Protocol (ADR-0013).

### Step 4: First Connection to a New Server

If the target server doesn't have your SSH public key yet, have the Agent call `bootstrap_host`:

```
Agent: I need to connect to my-server, please bootstrap first.
 Tool call: bootstrap_host({ "host": "my-server" })
 Native credential dialog pops up
 User enters password
 TermBridge deploys public key + verifies key auth
← Returns: { "status": "bootstrapped", "authentication": "public_key" }
```

All subsequent `open_session` calls use SSH key authentication.

> Full workflow: [docs/getting-started.md](docs/getting-started.md)

## Tool List (20 tools)

TermBridge exposes 20 tools to MCP clients, categorized by function:

### Host Management

| Tool | Description |
|---|---|
| `list_hosts` | List all Host aliases and hostnames from `~/.ssh/config` |

### Session Lifecycle

| Tool | Parameters | Description |
|---|---|---|
| `open_session` | `host`, `persistent?` | Establish SSH + PTY session, returns `session_id`. `persistent=true` enables remote daemon for cross-restart persistence |
| `send_input` | `session_id`, `data` | Send text to PTY stdin (`\n` for Enter, sent immediately without waiting for command completion) |
| `read_output` | `session_id`, `wait_for?`/`tail_lines?`/`since_cursor?`, `timeout_secs?`, `strip_ansi?` | Read PTY output, supports 4 modes: settle / wait_for / tail_lines / since_cursor. `strip_ansi=true` strips terminal control sequences (CSI/OSC/DCS), RingBuffer keeps raw bytes unchanged. Returns `session_state` field (`ready`/`lost`/`closing`/`closed`) for Agent disconnect awareness |
| `send_control` | `session_id`, `control_key` | Send control keys: `ctrl+c` / `ctrl+d` / `ctrl+z` / `tab` / `enter` / `escape` |
| `close_session` | `session_id` | Close session (idempotent), release SSH channel |
| `reconnect_session` | `session_id` | Reconnect Lost session: rebuild SSH + PTY, reuse original session_id. Buffer history not preserved. Interactive sessions only (persistent sessions use `attach_remote_session`) |
| `resize` | `session_id`, `cols`, `rows` | Resize PTY (window_change), supports TUI programs redrawing on window resize |

### SFTP File Operations

| Tool | Description |
|---|---|
| `sftp_transfer` | Single file upload / download (download uses atomic write: temp + fsync + rename) |
| `sftp_mkdir` | Create remote directory (mode as octal string e.g. `"755"`) |
| `sftp_list` | List remote directory contents (name/type/size/permissions) |
| `sftp_remove` | Delete remote file or directory (`recursive=true` for directory tree, system directories protected) |
| `sftp_chmod` | Change remote file/directory permissions (mode as octal string e.g. `"644"`) |
| `sftp_transfer_dir` | Recursive directory upload/download, auto-creates target directories, skips symlinks, returns file count |

### Remote Environment Detection

| Tool | Description |
|---|---|
| `detect_remote_env` | Detect remote OS (uname), shell, PATH, installed tools (python/node/rustc/go/docker/git etc.) via SSH exec, without polluting PTY session |

### Persistent Runtime (optional, opt-in)

| Tool | Description |
|---|---|
| `list_remote_sessions` | List all sessions on remote daemon (including detached) |
| `attach_remote_session` | Attach to an existing remote session (cross-MCP reconnect) |
| `detach_session` | Detach persistent session (keeps remote PTY, releases local connection) |

### Observability

| Tool | Description |
|---|---|
| `get_session_timeline` | Get session execution timeline: ordered list of command/output/control/state events (with timestamp + cursor metadata) |

### First-Time Authentication

| Tool | Description |
|---|---|
| `bootstrap_host` | Deploy SSH public key to remote `authorized_keys` (see dedicated chapter below) |

## bootstrap_host In Detail

`bootstrap_host` is the security design core of TermBridge, solving the "chicken-and-egg problem" of first login to a new server — no key means no connection, no connection means no key deployment.

### Workflow

```text
bootstrap_host(host)
    │
    ▼
Parse ~/.ssh/config (ssh -G)
    │
    ▼
Verify host key (reuse known_hosts, no auto-accept of changes)
    ├── Changed → HOST_KEY_REJECTED (requires manual intervention)
    │
    ▼
Try SSH Agent authentication
    ├── Success → status: already_configured
    │
    ▼
Try IdentityFile authentication
    ├── Success → status: already_configured
    │
    ▼
No IdentityFile → auto-generate ed25519 keypair (~/.ssh/id_ed25519)
    │
    ▼
Pop up credential input (Windows CredUI / POSIX tty)
    ├── User cancels → status: cancelled
    │
    ▼
Password SSH authentication (one-time)
    ├── Failure → status: authentication_failed
    │
    ▼
Deploy public key to remote ~/.ssh/authorized_keys (idempotent: skip if exists)
    │
    ▼
Close password connection
    │
    ▼
New connection + key auth verification (critical step, cannot skip)
    ├── Failure → status: bootstrap_failed
    │          (possible causes: sshd PubkeyAuthentication no / SELinux / home permissions)
    │
    ▼
status: bootstrapped
```

### Security Guarantees

| Property | Implementation |
|---|---|
| **Password never enters LLM context** | MCP tool schema has no `password` / `secret` / `passphrase` fields |
| **Password via independent channel** | Credential dialog handled by `termbridge-auth-helper` independent process, IPC fully isolated from MCP stdio |
| **Password not persisted** | Immediately `Zeroize`d after authentication, not written to files/logs/env vars |
| **Strict host key verification** | Reuses ADR-0005 known_hosts mechanism, no auto-accept of changes |
| **Idempotent public key deployment** | Checks if public key already exists before deployment, avoids duplicate writes |
| **Reconnection verification** | Must successfully re-authenticate with key after deploying public key before returning `bootstrapped` |

### Return Status

| status | Meaning |
|---|---|
| `already_configured` | Key authentication already available (SSH Agent or IdentityFile), bootstrap not needed |
| `bootstrapped` | Password auth + public key deployment + key verification all succeeded |
| `cancelled` | User cancelled password input |
| `authentication_failed` | Wrong password |
| `bootstrap_failed` | Public key deployed but key reconnection verification failed (check sshd config / permissions / SELinux) |

## SSH Config Recommendations

TermBridge parses system `~/.ssh/config` via `ssh -G`, supporting common directives:

```sshconfig
# Basic config
Host prod-server
    HostName 192.0.2.10
    User root
    Port 22
    IdentityFile ~/.ssh/id_ed25519

# Via jump host
Host bastion-prod
    HostName 203.0.113.50
    User ops
    ProxyJump bastion.example.com

# Strict host key checking (recommended)
Host *
    StrictHostKeyChecking accept-new
    UserKnownHostsFile ~/.ssh/known_hosts
```

**Supported**: HostName / User / Port / IdentityFile / ProxyJump / StrictHostKeyChecking / UserKnownHostsFile / IdentitiesOnly

**Authentication priority**: SSH Agent > IdentityFile > (password auth during bootstrap_host)

## Persistent Runtime (optional)

By default TermBridge is pure SSH: MCP server exits → session lost. For sessions surviving MCP restarts, use `open_session(persistent=true)`:

```text
open_session(host, persistent=true)
    │
    ▼
First time: deploy termbridge-agentd to remote ~/.local/share/termbridge/
    │
    ▼
daemon manages PTY + OutputBuffer (Unix socket communication)
    │
    ├── detach_session → keep remote PTY, release local connection
    │
    └── list_remote_sessions → attach_remote_session → cross-MCP reconnect
```

**Constraints** (Phase 3):
- Remote daemon crash = session lost (no disk persistence)
- daemon single-user mode, socket permission 0600
- No TCP / HTTP, Unix socket + SSH tunnel only

## Security Model

| Dimension | Policy |
|---|---|
| **Host Key** | Strict known_hosts verification, reject changes, no auto-accept (ADR-0005 §2) |
| **SSH Authentication** | Prefer SSH Agent / IdentityFile, password only for one-time bootstrap (ADR-0009) |
| **Password Isolation** | Password via independent helper process IPC, never enters MCP arguments / LLM context |
| **Log Redaction** | tracing logs auto-redact password / token / key and other sensitive fields (ADR-0005 §3) |
| **SFTP Path Policy** | Local path allowlist `[cwd, $TEMP/termbridge]` + env var `TERMBRIDGE_ALLOWED_LOCAL_PATHS` to append; remote path realpath resolution prevents `../` traversal (ADR-0005 §4) |
| **Download Atomic Write** | Temp file + fsync + rename, avoids half-written files being misread |

## Consumers

TermBridge Runtime supports three consumers, all following Agent Terminal Protocol (ADR-0013):

| Consumer | Entry | Use Case |
|----------|-------|----------|
| **MCP** | `termbridge-mcp` | AI Agents (TraeCode / Claude Code / Codex / OpenCode) |
| **CLI** | `termbridge` | Human admins, raw mode PTY, supports vim/top/htop |
| **GUI** | Tauri v2 + React + xterm.js | Visual terminal, Host/Session management |

## Architecture Decision Records (ADR)

| ADR | Topic | Status |
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
| [0010](docs/adr/0010-session-reconnect.md) | Session Disconnect Sensing + Manual Reconnect | Accepted |
| [0011](docs/adr/0011-input-semantics-and-execution-safety.md) | send_input Semantics + Execution Safety | Accepted |
| [0012](docs/adr/0012-execution-state-and-completion-protocol.md) | Execution Semantic Contract (9 contracts) | Accepted |
| [0013](docs/adr/0013-agent-terminal-protocol.md) | Agent Terminal Protocol (7 rules) | Accepted |
| [0014](docs/adr/0014-phase7-consumer-roadmap.md) | Phase 7 Consumer Roadmap | Accepted |
| [0015](docs/adr/0015-provider-api-freeze.md) | Provider API Freeze | Accepted |
| [0016](docs/adr/0016-runtime-freeze.md) | Runtime Freeze | Accepted |

## Project Structure

```text
TermBridge/
├── src/                              # Main crate
│   ├── domain/                       # Domain abstractions (CredentialProvider / Provider / Session / Timeline)
│   ├── application/                  # Business logic (BootstrapHost / Sessions / Hosts)
│   ├── infrastructure/               # Infrastructure (SSH / SFTP / Credential / DaemonProto)
│   ├── transport/mcp/                # MCP server (rmcp)
│   └── bin/termbridge.rs             # Human admin CLI
├── crates/
│   └── termbridge-auth-helper/       # Independent credential helper (cross-platform)
├── agentd/                           # Remote daemon (Linux only)
├── gui/                              # Tauri v2 + React + xterm.js
├── skills/termbridge/SKILL.md        # Agent Skill
├── examples/mcp/                     # MCP config templates
└── docs/adr/                         # Architecture decision records
```

## Roadmap

| Phase | Topic | Status |
|---|---|---|
| Phase 0 | Prototype validation (MCP / SSH PTY / ssh config) | ✅ Complete |
| Phase 1 | Interactive Session (SSH + PTY + SFTP basics) | ✅ Complete |
| Phase 2 | SFTP extensions (mkdir / list / remove / chmod) | ✅ Complete |
| Phase 3 | Remote Persistent Runtime (daemon + detach/attach) | ✅ Complete |
| Phase 4 | Observability (Timeline) | ✅ Complete |
| Phase 5 | Remote Workspace (SFTP dir recursive + env detection) | ✅ Complete |
| Phase 6 | Execution State + Reconnect + Agent Terminal Protocol | ✅ Complete |
| Phase 7 | CLI + Cross-platform + GUI + Provider API Freeze | ✅ Complete |
| Phase 8 | Adoption / Bootstrap (Skill + out-of-box + Dogfooding) | ✅ Complete |
| [ADR-0016](docs/adr/0016-runtime-freeze.md) | **Runtime Freeze** | ✅ Frozen |
| Future | Local / Docker / WSL Provider, Playbook, advanced GUI | Planned |

> **Boundary Statement** (ADR-0008): TermBridge is a Remote Terminal Runtime, not an AI Ops Platform. It does not handle config validation / playbook / service orchestration / desired state. Orchestration layer is a separate future project.

## Verification Matrix

| Test Suite | Result |
|---|---|
| P0 execution semantics (ADR-0012) | 33/33 ✅ |
| T17 attach/cursor boundary | 8/8 ✅ |
| Cross-restart E2E | 5/5 ✅ |
| T16 resize | 6/6 ✅ |
| Unit tests | 256/256 ✅ |

## License

[MIT](LICENSE)
