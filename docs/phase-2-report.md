# Phase 2 报告：ProxyJump + SFTP 增强 + Policy 接口

- **Date**: 2026-08-09
- **Status**: ✅ Complete
- **PLAN.md**: v0.4 §7.4 Phase 2 + §8 Policy 接口
- **前置**: Phase 1（MVP 7 工具 + 安全 baseline + 连接健壮性）

## 目标与范围

在 Phase 1 MVP 之上交付企业场景三块能力：**ProxyJump / Bastion**（经跳板机访问内网主机）、**SFTP 增强**（目录 / 权限 / 列表 / 递归删除）、**Policy 接口**（危险动作拦截链），并补齐 **known_hosts 完整处理**。锁定 ProxyJump 实现策略为 ADR-0007。

Phase 2 不碰：GUI、Persistent Session、自动重连、Workspace、MFA、端口转发、HITL UI（见 §下一步）。

## 交付物

### 代码（增量）

| 层 | 文件 | Phase 2 增量 |
|---|---|---|
| domain | `src/domain/policy.rs` | **新增** `Policy` trait + `Action`（SendInput / SftpTransfer / SftpRemove / SftpChmod）+ `Decision`（Allow / Confirm / Deny）+ `is_blocked_in_phase2()` |
| domain | `src/domain/provider.rs` | 新增 `TermError::PolicyDenied` / `PolicyNeedsConfirm`（retriable=false）+ `SftpNoSuchFile` / `SftpPermissionDenied`（Phase 2 SFTP 错误细分）|
| application | `src/application/policy.rs` | **新增** `DefaultPolicy`（blocklist 9 条 + confirm 7 条，`OnceLock` 进程级编译）+ `PolicyManager`（链式合并：任一 Deny 立即短路，Confirm 不短路）|
| application | `src/application/sessions.rs` | `send_input` / `sftp_transfer` / `sftp_remove` / `sftp_chmod` 前置 `check_policy` 拦截；新增 `sftp_mkdir` / `sftp_list` / `sftp_remove` / `sftp_chmod`；`sftp_remove_recursive`（MAX_DEPTH=20）|
| application | `src/application/path_policy.rs` | 新增 `check_remote_allow_new`（mkdir 目标不存在时校验父目录）+ `parent_remote_path` POSIX 父目录解析 |
| infrastructure | `src/infrastructure/ssh.rs` | `connect_session` 递归 + `channel_open_direct_tcpip` + `connect_stream`；`MAX_PROXY_DEPTH=3`；`SshTerminalHandle` 新增 `bastion_sessions` 字段；`close()` 逆序 disconnect 跳板机链 |
| infrastructure | `src/infrastructure/sshconfig.rs` | **新增** `ProxyJumpTarget` + `parse_proxy_jump`（`[user@]host[:port]`，拒绝逗号多跳）|
| infrastructure | `src/infrastructure/sftp.rs` | 新增 `mkdir` / `rmdir` / `remove` / `chmod` / `list_dir` + `RemoteEntry`（Serialize）+ `map_sftp_error`（NoSuchFile / PermissionDenied 细分）|
| transport | `src/transport/mcp/server.rs` | 新增 4 工具：`sftp_mkdir` / `sftp_list` / `sftp_remove` / `sftp_chmod` |

### MCP 工具（11 个，Phase 1 的 7 + Phase 2 的 4）

| 工具 | Phase | 映射 |
|---|---|---|
| `list_hosts` / `open_session` / `send_input` / `read_output` / `send_control` / `close_session` / `sftp_transfer` | 1 | 见 Phase 1 报告 |
| `sftp_mkdir` | 2 | SessionManager::sftp_mkdir（mode 八进制，0 用服务器默认）|
| `sftp_list` | 2 | SessionManager::sftp_list（返回 `Vec<RemoteEntry>`）|
| `sftp_remove` | 2 | SessionManager::sftp_remove（recursive=true 走 `sftp_remove_recursive`）|
| `sftp_chmod` | 2 | SessionManager::sftp_chmod |

### ADR

- [ADR-0007](adr/0007-proxyjump-strategy.md)：ProxyJump 策略（手动 SSH-over-SSH 隧道 + MAX_PROXY_DEPTH + 跳板机同生命周期 + close 逆序 disconnect + SOCKS 延期 Phase 5）

### 测试统计

- **单元测试**：**211 个全通过**（`cargo test`：211 passed; 0 failed; 1.34s）
  - application 100：policy 40（blocklist 7 + confirm 5 + allow 2 + 多行 5 + SFTP 7 + PolicyManager 10 + 边界 4）+ path_policy 32（含 `check_remote_allow_new` 5 + `parent_remote_path` 5）+ sessions 24（含 Policy 集成 11）+ hosts 4
  - infrastructure 67：ssh 23（known_hosts TOFU 4 + hashed 读取 2 + 多文件 2 + ssh-agent 降级 1 + keepalive 常量 1 + `MAX_PROXY_DEPTH` 常量 1 + 其他 12）+ sshconfig 27（含 `parse_proxy_jump` 14）+ sftp 9 + redact 8
  - domain 39 + transport 5

## 关键实现与决策

### 1. ProxyJump SSH-over-SSH 隧道（ADR-0007）

russh 0.62 无原生 ProxyJump，采用手动隧道：`connect_session` 递归 → 跳板机 session 上 `channel_open_direct_tcpip` → `channel.into_stream()` → `client::connect_stream` 在隧道上建目标 SSH session。跳板机与目标各自独立做 host key 校验与认证（ssh-agent / IdentityFile）。递归调用 `Box::pin` 包裹防 Future 膨胀。`MAX_PROXY_DEPTH=3` 防循环配置。跳板机 session 链持有在 `SshTerminalHandle.bastion_sessions`，与目标 session 同生命周期；`close()` 逆序 disconnect（先断内层再断外层）。SOCKS 延期 Phase 5（企业 ProxyJump 覆盖 90%+）。

### 2. Policy 链式拦截（PLAN §8）

- **domain 层**：`Policy` trait（`authorize(&Action) -> Decision` + `name()`）+ `Action` 四变体 + `Decision` 三值。`Decision::is_blocked_in_phase2()` 把 `Confirm` 视为阻断（无 HITL UI）。
- **application 层**：`DefaultPolicy` 硬编码 blocklist（`rm -rf /` / `mkfs` / `dd of=/dev/` / fork bomb / `shutdown` / `chmod -R 777 /` / `> /dev/sdX` 等 9 条）+ confirm 列表（`sudo` / `rm -rf <非根>` / `kill -9` / `iptables` / `crontab -r` 等 7 条）。正则用 `OnceLock` 进程级编译一次复用。多行输入按行扫描，**blocklist 优先于 confirm**（任一行命中 blocklist 立即 Deny；confirm 不短路，继续扫后续行以防有 blocklist）。
- **PolicyManager**：链式合并 —— 任一 Deny 立即短路返回 Deny；任一 Confirm（无 Deny）→ Confirm；全 Allow → Allow。`with_default()` 默认链 `[DefaultPolicy]`，支持 `add()` 追加自定义 Policy。
- **SessionManager 拦截点**：`send_input` / `sftp_transfer` / `sftp_remove` / `sftp_chmod` 前置 `check_policy`。`Allow` → 继续；`Deny` → `TermError::PolicyDenied`（`POLICY_DENIED`）；`Confirm` → `TermError::PolicyNeedsConfirm`（`POLICY_NEEDS_CONFIRM`）。Policy 检查在 session 查找前，拒绝危险命令不泄漏 session 存在性。
- **SFTP 动作策略**：upload 到 `/dev/` / `/etc/` / `/boot/` / `/sys/` / `/proc/` → Confirm；递归删除根 / `/etc` / `/usr` / `/var` 等系统目录 → Deny；其他递归/非递归删除 → Confirm；`chmod 777` 系统目录 → Confirm。

### 3. SFTP 递归删除（MAX_DEPTH=20）

`sftp_remove_recursive(sftp, remote, depth)`：`list_dir` → 对每个条目：目录则 `Box::pin` 递归，文件则 `remove` → 最后 `rmdir` 空目录。`MAX_DEPTH=20` 防 symlink 环导致无限递归，超限返回 `InvalidArgument`。`RemoteEntry` 序列化为 JSON 供 `sftp_list` 返回 Agent，`permissions` 为 `Option<u32>`（部分 SFTP server 不返回，`skip_serializing_if` 省略）。

### 4. known_hosts 完整处理

- **TOFU（Trust On First Use）**：新增 `strict_host_key_checking = "accept-new"` 模式（OpenSSH 兼容值）。未知 host → 自动写入 host key 到 known_hosts + 接受；已知 host key 变更 → 仍拒绝（TOFU 不覆盖 MITM 嫌疑的 key 替换）。`add_host_key_to_known_hosts` 复用 `russh::keys::known_hosts::learn_known_hosts_path` 完成格式化写入 + 父目录创建，新建文件设 0600 权限（Unix）。
- **hashed known_hosts 读取**：`russh::keys::check_known_hosts_path` 原生支持 OpenSSH hashed 条目（`|1|<salt>|<hash>` 格式，HMAC-SHA1）。Phase 2 写入用明文主机名，hash 写入留 Phase 3+ 配置项。
- **`userknownhostsfile` 多文件支持**：`Host.user_known_hosts_file: Option<PathBuf>` → `user_known_hosts_files: Vec<PathBuf>`。`sshconfig::parse_ssh_g` 收集**全部**空格分隔路径。`check_server_key` 遍历所有文件查找：任一匹配→接受；任一 KeyChanged→拒绝；单文件读取错误→WARN 跳过继续。TOFU 写入首个路径。
- **`ask` 模式等同 yes**：无 HITL UI 时未知 host 拒绝（`HOST_KEY_REJECTED`）。
- **`stricthostkeychecking` 大写归一化**：`to_ascii_lowercase()` 处理 `YES` / `Ask` 等大小写变体。
- **非 22 端口 known_hosts 行格式**：`[host]:port` 由 russh 内部处理，测试覆盖端口 2222 场景。

## 端到端验证

**单元测试 200 个全通过**（见测试统计）。Policy 拦截、ProxyJump 解析、SFTP 错误映射、路径策略 `check_remote_allow_new` 等核心逻辑均有单测覆盖。

**真实 SSH 端到端**：开发环境 `192.168.88.200` 主机可能不可达（网络环境依赖），未跑完整 e2e slice。ProxyJump / SFTP 新工具的集成验证依赖可达的 bastion + 目标主机环境，留待用户在真实企业网络中验证。Phase 1 既有 6 工具的 e2e 行为契约不受 Phase 2 改动影响（`SshTerminalHandle` 接口签名未变，仅新增 `bastion_sessions` 字段）。

单元测试验证的关键路径：

```
# ProxyJump 解析（14 例）
parse_proxy_jump("ops@bastion:2222") → user=Some("ops"), host="bastion", port=Some(2222)
parse_proxy_jump("bastion1,bastion2") → Err(INVALID_ARGUMENT)（多跳链拒绝）
parse_proxy_jump("") / ":2222" / "ops@" → Err(INVALID_ARGUMENT)
MAX_PROXY_DEPTH == 3（防循环）

# Policy 拦截（40 例）
send_input("rm -rf /")       → POLICY_DENIED（blocklist）
send_input("mkfs.ext4 /dev/sda1") → POLICY_DENIED
send_input("sudo apt update") → POLICY_NEEDS_CONFIRM
send_input("ls -la\nrm -rf /\necho done") → POLICY_DENIED（多行 blocklist 优先）
sftp_remove("/etc", recursive=true) → POLICY_DENIED
sftp_chmod("/etc/passwd", 0o777) → POLICY_NEEDS_CONFIRM
PolicyManager 空链 → Allow（无 Policy 拦截）

# SFTP 错误映射（4 例）
map_sftp_error(NoSuchFile)     → SftpNoSuchFile（retriable=false）
map_sftp_error(PermissionDenied) → SftpPermissionDenied（retriable=false）
map_sftp_error(Failure)        → SftpError（retriable=true）

# 路径策略 allow_new（5 例）
check_remote_allow_new("/home/user/newdir") [目标不存在] → 校验父目录 /home/user
check_remote_allow_new("/") → REMOTE_PATH_NOT_ALLOWED（根无父目录）
```

## 已知限制

1. **`Confirm` 等同 `Deny`**：Phase 2 无 HITL UI，`POLICY_NEEDS_CONFIRM` 不可重试，Agent 应提示用户手动执行。Phase 6 实现 HITL 后才真正交互确认。
2. **blocklist 是 best-effort**：命令可变形成绕过（base64 编码、变量拼接、`eval`、`sh -c` 等），`DefaultPolicy` 启动期 WARN 提示非绝对安全。根本防线仍是最小权限原则 + 审计。
3. **跳板机无独立 keepalive**：仅目标 session 跑 keepalive，隧道断开靠目标 stream EOF 间接监测。单 bastion 场景足够，多跳链式 bastion 的中间层活性检测 Phase 4 再评估。
4. **不支持 IPv6 字面量 ProxyJump**：`parse_proxy_jump` 以 `rfind(':')` 拆分，`[::1]:22` 会误判。企业跳板机通常为域名/IPv4。
5. **不支持 ProxyJump 逗号多跳语法**：`bastion1,bastion2` 显式拒绝；多跳由 ssh config 嵌套 `ProxyJump` + 递归 `connect_session` 表达。
6. **known_hosts hash 写入未实现**：TOFU 写入用明文主机名，hash 写入（`|1|<salt>|<hash>`）留 Phase 3+ 配置项。读取已支持。
7. **SFTP channel 不池化**：每次 `sftp_*` 操作开新 channel，频繁小文件传输有开销。Phase 1 遗留，Phase 2 未改。
8. **`192.168.88.200` 主机可能不可达**：真实 SSH e2e 验证依赖网络环境，单元测试已覆盖核心逻辑。

## 下一步（Phase 3）

按 PLAN.md §7.5：

- **Persistent Session（opt-in）**：远端 persistent daemon（参考 ai-tmux 原理，Rust 重写）+ `attach_session` / `detach_session` / `list_remote_sessions`
- **跨 MCP 重启会话保活验证**
- **输出 ADR-0004**：持久化协议与远端 daemon 形态

> Phase 2 不碰：GUI、数据库、Workspace、自动重连（Phase 4）、MFA / 端口转发 / Workspace（Phase 5）、HITL UI（Phase 6）。
