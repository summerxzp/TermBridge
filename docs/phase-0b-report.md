# Phase 0-B OutputEngine 原型报告

> 日期：2026-08-09 · 状态：✅ 核心完成，18 单测全通过

## 验证目标

在接 MCP/SSH 之前，独立实现并单测 TermBridge 的核心 IP——`OutputEngine`（PLAN §5.2 / §5.3 / §4.6 契约 12 条），用 fake PTY output 把最核心的并发与游标语义问题提前解决。

**不包含**：MCP transport、SSH/SFTP、Session 生命周期、Connection 管理（这些留给 Phase 0-C / Phase 1）。

## 环境

- Windows + Rust 1.96
- 依赖新增：`regex = "1"`、`parking_lot = "0.12"`

## 实现内容

### 1. `OutputRingBuffer` —— 物理环形 + 逻辑单调 written 计数器（契约 2 / 12）

[src/domain/output.rs](file:///e:/Code/TermBridge/src/domain/output.rs) L43–L224

- **物理环形 `buf: Vec<u8>` + `head` 游标**：满则覆盖最旧（环形）
- **逻辑单调 `written: u64`**：永不回绕，作绝对游标；`written` 持续累加，物理回绕只在 `buf[]` 上发生
- **容量**：默认 1MB / min 64KB / max 32MB（`new()` clamp，`new_raw()` 不 clamp 供测试）
- **`Notify`**：锁外唤醒，`write()` 后 `notify_one()`，丢一次无所谓（只是信号）

关键 API：
| 方法 | 语义 |
|------|------|
| `write(&[u8])` | PTY output 入口，推进 written + notify |
| `written() -> u64` | 单调游标快照 |
| `is_truncated(cursor) -> bool` | cursor 是否已被覆盖 |
| `read_since(cursor) -> Vec<u8>` | 纯读，无副作用 |
| `read_since_max(cursor, max) -> ReadSinceResult` | 带字节上限 + has_more + is_truncated |
| `tail(n_lines) -> Vec<u8>` | peek 最后 N 行，不推进任何 cursor |
| `wait_for_notify(dur) -> bool` | 阻塞等新 output |

**单次写入超过容量**：只保留最后 `size` 字节，`head = 0`（避免环形多次回绕）。

### 2. 双游标机制（契约 3 / 4 / 11）

| 游标 | 持有者 | 推进规则 | 模式 |
|------|--------|---------|------|
| `mark_cursor` | OutputEngine 内部（`Mutex<u64>`） | settle 命中推进 / wait_for 命中推进 / wait_for 超时**不**推进 | settle、wait_for |
| `since_cursor` | 调用方自管 | 永不由 engine 推进 | since_cursor（多 consumer 增量读） |
| tail（peek） | 无 cursor | 不推进任何 cursor | tail_lines |

**多 consumer 支持**：`since_cursor` 模式完全不动 `mark_cursor`，多个调用方可各自追踪自己的 cursor 独立增量读。

### 3. `read_output` 三模式调度（契约 3 / 4 / 5）

[src/domain/output.rs](file:///e:/Code/TermBridge/src/domain/output.rs) L362–L406

优先级：`since_cursor` > `tail_lines` > `wait_for` > 默认 `settle`。

```
read_output(params):
  if since_cursor → ReadSinceMax（不推进 mark）
  elif tail_lines → tail（不推进任何 cursor）
  elif wait_for   → read_with_wait_for
  else            → read_with_settle（默认 drain 语义）
```

### 4. `wait_for` 模式（契约 5）

[src/domain/output.rs](file:///e:/Code/TermBridge/src/domain/output.rs) L410–L484

- **① 先扫已有 unread**（`mark → written`），命中立即返回
- **② 未命中**：`select!` 等 deadline 或 `Notify`
- **③ 新数据到达**：全量重扫（`mark → written`，不推进 last_mark，避免漏跨 chunk 匹配）
- **命中**：推进 `mark_cursor` 到 written，返回匹配行 ± context_lines
- **超时**：**不**推进 `mark_cursor`（留作后续 read_output 未读数据），返回已扫 unread 供观察
- **正则编译失败**：回退纯文本 `contains`（`find_subslice`）

### 5. `settle` 模式（契约 3，默认 drain 语义）

[src/domain/output.rs](file:///e:/Code/TermBridge/src/domain/output.rs) L488–L549

- **50ms 轮询**（`SETTLE_POLL_MS`）
- **300ms 稳定阈值**（`SETTLE_THRESHOLD_MS`）：输出连续 300ms 不变 → settled
- **空输出永不 settled**：避免命令还没产出就提前返回
- **prompt 检测**：尾部 `$ ` / `# ` / `> ` 立即返回（不等 300ms）
- **超时**：推进 mark 到当前 written，返回累积 output

### 6. 辅助函数

- `match_pattern`：正则优先（UTF-8 数据），失败回退 `find_subslice`
- `extract_context`：匹配行 ± context_lines 行
- `has_prompt_suffix`：简单 prompt 后缀检测
- `find_subslice`：字节子序列查找

## 行为契约覆盖（§4.6 共 12 条）

| # | 契约 | 本模块相关 | 覆盖测试 |
|---|------|-----------|---------|
| 1 | Session 是唯一 terminal state owner | Session 层（Phase 1） | — |
| 2 | PTY output 永久进入 bounded buffer，满则丢最旧 | ✅ | `test_buffer_overflow_drops_oldest` |
| 3 | read_output 默认 drain（settle），推进 mark | ✅ | `test_settle_advances_mark_cursor` |
| 4 | tail_lines 不推进任何 cursor | ✅ | `test_tail_lines_does_not_advance_cursor` |
| 5 | wait_for 先扫已有 + 等未来；命中推进，超时不推进；正则失败回退 contains | ✅ | `test_wait_for_matches_existing` / `test_wait_for_waits_for_future_output` / `test_wait_for_timeout_does_not_advance` / `test_wait_for_regex_fallback_to_contains` |
| 6 | timeout 只约束本次调用 | ✅ | `test_timeout_does_not_close_session` |
| 7 | send_input 不等待 | Session 层 | — |
| 8 | Ctrl+C 是 control | Session 层 | — |
| 9 | Session close 才结束 shell | Session 层 | — |
| 10 | Connection disconnect 不销毁 Session | Session 层 | — |
| 11 | since_cursor 不推进 mark，支持多 consumer | ✅ | `test_since_cursor_multi_consumer` / `test_since_cursor_truncation` |
| 12 | RingBuffer 物理环形 + 逻辑单调 written | ✅ | `test_ring_buffer_wrap_around` / `test_written_monotonic` |

**settle 细节测试**：`test_settle_empty_never_settled`（空输出永 unsettled）、`test_settle_prompt_detection`（prompt 立即返回）。

**RingBuffer 基础测试**：`test_tail_basic` / `test_tail_zero` / `test_empty_buffer` / `test_write_empty_noop`。

**Phase 0-B 范围内的 7/12 条契约全部由单测验证。** 其余 5 条（1/7/8/9/10）是 Session/Connection 层语义，留给 Phase 1。

## 测试结果

```
running 18 tests
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.32s
```

| 测试 | 验证点 |
|------|--------|
| `test_buffer_overflow_drops_oldest` | 契约 2：满则丢最旧，written 单调 |
| `test_written_monotonic` | 契约 12：written 永不回绕 |
| `test_settle_advances_mark_cursor` | 契约 3：settle 推进 mark |
| `test_tail_lines_does_not_advance_cursor` | 契约 4：tail 不推进 cursor |
| `test_wait_for_matches_existing` | 契约 5：先扫已有命中 |
| `test_wait_for_waits_for_future_output` | 契约 5：等未来 output 命中 |
| `test_wait_for_timeout_does_not_advance` | 契约 5：超时不推进 mark，数据留作未读 |
| `test_wait_for_regex_fallback_to_contains` | 契约 5：非法正则回退 contains |
| `test_timeout_does_not_close_session` | 契约 6：timeout 后 engine 仍可用 |
| `test_since_cursor_multi_consumer` | 契约 11：多 consumer 增量读，mark 不动 |
| `test_since_cursor_truncation` | 契约 11：截断报告 is_truncated |
| `test_ring_buffer_wrap_around` | 契约 12：物理环形回绕 |
| `test_tail_basic` / `test_tail_zero` | tail 行切分 |
| `test_empty_buffer` / `test_write_empty_noop` | 边界 |
| `test_settle_empty_never_settled` | 空输出永 unsettled |
| `test_settle_prompt_detection` | prompt 后缀立即返回 |

`cargo clippy --lib --tests` 干净（仅 cargo config 无关 warning）。

## 关键决策与发现

1. **`new_raw` 不能用 `#[cfg(test)]`**：`new()` 内部调 `new_raw()`，若 `new_raw` 仅 test build 存在，非 test 编译会断。改为 `pub(crate)` 常驻，测试与生产共用。
2. **wait_for 超时必须不推进 mark**（契约 5）：初版误把超时分支写成 `*mark = written()`，会把未匹配数据标为已消费，后续 read_output 漏读。修正为超时返回已扫 unread 供观察，但 mark 保持原位，数据留作未读。
3. **wait_for 未命中不推进 last_mark**：若每次扫完推进 last_mark，下次只扫增量，会漏掉跨 chunk 的 pattern 匹配。改为 last_mark 始终保持初始 mark，每次全量重扫（`mark → written`）。数据量通常不大，可接受。
4. **settle 用 `tokio::select!` + `sleep(poll)` 轮询**：不依赖 Notify，因为 settle 关心的是"输出停止变化"，不是"有新数据"。空输出直接 `continue`，绝不 settled。
5. **`OutputRingBuffer` 用 `parking_lot::Mutex`**：临界区极短（仅 buf 操作），Notify 在锁外。`Arc<Mutex<Inner>> + Arc<Notify>` 让 buffer 可 clone（PTY read task 持有一份写，engine 持有一份读）。
6. **SessionState 状态机不在 Phase 0-B 实现**：PLAN 列了 `SessionState 状态机迁移`，但 StateMachine 依赖 Session/Connection 抽象，属 Phase 1 MVP 范畴。Phase 0-B 聚焦 OutputEngine（PLAN 风险表明确"Output 语义实现复杂 → Phase 0-B 独立原型"），把并发与游标问题先解决。

## 已知限制（Phase 0-C / Phase 1 补）

- **日志 tee 未实现**：PLAN §5.2 提到 PTY output 同时 tee 到滚动日志文件作兜底。Phase 0-B 仅 RingBuffer，日志 tee 留给 Phase 1（需要 Session 持有日志 handle）。
- **`mark_to_latest` 未被任何测试覆盖**：API 已实现，但语义验证需 Session 层场景（detach 后 reattach 跳过历史）。
- **prompt 检测过于简单**：仅 `$ ` / `# ` / `> ` 后缀。Phase 1 需支持自定义 PS1 / zsh / fish prompt。
- **未接真实 PTY**：所有测试用 fake `buffer.write()`。真实 PTY read task 的集成验证在 Phase 0-C vertical slice。
- **无并发压测**：18 个单测验证语义正确性，但未压测多 reader 高频 write 场景。Phase 1 补集成测试。

## 文件清单

| 文件 | 行数 | 内容 |
|------|------|------|
| [src/domain/output.rs](file:///e:/Code/TermBridge/src/domain/output.rs) | 897 | OutputRingBuffer + OutputEngine + 三模式 read_output + 18 单测 |
| [src/domain/mod.rs](file:///e:/Code/TermBridge/src/domain/mod.rs) | 4 | `pub mod output` |
| [src/lib.rs](file:///e:/Code/TermBridge/src/lib.rs) | 4 | `pub mod domain` |
| [Cargo.toml](file:///e:/Code/TermBridge/Cargo.toml) | — | 新增 `regex`、`parking_lot` |

## 结论

**Phase 0-B 核心完成，可进入 Phase 0-C。**

OutputEngine 的核心并发与游标语义已由 18 个单测验证，覆盖 §4.6 契约 12 条中 Phase 0-B 范围内的全部 7 条（2/3/4/5/6/11/12）。最关键的 `wait_for` 超时不推进 mark、双游标独立、settle 空输出永不 settled 三条语义均锁定。

**Phase 0-C 目标**（PLAN §9）：
1. `rmcp` → `SessionManager` 接通
2. vertical slice：`open_session(host)` → `send_input("ls\n")` → `read_output` → `send_control(Ctrl+C)` → `close_session`
3. 输出 ADR-0001（构建策略 + 核心 crate）、ADR-0002（stdio only）、ADR-0006（ssh -G）

Phase 0-C 将把 OutputEngine 接入真实 russh PTY + rmcp transport，在 Windows 上连一台 Linux 跑通 6 个工具，验证端到端行为符合 §4.6 契约。
