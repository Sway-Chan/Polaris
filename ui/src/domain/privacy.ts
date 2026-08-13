/**
 * 隐私锁纯逻辑（无 React / 无 IPC）—— 供 LockOverlay / idle 计时 hook / LogsScreen 复用 + 单测。
 *
 * 三块决策全在此单点，组件只做渲染 + IO：
 *  1. resolveUnlockAttempt —— 解锁提交决策（对齐后端 unlock_core：未设密码 hash 空 = 空密码自由解锁）。
 *  2. shouldArmIdleLock —— 闲置计时是否武装（仅「开了自动隐私锁」且「当前未锁」时）。
 *  3. redactSensitive —— 日志行 域名 / IPv4 / IPv6 客户端兜底脱敏（honor #log-privacy-note 承诺；
 *     后端在隐私态抬日志级别是源头脱敏，此为覆盖「切换前已缓冲旧行」的显示层兜底）。
 */

/** 自动隐私锁闲置阈值（原型 Settings「闲置 10 分钟」）。 */
export const IDLE_PRIVACY_LOCK_MS = 10 * 60 * 1000;

/**
 * 解锁提交决策：
 *  - 已设密码且输入为空 → 'require-input'：本地提示「请输入密码」，不打后端（对齐原型 tryUnlock 空值分支）。
 *  - 其余（含**未设密码 + 空输入**）→ 'unlock'：交后端 privacy_unlock 判定。
 *    未设密码时后端 unlock_core 对空 hash 恒返 ok:true（自由解锁），故空密码在此放行。
 */
export function resolveUnlockAttempt(
  hasPassword: boolean,
  input: string,
): 'require-input' | 'unlock' {
  return hasPassword && input.length === 0 ? 'require-input' : 'unlock';
}

/** 闲置计时是否武装：仅当开了「自动隐私锁」且当前未处于锁定态（已锁再武装 = 重复触发 setPrivacyMode）。 */
export function shouldArmIdleLock(autoPrivacyMode: boolean, locked: boolean): boolean {
  return autoPrivacyMode && !locked;
}

/**
 * 实时日志显示层是否脱敏（C18）：隐私锁开（源头/显示双重兜底）**或**用户开了「常态脱敏」偏好。
 * 二者任一为真即对该行域名/IP 走 `redactSensitive`。privacyMode 优先——锁定期恒脱敏，与承诺一致。
 */
export function shouldRedactLogs(privacyMode: boolean, redactPref: boolean): boolean {
  return privacyMode || redactPref;
}

// ── 客户端兜底脱敏 ──
const IPV4 = /\b(?:\d{1,3}\.){3}\d{1,3}\b/g;
// IPv6：要求 ≥3 段冒号分隔（{3,7} 组「seg:」），故 HH:MM:SS 时间戳（仅 2 冒号）不误伤；
// 压缩形（::1）罕见于代理日志，为规避时间戳误报而不覆盖，属可接受取舍。
const IPV6 = /\b(?:[0-9a-fA-F]{1,4}:){3,7}[0-9a-fA-F]{1,4}\b/g;
// 域名：≥1 段 + 纯字母 TLD（≥2）。TLD 限纯字母 → 版本号（1.14.0-alpha.43 / v0.1.1）末段非纯字母，不误伤。
const DOMAIN = /\b(?:[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?\.)+[a-zA-Z]{2,}\b/g;
const MASK = '•••'; // •••

/**
 * 掩去日志文本中的 IPv4 / IPv6 / 域名（隐私态显示层兜底）。
 * 过度脱敏可接受（隐私优先，如 config.json → •••）；**欠脱敏不可接受**（会泄露浏览目标）。
 * 先 IPv4/IPv6 再域名：IP 被掩成 ••• 后不含点，域名正则不会二次命中。
 */
export function redactSensitive(text: string): string {
  return text.replace(IPV4, MASK).replace(IPV6, MASK).replace(DOMAIN, MASK);
}
