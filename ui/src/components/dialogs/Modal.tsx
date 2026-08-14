/**
 * Modal 原语 —— 原生 `<dialog>` + `showModal()`（§1.2 选型：三端 floor 实证过线）。
 *
 * 原生免费得到（替代原型 ~60 行手写 trapFocus + .fx-sentinel 哨兵，整套弃掉）：
 *  - top-layer 渲染（不吃 z-index 战争、不被 ancestor overflow 裁切）
 *  - showModal 使背景 inert（焦点圈禁 + 指针锁定）
 *  - ESC → `cancel` 事件
 *  - `::backdrop` 遮罩
 *
 * 自补（原生不给的）：
 *  - **scrim 点击关闭**：pointerdown+pointerup **均落 dialog 元素本身**（=backdrop 区，e.target===dialog）
 *    才关 —— 防文本拖选出界误关（§1.2）。
 *  - **显式初始聚焦**：showModal 后聚焦 dlg-body 首个可聚焦控件（不赌 autofocus 的 WebKit quirk）。
 *  - **关闭焦点还原**：保底记录 showModal 前 activeElement，卸载时还原（原型 :3823 语义）。
 *  - **ESC 优先级链**：csel 菜单打开时 ESC 只关菜单（cancel 里 preventDefault + 关 csel）；
 *    再 ESC → 关弹窗。csel 开关态经 ModalContext.cselCloseRef 传给本组件（见 Csel.tsx）。
 *    ⚠️ ESC 只在 `cancel` 事件里处理一次（不在 csel keydown 里再处理），避免 keydown+cancel 双关。
 *
 * 嵌套：每个入栈弹窗 = 一个本组件实例，mount 时 showModal（叠 top-layer 顶层），unmount 时 close。
 * DialogHost 按 store.stack 顺序渲染，天然映射叠放次序；ESC/scrim 只作用最顶层（原生只向顶层 dialog 派 cancel）。
 */

import {
  createContext,
  useEffect,
  useRef,
  type CSSProperties,
  type ReactNode,
  type MutableRefObject,
} from 'react';
import { useTranslation } from 'react-i18next';
import { useDialogTopLayerStore } from './dialog-top-layer';

/** Modal 向子树（Csel）暴露的协作上下文。 */
export interface ModalContextValue {
  /**
   * 当前打开的 csel 的关闭函数（null = 无 csel 打开）。
   * Csel 打开时写入、关闭时置 null；Modal 的 cancel/scrim 处理据此实现「先关菜单，再关弹窗」。
   */
  cselCloseRef: MutableRefObject<(() => void) | null>;
}

export const ModalContext = createContext<ModalContextValue | null>(null);

export interface ModalProps {
  /** 标题图标（dlg-ic 内 svg）。 */
  icon: ReactNode;
  /** 标题文案。 */
  title: string;
  /** aria-labelledby 目标 id（挂在标题 <b> 上）。 */
  titleId: string;
  /** 请求关闭（ESC / scrim / X）。由调用方决定 pop 语义（含脏态确认等）。 */
  onClose: () => void;
  /** footer 按钮组。 */
  footer: ReactNode;
  /** body 内容（自动包 .dlg-body）。 */
  children: ReactNode;
  /** dialog 额外类名。 */
  className?: string;
  /**
   * dialog 元素内联样式（原型个别弹窗按 id 覆写宽度，如 #wg-dialog/#res-catalog-dialog 的
   * `style="width:min(480px,100%)"` —— 原型本身就是内联样式而非 class，逐字对齐同法）。
   */
  style?: CSSProperties;
}

function focusFirst(dialog: HTMLDialogElement): void {
  const body = dialog.querySelector<HTMLElement>('.dlg-body');
  const scope = body ?? dialog;
  // 只自动聚焦「输入类」控件（input/textarea/显式 autofocus）——聚焦输入框显示焦点环是预期 UX。
  // 不再兜底聚焦首个普通按钮（真机：无输入的弹窗如 TS 登录/确认框会把首个 seg tab / dialog 自身
  // 聚焦成一圈醒目蓝框，用户明确反感）；无输入时聚焦 dialog 容器本身（CSS 已置 outline:none，无环），
  // 键盘用户仍可 Tab 到按钮。
  // **显式落点优先**：`[data-autofocus]` 是调用方点名要聚焦的那一个，必须先查、单独查。
  // 与下面的通用选择器写在同一条逗号列表里是不行的——`querySelector` 返回的是**文档序**首个匹配，
  // 不是"按选择器优先级"，故「订阅弹窗要求聚焦 URL 框」会被排在它前面的名称框吃掉
  // （真实症状：更多菜单里「编辑 URL」和「重命名」聚焦到同一个框，两项失去区分）。
  // 用 data-* 属性而非 React 的 `autoFocus`：后者由 React 在挂载时自行 .focus()、不落 DOM 属性，
  // 既查不到、又会被随后跑的本函数覆盖。
  const explicit = scope.querySelector<HTMLElement>('[data-autofocus]');
  const target =
    explicit ??
    scope.querySelector<HTMLElement>(
      '[autofocus], input:not([disabled]):not([type="hidden"]), textarea:not([disabled])',
    );
  (target ?? dialog).focus();
}

export function Modal({
  icon,
  title,
  titleId,
  onClose,
  footer,
  children,
  className,
  style,
}: ModalProps) {
  const { t } = useTranslation();
  const dialogRef = useRef<HTMLDialogElement>(null);
  const cselCloseRef = useRef<(() => void) | null>(null);
  // pointerdown 是否落在 backdrop（dialog 元素本身）—— 防拖选出界误关。
  const downOnBackdropRef = useRef(false);
  // onClose 用 ref 存最新值，避免进 effect 依赖数组导致 showModal 重跑。
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    // 记录打开前焦点（StrictMode 双跑：cleanup 已把焦点还原到 trigger，故重跑时再次捕获仍是 trigger）。
    const prevFocus = document.activeElement as HTMLElement | null;
    if (!dialog.open) dialog.showModal();
    focusFirst(dialog);
    // 登记进 top-layer 元素栈 —— toast 要挂到「最顶层弹窗」的子树里才不会被 ::backdrop 压住
    // （理由与实测见 ./dialog-top-layer.ts 模块头）。**必须在 showModal 之后**：登记的语义是
    // 「这个元素现在在 top-layer 里」，提前登记会让 toast 挂到一个还没提层的节点上。
    const { register, unregister } = useDialogTopLayerStore.getState();
    register(dialog);
    return () => {
      unregister(dialog);
      if (dialog.open) dialog.close();
      // 还原焦点（保底；多数实现原生会还原，spec 未强制）。
      if (prevFocus && document.contains(prevFocus)) prevFocus.focus();
    };
  }, []);

  return (
    <ModalContext.Provider value={{ cselCloseRef }}>
      <dialog
        ref={dialogRef}
        className={className ? `dlg ${className}` : 'dlg'}
        style={style}
        aria-labelledby={titleId}
        // ESC：原生派 cancel 到最顶层 dialog。csel 开 → 只关菜单；否则请求关弹窗。
        onCancel={(e) => {
          e.preventDefault();
          if (cselCloseRef.current) {
            cselCloseRef.current();
          } else {
            onCloseRef.current();
          }
        }}
        onPointerDown={(e) => {
          downOnBackdropRef.current = e.target === dialogRef.current;
        }}
        onPointerUp={(e) => {
          const upOnBackdrop = e.target === dialogRef.current;
          const bothOnBackdrop = downOnBackdropRef.current && upOnBackdrop;
          downOnBackdropRef.current = false;
          if (!bothOnBackdrop) return;
          // csel 开 → 该次点击交给 csel 的 outside-click 关菜单，弹窗不关（先关菜单，再关弹窗）。
          if (cselCloseRef.current) return;
          onCloseRef.current();
        }}
      >
        <div className="dlg-head">
          <span className="dlg-ic">{icon}</span>
          <b id={titleId}>{title}</b>
          <button
            type="button"
            className="dlg-x"
            aria-label={t('common.close')}
            onClick={() => onCloseRef.current()}
          >
            <svg viewBox="0 0 24 24" width="16" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M5 5l14 14M19 5L5 19" />
            </svg>
          </button>
        </div>
        <div className="dlg-body">{children}</div>
        <div className="dlg-foot">{footer}</div>
      </dialog>
    </ModalContext.Provider>
  );
}

export default Modal;
