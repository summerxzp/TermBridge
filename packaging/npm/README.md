# termbridge-mcp（npm 安装器壳）

TermBridge 是原生 Rust 二进制，通过 GitHub Releases 发布。本 npm 包只是一个
**下载器壳**：自动下载当前平台的最新二进制到本地缓存并启动，让用户获得与
`npx -y chrome-devtools-mcp@latest` 一致的安装体验。

## 用法

MCP server（配置到 MCP 客户端）：

```json
{
  "mcpServers": {
    "termbridge": {
      "command": "npx",
      "args": ["-y", "termbridge-mcp@latest"]
    }
  }
}
```

也可直接运行任一命令：

```bash
npx -y termbridge-mcp           # MCP server（stdio）
npx -y termbridge hosts         # 人类管理员 CLI
npx -y termbridge-auth-helper   # 凭据辅助进程（一般由 termbridge-mcp 自动拉起）
```

## 工作方式

- 首次运行：下载 GitHub Releases 最新版对应平台资产（.zip / .tar.gz），
  校验随包发布的 `.sha256`，解压到
  `~/.cache/termbridge-npm/<version>/`，然后转发执行。
- 之后运行：直接启动缓存二进制（不联网）；后台每 24h 检查一次 GitHub，
  发现新版自动下载缓存，下次启动生效。
- 二进制更新由本壳负责；二进制内部的更新提示（`TERMBRIDGE_NO_UPDATE_CHECK`）
  作为未走 npx 场景的兜底。

本包**零运行时依赖**（纯 Node 内置模块 + 系统自带 `tar`），不引入供应链风险；
下载包经过 GitHub 随包发布的 `.sha256` 校验后才解压执行。

> 网络受限环境：若直连 GitHub 不稳定/被墙，可用
> `TERMBRIDGE_NPM_MIRROR=https://镜像域名` 环境变量覆盖下载与版本解析的源
> （最新版本号通过 `/releases/latest` 302 重定向解析，不依赖 GitHub API 配额）。

## 发布

打 `v*` tag 走 release.yml 构建 Releases 后，需同步发布 npm：

```bash
cd packaging/npm
npm version <与 Cargo 版本一致>
npm publish
```

## 平台

windows-x64（.zip）、linux-x64 / macos-arm64（.tar.gz），与 release.yml 发布矩阵一致。