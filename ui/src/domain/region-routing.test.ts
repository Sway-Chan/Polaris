/**
 * 反向（回国）语义判据 —— 主页持久指示器的唯一真值源。
 * 真机 2026-07-20 §1.4：reverse 在主页零可见性，用户误触后不知分流语义已反转。
 */
import { describe, it, expect } from 'vitest';
import { isReverseRegionRouting, DEFAULT_REGION_ROUTING } from './region-routing';

describe('isReverseRegionRouting', () => {
  it('地区分流开启 + reverse → true（指示器出现）', () => {
    expect(
      isReverseRegionRouting({ regionRouting: { enabled: true, region: 'cn', reverse: true } })
    ).toBe(true);
  });

  it('地区分流开启 + 正向 → false（零噪音）', () => {
    expect(
      isReverseRegionRouting({ regionRouting: { enabled: true, region: 'cn', reverse: false } })
    ).toBe(false);
  });

  it('地区分流关闭时 reverse 是死数据 → false', () => {
    expect(
      isReverseRegionRouting({ regionRouting: { enabled: false, region: 'cn', reverse: true } })
    ).toBe(false);
  });

  it('缺省 regionRouting 走默认（正向）→ false', () => {
    expect(DEFAULT_REGION_ROUTING.reverse).toBe(false);
    expect(isReverseRegionRouting({})).toBe(false);
  });

  it('config 未加载（null/undefined）→ false，不早报', () => {
    expect(isReverseRegionRouting(null)).toBe(false);
    expect(isReverseRegionRouting(undefined)).toBe(false);
  });

  it('非 cn 地区同样识别反向（ir/ru 不特殊）', () => {
    expect(
      isReverseRegionRouting({ regionRouting: { enabled: true, region: 'ru', reverse: true } })
    ).toBe(true);
  });
});
