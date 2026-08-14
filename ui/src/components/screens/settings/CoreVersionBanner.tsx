/**
 * CoreVersionBanner —— sing-box 内核版本变更横幅（对齐 上游 `core-version-banner.tsx`）。
 *
 * 挂在「更新」页「3. sing-box 内核」卡片上方（上游 挂在 advanced-settings 顶部；本仓内核卡在更新页，
 * 这里是对应位置）。
 *
 * # 后端真实状态（2026-07-20 #14 接线批后；**本组件刻意不做假 UI**）
 *
 * 全链路已接线：
 *  - `core_get_version_info` 返回 `pendingChangeNotice` + **真实** `hasBackup`（读 `<core>.bak`）；
 *  - `core_update_ack_version_change` 清除它（show→ack，弹一次而非每次启动都弹）；
 *  - `EVENT_CORE_VERSION_CHANGED` **已有发射点**（`swap_core_with_restart` 换核成功即发），
 *    `pendingChangeNotice` 亦由同一处写入 ⇒ 本横幅现在会真实触发；
 *  - `core_rollback` / `core_replace_manual` / `core_reset_factory` 均为真实现，**零提权**
 *    （落位于 `<config_dir>/core_update/`，用户可写）。
 *
 * ⚠️ 上一版本模块文档称「其余全是桩」「`hasBackup` 硬编码 false」「零发射点」——**均已过时**，
 * 本批一并订正（该文件所在的更新域有注释滞后于实现的既往史）。
 *
 * 判定逻辑抽在 `settings-logic.ts::coreBannerState`，由单测锁死。
 */

import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { coreUpdateApi } from '@/ipc/api-client';
import { Button } from './primitives';
import {
  coreBannerState,
  type CoreVersionInfoLike,
  type CoreVersionChangedPayload,
} from './settings-logic';

export default function CoreVersionBanner() {
  const { t } = useTranslation();
  const [versionInfo, setVersionInfo] = useState<CoreVersionInfoLike | null>(null);
  const [eventPayload, setEventPayload] = useState<CoreVersionChangedPayload | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const [replacing, setReplacing] = useState(false);
  const [replaceErr, setReplaceErr] = useState('');

  useEffect(() => {
    let cancelled = false;

    // 1. 挂载即拉一次持久快照——事件可能早于本组件挂载就发过了（主进程启动期），只靠订阅会漏。
    void coreUpdateApi
      .getVersionInfo()
      .then((info) => {
        if (cancelled) return;
        setVersionInfo(info);
        // 展示即 ack：清掉后端持久通知，使横幅弹一次而非每次启动都弹。
        if (info.pendingChangeNotice) void coreUpdateApi.ackVersionChange().catch(() => undefined);
      })
      .catch(() => undefined);

    // 2. 订阅即时推送。收到即复位 dismissed（新的版本变更应重新可见），并同样 ack 防重启后复现。
    const off = coreUpdateApi.onVersionChanged((data) => {
      setEventPayload(data);
      setDismissed(false);
      void coreUpdateApi.ackVersionChange().catch(() => undefined);
    });

    return () => {
      cancelled = true;
      off();
    };
  }, []);

  const state = coreBannerState({ versionInfo, eventPayload, dismissed });
  if (!state.visible || !state.notice) return null;

  const { previousVersion, currentVersion } = state.notice;

  return (
    <div className="core-ver-banner" id="core-ver-banner" role="status">
      <span className="cvb-ic" aria-hidden="true">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z" />
          <path d="M12 9v4M12 17h.01" />
        </svg>
      </span>
      <div className="cvb-tx">
        <b>{t('settings.coreVersion.changedTitle')}</b>
        <span>
          {t(`settings.coreVersion.${state.descKey}`, { previousVersion, currentVersion })}
        </span>
        {/* 无备份可回滚的诚实说明：回滚按钮按 上游 逻辑仅在有备份时渲染，若不解释，用户会以为
            「有回滚功能但找不到入口」。这行只在确实无备份时出现（如首次播种后尚未换过核）。 */}
        {!state.showRollback && (
          <span className="cvb-note">
            {t('settings.coreVersion.noRollbackEntryNote')}
          </span>
        )}
        {replaceErr && <span className="cvb-note">{replaceErr}</span>}
      </div>
      {/* 手动换核：`core_replace_manual` 已接线（零提权）。无参调用 → 后端弹系统文件选择器；
          需二次确认时返 `{needConfirm}`，由「更新」页的同名入口承载完整确认流程，
          此处横幅只做快捷入口，失败信息经 `replaceFailed` 行内回显。 */}
      <Button
        variant="ghost"
        size="sm"
        disabled={state.manualReplaceDisabled || replacing}
        onClick={() => {
          setReplacing(true);
          setReplaceErr('');
          void coreUpdateApi
            .replaceManual()
            .then((r) => {
              if (!r.ok && !('cancelled' in r && r.cancelled)) {
                setReplaceErr(
                  ('error' in r && r.error) ||
                    t('settings.core.swapFailedShort'),
                );
              }
            })
            .catch((e: unknown) =>
              setReplaceErr(e instanceof Error ? e.message : String(e)),
            )
            .finally(() => setReplacing(false));
        }}
      >
        <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M4 20h16M12 4v10M8 8l4-4 4 4" />
        </svg>
        {/* 文案与本页内核卡里的同名按钮**逐字一致**（同一页面、同一动作理应同一说法）。
            走 `settings.coreManagement.manualSwap`——本批新增，专为这句话立键，两处共用同一个键即
            结构上保证不再分叉。仍不用既有的 `manualReplace`：它的中文是「手动替换」，同屏两处说法
            不同会让用户以为是两个功能。 */}
        <span>{t('settings.coreManagement.manualSwap')}</span>
      </Button>
      <Button
        variant="ghost"
        size="sm"
        className="cvb-dismiss"
        onClick={() => setDismissed(true)}
        data-tip={t('settings.coreVersion.dismiss')}
        aria-label={t('settings.coreVersion.dismiss')}
      >
        <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M18 6 6 18M6 6l12 12" />
        </svg>
      </Button>
    </div>
  );
}
