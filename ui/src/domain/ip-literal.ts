/**
 * IP 字面量判定 —— Rust `crates/config-engine/src/user_config/ip.rs` 的前端对应物。
 *
 * **为什么单独成模块**：此前这套谓词以**私有副本**的形式住在 `screens/settings/SettingsDns.tsx` 里。
 * `control_url` 校验（`domain/control-url.ts`）需要同一套判据，再抄一份就是第三份实现 ——
 * 三份之间任何一处修边界都不会有门说话。故抽到这里，SettingsDns 改为引用，副本数从 2 回到 1。
 *
 * 语义与 Rust 侧逐条对齐（含「允许前导零」这个刻意的宽松点，见 `ip.rs` 头注：对拍需与 上游
 * 正则 `1?\d?\d` 一致，比 `std::net::IpAddr` 宽）。
 */

/** `[::1]` → `::1`；单边畸形原样返回（下游 `isIpv6Literal` 据实拒之）。对齐 Rust `strip_brackets`。 */
export function stripBrackets(host: string): string {
  return host.length >= 2 && host.startsWith('[') && host.endsWith(']') ? host.slice(1, -1) : host;
}

/** IPv4 单段：1-3 位纯数字且 ≤255（允许前导零，对齐 Rust `is_ipv4_segment` 的 `1?\d?\d` 语义）。 */
function isIpv4Segment(seg: string): boolean {
  return seg.length > 0 && seg.length <= 3 && /^\d+$/.test(seg) && Number(seg) <= 255;
}

/** 严格四段 IPv4。对齐 Rust `is_ipv4`。 */
export function isIpv4(host: string): boolean {
  const parts = host.split('.');
  return parts.length === 4 && parts.every(isIpv4Segment);
}

/**
 * IPv6 字面量（去括号后 ≥2 个冒号）：(1) 纯 hex+冒号；(2) IPv4-mapped（hex+冒号前缀 + 点分末段）。
 * 对齐 Rust `is_ipv6_literal`。
 */
export function isIpv6Literal(host: string): boolean {
  const h = stripBrackets(host);
  if ((h.match(/:/g)?.length ?? 0) < 2) return false;
  if (/^[0-9a-fA-F:]+$/.test(h)) return true;
  const last = h.lastIndexOf(':');
  return /^[0-9a-fA-F:]+$/.test(h.slice(0, last + 1)) && isIpv4(h.slice(last + 1));
}

/** IP 字面量（v4 或 v6）。对齐 Rust `is_ip_literal`。 */
export function isIpLiteral(host: string): boolean {
  return isIpv4(host) || isIpv6Literal(host);
}
