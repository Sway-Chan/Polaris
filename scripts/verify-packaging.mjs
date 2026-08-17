#!/usr/bin/env node
/**
 * verify-packaging.mjs — 打包链不变量检查（三模式）。
 *
 * 存在理由：「每个安装包只含且恰含本平台内核」这条核心交付契约，此前**全靠四个 conf 文件名拼对，
 * 零断言**。任一 per-platform conf 被改名 / 漏建 / 键名打错，合并结果仍是合法 JSON、bundler 照常
 * 出包，装出来的 app 没有内核 —— 只在用户机器上 `resolve_core_binary` → Err 才暴露。
 * 本脚本把那条契约变成**会转红的门**。
 *
 * 三个模式（各自独立、都可在任意平台的开发机上跑）：
 *
 *   node scripts/verify-packaging.mjs confs
 *     纯静态：只读 5 个 conf + core-manifest.json + package.yml。不需要构建产物。
 *     守：公共资源不丢 / 每个 conf 恰含一个平台内核 / base 不含任何平台内核 /
 *         每个 conf 都被 workflow 显式引用（改名即红）/ offline conf 不越权覆盖 base。
 *
 *   node scripts/verify-packaging.mjs payload --label <label> --root <bundle 根>
 *     构建后：`--root` 收 **bundle 根**（`target/release/bundle`，传了 --target 时
 *     `target/<triple>/release/bundle`），对该 label 的**每个 bundle target 各自**断言
 *     「恰含本平台那一份内核、且字节大小与源 `resources/<平台>/` 一致」。
 *     ⚠️ 不可指向 `target/release`：那里的 `_up_/resources/` 是 **cargo build script 的 staging copy**，
 *        与 bundler 有没有把内核铺进包无关，打在它上面等于没有门（详见 BUNDLE_TREES 注释）。
 *     ⚠️ windows 腿是**例外**：NSIS 把资源从源路径直接编进 .exe，bundle 侧无副本可扫，
 *        该腿如实退化为 **staging 检查**（输出里明确标注），不冒充产物验证。
 *
 *   node scripts/verify-packaging.mjs assets --label <label> --dir <dir>
 *     构建后：把产物文件名喂给 `crates/updater/src/github.rs::find_suitable_update_asset`
 *     的同一套选包规则，断言本 job 产出的资产**恰好命中一个**。
 *     Windows 契约尤其脆，且**按形态分成两条互不相交的规则**：
 *       - installed → `.exe` 且名含 `win`。Tauri NSIS 默认名 `Polaris_<ver>_x64-setup.exe`
 *         **不含 win** ⇒ 不改名的话 Windows 用户永远收不到更新且静默。
 *       - loose（便携）→ `polaris-portable-*.zip`。便携产物是 zip，结构性进不了上面那条 `.exe`
 *         过滤；此前两个形态共用同一候选集 ⇒ 便携用户恒被发 NSIS 安装器（#72 形态错配本体，
 *         2026-07-22 修）。故这里两条规则各自断言，只镜像一半就等于没守住便携形态。
 *     `--label release` 是**聚合口径**（四 job 产物汇进同一 release 后跑）：断言两个架构的 dmg
 *     各恰一个 + win setup 恰一个 + offline setup 恰一个 + 便携 zip 恰一个 + linux 双形态各恰一个，
 *     且便携候选与安装态候选不相交。per-job 口径断言「不得出现另一架构」，
 *     聚合侧两架构本就都在，故必须分开，不能复用。
 *
 *     assets 模式除命名外还有**两道内容门**（射程都只覆盖 updater 会真正命中的那些资产）：
 *       - **体积门（U2）**：`> MAX_UPDATE_ASSET_BYTES` 即红。见该常量文档；
 *       - **摘要门（U3）**：`--label release` 下 `SHA256SUMS` 缺失、格式坏、覆盖面对不上或
 *         逐条摘要不符即红。见 [`checkSha256Sums`]。
 *     `--names-only` 供**发布后**那一遍用：那时喂进来的是按真实资产名造的**同名空文件**
 *     （不回下 ~600 MB 真产物），体积与摘要在其上不可判定，故显式跳过并在输出里如实标注 ——
 *     缺了这个开关，那一遍会用 0 字节的假文件去比摘要，得到一片恒红。
 *
 * 退出码：0 = 全部不变量成立；1 = 有违反（逐条打印）。
 */

import { readFileSync, existsSync, statSync, readdirSync } from 'fs';
import { createHash } from 'crypto';
import { join, dirname, resolve, basename, relative } from 'path';
import { fileURLToPath } from 'url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SRC_TAURI = join(ROOT, 'src-tauri');
const WORKFLOW = join(ROOT, '.github/workflows/package.yml');

/**
 * label（CI matrix）→ 平台内核目录名（resources/ 下的目录 = core-manifest 的 key）。
 * 单一真值：平台集合本身取自 core-manifest.json 的 coreArchiveSha256 键，
 * 新增平台却忘了配 conf ⇒ confs 模式转红。
 */
const LABEL_TO_CORE = {
  linux: 'linux',
  windows: 'win',
  'macos-arm64': 'mac-arm64',
  'macos-x64': 'mac-x64',
};

/** 平台内核目录 → 该平台的 tauri conf 文件名（相对 src-tauri/）。 */
const CORE_TO_CONF = {
  linux: 'tauri.linux.conf.json',
  win: 'tauri.windows.conf.json',
  'mac-arm64': 'tauri.macos-arm64.conf.json',
  'mac-x64': 'tauri.macos-x64.conf.json',
};

const errors = [];
const notes = [];
const fail = (msg) => errors.push(msg);
const note = (msg) => notes.push(msg);

/**
 * 输入侧读取失败（文件缺失 / 坏 JSON）。与「不变量被违反」区分开：
 * 前者是**门自己读不到前置**，后者是门读到了并判红。两者都 exit 1，但措辞必须不同 ——
 * 否则 CI 日志里只有一坨 `node:fs` / `JSON.parse` 裸栈，看不出「哪个不变量因此无从断言」。
 */
class InputError extends Error {}

/**
 * @param {string} path 要读的文件
 * @param {string} why  读它是为了断言什么 —— 读失败时这句话就是 CI 日志里唯一的线索
 */
function readJson(path, why) {
  let text;
  try {
    text = readFileSync(path, 'utf8');
  } catch (e) {
    throw new InputError(`读不到 ${path}（${e.code ?? e.message}）—— ${why}`);
  }
  try {
    return JSON.parse(text);
  } catch (e) {
    throw new InputError(`${path} 不是合法 JSON：${e.message} —— ${why}`);
  }
}

/**
 * resources 条目形如 `../resources/mac-x64/` → 取出 `mac-x64`；非平台条目返回 null。
 *
 * 判据是**前缀**（`../resources/<平台>/…`）而非「整串恰为目录」：`../resources/win/sing-box.exe`
 * 这种**文件粒度**条目照样把该平台内核塞进包里，只认目录形态会让它整条逃逸——
 * 即 §10.2「四平台内核死重」的文件粒度版本（实测变异 M2b：四份 conf + base 各加一条
 * `../resources/win/sing-box.exe`，旧判据 exit 0，四个包全部夹带 Windows 内核）。
 */
function coreDirOf(entry, platforms) {
  const m = /^\.\.\/resources\/([^/]+)(?:\/|$)/.exec(String(entry).replace(/\\/g, '/'));
  if (!m) return null;
  return platforms.includes(m[1]) ? m[1] : null;
}

// ───────────────────── workflow matrix 解析（不变量 D 用）─────────────────────
/** 剥掉行尾 YAML 注释（引号内的 `#` 不算注释起点）。 */
function stripYamlComment(line) {
  let quote = null;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (quote) {
      if (c === quote) quote = null;
    } else if (c === '"' || c === "'") {
      quote = c;
    } else if (c === '#' && (i === 0 || /\s/.test(line[i - 1]))) {
      return line.slice(0, i);
    }
  }
  return line;
}

/** 去掉标量外层的成对引号。 */
function unquoteScalar(v) {
  const t = v.trim();
  const paired = t.length >= 2 && ((t[0] === "'" && t.endsWith("'")) || (t[0] === '"' && t.endsWith('"')));
  return paired ? t.slice(1, -1) : t;
}

/**
 * 在 `[from, to)` 行区间里，按**兄弟层缩进**找名为 `key` 的映射键，返回它的块范围。
 *
 * 「兄弟层」= 区间内第一条非空行的缩进。更深的行一律跳过 ⇒ 不会把
 * `steps: - with: include:` 之类深层同名键误当成本层的。缩进 ≤ `parentIndent`
 * 即离开父块，直接判无。
 *
 * @returns {{start:number, indent:number, end:number}|null} start=该键所在行；end=块结束行（不含）
 */
function findKeyBlock(lines, from, to, parentIndent, key) {
  let siblingIndent = null;
  for (let i = from; i < to; i++) {
    const raw = stripYamlComment(lines[i]);
    if (raw.trim() === '') continue;
    const indent = raw.length - raw.trimStart().length;
    if (indent <= parentIndent) return null; // 已离开父块
    if (siblingIndent === null) siblingIndent = indent;
    if (indent !== siblingIndent) continue; // 更深层，不是本层的键
    const m = /^([A-Za-z0-9_.-]+):\s*(.*)$/.exec(raw.trim());
    if (!m || m[1] !== key) continue;
    let end = to;
    for (let j = i + 1; j < to; j++) {
      const r2 = stripYamlComment(lines[j]);
      if (r2.trim() === '') continue;
      const ind2 = r2.length - r2.trimStart().length;
      if (ind2 <= siblingIndent) {
        end = j;
        break;
      }
    }
    return { start: i, indent: siblingIndent, end };
  }
  return null;
}

/**
 * 解析 workflow 的平台矩阵，返回腿级平铺键值对（每条腿一个对象）。
 *
 * 🔴 **数据本体已不在 `jobs.package.strategy.matrix.include`**（2026-07-31 起）：矩阵改成运行时
 * 由 `jobs.setup` 解析（`include: ${ fromJSON(needs.setup.outputs.include) }`），JSON 数据搬进了
 * setup 那个 step 的 shell 里（`all='[...]'`）。本函数没跟着搬，于是从那天起它恒返回 null
 * ⇒ 不变量 D 恒判红 ⇒ **`Verify packaging conf invariants` 这一步拦住了所有平台的打包**
 * （它排在 `Build installers` 之前）。2026-08-05 首次跑 linux 打包腿时才暴露。
 *
 * 教训记在这里而不是 commit 里：把「数据本体」搬家时，**搬的不只是那段数据和它的注释，还有所有
 * 按路径锚定它的消费方**。那次搬家的注释写了「随数据本体原样搬来」，却没提还有一个脚本在按老路径找。
 *
 * 只覆盖本仓用到的形态（`- key: value` 的标量映射），**不是**通用 YAML 解析器——
 * 为它引一个 YAML 依赖不值得（本脚本零依赖，CI 里直接 `node scripts/verify-packaging.mjs` 跑）。
 *
 * 🔴 **必须按完整路径锚定，不能「全文件第一个 include:」**：后者靠「package 恰好是文件里第一个
 * 带 matrix 的 job」这个**位置巧合**成立。实测变异 Y2：在 `package` 之前插一个带 matrix 的诱饵
 * job（四腿 conf 全对）、同时把真 `macos-x64` 腿的 `--config` 删掉 ⇒ 旧实现 exit 0 **假绿**，
 * 整条不变量 D 被一个装饰性 job 顶替。现改为 jobs → package → strategy → matrix → include
 * 逐级下钻，认的是**那条 include**，不是「某条 include」。
 *
 * 🔴 **只收缩进恰等于腿首行键位的键**：解析器此前把任意深度的 `k: v` 平铺进当前腿。
 * 实测变异 Y1：把 `tauri_args` 下沉进腿内 `env:` 子块（YAML 上该腿根本没有腿级 `tauri_args`）
 * ⇒ 旧实现照样收进 `leg.tauri_args`，exit 0 **假绿**。现在深层键被跳过 ⇒ 该腿没有 `tauri_args`
 * ⇒ 不变量 D 判红。（锚点 Y7 / 多行块标量 Y8 本就 fail-closed，实测确认。）
 *
 * **注释必须先剥掉**：不变量 D 要断言的是「本腿真的传了自己那份 conf」这条**绑定关系**，
 * 而旧实现是对整份 YAML 文本（含注释）做 `includes` —— 注释里出现同名路径即可满足。
 *
 * 路径走不通（job 改名 / 结构变形）一律返回 null ⇒ 调用侧判红，不静默跳过不变量 D。
 */
function parseMatrixInclude(text) {
  const lines = text.split('\n');
  const jobs = findKeyBlock(lines, 0, lines.length, -1, 'jobs');
  if (!jobs) return null;
  // 锚在 `jobs.setup` 内，**不是**全文件搜 `all='` —— 同 Y2 变异那条纪律：全文件搜靠「本文件里
  // 只有一处这样的赋值」这个位置巧合成立，插一个诱饵 job 就能顶替整条不变量 D。
  const setup = findKeyBlock(lines, jobs.start + 1, jobs.end, jobs.indent, 'setup');
  if (!setup) return null;
  const block = lines.slice(setup.start, setup.end).join('\n');
  // 数据本体形如 `all='[ {...}, ... ]'`（shell 单引号串，JSON 内只有双引号，故非贪婪到第一个 `'` 即可）。
  const m = block.match(/\ball='(\[[\s\S]*?\])'/);
  if (!m) return null;
  let legs;
  try {
    legs = JSON.parse(m[1]);
  } catch {
    return null; // JSON 写坏 ⇒ 判红，不静默跳过不变量 D
  }
  if (!Array.isArray(legs) || legs.length === 0) return null;
  if (!legs.every((l) => l && typeof l === 'object' && !Array.isArray(l))) return null;
  return legs;
}

/**
 * 取出 `tauri_args` 里所有 `--config <path>` 的路径。
 *
 * 用**分词**而非 `includes(...)` 子串匹配：子串判据下 `--config src-tauri/tauri.linux.conf.json.bak`
 * （变异 Y3）与「本腿 conf 后再追加一个别平台 conf」（变异 Y4）都 exit 0。前者在 tauri 参数解析期
 * 硬失败、后者由 payload 门接住，影响有界 —— 但这条门自称断言的是「本腿真的传了**自己那份**
 * conf」，就该自己守住，不该把判定外包给下游。
 */
function configArgsOf(tauriArgs) {
  const toks = String(tauriArgs ?? '')
    .trim()
    .split(/\s+/)
    .filter(Boolean);
  const out = [];
  for (let i = 0; i < toks.length; i++) {
    if (toks[i] === '--config' && i + 1 < toks.length) out.push(unquoteScalar(toks[i + 1]));
    else if (toks[i].startsWith('--config=')) out.push(unquoteScalar(toks[i].slice('--config='.length)));
  }
  return out;
}

// ───────────────────────── 模式 1：静态 conf 不变量 ─────────────────────────
function checkConfs() {
  // 本分支自己的错误计数起点：末尾那句 note 只能在**本模式**没报错时打（同 checkAssets 的 release 分支）。
  const errorsBefore = errors.length;
  const manifest = readJson(
    join(SRC_TAURI, 'core-manifest.json'),
    '平台集合无真值来源 ⇒ conf 模式的全部不变量（A/B/C/D + offline）都无从断言'
  );
  const platforms = Object.keys(manifest.coreArchiveSha256 ?? {});
  if (platforms.length === 0) {
    fail('core-manifest.json: coreArchiveSha256 为空 —— 平台集合无真值来源');
    return;
  }

  // 平台集合 ↔ conf 映射必须一一对应（新增平台忘了配 conf / conf 多余，都转红）。
  for (const p of platforms) {
    if (!CORE_TO_CONF[p]) {
      fail(`core-manifest 有平台 '${p}'，但 verify-packaging.mjs 的 CORE_TO_CONF 未登记对应 conf`);
    }
  }
  for (const p of Object.keys(CORE_TO_CONF)) {
    if (!platforms.includes(p)) {
      fail(`CORE_TO_CONF 登记了 '${p}'，但 core-manifest.coreArchiveSha256 里没有该平台`);
    }
  }

  const base = readJson(
    join(SRC_TAURI, 'tauri.conf.json'),
    '不变量 A（公共资源逐条同步到四份 conf）与「base 不得含平台内核」都无从断言'
  );
  const baseResources = base.bundle?.resources;
  if (!Array.isArray(baseResources)) {
    fail('tauri.conf.json: bundle.resources 缺失或不是数组');
    return;
  }

  // base 不得含任何平台内核目录：含了就等于「四平台内核全塞进每个包」的老毛病复发
  // （§10.2：四平台内核 210MB 死重）。
  for (const entry of baseResources) {
    const core = coreDirOf(entry, platforms);
    if (core) {
      fail(`tauri.conf.json: base bundle.resources 不得含平台内核目录 '${entry}' —— 会让每个平台包都塞进它`);
    }
  }

  const workflow = existsSync(WORKFLOW) ? readFileSync(WORKFLOW, 'utf8') : null;
  if (workflow === null) fail(`找不到 workflow：${WORKFLOW}`);

  // matrix 解析失败一律判红（前置缺失 ⇒ 失败，不静默跳过不变量 D）。
  const legs = workflow === null ? null : parseMatrixInclude(workflow);
  if (workflow !== null && (legs === null || legs.length === 0)) {
    fail('.github/workflows/package.yml: 解析不到平台矩阵（jobs.setup 的 Resolve platform matrix 里那段 all=[...] JSON）—— 不变量 D（每条腿绑定自己那份 conf）无从断言');
  }
  const legByLabel = new Map();
  for (const leg of legs ?? []) {
    if (!leg.label) {
      fail(`.github/workflows/package.yml: matrix include 有一条腿没有 label：${JSON.stringify(leg)}`);
      continue;
    }
    if (!LABEL_TO_CORE[leg.label]) {
      fail(`.github/workflows/package.yml: matrix 腿 label '${leg.label}' 未登记在 LABEL_TO_CORE`);
      continue;
    }
    if (legByLabel.has(leg.label)) {
      fail(`.github/workflows/package.yml: matrix 里 label '${leg.label}' 重复出现`);
      continue;
    }
    legByLabel.set(leg.label, leg);
  }

  for (const p of platforms) {
    const confName = CORE_TO_CONF[p];
    if (!confName) continue;
    const confPath = join(SRC_TAURI, confName);
    if (!existsSync(confPath)) {
      fail(`平台 '${p}' 的 conf 缺失：src-tauri/${confName}`);
      continue;
    }
    let conf;
    try {
      conf = readJson(confPath, `平台 '${p}' 的不变量 A/B/C（公共资源不丢 / 恰含本平台内核 / 路径存在）无从断言`);
    } catch (e) {
      fail(e.message);
      continue;
    }

    // per-platform conf 的**顶层键白名单**：只准 `$schema` + `bundle`，`bundle` 下只准 `resources`。
    //
    // 与 offline conf 那段（见下方）是**同一个失败面**：`--config` 传入的键按 RFC 7396 合并，
    // **覆盖 base**。§10.2 记了它真发生过一次 —— offline conf 硬编码 version 覆盖 base，
    // base 版本号一升，离线安装包仍被打成旧版本号。四份 per-platform conf 走的是同一条
    // `--config` 通路，此前却只校验 `bundle.resources`，覆盖不对称：实测变异 M10
    // （`tauri.linux.conf.json` 加 `"version": "9.9.9"` + `"productName": "Bogus"`）旧实现 exit 0。
    const confTopKeys = Object.keys(conf).filter((k) => k !== '$schema');
    if (confTopKeys.length !== 1 || confTopKeys[0] !== 'bundle') {
      fail(
        `src-tauri/${confName}: 顶层只应有 bundle（+$schema），实为 ${JSON.stringify(confTopKeys)} —— ` +
          `\`--config\` 按 RFC 7396 合并会**覆盖 base**（version / productName / identifier 尤其危险：` +
          `base 版本号一升，该平台包仍被打成旧版本号）`
      );
    }
    const confBundleKeys = Object.keys(conf.bundle ?? {});
    if (confBundleKeys.length !== 1 || confBundleKeys[0] !== 'resources') {
      fail(
        `src-tauri/${confName}: bundle 下只应有 resources，实为 ${JSON.stringify(confBundleKeys)} —— ` +
          `本仓四份 per-platform conf 的唯一职责是按平台筛内核，其余 bundle 配置归 base 单一真值`
      );
    }

    const res = conf.bundle?.resources;
    if (!Array.isArray(res)) {
      // Tauri 2 的 `bundle.resources` 另有合法的 map 形态（`{"src":"dest"}`），此处**故意**只放行数组：
      // 本仓四份 per-platform conf 全靠「数组整体替换」这条 RFC 7396 语义来按平台筛内核，
      // 混入 map 形态会让不变量 A/B 的判据失效。故这是**本仓房规**，不是 Tauri 的限制。
      fail(`src-tauri/${confName}: bundle.resources 缺失或不是数组 —— 本仓只用数组形态（RFC 7396 数组整体替换是按平台筛内核的机制本身；Tauri 的 map 形态在此不受支持）`);
      continue;
    }

    // 不变量 A：公共项必须**逐条**出现在每个平台 conf 里。
    // 数组是整体替换不是合并 ⇒ 往 base 加新公共资源而忘了同步四份，四个包全部静默不含它。
    for (const common of baseResources) {
      if (!res.includes(common)) {
        fail(`src-tauri/${confName}: 缺公共资源 '${common}'（base 有；数组整体替换 ⇒ 不同步就静默丢失）`);
      }
    }

    // 不变量 B：恰含一个平台内核目录，且就是本平台的。
    // 按**去重后的平台集合**判定（而非条目数）：coreDirOf 现在也认文件粒度条目，
    // 同平台多条（`../resources/win/a` + `../resources/win/b`）是合法的，跨平台才是死重。
    const cores = [...new Set(res.map((e) => coreDirOf(e, platforms)).filter(Boolean))];
    if (cores.length !== 1 || cores[0] !== p) {
      fail(
        `src-tauri/${confName}: 平台内核应恰为 ['${p}']，实为 ${JSON.stringify(cores)}` +
          ` —— 相关条目：${JSON.stringify(res.filter((e) => coreDirOf(e, platforms)))}`
      );
    }

    // 不变量 C：引用的目录必须真实存在（conf 写对了但目录没 fetch 也要红）。
    for (const entry of res) {
      const abs = resolve(SRC_TAURI, entry);
      if (!existsSync(abs)) {
        fail(`src-tauri/${confName}: 资源路径不存在 '${entry}' → ${abs}`);
      }
    }

    // 不变量 D：workflow 的 matrix 里，**本平台那条腿**必须在自己的 `tauri_args` 里显式传自己那份 conf。
    //
    // 只靠 Tauri 的「按平台名自动合并」= 文件一改名就静默失效（正是本检查要堵的失败面）；
    // 显式 --config 时改名会得到 `failed to read configuration file` 硬失败。
    //
    // 🔴 这里断言的是**绑定关系**，不是「文本里出现过这个字符串」。旧写法对整份 YAML（含注释）
    // 做 `includes`，两类真缺陷整条逃逸（均已实测 exit 0）：
    //   - 变异 M4：从 linux 腿删掉 `--config`、把路径留在注释里 ⇒ 子串仍在，静默放行；
    //   - 变异 M3b：两条 mac 腿的 conf **对调** ⇒ arm64 包塞 x64 核，两个字符串都还在，静默放行。
    const label = Object.keys(LABEL_TO_CORE).find((l) => LABEL_TO_CORE[l] === p);
    if (workflow !== null && legs !== null && legs.length > 0) {
      const leg = legByLabel.get(label);
      if (!leg) {
        fail(`.github/workflows/package.yml: matrix 里没有 label '${label}' 的腿 —— 平台 '${p}' 不会被构建`);
      } else {
        // 断言的是 `--config` 的**集合恰为 {本腿那份}**，不是「字符串里出现过它」：
        // 子串判据放行 `...conf.json.bak`（Y3）与「本腿 conf + 追加一个别平台 conf」（Y4）。
        const confs = configArgsOf(leg.tauri_args);
        if (confs.length !== 1 || confs[0] !== `src-tauri/${confName}`) {
          fail(
            `.github/workflows/package.yml: matrix 腿 '${label}' 的 \`--config\` 应恰为 ` +
              `['src-tauri/${confName}']，实为 ${JSON.stringify(confs)}（tauri_args = ` +
              `${JSON.stringify(leg.tauri_args ?? null)}）—— 少了它会退回隐式合并（改名即静默失效）；` +
              `多一个别平台 conf 会按 RFC 7396 数组整体替换，该腿打进错误平台的内核`
          );
        }
      }
    }
  }

  // matrix 腿必须覆盖全部平台（多出来的腿在上面 LABEL_TO_CORE 校验里已拦）。
  for (const l of Object.keys(LABEL_TO_CORE)) {
    if (legs !== null && legs.length > 0 && !legByLabel.has(l)) {
      fail(`.github/workflows/package.yml: matrix include 缺 label '${l}' 的腿`);
    }
  }

  // offline conf：只准携带 webviewInstallMode。
  // 曾经复制了 productName/version/identifier —— 其中 version 在合并顺序上**覆盖 base**
  // （已实测：`--config` 传 version 会顶掉 tauri.conf.json 的值），base 版本号一升，
  // 离线安装包仍被打成旧版本号。
  const offlinePath = join(SRC_TAURI, 'tauri.offline.conf.json');
  if (!existsSync(offlinePath)) {
    fail('src-tauri/tauri.offline.conf.json 缺失');
  } else {
    const offline = readJson(offlinePath, 'offline conf「不越权覆盖 base」无从断言');
    const topKeys = Object.keys(offline).filter((k) => k !== '$schema');
    if (topKeys.length !== 1 || topKeys[0] !== 'bundle') {
      fail(`tauri.offline.conf.json: 顶层只应有 bundle（+$schema），实为 ${JSON.stringify(topKeys)} —— 冗余键会覆盖 base（version 尤其危险）`);
    }
    const bundleKeys = Object.keys(offline.bundle ?? {});
    if (bundleKeys.length !== 1 || bundleKeys[0] !== 'windows') {
      fail(`tauri.offline.conf.json: bundle 下只应有 windows，实为 ${JSON.stringify(bundleKeys)}`);
    }
    const winKeys = Object.keys(offline.bundle?.windows ?? {});
    if (winKeys.length !== 1 || winKeys[0] !== 'webviewInstallMode') {
      fail(`tauri.offline.conf.json: bundle.windows 下只应有 webviewInstallMode，实为 ${JSON.stringify(winKeys)}`);
    }
    // Tauri 2 的枚举是 camelCase；v1 的 PascalCase `OfflineInstaller` 会被 schema 拒收
    // （实测：`is not valid under any of the schemas listed in the 'oneOf' keyword`）。
    const mode = offline.bundle?.windows?.webviewInstallMode?.type;
    if (mode !== 'offlineInstaller') {
      fail(`tauri.offline.conf.json: webviewInstallMode.type 应为 'offlineInstaller'（Tauri 2 camelCase），实为 ${JSON.stringify(mode)}`);
    }
  }

  checkMacOpenGuide();

  // 失败时**不得**打这句：它字面断言「各含 1 份内核」，与紧随其后的 FAILED 并存就是
  // 一句字面为假的 ok 断言（正是本轮反复在查的「note 声称的比它验的多」）。
  if (errors.length === errorsBefore) {
    note(`conf 不变量：平台 ${platforms.join(', ')}，各含 1 份内核 + ${baseResources.length} 项公共资源`);
  }
}

/**
 * macOS 首次打开引导（#318）—— 三处文案/路径的一致性。
 *
 * # 为什么值得单独一条
 *
 * 这份引导是**用户在拿不到任何其它帮助时**唯一能看到的东西：他双击 app 报「已损坏」，
 * 此时他既没进过 README、也没进过应用（进不去）。而它的三个组成部分分别住在三个文件里：
 * 内容在 `packaging/`、注入与文件名在 `package.yml`、同一条命令的另一份在 `README.md`。
 * 任意一处改了名字或命令，症状都不是报错，而是「用户照着做但没用」。
 *
 * 本条在 **Linux 的 confs 腿**跑（打包前、1x 计费），不必等 mac 腿。mac 腿另有一条
 * 「把 dmg 挂回来看文件在不在」的开箱验 —— 两者管的是不同的东西：这里管**说得对不对**，
 * 那里管**塞没塞进去**。
 */
function checkMacOpenGuide() {
  const guidePath = join(ROOT, 'packaging', 'macos-dmg-open-guide.txt');
  if (!existsSync(guidePath)) {
    fail(`packaging/macos-dmg-open-guide.txt 不存在 —— dmg 内附引导那一步会直接失败`);
    return;
  }
  const guide = readFileSync(guidePath, 'utf8');
  const pkg = readFileSync(join(ROOT, '.github', 'workflows', 'package.yml'), 'utf8');
  const readme = readFileSync(join(ROOT, 'README.md'), 'utf8');

  // 唯一真值取 README（它是既有的、用户可见的那一份），引导必须照抄同一条命令。
  // 两处给不同命令 = 用户照着 dmg 里那份做完发现还是打不开，而 README 里写着另一条。
  const CMD = 'xattr -cr /Applications/Polaris.app';
  if (!readme.includes(CMD)) {
    fail(`README.md 里的 quarantine 命令不再是 \`${CMD}\` —— 真值变了，引导要跟着改（本门也要跟着改）`);
  }
  if (!guide.includes(CMD)) {
    fail(`引导里的命令与 README 不一致，应含 \`${CMD}\``);
  }
  // 开箱验也必须核对同一条命令；若仍搜旧命令/旧参数，DMG 明明正确却会在最后一步假红。
  if (!pkg.includes(`grep -Fq '${CMD}'`)) {
    fail(`package.yml 的 DMG 开箱验没有按完整新命令核对：应包含 \`grep -Fq '${CMD}'\``);
  }
  // 中英双语：dmg 是发给所有用户的，只有中文等于对一半用户没写。
  if (!/Applications folder/i.test(guide)) {
    fail('引导缺英文段 —— dmg 面向全部用户，单语等于对另一半人没写');
  }

  // 文件名：注入与开箱验两步各写一份，且必须以数字开头（Finder 按名排序，
  // 排在 app 后面的引导等于没有引导）。
  const names = [...pkg.matchAll(/guide_name="([^"]+)"/g)].map((m) => m[1]);
  if (names.length !== 2) {
    fail(`package.yml 里 guide_name 出现 ${names.length} 次，应恰好 2 次（注入 + 开箱验各一）`);
  } else if (names[0] !== names[1]) {
    fail(`package.yml 的两处 guide_name 不一致：${JSON.stringify(names)} —— 开箱验会去找一个不存在的名字`);
  } else if (!/^\d/.test(names[0])) {
    fail(`引导文件名 \`${names[0]}\` 不以数字开头 —— Finder 按名排序会把它排到 app 后面`);
  }

  if (!pkg.includes('packaging/macos-dmg-open-guide.txt')) {
    fail('package.yml 不再引用 packaging/macos-dmg-open-guide.txt —— 引导不会进 dmg');
  }
  note('macOS 首次打开引导：命令与 README 一致、中英双语、文件名两处同名且排序在前');
}

// ───────────────────────── 模式 2：构建产物载荷 ─────────────────────────

/**
 * 随包二进制家族：**每一个都必须逐个验**，不是「验内核就代表验了包」。
 *
 * 2026-08-10 实证：本门此前只扫 `sing-box`，于是三平台全部出货过**不含 `polaris-helper` 的安装包**，
 * 且四条腿全绿 —— package.yml 里从来没有构建/铺放 helper 的步骤，而门对它结构性失明，
 * 两个洞互相遮掩。取证方式是把 macos-arm64 的 dmg 拉下来解 UDIF 后在 HFS+ 目录里搜文件名：
 * `sing-box` 命中 2、`Polaris` 6、`Info.plist` 2，`polaris-helper` **0**（同一探针对存在的名字有命中 ⇒
 * 这个 0 是真缺失，不是探针坏）。后果不是少个可选件：`resolve_helper_binary` → Err ⇒ helper 装不上
 * ⇒ macOS/Windows 的 TUN 与特权网络操作整条不可用，而 app 照常启动、构建期与打包期零报错。
 *
 * 判据写成表而不是把 helper 硬编在内核那段里：再加第三个随包二进制时，漏掉它的默认后果是
 * 「表里没有 ⇒ 没人验」，与本次同型。故表本身也被 `payload_family_table_covers_all_bundled_bins`
 * 之外的东西约束不了 —— 这一条只能靠人，写在这里提醒下一个加二进制的人回来补一行。
 */
const PAYLOAD_FAMILIES = [
  {
    what: 'sing-box',
    names: new Set(['sing-box', 'sing-box.exe']),
    consequence: '用户机器上 resolve_core_binary → Err 才暴露的静默坏包',
  },
  {
    what: 'polaris-helper',
    names: new Set(['polaris-helper', 'polaris-helper.exe']),
    consequence:
      '用户机器上 resolve_helper_binary → Err ⇒ 特权 helper 装不上（TUN / 路由 / DNS 接管整条不可用），' +
      'app 仍能启动，故不装 TUN 试一次发现不了',
  },
];

// `names` 无默认值且排在 `out` 之前：漏传会当场 TypeError，而不是静默扫出空集合
// —— 空集合会一路走成「找不到 ⇒ 判红」，方向虽朝红，但红的理由是假的，排查要多绕一圈。
function walk(dir, names, out = [], depth = 0) {
  // 深度上限防意外深树 + 防 symlink 环路。留足余量：deb 的 staging 路径已到 10 层。
  //
  // 🔴 **symlink 必须跟进**（2026-08-05，首次 mac CI 实证驱动）：此前用 `Dirent.isDirectory()`，
  // 它对 symlink 恒为 false ⇒ 指向目录的软链整棵子树被跳过。macOS bundler 铺 `.app` 时
  // 资源很可能是软链（Linux 的 deb/AppImage 不是，所以 linux 腿一直是绿的，掩盖了这一点）。
  // 用 `statSync`（跟随软链）判类型；环路由 depth 上限兜住 —— 20 层内绕不回来就是真的深树。
  if (depth > 20) return out;
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const e of entries) {
    const full = join(dir, e.name);
    let st;
    try {
      st = statSync(full); // 跟随 symlink；断链会抛 → 跳过
    } catch {
      continue;
    }
    if (st.isDirectory()) walk(full, names, out, depth + 1);
    else if (st.isFile() && names.has(e.name)) out.push(full);
  }
  return out;
}

/**
 * 扫不到内核时的**布局取证**：列出该 scope 下的实际路径样本（标注类型），供一次跑就拿到真相。
 *
 * 没有它的话，失败消息只说「找到的 sing-box 文件：[]」—— 分不清是「bundler 没铺」「铺在别处」
 * 还是「扫描器进不去」。而 mac 腿每验证一次是 10x 计费的一整轮，猜错一次就是几百计费分钟。
 */
function layoutSample(dir, limit = 40) {
  const rows = [];
  const rec = (d, depth) => {
    if (depth > 6 || rows.length >= limit) return;
    let entries;
    try {
      entries = readdirSync(d, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      if (rows.length >= limit) return;
      const full = join(d, e.name);
      const link = e.isSymbolicLink() ? ' →(symlink)' : '';
      let kind = '?';
      try {
        const st = statSync(full);
        kind = st.isDirectory() ? 'dir ' : `file ${st.size}B`;
      } catch {
        kind = 'broken-link';
      }
      rows.push(`    ${kind}${link}  ${relative(dir, full)}`);
      if (kind === 'dir ') rec(full, depth + 1);
    }
  };
  rec(dir, 0);
  return rows.length > 0 ? rows.join('\n') : '    （空目录）';
}

/**
 * label → 该 job 在 **bundle 根**下能被扫描的产物目录（bundler 真铺出来的目录树）。
 *
 * 为什么只列这几个：`.dmg` / NSIS 的 `.exe` / `.deb` / `.AppImage` 都是**压缩或编译后的单文件**，
 * `walk()` 进不去；能回答「装出来到底有没有内核」的只有 bundler 留下的目录树。
 *
 * 🔴 这份表是本模式从「staging 检查」变回「产物验证」的关键。此前 `--root` 收的是 `target/release`，
 * 而 `target/release/_up_/resources/<平台>/` 是 **cargo build script 的 staging copy**，
 * 与 bundler 有没有把内核铺进包**完全无关**（实证：本机 `target/debug/` 下同样有 `_up_/resources/`，
 * 却根本没有 `bundle/` 目录 ⇒ 那棵树纯由 `cargo build` 生成）。于是变异 P2（deb/AppImage 里零内核）
 * 与 P3（`Polaris.app` 里零内核）均 exit 0 —— 「装出来没核」这条压根没守住。
 *
 * 实证来源（tauri-cli 2.11.4 预编译二进制 `ui/node_modules/@tauri-apps/cli-linux-x64-gnu/
 * cli.linux-x64-gnu.node` 的字符串表 / 内嵌模板，`strings` 可复现）：
 *   - deb：`bundle/deb` + `crates/tauri-bundler/src/bundle/linux/debian.rs`
 *     + `Failed to copy resource files` + `Failed to tar/gzip data directory`
 *     ⇒ 先把资源铺进 `bundle/deb/<pkg>/data/…` 再打 tar，目录树留存可扫。
 *   - appimage：`bundle/appimage` + `_deb` + `.AppDir` ⇒ `bundle/appimage/<pkg>.AppDir/…` 留存可扫。
 *   - nsis：内嵌 installer.nsi 模板是 `File /a "/oname={{this.[1]}}" "{{no-escape @key}}"`，
 *     `@key` 是资源的**源路径** ⇒ NSIS 直接从 `resources/` 编进 `.exe`，
 *     **bundle 侧不存在任何副本**。故 windows 腿列空 —— 该腿只能是 staging 检查，
 *     本脚本**如实这么标注**，不冒充产物验证。
 *
 * ✅ macOS（`bundle/macos/<Product>.app/Contents/Resources/_up_/resources/mac-<arch>/`）**已实证**
 *    （2026-08-05 首次跑 macos-arm64 打包腿）。第一跑扫到的是**空目录** —— 因为
 *    `tauri.conf.json` 的 `bundle.targets` 当时只有 `dmg`，bundler 不保留 `.app`，
 *    `bundle/macos/` 自然是空的。已给 targets 补上 `app`：dmg 本就是从那个 `.app` 打出来的，
 *    保留它零成本（`Upload artifacts` 只收 `*.dmg`，`.app` 不进 artifact）。
 *
 *    **如实标注一处差额**：本门验的是 `.app`，不是 dmg 内部。要验 dmg 得 `hdiutil attach` 挂载，
 *    那条路只能在 macOS 上跑、且与「脚本三模式都可在任意平台开发机上跑」冲突。dmg 由该 `.app`
 *    打出，中间丢文件属 Tauri 内部行为，风险极低但**不是零** —— 这是本门当前的射程边界。
 *
 * ⚠️ 旧注（保留作教训）：此处曾写「未在本机实证：本机是 Linux，
 *    上面那份 CLI 二进制里 macOS bundler 被 cfg 掉，没有对应字符串，也跑不了真 mac build。
 *    它是 **fail-closed 假设**——布局若与此不符，该步是**转红**（目录不存在 / 扫不到内核），
 *    不会假绿；首次 mac CI 跑一次即可证实或证伪。
 */
const BUNDLE_TREES = {
  linux: ['deb', 'appimage'],
  'macos-arm64': ['macos'],
  'macos-x64': ['macos'],
  windows: [], // 空 = 无 bundle 侧副本可扫 ⇒ 退化为 staging 检查（见上方 nsis 实证）
};

function checkPayload(label, root) {
  // 本模式自己的错误计数起点：末尾两句 note 只能在**本模式**没报错时打。
  // 此前无条件打印 ⇒ 变异 D/E（某个 bundle target 缺失）的输出里
  // `ok: payload：linux → 产物验证，bundle/deb + bundle/appimage 各自命中 linux（体积与源一致）`
  // 与紧随的 `FAILED` 并存 —— exit code 仍 1，门没坏，但 CI 日志里出现一句**字面为假**的断言。
  const errorsBefore = errors.length;
  const expected = LABEL_TO_CORE[label];
  if (!expected) {
    fail(`未知 label '${label}'，合法值：${Object.keys(LABEL_TO_CORE).join(', ')}`);
    return;
  }
  const trees = BUNDLE_TREES[label];
  if (!trees) {
    fail(`label '${label}' 未登记 BUNDLE_TREES —— 新增平台必须同时声明它的 bundle 产物目录`);
    return;
  }
  const rootDir = resolve(ROOT, root);
  if (!existsSync(rootDir)) {
    fail(
      `产物根不存在：${rootDir} —— payload 模式必须在构建之后跑。\n` +
        `  仓库根是 cargo workspace 根，产物不在 src-tauri/target/；` +
        `且本模式的 --root 要指向 **bundle 根**（target/release/bundle，传了 --target 时 target/<triple>/release/bundle）。`
    );
    return;
  }
  if (!statSync(rootDir).isDirectory()) {
    fail(`产物根不是目录：${rootDir}`);
    return;
  }

  // bundler 把 `../resources/x` 铺成 `_up_/resources/x`（tauri-utils::resource_relpath：ParentDir → `_up_`）。
  // 但**不把 `_up_` 写死进判据**：各平台 bundler 的中间 staging 目录布局未逐一实证过，写死会在
  // 布局不同的平台上假红。判据放宽为「路径里出现 `/resources/<manifest 里的平台名>/` 的 sing-box」——
  // 源码树的 resources/ 不在 bundle/ 下，不会被误判。日志里回打实际布局，布局变了肉眼可见。
  const platforms = Object.keys(
    readJson(join(SRC_TAURI, 'core-manifest.json'), '平台目录集合无真值来源 ⇒ payload 模式无从判定产物里的内核属于哪个平台')
      .coreArchiveSha256 ?? {}
  );
  const srcDir = join(ROOT, 'resources', expected);
  if (!existsSync(srcDir)) {
    // 前置缺失一律判红。此前体积断言被 `existsSync(src)` 包着 ⇒ 源不在就静默跳过，
    // 变异 P5（产物是 2 字节残包 + 源缺失）exit 0 —— 门自身前置缺失时静默跳步 = 没门。
    fail(`源内核目录不存在：${srcDir} —— 体积断言无从比对（前置缺失判红，不跳过）`);
  }

  // 断言口径：**每个 bundle target 各自命中**。少一个形态（deb 在、AppImage 掉了）也要红。
  const scopes =
    trees.length > 0
      ? trees.map((t) => ({ name: `bundle/${t}`, dir: join(rootDir, t), artifact: true }))
      : [{ name: root, dir: rootDir, artifact: false }];

  for (const scope of scopes) {
    if (!existsSync(scope.dir)) {
      fail(
        `产物目录不存在：${scope.dir}\n` +
          `  期望它是 ${scope.name}（bundler 为 ${label} 铺出的产物树）。\n` +
          `  常见原因：① bundler 没产出该形态；② --root 指的不是 bundle 根` +
          `（应为 target/release/bundle 或 target/<triple>/release/bundle，不是 target/release）。`
      );
      continue;
    }

    for (const family of PAYLOAD_FAMILIES) {
    const all = walk(scope.dir, family.names);
    const seen = new Map();
    for (const p of all) {
      // 取**最后**一处 `<...>/resources/<平台>/`：路径里可能先出现 resources/dashboard/ 之类的
      // 非平台段，用首个匹配会误判成「不是平台核」而漏掉。
      const segs = p.replace(/\\/g, '/').split('/');
      let core = null;
      for (let i = 0; i < segs.length - 1; i++) {
        if (segs[i] === 'resources' && platforms.includes(segs[i + 1])) core = segs[i + 1];
      }
      if (!core) continue;
      if (!seen.has(core)) seen.set(core, []);
      seen.get(core).push(p);
    }

    const hits = [...seen.values()].flat();
    if (hits.length === 0) {
      fail(
        `${scope.name} 里找不到任何 \`resources/<平台>/${family.what}*\` —— ` +
          `${scope.artifact ? `该产物装出来没有 ${family.what}` : `本平台 staging 里没有 ${family.what}`}` +
          `（${family.consequence}）。\n` +
          `  已扫描：${scope.dir}\n` +
          `  期望平台目录：${expected}（合法平台：${platforms.join(', ')}）\n` +
          `  该目录下找到的 ${family.what} 文件：${JSON.stringify(all.slice(0, 20))}\n` +
          `  实际布局样本（前 40 条，标注类型与软链）：\n${layoutSample(scope.dir)}`
      );
      continue;
    }

    const dirs = [...seen.keys()].sort();
    if (dirs.length !== 1 || dirs[0] !== expected) {
      fail(
        `${scope.name} 的 ${family.what} 平台应恰为 ['${expected}']，实为 ${JSON.stringify(dirs)} —— ` +
          `混进了别平台产物（§10.2 死重回潮）或缺本平台那份`
      );
    }

    // 体积断言不用魔数：直接与源 resources/<平台>/ 里那份比对大小。源缺失 = 判红，不跳过。
    for (const p of seen.get(expected) ?? []) {
      const src = join(srcDir, basename(p));
      if (!existsSync(src)) {
        fail(`${scope.name}: 产物里有 ${p}，但源 ${src} 不存在 —— 体积无从比对（前置缺失判红，不跳过）`);
        continue;
      }
      const got = statSync(p).size;
      const want = statSync(src).size;
      if (got !== want) {
        fail(`${scope.name}: 产物 ${family.what} 体积不符：${p} = ${got}B，源 ${src} = ${want}B`);
      }
    }

    for (const p of hits) console.log(`     ${p.replace(ROOT + '/', '')}`);
    }
  }

  if (errors.length !== errorsBefore) return; // 有违反就不打「成立」的 note

  if (trees.length > 0) {
    note(
      `payload：${label} → 产物验证，${scopes.map((s) => s.name).join(' + ')} 各自命中 ${expected} 的 ` +
        `${PAYLOAD_FAMILIES.map((f) => f.what).join(' + ')}（体积与源一致）`
    );
  } else {
    // 如实标注，不冒充产物验证：NSIS 把资源从**源路径**直接编进 .exe，bundle 侧没有可扫的副本，
    // 故这条腿只能证明「cargo 侧 staging 恰好只有本平台那几份且体积对」，证明不了安装器内容。
    note(
      `payload：${label} → **staging 检查**（不是产物验证）：扫的是 cargo build 铺的 ${root}/_up_/resources/，` +
        `恰含 ${expected} 的 ${PAYLOAD_FAMILIES.map((f) => f.what).join(' + ')} 且体积与源一致。` +
        `NSIS 从源路径直接编译资源进 .exe，bundle 侧无副本可扫 ⇒ ` +
        `「安装器内容是否含这些二进制」在本仓无自动门，由 Windows 真机安装验证覆盖。`
    );
  }
}

// ───────────────────────── 模式 3：产物命名 ↔ updater 选包契约 ─────────────────────────
/**
 * 与 `crates/updater/src/github.rs::find_suitable_update_asset` 同口径（**大小写敏感**）。
 *
 * Windows 侧那个函数按形态分两条**互不相交**的规则，故这里也是两个函数，别只镜像一半：
 *  - [`updaterWindowsCandidates`] ← installed 形态（`.exe` 且名含 `win`）；
 *  - [`updaterPortableCandidates`] ← loose 形态（`polaris-portable-` 前缀 + `.zip`）。
 *
 * 只镜像 installed 那条正是本轮修掉的缺陷得以长期存活的原因之一：便携形态在 release 里
 * 有没有可选的产物，此前**没有任何断言按 updater 的口径**去问。
 */
function updaterWindowsCandidates(names) {
  return names.filter((n) => n.endsWith('.exe') && n.includes('win'));
}
/** loose（便携）形态的候选集 = `github.rs` 的 `PORTABLE_ZIP_PREFIX` + `.zip`，逐字同口径。 */
function updaterPortableCandidates(names) {
  return names.filter((n) => n.startsWith('polaris-portable-') && n.endsWith('.zip'));
}
function updaterMacCandidates(names, archTag) {
  return names.filter((n) => n.includes(archTag) && n.endsWith('.dmg'));
}

// ───────────── U2：updater 目标资产的体积门 ─────────────
/**
 * updater **真正会下载**的那个资产的体积上限。超过即红。
 *
 * # 为什么它不等于客户端的 `APP_UPDATE_MAX_BYTES`（512 MiB）
 *
 * 那个常量是客户端的**绝对写入闸**（`src-tauri/src/commands/updater.rs`），职责是「别让一个撒谎的
 * 服务端把用户的盘写满」，故留了一个数量级以上的余量。拿它当发布门等于**没有门**：安装包从
 * 52 MiB 涨到 300 MiB 照样全绿，而这道门的全部理由是「体积再涨在发布时自曝」——U1 那个缺陷
 * （产物 52 MiB 撞上 16 MiB 下载闸、构建 CI 一路绿、只有用户真机更新失败才暴露）正是这么长出来的。
 * 早警值必须**贴着真实量级**设，不是贴着灾难值设。
 *
 * # 96 MiB 是怎么定出来的（实测，非拍脑袋）
 *
 * 本机留存的真实 CI 产物逐个 `stat`（updater 目标资产口径）：
 *
 * | 资产 | 字节 | MiB | 出处 |
 * |---|---|---|---|
 * | `*-mac-arm64.dmg` | 54,232,313（12 份留存里的最大值） | 51.72 | 本地 `/tmp/polaris-mac*` CI 产物 |
 * | `*-mac-x64.dmg`   | 51,102,510 | 48.73 | run 30990315709（`docs/polaris/design/polaris-windows-packaging-first-green-2026-08-05.md`） |
 * | `*-win-setup.exe` | 39,015,611 | 37.21 | run 31659532293 |
 * | `polaris-portable-*.zip` | 53,347,731 | 50.88 | run 31659532293 |
 * | `.deb` / `.AppImage` | **未测** | — | 本机无留存产物，且本轮不联网取；见下 |
 *
 * 取 **96 MiB ≈ 实测最大值（51.72 MiB）的 1.86 倍**：
 *  - 常规增长（内核/cronet/dashboard 版本迭代，每次几 MiB）撞不到，不会假红；
 *  - 已知的两类**阶跃式**回潮全部落在门外：四平台内核死重（§10.2，约 210 MiB）、
 *    误把 WebView2 离线负载打进主安装包（离线版实测 251,392,830 B = 239.75 MiB）；
 *  - 比客户端绝对闸早 5.3 倍触发 —— 它才是「早警」的那一份。
 *
 * Linux 两形态未测是**如实登记的判据缺口**：它们与 win/mac 同一份 payload（sing-box 内核 +
 * dashboard + 前端），量级同族，96 MiB 有近一倍余量；但这是推断不是实测。真红时先看输出里印的
 * 实际字节数再决定是「产物真涨了」还是「门定紧了」，别直接调门。
 *
 * # 射程：只覆盖 updater 会命中的资产
 *
 * `*-offline-setup.exe`（239.75 MiB）**故意不在射程内** —— 它是 LTSC/内网的**手动下载**变体，
 * 名字里没有 `win`，`find_suitable_update_asset` 结构性选不到它，自动更新腿永远不会去下它。
 * 把它一并卡掉只会让这道门在第一次跑的时候就假红。
 *
 * # 改这个值 = 改两处
 *
 * 另一份在 `src-tauri/src/commands/updater.rs` 的测试模块（`PACKAGING_MAX_UPDATE_ASSET_MIB`），
 * 由 `packaging_size_gate_is_mirrored_and_stays_under_the_client_write_gate` 逐字比对本行文本，
 * 两份漂开即转红（D5：维护两份 + 一条一致性测试钉死）。**改这里必须同步改那里**，
 * 且那条测试还会拦住「把它调到客户端写入闸之上」——那等于发一个客户端结构性下不动的包。
 */
const MAX_UPDATE_ASSET_BYTES = 96 * 1024 * 1024;

/** 随 release 一起发布的摘要清单（U3）。名字是**资产名**，不是路径。 */
const SHA256SUMS_NAME = 'SHA256SUMS';

const mib = (n) => `${(n / 1024 / 1024).toFixed(2)} MiB`;

/**
 * 本 label 下 **updater 会真正命中**的资产名 —— 体积门的射程恰好是这些。
 *
 * 判据一律**复用**上面那三个候选函数（与 `github.rs::find_suitable_update_asset` 同口径），
 * 本函数只负责「哪个 label 该看哪几条规则」的装配，不另写一套过滤条件：选包规则一改，
 * 这里跟着改，不会出现「命名门还在绿、体积门量错了对象」。
 *
 * - `windows` 腿只装配 installed 形态：该 job 的 `--dir dist-win` 里**没有**便携 zip
 *   （它打在仓库根），装上去等于加一条恒为空、永远不可能转红的断言；便携 zip 的体积由
 *   `release` 聚合口径覆盖（那里它确实在）。
 * - `linux` 腿两形态都算：per-job 口径只断言「各至少一个」，故这里也不假设恰好一个。
 */
function updaterTargetNames(label, names) {
  switch (label) {
    case 'windows':
      return updaterWindowsCandidates(names);
    case 'macos-arm64':
    case 'macos-x64':
      return updaterMacCandidates(names, LABEL_TO_CORE[label]);
    case 'linux':
      return names.filter((n) => n.endsWith('.deb') || n.endsWith('.AppImage'));
    case 'release':
      return [
        ...updaterMacCandidates(names, 'mac-arm64'),
        ...updaterMacCandidates(names, 'mac-x64'),
        ...updaterWindowsCandidates(names),
        ...updaterPortableCandidates(names),
        ...names.filter((n) => n.endsWith('.deb') || n.endsWith('.AppImage')),
      ];
    default:
      return [];
  }
}

/**
 * 体积门本体。**逐个印出实际体积**（不只在超限时印）：这道门将来要不要调、调到哪，
 * 唯一的依据就是历次 CI 日志里的这些数 —— 只在红的时候才印，等于把定阈值的数据丢了。
 */
function checkUpdateAssetSizes(label, targets, pathOf) {
  for (const name of targets) {
    const p = pathOf(name);
    if (!p) continue; // 命名门已经在报这一条了，这里不重复报。
    const size = statSync(p).size;
    if (size > MAX_UPDATE_ASSET_BYTES) {
      fail(
        `体积门（U2）：updater 目标资产 '${name}' 为 ${mib(size)}（${size} B），超过上限 ${mib(MAX_UPDATE_ASSET_BYTES)}。\n` +
          `  这道门是**早警**，不是客户端能力上限：客户端绝对写入闸是 512 MiB，走到那儿才炸就等于没警。\n` +
          `  先判定是「产物真的涨了」（查 payload：内核 / cronet / dashboard / 是否误把离线负载打进主包）\n` +
          `  还是「上限定紧了」；确属预期增长再同步改两处常量（本文件 MAX_UPDATE_ASSET_BYTES +\n` +
          `  src-tauri/src/commands/updater.rs 测试模块的 PACKAGING_MAX_UPDATE_ASSET_MIB），否则一致性测试会红。`
      );
    } else {
      note(`体积：${label} → '${name}' ${mib(size)} ≤ 上限 ${mib(MAX_UPDATE_ASSET_BYTES)}`);
    }
  }
}

// ───────────── U3：随包 SHA256SUMS ─────────────
/**
 * `SHA256SUMS` 门：缺失 / 格式坏 / 覆盖面对不上 / 逐条摘要不符，任一即红。
 *
 * # 它守的是什么（别夸大）
 *
 * 守的是**发布流程**：生成步骤压根没跑、跑了但漏了某个平台的资产、或者清单与真实产物对不上。
 * 缺了这道门，「发布带摘要」就只是 workflow 里一句无人核对的 shell —— 而一个静默不产出的
 * 生成步骤，与产出正确的生成步骤，在 CI 日志里长得一模一样。
 *
 * 它**不是**安全边界：`SHA256SUMS` 与安装包走同一 HTTPS 通道、同一 release、同一发布账号，
 * 能替换安装包的人同样能替换它。它防的是**传输损坏与截断**，不防「GitHub 账号或 TLS 被攻破」。
 * 端到端完整性需要签名（公钥内置于应用），那是独立决策，本轮不做，也不假装 SHA 等价于它。
 *
 * # 判据不是「文件在不在」
 *
 * 逐条**重算** sha256 与清单比对，并要求清单与实际资产**双向**覆盖（少一条 = 有资产没被摘要，
 * 多一条 = 摘要指向一个不存在的资产）。只查在场的话，一个空文件、或者上一轮遗留的旧清单，
 * 照样能让门全绿。
 */
function checkSha256Sums(names, pathOf, namesOnly) {
  if (!names.includes(SHA256SUMS_NAME)) {
    fail(
      `摘要门（U3）：release 资产里缺 \`${SHA256SUMS_NAME}\` —— 发布流程的生成步骤没跑或产物没被上传。\n` +
        `  缺了它，「随包发布摘要」这条承诺在真实 release 上不成立（消费侧将来要不要接是另一回事）。`
    );
    return;
  }
  if (namesOnly) {
    note(`摘要：${SHA256SUMS_NAME} 在场（--names-only：内容比对不可判定，见文件头）`);
    return;
  }

  // 清单按**资产名**索引（release 里的资产是平铺的）。同名文件出现在两个子目录时，
  // 「这条摘要说的是哪一个」无从判定 —— 不可判定就必须红，不能挑一个继续。
  const dupes = names.filter((n, i) => names.indexOf(n) !== i);
  if (dupes.length > 0) {
    fail(`摘要门（U3）：出现同名资产 ${JSON.stringify([...new Set(dupes)])} —— 摘要按资产名索引，同名即不可判定`);
    return;
  }

  const text = readFileSync(pathOf(SHA256SUMS_NAME), 'utf8');
  const listed = new Map();
  const lines = text.split('\n');
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (line.trim() === '') continue;
    // `sha256sum` 的两种输出形态：文本模式两个空格、二进制模式 ` *`。两者都收。
    const m = /^([0-9a-f]{64}) [ *](.+)$/.exec(line);
    if (!m) {
      fail(`摘要门（U3）：${SHA256SUMS_NAME} 第 ${i + 1} 行不是 sha256sum 格式：${JSON.stringify(line)}`);
      return;
    }
    if (listed.has(m[2])) {
      fail(`摘要门（U3）：${SHA256SUMS_NAME} 里 '${m[2]}' 有重复条目 —— 取哪条不确定`);
      return;
    }
    listed.set(m[2], m[1]);
  }

  const assets = names.filter((n) => n !== SHA256SUMS_NAME);
  const missing = assets.filter((n) => !listed.has(n));
  const extra = [...listed.keys()].filter((n) => !assets.includes(n));
  if (missing.length > 0) {
    fail(
      `摘要门（U3）：${SHA256SUMS_NAME} 漏了 ${missing.length} 个资产 ${JSON.stringify(missing)}。\n` +
        `  漏掉的那个资产等于没随包发摘要 —— 生成步骤的过滤条件与实际产物集对不上。`
    );
  }
  if (extra.length > 0) {
    fail(
      `摘要门（U3）：${SHA256SUMS_NAME} 里有 ${extra.length} 条指向不存在的资产 ${JSON.stringify(extra)}。\n` +
        `  多半是上一轮的残留清单被当成本轮产物 —— 它会让「摘要齐了」这件事变成假的。`
    );
  }

  let checked = 0;
  for (const [name, want] of listed) {
    const p = pathOf(name);
    if (!p) continue; // 已由上面的 extra 报了。
    const got = createHash('sha256').update(readFileSync(p)).digest('hex');
    if (got !== want) {
      fail(
        `摘要门（U3）：'${name}' 的实际 sha256 与 ${SHA256SUMS_NAME} 不符。\n` +
          `  清单：${want}\n  实际：${got}\n` +
          `  清单是在产物落定**之后**生成的，对不上意味着两者之间还有一步在改产物（改名 / 重打 / 覆盖）。`
      );
    } else {
      checked++;
    }
  }
  if (missing.length === 0 && extra.length === 0 && checked === assets.length) {
    note(`摘要：${SHA256SUMS_NAME} 覆盖全部 ${checked} 个资产且逐条重算相符`);
  }
}

function checkAssets(label, dir, namesOnly = false) {
  const abs = resolve(ROOT, dir);
  if (!existsSync(abs)) {
    fail(`assets 模式：目录不存在 ${abs}`);
    return;
  }
  // --dir 指向文件时 readdirSync 会抛 ENOTDIR 裸栈（失败方向没错，但读不出所以然）。
  if (!statSync(abs).isDirectory()) {
    fail(`assets 模式：--dir 指向的不是目录：${abs}`);
    return;
  }
  // 命名契约按**文件名**判，体积/摘要两道内容门要按**路径**读 —— 故收全路径再投影出名字，
  // 而不是像原来那样只收名字（dist-release 下平铺与嵌套并存，名字回不去路径）。
  const paths = walk2(abs);
  const names = paths.map((p) => basename(p));
  const pathOf = (n) => paths.find((p) => basename(p) === n);
  if (names.length === 0) {
    fail(`assets 模式：${abs} 下没有任何文件`);
    return;
  }

  if (label === 'windows') {
    const cands = updaterWindowsCandidates(names);
    if (cands.length !== 1) {
      fail(
        `Windows updater 契约：应恰有 1 个「.exe 且名含 win」的产物，实为 ${cands.length} 个 ${JSON.stringify(cands)}。\n` +
          `  0 个 ⇒ find_suitable_update_asset 恒返回 None，用户永远收不到更新且静默；\n` +
          `  >1 个 ⇒ 选哪个取决于 release 资产顺序，不确定。\n` +
          `  全部产物：${JSON.stringify(names)}`
      );
    } else if (!cands[0].includes('setup')) {
      fail(`Windows updater 契约：唯一候选 '${cands[0]}' 不含 'setup'，安装态用户会被判成非安装器产物`);
    } else {
      note(`assets：windows → updater 唯一命中 '${cands[0]}'`);
    }
  } else if (label === 'macos-arm64' || label === 'macos-x64') {
    const mine = LABEL_TO_CORE[label]; // mac-arm64 / mac-x64
    const other = mine === 'mac-arm64' ? 'mac-x64' : 'mac-arm64';
    const cands = updaterMacCandidates(names, mine);
    if (cands.length !== 1) {
      fail(
        `macOS updater 契约：应恰有 1 个名含 '${mine}' 的 .dmg，实为 ${cands.length} 个 ${JSON.stringify(cands)}。\n` +
          `  0 个 ⇒ find_suitable_update_asset 对该架构恒返回 None，该架构用户永远收不到更新且静默\n` +
          `        （2026-07-21 起已取消「任意 .dmg」回落：宁可不更新，也不发错架构包）。\n` +
          `  >1 个 ⇒ 选哪个取决于 release 资产顺序，不确定。\n` +
          `  全部产物：${JSON.stringify(names)}`
      );
    }
    const wrong = updaterMacCandidates(names, other);
    if (wrong.length !== 0) {
      fail(`macOS updater 契约：本 job 不应产出名含 '${other}' 的 dmg，实为 ${JSON.stringify(wrong)}`);
    }
    if (cands.length === 1 && wrong.length === 0) note(`assets：${label} → updater 唯一命中 '${cands[0]}'`);
  } else if (label === 'release') {
    // 本分支自己的错误计数起点：末尾那句 note 只能在**本分支**没报错时打。
    // 读模块全局 `errors.length === 0` 今天碰巧对（release 是最后一项），
    // 一旦有别的检查排在它前面就静默失效。
    const errorsBefore = errors.length;
    // 聚合口径：四个 job 的产物汇到**同一个 release** 之后跑。
    // 与 per-job 口径的区别：per-job 断言「本 job 不得产出另一架构」，聚合侧两架构本就都在，
    // 故这里改断言「每个架构**恰好一个**」——少一个 = 该架构用户静默收不到更新
    // （github.rs 已取消跨架构回落），多一个 = updater 取首个命中，选谁看资产顺序。
    for (const archTag of ['mac-arm64', 'mac-x64']) {
      const cands = updaterMacCandidates(names, archTag);
      if (cands.length !== 1) {
        fail(
          `release 契约：名含 '${archTag}' 的 .dmg 应恰有 1 个，实为 ${cands.length} 个 ${JSON.stringify(cands)}。\n` +
            `  0 个 ⇒ 该架构用户 find_suitable_update_asset 恒 None，永远收不到更新且静默；\n` +
            `  >1 个 ⇒ updater 取首个命中，选谁取决于 release 资产顺序。`
        );
      }
    }
    const win = updaterWindowsCandidates(names);
    if (win.length !== 1) {
      fail(
        `release 契约：「.exe 且名含 win」应恰有 1 个，实为 ${win.length} 个 ${JSON.stringify(win)}。\n` +
          `  离线版故意不含 'win'（手动下载变体，不该被自动更新选中）——出现 >1 个说明该纪律被破坏。`
      );
    } else if (!win[0].includes('setup')) {
      fail(`release 契约：唯一 win 候选 '${win[0]}' 不含 'setup'，安装态用户会被判成非安装器产物`);
    }
    // Linux 两形态各自可选包（loose→AppImage / installed→deb），口径与 dmg / win setup 一致：**恰好一个**。
    //
    // 为什么是 ==1 而不是 >=1：`github.rs::find_suitable_update_asset` 的 Linux 分支是
    // `app_image.first()` / `deb.first()`（`crates/updater/src/github.rs:360-370`）——
    // 与 mac/win 同款「取首个命中」，>1 个时选谁**取决于 release 资产顺序**，不确定。
    // 这正是另外三类产物立 ==1 的理由，Linux 没有理由例外。且 linux 只有一条 matrix 腿、
    // 每种形态各产一个 ⇒ ==1 是真实状态，不会假红。
    for (const [ext, form] of [
      ['.deb', 'installed'],
      ['.AppImage', 'loose'],
    ]) {
      const got = names.filter((n) => n.endsWith(ext));
      if (got.length !== 1) {
        fail(
          `release 契约：\`${ext}\` 应恰有 1 个，实为 ${got.length} 个 ${JSON.stringify(got)}。\n` +
            `  0 个 ⇒ Linux ${form} 形态选不到包；\n` +
            `  >1 个 ⇒ updater 取首个命中（github.rs 的 \`${ext === '.deb' ? 'deb' : 'app_image'}.first()\`），选谁取决于资产顺序。`
        );
      }
    }

    // Windows 的另外两件交付物：updater 选不到它们（离线版故意不含 `win`；portable 不是 .exe），
    // 故上面的 updater 口径断言**一个都盖不到**——掉了照样全绿。它们是 README「Windows 双安装器」
    // 一节明写的交付物（离线版 = LTSC/内网场景的唯一出路），必须各自单独断言。
    const offline = names.filter((n) => n.endsWith('-offline-setup.exe'));
    if (offline.length !== 1) {
      fail(
        `release 契约：\`*-offline-setup.exe\`（WebView2 离线安装器）应恰有 1 个，实为 ${offline.length} 个 ${JSON.stringify(offline)}。\n` +
          `  0 个 ⇒ LTSC / 内网 / 无 WebView2 的用户没有可用安装包（bootstrapper 装不上）；\n` +
          `  >1 个 ⇒ 命名纪律已破，用户不知道该下哪个。`
      );
    }
    // 便携 zip：口径**就是 updater 的 loose 形态选包规则**（`updaterPortableCandidates`），
    // 不再是一条只问「有没有这个文件」的独立正则。两者今天等价，但把断言挂在 updater 口径上，
    // 选包判据一改这里就跟着改，不会出现「门还在绿、选包器已经选不到它」。
    //
    // 0 个 ⇒ 便携用户 `find_suitable_update_asset` 恒 None ⇒ 如实「无更新」（不再被推安装器，
    //        但也永远更新不了）；>1 个 ⇒ updater 取首个命中，选谁取决于 release 资产顺序。
    const portable = updaterPortableCandidates(names);
    if (portable.length !== 1) {
      fail(
        `release 契约：\`polaris-portable-*.zip\`（免安装绿色版 = updater loose 形态的唯一候选）应恰有 1 个，` +
          `实为 ${portable.length} 个 ${JSON.stringify(portable)}。\n` +
          `  0 个 ⇒ 便携用户恒收不到更新（github.rs 的 Windows loose 分支无回落，返回 None）；\n` +
          `  >1 个 ⇒ updater 取首个命中，选谁取决于 release 资产顺序。`
      );
    }
    // 注：两条 Windows 规则的「互不相交」**不在此断言** —— 判据是 `.zip` 与 `.exe` 两个互斥的
    // 后缀，同一个文件名不可能同时满足，写出来的检查恒为空、永远不可能转红。
    // 不可证伪的断言比没有断言更坏（它让人以为这条被守着），故这里只留这句说明。
    // 真正会变的是「判据本身被改宽」，那由 `updaterPortableCandidates` 与 github.rs 的
    // 逐字同口径 + 两侧各自的单测覆盖。

    // `--clobber` 失效时 GitHub 给同名资产追加 `.1` 后缀（`foo.dmg` + `foo.dmg.1` 并存）。
    // 这类重复项**逃过上面所有扩展名断言**（`foo.dmg.1` 不 endsWith('.dmg')），故单独查。
    const dupes = names.filter((n) => /\.\d+$/.test(n) && names.includes(n.replace(/\.\d+$/, '')));
    if (dupes.length > 0) {
      fail(
        `release 契约：出现 \`.N\` 后缀重复资产 ${JSON.stringify(dupes)} —— \`gh release upload --clobber\` 未生效。\n` +
          `  同一产物在 release 里存在两份，updater 命中哪个取决于资产顺序。`
      );
    }

    if (errors.length === errorsBefore) {
      note(`assets：release → 四平台命名契约成立（${names.length} 个资产）`);
    }
  } else if (label === 'linux') {
    const deb = names.filter((n) => n.endsWith('.deb'));
    const appimage = names.filter((n) => n.endsWith('.AppImage'));
    if (deb.length === 0) fail(`Linux updater 契约：缺 .deb（installed 形态选不到包）。全部产物：${JSON.stringify(names)}`);
    if (appimage.length === 0) fail(`Linux updater 契约：缺 .AppImage（loose 形态选不到包）。全部产物：${JSON.stringify(names)}`);
    if (deb.length > 0 && appimage.length > 0) note(`assets：linux → deb ${deb.length} 个 / AppImage ${appimage.length} 个`);
  } else {
    fail(`未知 label '${label}'，合法值：${Object.keys(LABEL_TO_CORE).join(', ')}, release`);
  }

  // ── 内容门（命名门之后跑：命名不成立时「哪个是 updater 目标」本身就不确定）──
  if (namesOnly) {
    // 如实标注跳过了什么。不打这条 note 的话，发布后那一遍看起来与内容门跑过的那遍一模一样。
    note(`assets：${label} → **仅命名口径**（--names-only）：喂进来的是同名空文件，体积门与摘要内容比对不可判定，已跳过`);
  } else {
    checkUpdateAssetSizes(label, updaterTargetNames(label, names), pathOf);
  }
  // 摘要门只挂**聚合口径**：`SHA256SUMS` 是四个 job 的产物汇进 dist-release 之后才生成的
  // （一个 release 一份，按资产名索引），per-job 目录里结构性不存在它 —— 在那儿断言它必然恒红。
  // `--names-only` 下仍验它**在场**（那一层用空文件也判得了），只跳过内容比对。
  if (label === 'release') checkSha256Sums(names, pathOf, namesOnly);
}

/** 递归收集文件**全路径**（命名契约用 `basename` 投影，内容门直接拿路径读）。 */
function walk2(dir, out = [], depth = 0) {
  if (depth > 8) return out;
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    if (e.isDirectory()) walk2(join(dir, e.name), out, depth + 1);
    else if (e.isFile()) out.push(join(dir, e.name));
  }
  return out;
}

// ───────────────────────── 入口 ─────────────────────────
function argOf(flag) {
  const i = process.argv.indexOf(flag);
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : null;
}

const mode = process.argv[2];
try {
  runMode();
} catch (e) {
  // 输入侧读取失败：转成一条**可读的**违反，而不是把 `node:fs` / `JSON.parse` 裸栈甩进 CI 日志。
  // 退出码不变（仍走下面的 errors.length > 0 → exit 1），只是让人看得出哪个文件、哪条不变量因此断不了。
  if (e instanceof InputError) fail(e.message);
  else throw e;
}

function runMode() {
switch (mode) {
  case 'confs':
    checkConfs();
    break;
  case 'payload': {
    // `--root` **必填，无默认值**：旧默认 `'target'` 会把整棵 target/ 树扫进来，
    // 命中的是 cargo build script 的 staging copy（`target/release/_up_/resources/`），
    // 与 bundler 是否铺进包无关 —— 正是本轮修掉的假绿。漏传一律硬失败，不回落到某个「差不多」的根。
    const root = argOf('--root');
    if (!root) {
      console.error(
        'payload 模式必须显式传 --root（bundle 根）：\n' +
          '  Linux/Windows: --root target/release/bundle\n' +
          '  macOS:         --root target/<triple>/release/bundle\n' +
          '  （windows 腿例外：NSIS 无 bundle 侧副本，传 target/release 作 staging 检查）'
      );
      process.exit(2);
    }
    checkPayload(argOf('--label'), root);
    break;
  }
  case 'assets':
    // `--names-only`：只跑命名口径（发布后那一遍喂的是同名空文件），见文件头。
    checkAssets(argOf('--label'), argOf('--dir') ?? '.', process.argv.includes('--names-only'));
    break;
  default:
    console.error(
      '用法: node scripts/verify-packaging.mjs <confs|payload|assets> [--label <label>] [--root <bundle 根>] [--dir <dir>] [--names-only]'
    );
    process.exit(2);
}
}

for (const n of notes) console.log(`ok: ${n}`);
if (errors.length > 0) {
  console.error(`\nFAILED: ${errors.length} 条打包不变量被违反：`);
  for (const e of errors) console.error(`  ✗ ${e}`);
  process.exit(1);
}
console.log(`ok: verify-packaging ${mode} 全部不变量成立。`);
