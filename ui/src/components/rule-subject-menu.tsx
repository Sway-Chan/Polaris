/** 连接页与拓扑共用的“把观测对象写入规则”菜单项；追加写入只保留这一条执行腿。 */
import { useCallback, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { ruleTypeNameKey, type RuleSubject } from '@/domain/rules';
import { api } from '@/ipc';
import { toast } from '@/lib/error-handler';
import { editRoute } from '@/lib/staged-config';
import { useAppStore, useEffectiveRules } from '@/store/app-store';
import { useNavStore } from '@/store/nav-store';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { useDialogStore } from '@/components/dialogs/dialog-store';
import {
  analyzeRuleCoverage,
  appendSubjectToRule,
  type RuleAppendTarget,
} from '@/components/dialogs/rule-append';

function PlusIcon() {
  return (
    <svg viewBox="0 0 24 24" width="15" fill="none" stroke="currentColor" strokeWidth="1.8">
      <path d="M12 5v14M5 12h14" />
    </svg>
  );
}

function MergeIcon() {
  return (
    <svg viewBox="0 0 24 24" width="15" fill="none" stroke="currentColor" strokeWidth="1.8">
      <path d="M6 3v6a4 4 0 004 4h8M14 9l4 4-4 4" />
    </svg>
  );
}

export function RuleSubjectMenuItems({
  subject,
  onDone,
}: {
  subject: RuleSubject;
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const navigate = useNavStore((state) => state.navigate);
  const openDialog = useDialogStore((state) => state.open);
  const loadConfig = useAppStore((state) => state.loadConfig);
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((state) => state.stage);
  const rules = useEffectiveRules();

  const coverage = useMemo(() => analyzeRuleCoverage(rules, subject), [rules, subject]);
  const covering = useMemo(
    () => (coverage.firstId ? (rules.find((rule) => rule.id === coverage.firstId) ?? null) : null),
    [rules, coverage.firstId],
  );
  const coveringName = covering
    ? covering.remarks?.trim() || t(ruleTypeNameKey(covering.type))
    : '';

  /** 追加到已有规则 —— 全仓唯一的 `api.rules.update` 追加调用点。 */
  const append = useCallback(
    async (target: RuleAppendTarget) => {
      const base = rules.find((rule) => rule.id === target.ruleId) ?? null;
      const next = base ? appendSubjectToRule(base, target, subject) : null;
      if (!next) {
        if (target.block === 'contains') {
          toast.success(t('rules.subjectAlreadyInRule', { value: subject.value }));
        } else {
          toast.error(t('rules.appendFail'));
        }
        return;
      }
      const label = next.remarks?.trim() || t(ruleTypeNameKey(next.type));
      try {
        if (editRoute('customRules', stagingEnabled) === 'staged') {
          stage({
            id: `rule:${next.id}`,
            kind: 'rule',
            label: `${t('rules.editTitle')} ${label}`,
            entityPath: ['customRules', next.id],
            nextValue: next,
          });
        } else {
          await api.rules.update(next);
          void loadConfig(true);
        }
        toast.success(t('rules.appendDone', { value: subject.value, rule: label }));
      } catch {
        toast.error(t('rules.appendFail'));
      }
    },
    [rules, subject, t, stagingEnabled, stage, loadConfig],
  );

  const pickExisting = (
    <button
      key="pick"
      type="button"
      className="ctx-i"
      onClick={() => {
        onDone();
        openDialog({ kind: 'rule-pick', subject, onPick: (target) => void append(target) });
      }}
      data-tip={covering ? t('rules.subjectAlreadyInRule', { value: subject.value }) : undefined}
    >
      <MergeIcon />
      {t('rules.addExisting')}
      {covering && <span className="ctx-note">{coveringName}</span>}
    </button>
  );

  const createNew = (
    <button
      key="new"
      type="button"
      className="ctx-i"
      onClick={() => {
        onDone();
        navigate('rules');
        openDialog({ kind: 'rule', preset: { type: subject.type, value: subject.value } });
      }}
    >
      <PlusIcon />
      {t('rules.addNew')}
    </button>
  );

  return <>{covering ? [pickExisting, createNew] : [createNew, pickExisting]}</>;
}

export default RuleSubjectMenuItems;
