/**
 * 锚定下拉菜单的**定位 + 首项聚焦** —— 原型 `miniMenu`（`proto:3245-3253`）的两条腿。
 *
 * # 为什么是一个 hook 而不是每处各写一遍
 *
 * 本仓有 5 处 `.mini-menu`（节点页 3 处、订阅信息条 1 处、应用分流策略 pill 1 处）。此前：
 *  - 前 4 处是纯 CSS 锚定（`top:calc(100% + 6px)` + `left/right:0`），**零测量零 clamp**
 *    ⇒ 窄窗 / 触发钮靠边时菜单溢出窗口（`proto:3249-3251` 的四向夹紧没移植）；
 *  - 第 5 处自己写了一份测量 + `Math.min(...)`，**缺 `Math.max(8, …)` 那一半**
 *    ⇒ 容器比菜单还窄时给出负坐标。这正是 `overlay-position.ts:56` 头注说的「抄第二遍必然分叉」。
 *
 * 边界数学收在 `placeAnchoredMenu`（纯函数，可在 node 环境直测）；本 hook 只做「量 → 落位 → 聚焦」，
 * 与 `lib/confirm-twice.ts` 同一分层理由（vitest 是 `environment:'node'`，hook 本身渲染不了）。
 *
 * # 为什么保留 `position:absolute`
 *
 * `.win` 是 `container: win / inline-size`（`prototype.css:108`），隐含 layout containment ⇒ 它会给
 * `position:fixed` 后代建包含块，「fixed 相对视口」在这里不成立。菜单继续相对最近的定位祖先绝对定位，
 * 只把坐标改成**实测 + 在 `.win` 内 clamp** 的结果（算完再减去定位祖先的偏移换回本地坐标系）。
 *
 * # 关闭腿不在这里
 *
 * 点外部 / ESC 关闭各消费点已有各自的 effect。把它们收口是另一条判定（浮层关闭腿无单点收口），
 * 需要先裁定事件类型（`click` vs `mousedown`）与跨浮层的 ESC 优先级链，**不在本 hook 射程内**。
 */
import { useLayoutEffect, useRef, useState } from 'react';
import { placeAnchoredMenu, type MenuAlign, type OverlayBox } from './overlay-position';

/** 菜单与触发钮的竖直间距 —— 沿用本仓既有的 6px（原型是 4px，此处不为对拍去动既定观感）。 */
export const ANCHORED_MENU_GAP = 6;

/** 菜单项选择器（原型 `:3253` 的 `m.querySelector('.mi')`）。 */
const MENU_ITEM_SELECTOR = '.mi';

export interface AnchoredMenu<A extends HTMLElement, M extends HTMLElement> {
  /** 挂在触发钮（或包住它的定位容器）上。 */
  anchorRef: React.RefObject<A>;
  /** 挂在 `.mini-menu` 根上。 */
  menuRef: React.RefObject<M>;
  /** 直接铺进菜单的 `style`。未测量前是 `visibility:hidden`，避免落位前闪一帧在左上角。 */
  style: React.CSSProperties;
}

export function useAnchoredMenu<A extends HTMLElement, M extends HTMLElement>(
  open: boolean,
  align: MenuAlign,
): AnchoredMenu<A, M> {
  const anchorRef = useRef<A>(null);
  const menuRef = useRef<M>(null);
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null);

  useLayoutEffect(() => {
    if (!open) {
      setPos(null);
      return;
    }
    const anchor = anchorRef.current;
    const menu = menuRef.current;
    if (!anchor || !menu) return;

    const wrapEl = document.querySelector('.win');
    const wrapRect = wrapEl?.getBoundingClientRect();
    // `.win` 拿不到（测试 / 极早期帧）时退到视口：宁可按视口 clamp，也不要退回「不 clamp」。
    const box: OverlayBox = wrapRect
      ? { left: wrapRect.left, top: wrapRect.top, width: wrapRect.width, height: wrapRect.height }
      : { left: 0, top: 0, width: window.innerWidth, height: window.innerHeight };

    const triggerRect = anchor.getBoundingClientRect();
    const menuRect = menu.getBoundingClientRect();
    const placed = placeAnchoredMenu(
      triggerRect,
      { w: menuRect.width, h: menuRect.height },
      box,
      align,
      ANCHORED_MENU_GAP,
    );

    // 换回定位祖先的本地坐标系（菜单仍是 absolute）。offsetParent 缺席 ⇒ 已是相对视口，直接用。
    const parent = (menu.offsetParent as HTMLElement | null)?.getBoundingClientRect();
    setPos({
      left: placed.left - (parent?.left ?? 0),
      top: placed.top - (parent?.top ?? 0),
    });

    // 原型 `:3253`：打开即把焦点送进菜单，否则 Tab 序仍停在触发钮上、键盘用户进不去。
    menu.querySelector<HTMLElement>(MENU_ITEM_SELECTOR)?.focus();
  }, [open, align]);

  return {
    anchorRef,
    menuRef,
    style: pos
      ? { position: 'absolute', left: pos.left, top: pos.top }
      : { position: 'absolute', left: 0, top: 0, visibility: 'hidden' },
  };
}
