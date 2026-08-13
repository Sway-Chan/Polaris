/**
 * 连接页行右键菜单（G4）的**接线**守卫。
 *
 * 为什么是源码结构守卫：本仓 vitest 是 node 环境、全仓无组件渲染测试（`App.test.ts` 头注），
 * 而这条缺口的性质正是「后端全在、只差一个菜单」—— 四个动作各自的实现早有覆盖
 * （`connections_close` 有 Rust 侧测试、`rules.add` 有规则页测试、`clampToWrap` 有本仓
 * `lib/overlay-position.test.ts`），缺的从来是**调用点**。逻辑单测再多也照不出「菜单没接上」。
 * 沿用既有范式（`nodes/nodes-speedtest-wiring.test.ts`、`store/latency-wiring-invariants.test.ts`）。
 *
 * 守形态不守措辞：断言的是「哪条腿调了什么」，改文案/改变量名不误伤，把动作摘掉必然转红。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

/**
 * 去注释后的源码 —— 所有断言都跑在它上面。
 *
 * 本文件的注释里逐字写着被守的调用形态（`onClose(row)` 等），直接扫原文会让
 * 「把动作删了、注释留着」照样绿。`[^:]` 前瞻避免把 `https://` 当行注释。
 */
const code = (src: string): string =>
  src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');

const SRC = code(
  readFileSync(fileURLToPath(new URL('./ConnectionsScreen.tsx', import.meta.url)), 'utf8'),
);

/**
 * 「加规则」那两项 2026-07-30 起搬进共用组件 `components/host-rule-menu.tsx`（拓扑右键菜单与本页
 * 共用同一条腿 + 同一套排序判据，同 `lib/use-rule-delete.ts` 的先例）。守卫跟着搬 —— 留在本文件
 * 里扫 `ConnectionsScreen.tsx` 只会恒绿（那两项已经不在这里了）。
 */
const MENU_SRC = code(
  readFileSync(fileURLToPath(new URL('../../host-rule-menu.tsx', import.meta.url)), 'utf8'),
);

describe('行右键菜单已接线', () => {
  /**
   * 菜单必须由**行**的 contextmenu 打开，且吃掉原生菜单。
   *
   * 变异对照：删掉 `onContextMenu` → 第一条转红；删掉 `preventDefault()` → 第二条转红
   * （不吃掉的话浏览器原生菜单会盖在自绘菜单上，两个菜单同屏）。
   */
  it('tr 上挂 onContextMenu 且吃掉原生菜单', () => {
    expect(SRC).toContain('onContextMenu={(e) =>');
    const at = SRC.indexOf('onContextMenu={(e) =>');
    expect(SRC.slice(at, at + 200)).toContain('e.preventDefault()');
    expect(SRC.slice(at, at + 200)).toContain('setMenu({');
  });

  /**
   * 四个动作各自接到**既有**的真实现，一个都不许是占位。
   *
   * 变异对照：把任一 `onClick` 换成空函数 / toast 占位 → 对应那条转红。
   */
  it('四个动作接到既有实现（复制域名 / 复制 IP / 加规则 / 关闭连接）', () => {
    expect(SRC).toContain('copyText(menu.row.host)');
    expect(SRC).toContain('copyText(menu.row.dest)');
    expect(SRC).toContain('<HostRuleMenuItems host={menu.row.host}');
    // 关闭走既有 onClose（乐观移除 + 失败回滚 + 抑制集），不得另写一条裸 close。
    expect(SRC).toContain('void onClose(row)');
  });

  /**
   * 「新建规则」走规则弹窗，不另造一条直写腿；「加入已有规则」走选择器 + 追加腿。
   *
   * 变异对照：把新建那条换成 `api.rules.add({...})` 直写 → 第二条转红。直写会绕开弹窗的校验与
   * action 选择，用户拿不到「代理 / 直连」的选择权。把「加入已有」那条摘掉 → 第三条转红。
   */
  it('加规则 = 跳规则页 + 打开预填条件的完整弹窗（腿在共用组件里）', () => {
    expect(MENU_SRC, '共用组件读空了 —— 下面两条会恒绿').toContain('HostRuleMenuItems');
    expect(MENU_SRC).toContain("navigate('rules')");
    expect(MENU_SRC).toContain("openDialog({ kind: 'rule', presetDomain: host })");
    expect(MENU_SRC).toContain("openDialog({ kind: 'rule-pick'");
  });

  /**
   * 三条关闭腿缺一不可，滚动那条尤其容易漏。
   *
   * 变异对照：删掉 `scroll` 监听（或去掉捕获参数 `true`）→ 第三条转红。菜单是 `position:fixed`，
   * 不关的话表一滚它就浮在原处指着**另一行**——比没有菜单更危险。
   * 滚动事件不冒泡，非捕获阶段收不到 `.conn-scroll` 的滚动。
   */
  it('点空白 / ESC / 滚动都能关掉菜单', () => {
    expect(SRC).toContain("document.addEventListener('mousedown', onDown)");
    expect(SRC).toContain("document.addEventListener('keydown', onKey)");
    expect(SRC).toContain("document.addEventListener('scroll', onScroll, true)");
    // 注册了就必须卸载，否则每次开菜单都往 document 上叠一层监听。
    expect(SRC).toContain("document.removeEventListener('scroll', onScroll, true)");
  });

  /**
   * 复用原型样式类，**零新增 CSS**（`components.css` / `prototype.css` 是禁区，
   * 端口侧改动一律走 `index.css` 覆盖层；这里连覆盖层都不需要）。
   *
   * 变异对照：另起一个 `.conn-ctx` 类名 → 本条转红。
   */
  it('复用 .ctx-menu / .ctx-i，不新造样式', () => {
    expect(SRC).toContain('className="ctx-menu"');
    expect(SRC).toContain('className="ctx-i"');
    expect(SRC).toContain('className="ctx-i danger"');
  });
});
