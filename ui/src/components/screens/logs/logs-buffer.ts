/**
 * 日志缓冲的两条入口（水合快照 / 流式增量）的合并语义 —— 纯函数，供 `LogsScreen` 接线。
 *
 * # 为什么水合不能复用流式那套「游标去重 + 整体替换」
 *
 * 两条腿在挂载时**同时**起跑：`logs.get` 是异步的，而订阅腿可能在它 resolve 之前就送来第一批。
 * 那一批把游标 `lastId` 推到 N ⇒ 随后到达的快照全部 `_id ≤ N` ⇒ 被游标去重成空 ⇒
 * 而水合写的是 `setLogs(...)`（**整体替换**）⇒ 已经入列的行被一并清掉。
 * 用户侧表现：**核在高频输出日志时进日志页，历史区恒为空**，直到下一条新行到达才开始有内容。
 * （2026-07-28 独立复审抓出。）
 *
 * 根因是「游标」这个数据结构回答不了水合要问的问题：游标只知道「见过的最大 id」，而水合要问的是
 * 「这条具体的行在不在列里」。故水合走**集合去重 + 合并**，游标只作为**流式腿**的快进标记同步推进
 * （不推进的话，快照里比游标新的那几行会被随后的流式批当成新行再收一次 = 重复行）。
 */

import type { LogEntry } from '@/contracts/types';

/**
 * 渲染端日志行 = 后端 `LogEntry` + 单调 `_id`（环形缓冲 seq，`misc.rs::log_record_to_entry` 带出）。
 *
 * 用它做 **key**：环形缓冲滑动（丢最旧）后剩余行的 key 不变。退化成 `timestamp-index` 时首元素一淘汰，
 * 后面每行的 index 全体前移 → React 认定整列换了身份，滚动期全量重渲并打断文本选区。
 *
 * 用它做 **去重键**：后端 emitter 是单例，第二次进本页时 `logs.get` 的水合快照与 emitter 下一 tick 的
 * 增量有个 ≤150ms 的重叠窗，同一条日志会到两次。
 *
 * 可选（`?`）+ 就地交集类型而非改 `contracts/types`：缺字段时必须**放行而不是吞掉日志**，可选正好
 * 承载这个回落语义。契约类型补 `_id?: number` 属后续清理。
 */
export type LogRow = LogEntry & { _id?: number };

/** 排序键：缺 `_id` 的行（非 Tauri mock / 旧后端）排到末尾，保持「最新在下」的阅读方向。 */
function orderKey(l: LogRow): number {
  return typeof l._id === 'number' ? l._id : Number.MAX_SAFE_INTEGER;
}

/**
 * 把水合快照并进当前缓冲。
 *
 * - **合并而非替换**：`prev` 里可能已有订阅腿先送到的行，替换会把它们清掉（本模块头描述的缺陷）。
 * - 去重按 `_id` 集合（`prev` 已有的 id 不再收）；缺 `_id` 的快照行一律放行 —— 与流式腿同款取向：
 *   宁可偶有重复行，也不能因为字段缺失把日志吞掉，日志页是排障的最后一根线。
 * - 合并后按 `_id` 升序（两条腿各自单调，交错到达后只有排序能恢复真实时序），再截尾到 `max`。
 * - 快照没带来任何新行时**返回原数组**（引用不变 ⇒ React 不做无谓重渲）。
 */
export function mergeHydration(prev: LogRow[], snapshot: LogRow[], max: number): LogRow[] {
  const seen = new Set<number>();
  for (const l of prev) {
    if (typeof l._id === 'number') seen.add(l._id);
  }
  const fresh = snapshot.filter((l) => typeof l._id !== 'number' || !seen.has(l._id));
  if (fresh.length === 0) return prev;
  if (prev.length === 0) return fresh.length > max ? fresh.slice(-max) : fresh;
  // Array.prototype.sort 自 ES2019 起稳定 ⇒ 同键（皆缺 _id）的行保持相对次序。
  const merged = [...prev, ...fresh].sort((a, b) => orderKey(a) - orderKey(b));
  return merged.length > max ? merged.slice(-max) : merged;
}

/**
 * 快照里的最大 `_id`（没有可用 id 时返 `null`）—— 水合后用它**快进流式游标**。
 *
 * 少了这一步：快照里那些「后端已记录、emitter 还没推」的行会被随后的流式批当成新行再收一次。
 */
export function maxLogId(batch: LogRow[]): number | null {
  let max: number | null = null;
  for (const l of batch) {
    if (typeof l._id === 'number' && (max === null || l._id > max)) max = l._id;
  }
  return max;
}
