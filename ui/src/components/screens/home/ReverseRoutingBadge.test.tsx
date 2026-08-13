/**
 * 主页反向（回国）持久指示器的渲染断言。
 *
 * 本仓 vitest 跑 node 环境、无 jsdom/testing-library（且本轮不许新增依赖）→ 用 react-dom/server
 * 的 renderToStaticMarkup 做真实渲染后的 DOM 断言（已装依赖，零新增）。
 * i18n 被 mock 成恒等函数：断言落在 key 上，与具体语种文案解耦。
 */
import { describe, it, expect, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { renderToStaticMarkup } from 'react-dom/server';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

const { ReverseRoutingBadge } = await import('./ReverseRoutingBadge');

describe('ReverseRoutingBadge', () => {
  it('reverse=true → 指示器出现，带 warn 视觉与说明 tooltip', () => {
    const html = renderToStaticMarkup(<ReverseRoutingBadge reverse={true} />);
    expect(html).toContain('home.reverseRoutingBadge');
    expect(html).toContain('home.reverseRoutingTip');
    // 复用既有 .pill.warn 原语，不自造新视觉
    expect(html).toContain('pill warn');
  });

  it('reverse=false → 完全不渲染（正向语义下零额外噪音）', () => {
    const html = renderToStaticMarkup(<ReverseRoutingBadge reverse={false} />);
    expect(html).toBe('');
  });

  /**
   * 组件自身两条腿之外，还剩一条逃逸路径：**HomeScreen 压根不挂这个徽章**——那正是本次要修的
   * 「零可见性」原状，且组件级测试全绿也照样漏过。无 jsdom/testing-library 渲染不了 HomeScreen
   * （store/ipc/i18n 依赖过重），故退而对接线做源码级断言：能挡住「整行被删/传死值」，
   * 挡不住摆放位置与视觉（那部分是真机门）。
   */
  it('HomeScreen 确实接了徽章（挡住接线被整行删除）', () => {
    const src = readFileSync(new URL('./HomeScreen.tsx', import.meta.url), 'utf8');
    expect(src).toContain('<ReverseRoutingBadge reverse={reverseRouting} />');
    expect(src).toContain('isReverseRegionRouting(config)');
  });
});
