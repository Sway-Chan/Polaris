#!/usr/bin/env node
/**
 * fetch-dashboard.mjs — 拉取 sing-box 官方面板（SagerNet/sing-box-dashboard 的 gh-pages 构建产物）到
 * resources/dashboard/，供 Tauri bundle resources 随安装包打包。
 *
 * 用法：node scripts/fetch-dashboard.mjs [--force]
 *
 * 为什么随包内置：面板原走 sing-box services[].dashboard.enabled 首次联网下载，慢启动 + 离线不可用。改随包后
 * 核以 dashboard:{enabled,path:<bundledDir>} 直接 serve 本地文件（零联网）。运行时「刷新面板资源」下载新版
 * 覆盖至 <userData>/dashboard（serve 优先用覆盖版、否则回落内置）。
 *
 * 机制：下载 gh-pages 分支 zipball → 解压 → 把含 index.html 的目录平铺到 resources/dashboard/。
 * 原子落地：先解到临时目录、校验 index.html 存在再 rename 替换。dereference:true 防符号链接打进包。
 *
 * 直移自 上游 scripts/fetch-dashboard.mjs（语言无关）。
 */
import { execFileSync } from 'child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, renameSync } from 'fs';
import { tmpdir } from 'os';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';

import { extractZip } from './lib/extract-zip.mjs';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const DEST = join(ROOT, 'resources', 'dashboard');
const FORCE = process.argv.includes('--force');

const ZIP_URL = 'https://github.com/SagerNet/sing-box-dashboard/archive/refs/heads/gh-pages.zip';

// ⚠️ **本脚本刻意不接 `lib/fetch-stamp.mjs` 的版本判据**（fetch-core / fetch-cronet 都接了）。
// 那套判据的指纹是「版本号 + 钉扎 sha」，而 dashboard 这两样**一个都没有**：源是 gh-pages
// 分支的活动 zip（上面这个 ZIP_URL 恒定不变、内容随上游滚动），既无版本号也无 sha pin。
// 拿恒定的 URL 当指纹只会让判据**恒为 fresh**，即制造一个看起来有版本控制、实则永不失效的门。
// 宁可如实保留「文件在就跳过」，把这个缺口摆在明面上：要更新 dashboard 一律 `--force`。
// （补 sha pin 是另一件事，见对拍报告里「fetch-dashboard 无 sha256 钉扎」那条，未做。）
if (existsSync(join(DEST, 'index.html')) && !FORCE) {
  console.log(`skip (exists): resources/dashboard/index.html —— 无版本判据，更新须 --force`);
  process.exit(0);
}

const work = mkdtempSync(join(tmpdir(), 'polaris-dashboard-'));
const zipPath = join(work, 'dashboard.zip');
const extractDir = join(work, 'extracted');

try {
  console.log(`downloading sing-box-dashboard (gh-pages) → ${zipPath} ...`);
  execFileSync('curl', ['-fL', '--retry', '3', '-o', zipPath, ZIP_URL], { stdio: 'inherit' });

  mkdirSync(extractDir, { recursive: true });
  console.log('extracting ...');
  extractZip(zipPath, extractDir);

  const top = readdirSync(extractDir).map((n) => join(extractDir, n));
  let uiRoot = top.find((p) => existsSync(join(p, 'index.html')));
  if (!uiRoot && existsSync(join(extractDir, 'index.html'))) uiRoot = extractDir;
  if (!uiRoot) {
    throw new Error('解压产物中未找到 index.html（gh-pages 结构可能变化）');
  }

  // 原子替换：先拷到 DEST.tmp 再 rename 顶替。
  const tmpDest = `${DEST}.tmp`;
  rmSync(tmpDest, { recursive: true, force: true });
  mkdirSync(dirname(DEST), { recursive: true });
  cpSync(uiRoot, tmpDest, { recursive: true, dereference: true });
  rmSync(DEST, { recursive: true, force: true });
  renameSync(tmpDest, DEST);

  console.log(`ok: resources/dashboard/ ready (index.html present).`);
} catch (e) {
  console.error(`FAILED: ${e.message}`);
  process.exitCode = 1;
} finally {
  rmSync(work, { recursive: true, force: true });
}
