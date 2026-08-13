//! 内置应用分流预设 —— **Rust 是单一真值（SoT）**，前端经 `app_presets_list` command 拉取。
//!
//! 表本体见 `app_rules_preset_data.rs`（16 条，行是维护单元）。本模块提供两个投影 + 消费函数：
//! - [`AppPreset`]（`all_presets()`）：路由生成消费的子集，**不含 UI 列**（builder 零污染）。
//! - [`AppPresetDto`]（`all_presets_dto()`）：全列（含 labelKey/emoji/iconUrl），下发前端渲染。
//!
//! 历史：本模块曾是 `src/shared/app-rules-preset.ts` 的手抄投影（TS 为真源）。现已反转，TS 表已删。

#![forbid(unsafe_code)]

use crate::user_config::rule::{AppRule, CustomAppPreset, RuleAction};
use serde::Serialize;
use std::collections::HashSet;

/// 应用分流预设（路由生成消费的子集）。上游 `AppPreset` 后端投影。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPreset {
    pub id: String,
    pub geosite_tags: Vec<String>,
    pub geoip_tags: Vec<String>,
    pub process_names: Vec<String>,
    pub category: String,
}

/// 内置预设全列 DTO —— `app_presets_list` command 的载荷，对齐前端 `AppPreset` interface
/// （`ui/src/shared/app-rules-preset.ts`）逐字段。
///
/// **键名契约**：`rename_all = "camelCase"` 产出 `labelKey`/`iconUrl`/`geositeTags`/`geoipTags`/
/// `processNames` —— 与 TS interface 一致。改键名 = 破坏前端渲染，且 **tsc 抓不到**（invoke 返回值
/// 是 as-cast）→ 由 `tests/frontend_sot_guard.rs` 锁死。
///
/// `iconUrl` 用 `Option` + `skip_serializing_if`：对齐 TS `iconUrl?: string`（自定义预设可无图标）。
/// `geoipTags`/`processNames` 恒序列化为数组（TS 侧标 `?` 但消费点全是 `|| []` / `.some()`，
/// 空数组与缺省等价，恒发更简单）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppPresetDto {
    pub id: String,
    pub label_key: String,
    pub emoji: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    pub geosite_tags: Vec<String>,
    pub geoip_tags: Vec<String>,
    pub process_names: Vec<String>,
    pub category: String,
}

include!("app_rules_preset_data.rs");

/// 根据 appId 查找预设（先内置，后自定义）。上游 `getAppPreset`。
///
/// 内置预设查 `all_presets()`；自定义预设查 `custom_presets`，将 `CustomAppPreset` 转为兼容格式。
/// 找不到返回 `None`。
pub fn get_app_preset(app_id: &str, custom_presets: &[CustomAppPreset]) -> Option<AppPreset> {
    let builtin = all_presets();
    if let Some(p) = builtin.iter().find(|p| p.id == app_id) {
        return Some(p.clone());
    }
    custom_presets
        .iter()
        .find(|p| p.id == app_id)
        .map(|c| AppPreset {
            id: c.id.clone(),
            geosite_tags: c.geosite_tags.clone(),
            geoip_tags: c.geoip_tags.clone(),
            process_names: c.process_names.clone().unwrap_or_default(),
            // 后端不消费 category；分组呈现由渲染层直接读 custom.category。
            category: "tools".to_string(),
        })
}

/// 按 appId 查全列预设（先内置，后自定义）——[`get_app_preset`] 的 UI 列版本。
///
/// 与 [`get_app_preset`] 的唯一差别是**多带 UI 列**（labelKey/emoji/iconUrl）。消费方：
/// `rule_resource_refs::enumerate_resource_refs` 要 `labelKey` 当引用徽标文案。
///
/// 自定义预设的 `labelKey` 取 `name`（自定义应用直接存名称，非 i18n key —— 渲染端据
/// `RuleResourceRef.appBuiltin` 决定要不要过 i18n）；`category` 取**真实值**而非
/// [`get_app_preset`] 那个 `"tools"` 占位 —— 后者的占位注释「后端不消费 category」在后端成立，
/// 但本 DTO 是给渲染端的，分组呈现要真值。
pub fn get_app_preset_dto(
    app_id: &str,
    custom_presets: &[CustomAppPreset],
) -> Option<AppPresetDto> {
    if let Some(p) = all_presets_dto().into_iter().find(|p| p.id == app_id) {
        return Some(p);
    }
    custom_presets
        .iter()
        .find(|p| p.id == app_id)
        .map(|c| AppPresetDto {
            id: c.id.clone(),
            label_key: c.name.clone(),
            emoji: c.emoji.clone(),
            icon_url: c.icon_url.clone(),
            geosite_tags: c.geosite_tags.clone(),
            geoip_tags: c.geoip_tags.clone(),
            process_names: c.process_names.clone().unwrap_or_default(),
            category: c.category.clone().unwrap_or_else(|| "tools".to_string()),
        })
}

/// 默认应用分流规则：为每个内置预设生成「代理·跟全局」规则。上游 `defaultAppRules`。
pub fn default_app_rules() -> Vec<AppRule> {
    all_presets()
        .iter()
        .map(|p| AppRule {
            app_id: p.id.clone(),
            action: RuleAction::Proxy,
            enabled: true,
            target_server_id: None,
        })
        .collect()
}

/// 一次性默认注入合并（幂等）。上游 `seedDefaultAppRules`。
///
/// 为未配置的预设补默认规则；剔除已下线预设的残留规则；保留用户已配置的预设规则与自定义 app（custom-*）。
pub fn seed_default_app_rules(existing: &[AppRule]) -> Vec<AppRule> {
    let presets = all_presets();
    let valid_ids: HashSet<String> = presets.iter().map(|p| p.id.clone()).collect();
    let kept: Vec<AppRule> = existing
        .iter()
        .filter(|r| valid_ids.contains(&r.app_id) || r.app_id.starts_with("custom-"))
        .cloned()
        .collect();
    let have: HashSet<String> = kept.iter().map(|r| r.app_id.clone()).collect();
    let mut result = kept;
    for r in default_app_rules() {
        if !have.contains(&r.app_id) {
            result.push(r);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_presets_include_youtube_telegram() {
        let presets = all_presets();
        let ids: Vec<&str> = presets.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"youtube"));
        assert!(ids.contains(&"telegram"));
        assert!(ids.contains(&"steam"));
    }

    #[test]
    fn get_builtin_preset() {
        let p = get_app_preset("youtube", &[]).unwrap();
        assert!(p.geosite_tags.contains(&"youtube".to_string()));
    }

    #[test]
    fn get_custom_preset() {
        let custom = vec![CustomAppPreset {
            id: "custom-foo".into(),
            name: "Foo".into(),
            emoji: "🚀".into(),
            icon_url: None,
            geosite_tags: vec!["foo".into()],
            geoip_tags: vec![],
            process_names: Some(vec!["FooApp".into()]),
            category: Some("tools".into()),
        }];
        let p = get_app_preset("custom-foo", &custom).unwrap();
        assert!(p.geosite_tags.contains(&"foo".to_string()));
        assert!(p.process_names.contains(&"FooApp".to_string()));
    }

    #[test]
    fn get_unknown_returns_none() {
        assert!(get_app_preset("nonexistent", &[]).is_none());
    }

    #[test]
    fn default_rules_one_per_builtin() {
        let rules = default_app_rules();
        let presets = all_presets();
        assert_eq!(rules.len(), presets.len());
        assert!(rules
            .iter()
            .all(|r| r.enabled && r.action == RuleAction::Proxy));
    }

    #[test]
    fn seed_keeps_custom_and_fills_missing() {
        let existing = vec![AppRule {
            app_id: "custom-x".into(),
            action: RuleAction::Direct,
            enabled: true,
            target_server_id: None,
        }];
        let seeded = seed_default_app_rules(&existing);
        // custom-x 保留
        assert!(seeded.iter().any(|r| r.app_id == "custom-x"));
        // 内置全补
        for p in all_presets() {
            assert!(seeded.iter().any(|r| r.app_id == p.id), "missing {}", p.id);
        }
    }

    #[test]
    fn seed_drops_offline_preset() {
        let existing = vec![AppRule {
            app_id: "bilibili".into(), // 已下线
            action: RuleAction::Proxy,
            enabled: true,
            target_server_id: None,
        }];
        let seeded = seed_default_app_rules(&existing);
        assert!(!seeded.iter().any(|r| r.app_id == "bilibili"));
    }

    // ── DTO 门 ──────────────────────────────────────────────────────────
    //
    // 「前端不得再有第二份表」那几条**读前端源码**的门在 `tests/frontend_sot_guard.rs`（集成测试，
    // 与 catalog 的同类门同处一室、共用剥注释器）。本处只放**纯 Rust 侧**的自洽性门。

    #[test]
    fn dto_keys_are_camel_case_contract() {
        // DTO 键名是跨语言契约（前端 AppPreset interface）。与前端字段的对差在
        // tests/frontend_sot_guard.rs::frontend_preset_interface_matches_rust_dto_fields。
        let dto = &all_presets_dto()[0];
        let json = serde_json::to_value(dto).expect("DTO 序列化");
        let obj = json.as_object().expect("DTO 应为对象");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "category",
                "emoji",
                "geoipTags",
                "geositeTags",
                "iconUrl",
                "id",
                "labelKey",
                "processNames"
            ],
            "DTO 键名与前端 AppPreset interface 漂移"
        );
    }

    #[test]
    fn dto_and_routing_projection_share_one_table() {
        // 两个投影必须来自同一张表：条数相同、id 逐条同序。若有人给某个投影另建数据源 → 红。
        let routing = all_presets();
        let dto = all_presets_dto();
        assert_eq!(routing.len(), 16, "内置预设 16 条");
        assert_eq!(routing.len(), dto.len(), "两投影条数不一致 → 数据源分裂");
        for (r, d) in routing.iter().zip(dto.iter()) {
            assert_eq!(r.id, d.id, "两投影 id 序列不一致 → 数据源分裂");
            assert_eq!(r.geosite_tags, d.geosite_tags);
            assert_eq!(r.geoip_tags, d.geoip_tags);
            assert_eq!(r.process_names, d.process_names);
            assert_eq!(r.category, d.category);
        }
    }

    #[test]
    fn dto_ui_columns_populated() {
        // UI 列不得空——空 labelKey 会让卡片显示 id，空 emoji 让 iconUrl 失败时无兜底。
        for p in all_presets_dto() {
            assert!(!p.label_key.is_empty(), "{} 缺 labelKey", p.id);
            assert!(
                !p.emoji.is_empty(),
                "{} 缺 emoji（iconUrl 失败即无兜底）",
                p.id
            );
            let url = p.icon_url.as_deref().unwrap_or("");
            assert!(
                url.starts_with("https://"),
                "{} 的 iconUrl 非 https（{url:?}）",
                p.id
            );
        }
    }

    #[test]
    fn dto_geo_tags_are_covered_by_builtin_rulesets() {
        // 每条预设引用的 geo tag 必须在随包内置 geo 规则集里，否则该应用的域名兜底规则会被
        // fail-closed 剪枝（route.rs:998）→ 应用分流只剩进程名生效。加预设漏加 tag → 红。
        let (geosite, geoip) = crate::user_config::builtin_geo_rulesets::app_geo_tags();
        for p in all_presets_dto() {
            for t in &p.geosite_tags {
                let tag = format!("geosite-{}", t.to_ascii_lowercase());
                assert!(
                    geosite.contains(&tag),
                    "预设 {} 引用 {tag}，但 builtin_geo_rulesets 的 APP_GEOSITE_TAGS 没有它",
                    p.id
                );
            }
            for t in &p.geoip_tags {
                let tag = format!("geoip-{}", t.to_ascii_lowercase());
                assert!(
                    geoip.contains(&tag),
                    "预设 {} 引用 {tag}，但 builtin_geo_rulesets 的 APP_GEOIP_TAGS 没有它",
                    p.id
                );
            }
        }
    }

    #[test]
    fn dto_lookup_builtin_wins_and_custom_keeps_real_category() {
        // 内置优先（自定义影子不了内置 id）。
        let shadow = vec![CustomAppPreset {
            id: "youtube".into(),
            name: "Shadow".into(),
            emoji: "x".into(),
            icon_url: None,
            geosite_tags: vec!["evil".into()],
            geoip_tags: vec![],
            process_names: None,
            category: Some("game".into()),
        }];
        let p = get_app_preset_dto("youtube", &shadow).unwrap();
        assert_eq!(p.label_key, "youtube");
        assert_eq!(p.geosite_tags, vec!["youtube".to_string()]);

        // 自定义：labelKey 取 name，category 取真值（非 get_app_preset 那个 "tools" 占位）。
        let custom = vec![CustomAppPreset {
            id: "custom-foo".into(),
            name: "我的 Foo".into(),
            emoji: "🚀".into(),
            icon_url: Some("https://e.com/f.png".into()),
            geosite_tags: vec!["foo".into()],
            geoip_tags: vec![],
            process_names: Some(vec!["FooApp".into()]),
            category: Some("game".into()),
        }];
        let p = get_app_preset_dto("custom-foo", &custom).unwrap();
        assert_eq!(p.label_key, "我的 Foo");
        assert_eq!(
            p.category, "game",
            "自定义预设的 category 应取真值供渲染端分组"
        );
        assert_eq!(p.process_names, vec!["FooApp".to_string()]);

        assert!(get_app_preset_dto("nonexistent", &[]).is_none());
    }
}
