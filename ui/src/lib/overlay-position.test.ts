/**
 * `clampToWrap` 的行为门。
 *
 * 这段边界数学此前是 `ConnectionTopology.tsx` 的私有函数、零覆盖，而它同时被 tooltip 与两处右键菜单
 * 消费 —— 算错的表现是「菜单半个身子露在容器外」或「贴在左上角」，两者都不会让任何测试转红。
 * 抽成共享模块的同时把它的四条属性钉住。
 */
import { describe, it, expect } from 'vitest';
import { clampToWrap, placeAnchoredMenu, placeTip, OVERLAY_EDGE_PAD } from './overlay-position';

/** 400×300 的容器，左上角在视口 (100, 50)。 */
const WRAP = { left: 100, top: 50, width: 400, height: 300 };
const SIZE = { w: 120, h: 80 };

describe('clampToWrap', () => {
  /** 变异对照：把 `clientX - wrap.left` 换成 `clientX` → 本条转红（坐标没换算到容器系）。 */
  it('视口坐标换算成容器内坐标', () => {
    expect(clampToWrap(WRAP, 150, 100, SIZE, 0)).toEqual({ left: 50, top: 50 });
  });

  /** 偏移按方向叠加（tooltip 用 12px 让开指针）。 */
  it('偏移量加在容器内坐标上', () => {
    expect(clampToWrap(WRAP, 150, 100, SIZE, 12)).toEqual({ left: 62, top: 62 });
  });

  /**
   * 近右/下缘翻到指针另一侧。
   *
   * 变异对照：删掉两个三元里的翻转分支（恒 `x + offset`）→ 本条转红 —— 浮层会越过右下边界，
   * 表现为菜单被容器裁掉半截。
   */
  it('近右/下缘时翻到指针另一侧', () => {
    // x=460 → 容器内 360；360+120 > 400 ⇒ 翻到左侧 = 360-120 = 240。
    // y=320 → 容器内 270；270+80 > 300 ⇒ 翻到上方 = 270-80 = 190。
    expect(clampToWrap(WRAP, 460, 320, SIZE, 0)).toEqual({ left: 240, top: 190 });
  });

  /**
   * 翻转后仍越界（容器比浮层还窄）→ 贴边，**绝不**给出负坐标。
   *
   * 变异对照：去掉外层 `Math.max(OVERLAY_EDGE_PAD, …)` → 返回负值 → 本条转红。
   */
  it('容器比浮层还小 → 贴边而不是溢出到容器外', () => {
    const tiny = { left: 0, top: 0, width: 60, height: 40 };
    expect(clampToWrap(tiny, 55, 38, SIZE, 0)).toEqual({
      left: OVERLAY_EDGE_PAD,
      top: OVERLAY_EDGE_PAD,
    });
  });
});

/**
 * `placeTip` —— tooltip 引擎的元素锚定定位（原型 `tipPlace:3131-3152`）。
 *
 * 与 `clampToWrap` 分属两种锚定语义（元素边 vs 指针），共用的只有 `clampAxis` 那段边界数学。
 * 「实际会不会出屏」最终要真机看（字体/缩放决定 tip 实测尺寸），但**边界规则**在这层是确定的。
 */
describe('placeTip', () => {
  /** 1000×800 视口（引擎里 box 恒为视口）。 */
  const VIEW = { left: 0, top: 0, width: 1000, height: 800 };
  const TIP = { w: 100, h: 40 };
  const GAP = 6;
  /** 屏幕正中的 40×20 触发器，四向都放得下。 */
  const CENTER = { left: 480, top: 390, width: 40, height: 20 };

  it('默认 top：水平居中于触发器、贴其上缘', () => {
    // x = 480 + 20 - 50 = 450；y = 390 - 40 - 6 = 344。
    expect(placeTip(CENTER, TIP, VIEW, 'top', GAP)).toEqual({ side: 'top', left: 450, top: 344 });
  });

  it('right：垂直居中于触发器、贴其右缘（折叠侧栏用这一档）', () => {
    // x = 480 + 40 + 6 = 526；y = 390 + 10 - 20 = 380。
    expect(placeTip(CENTER, TIP, VIEW, 'right', GAP)).toEqual({ side: 'right', left: 526, top: 380 });
  });

  /**
   * 顶部放不下 → 翻到底部（原生 title 没有这条：它跟随鼠标，方向不可控）。
   *
   * 变异对照：删掉 `if (overflows) side = OPPOSITE_SIDE[side]` → 本条转红（tip 被 clamp 压在顶边，
   * 盖住触发元素自身）。
   */
  it('首选方位越界时翻到对侧', () => {
    const nearTop = { left: 480, top: 4, width: 40, height: 20 };
    // top 需 y = 4-40-6 = -42 < PAD ⇒ 翻 bottom：y = 4+20+6 = 30。
    expect(placeTip(nearTop, TIP, VIEW, 'top', GAP)).toEqual({ side: 'bottom', left: 450, top: 30 });

    const nearRight = { left: 960, top: 390, width: 40, height: 20 };
    // right 需 x = 1000+6 → 越右缘 ⇒ 翻 left：x = 960-100-6 = 854。
    expect(placeTip(nearRight, TIP, VIEW, 'right', GAP)).toEqual({ side: 'left', left: 854, top: 380 });
  });

  /**
   * **窄窗不出屏**：触发器贴在左缘，top 档的水平居中会算出负 x ⇒ 必须被 clamp 回 PAD。
   *
   * 变异对照：把返回里的 `clampAxis(...)` 换成裸 `p.x - box.left` → 本条转红（tip 左半截跑出屏）。
   */
  it('窄窗/贴边：clamp 回边距内，绝不给出屏坐标', () => {
    const atLeft = { left: 0, top: 390, width: 20, height: 20 };
    const p = placeTip(atLeft, TIP, VIEW, 'top', GAP);
    expect(p.left).toBe(OVERLAY_EDGE_PAD);

    const atRight = { left: 980, top: 390, width: 20, height: 20 };
    const q = placeTip(atRight, TIP, VIEW, 'top', GAP);
    expect(q.left).toBe(VIEW.width - TIP.w - OVERLAY_EDGE_PAD);
  });

  /**
   * 视口比 tip 还窄（真机上是极端缩放/极窄窗）→ 贴边给 PAD，**不给负坐标**。
   *
   * 变异对照：去掉 `clampAxis` **外层**的 `Math.max(PAD, …)` → 返回负值 → 本条转红（已实跑）。
   * 内层那个 `Math.max(PAD, extent - size - PAD)` 则是**可证冗余**的（`E < PAD` 时两种写法都落到
   * `PAD`），改坏它不会转红 —— 它是从 `clampToWrap` 原样继承下来的防御代码，本批不动它，
   * 但别把它当成有牙的防线。
   */
  it('视口比 tip 还小时贴边而不是溢出到屏外', () => {
    const tiny = { left: 0, top: 0, width: 60, height: 30 };
    const p = placeTip({ left: 10, top: 10, width: 10, height: 10 }, TIP, tiny, 'top', GAP);
    expect(p).toEqual({ side: 'bottom', left: OVERLAY_EDGE_PAD, top: OVERLAY_EDGE_PAD });
  });

  /**
   * 翻转只做一次：两侧都放不下时不来回弹，靠 clamp 收口（与原型同）。
   *
   * 60 高的视口塞 40 高的 tip：翻到 bottom 后 y=46 仍越下缘，被 clamp 到上界 60-40-8=12
   * （**不是** 8 —— 贴的是下边距，不是上边距）。
   */
  it('两侧都放不下时不反复翻转，落在 clamp 后的位置', () => {
    const shortView = { left: 0, top: 0, width: 1000, height: 60 };
    const p = placeTip({ left: 480, top: 20, width: 40, height: 20 }, TIP, shortView, 'top', GAP);
    expect(p.side).toBe('bottom');
    expect(p.top).toBe(shortView.height - TIP.h - OVERLAY_EDGE_PAD);
  });
});

/**
 * `placeAnchoredMenu` 的行为门 —— 移植原型 `miniMenu:3249-3251` 的两轴夹紧。
 *
 * 被守的缺陷：本仓四处 `.mini-menu` 是纯 CSS 锚定（`top:calc(100% + 6px)` + `left/right:0`），
 * 窄窗/靠边时整块菜单溢出窗口且没有任何测试会转红。
 */
describe('placeAnchoredMenu', () => {
  /** 925×740 的 `.win`，左上角在视口 (20, 10)。 */
  const WIN = { left: 20, top: 10, width: 925, height: 740 };
  const MENU = { w: 200, h: 160 };
  const GAP = 6;

  it('left 对齐：菜单左缘贴触发钮左缘，顶在触发钮下方 gap 处', () => {
    // 触发钮在 win 内 (100, 200) 处，40 高。
    const p = placeAnchoredMenu({ left: 120, top: 210, width: 90, height: 40 }, MENU, WIN, 'left', GAP);
    expect(p).toEqual({ left: 120, top: 210 + 40 + GAP });
  });

  /** 变异对照：把 `align === 'right'` 那支也算成 `trigger.left` → 本条转红（右对齐塌成左对齐）。 */
  it('right 对齐：菜单右缘贴触发钮右缘', () => {
    const p = placeAnchoredMenu({ left: 400, top: 210, width: 90, height: 40 }, MENU, WIN, 'right', GAP);
    expect(p.left).toBe(400 + 90 - MENU.w);
  });

  /** 变异对照：删掉 `clampAxis` → 本条转红（菜单右半身跑到窗口外）。 */
  it('触发钮靠右缘时按窗口右边界夹回，绝不出窗', () => {
    const nearRight = { left: WIN.left + WIN.width - 60, top: 210, width: 50, height: 40 };
    const p = placeAnchoredMenu(nearRight, MENU, WIN, 'left', GAP);
    expect(p.left).toBe(WIN.left + WIN.width - MENU.w - OVERLAY_EDGE_PAD);
  });

  /** 触发钮贴底时同理按下边界夹回（原型没有「翻到上方」那一支，此处逐字对齐）。 */
  it('触发钮靠下缘时按窗口下边界夹回', () => {
    const nearBottom = { left: 120, top: WIN.top + WIN.height - 50, width: 90, height: 40 };
    const p = placeAnchoredMenu(nearBottom, MENU, WIN, 'left', GAP);
    expect(p.top).toBe(WIN.top + WIN.height - MENU.h - OVERLAY_EDGE_PAD);
  });

  /** 窗口比菜单还小 —— 走 `clampAxis` 外层的 `Math.max(PAD, …)`，贴边而不是给负坐标。 */
  it('窗口比菜单还小时贴边，不给负坐标', () => {
    const tiny = { left: 0, top: 0, width: 100, height: 90 };
    const p = placeAnchoredMenu({ left: 10, top: 10, width: 20, height: 20 }, MENU, tiny, 'left', GAP);
    expect(p).toEqual({ left: OVERLAY_EDGE_PAD, top: OVERLAY_EDGE_PAD });
  });
});
