/**
 * SettingsBackup —— 备份子页（原型 [data-sec="backup"] L2326-2339）。
 *
 * 6 类按 Polaris backup-categories + 全选（indeterminate on partial）：
 *   manualNodes / meshNodes / subscriptions / customRules / appRules / generalSettings
 *
 * 导出走 backupApi.export(categories)；导入交给 BackupImportDialog（D5，`kind:'backup-import'`）——
 * 弹窗内部驱动 importPick → 逐类目预览勾选 → importApply，本屏「导入…」按钮只负责 open()。
 */

import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { UserConfig } from '@/contracts/types';
import {
  BACKUP_CATEGORIES,
  type BackupCategory,
} from '@/domain/backup-categories';
import { backupApi } from '@/ipc/api-client';
import { toast } from '@/lib/error-handler';
import { useDialogStore } from '@/components/dialogs/dialog-store';
import { Phead, SetBlock, SetRow, Switch, Button } from './primitives';

/**
 * 6 类标签 → i18n key（模块级只存 key、渲染时才 t：常量在 import 期求值，那时语言还没被
 * `syncLanguageChoice` 校正，直接存译文会钉死在首屏解析出的语言上）。
 *
 * 四类直接接既有的 `settings.advanced.backup.*`（文案逐字相同，不另造重复键）；
 * subscriptions / customRules 两类本页多说了「含什么」（展开节点 / 规则集资源），
 * 与旧键的「订阅源」「自定义规则」不是一句话，故单列在 `settings.backup.*`。
 */
const CATEGORY_LABEL_KEYS: Record<BackupCategory, string> = {
  manualNodes: 'settings.advanced.backup.manualNodes',
  meshNodes: 'settings.advanced.backup.meshNodes',
  subscriptions: 'settings.backup.catSubscriptions',
  customRules: 'settings.backup.catCustomRules',
  appRules: 'settings.advanced.backup.appRules',
  generalSettings: 'settings.advanced.backup.generalSettings',
};

export interface SettingsBackupProps {
  config: UserConfig;
}

export default function SettingsBackup({ config: _config }: SettingsBackupProps) {
  const { t } = useTranslation();
  const openDialog = useDialogStore((s) => s.open);
  const [selected, setSelected] = useState<Set<BackupCategory>>(
    () => new Set(BACKUP_CATEGORIES),
  );
  const [busy, setBusy] = useState(false);

  const allOn = selected.size === BACKUP_CATEGORIES.length;
  const someOn = selected.size > 0 && !allOn;

  const selectedArr = useMemo(() => Array.from(selected), [selected]);

  function toggleCat(cat: BackupCategory) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(cat)) next.delete(cat);
      else next.add(cat);
      return next;
    });
  }
  function toggleAll() {
    setSelected(allOn ? new Set() : new Set(BACKUP_CATEGORIES));
  }

  async function doExport() {
    setBusy(true);
    try {
      const res = await backupApi.export(selectedArr.length ? selectedArr : undefined);
      if (res.success) {
        // toast.success 门面为单串（无 description 形参，见 lib/error-handler.ts ToastImpl），
        // exportSuccessDesc 暂无展示位——仅传标题，与全库 success 调用口径一致。
        // 键从 `settings.backup.*` 改到 `settings.advanced.backup.*`：前者**五个 locale 里都不存在**，
        // 这两句一直落在 defaultValue 的中文上，en/ru/fa 用户看到的也是中文。后者文案逐字相同且五语齐全。
        toast.success(t('settings.advanced.backup.exportSuccess'));
      } else if (res.error !== 'cancelled') {
        // 用户主动取消保存对话框不算失败，不报；其余（写盘失败等）如实报出原因。
        toast.error(t('settings.advanced.backup.exportFail'), res.error);
      }
    } catch (err) {
      console.error('[SettingsBackup] export failed:', err);
      toast.error(
        t('settings.advanced.backup.exportFail'),
        err instanceof Error ? err.message : undefined,
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className="screen" data-sec="backup">
      <Phead title={t('settings.nav.backup')} sub={t('settings.backup.pageSub')} />

      <SetBlock id="backup-block">
        <SetRow
          label={t('settings.advanced.backup.selectAll')}
          style={{ borderBottom: '1px solid hsl(var(--line))' }}
        >
          <Switch
            id="backup-master"
            checked={allOn}
            indeterminate={someOn}
            onChange={toggleAll}
            aria-label="Select all"
          />
        </SetRow>

        {BACKUP_CATEGORIES.map((cat) => (
          <SetRow key={cat} label={t(CATEGORY_LABEL_KEYS[cat])}>
            <Switch
              className="backup-cat"
              checked={selected.has(cat)}
              onChange={() => toggleCat(cat)}
              aria-label={t(CATEGORY_LABEL_KEYS[cat])}
            />
          </SetRow>
        ))}

        <div style={{ display: 'flex', gap: 9, paddingTop: 14 }}>
          <Button
            variant="ghost"
            style={{ flex: 1, justifyContent: 'center' }}
            onClick={doExport}
            disabled={busy || selectedArr.length === 0}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M12 3v11M8 10l4 4 4-4M4 19h16" />
            </svg>
            <span>{t('settings.backup.exportSelected')}</span>
          </Button>
          <Button
            variant="flow"
            style={{ flex: 1, justifyContent: 'center' }}
            onClick={() => openDialog({ kind: 'backup-import' })}
            disabled={busy}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
              <path d="M12 15V4M8 8l4-4 4 4M4 19h16" />
            </svg>
            <span>{t('settings.backup.import')}</span>
          </Button>
        </div>
      </SetBlock>
    </section>
  );
}
