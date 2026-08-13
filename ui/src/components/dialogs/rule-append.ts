/**
 * 「把一个观测到的主机名追加进**已有**规则」的纯判据与纯变换。
 *
 * 与 `rule-cond.ts` 同层同目录（本仓 vitest 是 node 环境无 jsdom，判据留在 `.tsx` 里等于没有门），
 * 不是新抽象层。逐类型语义一条都不在这里 —— 命中判据在 `domain/rules.ts` 的描述符 `test` 字段，
 * 值合法性在 `validateRuleValue`，AND/OR 合成在 `rule-cond.ts` 的 `matchConditionValues`。
 *
 * # 为什么「加入已有规则」不是便利功能，而是这条路上唯一有效的动作（两条硬事实）
 *
 * 1. **新建的规则恒落列表末尾**（`commands/rules.rs` 的 `rules_add` 是 `arr.push`），而路由语义是
 *    **先匹配先生效**。前面已有规则能命中同一域名时，新建的那条压根不生效，且 UI 零提示。
 * 2. **追加到已有 Ext 规则零重启**（值不进 config norm ⇒ `classify_switch` 判 `NoOp` ⇒ 规则文件
 *    原子 rename-over + sing-box fswatch 热重载）；而**新建**必然改变 `customRules` 数组长度 ⇒
 *    norm 不等 ⇒ 去抖整核重启。
 *    ⚠️ 第 2 条对**新开条件**那条腿有两个例外：Ext 规则的条件结构同样只落规则文件，故通常仍是
 *    NoOp，但 ① `bypassFakeIP:true` 且原本无域名值的规则，新开域名条件会让 `dns_headless_rules`
 *    （`custom_rule_files.rs:167`）由 `None` 变 `Some` ⇒ config.json 多注册一个 `{base}-dns`
 *    rule_set ⇒ norm 变 ⇒ 重启；② 原本 `ExtSkip`（条件值全空）的退化规则会变成 `Ext` ⇒
 *    config.json 多一条 route 规则 ⇒ 重启。两者都由 `classify_switch` 如实判定，本模块不假装、
 *    也不据此拦人。
 *
 * # 候选面：**列出全部规则**，不能追加的置灰并逐条给原因
 *
 * 可追加有两条腿：
 *
 * 1. **规则已有域名族「字面量」条件** ⇒ 往那个条件里追加值。单条件内多值恒 OR
 *    （`cond_matcher_fields` 把同条件的值拼进同一个 matcher 字段），纯扩宽、可预测。
 * 2. **规则没有能收下这个值的域名族条件，且 `combineMode !== 'and'`** ⇒ 为它**新开**一个
 *    `PRESET_HOST_RULE_TYPE`（= `domainSuffix`，与右键直写腿同一个常量）条件。
 *
 * 第 2 条腿此前整个被禁，理由写的是「新开条件会改变规则的**逻辑形状**」。查生成侧后，该理由
 * **只对 `combineMode === 'and'` 成立**：
 *
 *  - `custom_rule_files.rs:236` 与 `custom_rules.rs:507`：`is_and = combine_mode == Some(And)`
 *    ⇒ `None` 与 `Some(Or)` 走同一条路；`store/sanitize.rs:383-388` 把非 `and`/`or` 的值直接删掉
 *    ⇒ 回落 `None`。**默认就是 OR**。
 *  - 非 and 且全部条件属 OR 组（域名族 + `ipCidr`，见 `is_or_group`）⇒ merge 进**同一个** rule
 *    object，而 sing-box 在单条 default rule 内对这一组本就是 OR。
 *  - 否则走 logical：`mode = combine_mode.unwrap_or(Or)` ⇒ 显式 `"or"`（Ext 路 `:273` 与 Inline 路
 *    `custom_rules.rs:566` 两处同形）。
 *
 *  ⇒ `or` 下「新开一个域名条件」与「往已有域名条件追加值」**生成结果等价**，都是纯 OR 扩宽。
 *  ⇒ 只有 `and` 才是求交（「域名 AND IP 段」是语义完全不同的规则）。**故只有 `and` 置灰**，
 *    并指向规则弹窗显式编辑。单条件 + `combineMode:'and'` 也一样拦：那个 `and` 今天是潜伏的
 *    （`conds.len()==1` 时 mergeable，模式不起作用），新开第二个条件会把它**激活**成求交；
 *    UI 不能替用户把它悄悄改成 `or` —— 那才是真的改写了规则的逻辑形状。
 *
 * - **`domainRegex` 永不作为追加目标**：把字面主机名塞进正则表需要转义（`.` 会从字面点变成通配符），
 *   是正确性陷阱；`rule_validate.rs` 只禁 RE2 非法项，不会替用户转义。同源判据在 `rule-cond.ts` 的
 *   `setCondTypeAt`（「`domainRegex` 永不接受任何带过来的值」）已落地成门。
 *   但「只有 `domainRegex` 条件」的规则**不再**因此被排除 —— 它走第 2 条腿新开一个字面量条件，
 *   转义陷阱恰恰是这样绕开的，而不是靠把整条规则从清单里删掉。
 * - **`domainKeyword` 保留**，但调用方必须显式标出类型：整个主机名当关键词是**更窄**的匹配
 *   （`example.com` 会命中 `notexample.com.evil.tld` 这类子串），用户看到类型才知道自己拿到了什么。
 * - **禁用规则也是合法目标**（用户可能在攒一条待启用的规则），但要标出禁用态。这与既有两条判据
 *   （`meshOverlapRuleIds` / `missingResourceRuleIds` 只判已启用）口径不同 —— 那两条是**冲突告警**
 *   （只有启用才会冲突），本处是**写入目标**。
 */
import type { Rule, RuleCondition, RuleType } from '@/contracts/types';
import {
  PRESET_HOST_RULE_TYPE,
  RULE_TYPE_IDS,
  RULE_TYPES,
  ruleConditions,
  validateRuleValue,
} from '@/domain/rules';
import { matchConditionValues } from './rule-cond';

/**
 * 可作为追加目标的条件类型 —— 域名族里**值是字面主机名**的那三个。
 *
 * 派生自描述符表的 `category`（不另立第二张类型表），只显式减掉 `domainRegex` 那一个：它的值是
 * **模式**不是字面量，是本模块唯一需要点名的例外。第 16 个域名类型若同样是模式类，加进下面这个
 * 排除集；若是字面量类，自动被纳入而无需改代码。
 */
const PATTERN_DOMAIN_TYPES: ReadonlySet<RuleType> = new Set<RuleType>(['domainRegex']);

export const APPENDABLE_HOST_TYPES: readonly RuleType[] = RULE_TYPE_IDS.filter(
  (id) => RULE_TYPES[id].category === 'domain' && !PATTERN_DOMAIN_TYPES.has(id)
);

const APPENDABLE = new Set<RuleType>(APPENDABLE_HOST_TYPES);

/** 新开条件时用的类型 —— 与右键「加入自定义规则」直写腿同一个常量，两条腿产出同形规则。 */
export const NEW_COND_TYPE: RuleType = PRESET_HOST_RULE_TYPE;

/**
 * 不可追加的原因（`null` = 可追加）。**每一项都指向一条具体出路**，不是笼统的「不可追加」。
 *
 *  - `contains`    该条件已含这个值 —— 成功的无事可做，不是失败。
 *  - `andMode`     规则是「全部条件都命中」（`combineMode:'and'`）且没有能收下这个值的域名条件 ——
 *                  新开条件会变成求交而非扩宽，去规则弹窗显式编辑。
 *  - `valueUnfit`  这个值本身进不了域名字面量条件（真实命中的是 IPv6 主机名：`isRuleableHost`
 *                  只判「含 `.` 或 `:`」，故 `2606:4700::1` 会走到这里，而它过不了 `domain` /
 *                  `domainSuffix` 的域名形状校验）。
 *
 * 注意这里**没有**「无域名条件」与「只有域名正则」两类 —— 那两种规则现在走新开条件腿，是可追加的。
 */
export type AppendBlock = 'contains' | 'andMode' | 'valueUnfit';

/** 一个「往哪条规则的哪个条件里追加」的目标。**每条规则至少一项**（不能追加的带 `block`）。 */
export interface RuleAppendTarget {
  readonly ruleId: string;
  /** 规则在**有效**规则数组里的下标 = 优先级（越小越先匹配）。 */
  readonly ruleIndex: number;
  /** 规则备注（空串 = 无备注，由渲染层回落成 `ruleType` 的类型名）。 */
  readonly remarks: string;
  readonly enabled: boolean;
  /**
   * 规则自身的镜像（= `conditions[0]` 的类型与值）—— **认规则**用，与下面的 `type`/`values`
   * （**认目标条件**）是两码事。没有目标条件的行（新开腿 / 置灰行）第二行放的是动作或原因，
   * 认不出是哪条规则，就得靠这一对把规则身份写进第一行（口径同 `RuleItem.tsx::ruleTitle`：
   * 无备注就拿首条件类型当标题）。
   */
  readonly ruleType: RuleType;
  readonly ruleValues: readonly string[];
  /**
   * 目标条件下标。`>= 0` = 索引进 `ruleConditions(rule)`；`-1` = **为该规则新开一个条件**
   * （`block !== null` 时同样是 `-1`，那种行根本没有目标）。
   */
  readonly condIndex: number;
  /** 目标条件的类型（新开腿 = `NEW_COND_TYPE`；`block !== null` 时回落成 `ruleType`，无意义）。 */
  readonly type: RuleType;
  /** 目标条件的现有值（新开腿与 `block !== null` 时为空）。 */
  readonly values: readonly string[];
  /** `null` = 可追加；否则 = 不可追加的原因（渲染层据此置灰 + 逐条说明）。 */
  readonly block: AppendBlock | null;
  /** 检索语料（已小写）。**置灰项同样参与检索** —— 搜得到规则名却搜不到规则，用户会以为规则不存在。 */
  readonly search: readonly string[];
}

const lower = (v: string): string => v.trim().toLowerCase();

/** 条件的值数组（防御非数组 / 非字符串：旁路 `config:save` 可注入）。 */
function condValues(cond: RuleCondition): string[] {
  return Array.isArray(cond.values) ? cond.values.filter((v): v is string => typeof v === 'string') : [];
}

/**
 * 全部规则的追加目标（每条规则至少一项；顺序 = 规则顺序 → 条件顺序，**未排序**）。
 *
 * `validateRuleValue` 这道过滤是 **fail-closed**，与 `rule-cond.ts` 里候选池那道同源：不置灰就点得
 * 下去的必须存得下去。上架一个点一下就在保存时被后端拒掉的目标，比把它置灰并说明原因更糟。
 *
 * 排序不在这里做（`sortAppendTargets` 单独一支）：本函数的产物要能被「顺序 = 规则顺序」的断言直接检查。
 */
export function ruleAppendTargets(rules: readonly Rule[], value: string): RuleAppendTarget[] {
  const v = value.trim();
  if (!v) return [];
  const lv = v.toLowerCase();
  const out: RuleAppendTarget[] = [];
  rules.forEach((rule, ruleIndex) => {
    const conds = ruleConditions(rule).filter((c): c is RuleCondition => !!c);
    const remarks = (rule.remarks ?? '').trim();
    const ruleType = conds[0]?.type ?? rule.type;
    const ruleValues = conds[0] ? condValues(conds[0]) : [];
    // 检索语料按**整条规则**取（不按单个条件）：一条规则的任一个值都该能把它搜出来。
    const search = [remarks, ruleType, ...conds.flatMap((c) => [c.type, ...condValues(c)])]
      .filter(Boolean)
      .map((s) => s.toLowerCase());
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
      .filter(({ cond }) => APPENDABLE.has(cond.type) && validateRuleValue(cond.type, v));

    if (hits.length > 0) {
      for (const { cond, condIndex } of hits) {
        const values = condValues(cond);
        out.push({
          ...base,
          condIndex,
          type: cond.type,
          values,
          block: values.some((x) => lower(x) === lv) ? 'contains' : null,
        });
      }
      return;
    }

    // 没有能收下这个值的既有条件 ⇒ 新开条件腿（失败原因二选一，值不合形状优先于组合模式）。
    const block: AppendBlock | null = !validateRuleValue(NEW_COND_TYPE, v)
      ? 'valueUnfit'
      : rule.combineMode === 'and'
        ? 'andMode'
        : null;
    out.push({
      ...base,
      condIndex: -1,
      type: block === null ? NEW_COND_TYPE : ruleType,
      values: [],
      block,
    });
  });
  return out;
}

/**
 * 展示顺序：可追加在前 → 已包含 → 其余置灰，**同档内保持规则顺序**（顺序 = 优先级，不许打乱）。
 *
 * `contains` 单独一档而不与其它置灰项混在一起：它是「已经做到了」，与「做不到」不是一回事，
 * 混排会让用户在一堆做不到里翻找那条其实已经覆盖了的规则。
 */
const RANK: Record<AppendBlock | 'ok', number> = { ok: 0, contains: 1, andMode: 2, valueUnfit: 2 };

export function sortAppendTargets(targets: readonly RuleAppendTarget[]): RuleAppendTarget[] {
  return targets
    .map((t, i) => ({ t, i }))
    .sort((a, b) => RANK[a.t.block ?? 'ok'] - RANK[b.t.block ?? 'ok'] || a.i - b.i)
    .map(({ t }) => t);
}

/** 按检索词过滤（空词 = 原样副本），口径同 `rule-cond.ts` 的 `matchRuleValueOptions`。 */
export function matchAppendTargets(
  targets: readonly RuleAppendTarget[],
  query: string
): RuleAppendTarget[] {
  const q = query.trim().toLowerCase();
  if (!q) return targets.slice();
  return targets.filter((t) => t.search.some((s) => s.includes(q)));
}

/**
 * 追加一个值，产出**整条**新规则；无事可做 / 目标漂移 / 目标本就置灰 ⇒ `null`（调用方按 no-op 处理）。
 *
 * 三条约束，每条都必须在写入侧成立：
 *  1. **`{ ...base }` 起底**：保全 `tlsSpoof` / `tlsSpoofMethod` 这类不在本函数视野里的字段。
 *  2. **镜像不变式**：`Rule.type` / `Rule.values` 恒 = `conditions[0]` 的镜像。读盘侧
 *     `store/sanitize.rs:436-440` 会重镜像，但**别指望它兜底** —— 写入侧不镜像的话，从写入到下次读盘
 *     之间的内存态就是不一致的（列表按 `rule.type` 画图标、`ruleTitle` 按它回落标题）。
 *     新开的条件**追加在末尾**，故 `conditions[0]` 不动；镜像仍从 `next[0]` 现算，不靠「它没变」的假设。
 *  3. **目标漂移防御**：从「打开选择器」到「点下某一项」之间，规则可能被别处改过（暂存重放 /
 *     另一处编辑）。既有条件腿是位置寻址，位置上换了别的类型就必须放弃；新开条件腿则要复核
 *     **判据本身**仍成立（还是没有能收下这个值的同族条件、还不是 `and`）—— 否则会在一条已经有域名
 *     条件的规则上再挂一个多余条件，或者把条件挂进一条已被改成求交的规则里。
 *
 * 单条件规则保持单条件形态（`conditions` / `combineMode` 显式清成 `undefined`），与规则弹窗提交腿同形。
 */
export function appendValueToRule(
  base: Rule,
  target: RuleAppendTarget,
  value: string
): Rule | null {
  const v = value.trim();
  if (!v || base.id !== target.ruleId || target.block !== null) return null;
  const conds = ruleConditions(base).filter((c): c is RuleCondition => !!c);
  if (!validateRuleValue(target.type, v)) return null;

  let next: RuleCondition[];
  if (target.condIndex < 0) {
    // 新开条件腿：判据复核（漂移防御）——「不给 and 规则新开条件」这条线在写入侧同样有牙。
    if (base.combineMode === 'and') return null;
    if (conds.some((c) => APPENDABLE.has(c.type) && validateRuleValue(c.type, v))) return null;
    next = [...conds, { type: target.type, values: [v] }];
  } else {
    const cond = conds[target.condIndex];
    if (!cond || cond.type !== target.type || !APPENDABLE.has(cond.type)) return null;
    const values = condValues(cond);
    if (values.some((x) => lower(x) === v.toLowerCase())) return null; // 已包含 = 无事可做
    next = conds.map((c, i) =>
      i === target.condIndex ? { type: c.type, values: [...values, v] } : c
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

/**
 * 「哪些规则看起来会命中这个域名」+「顺序上第一条是谁」。
 *
 * 与 `meshOverlapRuleIds` / `missingResourceRuleIds` / `stagedOnlyIds` 同族：纯函数判据 + 渲染层角标。
 *
 * ⚠️ **只能做提示，不能做门。** 判定复用 `matchConditionValues`，那是**客户端启发式**（geoip 恒判命中、
 * geosite 退化成子串、IP 只比前两段），权威匹配器在 sing-box 内核。据此禁用「新建规则」= 把一个
 * 会误报也会漏报的启发式升格成阻断闸门。文案同样必须如实（「可能先命中」而不是「不会生效」）。
 *
 * 只看**已启用**规则：禁用规则不下发，遮蔽不了任何东西（与 `meshOverlapRuleIds` 同口径）。
 * 注意这与 `ruleAppendTargets` 收禁用规则不矛盾 —— 那边问的是「能不能写进去」，这边问的是
 * 「写进去会不会被前面的先吃掉」。
 */
export interface DomainCoverage {
  /** 启发式命中该值的**已启用**规则 id。 */
  readonly coveredIds: ReadonlySet<string>;
  /** 顺序上第一条命中的规则下标（先匹配先生效 ⇒ 它就是实际生效的那条）；-1 = 无。 */
  readonly firstIndex: number;
  readonly firstId: string | null;
}

export function analyzeDomainCoverage(rules: readonly Rule[], value: string): DomainCoverage {
  const coveredIds = new Set<string>();
  let firstIndex = -1;
  let firstId: string | null = null;
  const v = value.trim();
  if (v) {
    rules.forEach((rule, i) => {
      if (rule.enabled !== true) return;
      // 走**值已拆好**的那一层，不经草稿串：`splitVals` 会把 `domainRegex` 值里合法的逗号
      // （`^a{1,3}$`）当分隔符，一条正则被拆成两条乱码。合成逻辑仍只此一份。
      const conds = ruleConditions(rule).map((c) => ({ type: c.type, values: condValues(c) }));
      if (matchConditionValues(conds, rule.combineMode === 'and' ? 'and' : 'or', v) !== 'hit') return;
      coveredIds.add(rule.id);
      if (firstIndex < 0) {
        firstIndex = i;
        firstId = rule.id;
      }
    });
  }
  return { coveredIds, firstIndex, firstId };
}

/** 该目标是否被**更靠前**的规则遮蔽（追加进去可能不生效）。同样只是提示。 */
export function isShadowedTarget(coverage: DomainCoverage, target: RuleAppendTarget): boolean {
  return coverage.firstIndex >= 0 && coverage.firstIndex < target.ruleIndex;
}
