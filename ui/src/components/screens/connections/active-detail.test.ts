import { describe, expect, it } from 'vitest';
import type { ConnectionEntry, ConnectionsDetailUpdate } from '@/contracts/types';
import { applyActiveDetailUpdate, stickyDisplay, type ActiveDetailSync } from './active-detail';

const entry = (id: string, upload = 0, download = 0): ConnectionEntry => ({
  id,
  chains: ['direct'],
  rule: 'final',
  metadata: { host: `${id}.example` },
  upload,
  download,
});

const frame = (
  options: Partial<ConnectionsDetailUpdate> = {},
): ConnectionsDetailUpdate => ({
  reset: false,
  generation: 1,
  sequence: 1,
  connections: [],
  counters: [],
  removedIds: [],
  at: 1,
  ...options,
});

const state = () => ({
  index: new Map<string, ConnectionEntry>(),
  sync: { generation: null, sequence: 0 } satisfies ActiveDetailSync,
});

describe('活动连接增量索引', () => {
  it('必须先收到 reset 基线，孤立增量不会污染空索引', () => {
    const { index, sync } = state();
    const result = applyActiveDetailUpdate(
      index,
      sync,
      frame({ connections: [entry('orphan')] }),
    );
    expect(result.accepted).toBe(false);
    expect(index).toHaveLength(0);
    expect(sync.generation).toBeNull();
  });

  it('reset 取代旧代际，计数帧只替换对应对象', () => {
    const { index, sync } = state();
    applyActiveDetailUpdate(
      index,
      sync,
      frame({ reset: true, connections: [entry('a'), entry('b')], sequence: 1 }),
    );
    const stable = index.get('b');
    const result = applyActiveDetailUpdate(
      index,
      sync,
      frame({ sequence: 2, counters: [{ id: 'a', upload: 10, download: 20 }] }),
    );
    expect(result.accepted).toBe(true);
    expect(result.changedIds).toEqual(new Set(['a']));
    expect(index.get('a')).toMatchObject({ upload: 10, download: 20 });
    expect(index.get('b')).toBe(stable);
  });

  it('拒绝重复、乱序、旧代及没有 reset 的新代增量', () => {
    const { index, sync } = state();
    applyActiveDetailUpdate(
      index,
      sync,
      frame({ reset: true, connections: [entry('a')], sequence: 5 }),
    );
    for (const update of [
      frame({ sequence: 5, removedIds: ['a'] }),
      frame({ sequence: 4, removedIds: ['a'] }),
      frame({ generation: 0, sequence: 99, removedIds: ['a'] }),
      frame({ generation: 2, sequence: 1, removedIds: ['a'] }),
    ]) {
      expect(applyActiveDetailUpdate(index, sync, update).accepted).toBe(false);
      expect(index.has('a')).toBe(true);
    }
  });

  it('新代 reset 清除旧成员，后续删除按 id 生效', () => {
    const { index, sync } = state();
    applyActiveDetailUpdate(
      index,
      sync,
      frame({ reset: true, connections: [entry('old')], sequence: 1 }),
    );
    const reset = applyActiveDetailUpdate(
      index,
      sync,
      frame({ reset: true, generation: 2, sequence: 1, connections: [entry('new')] }),
    );
    expect(reset.removedIds).toEqual(new Set(['old']));
    expect([...index.keys()]).toEqual(['new']);

    const removed = applyActiveDetailUpdate(
      index,
      sync,
      frame({ generation: 2, sequence: 2, removedIds: ['new'] }),
    );
    expect(removed.removedIds).toEqual(new Set(['new']));
    expect(index).toHaveLength(0);
  });

  it('空增量心跳推进序列但保留全部对象引用', () => {
    const { index, sync } = state();
    applyActiveDetailUpdate(
      index,
      sync,
      frame({ reset: true, connections: [entry('a')], sequence: 1 }),
    );
    const stable = index.get('a');
    const heartbeat = applyActiveDetailUpdate(index, sync, frame({ sequence: 2, at: 2 }));
    expect(heartbeat.accepted).toBe(true);
    expect(heartbeat.changedIds.size).toBe(0);
    expect(index.get('a')).toBe(stable);
    expect(sync.sequence).toBe(2);
  });
});

// ── M8 显示迟滞（stickyDisplay）：泵在「每秒换串」，不在重渲染 ────────────────────

describe('M8 stickyDisplay 显示迟滞', () => {
  it('阈值内不换显示值（相对 1/16），超阈跳真值', () => {
    expect(stickyDisplay(833, 860)).toBe(833); // +3.2% ≤ 6.25% → 粘住
    expect(stickyDisplay(833, 912)).toBe(912); // +9.5% 超阈 → 跳
    expect(stickyDisplay(833, 800)).toBe(833); // -4% ≤ 6.25% → 粘住
    expect(stickyDisplay(1024, 1024)).toBe(1024); // 相等 → 原值
  });

  it('绝对 1 单位下限压掉亚单位噪声（keepalive 微流量）', () => {
    expect(stickyDisplay(0, 0.4)).toBe(0); // 亚 B/s 抖动不闪表
    expect(stickyDisplay(0, 2)).toBe(2); // 超过 1 → 显示
  });

  it('单调增长必然追上（基准是显示值，不会累积漂移）', () => {
    let shown = 100;
    for (let i = 0; i < 40; i++) shown = stickyDisplay(shown, 100 + i); // 每步 +1
    // 100→106 步步 ≤6.25%？106-100=6 ≤ 6.25 → 粘；继续到 +7 时 7>106/16=6.6 跳。最终必到 139。
    expect(shown).toBe(139);
  });

  it('rel 参数收紧累计流量的迟滞（1/64 对齐 fmtBytes 粒度）', () => {
    expect(stickyDisplay(100, 101.4, 64)).toBe(100); // 1.4 ≤ max(1, 1.5625)
    expect(stickyDisplay(100, 103, 64)).toBe(103); // 3 > 1.5625 → 跳
  });

  it('fresh 非有限数保持显示值（数据异常不闪表）', () => {
    expect(stickyDisplay(512, Number.NaN)).toBe(512);
    expect(stickyDisplay(512, Number.POSITIVE_INFINITY)).toBe(512);
  });
});
