/**
 * 订阅信息栏的进度/失败呈现门 —— 「reducer 对了但界面上没接」这一类假绿的唯一防线。
 *
 * 手段沿用本仓既有先例（`harness-screens.test.tsx` / `settings/terminal-env-and-fold.test.tsx`）：
 * node 环境 + `react-dom/server` 真渲染真组件。本仓刻意不装 jsdom / testing-library，别为这道门破例。
 *
 * `t()` 桩把插值原样拼回 key 后面（`key(done=2,total=5)`），这样「provider 计数没传进文案」
 * 这个变异也抓得到 —— 只返回 key 的桩抓不到它。
 *
 * **明确不在射程**：CSS（node 下无样式，浅/深两档的对比度靠复用既有 `.pill.warn/.pill.err`
 * 与 `.spinner` 三个**已在两档主题下定义**的类，本次零新增样式）；tooltip 的真实弹出行为。
 */
import { describe, it, expect, vi } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import type { SubscriptionConfig } from '@/contracts/types';
import type { SubscriptionUpdateProgress } from '@/contracts/subscription-progress';

vi.mock('react-i18next', () => ({
  initReactI18next: { type: '3rdParty', init: () => {} },
  useTranslation: () => ({
    t: (key: string, opts?: unknown) => {
      if (opts && typeof opts === 'object') {
        const vars = Object.entries(opts as Record<string, unknown>)
          .filter(([k]) => k !== 'defaultValue')
          .map(([k, v]) => `${k}=${String(v)}`)
          .join(',');
        return vars ? `${key}(${vars})` : key;
      }
      return key;
    },
    i18n: { language: 'zh-CN' },
  }),
}));

/** node 无 document；本组件树里的锚定菜单 hook 在模块/渲染期会摸它。 */
(globalThis as unknown as { document: unknown }).document = {
  documentElement: { dir: '', lang: '', getAttribute: () => null, setAttribute: () => {} },
  body: { nodeType: 1 },
};

const { SubInfoBar } = await import('./SubInfoBar');

const SUB: SubscriptionConfig = {
  id: 'sub-a',
  name: '机场 A',
  url: 'https://example.invalid/sub',
  autoUpdate: true,
  createdAt: '2026-07-01T00:00:00Z',
  lastUpdated: '2026-07-01T00:00:00Z',
};

/** 只截刷新按钮那一段：同一栏里「更多」按钮在未接 `onMenuAction` 时本就 disabled，整串搜 'disabled' 会误判。 */
const refreshBtn = (html: string): string => {
  const at = html.indexOf('<button type="button" class="btn ghost sm"');
  expect(at, '刷新按钮必须存在（本断言的锚点）').toBeGreaterThanOrEqual(0);
  return html.slice(at, html.indexOf('</button>', at));
};

const render = (progress?: SubscriptionUpdateProgress | null) =>
  renderToStaticMarkup(
    <SubInfoBar
      subscription={SUB}
      nodeCount={12}
      config={{ autoUpdateSubscriptionOnStart: true, subscriptionUpdateIntervalHours: 12 }}
      progress={progress}
    />
  );

describe('SubInfoBar 更新进度', () => {
  it('无进度时照旧显示自动更新徽标、刷新按钮可点', () => {
    const html = render(null);
    // 正向对照：证明下面那些「不含」断言不是因为整个组件渲染成了空。
    expect(html).toContain('nodes.subAutoUpdateEvery');
    expect(refreshBtn(html)).toContain('nodes.subRefresh');
    expect(refreshBtn(html)).not.toContain('disabled');
    expect(html).not.toContain('nodes.subUpdating');
  });

  it('拉取中：进度徽标顶掉自动徽标，刷新按钮转圈且禁点', () => {
    const html = render({ subscriptionId: 'sub-a', phase: 'fetching' });
    expect(html).toContain('nodes.subUpdatingFetching');
    expect(html).toContain('spinner');
    // 变异锁：不禁用 → 用户连点会真的并发拉两次同一订阅（后端无单飞闸），两次对账互相覆盖。
    expect(refreshBtn(html)).toContain('disabled');
    expect(html).toContain('nodes.subUpdating');
    // 更新进行中时「下次几点自动刷」是此刻最不相关的一条信息，让位给进度。
    expect(html).not.toContain('nodes.subAutoUpdateEvery');
  });

  it('provider 阶段带真计数（done/total 必须进到文案里）', () => {
    const html = render({ subscriptionId: 'sub-a', phase: 'providers', done: 2, total: 5 });
    // 变异锁：只渲染阶段名、丢掉计数 → 全库唯一有真进度的那个阶段退化成又一个静止的转圈。
    expect(html).toContain('nodes.subUpdatingProviders(done=2,total=5)');
  });

  it('落盘阶段单独可见', () => {
    expect(render({ subscriptionId: 'sub-a', phase: 'applying' })).toContain(
      'nodes.subUpdatingApplying'
    );
  });

  it('失败：徽标常驻 + tooltip 逐字给后端真错因 + 刷新按钮可再点', () => {
    const html = render({
      subscriptionId: 'sub-a',
      phase: 'failed',
      error: '全局策略要求「所有订阅经代理更新」，但当前代理不可用',
    });
    expect(html).toContain('nodes.subUpdateFailed');
    expect(html).toContain('pill err');
    // 变异锁：把 tooltip 换成笼统兜底文案 → 用户看得见「失败了」却看不见失败在哪一步。
    expect(html).toContain('全局策略要求');
    // 失败态必须能立刻重试；且失败会长期挂着 ⇒ 不许永久遮住自动更新徽标那条真信息。
    expect(refreshBtn(html)).not.toContain('disabled');
    expect(html).toContain('nodes.subAutoUpdateEvery');
  });
});
