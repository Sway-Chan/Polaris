/**
 * RuleHoverCard —— 规则名 hover 详情卡内容（设计文档 §1.5 RuleHoverCard；原型 registerTipCard('rule', …)
 * polaris-prototype.html :5230-5250）：名称 + 标签（禁用/AND-OR）· 逐条件行（类型 pill + 值）· 分隔线 ·
 * 策略行（action pill + 目标节点）。挂载方式见 HoverCard.tsx——本文件只导出内容，定位/延迟/挂载由调用方
 * （RuleItem.tsx）通过 useHoverCard() + <HoverCardPanel> 组装，不在此重复。
 */
import { useTranslation } from 'react-i18next';
import type { Rule } from '@/contracts/types';
import { ruleConditions } from '@/domain/rules';

/** 条件类型展示名兜底（zh 默认值，键名沿用 RuleDialog.tsx 已建立的 `rules.type.<id>` i18n 约定，
 * 不导入该文件本身——dialogs/* 归另一并行 agent，本卡只借用同一 key 命名空间）。 */
export const TYPE_LABEL: Partial<Record<Rule['type'], string>> = {
  domain: '域名',
  domainSuffix: '域名后缀',
  domainKeyword: '域名关键词',
  domainRegex: '域名正则',
  ipCidr: '目标 IP/CIDR',
  sourceIpCidr: '源 IP/CIDR',
  port: '目标端口',
  sourcePort: '源端口',
  sourceMac: '源设备 MAC',
  sourceHostname: '源设备主机名',
  processName: '进程名',
  processPath: '进程路径',
  geosite: 'Geosite',
  geoip: 'GeoIP',
  ruleSet: '规则集',
};

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
        {!rule.enabled && <span className="rl-hc-tag off">{t('rules.disabledBadge', '已禁用')}</span>}
        {conds.length > 1 && (
          <span className="rl-hc-tag">{isOr ? t('rules.combineOr', '满足任一') : t('rules.combineAnd', '全部满足')}</span>
        )}
      </div>
      <div className="rl-hc-conds">
        {conds.length === 0 ? (
          <div className="rl-hc-cond">
            <span className="rl-hc-val" style={{ color: 'hsl(var(--fg-faint))' }}>
              {t('rules.noConditions', '（无条件）')}
            </span>
          </div>
        ) : (
          conds.map((c, i) => (
            <div className="rl-hc-cond" key={i}>
              <span className="pill region rl-hc-ty">{t(`rules.types.${c.type}.name`, TYPE_LABEL[c.type] ?? c.type)}</span>
              <span className="mono rl-hc-val">{c.values.join(', ')}</span>
            </div>
          ))
        )}
      </div>
      <div className="tc-sep" />
      <div className="tc-row">
        <span className="tc-lbl">{t('rules.policy', '策略')}</span>
        {rule.action === 'direct' ? (
          <span className="pill act-direct">{t('rules.targetDirect', '直连')}</span>
        ) : rule.action === 'block' ? (
          <span className="pill act-block">{t('rules.targetBlock', '阻断')}</span>
        ) : (
          <>
            <span className="pill act-proxy">{t('rules.policyProxy', '代理')}</span>
            <span className="rl-hc-node">→ {targetNodeName ?? t('rules.targetDefaultProxy', '默认代理')}</span>
          </>
        )}
      </div>
    </>
  );
}
