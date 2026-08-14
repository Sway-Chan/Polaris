/**
 * ResourceRefsHoverCard —— 规则资源引用 hover 详情卡内容（设计文档 §1.5 refs；原型 registerTipCard('refs', …)
 * polaris-prototype.html :5205-5211 + `.ref-line` 样式 :1026）：逐条列出引用本资源的路由规则 / 应用分流卡片
 * （kind pill + 文案）。builtin geo 资源在 referencedBy===0 时展示「智能分流基线」系统行（由调用方合成，
 * 见 ResourcesScreen.tsx 的 RefBadge——与既有 sys 徽标 title="智能分流" 同一口径，非新增判定）。
 *
 * # 为什么这里重新算，而不是等 Rust 下发
 *
 * 反向枚举的权威已归 Rust（`crates/config-engine/.../rule_resource_refs.rs`），供 `referencedBy`
 * 计数与 `rule_resources_delete` 的 `referencingRules` 用——但那是**已定消费方的 DTO 字段**，唯一
 * 落地点是 delete 命令（语义是删除确认，不该被 hover 借用去 side-effect 触发）。本卡是**同步渲染路径
 * 上的即时展示谓词**，与 domain/rule-resource-refs.ts 保留的「正向」判定同一类（③类：per-row 渲染
 * 无法逐次 invoke，输入已在 store，错了只是卡片瑕疵、不进 config）。复用该文件导出的
 * ruleResourceTags/appPresetResourceTags 做同源 tag 匹配，未新增算法、不影响 referencedBy 计数本身。
 */
import { useTranslation } from 'react-i18next';
import type { AppRule, CustomAppPreset, Rule } from '@/contracts/types';
import { getAppPreset, type AppPreset } from '@/domain/app-rules-preset';
import { ruleResourceTags, appPresetResourceTags } from '@/domain/rule-resource-refs';
import { ruleConditions } from '@/domain/rules';
import { appPresetLabel } from './AppRuleHoverCard';

export interface ResourceRef {
  kind: 'route' | 'app' | 'system';
  label: string;
  /** kind==='app' 时：该应用是否自定义（决定 label 是否需要过 t()，见 appPresetLabel）；
   *  route/system 两 kind 的 label 已是最终展示文案，恒为 undefined，渲染层不查该字段。 */
  isCustom?: boolean;
}

const REF_KIND_CLS: Record<ResourceRef['kind'], string> = {
  route: 'proto',
  app: 'aur',
  system: 'region',
};

/** resId → geo tag（`builtin:geosite-x` → `geosite-x`；其余原样），口径同 Rust geo_tag_of。 */
function geoTagOf(resId: string): string {
  return resId.startsWith('builtin:') ? resId.slice('builtin:'.length) : resId;
}

/** 规则摘要文案：备注优先，否则首条件 `type: value`（截 24 字符），口径对齐 Rust enumerate_resource_refs。 */
function ruleRefLabel(rule: Rule): string {
  const remarks = rule.remarks?.trim();
  if (remarks) return remarks;
  const c0 = ruleConditions(rule)[0];
  if (!c0) return rule.type;
  const v0 = (c0.values[0] ?? '').slice(0, 24);
  return `${c0.type}: ${v0}`;
}

/**
 * 引用本资源（resId）的已启用路由规则 + 应用分流卡片（口径对齐 Rust enumerate_resource_refs：
 * ruleSet `res:<id>` 精确引用 / geosite·geoip 裸 tag 条件 / 应用预设 geositeTags·geoipTags 间接引用）。
 */
export function resourceRefs(
  resId: string,
  rules: Rule[],
  appRules: AppRule[],
  builtinPresets: AppPreset[],
  customPresets?: CustomAppPreset[],
): ResourceRef[] {
  const tag = geoTagOf(resId);
  const out: ResourceRef[] = [];
  for (const r of rules) {
    if (!r.enabled) continue;
    if (ruleResourceTags(r).includes(tag)) out.push({ kind: 'route', label: ruleRefLabel(r) });
  }
  for (const ar of appRules) {
    if (!ar.enabled) continue;
    const preset = getAppPreset(ar.appId, builtinPresets, customPresets);
    if (!preset) continue;
    if (appPresetResourceTags(preset).includes(tag)) {
      // 本函数是纯函数（无 useTranslation），故只记「是否自定义」，翻译延后到渲染层的
      // appPresetLabel（同 AppRuleHoverCard 单一收口，避免 preset.labelKey 是内置 i18n key 时
      // 被当成最终文案直出）。
      const isCustom = !builtinPresets.some((bp) => bp.id === preset.id);
      out.push({ kind: 'app', label: preset.labelKey || ar.appId, isCustom });
    }
  }
  return out;
}

/** 内容渲染：`refs` 由调用方保证非空（未引用态走既有 `.ref-badge.none` 的纯文本 `data-tip`，不挂本卡）。 */
export function ResourceRefsHoverCardContent({ refs }: { refs: ResourceRef[] }) {
  const { t } = useTranslation();
  return (
    <>
      {refs.map((r, i) => (
        <div className="ref-line" key={i}>
          <span className={`pill ${REF_KIND_CLS[r.kind]}`}>
            {t(`resources.refKind.${r.kind}`)}
          </span>
          <span>
            {r.kind === 'app' ? appPresetLabel(t, r.label, r.label, r.isCustom ?? false) : r.label}
          </span>
        </div>
      ))}
    </>
  );
}
