'use strict';
// TermBridge 过渡期 npm 壳（长期方向见 packaging/npm-platform/ 平台包方案）：
//   1. npm 包版本 = GitHub Release 版本（v<PKG_VERSION>，严格绑定）——
//      `npx termbridge-mcp@0.2.1` 就精确下载并运行 v0.2.1，符合 npm 锁语义
//   2. 首次运行：下载对应平台资产 → sha256 校验 → 解压缓存 → 启动完整目录中的
//      二进制（trio 同目录、agentd 相对路径均保持 release 原布局）
//   3. 后续运行：缓存命中即启动，零联网；无后台自动升级（版本由 npm 决定）
//   4. 网络受限：TERMBRIDGE_NPM_MIRROR 覆盖下载源
// 失败静默：下载/网络异常不阻塞，给出明确错误；不破坏已缓存版本。

const fs = require('fs');
const os = require('os');
const path = require('path');
const https = require('https');
const crypto = require('crypto');
const { spawnSync } = require('child_process');

// npm 包版本与 GitHub Release tag 严格一致（v<PKG_VERSION>）
const PKG_VERSION = require('./package.json').version;
const REPO = 'summerxzp/TermBridge';
const CACHE_ROOT = path.join(os.homedir(), '.cache', 'termbridge-npm'); // <CACHE_ROOT>/<version>/
const LOCK_DIR = path.join(CACHE_ROOT, '.lock');
const MAX_DOWNLOAD_BYTES = 512 * 1024 * 1024; // 防呆上限（正常包远小于此）
const UA = 'termbridge-npm-launcher';

// GitHub 直连不稳定时的下载源兜底（仅资产下载）
const MIRROR = (process.env.TERMBRIDGE_NPM_MIRROR || '').trim().replace(/\/+$/, '');
const WEB_BASE = MIRROR || 'https://github.com';

// 发布矩阵与 release.yml 一致。Unix 归档 0.2.x 曾带顶层目录（assetBase/），
// 新版本已改扁平；wrapper 在启动时对两种布局都做探测（findBinary）。
const PLATFORMS = {
  'win32-x64': {
    asset: 'termbridge-windows-x86_64.zip',
    kind: 'zip',
    bins: {
      termbridge: 'termbridge.exe',
      'termbridge-mcp': 'termbridge-mcp.exe',
      'termbridge-auth-helper': 'termbridge-auth-helper.exe',
    },
  },
  'linux-x64': {
    asset: 'termbridge-linux-x86_64.tar.gz',
    kind: 'tar.gz',
    bins: {
      termbridge: 'termbridge',
      'termbridge-mcp': 'termbridge-mcp',
      'termbridge-auth-helper': 'termbridge-auth-helper',
    },
  },
  'darwin-arm64': {
    asset: 'termbridge-macos-arm64.tar.gz',
    kind: 'tar.gz',
    bins: {
      termbridge: 'termbridge',
      'termbridge-mcp': 'termbridge-mcp',
      'termbridge-auth-helper': 'termbridge-auth-helper',
    },
  },
};

// ───────────────────────────────────────────────────────────────────────
// 入口：main('termbridge') / main('termbridge-mcp') / ...
// ───────────────────────────────────────────────────────────────────────

async function main(name) {
  try {
    const bin = await ensureBinary(name);
    // 不设置 cwd：继承调用方工作目录，避免改变 path_policy 的本地路径语义
    const r = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
    if (r.error) throw r.error;
    process.exitCode = r.status ?? 1;
  } catch (e) {
    console.error(`\nTermBridge npm 壳启动失败：${e.message}\n`);
    process.exitCode = 1;
  }
}

/** 解析本机平台二进制路径（缓存命中零联网；首次运行阻塞下载 v<PKG_VERSION>） */
async function ensureBinary(name) {
  const platform = PLATFORMS[`${process.platform}-${process.arch}`];
  if (!platform) {
    throw new Error(
      `不支持的平台：${process.platform}/${process.arch}（发布矩阵：windows-x64 / linux-x64 / macos-arm64）`,
    );
  }
  const rel = platform.bins[name];
  if (!rel) throw new Error(`未知二进制：${name}`);

  const installDir = path.join(CACHE_ROOT, PKG_VERSION);
  const cached = findBinary(installDir, rel);
  if (cached) return cached; // 快速路径：已缓存（含已下载的完整版本目录）

  console.error(
    `TermBridge：首次运行，正在下载 v${PKG_VERSION} 二进制（完成后缓存于本地）…`,
  );
  await downloadAndExtract(PKG_VERSION, platform);
  const bin = findBinary(installDir, rel);
  if (!bin) throw new Error(`解压完成但找不到 ${rel}（归档布局异常）`);
  return bin;
}

// ───────────────────────────────────────────────────────────────────────
// 下载 / 校验 / 解压
// ───────────────────────────────────────────────────────────────────────

/** 下载 release 资产 → sha256 校验 → 解压（跨进程互斥） */
async function downloadAndExtract(version, platform) {
  return withLock(async () => {
    const installDir = path.join(CACHE_ROOT, version);
    if (findBinary(installDir, platform.bins.termbridge)) return; // 已就绪

    const base = `${WEB_BASE}/${REPO}/releases/download/v${version}/${platform.asset}`;
    const dl = await httpsGet(base, MAX_DOWNLOAD_BYTES, {
      timeoutMs: 60_000,
      deadlineMs: 300_000, // 5 分钟硬截止，网络半断时干净失败（不挂起）
    }).catch((e) => {
      throw new Error(`下载 ${platform.asset} 失败（${e.message}）。` +
        `确认 v${version} 已在 GitHub Releases 发布；或设置 TERMBRIDGE_NPM_MIRROR 指定镜像源`);
    });
    const buf = dl.body;

    const expected = (
      await httpsGet(`${base}.sha256`, 4096, { timeoutMs: 15_000, deadlineMs: 20_000 })
    ).body
      .toString('utf8')
      .trim()
      .split(/\s+/)[0]
      .toLowerCase();
    const actual = sha256(buf);
    if (!expected || actual !== expected) {
      throw new Error(`sha256 校验失败：${platform.asset}`);
    }

    const tmp = path.join(CACHE_ROOT, `.tmp-${version}`);
    fs.mkdirSync(installDir, { recursive: true }); // 系统 tar 的 -C 要求目标目录已存在
    fs.writeFileSync(tmp, buf);
    try {
      extractArchive(tmp, installDir, platform.kind);
    } finally {
      fs.rmSync(tmp, { force: true });
    }

    // 校验关键产物存在（根目录或带顶层目录的旧归档），POSIX 补执行位
    for (const rel of Object.values(platform.bins)) {
      const p = findBinary(installDir, rel);
      if (!p) throw new Error(`解压产物缺失：${rel}`);
      if (process.platform !== 'win32') fs.chmodSync(p, 0o755);
    }
  });
}

/** 兼容两种归档布局：`<installDir>/<rel>` 或 `<installDir>/<assetBase>/<rel>` */
function findBinary(installDir, rel) {
  const root = path.join(installDir, rel);
  if (fs.existsSync(root)) return root;
  for (const sub of fs.existsSync(installDir) ? fs.readdirSync(installDir) : []) {
    const candidate = path.join(installDir, sub, rel);
    if (path.isAbsolute(sub) === false && fs.existsSync(candidate) && fs.statSync(candidate).isFile()) {
      return candidate;
    }
  }
  return null;
}

/** 解压：统一走系统自带 tar（零运行时依赖）。Win10+/macOS 为 bsdtar（支持 zip），
 *  Linux 为 GNU tar（只处理 tar.gz）——zip 仅在 windows 出现，恰好匹配 */
function extractArchive(tmp, destDir, kind) {
  const args = kind === 'zip' ? ['-xf', tmp, '-C', destDir] : ['-xzf', tmp, '-C', destDir];
  const r = spawnSync('tar', args, { stdio: 'pipe' });
  if (r.status !== 0) {
    throw new Error(`解压失败（tar ${args.join(' ')}）: ${String(r.stderr).trim()}`);
  }
}

async function withLock(fn) {
  fs.mkdirSync(CACHE_ROOT, { recursive: true }); // 保证父目录存在，锁冲突仅由 EEXIST 表达
  const deadline = Date.now() + 30_000;
  for (;;) {
    try {
      fs.mkdirSync(LOCK_DIR);
      break;
    } catch (e) {
      if (e.code !== 'EEXIST') throw e;
      if (Date.now() > deadline) throw new Error('等待其他下载任务超时');
      await new Promise((r) => setTimeout(r, 200));
    }
  }
  try {
    return await fn();
  } finally {
    fs.rmSync(LOCK_DIR, { recursive: true, force: true });
  }
}

function sha256(buf) {
  return crypto.createHash('sha256').update(buf).digest('hex');
}

/**
 * 极简 https GET：超时兜底（不挂起）+ 重定向跟随，返回 { status, headers, body }
 */
function httpsGet(url, maxBytes, { timeoutMs = 30_000, deadlineMs = timeoutMs * 4 } = {}) {
  return new Promise((resolve, reject) => {
    const doGet = (target, redirectsLeft) => {
      const req = https.get(target, { headers: { 'User-Agent': UA, Accept: 'application/vnd.github+json' } });
      const deadline = setTimeout(() => req.destroy(new Error('请求超时')), deadlineMs);
      req.setTimeout(timeoutMs, () => req.destroy(new Error('连接空闲超时')));
      req.on('error', (e) => {
        clearTimeout(deadline);
        reject(e);
      });
      req.on('response', (res) => {
        const isRedirect = [301, 302, 303, 307, 308].includes(res.statusCode);
        if (isRedirect) {
          res.resume();
          clearTimeout(deadline);
          const loc = res.headers.location;
          if (!loc || redirectsLeft <= 0) return reject(new Error('重定向异常'));
          return doGet(new URL(loc, target).toString(), redirectsLeft - 1);
        }
        if (res.statusCode !== 200) {
          clearTimeout(deadline);
          res.resume();
          return reject(new Error(`HTTP ${res.statusCode}（${target}）`));
        }
        const chunks = [];
        let size = 0;
        res.on('data', (c) => {
          size += c.length;
          if (size > maxBytes) {
            clearTimeout(deadline);
            req.destroy(new Error('下载超过大小上限'));
          } else {
            chunks.push(c);
          }
        });
        res.on('end', () => {
          clearTimeout(deadline);
          resolve({ status: 200, headers: res.headers, body: Buffer.concat(chunks) });
        });
        res.on('error', (e) => {
          clearTimeout(deadline);
          reject(e);
        });
      });
    };
    doGet(url, 10);
  });
}

module.exports = { main, findBinary, extractArchive, sha256 };