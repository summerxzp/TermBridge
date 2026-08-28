// build-platform-packages.mjs
//
// 从 release staging 目录生成 3 个平台包（termbridge-<os>-<arch>）并同步主包版本。
// 每个平台包内是「完整 release 目录」（trio 二进制 + resources/agentd + SKILL.md + 配置），
// 保证 exe 同目录布局与 Rust 侧 current_exe()/相对路径语义，与 esbuild 式"单二进制拆包"不同。
// 命名：无 scope（termbridge-mcp / termbridge-win32-x64 …），与 chrome-devtools-mcp 习惯一致，
//       也避免 @termbridge org 抢注的不确定性；成熟后可再迁入组织 scope。
//
// 用法：
//   node scripts/build-platform-packages.mjs \
//     --version 0.2.1 \
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

  const pkg = {
    name: `termbridge-${key}`,
    version: args.version,
    description: `TermBridge runtime for ${key} (完整 release 目录，含 trio 二进制 / resources/agentd / SKILL.md)`,
    license: 'Apache-2.0',
    os: meta.os,
    cpu: meta.cpu,
    files: ['*'],
  };
  writeFileSync(path.join(destDir, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
  console.log(`✓ 平台包 ${pkg.name}@${args.version}  →  ${destDir}`);
}

// 2) 同步主包版本与 optionalDependencies
const mainPkgPath = args.main;
const mainPkg = JSON.parse(readFileSync(mainPkgPath, 'utf8'));
mainPkg.version = args.version;
for (const key of Object.keys(PLATFORM_META)) {
  mainPkg.optionalDependencies[`termbridge-${key}`] = args.version;
}
writeFileSync(mainPkgPath, JSON.stringify(mainPkg, null, 2) + '\n');
console.log(`✓ 主包 ${mainPkg.name}@${args.version}  版本与 optionalDependencies 已同步`);
mkdirSync(args.outDir, { recursive: true });