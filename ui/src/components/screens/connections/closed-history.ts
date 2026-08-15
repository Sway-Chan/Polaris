import type {
  ClosedConnectionEntry,
  ConnectionsClosedUpdate,
} from '@/contracts/types';

/** 与后端 `MAX_CLOSED_HISTORY` 同一边界；前端仍自我裁剪，防异常帧撑破上限。 */
export const MAX_CLOSED_HISTORY = 1_000;

/**
 * 把一帧已结束增量并入按 id 持有的本地索引。
 *
 * `index` 是该订阅生命周期内的可变 ref；React state 只接收最后的有序数组。这样常态新增一条时，
 * 其余 999 条保持原对象引用，静态行投影缓存也能继续命中。
 */
export function applyClosedHistoryUpdate(
  index: Map<string, ClosedConnectionEntry>,
  update: ConnectionsClosedUpdate,
): ClosedConnectionEntry[] {
  if (update.reset) index.clear();
  for (const id of update.removedIds ?? []) index.delete(id);
  for (const item of update.connections) index.set(item.entry.id, item);

  const ordered = [...index.values()].sort((a, b) => b.closedAt - a.closedAt);
  if (ordered.length <= MAX_CLOSED_HISTORY) return ordered;

  for (const item of ordered.slice(MAX_CLOSED_HISTORY)) index.delete(item.entry.id);
  return ordered.slice(0, MAX_CLOSED_HISTORY);
}
