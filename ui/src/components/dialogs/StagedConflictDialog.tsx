/**
 * 保存冲突弹窗（暂存层 spec §2.5 Q8-b 第 4 步）。
 *
 * # 什么时候出现
 *
 * 只在**两个人类写者同时改了同一个实体**时 —— 用户在 UI 上暂存了对某实体的编辑，而同一实体
 * 在磁盘侧也被改过（CLI / 编辑器手改 config.json，或另一个窗口）。后台写盘者（订阅调度器写
 * `etag`、规则资源调度器写 `updatedAt`、后端权威字段）与用户能编辑的实体天然不重叠 ⇒ 恒落
 * 「无重叠 → 静默合并」腿，不到这里。托盘的三件事全走直接落盘、不产生 staged ⇒ 它只作为
 * 「被合并方」出现，也不到这里。**实践中本弹窗接近于零**，但通路必须在。
 *
 * # 为什么是逐条选而不是「全用我的 / 全用磁盘的」两颗按钮
 *
 * 冲突条目彼此独立（改的是不同实体），一刀切会逼用户为了保住 A 而连带覆盖 B。默认全选「用我的」
 * ——用户此刻的心智是「我要保存我刚才改的东西」，让默认值与那个心智一致，逐条改的只是例外。
 */

import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import { Modal } from './Modal';
import type { StagedConflict } from '@/store/staged-config-store';

export interface StagedConflictDialogProps {
  conflicts: readonly StagedConflict[];
  /** 用户确认：传回选了「用我的」的条目 id 集合（未列入的 = 用磁盘的，条目会被丢弃）。 */
  onResolve: (keepEntryIds: string[]) => void;
  /** 关掉、不保存（ESC / scrim / X / 取消）。staged 一条不丢。 */
  onDismiss: () => void;
}

type Side = 'mine' | 'disk';

export function StagedConflictDialog({
  conflicts,
  onResolve,
  onDismiss,
}: StagedConflictDialogProps) {
  const { t } = useTranslation();
  // 默认「用我的」：见文件头。缺 key = 未被改过 = 仍是默认。
  const [choice, setChoice] = useState<Record<string, Side>>({});
  const sideOf = (id: string): Side => choice[id] ?? 'mine';

  return (
    <Modal
      titleId="staged-conflict-title"
      title={t('home.stagedConflictTitle')}
      onClose={onDismiss}
      icon={
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
          <path d="M12 9v4M12 17h.01M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0Z" />
        </svg>
      }
      footer={
        <>
          <button type="button" className="btn ghost" onClick={onDismiss}>
            {t('common.cancel')}
          </button>
          <button
            type="button"
            className="btn flow"
            onClick={() =>
              onResolve(conflicts.filter((c) => sideOf(c.entryId) === 'mine').map((c) => c.entryId))
            }
          >
            {t('home.stagedConflictSave')}
          </button>
        </>
      }
    >
      <div className="card-sub" style={{ marginTop: -2, lineHeight: 1.6 }}>
        {t('home.stagedConflictDesc')}
      </div>
      {conflicts.map((c) => (
        <div className="fld" key={c.entryId}>
          <label className="fld-l">{c.label}</label>
          <div className="seg2" role="group" aria-label={c.label} style={{ display: 'flex' }}>
            {(['mine', 'disk'] as const).map((side) => (
              <button
                key={side}
                type="button"
                style={{ flex: 1 }}
                className={sideOf(c.entryId) === side ? 'on' : ''}
                aria-pressed={sideOf(c.entryId) === side}
                onClick={() => setChoice((prev) => ({ ...prev, [c.entryId]: side }))}
              >
                {t(side === 'mine' ? 'home.stagedConflictMine' : 'home.stagedConflictDisk')}
              </button>
            ))}
          </div>
          {/* 两侧值逐字列出。**不做「只显示差异字段」**：实体粒度是 U-4 拍板的语义，
              只给差异字段会让用户以为合并是按字段做的，而它不是。
              磁盘侧实体已被删（`disk === null`）时说「已删除」而不是显示空串 —— 那是两回事。 */}
          <textarea
            className="input mono"
            rows={3}
            readOnly
            value={`${t('home.stagedConflictMine')}: ${c.mine}\n${t('home.stagedConflictDisk')}: ${
              c.disk ?? t('home.stagedConflictDiskDeleted')
            }`}
          />
        </div>
      ))}
    </Modal>
  );
}

export default StagedConflictDialog;
