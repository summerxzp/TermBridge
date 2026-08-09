# ADR-0003：Output 缓冲策略 —— RingBuffer + 双游标 + settle

- **Status**: Accepted
- **Date**: 2026-08-09
- **Phase**: 1
- **Supersedes**: —

## Context

PTY output 是**流式字节流**而非一次性结果，TermBridge 需同时满足三类消费者：

1. **Agent 增量读**——按需拉取自上次以来的新输出，支持多 consumer 各自追踪进度。
2. **Agent peek**——快速看一眼"最近发生了什么"（tail），不破坏消费位。
3. **Agent 阻塞匹配**——`wait_for` 等待某正则出现，跨多个 PTY write chunk。

Phase 0-B 已用 fake PTY output 验证 `src/domain/output.rs` 的 RingBuffer + 双游标 + Waiter + settle 实现，行为符合 PLAN §4.6 契约 12 条。本 ADR 锁定参数与语义，进入 Phase 1 后不再变动核心结构。

参考：pty-mcp `internal/buffer/ring.go` 的物理环形 + 单调计数器设计已验证可行，TermBridge 用 Rust 重写。

## Decision

### 1. 物理环形 buffer + 逻辑单调 `written` 计数器

| 项 | 取值 | 说明 |
|---|---|---|
| `DEFAULT_BUFFER_SIZE` | 1 MB | 覆盖典型长命令输出（kernel build log、`apt install`、`tail -f` 数秒累积） |
| `MIN_BUFFER_SIZE` | 64 KB | 防误配过小 |
| `MAX_BUFFER_SIZE` | 32 MB | 上限，防误配耗尽内存 |
| `buf: Vec<u8>` | 物理环形 | `head` 在 `[0, size)` 内回绕，满则覆盖最旧 |
| `written: u64` | 单调计数器 | 永不回绕，作绝对游标，避免游标比较的二义性 |

`size.clamp(MIN, MAX)` 在 `OutputRingBuffer::new` 内强制。1 MB 的依据：典型 `make -j$(nproc)` 单次刷屏 200–800 KB，1 MB 足以容纳完整一轮输出供 Agent 回看；超长流式（`tail -f`）依赖 settle / since_cursor 增量读，buffer 只需滚动窗口。

### 2. 双游标语义

| 游标 | 类型 | 推进方 | 用途 |
|---|---|---|---|
| `mark_cursor` | `Mutex<u64>`（OutputEngine 内） | settle 命中 / wait_for 命中 / `mark_to_latest()` | 内部消费位，假定单 consumer（MCP 串行调用） |
| `since_cursor` | 调用方自管 | **不**由 TermBridge 推进 | 多 consumer 增量读，`ReadSinceMax(cursor, max_bytes)` 返回 `{output, new_cursor, has_more, is_truncated}` |

- `tail_lines` 模式不推进任何 cursor（纯 peek）。
- `mark_cursor` 与 `since_cursor` 相互独立——`since_cursor` 路径不触碰 `mark_cursor`，反之亦然。

### 3. `read_output` 三模式调度（互斥优先级）

```
since_cursor > tail_lines > wait_for > 默认 settle
```

| 模式 | 行为 | 推进 mark |
|---|---|---|
| `since_cursor` | `ReadSinceMax(cursor, max_bytes)`，返回增量 + `has_more`/`is_truncated` | 否 |
| `tail_lines` | `buffer.tail(n)`，上限 `MAX_TAIL_LINES=100` | 否 |
| `wait_for` | 先扫 unread（mark→written），命中即推进；未命中 `Notify` 阻塞等新 output，全量重扫；超时不推进，返回当前 unread | 命中推进，超时不推进 |
| 默认 settle | 50ms 轮询 `Since(mark)`，输出稳定 ≥300ms 或检测到 prompt（`$ `/`# `/`> ` 结尾）即返回；空输出永不 settled；timeout 推进 mark | 是 |

正则编译失败回退纯文本 `contains`（契约 5）。`wait_for` 跨 chunk 全量重扫（mark→written），保证跨 write 的匹配不漏。

### 4. settle 阈值

| 常量 | 值 | 依据 |
|---|---|---|
| `SETTLE_POLL_MS` | 50 | 平衡 CPU 与响应延迟 |
| `SETTLE_THRESHOLD_MS` | 300 | 经验值：交互式命令输出完毕到下一字符的典型间隔 > 300ms |
| `DEFAULT_TIMEOUT_SECS` | 5 | Agent 友好的默认等待 |
| `MAX_TIMEOUT_SECS` | 60 | 防长阻塞 |
| `MAX_CONTEXT_LINES` | 50 | `wait_for` 命中行前后上下文上限 |

### 5. 截断检测

`is_truncated(cursor)`：

- `written <= size`（未溢出）：`cursor > written` 即无效。
- `written > size`（已溢出，最旧被覆盖）：有效区间 `[written - size, written]`，`cursor < written - size` 即无效。

`since_cursor` 模式返回 `is_truncated=true` 时，调用方应重置 cursor 到 `new_cursor`（即当前 buffer 全部）重新消费。

### 6. 日志 tee（Phase 2+ 预留）

PLAN §5.2 设计了 `MultiWriter ──┬──→ RingBuffer └──→ 滚动日志文件`，buffer 溢出数据仍有日志兜底。**Phase 1 仅实现 RingBuffer**，MultiWriter / 滚动日志 / 完整 PTY 录制推迟到 Phase 2+。当前 `OutputRingBuffer::write` 只写 `buf[]`，结构上已为后续接入 `MultiWriter` 留好单一入口。

## Consequences

- ✅ **固定内存**：单 Session buffer 上限 32 MB，`tail -f` 不会撑爆内存。
- ✅ **增量读**：`since_cursor` 支持多 consumer 各自追踪，无相互干扰。
- ✅ **绝对游标简单**：`written` 单调，`is_truncated` 仅一次比较。
- ✅ **Prompt 即时返回**：settle 模式检测到 `$ `/`# `/`> ` 立即返回，不等 300ms。
- ⚠️ **截断丢旧数据**：buffer 满后最旧数据被覆盖，Agent 若不及时消费会丢失。Phase 2+ 日志 tee 兜底。
- ⚠️ **`wait_for` CPU 开销**：每次 Notify 唤醒全量重扫 unread（mark→written），高频 `tail -f` + 长 pattern 会有可观的 regex 扫描成本。MVP 可接受（单 Session 串行），Phase 4 若成瓶颈可改为增量匹配窗口。
- ⚠️ **`mark_cursor` 假定单 consumer**：MCP stdio 串行调用成立；若未来 GUI 并发读同 Session，需用 `since_cursor` 路径。
