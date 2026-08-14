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
 * WARP 的 System 接入模式不能开启（WARP 走 system 内核接口会与主 TUN 抢 utun ⇒
 * `Connect: resource busy` FATAL，真机实证见 `domain/warp.ts`）。专用 WARP 表单只展示适用字段，
 * 因此不渲染这一项；通用 WireGuard 编辑器仍可能打开 WARP 存量配置，必须展示禁用项并解释原因。
 *
 * 三条不变式：
 *  1. 禁用的开关**仍然渲染**（可见），不是被滤掉；
 *  2. 它带原生 `disabled` ⇒ 点击事件根本不派发 ⇒ `onChange` 结构上不可达（不是「拦得住就好」）；
 *  3. 禁用时显示的是**为什么不能开**，不是那条描述「开启后会怎样」的常态 hint。
 *
 * # 抓不到什么
 *
 *  - `.swt:disabled` 的视觉（不透明度/光标）—— CSS 不在渲染射程内，真机看。
 *  - `WgDialog` 真实的运行时取值：该模块在 node 环境 import 即炸（`document is not defined`，
 *    模块加载期就有依赖碰 DOM），所以 `wgSpec()` 的返回值测不到。下面第二组用源码结构门钉职责边界。
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
  hint: 'wg.reverseMeshHint',
  disabledHint: 'wg.reverseMeshWarp',
};

/** i18n 未初始化时返回键名；本组只验证结构切换，译文完整性由 i18n coverage 负责。 */
const render = (spec: FieldSpec, value: unknown = false) =>
  renderToStaticMarkup(<FieldRenderer spec={spec} value={value as never} onChange={() => {}} />);

describe('switch 禁用态：可见但不可写，且说明为什么', () => {
  it('自检：常态（未禁用）确实渲染出一个可用开关 + 常态 hint —— 阴性对照', () => {
    const html = render(SPEC);
    expect(html).toContain('role="switch"');
    expect(html).not.toContain('disabled');
    expect(html).toContain('wg.reverseMeshHint');
    expect(html).not.toContain('wg.reverseMeshWarp');
  });

  it('不变式1+2：禁用时仍然渲染，且带原生 disabled（onChange 结构上不可达）', () => {
    // 牙：把 `disabled={off}` 删掉 → 开关可点 → 红。
    const html = render({ ...SPEC, disabled: true });
    expect(html).toContain('role="switch"');
    expect(html).toContain('disabled');
    // 「可见」= 标签还在。若哪天有人改回 `when` 那种整条滤掉的写法，标签会一起消失。
    expect(html).toContain('wg.reverseMesh');
  });

  it('不变式3：禁用时信息提示换成「为什么不能开」，常态说明不再显示', () => {
    // 牙：把 hint 的三元换成恒取 spec.hint → 用户读到的是一条拨不动的开关「拨动后会怎样」→ 红。
    const html = render({ ...SPEC, disabled: true });
    expect(html).toContain('wg.reverseMeshWarp');
    expect(html).not.toContain('wg.reverseMeshHint');
    expect(html).toContain('class="info-i"');
    expect(html).not.toContain('fld-hint');
  });

  it('禁用但没给 disabledHint → 退回常态 hint（不留空白，也不吞掉说明）', () => {
    const { disabledHint: _k, ...noDisabledHint } = SPEC as Extract<
      FieldSpec,
      { t: 'switch' }
    >;
    const html = render({ ...noDisabledHint, disabled: true });
    expect(html).toContain('disabled');
    expect(html).toContain('wg.reverseMeshHint');
  });

  it('禁用与「开/关」正交：已开启的开关被禁用时，aria-checked 仍如实报 true', () => {
    // 通用 WireGuard 编辑器遇到存量 reverseMesh:true 的 WARP 节点时就是这一态 ——
    // 界面必须如实显示「它现在是开的、而你不能改」，不能假装是关的。
    const html = render({ ...SPEC, disabled: true }, true);
    expect(html).toContain('aria-checked="true"');
    expect(html).toContain('disabled');
  });
});

/**
 * 两张表单的 `reverseMesh` 职责边界 —— **源码结构门，不是行为门**。
 *
 * # 为什么只能是结构门
 *
 * 理想做法是 import `wgSpec()` 直接看返回值。做不到：`WgDialog.tsx` 在 node 环境加载期就有依赖
 * 访问 DOM。故这里读源码：WARP 专用表单不得出现不适用项，通用 WG 表单则保留可见禁用项。
 *
 * # 为什么值得有
 *
 * 这道门同时防两种回退：把不适用项重新塞进 WARP 专用表单，以及让通用 WG 编辑器里的 WARP
 * 开关恢复可写或重新被隐藏。
 *
 * # 剔注释是承重步骤
 *
 * 本文件与那两个 dialog 的注释里反复出现 `disabled` / `when` / `隐藏` 等字样。不剔注释的话，
 * 把代码删干净、注释留着，门照样报绿（同款教训见 `protocol-settings-coverage.test.ts` 的
 * `stripComments` 文档）。
 */
describe('reverseMesh 表单边界：WARP 专用表单省略，通用 WG 可见但禁用', () => {
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

  const warp = stripComments(read('./WarpDialog.tsx'));
  const wg = specEntry(read('./WgDialog.tsx'));

  it('WARP 专用表单不展示不适用的 reverseMesh 项', () => {
    expect(warp).not.toContain("k: 'reverseMesh'");
    expect(warp).toContain('buildWarpSettings');
  });

  it('WG：按 WARP 判据禁用（不是写死 true —— 普通 WG 节点必须还能开）', () => {
    // 牙：改成 `disabled: true` → 普通 WG 也禁了 → 红；删掉 → 红。
    expect(wg).toMatch(/\bdisabled:\s*isWarpDraft\(/);
    expect(wg).not.toMatch(/\bdisabled:\s*true\b/);
  });

  it('WG 给出 disabledHint，禁用时解释原因', () => {
    expect(wg).toMatch(/disabledHint:\s*'wg\.reverseMeshWarp'/);
    expect(wg).not.toMatch(/disabledHintZh:\s*'/);
  });

  it('WG 不得再用 when 把它整条隐掉', () => {
    // 牙：把 `disabled: …` 换回 `when: (v) => !isWarpDraft(v, base)` → 红。
    expect(wg).not.toMatch(/\bwhen:\s*\(/);
  });
});
