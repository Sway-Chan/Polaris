#!/usr/bin/env node
/**
 * fetch-core.mjs — 按 core-manifest.json 的 bundledCoreVersion 从 SagerNet/sing-box 官方 release
 * 下载各平台 sing-box 二进制到 resources/{平台}/，供 Tauri bundle externalBin/resources 随安装包打包
 * （与 libcronet/dashboard 同「现拉现打、不入库」模式）。
 *
 * 用法：node scripts/fetch-core.mjs [--force] [--platform=<key>[,<key>…]]
 *
 * `--platform` 只筛「本次要下哪几个平台」，**不改变任何完整性语义**：被选中的仍逐字节校验 sha
 * pin，未选中的**原样保留**盘上已有文件（不删、不动）。key 取值见下面 TARGETS 的 `key` 列：
 * `linux` / `win` / `mac-x64` / `mac-arm64`。
 *
 * 为什么需要它：四平台合计约 309 MB，而单次任务通常只用到其中一两个 —— 本机开发要 `linux`
 * （拿它跑 `sing-box check` 静态校验生成的配置），装 Mac 真机要 `mac-arm64`，`win` / `mac-x64`
 * 在这两种场景里零消费。且部署的 rsync **不排除** `resources/`，白下的那几份会连带白传一遍。
 *
 * ⚠️ **默认仍是全平台**，不要改成「按当前 runner 过滤」：CI 要四平台齐全（Linux 上交叉构建
 * Windows/macOS 安装包），而 `--platform` 是显式的按次选择，漏传只会多下、不会少下 ——
 * 这个方向的失败是浪费带宽，反过来（默认只下当前平台）的失败是打出缺核的包，运行期才炸。
 *
 * 完整性 pin：core-manifest.json 的 coreArchiveSha256 是「下载的压缩包(tar.gz/zip)本体」的 sha256，其值
 * == 官方 release REST API 返回的 asset digest（gh api repos/SagerNet/sing-box/releases/tags/v<版本>
 * --jq '.assets[]|{name,digest}'，一行可取，换核直接抄、不必下载解压计算）。下载后对压缩包逐字节校验，不符
 * 即失败（fail-fast：损坏 / 截断 / 投毒在解压前就拦）。压缩包正确则解出的二进制必正确（tar/zip 解压确定性），
 * 故不必再对解压后二进制单独算 sha。
 *
 * 直移自 上游 scripts/fetch-core.mjs（语言无关，仅改路径常量 上游→polaris、manifest 路径 src/shared→src-tauri）。
 * 跨平台一致：默认全 4 平台核一律下载（不按当前 runner 过滤）——支持 Linux 上交叉构建。
 * skip-exists 已落地则跳过。
 */
import { execFileSync } from 'child_process';
import { createHash } from 'crypto';
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
} from 'fs';
import { tmpdir } from 'os';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';

import { extractZip, findInZipRoot } from './lib/extract-zip.mjs';
import { isFresh, readStamps, recordStamp } from './lib/fetch-stamp.mjs';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

// 版本耦合的唯一真源：Rust 主进程（src-tauri）共享同一份 manifest，升级核心只需改 core-manifest.json。
const manifest = JSON.parse(readFileSync(join(ROOT, 'src-tauri/core-manifest.json'), 'utf-8'));
const VERSION = manifest.bundledCoreVersion; // 资产名不带 v；release tag 带 v（如 v1.14.0-alpha.44）
const SHA = manifest.coreArchiveSha256 || {}; // 压缩包 sha == 官方 release API 的 asset digest
const REPO = 'SagerNet/sing-box';
const FORCE = process.argv.includes('--force');

// resources 目标目录 ← 官方资产名(压缩包) → 落地二进制名 → coreArchiveSha256 key。
const TARGETS = [
  { dir: 'resources/linux', asset: `sing-box-${VERSION}-linux-amd64.tar.gz`, bin: 'sing-box', key: 'linux' },
  { dir: 'resources/win', asset: `sing-box-${VERSION}-windows-amd64.zip`, bin: 'sing-box.exe', key: 'win' },
  { dir: 'resources/mac-x64', asset: `sing-box-${VERSION}-darwin-amd64.tar.gz`, bin: 'sing-box', key: 'mac-x64' },
  { dir: 'resources/mac-arm64', asset: `sing-box-${VERSION}-darwin-arm64.tar.gz`, bin: 'sing-box', key: 'mac-arm64' },
];

const sha256 = (file) => createHash('sha256').update(readFileSync(file)).digest('hex');

/**
 * `--platform=a,b` → 只处理这些 key。缺省（未传该 flag）= 全平台。
 *
 * 未知 key 一律 **fail-fast**，不静默忽略：`--platform=mac`（少了 `-arm64`）静默忽略的后果是
 * 「命令成功、一个核都没下、打包时才发现缺核」，而拼错平台名恰恰是最容易犯的错。
 * 空值（`--platform=`）同样拒绝 —— 它更可能是脚本拼接漏了变量，而不是「我想要零个平台」。
 */
function selectedKeys() {
  const arg = process.argv.find((a) => a.startsWith('--platform='));
  if (!arg) return null; // 全平台
  const all = TARGETS.map((t) => t.key);
  const want = arg.slice('--platform='.length).split(',').map((s) => s.trim()).filter(Boolean);
  if (want.length === 0) {
    console.error(`FAILED: --platform 值为空。可选：${all.join(' / ')}（不传该 flag = 全平台）`);
    process.exit(1);
  }
  const bad = want.filter((k) => !all.includes(k));
  if (bad.length > 0) {
    console.error(`FAILED: 未知平台 ${bad.join(', ')}。可选：${all.join(' / ')}`);
    process.exit(1);
  }
  return want;
}

const ONLY = selectedKeys();
const PICKED = ONLY ? TARGETS.filter((t) => ONLY.includes(t.key)) : TARGETS;
if (ONLY) {
  const skipped = TARGETS.filter((t) => !ONLY.includes(t.key)).map((t) => t.key);
  // 显式播报跳过了什么：否则「4 ready」变「2 ready」会被读成两个失败。
  console.log(`--platform=${ONLY.join(',')} ⇒ 本次只处理 ${ONLY.length} 个平台，跳过（盘上文件原样保留）：${skipped.join(', ') || '无'}`);
}

let ok = 0;
let failed = 0;
const stamps = readStamps(ROOT);
for (const t of PICKED) {
  const absDir = join(ROOT, t.dir);
  const dest = join(absDir, t.bin);

  // 完整性 pin 是供应链防护核心：缺 pin 直接 fail（绝不无校验拉可执行核）。
  // **位置在 skip 判据之前**：指纹要拿 pin 参与构成，且缺 pin 时无论盘上有什么都该红。
  const want = (SHA[t.key] || '').replace(/^sha256:/, '');
  if (!want) {
    console.error(
      `  FAILED ${t.key}: core-manifest.json 缺 coreArchiveSha256[${t.key}] pin → 拒绝无完整性校验拉取（换版本须同步补；值=官方 release API 的 asset digest）`
    );
    failed++;
    continue;
  }

  // skip 判据 = 「产物在 **且** 它就是当前钉扎版本解出来的那一份」，不是「文件在不在」。
  // 指纹取 `版本|压缩包 sha` 两项：改版本号或改 pin 任一处，盘上旧产物立刻失效。
  const fingerprint = `${VERSION}|${want}`;
  if (!FORCE && isFresh(stamps, `core:${t.key}`, fingerprint, existsSync(dest))) {
    console.log(`skip (up to date): ${t.dir}/${t.bin} @ ${VERSION}`);
    ok++;
    continue;
  }
  if (!FORCE && existsSync(dest)) {
    // 陈旧产物**明说**，不混在 skip 里。此前这里印的是 `skip (exists)` + 一句「若刚改版本须加
    // --force」的提示 —— 跑在成功路径上的提示不是门，人不读就静默带旧核出包。
    console.log(`stale: ${t.dir}/${t.bin} 不是 ${VERSION} 的产物，重新拉取`);
  }

  mkdirSync(absDir, { recursive: true });
  const url = `https://github.com/${REPO}/releases/download/v${VERSION}/${t.asset}`;
  const work = mkdtempSync(join(tmpdir(), 'polaris-core-'));
  try {
    const archive = join(work, t.asset);
    console.log(`downloading ${t.asset} ...`);
    // -fL：失败返回非零（不把 404 页面当成功）+ 跟随重定向；--retry 抗瞬时抖动。
    execFileSync('curl', ['-fL', '--retry', '3', '-o', archive, url], { stdio: 'inherit' });

    const got = sha256(archive);
    if (got !== want) {
      throw new Error(`压缩包 sha256 不符：期望 ${want}，实得 ${got}（版本漂移 / 投毒 / 截断）`);
    }

    const extractDir = join(work, 'x');
    mkdirSync(extractDir, { recursive: true });
    if (t.asset.endsWith('.zip')) {
      // zip 走 `extractZip`（Windows 上没有 unzip，见该模块头注）。tar.gz 三平台都能用系统 tar。
      extractZip(archive, extractDir);
    } else {
      execFileSync('tar', ['xzf', archive, '-C', extractDir], { stdio: 'inherit' });
    }

    const binPath = findInZipRoot(extractDir, t.bin);
    if (!binPath) throw new Error(`解压产物未找到 ${t.bin}（官方资产结构可能变化）`);

    // 原子落地：拷到 .tmp → chmod（unix 可执行）→ rename 顶替。
    const tmpDest = `${dest}.tmp`;
    rmSync(tmpDest, { force: true });
    copyFileSync(binPath, tmpDest);
    if (t.bin !== 'sing-box.exe') chmodSync(tmpDest, 0o755);
    renameSync(tmpDest, dest);
    // 指纹在**产物 rename 之后**才记：反过来会在落地失败时留下「指纹说是新的、盘上还是旧的」，
    // 那比没有指纹更糟 —— 下一趟会 skip 掉它。
    recordStamp(ROOT, `core:${t.key}`, fingerprint);
    console.log(`  ok: ${t.dir}/${t.bin} (archive sha ${got.slice(0, 12)}…)`);
    ok++;
  } catch (e) {
    console.error(`  FAILED ${t.key}: ${e.message}`);
    failed++;
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

console.log(
  `\nsing-box cores: ${ok} ready, ${failed} failed (version ${VERSION})` +
    `${ONLY ? `，本次限定平台 ${ONLY.join(',')}（共 ${TARGETS.length} 个平台）` : ''}.`,
);
process.exit(failed > 0 ? 1 : 0);
