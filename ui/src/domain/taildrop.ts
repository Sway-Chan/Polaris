/**
 * Taildrop 的纯判定（无 DOM / 无网络 / 无 react，可直测）。
 *
 * 只放两件事：**后端错误 code → i18n 键**的查表，和**能不能用**的三态判定。两者都是「同一个判据
 * 会在多处被问到」的东西 —— 弹窗要问、入口按钮要问、角标要问，写三份必然漂。
 */

import type { TailscaleStatusEvent } from '@/contracts/tailscale-status';

/**
 * 后端 `ApiResponse.code` → i18n 键。
 *
 * **Rust 侧的 `error` 字段是给日志看的英文诊断，不是给用户看的文案** —— 把中文写进 Rust 就等于
 * 把文案钉死在那一侧、绕开 i18n（本仓 Rust 用户可见 sink 禁裸中文，由 `i18n.rs` 自己的测试守）。
 * 故失败一律靠 `code` 查这张表；查不到的 code 回落成通用文案，**不回落成把英文诊断直接显示给用户**。
 */
export const TAILDROP_ERROR_KEY: Record<string, string> = {
  TAILDROP_ENDPOINT_UNAVAILABLE: 'taildrop.errUnavailable',
  TAILDROP_API_UNREACHABLE: 'taildrop.errApi',
  TAILDROP_CALL_FAILED: 'taildrop.errCall',
  TAILDROP_WRITE_FAILED: 'taildrop.errWrite',
};

/** 通用兜底键（未知 code / 无 code 的传输层异常）。 */
export const TAILDROP_ERROR_FALLBACK_KEY = 'taildrop.errUnknown';

/** 把后端 code 换成 i18n 键。`undefined` / 未登记 → 兜底键。 */
export function taildropErrorKey(code?: string): string {
  if (!code) return TAILDROP_ERROR_FALLBACK_KEY;
  return TAILDROP_ERROR_KEY[code] ?? TAILDROP_ERROR_FALLBACK_KEY;
}

/**
 * 该节点此刻能不能用 Taildrop —— 三态，**不是布尔**。
 *
 * 三态各自对应完全不同的下一步动作，塌成布尔就会做出「灰着但不说为什么」的按钮：
 *  - `ready`：可用。
 *  - `offline`：核没在跑 / 没收到状态帧 / 该节点未登录 tailnet ⇒ 用户该去连接。
 *  - `notGranted`：连上了，但 tailnet 没授 `cap/file-sharing` ⇒ 用户要去 admin console 开，
 *    **在本应用里做什么都没用**。这一格正是「拨了不生效的控件」的来源，必须能被说出来。
 */
export type TaildropAvailability = 'ready' | 'offline' | 'notGranted';

export function taildropAvailability(
  status: TailscaleStatusEvent | undefined
): TaildropAvailability {
  if (!status || !status.loggedIn) return 'offline';
  return status.canShareFiles ? 'ready' : 'notGranted';
}

/**
 * 角标数字：未读数。
 *
 * 取 `unreadFileCount` 而不是 `waitingFileCount` —— 读过但没删的文件仍在 waiting 里，
 * 拿 waiting 当角标会让角标永远消不掉（用户没有任何办法让它归零，除非删文件）。
 * 不可用时恒 0：一个连不上的收件箱不该在界面上顶着数字。
 */
export function taildropBadgeCount(status: TailscaleStatusEvent | undefined): number {
  if (taildropAvailability(status) !== 'ready') return 0;
  return Math.max(0, status?.unreadFileCount ?? 0);
}

/**
 * 接收进度百分比（0–100，整数）。`size <= 0` → 0：核在拿到总长前会先报 0，
 * 除零会得到 `NaN` 并把进度条渲染成空白，而空白与「0%」在界面上是两回事。
 */
export function receivingPercent(receivedBytes: number, size: number): number {
  if (!Number.isFinite(size) || size <= 0) return 0;
  const pct = Math.round((receivedBytes / size) * 100);
  return Math.min(100, Math.max(0, pct));
}
