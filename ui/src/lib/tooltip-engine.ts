/**
 * 统一 tooltip 引擎 —— 移植原型 `proto:3105-3196`（`initTips` / `tipShow` / `tipPlace` / `tipHide`）。
 *
 * # 为什么存在（不是「原生 title 的花哨版」）
 *
 * 原型 `proto:195-198` 是带署名的设计决定，原文写着这套引擎 *replaces … all native title=（§4 migration）*。
 * 本仓此前 113 处走原生 `title=`，相对引擎缺四条**用户可感知**的能力：
 *
 *  1. **无 skip-delay** —— 扫一排图标钮时每颗都要重新等满首次延迟。本模块 `TIP_SKIP=300`。
 *  2. **方向不可控** —— 原生 tip 跟随鼠标，折叠侧栏上会压住导航项自身。本模块认 `data-tip-side`。
 *  3. **键盘焦点从不显示** —— 原生 `title` 只在 hover 出现，键盘/读屏用户完全拿不到。这是无障碍缺陷，
 *     不只是观感。本模块在 `:focus-visible` 上显示，并挂 `aria-describedby`（见下）。
 *  4. **不跟深色主题** —— 原生 tip 是 OS 外观，mac/Windows 迥异。本模块用 `#tip` 的 Conduit token 皮肤。
 *
 * # 读屏不倒退：`aria-describedby`
 *
 * 原生 `title` 是会被读屏**播报**的。只把它换成一个自绘浮层 = 视觉用户拿到了、读屏用户丢了 —— 那是
 * 净倒退。故显示期间给触发元素挂 `aria-describedby="tip"`（`#tip` 自带 `role="tooltip"`），隐藏时还原
 * 原值。**这一条是本次迁移不倒退的前提，不是可选装饰。**
 *
 * # 为什么是命令式全局委托，不是 React 组件
 *
 * 触发器有 84 处、分布在所有屏，属性驱动（`data-tip`）让消费方只写一个属性、无需 import/ref/包裹层；
 * 换成组件要在每个消费点引入包裹节点，会动布局。这也正是原型的形态，移植成本最低。
 * React 侧只需在 `App.tsx` 挂一次（`useEffect(() => initTooltips(), [])`）。
 *
 * # 挂载宿主（两个真机踩过的坑，别改回 body 一刀切）
 *
 *  - `.win`/`.main` 用 `container-type:inline-size`，隐含 layout containment ⇒ 会给 `position:fixed`
 *    后代建包含块，坐标被容器左上角偏移（同 `HoverCard.tsx:133` / `Csel.tsx:200` 的实测坑）⇒ 挂 body。
 *  - 但弹窗是原生 `<dialog>` + `showModal()`（`Modal.tsx:110`），在 **top layer**；挂 body 的 tip 会被
 *    压在弹窗下面看不见 ⇒ 触发器在 `<dialog>` 内时挂进那个 dialog（top layer 内 fixed 仍相对视口，
 *    坐标系不变，只改叠放）。
 *
 * 样式全部复用既有 `styles/prototype.css:203-208` 的 `#tip` / `#tip.show`（本次之前是零消费方的死 CSS）。
 */
import { placeTip, tipSideOf, type OverlayBox } from './overlay-position';

/** 首次打开延迟（原型 `proto:3113` `TIP_DELAY=500`；与 `HoverCard.tsx` 的 `OPEN_DELAY` 同源）。 */
export const TIP_DELAY = 500;
/** skip-delay 窗口：刚关过 tip 或已有 tip 开着时**立即**出，不重新等（原型 `TIP_SKIP=300`，radix 同款）。 */
export const TIP_SKIP = 300;
/** 触发元素与 tip 的间距（原型 `tipPlace` 的 `OFF=6`）。 */
export const TIP_GAP = 6;
/** `#tip` 宿主元素 id —— 与 `styles/prototype.css` 的 `#tip` 选择器、`aria-describedby` 指向同一个值。 */
export const TIP_ELEMENT_ID = 'tip';
/** 触发器判据：属性驱动，任何元素挂上 `data-tip` 即可（含子元素冒泡，走 `closest`）。 */
export const TIP_TRIGGER_SELECTOR = '[data-tip]';

/**
 * 打开延迟的状态机（原型 `initTips:3175` 那一行）—— 纯函数，直测。
 *
 * 「已有 tip 开着」与「刚关过不到 `TIP_SKIP`」都算连扫同一排控件，立即出；否则等满 `TIP_DELAY`。
 */
export function tipOpenDelay(now: number, tipOpen: boolean, lastCloseAt: number): number {
  return tipOpen || now - lastCloseAt < TIP_SKIP ? 0 : TIP_DELAY;
}

/**
 * `:focus-visible` = **真**键盘焦点（鼠标按下取得的焦点不算，否则每次点按钮都弹 tip）。
 *
 * 选择器不被支持时**放行**：宁可多显示一次，也不能让键盘用户完全拿不到 —— 那正是本次要修的缺陷本身。
 */
function isKeyboardFocus(el: Element): boolean {
  try {
    return el.matches(':focus-visible');
  } catch {
    return true;
  }
}

/**
 * 装引擎，返回拆卸函数（`useEffect` 直接返回它即可）。
 *
 * 每次调用自带一套独立状态与自己的 `#tip` 元素，拆卸时全部清干净 —— StrictMode 的
 * mount→unmount→mount 双调用不会残留监听或孤儿节点。
 */
export function initTooltips(): () => void {
  let tipEl: HTMLDivElement | null = null;
  let current: HTMLElement | null = null;
  /** 触发元素原有的 `aria-describedby`（显示期被我们占用，隐藏时还原；无原值则删属性）。 */
  let describedBySaved: string | null = null;
  let openTimer: ReturnType<typeof setTimeout> | undefined;
  let lastCloseAt = 0;

  const ensure = (): HTMLDivElement => {
    if (tipEl) return tipEl;
    const el = document.createElement('div');
    el.id = TIP_ELEMENT_ID;
    el.setAttribute('role', 'tooltip');
    tipEl = el;
    return el;
  };

  const triggerOf = (node: EventTarget | null): HTMLElement | null =>
    node instanceof Element ? node.closest<HTMLElement>(TIP_TRIGGER_SELECTOR) : null;

  const show = (el: HTMLElement): void => {
    const text = el.dataset.tip;
    // 空串 / 元素已卸载都不显示：`data-tip={cond ? x : undefined}` 这种写法与原来的
    // `title={cond ? x : undefined}` 语义一致（React 在 undefined 时根本不渲染该属性）。
    if (!text || !el.isConnected) return;
    const tip = ensure();
    tip.textContent = text;
    const host: Element = el.closest('dialog') ?? document.body;
    if (tip.parentElement !== host) host.appendChild(tip);

    // 两段式定位（同 HoverCard）：先归零测真实尺寸，再算位置。这次 `getBoundingClientRect` 顺带把
    // `opacity:0` 冲刷成已计算样式，后面那句 `add('show')` 才有得过渡（否则同帧批处理掉 = 无淡入）。
    tip.classList.remove('show');
    tip.style.position = 'fixed';
    tip.style.left = '0px';
    tip.style.top = '0px';
    const trigger = el.getBoundingClientRect();
    const size = tip.getBoundingClientRect();
    const viewport: OverlayBox = {
      left: 0,
      top: 0,
      width: window.innerWidth,
      height: window.innerHeight,
    };
    const p = placeTip(
      trigger,
      { w: size.width, h: size.height },
      viewport,
      tipSideOf(el.dataset.tipSide),
      TIP_GAP,
    );
    tip.style.left = `${p.left}px`;
    tip.style.top = `${p.top}px`;

    current = el;
    describedBySaved = el.getAttribute('aria-describedby');
    el.setAttribute('aria-describedby', TIP_ELEMENT_ID);
    tip.classList.add('show');
  };

  const hide = (): void => {
    clearTimeout(openTimer);
    openTimer = undefined;
    if (current) {
      if (describedBySaved === null) current.removeAttribute('aria-describedby');
      else current.setAttribute('aria-describedby', describedBySaved);
      describedBySaved = null;
      lastCloseAt = Date.now();
      current = null;
    }
    tipEl?.classList.remove('show');
  };

  const onOver = (e: Event): void => {
    const el = triggerOf(e.target);
    if (!el || el === current) return;
    clearTimeout(openTimer);
    openTimer = setTimeout(() => show(el), tipOpenDelay(Date.now(), current !== null, lastCloseAt));
  };

  const onOut = (e: Event): void => {
    const el = triggerOf(e.target);
    if (!el) return;
    // 仍在触发器内部（在它的子元素之间移动）不算离开。`#tip` 是 `pointer-events:none`，
    // 指针永远落不到它身上，故无需像原型那样额外判 `tipEl.contains(to)`（那是富卡片才需要的）。
    const to = (e as MouseEvent).relatedTarget;
    if (to instanceof Node && el.contains(to)) return;
    clearTimeout(openTimer);
    openTimer = undefined;
    if (el === current) hide();
  };

  const onFocusIn = (e: Event): void => {
    const el = triggerOf(e.target);
    if (!el || el === current || !isKeyboardFocus(el)) return;
    // 键盘路径不等延迟：焦点是明确的意图表达，没有「扫过去」的误触问题（原型 :3186-3190 同）。
    clearTimeout(openTimer);
    openTimer = undefined;
    show(el);
  };

  const onFocusOut = (e: Event): void => {
    if (triggerOf(e.target) === current) hide();
  };

  const onKeyDown = (e: Event): void => {
    if ((e as KeyboardEvent).key === 'Escape') hide();
  };

  document.addEventListener('mouseover', onOver);
  document.addEventListener('mouseout', onOut);
  document.addEventListener('focusin', onFocusIn);
  document.addEventListener('focusout', onFocusOut);
  document.addEventListener('keydown', onKeyDown);
  // capture：内层滚动器（节点列表 / 日志 / 连接表）的滚动不冒泡到 window，不捕获就抓不到。
  window.addEventListener('scroll', hide, true);
  window.addEventListener('resize', hide);

  return () => {
    document.removeEventListener('mouseover', onOver);
    document.removeEventListener('mouseout', onOut);
    document.removeEventListener('focusin', onFocusIn);
    document.removeEventListener('focusout', onFocusOut);
    document.removeEventListener('keydown', onKeyDown);
    window.removeEventListener('scroll', hide, true);
    window.removeEventListener('resize', hide);
    hide();
    tipEl?.remove();
    tipEl = null;
  };
}
