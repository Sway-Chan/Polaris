/**
 * 全局节点选择「直连」哨兵（#73 proxifier）。
 *
 * selectedServerId 取此值 = 全局出口走 direct（proxy-selector default=direct）。配 smart 模式即 proxifier：
 * 未命中规则的流量直连，仅 custom/app `action=proxy`+指定目标节点的走代理。global 模式下=等同直连模式。
 *
 * **只作"全局节点选择"用**（首页 / 托盘「选择服务器」），不作 per-rule 目标（per-rule 直连用 action=direct）。
 * 故不会进 rule-sel 目标解析；仅 generateSingBoxConfig/buildOutbounds/planHotSwitch 识别它。
 */
export const DIRECT_SERVER_ID = '__direct__';

export function isDirectSelection(selectedServerId: string | null | undefined): boolean {
  return selectedServerId === DIRECT_SERVER_ID;
}

/**
 * 全局节点选择「阻断」哨兵（Polaris 新增，上游 无对应物）。Rust 侧对拍：
 * `crates/config-engine/src/user_config/dns_constants.rs BLOCK_SERVER_ID`。
 *
 * **语义边界（写给后来者，别当 bug 修）**：出口选单支配的是「本该走出口的那部分流量」，不是全部流量。
 * 直连规则（LAN/私网、`geosite-cn`/`geoip-cn`、ICMP、`protocol:dns`、DoH 引导、sing-box 自身进程）
 * 都是 `action:route → outbound:direct` **显式命中、压根不经过 proxy-selector**，阻断影响不到它们。
 * 于是三种代理模式下观感差异很大，这是出口语义的正确外延：
 *   - `smart`：国内照常直连、只断「本该走代理」的境外流量；
 *   - `global`：断几乎全部，仅剩上面那批豁免；
 *   - `direct`：`route.final` 恒 = `direct`、无流量经 selector ⇒ **本哨兵无效**，故 UI 在该模式下禁用该选项。
 *
 * 「全流量 kill switch」是另一个功能（要连 LAN/DNS/管理面一起掐），不走出口选单。
 * 订阅更新/检查更新（`update-in` inbound）已在 route 侧豁免阻断，否则用户只剩「切回出口」一条自救路。
 */
export const BLOCK_SERVER_ID = '__block__';

export function isBlockSelection(selectedServerId: string | null | undefined): boolean {
  return selectedServerId === BLOCK_SERVER_ID;
}

/**
 * 是否「非节点哨兵」（direct / block）—— 收口所有「该 id 不是真实节点」的判据：
 * 豁免存在性校验、不进节点引用集、无真实出站可测、分组归属恒为空集。
 *
 * 逐处写 `isDirectSelection(x) || isBlockSelection(x)` 是 `__direct__` 当年铺开到 ~8 处的成因；
 * 加第三个哨兵时只改这里。
 */
export function isSentinelSelection(selectedServerId: string | null | undefined): boolean {
  return isDirectSelection(selectedServerId) || isBlockSelection(selectedServerId);
}

/**
 * 全局出口的 proxy-selector 成员 tag（单一真值，收口「selectedServerId → memberTag」）：直连哨兵→'direct'，
 * 否则查 idToTagMap 得节点 tag。generate-time 的 selector default 与 hot-switch 的 PUT 目标共用此映射、锁步
 * （改哨兵值 / direct tag 名只此一处）。未知节点返回 undefined，由调用方按场景兜底
 * （buildOutbounds → nodeTags[0]/'proxy'；planHotSwitch → 退回重启）。
 */
export function resolveGlobalExitTag(
  selectedServerId: string | null | undefined,
  idToTagMap: Map<string, string> | null | undefined
): string | undefined {
  if (isDirectSelection(selectedServerId)) return 'direct';
  // 阻断哨兵 → 既有 block 出站（`outbounds.rs` 无条件生成，应用分流的 action=block 也用它）。
  if (isBlockSelection(selectedServerId)) return 'block';
  return selectedServerId ? idToTagMap?.get(selectedServerId) : undefined;
}

/**
 * 删「当前选中」节点时的兜底出口（D4，见 docs/design/polaris-node-change-restart）：从剩余候选里挑**最快**
 * （latencyMap 最低正值），无任何正测速值则回退列表第一个候选，空候选返回 null（调用方置 selectedServerId=null → direct）。
 * 纯函数、注入 latencyMap（渲染端会话态），可离线单测。latency<=0（超时 -1 / 未测）不参与「最快」比较，仅靠首个兜底。
 * candidateIds 须按列表序传入（保「无测速值回退第一个」= 用户列表第一个）。
 */
export function pickFallbackExit(
  candidateIds: string[],
  latencyMap: Record<string, number>
): string | null {
  if (candidateIds.length === 0) return null;
  let bestId: string | null = null;
  let bestLatency = Infinity;
  for (const id of candidateIds) {
    const l = latencyMap[id];
    if (typeof l === 'number' && l > 0 && l < bestLatency) {
      bestLatency = l;
      bestId = id;
    }
  }
  return bestId ?? candidateIds[0];
}
