/**
 * 启动 gate 剔除原因（`InvalidNodeInfo.reason`）→ i18n 键。
 *
 * # 为什么需要这张表
 *
 * 后端下发的 `reason` 是**稳定机器 token**（`detour-cascade` / `control-url-ip` …），刻意不是人话
 * ——文案要按用户语言渲染，后端不该下发已定死语言的句子（见 `outbounds.rs`
 * `INVALID_REASON_DETOUR_CASCADE` 头注）。但在本表出现之前，`NodeCard` 是把 token **原样拼进
 * tooltip** 的：
 *
 * ```
 * `${t('nodes.nodeInvalid')}${invalidReason ? `: ${invalidReason}` : ''}`
 * ```
 *
 * ⇒ 用户看到的是「节点配置无效，已在启动时跳过: detour-cascade」——后半截是开发者标识符，
 * 五个语种一视同仁地看不懂，而且它恰恰是**唯一说明「为什么」**的那半句。
 *
 * 本表把 token 翻成可行动的话；token 未登记时**只渲染前半句**（不再吐标识符），
 * 由 `contracts/invalid-node-reason-coverage.test.ts` 保证「后端有的 token 这里都有」。
 *
 * 显式映射表而非约定拼接 `nodes.invalid.<token>`：拼接法在 token 漂移时会把
 * `nodes.invalid.some-new-token` 这串标识符渲染给用户，映射表则安静回落到通用句
 * （范式同 `domain/proxy-error-text.ts`）。
 */

/** reason token → i18n 键。键集由 `contracts/invalid-node-reason-coverage.test.ts` 与 Rust 源码对账。 */
export const INVALID_NODE_REASON_KEY: Readonly<Record<string, string>> = {
  'detour-cascade': 'nodes.invalidDetourCascade',
  'control-url-ip': 'nodes.invalidControlUrlIp',
  'control-url-scheme': 'nodes.invalidControlUrlScheme',
  'control-url-invalid': 'nodes.invalidControlUrlMalformed',
};

/**
 * 剔除原因 → 给用户看的整句。
 *
 * `reason` 为 `undefined` 表示该节点**没被剔除** → 返回 `null`（调用方据此不挂 tooltip）。
 * 空串 / 未登记 token → 只给通用句（`nodes.nodeInvalid`），**绝不把 token 拼给用户**。
 */
export function invalidNodeReasonText(
  reason: string | undefined,
  t: (key: string, fallback?: string) => string
): string | null {
  if (reason === undefined) return null;
  const generic = t('nodes.nodeInvalid');
  // 原型链上的属性（'toString' / 'constructor'）不得被当成键 —— reason 是从 IPC 收来的任意串，
  // 直接下标会取到 Object.prototype 上的函数，落进 t() 就是一串 [native code]。
  if (!Object.prototype.hasOwnProperty.call(INVALID_NODE_REASON_KEY, reason)) return generic;
  // 分隔符也必须走 locale：全角「：」只在中文里对，英/俄/波斯语用它是错的
  // （俄语用户会看到 `Node config invalid：reason`）。走 `t(key, 默认值)` 这条既有 idiom，
  // 与仓里 800+ 处同形，也让 i18n 裸 CJK 门自然放行。
  return `${generic}${t('nodes.nodeInvalidSep')}${t(INVALID_NODE_REASON_KEY[reason])}`;
}
