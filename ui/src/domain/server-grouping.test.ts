/**
 * server-grouping 分组门 —— 守「空订阅没有 tab ⇒ 该订阅永远删不掉」这个缺陷的**根因**。
 *
 * 缺陷形态：订阅的 SubInfoBar / 「更多」菜单 / 删除入口全挂在它自己的 tab 上，而分组以
 * `subServers.length > 0` 为出组判据 ⇒ 节点被清空的订阅连同这些入口一起从 UI 消失，config 里
 * 却还留着它。空订阅正是最该被删掉的那一类，偏偏是唯一删不掉的。
 *
 * 断言的是**判据的分叉**（includeEmptyGroups 两侧行为不同），不是某个具体订阅名/顺序快照：
 * 节点列表页要空组（承载管理入口），节点选择器/托盘不要（那里空组只是噪音，也不承载入口）。
 */
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { groupServersBySubscription, defaultOpenGroupIds } from './server-grouping';
import { DIRECT_SERVER_ID } from './direct-selection';
import type { ServerConfig, SubscriptionConfig } from '../contracts/types';

const server = (id: string, over: Partial<ServerConfig> = {}): ServerConfig =>
  ({
    id,
    name: id,
    protocol: 'vmess',
    address: '10.0.0.1',
    port: 443,
    ...over,
  }) as ServerConfig;

const sub = (id: string, name = id): SubscriptionConfig => ({ id, name, url: `https://x/${id}` }) as SubscriptionConfig;

describe('groupServersBySubscription —— 空订阅的可达性', () => {
  it('includeEmptyGroups=true：节点数为 0 的订阅仍出组（否则它的删除入口不可达）', () => {
    const groups = groupServersBySubscription([], [sub('s1', '机场甲')], true);
    const g = groups.find((x) => x.id === 's1');
    expect(g, '空订阅被丢掉了 —— 该订阅将无 tab、无「更多」菜单、无删除入口').toBeDefined();
    expect(g!.name).toBe('机场甲');
    expect(g!.servers).toEqual([]);
  });

  it('includeEmptyGroups=false（节点选择器/托盘默认）：空订阅不出组', () => {
    const groups = groupServersBySubscription([], [sub('s1')], false);
    expect(groups.map((g) => g.id)).not.toContain('s1');
  });

  it('省略第三参 = false（选择器等消费方的既有调用形态不受影响）', () => {
    expect(groupServersBySubscription([], [sub('s1')]).map((g) => g.id)).not.toContain('s1');
  });

  it('非空订阅在两种模式下都出组，且只收自己的节点', () => {
    const servers = [server('a', { subscriptionId: 's1' }), server('b')];
    for (const flag of [true, false]) {
      const g = groupServersBySubscription(servers, [sub('s1')], flag).find((x) => x.id === 's1');
      expect(g!.servers.map((s) => s.id)).toEqual(['a']);
    }
  });

  it('顺序恒为 自建 → 组网 → 各订阅（空订阅插进来也不改这个次序）', () => {
    const groups = groupServersBySubscription([], [sub('s1'), sub('s2')], true);
    expect(groups.map((g) => g.id)).toEqual(['manual', 'mesh', 's1', 's2']);
  });

  it('孤儿节点（订阅已删）仍并入自建，不会因空订阅新规则被吞掉', () => {
    const groups = groupServersBySubscription([server('a', { subscriptionId: 'gone' })], [sub('s1')], true);
    expect(groups.find((g) => g.isManual)!.servers.map((s) => s.id)).toEqual(['a']);
    expect(groups.find((g) => g.id === 's1')!.servers).toEqual([]);
  });

  it('OpenConnect/OpenVPN 始终归组网；meshRoutes 只决定路由能力、不决定 UI 分组', () => {
    const groups = groupServersBySubscription([
      server('oc', { protocol: 'openconnect' }),
      server('ovpn', { protocol: 'openvpn-client', meshRoutes: ['10.10.0.0/16'] }),
      server('proxy'),
    ]);
    expect(groups.find((g) => g.id === 'mesh')!.servers.map((s) => s.id)).toEqual(['oc', 'ovpn']);
    expect(groups.find((g) => g.id === 'manual')!.servers.map((s) => s.id)).toEqual(['proxy']);
  });

  it('订阅归属优先：订阅下发的 endpoint 不会被抽到本地组网组', () => {
    const groups = groupServersBySubscription(
      [server('oc-sub', { protocol: 'openconnect', subscriptionId: 's1' })],
      [sub('s1')],
      true,
    );
    expect(groups.find((g) => g.id === 'mesh')!.servers).toEqual([]);
    expect(groups.find((g) => g.id === 's1')!.servers.map((s) => s.id)).toEqual(['oc-sub']);
  });
});

/**
 * defaultOpenGroupIds —— 三处节点选择器共用的「默认展开哪些组」。
 *
 * 断言的是**判据本身**（组件交互在这一层测不了：本仓 vitest 是 node 环境、无 jsdom），
 * 而判据正是缺陷所在：旧实现在「没选中任何节点」时退回 `groups[0]`，于是「默认折叠」在
 * 最需要它的场景恰恰不成立。下面第二条就是那条回落的靶子。
 */
describe('defaultOpenGroupIds —— 默认展开集', () => {
  const servers = [
    server('m1'),
    server('a1', { subscriptionId: 's1' }),
    server('b1', { subscriptionId: 's2' }),
    server('b2', { subscriptionId: 's2' }),
  ];
  const groups = groupServersBySubscription(servers, [sub('s1'), sub('s2')]);

  it('自检：样本确实是多组（单组时下面几条会退化成恒真）', () => {
    expect(groups.map((g) => g.id)).toEqual(['manual', 's1', 's2']);
  });

  it('只展开含选中节点的那一组，其余全折叠', () => {
    expect([...defaultOpenGroupIds(groups, 'b2')]).toEqual(['s2']);
    expect([...defaultOpenGroupIds(groups, 'm1')]).toEqual(['manual']);
  });

  it('没有选中节点 ⇒ 空集（全折叠，不许退回第一组）', () => {
    // 变异靶：给函数加回 `?? groups[0]?.id` 的回落，本条立刻转红。
    expect([...defaultOpenGroupIds(groups, undefined)]).toEqual([]);
    expect([...defaultOpenGroupIds(groups, null)]).toEqual([]);
    expect([...defaultOpenGroupIds(groups, '')]).toEqual([]);
  });

  it('选中项不属于任何组（直连哨兵 / 指向已删节点的残留 id）⇒ 空集', () => {
    expect([...defaultOpenGroupIds(groups, DIRECT_SERVER_ID)]).toEqual([]);
    expect([...defaultOpenGroupIds(groups, 'deleted-node')]).toEqual([]);
  });

  it('空分组表不炸（配置还没加载完时的首帧）', () => {
    expect([...defaultOpenGroupIds([], 'b2')]).toEqual([]);
  });

  it('返回的是**新集合**，调用方 toggle 时改的不是共享对象', () => {
    const a = defaultOpenGroupIds(groups, 'b2');
    const b = defaultOpenGroupIds(groups, 'b2');
    expect(a).not.toBe(b);
  });
});

/**
 * 四处选择器**真的共用这一份判据**的接线守卫。
 *
 * 2026-07-30 由三处扩到四处：**首页出口选单 `NodeMenu.tsx` 原是漏网的第四处** —— 66746e4 收口
 * 时它不在名单里，一直留着自己那份带 `groups[0]` 回落的局部实现。回落让「默认折叠」恰恰在最需要
 * 它的场景不成立（还没选节点、或选的是不属于任何组的直连/阻断哨兵时，一打开就有一组无关的铺开）。
 *
 * 为什么要源码结构断言：判据抽出来了但某一处仍留着自己那份局部实现，是纯逻辑单测永远抓不到的
 * 一类回归 —— 函数照样全绿，那一处照样展开错组。本仓既有守卫（config-read-wiring / tray-live-wiring）
 * 同款手法。
 */
describe('接线：四处节点选择器共用 defaultOpenGroupIds', () => {
  /** 去注释：本仓注释习惯逐字引用被删掉的旧形态（下面就要负向断言 `groups[0]?.id`）。 */
  const code = (src: string): string =>
    src.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/(^|[^:])\/\/.*$/gm, '$1');

  const CONSUMERS = [
    '../components/screens/apppolicy/AppPolicyScreen.tsx',
    '../components/dialogs/RuleDialog.tsx',
    '../tray/TrayMenu.tsx',
    '../components/screens/home/NodeMenu.tsx',
  ] as const;

  const sources = CONSUMERS.map((rel) => ({
    rel,
    src: code(readFileSync(fileURLToPath(new URL(rel, import.meta.url)), 'utf8')),
  }));

  it('自检：四个源文件都读到了且去注释后仍是可断言的代码', () => {
    for (const { rel, src } of sources) {
      expect(src.length, `${rel} 读空了 —— 被改名/移走了？`).toBeGreaterThan(2000);
      expect(src, `${rel} 去注释把源码吃光了`).toContain('import');
    }
  });

  it('四处都调用了 defaultOpenGroupIds（不是各写各的）', () => {
    for (const { rel, src } of sources) {
      expect(src, `${rel} 没在用共享判据`).toContain('defaultOpenGroupIds(');
    }
  });

  it('四处都不得留下 `groups[0]` 那条「没选中就展开第一组」的回落', () => {
    for (const { rel, src } of sources) {
      expect(src, `${rel} 又长回了第一组回落`).not.toMatch(/groups\[0\]\s*\??\.\s*id/);
    }
  });
});
