/**
 * 规则弹窗「匹配条件」草稿的纯判据 —— 多值拆分、测试匹配。
 *
 * 为什么抽出组件：本仓 vitest 是 **node 环境无 jsdom**（`vite.config.ts` 的 `test.environment`），
 * 判据留在 `.tsx` 里等于没有门（同 `rule-set-pick.ts` / `csel-logic.ts` / `apppolicy-logic.ts` 的
 * 先例）。本文件与它们同层同目录，不是新抽象层，只是把已有的行内表达式挪到可断言的位置。
 *
 * 逐类型的语义**一条都不在这里**：域名/IP 的命中判据在 `domain/rules.ts` 的描述符 `test` 字段上。
 * 本文件只负责「怎么组合」（AND/OR、适用性过滤），故新增第 16 个类型不会碰它。
 */
import type { RuleType, RuleResourceListItem, SystemProcessInfo } from '@/contracts/types';
import { RULE_TYPES, validateRuleValue } from '@/domain/rules';
import { geoTagOf } from '@/domain/rule-resource-refs';
import { ruleSetRef } from './rule-set-pick';

/** 编辑草稿的单条件（`v` = 原始多值串，提交时 `splitVals`）。 */
export interface Cond {
  t: RuleType;
  v: string;
}

/** 逗号 / 换行分隔多值（原型 splitVals :3904）：拆分 → trim → 去空。 */
export const splitVals = (v: string): string[] =>
  String(v ?? '')
    .split(/[,\n]/)
    .map((s) => s.trim())
    .filter(Boolean);

// ─────────────────────────────────────────────────────────────────────────────
// 草稿编辑（类型切换 / 勾选）
// ─────────────────────────────────────────────────────────────────────────────

/**
 * 切换某个条件的类型 —— **一律清空值**，可枚举类型的勾选态随之归零。
 *
 * 判据是 `(类型, 值)` 的**原子性**，不是「顺手清一下更干净」：每个类型自带解析 + 变换 + 分区
 * （Rust `builder/custom_rule_files.rs:66-133`），值不是类型无关的载荷。
 *  · `域名 → 域名后缀`：额外命中全部子域名，且手打的 `*.` 前缀被**静默剥掉**；
 *  · `→ 域名关键词`：变子串匹配（`example.com` 命中 `notexample.com.evil.tld`）；
 *  · `→ 域名正则`：`.` 从字面点变成**通配**（这条最凶，故 `domainRegex` 永不接受任何带过来的值）；
 *  · `ipCidr → sourceIpCidr` / `port → sourcePort`：方向相反（目的 ↔ 来源）。
 * 一个「能换类型但保留值」的控件，是把「替换一个条件」伪装成「改一个字段」。
 *
 * 切到**同一个**类型是 no-op（`Csel` 重选当前项也会触发 onChange —— 不挡就会在用户重新点开
 * 下拉、又选了原来那一项时把已填内容清光）。
 */
export function setCondTypeAt(conds: readonly Cond[], i: number, tp: RuleType): Cond[] {
  return conds.map((c, idx) => (idx === i && c.t !== tp ? { t: tp, v: '' } : c));
}

/** 该条件当前已选中的值（小写归一 —— geo 标签大小写不敏感、Windows 进程名亦然）。 */
export function selectedValueSet(v: string): Set<string> {
  return new Set(splitVals(v).map((s) => s.toLowerCase()));
}

/**
 * 勾选 / 取消一个候选值。取消按**小写**比对（勾选态就是这么算的，两处必须同口径，否则会出现
 * 「显示为勾上、点一下取消不掉」）；追加保留原样大小写。
 */
export function toggleCondValueAt(conds: readonly Cond[], i: number, value: string): Cond[] {
  const lv = value.toLowerCase();
  return conds.map((c, idx) => {
    if (idx !== i) return c;
    const cur = splitVals(c.v);
    const next = cur.some((x) => x.toLowerCase() === lv)
      ? cur.filter((x) => x.toLowerCase() !== lv)
      : [...cur, value];
    return { ...c, v: next.join(', ') };
  });
}

// ─────────────────────────────────────────────────────────────────────────────
// 候选池 → 勾选项
// ─────────────────────────────────────────────────────────────────────────────

/** 候选池的分组（`GroupAxis:'origin'` 的两个取值）。 */
export type RuleValueGroup = 'builtin' | 'external';

export interface RuleValueOption {
  /** 勾选后写进条件值的串。 */
  value: string;
  /** 显示文本。 */
  label: string;
  /** 副文本（tooltip：进程路径 / 资源 id）。 */
  hint?: string;
  /** 分组键（描述符 `groupBy` 为 null 时恒 undefined）。 */
  group?: RuleValueGroup;
  /** 该项不来自候选池，而是「已选但池里没有」（见 [`offPoolSelectedOptions`]）。 */
  offPool?: boolean;
  /** 检索语料（已小写），字段由描述符的 `searchFields` 声明。 */
  search: readonly string[];
}

/** 按描述符声明的字段名从原始记录取检索语料。 */
const corpus = (
  raw: Readonly<Record<string, string | undefined>>,
  fields: readonly string[]
): string[] =>
  fields
    .map((f) => raw[f])
    .filter((s): s is string => !!s)
    .map((s) => s.toLowerCase());

/**
 * geoTag 池 → 勾选项。三个类型共用 `api.ruleResources.list()` 这一个数据源，差别只在**寻址**：
 *  · `res-id`（规则集）：全量，值 = `res:<资源 id>`；
 *  · `bare`（geosite/geoip）：只取 tag 以 `<类型 id>-` 打头的，值 = 去前缀后的裸 tag ——
 *    前缀从**描述符 id 自己**推，故第 16 个 geo 类型无需改本函数。
 *
 * 分组判据 = `item.builtin === true`，即 Rust `is_bundled_geo_tag` 的前端投影（同 `rule-set-pick.ts`
 * 的头注），不得另立门户（按 id 前缀猜 / 按 category 猜）。
 */
export function geoPoolOptions(
  type: RuleType,
  items: readonly RuleResourceListItem[]
): RuleValueOption[] {
  const src = RULE_TYPES[type].source;
  if (src.kind !== 'pool' || src.pool !== 'geoTag') return [];
  const prefix = `${type}-`;
  const out: RuleValueOption[] = [];
  const seen = new Set<string>();
  for (const it of items) {
    const tag = geoTagOf(it.id);
    let value: string;
    let label: string;
    let raw: Record<string, string | undefined>;
    if (src.addressing === 'res-id') {
      value = ruleSetRef(it.id);
      label = it.name;
      raw = { name: it.name, id: it.id, tag };
    } else {
      if (!tag.startsWith(prefix)) continue;
      const bare = tag.slice(prefix.length);
      if (!bare) continue;
      value = bare;
      label = bare;
      raw = { tag: bare, name: it.name, id: it.id };
    }
    // fail-closed：勾得出来的必须存得下去。过不了 `validateRuleValue` 的候选一律不上架 ——
    // 上架一个点一下就在保存时被拒的 chip，比不给候选更糟（用户会以为是保存坏了）。
    if (seen.has(value.toLowerCase()) || !validateRuleValue(type, value)) continue;
    seen.add(value.toLowerCase());
    out.push({
      value,
      label,
      hint: it.id,
      group: it.builtin === true ? 'builtin' : 'external',
      search: corpus(raw, src.searchFields),
    });
  }
  return out;
}

/**
 * 进程池 → 勾选项。`proc-path` 会把**无 path 的进程整条剔掉**（Windows 的 `tasklist` 不给路径 ⇒
 * `path` 恒 `None`；Linux 内核线程无 `exe`、回落 `comm`，本机实测 356 个名字里 272 个无 path）——
 * 回落成进程名会产出一个过不了 `validateRuleValue('processPath', …)` 的值，勾一下就等于埋一颗
 * 保存时才炸的雷。剔掉后该平台的勾选区可能是空的，手填腿仍在（`allowFreeInput`）。
 */
export function processPoolOptions(
  type: RuleType,
  procs: readonly SystemProcessInfo[]
): RuleValueOption[] {
  const src = RULE_TYPES[type].source;
  if (src.kind !== 'pool' || src.pool !== 'process') return [];
  const wantPath = src.addressing === 'proc-path';
  const out: RuleValueOption[] = [];
  const seen = new Set<string>();
  for (const p of procs) {
    const value = wantPath ? p.path : p.name;
    // fail-closed 同上。真实命中：Linux 内核线程名含 `/`（`kworker/0:1`），过不了
    // `validateRuleValue('processName', …)` 的「不含路径分隔符」判据。
    if (!value || seen.has(value.toLowerCase()) || !validateRuleValue(type, value)) continue;
    seen.add(value.toLowerCase());
    out.push({ value, label: p.name, hint: p.path, search: corpus({ name: p.name, path: p.path }, src.searchFields) });
  }
  return out;
}

/**
 * 候选排序 —— **已选（快照）> 内置 > 名称**。
 *
 * `selected` 必须是「打开弹窗 / 切换条件类型」那一刻的**快照**，不是实时勾选态。这不是两条需求
 * 而是配套的一条（陈先生 2026-07-30：「已下载、已选择的……优先靠前排序。**编辑过程不动排序**」）：
 * 按实时态排，每勾一个它立刻跳到顶部、列表在手底下乱动，是最差的交互。
 * 故本函数**只**接一个显式传入的集合，自己不去读任何实时来源 —— 退化只可能发生在调用点，
 * 那一侧由 `rule-cond.test.ts` 的接线门守。
 *
 * 「内置优先」只对 geo 池成立；进程池的项 `group` 恒 undefined ⇒ 该级恒相等，自动退化成
 * 「已选 > 名称」。故一个函数覆盖两个池，不按 `pool` 分叉、更不点名类型。
 *
 * 注：候选池里的项**全部是本地已有 / 当前在跑的**（源为 `api.ruleResources.list()` /
 * `api.system.listProcesses()`）⇒「已下载」在这里不是区分维度，`builtin`/`external` 才是。
 * 陈先生说的「已下载优先」在**资源库**那边才是真维度。
 */
export function sortRuleValueOptions(
  options: readonly RuleValueOption[],
  selected: ReadonlySet<string>
): RuleValueOption[] {
  const selRank = (o: RuleValueOption) => (selected.has(o.value.toLowerCase()) ? 0 : 1);
  const grpRank = (o: RuleValueOption) => (o.group === 'external' ? 1 : 0);
  // Array#sort 自 ES2019 起保证稳定 ⇒ 三级键全相等时保持投影顺序，不需要再挂一个下标兜底。
  return options.slice().sort((a, b) => {
    const s = selRank(a) - selRank(b);
    if (s !== 0) return s;
    const g = grpRank(a) - grpRank(b);
    if (g !== 0) return g;
    return a.label.localeCompare(b.label);
  });
}

/**
 * 「已选、但候选池里没有」的值 → 勾选项（恒 `offPool: true`，供调用方显式标注）。
 *
 * 为什么必须有：候选池只列**本地已有 / 当前在跑**的项，而已存在的规则里完全可能有池外的值 ——
 * 手填的 `res:<id>`、引用了上游有而本地还没下载的 tag、给未运行的应用建的进程规则。
 * 文本框折叠之后，这些值若不在勾选区露面就**看不见也删不掉** —— 那不是「避免误修改」，
 * 是「无法修改」。这同时修掉一个既有盲区：改动前勾选区只映射候选池，引用了未下载 tag 的值
 * 在勾选区**根本不出现**，用户只能靠文本框看见它们。
 *
 * 比对按小写（与 `selectedValueSet` / `toggleCondValueAt` 同口径，三处必须同口径，否则会出现
 * 「显示为池外、点一下取消不掉」）；输出保留原样大小写，点掉时才能与文本里那份对上。
 */
export function offPoolSelectedOptions(
  value: string,
  pool: readonly RuleValueOption[],
  hint: string
): RuleValueOption[] {
  const inPool = new Set(pool.map((o) => o.value.toLowerCase()));
  const out: RuleValueOption[] = [];
  const seen = new Set<string>();
  for (const v of splitVals(value)) {
    const lv = v.toLowerCase();
    if (inPool.has(lv) || seen.has(lv)) continue;
    seen.add(lv);
    out.push({ value: v, label: v, hint, offPool: true, search: [lv] });
  }
  return out;
}

/**
 * 提交前逐值校验 —— 返回**非法值**（空数组 = 可提交）。判据复用 `domain/rules.ts` 的
 * `validateRuleValue`（15/15 全覆盖，与 Rust 权威同源），不在此重写一份。
 *
 * 此前提交只校验「名称非空」+「至少一个条件有值」，**对值本身零校验**，全靠后端返 `RULE_INVALID`
 * 再回显 —— 一次往返之后才知道自己那行 `10.0.0.0/40` 不合法。
 *
 * 为什么 15 个类型全校验、而不只校验 10 个自由输入类型：勾选面已被 [`geoPoolOptions`] /
 * [`processPoolOptions`] 用同一个 `validateRuleValue` 过滤过（勾得出来的必然合法），故这里
 * 真正拦下的恰恰是**池类型里手填的那部分**——而那正是 `allowFreeInput` 敞开的口子。
 */
export function invalidCondValues(conds: readonly Cond[]): Array<{ type: RuleType; value: string }> {
  const out: Array<{ type: RuleType; value: string }> = [];
  for (const c of conds) {
    for (const v of splitVals(c.v)) {
      if (!validateRuleValue(c.t, v)) out.push({ type: c.t, value: v });
    }
  }
  return out;
}

/** 按检索词过滤（空词 = 原样副本）。语料在投影时就按描述符的 `searchFields` 算好了。 */
export function matchRuleValueOptions(
  options: readonly RuleValueOption[],
  query: string
): RuleValueOption[] {
  const q = query.trim().toLowerCase();
  if (!q) return options.slice();
  return options.filter((o) => o.search.some((s) => s.includes(q)));
}

/** 测试匹配（客户端启发式，对齐原型 ruleTest :5090；权威匹配在内核，仅即时反馈）。 */
export type TestResult = 'empty' | 'hit' | 'miss' | 'untestable';

/** 被测输入像不像 IP（v4 点分 / 含冒号的 v6，允许带掩码）。 */
const looksLikeIp = (v: string): boolean =>
  /^\d{1,3}(\.\d{1,3}){3}(\/\d+)?$/.test(v) || (/^[0-9a-f:]+(\/\d+)?$/i.test(v) && v.includes(':'));

/**
 * 逐条件算命中，再按 AND/OR 合成 —— **值已拆好**的形态（规则实体 `RuleCondition[]` 的形状）。
 *
 * 「不适用」（域名轴的类型收到 IP 输入 / 反之 / 该类型压根没有 test）**不参与合成**，
 * 而不是记作未命中 —— 否则一条「域名 + 端口」的 AND 规则永远测不出命中（端口条件恒 false）。
 * 一条都不适用 ⇒ `untestable`。
 *
 * 为什么与 [`computeTestMatch`] 分成两层：草稿态的值是**一个多值串**（要 `splitVals`），而已落盘的
 * 规则值本来就是数组。让规则实体侧先 `join` 再被拆一次会踩到 `domainRegex` ——`^a{1,3}$` 里的逗号
 * 会被 `splitVals` 当成分隔符，一条正则被拆成两条乱码。合成逻辑只此一份，拆分只发生在草稿那一侧。
 */
export function matchConditionValues(
  conds: readonly { readonly type: RuleType; readonly values: readonly string[] }[],
  logic: 'and' | 'or',
  input: string
): TestResult {
  const raw = input.trim();
  if (!raw) return 'empty';
  const probe = { raw, lower: raw.toLowerCase() };
  const isIp = looksLikeIp(raw);
  const applied: boolean[] = [];
  for (const c of conds) {
    const toks = c.values.map((s) => s.trim().toLowerCase()).filter(Boolean);
    if (!toks.length) continue;
    const spec = RULE_TYPES[c.type]?.test;
    if (!spec) continue;
    if ((spec.axis === 'ip') !== isIp) continue; // 轴不匹配 = 不适用，不是未命中
    applied.push(spec.match(toks, probe));
  }
  if (!applied.length) return 'untestable';
  const hit = logic === 'or' ? applied.some(Boolean) : applied.every(Boolean);
  return hit ? 'hit' : 'miss';
}

/** 草稿态（多值串）的「测试匹配」——拆值后交给 [`matchConditionValues`]，合成逻辑不重写第二份。 */
export function computeTestMatch(conds: readonly Cond[], logic: 'and' | 'or', input: string): TestResult {
  return matchConditionValues(
    conds.map((c) => ({ type: c.t, values: splitVals(c.v) })),
    logic,
    input
  );
}
