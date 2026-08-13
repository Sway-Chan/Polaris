/**
 * i18n locale parity 门 —— 防「翻译缺口无声增长」与「字符串父级被当命名空间寻址」两类复发。
 *
 * 为什么需要这道门（根因）：
 *  - 缺口能长到 480 键，是因为全仓从未有任何一致性断言：加 en-US 键不同步其它语种，CI 恒绿。
 *  - `rules.region` 在 ru/fa 曾是字符串、在 en/zh 是对象 `{cn,ir,ru}`。消费点 `t('rules.region.cn', '中国大陆')`
 *    在字符串上无法继续寻址 → 该语种解析失败。同型缺陷此前已出现过一次（`rules.regionRouting.sub`，
 *    已改扁平键 `regionRoutingSub` 修掉），属**复发**，故单列一条形态门。
 *
 * 三层断言：
 *  1. 结构自检（非空）—— 读到 0 个 locale 文件必须转红，而不是 0 个用例恒绿空转。
 *  2. 键集：zh-CN/zh-TW 对 en-US 严格全等；ru/fa 走**精确棘轮**（见下）。
 *  3. 形态：所有语种在共有键上 string/object 必须一致（B1 那类 bug 的根治门）。
 *  4. 可寻址性：代码里每个 `t('a.b.c')` 字面量键，在每个语种里都不得被「字符串父级」挡住。
 *
 * 棘轮为什么用**精确相等**而非 `<=`：
 *   `<=` 挡不住「有人把基线调高来消音」——477 <= 600 照样绿。精确相等两个方向都会说话：
 *     · 实际缺口 > 基线 → 新增键没同步，补键或补译；
 *     · 实际缺口 < 基线 → 债务已下降，把基线一起调低（只许降不许升）。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const LOCALES_DIR = fileURLToPath(new URL('./locales', import.meta.url));
const SRC_DIR = fileURLToPath(new URL('..', import.meta.url));

/** 基准语种：键集与形态的唯一真值源。 */
const REFERENCE = 'en-US';

/** 必须与 en-US 严格全等的语种（当前缺口为 0，无债务，直接严格断言）。 */
const STRICT_LOCALES = ['zh-CN', 'zh-TW'] as const;

/**
 * 已知债务上限 —— **只许降不许升**。
 *
 * ru/fa 的翻译常规不同步（handoff 记为单独议题，不在本门射程内补齐）。这里把当前缺口数钉死为基线：
 * 新增 en-US 键而未同步 ru/fa 会让缺口变大 → 转红；补译让缺口变小 → 也转红，提示把基线调低。
 * 任何一方向的改动都必须显式改这个常量，杜绝无声漂移。
 *
 * 2026-07-21 实测基线（`rules.region` 形态修复 + 死键 `nodes.subAutoUpdateDisabledHint` 清理后）= 477/477。
 * 2026-07-28 下调：rules 角标/重排/地区卡一批新键补齐时，顺手把 ru/fa 已缺的 `rules.*`
 * （name/namePh/errNoCond/combineMode/priority/yourRegion/backHome 等）一并补译 → 缺口下降；
 * 同批清掉 fa 的死键 `connections.colSource`（en-US/zh-CN/zh-TW/ru 早已删，仅 fa 漏删）。
 *
 * 2026-07-29 再下调 467→465：暂存层 P4 的 18 个 `home.pending*` 新键**五语种同批补齐**（缺口不变），
 * 同时删掉 `home.pendingApplied` / `home.pendingDeferred` —— spec §2.5 Q8 定死 `applied`/`deferred`
 * 只表达「排程成功」，不再报「已应用」，这两句话没有了消费点。它们此前只在 en/zh 三语存在，
 * 删掉即让基准少 2 键 ⇒ ru/fa 缺口 -2。
 *
 * 2026-07-29 再下调 465→461：破坏性操作确认收口第二批。6 个新键（`home.pendingResetConfirm` /
 * `resources.resetBuiltinConfirm` / `resources.deleteConfirmPlain` / `resources.deleteConfirmRefd` /
 * `rules.deleteConfirmAgain` / `settings.core.rollbackConfirm`）**五语种同批补齐**（缺口不变），
 * 同时删掉 4 个随确认弹窗一起消失的键（`rules.deleteTitle` / `rules.deleteMsg` /
 * `appPolicy.removeConfirmTitle` / `appPolicy.removeConfirmMsg`）—— 它们此前只在 en/zh 三语存在，
 * 删掉即让基准少 4 键 ⇒ ru/fa 缺口 -4。
 */
const MISSING_KEY_DEBT: Record<string, number> = {
  // 2026-07-29：461 → 460。本轮撤除 `flashHot`（切节点/切直连/跨屏高亮三处浮标全部不移植）与
  // 应用分流「实验性」徽章，连带删掉 `home.hotSwitchFlash` / `home.flashInterceptHint` /
  // `appPolicy.experimental` 三个键。zh-CN 少了键 ⇒ ru/fa 的缺口自然降一格 —— 是删键降的，
  // 不是补译降的，故上限跟着降但**不代表翻译进度有推进**。
  //
  // 2026-07-30：460 → 457。弹窗提交级错误条改右下角 toast 这一批，三笔各降 1：
  //   ①②**删死键** `rules.saveFail`（「保存失败，可重试：」前缀，改 toast 后标题走 `common.saveFailed`）
  //      与 `rules.pickFromProcesses`（规则弹窗「从进程选择」按钮整个删掉，行内勾选区已取代它）——
  //      两键此前只在 en/zh 三语存在，删掉即让基准少 2 键 ⇒ ru/fa 缺口 -2（**删键降的，非补译**）；
  //   ③ **补译** `rules.invalidHead`：它从「拼在错误条前面的前缀」变成 toast 标题（值同时去掉尾部
  //      冒号），既然还活着就必须五语齐 ⇒ ru/fa 各补一条，缺口 -1（**这一笔是真的补译**）。
  // 同批新增的 `ts.loginFailed` / `import.failed` 五语同批补齐 ⇒ 缺口不变（ru/fa 此前**整个**
  // `ts` / `import` 命名空间都不存在，故它们在这两份里是新建命名空间的首条）。
  //
  // 2026-07-30：457 → 456。资源库页脚按钮文案 `resCatalog.downloadSelected`（「下载选中」）改名为
  // `resCatalog.download`（「下载」）—— 选中数已由紧随其后的 `(N)` 表达，标题里再写一次「选中」是
  // 同义重复（真机反馈）。**这一笔是补译**：旧键此前只在 en/zh 三语存在（ru/fa 各缺 1），改名时
  // 顺手五语补齐 ⇒ ru/fa 各 -1。基准键总数不变（一进一出），故不是「删键降的」。
  //
  // 2026-07-31：456 → 455。测速进度改全局 sticky toast 这一批**删死键** `nodes.testing`
  //（「测速中 {{tested}}/{{total}}」）—— 节点页那行屏内进度文本被删（判据见
  // `NodesScreen.tsx` 的 `runSpeedTest` 上方），该键随之零消费点。它此前只在 en/zh 三语存在，
  // 删掉即让基准少 1 键 ⇒ ru/fa 缺口 -1（**删键降的，非补译**）。
  // 同批的进度 toast 四条文案**零新增键**：全部复用 `nodes.speedTest{ingNodes,Done,Interrupted,
  // InterruptedSummary}` —— 这四个键 1:1 移植时译文就已五语齐备，只是一直没有消费点。
  //
  // 2026-07-31：455 → 451。错误 toast 改按 `errorCode` 取键这一批（`domain/proxy-error-text.ts`），
  // **四条全是补译**（基准键总数不变）：`home.proxyCrashed` / `home.proxyMisdirected` /
  // `home.ruleResourcesMissing` 此前只在 en/zh 三语存在 —— 它们正是被后端中文 `message` 压了一年、
  // 从未真正渲染过的那三个死键，现在成了这条路径的唯一文案源，必须五语齐；
  // `home.pendingApplyFailed`（「应用失败」不带原因的那句）是三段式第 3 段的落点，同批补齐。
  //
  // 2026-08-07：451 → 450。**一笔真补译**：`common.optional`（「可选」徽标）此前只在
  // en-US/zh-CN/zh-TW 三语存在，ru/fa 各缺 1 —— 于是这两个语种在 **52 个 `opt:true` 字段** 与
  // `SubDialog.tsx` 的手写徽标上，渲染的都是 `field-spec.tsx` 里那个 zh 默认「可选」，
  // 即中文。基准键总数不变（一个键都没新增/删除），纯粹是 ru/fa 各补一条 ⇒ 缺口 -1。
  //
  // 同批的 `node.field.{h2Host,h2Method,h2Headers,secretKeys}Hint` 四个新键**五语同批补齐**
  // ⇒ 缺口不变（en-US 多 4 键、ru/fa 也各多 4 键）。那四条是把塞在标签里的整句说明拆进 hint，
  // 标签只留字段名（见 `node-spec.ts` 的 h2 段与 `styles/text-fit.test.ts` 的 `.fld-l` 2 行预算）。
  //
  // 2026-08-07：450 → 430。**删键降的，不是补译降的** —— 全仓 592 条死键（声明了但没有任何
  // 消费点）随 `i18n-coverage` 的 G6b 清零一并删除，其中 20 条恰是 ru/fa 也没有的，
  // 基准少 592 键 ⇒ 缺口 −20。剩下这 430 条条条影响界面，是真正待补的那部分。
  //
  // 2026-08-09：430 → **0，债务清零**。那 430 条（ru/fa 缺的是同一批）全部补译落地 ——
  // 在此之前俄语/波斯语用户有 **28% 的界面回落成英文**（`fallbackLng: 'en-US'`），
  // 而这道门只是把这个数字钉住、从不逼它下降。
  // 产出方式与它的**唯一真门**：六个译者分批产出 + 按语种终校，但译文质量不是本门能保证的；
  // 落盘前过的是一道机械校验（键集恰好等于缺口集、占位符 `{{…}}` 与 en-US 逐字一致、非空、
  // 无 RTL 方向控制符），该校验自身用五种坏数据变异验过。终校两轮共改 28 条，
  // 绝大多数是**跨批术语漂移**（同一个英文词被不同译者译成两三种说法），
  // 其中两条是「提示语里引号引用的控件名与真实 UI 文案对不上」—— 用户照着提示找不到那个控件。
  //
  // 🔴 **清零后本门即零容忍**：任何新增 en-US 键不同步 ru/fa 都会当场转红（与 zh 两语同档）。
  // 这正是本表设计时要的终局：把「翻译常规不同步」从一个被默许的常态变成一次构建失败。
  ru: 0,
  fa: 0,
};

/**
 * 可寻址性门的已知豁免 —— 字符串父级挡住子键，**修法在组件侧**（不在本文件射程）。
 *
 * 2026-07-21：**当前为空，零豁免**。原先登记的两条（`nodes.empty.filtered` / `rules.backHome.tip`）
 * 已按 `rules.regionRoutingSub` 的先例改扁平键（`nodes.emptyFiltered` / `rules.backHomeTip`）并在
 * 5 份 locale 补齐，故从表中摘除。
 *
 * 注意：豁免只对「已登记的具体键」生效，新出现的同型键一律转红。再次登记前先确认真的修不了 ——
 * 这两条的经验是：全语种命中（含 en-US）时 fallbackLng 救不了，所有语言的用户都会看到硬编码中文默认值。
 */
const UNADDRESSABLE_DEBT = new Set<string>([]);

type Shape = 'object' | 'string' | 'other';
type Json = Record<string, unknown>;

/** 拍平成 `路径 → 形态`，中间节点也入表（形态断言要看 object/string 的分叉点）。 */
function flatten(obj: Json, prefix = '', out: Map<string, Shape> = new Map()): Map<string, Shape> {
  for (const [k, v] of Object.entries(obj)) {
    const path = prefix ? `${prefix}.${k}` : k;
    if (v !== null && typeof v === 'object' && !Array.isArray(v)) {
      out.set(path, 'object');
      flatten(v as Json, path, out);
    } else {
      out.set(path, typeof v === 'string' ? 'string' : 'other');
    }
  }
  return out;
}

/**
 * 辅助 webview 分区（`locales/auxiliary/`）—— 托盘浮层与更新弹窗的 `tray.*` / `updatePopup.*`。
 *
 * 为什么单独一份文件而不是并进主 locale：Rollup 的 chunk 粒度是**模块**、不是导出。主窗要整份
 * locale，辅助窗只要几十条；只要两边 import 同一个 `.json`，那 537 kB 就会落进它们**共享**的 chunk
 * （实测：辅助窗 preload 的共享 chunk 从 9.6 kB 涨到 544 kB）。分区后两边 import 图不再相交，
 * 辅助窗只付自己那 12 kB。
 *
 * **它不是第二个真值源**：同一个键不会同时存在于两边（下面有断言），且本门把两边**合并**后
 * 一起做键集/形态/棘轮判定 —— 分区在打包上分家，在门面前是一份。
 */
const AUX_DIR = join(LOCALES_DIR, 'auxiliary');

function loadLocales(): Map<string, Json> {
  const files = readdirSync(LOCALES_DIR).filter((f) => f.endsWith('.json'));
  if (files.length === 0) throw new Error(`${LOCALES_DIR} 下一个 .json 都没有 —— 目录改名了？`);
  return new Map(
    files.map((f) => {
      const name = f.replace(/\.json$/, '');
      const main = JSON.parse(readFileSync(join(LOCALES_DIR, f), 'utf-8')) as Json;
      // 读不到 aux 分区必须**抛**而不是跳过：静默跳过就等于辅助窗的键又一次逃出所有门。
      const aux = JSON.parse(readFileSync(join(AUX_DIR, f), 'utf-8')) as Json;
      for (const k of Object.keys(aux)) {
        if (k in main) throw new Error(`${name}: 命名空间 "${k}" 在主分区与 aux 分区同时存在 —— 分区必须互斥`);
      }
      return [name, { ...main, ...aux }];
    })
  );
}

const locales = loadLocales();
const flat = new Map([...locales].map(([name, data]) => [name, flatten(data)]));

/** 只取叶子（真正要翻译的条目）；中间 object 节点由形态门单独管。 */
function leaves(name: string): Set<string> {
  return new Set([...flat.get(name)!].filter(([, s]) => s !== 'object').map(([k]) => k));
}

// ---------------------------------------------------------------------------
// 1. 结构自检：这道门自己不能空转
// ---------------------------------------------------------------------------
describe('locale 门自检（非空）', () => {
  it('至少读到基准语种在内的多个 locale 文件', () => {
    // 读到 0 个文件（目录改名/路径写错）时，下面所有 for-of 断言都会静默 0 次执行 —— 恒绿假门。
    expect(locales.size).toBeGreaterThan(0);
    expect([...locales.keys()]).toContain(REFERENCE);
    expect(locales.size).toBeGreaterThanOrEqual(2);
  });

  it('基准语种键量在合理量级（防读到空对象/截断文件）', () => {
    expect(leaves(REFERENCE).size).toBeGreaterThan(1000);
    for (const name of locales.keys()) {
      expect(leaves(name).size, `${name} 键量异常偏低`).toBeGreaterThan(1000);
    }
  });

  it('可寻址性扫描确实扫到了源码（防扫 0 个文件恒绿）', () => {
    expect(tKeys.size).toBeGreaterThan(100);
  });
});

// ---------------------------------------------------------------------------
// 2. 键集：严格语种 + 棘轮语种
// ---------------------------------------------------------------------------
describe('locale 键集 parity', () => {
  const ref = leaves(REFERENCE);

  for (const name of STRICT_LOCALES) {
    it(`${name} 与 ${REFERENCE} 键集严格全等`, () => {
      const cur = leaves(name);
      const missing = [...ref].filter((k) => !cur.has(k)).sort();
      const extra = [...cur].filter((k) => !ref.has(k)).sort();
      expect(missing, `${name} 缺键（补译后加进 ${name}.json）`).toEqual([]);
      expect(extra, `${name} 多键（en-US 没有 → 死键，删掉）`).toEqual([]);
    });
  }

  for (const [name, debt] of Object.entries(MISSING_KEY_DEBT)) {
    it(`${name} 缺口精确等于已知债务上限 ${debt}（只许降不许升）`, () => {
      expect([...locales.keys()], `${name}.json 不存在`).toContain(name);
      const cur = leaves(name);
      const missing = [...ref].filter((k) => !cur.has(k));
      expect(
        missing.length,
        missing.length > debt
          ? `${name} 缺口从 ${debt} 涨到 ${missing.length}：新增键未同步，请补键或补译`
          : `${name} 缺口已降到 ${missing.length}：请把 MISSING_KEY_DEBT.${name} 一并调低`
      ).toBe(debt);
    });

    it(`${name} 无多余死键（en-US 没有的键）`, () => {
      const extra = [...leaves(name)].filter((k) => !ref.has(k)).sort();
      expect(extra, `${name} 存在 en-US 没有的键 → 死键`).toEqual([]);
    });
  }
});

// ---------------------------------------------------------------------------
// 3. 形态一致性（B1 根治门）—— 所有语种，无豁免
// ---------------------------------------------------------------------------
describe('locale 类型形态一致性', () => {
  const ref = flat.get(REFERENCE)!;

  for (const name of locales.keys()) {
    if (name === REFERENCE) continue;
    it(`${name} 与 ${REFERENCE} 在共有键上形态一致（string vs object）`, () => {
      const cur = flat.get(name)!;
      const mismatch: string[] = [];
      for (const [key, shape] of cur) {
        const refShape = ref.get(key);
        if (refShape !== undefined && refShape !== shape) {
          mismatch.push(`${key}: ${REFERENCE}=${refShape} ${name}=${shape}`);
        }
      }
      // 形态不一致 = 消费点 t('x.y.z') 在某些语种寻址失败（B1：ru/fa 的 rules.region 曾是字符串）。
      expect(mismatch.sort(), `${name} 形态与基准分叉`).toEqual([]);
    });
  }
});

// ---------------------------------------------------------------------------
// 4. 可寻址性：t('a.b.c') 不得被字符串父级挡住
// ---------------------------------------------------------------------------

/** 递归收集 src 下的 .ts/.tsx（含本文件所在目录以外的全部前端源码）。 */
function collectSources(dir: string, acc: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    if (entry === 'node_modules' || entry === 'dist') continue;
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) collectSources(full, acc);
    else if (/\.tsx?$/.test(entry)) acc.push(full);
  }
  return acc;
}

/** 抽出所有 `t('a.b.c')` 字面量键（动态拼接的键抽不到，本门只覆盖静态键）。 */
function extractTKeys(): Map<string, string> {
  const re = /\bt\(\s*(['"])([A-Za-z0-9_.]+)\1/g;
  const found = new Map<string, string>();
  for (const file of collectSources(SRC_DIR)) {
    const text = readFileSync(file, 'utf-8');
    const lines = text.split('\n');
    lines.forEach((line, i) => {
      for (const m of line.matchAll(re)) {
        if (!found.has(m[2])) found.set(m[2], `${file}:${i + 1}`);
      }
    });
  }
  return found;
}

const tKeys = extractTKeys();

/**
 * 复刻 i18next 的 deepFind 语义：每层从短到长尝试拼接段，故 `{"empty.filtered": "..."}` 这类
 * 扁平键也算可寻址（已对 i18next 23.16.8 实测验证）。返回 true 表示结构上能走到底。
 */
function isAddressable(node: unknown, parts: string[]): boolean {
  if (parts.length === 0) return typeof node === 'string';
  if (node === null || typeof node !== 'object') return false;
  const obj = node as Json;
  for (let j = 1; j <= parts.length; j++) {
    const candidate = parts.slice(0, j).join('.');
    if (candidate in obj && isAddressable(obj[candidate], parts.slice(j))) return true;
  }
  return false;
}

/** 键在该语种里「不存在」（可接受，走 fallbackLng）还是「被字符串父级挡住」（bug）。 */
function blockedBy(data: Json, key: string): string | null {
  const parts = key.split('.');
  let cur: unknown = data;
  for (let i = 0; i < parts.length; i++) {
    if (cur === null || typeof cur !== 'object') return parts.slice(0, i).join('.');
    cur = (cur as Json)[parts[i]];
    if (cur === undefined) return null; // 纯缺键，不是形态问题
  }
  return null;
}

describe('t() 键可寻址性（字符串父级不得被当命名空间）', () => {
  for (const name of locales.keys()) {
    it(`${name}: 无「字符串父级挡住子键」的消费点`, () => {
      const data = locales.get(name)!;
      const blocked: string[] = [];
      for (const [key, where] of tKeys) {
        if (UNADDRESSABLE_DEBT.has(key)) continue;
        if (isAddressable(data, key.split('.'))) continue;
        const parent = blockedBy(data, key);
        if (parent !== null) blocked.push(`${key}（父 "${parent}" 是字符串）@ ${where}`);
      }
      // 修法：把消费点的键改扁平命名（先例 rules.regionRoutingSub），或让该父级在所有语种都是对象。
      expect(blocked.sort(), `${name} 存在被字符串父级挡住的键`).toEqual([]);
    });
  }

  it('已登记的豁免键仍然真实存在（防豁免表变僵尸）', () => {
    // 豁免表为空时下面的 for-of 会 0 次执行。这里**只是提前返回**，不再放一句
    // `expect([...UNADDRESSABLE_DEBT]).toEqual([])` —— 那句写在 `if (size === 0)` 里面，
    // 断言的正是它刚分支过的字面量，必然通过，是条自证的僵尸断言（复审 #13）。
    // 「当前零豁免」这个事实由本注释记录即可，不该伪装成一道门。
    if (UNADDRESSABLE_DEBT.size === 0) return;
    // 组件侧改扁平命名后这条会转红，提示把 UNADDRESSABLE_DEBT 里的条目删掉。
    for (const key of UNADDRESSABLE_DEBT) {
      expect(tKeys.has(key), `豁免键 ${key} 已无消费点，请从 UNADDRESSABLE_DEBT 删除`).toBe(true);
      expect(
        isAddressable(locales.get(REFERENCE)!, key.split('.')),
        `豁免键 ${key} 在 ${REFERENCE} 已可寻址，请从 UNADDRESSABLE_DEBT 删除`
      ).toBe(false);
    }
  });
});
