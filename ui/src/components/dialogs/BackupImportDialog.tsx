/**
 * BackupImportDialog —— 备份导入预览弹窗（原型 #import-dialog :2748-2773）。
 *
 * UX 升级，非新增流程：`SettingsBackup.tsx` 原 `doImport()` 盲恢复全部类别（importPick 后直接拿
 * `available` 整份 importApply，无选择、无预览）。本弹窗插入一步：importPick 拿到的 `available` + `counts`
 * 先渲染成逐类目勾选预览，用户确认所选类别后才 importApply——两步都是**真后端**（backupApi.importPick /
 * importApply），无 stub 需要降级。
 *
 * 由 SettingsBackup「导入…」按钮 `open({kind:'backup-import'})` 触发（替换原直调 doImport）。
 */

import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from '@/lib/error-handler';
import { api } from '@/ipc';
import { BACKUP_CATEGORIES, type BackupCategory } from '@/domain/backup-categories';
import { cn } from '@/lib/utils';
import { Modal } from './Modal';
import { useDialogStore } from './dialog-store';

const CATEGORY_LABELS: Record<BackupCategory, string> = {
  manualNodes: '手动节点',
  meshNodes: '组网节点',
  subscriptions: '订阅（含展开节点）',
  customRules: '自定义规则（含规则集资源）',
  appRules: '应用分流',
  generalSettings: '通用设置',
};

interface Picked {
  filePath: string;
  available: BackupCategory[];
  counts: Partial<Record<BackupCategory, number>>;
}

function ImportIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M12 15V4M8 8l4-4 4 4M4 19h16" />
    </svg>
  );
}

export function BackupImportDialog() {
  const { t } = useTranslation();
  const open = useDialogStore((s) => s.open);
  const close = useDialogStore((s) => s.close);

  const [picked, setPicked] = useState<Picked | null>(null);
  const [selected, setSelected] = useState<Set<BackupCategory>>(new Set());
  const [busy, setBusy] = useState(false);

  const doPick = async () => {
    setBusy(true);
    try {
      const r = await api.backup.importPick();
      if (r.canceled) return;
      if (!r.filePath || !r.available) {
        toast.error(t('backupImport.errParse', '未能解析备份文件'), r.error ?? undefined);
        return;
      }
      setPicked({ filePath: r.filePath, available: r.available, counts: r.counts ?? {} });
      setSelected(new Set(r.available));
    } catch (e) {
      toast.error(t('backupImport.errParse', '未能解析备份文件'), e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const toggle = (cat: BackupCategory) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(cat)) next.delete(cat);
      else next.add(cat);
      return next;
    });
  };

  const requestClose = () => {
    if (picked) {
      open({
        kind: 'confirm',
        payload: {
          title: t('backupImport.discardTitle', '放弃导入？'),
          message: t('backupImport.discardMsg', '已选择的备份文件与类别将不会恢复。'),
          confirmLabel: t('node.discard', '放弃'),
          danger: true,
          onConfirm: () => {
            close();
            close();
          },
        },
      });
    } else {
      close();
    }
  };

  const handleApply = async () => {
    if (!picked || selected.size === 0) return;
    setBusy(true);
    try {
      const r = await api.backup.importApply(picked.filePath, [...selected]);
      if (!r.success) {
        toast.error(t('backupImport.errApply', '恢复失败'), r.error ?? undefined);
        return;
      }
      close();
    } catch (e) {
      toast.error(t('backupImport.errApply', '恢复失败'), e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const fileName = picked ? picked.filePath.split(/[\\/]/).pop() ?? picked.filePath : '';

  return (
    <Modal
      titleId="import-dlg-title"
      title={t('backupImport.title', '导入备份')}
      onClose={requestClose}
      icon={<ImportIcon />}
      footer={
        <>
          <button type="button" className="btn ghost" onClick={requestClose}>
            {t('common.cancel', '取消')}
          </button>
          <button
            type="button"
            className="btn flow"
            onClick={() => void handleApply()}
            disabled={!picked || selected.size === 0 || busy}
          >
            {t('backupImport.restoreSelected', '恢复所选')}
          </button>
        </>
      }
    >
      {!picked ? (
        <div
          className="dz"
          role="button"
          tabIndex={0}
          onClick={() => void doPick()}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              void doPick();
            }
          }}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
            <path d="M12 15V4M8 8l4-4 4 4" />
            <path d="M4 15v3a2 2 0 002 2h12a2 2 0 002-2v-3" />
          </svg>
          <div>{t('backupImport.pickHint', '点击选择备份文件')}</div>
          <div style={{ fontSize: 10.5, marginTop: 4 }}>polaris-backup.json</div>
        </div>
      ) : (
        <>
          <div className="card-sub">
            {t('backupImport.fileLabel', { defaultValue: '文件：{{name}}', name: fileName })}
          </div>
          <div className="fld">
            <label className="fld-l">{t('backupImport.categories', '文件内的类别')}</label>
            <div className="parse-list" style={{ maxHeight: 'none' }}>
              {BACKUP_CATEGORIES.filter((cat) => picked.available.includes(cat)).map((cat) => (
                <label
                  key={cat}
                  className="pl-row"
                  style={{ justifyContent: 'space-between', fontFamily: 'var(--sans)' }}
                >
                  <span style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                    <span
                      className={cn('swt', selected.has(cat) && 'on')}
                      role="switch"
                      aria-checked={selected.has(cat)}
                      tabIndex={0}
                      onClick={() => toggle(cat)}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter' || e.key === ' ') {
                          e.preventDefault();
                          toggle(cat);
                        }
                      }}
                    />
                    <span>{CATEGORY_LABELS[cat]}</span>
                  </span>
                  <span className="mono" style={{ color: 'hsl(var(--fg-faint))' }}>
                    {picked.counts[cat] ?? '—'}
                  </span>
                </label>
              ))}
            </div>
          </div>
          <div
            className="rules-note"
            style={{
              margin: 0,
              borderColor: 'hsl(var(--warn)/0.3)',
              background: 'hsl(var(--warn-weak)/0.5)',
              color: 'hsl(var(--warn))',
            }}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} style={{ width: 15 }}>
              <circle cx="12" cy="12" r="9" />
              <path d="M12 8v5M12 16h.01" />
            </svg>
            <span>
              {t(
                'backupImport.replaceWarn',
                '选中类别将被备份内容整体替换；未选类别保留不变。',
              )}
            </span>
          </div>
        </>
      )}
    </Modal>
  );
}

export default BackupImportDialog;
