# ADR-0015：Provider API 冻结

- **Status**: Accepted
- **Date**: 2026-08-11
- **Phase**: 7（冻结点）
- **Supersedes**: —
- **Depends on**: ADR-0004（persistent runtime）、ADR-0008（scope boundary）、ADR-0013（Agent Terminal Protocol）

## 1. Motivation

TermBridge 从 Phase 0 走到 Phase 7，Provider 抽象经历了多轮演化：

- Phase 0-C：`TerminalProvider` + `TerminalHandle` 初版（SSH only）
- Phase 1：加 `Any` supertrait + `as_any()` 以支持 SFTP 下转
- Phase 3：`PersistentProvider` + `PersistentTerminalHandle` 引入，detach/attach 能力如何暴露成为焦点
- Phase 5-B：`detect_remote_env` 通过 handle 下转访问 SSH exec channel
- Phase 7-A：CLI raw mode 需要 `read_raw` / `write_raw`，决定放在 SessionManager 而非 trait

每一步都面临"某能力放 trait 还是 downcast"的决策。现在 trait 形态已稳定，两个 Provider（SshProvider / PersistentProvider）验证了抽象的完备性，33/33 P0 测试 + cross-restart E2E 回归通过。**冻结 API，为后续 Provider 扩展（Local / Docker / 容器）提供稳定契约。**

## 2. 决策

### 2.1 TerminalProvider trait（2 方法）

```rust
#[async_trait]
pub trait TerminalProvider: Send + Sync {
    /// 创建一个 Terminal Backend，返回 Handle。
    async fn open(
        &self,
        request: OpenTerminalRequest,
    ) -> Result<Arc<dyn TerminalHandle>, TermError>;

    /// 下转 &self 为 &dyn Any，供 SessionManager 访问 Provider 特有能力
    /// （如 PersistentProvider 的 list_remote_sessions / attach_remote_session）。
    fn as_any(&self) -> &dyn Any;
}
```

**职责**：工厂角色。`SessionManager` 持有 `Arc<dyn TerminalProvider>`，通过 `open()` 创建 handle，不关心是 SSH / Local / Docker。

**`open()` 返回 `Arc<dyn TerminalHandle>` 而非 `Box`**：PTY read task 需要共享 handle 调 `read()`，Arc 让 Session（write/control/close）与 read task（read）各持一份。

### 2.2 TerminalHandle trait（6 方法）

```rust
#[async_trait]
pub trait TerminalHandle: Send + Sync + Any {
    /// 读一批原始 PTY output。Ok(None) = PTY EOF / channel closed。
    async fn read(&self) -> Result<Option<Bytes>, TermError>;

    /// 写输入到 PTY（立即返回，不等命令完成）。
    async fn write(&self, data: &[u8]) -> Result<(), TermError>;

    /// 发控制字符（Ctrl+C / Ctrl+D / Ctrl+Z / Tab / Enter / Escape）。
    async fn send_control(&self, c: ControlKey) -> Result<(), TermError>;

    /// 调整 PTY 尺寸（window_change）。
    async fn resize(&self, size: PtySize) -> Result<(), TermError>;

    /// 关闭 PTY + channel + 远端 shell（幂等）。
    async fn close(&self) -> Result<(), TermError>;

    /// 下转 &self 为 &dyn Any，供 SessionManager 访问 handle 特有能力
    /// （如 SshTerminalHandle 的 SFTP、PersistentTerminalHandle 的 detach）。
    fn as_any(&self) -> &dyn Any;
}
```

**职责**：PTY 后端句柄。生命周期与 PTY channel 绑定，`close()` 释放远端 shell。

**`Send + Sync + Any`**：`Send + Sync` 让 `Arc<dyn TerminalHandle>` 可跨线程（read task 持有 clone）；`Any` supertrait 让 `as_any()` 可下转到具体类型。

### 2.3 统一操作（6 个）

SessionManager 基于 trait 的 2+6=8 个方法，对上层暴露 6 个统一操作：

| 统一操作 | 调用路径 |
|---------|---------|
| open | `provider.open()` → `Session::new()` |
| send | `session.send_input()` → `handle.write()` |
| read | `session.read_output()` → `OutputEngine`（由 `handle.read()` 灌注） |
| wait | `session.read_output(wait_for=...)` → `OutputEngine` 匹配 |
| interrupt | `session.send_control()` → `handle.send_control()` |
| close | `session.close()` → `handle.close()` |

**任何实现 8 个 trait 方法的 Provider 自动获得这 6 个统一操作。** CLI raw mode 的 `read_raw` / `write_raw` / `resize` 是 SessionManager 直接透传 handle（绕过 OutputEngine / Policy），不新增 trait 方法。

## 3. 为什么 detach/attach 不放入 trait

### 3.1 语义不对称

| 能力 | SshProvider | PersistentProvider |
|------|-------------|-------------------|
| detach | ✗ 语义不存在（SSH 直连，断开即销毁） | ✓ 远端 daemon 保活 PTY |
| attach | ✗ 语义不存在 | ✓ 重连到 daemon 已有 session |
| list_remote_sessions | ✗ 无 daemon | ✓ daemon session 列表 |
| SFTP | ✓ 有 SSH channel | ✗ 无直接 SSH channel |
| exec channel | ✓ 有 SSH session | ✗ 无直接 SSH session |

**detach/attach 是 Persistent 特有能力，SFTP/exec 是 SSH 特有能力。** 把任一方放入 trait，另一方就要 stub 返回 `Unsupported`，trait 被污染。

### 3.2 downcast 模式

特有能力通过 `as_any()` 下转访问：

```rust
// SessionManager::detach_session
let persistent = handle
    .as_any()
    .downcast_ref::<PersistentTerminalHandle>()
    .ok_or_else(|| TermError::InvalidArgument("not a persistent handle".into()))?;
persistent.detach().await?;

// SessionManager::sftp_transfer
let ssh_handle = handle
    .as_any()
    .downcast_ref::<SshTerminalHandle>()
    .ok_or_else(|| TermError::InvalidArgument("not an SSH handle".into()))?;
ssh_handle.open_sftp_provider().await?;
```

**优点**：
- trait 保持精简（只含所有 backend 共有的操作）
- 特有能力类型安全（downcast 失败返回明确错误，非运行时 panic）
- 新增 Provider 无需为不相关的能力写 stub

### 3.3 as_any 不提供默认实现

```rust
fn as_any(&self) -> &dyn Any { self }
```

不放默认实现：默认 `self` 在 trait 对象上下文中无法编译（`Self` unsized）。实现方必须显式写 `fn as_any(&self) -> &dyn Any { self }`，依赖 `Self: Any`。

## 4. 现有 Provider 实现对照

### 4.1 SshProvider / SshTerminalHandle

| trait 方法 | 实现 |
|-----------|------|
| `provider.open()` | russh connect + auth + RequestPty + shell |
| `provider.as_any()` | `self` |
| `handle.read()` | `ssh_channel.read()` |
| `handle.write()` | `ssh_channel.data()` |
| `handle.send_control()` | `ssh_channel.data(ControlKey::as_bytes())` |
| `handle.resize()` | `ssh_channel.window_change()` |
| `handle.close()` | `ssh_channel.close()` + session drop |
| `handle.as_any()` | `self` |

**特有能力（downcast 访问）**：`open_sftp_provider()`、`exec_command()`（detect_remote_env 用）。

### 4.2 PersistentProvider / PersistentTerminalHandle

| trait 方法 | 实现 |
|-----------|------|
| `provider.open(persistent=false)` | 委托 SshProvider（Interactive 路径） |
| `provider.open(persistent=true)` | check_runtime → deploy → bootstrap_daemon → session_create → session_attach |
| `provider.as_any()` | `self` |
| `handle.read()` | 优先 initial_data 快照，然后 pty_data_rx + event_rx select |
| `handle.write()` | `daemon.session_send_input()` |
| `handle.send_control()` | `daemon.session_send_control()` |
| `handle.resize()` | `daemon.session_resize()` |
| `handle.close()` | `daemon.session_close()`（幂等） |
| `handle.as_any()` | `self` |

**特有能力（downcast 访问）**：`detach()`、`list_remote_sessions()`、`attach_remote_session()`。

## 5. 新增 Provider 接入指南

以 LocalProvider（Phase 5+ 规划，本地 PTY）为例：

### 5.1 实现 8 个 trait 方法

```rust
pub struct LocalProvider;

#[async_trait]
impl TerminalProvider for LocalProvider {
    async fn open(&self, request: OpenTerminalRequest) -> Result<Arc<dyn TerminalHandle>, TermError> {
        // 用 portable-pty 创建本地 PTY
        let handle = Arc::new(LocalTerminalHandle::new(request.pty_size)?);
        Ok(handle as Arc<dyn TerminalHandle>)
    }

    fn as_any(&self) -> &dyn Any { self }
}

pub struct LocalTerminalHandle { /* portable-pty 句柄 */ }

#[async_trait]
impl TerminalHandle for LocalTerminalHandle {
    async fn read(&self) -> Result<Option<Bytes>, TermError> { /* ... */ }
    async fn write(&self, data: &[u8]) -> Result<(), TermError> { /* ... */ }
    async fn send_control(&self, c: ControlKey) -> Result<(), TermError> { /* ... */ }
    async fn resize(&self, size: PtySize) -> Result<(), TermError> { /* ... */ }
    async fn close(&self) -> Result<(), TermError> { /* ... */ }
    fn as_any(&self) -> &dyn Any { self }
}
```

### 5.2 自动获得 6 个统一操作

实现完成后，LocalProvider 自动支持：
- `open_session` → `provider.open()`
- `send_input` → `handle.write()`（经 Policy 拦截）
- `read_output` → `OutputEngine`（由 `handle.read()` 灌注）
- `wait_for` → `OutputEngine` 匹配
- `send_control` → `handle.send_control()`
- `close_session` → `handle.close()`

无需修改 SessionManager、MCP server、CLI、GUI 任何上层代码。

### 5.3 特有能力按需 downcast

如果 LocalTerminalHandle 有特有能力（如 `spawn_with_pty()`），通过 `as_any()` 下转访问，不污染 trait。

## 6. 冻结后的变更约束

**trait 方法签名冻结**，以下变更视为破坏性：

- 新增 trait 方法（必须给默认实现或所有 Provider 同步更新）
- 修改方法签名（参数 / 返回类型）
- 移除方法

**允许的非破坏性变更**：
- 新增 Provider 实现（SshProvider / PersistentProvider 之外的）
- 新增特有能力（通过 downcast，不修改 trait）
- 修改 trait 方法的内部实现
- 扩展 `OpenTerminalRequest` / `PtySize` / `ControlKey` 的字段（向后兼容）

## 7. 验证状态

- **SshProvider**：Phase 1 实现，33/33 P0 测试通过
- **PersistentProvider**：Phase 3 实现，cross-restart E2E 回归通过（detach → kill MCP → restart → list → attach → read history → 继续交互）
- **downcast 模式**：SFTP（Phase 1）、detect_remote_env（Phase 5-B）、detach/attach（Phase 3-B）三条路径验证通过
- **CLI raw mode**：`read_raw` / `write_raw` / `resize` 放在 SessionManager 透传，Phase 7-A 验证通过
