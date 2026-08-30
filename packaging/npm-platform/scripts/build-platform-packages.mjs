// build-platform-packages.mjs
//
// 从 release staging 目录生成 3 个平台包（@summerxzp/termbridge-<os>-<arch>）并同步主包版本。
// 每个平台包内是「完整 release 目录」（trio 二进制 + resources/agentd + SKILL.md + 配置），
// 保证 exe 同目录布局与 Rust 侧 current_exe()/相对路径语义，与 esbuild 式"单二进制拆包"不同。
// 命名：主包 @summerxzp/termbridge-mcp + 平台包 @summerxzp/termbridge-<os>-<arch>
// （esbuild 同款模式；scope 绑定发布者身份，规避无 scope 新包名的 spam 风控）。
//
// 用法：
//   node scripts/build-platform-packages.mjs \
//     --version 0.3.0 \
//     --os-arch win32-x64 --staging ./staging/termbridge-windows-x86_64 \
//     [--os-arch linux-x64 --staging ./staging/termbridge-linux-x86_64] \
//     [--os-arch darwin-arm64 --staging ./staging/termbridge-macos-arm64] \
//     [--main ../package.json] [--out ./generated]
//
// 生成产物：./generated/termbridge-<os-arch>/（完整目录 + package.json）
// 并更新主包 package.json 的 optionalDependencies 与版本。

import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const DEFAULT_MAIN = path.resolve(HERE, '..', 'package.json');
const DEFAULT_OUT = path.resolve(HERE, '..', 'generated');

// npm scope（发布者身份；平台包与主包共用）
const SCOPE = '@summerxzp';

const PLATFORM_META = {
  'win32-x64': { os: ['win32'], cpu: ['x64'] },
  'linux-x64': { os: ['linux'], cpu: ['x64'] },
  'darwin-arm64': { os: ['darwin'], cpu: ['arm64'] },
};

function parseArgs(argv) {
  const out = { platforms: [], version: undefined, main: DEFAULT_MAIN, outDir: DEFAULT_OUT };
  let pendingKey = null;
  for (let i = 2; i < argv.length; i += 2) {
    const k = argv[i];
    const v = argv[i + 1];
    if (k === '--version') out.version = v;
    else if (k === '--main') out.main = path.resolve(v);
    else if (k === '--out') out.outDir = path.resolve(v);
    else if (k === '--os-arch') pendingKey = v;
    else if (k === '--staging') {
      if (!pendingKey) {
        console.error('--staging 前必须指定 --os-arch <key>');
        process.exit(2);
      }
      out.platforms.push({ key: pendingKey, staging: path.resolve(v) });
      pendingKey = null;
    }
  }
  return out;
}

const args = parseArgs(process.argv);
if (!args.version || args.platforms.length === 0) {
  console.error('用法：--version X.Y.Z --os-arch <key> --staging <dir> [更多平台] [--main] [--out]');
  process.exit(2);
}

// 1) 生成平台包
for (const { key, staging } of args.platforms) {
  const meta = PLATFORM_META[key];
  if (!meta) {
    console.error(`未知平台 key：${key}（支持 ${Object.keys(PLATFORM_META).join(' / ')}）`);
    process.exit(2);
  }
  if (!existsSync(staging)) {
    console.error(`staging 目录不存在：${staging}`);
    process.exit(2);
  }
  const destDir = path.join(args.outDir, `termbridge-${key}`);
  rmSync(destDir, { recursive: true, force: true });
  // 复制完整 release 目录内容
  cpSync(staging, destDir, { recursive: true });

  // bin 字段（P0，esbuild 同款做法）：npm pack 会把「未列入 bin 的文件」mode 归一化为
  // 0644，只有 bin 条目保留/强制 0755。缺 bin 会导致 Linux/macOS 平台包发布出去的二进制
  // 无执行位，launcher spawnSync 直接 EACCES（Windows 不受影响，故 CI 上难察觉）。
  // bin key 用无扩展名命令名（npm 据此建 node_modules/.bin shim），value 按包内实际
  // 存在的文件：Windows staging 是 .exe 后缀，Unix staging 无扩展名。
  const BIN_CMDS = ['termbridge', 'termbridge-mcp', 'termbridge-auth-helper']; // 与 launcher.js BIN_NAMES 对齐
  const binEntries = {};
  for (const cmd of BIN_CMDS) {
    if (existsSync(path.join(destDir, cmd))) {
      binEntries[cmd] = `./${cmd}`;
    } else if (existsSync(path.join(destDir, `${cmd}.exe`))) {
      binEntries[cmd] = `./${cmd}.exe`;
    }
  }
  if (Object.keys(binEntries).length === 0) {
    console.error(`staging 目录中未找到任何入口二进制（${BIN_CMDS.join(' / ')}）：${staging}`);
    process.exit(2);
  }

  const pkg = {
    name: `${SCOPE}/termbridge-${key}`,
    version: args.version,
    description: `TermBridge runtime for ${key} (完整 release 目录，含 trio 二进制 / resources/agentd / SKILL.md)`,
    license: 'Apache-2.0',
    os: meta.os,
    cpu: meta.cpu,
    bin: binEntries,
    files: ['*'],
    // scope 包默认 restricted，必须显式 public（OIDC/CI 发布同样生效）
    publishConfig: { access: 'public' },
  };
  writeFileSync(path.join(destDir, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
  console.log(`✓ 平台包 ${pkg.name}@${args.version}  →  ${destDir}`);
}

// 2) 同步主包版本与 optionalDependencies
const mainPkgPath = args.main;
const mainPkg = JSON.parse(readFileSync(mainPkgPath, 'utf8'));
mainPkg.version = args.version;
for (const key of Object.keys(PLATFORM_META)) {
  mainPkg.optionalDependencies[`${SCOPE}/termbridge-${key}`] = args.version;
}
writeFileSync(mainPkgPath, JSON.stringify(mainPkg, null, 2) + '\n');
console.log(`✓ 主包 ${mainPkg.name}@${args.version}  版本与 optionalDependencies 已同步`);
mkdirSync(args.outDir, { recursive: true });