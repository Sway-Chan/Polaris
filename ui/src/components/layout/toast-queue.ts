/**
 * Toast 队列语义（纯逻辑，从 `Toaster.tsx` 抽出以便单测）。
 *
 * # 为什么要抽出来
 *
 * 本仓 vitest 是 `environment:'node'` 无 jsdom（`vite.config.ts:76`，有意为之）⇒ 渲染层测不了。
 * 而队列语义恰恰是**最容易退化且退化后最刺眼**的一层：`upsert` 一旦退回「无条件 append」，
 * 一轮 50 个节点的测速就会刷出 50 条 toast 把屏幕糊满。故把「同 key 更新 / 溢出挤谁 / 谁自动消失」
 * 三条判定搬到本模块，`Toaster.tsx` 只剩「调它 + 渲染 + 起定时器」。
 *
 * # 三条语义
 *
 * 1. **同 key 原地更新**（`upsertToast`）：带 `dedupeKey` 的条目命中已有同 key 条目时**替换而非追加**，
 *    且**保持栈内位置不变**（进度不该因为自己刷新而跳到别人前面）。
 * 2. **溢出优先挤非 sticky**（`upsertToast` 内）：原型语义是「栈上限 2，新条挤掉最旧的」
 *    （`prototype.css` 侧的 `while(c.children.length>=2) c.firstChild.remove()`）。此处只加一条限定：
 *    有 sticky（持续状态）在场时先挤非 sticky 的。**既有行为逐字不变** —— 既有 toast 全是非 sticky，
 *    规则退化回「挤掉最旧的」。
 * 3. **sticky 不自动消失**（`autoDismissMs`）：进度是「持续状态」不是「一次性通知」，它必须活到事实失效为止。
 *
 * # 「持续状态该留 / 一次性通知该散」不是两套标准
 *
 * 本仓 2026-07-30 刚**撤掉**过一个 toast（IPv6 开关，commit `6e587a6`），理由写在
 * `screens/settings/SettingsTun.tsx:164`：「toast 几秒即散，事后回看开关读不到任何风险信息」。
 * 本模块新增的 sticky **不与那条冲突，两者是同一条规则的两个取值**：
 *
 * > **反馈的存活时长必须等于它所陈述事实的有效期。**
 *
 *  · IPv6 那句陈述的事实（「走 IPv6 的连接会一直等到超时」）在开关开着的**整个期间**都成立
 *    ⇒ 有效期 = 状态存续期 ⇒ 该由常驻 desc 承担，2.2s 的 toast **太短**；
 *  · 进度这句陈述的事实（「已测 12/50」）只在这一轮测速的十几秒内成立、测完即失效、事后回看毫无意义
 *    ⇒ 有效期 = 一次操作的执行期 ⇒ 该走瞬态通道，但 2.2s 的默认 toast **同样太短**（一轮测速远不止 2.2s）
 *    ⇒ 故 sticky，并在事件结束时由结论 toast 顶掉。
 *
 * 两条诊断同向（都是「默认 toast 太短」），只是修法不同：一个改常驻，一个改 sticky。
 * 谁要拿 IPv6 那条注释来「统一」掉本模块，请先回答「这条反馈陈述的事实有效期是多长」。
 */

/** 原型三档配色：ok/err 有专属样式，`''` 走基础样式（不自造第四档）。 */
export type ToastKind = 'ok' | 'err' | '';

export interface ToastEntry {
  /** 单调自增流水号。**每次 upsert 都换新**（见 `upsertToast`）——旧号即失效，在飞定时器随之变空转。 */
  id: number;
  /**
   * 外部去重键（`toast.info(msg, { key })` 传入；语义同 sonner 的 `{ id }` / 上游
   * `speed-test-toast.ts` 的 `TOAST_ID`）。缺席 = 普通一次性 toast，各自独立成条。
   */
  dedupeKey?: string;
  msg: string;
  /** 第二段（错误详情/原因），渲染成 `.toast-desc`。 */
  desc?: string;
  kind: ToastKind;
  /** 持续状态：不自动消失。见文件头第三节。**有动作时本位被压过**，见 `autoDismissMs`。 */
  sticky: boolean;
  /** 行内动作组（文案已翻译）。 */
  actions?: Array<{ label: string; onClick: () => void }>;
  /** 显式关闭入口（文案已翻译，供 aria-label 使用）。 */
  dismiss?: { label: string };
  /** 进场后置 true → 加 `.show`（原型的 requestAnimationFrame 两帧语义）。 */
  shown: boolean;
  /** 离场中 → 去 `.show`，`LEAVE_MS` 后移除。 */
  leaving: boolean;
}

/** 原型：栈里最多 2 条，新条挤掉最旧的。 */
export const MAX_STACK = 2;
export const VISIBLE_MS = 2200;
export const LEAVE_MS = 200;

/**
 * 带动作的 toast 的停留时长。**必须有限**（见 `autoDismissMs` 的判据），15s 的取法：
 *
 *  · 下界由「按钮要点得到」定：2.2s 的默认停留下，用户的视线还没落到右下角 toast 就没了 ⇒ 按钮形同虚设。
 *    唯一的真实消费场景（测速被中断 → 「继续」）恰恰发生在用户刚点完断开/切节点的那一刻，注意力在别处。
 *  · 上界由「不许赖着不走」定：屏幕右下角只有 2 条的栈位（`MAX_STACK`），一条常驻的 toast 会挤掉
 *    后续真正需要看的通知。
 *
 * 15s 落在两者之间，且**小于**静默兜底超时（`speedtest-progress-toast.ts` 的 20s）——两者不会互相追尾。
 */
export const ACTION_VISIBLE_MS = 15_000;

/**
 * 该条 toast 多久后自动淡出；`null` = 不自动消失（sticky）。
 *
 * **策略的唯一所在地**：`Toaster` 拿 `null` 就不起定时器。把判定留在这里而不是写成组件里的
 * `if (!sticky)`，是为了让「sticky 失效」这个变异能被纯逻辑单测直接抓到（组件在 node 环境不可测）。
 *
 * # 🔴 有动作 ⇒ 一定有出路（actions **压过** sticky）
 *
 * 一条「带按钮、又永不消失」的 toast 会永久占住右下角栈位。测速中断现在虽有显式关闭入口，
 * `ToastOptions` 的其它动作通知仍不保证都带 dismiss，故 actions 依旧必须压过 sticky，统一返回有限值。
 *
 * 于是带动作的 toast 恒有三条出路：① 点按钮（调用方随后会用同 key 顶掉它）；② `ACTION_VISIBLE_MS`
 * 后自散；③ 下一轮同 key 的 toast 顶掉；④ 调用方提供的关闭入口。**门**：
 * `autoDismissMs({sticky:true, actions:[...]})` 必须返回有限值。
 */
export function autoDismissMs(entry: Pick<ToastEntry, 'sticky' | 'actions'>): number | null {
  if (entry.actions && entry.actions.length > 0) return ACTION_VISIBLE_MS;
  return entry.sticky ? null : VISIBLE_MS;
}

/**
 * 入栈：带 `dedupeKey` 且已存在同 key ⇒ **原地替换**（位置不动，`shown` 沿用旧值以免重播进场动画）；
 * 否则追加，并按 [`MAX_STACK`] 挤旧。
 *
 * `shown` 沿用旧值这一条是必须的：新条目 `shown:false` 会让已经在屏上的进度 toast 每收一个事件
 * 就闪一次（掉 `.show` → 下一帧再加回）。`id` 则相反——**必须换新**，因为旧 id 上可能挂着一个
 * 在飞的淡出定时器（上一条是非 sticky 时），换号即让它按 id 匹配不到、自动空转。
 */
export function upsertToast(list: ToastEntry[], next: ToastEntry): ToastEntry[] {
  if (next.dedupeKey !== undefined) {
    const at = list.findIndex((it) => it.dedupeKey === next.dedupeKey);
    if (at >= 0) {
      const merged = { ...next, shown: list[at].shown };
      return [...list.slice(0, at), merged, ...list.slice(at + 1)];
    }
  }
  let kept = list;
  while (kept.length >= MAX_STACK) kept = evictOne(kept);
  return [...kept, next];
}

/**
 * 溢出时挤掉一条：**优先最旧的非 sticky**，全是 sticky 才挤最旧的。
 *
 * 数组是「旧 → 新」序，故 `findIndex` 命中的就是最旧的非 sticky。没有 sticky 在场时
 * `findIndex` 恒返 0 ⇒ 与原型的「挤掉最旧」逐字同义，既有 toast 的行为零变化。
 * 反之若不加这条限定，测速期间来两条普通 toast 就会把进度条挤没（用户看到进度凭空消失）。
 */
function evictOne(list: ToastEntry[]): ToastEntry[] {
  const victim = list.findIndex((it) => !it.sticky);
  const at = victim >= 0 ? victim : 0;
  return [...list.slice(0, at), ...list.slice(at + 1)];
}

/**
 * React 列表 key：有 `dedupeKey` 就用它，否则用流水号。
 *
 * **不能直接用 `id`**：`upsertToast` 每次更新都换 id，用 id 作 React key 会让同一条进度 toast
 * 每次刷新都被卸载重挂 —— DOM 节点重建 = 进场动画重播 = 每 200ms 闪一次。
 * 两个命名空间前缀隔开，避免 `dedupeKey === '3'` 撞上 `id === 3`。
 */
export function toastListKey(entry: Pick<ToastEntry, 'id' | 'dedupeKey'>): string {
  return entry.dedupeKey !== undefined ? `k:${entry.dedupeKey}` : `i:${entry.id}`;
}
