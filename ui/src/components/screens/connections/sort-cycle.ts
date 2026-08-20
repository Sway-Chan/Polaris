/** 表格排序三态：默认顺序 → 升序 → 降序 → 默认顺序。 */
export interface SortState<K> {
  key: K;
  dir: 1 | -1;
}

/**
 * 同列按三态循环；切换到另一列时从升序开始。
 * `null` 明确表示“不排序”，让调用方保留数据源自身的稳定顺序，而不是伪造一个默认 comparator。
 */
export function cycleSortState<K>(current: SortState<K> | null, key: K): SortState<K> | null {
  if (current?.key !== key) return { key, dir: 1 };
  if (current.dir === 1) return { key, dir: -1 };
  return null;
}
