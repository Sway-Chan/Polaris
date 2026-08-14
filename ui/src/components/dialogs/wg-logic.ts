/**
 * WgDialog 纯逻辑 —— .conf 粘贴解析结果 ↔ 表单草稿 ↔ ServerConfig 的对称接线（无 DOM/网络，可 vitest）。
 *
 * **解析本身不在这里**：复用 `domain/wg-quick.ts` 的 `parseWgQuickConf`（审计 §B 待接线物料，勿重写）。
 * 本文件只做「解析结果 → 表单字段」的填充映射（对齐原型 wgParseConf :4939）与「表单草稿 → WireGuard
 * ServerConfig」的构造（提交路径），两侧成对定义、往返可测（R5 对称层同型）。
 *
 * 全隧道语义（原型 :4943）：peer.AllowedIPs 里的 `0.0.0.0/0` / `::/0` 归 `allowInternet`（全隧道开关），
 * 表单的「路由网段」只显示**具体网段**（滤掉 catch-all）；提交时 catch-all 由 allowInternet 在生成侧接管。
 *
 * # 缺省即默认（`reverseMesh` / `reserved` 这一批的纪律，同 `ts-settings-logic.ts` 文件头）
 *
 * 键的**缺席**有确定语义，回显必须显示那个真实缺省，提交必须**不把默认值写成显式值**：写 `false`
 * 与「键不存在」对后端等价，却会把当下的默认值复制一份到磁盘上——日后改默认，存量配置不跟随，
 * 磁盘就成了第二个默认值真值源。故 `reverseMesh` 用户没开就**删键**，不写 `false`。
 *
 * 缺省口径逐条对齐**消费侧谓词**，不在这里另立一套：
 *  - `reverseMesh` 缺省 **false**：`meshUsesSystemInterface`（domain/endpoint-routes.ts:279）取
 *    `=== true`，Rust `mesh_uses_system_interface`（builder/endpoint_routes.rs:115-121）是
 *    `unwrap_or(false)`。
 *  - `reserved` 缺省 **不下发**：Rust 侧是 `Vec<u32>` + `skip_serializing_if = "Vec::is_empty"`
 *    （server_config.rs:138-139），消费侧 `builder/endpoints.rs:106` 的谓词是
 *    **`s.reserved.len() == 3`** —— 不满足就压根不写进 sing-box peer。故「不足/超过 3 项」与
 *    「键不存在」对后端**逐字等价**，提交侧按同一条谓词删键（见 [`parseReserved`]），
 *    不在盘上留一个永不生效的残值（同 `ts-settings-logic.ts` 对 `acceptDefaultResolvers` 的处理）。
 *
 * 既有的 `allowInternet` / `alwaysRouteSubnets` 沿用原写法（无条件写显式值），本轮不动：改它们会改到
 * 既有节点的落盘形状，属独立一批，且值与默认等价、无行为差。
 */

import type { ServerConfig } from '@/contracts/types';
import type { WireGuardSettings } from '@/contracts/types';
import type { FormValues } from './field-spec';
import { parseNumberField } from './field-spec';
import { parseWgQuickConf, type ParsedWgQuick } from '@/domain/wg-quick';
import { isWarpServer } from '@/domain/warp';
import { applyDetour, detourDraftValue, DETOUR_NONE } from './detour-options';

const CATCH_ALL = new Set(['0.0.0.0/0', '::/0']);

/** 逗号分隔字符串 → 去空 trim 列表。 */
export function splitCsv(v: unknown): string[] {
  if (typeof v !== 'string') return [];
  return v
    .split(',')
    .map((x) => x.trim())
    .filter(Boolean);
}

function str(v: unknown): string {
  return typeof v === 'string' ? v : '';
}

/**
 * `Reserved` 输入串 → `number[]`；**不满足消费侧谓词一律 `undefined`**（= 等价于缺席）。
 *
 * **全库唯一实现点**（`WarpDialog` 亦经此，不另写一份）。判据逐条来自后端，不另立一套：
 *  - **恰 3 项**：`builder/endpoints.rs:106` 只在 `s.reserved.len() == 3` 时才写进 sing-box peer；
 *  - **逐项 0–255 整数**：sing-box/xray 的 reserved 是 **3 字节**（来源 `mesh/warp.rs:179`
 *    `reserved_from_client_id` = base64(client_id) 的前 3 **字节**）。越界值即使落盘也永不生效，
 *    而负数/小数更糟：Rust 侧是 `Vec<u32>`，`-1` / `1.5` 会让**整条 `server.add/update` IPC**
 *    在 serde 反序列化处失败，用户只看到一句「保存失败」。
 *
 * 与 上游 `wireguard-form.tsx:53` `parseReserved` 的**一处刻意不同**：上游 是「先 filter 掉非法项
 * 再看剩几个」，于是 `1,2,999,3` 会被悄悄改写成 `[1,2,3]`（丢掉一项、且用户看不出）。此处改为
 * 「先要求恰 3 项、再逐项校验」，非法输入整体作废并由 [`reservedInputInvalid`] 在提交前报错。
 */
export function parseReserved(raw: unknown): number[] | undefined {
  const parts = splitCsv(raw);
  if (parts.length !== 3) return undefined;
  const nums = parts.map(Number);
  return nums.every((n) => Number.isInteger(n) && n >= 0 && n <= 255) ? nums : undefined;
}

/**
 * 「填了、但填得不对」——提交前拦下的判据（空 = 没填，合法）。
 *
 * 为什么要拦而不是静默丢：后端对不满足谓词的 `reserved` 是**静默忽略**（见 [`parseReserved`]），
 * 前端不拦，用户就会遇到「界面收下了、盘上没有」——既没报错也没回显，无从得知自己填错了。
 * 同 `ts-settings-logic.ts#invalidTsCidrs` 那条腿的理由。
 */
export function reservedInputInvalid(raw: unknown): boolean {
  return splitCsv(raw).length > 0 && parseReserved(raw) === undefined;
}

/** WG 表单草稿键（与 WG_SPEC 的 FieldSpec.k 一一对应）。 */
export interface WgDraft extends FormValues {
  address: string;
  port: number | undefined;
  privateKey: string;
  localAddress: string;
  peerPublicKey: string;
  preSharedKey: string;
  allowedIPs: string;
  persistentKeepalive: number | undefined;
  mtu: number | undefined;
  reserved: string;
  reverseMesh: boolean;
  allowInternet: boolean;
  alwaysRouteSubnets: boolean;
  /** 前置代理 —— 节点 id 或 `DETOUR_NONE` 哨兵（`detour-options.ts`）。对 上游的有意偏离。 */
  detour: string;
}

/**
 * 该草稿指向的是不是 Cloudflare WARP —— 判据同 `domain/warp.ts#isWarpServer`（`warpDevice` 标记 +
 * 端点域名兜底），不另立一套。
 *
 * WARP 恒 gVisor：内核接口对它无意义（不是子网路由器、不可被反向访问），且 WARP 的 `system:true`
 * 会与主 TUN / 另一 System 接口抢内核 utun → `Connect: resource busy` FATAL（`domain/warp.ts:39` 记的
 * 真机实证）。两侧现均已否决：渲染端 `meshUsesSystemInterface`（domain/endpoint-routes.ts）与
 * Rust `mesh_uses_system_interface`（`builder/endpoint_routes.rs:120` 调 `crate::warp::is_warp_server`），
 * 同源不漂移由 `contracts/warp-veto-parity.test.ts` 守。
 *
 * **但那两道守的是「读」，不是「写」**：它们让一个 `reverseMesh:true` 的 WARP 节点不发 `system:true`，
 * 却不阻止这个自相矛盾的值躺在磁盘上（导入配置 / 手改 config.json / 从 上游 迁移都能造出来）。
 * 控件禁用也挡不住它 —— 提交时 `...base` 会把存量值原样带过。故**提交侧必须再否决一次**：
 * WG 走 [`buildWgServer`]，WARP 走 [`buildWarpSettings`]。
 */
export function isWarpDraft(draft: FormValues, base?: ServerConfig): boolean {
  return isWarpServer({
    protocol: 'wireguard',
    address: str(draft.address),
    wireguardSettings: base?.wireguardSettings,
  });
}

/** 新增态默认草稿（原型 wgReset :4913：port 51820 / keep 25 / mtu 1408 / 两开关默认开）。 */
export function emptyWgDraft(): WgDraft {
  return {
    address: '',
    port: 51820,
    privateKey: '',
    localAddress: '',
    peerPublicKey: '',
    preSharedKey: '',
    allowedIPs: '',
    persistentKeepalive: 25,
    mtu: 1408,
    reserved: '', // 缺省即默认（见文件头）：不填 = 不下发 reserved
    reverseMesh: false, // 缺省即默认（见文件头）：gVisor 用户态，零提权
    allowInternet: true,
    alwaysRouteSubnets: true,
    detour: DETOUR_NONE, // 缺省即默认：不串联 ⇒ 提交时删键，不写字面量
  };
}

/**
 * 解析结果 → 表单草稿（原型 wgParseConf :4945 的字段填充等价）。
 * catch-all 抽进 allowInternet；allowedIPs 只留具体段。
 */
export function draftFromParsed(p: ParsedWgQuick): WgDraft {
  const all = p.settings.allowedIPs ?? [];
  const hasCatchAll = all.some((a) => CATCH_ALL.has(a));
  const specific = all.filter((a) => !CATCH_ALL.has(a));
  return {
    address: p.address,
    port: p.port,
    privateKey: p.settings.privateKey,
    localAddress: p.settings.localAddress.join(', '),
    peerPublicKey: p.settings.peerPublicKey,
    preSharedKey: p.settings.preSharedKey ?? '',
    allowedIPs: specific.join(', '),
    persistentKeepalive: p.settings.persistentKeepalive ?? 25,
    mtu: p.settings.mtu ?? 1408,
    // wg-quick .conf 里没有 Reserved（它是 sing-box/xray 对 WG 的扩展，不是 wg-quick 的 INI 键，
    // 故 `domain/wg-quick.ts` 的解析结果里也没有这一项）→ 恒缺省，由用户在表单里补。
    reserved: '',
    // wg-quick .conf 没有「接入模式」这个概念（它是 Polaris 的 Phase 2 语义，不是 WG 协议字段）→ 恒缺省。
    reverseMesh: false,
    allowInternet: hasCatchAll,
    alwaysRouteSubnets: true,
    // wg-quick .conf 同样没有「前置代理」这个概念（sing-box 的 Dial Field，不是 WG 协议字段）→ 恒缺省。
    detour: DETOUR_NONE,
  };
}

/** 粘贴文本 → 草稿（解析失败返 null，调用方提示改手填）。薄封装：解析走 wg-quick.ts。 */
export function parseConfToDraft(raw: string): WgDraft | null {
  const parsed = parseWgQuickConf(raw);
  return parsed ? draftFromParsed(parsed) : null;
}

/** 编辑态：现有 WireGuard ServerConfig → 表单草稿。 */
export function draftFromServer(server: ServerConfig): WgDraft {
  const s = server.wireguardSettings;
  const all = s?.allowedIPs ?? [];
  return {
    address: server.address ?? '',
    port: server.port,
    privateKey: s?.privateKey ?? '',
    localAddress: (s?.localAddress ?? []).join(', '),
    peerPublicKey: s?.peerPublicKey ?? '',
    preSharedKey: s?.preSharedKey ?? '',
    allowedIPs: all.filter((a) => !CATCH_ALL.has(a)).join(', '),
    persistentKeepalive: s?.persistentKeepalive ?? 25,
    mtu: s?.mtu ?? 1408,
    // 缺席回显空串（= 真实缺省「不下发」）。回显**照盘上原样**而不套 [`parseReserved`]：盘上若躺着
    // 一个不满足谓词的残值（如 `[1,2]`），用户得先看见它才谈得上改；提交时它会按同一条谓词被删掉。
    reserved: (s?.reserved ?? []).join(', '),
    // 缺席回显真实缺省 false，判据与消费侧 `meshUsesSystemInterface` 的 `=== true` 逐字对齐（见文件头）。
    reverseMesh: s?.reverseMesh === true,
    allowInternet: s?.allowInternet !== false,
    alwaysRouteSubnets: s?.alwaysRouteSubnets !== false,
    // detour 在 ServerConfig **顶层**（不在 wireguardSettings 里），故取 `server` 而非 `s`。
    detour: detourDraftValue(server),
  };
}

export interface WgValidationError {
  field: 'name' | 'address' | 'privateKey' | 'localAddress' | 'peerPublicKey';
}

/** 提交前必填校验（名称 + 地址/私钥/接口地址/对端公钥）。返回首个缺失字段，全填 → null。 */
export function validateWgDraft(name: string, draft: WgDraft): WgValidationError | null {
  if (!name.trim()) return { field: 'name' };
  if (!str(draft.address).trim()) return { field: 'address' };
  if (!str(draft.privateKey).trim()) return { field: 'privateKey' };
  if (splitCsv(draft.localAddress).length === 0) return { field: 'localAddress' };
  if (!str(draft.peerPublicKey).trim()) return { field: 'peerPublicKey' };
  return null;
}

/**
 * 表单草稿 → WireGuard ServerConfig（提交路径）。
 * base 提供 id / 保全非模型字段（编辑态；WARP 的 warpDevice 等经 base.wireguardSettings 起底）。
 */
export function buildWgServer(
  name: string,
  draft: WgDraft,
  base?: ServerConfig,
): ServerConfig {
  const settings: WireGuardSettings = {
    // 编辑态起底非表单字段（reverseMesh / warpDevice / reserved…）；新增态干净起。
    ...(base?.wireguardSettings ?? {}),
    privateKey: str(draft.privateKey).trim(),
    localAddress: splitCsv(draft.localAddress),
    peerPublicKey: str(draft.peerPublicKey).trim(),
    allowInternet: draft.allowInternet !== false,
    alwaysRouteSubnets: draft.alwaysRouteSubnets !== false,
  };
  const psk = str(draft.preSharedKey).trim();
  if (psk) settings.preSharedKey = psk;
  else delete settings.preSharedKey;
  const allowed = splitCsv(draft.allowedIPs);
  if (allowed.length) settings.allowedIPs = allowed;
  else delete settings.allowedIPs;
  const keep = parseNumberField(String(draft.persistentKeepalive ?? ''));
  if (keep !== undefined) settings.persistentKeepalive = keep;
  const mtu = parseNumberField(String(draft.mtu ?? ''));
  if (mtu !== undefined) settings.mtu = mtu;

  // Reserved —— **缺省即默认**：不满足消费侧谓词（恰 3 项 × 0–255）即等价于缺席，删键而非留残值。
  // `delete` 同样不是多余：上面 `...base?.wireguardSettings` 起底了存量值，不删就清不掉。
  const rsv = parseReserved(draft.reserved);
  if (rsv) settings.reserved = rsv;
  else delete settings.reserved;

  // 接入模式（reverseMesh）—— **缺省即默认**：没开就删键而不是写 false（见文件头）。
  // `delete` 不是多余的：上面 `...base?.wireguardSettings` 起底了存量值，不删就关不掉。
  // WARP 否决见 [`isWarpDraft`]：控件已禁用，但存量/手输值仍可能带着 true 走到这里。
  if (draft.reverseMesh === true && !isWarpDraft(draft, base)) settings.reverseMesh = true;
  else delete settings.reverseMesh;

  const server: ServerConfig = {
    id: base?.id ?? '',
    name: name.trim(),
    protocol: 'wireguard',
    address: str(draft.address).trim(),
    port: parseNumberField(String(draft.port ?? '')) ?? 51820,
    wireguardSettings: settings,
  };
  // 前置代理：此前是 `if (base?.detour) server.detour = base.detour`（读得进、写不出——
  // WG 表单没有这个控件，只能把存量值原样带过）。本轮补上控件后改由草稿定夺，
  // 「不串联」走删键（见 `detour-options.ts#applyDetour`）。
  applyDetour(server, draft.detour);
  if (base?.subscriptionId) server.subscriptionId = base.subscriptionId;
  if (base?.createdAt) server.createdAt = base.createdAt;
  return server;
}

/**
 * WARP 表单草稿 → `WireGuardSettings`（`WarpDialog` 的提交路径；注册态与编辑态共用）。
 *
 * `base` 两种来源：注册态 = `registerWarp` 刚下发的草稿（privateKey / peerPublicKey / reserved /
 * warpDevice…），编辑态 = 现有 WARP 节点的 `wireguardSettings`。表单只覆盖 MTU / 保活，注册凭据
 * 与 Reserved 原样带过；路由字段则收口为 WARP 固定语义。
 *
 * # 为什么放在这里而不是留在 `WarpDialog.tsx` 里当闭包
 *
 * 与 [`buildWgServer`] **并排**，「一处补了、另一处漏」这件事就在同屏可见；且它是纯函数，
 * `wg-logic.test.ts` 能直接给那道否决上牙（闭包只能靠源码 grep 守，是弱得多的门）。
 * 覆盖门（`contracts/protocol-settings-coverage.test.ts`）不受影响：它刻意不把本文件算作「编辑器」，
 * WARP 的 Reserved / Allowed IPs 不再是控件：前者由 Cloudflare 注册结果下发，后者固定为全隧道 peer。
 *
 * # `reverseMesh` 恒删键（本函数的承重行）
 *
 * WARP 结构上永远不能走 System 内核接口（理由见 [`isWarpDraft`]）。UI 不展示这个不适用的控件，
 * 但盘上原有的 `reverseMesh:true`（导入 / 手改 config.json / 上游 迁移三条不经渲染端的入口）
 * 仍会经上面的 `...base` 幸存下来，被原样写回。
 * 故此处无条件 `delete`：本弹窗产出的节点**恒是 WARP**（单例槽 + `registerWarp`／编辑现有 WARP），
 * 没有「有时该保留」的分支。删键而非写 `false` 是「缺省即默认」（见文件头）。
 */
export function buildWarpSettings(
  base: Partial<WireGuardSettings>,
  draft: FormValues,
): WireGuardSettings {
  const s: WireGuardSettings = { ...(base as WireGuardSettings) };
  const mtu = parseNumberField(String(draft.mtu ?? ''));
  if (mtu !== undefined && Number.isInteger(mtu) && mtu > 0) s.mtu = mtu;
  else delete s.mtu;
  const keep = parseNumberField(String(draft.keepalive ?? ''));
  if (keep !== undefined && Number.isInteger(keep) && keep >= 0) s.persistentKeepalive = keep;
  else delete s.persistentKeepalive;

  // WARP 是 Cloudflare 全隧道 peer；由 Polaris 的选择器/分流规则决定哪些流量使用它。
  // 删除旧配置里的自定义路由字段，让通用缺省语义生成 0/0 + ::/0，且不制造额外 force-route。
  delete s.allowedIPs;
  delete s.allowInternet;
  delete s.alwaysRouteSubnets;
  // ── WARP 恒否决 System 接入模式（见函数头「承重行」）。表单不展示也挡不住旧值。
  delete s.reverseMesh;
  return s;
}
