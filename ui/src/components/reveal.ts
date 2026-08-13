/**
 * reveal —— 「展开即露出」：折叠段展开后，把新露出的内容滚进可视区。
 *
 * # 解决的是什么
 *
 * 弹窗与设置页都只有**一个**滚动容器（`.dlg-body` / 设置页主滚动区），折叠段展开时新内容长在
 * 视口下沿之外 —— 没有滚动跟随，`<details>` 又是瞬时跳变，眼睛没有任何可追的变化，
 * 于是「点了没反应」。用户实测反馈：需要手动下拉，容易被忽视。
 *
 * # 为什么是滚动而不是展开动画
 *
 * 平滑滚动**本身就是**眼睛能追的运动，它同时解决「看不见」和「没反应」两件事。
 * 而 `::details-content` 高度过渡（`interpolate-size: allow-keywords`）目前只有 Chromium 有，
 * mac/Linux 的 WKWebView / WebKitGTK 降级为瞬时；更要命的是它与本模块**时序冲突**：
 * `toggle` 事件触发时高度正在动画，这里测到的是动画起点 ⇒ 滚动量算少。
 * 故本仓选滚动、不做高度动画；将来若要加，必须改成等 `transitionend` 再滚。
 *
 * # 三条判断都是刻意的
 *
 * ① **已经全看得见就一动不动** —— 无谓的滚动比不滚更让人失去位置感。
 * ② **滚动量以 summary 顶部封顶** —— 再多滚一像素就把标题顶出视口，用户会不知道自己在哪一块里。
 *    这条让「展开一个很长的段」表现为「标题贴到顶、下面尽量露出」，而不是把标题甩掉。
 * ③ **尊重 `prefers-reduced-motion`** —— 仓里 CSS 已有该媒体查询的先例（prototype.css:625），
 *    JS 侧不能自行其是。
 *
 * # 为什么用 getBoundingClientRect 而不是 offsetTop
 *
 * `offsetTop` 相对 `offsetParent`，只有当滚动容器恰好是 `offsetParent`（即它 `position` 非 static）
 * 时才等价。`.dlg-body` 现在没有 `position:relative` ⇒ 用 offsetTop 会**静默**算到别的祖先上、
 * 滚动量错得毫无征兆。rect 是视口坐标系，两个 rect 相减与祖先布局无关。
 */

import { useCallback, useEffect, useRef } from 'react';

/** 滚动量计算：从纯几何量算，与 DOM/浏览器无关，故可脱离 jsdom 直接单测。 */
export function computeRevealDelta(
  el: { top: number; bottom: number },
  container: { top: number; bottom: number },
): number {
  const overflow = el.bottom - container.bottom; // 展开后超出容器底部多少
  if (overflow <= 0) return 0; // 判断①
  const maxUp = el.top - container.top; // 判断②：再多滚就把标题顶出去
  return Math.max(0, Math.min(overflow, maxUp));
}

/**
 * 最近的**可滚动**祖先。
 *
 * 判据是「overflow 允许滚动 **且** 当前真的有可滚动空间」：只看 overflow 会选中
 * `.fld-fold`（它是 `overflow:hidden`，不滚）之类的盒子；只看 scrollHeight 会漏掉尚未溢出的容器。
 * 两条都要，且 `hidden` 不算 —— 它不能滚，对它 scrollBy 是静默无效。
 */
function scrollableAncestor(el: HTMLElement): HTMLElement | null {
  for (let p = el.parentElement; p; p = p.parentElement) {
    const oy = getComputedStyle(p).overflowY;
    if ((oy === 'auto' || oy === 'scroll') && p.scrollHeight > p.clientHeight) return p;
  }
  return null;
}

function prefersReducedMotion(): boolean {
  return typeof window !== 'undefined' && typeof window.matchMedia === 'function'
    ? window.matchMedia('(prefers-reduced-motion: reduce)').matches
    : false;
}

/** 把 `el` 新露出的部分滚进可视区。容器不可滚 / 已看得见 ⇒ 什么都不做。 */
export function revealElement(el: HTMLElement): void {
  const sc = scrollableAncestor(el);
  if (!sc) return;
  const delta = computeRevealDelta(el.getBoundingClientRect(), sc.getBoundingClientRect());
  if (delta <= 0) return;
  // jsdom 无 scrollBy —— 单测里渲染真实组件时不该因此炸。几何计算已由 computeRevealDelta 单独覆盖。
  if (typeof sc.scrollBy !== 'function') return;
  sc.scrollBy({ top: delta, behavior: prefersReducedMotion() ? 'auto' : 'smooth' });
}

/**
 * `<details onToggle>` 直挂的处理器：**只在展开时**滚，折叠时不动
 * （折叠只会让内容变短，此时滚动等于凭空把用户挪走）。
 *
 * 任何 `<details>` 都能挂，不要求它是 `.fld-fold` —— 本仓还有 `rule-test-det` / `tun-details` /
 * `us-notes` 三种自带样式的折叠，它们的 markup 不该为了拿到这个行为被迫改形。
 */
export function revealOnToggle(e: { currentTarget: HTMLDetailsElement }): void {
  if (e.currentTarget.open) revealElement(e.currentTarget);
}

/**
 * 扁平兄弟结构的分组展开（`.ns-grp` / `.csel-grp` / `.tray-group-h` 四处菜单）。
 *
 * 这些菜单**都**带 `max-height + overflow-y:auto`（`.node-menu` 430 / `.mini-menu` 360 /
 * `.csel-menu` 300 / `.tray-menu` 600），组头在底部时展开，新出现的项目整段落在菜单视区之外
 * —— 与折叠段同一形状。
 *
 * 但它们的 DOM 是**组头与项目并列的兄弟**，没有分组容器，所以「露出组头」等于没露出
 * （组头正是你刚点的那个，本来就看得见）。故把「组头 → 下一个组头之前的最后一个兄弟」
 * 当作一个整体：top 取组头（封顶判断因此仍然保住组头不被顶出），bottom 取该段末尾。
 *
 * 末尾判据只用「下一个带**同一个组头类名**的兄弟」这一条结构标记 —— 分隔线/小标题各菜单叫法不一，
 * 硬编会各漏各的。最后一组会因此把段尾算到菜单末尾（overshoot），但那是**优雅降级**：
 * delta 被组头到顶的距离封住 ⇒ 表现为「组头贴顶、下面尽量露出」，对最后一组恰好是想要的。
 */
export function revealSiblingGroup(header: HTMLElement): void {
  const marker = header.classList[0];
  if (!marker) return;
  let last: HTMLElement = header;
  for (let n = header.nextElementSibling; n; n = n.nextElementSibling) {
    if (n.classList.contains(marker)) break;
    last = n as HTMLElement;
  }
  const sc = scrollableAncestor(header);
  if (!sc) return;
  const delta = computeRevealDelta(
    { top: header.getBoundingClientRect().top, bottom: last.getBoundingClientRect().bottom },
    sc.getBoundingClientRect(),
  );
  if (delta <= 0 || typeof sc.scrollBy !== 'function') return;
  sc.scrollBy({ top: delta, behavior: prefersReducedMotion() ? 'auto' : 'smooth' });
}

/**
 * 「本次提交之后再露出」。
 *
 * `<details>` 有 `toggle` 事件（DOM 已经是展开态才触发），分组展开没有：点击时项目还没渲染，
 * 当场量到的是旧 DOM。用 `useEffect`（每次提交后都跑）而不是 `requestAnimationFrame` ——
 * 后者与 React 的提交时机只是**经验上**先后成立，前者是契约。
 *
 * 存的是 thunk 不是元素：四个调用点各自决定露出哪一段，这里只负责「什么时候」。
 */
export function useRevealAfterCommit(): (task: (() => void) | null) => void {
  const pending = useRef<(() => void) | null>(null);
  useEffect(() => {
    const task = pending.current;
    if (!task) return;
    pending.current = null;
    task();
  });
  return useCallback((task: (() => void) | null) => {
    pending.current = task;
  }, []);
}
