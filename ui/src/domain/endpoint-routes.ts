import type { ServerConfig, Protocol, UserConfig } from '../contracts/types';
import { dedupe, dedupeTrim } from './collections';
import { isWarpServer, warpSlotTaken } from './warp';

/**
 * 组网协议 —— 判据是**配置期就能声明可达网段**（⇒ 生成侧能为它发 force-route 规则）：
 * WireGuard 填 `allowedIPs`，Tailscale 有协议固定的 tailnet 两族段 + `routes`。
 * 单一真值，杜绝多处枚举漂移；与 Rust `is_mesh_protocol` 由 `mesh-predicates-parity` 对拍。
 *
 * ⚠️ **不等于「落在 sing-box `endpoints[]` 里的协议」**：openconnect / openvpn-client 同样落
 * `endpoints[]`，但网段由服务端运行期 push、配置期不可知 ⇒ 不在此表。判数据模型形态用
 * [`landsInEndpoints`]。本常量从前叫 `ENDPOINT_PROTOCOLS` 且注释就写着「顶层 endpoints[]、非
 * outbound」—— 那句描述按它自己的字面就是错的（漏了那两个），而名字与成员集不重合正是三处缺陷的根因。
 */
export const MESH_PROTOCOLS: readonly Protocol[] = ['wireguard', 'tailscale'];
export function isMeshProtocol(protocol: string | undefined): boolean {
  return !!protocol && MESH_PROTOCOLS.includes(protocol.toLowerCase() as Protocol);
}

/**
 * 落 sing-box 顶层 `endpoints[]`（而非 `outbounds[]`）的协议 —— **内核的数据模型形态**，与
 * [`isMeshProtocol`]（产品能力）是两件事。镜像 Rust `lands_in_endpoints`。
 *
 * 射程同 Rust 侧：`custom` 的 endpoint 腿看的是 `customSettings.isEndpoint`（节点级），不在本表，
 * 需要覆盖它的调用点自行并上那一支。
 */
export const ENDPOINT_LEG_PROTOCOLS: readonly Protocol[] = [
  'wireguard',
  'tailscale',
  'openconnect',
  'openvpn-client',
];
export function landsInEndpoints(protocol: string | undefined): boolean {
  return !!protocol && ENDPOINT_LEG_PROTOCOLS.includes(protocol.toLowerCase() as Protocol);
}

/**
 * 该**节点**是否具备组网能力 —— [`isMeshProtocol`] 的节点级形态，镜像 Rust `is_mesh_node`。
 *
 * 判据仍是「配置期能否声明可达网段」，只是对 openconnect / openvpn-client 而言这件事由用户填没填
 * `meshRoutes` 决定：填了，生成侧就为它发 force-route 规则，它与一个填了 `allowedIPs` 的 WG 节点在
 * 路由上再无分别；没填，它只是个普通出口。force-route、出口兜底等**看能力**的地方用本函数；
 * 节点页的 UI 归组看 [`landsInEndpoints`]，两者不能再耦合。
 */
export function isMeshNode(server: { protocol?: string; meshRoutes?: string[] }): boolean {
  const p = server.protocol?.toLowerCase();
  if (isMeshProtocol(p)) return true;
  if (p !== 'openconnect' && p !== 'openvpn-client') return false;
  return !!server.meshRoutes?.some((c) => c.trim() !== '');
}

/** 账号制协议（连控制面、无 server address/port）：当前仅 Tailscale。供连接闸门/校验豁免 address/port。 */
export function isAccountBasedProtocol(protocol: string | undefined): boolean {
  return protocol?.toLowerCase() === 'tailscale';
}

/**
 * 单例槽判定所需的**最小结构**（协议 / 地址 / warpDevice / id）。刻意不是 `ServerConfig`：
 * 提交闸门要在「尚未落盘、还没有 id」的草稿（`Omit<ServerConfig,'id'>`、导入解析产物）上判定，
 * 要求完整 ServerConfig 会把闸门挡在最需要它的那条腿（新增）之外。`ServerConfig[]` 结构上满足本型，
 * 既有调用方无需改动。
 */
export type MeshSlotServer = Parameters<typeof isWarpServer>[0] & { id?: string };

/**
 * Tailscale 单节点硬限：已存在一个 Tailscale 节点时该「槽位」即被占用。
 * 同一设备的所有 Tailscale 账号共用同一段网络地址（100.64.0.0/10 tailnet），多个会互相顶掉，故全局只许一个。
 * editingId 排除自身——编辑现有 TS 节点不算「再加一个」，必须放行。
 * 纯函数（仅看协议字段）：UI 主拦截点（use-server-actions）+ ConfigManager 兜底归一共用，可离线单测。
 */
export function tailscaleSlotTaken(servers: MeshSlotServer[], editingId?: string): boolean {
  return servers.some((s) => s.protocol?.toLowerCase() === 'tailscale' && s.id !== editingId);
}

/** 全局至多一个实例的组网协议槽。 */
export type MeshSingletonSlot = 'warp' | 'tailscale';

/**
 * **造节点路径的统一单例闸门**：候选节点会不会撞上已被占用的 WARP / Tailscale 槽位；撞上则返回槽名，否则 null。
 *
 * 为什么必须在**每条**造节点腿上判、而不只在接入区 UI 上分流：接入区只控「卡片入口给不给点」，
 * 而 WgDialog（粘贴 Cloudflare `.conf` → 端点域名兜底判定即 WARP）、ImportDialog（批量入库）、
 * NodeDialog、克隆 都能绕过接入区直调 `server:add` / `server:addBulk`，后端两命令均无守卫
 * （见 `src-tauri/src/commands/server.rs` 的 DESIGN-REVIEW(mesh-singleton-guard-renderer-only)）。
 * 第二个 WARP 会与主 TUN 抢内核 utun → `Connect: resource busy` FATAL（真机实证，见 meshUsesSystemInterface）；
 * 第二个 Tailscale 会与第一个互顶 tailnet 地址。
 *
 * editingId 排除自身：编辑现有 WARP/TS 节点不算「再加一个」，必须放行（与两个 slotTaken 同义）。
 * 纯函数：三个弹窗 + 克隆 + 批量导入共用同一真值，可离线单测。
 */
export function meshSingletonConflict(
  candidate: MeshSlotServer,
  servers: MeshSlotServer[],
  editingId?: string
): MeshSingletonSlot | null {
  if (isWarpServer(candidate) && warpSlotTaken(servers, editingId)) return 'warp';
  if (candidate.protocol?.toLowerCase() === 'tailscale' && tailscaleSlotTaken(servers, editingId)) {
    return 'tailscale';
  }
  return null;
}

/**
 * 批量入库（`server:addBulk`）的逐条单例准入：返回可入库的候选与被单例槽拒收的候选。
 *
 * **准入者即刻占槽**（`pool.push`）——否则同一批里的两个 WARP 会被同一份「槽位空闲」快照双双放行，
 * 逐条判定形同虚设。整批拒绝是错的：一条重复的 WARP 不该连累同批 50 个正常代理节点，
 * 故语义是过滤 + 如实报数，由调用方提示跳过条数。
 */
export function admitMeshSingletons<T extends MeshSlotServer>(
  candidates: readonly T[],
  servers: MeshSlotServer[]
): { admitted: T[]; rejected: T[] } {
  const pool: MeshSlotServer[] = [...servers];
  const admitted: T[] = [];
  const rejected: T[] = [];
  for (const c of candidates) {
    if (meshSingletonConflict(c, pool)) {
      rejected.push(c);
    } else {
      admitted.push(c);
      pool.push(c);
    }
  }
  return { admitted, rejected };
}

/** 全网段（catch-all / 全隧道）：IPv4 0.0.0.0/0 + IPv6 ::/0。单一真值——force-route 剥离、allowInternet=on
 * 注入 peer.allowed_ips、表单显示/录入剥离 均复用此清单，杜绝多处字面量漂移。 */
export const FULL_TUNNEL_CIDRS = ['0.0.0.0/0', '::/0'] as const;
const CATCH_ALL = new Set<string>(FULL_TUNNEL_CIDRS);

/** 从 CIDR 列表剥离全网段（catch-all），仅留具体段；逐项 trim 比对。allowedIPs 显示/force-route 共用。 */
export function stripCatchAll(cidrs: string[] | undefined): string[] {
  return (cidrs || []).filter((c) => !CATCH_ALL.has(c.trim()));
}

/** CIDR 列表是否含任一全网段（catch-all）= 全隧道意图。wg-quick 导入据此推断 allowInternet。 */
export function hasCatchAll(cidrs: string[] | undefined): boolean {
  return (cidrs || []).some((c) => CATCH_ALL.has(c.trim()));
}

/** Tailscale tailnet 自身段（CGNAT）。tailnet peer 的 IP 都在此；不在 bypass-LAN 私网表，故必须 force-route。 */
export const TAILNET_CGNAT = '100.64.0.0/10';
/**
 * Tailscale v6 tailnet 段（固定 ULA 前缀，Tailscale 官方把所有 tailnet v6 地址分配于此）。tailnet peer 的 v6 IP
 * 都在此；**⊂ bypass-LAN 的 `fc00::/7`（v6 ULA 全段）** → Windows 下会被 bypassLAN 内核排除出 TUN，须与 v4 tailnet
 * 同样 force-route + Windows carve 开洞（`subtractCidrs` v6-aware 自动挖 fd7a 洞），否则 enableIPv6 开启时 v6 tailnet
 * 流量去 exit 而非走 tailnet（缺此段的原 bug）。FakeIP 假 v6 段 `2001:2::/48` 在 benchmarking 保留段、不占 ULA，与此零相交。
 */
export const TAILNET_ULA_V6 = 'fd7a:115c:a1e0::/48';

// System 模式内核接口固定名（下发 sing-box system_interface_name）。出口托管 MeshExitRouteManager
// 按此名在内核接口装/清 exit 拆半默认路由 + 精确定位（见 mesh-exit-route.ts）。WG 端点名选项待证，
// 取不到则按隧道 IP 反查（uncertainties #1）。
export const TS_SYSTEM_INTERFACE_NAME = 'polaris-ts';
export const WG_SYSTEM_INTERFACE_NAME = 'polaris-wg';

/**
 * 该 endpoint 节点应被「强制路由到自身 tag」的具体 CIDR（userspace；优先于 bypass-LAN、独立于全局选中）。
 * 单一真值：节点路由由其配置 CIDR 决定，不再有独立「绕过局域网排除段」。
 *   - WireGuard：allowedIPs 去掉 0/0、::/0（catch-all 是全量代理语义、由 selector/final 接管）。
 *   - Tailscale：tailnet 段 100.64.0.0/10（自动，必需）+ routes（用户填的 advertised 子网）。
 * 非 endpoint 协议返回 []。重复/空白去除。
 */
export function endpointForcedRouteCidrs(server: ServerConfig): string[] {
  const p = server.protocol?.toLowerCase();
  let raw: string[] = [];
  if (p === 'wireguard') {
    raw = stripCatchAll(server.wireguardSettings?.allowedIPs);
  } else if (p === 'tailscale') {
    // 与 WireGuard 分支对齐：剥 catch-all（0/0），TS 全隧道走 exitNode，routes 不该承载 0.0.0.0/0。
    // 两族 tailnet 段恒发（v4 CGNAT + v6 ULA）：v6 段实际生效由全局 enableIPv6 门控——关闭时 AAAA 抑制、无 v6 流量，
    // force-route 规则携 v6 ip_cidr 无害；开启时确保 v6 tailnet peer（fd7a:115c:a1e0::/48）走 tailnet 而非 exit（原缺此段=bug）。
    raw = [TAILNET_CGNAT, TAILNET_ULA_V6, ...stripCatchAll(server.tailscaleSettings?.routes)];
  } else if (p === 'openconnect' || p === 'openvpn-client') {
    // 用户手填的内网段（这两个协议的段本由服务端运行期 push、配置期不可知）。去 catch-all 与另两支
    // 同理：0/0 属「全隧道」意图，由各自的出网开关表达，混进 force-route 会绕过那个开关。
    raw = stripCatchAll(server.meshRoutes);
  } else {
    return [];
  }
  return dedupeTrim(raw);
}

/**
 * 组网节点（WireGuard / WARP / Tailscale）是否允许作外网出口（「允许访问外网」开关）。
 * 缺省 true（向后兼容 + 新建默认开）；仅显式 false 关闭。非组网协议恒 true（该语义不适用）。
 * 单一真值：Layer A(allowed_ips)、Tailscale exit_node 门控、D4 final 兜底、UI 角标共用。
 */
export function meshAllowsInternet(server: ServerConfig): boolean {
  const p = server.protocol?.toLowerCase();
  if (p === 'wireguard') return server.wireguardSettings?.allowInternet !== false;
  // Tailscale：allowInternet 由 exit_node 派生（把表单单一真值 tailscale-form `allowInternet=!!exitNode` 下沉到谓词层，
  // §H.5/P0b）。治 S-b：quick-join 硬编码 `allowInternet:true` 但无 exit_node 时不再误判「承载全隧道」→ 公网从「进
  // tsnet 无出口的黑洞」变「回退 direct」（安全），且不为其装 OS 出口路由；与「无出口设备」警示语义严格镜像。
  // 存量 tailscaleSettings.allowInternet 字段谓词层忽略（向后兼容、不迁移）。
  if (p === 'tailscale') return !!server.tailscaleSettings?.exitNode?.trim();
  // OpenVPN 的全隧道开关。**缺省 true**（同 WG 那支）：判 false 的后果是用户选了它作出口、流量却被
  // 兜底回 direct —— 静默走明文，比多一次黑洞更坏。只在用户显式关掉时才认为不承载全隧道，而那恰是
  // 「只走公司内网段」的表达。OpenConnect 无对应开关（本就是全隧道），落末尾的 true。
  if (p === 'openvpn-client') return server.openvpnClientSettings?.redirect_gateway !== false;
  return true;
}

/**
 * 组网节点是否「始终路由其内网段」(force-route 常驻)。缺省 true（向后兼容 + 新建默认开=网段恒可达）；仅显式
 * false 关闭=「仅出网」语义：网段只在节点 engaged（被选中/被规则指向）时才路由。与 allowInternet 正交——
 * allowInternet 控 0/0 全隧道出网，本开关控 specific 段是否常驻。**只 gate route.rules，绝不碰 allowed_ips。**
 */
export function meshAlwaysRoutesSubnets(server: ServerConfig): boolean {
  const p = server.protocol?.toLowerCase();
  if (p === 'wireguard') return server.wireguardSettings?.alwaysRouteSubnets !== false;
  if (p === 'tailscale') return server.tailscaleSettings?.alwaysRouteSubnets !== false;
  return true;
}

/**
 * 该组网节点的 force-route 段本轮是否应发射（route-builder 块 0c 的 gate）。纯函数 + 注入式，全矩阵可单测：
 *   - alwaysRouteSubnets ON（默认/旧配置）→ 恒发射（现状，网段恒可达）。
 *   - OFF（仅出网）→ 仅当节点 engaged：被选中为主出口（selectedServerId），或被某条 enabled 规则/应用分流
 *     显式指向（ruleTargetedServerIds，由 route-builder 从 effectiveCustomRules/AppRules 的 targetServerId 汇集）。
 * 注意：本谓词只决定「是否把内网段 force-route 到自身 tag」，**不影响 peer.allowed_ips**——故 OFF 节点被选中时
 * 网段仍可达（隧道接受 + 此时本谓词返回 true → 发 force-route 覆盖 bypass-LAN）。
 */
export function shouldForceRouteSubnets(
  server: ServerConfig,
  selectedServerId: string | null | undefined,
  ruleTargetedServerIds: ReadonlySet<string>
): boolean {
  if (meshAlwaysRoutesSubnets(server)) return true;
  if (server.id === selectedServerId) return true;
  return ruleTargetedServerIds.has(server.id);
}

/**
 * 收集「显式指向某节点」的规则目标 id：仅 `enabled && action==='proxy' && targetServerId`——与全库 targetServerId
 * 消费口径一致（`targetServerId` 仅 action==='proxy' 时有效，见 Rule/AppRule 注释；非 proxy 的陈旧 targetServerId 不算）。
 * 守卫单一真值：route-builder 块 0c 的 engaged 判定 + warn/shadow 同步过滤共用。backend 传 effective 规则、UI 传
 * config 原始规则（结构兼容 `{enabled?, action?, targetServerId?}`，类型在各侧已知）。
 */
export function collectRuleTargetedServerIds(
  rules: ReadonlyArray<{ enabled?: boolean; action?: string; targetServerId?: string }> | undefined
): Set<string> {
  const ids = new Set<string>();
  for (const r of rules ?? []) {
    if (r.enabled && r.action === 'proxy' && r.targetServerId) ids.add(r.targetServerId);
  }
  return ids;
}

/* `customEndpointCarriesTraffic` 已删（2026-08-11）。
 *
 * 它是 Rust 侧 `crates/config-engine/src/builder/endpoint_routes.rs::custom_endpoint_carries_traffic`
 * 的**逐行复制品**，但**零消费点**（全仓无任何 import，连测试都没有）—— 判据活在生成侧，
 * 前端从来不需要自己算一遍。留着的唯一效果是两份键集必然漂移：实测 2026-08-11 补 OpenVPN
 * 的 `redirect_gateway` / `redirect_private` / `route_no_pull` 时，只有 Rust 那份被改，
 * 这份原样停在 WireGuard/Tailscale 的词汇上 —— 一份没人用、且已经错了的判据，比没有更糟。
 * 前端若将来真要预测重启，应当走 IPC 问生成侧，而不是再抄一份。 */


/**
 * ⚠️ `referencedServerIds` 已于 2026-07-29 从本文件**删除**，请勿凭记忆再移植一份回来。
 *
 * 它曾是 Rust `config-engine/src/builder/endpoint_routes.rs::referenced_server_ids` 的 1:1 镜像，
 * 但后来只有 Rust 一侧演进（最近一次是把 selector default 兜底可能命中的节点纳入引用集），
 * TS 这份从此与判据分叉，且**全仓零调用点**——留着的唯一作用是等着某天被人接上，
 * 然后产生「UI 说可以 defer、后端说必须重启」这种两侧同名不同义的静默分叉。
 *
 * 引用面的**单一真值在 Rust**。渲染端若确实需要这个判断，走 IPC 问后端，不要再复刻。
 */

/**
 * 本轮「实际会发射 force-route」的组网节点（与块 0c `shouldForceRouteSubnets` 同口径）：alwaysRouteSubnets ON、
 * 或被选中、或被规则显式指向。供「自定义规则与组网段重叠」warn / 「网段被覆盖」shadow 角标与**发射端同口径**，
 * 杜绝对「仅出网且未 engaged」节点虚报覆盖/被覆盖（非组网协议恒保留，对 cidr/shadow 计算无副作用）。
 *
 * 注（advisory 边界，非路由层）：backend 调用方传 effective 规则（已按 proxyMode/appRoutingEnabled mode-gate），
 * UI 调用方传 config 原始规则——故 global/direct 模式下 UI 的 ruleTargetedServerIds 估计可能偏宽（把失效规则也算
 * engaged），极窄场景下角标指向略有偏差。仅影响提醒角标、不影响实际 route.rules / allowed_ips（backend 自洽）；
 * 不在 UI 复制 backend mode-gate 以免脆弱重复，作为已知 advisory 近似。
 */
export function meshForceRoutedServers(
  servers: ServerConfig[] | undefined,
  selectedServerId: string | null | undefined,
  ruleTargetedServerIds: ReadonlySet<string>
): ServerConfig[] {
  return (servers ?? []).filter((s) =>
    shouldForceRouteSubnets(s, selectedServerId, ruleTargetedServerIds)
  );
}

/**
 * Phase 2：组网节点是否启用 system 内核接口（reverseMesh=反向可达/被访问，WG `system:true` /
 * Tailscale `system_interface:true`）。缺省 false=userspace gVisor 栈（Phase 1）。**纯用户意图**：
 * 「reverseMesh ⟹ helper 提权已就位」由上层校验/连接闸门 + ProxyManager emit 门控强制（见 server-completeness
 * 与 buildOutbounds 的 allowSystemInterface），故本函数在 config 构建期可等同 effective system 态。
 */
export function meshUsesSystemInterface(server: ServerConfig): boolean {
  const p = server.protocol?.toLowerCase();
  if (p === 'wireguard') {
    // WARP（Cloudflare anycast 出口）不支持组网/反向可达，恒 gVisor——内核接口对它无意义：它不是子网路由器、
    // 不可被反向访问。且 WARP system:true 会与主 TUN / 另一 System 接口抢内核 utun 资源 →
    // `post-start endpoint/wireguard[Cloudflare WARP]: Connect: resource busy` FATAL（真机实证，多网卡时必现）。
    // 用 isWarpServer 鲁棒判定（含旧/导入的无 warpDevice 标记 WARP，按端点域名兜底）→ 无论 reverseMesh 一律否决。
    if (isWarpServer(server)) return false;
    return server.wireguardSettings?.reverseMesh === true;
  }
  if (p === 'tailscale') return server.tailscaleSettings?.reverseMesh === true;
  return false;
}

/**
 * 平台是否支持组网 System 内核接口模式（reverseMesh）。**Windows 禁 System**：Windows 上 sing-box 的 tsnet 给
 * polaris-ts 自装 exit 0/0 metric=0、抢直连/bootstrap DNS 致全网瘫，且无 macOS 的 ifscope 作用域可隔离 → System
 * 不可靠；故 Windows 一律强制 gVisor（userspace 栈零提权、不建内核接口、出口经 tsnet 内部转发不依赖 OS 路由）。
 * macOS/Linux 支持 System。接受 process.platform / window.electron.platform 取值，是「Windows 禁 System」的**单一
 * 真值谓词**——ProxyManager.systemInterfaceAvailable、start-retry 预算、UI AccessModeField、MeshExitRouteManager
 * 共用，避免散落多处平台判断漂移（同 neighbor.ts 的能力谓词模式）。
 */
export function meshSystemSupportedOnPlatform(
  platform: NodeJS.Platform | string | undefined
): boolean {
  return (platform || '').toLowerCase() !== 'win32';
}

/** 测速可行性能力位（path-aware）：主核 probe 池是否可用（=代理运行且池就绪）。 */
export interface SpeedTestCaps {
  mainCorePool?: boolean;
}

/**
 * 节点是否参与测速（path-aware，§16.1）。
 *  - reverseMesh(system 内核接口)：非选中时 dial 走 OS default = 测出直连假好值 → 恒排除。
 *  - **组网 mesh-only（`meshAllowsInternet=false`）：无公网出口 = 探测必进黑洞、必假超时 → 恒排除。**
 *    两族对称（此前只对 Tailscale 判、WireGuard 漏判）：TS 是「无 exitNode」，WG/WARP 是
 *    「allowInternet=off」——后者 peer.allowed_ips 只含具体段（见 wireguardPeerAllowedIps），
 *    公网探测 URL 不命中 cryptokey routing 即被丢弃。返回的 `-1` 在 UI 上读作「真实超时」而非「未测」
 *    ⇒ 与 TS-mesh-only 同样是伪造数值，故按同一条理由排除。
 *  - TS-exit（exitNode 非空 && 非 reverseMesh）：**仅主核池路径**可测（caps.mainCorePool）——主核 tsnet 认证态活着、
 *    TS tag 已是 probe-selector 成员；临时核路径建不出第二 tsnet 实例，维持排除。
 *  - custom endpoint：raw-JSON 无 gate 真值 → 恒排除。
 * 缺省 caps（不传/临时核口径）：TS 一律不可测——与 ProxyManager.buildSpeedTestOutbound 的 null 分支同口径。
 * WireGuard（非 reverseMesh 且允许外网）仍可测。
 */
export function isSpeedTestable(server: ServerConfig, caps?: SpeedTestCaps): boolean {
  const p = server.protocol?.toLowerCase();
  if (meshUsesSystemInterface(server)) return false; // reverseMesh 排除（直连假好值）
  // ⟺ TS 的 !!exitNode / WG·WARP 的 allowInternet !== false（本文件 meshAllowsInternet）。
  if (isMeshNode(server) && !meshAllowsInternet(server)) return false;
  if (p === 'tailscale') return caps?.mainCorePool === true;
  if (p === 'custom' && server.customSettings?.isEndpoint) return false;
  return true;
}

/**
 * 「全量测速」的目标 id 集（对齐 上游 `use-speed-test.ts:48-50`）。
 *
 * = 全部已配置节点经 [`isSpeedTestable`] 过滤后的 id，**保序**（结果按请求序流式回填）。
 * 抽成函数而非在各屏内联：首页圆钮与节点页「全部测速」必须同口径，否则同一句「全部测速」
 * 在两屏测出不同集合。空集由调用方处理（提示而非空跑，见 上游 `:51-54`）。
 *
 * `caps.mainCorePool` 是 **path-aware** 位：TS-exit 仅主核池可用（=代理在跑）时可测。
 * 渲染端高估时后端 `partition_pool` 兜真值 —— 只会让该节点缺席，不会产假数值。
 *
 * `excludeIds`（可选）= 结构上能测、但**这一轮不该交给后端**的 id（当前唯一来源：staged-only 节点，
 * 盘上还没有它 ⇒ 后端按 id 找不到）。不传 = 今天行为，既有全部调用点零改动。
 * 与卡上 ⚡ 的置灰口径必须同源，见 `nodes-logic.speedTestBlockReason` 的第三个入参。
 */
export function speedTestableIds(
  servers: readonly ServerConfig[],
  caps?: SpeedTestCaps,
  excludeIds?: ReadonlySet<string>
): string[] {
  return servers
    .filter((s) => isSpeedTestable(s, caps) && !excludeIds?.has(s.id))
    .map((s) => s.id);
}

/**
 * 该组网节点是否承载「全隧道默认出口」（全部出网流量经它）= 允许外网（**与接入模式正交**）。
 *  - WireGuard/WARP：gVisor 用裸 {0/0,::/0}；**system WG 用预折半 {0/1,128/1,::/1,8000::/1}**（见
 *    wireguardPeerAllowedIps）——cryptokey 覆盖等同 0/0，但 sing-box 装的是折半 ifscope 路由（像主 TUN 的
 *    BuildAutoRouteRanges 产物）、不装裸 default → 不触发删 macOS 全局 default（裸 0/0 会删、停核断网，monitor 实证）。
 *  - Tailscale：exit_node ≠ 0/0，出口由 MeshExitRouteManager 以 ifscope 托管、不碰全局 default。
 * 单一真值：Layer A(allowed_ips) / D4/D7 选中兜底 / TS exit_node 门控 / UI「可作出口」共用。
 */
export function meshNodeCarriesFullTunnel(server: ServerConfig): boolean {
  return meshAllowsInternet(server);
}

/**
 * WireGuard peer.allowed_ips（Layer A，栈内 cryptokey routing，**永不碰系统 main 表**）：
 *   - allowInternet=on  → dedup(specific ∪ {0.0.0.0/0, ::/0})（两族全给，不按地址族裁剪，v6 取舍交全局 enableIPv6）
 *   - allowInternet=off → specific（仅承载列表网段）；**specific 为空 → 返回 null**（空 allowed_ips 会让 sing-box
 *     FATAL，sing-box 1.13.13 实测 `missing allowed ips for peer 0` → 调用方据 null 跳过发射该 endpoint）。
 * specific 复用 endpointForcedRouteCidrs（WG=allowedIPs 去 catch-all、trim 去重）。
 */
export function wireguardPeerAllowedIps(server: ServerConfig): string[] | null {
  const specific = endpointForcedRouteCidrs(server);
  // 全隧道意图 → specific ∪ {0/0,::/0}。system WG 也用裸 0/0（cryptokey 需要）——预折半已证伪：sing-tun 落内核前
  // 把 0/1+128/1 合并回裸 0/0（netstat 实证），照样撞 en0 default 的 EEXIST、被 setRoutes 善后删掉（tun_darwin.go
  // :451），且 unsetRoutes 停核不回填 → 断网。该断网由 ProxyManager 的「全局 default 存/停核补回」安全网兜底。
  // off/空 → specific（空则 null，空 allowed_ips=FATAL，调用方据 null 跳过发射）。
  if (meshNodeCarriesFullTunnel(server)) {
    return dedupe([...specific, ...FULL_TUNNEL_CIDRS]);
  }
  return specific.length > 0 ? specific : null;
}

/**
 * 组网节点是否「关外网且无可路由网段」→ 不可发射/不可用（空 allowed_ips=FATAL，必须在生成期拦截，否则连累
 * 整份 sing-box 配置 FATAL）。仅 WireGuard/WARP 可能命中（off + 无具体段）；Tailscale off 仍达 tailnet
 * (auto 100.64.0.0/10) 故恒可发射、不算 unroutable。供 buildOutbounds 跳过发射 + 渲染侧连接闸门置灰共用。
 */
export function isMeshNodeUnroutable(server: ServerConfig): boolean {
  if (server.protocol?.toLowerCase() === 'wireguard') {
    return wireguardPeerAllowedIps(server) === null;
  }
  return false;
}

/**
 * D4/D7（+Phase2）：选中「不承载全隧道的组网节点」(WG/Tailscale，allowInternet=off **或** system:true 内核接口
 * 恒 specific-only，见 meshNodeCarriesFullTunnel) 为**主节点**时，「→代理」的用户出口
 * （global 的 route.final；smart 的 geosite-!cn / google 关键词 / final）应整体兜底回 'direct'，而非
 * proxy-selector——proxy-selector.default = 该 off-mesh 节点，非具体段/海外流量进其用户态栈被 cryptokey
 * routing 丢弃（allowed_ips 不含 0/0）→ 黑洞断网。具体段仍由 force-route（排在这些规则之前）经组网节点；
 * 用户其余流量直连保上网。**global 与 smart 同此兜底**（D7 修复：原仅 global 留下 smart 海外黑洞）；direct 模式
 * 本就 final=direct、无「→代理」规则，不适用。
 *
 * 残留（已知较窄，非本兜底覆盖）：用户显式创建的「应用分流·代理·无固定目标」规则仍 default=proxy-selector→
 * off-mesh 节点；该 app 的流量仍会被丢弃。属用户对 off-mesh 主节点显式指定代理的自相矛盾配置，由角标/警告提示，
 * 不在本运行期兜底内（彻底消除需「禁止 off-mesh 作主节点」更大改动，列为后续）。
 */
export function meshSelectedExitFallsBackToDirect(config: UserConfig): boolean {
  if ((config.proxyMode || 'smart').toLowerCase() === 'direct') return false;
  const selected = config.servers?.find((s) => s.id === config.selectedServerId);
  return (
    !!selected && isMeshNode(selected) && !meshNodeCarriesFullTunnel(selected)
  );
}

/**
 * 全部节点的 mesh force-route 段并集（去重）。供「路由规则与组网段重叠」提醒共用：
 * main 的 config-gen warn + renderer 的内联 hint/列表角标。用全量 servers（非仅 emitted）以覆盖潜在重叠。
 */
export function meshForcedRouteCidrs(servers: ServerConfig[]): string[] {
  return dedupe(servers.flatMap((s) => endpointForcedRouteCidrs(s)));
}

/** 一条被抢占的网段：`cidr` 不会经本节点路由，实际生效的是 `byId` 那个更早声明它的节点。 */
export interface ShadowedCidr {
  cidr: string;
  /** 抢占者 serverId（首声明者）。角标 tooltip 要答「被谁覆盖」，只给 cidr 用户仍无从下手。 */
  byId: string;
}

/**
 * 跨组网节点同网段「被覆盖（shadowed）」检测：按 `servers` 顺序「首声明者占有」（与 route-builder
 * `claimedCidrs` 同一不变量——一条 ip_cidr 只能指向一个 outbound，首条命中即生效）。返回 serverId →
 * 该节点中被更早节点抢占、因而**不会**实际生效的具体段（含抢占者 id，仅含有冲突的节点）。供列表
 * 「网段被覆盖」角标提醒用：用户据此去重/调序/用自定义规则覆盖。
 *
 * **调用方应传 emitted 口径**（`meshForceRoutedServers` 的产物），与发射端同口径，避免对「仅出网且未
 * engaged」的节点虚报覆盖——该函数本身不做这层过滤（它只实现「首声明者占有」这一条不变量）。
 */
export function meshShadowedCidrs(servers: ServerConfig[]): Map<string, ShadowedCidr[]> {
  const claimedBy = new Map<string, string>();
  const result = new Map<string, ShadowedCidr[]>();
  for (const s of servers) {
    const shadowed: ShadowedCidr[] = [];
    for (const c of endpointForcedRouteCidrs(s)) {
      const owner = claimedBy.get(c);
      if (owner !== undefined) shadowed.push({ cidr: c, byId: owner });
      else claimedBy.set(c, s.id);
    }
    if (shadowed.length > 0) result.set(s.id, shadowed);
  }
  return result;
}
