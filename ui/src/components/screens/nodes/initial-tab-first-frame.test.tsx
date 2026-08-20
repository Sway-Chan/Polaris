/**
 * 节点屏落地 tab 的**首帧**门 —— 守「点导航进本页先闪一帧『自建』再跳到选中节点所在订阅组」。
 *
 * # 为什么这道门必须是「真渲染首帧」，而不是又一组 `initialNodesTab` 单测
 *
 * `initialNodesTab` 的判据一直是对的、也一直有单测（`initial-tab.test.ts`）。缺陷在**消费时机**：
 * 原实现 `useState('manual')` + `useEffect` 定位，而 `useEffect` 在浏览器**绘制之后**才跑 ⇒
 * 「自建」那一帧是真的被画出来的（真机反馈：「先从自建到实际选中的订阅组一闪而过」）。
 * 判据单测对这类缺陷恒绿，只有把首帧真渲出来才看得见。
 *
 * # 手段与射程
 *
 * 沿用本仓既有先例：node 环境 + `react-dom/server` 的 `renderToStaticMarkup` 真渲染真组件
 * （`harness-screens.test.tsx`、`settings/terminal-env-and-fold.test.tsx`；本仓刻意不装 jsdom /
 * testing-library，别为这道门破例）。SSR 不跑 `useEffect` —— 对本缺陷这恰是**射程正好**：
 * 门看到的就是用户眼里那第一帧，而「首帧就该是正确那一组」正是修法的全部内容。
 * 一旦有人把初值改回常量、或把定位挪回 effect，本文件立刻转红。
 *
 * 用 `harness-fixture` 的 `DEMO_CONFIG` 而不是另造数据：它的 `selectedServerId: 's1'` 正好落在
 * 订阅 `sub1`（`s3` 是唯一的自建节点）——即「选中项不在首个组里」这个必要形态，另造一份等于
 * 把同一个形态维护两遍。本门因此依赖该 fixture 的这条关系，动它会连带红这里。
 */
import { describe, it, expect, vi } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { DEMO_CONFIG } from '../../../../harness-fixture';

/** t() 桩：返回 key 本身（同上述先例）——断言落在 fixture 数据与 DOM 结构上，与语种文案解耦。 */
vi.mock('react-i18next', () => ({
  initReactI18next: { type: '3rdParty', init: () => {} },
  useTranslation: () => ({ t: (key: string) => key, i18n: { language: 'zh-CN' } }),
}));

/** node 无 document，而 `@/i18n` 模块加载期就写 `<html dir/lang>`、Csel 还要 portal 到 body。 */
(globalThis as unknown as { document: unknown }).document = {
  documentElement: { dir: '', lang: '', getAttribute: () => null, setAttribute: () => {} },
  body: { nodeType: 1 },
};

const { useAppStore } = await import('@/store/app-store');
const { useSubscriptionProgressStore } = await import('@/store/use-subscription-progress-store');

/** 与 `app-store.loadConfig` 的 config→state 投影同形。 */
const SEED = {
  config: DEMO_CONFIG,
  servers: DEMO_CONFIG.servers,
  selectedServerId: DEMO_CONFIG.selectedServerId,
  rules: DEMO_CONFIG.customRules,
};
useAppStore.setState(SEED);
// zustand v4 在服务端渲染下读的是初始态快照（`getServerState || getInitialState`）——只 setState
// 会让本门对着空 store 渲染、退化成假绿。故对初始态对象就地播种（同 harness-screens.test.tsx）。
Object.assign(useAppStore.getInitialState(), SEED);

/** 整棵屏的依赖图第一次 vite transform 要好几秒，装载成本留在 collect 阶段，别撞 5s testTimeout。 */
const NodesScreen = (await import('./NodesScreen')).default;

const firstFrame = (): string => renderToStaticMarkup(<NodesScreen />);

/** tab 条上带 `on` 的那颗（`data-v` = 组 id）。恒只有一颗，返回 null 表示一颗都没选中。 */
function activeTabId(html: string): string | null {
  return html.match(/<button[^>]*class="on"[^>]*data-act="sub-tab"[^>]*data-v="([^"]*)"/)?.[1] ?? null;
}

describe('节点屏首帧就落在选中节点所在的组', () => {
  it('首帧激活的 tab = 选中节点 s1 所在的订阅组 sub1，不是常驻首组 manual', () => {
    // 变异验证：把 activeTab 初值改回 `useState<string>('manual')`（即缺陷本身）→ 本条转红。
    expect(activeTabId(firstFrame())).toBe('sub1');
  });

  it('首帧的卡片就是 sub1 那两张；自建组那张一帧都不出现', () => {
    const html = firstFrame();
    expect(html).toContain('香港 IEPL · 01');
    expect(html).toContain('日本 · 东京 02');
    // 这一句是「闪跳的第一帧」本体：缺陷版的首帧只有这张自建卡（且无订阅信息栏）。
    expect(html).not.toContain('自建 · 新加坡');
  });

  it('首帧就带订阅信息栏（订阅组独有），不是挂载后才补上', () => {
    // `.nd-subinfo` 只在 activeSub 存在时渲染 ⇒ 它在首帧出现，等价于「首帧已在订阅组上」。
    expect(firstFrame()).toContain('nd-subinfo');
  });

  it('任一订阅失败后，对应 tab 持续显示失败标识并携带完整错误 tooltip', () => {
    const progress = {
      sub1: {
        subscriptionId: 'sub1',
        phase: 'failed' as const,
        error: 'TLS handshake timeout',
      },
    };
    useSubscriptionProgressStore.setState({ progress });
    // Zustand 的 SSR 快照与运行态分离；首帧门两边都播种，避免只改 setState 产生假绿。
    Object.assign(useSubscriptionProgressStore.getInitialState(), { progress });

    const html = firstFrame();
    expect(html).toContain('data-tip="TLS handshake timeout"');
    expect(html).toContain('class="pill err sub-tab-failure"');
    expect(html).toContain('nodes.subUpdateFailed');
  });
});
