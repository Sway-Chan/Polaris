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
 */
import { useEffect, useState, type UIEvent } from 'react';

/**
 * 每批渲染数。`.aad-ico-grid` 是 `max-height:150px` 的滚动容器（原型 §AF2），可视区约 3 行 ≈ 24 格，
 * 一批 60 给足两屏余量、滚动不见白。规则弹窗的 `.rv-pick`（132px 的 chip 流）同量级，共用此值。
 */
export const SCROLL_BATCH_PAGE = 60;
/** 滚到距底部多少像素时追加下一批。 */
export const SCROLL_BATCH_AHEAD_PX = 240;

export interface ScrollBatch {
  /** 当前该渲染多少条（调用方自己 `slice(0, count)`）。 */
  count: number;
  /** 挂到滚动容器的 `onScroll`。 */
  onScroll: (e: UIEvent<HTMLElement>) => void;
}

/**
 * @param total     过滤后的**总条数**（到顶即止，不越过实际条数）。
 * @param resetKey  结果集的身份（通常是搜索词）。**一变即回首批** —— 否则搜完窄结果再清空搜索，
 *                  会残留一个大计数，等于分批白做。
 */
export function useScrollBatch(total: number, resetKey: unknown): ScrollBatch {
  const [count, setCount] = useState(SCROLL_BATCH_PAGE);
  useEffect(() => setCount(SCROLL_BATCH_PAGE), [resetKey]);
  return {
    count,
    onScroll: (e) => {
      const el = e.currentTarget;
      if (el.scrollHeight - el.scrollTop - el.clientHeight > SCROLL_BATCH_AHEAD_PX) return;
      setCount((c) => (c >= total ? c : c + SCROLL_BATCH_PAGE));
    },
  };
}
