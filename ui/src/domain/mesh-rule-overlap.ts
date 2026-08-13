/**
 * 路由规则 × 组网(WG/Tailscale) force-route 段的**重叠判定**（`meshOverlapRuleIds`）。
 *
 * # 它回答什么
 *
 * 组网节点会为自己的 `allowedIPs` / tailnet 段发射 force-route 条目（`endpointForcedRouteCidrs`），
 * 但**自定义规则的优先级更高**（优先级链：自定义规则 → 应用分流 → 组网 → 绕过局域网 → 智能 → 默认出口）。
 * 于是一条 `ipCidr` 规则只要与组网段相交，那一段流量就**不再走组网节点**了 —— sing-box 的
 * 「首条命中即生效」让这件事完全静默：配置能生成、内核能起、连接也通，只是没走你以为的路。
 * 本谓词把它在规则列表里标成「覆盖组网」角标（对齐 上游 `rules-page.tsx:109-131`）。
 *
 * # 为什么在这里新写一份 CIDR 相交
 *
 * 本仓 `domain/endpoint-routes.ts` 的 `meshShadowedCidrs` 只做**字面量去重**（同一条 cidr 串被两个
 * 节点声明），够它自己那个「同段先声明者胜」的判定，但答不了「`10.0.0.0/8` 与 `10.8.0.0/24` 相交吗」。
 * `domain/rules.ts` 的 `isValidIpCidr` 只判形状。故此处移植 上游 `src/shared/ip.ts:60-102` 的
 * 前缀比对算法（v4 用 uint32、v6 用 BigInt，跨族恒不相交），**逐字同口径**，不引第三方依赖。
 *
 * 纯函数、无 I/O。
 */
import type { Rule } from '../contracts/types';
import { ruleIpCidrs } from './rules';

/** IPv4 字面量（严格：每段 0-255，不含前导零之外的宽松形态）。 */
const IPV4_RE = /^(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)$/;

/** IPv4 CIDR `a.b.c.d[/n]` → [网络地址(uint32), 前缀]；非法/IPv6 → null。无 `/n` 视为 `/32`。 */
function parseIpv4Cidr(cidr: string): [number, number] | null {
  const [ipPart, prefixPart] = cidr.trim().split('/');
  const prefix = prefixPart === undefined ? 32 : Number(prefixPart);
  if (!IPV4_RE.test(ipPart) || !Number.isInteger(prefix) || prefix < 0 || prefix > 32) return null;
  const o = ipPart.split('.').map(Number);
  const ipInt = ((o[0] << 24) | (o[1] << 16) | (o[2] << 8) | o[3]) >>> 0;
  const mask = prefix === 0 ? 0 : (0xffffffff << (32 - prefix)) >>> 0;
  return [(ipInt & mask) >>> 0, prefix];
}

/** 两个 IPv4 CIDR 是否相交（按**较短前缀**比对网络地址）。任一非 IPv4 CIDR → false。 */
export function ipv4CidrsOverlap(a: string, b: string): boolean {
  const pa = parseIpv4Cidr(a);
  const pb = parseIpv4Cidr(b);
  if (!pa || !pb) return false;
  const minPrefix = Math.min(pa[1], pb[1]);
  if (minPrefix === 0) return true; // 0.0.0.0/0 覆盖一切
  const mask = (0xffffffff << (32 - minPrefix)) >>> 0;
  return ((pa[0] & mask) >>> 0) === ((pb[0] & mask) >>> 0);
}

/**
 * IPv6 字面量（含 `::` 压缩）→ 8 组 16-bit 无符号整数；非法 → null。
 *
 * **刻意不用 BigInt**：`vite.config.ts` 的 `build.target` 含 `safari13`，而 BigInt 字面量要 Safari 14 —
 * esbuild 无法降级 `0n`，会直接 `Big integer literals are not available` 硬失败（2026-07-28 踩到，
 * 单测在 node 下照绿、只有 `vite build` 会红）。8×16-bit 分组比对与 128-bit 整数按前缀比对等价，
 * 且与上面 v4 腿的 uint32 口径同构。
 */
function ipv6Groups(addr: string): number[] | null {
  const a = addr.trim();
  if (!a.includes(':') || !/^[0-9a-fA-F:]+$/.test(a)) return null;
  const halves = a.split('::');
  if (halves.length > 2) return null;
  const head = halves[0] ? halves[0].split(':') : [];
  const tail = halves.length === 2 && halves[1] ? halves[1].split(':') : [];
  if (halves.length === 1 && head.length !== 8) return null; // 无 :: 必须满 8 组
  const fill = 8 - head.length - tail.length;
  if (fill < (halves.length === 2 ? 1 : 0)) return null; // :: 至少省 1 组
  const groups = [...head, ...new Array(halves.length === 2 ? fill : 0).fill('0'), ...tail];
  if (groups.length !== 8) return null;
  const out: number[] = [];
  for (const g of groups) {
    if (g.length === 0 || g.length > 4) return null;
    const n = parseInt(g, 16);
    if (Number.isNaN(n)) return null;
    out.push(n);
  }
  return out;
}

/** 第 `i` 组（16 bit）在前缀 `prefix` 下的掩码：整组落在前缀内 → 0xffff，整组落在前缀外 → 0。 */
function groupMask(prefix: number, i: number): number {
  const bits = Math.min(16, Math.max(0, prefix - 16 * i));
  return bits === 0 ? 0 : (0xffff << (16 - bits)) & 0xffff;
}

/** IPv6 CIDR `addr/n` → [网络地址(8×16bit), 前缀]；非法/IPv4 → null。无 `/n` 视为 `/128`。 */
function parseIpv6Cidr(cidr: string): [number[], number] | null {
  const [ipPart, prefixPart] = cidr.trim().split('/');
  const prefix = prefixPart === undefined ? 128 : Number(prefixPart);
  if (!Number.isInteger(prefix) || prefix < 0 || prefix > 128) return null;
  const v = ipv6Groups(ipPart);
  if (v === null) return null;
  return [v.map((g, i) => g & groupMask(prefix, i)), prefix];
}

/** 两个 IPv6 CIDR 是否相交（按较短前缀比对网络地址）。任一非 IPv6 CIDR → false。 */
export function ipv6CidrsOverlap(a: string, b: string): boolean {
  const pa = parseIpv6Cidr(a);
  const pb = parseIpv6Cidr(b);
  if (!pa || !pb) return false;
  const minPrefix = Math.min(pa[1], pb[1]);
  if (minPrefix === 0) return true;
  for (let i = 0; i < 8; i += 1) {
    const m = groupMask(minPrefix, i);
    if (m === 0) break; // 后续组全在前缀外，无需比对
    if ((pa[0][i] & m) !== (pb[0][i] & m)) return false;
  }
  return true;
}

/** 两个 CIDR 是否相交（按地址族自动分派 v4/v6；**跨族恒不相交**）。 */
export function cidrsOverlap(a: string, b: string): boolean {
  return ipv4CidrsOverlap(a, b) || ipv6CidrsOverlap(a, b);
}

/** target 是否与候选集里任一 CIDR 相交（v4+v6 家族感知）。 */
export function cidrOverlapsAny(target: string, candidates: string[]): boolean {
  return candidates.some((c) => cidrsOverlap(target, c));
}

/**
 * 与组网 force-route 段重叠的**已启用**规则 id 集合（供规则列表就地角标）。
 *
 * - 只看 `ipCidr` 条件（`ruleIpCidrs`）：域名/端口/进程类规则与 IP 路由段不在同一判定面上，标了是噪音。
 * - **只判已启用规则**：禁用规则不下发，本就抢不走组网的路由（与 `missingResourceRuleIds` 同口径）。
 * - `meshCidrs` 应由调用方传 **emitted 口径**（`meshForcedRouteCidrs(meshForceRoutedServers(...))`），
 *   即「本轮真会发射 force-route」的那批节点的段 —— 与发射端同源，才不会对「仅出网、未 engaged」的
 *   节点虚报覆盖。
 */
export function meshOverlapRuleIds(rules: Rule[], meshCidrs: string[]): Set<string> {
  const ids = new Set<string>();
  if (!meshCidrs.length) return ids;
  for (const r of rules || []) {
    if (!r.enabled) continue;
    if (ruleIpCidrs(r).some((c) => cidrOverlapsAny(c, meshCidrs))) ids.add(r.id);
  }
  return ids;
}
