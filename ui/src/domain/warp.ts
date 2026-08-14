/**
 * Cloudflare WARP —— 渲染端持有的最小面。
 *
 * WARP 即 WireGuard：匿名注册一个设备 → 拿到一份 WG 配置（对端公钥/端点/分配 IP/client_id），
 * Polaris 直接作为普通 WireGuard endpoint 节点使用（已支持 reserved）。
 *
 * **注册 / 注销 / 待注销队列的单一真值在 Rust**（`crates/mesh/src/warp.rs`，含版本串常量、
 * `build_register_body` / `parse_register_response` / `build_unregister_request` /
 * `classify_deregister_result` / `enqueue_pending_deregister` / `plan_deregister_drain`，
 * 队列持久化见 `src-tauri/src/runtime/mesh.rs` 的 `warp_queue_path`）。
 * 渲染端只经 `api.registerWarp()` 拿草稿——那套逻辑要发 HTTP、要落盘，在 webview 里本就跑不起来，
 * 故 TS 副本已删（第二轮 shared 清理，审计 §D）。
 *
 * 本文件只保留两类：
 *  - ①跨语言类型镜像：`WarpWireGuardDraft`（`registerWarp` invoke 返回，对应 Rust `WarpWireGuardDraft`）；
 *  - ③同步渲染路径的即时门控谓词：`findWarpNode` / `warpSlotTaken`（单例闸，Rust 侧无对应物，
 *    输入均已在前端 store，漂移后果止于 UI）。
 *  - ④**双侧谓词**：`isWarpServer` —— 曾按 ③ 归类（「Rust 无对应物、后果止于 UI」），**那个前提是错的**：
 *    它同时是 `meshUsesSystemInterface` 的否决判据，而 `system:true` 的唯一发射方是 config-engine，
 *    落盘的 `servers[]` 又有导入配置 / 手改 `config.json` / 上游 迁移三条**不经渲染端**的入口 ⇒
 *    漂移后果是 `Connect: resource busy` **FATAL**，不止于 UI。Rust 对应物见
 *    `crates/config-engine/src/warp.rs` 的 `is_warp_server`（`WARP_ENDPOINT_DOMAIN` / `WARP_MTU` 的 Rust
 *    真值也在那儿，前端镜像由 `src/contracts/warp-veto-parity.test.ts` 守）。
 *
 * DESIGN-REVIEW(warp-singleton-subscription-path-uncovered)：单例闸门覆盖的是**渲染端造节点的每条腿**
 * （`meshSingletonConflict`，接线于 NodeDialog / WgDialog / ImportDialog / WarpDialog 注册 / 节点克隆）。
 * `apply_subscription`（`src-tauri/src/commands/subscription.rs`）直接把订阅解析产物写进 `servers[]`，
 * **不经 `server_add`、也不经渲染端** → 不在本闸门射程内。当前**不可达**故不处理：net-stack 的三个解析器
 * 都跳过 wireguard（`clash_parser.rs:177` 明列不支持、`singbox_import.rs` 的 `SINGBOX_SUPPORTED_TYPES`
 * 不含 wireguard、share-link 无 `wireguard://`），订阅里出不来 WARP/Tailscale 节点。
 * **触发条件**：一旦 net-stack 开始解析 wireguard / sing-box `endpoints[]` 型订阅
 * （`subscription.rs#detect_format` 已为 `endpoints` 留了分支），该路径即成为第 6 条造节点腿，
 * 必须在 Rust 侧补同款闸（Rust 版 `is_warp_server` 已就位、可直接复用，见 `commands/server.rs` 的
 * DESIGN-REVIEW(mesh-singleton-guard-renderer-only) 对「第二真值源」代价的权衡）。
 */

/** WARP 端点域名锚点：注册响应给出的 endpoint 均属此域（engage / 162.159.x 走 *.cloudflareclient.com）。 */
export const WARP_ENDPOINT_DOMAIN = 'cloudflareclient.com';

/** WARP 接口缺省 MTU；与 config-engine 的 `WARP_MTU` 逐字对拍。 */
export const WARP_MTU = 1280;

/**
 * 判定 WireGuard 节点是否为 Cloudflare WARP。**鲁棒**：新节点带自删凭据 `warpDevice`，但**旧/导入的 WARP 节点无此标记**
 * → 必须同时按端点域名（`*.cloudflareclient.com`）兜底，否则旧 WARP 漏判（真机实证：旧 WARP 节点 `warpDevice` 缺失，
 * 致接入模式/子网路由该隐藏未隐藏、且 system 误判可触发 `Connect: resource busy`）。供 builder 接入模式否决
 * （meshUsesSystemInterface）、WG 表单只读/隐藏组网、列表角标共用单一真值。
 */
export function isWarpServer(server: {
  protocol?: string;
  address?: string;
  wireguardSettings?: { warpDevice?: unknown } | null;
}): boolean {
  if (server.protocol?.toLowerCase() !== 'wireguard') return false;
  if (server.wireguardSettings?.warpDevice) return true;
  return (server.address || '').toLowerCase().includes(WARP_ENDPOINT_DOMAIN);
}

/** 组网接入区里已注册的 WARP 节点（单例：至多一个）。纯函数，供接入区从「接入」切「已接入·管理」+ 单测。 */
export function findWarpNode<T extends Parameters<typeof isWarpServer>[0]>(
  servers: T[]
): T | undefined {
  return servers.find((s) => isWarpServer(s));
}

/**
 * WARP 单例守卫：已存在 WARP 节点则「槽位」被占——接入区不再提供「再加一个」（行为变更，用户签核）。
 * editingId 排除自身——编辑现有 WARP 节点不算「再加一个」，必须放行（对照 tailscaleSlotTaken）。
 * 纯函数：UI 接入区分流 + saveServer/cloneServer 硬闸门（防手输/导入/克隆旁路造第二个）共用，可离线单测。
 */
export function warpSlotTaken(
  servers: (Parameters<typeof isWarpServer>[0] & { id?: string })[],
  editingId?: string
): boolean {
  return servers.some((s) => isWarpServer(s) && s.id !== editingId);
}

/**
 * WARP 注册产出的 WireGuard 草稿（无 id，供渲染端填表）——`api.registerWarp()` 的 invoke 返回。
 * 对应 Rust `crates/mesh/src/warp.rs` 的 `WarpWireGuardDraft`（①类类型镜像，无 codegen 时的必要对应物）。
 *
 * warpDevice = 自删凭据（deviceId+token），随节点落 wireguardSettings.warpDevice、与 privateKey 同脱敏
 * （历史上「token 不持久化」红线已被用户拍板放宽，见 WARP 设计 §设备移除 / 不变量）。
 */
export interface WarpWireGuardDraft {
  address: string;
  port: number;
  privateKey: string;
  peerPublicKey: string;
  localAddress: string[];
  reserved?: number[];
  meta: { deviceId: string; accountId: string; license: string; warpPlus: boolean };
  /** 远端设备自删凭据（deviceId+token）。删除此节点时据它发 DELETE /reg/{deviceId} 注销匿名设备。 */
  warpDevice: { deviceId: string; token: string };
}
