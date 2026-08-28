---
name: "termbridge"
description: "Operate remote Linux hosts via TermBridge terminal runtime. Invoke when user asks to run commands on, manage, debug, or deploy to a remote SSH host."
metadata:
  version: "0.3.0"
  mcp-server: termbridge
---

# TermBridge

TermBridge is a persistent, recoverable terminal runtime for AI agents. It exposes a remote SSH host's shell as a PTY session you can send input to, read output from, and resume after disconnect.

> **Version check**: `metadata.version` in this file is the TermBridge release this SKILL.md was packaged with. The MCP server reports its own version in the `serverInfo.version` field of the initialize handshake. If the two differ (e.g. the user updated termbridge but this skill was not re-synced from the release package), tell the user: "SKILL.md 版本落后，请从 release 包重新复制 SKILL.md 到 agent 的 skill 目录" and do not assume behaviors from newer releases.

Use it when the user wants to operate on a remote Linux host: run commands, debug services, edit files, deploy code, inspect logs.

## Core Workflow

```
1. list_hosts                      → confirm host alias is visible
2. bootstrap_host (first time)     → deploy SSH public key (one-time, key-auth hosts only)
3. open_session(host)              → get session_id
4. send_input(cmd)                 → run command (raw bytes, no auto-\n)
5. read_output(wait_for=marker)    → wait for completion
6. read_output(since_cursor=...)   → read full output
7. close_session                   → done
```

For an already-running persistent session, skip step 3 and `attach_remote_session` instead.

> **Host policy (ADR-0017 §3.3)**: `hosts.toml` defines per-host defaults (`auth` = `key` / `password` / `auto`; `session` = `standard` / `persistent`).
> - **`auth=password` host** → every `open_session` prompts the user for a password (out-of-band, never via MCP args). **Never call `bootstrap_host` on such a host** unless the user explicitly asks to switch to key auth.
> - **`bootstrap_host` does not modify host policy** — hosts.toml stays untouched; `open_session` keeps prompting until the user edits it.

> **Scope guidance**: For standard commands (ls, cat, grep, systemctl status), the workflow above is sufficient. For timeout / disconnect / TUI programs / retry scenarios, consult the **7 Rules** and **Decision Table** below.

## Input Semantics

`send_input()` sends **exactly the bytes you provide**. Nothing is added or modified.

### For shell commands: always include terminating LF

```
# BAD: command stays in input buffer, never submitted
send_input("ls -la")

# GOOD: LF terminates the command, shell executes it
send_input("ls -la\n")
```

### For interactive input: do NOT add LF unless submitting

Interactive prompts (sudo password, `read -p`, vim, REPL, mysql) expect input without automatic LF. Only add `\n` when the user intends to submit the input.

```
# sudo password prompt: send password + LF to submit
send_input("my_password\n")

# vim normal mode command: no LF
send_input(":w")
```

### Empty output ≠ command not executed

PTY is a stream. `read_output` immediately after `send_input` may return empty because the command is still running. Use the marker pattern (Rule 1) to wait for completion before reading.

## Tool Reference

| Tool | Purpose |
|------|---------|
| `list_hosts` | List hosts from `~/.ssh/config` |
| `bootstrap_host` | Deploy ed25519 public key (first-time only, idempotent). Do NOT call on `auth=password` hosts unless the user asks to switch to key auth |
| `open_session` | Open new PTY session (persistent=true for cross-restart). May trigger a user password prompt on `auth=password` hosts — do not assume immediate return |
| `attach_remote_session` | Attach to existing persistent session on daemon |
| `list_remote_sessions` | List daemon-hosted sessions on a host |
| `send_input` | Write raw bytes to PTY (no auto-append `\n`) |
| `read_output` | Read output: default / wait_for / tail_lines / since_cursor. `strip_ansi=true` removes terminal control sequences |
| `send_control` | Ctrl+C / Ctrl+D / Ctrl+Z |
| `resize` | Resize PTY (cols, rows) |
| `detach_session` | Detach but keep remote PTY alive |
| `close_session` | Close and terminate remote shell |
| `reconnect_session` | Recover a Lost session |
| `sftp_*` | File transfer / mkdir / list / remove / chmod |
| `detect_remote_env` | Probe remote OS / shell / tools (independent SSH exec) |
| `get_session_timeline` | Get session event timeline |

## The 7 Rules

### Rule 1: Completion — use marker, not prompt guessing

For commands that return control to the shell, use a completion marker with a unique request ID and exit code:

```bash
command
printf '\n__TB_DONE__:%s:%s\n' "$REQID" "$?"
```

- `$REQID`: unique ID per command (e.g. 5-hex `a3f1c`), prevents stale marker match
- `$?`: exit code (0 = success)
- `\n` prefix isolates marker on its own line
- `printf` (not `echo`) avoids PTY echo re-matching the marker literal

**Never** use `command && echo DONE` (fails on error, marker disappears) or `command; echo DONE` (no exit code).

### Rule 2: Timeout ≠ failure

`read_output` timeout only ends this call. The remote command keeps running, session stays Ready.

On timeout, choose:
- **Continue waiting** → call `read_output(wait_for=marker)` again
- **Interrupt** → `send_control("ctrl_c")`, then `read_output` to confirm
- **Let it run in background** → use `since_cursor` to poll periodically

**Never auto-retry on timeout.** The original command may still be running; retrying creates duplicate execution.

### Rule 3: Disconnect = UNKNOWN state

If `send_input` or `read_output` returns a connection error, the remote execution state is **UNKNOWN** (not FAILED). The command may have:
- completed before disconnect
- partially executed
- never started

**Never blindly retry.** Instead:
1. `reconnect_session(session_id)`
2. Run an idempotency check (see Rule 4)
3. Only retry if confirmed NOT-RUN

### Rule 4: Retry = reconnect + idempotency check first

Before retrying any command with side effects:

```bash
# Idempotency check examples
test -f /tmp/marker && echo EXISTS || echo MISSING      # file created?
systemctl is-active <service>                            # service restarted?
dpkg -l | grep -q "^ii  <pkg> " && echo INSTALLED       # package installed?
grep -q "expected" /etc/config && echo APPLIED           # config applied?
```

- EXISTS / INSTALLED / APPLIED / active → command already ran, do NOT retry
- MISSING / inactive → safe to retry

### Rule 5: Interactive/TUI — no marker

These programs occupy the foreground PTY and never return to shell, so markers never appear:

| Type | Examples | Exit with |
|------|----------|-----------|
| Full-screen TUI | `vim`, `nano`, `htop`, `top`, `less` | `:q` / `q` |
| Long-running monitor | `tail -f`, `journalctl -f`, `watch` | `send_control("ctrl_c")` |
| Nested shell | `ssh`, `bash`, `python`, `mysql` | `exit` / `send_control("ctrl_d")` |
| Interactive prompt | `read -p`, `passwd`, `sudo` (no TTY) | complete input, returns to shell |

For these, do NOT use completion markers. Use application-specific exit keys or `send_control`.

**Judgment criterion**: if the shell prompt will NOT reappear after the command, do not use marker mode.

### Rule 6: Cursor — for full output, use since_cursor

`wait_for` returns only the match context (matched line ± context_lines), **not** the full command output.

To get full output:

```
1. cursor_before = read_output(tail_lines=0).cursor
2. reqid = generate_unique_id()
3. send_input("command; printf '\\n__TB_DONE__:%s:%s\\n' \"$reqid\" \"$?\"\n")
4. r = read_output(wait_for="__TB_DONE__:<reqid>:", timeout_secs=60)
5. exit_code = parse from r.matched_text
6. full_output = read_output(since_cursor=cursor_before)
```

**Never** pass both `wait_for` and `since_cursor` in the same call — `since_cursor` takes priority and `wait_for` is silently ignored.

For precise text matching, strip ANSI sequences with regex `\x1b\[[0-9;?]*[a-zA-Z]`.

### Rule 7: Persistent — detach keeps PTY alive, attach resumes

- `detach_session` → local client disconnects, remote PTY keeps running, RingBuffer keeps accumulating
- `list_remote_sessions(host)` → list daemon-hosted sessions
- `attach_remote_session(host, remote_session_id)` → resume, returns buffered output since last cursor

Daemon crash = all detached sessions lost (Phase 3 does not recover). Must `open_session(persistent=true)` to rebuild.

## Decision Table

| Situation | Action |
|-----------|--------|
| First connection to a key-auth host | `bootstrap_host` (one-time, deploys SSH key) |
| First connection to an `auth=password` host | `open_session` directly — user is prompted for password; do NOT `bootstrap_host` unless the user explicitly asks to switch to key auth |
| `open_session` on an `auth=password` host | expect a user password prompt (out-of-band); do not assume immediate return |
| Subsequent connections | `open_session` (key auth, no password) |
| Plain command (ls, cat, grep) | `send_input` + marker + `read_output(wait_for)` |
| Need full command output | record cursor → send → wait_for → `read_output(since_cursor)` |
| Need clean text output (no ANSI) | `read_output(..., strip_ansi=true)` — removes CSI/OSC/DCS sequences |
| Long-running command (build) | do NOT retry on timeout; `since_cursor` poll |
| Watch output (tail -f) | `send_input` + `read_output(tail_lines)` periodically; `ctrl_c` to stop |
| vim / top / htop | no marker; use app exit keys (`:q`, `q`) or `ctrl_c` |
| `sudo -n` (non-interactive) | Auto-passthrough: `sudo -n ls`, `sudo -n du`, `sudo -n stat` etc. execute without confirmation. Must be `sudo -n` at line start, no shell chaining (`;` `&&` `|` etc.) |
| sudo (interactive, no `-n`) | Triggers `POLICY_NEEDS_CONFIRM`. Do NOT send password via `send_input`. Options: (1) use `sudo -n` if NOPASSWD; (2) ask user to run `termbridge session approve <session_id>` for unrestricted session; (3) execute manually in target terminal |
| SSH disconnect mid-command | state UNKNOWN → `reconnect_session` + idempotency check |
| Retry a side-effecting command | reconnect first, idempotency check, retry only if NOT-RUN |
| Cross-restart resume | `list_remote_sessions` → `attach_remote_session` |
| Resize terminal | `resize(session_id, cols, rows)` before running TUI |
| Edit remote file | `sftp_transfer(download)` → Edit → `sftp_transfer(upload)`. Local paths allowed under cwd or `$TEMP/termbridge` |
| Done with session | `close_session` (terminates remote shell) |
| Want to keep session for later | `detach_session` (PTY keeps running) |

## Key Constraints

- **`send_input` is raw bytes**: no auto-append `\n`. You must include `\n` yourself for Enter. No command boundary parsing.
- **`\r` is preserved**: do not strip carriage returns from input.
- **PTY echo**: the shell echoes input back. `wait_for("MARKER")` may match the echo — use `$REQID` variable so echo contains the variable name, not the value.
- **ANSI noise**: PTY output contains control sequences (bracketed paste `\x1b[?2004h/l`, OSC 7 `\x1b]7;...`, color codes, `\x1b[K`). Use `read_output(strip_ansi=true)` for clean text; RingBuffer keeps raw bytes, so cursor stays valid.
- **Credentials never in MCP params**: `bootstrap_host` accepts only `host`. Passwords are prompted via separate `termbridge-auth-helper` process, never exposed to MCP stdio.
- **Host policy is user intent (ADR-0017 §2.2)**: `hosts.toml` `auth=password` means password for every connection — it is NOT a gap to "fix". `bootstrap_host` never modifies host policy (returns a `hint` instead), so an `auth=password` host keeps prompting until the user edits hosts.toml. Never bootstrap a password-policy host as a convenience optimization.
- **Session approval mode (0.2.1+)**: Sessions start in `standard` mode (PolicyManager enforces blocklist + confirm). Users can elevate a session to `unrestricted` via `termbridge session approve <session_id>` (CLI, human-only). In unrestricted mode, **only confirm guardrails are skipped** (sudo, `rm -rf /tmp/x`, etc. execute without confirmation); **hard-deny rules still apply** (`rm -rf /`, `mkfs`, `dd of=/dev/`, etc. are still blocked). This does NOT bypass SSH credentials, path safety, or protocol invariants. Approval is session-scoped — closing the session resets to standard. Agent cannot self-approve; only the human user can via CLI. TermBridge Policy is a secondary guardrail (defense-in-depth), not a primary approval system — command approval is the Coding Agent's responsibility.

## Anti-Patterns

```python
# BAD: timeout → retry (creates duplicate execution)
r = read_output(wait_for="MARKER", timeout_secs=5)
if r.timed_out: send_input("command\n")  # original may still be running!

# BAD: send_input error → retry without reconnect (session Lost)
try: send_input("cmd\n")
except: send_input("cmd\n")  # will fail again, session is Lost

# BAD: marker on top (never appears, false "hung" diagnosis)
send_input("top\n")
read_output(wait_for="__TB_DONE__:", timeout_secs=10)  # top is TUI!

# BAD: depend on wait_for return as full output
r = read_output(wait_for="MARKER")
full = r.output  # only match context, not full output

# BAD: echo marker (PTY echo re-matches)
send_input("echo __TB_DONE__:abc:0\n")  # echo itself contains the marker!

# BAD: auto-bootstrap on a password-policy host (violates user intent, ADR-0017 §3.3)
bootstrap_host(host="prod")  # policy says auth=password — user wants password auth;
                             # bootstrap also does NOT change hosts.toml, so the prompt stays

# BAD: sudo without -n (triggers POLICY_NEEDS_CONFIRM, blocks automation)
send_input("sudo systemctl restart nginx\n")  # blocked!

# GOOD: sudo -n for NOPASSWD hosts (auto-passthrough)
send_input("sudo -n systemctl restart nginx\n")

# GOOD: if sudo needs password, ask user to approve session first
# User runs: termbridge session approve <session_id>
# Then: sudo commands execute without policy confirmation
```

## Quick Reference: Happy Path

```
# First time on a host
list_hosts
bootstrap_host(host="myserver")  # key-auth host only; auth=password hosts: skip (rule §3.3)
open_session(host="myserver", persistent=true, name="work")

# Run a command with completion marker
reqid = "a3f1c"
cursor = read_output(session_id, tail_lines=0).cursor
send_input(session_id, "systemctl status nginx; printf '\\n__TB_DONE__:%s:%s\\n' \"$reqid\" \"$?\"\n")
r = read_output(session_id, wait_for="__TB_DONE__:a3f1c:", timeout_secs=30)
exit_code = parse_exit_code(r.matched_text)
full_output = read_output(session_id, since_cursor=cursor)

# Done
close_session(session_id)
```
