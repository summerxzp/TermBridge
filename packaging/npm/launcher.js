'use strict';
// TermBridge npm 安装器壳（借鉴 chrome-devtools-mcp 的 npx 分发模式）：
//   1. 启动 → 有本地缓存则立即转发执行（不联网，启动快）
//   2. 后台异步检查 GitHub 最新 release（24h 限频），发现新版则下载到缓存，下次启动生效
//   3. 首次运行（无缓存）→ 阻塞下载对应平台最新二进制，校验 sha256 后解压启动
// 失败静默：任何下载/网络异常不影响已缓存版本的使用，下次运行重试。
//
// 与 Rust 侧 update_check 的分工：本壳负责"拿到并更新二进制"；二进制内部的
// update_check 负责"提示用户手动下载时用的包装版"（npx 场景下二进制更新由本壳完成）。

const fs = require('fs');
const os = require('os');
const path = require('path');
const https = require('https');
const crypto = require('crypto');
const { spawnSync } = require('child_process');

const REPO = 'summerxzp/TermBridge';
const API_LATEST = `https://api.github.com/repos/${REPO}/releases/latest`;
const CACHE_ROOT = path.join(os.homedir(), '.cache', 'termbridge-npm');
const STATE_FILE = path.join(CACHE_ROOT, 'state.json');
const LOCK_DIR = path.join(CACHE_ROOT, '.lock');
const THROTTLE_MS = 24 * 60 * 60 * 1000; // 24h，与 Rust 侧 update_check 一致
const MAX_DOWNLOAD_BYTES = 512 * 1024 * 1024; // 防呆上限（正常包远小于此）
const UA = 'termbridge-npm-launcher';

// GitHub 直连不稳定时的镜像兜底（仅覆盖下载与 /releases/latest 解析）
const MIRROR = (process.env.TERMBRIDGE_NPM_MIRROR || '').trim().replace(/\/+$/, '');
const WEB_BASE = MIRROR || 'https://github.com';

// 发布矩阵与 release.yml 保持一致：windows .zip，其他 .tar.gz
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
    const r = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
    if (r.error) throw r.error;
    process.exitCode = r.status ?? 1;
  } catch (e) {
    console.error(`\nTermBridge npm 壳启动失败：${e.message}\n`);
    process.exitCode = 1;
  }
}

/** 解析本机对应平台的二进制路径（快速路径不联网，首次运行会阻塞下载） */
async function ensureBinary(name) {
  const platform = PLATFORMS[`${process.platform}-${process.arch}`];
  if (!platform) {
    throw new Error(
      `不支持的平台：${process.platform}/${process.arch}（发布矩阵：windows-x64 / linux-x64 / macos-arm64）`,
    );
  }
  const rel = platform.bins[name];
  if (!rel) throw new Error(`未知二进制：${name}`);

  const state = readState();
  // 快速路径：已有可用缓存 → 立即启动，后台异步刷新
  if (state && state.version) {
    const bin = path.join(CACHE_ROOT, state.version, rel);
    if (fs.existsSync(bin)) {
      maybeBackgroundRefresh(state, platform); // fire-and-forget
      return bin;
    }
  }

  // 首次运行（无缓存）：阻塞下载最新 release
  console.error('TermBridge：首次运行，正在下载最新版二进制（完成后缓存于本地）…');
  const latest = await fetchLatestVersion();
  await downloadAndExtract(latest, platform);
  writeState({ version: latest, checkedAt: Date.now() });
  return path.join(CACHE_ROOT, latest, rel);
}

/** 后台检查新版本（24h 限频）：先占位刷新 checkedAt，下载新包写回缓存供下次启动 */
function maybeBackgroundRefresh(state, platform) {
  if (Date.now() - state.checkedAt < THROTTLE_MS) return;
  if (!writeState({ version: state.version, checkedAt: Date.now() })) return; // 占位限频
  setTimeout(async () => {
    try {
      const latest = await fetchLatestVersion();
      if (!latest || latest === state.version) return;
      await downloadAndExtract(latest, platform);
      writeState({ version: latest, checkedAt: Date.now() });
      console.error(`TermBridge 已自动缓存新版 v${latest}（下次启动生效）`);
    } catch {
      // 失败静默，checkedAt 已占位 → 24h 后重试
    }
  }, 0);
}

// ───────────────────────────────────────────────────────────────────────
// GitHub / 缓存 / 下载
// ───────────────────────────────────────────────────────────────────────

function readState() {
  try {
    return JSON.parse(fs.readFileSync(STATE_FILE, 'utf8'));
  } catch {
    return null;
  }
}

function writeState(state) {
  try {
    fs.mkdirSync(CACHE_ROOT, { recursive: true });
    fs.writeFileSync(STATE_FILE, JSON.stringify(state));
    return true;
  } catch {
    return false;
  }
}

/** 解析最新版本号：优先 GitHub API（免认证），失败/被限频则退化为
 *  `GET /releases/latest` 的 302 Location（免 API 配额，GitHub 网页重定向，
 *  镜像场景也走这条路）。返回去掉前导 v 的版本号 */
async function fetchLatestVersion() {
  if (!MIRROR) {
    try {
      const res = await httpsGet(API_LATEST, 1024 * 1024, {
        timeoutMs: 15_000,
        deadlineMs: 20_000,
      });
      const tag = JSON.parse(res.body.toString('utf8')).tag_name;
      if (typeof tag === 'string' && tag) return tag.replace(/^v/, '');
    } catch {
      // 静默降级到重定向法
    }
  }
  const res = await httpsGet(
    `${WEB_BASE}/${REPO}/releases/latest`,
    4096,
    { timeoutMs: 15_000, deadlineMs: 20_000, followRedirect: false },
  );
  const loc = res.headers.location || res.body.toString('utf8');
  const m = String(loc).match(/\/tag\/([^\/?#]+)/);
  if (!m) throw new Error('无法解析最新版本号');
  return m[1].replace(/^v/, '');
}

/** 下载 release 资产 → sha256 校验 → 解压到缓存目录（跨进程互斥，避免并发下载） */
async function downloadAndExtract(version, platform) {
  return withLock(async () => {
    const destDir = path.join(CACHE_ROOT, version);
    const probe = path.join(destDir, platform.bins.termbridge);
    if (fs.existsSync(probe)) return destDir; // 已就绪

    const base = `${WEB_BASE}/${REPO}/releases/download/v${version}/${platform.asset}`;
    console.error(`  - 下载 ${platform.asset} …`);
    const dl = await httpsGet(base, MAX_DOWNLOAD_BYTES, {
      timeoutMs: 60_000,
      deadlineMs: 300_000, // 5 分钟硬截止，网络半断时干净失败（不挂起）
    });
    const buf = dl.body;

    const expected = (await httpsGet(`${base}.sha256`, 4096, {
      timeoutMs: 15_000,
      deadlineMs: 20_000,
    }))
      .body.toString('utf8')
      .trim()
      .split(/\s+/)[0]
      .toLowerCase();
    const actual = sha256(buf);
    if (!expected || actual !== expected) {
      throw new Error(`sha256 校验失败：${platform.asset}`);
    }

    const tmp = path.join(CACHE_ROOT, `.tmp-${version}`);
    fs.mkdirSync(destDir, { recursive: true }); // 系统 tar 的 -C 要求目标目录已存在
    fs.writeFileSync(tmp, buf);
    try {
      extractArchive(tmp, destDir, platform.kind);
    } finally {
      fs.rmSync(tmp, { force: true });
    }

    // 校验解压产物 + POSIX 可执行位
    for (const rel of Object.values(platform.bins)) {
      const p = path.join(destDir, rel);
      if (!fs.existsSync(p)) throw new Error(`解压产物缺失：${rel}`);
      if (process.platform !== 'win32') fs.chmodSync(p, 0o755);
    }
    return destDir;
  });
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

/** 解压：统一走系统自带 tar（零运行时依赖）。Win10+/macOS 为 bsdtar（支持 zip），
 *  Linux 为 GNU tar（只处理 tar.gz）——zip 仅在 windows 出现，恰好匹配 */
function extractArchive(tmp, destDir, kind) {
  const args = kind === 'zip' ? ['-xf', tmp, '-C', destDir] : ['-xzf', tmp, '-C', destDir];
  const r = spawnSync('tar', args, { stdio: 'pipe' });
  if (r.status !== 0) {
    throw new Error(`解压失败（tar ${args.join(' ')}）: ${String(r.stderr).trim()}`);
  }
}

function sha256(buf) {
  return crypto.createHash('sha256').update(buf).digest('hex');
}

/**
 * 极简 https GET：超时兜底（不挂起）+ 可关闭重定向跟随，返回
 *   { status, headers, body }（followRedirect: false 时 body 为空）
 */
function httpsGet(url, maxBytes, { timeoutMs = 30_000, deadlineMs = timeoutMs * 4, followRedirect = true } = {}) {
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
        if (isRedirect && followRedirect) {
          res.resume();
          clearTimeout(deadline);
          const loc = res.headers.location;
          if (!loc || redirectsLeft <= 0) return reject(new Error('重定向异常'));
          return doGet(new URL(loc, target).toString(), redirectsLeft - 1);
        }
        if (!followRedirect && isRedirect) {
          clearTimeout(deadline);
          res.resume();
          return resolve({ status: res.statusCode, headers: res.headers, body: Buffer.alloc(0) });
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

module.exports = { main, extractArchive, httpsGet, sha256 };