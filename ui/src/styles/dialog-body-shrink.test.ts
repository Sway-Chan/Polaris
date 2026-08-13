/**
 * 弹窗滚动区不得压扁 clipped 子项 —— **CSS cascade 结构门，不是渲染门**。
 *
 * # 先把射程说清楚（别把绿当成「真机上折叠段展得开」）
 *
 * 本仓 vitest 是 `environment:'node'`（vite.config.ts），**无 jsdom / 无 CSSOM / 无排版引擎**。
 * 这道门做的事只有一件：把四个 CSS 文件按 `index.css` 里的 `@import` 顺序拼起来，解析出某个
 * 选择器在层叠之后的**声明值**，再对这些值下断言。它能回答「`.fld-fold` 在层叠后 flex-shrink 是几」，
 * **回答不了**「真机上折叠段展开后能不能滚」。后者只有真机能定，交接里已单列。
 * 因此本门的定位是**回归门**：把已经诊断清楚的那条不变式钉住，防它被后来的改动无声撤销。
 *
 * # 守的不变式
 *
 * `.dlg-body` 是 `.dlg`（`max-height:calc(100vh - 40px)` 的纵向 flex）里唯一 `flex:1 1 auto` 的一段，
 * 自带 `overflow-y:auto`。内容超高时**先跑 flex 收缩、后出滚动条**，而按 CSS Flexbox §4.5，
 * 子项的自动最小尺寸在 `overflow` 非 `visible` 时**塌成 0** ⇒ 那类子项会把整份负余量一个人吃光、
 * 被压到 0，再被自己的 `overflow:hidden` 把内容连同 `summary` 一起裁掉（陈先生真机看到的那条横线）。
 * 根治手法是覆盖层的一句 `.dlg-body > *{ flex-shrink:0 }`（index.css，写在全部 `@import` 之后）。
 *
 * # 五把锁
 *
 *  1. **解析器自检**：四个文件读得到、`@import` 顺序解析得出、`.dlg-body` 找得到。任一失败必须
 *     抛错转红 —— 「读不到就跳过」会让「没检查」与「检查通过」的输出不可区分 = 没有这道门。
 *  2. **前提仍在**：`.dlg-body` 仍是「会收缩 + 自带滚动」的纵向 flex。哪天它不再是（比如改成
 *     `flex:none` 或去掉 `overflow-y`），本门守的不变式就失去意义，此时必须**转红提醒重审**，
 *     而不是继续绿着守一条已经没有对象的规则。
 *  3. **缺陷前提仍在**：`.fld-fold` 的层叠赢家仍带非 visible 的 `overflow`。若哪天它变成 visible，
 *     自动最小尺寸自然回到内容高，保护规则就成了死规则 —— 同样该转红重审，不该沉默。
 *  4. **保护生效**：全仓已知的 4 类 clipped 直接子项（`.fld-fold` / `.cat-list` / `.proc-pick-list` /
 *     `.parse-list`）在层叠后 `flex-shrink` 均为 0，且没有任何后续规则给 `.dlg-body` 的直接子项
 *     重新发放非 0 的 shrink（防「有人开了豁免口又忘了补下限」）。
 *  5. **阳性对照**：把 `index.css` 覆盖层从输入里摘掉，同一套解析器必须判定 `.fld-fold` **可被压扁**。
 *     没有这一条，锁 4 只是「谓词恒假」的同义反复 —— 分不清「保护规则起了作用」与「这些类本来就
 *     不会被压」。配套阴性对照：`.fld`（overflow 默认 visible）在有无覆盖层两种输入下都不算缺陷。
 *  6. **落位**：那条规则必须写在 index.css 全部 `@import` **之后**。今天它不写在后面也照样生效
 *     （`prototype.css` 的 `.fld-fold` 压根没声明 flex/flex-shrink，无从反压），所以这条锁**不是**
 *     在守当下的正确性，是在守本门解析模型的前提 —— `cascadeSources` 恒把 index.css 排在最后。
 *     哪天有人给 `.fld-fold` 补一条 `flex:…`，落位就立刻变成承重的，而那时本门已经钉住了它。
 *
 * # 抓不到什么（如实记）
 *
 *  - **真实几何**：内容到底有没有超高、超多少、滚动条出没出，全在射程外（无排版引擎）。
 *  - **子项清单的完整性**：锁 4 的四个类是人工从 `components/dialogs/*.tsx` 数出来的，本门不解析 JSX。
 *    新加一个带 `overflow:hidden` 的直接子项，本门**不会**发现它 —— 但 `.dlg-body > *` 是通配选择器，
 *    新子项自动被保护，故漏检的后果止于「清单陈旧」，不是「缺陷复发」。
 *  - **特异性打架的一般情形**：解析器只按「同选择器字符串、后来者胜」算，不实现完整特异性算法。
 *    锁 5 的阳性对照是对这个简化的兜底 —— 它验的是端到端结论而不是解析器内部。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = (f: string) => fileURLToPath(new URL(f, import.meta.url));

const INDEX = readFileSync(here('./index.css'), 'utf8');

/** 从 index.css 现场解析 `@import` 顺序 —— 不写死文件名，改了顺序本门跟着变。 */
function importOrder(css: string): string[] {
  return [...css.matchAll(/@import\s+['"]\.\/([\w.-]+\.css)['"]/g)].map((m) => m[1]);
}

/** 层叠输入 = 按 @import 顺序的各文件 + 最后的 index.css 自身（覆盖层）。 */
function cascadeSources(opts: { withOverride: boolean }): string {
  const parts = importOrder(INDEX).map((f) => readFileSync(here(`./${f}`), 'utf8'));
  if (opts.withOverride) parts.push(INDEX);
  return parts.join('\n');
}

function stripComments(css: string): string {
  return css.replace(/\/\*[\s\S]*?\*\//g, '');
}

/**
 * 取一组选择器在层叠后的声明值（后来者胜）。选择器按**字符串精确匹配**规则头里的任一逗号项，
 * 故 `.dlg-body` 不会被 `.dlg-body .row2` 误命中。
 *
 * 传**一组**而不是一个，是因为保护规则与被保护类是两个不同的选择器：`.fld-fold` 的 shrink 由
 * `.dlg-body > *` 那条通配规则决定。两者特异性都是 (0,1,0)（各一个类；`>` 与 `*` 均不加权），
 * 平手 ⇒ 纯按源码顺序，而覆盖层在最后一个 `@import` 之后 ⇒ 通配规则胜。故本函数只按源码序取最后一条，
 * 无需实现完整特异性算法；这个简化由锁 5 的阳性对照兜底。
 */
function declOf(css: string, selectors: readonly string[], prop: string): string | undefined {
  const body = stripComments(css);
  let winner: string | undefined;
  for (const m of body.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const heads = m[1].split(',').map((s) => s.trim().replace(/\s+/g, ' '));
    if (!heads.some((h) => selectors.includes(h))) continue;
    for (const d of m[2].split(';')) {
      const i = d.indexOf(':');
      if (i < 0) continue;
      if (d.slice(0, i).trim() !== prop) continue;
      winner = d.slice(i + 1).trim();
    }
  }
  return winner;
}

/**
 * 在层叠后的 `flex-shrink` —— 同时认 `flex` 简写与长写，两者取源码序更靠后的那一条。
 * `flex:none` ⇒ 0；`flex:1 1 auto` ⇒ 1；`flex:1` ⇒ 1（单值 = flex-grow，shrink 取初始值 1）。
 * 缺省（谁都没写）⇒ 1，即「会被压」。
 */
function shrinkOf(css: string, selectors: readonly string[]): number {
  const body = stripComments(css);
  const at = (prop: string) => {
    let last = -1;
    for (const m of body.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
      const heads = m[1].split(',').map((s) => s.trim().replace(/\s+/g, ' '));
      if (!heads.some((h) => selectors.includes(h))) continue;
      if (new RegExp(`(^|;)\\s*${prop}\\s*:`).test(m[2])) last = m.index ?? last;
    }
    return last;
  };
  const shortAt = at('flex');
  const longAt = at('flex-shrink');
  if (shortAt < 0 && longAt < 0) return 1; // 初始值
  if (longAt > shortAt) return Number(declOf(css, selectors, 'flex-shrink'));
  const v = (declOf(css, selectors, 'flex') ?? '').trim();
  if (v === 'none') return 0;
  if (v === 'auto' || v === 'initial') return 1;
  const parts = v.split(/\s+/);
  return parts.length >= 2 ? Number(parts[1]) : 1;
}

/** overflow 是否让自动最小尺寸塌成 0（Flexbox §4.5：非 visible 即塌）。 */
function autoMinCollapses(css: string, selectors: readonly string[]): boolean {
  const all = declOf(css, selectors, 'overflow');
  const y = declOf(css, selectors, 'overflow-y');
  const v = y ?? all;
  return v !== undefined && v.trim() !== 'visible';
}

/**
 * 缺陷谓词：作为 `.dlg-body` 的**直接子项**时 clipped 且仍可收缩 ⇒ 会被压扁到 0、内容不可达。
 * overflow 只看这个类自己（通配规则不设 overflow）；shrink 要把通配保护一起算进来。
 */
function squashable(css: string, selector: string): boolean {
  return (
    autoMinCollapses(css, [selector]) && shrinkOf(css, [selector, UNIVERSAL_CHILD]) !== 0
  );
}

/** 覆盖层里那条通配保护的选择器（本门反复引用，抽成常量防拼写漂移）。 */
const UNIVERSAL_CHILD = '.dlg-body > *';

/** `.dlg-body` 的直接子项里、CSS 上带非 visible overflow 的全部类（人工清点，见文件头「抓不到什么」）。 */
const CLIPPED_CHILDREN = [
  '.fld-fold', // TsSettingsDialog / NodeDialog ×2 / SubDialog / WarpDialog —— 本次真机缺陷现场
  '.cat-list', // ResCatalogDialog
  '.proc-pick-list', // ProcPickDialog / RulePickDialog
  '.parse-list', // ImportDialog（在 fragment 里，仍是 .dlg-body 的直接 flex 子项）
] as const;

describe('弹窗滚动区的直接子项不得被压扁（CSS cascade 结构门）', () => {
  const css = cascadeSources({ withOverride: true });

  it('锁1 解析器自检：@import 顺序解析得出、各文件非空、.dlg-body 找得到', () => {
    const order = importOrder(INDEX);
    expect(order.length, '@import 一条都没解析到 —— 解析器失效，必须转红').toBeGreaterThan(0);
    expect(order).toContain('components.css');
    expect(order).toContain('prototype.css');
    // prototype.css 必须在最后（本门的「后来者胜」依赖这个前提，style-invariants 另有一条同款锁）
    expect(order[order.length - 1]).toBe('prototype.css');
    expect(css.length).toBeGreaterThan(10000);
    expect(declOf(css, ['.dlg-body'], 'display'), '.dlg-body 解析不到 display').toBeDefined();
  });

  it('锁2 前提：.dlg-body 仍是「会收缩 + 自带滚动」的纵向 flex（不再是则本门须重审）', () => {
    expect(declOf(css, ['.dlg-body'], 'display')).toBe('flex');
    expect(declOf(css, ['.dlg-body'], 'flex-direction')).toBe('column');
    expect(declOf(css, ['.dlg-body'], 'overflow-y')).toBe('auto');
    // 它自己必须是可收缩项（否则 .dlg 溢出、头尾也跟着被推走，是另一个缺陷）
    expect(shrinkOf(css, ['.dlg-body'])).not.toBe(0);
    expect(declOf(css, ['.dlg-body'], 'min-height')).toBe('0');
    // 头尾必须仍是 flex:none —— 它们不固定的话「只滚 body」这个前提就没了
    expect(shrinkOf(css, ['.dlg-head'])).toBe(0);
    expect(shrinkOf(css, ['.dlg-foot'])).toBe(0);
  });

  it('锁3 缺陷前提仍在：.fld-fold 的层叠赢家仍是 clipped（变 visible 则保护成死规则，须重审）', () => {
    expect(autoMinCollapses(css, ['.fld-fold'])).toBe(true);
    expect(declOf(css, ['.fld-fold'], 'overflow')).toBe('hidden');
  });

  it('锁4 保护生效：已知 clipped 直接子项在层叠后 flex-shrink 均为 0', () => {
    const bad = CLIPPED_CHILDREN.filter((s) => squashable(css, s));
    expect(bad, `这些子项仍会被 .dlg-body 压扁到 0：${bad.join(', ')}`).toEqual([]);
    // 通配保护本身必须在（且赢层叠）——上面那条对每个具体类都成立，这条钉住它来自同一条通配规则
    expect(shrinkOf(css, [UNIVERSAL_CHILD])).toBe(0);
  });

  it('锁4b 没有后来的规则给 .dlg-body 的直接子项重新发放非 0 shrink', () => {
    const body = stripComments(css);
    const offenders: string[] = [];
    for (const m of body.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
      for (const head of m[1].split(',').map((s) => s.trim().replace(/\s+/g, ' '))) {
        if (!/^\.dlg-body\s*>\s*/.test(head)) continue;
        const s = shrinkOf(css, [head, UNIVERSAL_CHILD]);
        if (s !== 0) offenders.push(`${head} → flex-shrink:${s}`);
      }
    }
    expect(offenders, `.dlg-body 的直接子项被重新开放了收缩：${offenders.join('; ')}`).toEqual([]);
  });

  it('锁5 阳性对照：摘掉 index.css 覆盖层后，同一套解析器必须判定 .fld-fold 会被压扁', () => {
    const without = cascadeSources({ withOverride: false });
    expect(
      squashable(without, '.fld-fold'),
      '没有覆盖层时本门也说「没问题」⇒ 它测的不是那条修复，是同义反复'
    ).toBe(true);
    // 阴性对照：普通 .fld（overflow 默认 visible）在两种输入下都不算缺陷 —— 谓词不是恒真
    expect(squashable(css, '.fld')).toBe(false);
    expect(squashable(without, '.fld')).toBe(false);
  });

  it('锁6 落位：保护规则写在 index.css 全部 @import 之后（本门解析模型的前提）', () => {
    const lastImport = [...INDEX.matchAll(/@import\s+['"][^'"]+['"]\s*;/g)]
      .map((m) => (m.index ?? 0) + m[0].length)
      .reduce((a, b) => Math.max(a, b), -1);
    expect(lastImport, '一条 @import 都没找到 —— 解析失效').toBeGreaterThan(0);
    const at = stripComments(INDEX).indexOf(UNIVERSAL_CHILD);
    expect(at, `index.css 里找不到 ${UNIVERSAL_CHILD} 规则`).toBeGreaterThan(-1);
    expect(at).toBeGreaterThan(lastImport);
  });
});
