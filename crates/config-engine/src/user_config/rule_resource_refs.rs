//! 规则资源引用枚举（**反向**：资源 → 引用它的规则）—— `rule_resources_list` 的 `referencedBy`
//! 与 `rule_resources_delete` 的 `referencingRules` 的单一真值。
//!
//! # 为什么在 Rust 而不是前端
//!
//! 前端 `ui/src/shared/rule-resource-refs.ts` 有同一份逻辑。审计 §B 判它「前端展示派生，合理」——
//! 那是在**消费方未定**时的判断。现在消费方定了：`referencedBy` / `referencingRules` 是
//! **command 返回的 DTO 字段**（`RuleResourceListItem` / `RuleResourceDeleteResult`），由后端算完下发。
//! 依 ~/docs/polaris/design/polaris-dialog-layer-and-governance.md §3.1 Q2：
//! 「**Rust 预计算派生字段随 DTO 下发（零漂移）> 前端谓词（有漂移敞口）**」→ 反向枚举归 Rust。
//!
//! 前端保留的是**正向**判定（`availableResourceTagSet` / `ruleHasMissingResource` /
//! `missingResourceRuleIds` / `missingResourceAppIds`）—— 那些是渲染期就地角标（③类即时门控谓词），
//! 输入已在 store、错了只是角标瑕疵，且不进 config。两者互补，不是重复。
//!
//! # 已登记缺口：`kind: "system"` 未实现
//!
//! `RuleResourceRef.kind` 的 TS 定义含 `'system'`，注释称「智能分流 geo 基线层对内置默认
//! (geosite-cn/geoip-cn/geolocation-!cn) 的隐式引用（**纳入 referencedBy 计数**）」。
//! **但 TS 侧 `enumerateResourceRefs` 从未 emit 过 system** —— 类型宣称的能力实现里没有。
//! 本移植保持 1:1（route + app 两类），**不擅自新增行为**：`smart_baseline_geo_tags()` 就在手边
//! （`region_routing.rs:94`），但要不要把基线算作引用、算了之后 geosite-cn 是否就删不掉，
//! 是产品判断，且当前无消费方（资源库弹窗未实现）。留待弹窗批与 UI 一起定。

#![forbid(unsafe_code)]

use serde::Serialize;

use crate::user_config::app_rules_preset::get_app_preset_dto;
use crate::user_config::builtin_geo_rulesets::BUILTIN_ID_PREFIX;
use crate::user_config::rule::{AppRule, CustomAppPreset, Rule, RuleType};
use crate::user_config::rules::rule_conditions;

/// 引用记录。上游 `RuleResourceRef`（`ui/src/shared/types/rules.ts:137`）。
///
/// `label`：route = 备注，无备注则首条件摘要；app = 内置 `labelKey`(i18n key) 或自定义 `name`
/// —— **渲染端据 `appBuiltin` 决定要不要过 i18n**，故这两者必须同时下发。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleResourceRef {
    /// `route` = 自定义路由规则；`app` = 应用分流卡片。（`system` 见模块文档，未实现。）
    pub kind: String,
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_builtin: Option<bool>,
}

/// 扫描输入。上游 `RefScanInput`。
#[derive(Debug, Default, Clone, Copy)]
pub struct RefScanInput<'a> {
    pub custom_rules: &'a [Rule],
    pub app_rules: &'a [AppRule],
    pub custom_app_presets: &'a [CustomAppPreset],
}

/// 归一 resId 为 geo tag：`builtin:geosite-x` → `geosite-x`；其余原样（`geosite-amazon` / `res_xxx`）。
/// 上游 `geoTagOf`。
pub fn geo_tag_of(res_id: &str) -> &str {
    res_id.strip_prefix(BUILTIN_ID_PREFIX).unwrap_or(res_id)
}

/// 把 `RuleType` 转成 TS 侧同名字符串（`ruleSet` / `domainSuffix` …）。
///
/// **走 serde 而非手写 match**：`RuleType` 的 `rename_all = "camelCase"` 是键名真值，手写第二张
/// 映射表就是又一处必然漂移的副本。
fn rule_type_str(t: RuleType) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// 拆 geo tag 为 (kind, name)：`geosite-youtube` → `(Geosite, "youtube")`。非 geo 形态 → None。
fn split_geo_tag(tag: &str) -> Option<(RuleType, String)> {
    // 注意 name 可含 '-'（`geolocation-!cn`）→ 只切首个分隔符，对齐 TS 的 `^(geosite|geoip)-(.+)$`。
    for (prefix, kind) in [("geosite-", RuleType::Geosite), ("geoip-", RuleType::Geoip)] {
        if let Some(rest) = tag.strip_prefix(prefix) {
            if rest.is_empty() {
                return None; // `(.+)` 要求至少 1 字符
            }
            return Some((kind, rest.trim().to_ascii_lowercase()));
        }
    }
    None
}

/// 枚举 resId 被哪些**已启用**规则引用。上游 `enumerateResourceRefs`。
///
/// resId 口径：`geosite-<tag>` / `geoip-<tag>` / `builtin:<tag>` / `res_<id>`。三类引用：
/// - customRules 的 `ruleSet` 条件 `res:<resId>`（精确串比）；
/// - customRules 的 geosite/geoip **类型条件**（值是裸 tag，如 `youtube`）；
/// - appRules 经 preset 的 geositeTags/geoipTags **间接引用**。
///
/// 后两类是历史缺口的补丁：原 RuleResourceManager 只扫第一类 → 删这些 geo 资源时不提示、
/// 补回后不触发 reload。
pub fn enumerate_resource_refs(res_id: &str, input: &RefScanInput<'_>) -> Vec<RuleResourceRef> {
    let mut refs: Vec<RuleResourceRef> = Vec::new();
    let res_ref = format!("res:{res_id}");
    let geo = split_geo_tag(geo_tag_of(res_id));

    // ① / ② 自定义路由规则
    for rule in input.custom_rules {
        if !rule.enabled {
            continue;
        }
        let conds = rule_conditions(rule);
        let matched = conds.iter().any(|c| {
            if c.type_field == RuleType::RuleSet && c.values.iter().any(|v| v == &res_ref) {
                return true;
            }
            match &geo {
                Some((kind, name)) if c.type_field == *kind => c
                    .values
                    .iter()
                    .any(|v| v.trim().to_ascii_lowercase() == *name),
                _ => false,
            }
        });
        if !matched {
            continue;
        }
        // label：备注优先；无备注取首条件摘要 `type: value`（值截 24 字符）。
        let label = rule
            .remarks
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| match conds.first() {
                Some(c0) => {
                    let v0 = c0.values.first().map(String::as_str).unwrap_or("");
                    // TS 用 `.slice(0,24)`（UTF-16 码元）；此处按 char 截。规则值实际是域名/IP/端口
                    // 等 ASCII，两者等价；真出现非 BMP 字符时长度可能差一两个字符，纯展示摘要，
                    // 不影响任何判定。
                    let v0: String = v0.chars().take(24).collect();
                    format!("{}: {}", rule_type_str(c0.type_field), v0)
                }
                None => rule_type_str(rule.type_field),
            });
        refs.push(RuleResourceRef {
            kind: "route".to_string(),
            id: rule.id.clone(),
            label,
            app_builtin: None,
        });
    }

    // ③ 应用分流：仅 geo tag 类资源可被 preset 引用
    if let Some((kind, name)) = geo {
        for ar in input.app_rules {
            if !ar.enabled {
                continue;
            }
            let preset = match get_app_preset_dto(&ar.app_id, input.custom_app_presets) {
                Some(p) => p,
                None => continue,
            };
            let tags = if kind == RuleType::Geosite {
                &preset.geosite_tags
            } else {
                &preset.geoip_tags
            };
            if tags.iter().any(|t| t.trim().to_ascii_lowercase() == name) {
                refs.push(RuleResourceRef {
                    kind: "app".to_string(),
                    id: ar.app_id.clone(),
                    label: preset.label_key.clone(),
                    // 内置 = 非 custom- 前缀 → 渲染端据此决定 label 是否过 i18n。
                    app_builtin: Some(!ar.app_id.starts_with("custom-")),
                });
            }
        }
    }

    refs
}

/// 是否被任意启用规则引用。上游 `isResourceReferenced`。
pub fn is_resource_referenced(res_id: &str, input: &RefScanInput<'_>) -> bool {
    !enumerate_resource_refs(res_id, input).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::rule::{RuleAction, RuleCondition};

    fn rule(id: &str, t: RuleType, values: &[&str], enabled: bool) -> Rule {
        Rule {
            id: id.into(),
            type_field: t,
            values: values.iter().map(|s| (*s).to_string()).collect(),
            enabled,
            action: RuleAction::Proxy,
            ..Default::default()
        }
    }

    fn app_rule(id: &str, enabled: bool) -> AppRule {
        AppRule {
            app_id: id.into(),
            action: RuleAction::Proxy,
            enabled,
            target_server_id: None,
        }
    }

    #[test]
    fn geo_tag_of_strips_builtin_prefix() {
        assert_eq!(geo_tag_of("builtin:geosite-cn"), "geosite-cn");
        assert_eq!(geo_tag_of("geosite-amazon"), "geosite-amazon");
        assert_eq!(geo_tag_of("res_123"), "res_123");
    }

    #[test]
    fn split_geo_tag_keeps_dashes_and_bang() {
        // `geolocation-!cn` 含 '-' 与 '!' → 只切首个前缀，其余原样（TS 的 `(.+)` 语义）。
        let (k, n) = split_geo_tag("geosite-geolocation-!cn").unwrap();
        assert_eq!(k, RuleType::Geosite);
        assert_eq!(n, "geolocation-!cn");
        assert!(split_geo_tag("res_123").is_none());
        assert!(split_geo_tag("geosite-").is_none(), "空 name 不算 geo tag");
    }

    #[test]
    fn rule_type_str_matches_serde_rename() {
        // 与 RuleType 的 rename_all="camelCase" 同源（手写映射表会漂移 → 本测试锁死）。
        assert_eq!(rule_type_str(RuleType::RuleSet), "ruleSet");
        assert_eq!(rule_type_str(RuleType::DomainSuffix), "domainSuffix");
        assert_eq!(rule_type_str(RuleType::Geosite), "geosite");
    }

    #[test]
    fn ref_via_rule_set_res_prefix() {
        let r = rule("r1", RuleType::RuleSet, &["res:geosite-amazon"], true);
        let input = RefScanInput {
            custom_rules: &[r],
            ..Default::default()
        };
        let refs = enumerate_resource_refs("geosite-amazon", &input);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, "route");
        assert_eq!(refs[0].id, "r1");
        assert_eq!(refs[0].label, "ruleSet: res:geosite-amazon");
    }

    #[test]
    fn ref_via_bare_geosite_condition() {
        // 缺口②：geosite 类型条件的值是裸 tag（`youtube`），资源 id 是 `geosite-youtube`。
        let r = rule("r2", RuleType::Geosite, &["  YouTube "], true);
        let input = RefScanInput {
            custom_rules: &[r],
            ..Default::default()
        };
        let refs = enumerate_resource_refs("geosite-youtube", &input);
        assert_eq!(refs.len(), 1, "裸 tag 引用（trim + 大小写不敏感）应命中");
        assert_eq!(refs[0].id, "r2");
    }

    #[test]
    fn geoip_condition_does_not_match_geosite_resource() {
        let r = rule("r3", RuleType::Geoip, &["cn"], true);
        let input = RefScanInput {
            custom_rules: &[r],
            ..Default::default()
        };
        assert!(enumerate_resource_refs("geosite-cn", &input).is_empty());
        assert_eq!(enumerate_resource_refs("geoip-cn", &input).len(), 1);
    }

    #[test]
    fn disabled_rule_not_counted() {
        let r = rule("r4", RuleType::Geosite, &["youtube"], false);
        let input = RefScanInput {
            custom_rules: &[r],
            ..Default::default()
        };
        assert!(enumerate_resource_refs("geosite-youtube", &input).is_empty());
    }

    #[test]
    fn remarks_win_over_condition_summary() {
        let mut r = rule("r5", RuleType::Geosite, &["youtube"], true);
        r.remarks = Some("  我的规则  ".into());
        let input = RefScanInput {
            custom_rules: &[r],
            ..Default::default()
        };
        assert_eq!(
            enumerate_resource_refs("geosite-youtube", &input)[0].label,
            "我的规则"
        );
    }

    #[test]
    fn blank_remarks_fall_back_to_summary() {
        let mut r = rule("r6", RuleType::Geosite, &["youtube"], true);
        r.remarks = Some("   ".into()); // 全空白 = 无备注（TS `.trim() || summary`）
        let input = RefScanInput {
            custom_rules: &[r],
            ..Default::default()
        };
        assert_eq!(
            enumerate_resource_refs("geosite-youtube", &input)[0].label,
            "geosite: youtube"
        );
    }

    #[test]
    fn summary_value_truncated_to_24() {
        let long = "a".repeat(50);
        let r = rule("r7", RuleType::RuleSet, &["res:geosite-amazon"], true);
        let mut r = r;
        r.conditions = Some(vec![
            RuleCondition {
                type_field: RuleType::Domain,
                values: vec![long],
            },
            RuleCondition {
                type_field: RuleType::RuleSet,
                values: vec!["res:geosite-amazon".into()],
            },
        ]);
        let input = RefScanInput {
            custom_rules: &[r],
            ..Default::default()
        };
        let refs = enumerate_resource_refs("geosite-amazon", &input);
        assert_eq!(refs.len(), 1, "多条件里任一命中即算引用");
        assert_eq!(refs[0].label, format!("domain: {}", "a".repeat(24)));
    }

    #[test]
    fn ref_via_builtin_app_preset() {
        // 缺口③：appRules 经内置 preset 间接引用 geo 资源。
        let input = RefScanInput {
            app_rules: &[app_rule("youtube", true)],
            ..Default::default()
        };
        let refs = enumerate_resource_refs("geosite-youtube", &input);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, "app");
        assert_eq!(refs[0].id, "youtube");
        assert_eq!(refs[0].label, "youtube", "内置 → label 是 i18n key");
        assert_eq!(refs[0].app_builtin, Some(true));
    }

    #[test]
    fn ref_via_builtin_app_preset_geoip() {
        // telegram 同时有 geosite 与 geoip tag。
        let input = RefScanInput {
            app_rules: &[app_rule("telegram", true)],
            ..Default::default()
        };
        assert_eq!(enumerate_resource_refs("geosite-telegram", &input).len(), 1);
        assert_eq!(enumerate_resource_refs("geoip-telegram", &input).len(), 1);
        // youtube 无 geoip tag → geoip-youtube 不该被 youtube 卡片引用。
        let input = RefScanInput {
            app_rules: &[app_rule("youtube", true)],
            ..Default::default()
        };
        assert!(enumerate_resource_refs("geoip-youtube", &input).is_empty());
    }

    #[test]
    fn ref_via_custom_app_preset_uses_name_and_flags_non_builtin() {
        let custom = vec![CustomAppPreset {
            id: "custom-foo".into(),
            name: "我的 Foo".into(),
            emoji: "🚀".into(),
            icon_url: None,
            geosite_tags: vec!["foo".into()],
            geoip_tags: vec![],
            process_names: None,
            category: Some("tools".into()),
        }];
        let input = RefScanInput {
            app_rules: &[app_rule("custom-foo", true)],
            custom_app_presets: &custom,
            ..Default::default()
        };
        let refs = enumerate_resource_refs("geosite-foo", &input);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].label, "我的 Foo", "自定义 → label 是 name 原文");
        assert_eq!(
            refs[0].app_builtin,
            Some(false),
            "custom- 前缀 → appBuiltin=false（渲染端据此不过 i18n）"
        );
    }

    #[test]
    fn disabled_app_rule_not_counted() {
        let input = RefScanInput {
            app_rules: &[app_rule("youtube", false)],
            ..Default::default()
        };
        assert!(enumerate_resource_refs("geosite-youtube", &input).is_empty());
    }

    #[test]
    fn builtin_prefixed_res_id_resolves_to_geo_tag() {
        // 资源 id 形如 `builtin:geosite-cn` 时，仍要匹配到裸 tag `cn` 的条件与 preset。
        let r = rule("r8", RuleType::Geosite, &["cn"], true);
        let input = RefScanInput {
            custom_rules: &[r],
            ..Default::default()
        };
        assert_eq!(
            enumerate_resource_refs("builtin:geosite-cn", &input).len(),
            1
        );
    }

    #[test]
    fn route_and_app_refs_accumulate() {
        let r = rule("r9", RuleType::Geosite, &["youtube"], true);
        let input = RefScanInput {
            custom_rules: &[r],
            app_rules: &[app_rule("youtube", true)],
            custom_app_presets: &[],
        };
        let refs = enumerate_resource_refs("geosite-youtube", &input);
        assert_eq!(refs.len(), 2, "route + app 两类引用应累加");
        assert!(is_resource_referenced("geosite-youtube", &input));
        assert!(!is_resource_referenced("geosite-netflix", &input));
    }

    #[test]
    fn unreferenced_resource_has_no_refs() {
        let input = RefScanInput::default();
        assert!(enumerate_resource_refs("geosite-amazon", &input).is_empty());
        assert!(!is_resource_referenced("geosite-amazon", &input));
    }
}
