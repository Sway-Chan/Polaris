//! `UserConfig` 反序列化的**键名契约**门 —— 用真实 config.json 键名喂进来，断言字段真的落位。
//!
//! # 为什么需要这扇门（审计 §K7「门开在别处」的又一活例）
//!
//! `UserConfig` 无 `rename_all`，靠**逐字段** `#[serde(rename)]` 对齐 camelCase。漏一个字段：
//! - 带 `#[serde(default)]` 的（`appRules` / `ruleResources`）→ **静默给空 Vec**，不报错；
//! - 于是 `route.rs:577` 的 app 分流循环恒空转、`add_local_geo_rule_set` 恒找不到本地副本。
//!
//! 实测（2026-07-16，本文件建立时）：`appRules` / `ruleResources` / `AppRule.appId` /
//! `RuleResource.sourceUrl|fileName|downloadedAt` **六处漏 rename** → 应用分流与规则资源在运行期
//! **整条不存在**。而当时：`UserConfig` 有测试、`route.rs` 的 app 分支有测试、金样有 37 场景 ——
//! **没有一扇门喂过真实键名的 appRules**（金样夹具里 `appRules` 一次都没出现过）。
//! 「有测试」与「测到了生产路径」是两件事。
//!
//! 本门的射程：**config.json 键名 → Rust 字段**这一跳。它不覆盖字段语义，只保证「读得进来」。
//! 加 UserConfig 字段时请在此补一行 —— 漏 rename 的代价是静默失能，不是编译错。

use polaris_config_engine::user_config::UserConfig;
use serde_json::json;

/// 真实 config.json 的最小形态（键名取自 `ui/src/shared/types/rules.ts` + `store/src/sanitize.rs`）。
fn real_config_json() -> serde_json::Value {
    json!({
        "servers": [],
        "proxyMode": "smart",
        "appRoutingEnabled": true,
        "appRules": [
            { "appId": "youtube", "action": "proxy", "enabled": true },
            { "appId": "custom-foo", "action": "direct", "enabled": true, "targetServerId": "s1" }
        ],
        "customAppPresets": [
            {
                "id": "custom-foo",
                "name": "Foo",
                "emoji": "🚀",
                "iconUrl": "https://example.com/foo.png",
                "geositeTags": ["foo"],
                "geoipTags": ["foo"],
                "processNames": ["FooApp"],
                "category": "tools"
            }
        ],
        "customRules": [
            { "id": "r1", "type": "domain", "values": ["example.com"], "action": "proxy", "enabled": true }
        ],
        "ruleResources": [
            {
                "id": "geosite-amazon",
                "name": "amazon",
                "category": "geosite",
                "sourceUrl": "https://raw.githubusercontent.com/x/y/geosite/amazon.srs",
                "fileName": "geosite-amazon.srs",
                "format": "binary",
                "size": 4096,
                "downloadedAt": "2026-01-01T00:00:00.000Z"
            }
        ]
    })
}

#[test]
fn app_rules_deserialize_from_camel_case_key() {
    let uc: UserConfig =
        serde_json::from_value(real_config_json()).expect("真实 config 应可反序列化");
    assert_eq!(
        uc.app_rules.len(),
        2,
        "appRules 没读进来（漏 #[serde(rename = \"appRules\")] → default 静默给空 Vec → 应用分流整条失效）"
    );
    assert_eq!(
        uc.app_rules[0].app_id, "youtube",
        "AppRule.appId 没读进来（漏 rename → app_id 为空串 → 永远匹配不到任何 preset）"
    );
    assert_eq!(uc.app_rules[1].app_id, "custom-foo");
    assert_eq!(uc.app_rules[1].target_server_id.as_deref(), Some("s1"));
}

#[test]
fn rule_resources_deserialize_from_camel_case_key() {
    let uc: UserConfig =
        serde_json::from_value(real_config_json()).expect("真实 config 应可反序列化");
    assert_eq!(
        uc.rule_resources.len(),
        1,
        "ruleResources 没读进来（漏 rename → 已下载的规则资源在配置生成时恒不可见）"
    );
    let r = &uc.rule_resources[0];
    assert_eq!(r.id, "geosite-amazon");
    assert_eq!(
        r.file_name, "geosite-amazon.srs",
        "RuleResource.fileName 没读进来 → add_local_geo_rule_set 拼不出落盘路径"
    );
    assert_eq!(
        r.source_url, "https://raw.githubusercontent.com/x/y/geosite/amazon.srs",
        "RuleResource.sourceUrl 没读进来"
    );
    assert_eq!(r.downloaded_at, "2026-01-01T00:00:00.000Z");
    assert_eq!(r.size, 4096);
}

#[test]
fn custom_app_presets_deserialize_from_camel_case_key() {
    // 这几个本来就有 rename，一并锁住（回归面同族）。
    let uc: UserConfig =
        serde_json::from_value(real_config_json()).expect("真实 config 应可反序列化");
    assert_eq!(uc.custom_app_presets.len(), 1);
    let p = &uc.custom_app_presets[0];
    assert_eq!(p.geosite_tags, vec!["foo".to_string()]);
    assert_eq!(p.geoip_tags, vec!["foo".to_string()]);
    assert_eq!(
        p.process_names.as_deref(),
        Some(&["FooApp".to_string()][..])
    );
    assert_eq!(p.icon_url.as_deref(), Some("https://example.com/foo.png"));
}

#[test]
fn custom_rules_deserialize_from_camel_case_key() {
    let uc: UserConfig =
        serde_json::from_value(real_config_json()).expect("真实 config 应可反序列化");
    assert_eq!(uc.custom_rules.len(), 1);
    assert_eq!(uc.custom_rules[0].id, "r1");
}

#[test]
fn round_trip_keeps_camel_case_keys() {
    // 序列化方向同样必须产 camelCase：orchestration.rs:105 把 UserConfig 序列化后做重启-norm 投影，
    // 键名错 → 投影里混进 snake_case 影子键。
    let uc: UserConfig = serde_json::from_value(real_config_json()).expect("反序列化");
    let out = serde_json::to_value(&uc).expect("序列化");
    let obj = out.as_object().expect("对象");
    for key in [
        "appRules",
        "ruleResources",
        "customRules",
        "customAppPresets",
    ] {
        assert!(obj.contains_key(key), "序列化产物缺 camelCase 键 {key}");
    }
    for ghost in [
        "app_rules",
        "rule_resources",
        "custom_rules",
        "custom_app_presets",
    ] {
        assert!(
            !obj.contains_key(ghost),
            "序列化产物出现 snake_case 影子键 {ghost}"
        );
    }
    // 往返幂等：再读回来字段仍在位。
    let again: UserConfig = serde_json::from_value(out).expect("往返反序列化");
    assert_eq!(again.app_rules.len(), 2);
    assert_eq!(again.rule_resources.len(), 1);
    assert_eq!(again.app_rules[0].app_id, "youtube");
}

#[test]
fn absent_keys_still_default_to_empty() {
    // rename 加上后，「键缺省 → 空 Vec」这条既有行为不得被破坏（老配置无 appRules 时不能报错）。
    let uc: UserConfig = serde_json::from_value(json!({ "servers": [] })).expect("最小 config");
    assert!(uc.app_rules.is_empty());
    assert!(uc.rule_resources.is_empty());
    assert!(uc.custom_rules.is_empty());
}
