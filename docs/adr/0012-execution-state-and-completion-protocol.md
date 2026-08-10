# ADR-0012：Execution State, Completion Protocol & Recovery Semantics

- **Status**: Accepted（9 契约 + Agent Terminal Protocol + P0 测试矩阵 25/25 PASS）
- **Date**: 2026-08-10
- **Phase**: 6-C
- **Supersedes**: —

## 1. Motivation

ADR-0011 完成了 `send_input` 字节透传 + 执行语义安全 P0 测试（20/20 PASS），验证了「命令正确传递到远端 PTY」。

但测试中暴露的 3 个 `wait_for` 问题（回显匹配、仅返回上下文、ANSI 干扰）揭示了一个更深的层面：

> **TermBridge 返回给 Agent 的状态，是否足以让 Agent 正确判断命令已完成、失败或仍在运行？**

这不是「字节透传」问题，而是「执行状态契约」问题。典型陷阱：

1. `wait_for` 命中 ≠ 命令成功（marker 可能出现但命令失败，或 marker 出现在回显中）
2. `wait_for` 可能命中**旧命令残留的 marker**（不属于当前命令）
3. `read_output` timeout ≠ 命令结束（远端命令仍在运行）
4. `send_input` 返回错误 ≠ 命令未执行（网络瞬断时无法判断远端是否已处理）
5. 并发 `send_input` 可能导致 PTY 字节交错（命令拼接错乱）

本 ADR 固化 9 个底层契约 + Agent Terminal Protocol，作为 TermBridge 作为 **Agent Terminal Runtime** 的语义层地基。**在此之上才谈得上 Agent、CLI、GUI、Playbook 等上层功能。**

## 2. Execution Model

### 2.1 九大底层契约

| # | 契约 | 含义 | 当前状态 |
|---|---|---|---|
| ① | Input：bytes 原样传递 | `send_input` 不修改、不追加 `\n`、不解析命令边界 | ✅ 已落地（ADR-0011） |
| ② | Output：raw bytes 保留 | RingBuffer 存原始 PTY 字节，含 ANSI / `\r` / 控制字符，永不清洗 | ✅ 已落地（ADR-0003） |
| ③ | Cursor：单调、可恢复 | `written` 单调递增，`since_cursor` 精确切片，溢出时 `is_truncated=true` | ✅ 已落地（ADR-0003） |
| ④ | Waiter：不消费、不丢数据 | `wait_for` 命中推进 `mark_cursor`，不影响 `since_cursor` 路径；多 waiter 不互斥 | ✅ 已落地（ADR-0003） |
| ⑤ | Timeout：不改变远端执行状态 | `read_output` timeout 只结束本次调用，远端命令仍运行，session 仍 Ready | ✅ 已落地（ADR-0003 契约 6） |
| ⑥ | Disconnect：执行状态未知 | `send_input` 成功 = 数据已交给 SSH transport；断线后远端执行状态 **unknown** | ⬜ 需文档明确（本 ADR） |
| ⑦ | Attach：精确恢复输出位置 | detach/attach 后 `since_cursor` 精确恢复，恰好一次、不重复、不遗漏 | ⬜ Phase 3 daemon 路径 |
| ⑧ | Completion：marker + exit-code + cursor boundary | `wait_for` 命中 ≠ 命令成功；Agent 必须用 exit-code-in-marker + cursor boundary | ⬜ 需文档明确（本 ADR） |
| ⑨ | Input Ownership：单 writer 串行 | 一个 session 同一时刻只有一个 active writer；`send_input` 必须串行；多 reader 并行 | ⬜ 需文档明确（本 ADR） |

### 2.2 Input 语义（契约 ①）

继承 ADR-0011：`send_input` 纯字节透传，零修改。不追加 `\n`，不剥离 `\r`，不解析命令边界。

### 2.3 Output 语义（契约 ②）

继承 ADR-0003：RingBuffer 存原始 PTY 字节。**无论是否提供 normalized view，RingBuffer 永远保存 raw bytes**，支持终端回放、ANSI 分析、调试。

`wait_for` 当前对 raw bytes 做正则匹配。Agent 侧如需精确文本匹配，自行剥离 ANSI（正则 `\x1b\[[0-9;?]*[a-zA-Z]`）。

**未来可选**（非 MVP）：`read_output` 增加 `match_mode` 参数：
```json
{ "wait_for": "Started", "match_mode": "ansi_stripped" }
```
- `raw`（默认）：对 raw bytes 匹配
- `ansi_stripped`：匹配前剥离 ANSI 序列（不影响 buffer 本身）

### 2.4 Cursor 语义（契约 ③④）

继承 ADR-0003：`written` 单调递增，`since_cursor` 精确切片，`mark_cursor` 由 `wait_for`/settle 推进。两条 cursor 路径相互独立。

**关键约束**（契约 ⑧ 补充）：`wait_for` **不保证**命中的 marker 属于当前命令。buffer 中可能残留旧命令的 marker，`wait_for` 会匹配到。Agent **必须**建立 cursor boundary + 用唯一 request_id 双重保障：

```
cursor_before = current written cursor                    # 记录命令前位置
send_input("command; printf '\\n__TB_DONE__:%s:%s\\n' \"$REQID\" \"$?\"")
r = read_output(wait_for="__TB_DONE__:$REQID:")           # 等 marker（不带 since_cursor）
full_output = read_output(since_cursor=cursor_before)     # 读完整输出
```

**注意**：`read_output` 的四种模式互斥（优先级 `since_cursor > tail_lines > wait_for > settle`，ADR-0003）。`wait_for` 不能与 `since_cursor` 同时使用——同时传 `since_cursor` 优先生效，`wait_for` 被忽略。因此：
- `wait_for` 调用**不带** `since_cursor`，靠唯一 `$REQID` 防止旧 marker 匹配
- `cursor_before` 用于命中后 `since_cursor` 读取完整输出，**不**用于限定 `wait_for` 扫描范围

### 2.5 Completion 语义（契约 ⑧）

#### Agent Terminal Protocol —— 固定 marker 格式

```text
__TB_DONE__:<request_id>:<exit_code>
```

- `__TB_DONE__`：固定前缀，所有 Agent 统一使用
- `<request_id>`：Agent 生成的短唯一 ID（如 5 位 hex），用于关联命令与 marker，防止旧 marker 匹配
- `<exit_code>`：命令退出码（`$?`）

**命令模式**：
```bash
command
printf '\n__TB_DONE__:%s:%s\n' "$REQID" "$?"
```

**Agent 行为**：
```
1. r0 = read_output(tail_lines=0)                       # 拿当前 written 作为 cursor_before
   cursor_before = r0.cursor
2. reqid = generate_short_id()                          # 生成唯一 ID
3. send_input("command; printf '\\n__TB_DONE__:%s:%s\\n' \"$reqid\" \"$?\"\n")
4. r = read_output(wait_for="__TB_DONE__:$reqid:", timeout_secs=60)
   # 注意：不带 since_cursor（与 wait_for 互斥，ADR-0003）
5. if r.timed_out: command 仍在运行（契约 ⑤），可 send_control(ctrl+c) 或继续等
6. if r.matched: 解析第二个冒号后的数字 = exit code（0=成功，非0=失败）
7. full_output = read_output(since_cursor=cursor_before)  # 读取完整输出
```

#### 模式对比

| 模式 | 示例 | 问题 |
|---|---|---|
| **BAD** | `command && echo __DONE__` | 命令失败时 marker 不出现，Agent 无法区分「失败」和「仍在运行」 |
| **BETTER** | `command; echo __DONE__` | marker 总是出现，但 Agent 不知道 exit code |
| **BEST** | `command; printf '__TB_DONE__:%s:%s\n' "$reqid" "$?"` | marker + request_id + exit code 闭环 |

#### 为什么不用 `exit "$rc"`

交互式 PTY 中 `exit` 会关闭 shell → session 进入 Lost。退出码通过 marker 文本传递即可。

#### marker 防回显匹配（ADR-0011 发现）

marker 字面量若出现在命令文本中，PTY 回显会触发 `wait_for` 提前匹配。**解法**：用 shell 算术展开构造 marker 固定前缀，或用 `$reqid` 变量使回显不含完整 marker 字面量。

#### TermBridge 不验证 marker 位置

TermBridge 不解析 shell，不判断 marker 是否在「命令末尾」。**Completion 正确性是 Agent protocol 责任**。Agent 必须把 marker 放在命令最后，确保命令执行完毕后才输出 marker。

### 2.6 Disconnect 语义（契约 ⑥）

```text
send_input("touch /tmp/test; sleep 20\n")
    ↓
网络瞬断 / sshd 被杀
    ↓
send_input 返回 Err（SSH channel 已断）或 read task 检测到 EOF
    ↓
session_state → Lost
```

**TermBridge 的承诺**：

```
Normal operation:
  send_input success = bytes accepted by local SSH transport

After disconnect:
  remote execution state = UNKNOWN
```

- `send_input` 成功 = 数据已写入 SSH channel 的发送缓冲区
- `send_input` 成功 **≠** 远端 shell 已读取并执行该命令
- 发生连接异常后，**无法可靠判断**远端是否已处理该输入

**TermBridge 不提供 exactly-once 语义，也不提供 at-least-once 语义。** 断线后的远端执行状态是 **unknown**：

| 情况 | 远端实际状态 |
|---|---|
| A | 数据未到达远端，命令未执行 |
| B | 远端 shell 已接收，命令正在执行 |
| C | 命令已执行完毕，响应丢失 |

Agent 重试时**必须**：
1. `reconnect_session` 恢复连接（ADR-0010）
2. 用**幂等性检查**确认命令是否已执行（如 `test -f /tmp/test`）
3. **不要假设**「请求失败 = 命令未执行」

## 3. Concurrency Rules（契约 ⑨）

### Reader：多并发

多个 `read_output` 调用可并发执行（`since_cursor` / `tail_lines` / `wait_for` 均为只读操作）。它们：
- 不互相消费数据
- 不推进彼此的 `since_cursor`
- `wait_for` 通过 `Notify` 唤醒所有等待者

### Writer：串行

一个 session 同一时刻**只有一个 active writer**。`send_input` 调用**必须串行**。

**原因**：PTY 是字节流，并发 `send_input` 会导致字节交错：
```
send_input("echo A\n")  ──┐
                           ├── PTY 收到 "echo eAcho\nA\n" → 命令错乱
send_input("echo B\n")  ──┘
```

**当前实现**：russh `ChannelWriteHalf` 内部串行化写入，保证单次 `send_input` 的字节完整性，但**不保证多次 `send_input` 的顺序**。

**Agent 责任**：Agent 必须确保对同一 session 的 `send_input` 调用是串行的（等前一次返回再发下一次）。MCP stdio 串行处理天然满足此约束；未来 GUI 并发调用时需注意。

**未来可选**（非 MVP）：SessionManager 内建 write mutex，自动串行化 `send_input`。当前不做，依赖 Agent 串行调用。

## 4. Recovery Semantics

### 4.1 Timeout 恢复（契约 ⑤）

```text
send_input("sleep 300\n")
read_output(wait_for="__NEVER__", timeout_secs=3)
    ↓
timed_out=true, matched=false
    ↓
session_state=ready（仍可操作）
    ↓
send_control(ctrl+c)   # 仍可中断 sleep
    ↓
read_output(wait_for="^C")  # 验证中断成功
```

**关键约束**：
- timeout 只约束本次 `read_output` 调用
- timeout **不发送任何控制信号**到远端
- timeout **不改变** `session_state`
- timeout 后 Agent 可继续 `read_output` / `send_input` / `send_control`

**Agent 最危险的 bug**：timeout 后误以为命令结束 → 重试 → 服务器上出现两个并发任务。契约 ⑤ 防止这类问题：timeout 后 session 仍 Ready，Agent 必须主动 `send_control(ctrl+c)` 或继续等待。

### 4.2 Disconnect 恢复（契约 ⑥ + ADR-0010）

```text
session_state = Lost
    ↓
Agent 决定重连
    ↓
reconnect_session(session_id)
    ↓
session_state = Ready（新 buffer，旧 buffer 不保留）
    ↓
幂等检查：send_input("test -f /tmp/test && echo EXISTS || echo MISSING")
    ↓
根据检查结果决定是否重试原命令
```

## 5. Non-Goals

- **不内建 `execute_and_wait` 工具**：Agent Terminal Protocol 是 Agent 语义层责任，TermBridge 只提供 `send_input` + `read_output(wait_for)` 原语
- **不做命令解析**：不解析命令边界、不识别 `cd`、不追踪 exit code（违反 ADR-0008 边界）
- **不做 exactly-once / at-least-once**：断线后远端执行状态 unknown，Agent 负责幂等检查
- **不做自动重连**：Agent 显式 `reconnect_session`（ADR-0010）
- **不做 ANSI 清洗**：buffer 永远 raw，匹配可选 `ansi_stripped`（未来）
- **不做 write mutex**：当前依赖 Agent 串行调用 `send_input`（MCP stdio 天然满足）
- **不验证 marker 位置**：Completion 正确性是 Agent protocol 责任

## 6. Future Considerations

### 6.1 Execution Correlation ID

未来 Agent 会执行多步操作（如部署 Wazuh：step1 → step2 → step3）。当前日志无法关联「哪条 send_input 属于哪个操作」。

**预留**：MCP metadata 可增加 `operation_id` 字段：
```json
{ "session_id": "xxx", "operation_id": "deploy-wazuh-step3", "data": "..." }
```

Phase 6-C **不实现**，仅在 ADR 留口。未来 Phase 4 timeline / observability 层可利用。

### 6.2 SessionManager Write Mutex

未来 GUI 并发调用场景下，SessionManager 可内建 per-session write mutex，自动串行化 `send_input`。当前 MCP stdio 串行处理天然满足，不实现。

## 7. Test Matrix

### P0：Execution State（Phase 6-C，25/25 PASS）

| # | 场景 | 验证点 | 契约 | 状态 |
|---|---|---|---|---|
| T10 | 命令失败状态 | `false; printf '__TB_DONE__:%s:%s\n' "$reqid" "$?"` → wait_for 命中 + rc=1；`true` → rc=0 | ⑧ | ✅ |
| T11 | marker 提前出现 | `printf 'T%sONE\n' $((11)); sleep 5; printf 'L%sATE\n' $((11))` → wait_for 立即返回 + L11ATE 未出现；6s 后出现。TermBridge 不验证 marker 位置 | ⑧ | ✅ |
| T12 | timeout 后 session 状态 | `sleep 300` + wait_for timeout=3 → timed_out + session_state=ready + Ctrl+C 中断 + exit 130 | ⑤ | ✅ |
| T13 | 连续命令 cursor 隔离 | cmd A (AAA_T13) / cmd B (BBB_T13) + reqid marker → since_cursor 精确切片，output_A 不含 BBB，output_B 不含 AAA | ③④⑧ | ✅ |
| T14 | 并发 waiter（串行限制） | MCP stdio 串行架构，无法真并发；验证连续两个 wait_for 不互相干扰。契约 ④ 并发验证由 Rust 单元测试覆盖（ADR-0003） | ④ | ✅（串行限制已文档化） |
| T15 | disconnect 中途写入 | `touch + nohup pkill & + sleep 30` → sshd kill → session_state=lost → reconnect → 幂等检查 FILE_EXISTS（touch 已执行）→ "请求失败≠命令未执行" | ⑥ | ✅ |

### P0：Attach cursor 边界（Phase 3 daemon 路径，单独一轮）

| # | 场景 | 验证点 | 契约 | 状态 |
|---|---|---|---|---|
| T17 | detach/attach cursor 精确恢复 | cmd A → detach → cmd B（daemon 继续）→ attach(since_cursor) → 恰好一次不重复 | ⑦ | ⬜ |

### P1：PTY resize（需先实现 MCP resize 工具）

| # | 场景 | 验证点 | 契约 | 状态 |
|---|---|---|---|---|
| T16 | PTY resize | `top` + resize(40,120) → top 重绘为新尺寸（需先在 MCP 层暴露 `resize` 工具，当前 `send_control` 仅支持固定 ControlKey 枚举，`TerminalHandle.resize` 未暴露到 MCP） | — | ⬜（阻塞：缺 resize 工具） |

### 测试执行详情

- **测试脚本**：[examples/phase6c_p0_exec_state.ps1](../../examples/phase6c_p0_exec_state.ps1)
- **目标服务器**：192.168.1.171（Debian）
- **结果**：25 assertions / 25 PASS / 0 FAIL
- **覆盖**：T10-T15 共 6 个场景，覆盖命令失败状态、marker 提前出现、timeout 后 session 状态、连续命令 cursor 隔离、并发 waiter（串行限制）、disconnect 中途写入

### 关键测试发现

1. **PTY 回显污染输出**（T11 发现）：
   - 问题：命令文本中的字面量（如 `echo LATE_T11`）会被 PTY 回显到 buffer，`Read-Since` 读到回显中的字面量而非命令实际输出
   - 修复：用算术展开构造所有 marker（`printf 'L%sATE\n' $((11))`），使回显不含最终输出字面量
   - 归属：Agent 语义层责任（ADR-0011 最佳实践 #6 的延伸——不仅 marker，所有需匹配的输出都应避免回显污染）

2. **shell 阻塞导致后续命令排队**（T15 发现）：
   - 问题：`send_input("touch X; sleep 30")` 阻塞 shell，第二个 `send_input("pkill ...")` 排队等 sleep 30 完成才执行
   - 修复：合并为一条命令 `touch X; nohup bash -c 'sleep 2; pkill ...' & sleep 30`，用 nohup 后台启动 pkill
   - 归属：Agent 语义层责任——send_input 是字节流，Agent 需理解 shell 命令排队语义

3. **`since_cursor` 与 `wait_for` 互斥**（ADR 修正）：
   - 问题：ADR-0012 初稿中 `read_output(wait_for=..., since_cursor=cursor_before)` 示例错误——since_cursor 优先级更高，wait_for 被忽略
   - 修复：ADR-0012 §2.4/§2.5 已修正为两步调用（wait_for 不带 since_cursor；命中后单独 since_cursor 读取完整输出）

## Consequences

### 正面

- **9 契约固化**后，上层（Agent / CLI / GUI / Playbook）有稳定的执行语义地基
- **Agent Terminal Protocol**（固定 marker 格式 + request_id + exit_code）让命令完成 / 失败 / 超时有明确表达
- **cursor boundary** 要求防止旧 marker 匹配
- **disconnect unknown 语义**诚实表达限制，避免 Agent 误用 exactly-once
- **input ownership** 约束防止并发写入导致 PTY 字节交错
- **raw buffer 原则**保护终端回放 / ANSI 分析能力

### 代价

- Agent **必须遵循** Agent Terminal Protocol（固定 marker 格式 + cursor boundary + 串行写入）
- **不提供 exactly-once / at-least-once**（Agent 需自己做幂等检查）
- **ANSI `ansi_stripped` 匹配暂不提供**（Agent 自行处理 ANSI）
- **write mutex 暂不内建**（依赖 Agent 串行调用）

## Relationships

- **继承 ADR-0003** buffer 策略（raw bytes + cursor 机制 + 双游标语义）
- **继承 ADR-0008** 边界（不做命令解析，Agent 负责 completion 判断）
- **继承 ADR-0011** send_input 语义（纯字节透传 + 最佳实践）
- **补充 ADR-0010** reconnect（disconnect 后远端执行状态 unknown + 幂等检查建议）

## References

- [ADR-0003: Output Buffer Strategy](0003-output-buffer-strategy.md) — ring buffer + cursor + 双游标语义
- [ADR-0008: Scope Boundary](0008-scope-boundary.md) — TermBridge 定位 = Remote Terminal Runtime
- [ADR-0010: Session Reconnect](0010-session-reconnect.md) — 断线感知 + 手动重连
- [ADR-0011: Input Semantics and Execution Safety](0011-input-semantics-and-execution-safety.md) — send_input 语义 + 字节透传测试
