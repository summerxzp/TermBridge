# ADR-0006：OpenSSH config 兼容策略 —— `ssh -G` 子进程

- **Status**: Accepted
- **Date**: 2026-08-09
- **Phase**: 0-C
- **Supersedes**: —

## Context

TermBridge 需要消费用户的 `~/.ssh/config` 来获取连接参数（hostname / port / user / identityfile / proxyjump 等）。OpenSSH config 语法复杂，支持 `Include` / `Match` / `Host *` 通配 / `ProxyJump` / 多值字段等。

两条路线：
1. **`ssh -G <host>` 子进程**：调用系统 `ssh -G`，让 OpenSSH 自己解析完整 config，TermBridge 只消费最终展开的 `key value` 输出。
2. **`ssh2-config` crate**：纯 Rust 解析器，不依赖外部 ssh 二进制。

Phase 0-A（`examples/p0_ssh_config.rs`）实测 `ssh -G` 输出 74 个字段，覆盖 Include/Match/ProxyJump 全部场景；`ssh2-config` crate 对 Match/Include 支持不完整。

## Decision

**采用 `ssh -G <host>` 子进程策略。**

### 两层使用

1. **`list_hosts`（轻量）**：`application/hosts.rs` 直接扫描 `~/.ssh/config` 的 `Host` 行，过滤通配符，返回别名列表。**不调 `ssh -G`**（每个 Host 调一次太慢），仅展示用。
2. **`open_session`（完整解析）**：`infrastructure/sshconfig::resolve(alias)` 调 `ssh -G <alias>`，解析 stdout 为 `Host` 实体（hostname/port/user/identity_file/proxy_jump/strict_host_key_checking）。

### 解析规则（`parse_ssh_g`）

- `key value` 空格分隔，key 全小写
- `identityfile` 多值取第一个**存在**的文件（`is_file` 过滤），`~` 展开
- `proxyjump` 值为 `none` 视为无 proxy
- `port` 缺省 22，`user` 缺省当前系统用户名

### 依赖前提

- 目标机器需装 OpenSSH client（`ssh` 在 PATH）。Windows 10+ 自带 OpenSSH，Linux/macOS 默认有。
- Phase 0-A 已验证 Windows OpenSSH 的 `ssh -G` 输出格式与 Linux 一致。

## Consequences

- ✅ 完整复用 OpenSSH config 解析能力（Include/Match/ProxyJump/Canonicalize 零成本继承），无需自己实现 parser
- ✅ `list_hosts` 快速返回（纯文本扫描），`open_session` 才付 `ssh -G` 一次延迟（~50ms，可接受）
- ✅ 用户现有 `~/.ssh/config` 配置直接生效，零迁移成本
- ⚠️ 依赖系统 `ssh` 二进制存在——若用户环境无 OpenSSH，需 fallback 或报错（Phase 0 假设开发者环境有 ssh）
- ⚠️ `ssh -G` 是同步子进程，`open_session` 用 `tokio::process::Command` 异步等待，不阻塞 runtime
- ⚠️ `identityfile` 取第一个存在文件：若用户有多个 key 且第一个不对，认证会失败。Phase 1 可改为遍历尝试，或依赖 ssh-agent（见 `SshProvider::open` 的 NoneAuth 预留）
- ⚠️ 不解析 `Match exec` 等动态指令的语义——但 `ssh -G` 已替我们执行，TermBridge 只看最终值，无影响
