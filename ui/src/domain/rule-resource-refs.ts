/**
 * 规则资源引用判定（**正向**：规则 → 它引用的资源是否本地可用）。
 *
 * # 反向枚举已移交 Rust（本批）
 *
 * 「某个资源被哪些规则引用」（`enumerateResourceRefs` / `isResourceReferenced`）**已删**，
 * 移至 `crates/config-engine/src/user_config/rule_resource_refs.rs`。原因不是「前端不能算」，
 * 而是**消费方定了**：它的两个消费点 `referencedBy`（`rule_resources_list`）与 `referencingRules`
 * （`rule_resources_delete`）都是 **command 返回的 DTO 字段** —— 依治理文档 §3.1 Q2
 * 「Rust 预计算派生字段随 DTO 下发（零漂移）> 前端谓词（有漂移敞口）」，后端算完下发即可，
 * 前端再持一份就是双份维护。审计 §B 曾判它「前端展示派生，合理」——那是消费方未定时的判断。
 *
 * # 本模块保留的（③类：同步渲染路径上的即时门控谓词）
 *
 * 正向判定服务于**规则列表/应用卡片的就地「资源缺失」角标**：输入（资源列表 + 规则）已在 store，
 * 逻辑是简单谓词，漂移后果仅为角标瑕疵（不进 config、不影响配置生成）。per-row 渲染无法逐次 invoke。
 * 与反向枚举互补，不是重复：反向答「删这个资源会影响谁」，正向答「这条规则现在能不能用」。
 *
 * tag 口径严格对齐 generateCustomRules / getRequiredGeoCategories（见各函数注释）。
 * 纯函数、无 I/O。
 */
import type { Rule, AppRule, CustomAppPreset, RuleResourceRef } from '../contracts/types';
import { ruleConditions } from './rules';
import { getAppPreset, type AppPreset } from './app-rules-preset';

export type { RuleResourceRef };

/** 归一 resId 为 geo tag：`builtin:geosite-x` → `geosite-x`；其余原样（`geosite-amazon` / `res_xxx`）。
 *
 * 导出（原为模块私有）：规则弹窗的「规则集」选择器要按同一口径判「引用的资源本地是否可用」
 * （`dialogs/rule-set-pick.ts`）。**不许它自己再写一遍归一** —— 两份归一必然在 `builtin:` 前缀这一
 * 处漂移，而漂移的形态就是「弹窗说缺、列表角标说不缺」。 */
export function geoTagOf(resId: string): string {
  return resId.startsWith('builtin:') ? resId.slice('builtin:'.length) : resId;
}

/**
 * 「本地可用资源」正向判断（与 ProxyManager 运行期 fail-closed 口径一致）——回答「某条规则 / 某个应用引用的规则资源是否
 * 本地缺失（已删除或文件丢失）」，供路由规则页 / 应用分流页就地标注「资源缺失」角标。
 *
 * 与 Rust 侧 enumerate_resource_refs（反向：资源 → 引用它的规则）互补：删除会把资源整条移出
 * config.ruleResources，反向枚举便无从命中已删项；正向以「规则引用的 tag 是否在本地可用集合内」
 * 判定，删除态与文件缺失态统一收口。
 *
 * tag 口径严格对齐 generateCustomRules / getRequiredGeoCategories：
 *   - ruleSet `res:<id>` → geoTagOf(id)（builtin:geosite-cn→geosite-cn；geosite-amazon / res_x 原样）
 *   - geosite 裸 tag → `geosite-<tag>`；geoip 裸 tag → `geoip-<tag>`（trim + lowercase 归一）
 *   - 应用分流 preset.geositeTags/geoipTags → `geosite-/geoip-<tag>`
 * 可用集合 = 列表项中 fileExists 为真者的 geoTagOf(id)；仅判已启用规则 / 应用（与运行期一致，禁用规则不下发本就无效果）。
 */
export function availableResourceTagSet(
  // `readonly`：本函数只读不写，收窄成可变数组会白白拒掉 readonly 入参（`rule-set-pick.ts` 的
  // 纯判据签名即是）。既有可变数组调用点不受影响（可变可赋给 readonly，反之不行）。
  resources: readonly { id: string; fileExists: boolean }[]
): Set<string> {
  return new Set((resources || []).filter((r) => r.fileExists).map((r) => geoTagOf(r.id)));
}

/** 单条路由规则引用的全部资源 tag（口径同 generateCustomRules 的条件解析）。 */
export function ruleResourceTags(rule: Rule): string[] {
  const tags: string[] = [];
  for (const c of ruleConditions(rule)) {
    if (c.type === 'ruleSet') {
      for (const v of c.values || []) {
        const s = v.trim();
        if (s.startsWith('res:')) tags.push(geoTagOf(s.slice('res:'.length)));
      }
    } else if (c.type === 'geosite' || c.type === 'geoip') {
      for (const v of c.values || []) {
        const t = v.trim().toLowerCase();
        if (t) tags.push(`${c.type}-${t}`);
      }
    }
  }
  return tags;
}

/** 该路由规则是否引用了「本地不可用」资源（缺失 / 已删除）。仅判已启用规则。 */
export function ruleHasMissingResource(rule: Rule, available: Set<string>): boolean {
  if (!rule.enabled) return false;
  return ruleResourceTags(rule).some((t) => !available.has(t));
}

/** 引用了缺失资源的路由规则 id 集合（供路由规则列表就地角标）。 */
export function missingResourceRuleIds(rules: Rule[], available: Set<string>): Set<string> {
  const s = new Set<string>();
  for (const r of rules || []) if (ruleHasMissingResource(r, available)) s.add(r.id);
  return s;
}

/** 应用分流预设引用的全部 geo tag（口径同 generateRouteConfig 的 app 分支 / getRequiredGeoCategories）。 */
export function appPresetResourceTags(preset: {
  geositeTags: string[];
  geoipTags?: string[];
}): string[] {
  return [
    ...(preset.geositeTags || []).map((t) => `geosite-${t.trim().toLowerCase()}`),
    ...(preset.geoipTags || []).map((t) => `geoip-${t.trim().toLowerCase()}`),
  ];
}

/**
 * 引用了缺失 geo 的应用 id 集合（进程名仍生效；仅判已启用 appRule）。
 *
 * `builtinPresets` 由调用方从 `useAppPresetsStore` 传入（Rust 下发的内置表）—— 本模块不再能隐式
 * 读到模块级 APP_PRESETS 常量，表已移交 Rust。
 */
export function missingResourceAppIds(
  appRules: AppRule[],
  available: Set<string>,
  builtinPresets: AppPreset[],
  customAppPresets?: CustomAppPreset[]
): Set<string> {
  const s = new Set<string>();
  for (const ar of appRules || []) {
    if (!ar.enabled) continue;
    const preset = getAppPreset(ar.appId, builtinPresets, customAppPresets);
    if (!preset) continue;
    if (appPresetResourceTags(preset).some((t) => !available.has(t))) s.add(ar.appId);
  }
  return s;
}
