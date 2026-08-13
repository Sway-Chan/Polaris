/**
 * 「Tailscale 出口名不副实」警示判定（纯函数，首页行内注脚用；§H）。
 *
 * 取代旧 §G「未选中就劝设为出口」（那是稳态噪音——用户显式选了别的出口/直连时反被劝）。这里只在【选中了 TS 当
 * 全局出口、却出不了公网】时给有效提示：
 *  - no-exit-device：选中 TS + 已登录 + 非 direct 模式 + 未配 exit_node → 公网并不经 TS（P0b 后回退 direct；S-b
 *    的 quick-join allowInternet:true 无 exit_node 亦归此），提示「已设为出口但未选出口设备」。**按 exit_node 直判、
 *    不信 allowInternet**（覆盖 S-b）。配置级问题，断开态也成立（截图 2 即断开）。
 *  - exit-device-offline：选中 TS + 已登录 + 已配 exit_node + 代理运行 + 该 exit 设备 peer offline → 公网可能出不去。
 *    仅代理运行时判（静态 snapshot 判 offline 会误报）。
 *  - exit-device-not-advertised：选中 TS + 已登录 + 已配 exit_node + 代理运行 + 该 exit 设备 peer 在线但【未广告出口】
 *    （exitNodeOption=false）→ 流量无法经其出网（TS 拒绝路由到非出口 peer），公网出不去。判据 exitNodeOption 与出口
 *    下拉「未广告出口」同源（TailscaleStatusPeer.exitNodeOption），修此前「在线但未广告」漏判→出口探测空转「检测中」。
 *
 *  - needs-auth：选中 TS + 代理运行 + 非 direct 模式 + **控制面明说这份凭据不能用**（末帧
 *    `backendState ∈ {NeedsLogin, NeedsMachineAuth}` 或 key 已过期）→ 该 endpoint 从未认证成功，
 *    **永远不承载流量**，公网静默走别处。见下方「为什么 needs-auth 必须存在」。
 *
 * # 为什么 needs-auth 必须存在（这条是真机上咬人的那条）
 *
 * 原先第三道守卫是无条件的 `if (!loggedIn) return 'none'`，注释理由是「登录角标 / 登录 toast /
 * 出口让位已 own」。真机（2026-07-31）证伪了这个前提：TS 节点被设为出口、sing-box 日志里
 * `endpoint/tailscale[Tailscale]: Waiting for authentication` 出现 6 次、`Running` 0 次
 * ⇒ 该 endpoint 从未认证 ⇒ tailscale outbound 全程 2 条计数、流量实际走 vless/direct，
 * 而**首页一个字都没提示**：登录角标在【节点页】的卡片上（`nodes.meshTsNeedsLogin`），
 * 首页出口行看不到；登录 toast 只在 authURL **首次**到达那一刻弹一次，之后无痕。
 * 用户以为登录了、设了出口，实际全程静默走别处 —— 正是「静默失败」的教科书形态。
 *
 * **判据不靠超时猜**：只在控制面给出**终局否定**（NeedsLogin / NeedsMachineAuth / expired，
 * 与 `isDefinitiveTsLoginFrame` 的 definitive-out 同一口径）时才报。核启动早期的
 * `NoState` / `Stopped` 过渡帧折叠出的 `loggedIn=false` **不算**（那是「还没启完」不是「凭据无效」），
 * 无帧亦不报。故本条永不在「正在连」的正常过程中闪现。
 *
 * **判据取末帧而非折叠态 `loggedIn`**：`tailscaleLoginStates` 是缓存 + state 目录 + STATUS 三源折叠，
 * 其中 `applyTailscaleStateExists`（state 目录存在即 true）会把一个 NeedsLogin 的节点**盖回已登录**
 * （它随 `servers` 变化重跑，而登录完成不改 servers）⇒ 只看折叠态会漏判。末帧是控制面的直接口径。
 *
 * 未选中 TS / 非终局的未登录（启动过渡 / 无帧）/ direct 模式 / exit_node 为无法匹配的自定义值 → none
 * （不误报，且与出口让位 reconcileLoginFallback 天然互斥）。
 */
import type { ServerConfig } from '../contracts/types';
import type { TailscaleStatusEvent } from '../contracts/tailscale-status';
import { isDefinitiveTsLoginFrame } from './tailscale-conn-state';

export type TsExitWarning =
  | 'none'
  | 'needs-auth'
  | 'no-exit-device'
  | 'exit-device-offline'
  | 'exit-device-not-advertised';

export interface TsExitWarningInput {
  /** 当前选中的全局出口节点（config.selectedServerId 对应）。 */
  selectedServer: ServerConfig | undefined;
  /** 该出口的综合登录态（tailscaleLoginStates，缓存驱动，代理关时也有值）。 */
  loggedIn: boolean;
  /** 分流策略是否为全局直连（proxyMode==='direct'）。 */
  proxyModeDirect: boolean;
  /** 主核是否运行（peers/认证态新鲜度门——offline 与 needs-auth 判定均须实时 STATUS 流）。 */
  proxyRunning: boolean;
  /** 该 TS 节点的 STATUS 末帧（store.tailscaleStatuses[id]）：`peers` 与认证态同帧，无帧=undefined。 */
  status: TailscaleStatusEvent | undefined;
}

export function deriveTsExitWarning(i: TsExitWarningInput): TsExitWarning {
  const s = i.selectedServer;
  if (!s || s.protocol?.toLowerCase() !== 'tailscale') return 'none'; // 未选中 TS 出口 → 永不提示（§G 方向反转）
  if (i.proxyModeDirect) return 'none'; // 显式全直连（与 meshSelectedExitFallsBackToDirect 同口径）
  // 认证态优先于其余各条：endpoint 没认证成功就**根本不承载流量**，此时报「没选出口设备」是在指错方向
  // （用户照做去选一台，流量照样不经 TS）。根因先行。
  // 三重门缺一不可：① 有帧（无帧=不知道，不猜）② 核在跑（帧新鲜；核停后浏览器里补完的登录我们收不到，
  // 据陈旧 NeedsLogin 报错会变成误报）③ 该帧是**终局否定**（definitive-out：NeedsLogin/NeedsMachineAuth/
  // 过期），启动过渡帧的 loggedIn=false 不算。
  if (i.proxyRunning && i.status && !i.status.loggedIn && isDefinitiveTsLoginFrame(i.status)) {
    return 'needs-auth';
  }
  if (!i.loggedIn) return 'none'; // 其余未登录（启动过渡/无帧）：登录角标/toast/出口让位已 own，不叠加
  const exitNode = s.tailscaleSettings?.exitNode?.trim();
  if (!exitNode) return 'no-exit-device'; // 核心态：无 exit_node（S-a/S-b 统一，不信 allowInternet）→ 公网不经 TS
  if (!i.proxyRunning) return 'none'; // offline 判定须新鲜 STATUS（陈旧 snapshot 会误报）
  // exit_node 值与 peer 匹配（复用 exit-node-field 的 ip/hostName 口径）；匹配到才判，自定义值不匹配 → 不误报。
  const peer = i.status?.peers.find((p) => p.ip === exitNode || p.hostName === exitNode);
  if (peer && !peer.online) return 'exit-device-offline'; // 离线优先（离线态 exitNodeOption 可能陈旧，先报离线更可行动）
  if (peer && !peer.exitNodeOption) return 'exit-device-not-advertised'; // 在线但未广告出口 → 流量出不去（修空转检测）
  return 'none';
}
