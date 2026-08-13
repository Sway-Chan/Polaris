/**
 * 节点屏纯逻辑单测 —— 锁死「UI 显示态 ↔ 后端实际行为」的对齐点。
 *
 * 每个 describe 头注明**变异锁**：把被测判定改回错误写法后，哪条断言会转红。
 */

import { describe, it, expect } from 'vitest';
import {
  subAutoUpdateStatus,
  subEffectiveIntervalHours,
  SUB_DEFAULT_INTERVAL_HOURS,
  speedTestIdsForSelection,
  nodeUseAction,
  speedTestBlockReason,
  invalidNodeIndex,
  moveToGroupTargets,
  canMoveToGroup,
  warpCardAction,
  subDeleteNodeCount,
  subUsage,
  SUB_USAGE_WARN_PCT,
} from './nodes-logic';
import { isSpeedTestable } from '@/domain/endpoint-routes';
import type { ServerConfig } from '@/contracts/types';

/** 最小可用节点（默认 vmess = 结构上可测），按需覆盖协议/组网字段。 */
const srv = (id: string, extra: Partial<ServerConfig> = {}): ServerConfig =>
  ({ id, name: id, protocol: 'vmess', address: 'example.com', port: 443, ...extra }) as ServerConfig;

describe('subAutoUpdateStatus —— per-sub 开关开 ≠ 真会周期刷新', () => {
  const on = { autoUpdate: true };
  const off = { autoUpdate: false };

  it('per-sub 关 → manual（不管全局怎么设）', () => {
    expect(subAutoUpdateStatus(off, { autoUpdateSubscriptionOnStart: true })).toBe('manual');
    expect(subAutoUpdateStatus({}, { autoUpdateSubscriptionOnStart: true })).toBe('manual');
    // 缺省（老配置无该键）也算关：后端 `!= Some(true)` 严格真才跑。
    expect(subAutoUpdateStatus({ autoUpdate: undefined }, {})).toBe('manual');
  });

  it('per-sub 开但总开关关 → master-off（旧代码会显绿点说在自动刷新）', () => {
    expect(subAutoUpdateStatus(on, { autoUpdateSubscriptionOnStart: false })).toBe('master-off');
    // 总开关缺省 = 关（后端 `!= Some(true)`）——若这里写成 `!== false` 会误判成 active。
    expect(subAutoUpdateStatus(on, {})).toBe('master-off');
    expect(subAutoUpdateStatus(on, null)).toBe('master-off');
  });

  it('间隔 0（仅手动）→ startup-only：周期腿不跑，但启动补更腿仍会跑', () => {
    expect(
      subAutoUpdateStatus(on, {
        autoUpdateSubscriptionOnStart: true,
        subscriptionUpdateIntervalHours: 0,
      }),
    ).toBe('startup-only');
  });

  it('三门全通 → active（唯一给绿点的态）', () => {
    expect(
      subAutoUpdateStatus(on, {
        autoUpdateSubscriptionOnStart: true,
        subscriptionUpdateIntervalHours: 24,
      }),
    ).toBe('active');
    // 间隔缺省 → 后端回落 12h，照跑。
    expect(subAutoUpdateStatus(on, { autoUpdateSubscriptionOnStart: true })).toBe('active');
  });
});

describe('subEffectiveIntervalHours —— 徽标不再写死 12h', () => {
  it('正数原样用', () => {
    expect(subEffectiveIntervalHours({ subscriptionUpdateIntervalHours: 24 })).toBe(24);
    expect(subEffectiveIntervalHours({ subscriptionUpdateIntervalHours: 6 })).toBe(6);
  });
  it('缺省 / 非法 → 回落 12h（对齐后端 filter(>0).unwrap_or(12)）', () => {
    expect(subEffectiveIntervalHours({})).toBe(SUB_DEFAULT_INTERVAL_HOURS);
    expect(subEffectiveIntervalHours(null)).toBe(SUB_DEFAULT_INTERVAL_HOURS);
    expect(subEffectiveIntervalHours({ subscriptionUpdateIntervalHours: -1 })).toBe(12);
    expect(subEffectiveIntervalHours({ subscriptionUpdateIntervalHours: Number.NaN })).toBe(12);
    // 0 = 「仅手动」：不参与徽标数字（状态已是 startup-only），回落值只是兜底不炸。
    expect(subEffectiveIntervalHours({ subscriptionUpdateIntervalHours: 0 })).toBe(12);
  });
});

describe('speedTestIdsForSelection —— 测所选，不是测整组；且过 isSpeedTestable', () => {
  const visible = [srv('a'), srv('b'), srv('c')];

  it('只返回被选中的 id（变异锁：改回整组 → 本断言转红）', () => {
    expect(speedTestIdsForSelection(visible, new Set(['b']))).toEqual(['b']);
    expect(speedTestIdsForSelection(visible, new Set(['a', 'c']))).toEqual(['a', 'c']);
    // 关键否定断言：选 1 个绝不能吐出整组三个。
    expect(speedTestIdsForSelection(visible, new Set(['b']))).not.toEqual(['a', 'b', 'c']);
  });

  it('保持可见顺序（与批删/批量复制链接同口径）', () => {
    expect(speedTestIdsForSelection(visible, new Set(['c', 'a']))).toEqual(['a', 'c']);
  });

  it('已选但已被筛选移出视野的 id 不参与（避免测用户看不见的节点）', () => {
    expect(speedTestIdsForSelection([srv('a')], new Set(['a', 'zzz']))).toEqual(['a']);
  });

  it('空选 → 空数组（调用方据此禁用按钮，不发空测速请求）', () => {
    expect(speedTestIdsForSelection(visible, new Set())).toEqual([]);
  });

  it('勾中了结构上不可测的节点 → 剔除（变异锁：去掉 speedTestableIds 这层 → 本条转红）', () => {
    // reverseMesh 的 WG：dial 走 OS default，测出的是直连假好值，不能挂在组网节点名下。
    const list = [
      srv('ok'),
      srv('rev', { protocol: 'wireguard', wireguardSettings: { reverseMesh: true } } as Partial<ServerConfig>),
    ];
    expect(speedTestIdsForSelection(list, new Set(['ok', 'rev']))).toEqual(['ok']);
  });

  it('caps 透传：TS-exit 仅主核池可用时才纳入（path-aware，与首页同口径）', () => {
    const list = [
      srv('ok'),
      srv('ts', { protocol: 'tailscale', tailscaleSettings: { exitNode: 'node-x' } } as Partial<ServerConfig>),
    ];
    const all = new Set(['ok', 'ts']);
    expect(speedTestIdsForSelection(list, all, { mainCorePool: false })).toEqual(['ok']);
    expect(speedTestIdsForSelection(list, all, { mainCorePool: true })).toEqual(['ok', 'ts']);
  });
});

describe('speedTestBlockReason —— 置灰要给得出理由，且与 isSpeedTestable 严格同步', () => {
  const caps = { mainCorePool: true };

  it('可测节点 → null（不变量：null ⟺ isSpeedTestable）', () => {
    expect(speedTestBlockReason(srv('a'), caps)).toBeNull();
    expect(
      speedTestBlockReason(
        srv('wg', { protocol: 'wireguard', wireguardSettings: {} } as Partial<ServerConfig>),
        caps
      )
    ).toBeNull();
  });

  it('reverseMesh（System 内核接口）→ system-interface（WG / TS 两族同判）', () => {
    expect(
      speedTestBlockReason(
        srv('w', { protocol: 'wireguard', wireguardSettings: { reverseMesh: true } } as Partial<ServerConfig>),
        caps
      )
    ).toBe('system-interface');
    // TS 即使配了出口，reverseMesh 也优先命中（分支序必须镜像 isSpeedTestable）。
    expect(
      speedTestBlockReason(
        srv('t', {
          protocol: 'tailscale',
          tailscaleSettings: { exitNode: 'x', reverseMesh: true },
        } as Partial<ServerConfig>),
        caps
      )
    ).toBe('system-interface');
  });

  it('TS 无 exitNode → ts-no-exit；WG 关「允许访问外网」→ lan-only（两族理由不同，不得混说）', () => {
    expect(speedTestBlockReason(srv('t', { protocol: 'tailscale' } as Partial<ServerConfig>), caps)).toBe(
      'ts-no-exit'
    );
    expect(
      speedTestBlockReason(
        srv('w', {
          protocol: 'wireguard',
          wireguardSettings: { allowInternet: false },
        } as Partial<ServerConfig>),
        caps
      )
    ).toBe('lan-only');
  });

  it('TS-exit 但主核池不可用（代理没跑）→ ts-core-not-ready，而不是谎称"没有出口"', () => {
    const ts = srv('t', {
      protocol: 'tailscale',
      tailscaleSettings: { exitNode: 'node-x' },
    } as Partial<ServerConfig>);
    expect(speedTestBlockReason(ts, { mainCorePool: false })).toBe('ts-core-not-ready');
    expect(speedTestBlockReason(ts, caps)).toBeNull();
  });

  it('custom endpoint → custom-endpoint', () => {
    expect(
      speedTestBlockReason(
        srv('c', { protocol: 'custom', customSettings: { isEndpoint: true } } as Partial<ServerConfig>),
        caps
      )
    ).toBe('custom-endpoint');
  });

  it('不变量矩阵：null ⟺ isSpeedTestable（任一侧被单独改动都会在此转红）', () => {
    const matrix: ServerConfig[] = [
      srv('plain'),
      srv('wg', { protocol: 'wireguard', wireguardSettings: {} } as Partial<ServerConfig>),
      srv('wg-lan', { protocol: 'wireguard', wireguardSettings: { allowInternet: false } } as Partial<ServerConfig>),
      srv('wg-rev', { protocol: 'wireguard', wireguardSettings: { reverseMesh: true } } as Partial<ServerConfig>),
      srv('ts-mesh', { protocol: 'tailscale' } as Partial<ServerConfig>),
      srv('ts-exit', { protocol: 'tailscale', tailscaleSettings: { exitNode: 'x' } } as Partial<ServerConfig>),
      srv('ts-rev', {
        protocol: 'tailscale',
        tailscaleSettings: { exitNode: 'x', reverseMesh: true },
      } as Partial<ServerConfig>),
      srv('cus', { protocol: 'custom', customSettings: { isEndpoint: true } } as Partial<ServerConfig>),
      srv('cus-plain', { protocol: 'custom', customSettings: {} } as Partial<ServerConfig>),
    ];
    for (const c of [{ mainCorePool: true }, { mainCorePool: false }, undefined]) {
      for (const s of matrix) {
        expect(
          speedTestBlockReason(s, c) === null,
          `${s.id} caps=${JSON.stringify(c)}：理由与可测性判定分叉`
        ).toBe(isSpeedTestable(s, c));
        // 第三个入参不传 / 传 false ⇒ 与今天**逐字节**相同（总开关关着时 stagedOnly 恒空，
        // 走的就是这条腿）。变异对照：把 `if (stagedOnly)` 写成 `if (stagedOnly !== undefined)`，
        // 本断言立刻红 —— 全部节点会一律置灰。
        expect(speedTestBlockReason(s, c, false)).toBe(speedTestBlockReason(s, c));
      }
    }
  });

  it('staged-only 先于结构可测性判（不变量扩成 null ⟺ isSpeedTestable ∧ ¬stagedOnly）', () => {
    // 一个结构上完全可测的 VLESS 节点，只是盘上还没有它 —— 理由必须是「还没保存」，
    // 而不是「不支持测速」那种与事实无关的解释。
    expect(speedTestBlockReason(srv('plain'), { mainCorePool: true })).toBeNull();
    expect(speedTestBlockReason(srv('plain'), { mainCorePool: true }, true)).toBe('staged-only');
    // 结构上本就不可测的节点，staged-only 仍优先 —— 用户先要解决的是「保存」这件事。
    expect(
      speedTestBlockReason(
        srv('wg-lan', {
          protocol: 'wireguard',
          wireguardSettings: { allowInternet: false },
        } as Partial<ServerConfig>),
        { mainCorePool: true },
        true
      )
    ).toBe('staged-only');
  });
});

describe('invalidNodeIndex —— 节点卡消费 gate 剔除信息', () => {
  it('建 id → reason 索引', () => {
    const idx = invalidNodeIndex([
      { id: 's1', tag: 'proxy-s1', reason: 'detour-cascade' },
      { id: 's2', tag: 'proxy-s2', reason: 'bad config' },
    ]);
    expect(idx.s1).toBe('detour-cascade');
    expect(idx.s2).toBe('bad config');
    expect(idx.s3).toBeUndefined();
  });

  it('空 / null / undefined → 空索引（不炸）', () => {
    expect(invalidNodeIndex([])).toEqual({});
    expect(invalidNodeIndex(null)).toEqual({});
    expect(invalidNodeIndex(undefined)).toEqual({});
  });

  it('reason 缺失仍建索引（置灰照做，tooltip 由消费方兜底）', () => {
    const idx = invalidNodeIndex([{ id: 's1', tag: 't' } as never]);
    expect('s1' in idx).toBe(true);
    expect(idx.s1).toBe('');
  });
});

describe('moveToGroupTargets —— 诚实置灰而非假接线', () => {
  it('无可选目标分组 → 按钮不可用', () => {
    expect(moveToGroupTargets()).toEqual([]);
    expect(canMoveToGroup()).toBe(false);
  });
});

describe('warpCardAction —— 已注册后是管理菜单，不是直接开编辑弹窗', () => {
  it('已注册 → menu；未注册 → register', () => {
    expect(warpCardAction(true)).toBe('menu');
    expect(warpCardAction(false)).toBe('register');
  });
});

describe('subDeleteNodeCount —— 删订阅要报出连带移除的节点数', () => {
  const servers = [
    { subscriptionId: 'sub1' },
    { subscriptionId: 'sub1' },
    { subscriptionId: 'sub2' },
    {},
  ];
  it('只数该订阅下的节点', () => {
    expect(subDeleteNodeCount(servers, { id: 'sub1' })).toBe(2);
    expect(subDeleteNodeCount(servers, { id: 'sub2' })).toBe(1);
    expect(subDeleteNodeCount(servers, { id: 'nope' })).toBe(0);
  });
});

describe('subUsage —— 用量条阈值是契约数字（≥85% warn），不是随手写的 80', () => {
  const ui = (upload: number, download: number, total: number) => ({ upload, download, total });

  it('阈值常量对齐契约 / 上游 server-page.tsx', () => {
    // 变异锁：改回 80 → 本条 + 下面的 84.9% 那条同时转红。
    expect(SUB_USAGE_WARN_PCT).toBe(85);
  });

  it('84.9% 不告警、85% 起告警（边界含等号）', () => {
    expect(subUsage(ui(0, 849, 1000)).warn).toBe(false);
    expect(subUsage(ui(0, 850, 1000)).warn).toBe(true);
    // 此前写 80：81% 会误告警。
    expect(subUsage(ui(0, 810, 1000)).warn).toBe(false);
  });

  it('used = upload + download；百分比封顶 100（超售不得溢出进度条）', () => {
    const u = subUsage(ui(300, 200, 1000));
    expect(u.used).toBe(500);
    expect(u.total).toBe(1000);
    expect(u.pct).toBe(50);
    expect(subUsage(ui(0, 5000, 1000)).pct).toBe(100);
  });

  it('机场不下发总量（total 缺/0）→ 百分比 0、不告警（不得除零成 NaN/Infinity）', () => {
    expect(subUsage(undefined)).toEqual({ used: 0, total: 0, pct: 0, warn: false });
    expect(subUsage(ui(10, 10, 0))).toEqual({ used: 20, total: 0, pct: 0, warn: false });
  });
});

/**
 * 变异锁：去掉 `noop` 那条 → 第 1 条转红（重选当前出口会白弹一条「已切换」）；
 * 让整卡也直切 → 第 3 条转红（整卡误触即当场改全局出口）；
 * 让按钮也确认 → 第 4 条转红（高频动作被收确认税）；
 * 把 `willRestart` 的判定挪到 `via` 之后 → 第 2 条的按钮腿转红（断连接那次必须两个面都确认）。
 */
describe('nodeUseAction —— 设为出口按触发面分档', () => {
  it('已是当前出口 → noop（后端重选同一节点是空操作，不该弹 toast）', () => {
    expect(nodeUseAction('a', 'a', false, 'card')).toBe('noop');
    expect(nodeUseAction('a', 'a', false, 'button')).toBe('noop');
    // 即便它同时在待应用差集里，也仍是 noop —— 没发生切换就没有重启。
    expect(nodeUseAction('a', 'a', true, 'card')).toBe('noop');
  });

  it('在待应用差集里 → confirm-restart，**两个触发面都要**（代价是断连接，与命中面无关）', () => {
    expect(nodeUseAction('b', 'a', true, 'card')).toBe('confirm-restart');
    expect(nodeUseAction('b', 'a', true, 'button')).toBe('confirm-restart');
  });

  it('整卡 + 普通节点 → confirm（最大命中面，误触即改全局出口且用户未必察觉）', () => {
    expect(nodeUseAction('b', 'a', false, 'card')).toBe('confirm');
    expect(nodeUseAction('b', null, false, 'card')).toBe('confirm');
    expect(nodeUseAction('b', undefined, false, 'card')).toBe('confirm');
  });

  it('显式按钮 + 普通节点 → switch（命中面小、语义明确，不收确认税）', () => {
    expect(nodeUseAction('b', 'a', false, 'button')).toBe('switch');
    expect(nodeUseAction('b', null, false, 'button')).toBe('switch');
  });
});
