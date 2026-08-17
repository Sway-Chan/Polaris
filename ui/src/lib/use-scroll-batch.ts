/**
 * `useScrollBatch` —— 滚动到底就追加一批的分批渲染（**唯一实现**）。
 *
 * # 为什么是 scroll 事件，不是 `loading="lazy"` / IntersectionObserver
 *
 * 真机（macOS/WKWebView）实测：`AppAddDialog` 的画廊图标全部加载失败，而 `polaris-icon` handler
 * **一条日志都没有** —— 请求根本没到 scheme handler，却触发了 img 的 `onerror`。同一个 scheme、
 * 同样的 `iconProxySrc` 产物，URL 面板的预览 img（无 lazy）是好的。两者唯一差别就是那个
 * `loading="lazy"`。IntersectionObserver 与 lazy loading 是同族机制（都靠 WebKit 的相交观测），
 * 在同一个 top-layer `<dialog>` + 小滚动容器组合里有踩同一个坑的风险，**不拿它做替代**。
 * scroll 事件是最老最稳的路径。（这段判据原逐字写在 `AppAddDialog.tsx` 的 `GALLERY_PAGE` 头注上，
 * 随实现一起搬到这里 —— 判据要跟着实现走，否则第二个消费方看不到它。`use-scroll-batch.test.ts`
 * 有门钉住「消费方不得出现 lazy / IntersectionObserver」。）
 *
 * # 分批同时约束并发出站
 *
 * 画廊 3100 个图标一次性 eager 加载 = 3100 次经核出站，与「设定即缓存」的隐私第一性相悖。
 * 分批把它压到一屏的量级。
 *
 * # 为什么抽成 hook
 *
 * 第二个消费方来了（规则弹窗的候选勾选区：ruleSet 外置 2000+、进程本机实测 356）。
 * 原实现是 `AppAddDialog` 里内联的 state + effect + handler，复制一份就会漂 —— 而漂出来的症状是
 * 「一个面板滚得动、另一个滚到底不再加载」。抽 hook 是为了消掉第二份，不是为了更优雅。
 *
 * # 为什么复位写在**渲染期**，不是 `useEffect`
 *
 * `resetKey` 变的那一帧必须按新结果集的首批画。写成 `useEffect(() => setCount(PAGE), [resetKey])`
 * 时复位跑在**提交之后**：切订阅组、搜索框每敲一个字，都会先按上一档那个大 count 把整帧画完
 * （最坏 600 张卡）再回落到 60 —— 正好是分批要消掉的那一帧，方向反了。改用 React 官方的
 * 「渲染期调整 state」形态（`prevResetKey` + 渲染中 `setState`）：React 丢弃这次渲染的输出、
 * 立刻拿新 state 重跑本组件，**提交前**就已经是 60，那一帧根本不存在。三个消费方同时受益。
 *
 * # 为什么判据是导出的纯函数，不留在 `onScroll` 闭包里
 *
 * 本仓 vitest 是 node 环境、无 jsdom，hook 的行为在那一层不可观测；而 `renderToStaticMarkup`
 * 的首帧对「会推进」与「永远只有首批」这两种状态**逐字一致**（SSR 不跑 effect）。判据留在闭包里，
 * 把推进条件写成恒假也能全绿。提成 [`shouldAdvance`] / [`advanceBatch`] 才能被正反两向直测，
 * 且 hook 只是它们的一层 `useState` 接线（`use-scroll-batch.test.ts` 用极简宿主驱动真实 hook 源码）。
 */
import { useState } from 'react';

/**
 * 每批渲染数。`.aad-ico-grid` 是 `max-height:150px` 的滚动容器（原型 §AF2），可视区约 3 行 ≈ 24 格，
 * 一批 60 给足两屏余量、滚动不见白。规则弹窗的 `.rv-pick`（132px 的 chip 流）同量级，共用此值。
 */
export const SCROLL_BATCH_PAGE = 60;
/** 滚到距底部多少像素时追加下一批。 */
export const SCROLL_BATCH_AHEAD_PX = 240;

/** 滚动容器的三个量（只要这三个，故写成结构型 —— 便于直测，也便于消费方喂祖先元素）。 */
export interface ScrollMetrics {
  scrollHeight: number;
  scrollTop: number;
  clientHeight: number;
}

/**
 * 「该不该再追加一批」的**唯一判据**：距底不足 [`SCROLL_BATCH_AHEAD_PX`] 就追加。
 *
 * 注意这条同时兼两个身份：滚动时是「快到底了，预取下一批」；每次提交后由消费方主动调用时是
 * 「内容还没撑出滚动条（距底 ≤ 余量），再来一批」——后者正是「初批必须覆盖视口」那条保证的实现。
 *
 * # `clientHeight <= 0` 必须先短路（**量不到就不判**）
 *
 * 全零输入（`scrollHeight = scrollTop = clientHeight = 0`）下 `0 <= 240` 恒真 ⇒「不该推进」这条
 * 分支永不成立 ⇒ [`advanceBatch`] 的 bail-out 只剩 `c >= total`。而节点屏的补批循环跑在 layout 档、
 * 同步、无上限：`total = 3000` 要 50 轮，正好撞上 React 的 `NESTED_UPDATE_LIMIT = 50` ⇒
 * `Maximum update depth exceeded`（白屏 / 落进错误边界），而不是「就地 bail-out」。
 *
 * 容器量到 0 的真实入口是 `ResizeObserver`：被观测元素停止渲染时会投递一次 0×0
 * （窗口最小化 / 被完全遮挡时 WebView2 与 WKWebView 保不保留 layout 值，靠 code review 判不准）。
 * 这里**不**走 fail-open：为一个最小化的窗口渲染三千张卡是把一个不可见的问题换成一个可见的卡顿。
 * 量不到就不判，盒子恢复成真实尺寸时 RO 会再投递一次，这条路径自愈。
 */
export function shouldAdvance(m: ScrollMetrics): boolean {
  if (m.clientHeight <= 0) return false;
  return m.scrollHeight - m.scrollTop - m.clientHeight <= SCROLL_BATCH_AHEAD_PX;
}

/**
 * 分批计数的状态机：不该推进、或已取完，都返回**原值**（React 就地 bail-out ⇒ 补批循环收敛）。
 * 越过 `total` 不裁剪：消费方一律 `slice(0, count)`，裁不裁结果一样，而不裁能让「已取完」这件事
 * 只由 `c >= total` 一条判据表达。
 *
 * 「不自激」有**两条**前提，缺一条循环就只靠 `c >= total` 兜底、轮数 = `total / PAGE`：
 * ① 容器有真实高度（否则 [`shouldAdvance`] 恒真，见其头注的全零输入）；② `total` 有限。
 */
export function advanceBatch(count: number, total: number, m: ScrollMetrics): number {
  if (!shouldAdvance(m)) return count;
  return count >= total ? count : count + SCROLL_BATCH_PAGE;
}

export interface ScrollBatch {
  /** 当前该渲染多少条（调用方自己 `slice(0, count)`）。 */
  count: number;
  /**
   * 挂到滚动容器的 `onScroll`。
   *
   * 形参写成**结构型**（只要一个 `currentTarget`）而不是 `React.UIEvent`：本函数从头到尾只读
   * `e.currentTarget`，而第三个消费方（节点网格）的滚动容器是 `AppShell` 的 `.main-scroll`
   * ——**祖先**元素，不在本组件的 JSX 里，只能走原生 `addEventListener` / 直接调用。若签名钉死
   * React 合成事件，那边就得造一个 `as unknown as UIEvent` 的假事件来喂它，纯属为类型而说谎。
   * 逆变使这个更宽的形参依然能直接写成 `onScroll={onScroll}`（前两个消费方一字未改）。
   */
  onScroll: (e: { currentTarget: ScrollMetrics }) => void;
  /**
   * 一次取到底（`count = total`）。**只给失效路径兜底**，不是给「我不想分批」用的。
   *
   * 消费方拿不到自己的滚动容器时（第三个消费方要 `closest()` 找祖先，可能落空）就再也收不到任何
   * 事件。那时唯一安全的方向是「不分批」而不是「只剩首批」：多渲染的代价是一次卡顿，画不出来的
   * 代价是用户永远点不到剩下的节点，且**没有滚动条就没有「还有更多」的暗示**，他不会知道少了。
   */
  renderAll: () => void;
}

/**
 * `resetKey` 的取值域**必须是原始值**，签名故意收窄到这里（原先是 `unknown`）。
 *
 * 复位改到渲染期之后，判等走 `Object.is`：传对象/数组字面量 ⇒ 每次渲染都是新引用 ⇒ 每次渲染都
 * 复位并 `setState` ⇒ 渲染期自激，React 直接抛 `Maximum update depth exceeded`（白屏，不是退化）。
 * `unknown` 不但拦不住，还等于在邀请人传 `{ tab, search }`。三个消费方今天全是字符串。
 * 想用多个维度就拼成字符串（`NodesScreen` 用 NUL 作分隔符，免得 `a|b` 与 `a` + `|b` 撞）。
 */
export type ScrollBatchResetKey = string | number | boolean | null | undefined;

/**
 * @param total     过滤后的**总条数**（到顶即止，不越过实际条数）。
 * @param resetKey  结果集的身份（通常是搜索词）。**一变即回首批** —— 否则搜完窄结果再清空搜索，
 *                  会残留一个大计数，等于分批白做。取值域见 [`ScrollBatchResetKey`]。
 */
export function useScrollBatch(total: number, resetKey: ScrollBatchResetKey): ScrollBatch {
  const [count, setCount] = useState(SCROLL_BATCH_PAGE);
  // 渲染期复位（判据见头注「为什么复位写在渲染期」）：React 会丢弃这趟渲染、立刻重跑本组件，
  // 故 `resetKey` 变的那一次**提交出去的就是首批**，不存在按旧计数画的中间帧。
  const [prevResetKey, setPrevResetKey] = useState(resetKey);
  if (!Object.is(prevResetKey, resetKey)) {
    setPrevResetKey(resetKey);
    setCount(SCROLL_BATCH_PAGE);
  }
  return {
    count,
    onScroll: (e) => setCount((c) => advanceBatch(c, total, e.currentTarget)),
    renderAll: () => setCount((c) => (c >= total ? c : total)),
  };
}
