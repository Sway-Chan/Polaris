/**
 * RulePickDialog —— 「把这个域名加进**哪一条**已有规则」的选择器。
 *
 * 形态复用 `ProcPickDialog` 的 `onPick` 回调范式（`dialog-store.ts` 已把它登记为「同
 * `ConfirmPayload.onConfirm` 先例」，不是新造机制），皮肤复用 `.proc-pick-*` 那一套（零新增布局）。
 * 与它的差别只有一处：**单击即选**而不是多选批量提交 —— 一次点击写一条规则，没有「凑够几条再提交」
 * 的语义，摆一个底部「添加 N 项」是给一个不存在的批量动作造按钮。
 *
 * 本组件**只负责选**，写入在唯一的调用方 `components/host-rule-menu.tsx` 里（两个菜单共用那一条腿）。
 * 判据全部在 `rule-append.ts`（node 环境可直测），本文件只做渲染与检索。
 *
 * # 列**全部**规则，不能追加的置灰并说明原因
 *
 * 之前只列「已含域名族条件」的规则 ⇒ 用户的规则若多是 `ruleSet` / `geosite` / `processName`，
 * 这里一条候选都没有，只剩一句说明文字 —— 用户看到的是「这个功能坏了」，而不是「我的规则不合适」。
 * 现在每条规则至少一行：能追加的可点，不能的置灰并在第二行**逐条给原因与出路**（原因分类与判据
 * 在 `rule-append.ts` 的 `AppendBlock`）。置灰项同样参与检索 —— 搜得到规则名却搜不到规则，
 * 用户会以为规则不存在。
 *
 * 一条规则有多个域名族条件 ⇒ 列成多项，每项显式标出**条件类型**：往 `domainKeyword` 里追加一个
 * 完整主机名是**更窄**的匹配（子串语义），不标类型用户不知道自己拿到了什么。
 */

import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useEffectiveRules } from '@/store/app-store';
import { RULE_TYPES, ruleTypeNameKey } from '@/domain/rules';
import { Modal } from './Modal';
import { useDialogStore } from './dialog-store';
import {
  analyzeDomainCoverage,
  isShadowedTarget,
  matchAppendTargets,
  ruleAppendTargets,
  sortAppendTargets,
  type RuleAppendTarget,
} from './rule-append';

function RuleIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8}>
      <path d="M12 20h9M16.5 3.5a2.1 2.1 0 013 3L7 19l-4 1 1-4z" />
    </svg>
  );
}

export function RulePickDialog({
  domain,
  onPick,
}: {
  domain: string;
  onPick: (target: RuleAppendTarget) => void;
}) {
  const { t } = useTranslation();
  const close = useDialogStore((s) => s.close);
  // 展示面取 effective：暂存中新建/编辑过的规则也必须能当追加目标，否则「刚加的规则挑不到」。
  const rules = useEffectiveRules();
  const [query, setQuery] = useState('');

  const targets = useMemo(() => sortAppendTargets(ruleAppendTargets(rules, domain)), [rules, domain]);
  const coverage = useMemo(() => analyzeDomainCoverage(rules, domain), [rules, domain]);
  const shown = useMemo(() => matchAppendTargets(targets, query), [targets, query]);

  const pick = (target: RuleAppendTarget) => {
    if (target.block !== null) return;
    onPick(target);
    close(); // pop 本弹窗（同 ProcPickDialog）
  };

  /** 置灰行第二行的原因文案 —— 每条都带出路，不用笼统的「不可追加」。 */
  const whyText = (target: RuleAppendTarget): string | null => {
    switch (target.block) {
      case 'andMode':
        return t('rules.pickWhyAnd', {
          defaultValue:
            '该规则要求「全部条件都命中」，为它新增域名条件会变成求交而不是扩宽 —— 请在规则弹窗里显式编辑',
        });
      case 'valueUnfit':
        return t('rules.pickWhyUnfit', {
          defaultValue: '{{domain}} 不是合法的域名形状（例如 IPv6 地址），进不了域名条件',
          domain,
        });
      default:
        return null;
    }
  };

  return (
    <Modal
      titleId="rule-pick-title"
      title={t('rules.pickTitle', '加入已有规则')}
      onClose={close}
      icon={<RuleIcon />}
      footer={
        <button type="button" className="btn ghost" onClick={close}>
          {t('common.cancel', '取消')}
        </button>
      }
    >
      <div className="card-sub" style={{ marginTop: -4 }}>
        {t('rules.pickHint', {
          defaultValue: '选一条规则，把 {{domain}} 加进它的域名条件；没有域名条件的会为它新增一条',
          domain,
        })}
      </div>

      <div className="pp-tools">
        <label
          className="input"
          style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '0 11px' }}
        >
          <svg
            viewBox="0 0 24 24"
            width={14}
            fill="none"
            stroke="currentColor"
            strokeWidth={1.8}
            style={{ color: 'hsl(var(--fg-faint))', flex: 'none' }}
          >
            <circle cx="11" cy="11" r="7" />
            <path d="M20 20l-3-3" />
          </svg>
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t('rules.pickSearchPh', '搜索规则名 / 条件值…')}
            aria-label={t('rules.pickSearchPh', '搜索规则名 / 条件值…')}
            style={{
              border: 0,
              background: 'none',
              outline: 'none',
              flex: 1,
              padding: '8px 0',
              font: 'inherit',
              color: 'inherit',
            }}
          />
        </label>
      </div>

      <div className="proc-pick-list">
        {targets.length === 0 && (
          <div className="proc-pick-empty">{t('rules.pickEmpty', '还没有任何自定义规则')}</div>
        )}
        {targets.length > 0 && shown.length === 0 && (
          <div className="proc-pick-empty">{t('rules.pickNoMatch', '没有匹配的规则')}</div>
        )}
        {shown.map((target) => {
          const ruleName = t(ruleTypeNameKey(target.ruleType), RULE_TYPES[target.ruleType].nameZh);
          const typeName = t(ruleTypeNameKey(target.type), RULE_TYPES[target.type].nameZh);
          /* 无备注时的行名：有目标条件的行靠第二行「类型: 值」认规则，第一行给类型名就够（同
             `RuleItem.tsx::ruleTitle`）；没有目标条件的行（新开腿 / 置灰行）第二行放的是动作或
             原因，规则身份必须挪到第一行来，否则一屏几条无备注的 geosite 规则长得一模一样。 */
          const identity =
            target.condIndex < 0 && target.ruleValues.length > 0
              ? `${ruleName}: ${target.ruleValues.join(', ')}`
              : ruleName;
          // 遮蔽提示只对**可追加**的项有意义：置灰项本来就点不下去，再挂一个「前面可能先命中」是噪音。
          const shadowed = target.block === null && isShadowedTarget(coverage, target);
          const why = whyText(target);
          return (
            <button
              key={`${target.ruleId}#${target.condIndex}`}
              type="button"
              className="proc-pick-row"
              disabled={target.block !== null}
              onClick={() => pick(target)}
            >
              <span className="pp-main">
                <span className="pp-nm">{target.remarks || identity}</span>
                <span className="pp-path">
                  {why ??
                    (target.condIndex < 0
                      ? t('rules.pickNewCond', {
                          defaultValue: '将为它新增一个「{{type}}」条件',
                          type: typeName,
                        })
                      : target.values.length > 0
                        ? `${typeName}: ${target.values.join(', ')}`
                        : typeName)}
                </span>
              </span>
              {target.block === 'contains' && (
                <span className="pill region">{t('home.domainAlreadyInRule', { domain })}</span>
              )}
              {!target.enabled && (
                <span className="pill region">{t('rules.pickDisabledTag', '已禁用')}</span>
              )}
              {/* 优先级提示：**只是提示**。判据是客户端启发式（权威匹配在内核），故文案说「可能」，
                  且不据此禁用本项 —— 用户完全可以就是想把值加进后面那条。 */}
              {shadowed && (
                <span
                  className="pill warn"
                  data-tip={t('rules.pickShadowTip', {
                    defaultValue:
                      '更靠前的规则看起来也能命中 {{domain}}（客户端启发式判断，以内核实际匹配为准）',
                    domain,
                  })}
                >
                  {t('rules.pickShadowTag', '前面可能先命中')}
                </span>
              )}
            </button>
          );
        })}
      </div>
    </Modal>
  );
}

export default RulePickDialog;
