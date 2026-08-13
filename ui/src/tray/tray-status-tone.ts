/**
 * 浮层状态卡的四态折算（A2 的浮层半）。
 *
 * # 为什么单独抽一个模块
 *
 * 托盘一共有**两个**状态呈现面：原生托盘图标（Rust `tray.rs::resolve_tray_state`）与浮层状态卡上的
 * 那颗点。此前浮层只有 `running ? 'ok' : 'idle'` 两态 —— 与图标的老二态一起，把「起核中」和「崩溃」
 * 都显示成"未连接"。两面必须按**同一条优先级**折算，否则会出现「图标红叉、浮层说未连接」这种
 * 自相矛盾的呈现，而这种矛盾没有任何测试能在组件内联三元表达式里抓到。
 *
 * 故折算抽成纯函数，与 Rust 侧那份**逐分支同构**（优先级 Connected > Connecting > Error > Idle，
 * 每一级的理由见 `src-tauri/src/tray.rs::resolve_tray_state`），两边各有单测钉同一组用例。
 */

/**
 * 浮层状态。前四态与 Rust `TrayState` 对齐（小写形态便于直接当 CSS 修饰类的键）；
 * `degraded` 是**浮层独有的第五态**，理由见 [`trayStatusTone`] 的 `degraded` 入参。
 */
export type TrayStatusTone = 'connected' | 'degraded' | 'connecting' | 'error' | 'idle';

/**
 * 由 proxy 状态快照的三个位（+ 降级位）折出浮层状态。
 *
 * - `running` 压过一切：`set_nonfatal_error` 会在**活核**上留 errorCode（如 `SYSTEM_PROXY_FAILED`），
 *   那不是"没连上"。
 * - `starting` 压过 `errored`：新一轮起核已在飞，上一轮的失败不该盖住"正在重试"这个更新的事实。
 * - `errored` 压过 idle：这正是要补的缺口 —— 崩溃此前与用户主动断开完全同形。
 *
 * # `degraded`：running 分支内部的再分叉（2026-07-28 复审补）
 *
 * 「核在跑」与「流量经核」在 systemProxy 接管下是**两件事**（详见
 * `components/screens/home/connection-state.ts` 模块头）。主窗已按这条分叉展示（状态栏琥珀点 +
 * 首页降级横幅），浮层此前只有 `running → connected` ⇒ **OS 代理被手改时主窗亮琥珀「未生效」、
 * 托盘同一时刻显绿点「已连接」**——同一台机器上两个窗自相矛盾。
 *
 * 判定本身**不在本函数里重写**：调用方喂进 `deriveTakeoverConnState(...) === 'proxy-degraded'`
 * 的结果，本函数只负责它在四态优先级里的位置（= running 之内，故仍压过 starting/errored）。
 * 缺省 `false` ⇒ 不传即与改动前逐字节相同（Rust 侧图标那条腿没有这个位，见下方注释）。
 */
export function trayStatusTone(input: {
  running: boolean;
  starting: boolean;
  errored: boolean;
  /** 核在跑但流量没经核（systemProxy 未生效）。真值由 `deriveTakeoverConnState` 给。 */
  degraded?: boolean;
}): TrayStatusTone {
  if (input.running) return input.degraded ? 'degraded' : 'connected';
  if (input.starting) return 'connecting';
  if (input.errored) return 'error';
  return 'idle';
}

/**
 * 状态 → `.dot` 修饰类。
 *
 * 全部复用 `ui/src/styles/components.css` 已有的 `.dot.{ok,warn,err,idle}`（:70-75，含 warn 的
 * 外发光与 err 的 `--err` 红）——**不新增 CSS**：这四个修饰类是全仓状态点的既有词汇表，
 * 托盘另造一套只会让同一语义在不同屏上长得不一样。
 *
 * `degraded` 与 `connecting` **共用 warn**，这是刻意的：主窗 `StatusBar` 对 `proxy-degraded`
 * 用的正是 `warn`（琥珀 = 需要注意但不是错误），跨窗同语义必须同色阶。二者靠**文案**区分
 * （"连接中…" vs "系统代理未生效"），不靠色阶——这也是主窗的做法。
 */
export const TRAY_TONE_DOT_CLASS: Record<TrayStatusTone, string> = {
  connected: 'ok',
  degraded: 'warn',
  connecting: 'warn',
  error: 'err',
  idle: 'idle',
};
