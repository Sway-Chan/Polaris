/**
 * tooltip 引擎的**纯逻辑**行为门：延迟状态机 + `data-tip-side` 取值收敛。
 *
 * 方位/边界数学在 `overlay-position.test.ts`（`placeTip`）；「谁挂了引擎、还有没有原生 title」在
 * `tooltip-wiring.test.ts`。本仓 vitest 是 `environment:'node'` 无 jsdom ⇒ `initTooltips` 的 DOM 行为
 * （真的出没出、位置对不对）这一层测不了，属真机验证项，见交接说明。
 */
import { describe, it, expect } from 'vitest';
import { TIP_DELAY, TIP_SKIP, tipOpenDelay } from './tooltip-engine';
import { tipSideOf } from './overlay-position';

describe('tipOpenDelay —— skip-delay 状态机（原生 title 缺的第 1 条）', () => {
  const NOW = 10_000;

  /**
   * 冷启：既没有开着的 tip，也不在刚关过的窗口内 ⇒ 等满首次延迟。
   *
   * 变异对照：把 `TIP_DELAY` 返回改成 `0` → 本条转红（tip 变成一碰就弹，扫过界面全是闪烁）。
   */
  it('冷启等满 TIP_DELAY', () => {
    expect(tipOpenDelay(NOW, false, 0)).toBe(TIP_DELAY);
    expect(tipOpenDelay(NOW, false, NOW - TIP_SKIP)).toBe(TIP_DELAY);
    expect(tipOpenDelay(NOW, false, NOW - TIP_SKIP - 1)).toBe(TIP_DELAY);
  });

  /**
   * 已有 tip 开着（指针从一颗图标钮滑到相邻那颗）⇒ 立即换，不重新等。
   *
   * 变异对照：把 `tipOpen ||` 去掉 → 本条转红 —— 这正是原生 title 的行为（每颗都重新等满）。
   */
  it('已有 tip 开着时立即出', () => {
    expect(tipOpenDelay(NOW, true, 0)).toBe(0);
  });

  /**
   * 刚关过不到 TIP_SKIP ⇒ 仍算连扫同一排，立即出。
   *
   * 变异对照：把 `now - lastCloseAt < TIP_SKIP` 改成 `<= 0` → 本条转红。
   */
  it('刚关过 TIP_SKIP 窗口内立即出，出窗口后恢复等待', () => {
    expect(tipOpenDelay(NOW, false, NOW - 1)).toBe(0);
    expect(tipOpenDelay(NOW, false, NOW - (TIP_SKIP - 1))).toBe(0);
    // 边界：正好等于 TIP_SKIP 已出窗口（`<` 而非 `<=`）。
    expect(tipOpenDelay(NOW, false, NOW - TIP_SKIP)).toBe(TIP_DELAY);
  });

  /** 常量本身：与原型 `proto:3113` 逐字对齐，也是 `HoverCard.tsx` 富卡片档的同源取值。 */
  it('延迟常量与原型一致（500 / 300）', () => {
    expect(TIP_DELAY).toBe(500);
    expect(TIP_SKIP).toBe(300);
  });
});

describe('tipSideOf —— data-tip-side 取值收敛（原生 title 缺的第 2 条）', () => {
  /** 变异对照：把回落值改成 `'right'` → 本条转红。 */
  it('四个合法方位原样通过，其余一律回落 top', () => {
    expect(tipSideOf('top')).toBe('top');
    expect(tipSideOf('bottom')).toBe('bottom');
    expect(tipSideOf('left')).toBe('left');
    expect(tipSideOf('right')).toBe('right');
    // 属性缺失 / 拼错 / 空串都必须回落，不能落进 `at()` 的兜底分支（那会让 tip 悄悄跑到反方向）。
    expect(tipSideOf(undefined)).toBe('top');
    expect(tipSideOf(null)).toBe('top');
    expect(tipSideOf('')).toBe('top');
    expect(tipSideOf('TOP')).toBe('top');
    expect(tipSideOf('start')).toBe('top');
  });
});
