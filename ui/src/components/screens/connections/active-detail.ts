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

/**
 * M8 显示迟滞（2026-08-20）：连接表三个挥发性数值（上行速率/下行速率/累计流量）的显示值
 * 以上次显示为基准做粘滞——变化在阈值内不换值。
 *
 * 为什么在**数值层**而不是渲染层做：泵不在 React 重渲染（diff 未变的文本不写 DOM），而在
 * **每秒换串**：rate 是 Δcounter/Δt 的浮点，任何抖动（833→912 B/s）都会让 `fmtBytes` 产出
 * 新串 → 每秒一格 DOM 文本写 → WebKit 为高频重绘区域持续新建 graphics surface 且不及时回收
 * （.207/.152 归因：连接页 graphics dirty +111MB/min、region 数单调涨）。迟滞后空闲/低速
 * 抖动行的串完全稳定，DOM 写归零；真实变化（超阈）仍即时反映。副作用：排序读的也是粘滞值
 * ——次序抖动同步减少（行不再每秒换位重排），是收益不是代价。
 *
 * 阈值：相对 1/rel（速率 16≈6%、累计 64≈1.5%——对齐 fmtBytes 的 2-3 位有效数字粒度）+
 * 绝对 1 单位下限（压掉亚 B/s 的 keepalive 噪声）。基准是**上次显示值**而非真值：单调增长
 * 必然累积超阈后跳到真值，不会出现显示漂移。
 *
 * 纯函数无副作用；`fresh` 非有限数时保持显示值不变（数据异常不闪表）。
 */
export function stickyDisplay(shown: number, fresh: number, rel = 16): number {
  if (!Number.isFinite(fresh)) return shown;
  if (fresh === shown) return shown;
  const delta = Math.abs(fresh - shown);
  if (delta <= Math.max(1, Math.abs(shown) / rel)) return shown;
  return fresh;
}
