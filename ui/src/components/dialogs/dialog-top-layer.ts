/**
 * dialog-top-layer —— 当前打开中的 `<dialog>` 元素栈，供 toast 找到「最顶层弹窗」作挂载点。
 *
 * # 为什么需要它（根因，不是偏好）
 *
 * `showModal()` 把 `<dialog>` 提到 **top-layer**（Modal.tsx:110）。按 CSS 规范，top-layer 元素
 * **及其 `::backdrop`** 渲染在所有普通内容之上，**且 z-index 对它们无效**。而 `#toast-stack` 挂在
 * `.win` 内（AppShell.tsx）= 普通流 ⇒ **弹窗一开，toast 必被 `::backdrop` 压住**。
 *
 * 这不是推理，是实测（2026-07-30，WebKitGTK 4.1 + Chromium 双引擎）：toast 停在 `.win` 里时
 * `document.elementFromPoint(toast 中心)` 返回的是 `<dialog>`（即它的 `::backdrop`），不是 toast；
 * Chromium 截图上那 2 px 采样点是 `rgb(130,5,135)` —— 纯品红 `rgb(255,0,255)` 被 50% 遮罩压暗后的值。
 * 把 `z-index` 从 400 提到任意值都不改变结果。
 *
 * # 解法：与 csel 菜单同源
 *
 * 同一个坑 csel 菜单已经解过一次 —— `design/polaris-dialog-layer-and-governance.md:12`：
 * 「**csel 菜单必须挂 dialog 子树内**（全局单例菜单在 top-layer 下不可见不可点）」。toast 照办：
 * **有弹窗时 portal 进最顶层 dialog 的子树，无弹窗时仍挂 `.win`**。
 *
 * **不走 Popover API**：macOS floor 13.0 = 出厂 WebKit 16.0，Popover 要 Safari 17
 * （同文档 :51 已实证并裁定，不翻案）。
 *
 * # 为什么是一份 DOM 元素栈，而不是复用 `dialog-store.stack`
 *
 * `dialog-store` 持有的是**描述符**（`DialogDesc`），拿不到 DOM 节点，而 portal 需要的是节点本身。
 * 描述符栈与元素栈天然同序（`DialogHost` 按 `stack` 顺序渲染，每个入栈弹窗一个 `Modal` 实例，
 * 挂载 effect 按渲染序触发），但**不能靠它推**：`ConfirmDialog` 等由回调驱动的路径下，描述符入栈与
 * `showModal()` 之间隔着一次渲染，用描述符去猜元素会拿到还没挂上的那个。故由 `Modal` 自己在
 * `showModal()` 之后登记真实元素 —— 登记的是「已经在 top-layer 里的那个」，无时序缺口。
 *
 * 嵌套（proc-pick 从规则弹窗内打开）时后挂载者在栈尾 = 最顶层，与原生 top-layer 叠放次序一致。
 */

import { create } from 'zustand';

interface DialogTopLayerStore {
  /** 打开中的 `<dialog>`，按 `showModal()` 顺序（**末尾 = 最顶层**）。 */
  els: HTMLDialogElement[];
  /** Modal 挂载并 showModal 后登记自身。 */
  register: (el: HTMLDialogElement) => void;
  /** Modal 卸载时注销（与 register 严格成对，StrictMode 双跑下也守恒）。 */
  unregister: (el: HTMLDialogElement) => void;
}

export const useDialogTopLayerStore = create<DialogTopLayerStore>((set) => ({
  els: [],
  register: (el) => set((s) => (s.els.includes(el) ? s : { els: [...s.els, el] })),
  unregister: (el) => set((s) => ({ els: s.els.filter((x) => x !== el) })),
}));

/**
 * 最顶层的打开中弹窗；无弹窗时 `null`。
 *
 * 订阅式（非一次性读取）是必需的：toast 存活 2.4s，期间弹窗可能关闭（提交成功即关窗 + 成功 toast）。
 * 若挂载点是快照，弹窗一关那个 DOM 节点就被移除，React portal 的容器随之失效 ⇒ **toast 当场消失**。
 * 订阅让挂载点跟着栈变化迁回 `.win`，toast 完整播完它的 2.2s。
 */
export function useTopDialogEl(): HTMLDialogElement | null {
  return useDialogTopLayerStore((s) => s.els[s.els.length - 1] ?? null);
}
