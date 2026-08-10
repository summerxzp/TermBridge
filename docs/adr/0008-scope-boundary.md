# ADR-0008：Scope Boundary — TermBridge 是 Remote Terminal Runtime，不是 AI Ops Platform

- **Status**: Accepted
- **Date**: 2026-08-10
- **Phase**: 3（基于两次 Wazuh 配置实战反馈确立）
- **Supersedes**: —

## Context

Phase 3-A W3 期间通过 PersistentProvider 完成了两次真实运维任务（启用 Wazuh archives、下发 agent.conf）。实战暴露了一个定位漂移风险：AI 拿到 Terminal 之后，会自然开始做运维编排（备份→修改→验证→激活→重启），并要求 TermBridge 提供「命令前置校验」「执行→验证闭环」「多步 playbook」「错误诊断」等能力。

如果无差别吸收这些反馈，TermBridge 会膨胀成「Ansible + SSH + MCP + Terraform + Rundeck」四不像，失去它当前最漂亮的核心：**一个稳定、可编程、面向 AI 的远程 Terminal Runtime**。

同时需要澄清一个常见误解：「在目标服务器装 agent」并非 TermBridge 的默认形态。默认模式仍是纯 SSH（Phase 1），仅在 `open_session(persistent=true)` 时才部署远端 runtime。因此组件应称 **Remote Runtime / Session Runtime**，而非「Remote Agent」——后者易被误解为「每台服务器装一个 AI Agent」。

### 两次实战的具体反馈分类

| 反馈 | 本质归属 | 是否进 TermBridge |
|---|---|---|
| 命令前置校验（如 `log_format=file` 拦截） | 领域知识（Wazuh/nginx/docker...） | ❌ 留给未来 OpsSkill 层 |
| 执行→验证闭环（改了自动验） | Remote Action Model | ❌ 未来 Workflow 层 |
| 多步 playbook | Ansible 类编排 | ❌ 未来 Workflow 层 |
| Execution Timeline（命令/输出关联回放） | Terminal 可观测性 | ✅ TermBridge 范畴 |
| session 生命周期正确性 | Persistent Runtime 地基 | ✅ Phase 3 核心 |

## Decision

### 1. 术语澄清

- **Remote Runtime / Session Runtime**：`termbridge-agentd` 的正式术语定位，不是「Remote Agent」
- binary 名 `termbridge-agentd` 作为历史命名保留（避免 breaking change），但所有文档/ADR 统一用「Remote Runtime」
- 三层能力分级：
  - **Level 0 — 纯 SSH**（Phase 1）：远端零安装，exec/PTY/SFTP，覆盖 90% 日常开发测试
  - **Level 1 — Remote Runtime**（Phase 3）：opt-in 部署单二进制，增加 detach/attach/session persistence/output replay
  - **Level 2 — Automation Agent**（未来，不做）：playbook/validator/snapshot/rollback 一体化——明确不在 TermBridge 路线

### 2. TermBridge 职责边界

**✅ 负责（Terminal Runtime 范畴）**：

- remote terminal 连接（SSH/PTY/Local/Docker Provider）
- persistent session（detach/attach/cross-restart 保活）
- file transfer（SFTP）
- execution stream（byte stream + cursor + wait_for）
- session history（OutputEngine RingBuffer + cursor offset）
- execution timeline / observability（命令-输出关联、回放）

**❌ 不负责（Workflow / Ops Platform 范畴）**：

- config validation（ossec.conf/filebeat.yml/nginx 等领域语法校验）
- service orchestration（systemctl restart 链式编排）
- infrastructure desired state（Terraform 类声明式收敛）
- compliance check（CIS/STIG 合规评估）
- deployment workflow（蓝绿/滚动/回滚策略）
- playbook / action graph / verify-rollback 模型

### 3. 扩展机制：hook 接口（预留，不在 Phase 3 实现）

TermBridge 不内嵌领域知识，但提供 **扩展点**供未来独立层调用：

```
未来 OpsSkill 层
       │ 调用
       ▼
TermBridge MCP
  ├── pre_command_hook  （发送前回调，可拦截/改写）
  ├── post_command_hook （输出后回调，可触发验证）
  └── execution_timeline （只读事件流，供 AI 诊断）
```

Phase 3 **不实现** hook 接口，仅在 ADR 中预留位置，避免过早抽象。execution_timeline 作为 observability 能力放在 Phase 4。

### 4. 未来 Workflow 层的独立性

若未来要做 playbook/action/verify/rollback，应作为**独立项目**（暂名 `TermFlow` 或 `TermOps`），基于 TermBridge MCP 构建，而非塞进 TermBridge 本体：

```
AI Agent
   │
   ▼
TermFlow (Workflow Layer)   ← 未来独立项目
   │
   ▼
TermBridge MCP (Terminal Runtime)   ← 当前项目
   │
   ▼
SSH / Remote Runtime / Local / Docker Provider
```

### 5. 路线图调整

基于实战反馈，Phase 3 之后不直接做 Workspace，而是先补可靠性：

| Phase | 内容 | 定位 |
|---|---|---|
| **3（当前）** | Persistent Runtime：session 生命周期正确性（client crash → daemon survive → attach → cursor restore → output replay） | Terminal Runtime 地基 |
| **4** | 可靠性：session replay、execution timeline、command/output correlation、observability | Terminal Runtime 加固 |
| **5** | Remote Workspace：project upload、environment detect、task context | Terminal Runtime 扩展 |
| **6** | Workflow Layer（独立模块/项目）：Action / Verify / Rollback / Playbook | 上层编排，非 TermBridge 本体 |

### 6. Phase 3 收敛焦点

Phase 3 剩余工作**只做 session 生命周期正确性**，不被实战反馈带偏去加 playbook/validator/snapshot/rollback：

- W4：跨 MCP 重启重连 e2e（detach → MCP 重启 → attach 读存量）
- W5/W6：3 个新 MCP 工具（list_remote_sessions / attach_remote_session / detach_session）+ e2e

## Consequences

**正面**：
- 防止功能蔓延，保持核心简洁可维护
- 术语清晰，避免「Remote Agent」误解
- 路线图明确，Phase 3 聚焦点收敛
- 为未来 Workflow 层留出干净的扩展接口

**负面/权衡**：
- 实战中「命令前置校验」「验证闭环」等痛点短期内仍需 AI 自行承担（通过 read_output wait_for 等现有能力组合）
- hook 接口暂不实现，未来 OpsSkill 层需要时再补

**约束后续任务**：
- 任何「加 config parser / playbook / validator」的提议均需先引用本 ADR 复核是否越界
- 新功能提案必须能归入「✅ 负责」清单，否则归 TermFlow 项目
