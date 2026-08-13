import type { PendingNodeChanges } from '@/contracts/types';

/**
 * 选中「待入池(added)/待生效(modified)」节点时是否将触发自动整核重启。
 * 移植自 上游 `src/renderer/components/home/pending-select-hint.ts`（能力契约 §主页 Home「出口节点选择」）。
 *
 * 待应用差集里的 added/modified 均为**未被引用**节点（差集计算已按 referencedServerIds 过滤掉被引用者）。
 * 一旦选中其一 → 该节点从「未引用」变「被引用(selected)」→ 恒立即整核重启（不受 restartOnNodeChange 开关）
 * → 差集瞬态清空 → 待应用动作条无声卸载。据此在选节点入口给一次性提示，解释「动作条为何随即消失」。
 *
 * 核未运行时 pendingChanges 恒空 → 返回 false，不误报。
 *
 * **刻意不看 `removed`**：`removed` 里的节点已经不在 config 的 servers 里了，出口选单根本列不出它，
 * 这个函数永远不会被拿它的 id 调用。把它并进来是死代码，还会误导后来者以为「能选一个已删的节点」。
 */
export function willRestartOnSelect(
  pending: PendingNodeChanges,
  serverId: string
): boolean {
  return !!pending?.added?.includes(serverId) || !!pending?.modified?.includes(serverId);
}
