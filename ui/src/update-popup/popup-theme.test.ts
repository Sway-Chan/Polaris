/**
 * 更新弹窗的主题同源门 —— 守 2026-07-28 复审抓出的「闪烁方向被反转」。
 *
 * 缺陷原状：主进程按 `config.uiTheme` 折算了**原生窗底色**（`surface_color(native_dark(app))`），
 * 但弹窗页面 CSS 只有深色一档（`--surface:#161c24`）。于是浅色用户从「深底 → 深卡片」（无闪）
 * 变成「白底闪一格 → 深色卡片」。原生底色与页面主题必须**同源**，一个跟随一个写死等于把 bug 换个方向。
 *
 * 三条断言，各锁一层：
 *  1. **Rust 侧同源**：`build_popup_window` 里只解析一次 `native_dark`，`surface_color` 与
 *     `theme_boot_script` 吃的是**同一个绑定**。这条只能读源码断言——建窗要真 Tauri app 实例，
 *     单测够不着（视觉结果本身是真机门）。
 *  2. **CSS 有两档且键集对等**：浅色档漏一个变量 = 那个颜色在浅色下仍是深色档取值（半修）。
 *  3. **规则体里没有只在单档成立的中性叠色**：新增一条 `rgba(255,255,255,.06)` 就是「又只改了一半」
 *     的复发形态，故一律逼进变量。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const read = (rel: string) => readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8');
const stripCss = (s: string) => s.replace(/\/\*[\s\S]*?\*\//g, '');

const css = stripCss(read('./style.css'));
const rustSrc = stripCss(read('../../../src-tauri/src/runtime/update_popup.rs')).replace(
  /(^|[^:])\/\/.*$/gm,
  '$1',
);

/** 取某个选择器块里声明的全部自定义属性名。 */
function customProps(selector: string): Set<string> {
  const re = new RegExp(`${selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\s*\\{([^}]*)\\}`);
  const m = css.match(re);
  expect(m, `样式里找不到选择器 ${selector}`).not.toBeNull();
  return new Set([...m![1].matchAll(/(--[\w-]+)\s*:/g)].map((x) => x[1]));
}

describe('① Rust 侧：原生底色与页面主题种子出自同一个 dark', () => {
  it('build_popup_window 只解析一次 native_dark', () => {
    expect([...rustSrc.matchAll(/native_dark\(/g)]).toHaveLength(1);
    expect(rustSrc).toMatch(/let dark = crate::tray::native_dark\(app\);/);
  });

  it('surface_color 与 theme_boot_script 吃的都是那个绑定（不是各自再解析一次）', () => {
    expect(rustSrc).toContain('crate::tray::surface_color(dark)');
    expect(rustSrc).toContain('crate::tray::theme_boot_script(dark)');
  });
});

describe('② CSS：深/浅两档存在且变量键集对等', () => {
  const dark = customProps(':root');
  const light = customProps(":root[data-theme='light']");
  const fallback = customProps(':root:not([data-theme])');

  it('深色档非空（前提自检：选择器没被改名，断言不是在空集上恒绿）', () => {
    expect(dark.size).toBeGreaterThan(4);
    expect(dark.has('--surface')).toBe(true);
  });

  it('浅色档与深色档变量键集完全一致（漏一个 = 那个颜色浅色下仍是深色取值）', () => {
    expect([...light].sort()).toEqual([...dark].sort());
  });

  it('注入缺席兜底（prefers-color-scheme）也覆盖同一组键', () => {
    expect([...fallback].sort()).toEqual([...dark].sort());
  });

  it('--surface 两档与 Rust surface_color 逐字对齐（#161C24 / #FFFFFF）', () => {
    expect(css).toMatch(/:root\s*\{[^}]*--surface:\s*#161c24/i);
    expect(css).toMatch(/:root\[data-theme='light'\]\s*\{[^}]*--surface:\s*#ffffff/i);
  });
});

describe('③ 规则体里不得再出现只在单档成立的中性叠色', () => {
  it('白/黑半透明覆盖层只许出现在变量声明里', () => {
    // 变量块（`:root…{}` 三段）之外的正文。命中即是「新增了一条只改一半的颜色」。
    const bodyOnly = css.replace(/:root[^{]*\{[^}]*\}/g, '');
    const offenders = [...bodyOnly.matchAll(/rgba\(\s*(255,\s*255,\s*255|0,\s*0,\s*0|15,\s*23,\s*42)/g)];
    expect(offenders.map((m) => m[0])).toEqual([]);
  });
});
