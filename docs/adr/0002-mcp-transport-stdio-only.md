# ADR-0002：MCP transport = stdio only（MVP）

- **Status**: Accepted
- **Date**: 2026-08-09
- **Phase**: 0-C
- **Supersedes**: —

## Context

MCP 协议支持多种 transport（stdio / HTTP+SSE / WebSocket）。TermBridge 作为「AI Agent 调用的远程终端桥」，需要决定 MVP 阶段支持哪些 transport。

考虑因素：
1. **调用方形态**：MVP 调用方是本地 IDE 内嵌的 Agent（如 Trae / Claude Desktop），通过子进程拉起 TermBridge，stdin/stdout 通信。
2. **实现成本**：stdio 最简单，rmcp 原生支持 `serve(stdio())`；HTTP/SSE 需要额外 HTTP server + 鉴权 + 连接管理。
3. **安全边界**：stdio 隐含「调用方即本机进程」，天然隔离；HTTP 暴露端口需要额外鉴权，与 Phase 0「最小可用」目标冲突。
4. **日志冲突**：stdio 通道专用于 JSON-RPC，日志必须走 stderr（`tracing_subscriber::fmt().with_writer(stderr)`）。

## Decision

**Phase 0 / 1：只支持 stdio transport。**

- `main.rs` 调用 `TermBridgeServer::serve_stdio()`，内部 `self.serve(rmcp::transport::stdio()).await`
- 日志全部输出到 stderr，绝不写 stdout
- 不实现 HTTP/SSE/WebSocket transport
- `transport` 模块保留扩展位（`pub mod mcp`），未来可加 `pub mod http` 等

Phase 3+ 若出现「远程 Agent 调用 TermBridge」需求，再评估 HTTP transport + 鉴权。

## Consequences

- ✅ 实现极简，`main.rs` 仅 30 行
- ✅ 无网络端口暴露，安全边界清晰（本机进程间通信）
- ✅ 与 Trae / Claude Desktop 等 MCP host 的子进程模型天然契合
- ⚠️ 不支持「远程 Agent 调用」——若 Agent 在另一台机器，需先 SSH 到 TermBridge 所在机器再拉子进程
- ⚠️ 多个 Agent 进程会各自拉起独立 TermBridge 实例，Session 不共享（Phase 3 daemon 形态再解决，见 ADR-0004 预留）
- ⚠️ 调试时需注意：stdout 被占用，`println!` 会破坏 MCP 协议，全用 `tracing::info!` → stderr
