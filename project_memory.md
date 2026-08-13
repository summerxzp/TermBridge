# project_memory — 发版节点与决策记录

本文件记录发版节点与关键决策结论（§6.3 反馈决策原则：结论 + 理由）。仅项目内部使用。

## 发版节点

| 版本 | 日期 | 结论 |
|---|---|---|
| v0.2.0 | 2026-08-13 | Host Connection Policy（ADR-0017）全 7 步完成并发布。6 资产已上传，sha256 + 包结构验证通过（§2.3） |

## v0.2.0 关键决策与教训

### 决策：password + persistent 显式拒绝

- **结论**：`auth=password + session=persistent` 在弹密码前返回 `InvalidArgument`，不做静默降级
- **理由**：persistent runtime（check/deploy/bootstrap/exec）每一步开新 SSH 连接，依赖 key-based unattended auth；静默降级会破坏"用户请求的语义与实际执行语义必须一致"。ADR-0017 §2.3 已同步（§5 否决项 G/H）

### 决策：bootstrap 不修改 hosts.toml

- **结论**：`bootstrap_host` 成功返回 `hint`（建议手动改配置），配置只由用户显式编辑
- **理由**：Host Policy = 用户意图（§2.2 不可变原则），bootstrap 只改变 Remote State

### 教训：验证范围必须 ≥ 改动范围

- **事件**：open_session 签名 `bool → Option<bool>` 时漏改 `gui/src-tauri` 调用点，CI 三平台红（Release 不受影响）
- **根因**：本地验证只跑 `-p termbridge`，跨 crate 调用点未覆盖；打 tag 时 ci.yml 尚未全绿（违反 §8.3）
- **修复**：新增 `.githooks/pre-push`（镜像 ci.yml 矩阵，§8.4）；发版纪律：先 CI 绿再打 tag

### 教训：e2e 验证避免假阳性

- **事件**：`[hosts.192.168.1.180]` 被 TOML 解析为嵌套表，策略静默失效，首轮 e2e "通过"实为 system default
- **根因**：TOML 点号别名未加引号；加载器静默吞掉未知字段
- **修复**：加载 WARN + 文档提示引号写法；e2e 用可区分默认值的强断言（policy=persistent 不带参 → daemon session）

### Dogfooding 发现（真实 bug，已修复）

- `ssh -G` 在 Git for Windows 下阻塞等待 stdin EOF → open_session 挂起（stdin 置空修复）
- Git Bash 的 MSYS HOME 泄漏到 known_hosts 路径解析（环境问题，`~/.ssh/config` 加显式 `UserKnownHostsFile` 规避）

## 待办 / 备忘

- hint 字段（bootstrapped 状态）e2e 需全新无 key 主机验证（当前主机已有 key，返回 already_configured）
- 后台任务窗口站不可见导致密码弹窗静默取消（前台运行正常）——真实 MCP 客户端（IDE）场景待实测
- v1.0.0 准入条件：dogfooding ≥ 4 周、install 脚本、外部用户反馈（§1.2）
