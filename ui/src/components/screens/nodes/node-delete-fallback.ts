/**
 * 删除节点时的「兜底出口」计算（纯逻辑，供 NodesScreen 消费 + 单测）。
 *
 * 抽成独立模块而非留在 NodesScreen.tsx 里：这条计算是**流量裸奔的防线**，值得被单测钉住
 * （vitest 走 node 环境，不引 jsdom，故纯逻辑必须与组件文件分离——同 node-edit-routing 先例）。
 */

import type { ServerConfig } from '@/contracts/types';
import { pickFallbackExit } from '@/domain/direct-selection';
import { isServerComplete } from '@/domain/server-completeness';
import { meshNodeCarriesFullTunnel } from '@/domain/endpoint-routes';

/**
 * 兜底出口候选可用性谓词（忠实迁移 上游 `shared/fallback-exit.ts` `isViableFallbackExit`）：
 * 配置齐备（isServerComplete，内含 !isMeshNodeUnroutable）**且**承载全出网流量
 * （meshNodeCarriesFullTunnel：非组网节点恒真；WG allowInternet / TS exitNode 才真）。
 *
 * 关键（#291）：不过此谓词会静默选中不承载公网流量的节点——subnet-only 组网节点（WG allowInternet:false
 * 带具体网段 / TS 无 exitNode）字段齐备但只路由内网段，被选为主出口后公网流量走 direct = VPN 语义下的裸奔。
 */
export function isViableFallbackExit(server: ServerConfig): boolean {
  return isServerComplete(server) && meshNodeCarriesFullTunnel(server);
}

/**
 * D4（契约「删选中→兜底出口」+ `serverApi.delete` 自身文档「最快剩余节点」）：
 *  - 删的**不是**当前选中节点 → 返回 undefined（后端只在「删的是选中节点」时才查兜底，多传无意义）；
 *  - 删的**是**选中节点 → 从**剩余且可用**节点里按 `pickFallbackExit` 取最快（无任何正测速值则取候选首个）。
 *
 * 「可用」= `isViableFallbackExit`：先剔除配置不齐 / subnet-only 组网节点（否则 pickFallbackExit 的「无正延迟
 * 回退首个」会把这类节点当兜底 → 后端热重设后公网静默走 direct，#291 复现）。过滤须在 pickFallbackExit **之前**。
 * 无可用候选 → 返回 undefined，交后端落 DIRECT 哨兵 = **显式可见的直连**，而非静默裸奔。
 *
 * 修的 bug：调用点原先传的是 `selectedServerId` 本身，即**被删节点自己**——后端的 viable 校验（该 id 是否还在
 * 删后的 servers 里）恒为假 → 恒落 `DIRECT_SERVER_ID`。表现为「删掉当前节点 = 静默变直连」。
 *
 * `servers` 须按**列表序**传入（`pickFallbackExit` 的「无测速值回退第一个」= 用户看到的列表第一个）。
 * `latencies` 用渲染端测速流口径（null=超时 / undefined=未测）；`pickFallbackExit` 只收 `Record<string, number>`，
 * 故此处滤掉非数值——两种口径下超时/未测都不该被选为「最快」，滤掉后由其「回退首个」分支兜住。
 */
export function fallbackExitAfterDelete(
  servers: readonly ServerConfig[],
  selectedServerId: string | null | undefined,
  removedIds: ReadonlySet<string>,
  latencies: Readonly<Record<string, number | null | undefined>>
): string | undefined {
  if (!selectedServerId || !removedIds.has(selectedServerId)) return undefined;
  const candidates = servers.filter((s) => !removedIds.has(s.id) && isViableFallbackExit(s));
  const candidateIds = candidates.map((s) => s.id);
  const latencyMap: Record<string, number> = {};
  for (const id of candidateIds) {
    const v = latencies[id];
    if (typeof v === 'number') latencyMap[id] = v;
  }
  return pickFallbackExit(candidateIds, latencyMap) ?? undefined;
}
