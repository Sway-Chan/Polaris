/**
 * 地区卡说明文案随 reverse 反转的渲染断言（真机 2026-07-20 §1.4 伴生项）。
 *
 * 旧实现恒显正向文案「你所在地区的流量直连，其余全部走代理」，开启回国后语义相反却不变 → 误导。
 * 渲染方式同 ReverseRoutingBadge.test.tsx：node 环境 + renderToStaticMarkup，i18n mock 成恒等。
 */
import { describe, it, expect, vi } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import type { RegionRoutingConfig } from '@/contracts/types';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

const { GeoCard } = await import('./GeoCard');

function markup(routing: RegionRoutingConfig, isSmartMode = true): string {
  return renderToStaticMarkup(
    <GeoCard regionRouting={routing} onChange={() => {}} isSmartMode={isSmartMode} />
  );
}

/** 取**首个** .card-sub 的文本（i18n mock 下即 key 本身）——第二个是「仅智能分流」条件提示。 */
function subKeyOf(routing: RegionRoutingConfig): string | undefined {
  return markup(routing).match(/<div class="card-sub">([^<]*)<\/div>/)?.[1];
}

describe('GeoCard 说明文案', () => {
  it('正向 → 正向语义文案', () => {
    expect(subKeyOf({ enabled: true, region: 'cn', reverse: false })).toBe(
      'rules.regionRoutingSub'
    );
  });

  it('reverse=true → 切换到反向语义文案', () => {
    expect(subKeyOf({ enabled: true, region: 'cn', reverse: true })).toBe(
      'rules.regionRoutingSubReverse'
    );
  });

  it('两种语义的文案 key 不同（不得复用同一句）', () => {
    expect(subKeyOf({ enabled: true, region: 'cn', reverse: true })).not.toBe(
      subKeyOf({ enabled: true, region: 'cn', reverse: false })
    );
  });
});

describe('GeoCard 关态收起', () => {
  const on: RegionRoutingConfig = { enabled: true, region: 'cn', reverse: false };
  const off: RegionRoutingConfig = { enabled: false, region: 'cn', reverse: false };

  it('开 → 地区选择与「回国」都在', () => {
    const html = markup(on);
    expect(html).toContain('geo-region');
    expect(html).toContain('geo-rev-btn');
  });

  it('关 → 二者都不渲染（不是 disabled，是不存在）', () => {
    // 变异守卫：删掉 `{enabled && (…)}` 包裹 → 两条断言都转红。
    const html = markup(off);
    expect(html).not.toContain('geo-region');
    expect(html).not.toContain('geo-rev-btn');
  });

  it('优先级流程条与关态无关，恒在（讲的是全局链，非地区分流参数）', () => {
    expect(markup(off)).toContain('rl-chain-flow');
  });
});

describe('GeoCard「仅智能分流生效」提示', () => {
  const on: RegionRoutingConfig = { enabled: true, region: 'cn', reverse: false };

  it('非智能模式 → 出提示', () => {
    expect(markup(on, false)).toContain('rules.regionRoutingSmartOnly');
  });

  it('智能模式 → 不出（恒真的话 = 常驻噪音）', () => {
    // 变异守卫：去掉 `!isSmartMode &&` 守卫 → 本条转红。
    expect(markup(on, true)).not.toContain('rules.regionRoutingSmartOnly');
  });

  it('主说明句里不再硬编码「仅智能分流模式生效」尾巴', () => {
    expect(subKeyOf(on)).toBe('rules.regionRoutingSub');
  });
});
