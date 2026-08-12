# ADR-0010：Session 断线感知 + 手动重连（Phase 6-A）

- **Status**: Accepted
- **Date**: 2026-08-10
- **Phase**: 6-A
- **Supersedes**: —

## Context

Phase 1-5 + ADR-0009 完成了 SSH 链路全功能，但可靠性存在短板：**SSH 连接断开后 session 直接进入 Lost，无法恢复**。Agent 必须手动 `close_session` + `open_session` 重新开始，丢失所有上下文（cwd / 环境变量 / 后台进程 / buffer 历史）。

### 当前断线处理流程

```text
SSH 断开（网络抖动 / 服务器重启 / keepalive 超时）
    │
    ▼
PTY read task: handle.read() → Ok(None) / Err
    │
    ▼
read task 退出，置 SessionState::Lost
    │
    ▼
Session 留在 DashMap，buffer 仍可读（§4.6 契约 10）
    │
    ▼
send_input / send_control 拒绝（is_usable=false）
    │
    ▼
Agent 必须手动 close_session + open_session
```

### 问题分解

**1. Agent 不感知断线**

`read_output` 在 Lost 状态下仍返回 buffer 数据（正常行为），但返回结构 `ReadOutputResult` 中无 `state` 字段。Agent 无法区分"session 正常但暂无输出"和"session 已断开"。

**2. 无法重连**

`SshTerminalHandle` 的 reader/writer 来自一个 SSH channel，channel 死了就死了，无法重建。重连 = 新建整个 SSH 连接 + channel + PTY = 新建整个 `SshTerminalHandle`。

`Session.handle` 字段类型是 `Arc<dyn TerminalHandle>`（不可变），替换 handle 需要改可变性。

**3. 自动重连 vs 手动重连**

自动重连涉及：退避策略、重试次数限制、并发竞态（重连中 Agent 调 send_input）、shell 状态恢复（cwd/env/history）。复杂度高且控制权不在 Agent。

手动重连：Agent 决定何时重连、重连几次，TermBridge 只提供 `reconnect_session` 工具。符合 TermBridge "给 Agent 可编程 Terminal" 的定位。

### 调研中否决的方案

| 方案 | 否决理由 |
|---|---|
| 自动重连（read task 退出时自动重试） | 退避策略 / 重试次数 / 并发竞态复杂；Agent 失去控制权 |
| 保留 buffer 的重连（handle 原地替换） | Session.handle 不可变，改可变性影响所有方法；read task 持有旧 handle clone |
| 落盘持久化（ADR-0004 留的口子） | daemon 大改，风险高；Phase 6-A 先解决交互式 session 重连 |

## Decision

### 1. MVP 范围：手动 reconnect + 断线感知

**做**：
- `ReadOutputResult` 新增 `session_state` 字段，Agent 可感知断线
- 新增 `reconnect_session` MCP 工具，Agent 显式触发重连
- 重连后复用 session_id，新建 handle + 重启 read task
- shell 状态恢复：重连后自动发送 `cd <last_cwd>` 恢复工作目录

**不做**：
- 自动重连（退避 / 重试次数 / 并发）
- buffer 历史保留（重连后 buffer 从新开始，Agent 可 `pwd` + `ls` 重建上下文）
- 环境变量 / history / 后台进程恢复（shell 进程已死，无法恢复）

### 2. 断线感知：ReadOutputResult 扩展

```rust
pub struct ReadOutputResult {
    // ... 现有字段
    pub output: Vec<u8>,
    pub cursor: u64,
    pub has_more: bool,
    pub is_truncated: bool,
    pub matched: bool,
    pub timed_out: bool,
    pub mode: ReadMode,
    /// Phase 6-A：session 当前状态，Agent 据此判断是否断线。
    /// Ready = 正常；Lost = 已断开（buffer 仍可读但 PTY 不可写）；
    /// Closed = 已关闭。
    pub session_state: String,
}
```

Agent 检测 `session_state == "lost"` 时，可选择 `reconnect_session` 或 `close_session`。

### 3. reconnect_session 工具

```json
{
  "name": "reconnect_session",
  "description": "Reconnect a lost session. Re-establishes SSH connection + PTY. Buffer history is not preserved (starts fresh). Attempts to restore previous working directory via cd. Only works on Lost sessions.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "session_id": { "type": "string" }
    },
    "required": ["session_id"]
  }
}
```

**返回**：
```json
// 重连成功
{ "status": "reconnected", "session_id": "...", "host": "...", "cwd_restored": true }

// session 非 Lost 状态
{ "status": "not_lost", "session_id": "...", "current_state": "ready" }

// 重连失败（SSH 连接 / 认证失败）
{ "status": "failed", "session_id": "...", "reason": "auth_failed" }
```

### 4. 重连流程

```text
reconnect_session(session_id)
    │
    ▼
取出旧 Session Arc
    │
    ├── state != Lost → 返回 not_lost
    │
    ▼
记录旧 Session 的 host 信息（HostName）
记录旧 Session 的 last_cwd（见下方 cwd 追踪）
    │
    ▼
close 旧 Session（清理旧 handle + abort read task）
    │
    ▼
provider.open(OpenTerminalRequest { host, pty_size, ... })
    │
    ├── 失败 → 返回 failed（旧 session 已 close，Agent 需 open_session 新建）
    │
    ▼
新建 Session（新 buffer），复用旧 session_id
    │
    ▼
DashMap.insert(session_id, new_session)
    │
    ▼
如果 last_cwd 已知：send_input("cd <last_cwd>\n")
    │
    ▼
返回 reconnected (cwd_restored: true/false)
```

### 5. cwd 追踪

重连后恢复工作目录需要知道断线前的 cwd。方案：

**MVP：不主动追踪 cwd，重连后默认 home 目录**

Agent 如需恢复 cwd，在 reconnect 后自己 `pwd` 确认或 `cd` 到目标目录。理由：
- 主动追踪 cwd 需要解析每次 send_input 的命令（`cd xxx`），或定期 `exec("pwd")` 探测
- 解析命令是领域知识（shell 语法），违反 ADR-0008 边界
- 定期 exec pwd 增加开销和复杂度

**后续可选**：如果 Agent 反馈需要 cwd 恢复，加一个 `last_cwd` 字段，由 Agent 通过 `set_session_cwd` 工具显式设置（Agent 自己知道 cd 到了哪里，比 TermBridge 解析命令更准确）。

### 6. SessionManager.reconnect_session 实现

```rust
pub async fn reconnect_session(&self, session_id: &str) -> Result<ReconnectResult, TermError> {
    // 1. 取出旧 session
    let old_session = self.get_session(session_id)?;
    
    // 2. 检查 state
    if old_session.state() != SessionState::Lost {
        return Ok(ReconnectResult::NotLost { 
            current_state: format!("{:?}", old_session.state()).to_lowercase() 
        });
    }
    
    // 3. 记录 host + pty_size
    let host_name = old_session.host().to_string();
    let pty_size = old_session.pty_size();
    
    // 4. close 旧 session（清理 handle + read task）
    old_session.close().await?;
    
    // 5. 重新解析 ssh config + provider.open
    let host = sshconfig::resolve(&host_name).await?;
    let handle = self.provider.open(OpenTerminalRequest {
        host: host.clone(),
        pty_size,
        persistent: false,  // reconnect 只支持交互式 session
        name: None,
    }).await?;
    
    // 6. 新建 Session，复用 session_id
    let new_session = Session::new_with_id(
        session_id.to_string(),  // 复用旧 id
        host_name,
        pty_size,
        handle,
    );
    
    // 7. 替换 DashMap entry
    self.sessions.insert(session_id.to_string(), Arc::new(new_session));
    
    Ok(ReconnectResult::Reconnected { 
        host: host.name,
        cwd_restored: false,  // MVP 不恢复 cwd
    })
}
```

### 7. Session::new_with_id

当前 `Session::new` 接收 `id: SessionId` 参数（由 SessionManager 生成），所以复用 id 只需传旧 id 即可，**不需要新增 `new_with_id` 方法**。`Session::new` 本身就接受任意 id。

### 8. ReconnectResult 结构

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReconnectResult {
    Reconnected {
        session_id: String,
        host: String,
        cwd_restored: bool,
    },
    NotLost {
        session_id: String,
        current_state: String,
    },
    Failed {
        session_id: String,
        reason: String,
    },
}
```

### 9. 约束

- **仅支持交互式 session 重连**：`persistent=true` 的 session 走 daemon 路径，有自己的 attach/detach 机制（Phase 3-B），不走 reconnect_session
- **重连后 buffer 从新开始**：不保留旧 buffer 历史（Agent 可 `pwd` + `ls` 重建上下文）
- **重连失败后旧 session 已 close**：Agent 需 `open_session` 新建（新 session_id）
- **不恢复 shell 状态**：环境变量 / history / 后台进程随旧 shell 进程死亡而丢失

## Consequences

### 正面

- **Agent 可感知断线**：`read_output` 返回 `session_state`，Agent 不再盲操作
- **手动重连**：Agent 有完全控制权，决定何时重连、重连几次
- **实现简单**：复用现有 `provider.open` + `Session::new`，不改 Session 内部结构
- **session_id 稳定**：重连后 Agent 继续用原 session_id，无需更新引用

### 负面 / 代价

- **buffer 历史丢失**：重连后从新 buffer 开始（Agent 需重建上下文）
- **不自动**：Agent 必须显式调用 reconnect，不调用则 session 永远 Lost
- **重连失败需新建**：重连失败后旧 session 已 close，必须 open_session 新建
- **shell 状态丢失**：cwd / env / history 不恢复

### 边界

- 不做自动重连（退避 / 重试 / 并发控制）
- 不做 cwd 主动追踪（解析 shell 命令违反 ADR-0008）
- 不做 persistent session 重连（已有 attach/detach）
- 不做 buffer 历史保留（需改 Session.handle 可变性，改动大）

## Implementation Plan

### 文件清单

| 文件 | 操作 | 职责 |
|---|---|---|
| `docs/adr/0010-session-reconnect.md` | 新增（本文件） | 决策记录 |
| `src/domain/output.rs` | 修改 | `ReadOutputResult` 新增 `session_state` 字段 |
| `src/domain/session.rs` | 修改 | `read_output` 填充 `session_state` |
| `src/application/sessions.rs` | 修改 | 新增 `reconnect_session` 方法 + `ReconnectResult` |
| `src/transport/mcp/server.rs` | 修改 | 注册 `reconnect_session` 工具 + read_output 返回含 state |

### 实现顺序

1. **ReadOutputResult 扩展**：加 `session_state` 字段 + 所有 read_output 调用点填充
2. **reconnect_session 业务逻辑**：SessionManager 新增方法
3. **MCP 工具注册**：server.rs 新增 reconnect_session 工具
4. **e2e 验证**：连 171 → 断网 → read_output 感知 Lost → reconnect → 恢复

## Alternatives Considered

### A. 自动重连（read task 退出时自动重试）

**否决**：退避策略 / 重试次数 / 并发竞态复杂；Agent 失去控制权；不符合"可编程 Terminal"定位。

### B. 保留 buffer 的重连（handle 原地替换）

**否决**：Session.handle 不可变，改可变性（`Arc<Mutex<Arc<...>>>`）影响所有 handle 访问点；read task 持有旧 handle clone，需协调 abort 时机；改动面大。

### C. 落盘持久化（ADR-0004 留的口子）

**否决（Phase 6-A 不做）**：daemon 大改，风险高。先解决交互式 session 重连，落盘留 Phase 6-B。

### D. cwd 主动追踪（解析 cd 命令）

**否决**：解析 shell 命令是领域知识，违反 ADR-0008 边界。Agent 自己知道 cd 到了哪里，比 TermBridge 解析更准确。

## Relationships

- **继承 ADR-0008 边界**：不解析 shell 命令（cwd 追踪），不做自动编排（自动重连）
- **复用 ADR-0005 安全模型**：重连走 provider.open → 复用 SSH 认证 / host key 校验
- **不依赖 ADR-0004 persistent runtime**：reconnect 只支持交互式 session

## Implementation Status

**Status**: Implemented（2026-08-10）

### 实施清单

| 文件 | 改动 | 状态 |
|---|---|---|
| `src/domain/output.rs` | `ReadOutputResult` 新增 `session_state` 字段 | ✅ |
| `src/domain/session.rs` | `read_output` 填充 `session_state` | ✅ |
| `src/application/sessions.rs` | `session_state()` + `reconnect_session()` + `ReconnectResult` | ✅ |
| `src/application/sessions.rs` | `cleanup_detached_session` 修正：只移除 Closed，不移除 Lost（保留供 reconnect） | ✅ |
| `src/transport/mcp/server.rs` | `ReconnectSessionParams` + `reconnect_session` 工具注册 + `ReadOutputDto.session_state` | ✅ |

### 验证

- **单元测试**：33 个 sessions::tests 全通过，含 5 个 Lost 状态边界测试 + 2 个 cleanup 行为测试
- **e2e 验证**（`examples/e2e_phase6_reconnect.ps1`，目标 192.0.2.171）：8 步全 PASS
  1. open_session → session_state=ready
  2. send "exit" 触发 Lost
  3. read_output → session_state=lost（Agent 感知断线）
  4. Lost 状态 send_input → SESSION_CLOSED
  5. reconnect_session → status=reconnected, session_id 复用
  6. reconnect 后 read_output → session_state=ready
  7. reconnect 后执行 echo 命令成功
  8. Ready 状态 reconnect → not_lost
- **Phase 6-B 可靠性加固 e2e**（2026-08-10，目标 192.0.2.171）：
  - 场景 1（`examples/e2e_phase6b_kill_sshd.ps1`）：远端 `pkill -9 sshd session` → read task EOF → Lost → reconnect → ready。7 步全 PASS，验证 sshd 进程强杀后重连
  - 场景 2（`examples/e2e_phase6b_keepalive.ps1`）：连接空闲 40s（超过 keepalive 3×10s miss 阈值）→ 仍 ready → 命令成功。4 步全 PASS，验证 keepalive 维持空闲连接活性

### 实施中发现的设计冲突与修正

**冲突**：Phase 1 的 `cleanup_detached_session` 用 `is_detached()`（含 Lost）判断移除，导致 Lost session 在 send_input/send_control 时被立即移除，reconnect_session 找不到 session（SESSION_NOT_FOUND）。

**修正**：`cleanup_detached_session` 改为只移除 `is_closed()` 的 session。Lost session 保留供 reconnect，兜底清理由 idleReaper（30min 超时）负责。
