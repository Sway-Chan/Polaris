/**
 * HoverCard —— 非模态 hover 详情卡通用原语（设计文档 §1.5：RuleHoverCard / apprule / refs 三卡共用）。
 * ~/docs/polaris/design/polaris-dialog-layer-and-governance.md §1.5：
 *   「hover 详情卡与右键/mini 菜单沿用已验证的 clamp 模式……不用 Popover API（macOS 13.0 floor 不过线）」
 *
 * 绝对不经 dialog-store / DialogHost / <Modal> —— 这是与弹窗完全独立的 UI 类别：非 top-layer、
 * 非 inert、无焦点圈禁，纯 hover 触发的浮层。
 *
 * 定位复用 ConnectionTopology.tsx 的 clampToWrap 思路（同文件 :45-61：视口/容器坐标 + 近边翻转 +
 * 贴边不越界），改为对整个视口 clamp——三个消费点（规则名/应用卡/资源引用徽章）分布在可滚动列表行
 * 内，没有共同的小型定位容器可测，故用 position:fixed + getBoundingClientRect 直接相对视口定位。
 *
 * 交互延迟对齐原型统一 tooltip 引擎（prototype :3113 TIP_DELAY=500/TIP_CARD_CLOSE=80）：
 * mouseenter 500ms 后展开；mouseleave 留 80ms 宽限关闭，让指针能移入卡片本身（卡片的 mouseenter 会
 * 取消这个关闭计时）。ESC / 滚动 / resize 立即关闭。
 */
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode, type RefObject } from 'react';
import { createPortal } from 'react-dom';

const OPEN_DELAY = 500; // 原型 TIP_DELAY
const CLOSE_GRACE = 80; // 原型 TIP_CARD_CLOSE
const EDGE_PAD = 8;
const GAP = 8; // 触发元素与卡片的间距

interface Pos {
  left: number;
  top: number;
}

/** 默认贴触发元素下方左对齐；下缘放不下则翻到上方；右缘放不下则贴视口右缘，不越界。 */
function placeCard(triggerRect: DOMRect, size: { w: number; h: number }): Pos {
  let top = triggerRect.bottom + GAP;
  if (top + size.h > window.innerHeight - EDGE_PAD) {
    top = triggerRect.top - size.h - GAP;
  }
  let left = triggerRect.left;
  if (left + size.w > window.innerWidth - EDGE_PAD) {
    left = window.innerWidth - size.w - EDGE_PAD;
  }
  return { left: Math.max(EDGE_PAD, left), top: Math.max(EDGE_PAD, top) };
}

export function useHoverCard<T extends HTMLElement = HTMLElement>() {
  const triggerRef = useRef<T>(null);
  const cardRef = useRef<HTMLDivElement>(null);
  const openTimer = useRef<ReturnType<typeof setTimeout>>();
  const closeTimer = useRef<ReturnType<typeof setTimeout>>();
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<Pos | null>(null);

  const show = useCallback(() => {
    clearTimeout(closeTimer.current);
    clearTimeout(openTimer.current);
    openTimer.current = setTimeout(() => setOpen(true), OPEN_DELAY);
  }, []);

  const scheduleHide = useCallback(() => {
    clearTimeout(openTimer.current);
    clearTimeout(closeTimer.current);
    closeTimer.current = setTimeout(() => setOpen(false), CLOSE_GRACE);
  }, []);

  const hideNow = useCallback(() => {
    clearTimeout(openTimer.current);
    clearTimeout(closeTimer.current);
    setOpen(false);
  }, []);

  const cancelHide = useCallback(() => {
    clearTimeout(closeTimer.current);
  }, []);

  // 展开后按触发元素 + 卡片实测尺寸定位（原型同款两段式：先挂载测尺寸，再算位置，避免闪烁）。
  useLayoutEffect(() => {
    if (!open || !triggerRef.current || !cardRef.current) return;
    const tr = triggerRef.current.getBoundingClientRect();
    const cr = cardRef.current.getBoundingClientRect();
    setPos(placeCard(tr, { w: cr.width, h: cr.height }));
  }, [open]);

  // ESC / 滚动 / resize 关闭（原型 initTips :3193-3195 同语义）。
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') hideNow();
    };
    document.addEventListener('keydown', onKey);
    window.addEventListener('scroll', hideNow, true);
    window.addEventListener('resize', hideNow);
    return () => {
      document.removeEventListener('keydown', onKey);
      window.removeEventListener('scroll', hideNow, true);
      window.removeEventListener('resize', hideNow);
    };
  }, [open, hideNow]);

  useEffect(
    () => () => {
      clearTimeout(openTimer.current);
      clearTimeout(closeTimer.current);
    },
    [],
  );

  return {
    triggerRef,
    cardRef,
    open,
    pos,
    /** 展开到触发元素上：`<b ref={triggerRef} {...triggerHandlers}>`。 */
    triggerHandlers: { onMouseEnter: show, onMouseLeave: scheduleHide },
    /** 卡片本身也要接住这两个事件——指针移入卡片内应取消关闭，移出才真正关闭。 */
    cardHandlers: { onMouseEnter: cancelHide, onMouseLeave: hideNow },
  };
}

export function HoverCardPanel({
  cardRef,
  open,
  pos,
  onMouseEnter,
  onMouseLeave,
  children,
}: {
  cardRef: RefObject<HTMLDivElement>;
  open: boolean;
  pos: Pos | null;
  onMouseEnter: () => void;
  onMouseLeave: () => void;
  children: ReactNode;
}) {
  if (!open) return null;
  // `.hover-card` 是 position:fixed，本应相对视口；但 `.win`/`.main` 的 container-type（container query 需要）
  // 隐含 layout containment，会给 fixed 后代建包含块 → 卡片坐标被容器左上角偏移。portal 到 body 逃出容器，
  // 坐标恢复视口相对（hover 卡都在屏内容上、非 dialog 内，portal 到 body 恒安全）。
  return createPortal(
    <div
      ref={cardRef}
      className="hover-card show"
      role="tooltip"
      style={pos ?? { left: 0, top: 0, opacity: 0 }}
      onMouseEnter={onMouseEnter}
      onMouseLeave={onMouseLeave}
    >
      {children}
    </div>,
    document.body,
  );
}
