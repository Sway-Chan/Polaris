/**
 * Csel 纯逻辑层 —— 定位（fixed 视口坐标 + 近边翻转）与键盘协议 reducer。
 *
 * 抽为纯函数的动机（对齐 §1.3 移植坑 + vitest gate）：
 *  - 原型 `positionCsel` :5441 是 winRect 相对的 absolute 定位；React 版 listbox 用 `position:fixed`
 *    渲染在 dialog 子树内 —— 逃出 `.dlg-body` 的 `overflow-y:auto` 裁切（那个坑），坐标改为视口相对
 *    （fixed 的 containing block = 视口，trigger.getBoundingClientRect() 已是视口坐标，直接用）。
 *  - 定位/翻转/键盘全是纯计算，抽出后可 vitest 覆盖翻转边界与协议分支，不需 jsdom/真机。
 *
 * 键盘协议照原型 `cselKey` :5468（↑/↓ 环绕、Enter/Space 选中、Esc 关、Tab 关并放行）。
 * 注意：**Esc 不在此处处理** —— 原生 `<dialog>` 的 ESC 走 `cancel` 事件，由 Modal 统一路由
 * （csel 开 → 只关菜单；再 ESC → 关弹窗），避免 keydown 与 cancel 双重触发（见 Modal.tsx）。
 */

import type { ReactNode } from 'react';

/** 触发器/菜单矩形（视口坐标，取自 getBoundingClientRect）。 */
export interface Rect {
  left: number;
  top: number;
  bottom: number;
  width: number;
}

export interface Viewport {
  width: number;
  height: number;
}

export interface CselPosition {
  /** fixed left（视口坐标，px） */
  left: number;
  /** fixed top（视口坐标，px） */
  top: number;
  /** 菜单宽度锁定为触发器宽度（长选项省略号，不撑宽菜单，对齐原型 :1485 注释） */
  width: number;
  /** 是否向上翻转（下方空间不足） */
  flipped: boolean;
}

/** 视口边缘留白（原型 positionCsel 的 8px）。 */
export const CSEL_EDGE = 8;
/** 触发器与菜单间距（原型 positionCsel 的 5px）。 */
export const CSEL_GAP = 5;

/**
 * 计算 fixed 定位的 csel 菜单位置（视口坐标）。
 *
 * 语义对齐原型 positionCsel：
 *  - 默认贴触发器下沿（bottom + GAP）；
 *  - 下方放不下（top + menuHeight 超出视口下缘 - EDGE）→ 翻到触发器上方（top - menuHeight - GAP）；
 *  - 水平：左对齐触发器左沿；右溢出 → 贴右缘内收；再兜底不小于 EDGE。
 *
 * @param trigger 触发器矩形（视口坐标）
 * @param menuHeight 菜单实测高度（已受 CSS max-height 约束）
 * @param vp 视口尺寸
 */
export function computeCselPosition(
  trigger: Rect,
  menuHeight: number,
  vp: Viewport,
): CselPosition {
  const width = trigger.width;

  let top = trigger.bottom + CSEL_GAP;
  let flipped = false;
  if (top + menuHeight > vp.height - CSEL_EDGE) {
    // 下方不足 → 翻转向上；翻转后若容器比菜单还矮则贴顶边（绝不越出视口）
    top = Math.max(CSEL_EDGE, trigger.top - menuHeight - CSEL_GAP);
    flipped = true;
  }

  let left = trigger.left;
  if (left + width > vp.width - CSEL_EDGE) {
    left = Math.max(CSEL_EDGE, vp.width - width - CSEL_EDGE);
  }
  left = Math.max(CSEL_EDGE, left);

  return { left, top, width, flipped };
}

// ─────────────────────────────────────────────────────────────────────────────
// 分组（optgroup）支持 —— D4 规则弹窗 15 类型 5 分组必需（§1.3 openCsel flat-index 参照）。
//
// **ADDITIVE + 100% 向后兼容**：`<Csel>` 的 options 由 `CselOption[]` 拓宽为
// `CselOption[] | CselGroup[]`（并集，扁平仍是一等公民）。分组的键盘协议无需新 reducer——
// 头（group header）不占扁平索引，↑/↓ 天然跳过它：`buildCselRows` 把分组摊平成「头行 + 带
// flatIndex 的选项行」，选项的 flatIndex 跨组连续，`cselKeyReduce(active, flatCount, key)` 照旧
// 环绕。这样分组只改「渲染结构」，不改「导航算法」，与既有扁平路径同一套数学（对齐原型 openCsel
// 的注释「option buttons keep the FLAT option index」）。
// ─────────────────────────────────────────────────────────────────────────────

/** 单个选项（结构类型，与 Csel.tsx 的 CselOption 结构兼容；此处定义避免 csel-logic ↔ Csel 循环依赖）。 */
export interface CselOptionLike {
  value: string;
  label: string;
  disabled?: boolean;
  /**
   * 行首图标（`ReactNode`，省略 = 不画，既有全部消费方即此形态）。
   *
   * 为什么落在这个结构类型上：`Csel` 渲染的是 `buildCselRows` 摊出来的 `CselOptionLike`，
   * 字段不在这里 `row.opt.icon` 就取不到。`import type` 是纯类型引用，本模块仍无运行期依赖。
   */
  icon?: ReactNode;
  /**
   * 危险度：选中它会产生破坏性后果（当前唯一消费方 = 规则弹窗「目标出站」的**阻断**项）。
   *
   * 为什么加在通道上而不是给规则页单刷一层红：`.mini-menu`/`.node-menu` 的阻断项能上色，是因为
   * 它们是手写菜单、行上本就有 `.danger` 类；`Csel` 的选项此前只有 `value/label/disabled/icon`
   * 四个字段，**结构上无处表达危险度** —— 这才是「其他都是红色、规则页面不是」的根因（不是有人
   * 忘了调色）。补一个字段后，第 N 个需要标危险的下拉选项不必再手写一套菜单。
   *
   * 渲染端只把它落成 `.csel-opt.danger` / `.csel-trigger.danger` 两个类，取什么色由覆盖层定，
   * 与 `.mi.danger` / `.tray-i.danger` 共用同一条规则（styles/index.css「阻断=动作标签轴」段）。
   */
  danger?: boolean;
}

/** 分组：一个组标签 + 组内选项。 */
export interface CselGroup {
  label: string;
  options: readonly CselOptionLike[];
  /**
   * 可折叠组的**稳定键**。带 `id` ⇒ 该组可折叠（组头变成可点的展开钮，展开态按这个 id 记）；
   * 省略 ⇒ 恒展开（规则类型那 15×5 分组即此形态，逐字保持旧行为）。
   *
   * 为什么用「有没有 id」而不是另加一个 `collapsible` 布尔：折叠态必须有键才记得住，
   * 两个字段永远要一起给/一起不给，合成一个就没有「可折叠但没键」这种非法组合可表达。
   */
  id?: string;
}

/** Csel 的 options 入参：扁平选项数组 **或** 分组数组（二选一，判别见 isCselGrouped）。 */
export type CselOptions = readonly CselOptionLike[] | readonly CselGroup[];

/**
 * 判别 options 是否为分组形态。判据：非空且首元素带 `options` 数组（CselGroup 有 options，
 * CselOption 无）。空数组按扁平处理（无歧义，两形态的空表渲染一致）。
 */
export function isCselGrouped(options: CselOptions): options is readonly CselGroup[] {
  return (
    options.length > 0 &&
    Array.isArray((options[0] as CselGroup).options)
  );
}

/**
 * 摊平为扁平选项列表（value↔index 映射的单一口径；分组按声明顺序拼接）。
 *
 * **与折叠态无关**：折叠只藏渲染行，不改索引空间（见 buildCselRows）。故键盘协议的环绕基数
 * 恒为「全部选项数」，折叠组里的项照样能被 ↑/↓ 走到（走到时由调用方展开那组，见 cselGroupIdAt）。
 */
export function flattenCselOptions(options: CselOptions): CselOptionLike[] {
  return isCselGrouped(options)
    ? options.flatMap((g) => g.options.slice())
    : options.slice();
}

/** 渲染行：组头（不占扁平索引）或带 flatIndex 的选项（跨组连续）。 */
export type CselRow =
  | {
      kind: 'header';
      label: string;
      key: string;
      /** 可折叠组的 id（undefined = 该组不可折叠，组头只是视觉分隔）。 */
      groupId?: string;
      /** 该组当前是否折叠（折叠时其选项行不在本序列里）。 */
      collapsed: boolean;
      /** 组内选项总数 —— 折叠态下这是唯一能看出「这组有多少个」的信号。 */
      count: number;
    }
  | { kind: 'option'; opt: CselOptionLike; flatIndex: number };

/**
 * 构造渲染行序列。扁平 options → 全 option 行（flatIndex = 数组下标）；分组 options → 每组一个
 * header 行 + 其选项行，**选项的 flatIndex 跨组连续**（与 flattenCselOptions 的顺序一致）。
 * header 不进扁平索引空间 → ↑/↓（cselKeyReduce 在 flatCount 上环绕）自然跳过组头。
 *
 * 折叠组的选项行不渲染，但**索引照占**（`flat` 照样步进）—— flatIndex 恒等于该项在
 * `flattenCselOptions` 里的下标，与折叠态无关。这条不变量是键盘可达性的前提：
 *  - 组头不在焦点链里（菜单一 Tab 就关），折叠组里的项若同时退出索引空间，键盘用户就**永远
 *    够不到它们**，等于把一批节点藏死；
 *  - 索引不随折叠漂移，展开/收起也就不会把 `active` 悄悄挪到另一行上。
 * 走到折叠组里的项时由调用方把那组展开（`cselGroupIdAt`），可见性随之恢复。
 *
 * @param openIds 展开的可折叠组 id 集合；**省略 = 全展开**（无 `id` 的组本就恒展开，不受影响）。
 */
export function buildCselRows(options: CselOptions, openIds?: ReadonlySet<string>): CselRow[] {
  const rows: CselRow[] = [];
  let flat = 0;
  if (isCselGrouped(options)) {
    options.forEach((g, gi) => {
      const collapsed = g.id !== undefined && !(openIds?.has(g.id) ?? true);
      rows.push({
        kind: 'header',
        label: g.label,
        key: `csel-grp-${gi}`,
        groupId: g.id,
        collapsed,
        count: g.options.length,
      });
      if (collapsed) {
        flat += g.options.length; // 见头注：折叠不改索引空间
        return;
      }
      for (const o of g.options) rows.push({ kind: 'option', opt: o, flatIndex: flat++ });
    });
  } else {
    for (const o of options) rows.push({ kind: 'option', opt: o, flatIndex: flat++ });
  }
  return rows;
}

/**
 * 某个扁平索引落在哪个**可折叠**组里（不可折叠组 / 扁平选项 → undefined）。
 *
 * 键盘 ↑/↓ 走的是完整索引空间（折叠不改索引，见 buildCselRows），落进折叠组时靠这个把那组
 * 展开 —— 没有它，折叠组里的选项对键盘用户就是不可达的死区。
 */
export function cselGroupIdAt(options: CselOptions, flatIndex: number): string | undefined {
  if (!isCselGrouped(options)) return undefined;
  let start = 0;
  for (const g of options) {
    const end = start + g.options.length;
    if (flatIndex >= start && flatIndex < end) return g.id;
    start = end;
  }
  return undefined;
}

export type CselKeyAction = 'move' | 'choose' | 'close' | 'close-blur' | 'none';

export interface CselKeyResult {
  /** 新的高亮索引（move 时更新；其余保持入参） */
  active: number;
  action: CselKeyAction;
}

/**
 * 计算 ↑/↓ 环绕后的落点，跳过 `isDisabled` 判真的索引（LOW-8：禁用行永不该是 active 目标）。
 * 未传 `isDisabled` 时退化为原始环绕算术（向后兼容既有 3 参调用）。全部禁用（不应出现，调用方
 * 保证至少一项可用）时循环一圈找不到可用项 → 原地不动，防死循环。
 */
function stepPastDisabled(
  from: number,
  count: number,
  dir: 1 | -1,
  isDisabled?: (index: number) => boolean,
): number {
  if (!isDisabled) return (from + dir + count) % count;
  let next = from;
  for (let i = 0; i < count; i++) {
    next = (next + dir + count) % count;
    if (!isDisabled(next)) return next;
  }
  return from;
}

/**
 * csel 打开态的键盘协议 reducer（纯）。对齐原型 cselKey :5468。
 *  - ArrowDown/ArrowUp：active 环绕移动（move），跳过 disabled 项（LOW-8）
 *  - Enter/Space：选中当前 active（choose）
 *  - Escape：关闭菜单（close）—— 实际拦截在 Modal cancel，见文件头注释
 *  - Tab：关闭并放行焦点（close-blur）
 *  - 其它：none
 *
 * @param active 当前高亮索引
 * @param count 可选项数量（>0）
 * @param key KeyboardEvent.key
 * @param isDisabled 按 flatIndex 判断该项是否禁用（缺省 = 无禁用项，行为与之前一致）
 */
export function cselKeyReduce(
  active: number,
  count: number,
  key: string,
  isDisabled?: (index: number) => boolean,
): CselKeyResult {
  if (count <= 0) return { active, action: 'none' };
  switch (key) {
    case 'ArrowDown':
      return { active: stepPastDisabled(active, count, 1, isDisabled), action: 'move' };
    case 'ArrowUp':
      return { active: stepPastDisabled(active, count, -1, isDisabled), action: 'move' };
    case 'Enter':
    case ' ':
      return { active, action: 'choose' };
    case 'Escape':
      return { active, action: 'close' };
    case 'Tab':
      return { active, action: 'close-blur' };
    default:
      return { active, action: 'none' };
  }
}
