import type { ConnectionEntry, ConnectionsDetailUpdate } from '@/contracts/types';

/** 最近一次已应用的活动连接代际/序列。null 表示尚未收到可信 reset 基线。 */
export interface ActiveDetailSync {
  generation: number | null;
  sequence: number;
}

export interface ActiveDetailApplyResult {
  accepted: boolean;
  reset: boolean;
  changedIds: Set<string>;
  removedIds: Set<string>;
}

/**
 * 原子应用一帧活动连接增量。新代际必须从 reset 开始；旧代、重复及乱序帧不会改动索引。
 * Map 保持连接初次出现的稳定顺序，计数更新不会让整张表随 LRU 次序跳动。
 */
export function applyActiveDetailUpdate(
  index: Map<string, ConnectionEntry>,
  sync: ActiveDetailSync,
  update: ConnectionsDetailUpdate,
): ActiveDetailApplyResult {
  const ignored: ActiveDetailApplyResult = {
    accepted: false,
    reset: false,
    changedIds: new Set(),
    removedIds: new Set(),
  };
  const currentGeneration = sync.generation;
  if (currentGeneration === null) {
    if (!update.reset) return ignored;
  } else if (update.generation < currentGeneration) {
    return ignored;
  } else if (update.generation > currentGeneration) {
    if (!update.reset) return ignored;
  } else if (update.sequence <= sync.sequence) {
    return ignored;
  }

  const removedIds = new Set<string>();
  if (update.reset) {
    for (const id of index.keys()) removedIds.add(id);
    index.clear();
  }

  const changedIds = new Set<string>();
  for (const entry of update.connections) {
    index.set(entry.id, entry);
    changedIds.add(entry.id);
    removedIds.delete(entry.id);
  }
  for (const counters of update.counters ?? []) {
    const current = index.get(counters.id);
    if (current === undefined) continue;
    if (current.upload === counters.upload && current.download === counters.download) continue;
    index.set(counters.id, {
      ...current,
      upload: counters.upload,
      download: counters.download,
    });
    changedIds.add(counters.id);
  }
  for (const id of update.removedIds ?? []) {
    if (index.delete(id)) removedIds.add(id);
    changedIds.delete(id);
  }

  sync.generation = update.generation;
  sync.sequence = update.sequence;
  return { accepted: true, reset: update.reset, changedIds, removedIds };
}

export function clearActiveDetailState(
  index: Map<string, ConnectionEntry>,
  sync: ActiveDetailSync,
): void {
  index.clear();
  sync.generation = null;
  sync.sequence = 0;
}
