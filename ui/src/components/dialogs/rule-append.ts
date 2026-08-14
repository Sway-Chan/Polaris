/**
 * 把连接记录里观测到的规则对象追加进已有规则：本文件只负责纯判据与纯变换。
 *
 * 域名、目的 IP、进程名共用同一条写入腿，但可写条件不同：域名可进入字面量域名条件，IP 只进入
 * `ipCidr`，进程只进入 `processName`。新开条件始终使用对象自身的精确类型，避免入口显示的是 IP，
 * 最终却仍写成域名规则。
 */
import type { Rule, RuleCondition, RuleType } from '@/contracts/types';
import {
  RULE_TYPE_IDS,
  RULE_TYPES,
  ruleConditions,
  validateRuleValue,
  type RuleSubject,
} from '@/domain/rules';
import { matchConditionValues } from './rule-cond';

const PATTERN_DOMAIN_TYPES: ReadonlySet<RuleType> = new Set(['domainRegex']);
const DOMAIN_LITERAL_TYPES: readonly RuleType[] = RULE_TYPE_IDS.filter(
  (id) => RULE_TYPES[id].category === 'domain' && !PATTERN_DOMAIN_TYPES.has(id),
);

/** 对象可追加到哪些既有条件。新开条件不走该表，直接使用 `subject.type`。 */
export function appendableRuleTypes(subject: RuleSubject): readonly RuleType[] {
  switch (subject.kind) {
    case 'domain':
      return DOMAIN_LITERAL_TYPES;
    case 'ip':
      return ['ipCidr'];
    case 'process':
      return ['processName'];
  }
}

/**
 * `contains` 是成功的无事可做；`andMode` 表示新开条件会把扩宽变成求交；`valueUnfit` 表示值不合法。
 */
export type AppendBlock = 'contains' | 'andMode' | 'valueUnfit';

/** 一个“往哪条规则的哪个条件里追加”的目标；每条规则至少产生一项。 */
export interface RuleAppendTarget {
  readonly ruleId: string;
  readonly ruleIndex: number;
  readonly remarks: string;
  readonly enabled: boolean;
  /** 规则首条件镜像，用于无备注时识别规则。 */
  readonly ruleType: RuleType;
  readonly ruleValues: readonly string[];
  /** `-1` 表示为该规则新开条件。 */
  readonly condIndex: number;
  readonly type: RuleType;
  readonly values: readonly string[];
  readonly block: AppendBlock | null;
  readonly search: readonly string[];
}

const lower = (value: string): string => value.trim().toLowerCase();

function condValues(cond: RuleCondition): string[] {
  return Array.isArray(cond.values)
    ? cond.values.filter((value): value is string => typeof value === 'string')
    : [];
}

/** 全部规则的追加目标；顺序为规则顺序 → 条件顺序，排序由 `sortAppendTargets` 单独处理。 */
export function ruleAppendTargets(
  rules: readonly Rule[],
  subject: RuleSubject,
): RuleAppendTarget[] {
  const value = subject.value.trim();
  if (!value) return [];
  const normalized = value.toLowerCase();
  const appendable = new Set(appendableRuleTypes(subject));
  const out: RuleAppendTarget[] = [];

  rules.forEach((rule, ruleIndex) => {
    const conds = ruleConditions(rule).filter((cond): cond is RuleCondition => !!cond);
    const remarks = (rule.remarks ?? '').trim();
    const ruleType = conds[0]?.type ?? rule.type;
    const ruleValues = conds[0] ? condValues(conds[0]) : [];
    const search = [remarks, ruleType, ...conds.flatMap((cond) => [cond.type, ...condValues(cond)])]
      .filter(Boolean)
      .map((term) => term.toLowerCase());
    const base = {
      ruleId: rule.id,
      ruleIndex,
      remarks,
      enabled: rule.enabled === true,
      ruleType,
      ruleValues,
      search,
    };

    const hits = conds
      .map((cond, condIndex) => ({ cond, condIndex }))
      .filter(({ cond }) => appendable.has(cond.type) && validateRuleValue(cond.type, value));

    if (hits.length > 0) {
      for (const { cond, condIndex } of hits) {
        const values = condValues(cond);
        out.push({
          ...base,
          condIndex,
          type: cond.type,
          values,
          block: values.some((item) => lower(item) === normalized) ? 'contains' : null,
        });
      }
      return;
    }

    const block: AppendBlock | null = !validateRuleValue(subject.type, value)
      ? 'valueUnfit'
      : rule.combineMode === 'and'
        ? 'andMode'
        : null;
    out.push({
      ...base,
      condIndex: -1,
      type: block === null ? subject.type : ruleType,
      values: [],
      block,
    });
  });

  return out;
}

const RANK: Record<AppendBlock | 'ok', number> = {
  ok: 0,
  contains: 1,
  andMode: 2,
  valueUnfit: 2,
};

/** 可追加 → 已包含 → 其余置灰；同档内保持规则优先级顺序。 */
export function sortAppendTargets(targets: readonly RuleAppendTarget[]): RuleAppendTarget[] {
  return targets
    .map((target, index) => ({ target, index }))
    .sort(
      (a, b) =>
        RANK[a.target.block ?? 'ok'] - RANK[b.target.block ?? 'ok'] || a.index - b.index,
    )
    .map(({ target }) => target);
}

export function matchAppendTargets(
  targets: readonly RuleAppendTarget[],
  query: string,
): RuleAppendTarget[] {
  const normalized = query.trim().toLowerCase();
  if (!normalized) return targets.slice();
  return targets.filter((target) => target.search.some((term) => term.includes(normalized)));
}

/** 追加对象并返回完整规则；无事可做、目标漂移或目标置灰时返回 `null`。 */
export function appendSubjectToRule(
  base: Rule,
  target: RuleAppendTarget,
  subject: RuleSubject,
): Rule | null {
  const value = subject.value.trim();
  if (!value || base.id !== target.ruleId || target.block !== null) return null;
  const conds = ruleConditions(base).filter((cond): cond is RuleCondition => !!cond);
  const appendable = new Set(appendableRuleTypes(subject));
  if (!validateRuleValue(target.type, value)) return null;

  let next: RuleCondition[];
  if (target.condIndex < 0) {
    if (base.combineMode === 'and' || target.type !== subject.type) return null;
    if (conds.some((cond) => appendable.has(cond.type) && validateRuleValue(cond.type, value))) {
      return null;
    }
    next = [...conds, { type: target.type, values: [value] }];
  } else {
    const cond = conds[target.condIndex];
    if (!cond || cond.type !== target.type || !appendable.has(cond.type)) return null;
    const values = condValues(cond);
    if (values.some((item) => lower(item) === value.toLowerCase())) return null;
    next = conds.map((item, index) =>
      index === target.condIndex ? { type: item.type, values: [...values, value] } : item,
    );
  }

  const multi = next.length > 1;
  return {
    ...base,
    type: next[0].type,
    values: next[0].values,
    conditions: multi ? next : undefined,
    combineMode: multi ? base.combineMode : undefined,
  };
}

export interface RuleCoverage {
  readonly coveredIds: ReadonlySet<string>;
  readonly firstIndex: number;
  readonly firstId: string | null;
}

/** 只保留能由当前对象判断的条件；AND 规则含其它维度时信息不足，不声称已覆盖。 */
function matchingConditions(rule: Rule, subject: RuleSubject): RuleCondition[] | null {
  const conds = ruleConditions(rule).filter((cond): cond is RuleCondition => !!cond);
  const compatible = conds.filter((cond) => {
    if (subject.kind === 'process') return cond.type === 'processName';
    return RULE_TYPES[cond.type]?.test?.axis === subject.kind;
  });
  if (compatible.length === 0) return null;
  if (rule.combineMode === 'and' && compatible.length !== conds.length) return null;
  return compatible;
}

function subjectMatchesRule(rule: Rule, subject: RuleSubject): boolean {
  const conds = matchingConditions(rule, subject);
  if (!conds) return false;
  const logic = rule.combineMode === 'and' ? 'and' : 'or';
  if (subject.kind !== 'process') {
    return (
      matchConditionValues(
        conds.map((cond) => ({ type: cond.type, values: condValues(cond) })),
        logic,
        subject.value,
      ) === 'hit'
    );
  }
  const value = lower(subject.value);
  const hits = conds.map((cond) => condValues(cond).some((item) => lower(item) === value));
  return logic === 'and' ? hits.every(Boolean) : hits.some(Boolean);
}

/**
 * 客户端启发式覆盖提示。它只影响菜单排序与提示，不得据此禁用新建；权威匹配结果仍由内核决定。
 */
export function analyzeRuleCoverage(
  rules: readonly Rule[],
  subject: RuleSubject,
): RuleCoverage {
  const coveredIds = new Set<string>();
  let firstIndex = -1;
  let firstId: string | null = null;
  if (!subject.value.trim()) return { coveredIds, firstIndex, firstId };

  rules.forEach((rule, index) => {
    if (rule.enabled !== true || !subjectMatchesRule(rule, subject)) return;
    coveredIds.add(rule.id);
    if (firstIndex < 0) {
      firstIndex = index;
      firstId = rule.id;
    }
  });
  return { coveredIds, firstIndex, firstId };
}

export function isShadowedTarget(coverage: RuleCoverage, target: RuleAppendTarget): boolean {
  return coverage.firstIndex >= 0 && coverage.firstIndex < target.ruleIndex;
}
