/**
 * dialog-store —— 弹窗栈（Zustand，与 app-store 同模式：create<T>((set,get)=>...)）。
 *
 * 设计（§1.3）：
 *  - **单 store + 单 DialogHost + discriminated union**：一处枚举全部弹窗，类型安全 payload。
 *    [选 单 store] 存在跨屏触发（拓扑右键「为其加规则」跳规则屏开弹窗）与嵌套回填（proc-pick），
 *    局部 useState 需 prop-drilling / 事件总线，12 弹窗分散必漂移。
 *    [不选 每屏 useState]（跨屏做不干净）[不选 路由驱动]（Polaris 无 router，为弹窗引入路由属过度设计）。
 *  - **栈式**：open=push，close=pop 顶层，closeAll=清空。原生 dialog 按 showModal 顺序叠 top-layer，
 *    ESC 关最顶层，store 栈镜像之（嵌套语义，§1.3）。
 *
 * DialogDesc 已全量落位（res-url/confirm 地基 + D2–D5 的 node/import/sub/warp/ts/wg/rule/proc-pick/
 * app-add/res-catalog/backup-import），各 kind 的组件文件由对应批填充。
 * DialogHost 的 switch 用 never 兜底保证新增 kind 时 TS 强制补 case（穷尽性）。
 */

import { create } from 'zustand';
import type { SystemProcessInfo } from '@/contracts/types';
import type { RulePreset, RuleSubject } from '@/domain/rules';
import type { RuleAppendTarget } from './rule-append';

/** 通用确认弹窗载荷（可复用基建：删除确认 / 放弃更改 / 危险操作二次确认）。 */
export interface ConfirmPayload {
  title: string;
  message: string;
  /** 确认按钮文案（默认走 i18n common.confirm） */
  confirmLabel?: string;
  /** 取消按钮文案（默认走 i18n common.cancel） */
  cancelLabel?: string;
  /** 危险语气（确认按钮红色，用于删除/清空等破坏性操作） */
  danger?: boolean;
  /**
   * 确认回调。**由回调自行负责关闭**（close() 关自身 / closeAll() 连底层一并关）——
   * 不自动 pop，避免「弹窗内嵌栈」下 pop 顶层≠pop 目标的歧义。
   */
  onConfirm: () => void | Promise<void>;
  /**
   * 取消/关闭回调（Cancel 按钮 / X / ESC / scrim 任一路径）。**由 `ConfirmDialog` 在 `close()` 之后调**，
   * 故回调内不要再关一次。
   *
   * 存在的理由：把确认框包成 `Promise<boolean>` 的调用点必须在**所有**离开路径上落定，
   * 否则用户按 ESC 之后那个 promise 永不 settle。
   * 只挂 onConfirm 的话，「取消」与「弹窗还开着」在调用点看来完全一样。
   *
   * ⚠️ `closeAll()` 绕过本回调（它直接清栈、不经组件）—— 那种情形下 promise 不落定，
   * 破坏性动作**不会执行**（fail-closed，方向安全）。
   */
  onCancel?: () => void;
}

/**
 * 弹窗描述符（discriminated union）。§1.3 目标 union 全量落位（D1 地基 + D2–D5 各批填组件）。
 * 契约在此冻结：DialogHost 的 case 与各 *Dialog 组件的 props 签名由本 union 单点决定；
 * 各批只覆写自己那一个组件文件（不改本文件、不改 DialogHost），故 union/host 无并行写冲突。
 * payload 形态取自 design/polaris-dialog-layer-and-governance.md §1.3（proc-pick 用 onPick 回调，同 ConfirmPayload.onConfirm 先例）。
 */
export type DialogDesc =
  | { kind: 'res-url' }
  | { kind: 'confirm'; payload: ConfirmPayload }
  // D2：节点表单 + 手动导入
  | { kind: 'node'; serverId?: string; initialProto?: 'openconnect' | 'openvpn-client' } // initialProto 仅用于组网隧道入口预选
  | { kind: 'import' } // 手动导入（粘贴/文件），恒新增
  // D3：订阅 + 组网接入选择器
  // focus：订阅「更多」菜单的「重命名」/「编辑 URL」两项落到同一弹窗的不同字段（原型 subMenu 是两项，
  // 但 Polaris 只有一个订阅表单）——用 autoFocus 落点区分，而不是摆两个点了完全一样的菜单项。
  | { kind: 'sub'; subId?: string; focus?: 'name' | 'url'; onAdded?: (subId: string) => void }
  | { kind: 'warp'; edit?: boolean } // WARP 单例槽，无 serverId（弹窗自查现有节点）
  | { kind: 'ts-login' }
  | { kind: 'ts-settings' }
  | { kind: 'wg'; serverId?: string } // WG 可多实例，携 id
  | {
      kind: 'mesh-join';
      onTsLogout: (node: import('@/contracts/types').ServerConfig) => void;
      onWarpReregister: (node: import('@/contracts/types').ServerConfig) => void;
      onWarpDeregister: (node: import('@/contracts/types').ServerConfig) => void;
    }
  // D4：规则编辑 + 进程选择器
  | { kind: 'rule'; ruleId?: string; preset?: RulePreset }
  | { kind: 'proc-pick'; onPick: (procs: SystemProcessInfo[]) => void } // 批量选中回调（原型是多选+底部「添加 N 项」批量提交，非单击即选；调用方闭包写回自身 useState，同 ConfirmPayload.onConfirm 先例）
  // 「加入已有规则…」的目标选择器：单击即选（一次点击写一条规则，没有批量语义），回调同 proc-pick
  // 先例 —— 写入腿在唯一调用方 `components/rule-subject-menu.tsx` 里，弹窗本身只负责选。
  | { kind: 'rule-pick'; subject: RuleSubject; onPick: (target: RuleAppendTarget) => void }
  // D5：自定义应用 + 资源库 + 备份导入
  | { kind: 'app-add' } // 仅新增（自定义应用无编辑态）
  | { kind: 'res-catalog' }
  | { kind: 'backup-import' };

export type DialogKind = DialogDesc['kind'];

interface DialogStore {
  /** 栈式：镜像原生 dialog 的 top-layer 叠放顺序（末尾 = 最顶层）。 */
  stack: DialogDesc[];
  /** push 一个弹窗（叠到栈顶）。 */
  open: (desc: DialogDesc) => void;
  /** pop 顶层弹窗。 */
  close: () => void;
  /** 清空全部弹窗。 */
  closeAll: () => void;
}

export const useDialogStore = create<DialogStore>((set) => ({
  stack: [],
  open: (desc) => set((s) => ({ stack: [...s.stack, desc] })),
  close: () => set((s) => ({ stack: s.stack.slice(0, -1) })),
  closeAll: () => set({ stack: [] }),
}));
