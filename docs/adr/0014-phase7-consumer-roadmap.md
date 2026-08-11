# ADR-0014：Phase 7 Consumer Layer Roadmap (CLI + GUI + Cross-Platform)

- **Status**: Accepted
- **Date**: 2026-08-11
- **Phase**: 7
- **Supersedes**: —
- **Depends on**: ADR-0012（执行语义契约）、ADR-0013（Agent Terminal Protocol）、ADR-0008（职责边界）、ADR-0004（persistent runtime）

## 1. Context

### 1.1 当前状态：Runtime Contract 已冻结

Phase 6-C 完成后，TermBridge 的底层 runtime 已完成冻结：

```
Runtime Contract（已冻结）
 ├─ ADR-0012: 9 大 runtime 契约（33/33 P0 PASS）
 ├─ ADR-0013: Agent Terminal Protocol（7 条消费者规则）
 ├─ Provider API: 已冻结（TerminalProvider 2 方法 + TerminalHandle 6 方法）
 ├─ cross-restart E2E: ALL PASSED（detach→kill MCP→restart→list→attach→read history）
 └─ T17 attach/cursor 边界: 8/8 PASS
```

### 1.2 转折点：从"runtime 能否工作"到"runtime 是否好用"

之前所有工作在证明「这个 runtime 能不能可靠工作」。下一步应该转向证明「人和 Agent 实际使用起来是否舒服」。

当前 TermBridge 缺的不是能力，而是一个「真实入口」：
- AI Agent 已有 MCP stdio 入口（19 个工具）
- **人类管理员没有入口**（现有 cli/ 是 daemon 测试工具，不是人类工具）

### 1.3 现状核实

| 组件 | 现状 | 缺口 |
|---|---|---|
| 主控端 termbridge-mcp.exe | Rust 跨平台编译 | macOS/Linux 从未实际验证 |
| agentd | Linux-only（远端，合理） | — |
| credential helper | 架构跨平台（platform/{windows,linux,macos}.rs） | **macOS/Linux 是 stub，返回 Unsupported** |
| MCP resize 工具 | `TerminalHandle::resize` trait 方法已存在，两个 Provider 都实现 | MCP 未暴露 |
| CLI | cli/ crate 是 daemon 测试工具 | 不是人类管理员工具 |
| GUI | 无 | — |

## 2. Decision

### 2.1 路线总览

```
Phase 7（基于已冻结的 Runtime Contract）
 │
 ├─ 7-A: CLI MVP + T16 resize（合并，最高优先级）
 │   ├─ 新建 termbridge CLI crate（人类管理员工具）
 │   ├─ 命令: hosts / connect / sessions / attach / detach
 │   ├─ raw mode + crossterm（支持 vim/top/journalctl -f）
 │   ├─ WINCH 信号 → TerminalHandle.resize()（T16 验证）
 │   ├─ MCP resize 工具暴露（AI Agent 用）
 │   └─ 验证: CLI 开 vim，resize 窗口，验证重绘
 │
 ├─ 7-B: 跨平台构建验证
 │   ├─ Windows（已验证）
 │   ├─ Linux 主控端手动编译验证
 │   ├─ macOS 主控端手动编译验证
 │   ├─ credential helper macOS/Linux 实现（Keychain / libsecret / tty prompt）
 │   └─ GitHub Actions CI（windows/ubuntu/macos）
 │
 ├─ 7-C: GUI MVP（后置）
 │   ├─ Tauri + React + xterm.js
 │   ├─ Session Manager（hosts 列表 + sessions 列表）
 │   ├─ attach/detach 按钮
 │   ├─ 终端窗口（xterm.js，resize 支持）
 │   └─ Rust 后端直接调 termbridge core（不走 MCP）
 │
 └─ 7-D: 生态（独立项目，不在此 Phase）
     ├─ Playbook Engine
     ├─ Agent Skill
     └─ Workflow
```

### 2.2 优先级

| 优先级 | Phase | 理由 |
|---|---|---|
| ⭐⭐⭐⭐⭐ | 7-A CLI + T16 | CLI 是 runtime 第一消费者，验证好用性 |
| ⭐⭐⭐⭐ | 7-B 跨平台 | TermBridge 定位是跨平台 runtime，需验证 |
| ⭐⭐⭐ | 7-C GUI | 后置，CLI 已能验证 runtime |
| ⭐⭐ | 7-D 生态 | ADR-0008 已划定边界，独立项目 |

## 3. Phase 7-A: CLI MVP + T16 resize

### 3.1 目标

证明人类管理员可以舒服使用 TermBridge，达到 `ssh ubuntu-test` 90% 体验。

### 3.2 CLI UX（写代码前先冻结）

```bash
# 查看配置主机
termbridge hosts
# 输出:
# NAME          STATUS
# ubuntu-test   online
# wazuh-prod    online

# 连接（进入交互式终端）
termbridge connect ubuntu-test
# 进入:
# root@ubuntu-test:~#
# 支持: Ctrl+C / Ctrl+D / Ctrl+Z / vim / top / htop / journalctl -f / resize

# 查看远端 persistent session
termbridge sessions
# 输出:
# ID        NAME        STATE
# a81f      wazuh       detached
# b92a      build       running

# attach 到 persistent session
termbridge attach a81f
# 进入远端 session 终端

# detach 当前 session（快捷键: Ctrl+D 不行，用命令）
termbridge detach <session-id>
```

### 3.3 必须支持的能力

| 能力 | 实现 |
|---|---|
| host discovery | `termbridge hosts` 列出已配置主机 |
| open | `termbridge connect <host>` 开新 session |
| interactive terminal | raw mode，stdin/stdout 直连 PTY |
| Ctrl+C / Ctrl+D / Ctrl+Z | raw mode 下直接传字节，不触发本地 SIGINT |
| resize | WINCH 信号 → `TerminalHandle.resize()` |
| vim / top / htop / journalctl -f | raw mode + resize 保障 |
| detach | `termbridge detach` 或快捷键（如 Ctrl+\） |
| attach | `termbridge attach <session-id>` |
| session list | `termbridge sessions` 列出远端 persistent session |

### 3.4 技术方案

#### 3.4.1 依赖

| 依赖 | 用途 |
|---|---|
| `crossterm` | raw mode / resize event / Windows console / ANSI |
| `clap` | CLI framework（命令解析） |
| `tokio` | async runtime（已有） |
| `termbridge`（本 crate） | Runtime API 调用 |

#### 3.4.2 架构

```
CLI Process
 │
 ├─ clap 解析命令
 │
 ├─ connect/attach → 调用 termbridge core API
 │   ├─ open_session(host, persistent=true)
 │   └─ attach_remote_session(host, session_id)
 │
 ├─ 进入交互式终端
 │   ├─ crossterm::enable_raw_mode()
 │   ├─ stdin task: stdin → handle.write()
 │   ├─ stdout task: handle.read() → stdout
 │   └─ resize task: WINCH/event → handle.resize()
 │
 └─ 退出: crossterm::disable_raw_mode() + close_session
```

#### 3.4.3 raw mode 核心逻辑

```rust
// 进入 raw mode
crossterm::terminal::enable_raw_mode()?;

// 拦截 Ctrl+C（raw mode 下不触发 SIGINT）
// stdin 字节直接传给 handle.write()

// 监听 resize 事件
let mut events = crossterm::event::poll?;
tokio::spawn(async move {
    loop {
        if let Ok(CEvent::Resize(cols, rows)) = read() {
            handle.resize(PtySize { rows, cols }).await?;
        }
    }
});

// stdin → PTY
tokio::spawn(async move {
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 1024];
    loop {
        let n = stdin.read(&mut buf).await?;
        if n == 0 { break; }
        handle.write(&buf[..n]).await?;
    }
});

// PTY → stdout
loop {
    if let Some(data) = handle.read().await? {
        stdout.write_all(&data).await?;
        stdout.flush().await?;
    } else {
        break; // PTY EOF
    }
}

// 退出时恢复
crossterm::terminal::disable_raw_mode()?;
```

### 3.5 T16 resize 验证方案

不单独写 MCP resize 测试脚本，改为 CLI 集成验证：

```
1. termbridge connect ubuntu-test
2. vim /tmp/test.txt
3. 调整终端窗口大小（80x24 → 160x50 → fullscreen）
4. 验证 vim 重绘正确
5. :q 退出 vim
6. top → resize → 验证布局
7. htop → resize → 验证布局
8. journalctl -f → resize → 验证输出
```

同时暴露 MCP `resize` 工具（AI Agent 用）：
- 参数：`session_id: String, cols: u16, rows: u16`
- 调用：`session.handle.resize(PtySize { rows, cols })`

### 3.6 现有 cli/ 处理

- **保留**：cli/ 是 daemon 测试工具，用于开发时手动验证 daemon，不删除
- **明确标注**：在 cli/src/main.rs 头部注释明确"这是 daemon 测试工具，不是人类管理员工具"
- **新 CLI 位置**：在主 crate 新增 `src/bin/termbridge.rs` 作为人类 CLI 入口（或新建 `crates/termbridge-cli/`）

### 3.7 验收标准

- [ ] `termbridge hosts` 列出主机
- [ ] `termbridge connect <host>` 进入交互式终端
- [ ] Ctrl+C / Ctrl+D / Ctrl+Z 正确传递
- [ ] vim 可正常使用，resize 后重绘正确
- [ ] top / htop 可正常使用，resize 后布局正确
- [ ] journalctl -f 可正常使用，resize 后输出正确
- [ ] `termbridge sessions` 列出远端 persistent session
- [ ] `termbridge attach <id>` attach 到 persistent session
- [ ] `termbridge detach` detach 当前 session
- [ ] MCP `resize` 工具暴露且可用

## 4. Phase 7-B: 跨平台构建验证

### 4.1 目标

确认一套代码三个平台编译运行，credential helper 全平台可用。

### 4.2 任务

#### 4.2.1 主控端编译验证

| 平台 | 任务 | 预期风险 |
|---|---|---|
| Windows | 已验证 | 无 |
| Linux | `cargo build --release` | 低（agentd 已 Linux 验证） |
| macOS | `cargo build --release` | 中（从未测试，可能有依赖问题） |

#### 4.2.2 credential helper 补全

当前状态：
- `platform/windows.rs`：CredUI 真实实现 ✅
- `platform/macos.rs`：stub，返回 Unsupported ❌
- `platform/linux.rs`：stub，返回 Unsupported ❌

需补全：
- **macOS**：Keychain API（`Security.framework`）或 fallback 到 `read tty`
- **Linux**：`libsecret`（GNOME）或 `kdialog` / `zenity`（GUI）或 `read -s tty`（fallback）

实现策略：
- 优先用系统原生 keychain
- fallback 到 tty prompt（无 GUI 环境可用）

#### 4.2.3 CI

GitHub Actions matrix：
```yaml
strategy:
  matrix:
    os: [windows-latest, ubuntu-latest, macos-latest]
```

产物：
- Windows: `termbridge-mcp.exe` + `termbridge.exe`(CLI) + `termbridge-credential-helper.exe`
- Linux: `termbridge` + `termbridge-credential-helper`
- macOS: `termbridge` + `termbridge-credential-helper`

### 4.3 命名调整（可选）

| 现名 | 新名 | 理由 |
|---|---|---|
| `termbridge-credential-prompt` | `termbridge-auth-helper` | 去掉 Windows 绑定感（`.exe` 是平台后缀，不是名字的一部分） |

非阻塞，可在 7-B 顺手改。

## 5. Phase 7-C: GUI MVP

### 5.1 目标

可视化 session manager，不自研终端 emulator。

### 5.2 范围

**做**：
- Hosts 列表（左侧）
- Sessions 列表（中间）
- 终端窗口（右侧，xterm.js）
- attach/detach 按钮
- resize 同步

**不做**：
- 文件管理器
- 多 tab terminal
- AI workflow
- 命令历史分析
- 配置编辑器

### 5.3 技术方案

```
GUI Process（Tauri）
 │
 ├─ 前端: React + xterm.js
 │   ├─ Hosts/Sessions 列表 UI
 │   ├─ xterm.js 终端渲染
 │   └─ resize event → Tauri IPC → Rust 后端
 │
 ├─ 后端: Tauri Rust
 │   ├─ 直接调用 termbridge core（进程内，不走 MCP）
 │   ├─ open_session / attach_remote_session
 │   ├─ stdin: xterm.js input → Tauri IPC → handle.write()
 │   ├─ stdout: handle.read() → Tauri IPC → xterm.js
 │   └─ resize: xterm.js resize → Tauri IPC → handle.resize()
 │
 └─ 不走 MCP JSON-RPC（MCP 是 Agent 协议，不应作内部 API）
```

### 5.4 关键约束

- **xterm.js 处理终端渲染**：不自研 ANSI parser / cursor movement / selection / clipboard / IME
- **Rust 后端直调 core**：避免 MCP 协议开销，性能更好
- **resize 同步**：xterm.js resize event → Tauri IPC → `handle.resize()`

## 6. Phase 7-D: 生态（独立项目）

不在本 Phase 实施，仅记录方向：

- **Playbook Engine**：独立项目，不污染 core（ADR-0008 已划定边界）
- **Agent Skill**：基于 ADR-0013 的 7 条规则封装
- **Workflow**：编排层，遵守 ADR-0013

## 7. Consequences

### 7.1 新增依赖

- `crossterm`：CLI raw mode / resize / ANSI
- `clap`：CLI 命令解析（可能已有，需确认）
- `tauri` + `react` + `xterm.js`：GUI（7-C 才引入）

### 7.2 Crate 结构调整

```
termbridge/
├── Cargo.toml              # 主 crate（termbridge-mcp.exe MCP server + termbridge.exe CLI）
├── src/
│   ├── bin/
│   │   └── termbridge.rs   # 新增:人类 CLI 入口
│   └── ...                 # 现有 core
├── cli/                    # 保留:daemon 测试工具
├── agentd/                 # 保留:远端 daemon（Linux-only）
└── crates/
    └── termbridge-credential-prompt/  # 可能改名 termbridge-auth-helper
```

### 7.3 不变的部分

- Runtime Contract（ADR-0012）不变
- Agent Terminal Protocol（ADR-0013）不变
- Provider API 不变
- MCP 工具集（除新增 resize）不变
- agentd 协议不变

## 8. Risks

| 风险 | 影响 | 缓解 |
|---|---|---|
| CLI raw mode 跨平台差异 | Windows/Linux/macOS terminal handling 不同 | crossterm 抹平，但需三平台验证 |
| macOS 首次编译 | 从未测试，可能有未知依赖问题 | 7-B 先手动编译，确认后再上 CI |
| credential helper macOS/Linux 实现 | 当前是 stub | 7-B 补全，fallback 到 tty prompt |
| GUI 工程量 | 即使 Tauri + xterm.js，前端开发仍需时间 | 7-C 后置，不阻塞 7-A/7-B |
| CLI scope 蔓延 | 容易加 dashboard / 多窗口 / session 管理中心 | 严格按 3.2 的 UX 冻结，不扩功能 |

## 9. References

- [ADR-0012](0012-execution-state-and-completion-protocol.md)：执行语义契约
- [ADR-0013](0013-agent-terminal-protocol.md)：Agent Terminal Protocol
- [ADR-0008](0008-scope-boundary.md)：TermBridge 定位与职责边界
- [ADR-0004](0004-remote-persistent-runtime.md)：persistent runtime 架构
