/**
 * 规则集选择器的纯判据（RuleDialog 的 `ruleSet` 条件用）—— 检索命中 / 三态判定 / 缺失引用。
 *
 * ⚠️ **`ruleSetPickGroups` 已删**：它产出的是 `CselGroup`（下拉的 optgroup），而规则集的选择
 * 已从「下拉里选一条即追加」改成勾选区（`rule-cond.ts` 的 `geoPoolOptions` + `.rv-pick`）。
 * 留着一份没有消费方的分组器，只会在两份「命中/分区」判据之间漂移。分区判据搬去了
 * `geoPoolOptions`（同一条 `item.builtin === true`），检索仍共用本文件的 `ruleSetPickMatches`。
 *
 * 为什么抽成纯函数而不留在 `RuleDialog.tsx`：本仓 vitest 是 **node 环境无 jsdom**
 * （`vite.config.ts` 的 `test.environment`），判据留在组件里等于没有门（同 `apppolicy-logic.ts`
 * 的先例，见 commit 29586cd）。本文件与 `csel-logic.ts` / `res-url-logic.ts` 同层同目录，
 * 不是新抽象层，只是把已有的行内表达式挪到可断言的位置。
 *
 * # 分区判据是**唯一真值的前端投影**，不是新判据
 *
 * 「随包内置」在 Rust 侧只有一个判据：`config-engine/user_config/builtin_geo_rulesets.rs`
 * 的 `is_bundled_geo_tag(tag)`（= 随包表 `builtin_geo_rulesets()` 全量 tag 的集合判定）。
 * `rule_resources_list`（`src-tauri/src/commands/rules.rs:474`）把该表逐条投影成列表项时写
 * `builtin: Some(true)`，用户下载的那批写 `builtin: None` ⇒ 前端 `item.builtin === true`
 * **就是**那个判据的投影，不得另立门户（例如按 id 前缀猜、按 category 猜）。
 *
 * ⚠️ **id 形态两套，勿混**：
 *  - 随包行 id = `builtin:<tag>`（`builtin_id_for`），如 `builtin:geosite-cn`；
 *  - 资源库 catalog / 已下载资源 id = 裸 `<tag>`，如 `geosite-youtube`。
 * 引用值一律 `res:<id>`（原样带上上面那两种形态之一）—— 生成端
 * `config-engine/builder/custom_rules.rs:139` 只认 `res:` 前缀，其余 warn + 跳过（fail-closed）。
 * 归一到 geo tag 的口径复用 `domain/rule-resource-refs.geoTagOf`，不在此重写。
 */
import type { RuleResourceListItem } from '@/contracts/types';
import { availableResourceTagSet, geoTagOf } from '@/domain/rule-resource-refs';

/** 引用前缀（生成端唯一认得的形态）。 */
export const RULE_SET_REF_PREFIX = 'res:';

/** 资源 id → 条件值里的引用形态。 */
export function ruleSetRef(id: string): string {
  return `${RULE_SET_REF_PREFIX}${id}`;
}

/**
 * 「挑不出来」的三种原因 + 正常态。**文案与提示行的分派唯一依据**。
 *
 * - `loading` —— 清单还在飞（`resItems === null`，惰性拉取有真实空窗期）。不是「没有」。
 * - `failed`  —— 拉过且失败了（`resItems === []`）。成功的 `rule_resources_list` **恒非空**：
 *   它无条件把随包表（`builtin_geo_rulesets()`）逐条投影进结果 ⇒ 空数组只可能来自 catch 腿。
 *   这条不变式是本判据能区分 `failed` 与 `noMatch` 的全部依据，改动 command 时要一起看。
 * - `noMatch` —— 清单拿到了，但检索把所有条目滤掉了。**只有这一态该说「无匹配」**。
 * - `ok`      —— 至少挑得出一条。
 */
export type RuleSetPickState = 'loading' | 'failed' | 'noMatch' | 'ok';

/**
 * 命中检索词的条目（`ruleSetPickGroups` 与 `ruleSetPickState` **共用**这一个过滤器）。
 *
 * 抽出来的理由就是「同一个『命中』只能有一份定义」：分组渲染与状态判定若各写一遍过滤条件，
 * 会漂移成「菜单里明明有行、提示行却说无匹配」（或反之）。
 *
 * 匹配 `name` 与 `id` 两者：随包行的 name 是裸 tag（`geosite-cn`），下载行的 name 是 catalog 名
 * （`youtube`）而只有 id 含 `geosite-` 前缀 —— 只匹配一个会让「搜 geosite」在两组里表现不一致。
 */
export function ruleSetPickMatches(
  items: readonly RuleResourceListItem[],
  query: string,
): RuleResourceListItem[] {
  const q = query.trim().toLowerCase();
  if (!q) return items.slice();
  return items.filter((r) => r.name.toLowerCase().includes(q) || r.id.toLowerCase().includes(q));
}

/**
 * 判定当前处于哪一态（纯函数）。`items === null` = 还没拉到；`[]` = 拉了但失败（见类型注释里的
 * 「成功恒非空」不变式）；其余按共用过滤器的命中数分 `noMatch` / `ok`。
 */
export function ruleSetPickState(
  items: readonly RuleResourceListItem[] | null,
  query: string,
): RuleSetPickState {
  if (items === null) return 'loading';
  if (items.length === 0) return 'failed';
  return ruleSetPickMatches(items, query).length === 0 ? 'noMatch' : 'ok';
}

/**
 * 本条件里引用了、但**本地不可用**的规则集引用（`res:<id>` 原样返回，去重保序）。
 *
 * 可用判据复用 `availableResourceTagSet`（= 列表项中 `fileExists` 为真者的 geo tag 集合）——
 * 与路由规则列表 / 应用分流卡片上那个「资源缺失」角标**同一条线**，不另立标准：同一条规则在
 * 弹窗里说「缺 1 个」、在列表上不标角标，比两处都不提示更糟。
 *
 * 为什么这个提示必须存在：`res:` 引用指向的资源被删/文件丢失后，生成 sing-box 配置时
 * `custom_rules.rs` 会 fail-closed 剪掉该条件且**只留一行 warn**（2026-07-30 真机反馈），
 * 规则静默不生效。这是唯一能在编辑期把它说出来的位置。
 */
export function missingRuleSetRefs(
  values: readonly string[],
  items: readonly RuleResourceListItem[],
): string[] {
  const available = availableResourceTagSet(items);
  const out: string[] = [];
  const seen = new Set<string>();
  for (const raw of values) {
    const v = raw.trim();
    if (!v.startsWith(RULE_SET_REF_PREFIX) || seen.has(v)) continue;
    seen.add(v);
    if (!available.has(geoTagOf(v.slice(RULE_SET_REF_PREFIX.length)))) out.push(v);
  }
  return out;
}
