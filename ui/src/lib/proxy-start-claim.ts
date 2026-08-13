/**
 * 「本窗口刚发起过起核」认领闸门 —— 提权门两码（`HELPER_NOT_INSTALLED` / `HELPER_GATE_ABORTED`）
 * 与 `ROOT_ORPHAN_BLOCKED`（残留 root 孤儿阻断起核）共三码的**唯一去重依据**。
 *
 * # 为什么需要它
 *
 * 后端 `runtime/proxy.rs::set_error` 对这三码是**双出口**：既 emit `event:proxyError`（事件腿），
 * 又把码经 `commands/proxy.rs` 带回让 `api.proxy.start` reject（await 腿）。两条腿各自提示 ⇒ 同一次
 * 失败弹两遍（`HELPER_NOT_INSTALLED` 更是 toast + 桌面通知 + 「去安装」模态三重）。这正是 App.tsx
 * 对 `STARTUP_FAILED` 写明的既有约定所禁止的：「两处都报 = 同一次失败弹两遍」。
 *
 * 但**不能**照搬 `STARTUP_FAILED` 的「事件腿整条忽略」：这三码的发起方常常没人 await —— 托盘切档位、
 * 启动自动连接、`switchMode` 去抖重启都不经 Home 连接按钮，忽略即回到「点了没反应」的静默丢弃
 * （真机反馈的直接成因，正是第二批要修的病）。`onError` 自身分不出「有没有人在 await」，
 * 故由发起方**显式认领**：Home 连接按钮把 `startProxy()` 包进 `withProxyStartClaim`，
 * 认领期内事件腿让位给 await 腿；无人认领（托盘/自动连接）时事件腿照常报。
 *
 * # 为什么带宽限期而不是纯同步标志
 *
 * Tauri 的事件投递与命令 reject 走**不同**的 IPC 通道，到达顺序不保证：事件可能在 `await` 落定
 * 之前到（此时 depth>0 覆盖），也可能在之后到（此时只有宽限期覆盖）。纯同步标志（仅 depth）在后者
 * 下会漏 ⇒ 退回双报。故落定后再压 `CLAIM_GRACE_MS` 的尾巴，覆盖两种顺序。
 *
 * 反向代价：宽限期内**其它入口**（托盘）恰好也失败会被一并吞掉。可接受 —— 窗口 2s、且用户此刻正盯着
 * 连接按钮（await 腿会给它自己的提示），远轻于「每次失败弹两三遍」。宽限期不宜再放大，放大即向
 * 「静默丢弃」倾斜。
 *
 * # 射程边界（勿扩大）
 *
 * 认领**只**用于抑制这三码的提示，不抑制 `refreshProxyStatus`（核未起是终态，两条路径都得刷，
 * 否则 UI 停在假「已连接」），也不抑制崩溃腿/出口误导腿等其它码 —— 那些码与连接按钮的 await 腿无关，
 * 抑制它们等于凭空制造静默。
 */

/** 认领落定后的宽限窗口（ms）：覆盖「事件晚于 promise reject 到达」的顺序。 */
const CLAIM_GRACE_MS = 2000;

/** 在飞的认领数（可重入：理论上不会并发，但计数比布尔更耐嵌套/重入）。 */
let inFlight = 0;
/** 最近一次认领落定后的宽限截止时间戳（ms）。 */
let claimedUntil = 0;

/**
 * 认领一次「本窗口发起的起核」。认领期 = 调用在飞期间 + 落定后 `CLAIM_GRACE_MS`。
 * 透传 `run()` 的 resolve/reject（发起方仍靠 await 腿自己 catch 并提示）。
 */
export async function withProxyStartClaim<T>(run: () => Promise<T>): Promise<T> {
  inFlight += 1;
  try {
    return await run();
  } finally {
    inFlight -= 1;
    claimedUntil = Date.now() + CLAIM_GRACE_MS;
  }
}

/** 当前是否处于认领期（事件腿据此让位给 await 腿）。 */
export function isProxyStartClaimed(): boolean {
  return inFlight > 0 || Date.now() < claimedUntil;
}

/** 仅供单测复位模块级状态（模块单例跨用例存活，不复位会串味）。生产代码不调。 */
export function resetProxyStartClaim(): void {
  inFlight = 0;
  claimedUntil = 0;
}
