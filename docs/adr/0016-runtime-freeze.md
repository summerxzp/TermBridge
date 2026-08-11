# ADR-0016：Runtime Freeze

- **Status**: Accepted
- **Date**: 2026-08-11
- **Phase**: 8（冻结点）
- **Supersedes**: —
- **Depends on**: ADR-0012（Runtime Contract）、ADR-0013（Agent Terminal Protocol）、ADR-0015（Provider API Freeze）

## 1. Motivation

TermBridge 从 Phase 0 走到 Phase 8，三大契约已先后冻结：

| 契约 | ADR | 冻结状态 |
|------|-----|---------|
| Runtime Contract（9 大契约） | ADR-0012 | 33/33 P0 + T17 8/8 + cross-restart 5/5 |
| Agent Terminal Protocol（7 条规则） | ADR-0013 | 由 ADR-0012 P0 矩阵覆盖 |
| Provider API（2+6 trait 方法） | ADR-0015 | 两个 Provider 验证完备性 |

Phase 8 首轮 Dogfooding（真实运维任务：排查→修复→验证）暴露的痛点已分析完成：

- 核心能力稳定：session 持续 1h+、10+ 命令、sftp 操作零断连
- marker/cursor/sftp 原子写/pty 稳定性均可靠
- 痛点集中在**最后一公里 ergonomics**（SFTP 路径、ANSI 噪音），不是 Runtime 结构性问题

实际修复（P1 SFTP local path + P3 strip_ansi）是**纯增量参数**，不修改任何已有 API 签名：
- `PathPolicy::default_from_cwd()` 签名不变，只扩展内部默认白名单
- `ReadOutputParams` 加 `strip_ansi: bool` 字段，`Default` 实现保持 `false`，向后兼容

**Runtime Core 已达可冻结状态。**

## 2. 决策

### 2.1 冻结范围

以下 API 签名**不可变**（只接受 bug fix，不接受功能性修改）：

**Domain trait**（ADR-0015）：
- `TerminalProvider`（2 方法：`open` / `as_any`）
- `TerminalHandle`（6 方法：`read` / `write` / `send_control` / `resize` / `close` / `as_any`）

**Runtime Contract**（ADR-0012 九大契约）：
- Input / Output / Cursor / Waiter / Timeout / Disconnect / Attach / Completion / Ownership 语义不变

**MCP 工具 schema**（20 工具）：
- 现有工具的参数名、类型、返回结构不可变
- 新增可选参数允许（向后兼容，`#[serde(default)]`）
- 新增工具允许（不破坏现有调用方）

**Agent Terminal Protocol**（ADR-0013 七条规则）：
- 7 条规则语义不变
- Skill 作为 operational version 可迭代，但不得违反 Protocol

### 2.2 修改规则

| 修改类型 | 是否允许 | 流程 |
|---------|---------|------|
| Bug fix（不改签名） | ✅ 允许 | 直接修复 + 测试 |
| 新增可选参数 | ✅ 允许 | 向后兼容，`#[serde(default)]` |
| 新增工具 | ✅ 允许 | 不破坏现有工具 |
| 新增 Provider | ✅ 允许 | 实现 ADR-0015 trait |
| 修改已有签名 | ❌ 拒绝 | 需新 ADR 推翻本冻结 |
| 修改契约语义 | ❌ 拒绝 | 需新 ADR 推翻本冻结 |

### 2.3 反模式：因 Agent 使用习惯增加 Core API

**禁止**：因 Agent 使用某功能"不方便"就在 Core 加高阶 API。

典型反例（已拒绝）：
- `run_command` 高阶工具（封装 cursor→send→wait→read 四步）—— 违反 ADR-0008 scope boundary，应由 Skill/Agent 封装
- 自动 ANSI 剥离（改 RingBuffer）—— 违反 ADR-0012 契约 ③ Cursor raw bytes 语义

**正确路径**：
1. Skill 不够好 → 改 Skill
2. Skill 无法表达真实需求 → 才回头评估是否修改 Protocol
3. 修改 Protocol → 需新 ADR 推翻本冻结，论证必要性

## 3. Consequences

### 3.1 对开发者

- 修改 Core 前先检查本 ADR 冻结范围
- 新增功能优先考虑 Skill / Agent 端封装，而非 Core API
- Bug fix 不受影响

### 3.2 对 AI Agent

- 现有 MCP 工具调用方式永远有效
- 可依赖 ADR-0013 七条规则做决策
- Skill 可能迭代，但 Protocol 不变

### 3.3 对项目演进

剩余 Phase 8 工作不涉及 Core：
- 8-B：doctor 命令 + 预构建 release artifacts
- 8-C：持续 Dogfooding
- 8-D：正式 release

Phase 9+（Playbook / 高级 GUI / 更多 Provider / 自动化）在 Core 冻结基础上扩展，不修改 Core。

## 4. 验证矩阵

冻结时的验证状态：

| 测试矩阵 | 结果 |
|---------|------|
| ADR-0012 P0 | 33/33 PASS |
| T17 attach/cursor | 8/8 PASS |
| Cross-restart E2E | 5/5 PASS |
| T16 resize | 6/6 PASS |
| 单元测试 | 256/256 PASS |

## 5. References

- [ADR-0012](0012-execution-state-and-completion-protocol.md)：Runtime Contract（9 大契约）
- [ADR-0013](0013-agent-terminal-protocol.md)：Agent Terminal Protocol（7 条规则）
- [ADR-0015](0015-provider-api-freeze.md)：Provider API Freeze
- [ADR-0008](0008-scope-boundary.md)：Scope Boundary
