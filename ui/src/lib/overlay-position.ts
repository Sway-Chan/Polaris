/**
 * 容器内浮层定位（右键菜单 / tooltip 共用）。
 *
 * 抽出来的理由是**第三次要用**：`ConnectionTopology` 的 tooltip 与右键菜单已在用，
 * 连接页行右键菜单是第三处。`.ctx-menu` 是 `position:absolute`（`components.css:251`），
 * 所以坐标必须换算到最近的定位祖先，并 clamp 在它内部——这段边界数学抄第三遍必然分叉。
 *
 * （`hover-cards/HoverCard.tsx` 有一套更复杂的近边翻转，语义不同，**不并入**本模块。）
 */

/** 浮层与容器边缘的最小间距（原型 `showCtx:3770` 的 8px）。 */
export const OVERLAY_EDGE_PAD = 8;

/**
 * 容器矩形的最小面。
 *
 * 收成结构类型而非 `DOMRect`，是为了让「容器 = 视口」这个用法不必造假 `DOMRect`：
 * 连接页的行右键菜单挂在 `overflow:auto` 的 `.conn-scroll` 里，绝对定位会以**内容原点**为基准
 * 而 `getBoundingClientRect` 给的是**可视框**，滚动后两者错位 ⇒ 那处改用 `position:fixed` + 视口 clamp。
 * `DOMRect` 结构上满足本类型，既有调用点一字不改。
 */
export interface OverlayBox {
  left: number;
  top: number;
  width: number;
  height: number;
}

/**
 * 把视口坐标转成 `wrap` 内坐标并 clamp 到容器内。
 *
 * 浮层尺寸首帧未知（内容决定宽高）⇒ 调用方的用法是「先以 size=0 放置 + 透明，测得真实尺寸后再算一次」。
 * 近右/下缘时翻到指针另一侧；翻转后仍越界（容器比浮层还窄）则贴边——**绝不超出容器**。
 */
export function clampToWrap(
  wrap: OverlayBox,
  clientX: number,
  clientY: number,
  size: { w: number; h: number },
  offset: number,
): { left: number; top: number } {
  const x = clientX - wrap.left;
  const y = clientY - wrap.top;
  const wantLeft = x + offset + size.w > wrap.width ? x - offset - size.w : x + offset;
  const wantTop = y + offset + size.h > wrap.height ? y - offset - size.h : y + offset;
  return {
    left: clampAxis(wantLeft, wrap.width, size.w),
    top: clampAxis(wantTop, wrap.height, size.h),
  };
}

/**
 * 单轴 clamp：把容器内坐标夹进 `[PAD, extent - size - PAD]`。
 *
 * `clampToWrap`（指针锚定）与 `placeTip`（元素锚定）共用的**唯一**边界数学 —— 抽出来不是为了造抽象层，
 * 是因为内层那个 `Math.max(PAD, …)` 兜底（容器比浮层还窄时给 PAD 而非负值）抄第二遍必然分叉：
 * 漏掉它的表现是浮层跑到容器外的负坐标上，而两侧都没有测试会因此转红。
 */
function clampAxis(want: number, extent: number, size: number): number {
  return Math.max(
    OVERLAY_EDGE_PAD,
    Math.min(want, Math.max(OVERLAY_EDGE_PAD, extent - size - OVERLAY_EDGE_PAD)),
  );
}

/** 下拉菜单贴触发钮的哪一边对齐（`right` = 菜单右缘对齐触发钮右缘，即 CSS 里的 `right:0`）。 */
export type MenuAlign = 'left' | 'right';

/**
 * **锚定下拉菜单**的定位 —— 移植原型 `miniMenu`（`proto:3245-3252`）。
 *
 * 与另外两个的分工：`clampToWrap` 是指针锚定（右键菜单）、`placeTip` 是元素锚定但**居中**于触发元素
 * （tooltip）。下拉菜单两者都不是：它对齐触发钮的某一条竖边、挂在下方，越界只 clamp **不翻转**
 * （原型就是纯 `Math.max(8, Math.min(...))` 两轴夹紧，没有翻到上方那一支）。三者共用 `clampAxis`。
 *
 * 坐标进出都是 `box` 所在的坐标系（调用点里 `box` = `.win` 的视口矩形 ⇒ 即 client 坐标）。
 * 本仓此前四处 mini-menu 是纯 CSS 锚定（`top:calc(100% + 6px)` + `left/right:0`），零测量零 clamp
 * ⇒ 窄窗 / 触发钮靠边时菜单直接溢出窗口。
 */
export function placeAnchoredMenu(
  trigger: OverlayBox,
  menu: { w: number; h: number },
  box: OverlayBox,
  align: MenuAlign,
  gap: number,
): { left: number; top: number } {
  const wantLeft = align === 'right' ? trigger.left + trigger.width - menu.w : trigger.left;
  const wantTop = trigger.top + trigger.height + gap;
  return {
    left: box.left + clampAxis(wantLeft - box.left, box.width, menu.w),
    top: box.top + clampAxis(wantTop - box.top, box.height, menu.h),
  };
}

/** tooltip 相对触发元素的四个方位（原型 `tipPlace:3131-3152` 的 `side`）。 */
export type TipSide = 'top' | 'bottom' | 'left' | 'right';

const OPPOSITE_SIDE: Record<TipSide, TipSide> = {
  top: 'bottom',
  bottom: 'top',
  left: 'right',
  right: 'left',
};

/**
 * 把 `data-tip-side` 的原始属性值收成合法方位，认不出就回落 `top`（原型 `el.dataset.tipSide||'top'`）。
 *
 * 属性值来自 DOM 字符串，TS 类型管不到；拼错时**必须**回落到可用方位而不是让 `at()` 落进 `right` 分支，
 * 否则一个 typo 会让 tip 悄悄跑到反方向，且没有任何报错。
 */
export function tipSideOf(raw: string | undefined | null): TipSide {
  return raw === 'top' || raw === 'bottom' || raw === 'left' || raw === 'right' ? raw : 'top';
}

/**
 * **元素锚定**的浮层定位（tooltip 引擎专用）—— 移植原型 `tipPlace`（`proto:3131-3152`）。
 *
 * 与 `clampToWrap` 的分工：那个是**指针锚定**（跟随 clientX/clientY，近边翻到指针另一侧），右键菜单用；
 * 这个是**元素锚定**（贴触发元素的某一边，越界翻到对侧边），tooltip 用。两者的差别不是参数不同而是
 * 语义不同（「翻转」翻的是不同的东西），故不能互相顶替；共用的边界数学收在 `clampAxis`。
 *
 * 翻转只做一次（与原型一致）：翻完仍放不下时靠 clamp 贴边，**绝不超出 `box`**。
 * 坐标进出都是 `box` 所在的坐标系（引擎里 `box` = 视口 ⇒ 即 client 坐标）。
 */
export function placeTip(
  trigger: OverlayBox,
  tip: { w: number; h: number },
  box: OverlayBox,
  preferred: TipSide,
  gap: number,
): { left: number; top: number; side: TipSide } {
  const at = (s: TipSide): { x: number; y: number } => {
    const cx = trigger.left + trigger.width / 2 - tip.w / 2;
    const cy = trigger.top + trigger.height / 2 - tip.h / 2;
    if (s === 'top') return { x: cx, y: trigger.top - tip.h - gap };
    if (s === 'bottom') return { x: cx, y: trigger.top + trigger.height + gap };
    if (s === 'left') return { x: trigger.left - tip.w - gap, y: cy };
    return { x: trigger.left + trigger.width + gap, y: cy };
  };

  const right = box.left + box.width;
  const bottom = box.top + box.height;
  let side = preferred;
  const first = at(side);
  const overflows =
    (side === 'top' && first.y < box.top + OVERLAY_EDGE_PAD) ||
    (side === 'bottom' && first.y + tip.h > bottom - OVERLAY_EDGE_PAD) ||
    (side === 'left' && first.x < box.left + OVERLAY_EDGE_PAD) ||
    (side === 'right' && first.x + tip.w > right - OVERLAY_EDGE_PAD);
  if (overflows) side = OPPOSITE_SIDE[side];

  const p = at(side);
  return {
    side,
    left: box.left + clampAxis(p.x - box.left, box.width, tip.w),
    top: box.top + clampAxis(p.y - box.top, box.height, tip.h),
  };
}
