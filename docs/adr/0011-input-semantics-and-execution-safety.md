# ADR-0011：send_input 语义与执行安全测试矩阵（Phase 6-B）

- **Status**: Accepted（决策部分）/ WIP（测试矩阵持续补充）
- **Date**: 2026-08-10
- **Phase**: 6-B
- **Supersedes**: —

## Context

Phase 6-A 完成了 9 场景 31 断言的「字节透传压力测试」（[examples/phase6_escape_stress.ps1](../../examples/phase6_escape_stress.ps1)），验证了 `send_input` 从 MCP JSON → SSH channel → PTY 全链路无篡改。

但「字节透传」≠「执行语义安全」。TermBridge 作为 AI Agent 操作真实终端的 Runtime，最大风险不是 SSH 断线，而是：

> **Agent 以为执行了命令 A，服务器实际执行了命令 B；或命令边界不符合 Agent 预期。**

这类风险属于「执行语义」层面，无法靠字节透传测试覆盖，必须建立独立的测试矩阵与明确的设计决策。

### 本 ADR 要解决的问题

1. **明确 `send_input` 的语义边界**：是否自动追加 `\n`？是否处理 `\r`？谁负责命令边界？
2. **建立执行安全测试矩阵**：从「字节可靠性」扩展到「命令边界、控制字符、交互式程序、大输出、危险操作」
3. **分层风险归属**：哪些是 TermBridge core 责任，哪些是 Agent 语义责任，哪些是文档指导责任

## Decision

### 1. `send_input` 语义：纯字节透传，零修改

**决策**：`send_input` 对传入的 `data` 字段做**纯字节透传**，不做任何修改。

- **不自动追加 `\n`**：Agent 必须显式在 `data` 末尾加 `\n` 表示 Enter
- **不剥离任何字节**：包括 `\r`、`\t`、ANSI 转义序列、NULL 字节
- **不做命令边界解析**：TermBridge 不知道「一条命令」从哪开始到哪结束
- **不做引号/转义处理**：shell 语义由远端 shell 解释，TermBridge 不参与

**当前实现已经符合此决策**（无需修改）：

```rust
// src/domain/session.rs:248
pub async fn send_input(&self, data: &[u8]) -> Result<(), TermError> {
    if !self.state().is_usable() {
        return Err(TermError::SessionClosed(self.id.clone()));
    }
    self.touch_last_activity();
    let cursor_before = self.output.buffer().written();
    self.timeline.record_command(data, cursor_before);
    self.handle.write(data).await  // ← 纯透传
}
```

**理由**：TermBridge 定位 = Remote Terminal Runtime（ADR-0008），不是命令解析器、不是 playbook 引擎、不是 config validator。命令语义、引号、展开、边界识别都是 Agent 的责任。TermBridge 在 core 层做任何「智能处理」都会：
- 破坏可预测性（Agent 无法推断实际发送了什么）
- 引入隐式行为（不同场景需要不同处理，core 层无法穷举）
- 违反「给 Agent 一个真正 Terminal」的核心理念

### 2. `\r`（回车）处理策略：不处理

**决策**：`send_input` **不剥离 `\r`**，保持纯字节透传。

**为什么不剥离**：
- `\r` 是合法的 PTY 控制字符（如 `printf 'hello\rworld'` 中 `\r` 让光标回到行首，是有意为之的终端控制）
- 盲目 `input.replace("\r", "")` 会破坏这类合法用法，违反透传原则
- Windows CRLF 问题（`\r\n` → PTY 回显 `^M`）是调用方问题，不应在 core 层掩盖

**Windows 换行问题的正确解法**：
- **Agent prompt / Skill 层指导**：要求 Agent 发送多行命令时使用 LF（`\n`），不使用 CRLF
- **未来可选**（非 MVP）：`send_input` 增加 `normalize_newline: bool` 参数，默认 `false`，由 Agent 显式开启。避免在 core 层做隐式默认处理

**当前状态**：不实施任何 `\r` 处理，在文档中明确指导 Agent 使用 LF。

### 3. `read_output` 语义：不关联命令边界

**决策**：`read_output` 只返回 buffer 中的字节流，**不区分哪条命令产生了哪段输出**。

- Agent 不能假设「`send_input` 后立即 `read_output` 拿到的就是这条命令的输出」
- 因为 PTY 是流式的，前一条命令可能还在输出，read_output 会拿到混合内容
- 时序问题：`send_input("cmd A")` → `send_input("cmd B")` → `read_output()` 可能拿到 A 回显 + A 输出 + B 回显 + B 输出

**Agent 必须使用 marker 模式建立命令-输出关联**：

```bash
# Agent 发送命令时追加唯一 marker
send_input("apt update && echo '__TERM_DONE__'\n")

# read_output 用 wait_for 等待 marker
read_output(wait_for="__TERM_DONE__", timeout_secs=60)
```

**TermBridge 的责任**：提供 `wait_for` / `since_cursor` / `tail_lines` 三种读模式，让 Agent 能可靠地建立命令边界。**不内建 marker 机制**（属于 Agent 语义层）。

### 4. `send_control` 语义：控制字符透传

**决策**：`send_control` 发送标准终端控制字符（Ctrl+C = 0x03, Ctrl+D = 0x04, Ctrl+Z = 0x1A 等），由远端 PTY/shell 解释。

- TermBridge 不解释控制字符的语义（如「Ctrl+C = 中断当前命令」是 shell 行为）
- Agent 必须知道：Ctrl+C 发送 0x03，PTY 收到后产生 SIGINT；Ctrl+D 在空行发送 EOF

## Test Matrix

### 已覆盖（Phase 6-B 字节透传，31/31 PASS）

| # | 场景 | 验证点 | 结果 |
|---|---|---|---|
| S1 | 单引号特殊字符 | `$` `` ` `` `\` `!` `\|` 全字面量 | ✅ |
| S2 | 双引号 shell 展开 | `$HOME` 正确展开为真实路径 | ✅ |
| S3 | awk 深层嵌套引号 + `\x22` | 产出正确 JSON | ✅ |
| S4 | 3600 字符长内容 | wc 精确匹配 + head/tail 验证 | ✅ |
| S5 | heredoc + 全特殊字符 JSON | 空格/引号/美元/反引号/感叹号/中文 | ✅ |
| S6 | `$()` 命令替换 + 管道链 | `wc -l` / `tr` / `date` 正确执行 | ✅ |
| S7 | `printf` 真实控制字符 | Tab 分隔 + ANSI 色码按字节保留 | ✅ |
| S8 | 8192 字节巨量数据 | wc=8192 + head/tail 精确验证 | ✅ |
| S9 | 感叹号 history token | `!!` `!$` `!*` 单引号下不展开 | ✅ |

### P0 已覆盖（Phase 6-C 执行语义安全，20/20 PASS）

| # | 场景 | 验证点 | 结果 |
|---|---|---|---|
| T1 | Ctrl+C 中断 | `sleep 300` + 0x03 → `^C` + exit 130 | ✅ |
| T2 | Ctrl+D EOF | `cat` + 0x04 → EOF 退出 | ✅ |
| T3 | Ctrl+Z 暂停/恢复 | `sleep 300` + 0x1A → Stopped + `fg` 恢复 | ✅ |
| T4 | PTY tty 确认 | `tty` → `/dev/pts/0`（非 "not a tty"） | ✅ |
| T5 | 交互式 `read` | `read -p "input:" x` + send_input → 正确读取 | ✅ |
| T6 | sudo Policy 拦截 | `sudo -n true` → `POLICY_NEEDS_CONFIRM`，password 不进 LLM context | ✅ |
| T7 | 1.5MB 大输出 ring buffer 截断 | 150×10000 字节多批次输出 → `is_truncated=true` + `written>1MB` + buffer 内容 cap 1MB | ✅ |
| T8 | command marker + wait_for | `cmd && echo __DONE__` + `wait_for=__DONE__` → 可靠关联 | ✅ |
| T9 | 多命令时序边界 | 连续 `send_input` 无 `\n` → 命令拼接；有 `\n` → 分开执行 | ✅ |

### 待补 P1（健壮性增强）

| # | 场景 | 验证点 | 状态 |
|---|---|---|---|
| T10 | UTF-8 多字节拆包 | emoji（4字节）大量输出，buffer 不产生乱码 | ⬜ |
| T11 | shell 兼容（sh/dash） | `/bin/sh` 下 quote/expand 行为差异 | ⬜ |
| T12 | 断线中执行语义 | persistent session 运行中杀 termbridge → reattach → 输出连续无重复 | ⬜（Phase 3 persistent 路径） |
| T13 | 特殊控制字符全集 | Ctrl+A/E/K/L/W/U 等 readline 编辑键 | ⬜ |

## Risk Layering（风险归属分层）

明确「谁负责什么」，避免在错误的层做处理。

| 层级 | 职责 | 不做的事 |
|---|---|---|
| **TermBridge core** | 字节透传、buffer 管理、session 生命周期、断线感知/重连 | 不解析命令、不处理引号、不剥离 `\r`、不自动追加 `\n` |
| **Agent 语义层** | 命令构造、引号使用、命令边界（`\n`）、marker 模式、shell 知识 | 不假设 TermBridge 会「智能修正」输入 |
| **文档指导层** | 最佳实践、marker 模式示例、LF 换行指导、危险命令清单 | 不替代 core 或 Agent，仅提供指引 |

### Agent 最佳实践（文档指导层）

1. **换行用 LF**：多行命令用 `\n`，不用 `\r\n`（避免 `^M` 回显）
2. **命令边界用 marker**：`cmd && echo '__DONE__'` + `wait_for`
3. **增量读用 `since_cursor`**：避免历史污染，不漏数据
4. **引号意识**：单引号字面量、双引号展开 `$`，heredoc 带引号不展开
5. **控制字符用 `send_control`**：不发 `0x03` 字面量到 `send_input`（虽然透传也行，但语义更清晰用 `send_control`）
6. **marker 防回显匹配**（Phase 6-C 发现）：`wait_for` 会扫描 PTY 回显的命令文本。如果 marker 字面出现在命令中（如 `echo GEN_DONE_T7`），`wait_for` 会匹配回显而非实际输出，导致提前返回。**解法**：用 shell 算术展开构造 marker，使回显不含 marker 字面量：
   - `echo GEN_DONE_T$((7))` → 输出 `GEN_DONE_T7`，回显含 `GEN_DONE_T$((7))`（不含 `GEN_DONE_T7`）
   - `printf 'T9A_DON%s\n' E` → 输出 `T9A_DONE`，回显含 `T9A_DON%s\n' E`（不含 `T9A_DONE`）
7. **`wait_for` 只返回匹配上下文**（Phase 6-C 发现）：`wait_for` 命中后返回 `extract_context` 结果（匹配行 ± `context_lines`），**不返回全部输出**。如需完整命令输出，应在 `wait_for` 匹配后用 `since_cursor` 从命令前位置读取：
   ```
   cursor_before = read_output(since_cursor=cursor)  # 记录位置
   send_input("cmd && echo MARKER\n")
   read_output(wait_for="MARKER")                     # 等待完成
   full_output = read_output(since_cursor=cursor_before)  # 读取完整输出
   ```
8. **PTY bracketed paste mode**（Phase 6-C 发现）：交互式 shell 会在命令输出前后插入 ANSI 转义序列 `\x1b[?2004l`（禁用）和 `\x1b[?2004h`（启用）。解析输出做精确匹配时需剥离这些序列（正则 `\x1b\[[0-9;?]*[a-zA-Z]`）。

## Open Questions

1. **`send_input` 是否需要 `normalize_newline` 可选参数？**
   - 当前决策：不需要（MVP 纯透传）
   - 未来若 Agent 反馈 CRLF 是高频痛点，可考虑增加（默认 `false`）

2. **command marker 是否需要 TermBridge 内建支持？**
   - 当前决策：不需要（Agent 语义层责任）
   - 未来可在 Skill / prompt 层封装 marker 模式最佳实践

3. **`\r` 是否需要在 Policy 层告警？**
   - 当前决策：不需要（Policy 层只拦危险命令，不做字节清洗）
   - 可选：tracing 层在输入含 `\r` 时 debug 日志提示（不改字节）

## Implementation Status

- **决策部分**：已落地（当前代码即符合决策，无需修改）
- **P0 测试矩阵**：已完成（Phase 6-C，20/20 PASS，2026-08-10）
- **P1 测试矩阵**：待实施（Phase 6-C 或后续）
- **Agent 最佳实践文档**：已写入本 ADR（含 Phase 6-C 三项关键发现）

### Phase 6-C 验证详情

- **测试脚本**：[examples/phase6c_p0_exec_safety.ps1](../../examples/phase6c_p0_exec_safety.ps1)
- **目标服务器**：192.168.1.171（Debian）
- **结果**：20 assertions / 20 PASS / 0 FAIL
- **覆盖**：T1-T9 共 9 个场景，覆盖控制字符、PTY 交互、Policy 拦截、ring buffer 截断、命令边界

### Phase 6-C 发现的关键问题与修复

1. **`wait_for` 回显匹配**（测试设计问题，非 core bug）：
   - 问题：`wait_for("MARKER")` 会匹配 PTY 回显的命令文本中的 `MARKER` 子串，导致提前返回
   - 修复：用 `$((expr))` 算术展开或 `printf` 格式化构造 marker，使回显不含 marker 字面量
   - 归属：Agent 语义层责任（文档指导层补充最佳实践 #6）

2. **`wait_for` 返回值仅为匹配上下文**：
   - 问题：`wait_for` 命中后返回 `extract_context` 结果，不含完整输出
   - 修复：`wait_for` 匹配后用 `since_cursor` 从命令前位置读取完整输出
   - 归属：Agent 语义层责任（文档指导层补充最佳实践 #7）

3. **PTY bracketed paste mode ANSI 序列**：
   - 问题：交互式 shell 在输出前后插入 `\x1b[?2004l` / `\x1b[?2004h`，干扰精确匹配
   - 修复：解析输出时用正则 `\x1b\[[0-9;?]*[a-zA-Z]` 剥离 ANSI 序列
   - 归属：Agent 语义层责任（文档指导层补充最佳实践 #8）

## References

- [ADR-0008: Scope Boundary](0008-scope-boundary.md) — TermBridge 定位 = Remote Terminal Runtime
- [ADR-0003: Output Buffer Strategy](0003-output-buffer-strategy.md) — ring buffer + cursor 机制
- [ADR-0009: Bootstrap Host and Credential Provider](0009-bootstrap-host-and-credential-provider.md) — password 不进 LLM context
- [ADR-0010: Session Reconnect](0010-session-reconnect.md) — 断线感知 + 手动重连
- [examples/phase6_escape_stress.ps1](../../examples/phase6_escape_stress.ps1) — 字节透传压力测试脚本（31/31 PASS）
- [examples/phase6c_p0_exec_safety.ps1](../../examples/phase6c_p0_exec_safety.ps1) — 执行语义安全 P0 测试脚本（20/20 PASS）
