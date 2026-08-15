/**
 * 连接页三个生命周期视图的顺序与默认值守卫。
 *
 * 为什么是源码结构守卫：本仓 vitest 是 node 环境、全仓无组件渲染测试
 * （见 `connections-context-menu.test.ts` 头注）。tab 顺序与初值都是 JSX/字面量层的事实，
 * 逻辑单测照不出来，源码守卫是这里唯一照得出「顺序被换回去 / 默认被改回明细」的手段。
 *
 * 顺序与默认是**两件独立的事**（各自能单独回退），故分两条断言，各带各的变异对照。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

/** 去注释后的源码：注释里逐字写着「拓扑在前」，扫原文会让改了 JSX、留着注释的版本照样绿。 */
const code = (src: string): string =>
  src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/(^|[^:])\/\/.*$/gm, '$1');

const SRC = code(
  readFileSync(fileURLToPath(new URL('./ConnectionsScreen.tsx', import.meta.url)), 'utf8')
);

describe('连接页视图 tab', () => {
  /**
   * 拓扑按钮必须排在明细按钮**之前**。
   *
   * 断在 tablist 容器**之内**，免得被页面别处的同名调用（如 TOP 视图正文里的标题）带偏；
   * 用 `setView('top')` / `setView('table')` 作锚点而不是文案 key —— 换文案不误伤，换顺序必红。
   *
   * 变异对照：把两个 `<button>` 换回原顺序（明细在前）→ 本条转红。
   */
  it('tab 顺序 = 拓扑 → 活动 → 已结束', () => {
    const start = SRC.indexOf('role="tablist"');
    expect(start).toBeGreaterThan(-1);
    const end = SRC.indexOf('</div>', start);
    const tablist = SRC.slice(start, end);

    const top = tablist.indexOf("setView('top')");
    const active = tablist.indexOf("setView('active')");
    const closed = tablist.indexOf("setView('closed')");
    expect(top).toBeGreaterThan(-1);
    expect(active).toBeGreaterThan(-1);
    expect(closed).toBeGreaterThan(-1);
    expect(top).toBeLessThan(active);
    expect(active).toBeLessThan(closed);
  });

  /**
   * 进页默认视图 = 拓扑。
   *
   * 变异对照：初值改回 `useState<ConnView>('table')` → 本条转红。
   */
  it('默认视图 = 拓扑', () => {
    expect(SRC).toContain("useState<ConnView>('top')");
  });

  /**
   * 默认拓扑成立的前提：aggregate 腿的 gate 是 `view === 'top'`，进页即订、有数据源。
   * 若哪天有人把 gate 改成别的（或干脆把默认视图的订阅腿摘了），默认视图会变成空屏
   * ——那是比顺序错更难发现的回归，故一并钉住。
   *
   * 变异对照：把 `if (view !== 'top') return;` 改成 `'table'`（或删掉整条 aggregate 订阅）→ 本条转红。
   */
  it('默认视图的数据源（aggregate 订阅）随该视图开启', () => {
    expect(SRC).toContain("if (view !== 'top') return;");
    expect(SRC).toContain("api.stats.subscribe('aggregate')");
  });
});
