# termbridge-mcp（平台包方案，长期主渠道）

Rust 二进制通过 npm **平台包**分发（借鉴 esbuild / Biome / SWC 的主包 + 平台包模式）：

- 主包 `termbridge-mcp`：薄启动器，仅解析本机平台包并启动二进制（零网络/零下载）。
- 平台包 `@termbridge/win32-x64|linux-x64|darwin-arm64`：每个包内含**完整 release
  目录**（trio 二进制同目录 + `resources/agentd` + `SKILL.md` + 配置），
  保证 `current_exe()`（helper 同目录）与 agentd/LOCALAPPDATA 布局语义不受影响。
- 安装/更新全走 npm registry（可配 npmmirror/内网源）；`npx -y termbridge-mcp@latest`
  每次启动解析最新版。

## 使用

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

## 发版（CI 中执行，勿手动提交 package.json 版本）

```bash
# 1. 从 release stgging 目录生成 3 个平台包 + 同步主包版本
node scripts/build-platform-packages.mjs \
  --version 0.3.0 \
  --os-arch win32-x64 --staging <dir>/termbridge-windows-x86_64 \
  --os-arch linux-x64 --staging <dir>/termbridge-linux-x86_64 \
  --os-arch darwin-arm64 --staging <dir>/termbridge-macos-arm64

# 2. 逐个 npm publish（平台包在前，主包最后）
npm publish generated/@termbridge/win32-x64 --access public
npm publish generated/@termbridge/linux-x64 --access public
npm publish generated/@termbridge/darwin-arm64 --access public
npm publish .
```

建议接入 release.yml（见仓库 workflow），并配置 npmjs Trusted Publishing（OIDC）替代长期 token。

## 与 packaging/npm（过渡壳）的关系

`packaging/npm` 是过渡期「npm 壳 + GitHub Releases 下载」方案（GitHub 通畅环境的
备选）；本目录是主渠道。两者发同一批二进制，不冲突。