'use strict';
// 薄主包启动器（平台包方案，长期主渠道）：
//   1. 平台包 @termbridge/<os>-<arch> 内是完整 release 目录（trio 同目录 +
//      resources/agentd + SKILL.md + 配置），安装时由 npm optionalDependencies 装好；
//   2. 本启动器仅需：解析本机平台包路径 → 启动其中二进制，零网络、零下载、零缓存；
//   3. 不设置 cwd：继承调用方工作目录，避免改变 path_policy 的本地路径语义；
//      Rust 侧通过 current_exe() 找同目录 helper / resources，与 cwd 无关。
const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const PLATFORMS = {
  'win32-x64': { pkg: 'termbridge-win32-x64', exe: true },
  'linux-x64': { pkg: 'termbridge-linux-x64', exe: false },
  'darwin-arm64': { pkg: 'termbridge-darwin-arm64', exe: false },
};
const BIN_NAMES = {
  termbridge: 'termbridge',
  'termbridge-mcp': 'termbridge-mcp',
  'termbridge-auth-helper': 'termbridge-auth-helper',
};

function main(name) {
  const key = `${process.platform}-${process.arch}`;
  const meta = PLATFORMS[key];
  try {
    if (!meta) {
      throw new Error(`不支持的平台：${process.platform}/${process.arch}（发布矩阵：win32-x64 / linux-x64 / darwin-arm64）`);
    }
    const base = BIN_NAMES[name];
    if (!base) throw new Error(`未知二进制：${name}`);
    const pkgDir = path.dirname(require.resolve(`${meta.pkg}/package.json`));
    const bin = path.join(pkgDir, meta.exe ? `${base}.exe` : base);
    if (!fs.existsSync(bin)) throw new Error(`二进制不存在：${bin}`);
    const r = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
    if (r.error) throw r.error;
    process.exitCode = r.status ?? 1;
  } catch (e) {
    console.error(
      `\nTermBridge 启动失败：${e.message}\n` +
        `（npx 场景若用了 --no-optional 会缺失平台包 ${meta ? meta.pkg : ''}；` +
        `安装方式：npx -y termbridge-mcp，更新：npx -y termbridge-mcp@latest）`,
    );
    process.exitCode = 1;
  }
}

module.exports = { main };