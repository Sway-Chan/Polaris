/**
 * ConfirmDialog —— 通用确认弹窗（可复用基建：删除确认 / 放弃更改 / 危险操作二次确认）。
 *
 * 契约：**onConfirm 自行负责关闭**（见 dialog-store ConfirmPayload 注释）。本组件只在 Confirm 点击时
 * 调 onConfirm；Cancel/X/ESC/scrim → close()（pop 自身）**再调 onCancel**。这样嵌套栈下「关哪一层」
 * 由调用点显式决定，无「pop 顶层≠pop 目标」歧义。
 *
 * `onCancel` 让「用户离开了」成为调用点可观测的事件 —— 想把本弹窗包成 `Promise<boolean>` 的调用点
 * 靠它在 ESC/scrim 路径上落定，否则 promise 悬着。
 *
 * ⚠️ **破坏性操作不走本组件**：卸载 Polaris / 卸载提权助手 / 删节点 / 批删 / 清空日志 / 关闭全部连接
 * 一律用原地二次点击（`lib/confirm-twice.ts`，对齐原型 confirmTwice L3211），形态由
 * `lib/destructive-confirm-wiring.test.ts` 钉死。本组件服务的是「需重启」「放弃更改」「换模式前置忠告」
 * 这类需要成段解释的确认。
 */

import { useTranslation } from 'react-i18next';
import { Modal } from './Modal';
import { useDialogStore, type ConfirmPayload } from './dialog-store';

export function ConfirmDialog({ payload }: { payload: ConfirmPayload }) {
  const { t } = useTranslation();
  const close = useDialogStore((s) => s.close);
  /** 所有「不是确认」的离开路径统一走这里：先 pop 自身，再通知调用点。顺序不可颠倒 ——
   *  onCancel 里可能同步 resolve 出一个立刻再开弹窗的流程（双重确认的第二问就是这样）。 */
  const dismiss = () => {
    close();
    payload.onCancel?.();
  };

  return (
    <Modal
      titleId="confirm-dlg-title"
      title={payload.title}
      onClose={dismiss}
      icon={
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M12 9v4M12 17h.01M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0Z" />
        </svg>
      }
      footer={
        <>
          <button type="button" className="btn ghost" onClick={dismiss}>
            {payload.cancelLabel ?? t('common.cancel')}
          </button>
          <button
            type="button"
            className={`btn ${payload.danger ? 'danger' : 'flow'}`}
            onClick={() => {
              void payload.onConfirm();
            }}
          >
            {payload.confirmLabel ?? t('common.confirm')}
          </button>
        </>
      }
    >
      {/* `pre-line`：保留 message 里的换行、折叠多余空格。「设置需重启」这类必须交代前因后果的
          文案要分段才读得懂 —— 挤成一坨等于没写。
          ⚠️ **不是零影响**：既有调用点里 `SettingsUpdate.tsx:266-279`（三条更新前置忠告
          `settings.update.advisory.{macosGatekeeper,windowsSmartScreen,debElevation}.message`）
          在 en-US / zh-CN 里本来就含 `\n\n`，改前被折成一段、改后变两段 ⇒ 弹窗变高。
          多半是变好（文案本就按分段写的），但那是 mac / Win / deb 三条平台特定更新路径上的
          **未真机验证的视觉变更**，别把它当已验过。其余调用点确为单句。 */}
      <div className="card-sub" style={{ marginTop: -2, lineHeight: 1.6, whiteSpace: 'pre-line' }}>
        {payload.message}
      </div>
    </Modal>
  );
}

export default ConfirmDialog;
