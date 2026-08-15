import { describe, expect, it } from 'vitest';
import type { ConnectionEntry, ConnectionsDetailUpdate } from '@/contracts/types';
import { applyActiveDetailUpdate, type ActiveDetailSync } from './active-detail';

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
