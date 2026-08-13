#!/usr/bin/env node
/**
 * fetch-cronet.mjs — 下载各平台 NaiveProxy 核心库 libcronet 到 resources/{平台}/，
 * 供 Tauri bundle resources 随安装包打包（与 sing-box 二进制同模式）。
 *
 * 用法：node scripts/fetch-cronet.mjs [--force] [--skip-gomod-check]
 *
 * ⚠️ macOS 不在此脚本范围：cronet 在 mac 上不走动态库。mac-arm64 与 mac-x64 的 sing-box 二进制**都已把
 *   cronet 静态编入（CGO）**，naive 两架构均开箱即用。strings 坐实：两架构 tags 同含 `with_naive_outbound`、
 *   cronet 符号计数均 1588、二进制 73/78MB（远大于走动态库的 linux 70/win 71MB）。详见 README。
 *
 * # 渠道：Go module proxy，**不是** GitHub Releases（2026-08-05 换）
 *
 * 此前从 `SagerNet/cronet-go` 的 **Releases 资产**（`libcronet-linux-amd64.so` 等）下载，版本 pin 是
 * release tag（如 `v148.0.7778.96-1`）。**那条渠道与核已经脱钩**：sing-box 自 1.14 起把 libcronet 作为
 * Go module 依赖（`github.com/sagernet/cronet-go/lib/<平台>`，伪版本 `v0.0.0-<时间戳>-<commit>`），
 * 而 cronet-go 的 Releases 页停在 2026-05-13 的 v148 —— 核要的 150.0.7871.63 那一版**根本没有对应的
 * Release**。继续走 Releases 只能拿到一个越来越旧的库。
 *
 * 实测（2026-08-05）：`1.14.0-beta.5` 的 `go.mod` 钉 `lib/linux_amd64 v0.0.0-20260731161755-38229fb700f6`，
 * 该模块 zip 18.9 MB，内含的 `libcronet.so` 版本串正是 `150.0.7871.63`；Windows 同伪版本、同版本串。
 *
 * # 版本耦合由脚本自己核对，不靠人记（`--skip-gomod-check` 可关）
 *
 * 每次下载前拉 `sing-box@v<bundledCoreVersion>` 的 `go.mod`，取其中 `cronet-go/lib/linux_amd64` 的伪版本，
 * 与 manifest 的 `cronetModuleVersion` 逐字比对，不符即 fail。**这是本次改造的核心**：旧脚本对「核升了、
 * cronet 没跟」毫无察觉——两条渠道各自成立、各自成功，只是拿到的不是同一版；而 cronet 走 C API（Chromium
 * 稳定 ABI），版本不匹配多半也不会当场报错，于是漂移可以静默存在很久。让它自曝，比事后从 naive 的符号
 * 错误倒推便宜得多。
 *
 * # 与 `fetch-core.mjs` 同一套做法（刻意对齐，勿各写各的）
 *
 * 下载走 `curl -fL --retry 3`（而非自写 https.get：`-f` 拦 404 页面冒充成功、`-L` 跟重定向、`--retry`
 * 抗瞬时抖动，全是白给的）、解压走系统 `unzip`、临时产物落 `mkdtemp` 工作区并在 `finally` 里整个删掉、
 * 落地走 `.tmp` → `rename`。**唯一有意的差异是校验对象**：
 *
 * - `fetch-core` 校验**压缩包**（其 sha 就是 GitHub release API 给的 asset digest，可直接抄）；
 * - 本脚本校验**解压出来的库本体** —— module zip 的字节受 proxy 打包方式影响，而库本体才是运行期真正
 *   被 dlopen 的东西。故先解压再校验，manifest 里 `cronetArchiveSha256` 存的是库本体的 sha。
 *
 * 两个脚本相似归相似，**下载与原子落地不抽公共模块**：那两处一共两个调用点，抽出来的抽象比它消掉的
 * 重复更重。对齐的是**写法**，不是造一层共享代码。
 *
 * ⚠️ **解压是例外**（2026-08-05 收窄上面这条）：`lib/extract-zip.mjs` 确实抽了出来。判据不是「重复」——
 * 是「zip 用哪个解压器」变成了一条**跨平台判据**（Linux 的 GNU tar 不认 zip、Windows 没有 unzip），
 * 三个 fetch 脚本各写一遍必然各自漂，且漂了以后只在某一条 CI 腿上炸。判据本身需要单一真值点，
 * 与「省几行重复」无关。
 */
import { execFileSync } from 'child_process';
import { createHash } from 'crypto';

import { extractZip, findInZipRoot } from './lib/extract-zip.mjs';
import { isFresh, readStamps, recordStamp } from './lib/fetch-stamp.mjs';
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  renameSync,
  rmSync,
} from 'fs';
import { tmpdir } from 'os';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

const coreManifest = JSON.parse(readFileSync(join(ROOT, 'src-tauri/core-manifest.json'), 'utf-8'));
const CORE_VERSION = coreManifest.bundledCoreVersion;
const CRONET_VERSION = coreManifest.cronetVersion; // 人读的 Chromium 版本（如 150.0.7871.63）
const MODULE_VERSION = coreManifest.cronetModuleVersion; // 下载用的 Go 伪版本
const CRONET_SHA = coreManifest.cronetArchiveSha256 || {};
const MODULE_PROXY = 'https://proxy.golang.org';
const MODULE_BASE = 'github.com/sagernet/cronet-go/lib';
const FORCE = process.argv.includes('--force');
const SKIP_GOMOD_CHECK = process.argv.includes('--skip-gomod-check');
const CHECK_ONLY = process.argv.includes('--check-only');

// 仅 linux/windows 走动态库；mac 静态编入核心二进制，不需下载。
// `module` = cronet-go 的 per-平台子模块名；`member` = zip 里那个库文件的基名。
const TARGETS = [
  { dir: 'resources/linux', module: 'linux_amd64', member: 'libcronet.so', out: 'libcronet.so', key: 'linux' },
  { dir: 'resources/win', module: 'windows_amd64', member: 'libcronet.dll', out: 'libcronet.dll', key: 'win' },
];

const sha256 = (file) => createHash('sha256').update(readFileSync(file)).digest('hex');

/**
 * 从核的 go.mod 取它真正依赖的 cronet-go lib 伪版本，与 manifest 比对。
 *
 * 只读 `lib/linux_amd64` 一行：同一次 cronet-go 构建里所有 per-平台子模块共用同一个伪版本
 * （实测 beta.5 的 31 个 `lib/*` 行版本串完全一致），取任意一行即可代表整批。
 */
function crossCheckWithGoMod() {
  const url = `https://raw.githubusercontent.com/SagerNet/sing-box/v${CORE_VERSION}/go.mod`;
  const text = execFileSync('curl', ['-fsSL', '--retry', '3', url], { encoding: 'utf-8' });
  const m = text.match(/github\.com\/sagernet\/cronet-go\/lib\/linux_amd64\s+(\S+)/);
  if (!m) {
    throw new Error(
      `sing-box v${CORE_VERSION} 的 go.mod 里找不到 cronet-go/lib/linux_amd64 —— ` +
        '上游可能改了依赖结构，本脚本的渠道假设需要重新确认'
    );
  }
  if (m[1] !== MODULE_VERSION) {
    throw new Error(
      `cronet 版本与核不匹配：sing-box v${CORE_VERSION} 要 ${m[1]}，` +
        `而 core-manifest.json 的 cronetModuleVersion 是 ${MODULE_VERSION}。\n` +
        '  → 升级核时必须同步这一项（并重算 cronetArchiveSha256）'
    );
  }
  console.log(`go.mod 交叉校验通过：sing-box v${CORE_VERSION} ↔ cronet ${MODULE_VERSION}`);
}

if (!MODULE_VERSION) {
  console.error('core-manifest.json 缺 cronetModuleVersion —— 无从确定要下哪一版，拒绝继续');
  process.exit(1);
}

// ── `--check-only`：只跑 go.mod 交叉校验，一个字节都不下载（2026-08-10 新增，给 ci.yml 用）──
//
// 动机是一次实付的教训：抬核到 1.14.0-beta.12 时漏改 cronetModuleVersion，本脚本的交叉校验**抓住了**
// 它，但抓住的地方是 package.yml 的 macos-arm64 腿 —— 私有仓 10x 计费，为一个改一行就能修的元数据
// 漂移烧掉一整条打包腿；而 ci.yml（push main 只起 1 个 1x 的 ubuntu job）里没有任何等价判据。
// 门本身不缺，缺的是**门装在哪条腿上**：判据只需要核版本 + manifest + 一次 go.mod 的 curl，
// 三样在最便宜那条腿上全都有，没有任何理由等到打包才问。
//
// 绕过下面那套 stamp 快路径（不复用 `pending`/`gomodChecked`）。**这是纵深防御，不是在修一个实测到
// 的假绿** —— 两条实测（2026-08-10）把「不绕就会漏」这个说法否掉了，别照着那个错理由传下去：
//   · `cronet:gomod-check` 的指纹本就含核版本（`${CORE_VERSION}|${MODULE_VERSION}`），故本次这类
//     漂移（核动、cronet 没动）指纹必然失配、校验照跑 —— 上面那段注释早把这个洞堵了；
//   · `resources/.fetch-stamp.json` 被 .gitignore 挡在库外，CI 恒为干净 runner，绕不绕零差别。
// 留着绕过，是因为「这条门跑不跑取决于一个可变的本地缓存文件」本身就不该成立：唯一能让 stamp
// 新鲜而结论已过期的路径是上游移动 tag（同版本号换 go.mod 内容），罕见但不是不可能，而代价只是
// 一次 ~10KB 的 curl。为一个 flag 的名字兑现它字面意思付这点钱，比让它依赖缓存状态划算。
//
// 与 `--skip-gomod-check` 同时给是自相矛盾（一个要求只做这件事、另一个要求别做这件事），
// 且失败方向朝绿，故硬拒而不是挑一个赢：让配错的人当场看见，而不是收下一个假绿。
if (CHECK_ONLY) {
  if (SKIP_GOMOD_CHECK) {
    console.error('FAILED: --check-only 与 --skip-gomod-check 互斥（同时给会得到一个什么都没验的绿）');
    process.exit(1);
  }
  try {
    crossCheckWithGoMod();
  } catch (e) {
    console.error(`FAILED: ${e.message}`);
    process.exit(1);
  }
  process.exit(0);
}

// 全部目标都已就位（且非 --force）时不必联网校验：这一趟本来就什么都不下。
//
// 「就位」= 产物在 **且** 指纹是当前钉扎版本的（`fingerprintOf`），不是「文件在不在」——
// 否则升了 cronetModuleVersion 之后，盘上那批旧库会让本判据认为「什么都不用下」，
// 从而**连 go.mod 交叉校验一起跳过**，正是最需要它报警的那一次反而不报。
const stamps = readStamps(ROOT);
const fingerprintOf = (t) => `${MODULE_VERSION}|${(CRONET_SHA[t.key] || '').replace(/^sha256:/, '')}`;
const isCurrent = (t) =>
  isFresh(stamps, `cronet:${t.key}`, fingerprintOf(t), existsSync(join(ROOT, t.dir, t.out)));
const pending = TARGETS.filter((t) => FORCE || !isCurrent(t));

// 上面那条判据只覆盖了「cronet 版本变了」这一半。**另一半反向的情形它盖不住**：
// 升 `bundledCoreVersion` 而 `cronetModuleVersion` 原样不动 —— 此时全部产物指纹依旧新鲜，
// `pending` 为空 ⇒ go.mod 交叉校验整个不跑。而这正是最需要它的那一次：新核到底还要不要
// 同一版 cronet，答案只在新核的 go.mod 里。判据（cronetModuleVersion）恰好是校验要去
// **发现**的那个值 ⇒ 循环，校验只能在有人已经知道答案之后才跑。
//
// 故给交叉校验一条**自己的**指纹，含核版本：核一动它就过期，校验必跑一次。
// 通过后落章，同一对 (核, cronet) 不重复联网。
const GOMOD_CHECK_KEY = 'cronet:gomod-check';
const gomodFingerprint = `${CORE_VERSION}|${MODULE_VERSION}`;
const gomodChecked = isFresh(stamps, GOMOD_CHECK_KEY, gomodFingerprint, true);

if ((pending.length > 0 || !gomodChecked) && !SKIP_GOMOD_CHECK) {
  try {
    crossCheckWithGoMod();
    // 落章放在**校验通过之后**：抛异常那条腿不落章 ⇒ 下次仍会重跑，
    // 不会出现「上次失败了但章已经盖上、于是再也不校验」。
    recordStamp(ROOT, GOMOD_CHECK_KEY, gomodFingerprint);
  } catch (e) {
    console.error(`FAILED: ${e.message}`);
    process.exit(1);
  }
}

let ok = 0;
let failed = 0;
for (const t of TARGETS) {
  const absDir = join(ROOT, t.dir);
  const dest = join(absDir, t.out);
  // 判据同上面 `pending` 那处（共用 `isCurrent`）：产物在 **且** 是当前钉扎版本的产物。
  if (!FORCE && isCurrent(t)) {
    console.log(`skip (up to date): ${t.dir}/${t.out} @ ${MODULE_VERSION}`);
    ok++;
    continue;
  }
  if (!FORCE && existsSync(dest)) {
    console.log(`stale: ${t.dir}/${t.out} 不是 ${MODULE_VERSION} 的产物，重新拉取`);
  }
  // 完整性 pin 是供应链防护核心：缺 pin 直接 fail（libcronet 是运行期 dlopen 执行的原生库，
  // 与 sing-box 核同级别，绝不无校验拉取）。
  const want = (CRONET_SHA[t.key] || '').replace(/^sha256:/, '');
  if (!want) {
    console.error(
      `  FAILED ${t.key}: core-manifest.json 缺 cronetArchiveSha256[${t.key}] pin → 拒绝无完整性校验拉取`
    );
    failed++;
    continue;
  }
  mkdirSync(absDir, { recursive: true });
  const url = `${MODULE_PROXY}/${MODULE_BASE}/${t.module}/@v/${MODULE_VERSION}.zip`;
  const work = mkdtempSync(join(tmpdir(), 'polaris-cronet-'));
  try {
    const archive = join(work, 'mod.zip');
    console.log(`downloading ${MODULE_BASE}/${t.module}@${MODULE_VERSION} → ${t.dir}/${t.out} ...`);
    execFileSync('curl', ['-fL', '--retry', '3', '-o', archive, url], { stdio: 'inherit' });

    // zip 内路径带 `<module>@<version>/` 前缀。此前用 `unzip -j '*/<member>'`（junk-paths +
    // 通配选成员）只解出要的那一个；改为全解 + JS 定位，因为那套 flag 在 Windows 唯一可用的
    // 解压器 bsdtar 上没有逐字对应物（见 lib/extract-zip.mjs 头注）。多解出来的字节全在
    // `work` 这个 mkdtemp 工作区里，finally 整个 rmSync，不留痕。
    const extractDir = join(work, 'x');
    mkdirSync(extractDir, { recursive: true });
    extractZip(archive, extractDir);
    const libPath = findInZipRoot(extractDir, t.member);
    if (!libPath) {
      throw new Error(`module zip (${t.module}) 里没有 ${t.member}（上游布局可能变化）`);
    }

    // 校验的是**库本体**不是 zip —— 理由见头注「与 fetch-core.mjs 同一套做法」一节。
    const got = sha256(libPath);
    if (got !== want) {
      throw new Error(`sha256 不符：期望 ${want}，实得 ${got}（版本漂移 / 投毒 / 截断）`);
    }

    // 原子落地：拷到 .tmp → rename 顶替（中断不会留下半个库被下次 skip-exists 当成好的）。
    const tmpDest = `${dest}.tmp`;
    rmSync(tmpDest, { force: true });
    copyFileSync(libPath, tmpDest);
    renameSync(tmpDest, dest);
    // 指纹在**产物 rename 之后**才记（理由同 fetch-core：先记会在落地失败时留下
    // 「指纹说是新的、盘上还是旧的」，下一趟直接 skip 掉，比没指纹更糟）。
    recordStamp(ROOT, `cronet:${t.key}`, fingerprintOf(t));
    console.log(`  ok (sha ${want.slice(0, 12)}…)`);
    ok++;
  } catch (e) {
    console.error(`  FAILED ${t.key}: ${e.message}`);
    failed++;
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}
console.log(`\ncronet libs: ${ok} ready, ${failed} failed (chromium ${CRONET_VERSION} / ${MODULE_VERSION}).`);
console.log('macOS: cronet 静态编入 mac-arm64 核心，无需下载（见脚本头注）。');
process.exit(failed > 0 ? 1 : 0);
