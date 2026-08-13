/**
 * harness fixture 的逐屏渲染冒烟门 —— 「某屏在 harness 里塌了」必须当场自曝。
 *
 * # 这道门补的是类型检查够不到的那一半
 *
 * `harness-fixture.ts` 已带 `UserConfig` 标注、且已随根层通配进 tsconfig，故**schema 不符**这一类
 * （2026-07-30 那次：`customRules` 停在旧 Rule schema）现在 `tsc` 就会红。但类型对、数据不对照样能
 * 炸：某个字段是合法的 `string[]` 却为空、某个 id 指向不存在的实体、某个可选字段缺了而消费点没防守
 * —— 这些只有真渲染才看得见。harness 的失效形态又恰恰是**静默**的：React 树一卸载就是白屏，
 * 没有报错、没有退出码，用它做过 UI 实测的人只会以为「这屏就长这样」。
 *
 * 手段沿用本仓既有先例：node 环境 + `react-dom/server` 的 `renderToStaticMarkup` 真渲染真组件
 * （`components/screens/settings/terminal-env-and-fold.test.tsx:1-25`；本仓刻意不装 jsdom /
 * testing-library，别为这道门破例）。
 *
 * # 明确不在射程（如实标注，不假装覆盖）
 *
 *  · **只有首帧**。`useEffect` 在 SSR 不跑 ⇒ 所有「挂载后 invoke 回填 → 重渲」的内容都看不到
 *    （节点测速值、解锁检测、helper 状态、SettingsPage 的 useConfig 异步壳…）。真正在 harness 里
 *    炸的那次是**首帧**炸的，这道门盯的就是首帧；异步二帧仍归真机门。
 *  · **交互路径不覆盖**：点开弹窗、下拉、拖拽排序都要真 DOM 事件。`rule_resources_get_catalog`
 *    这类只在弹窗里被消费的 mock 返回体，本门验不到（那条已由 harness-main 的注释就地钉住）。
 *  · **应用分流屏是空壳**：卡片墙由 `app_presets_list` 驱动，harness 把它 mock 成 `[]`，
 *    故 `DEMO_CONFIG.appRules` 在任何屏上都渲染不出来 —— 该屏「能渲染」不等于「规则被渲染过」。
 *  · **CSS / 布局 / 视觉**一概不验（node 下无样式）。
 */
import { describe, it, expect, vi } from 'vitest';
import type { ComponentType } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import type { UserConfig } from '@/contracts/types';
import { DEMO_CONFIG } from '../harness-fixture';

/** t() 桩：返回 key 本身（同上述先例）——断言落在结构与 fixture 数据上，与语种文案解耦。 */
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

/** 与 `app-store.loadConfig` 的 config→state 投影同形（harness 里真正走的就是那条腿）。 */
const SEED = {
  config: DEMO_CONFIG,
  servers: DEMO_CONFIG.servers,
  selectedServerId: DEMO_CONFIG.selectedServerId,
  rules: DEMO_CONFIG.customRules,
};
useAppStore.setState(SEED);
// zustand v4 在服务端渲染下读的是 `api.getServerState || api.getInitialState`（esm/index.mjs:20），
// 也就是**初始态**——只 setState 的话每一屏都会拿着空 store 渲染，这道门就退化成「渲染空屏也全绿」
// 的假门。故对初始态对象就地播种。这条腿一旦失效（zustand 升级改了快照来源），下面的「正向对照」
// 会立刻转红，不会静默变空。
Object.assign(useAppStore.getInitialState(), SEED);

/** 设置子页统一收 `{config, update}`；主屏组件无 props，多给的属性 React 会忽略。 */
type Screen = ComponentType<{ config?: UserConfig; update?: (patch: Partial<UserConfig>) => Promise<void> }>;
const noop = async () => {};

/**
 * 覆盖面 = 侧栏 7 个主屏 + 设置页 9 个子页（`ScreenRouter` / `SettingsPage` 的全部分支）。
 *
 * 在**模块加载期**一次性 import 完，不留到各 it 里懒加载：整棵屏的依赖图第一次被 vite transform
 * 要好几秒，落在 it 体内就会撞上默认 5s testTimeout —— 单跑本文件时不撞、跟全量套件并发时撞，
 * 正是最坏的那种 flaky。装载成本挪到 collect 阶段后，it 体内只剩一次纯渲染。
 */
const SCREENS: [string, Screen][] = await Promise.all(
  (
    [
      ['home', () => import('@/components/screens/home/HomeScreen')],
      ['nodes', () => import('@/components/screens/nodes/NodesScreen')],
      ['rules', () => import('@/components/screens/rules/RulesScreen')],
      ['apppolicy', () => import('@/components/screens/apppolicy/AppPolicyScreen')],
      ['resources', () => import('@/components/screens/resources/ResourcesScreen')],
      ['connections', () => import('@/components/screens/connections/ConnectionsScreen')],
      ['logs', () => import('@/components/screens/logs/LogsScreen')],
      ['settings/general', () => import('@/components/screens/settings/SettingsGeneral')],
      ['settings/display', () => import('@/components/screens/settings/SettingsDisplay')],
      ['settings/network', () => import('@/components/screens/settings/SettingsNetwork')],
      ['settings/dns', () => import('@/components/screens/settings/SettingsDns')],
      ['settings/tun', () => import('@/components/screens/settings/SettingsTun')],
      ['settings/update', () => import('@/components/screens/settings/SettingsUpdate')],
      ['settings/backup', () => import('@/components/screens/settings/SettingsBackup')],
      ['settings/helper', () => import('@/components/screens/settings/SettingsHelper')],
      ['settings/about', () => import('@/components/screens/settings/SettingsAbout')],
    ] as [string, () => Promise<{ default: unknown }>][]
  ).map(async ([name, load]) => [name, (await load()).default as Screen] as [string, Screen]),
);

const screenNamed = (name: string): Screen => SCREENS.find(([n]) => n === name)![1];
const render = (Screen: Screen): string =>
  renderToStaticMarkup(<Screen config={DEMO_CONFIG} update={noop} />);

describe('harness fixture 喂给每一屏都渲染得出来', () => {
  for (const [name, Screen] of SCREENS) {
    it(`${name} 首帧不抛`, () => {
      // 抛出即失败（这正是白屏的成因：渲染期异常 → React 卸载整棵树）；同时挡「渲染成空」。
      expect(render(Screen)).not.toBe('');
    });
  }
});

// ---------------------------------------------------------------------------
// 正向对照 —— 证明 fixture 真的流进了屏里，而不是每屏都在渲染空 store
// ---------------------------------------------------------------------------

describe('正向对照：上面那 16 个「不抛」不是空转', () => {
  it('规则屏渲染出 fixture 的 2 条规则，且条件摘要真读到了 values（不是空转）', () => {
    const html = render(screenNamed('rules'));
    expect(html).toContain('id="rule-count">2<');
    // 本条守的**意图未变**：证明 `c.values` 真被消费过（旧 schema 下它是 undefined，一读就炸）。
    // 只是判据换了形态 —— 2026-08-03 条件摘要从「逐值平铺」改成「类型 ×n」计数（值明细移进 hover
    // 面板，而 `HoverCardPanel` 在 open=false 时 return null ⇒ 静态渲染里必然没有值文本）。
    // 计数是同等强度的正向对照，甚至更强：`undefined.length` 同样会抛，而数错了这里直接转红。
    expect(html).toContain('×2'); // geosite: netflix, disney
    expect(html).toContain('×1'); // ruleSet: category-ads-all
  });

  it('节点屏渲染出 fixture 的节点与订阅（store 播种腿还活着）', () => {
    const html = render(screenNamed('nodes'));
    // 首帧停在 fixture 的 `selectedServerId: 's1'` 所在的订阅组（sub1），故断言它那张卡 +
    // 订阅 tab 的名字。「首帧选谁」本身是另一件事，由
    // `components/screens/nodes/initial-tab-first-frame.test.tsx` 专门守。
    expect(html).toContain('香港 IEPL · 01');
    expect(html).toContain('IEPL 机场');
  });

  it('设置页按 fixture 的端口回显（config 经 props 那条腿还活着）', () => {
    const html = render(screenNamed('settings/network'));
    expect(html).toContain('http://127.0.0.1:7890');
  });
});
