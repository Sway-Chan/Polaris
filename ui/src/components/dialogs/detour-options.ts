/**
 * endpoint 弹窗的「前置代理 / detour」共用纯逻辑 —— 候选构造 + 提交写回。
 * 无 DOM / 无 react / 无 i18next 运行时依赖，可 vitest 直测（文案由组件译好传入）。
 *
 * # 这一层是对 上游的**有意偏离**（用户已授权，不是移植遗漏）
 *
 * 上游的 WireGuard / WARP / Tailscale 三个表单都没有 detour，`SingBoxEndpoint` 类型也没有这个键——
 * 它只在**代理 outbound** 上支持链式前置代理。Polaris 把这条能力延伸到 endpoint：生成侧见
 * `crates/config-engine/src/singbox/endpoint.rs` 的 `Endpoint::detour`（含 2026-07-31 的 loopback
 * A/B 实测数据），发射与剪枝见 `crates/config-engine/src/builder/outbounds.rs`。
 *
 * # 为什么三处共用一个函数（而不是各写各的）
 *
 * 此前 `WarpDialog` 自己拼了一份候选（`servers.filter(s => s.id !== editNode?.id)`），
 * 而生成侧对「detour 目标是 endpoint」是**直接丢弃**——那份候选里的 WG/TS 节点选了等于没选。
 * 三个弹窗各拼一份，等于把「候选集要跟生成侧的排除对齐」这条不变式交给人眼守三遍。
 */

import type { ServerConfig } from '@/contracts/types';
import type { SelectOption } from './field-spec';
import { landsInEndpoints } from '@/domain/endpoint-routes';

/**
 * 「不串联」哨兵。**不是**一个真的出站 tag：生成侧的 `direct` 出站另有其名，
 * 这里的 `'direct'` 只是下拉的空值占位，提交时被 [`applyDetour`] 翻译成「删键」。
 * 与 `NodeDialog` / `WarpDialog` 原有写法同字面量（那两处此前是裸串，本模块收口为常量）。
 */
export const DETOUR_NONE = 'direct';

/**
 * 前置代理候选：「不串联」+ 其余可作 detour 目标的节点。
 *
 * 两条排除，逐条对齐生成侧 `builder/outbounds.rs#resolve_detour_tag`：
 *  1. **自身**（`selfId`）—— 自指是环，生成侧的环检测会把它整条丢掉；
 *  2. **endpoint 类协议**（`isMeshProtocol` = wireguard / tailscale）—— 生成侧
 *     `is_mesh_protocol` 那一支直接丢弃 detour，留在候选里就是个选了没用的选项。
 *
 * # 射程如实标注：`custom` + `isEndpoint` 的节点**没有**被排除
 *
 * 它在生成侧走 `pending_endpoints`（不是 outbounds），故 `valid_tags` 里没有它的 tag ⇒
 * 指向它的 detour 会被判成**悬空引用**，引用方整个节点被剪掉并上报 invalid node。
 * 这里不排它，是为了与 Rust 侧 `is_mesh_protocol` 的判据**逐字一致**（那边同样只认
 * wireguard / tailscale）—— 两侧同源好过前端自己多一条规则、日后两边各自漂移。
 * 要收掉这个残留缺口，得**先**在 Rust 侧把 custom-endpoint 并进同一条排除（那会改到既有的
 * 代理 outbound 腿的行为），再同步这里；不属本批。
 */
export function endpointDetourOptions(
  servers: readonly ServerConfig[],
  selfId: string | undefined,
  noneLabel: string
): SelectOption[] {
  return [
    [DETOUR_NONE, noneLabel] as SelectOption,
    ...servers
      .filter((s) => s.id !== selfId && !landsInEndpoints(s.protocol))
      .map((s) => [s.id, s.name] as SelectOption),
  ];
}

/** 存量 `ServerConfig.detour` → 下拉初值（缺席 = 不串联）。 */
export function detourDraftValue(server?: ServerConfig): string {
  return server?.detour ? server.detour : DETOUR_NONE;
}

/**
 * 草稿值 → 写回 `ServerConfig.detour`（**原地改传入对象**，调用方传的都是刚拼好的新对象）。
 *
 * 「缺省即默认」：选了哨兵 / 空值 ⇒ **删键**而不是写 `'direct'`。写字面量 `'direct'` 会让生成侧
 * 去 `config.servers` 里找一个 id 为 `direct` 的节点、找不到 → 静默无 detour，行为虽同，
 * 但磁盘上从此躺着一个假的 id 引用（同 `wg-logic.ts` / `ts-settings-logic.ts` 文件头那条纪律）。
 *
 * `delete` 不是多余的：三个调用点都从 `...base` / `...node` 起底，不删就清不掉存量值。
 */
export function applyDetour<T extends ServerConfig | Omit<ServerConfig, 'id'>>(
  server: T,
  value: unknown
): T {
  const v = typeof value === 'string' ? value.trim() : '';
  if (v && v !== DETOUR_NONE) server.detour = v;
  else delete server.detour;
  return server;
}
