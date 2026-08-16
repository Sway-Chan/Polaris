/**
 * TaildropDialog —— Tailscale 收件箱（sing-box 1.14.0-beta.15 起）。
 *
 * # 它治的是一个已经存在的黑箱
 *
 * 核从 beta.15 起**无条件**建收件目录并注册收件 handler（`protocol/tailscale/endpoint.go:253-263`），
 * 所以只要 tailnet 授了 `cap/file-sharing`，对端发来的文件早就在往盘上落。本弹窗之前，用户拥有的是
 * 一个看不见、也清不掉的收件箱 —— 这不是新功能，是补一个已经张开的口子。
 *
 * # 三态而不是「灰掉」
 *
 * 能不能用由 [`taildropAvailability`] 判三态，界面据此**说出原因**：核没跑 / tailnet 没授权 / 可用。
 * 尤其 `notGranted` 那一格——在本应用里怎么点都没用，得去 admin console 开——不说出来就是本仓
 * 反复记过的「拨了不生效的控件」（`ts-settings-logic.ts` 头注为 allowInternet、resolveByName 各记过一次）。
 *
 * # 拉取时机
 *
 * 打开时拉一次快照 + 标记已读；每次操作（取件 / 删除 / 取消）之后再拉一次。**不常驻订阅**：
 * 面板生命周期以分钟计，而角标要的三个计数本来就随 STATUS 事件流下发（见 `domain/taildrop.ts`）。
 * 接收中的进度因此不是逐字节刷新的 —— 面板上给一个显式的「刷新」按钮，不假装成实时。
 */

import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from '@/lib/error-handler';
import { api } from '@/ipc';
import { IpcError } from '@/ipc/ipc-client';
import type { TaildropInbox } from '@/contracts/taildrop';
import { fmtBytes } from '@/components/screens/shared/format';
import { relativeTimeText } from '@/lib/relative-time';
import { taildropAvailability, taildropErrorKey, receivingPercent } from '@/domain/taildrop';
import { useConfirmTwice } from '@/lib/confirm-twice';
import { cn } from '@/lib/utils';
import { useAppStore } from '@/store/app-store';
import { Modal } from './Modal';
import { useDialogStore } from './dialog-store';

const EMPTY: TaildropInbox = { files: [], receiving: [] };

/** 删除确认的站点前缀（`confirm-twice` 的 key）。实例后缀是文件名 —— 一行一个独立武装态。 */
const TAILDROP_DEL_PREFIX = 'taildrop-del:';

export function TaildropDialog({ serverId }: { serverId: string }) {
  const { t } = useTranslation();
  const close = useDialogStore((s) => s.close);
  // 删除走**原地二次点击**而不是嵌套 confirm 弹窗 —— 本仓破坏性操作的统一交互
  // （`lib/confirm-twice.ts` 是全仓唯一实现，`destructive-confirm-wiring.test.ts` 守着「别处不许再长一套」）。
  const { armed, confirmTwice } = useConfirmTwice();
  const status = useAppStore((s) => s.tailscaleStatuses[serverId]);
  const availability = taildropAvailability(status);

  const [inbox, setInbox] = useState<TaildropInbox>(EMPTY);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);

  /** 后端失败 → 按 code 查 i18n 键。**绝不把 `error` 里的英文诊断直接显示给用户**（那是给日志的）。 */
  const reportError = useCallback(
    (e: unknown) => {
      const code = e instanceof IpcError ? e.code : undefined;
      toast.error(t(taildropErrorKey(code)));
    },
    [t]
  );

  const refresh = useCallback(async () => {
    if (availability !== 'ready') return;
    setLoading(true);
    try {
      setInbox(await api.server.taildropList(serverId));
    } catch (e) {
      reportError(e);
    } finally {
      setLoading(false);
    }
  }, [availability, serverId, reportError]);

  useEffect(() => {
    void refresh();
    // 打开即清未读角标。失败**静默**：标记已读纯属体验，为它弹一个红条会盖住真正的内容。
    if (availability === 'ready') void api.server.taildropMarkRead(serverId).catch(() => {});
  }, [refresh, availability, serverId]);

  /** 包一层：操作期间禁用该行按钮，完成后重新拉一次快照（后端无推送，不拉就看不到结果）。 */
  const run = async (key: string, fn: () => Promise<void>) => {
    setBusy(key);
    try {
      await fn();
      await refresh();
    } catch (e) {
      reportError(e);
    } finally {
      setBusy(null);
    }
  };

  const onSave = (name: string) =>
    run(`save:${name}`, async () => {
      const r = await api.server.taildropSave(serverId, name);
      // 取消不是失败：用户按了保存框的取消，什么都不该提示。
      if (!r.canceled) toast.success(t('taildrop.saved'));
    });

  const delKey = (name: string) => `${TAILDROP_DEL_PREFIX}${name}`;
  const onDelete = (name: string) =>
    confirmTwice(`${TAILDROP_DEL_PREFIX}${name}`, () => {
      void run(delKey(name), () => api.server.taildropDelete(serverId, name));
    });

  const onCancelReceiving = (senderId: string, name: string) =>
    run(`cancel:${senderId}:${name}`, () => api.server.taildropCancel(serverId, senderId, name));

  const body = () => {
    if (availability === 'offline') return <p className="tdrop-empty">{t('taildrop.offline')}</p>;
    if (availability === 'notGranted')
      return <p className="tdrop-empty">{t('taildrop.notGranted')}</p>;
    if (inbox.files.length === 0 && inbox.receiving.length === 0)
      return <p className="tdrop-empty">{loading ? t('common.loading') : t('taildrop.empty')}</p>;

    return (
      <>
        {inbox.receiving.length > 0 && (
          <>
            <div className="field-lbl">{t('taildrop.receiving')}</div>
            <ul className="tdrop-list">
              {inbox.receiving.map((r) => {
                const key = `cancel:${r.senderID}:${r.name}`;
                return (
                  <li key={key} className="tdrop-row">
                    <div className="tdrop-main">
                      <span className="tdrop-name mono">{r.name}</span>
                      <span className="tdrop-meta">
                        {t('taildrop.fromSender', { sender: r.senderName })} ·{' '}
                        {receivingPercent(r.receivedBytes, r.size)}% ·{' '}
                        {fmtBytes(r.receivedBytes)} / {fmtBytes(r.size)}
                      </span>
                    </div>
                    <button
                      type="button"
                      className="btn ghost sm danger-text"
                      disabled={busy === key}
                      onClick={() => void onCancelReceiving(r.senderID, r.name)}
                    >
                      {t('taildrop.cancelReceiving')}
                    </button>
                  </li>
                );
              })}
            </ul>
          </>
        )}
        {inbox.files.length > 0 && (
          <>
            <div className="field-lbl">{t('taildrop.waiting')}</div>
            <ul className="tdrop-list">
              {inbox.files.map((f) => (
                <li key={f.name} className="tdrop-row">
                  <div className="tdrop-main">
                    <span className="tdrop-name mono">{f.name}</span>
                    <span className="tdrop-meta">
                      {t('taildrop.fromSender', { sender: f.senderName })} · {fmtBytes(f.size)} ·{' '}
                      {relativeTimeText(f.modifiedAt * 1000, t)}
                    </span>
                  </div>
                  <div className="tdrop-actions">
                    <button
                      type="button"
                      className="btn ghost sm"
                      disabled={busy === `save:${f.name}`}
                      onClick={() => void onSave(f.name)}
                    >
                      {t('common.save')}
                    </button>
                    <button
                      type="button"
                      className={cn('btn ghost sm danger-text', armed === delKey(f.name) && 'confirming')}
                      disabled={busy === delKey(f.name)}
                      onClick={() => onDelete(f.name)}
                    >
                      {armed === delKey(f.name) ? t('common.confirmAgain') : t('common.delete')}
                    </button>
                  </div>
                </li>
              ))}
            </ul>
          </>
        )}
      </>
    );
  };

  return (
    <Modal
      titleId="taildrop-dlg-title"
      title={t('taildrop.title')}
      onClose={close}
      icon={
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M12 3v12m0 0l-4-4m4 4l4-4M4 17v2a2 2 0 002 2h12a2 2 0 002-2v-2" />
        </svg>
      }
      footer={
        <>
          <button
            type="button"
            className="btn ghost"
            disabled={loading || availability !== 'ready'}
            onClick={() => void refresh()}
          >
            {t('common.refresh')}
          </button>
          <button type="button" className="btn" onClick={close}>
            {t('common.close')}
          </button>
        </>
      }
    >
      {body()}
    </Modal>
  );
}
