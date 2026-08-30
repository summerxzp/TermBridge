# termbridge-mcp-downloader（npm 下载器壳 · 过渡/遗留渠道）

> **这不是主渠道。** 长期主渠道是 [`../npm-platform`](../npm-platform) 平台包方案，
> MCP 配置直接使用：`"command": "npx", "args": ["-y", "@summerxzp/termbridge-mcp@latest"]`。

本包（npm 包名 `termbridge-mcp-downloader`，**未发布到 npm**，仅保留本地/手工测试用）是一个
**下载器壳**：运行时按 npm 包版本从 GitHub Releases 下载对应平台二进制并转发执行，获得与
`npx -y <downloader>` 类似的体验。

## 它做什么

- **版本严格绑定**：npm 包版本 = GitHub Release tag（`termbridge-mcp-downloader@0.2.1`
  精确下载并运行 v0.2.1），符合 npm 锁文件语义。二进制不随 npm 注册表分发。
- **仓库硬编码**：资产固定下载自 `summerxzp/TermBridge` 的 Releases（`launcher.js` 内
  `REPO` 常量），不读取环境变量切换仓库；可用 `TERMBRIDGE_NPM_MIRROR` 覆盖资产下载源
  （网络受限环境兜底，版本始终取 npm 包版本）。
- 首次运行：下载对应平台资产（.zip / .tar.gz），校验随包发布的 `.sha256`，解压到
  `~/.cache/termbridge-npm/<version>/`，然后转发执行；之后运行直接启动缓存二进制（不联网）。
- 本包**零运行时依赖**（纯 Node 内置模块 + 系统自带 `tar`）。

## 入口命令（经 bin 包装脚本转发）

```bash
termbridge-mcp           # MCP server（stdio）
termbridge hosts         # 人类管理员 CLI
termbridge-auth-helper   # 凭据辅助进程（一般由 termbridge-mcp 自动拉起）
```

## 发布与维护（重要）

- 本包**不由 CI 发布**：release.yml 的 `npm-packages` job 只发布 `packaging/npm-platform`
  下的主包 + 平台包（`@summerxzp/termbridge-mcp` 与 `@summerxzp/termbridge-<os>-<arch>`）。
- 本包为**过渡/遗留方案**，历史上曾计划作为 npx 入口，现已被平台包方案取代；
  如需手工发布，须自行处理版本与 `npm publish`，不属于标准发版流程。

## 平台

windows-x64（.zip）、linux-x64 / macos-arm64（.tar.gz），与 release.yml 发布矩阵一致。
