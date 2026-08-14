/**
 * RuleHoverCard —— 规则名 hover 详情卡内容（设计文档 §1.5 RuleHoverCard；原型 registerTipCard('rule', …)
 * polaris-prototype.html :5230-5250）：名称 + 标签（禁用/AND-OR）· 逐条件行（类型 pill + 值）· 分隔线 ·
 * 策略行（action pill + 目标节点）。挂载方式见 HoverCard.tsx——本文件只导出内容，定位/延迟/挂载由调用方
 * （RuleItem.tsx）通过 useHoverCard() + <HoverCardPanel> 组装，不在此重复。
 */
import { useTranslation } from 'react-i18next';
import type { Rule } from '@/contracts/types';
import { ruleConditions } from '@/domain/rules';

export function RuleHoverCardContent({
  rule,
  targetNodeName,
}: {
  rule: Rule;
  targetNodeName?: string;
}) {
  const { t } = useTranslation();
  const conds = ruleConditions(rule);
  const isOr = (rule.combineMode ?? 'or') === 'or';
  const name = rule.remarks?.trim() || rule.type;

  return (
    <>
      <div className="tip-t rl-hc-head">
        <span className="rl-hc-nm">{name}</span>
        {!rule.enabled && <span className="rl-hc-tag off">{t('rules.disabledBadge')}</span>}
        {conds.length > 1 && (
          <span className="rl-hc-tag">{isOr ? t('rules.combineOr') : t('rules.combineAnd')}</span>
        )}
      </div>
      <div className="rl-hc-conds">
        {conds.length === 0 ? (
          <div className="rl-hc-cond">
            <span className="rl-hc-val" style={{ color: 'hsl(var(--fg-faint))' }}>
              {t('rules.noConditions')}
            </span>
          </div>
        ) : (
          conds.map((c, i) => (
            <div className="rl-hc-cond" key={i}>
              <span className="pill region rl-hc-ty">{t(`rules.types.${c.type}.name`)}</span>
              <span className="mono rl-hc-val">{c.values.join(', ')}</span>
            </div>
          ))
        )}
      </div>
      <div className="tc-sep" />
      <div className="tc-row">
        <span className="tc-lbl">{t('rules.policy')}</span>
        {rule.action === 'direct' ? (
          <span className="pill act-direct">{t('rules.targetDirect')}</span>
        ) : rule.action === 'block' ? (
          <span className="pill act-block">{t('rules.targetBlock')}</span>
        ) : (
          <>
            <span className="pill act-proxy">{t('rules.policyProxy')}</span>
            <span className="rl-hc-node">→ {targetNodeName ?? t('rules.targetDefaultProxy')}</span>
          </>
        )}
      </div>
    </>
  );
}
