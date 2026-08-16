/**
 * 拓扑出口条配色门 —— 「不能谎报」的图形侧一条。
 *
 * 拓扑右列的 tag 直接来自 stats 帧里的 sing-box 出站名，所以「全局出口选阻断」与「应用分流 action=block」
 * 都表现为 `block` 这一条腿。它必须与正常代理条**可辨**：画成同色 = 界面表示流量在正常出网，而实际
 * 全被丢弃。此前 `isDirectOutbound` 只把 `direct` 判灰，`block` 落到 else 拿了普通代理的 flow 蓝。
 *
 * 只断言「三类颜色互不相同 + block 用错误色」，不钉死具体 HSL 变量名（那是配色实现细节）。
 */
import { describe, expect, it } from 'vitest';
import {
  collectLinkedIds,
  computeTopologyLayout,
  MAX_FLOW_THICKNESS,
  MAX_TOPOLOGY_SLOTS,
  topologySlotCapacity,
} from './topology-layout';
import type { ConnectionsAggregate } from '@/contracts/types';

/** 单 host、多出口的最小 aggregate（host 名不走 TOPOLOGY_OTHERS_KEY，故 t 不会被调用到）。 */
function aggregateWith(outbounds: string[]): ConnectionsAggregate {
  return {
    hosts: [
      {
        name: 'example.com',
        count: outbounds.length * 10,
        flows: outbounds.map((o) => ({ outbound: o, count: 10 })),
      },
    ],
    outbounds: outbounds.map((o) => ({ name: o, count: 10 })),
  } as unknown as ConnectionsAggregate;
}

function aggregateHosts(count: number): ConnectionsAggregate {
  return {
    hosts: Array.from({ length: count }, (_, index) => ({
      name: `target-${index}.example`,
      count: 1,
      flows: [{ outbound: 'HK', count: 1 }],
      recent: index >= Math.max(0, count - 5),
    })),
    outbounds: [{ name: 'HK', count }],
    total: count,
    at: 0,
  };
}

describe('视口容量与纵向对齐', () => {
  it('默认窗口高度档稳定在 16 槽，超出默认档后再随高度增长', () => {
    expect(topologySlotCapacity(301)).toBe(16);
    expect(topologySlotCapacity(303)).toBe(16);
    expect(topologySlotCapacity(320)).toBe(16);
    expect(topologySlotCapacity(337)).toBe(16);
    expect(topologySlotCapacity(357)).toBe(17);
    expect(topologySlotCapacity(425)).toBe(21);
    expect(topologySlotCapacity(250)).toBeLessThan(16);
    expect(topologySlotCapacity(0)).toBe(16);
  });

  it('最大化窗口把新增高度换成槽位，并在异常大画布上守住后端同款硬上限', () => {
    expect(topologySlotCapacity(730)).toBe(40);
    expect(topologySlotCapacity(766)).toBe(42);
    expect(topologySlotCapacity(730)).toBeGreaterThan(topologySlotCapacity(337));
    expect(topologySlotCapacity(100_000)).toBe(MAX_TOPOLOGY_SLOTS);
  });

  it('三列内容组共享同一纵向中心线，少量数据不会被拉伸铺满', () => {
    const result = computeTopologyLayout(aggregateWith(['HK', 'direct']), 800, (key) => key, 500);
    const centers = (type: 'source' | 'host' | 'outbound') => {
      const nodes = result.nodes.filter((node) => node.type === type);
      const top = Math.min(...nodes.map((node) => node.y));
      const bottom = Math.max(...nodes.map((node) => node.y + node.height));
      return (top + bottom) / 2;
    };
    expect(centers('source')).toBeCloseTo(centers('host'), 5);
    expect(centers('host')).toBeCloseTo(centers('outbound'), 5);
    expect(result.nodes.find((node) => node.type === 'host')?.height).toBeLessThanOrEqual(MAX_FLOW_THICKNESS);
  });

  it('画布增高只增加槽位，不把任何一列的单条流向加粗', () => {
    const compact = computeTopologyLayout(aggregateWith(['HK']), 800, (key) => key, 337);
    const maximized = computeTopologyLayout(aggregateWith(['HK']), 1400, (key) => key, 730);

    for (const result of [compact, maximized]) {
      expect(result.nodes).not.toHaveLength(0);
      expect(Math.max(...result.nodes.map((node) => node.height))).toBe(MAX_FLOW_THICKNESS);
    }
  });

  it('最大化运行态 40 槽满载时所有节点仍留在画布内', () => {
    const canvasHeight = 730;
    const result = computeTopologyLayout(aggregateHosts(40), 1400, (key) => key, canvasHeight);
    const hosts = result.nodes.filter((node) => node.type === 'host');

    expect(hosts).toHaveLength(40);
    expect(result.nodes.every((node) => node.y >= 0 && node.y + node.height <= canvasHeight)).toBe(true);
  });
});

function outboundColors(outbounds: string[]): Record<string, string> {
  const { nodes } = computeTopologyLayout(aggregateWith(outbounds), 600, (k) => k, 400);
  const out: Record<string, string> = {};
  for (const n of nodes) {
    if (n.type === 'outbound') out[n.name] = n.color;
  }
  return out;
}

describe('出口条配色', () => {
  /**
   * 变异锁：把 `outboundColor` 里 `if (isBlockedOutbound(name)) return COLOR_BLOCKED` 删掉 →
   * block 与 HK 同色 → 转红。
   */
  it('block / direct / 普通代理 三类颜色互不相同', () => {
    const c = outboundColors(['HK', 'direct', 'block']);
    expect(c.block).toBeTruthy();
    expect(c.block).not.toBe(c.HK);
    expect(c.block).not.toBe(c.direct);
    expect(c.direct).not.toBe(c.HK);
  });

  /**
   * 阻断用**警示色**而非错误色。`--err` 那条腿是刻意否掉的：阻断是用户主动选择，且应用分流的
   * 常驻 block 规则会让红条常驻、把「需要你注意」脱敏掉（判据见 `topology-layout.ts` 的
   * `COLOR_BLOCKED` 注释）。这条门同时钉住「不许退回 `--err`」与「不许复用 direct 的次要灰」。
   */
  it('block 用警示色（--warn），既不是错误色也不是 direct 的次要灰', () => {
    const c = outboundColors(['HK', 'direct', 'block']);
    expect(c.block).toContain('--warn');
    expect(c.block).not.toContain('--err');
    expect(c.direct).toContain('--fg-faint');
  });

  /** tag 大小写不敏感：sing-box 侧 tag 恒小写，但判据用 toLowerCase，别因大写漏判。 */
  it('大写 BLOCK 同样判为阻断色', () => {
    const c = outboundColors(['BLOCK']);
    expect(c.BLOCK).toContain('--warn');
  });

  /** 对照腿：普通节点名不得被误判成阻断（如名字里含 block 的节点）。 */
  it('名字里含 block 的普通节点不被误判', () => {
    const c = outboundColors(['block-hk-01']);
    expect(c['block-hk-01']).not.toContain('--warn');
  });
});

/**
 * 缎带身份门 —— 拓扑提频（后端 aggregate 节拍 1s → 250ms）的前置修复。
 *
 * links 的顺序继承自 stats-engine 侧的 count 降序，**每来一帧就可能整体重排**；而签名去重的语义恰恰是
 * 「只有内容变了才推帧」⇒ 推到前端的每一帧几乎都是排序可能变过的帧。身份若挂在数组下标上：
 *  - React 把同一个 `<path>` DOM 复用给语义完全不同的缎带 → `.hl` 类与 opacity .16s 过渡的中间态被继承；
 *  - hover 焦点（存的也是下标）在换帧后静默指向另一条链路。
 * 1s 一拍时是「偶尔跳一下」，4Hz 后就是常态 —— 故身份必须由内容决定。
 */
describe('缎带身份', () => {
  /** host 计数可控的双 host aggregate（计数决定 stats-engine 侧的排序，此处直接摆好模拟两种帧序）。 */
  function twoHosts(first: string, firstCount: number, second: string, secondCount: number) {
    return {
      hosts: [
        { name: first, count: firstCount, flows: [{ outbound: 'HK', count: firstCount }] },
        { name: second, count: secondCount, flows: [{ outbound: 'HK', count: secondCount }] },
      ],
      outbounds: [{ name: 'HK', count: firstCount + secondCount }],
    } as unknown as ConnectionsAggregate;
  }

  const layout = (agg: ConnectionsAggregate) => computeTopologyLayout(agg, 600, (k) => k, 400);

  /**
   * 变异锁：把 `TopoLink.id` 改回 `link-<下标>`（或任何含下标的表达式）⇒ 本测转红。
   * 判据是「同一条 (source,target) 缎带在两帧里 id 相同，尽管它的数组下标变了」。
   */
  it('缎带 id 由两端节点决定，帧间重排不改变身份', () => {
    const frameA = layout(twoHosts('a.com', 9, 'b.com', 1));
    const frameB = layout(twoHosts('b.com', 9, 'a.com', 1)); // 排序翻转 → 下标互换

    const idOf = (links: ReturnType<typeof layout>['links'], target: string) =>
      links.find((l) => l.target === target)?.id;

    const aId = idOf(frameA.links, 'mid-a.com');
    expect(aId).toBeTruthy();
    expect(idOf(frameB.links, 'mid-a.com')).toBe(aId);

    // 下标确实换了位置——否则本测没有区分力（下标身份也会「碰巧」通过）。
    const idxA = frameA.links.findIndex((l) => l.target === 'mid-a.com');
    const idxB = frameB.links.findIndex((l) => l.target === 'mid-a.com');
    expect(idxA).not.toBe(idxB);
  });

  /** 同一帧内 id 必须唯一，否则 React key 冲突（比下标身份更糟）。 */
  it('同一帧内缎带 id 唯一', () => {
    const { links } = layout(twoHosts('a.com', 5, 'b.com', 3));
    expect(new Set(links.map((l) => l.id)).size).toBe(links.length);
  });

  /** 高亮集收集的是同一套 id —— 两处若各用一套身份，hover 会点亮不了自己那条缎带。 */
  it('collectLinkedIds 收集的缎带 id 与 link.id 同一套', () => {
    const { links } = layout(twoHosts('a.com', 5, 'b.com', 3));
    const target = links[0];
    const set = collectLinkedIds(links, [target.source, target.target]);
    expect(set.has(target.id)).toBe(true);
  });
});
