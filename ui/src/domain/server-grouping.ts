import type { ServerConfig, SubscriptionConfig } from '../contracts/types';
import { landsInEndpoints } from './endpoint-routes';

/**
 * 节点分组（自建 + 组网 + 各订阅），供节点选择器 / 节点列表页 tab / 任意需要「分组」展示处共用，
 * **单一真值**——杜绝各处各自分组导致「列表页归组网、下拉归自建」的口径漂移。
 */
export interface ServerGroup {
  /** 'manual' / 'mesh' / 订阅 id */
  id: string;
  /** 订阅组=订阅名（直接展示）；自建/组网组=占位 'manual'/'mesh'，**消费方一律按 isManual/isMesh 本地化、不读 name**。 */
  name: string;
  isManual: boolean;
  /** 组网组（WireGuard/WARP/Tailscale 等 endpoint 协议）：调用方本地化为「组网」。 */
  isMesh?: boolean;
  servers: ServerConfig[];
}

/**
 * 把节点按归属分组：自建（无 subscriptionId，或 subscriptionId 指向已删订阅的孤儿）→ 再拆出**组网**
 * （endpoint 协议 WireGuard/WARP/Tailscale/OpenConnect/OpenVPN）独立成组，置于自建之后；
 * 其后每个订阅一组。这里判的是 UI 产品归属，不是「是否配置了内网路由」：OpenConnect/OpenVPN
 * 即使未填 `meshRoutes` 也仍是网络接入，只是作为普通 VPN 出口使用。
 * 顺序：自建 → 组网 → 各订阅（与订阅入参一致），与节点列表页 tab 顺序一致。
 * 空组是否保留由 `includeEmptyGroups` 决定（见该参数注释）。
 */
export function groupServersBySubscription(
  servers: ServerConfig[],
  subscriptions: SubscriptionConfig[] = [],
  /**
   * true = 即便无节点也保留空组（节点列表页用）：
   *  - 「自建」+「组网」两个常驻主 tab——节点列表页需常驻承载接入引导，且「自建」是默认落地 tab，
   *    无节点时也必须在，否则空态只剩「组网·0」、丢了自建入口（真机实测确认）；
   *  - **节点数为 0 的订阅**——订阅的 SubInfoBar / 「更多」菜单 / 删除入口全都挂在它自己的 tab 上，
   *    以 `length > 0` 为出组判据会让「节点被清空的订阅」连同这些入口一起消失 ⇒ **该订阅再也删不掉**
   *    （订阅本身还在 config 里，只是 UI 无从触达）。空订阅正是最需要删除入口的那一类。
   *
   * 节点选择器 / 托盘等「选一个节点」的消费方默认 false——那里空组只是噪音，且它们不承载管理入口。
   */
  includeEmptyGroups = false
): ServerGroup[] {
  const knownIds = new Set(subscriptions.map((s) => s.id));
  const groups: ServerGroup[] = [];

  // 自建 = 无归属 或 归属订阅已不存在（孤儿不丢，并入自建）；再按 endpoint 数据模型拆出「组网」。
  // 路由能力仍由 isMeshNode/meshRoutes 判定，不能拿来决定 UI 桶，否则从组网入口创建的企业 VPN
  // 在不填内网段时会保存后“消失”到自建 Tab。
  const manualAll = servers.filter((s) => !s.subscriptionId || !knownIds.has(s.subscriptionId));
  const manual = manualAll.filter((s) => !landsInEndpoints(s.protocol));
  const mesh = manualAll.filter((s) => landsInEndpoints(s.protocol));
  // 自建常驻：节点列表页（includeEmptyGroups）即使无自建节点也保留空的「自建」tab 作默认落地。
  if (manual.length > 0 || includeEmptyGroups) {
    groups.push({ id: 'manual', name: 'manual', isManual: true, servers: manual });
  }
  if (mesh.length > 0 || includeEmptyGroups) {
    groups.push({ id: 'mesh', name: 'mesh', isManual: false, isMesh: true, servers: mesh });
  }

  for (const sub of subscriptions) {
    const subServers = servers.filter((s) => s.subscriptionId === sub.id);
    if (subServers.length > 0 || includeEmptyGroups) {
      groups.push({ id: sub.id, name: sub.name, isManual: false, servers: subServers });
    }
  }

  return groups;
}

/**
 * 分组折叠的**默认展开集**：只展开「含当前选中节点」的那一组，其余全折叠；没有选中节点 ⇒ 空集。
 *
 * 三处节点选择器共用这一份判据（应用分流的策略菜单 / 规则弹窗的目标出站 / 托盘「全部节点」）——
 * 各写各的必然分叉，此前应用分流那份就多了一条「没命中就退回 `groups[0]`」的回落，于是
 * **「默认折叠」恰恰在最需要它的场景不成立**：还没指定节点、正要从一堆订阅里挑的时候，一打开就有
 * 一组铺开，且铺开的那组与用户想找的没有任何关系。没有选中项时正确答案是「全折叠」，不是「猜一个」。
 *
 * 返回集合而非单个 id：展开态本身允许多组同时打开（用户手动展开第二组不该把第一组关掉），
 * 本函数只负责给这个集合的**初值**。
 *
 * 直连哨兵（`DIRECT_SERVER_ID`）与已删节点的残留 id 都不属于任何组 ⇒ 自然落到空集，
 * 不需要调用方额外判空（也正是这条让上面那个 `groups[0]` 回落显得「有用」的假象来源）。
 */
export function defaultOpenGroupIds(
  groups: readonly ServerGroup[],
  selectedServerId: string | null | undefined
): Set<string> {
  if (!selectedServerId) return new Set();
  const hit = groups.find((g) => g.servers.some((s) => s.id === selectedServerId));
  return hit ? new Set([hit.id]) : new Set();
}
