import { describe, expect, it } from 'vitest';
import { retainItemsById, retainRecordKeys } from './retain-entities';

describe('配置实体缓存对账', () => {
  it('无孤儿时保持 record 原引用', () => {
    const record = { a: 1, b: 2 };
    expect(retainRecordKeys(record, new Set(['a', 'b', 'c']))).toBe(record);
  });

  it('只保留仍存在的 record 键', () => {
    const record = { a: 1, deleted: 2 };
    expect(retainRecordKeys(record, new Set(['a']))).toEqual({ a: 1 });
  });

  it('数组无孤儿保持引用，有孤儿才过滤', () => {
    const items = [{ id: 'a', value: 1 }, { id: 'deleted', value: 2 }];
    expect(retainItemsById(items, new Set(['a', 'deleted']))).toBe(items);
    expect(retainItemsById(items, new Set(['a']))).toEqual([{ id: 'a', value: 1 }]);
  });
});
