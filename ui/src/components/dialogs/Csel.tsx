/**
 * <Csel> —— 全应用统一的自定义下拉。
 *
 * 原型坑：csel 是全局单例菜单 `#csel-menu` 挂 winEl（`cselMenuEl` :5422）。在原生 `<dialog>` 下
 * **被 top-layer 遮盖 + inert 冻结**（菜单在 dialog 子树外）→ 不可见不可点。**从原型照搬会踩的第一个坑。**
 *
 * React 版实现：
 *  - 触发器 + listbox 渲染在**自身组件内**（dialog 子树内）→ 仍在 top-layer，不被 inert；无 portal、无单例；
 *  - listbox 用 `position:fixed` 按触发器 getBoundingClientRect() 定位 + 近边翻转（computeCselPosition）
 *    → fixed 逃出 `.dlg-body` 的 `overflow-y:auto` 裁切（R8 下拉遮挡根治），坐标视口相对；
 *  - 键盘协议照原型（↑/↓/Enter/Space/Tab，cselKeyReduce）；**ESC 不在此处理**，交 Modal 的 cancel 统一
 *    路由（csel 开→只关菜单；再 ESC→关弹窗），避免 keydown+cancel 双关；
 *  - 受控组件 value/onChange，不保留隐藏原生 select（React 无存量 .value 读取包袱）；
 *  - 打开态经 ModalContext.cselCloseRef 上报 Modal（供 ESC/scrim 先关菜单）。
 *
 * 分组 <optgroup>（规则 15 类型 5 分组，D4）：options 由 `CselOption[]` 拓宽为
 * `CselOption[] | CselGroup[]`（ADDITIVE，扁平 100% 向后兼容——D3/D5/FieldRenderer 仍传扁平数组）。
 * 分组只改渲染结构（组头 + 选项行），导航仍走 flatIndex 数学（buildCselRows/cselKeyReduce），组头不占索引。
 */

import {
  useCallback,
  useContext,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react';
import { createPortal } from 'react-dom';
import { ModalContext } from './Modal';
import { revealSiblingGroup, useRevealAfterCommit } from '@/components/reveal';
import {
  buildCselRows,
  cselGroupIdAt,
  cselKeyReduce,
  computeCselPosition,
  CSEL_GAP,
  flattenCselOptions,
  type CselGroup,
  type CselPosition,
  type Rect,
} from './csel-logic';

export interface CselOption {
  value: string;
  label: string;
  disabled?: boolean;
  /**
   * 行首图标（省略 = 不画）。用于让本组件承载的下拉与 `.node-menu` / `.tray-menu` 那两处节点
   * 选择器**同一套行词汇**（那两处的节点行本就带国旗、策略行本就带 svg，只有这里此前什么都没有）。
   * 渲染端只负责摆位（`.csel-ico` 定尺寸并复位 prototype 那条 `.sel svg{position:absolute}`
   * 泄漏，见 styles/index.css），画什么由调用方给。
   */
  icon?: ReactNode;
  /** 危险度（选中即破坏性，如「阻断」）。语义与落类见 csel-logic.ts `CselOptionLike.danger`。 */
  danger?: boolean;
}

export type { CselGroup } from './csel-logic';

export interface CselProps {
  value: string;
  onChange: (value: string) => void;
  disabled?: boolean;
  /**
   * 选项：扁平 `CselOption[]`（向后兼容，绝大多数消费方）**或** 分组 `CselGroup[]`（D4 规则类型 5 分组）。
   * 二选一由 isCselGrouped 判别，扁平路径行为与分组前逐字一致。
   */
  options: readonly CselOption[] | readonly CselGroup[];
  id?: string;
  ariaLabel?: string;
  /** 附加到 `.sel.csel` 外层的类名（工具栏 `.nt-proto`/`.nh-sort` 布局类须落在 `.sel` 上——
   *  `.node-toolbar > .sel` flex 规则依赖 `.sel` 为直接子；Settings 下拉的布局类同理）。 */
  className?: string;
  /**
   * 可折叠分组（`CselGroup.id`）的**初始展开集**，每次打开菜单时重置为它。
   *
   * 折叠态是「本次打开」的临时视图态，不跨次残留：菜单关着的期间 `value` 可能被别处改掉，
   * 沿用上次的展开集就会展开错组。默认展开哪一组由调用方按语义算（节点选择器一律用
   * `domain/server-grouping` 的 `defaultOpenGroupIds`：只展开含当前选中项的那组）。
   * 省略 = 不折叠任何组（扁平 / 无 id 分组的既有消费方逐字不受影响）。
   */
  openGroupIds?: ReadonlySet<string>;
  /**
   * `value` 命中不了任何选项时触发器显示的文案（原生 `<select>` 的空态 placeholder 语义）。
   *
   * 存在的理由是**去掉一个 hack**，不是加能力：规则集选择器那类「动作型下拉」（选中即追加一条
   * 引用、`value` 恒为空串）此前靠塞一个 `{value:'', label:'从已下载的规则集添加…'}` 的假选项
   * 来给触发器供文案 —— 那个假选项会占一个扁平索引、能被 ↑/↓ 走到、能被 Enter「选中」（no-op），
   * 且**分组化之后无处安放**（它不属于内置也不属于外置，硬塞进任一组就是说谎）。
   * 省略 ⇒ 行为与之前逐字一致（命不中就显空串）。
   */
  placeholder?: string;
}

/** 未传 `openGroupIds` 时的稳定空集（每次新建会让 useState 初值/依赖数组无谓抖动）。 */
const NO_OPEN_GROUPS: ReadonlySet<string> = new Set();

interface OpenState {
  open: boolean;
  /** 触发器矩形（打开瞬间捕获，供菜单宽度锁定 + 定位测量） */
  rect: Rect | null;
}

export function Csel({
  value,
  onChange,
  disabled,
  options,
  id,
  ariaLabel,
  className,
  openGroupIds,
  placeholder,
}: CselProps) {
  const ctx = useContext(ModalContext);
  const wrapRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const [state, setState] = useState<OpenState>({ open: false, rect: null });
  const [pos, setPos] = useState<CselPosition | null>(null);
  const [active, setActive] = useState(0);
  const [openGroups, setOpenGroups] = useState<ReadonlySet<string>>(openGroupIds ?? NO_OPEN_GROUPS);

  // 扁平选项列表（value↔flatIndex 的单一口径，**与折叠无关**）+ 渲染行（分组时含组头，
  // 折叠组只渲染组头但索引照占，见 buildCselRows 头注）。
  const flat = useMemo(() => flattenCselOptions(options), [options]);
  const rows = useMemo(() => buildCselRows(options, openGroups), [options, openGroups]);

  const currentIndex = flat.findIndex((o) => o.value === value);
  const currentLabel = currentIndex >= 0 ? flat[currentIndex].label : (placeholder ?? '');
  /**
   * 触发器也吃 `danger` —— 菜单里表达不了「已选中的危险项」。
   *
   * 理由是层叠事实而非偏好：菜单内选中态由 `.csel-opt.on`（styles/index.css，**全仓共享、无容器
   * 前缀**、三处节点选择器统一门在守）占着 flow-weak/flow-hi，它必须赢过 `.danger`，否则那道门
   * 描述的「下拉里被选中的那一项在全仓是同一个视觉」就破了。⇒ 选中态的红只能落在触发器上，而
   * 触发器恰恰是菜单关着时（99% 的时间）唯一可见的那一格。
   */
  const currentDanger = currentIndex >= 0 && flat[currentIndex].danger === true;

  const closeMenu = useCallback(() => {
    setState((s) => (s.open ? { open: false, rect: null } : s));
    setPos(null);
    // 焦点在 csel 内（trigger 或即将卸载的 option）→ 收回 trigger；在别处→不抢焦点。
    const wrap = wrapRef.current;
    if (wrap && wrap.contains(document.activeElement)) triggerRef.current?.focus();
  }, []);

  const openMenu = useCallback(() => {
    const trig = triggerRef.current;
    if (!trig) return;
    const r = trig.getBoundingClientRect();
    // 折叠态每次开菜单重置（见 openGroupIds 的注释）。
    setOpenGroups(openGroupIds ?? NO_OPEN_GROUPS);
    setState({ open: true, rect: { left: r.left, top: r.top, bottom: r.bottom, width: r.width } });
    setActive(currentIndex >= 0 ? currentIndex : 0);
  }, [currentIndex, openGroupIds]);

  /** 展开/收起一个可折叠组（多组可同时展开，非手风琴互斥——与 `.mini-menu`/`.node-menu` 同款）。 */
  const scheduleReveal = useRevealAfterCommit();
  const toggleGroup = useCallback((gid: string) => {
    setOpenGroups((prev) => {
      const next = new Set(prev);
      if (next.has(gid)) next.delete(gid);
      else next.add(gid);
      return next;
    });
  }, []);

  const choose = useCallback(
    (i: number) => {
      const opt = flat[i];
      if (!opt || opt.disabled) return;
      if (opt.value !== value) onChange(opt.value);
      closeMenu();
    },
    [flat, value, onChange, closeMenu],
  );

  // 定位：菜单渲染后（layout 阶段）测高 → 计算 fixed 坐标 + 翻转，避免闪跳。
  // `openGroups` 进依赖：展开一个组会把菜单撑高，不重算就会从视口下沿溢出（翻转判据用的是旧高度）。
  useLayoutEffect(() => {
    if (!state.open || !state.rect) {
      setPos(null);
      return;
    }
    const menu = menuRef.current;
    if (!menu) return;
    setPos(
      computeCselPosition(state.rect, menu.offsetHeight, {
        width: window.innerWidth,
        height: window.innerHeight,
      }),
    );
  }, [state.open, state.rect, openGroups]);

  // 打开态副作用：上报 Modal（ESC/scrim 先关菜单）+ 外部点击/滚动/缩放关菜单（对齐原型 :5502）。
  useLayoutEffect(() => {
    if (!state.open) return;
    if (ctx) ctx.cselCloseRef.current = closeMenu;

    const onDown = (e: PointerEvent) => {
      const t = e.target as Node;
      // 菜单可能 portal 到 body（脱离 wrapRef 子树，见下方 createPortal）→ 须同时查 menuRef，
      // 否则点菜单项时 pointerdown 先判「外部」关闭、卸载按钮 → onClick 落空（选不中）。
      if (wrapRef.current?.contains(t) || menuRef.current?.contains(t)) return;
      closeMenu();
    };
    const onScroll = (e: Event) => {
      const t = e.target as Node;
      if (menuRef.current?.contains(t)) return; // 菜单自身内部滚动（长列表 scrollIntoView）不自关
      closeMenu();
    };
    document.addEventListener('pointerdown', onDown, true);
    document.addEventListener('scroll', onScroll, true);
    window.addEventListener('resize', closeMenu);
    return () => {
      if (ctx && ctx.cselCloseRef.current === closeMenu) ctx.cselCloseRef.current = null;
      document.removeEventListener('pointerdown', onDown, true);
      document.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('resize', closeMenu);
    };
  }, [state.open, ctx, closeMenu]);

  // 高亮项滚入可视（长列表）。
  useLayoutEffect(() => {
    if (!state.open || !pos) return;
    const el = menuRef.current?.querySelector<HTMLElement>('.csel-opt.active');
    el?.scrollIntoView({ block: 'nearest' });
  }, [state.open, pos, active]);

  const onTriggerKeyDown = (e: React.KeyboardEvent<HTMLButtonElement>) => {
    if (!state.open) {
      if (e.key === 'ArrowDown' || e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        openMenu();
      }
      return;
    }
    if (e.key === 'Escape') return; // Modal.onCancel 统一处理 ESC 链（避免双关）
    // LOW-8：↑/↓ 跳过 disabled 选项，禁用行永不落为 active（否则 Enter 在其上是无声 no-op，体感卡死）。
    const { active: next, action } = cselKeyReduce(active, flat.length, e.key, (i) => flat[i]?.disabled === true);
    if (action === 'close-blur') {
      closeMenu(); // Tab：不 preventDefault，放行默认焦点移出
      return;
    }
    if (action === 'none') return;
    e.preventDefault();
    if (action === 'move') {
      setActive(next);
      // 落进折叠组 ⇒ 就地展开那一组。组头不在焦点链里（菜单一 Tab 就关），不展开的话折叠组里的
      // 选项对键盘用户是**不可达的死区** —— 折叠是给鼠标省滚动的，不该顺手把一批节点藏死。
      const gid = cselGroupIdAt(options, next);
      if (gid !== undefined && !openGroups.has(gid)) toggleGroup(gid);
    } else if (action === 'choose') choose(active);
  };

  // 宽度：以触发器宽为**下限**（minWidth），实际宽随内容自适应（width:auto）——固定 width=触发器宽时
  // CJK 选项标签（默认顺序/名称/延迟…）被 `.csel-opt>span` 的 ellipsis 截成「默./名./延.」（真机确认）。
  // maxWidth 封顶防超长选项撑爆；右溢出交给 computeCselPosition 的近边翻转（以触发器宽估算，够用）。
  const menuStyle: React.CSSProperties = {
    position: 'fixed',
    minWidth: state.rect?.width,
    maxWidth: 320,
    left: pos?.left ?? state.rect?.left ?? 0,
    top: pos?.top ?? (state.rect ? state.rect.bottom + CSEL_GAP : 0),
    visibility: pos ? 'visible' : 'hidden',
  };

  // 菜单节点。`position:fixed` 本应相对视口，但 `.win`/`.main` 用 `container-type:inline-size`（container
  // query 需要）——container-type 隐含 layout containment，会给 fixed 后代建**包含块** → 菜单坐标被容器
  // 左上角偏移（工具栏/Settings 下拉右漂 148px 并裁切，真机实测）。dialog 内的 Csel 在 `<dialog>` top-layer
  // 里逃逸 containment（ctx 存在），故仅在**非 dialog（!ctx）**时 portal 到 body 逃出容器；坐标即恢复视口相对。
  const menuNode = (
    <div ref={menuRef} className="csel-menu" role="listbox" style={menuStyle}>
      {rows.map((row) => {
        if (row.kind === 'header') {
          const gid = row.groupId;
          // 不可折叠的组（规则类型 15×5 等）：组头仍是纯视觉分隔，不占扁平索引、不可聚焦。
          if (gid === undefined) {
            return (
              <div key={row.key} className="csel-grp" role="presentation">
                {row.label}
              </div>
            );
          }
          // 可折叠组：组头变成展开钮。**不给 role** —— listbox 里没有「可展开的组头」这个角色，
          // 硬套 `option`/`presentation` 都是撒谎（前者会被读成一个可选项，后者对可点元素非法）；
          // 用原生 button 的隐含角色 + aria-expanded，与 `.mini-menu`/`.node-menu` 的 `.ns-grp` 同形。
          return (
            <button
              key={row.key}
              type="button"
              className={`csel-grp csel-grp-t${row.collapsed ? '' : ' open'}`}
              aria-expanded={!row.collapsed}
              onClick={(e) => {
                const header = e.currentTarget;
                toggleGroup(gid);
                scheduleReveal(row.collapsed ? () => revealSiblingGroup(header) : null);
              }}
            >
              <svg
                className="csel-grp-chev"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth={2}
              >
                <path d="M9 6l6 6-6 6" />
              </svg>
              <span>{row.label}</span>
              <span className="csel-grp-c">{row.count}</span>
            </button>
          );
        }
        return (
          <button
            key={`opt-${row.flatIndex}`}
            type="button"
            className={`csel-opt${row.opt.value === value ? ' on' : ''}${row.flatIndex === active ? ' active' : ''}${row.opt.disabled ? ' disabled' : ''}${row.opt.danger ? ' danger' : ''}`}
            role="option"
            aria-selected={row.opt.value === value}
            aria-disabled={row.opt.disabled || undefined}
            onClick={() => choose(row.flatIndex)}
            onMouseMove={() => setActive(row.flatIndex)}
          >
            {row.opt.icon}
            {/* `.csel-lbl`：有图标时 label 不再是 `:first-child`，prototype.css:1540 那条
                `.csel-opt > span:first-child`（flex:1 + 省略号）就命不中它 ⇒ 把 flex/截断钉在类上
                （规则在 styles/index.css 覆盖层）。无图标时两条同时命中，取值一致，行为不变。 */}
            <span className="csel-lbl">{row.opt.label}</span>
            <svg className="csel-ck" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2.4}>
              <path d="M5 12l5 5 9-11" />
            </svg>
          </button>
        );
      })}
    </div>
  );
  const menu = ctx ? menuNode : createPortal(menuNode, document.body);

  return (
    <div ref={wrapRef} className={`sel csel${state.open ? ' open' : ''}${className ? ` ${className}` : ''}`}>
      <button
        ref={triggerRef}
        type="button"
        disabled={disabled}
        id={id}
        className={`csel-trigger${currentDanger ? ' danger' : ''}`}
        aria-haspopup="listbox"
        aria-expanded={state.open}
        aria-label={ariaLabel}
        onClick={(e) => {
          e.preventDefault();
          if (state.open) closeMenu();
          else openMenu();
        }}
        onKeyDown={onTriggerKeyDown}
      >
        <span className="csv">{currentLabel}</span>
        <svg className="csel-chev" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M6 9l6 6 6-6" />
        </svg>
      </button>

      {state.open && menu}
    </div>
  );
}

export default Csel;
