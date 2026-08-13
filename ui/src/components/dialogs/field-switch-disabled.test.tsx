/**
 * `FieldSpec` switch 的禁用态 —— 「可见但禁用 + 说明为什么」这条形态的门。
 *
 * # 为什么这道门是**渲染门**（本仓少数几道之一）
 *
 * vitest 是 `environment:'node'`、无 jsdom，但 `renderToStaticMarkup` 只要 React 本身，不需要 DOM
 * （既有先例：`components/screens/settings/terminal-env-and-fold.test.tsx`）。所以「开关渲染出来没有」
 * 「带没带 `disabled`」「hint 换没换」这三件事是**能真测的**，不必退化成源码 grep。
 * 测不到的是几何与交互（真机才有）——本门不碰那两样。
 *
 * # 守什么
 *
 * WARP 的 System 接入模式此前在两个弹窗里的形态是「隐藏」：`WarpDialog` 干脆不渲染，
 * `WgDialog` 用 `when: v => !isWarpDraft(v, base)` 整条滤掉。不能开是对的（WARP 走 system 内核接口
 * 会与主 TUN 抢 utun ⇒ `Connect: resource busy` FATAL，真机实证见 `domain/warp.ts`），
 * **但隐藏且不解释**会让用户分不清「不支持」「没做」「藏在别处」——本仓正在系统性消除这种形态。
 *
 * 三条不变式：
 *  1. 禁用的开关**仍然渲染**（可见），不是被滤掉；
 *  2. 它带原生 `disabled` ⇒ 点击事件根本不派发 ⇒ `onChange` 结构上不可达（不是「拦得住就好」）；
 *  3. 禁用时显示的是**为什么不能开**，不是那条描述「开启后会怎样」的常态 hint。
 *
 * # 抓不到什么
 *
 *  - `.swt:disabled` 的视觉（不透明度/光标）—— CSS 不在渲染射程内，真机看。
 *  - **两个弹窗真实的运行时取值**：`WarpDialog` / `WgDialog` 在 node 环境 import 即炸
 *    （`document is not defined`，模块加载期就有依赖碰 DOM），所以 `advSpec()` / `wgSpec()` 的返回值
 *    测不到。下面第二组退而求其次，用**源码结构门**钉那两张表里的 `reverseMesh` 项 —— 见该组的自述。
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { renderToStaticMarkup } from 'react-dom/server';
import { FieldRenderer, type FieldSpec } from './field-spec';

const SPEC: FieldSpec = {
  t: 'switch',
  k: 'reverseMesh',
  label: 'wg.reverseMesh',
  zh: 'System 接入模式（内核接口）',
  hint: 'wg.reverseMeshHint',
  hintZh: '建真实内核接口……',
  disabledHint: 'wg.reverseMeshWarp',
  disabledHintZh: 'WARP 不支持 System 接入模式：会与主 TUN 抢内核接口。',
};

/** i18n 未初始化时 `t(key, default)` 回落到 default，故断言打在中文缺省上。 */
const render = (spec: FieldSpec, value: unknown = false) =>
  renderToStaticMarkup(<FieldRenderer spec={spec} value={value as never} onChange={() => {}} />);

describe('switch 禁用态：可见但不可写，且说明为什么', () => {
  it('自检：常态（未禁用）确实渲染出一个可用开关 + 常态 hint —— 阴性对照', () => {
    const html = render(SPEC);
    expect(html).toContain('role="switch"');
    expect(html).not.toContain('disabled');
    expect(html).toContain('建真实内核接口');
    expect(html).not.toContain('WARP 不支持');
  });

  it('不变式1+2：禁用时仍然渲染，且带原生 disabled（onChange 结构上不可达）', () => {
    // 牙：把 `disabled={off}` 删掉 → 开关可点 → 红。
    const html = render({ ...SPEC, disabled: true });
    expect(html).toContain('role="switch"');
    expect(html).toContain('disabled');
    // 「可见」= 标签还在。若哪天有人改回 `when` 那种整条滤掉的写法，标签会一起消失。
    expect(html).toContain('System 接入模式');
  });

  it('不变式3：禁用时 hint 换成「为什么不能开」，常态 hint 不再显示', () => {
    // 牙：把 hint 的三元换成恒取 spec.hint → 用户读到的是一条拨不动的开关「拨动后会怎样」→ 红。
    const html = render({ ...SPEC, disabled: true });
    expect(html).toContain('WARP 不支持');
    expect(html).not.toContain('建真实内核接口');
  });

  it('禁用但没给 disabledHint → 退回常态 hint（不留空白，也不吞掉说明）', () => {
    const { disabledHint: _k, disabledHintZh: _v, ...noDisabledHint } = SPEC as Extract<
      FieldSpec,
      { t: 'switch' }
    >;
    const html = render({ ...noDisabledHint, disabled: true });
    expect(html).toContain('disabled');
    expect(html).toContain('建真实内核接口');
  });

  it('禁用与「开/关」正交：已开启的开关被禁用时，aria-checked 仍如实报 true', () => {
    // 存量 reverseMesh:true 的 WARP 节点（导入/手改/迁移）打开弹窗时就是这一态 ——
    // 界面必须如实显示「它现在是开的、而你不能改」，不能假装是关的。
    const html = render({ ...SPEC, disabled: true }, true);
    expect(html).toContain('aria-checked="true"');
    expect(html).toContain('disabled');
  });
});

/**
 * 两张 FieldSpec 表里的 `reverseMesh` 项 —— **源码结构门，不是行为门**。
 *
 * # 为什么只能是结构门
 *
 * 理想做法是 import `advSpec()` / `wgSpec()` 直接看返回值。做不到：这两个函数住在
 * `WarpDialog.tsx` / `WgDialog.tsx` 里，而那两个模块在 node 环境**加载期**就炸
 * （`document is not defined`）。搬进纯 `.ts` 逻辑模块能解决加载问题，但**那样是错的** ——
 * `contracts/protocol-settings-coverage.test.ts` 刻意只把这两个 `.tsx` 算作「编辑器」，
 * 判据就是「FieldSpec 表里有没有这一项」＝**有没有控件**；把表搬走等于把那道门架空。
 * 故这里读源码。它证明不了运行时真的传了 `disabled`，只证明表里那一项是这么写的。
 *
 * # 为什么值得有
 *
 * 变异实测：把 `WarpDialog` 那条的 `disabled: true` 删掉 —— tsc 绿（少写可选属性合法）、
 * build 绿、覆盖门绿（它只问 `k: 'reverseMesh'` 在不在）、上面那组渲染门也绿（它测的是渲染器，
 * 喂的是合成 spec）。**全仓没有任何门会红**，WARP 的开关就这么变回可点的了。本组就是补这个洞。
 *
 * # 剔注释是承重步骤
 *
 * 本文件与那两个 dialog 的注释里反复出现 `disabled` / `when` / `隐藏` 等字样。不剔注释的话，
 * 把代码删干净、注释留着，门照样报绿（同款教训见 `protocol-settings-coverage.test.ts` 的
 * `stripComments` 文档）。
 */
describe('FieldSpec 表里的 reverseMesh 项：可见但禁用（源码结构门）', () => {
  const read = (f: string) => readFileSync(fileURLToPath(new URL(f, import.meta.url)), 'utf8');
  const stripComments = (s: string) => s.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\n]*/g, '');

  /** 取包含 `k: 'reverseMesh'` 的那个对象字面量（从该处向两边配对花括号）。 */
  function specEntry(src: string): string {
    const body = stripComments(src);
    const at = body.indexOf("k: 'reverseMesh'");
    expect(at, "源码里找不到 `k: 'reverseMesh'` —— 解析失效或控件没了，两种都必须转红").toBeGreaterThan(-1);
    let start = at;
    for (let d = 0; start > 0; start--) {
      if (body[start] === '}') d++;
      else if (body[start] === '{') {
        if (d === 0) break;
        d--;
      }
    }
    let end = at;
    for (let d = 0; end < body.length; end++) {
      if (body[end] === '{') d++;
      else if (body[end] === '}') {
        if (d === 0) break;
        d--;
      }
    }
    const entry = body.slice(start, end + 1);
    expect(entry, '花括号配对失败 —— 解析器失效').toContain("k: 'reverseMesh'");
    return entry;
  }

  const warp = specEntry(read('./WarpDialog.tsx'));
  const wg = specEntry(read('./WgDialog.tsx'));

  it('自检：两个弹窗里都解析到了这一项，且都是 switch', () => {
    for (const [name, e] of [['WarpDialog', warp], ['WgDialog', wg]] as const) {
      expect(e, `${name} 解析到的不是 switch 项`).toContain("t: 'switch'");
      expect(e.length, `${name} 解析出的片段太短，配对可能出错`).toBeGreaterThan(60);
    }
  });

  it('WARP：恒禁用（disabled: true 写死，不是条件）', () => {
    // 牙：删掉 `disabled: true` → 红。这正是全仓其它门都抓不到的那个变异。
    expect(warp).toMatch(/\bdisabled:\s*true\b/);
  });

  it('WG：按 WARP 判据禁用（不是写死 true —— 普通 WG 节点必须还能开）', () => {
    // 牙：改成 `disabled: true` → 普通 WG 也禁了 → 红；删掉 → 红。
    expect(wg).toMatch(/\bdisabled:\s*isWarpDraft\(/);
    expect(wg).not.toMatch(/\bdisabled:\s*true\b/);
  });

  it('两处都给了 disabledHint（禁用而不解释 = 本轮要消除的那个形态）', () => {
    for (const [name, e] of [['WarpDialog', warp], ['WgDialog', wg]] as const) {
      expect(e, `${name} 的 reverseMesh 禁用了却没说为什么`).toMatch(
        /disabledHint:\s*'wg\.reverseMeshWarp'/
      );
      expect(e, `${name} 缺 zh 缺省`).toMatch(/disabledHintZh:\s*'/);
    }
  });

  it('两处都**不得**再用 when 把它整条隐掉（隐藏是这次要改掉的旧形态）', () => {
    // 牙：把 `disabled: …` 换回 `when: (v) => !isWarpDraft(v, base)` → 红。
    for (const [name, e] of [['WarpDialog', warp], ['WgDialog', wg]] as const) {
      expect(e, `${name} 的 reverseMesh 又被 when 隐藏了`).not.toMatch(/\bwhen:\s*\(/);
    }
  });
});
