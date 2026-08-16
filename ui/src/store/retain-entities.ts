/**
 * 配置实体拥有的派生缓存对账工具。
 *
 * 常态是配置实体未删除，因此无变化必须返回原引用；只有发现孤儿键时才分配新容器。这样既释放陈旧
 * 数据，也不把每次 configChanged 变成全局 Zustand 通知或 localStorage 重写。
 */

export function retainRecordKeys<T>(
  record: Record<string, T>,
  keep: ReadonlySet<string>
): Record<string, T> {
  let removed = false;
  for (const key of Object.keys(record)) {
    if (!keep.has(key)) {
      removed = true;
      break;
    }
  }
  if (!removed) return record;

  const next: Record<string, T> = {};
  for (const [key, value] of Object.entries(record)) {
    if (keep.has(key)) next[key] = value;
  }
  return next;
}

export function retainItemsById<T extends { id: string }>(
  items: T[],
  keep: ReadonlySet<string>
): T[] {
  if (items.every((item) => keep.has(item.id))) return items;
  return items.filter((item) => keep.has(item.id));
}
