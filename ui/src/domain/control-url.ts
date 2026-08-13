/**
 * Tailscale `control_url` 前端校验 —— Rust `user_config/control_url.rs` 的**同判据**前端对应物。
 *
 * # 为什么前端也要有一份
 *
 * 后端那道 gate 是 fail-closed 的兜底：它在**生成 sing-box 配置时**把坏节点剔掉，用户看到的是
 * 节点卡置灰 + tooltip —— 那是**事后**告知，而且此时用户已经离开了填这个字段的那个弹窗。
 * 真正能让人「改对」的时机是**保存那一刻**，光标还停在 `controlUrl` 输入框旁边。故前端拦在保存前，
 * 后端拦在下发前，两道各守各的时机，缺一不可（后端那道还兼管「配置是从旧版本/订阅/手改 JSON 来的」）。
 *
 * # 判据同源纪律
 *
 * 判据本体是内核行为（见 Rust 侧头注：`endpoint.go:195` 的无条件类型断言）。两份实现同步靠
 * `contracts/control-url-parity.test.ts` —— 它**把 Rust 单测里的 URL 语料表读进来**当真值跑本模块，
 * 不是抄一份镜像常量。Rust 侧改了判据而这边没跟 ⇒ 那道门红。
 */

import { isIpLiteral, stripBrackets } from './ip-literal';

/** 被拒成因。取值与 Rust `reject_token` 逐字相同 —— 它们同时是 i18n 映射表的键。 */
export type ControlUrlReject = 'control-url-ip' | 'control-url-scheme' | 'control-url-invalid';

/**
 * host 是否 IP 字面量。
 *
 * **zone id 必须先截断**：`fe80::1%eth0` 在内核那边被 Go `netip` 判为 IP（实测 panic）；
 * 不截断这里会漏判成域名 = fail-open，正是这道门要防的事。
 */
function isIpHost(host: string): boolean {
  const h = stripBrackets(host).split('%')[0] ?? '';
  return isIpLiteral(h);
}

/** 端口后缀（`:8080`）。空端口（`host:`）不算。 */
function isPortSuffix(s: string): boolean {
  return /^:\d+$/.test(s);
}

/**
 * host 是否**可能**是域名。
 *
 * 只列否定字符、不做 LDH 白名单 —— headscale 用 IDN 域名完全合法，白名单会误伤。
 */
function isHostnameLike(host: string): boolean {
  // eslint-disable-next-line no-control-regex
  return host.length > 0 && !/[:/?#@[\]\\]|[\u0000-\u001f\u007f]/.test(host);
}

/**
 * Tailscale `control_url` 校验：`null` = 可用，非 null = **必须拦在保存/下发之前**。
 *
 * 空串 / 全空白 → `null`（没填 = 用官方 controlplane，内核走恒安全的 else 分支）。
 */
export function controlUrlReject(raw: string | null | undefined): ControlUrlReject | null {
  const s = (raw ?? '').trim();
  if (s === '') return null;
  // 内嵌空白 → 内核 `url.Parse` 直接报错，FATAL 掉整个核。
  if (/\s/.test(s)) return 'control-url-invalid';

  const pos = s.indexOf('://');
  if (pos < 0) return 'control-url-scheme';
  const scheme = s.slice(0, pos);
  if (!/^[a-zA-Z][a-zA-Z0-9+\-.]*$/.test(scheme)) return 'control-url-scheme';

  // authority = scheme 之后、首个 `/ ? #` 之前；再剥 userinfo（内核 `url.Hostname()` 同样只取 host）。
  const rest = s.slice(pos + 3);
  const authority = rest.split(/[/?#]/)[0] ?? '';
  const at = authority.lastIndexOf('@');
  const hostport = at >= 0 ? authority.slice(at + 1) : authority;

  // 方括号形式：内核只接受 IPv6 —— 是 IP 就 panic，不是 IP 就 `parse control URL` FATAL。
  if (hostport.startsWith('[')) {
    const end = hostport.indexOf(']');
    if (end < 0) return 'control-url-invalid';
    const inner = hostport.slice(1, end);
    const after = hostport.slice(end + 1);
    if (after !== '' && !isPortSuffix(after)) return 'control-url-invalid';
    return isIpHost(inner) ? 'control-url-ip' : 'control-url-invalid';
  }

  // 非方括号：末段全数字才当端口剥掉；否则残留的冒号意味着裸 IPv6 之类的畸形。
  const colon = hostport.lastIndexOf(':');
  let host = hostport;
  if (colon >= 0) {
    if (!isPortSuffix(hostport.slice(colon))) return 'control-url-invalid';
    host = hostport.slice(0, colon);
  }

  if (host === '') return 'control-url-invalid';
  if (isIpHost(host)) return 'control-url-ip';
  if (!isHostnameLike(host)) return 'control-url-invalid';
  return null;
}
