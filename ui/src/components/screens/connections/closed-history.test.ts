import { describe, expect, it } from 'vitest';
import type { ClosedConnectionEntry, ConnectionsClosedUpdate } from '@/contracts/types';
import { applyClosedHistoryUpdate, MAX_CLOSED_HISTORY } from './closed-history';

const item = (id: string, closedAt: number): ClosedConnectionEntry => ({
  entry: { id, chains: [], rule: '' },
  closedAt,
});

const frame = (
  connections: ClosedConnectionEntry[],
  options: Partial<ConnectionsClosedUpdate> = {},
): ConnectionsClosedUpdate => ({
  reset: false,
  connections,
  removedIds: [],
  at: 1,
  ...options,
});

describe('已结束历史增量合并', () => {
  it('reset 取代全部旧历史并按结束时间降序', () => {
    const index = new Map([['old', item('old', 1)]]);
    const result = applyClosedHistoryUpdate(
      index,
      frame([item('a', 10), item('b', 20)], { reset: true }),
    );
    expect(result.map((x) => x.entry.id)).toEqual(['b', 'a']);
    expect(index.has('old')).toBe(false);
  });

  it('常态只 upsert 变更项，未变条目保持原对象引用', () => {
    const stable = item('stable', 10);
    const index = new Map([[stable.entry.id, stable]]);
    const result = applyClosedHistoryUpdate(index, frame([item('new', 20)]));
    expect(result[1]).toBe(stable);
    expect(result.map((x) => x.entry.id)).toEqual(['new', 'stable']);
  });

  it('同帧淘汰与 upsert 以最新条目为准', () => {
    const index = new Map([
      ['a', item('a', 10)],
      ['b', item('b', 20)],
    ]);
    const result = applyClosedHistoryUpdate(
      index,
      frame([item('a', 30)], { removedIds: ['a', 'b'] }),
    );
    expect(result.map((x) => x.entry.id)).toEqual(['a']);
    expect(result[0].closedAt).toBe(30);
  });

  it('异常超长帧仍裁剪到 1000，索引不留被淘汰项', () => {
    const index = new Map<string, ClosedConnectionEntry>();
    const incoming = Array.from({ length: MAX_CLOSED_HISTORY + 2 }, (_, i) =>
      item(`c${i}`, i),
    );
    const result = applyClosedHistoryUpdate(index, frame(incoming, { reset: true }));
    expect(result).toHaveLength(MAX_CLOSED_HISTORY);
    expect(index).toHaveLength(MAX_CLOSED_HISTORY);
    expect(index.has('c0')).toBe(false);
    expect(index.has('c1')).toBe(false);
  });
});
