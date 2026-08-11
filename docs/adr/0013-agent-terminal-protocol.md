# ADR-0013：Agent Terminal Protocol

- **Status**: Accepted
- **Date**: 2026-08-11
- **Phase**: 6-C
- **Supersedes**: —
- **Depends on**: ADR-0012（执行语义契约）、ADR-0011（send_input 语义）、ADR-0004（persistent runtime）、ADR-0010（reconnect）

## 1. Motivation

ADR-0012 定义了 9 大 runtime 契约（Input / Output / Cursor / Waiter / Timeout / Disconnect / Attach / Completion / Ownership），33/33 P0 测试通过。但 ADR-0012 是 **runtime 侧契约**——它规定 TermBridge 保证什么。

实际使用中，Agent 是否能正确使用这些契约是另一个问题。ADR-0011 的 20/20 测试和 ADR-0012 的 33/33 测试中，Agent 犯过的错包括：

- `wait_for` timeout → 误判命令失败 → 重试 → 服务器上出现两个并发任务
- `send_input` 返回 Err → 误判命令未执行 → 盲目重试 → 重复执行
- 对 `top` / `vim` 使用 marker completion → marker 永远不出现 → 误判卡死
- 依赖 `wait_for` 返回值做完整输出匹配 → 只拿到匹配上下文，丢失命令输出

这些都不是 runtime bug，而是 **Agent 使用规范缺失**。

本 ADR 定义 **Agent Terminal Protocol**——ADR-0012 的消费者侧规范。它不重复 runtime 契约，而是规定 Agent 如何正确消费这些契约。

## 2. 适用范围

### 2.1 适用：返回 shell 的命令

Protocol 适用于最终将控制权返回当前 shell 的命令序列，包括：

- 短命令：`ls` / `cat` / `echo` / `grep` / `find`
- 长命令：`apt install` / `cargo build` / `systemctl restart`
- 管道与重定向：`cmd | grep x > file`
- 后台任务：`(sleep 5; echo done) &`（控制权立即返回 shell）
- 命令序列：`cmd1 && cmd2 || cmd3`

这些命令的特征：执行完毕后 shell prompt 重新出现，Agent 可以继续 `send_input`。

### 2.2 不适用：TUI / 交互式 / 长驻程序

Protocol **不适用**于占用 foreground PTY 且不返回 shell 的程序：

| 类型 | 示例 | 退出方式 |
|---|---|---|
| 全屏 TUI | `vim` / `nano` / `htop` / `top` / `less` / `more` | 应用特定键（`:q` / `q`） |
| 监控长驻 | `watch` / `tail -f` / `journalctl -f` | `Ctrl+C` |
| 嵌套 shell | `ssh another-host` / `bash` / `python` / `mysql` | `exit` / `Ctrl+D` |
| 交互式 prompt | `read -p` / `passwd` / `sudo`（无 TTY 免密时） | 输入完成后返回 shell |

这些程序的特征：占用 foreground PTY，shell prompt 不出现，marker 永远不会输出。Agent 必须使用规则 5（Interactive/TUI 模式）。

## 3. 七条规则

### 规则 1：Completion——用 operation_id + completion marker 关联命令与结果

**陈述**：对于适用范围（§2.1）内的命令，Agent 必须使用固定 marker 格式 `__TB_DONE__:<request_id>:<exit_code>` 关联命令与结果，从 marker 解析 exit_code 判断命令完成/失败。marker 必须放在命令最后，确保命令执行完毕后才输出。

**反例**：

```bash
# BAD: 命令失败时 marker 不出现，Agent 无法区分"失败"和"仍在运行"
command && echo __DONE__

# BAD: marker 总是出现但 Agent 不知道 exit code
command; echo __DONE__

# BAD: 把 marker 字面量写进命令文本，PTY 回显触发 wait_for 提前匹配
send_input("echo __TB_DONE__:abc:0\n")
```

**正确做法**：

```bash
# 用 printf 输出 marker，用 $REQID 变量使回显不含完整 marker 字面量
# $REQID 是 Agent 生成的唯一 ID（如 5 位 hex），每次命令不同
command
printf '\n__TB_DONE__:%s:%s\n' "$REQID" "$?"
```

- `\n` 前缀确保 marker 独立成行，避免与 shell 提示符交错
- `$REQID` 变量使回显含变量名而非具体值，`wait_for` 搜索具体值不会被回显触发
- `$?` 捕获命令 exit code，Agent 从 marker 第二个冒号后解析（0=成功，非 0=失败）

**契约引用**：ADR-0012 契约 ⑧ Completion（marker + exit-code + cursor boundary）

---

### 规则 2：Timeout——不得直接认为 command failed

**陈述**：`read_output` timeout 只结束本次调用，远端命令仍运行，session 仍 Ready（ADR-0012 契约 ⑤）。Agent 不得将 timeout 等同于命令失败或卡死，必须主动决策：继续等待、中断命令、或让命令后台运行。

**反例**：

```python
# BAD: timeout → 判定失败 → 重试 → 服务器上出现两个并发任务
r = read_output(wait_for="MARKER", timeout_secs=3)
if r.timed_out:
    retry_command()  # 危险！原命令可能仍在运行
```

**正确做法**：

```python
r = read_output(wait_for="MARKER", timeout_secs=60)
if r.timed_out:
    # 命令仍在远端运行，session_state=ready
    # 选项 A: 继续等待
    r = read_output(wait_for="MARKER", timeout_secs=60)
    # 选项 B: 主动中断
    send_control(ctrl+c)
    read_output(wait_for="^C")  # 验证中断
    # 选项 C: 让命令继续后台运行（若 Agent 期望长任务）
    # 用 since_cursor 周期性读输出
```

**契约引用**：ADR-0012 契约 ⑤ Timeout（不改变远端执行状态）

---

### 规则 3：Disconnect——execution state = UNKNOWN，不得盲目 retry

**陈述**：连接异常后，远端执行状态为 UNKNOWN（不是 FAILED）。UNKNOWN 是临时状态，不是终态（ADR-0012 §2.6 执行三态模型）。Agent 必须通过 reconnect + 幂等检查将 UNKNOWN 解析为终态：COMPLETED（命令已执行）或 NOT-RUN（命令未执行，可安全重试）。

**反例**：

```python
# BAD: send_input 返回 Err → 判定命令未执行 → 立即重试 → 重复执行
try:
    send_input("systemctl restart wazuh-agent\n")
except:
    send_input("systemctl restart wazuh-agent\n")  # 危险！可能已执行过
```

**正确做法**：

```python
# Agent 发送 systemctl restart wazuh-agent
# SSH 恰好在命令执行后断开 → TermBridge 报告 UNKNOWN
# Agent 不应盲目重试，而应:
reconnect_session(session_id)                              # ADR-0010
# 幂等检查
send_input("systemctl is-active wazuh-agent\n")           # 幂等查询
r = read_output(wait_for="active|inactive")
# active → COMPLETED（命令已执行，无需重试）
# inactive → NOT-RUN（命令未执行，可安全重试）
```

**关键**：`send_input` 成功 = 字节已写入 SSH channel 发送缓冲区，**不等于**远端 shell 已读取并执行该命令。断线后 Agent 无法判断命令执行到哪一步。

**契约引用**：ADR-0012 契约 ⑥ Disconnect（执行状态未知）+ §2.6 执行三态模型

---

### 规则 4：Retry——优先 reconnect + idempotency check

**陈述**：重试前必须先 `reconnect_session` 恢复连接，再用幂等性检查确认命令是否已执行。幂等检查通过后，根据结果决定是否重试原命令。不得在未重连、未幂等检查的情况下直接重发原命令。

**反例**：

```python
# BAD: 未 reconnect 直接 send_input → session_state=Lost，调用失败
send_input("command\n")  # 失败

# BAD: reconnect 后直接重发原命令 → 命令已执行过，造成重复
reconnect_session(session_id)
send_input("systemctl restart wazuh-agent\n")  # 二次重启！
```

**正确做法**：

```python
# session_state = Lost
# Agent 决定重试
reconnect_session(session_id)
# session_state = Ready（新 buffer，旧 buffer 不保留）

# 幂等检查
send_input("test -f /tmp/marker && echo EXISTS || echo MISSING\n")
r = read_output(wait_for="EXISTS|MISSING")
# EXISTS → 命令已执行，无需重试
# MISSING → 命令未执行，可安全重试原命令
```

**幂等检查模式库**（附录 §5 提供常见命令的幂等检查模板）

**契约引用**：ADR-0012 契约 ⑥ + ADR-0010 reconnect

---

### 规则 5：Interactive/TUI——不使用 completion marker protocol

**陈述**：对 TUI / 交互式 / 长驻程序（§2.2），Agent 不得使用 exit-code marker completion。必须使用 `send_control`（Ctrl+C / Ctrl+D）、应用特定退出键（vim 的 `:q`、top 的 `q`）、或 session_state 判断。

**反例**：

```python
# BAD: 对 top 使用 marker completion → marker 永远不出现 → 误判卡死
send_input("top\n")
r = read_output(wait_for="__TB_DONE__:", timeout_secs=10)
if r.timed_out:
    raise Exception("top hung!")  # 误判！top 是 TUI，不返回 shell
```

**正确做法**：

```python
# 识别 TUI 程序 → 用应用特定方式退出
# top/htop/watch → send_control("q") 或 send_control(ctrl+c)
# vim → send_input(":q\n")
# less/more → send_control("q")
# tail -f → send_control(ctrl+c)
# ssh another-host → send_input("exit\n") 或 send_control(ctrl+d)

# 程序退出后等待 shell prompt 回显
r = read_output(wait_for="\\$")  # 等待 prompt
```

**判断标准**：如果命令执行后 shell prompt 不会重新出现，就不适用 marker completion。

**契约引用**：ADR-0012 §2.5 Completion Protocol 适用范围

---

### 规则 6：Cursor——需要完整输出时使用 cursor，不依赖 wait_for 返回值

**陈述**：`wait_for` 命中后返回的是匹配上下文（匹配行 ± context_lines），**不返回全部输出**。Agent 需要完整命令输出时，必须先记录命令前 cursor 位置，`wait_for` 命中后用 `since_cursor=cursor_before` 读取完整输出。

**关键约束**：`wait_for` 调用本身**不带** `since_cursor`（两者互斥，`since_cursor` 优先级更高，`wait_for` 会被忽略——ADR-0003 四模式互斥）。

**反例**：

```python
# BAD: 依赖 wait_for 返回值作为完整输出 → 只拿到匹配上下文
r = read_output(wait_for="MARKER")
full_output = r.output  # 只有匹配行 ± context_lines，丢失命令实际输出

# BAD: 同时传 wait_for + since_cursor → since_cursor 优先生效，wait_for 被忽略
r = read_output(wait_for="MARKER", since_cursor=cursor_before)
# Agent 误以为 marker 出现了，实际 wait_for 没生效
```

**正确做法**：

```python
# 1. 记录命令前 cursor 位置
r0 = read_output(tail_lines=0)
cursor_before = r0.cursor

# 2. 生成唯一 request_id
reqid = generate_short_id()

# 3. 发送命令 + marker
send_input("command; printf '\\n__TB_DONE__:%s:%s\\n' \"$reqid\" \"$?\"\n")

# 4. wait_for（不带 since_cursor，靠唯一 reqid 防止旧 marker 匹配）
r = read_output(wait_for=f"__TB_DONE__:{reqid}:", timeout_secs=60)

# 5. 命中后用 since_cursor 读取完整输出
if r.matched:
    exit_code = parse_exit_code(r.matched_text)
    full_output = read_output(since_cursor=cursor_before)
    # 如需精确文本匹配，自行剥离 ANSI：正则 \x1b\[[0-9;?]*[a-zA-Z]
```

**契约引用**：ADR-0012 契约 ③ Cursor + 契约 ④ Waiter + 契约 ⑧ Completion

---

### 规则 7：Persistent——detach 后通过 remote session list + attach 恢复

**陈述**：persistent session detach 后，远端 PTY 由 daemon 保活，RingBuffer 持续累积输出。Agent 重连时必须通过 `list_remote_sessions(host)` 列出远端 session，再用 `attach_remote_session(host, session_id)` attach 回去。daemon 崩溃后 session 丢失（Phase 3 不恢复），必须重新 `open_session(persistent=true)` 重建。

**反例**：

```python
# BAD: detach 后误以为 session 丢失，重新 open_session → 旧 session 仍在 daemon 中运行，资源泄漏
detach_session(session_id)
# ... MCP 重启 ...
new_session = open_session(host, persistent=true)  # 创建新 session，旧的还在！

# BAD: attach 时不带 since_cursor → 从 0 开始读，重复读已读过的数据
attach_remote_session(host, session_id)  # 没传 since_cursor
```

**正确做法**：

```python
# 跨 MCP 重启恢复流程
# 1. termbridge.exe 重启后，daemon 进程不受影响

# 2. 列出远端 session
sessions = list_remote_sessions(host)
# → 返回 Vec<RemoteSessionInfo>，含 id / name / state / written

# 3. 选中目标 session（可通过 name 或 written 字段识别）
target = select_session_by_name(sessions, "my-session")

# 4. attach（带 since_cursor 做增量恢复）
result = attach_remote_session(host, target.id)
# daemon 增量返回 since_cursor → written 之间的数据

# 5. 检查 is_truncated
if result.is_truncated:
    # since_cursor 已被 RingBuffer 截断
    # 选项 A: 接受部分丢失，从当前最早位置继续读
    # 选项 B: tail_lines 兜底，拉 RingBuffer 当前全部内容的尾部 N 行

# 6. session_state = Ready，可继续 send_input / read_output
```

**daemon 崩溃语义**：daemon 进程崩溃 = 所有 detached session 丢失（Phase 3 不恢复）。client 侧 session 检测到 socket EOF → 状态转 `Lost` → 必须重新 `open_session(persistent=true)` 重建。

**契约引用**：ADR-0012 契约 ⑦ Attach（精确恢复输出位置）+ ADR-0004 §5 Buffer 归属

## 4. Consequences

### 4.1 对 Agent 的约束

- Agent 必须实现 `generate_short_id()` 生成唯一 request_id（如 5 位 hex）
- Agent 必须维护 per-session 的 `cursor_before` 状态，用于 `since_cursor` 读取完整输出
- Agent 必须识别 TUI 程序并切换到 Interactive/TUI 模式（规则 5）
- Agent 必须实现幂等检查逻辑，不得盲目重试（规则 3/4）
- Agent 必须理解 `wait_for` 返回值仅为匹配上下文，不依赖其作为完整输出（规则 6）

### 4.2 对 TermBridge 的约束

- TermBridge 不为 Protocol 增加任何 runtime 功能——Protocol 是纯消费者侧规范
- TermBridge 保持 9 大契约不变（ADR-0012）
- 未来 CLI / GUI / Playbook 等上层消费者都必须遵守本 Protocol

### 4.3 对上层消费者的影响

| 消费者 | 影响 |
|---|---|
| Claude Code / Codex / OpenCode 等 AI Agent | 必须遵守 7 条规则，特别是规则 1/2/3（completion/timeout/disconnect） |
| CLI | 自动化脚本需实现 marker 模式和幂等检查 |
| GUI | 可向用户暴露 cursor / marker 状态，但底层仍遵守 Protocol |
| Playbook | 每个 step 必须是幂等的，断线后可安全重试 |

## 5. 附录：幂等检查模式库

常见命令的幂等检查模板。Agent 重试前必须先执行幂等检查，确认命令是否已执行。

### 5.1 文件操作

```bash
# touch（幂等检查）
test -f /tmp/marker && echo EXISTS || echo MISSING

# mkdir（幂等检查）
test -d /path/to/dir && echo EXISTS || echo MISSING

# cp/mv（幂等检查）
test -f /target/path && echo EXISTS || echo MISSING
```

### 5.2 服务管理

```bash
# systemctl restart（幂等检查）
systemctl is-active wazuh-agent
# active → 已重启（无需重试）
# inactive → 未重启或重启失败（需排查）

# systemctl enable（幂等检查）
systemctl is-enabled wazuh-agent
# enabled → 已启用
# disabled → 未启用
```

### 5.3 包安装

```bash
# apt install（幂等检查）
dpkg -l | grep -q "^ii  package-name " && echo INSTALLED || echo MISSING

# cargo build（幂等检查）
test -f target/release/binary && echo EXISTS || echo MISSING
# 注意：cargo build 不是天然幂等，需结合 git commit hash 验证版本
```

### 5.4 配置修改

```bash
# 文件修改（幂等检查）
grep -q "expected-content" /etc/config/file && echo APPLIED || echo MISSING

# sed 替换（幂等检查）
grep -c "replaced-pattern" /etc/config/file
# 0 → 未替换
# >0 → 已替换（次数）
```

## 6. 测试矩阵

本 Protocol 的测试由 ADR-0012 P0 测试矩阵覆盖（33/33 PASS），不单独维护测试脚本。关键对应关系：

| 规则 | 测试覆盖 | ADR-0012 测试 |
|---|---|---|
| 规则 1 Completion | T10（命令失败状态）+ T11（marker 提前出现）+ T13（cursor 隔离） | ✅ |
| 规则 2 Timeout | T12（timeout 后 session 状态） | ✅ |
| 规则 3 Disconnect | T15（disconnect 中途写入） | ✅ |
| 规则 4 Retry | T15（幂等检查：touch 已执行 → FILE_EXISTS） | ✅ |
| 规则 5 Interactive/TUI | ADR-0011 T5（Ctrl+C）/ T6（Ctrl+D）/ T7（Ctrl+Z） | ✅ |
| 规则 6 Cursor | T13（since_cursor 精确切片）+ T17（attach cursor 边界） | ✅ |
| 规则 7 Persistent | T17（detach/attach cursor 精确恢复）+ cross-restart E2E | ✅ |

## 7. References

- [ADR-0012](0012-execution-state-and-completion-protocol.md)：执行语义契约（9 大契约 + Agent Terminal Protocol 雏形）
- [ADR-0011](0011-input-semantics-and-execution-safety.md)：send_input 语义 + 8 条 Agent 最佳实践
- [ADR-0004](0004-remote-persistent-runtime.md)：persistent runtime 架构 + detach/attach 语义
- [ADR-0010](0010-session-reconnect.md)：session reconnect
- [ADR-0008](0008-scope-boundary.md)：TermBridge 定位与职责边界
