/**
 * G12 —— 更新弹窗「后端动作白名单 ↔ 渲染端按钮」双向对拍。
 *
 * # 守什么
 *
 * 后端 `crates/updater/src/popup.rs` 用 `PopupAction::is_valid_for(phase)` 定义了每个阶段允许哪些动作，
 * 非法动作**静默忽略**（对齐上游语义，见该函数注释）。渲染端 `ui/src/update-popup/main.ts` 按阶段画按钮。
 * 两侧各写各的，于是有两类只能静态查出来的死角：
 *
 *  - **孤儿动作**：白名单允许、渲染端没有入口。后端可以有完整单测、永远绿，能力却用户不可达。
 *    `viewLog` 曾是活样本（`popup.rs` 里枚举 + `is_non_resolving` + 白名单 + 三条单测俱全，
 *    `main.ts` 的 `data-act` 全集里零命中）。**单向对拍结构性看不见这一条** —— 从后端往前看它是「已实现」，
 *    从前端往后看它根本不存在。
 *  - **死按钮**：渲染端发得出、白名单拒收。用户点了 / 按了键，什么都不会发生，也没有任何报错。
 *    Esc 曾无条件发 `close` 就撞在这上面（progress / remind 两态皆拒）。
 *
 * 上面两条到 2026-07-29 都已在实现侧修掉（remind 加了 `viewLog` 按钮；Esc 与角标关闭钮改走
 * `exitActionFor(phase)`）。**本门因此不是「记账」而是「防复发」**：两张登记表现在是空的，
 * 任一侧再长出孤儿 / 死按钮都会转红。空表不等于空转 —— 差集算子本身在自检段用合成样本正反两向证过。
 *
 * 这是「后端枚举 × 前端按钮」这类配对的**通用模板**：判据面是两份源码里的集合，抓的是集合差。
 *
 * # 判据面
 *
 *  - 后端：`crates/updater/src/popup.rs` 的 `pub enum PopupAction` 变体表 + `is_valid_for` 的 match 臂。
 *  - 前端：`ui/src/update-popup/main.ts` 里 `render()` 各 `case '<phase>':` 分支渲染出的 `data-act` 集合，
 *    外加两条**动作不写死在 `data-act` 里**的腿 —— Esc 键与角标关闭钮，二者的动作都取自
 *    `ui/src/update-popup/exit-action.ts` 的 `exitActionFor(phase)`。漏掉它们会把可达集算少、孤儿算多。
 *
 * # 为什么是源码结构守卫
 *
 * 本仓 vitest 是 `environment:'node'`（无 jsdom，有意为之）⇒ 渲染不了页面；而 Rust 侧的白名单单测跑在
 * cargo 里，两边天然不在同一个进程。「集合差」恰好是纯文本可判的那一层，且正是缺陷所在的那一层。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = fileURLToPath(new URL('../../..', import.meta.url));
const POPUP_RS = join(REPO, 'crates', 'updater', 'src', 'popup.rs');
const MAIN_TS = join(REPO, 'ui', 'src', 'update-popup', 'main.ts');
const EXIT_TS = join(REPO, 'ui', 'src', 'update-popup', 'exit-action.ts');

const RS = readFileSync(POPUP_RS, 'utf8');
const TS = readFileSync(MAIN_TS, 'utf8');
const EXIT = readFileSync(EXIT_TS, 'utf8');

// ── 解析器 ──────────────────────────────────────────────────────────────────

/** `ManualDownload` → `manualDownload`（= `#[serde(rename_all = "camelCase")]` 的产物）。 */
function camel(variant: string): string {
  return variant.charAt(0).toLowerCase() + variant.slice(1);
}

/** `pub enum PopupAction { … }` 的变体表（跳注释行）。 */
function parseActionEnum(rs: string): string[] {
  const at = rs.indexOf('pub enum PopupAction {');
  if (at < 0) return [];
  const body = rs.slice(at, rs.indexOf('\n}', at));
  return [...body.matchAll(/^\s{4}([A-Z]\w*),/gm)].map((m) => camel(m[1]));
}

/** `is_valid_for` 的 `PopupPhase::X => matches!(self, Self::A | Self::B)` 全部臂。 */
function parseWhitelist(rs: string): Record<string, string[]> {
  const at = rs.indexOf('pub fn is_valid_for(');
  if (at < 0) return {};
  const body = rs.slice(at, rs.indexOf('\n    }', at));
  const out: Record<string, string[]> = {};
  for (const m of body.matchAll(/PopupPhase::(\w+)\s*=>\s*matches!\(([^)]*)\)/g)) {
    out[m[1].toLowerCase()] = [...m[2].matchAll(/Self::(\w+)/g)].map((v) => camel(v[1]));
  }
  return out;
}

/** `render()` 里各 `case '<phase>':` 分支渲染出的 `data-act` 集合。 */
function parseRendered(ts: string): Record<string, string[]> {
  const at = ts.indexOf('function render(');
  if (at < 0) return {};
  const body = ts.slice(at, ts.indexOf('\n}', at));
  const out: Record<string, string[]> = {};
  const marks = [...body.matchAll(/case '(\w+)':|^\s{4}default:/gm)];
  for (let i = 0; i < marks.length; i++) {
    const name = marks[i][1];
    if (!name) continue; // default 分支单独断言，见下
    const seg = body.slice(marks[i].index, marks[i + 1]?.index ?? body.length);
    out[name] = [...seg.matchAll(/data-act="(\w+)"/g)].map((m) => m[1]);
  }
  return out;
}

/**
 * 「退出弹窗」在各阶段发哪个动作 —— `exit-action.ts` 的 `exitActionFor`。
 *
 * 它同时是**两条腿**的动作源：Esc 键（`main.ts` 的 keydown）与角标关闭钮（`closeButton(phase)`），
 * 两者都不是写死的 `data-act` 字面量 ⇒ 只扫 `data-act="..."` 会漏掉它们，把可达动作算少、
 * 把孤儿算多。故单独解析。返回 `phase → action`，`*` 为兜底分支。
 */
function parseExitActions(exitSrc: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const m of exitSrc.matchAll(/if \(phase === '(\w+)'\) return '(\w+)';/g)) out[m[1]] = m[2];
  const fallback = /\n  return '(\w+)';/.exec(exitSrc);
  if (fallback) out['*'] = fallback[1];
  return out;
}

const exitFor = (phase: string, map: Record<string, string>): string | undefined =>
  map[phase] ?? map['*'];

/** 某阶段的渲染块里是否挂了角标关闭钮（它的 `data-act` 由 `exitActionFor(phase)` 决定）。 */
function hasCloseButton(ts: string, phase: string): boolean {
  const at = ts.indexOf('function render(');
  const body = ts.slice(at, ts.indexOf('\n}', at));
  const marks = [...body.matchAll(/case '(\w+)':/g)];
  const i = marks.findIndex((m) => m[1] === phase);
  if (i < 0) return false;
  const seg = body.slice(marks[i].index, marks[i + 1]?.index ?? body.length);
  return /\$\{closeButton\(/.test(seg);
}

/** 某阶段用户实际够得着的动作全集 = 字面量按钮 ∪ 角标关闭钮 ∪ Esc。 */
function reachableIn(phase: string): Set<string> {
  const s = new Set(RENDERED[phase] ?? []);
  const exit = exitFor(phase, EXIT_ACTIONS);
  if (exit) {
    s.add(exit); // Esc 恒可用（无条件生效，只是发什么按 phase 映射）
    if (hasCloseButton(TS, phase)) s.add(exit);
  }
  return s;
}

/** 白名单 ↔ 可达集的双向差。抽成函数是为了能在合成样本上证明它真的会报（见自检段）。 */
function actionDiff(
  whitelist: Record<string, string[]>,
  reachable: (phase: string) => Set<string>,
): { orphans: string[]; dead: string[] } {
  const orphans: string[] = [];
  const dead: string[] = [];
  for (const [phase, allowed] of Object.entries(whitelist)) {
    const r = reachable(phase);
    for (const a of allowed) if (!r.has(a)) orphans.push(`${phase}::${a}`);
    for (const a of r) if (!allowed.includes(a)) dead.push(`${phase}::${a}`);
  }
  return { orphans: orphans.sort(), dead: dead.sort() };
}

const ACTIONS = parseActionEnum(RS);
const WHITELIST = parseWhitelist(RS);
const RENDERED = parseRendered(TS);
const EXIT_ACTIONS = parseExitActions(EXIT);

// ── 自曝：任一解析器返回空 = 后面全部集合差恒为空集 = 这道门不存在 ──

if (ACTIONS.length === 0) {
  throw new Error(`[update-popup-action-parity] 解析不出 PopupAction 变体表（${POPUP_RS}）`);
}
if (Object.keys(WHITELIST).length === 0) {
  throw new Error(`[update-popup-action-parity] 解析不出 is_valid_for 的白名单（${POPUP_RS}）`);
}
if (Object.keys(RENDERED).length === 0) {
  throw new Error(`[update-popup-action-parity] 解析不出 render() 的 data-act 集合（${MAIN_TS}）`);
}
if (Object.keys(EXIT_ACTIONS).length === 0 || !EXIT_ACTIONS['*']) {
  throw new Error(
    `[update-popup-action-parity] 解析不出 exitActionFor 的 phase → 动作映射（${EXIT_TS}）—— ` +
      'Esc 与角标关闭钮的动作源没了，可达集会算少、孤儿会算多',
  );
}
if (!/sendAction\(exitActionFor\(currentPhase\)\)/.test(TS)) {
  throw new Error(
    `[update-popup-action-parity] Esc 不再走 exitActionFor —— 逃生腿换了形态，请一并改本守卫的判据（${MAIN_TS}）`,
  );
}
for (const [phase, acts] of Object.entries(WHITELIST)) {
  if (acts.length === 0) {
    throw new Error(`[update-popup-action-parity] 阶段 ${phase} 的白名单解析成空集 —— 解析器塌了`);
  }
}

// ── 登记表：已知的孤儿 / 死按钮 ─────────────────────────────────────────────

/**
 * 已知孤儿（白名单有、用户够不着）。**逐条待修，不是豁免。**
 * 修好（渲染端加上入口）之后本表必须一起改小 —— 下面的断言是 `toEqual`，只降不升都会说话。
 */
const KNOWN_ORPHANS: Readonly<Record<string, string>> = {
  // 空 = 当前零孤儿。`remind::viewLog` 曾在此登记，2026-07-29 渲染端补上按钮后出表。
};

/**
 * 已知死按钮（渲染端发得出、白名单拒收 ⇒ 静默无反应）。**逐条待修，不是豁免。**
 */
const KNOWN_DEAD: Readonly<Record<string, string>> = {
  // 空 = 当前零死按钮。`remind::close` / `progress::close` 曾在此登记（Esc 恒发 close 被白名单拒），
  // 2026-07-29 Esc 与角标关闭钮改走 `exitActionFor(phase)` 后出表。
};

// ── 断言 ────────────────────────────────────────────────────────────────────

describe('守卫自检：解析器与差集算子都真的会报（防空集合让集合差恒为空）', () => {
  it('枚举 / 白名单解析器在合成 Rust 片段上命中', () => {
    const rs = [
      'pub enum PopupAction {',
      '    /// 注释行不该被当成变体',
      '    Alpha,',
      '    BetaGamma,',
      '}',
      '    pub fn is_valid_for(self, phase: PopupPhase) -> bool {',
      '        match phase {',
      '            PopupPhase::Remind => matches!(self, Self::Alpha | Self::BetaGamma),',
      '            PopupPhase::Done => matches!(self, Self::Alpha),',
      '        }',
      '    }',
    ].join('\n');
    expect(parseActionEnum(rs)).toEqual(['alpha', 'betaGamma']);
    expect(parseWhitelist(rs)).toEqual({ remind: ['alpha', 'betaGamma'], done: ['alpha'] });
  });

  it('渲染端 / 退出动作解析器在合成 TS 片段上命中', () => {
    const ts = [
      'function render(state) {',
      '  switch (state.phase) {',
      "    case 'remind':",
      '      root.innerHTML = `<button data-act="skip">x</button><button data-act="update">y</button>`;',
      '      break;',
      "    case 'done':",
      '      root.innerHTML = `<div/>`;',
      '      break;',
      '    default:',
      '      root.innerHTML = `<button data-act="close">z</button>`;',
      '  }',
      '}',
    ].join('\n');
    expect(parseRendered(ts)).toEqual({ remind: ['skip', 'update'], done: [] });

    const exit = [
      'export function exitActionFor(phase) {',
      "  if (phase === 'progress') return 'cancel';",
      "  if (phase === 'remind') return 'later';",
      "  return 'close';",
      '}',
    ].join('\n');
    expect(parseExitActions(exit)).toEqual({ progress: 'cancel', remind: 'later', '*': 'close' });
  });

  /**
   * **两张登记表现在都是空的**，`toEqual([])` 若没有这条就成了「解析器塌了也照样绿」。
   * 这里拿合成的白名单 / 可达集正反两向证明差集算子真的会报。
   */
  it('差集算子正反两向都判得动（空登记表不等于空转）', () => {
    const clean = actionDiff({ remind: ['a', 'b'] }, () => new Set(['a', 'b']));
    expect(clean).toEqual({ orphans: [], dead: [] });

    const broken = actionDiff({ remind: ['a', 'b'], done: ['c'] }, (p) =>
      p === 'remind' ? new Set(['a', 'zz']) : new Set(),
    );
    expect(broken.orphans, '白名单有而够不着的没被报成孤儿').toEqual(['done::c', 'remind::b']);
    expect(broken.dead, '够得着而白名单拒的没被报成死按钮').toEqual(['remind::zz']);
  });

  it('真判据面非空（上面的合成样本过了不代表真文件也解析得动）', () => {
    expect(ACTIONS.length).toBeGreaterThanOrEqual(6);
    expect(Object.keys(WHITELIST).sort()).toEqual(['done', 'error', 'progress', 'remind']);
    expect(Object.keys(RENDERED).sort()).toEqual(['done', 'error', 'progress', 'remind']);
    // 四态的角标关闭钮都真的挂上了（原型 :2948 `.ut-x` 四态恒在）——少一个，那一态的可达集就少一条腿。
    for (const p of ['remind', 'progress', 'done', 'error']) {
      expect(hasCloseButton(TS, p), `${p} 态没有角标关闭钮`).toBe(true);
    }
  });
});

describe('G12：动作白名单 ↔ 渲染端按钮双向对拍', () => {
  it('两侧阶段集合相等（后端加了新 phase 而前端没跟 ⇒ 转红）', () => {
    // 变异对照：给 is_valid_for 加一条 `PopupPhase::Foo => …` → 本条转红。
    expect(Object.keys(RENDERED).sort()).toEqual(Object.keys(WHITELIST).sort());
  });

  it('渲染端画出的每个 data-act 都是真实存在的 PopupAction（否则后端 serde 直接判「未知弹窗动作」）', () => {
    // 变异对照：把 main.ts 的 data-act="retry" 改成 data-act="retryy" → 本条转红。
    const bogus = [
      ...Object.values(RENDERED).flat(),
      ...Object.values(EXIT_ACTIONS),
    ].filter((a) => !ACTIONS.includes(a));
    expect([...new Set(bogus)], 'data-act 不在 PopupAction 枚举里').toEqual([]);
  });

  it('孤儿动作（白名单有、渲染端无入口）与登记表逐条相等', () => {
    // 变异对照：把 main.ts remind 态的 data-act="skip" 删掉 → 多出 `remind::skip` → 转红。
    expect(
      actionDiff(WHITELIST, reachableIn).orphans,
      '孤儿动作与登记表不符 —— 见 KNOWN_ORPHANS 头注',
    ).toEqual(Object.keys(KNOWN_ORPHANS).sort());
  });

  it('死按钮（渲染端发得出、白名单拒收）与登记表逐条相等', () => {
    // 变异对照：把 popup.rs 的 `PopupPhase::Error => matches!(self, Retry | ManualDownload | Close)`
    // 里的 `Close` 删掉 → 多出 `error::close` → 转红。
    expect(
      actionDiff(WHITELIST, reachableIn).dead,
      '死按钮与登记表不符 —— 见 KNOWN_DEAD 头注',
    ).toEqual(Object.keys(KNOWN_DEAD).sort());
  });

  it('孤儿 / 死按钮的登记理由都写清了，且不许伪装成豁免', () => {
    for (const [k, why] of [...Object.entries(KNOWN_ORPHANS), ...Object.entries(KNOWN_DEAD)]) {
      expect(why.length, `${k} 的理由太短`).toBeGreaterThan(20);
      expect(why, `${k} 的理由是自我循环`).not.toMatch(/本仓先例|既有惯例|历来如此/);
      expect(why, `${k} 不是豁免，理由须以「待修」起头`).toMatch(/^待修/);
    }
  });

  it('未知 phase 的兜底分支仍在，且带逃生按钮（不留空白窗 —— #300 的形态）', () => {
    // 这条不属于阶段对拍（default 不是真 phase），但它是本页最后一道逃生腿，删了没人会知道。
    const at = TS.indexOf('function render(');
    const def = TS.slice(TS.indexOf('default:', at), TS.indexOf('\n}', at));
    expect(def, 'default 兜底分支没了或不再画逃生按钮').toMatch(/data-act="close"/);
  });
});
