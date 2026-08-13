import type { ServerGroup } from '@/domain/server-grouping';

/**
 * 进入节点页时应落在哪个 tab：**当前选中节点所在的分组**。
 *
 * # 为什么不是常量 'manual'
 *
 * 原实现 `useState('manual')` 把落地 tab 写死，只在「manual 组不存在」时才回落 `groups[0]`。于是订阅用户
 * 每次进节点页都落在空的「自建」上，要自己找回正在用的那个订阅——而「我现在用的是哪个节点」正是进这一页
 * 最常见的意图。
 *
 * # 边界
 *
 * - 选中 id 为空 / 找不到（刚删、`__direct__` 直连伪节点）→ 回落 `groups[0]`（保持既有落地行为，不空白）。
 * - `groups` 为空（store 尚未水合）→ 返回 `null`，调用方**不要**据此设 tab：此时定位没有信息量，
 *   设了等于把落地 tab 钉死在错的组上，等 groups 到齐反而没机会再定位。
 */
export function initialNodesTab(
  groups: ServerGroup[],
  selectedServerId: string | null | undefined
): string | null {
  if (groups.length === 0) return null;
  if (selectedServerId) {
    const owner = groups.find((g) => g.servers.some((s) => s.id === selectedServerId));
    if (owner) return owner.id;
  }
  return groups[0].id;
}
