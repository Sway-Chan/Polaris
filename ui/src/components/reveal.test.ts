/**
 * 「展开即露出」的两道门。
 *
 * # 判据取自「这个交互必须成立什么」，不是「当初改了哪几个文件」
 *
 * 用户反馈的是节点表单的某一处折叠，但同一形状全仓有十处（六处 `.fld-fold`、三处自带样式的
 * `<details>`、一处非 details 的内联导入面板）。若把门写成「NodeDialog 里那两处用了 Fold」，
 * 下一个人新加一个折叠仍然默认是坏的，而门照绿。故门写成**全仓扫描**：
 * 每一个 `<details>` 都必须挂上 `onToggle` —— 这是「展开时有人管」的唯一结构证据。
 *
 * # 为什么不把门写成「都必须用 `<Fold>`」
 *
 * `rule-test-det` / `tun-details` / `us-notes` 三处有各自的类名与内联样式，逼它们改形去换取
 * 这个行为，是用视觉回归的风险买一致性。行为（露出）与形态（markup）是两件事，门只锁前者。
 */
import { describe, it, expect } from 'vitest';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import { computeRevealDelta } from './reveal';

// ---------------------------------------------------------------------------
// 1. 滚动量：纯几何，脱离 DOM 直接算
// ---------------------------------------------------------------------------

describe('computeRevealDelta', () => {
  const container = { top: 100, bottom: 500 };

  it('已经全看得见 → 不滚（无谓滚动比不滚更让人失去位置感）', () => {
    expect(computeRevealDelta({ top: 200, bottom: 400 }, container)).toBe(0);
  });

  it('底部恰好贴着容器下沿 → 不滚（边界不该触发）', () => {
    expect(computeRevealDelta({ top: 200, bottom: 500 }, container)).toBe(0);
  });

  it('溢出一点点 → 只滚溢出量', () => {
    expect(computeRevealDelta({ top: 200, bottom: 560 }, container)).toBe(60);
  });

  it('内容比容器还高 → 滚动量以 summary 顶部封顶，标题不会被顶出视口', () => {
    // 溢出 700，但顶部距容器顶只有 50 ⇒ 最多滚 50
    expect(computeRevealDelta({ top: 150, bottom: 1200 }, container)).toBe(50);
  });

  it('已经贴顶且仍然溢出 → 不滚（再滚就是把标题甩出去）', () => {
    expect(computeRevealDelta({ top: 100, bottom: 1200 }, container)).toBe(0);
  });

  it('元素整体在容器上方（异常输入）→ 不返回负数', () => {
    expect(computeRevealDelta({ top: -300, bottom: -100 }, container)).toBe(0);
  });
});

// ---------------------------------------------------------------------------
// 2. 全仓扫描：没有一个折叠是「展开了没人管」的
// ---------------------------------------------------------------------------

const SRC = fileURLToPath(new URL('..', import.meta.url));

function sourceFiles(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const full = join(dir, name);
    if (statSync(full).isDirectory()) {
      sourceFiles(full, out);
    } else if (name.endsWith('.tsx') && !name.includes('.test.')) {
      // 测试文件自己会在断言字符串里写 `<details class=…`，扫进来是假红。
      out.push(full);
    }
  }
  return out;
}

/**
 * 注释必须先剥掉：本门第一版扫到的唯一「违规」是 ListEditor 里一句**解释这道门的注释**
 * （「内联导入面板不是 <details>…」）—— 判据被自己写的散文污染，红得毫无信息量。
 */
function stripComments(src: string): string {
  return src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\n]*/g, '');
}

/**
 * 从 `<details` 截到它的 `<summary`：不能截到第一个 `>`，因为属性里的
 * `onToggle={(e) => …}` 自带 `>`，naive 截断会把 onToggle 切掉一半、判成没挂。
 */
function detailsTags(raw: string): string[] {
  const src = stripComments(raw);
  const tags: string[] = [];
  let i = src.indexOf('<details');
  while (i !== -1) {
    const end = src.indexOf('<summary', i);
    tags.push(src.slice(i, end === -1 ? i + 400 : end));
    i = src.indexOf('<details', i + 1);
  }
  return tags;
}

describe('展开即露出 —— 全仓不变量', () => {
  const files = sourceFiles(SRC);

  it('扫到的源文件数量合理（扫描器本身没瞎）', () => {
    expect(files.length).toBeGreaterThan(20);
  });

  it('每一个 <details> 都挂了 onToggle', () => {
    const offenders: string[] = [];
    for (const f of files) {
      for (const tag of detailsTags(readFileSync(f, 'utf8'))) {
        if (!tag.includes('onToggle')) offenders.push(`${f.slice(SRC.length)}: ${tag.trim().slice(0, 80)}`);
      }
    }
    expect(offenders, '这些 <details> 展开后不会把内容滚进视区，用户会以为点了没反应').toEqual([]);
  });

  it('确有 <details> 被扫到（防止上一条因为扫了个空集合而假绿）', () => {
    const total = files.reduce((n, f) => n + detailsTags(readFileSync(f, 'utf8')).length, 0);
    expect(total).toBeGreaterThanOrEqual(5);
  });

  it('扫描器会剥注释 —— 注释里提到 <details> 不该被算成违规（本门第一版就栽在这）', () => {
    expect(detailsTags('// 说明：<details> 是折叠元素\nconst a = 1;')).toEqual([]);
    expect(detailsTags('/* <details> 举例 */')).toEqual([]);
    // 但真标签仍要被扫到，剥注释不能顺手把射程剥没了
    expect(detailsTags('<details onToggle={x}><summary>t</summary></details>')).toHaveLength(1);
  });

  it('Fold 的 onToggle 里真的调了 revealOnToggle（不只是同步 state）', () => {
    const src = readFileSync(join(SRC, 'components/Fold.tsx'), 'utf8');
    expect(src).toMatch(/onToggle=\{[\s\S]*revealOnToggle\(e\)[\s\S]*\}/);
  });

  it('可滚菜单里的分组展开全部露出（.ns-grp / .csel-grp / .tray-group-h 同一形状）', () => {
    // 判据取「用了 openGroups 这个惯用法的组件」，不是「本次改了哪四个文件」：
    // 四个菜单都带 max-height + overflow-y:auto（.node-menu 430 / .mini-menu 360 /
    // .csel-menu 300 / .tray-menu 600），组头在底部时展开，新项目整段落在菜单视区之外。
    // 第五个人再照抄这个惯用法加一个分组菜单，忘了露出 → 本条转红。
    const offenders = files
      .filter((f) => readFileSync(f, 'utf8').includes('openGroups'))
      .filter((f) => !readFileSync(f, 'utf8').includes('revealSiblingGroup'))
      .map((f) => f.slice(SRC.length));
    expect(offenders, '这些分组菜单展开后新项目落在视区外').toEqual([]);
  });

  it('确有分组菜单被扫到（防止上一条在空集合上恒绿）', () => {
    const n = files.filter((f) => readFileSync(f, 'utf8').includes('openGroups')).length;
    expect(n).toBeGreaterThanOrEqual(4);
  });

  it('ListEditor 的内联导入面板也走同一条路（它不是 details，拿不到 toggle）', () => {
    const src = readFileSync(join(SRC, 'components/screens/settings/ListEditor.tsx'), 'utf8');
    expect(src).toContain('revealElement(importPanelRef.current)');
    expect(src).toMatch(/\}, \[importOpen\]\);/);
  });
});
