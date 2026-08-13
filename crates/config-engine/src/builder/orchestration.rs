//! 编排收尾函数（上游 `ProxyManager.ts` 的纯逻辑子集）。
//!
//! 含：stableStringify（键序无关序列化）、serverFingerprint（节点指纹）、
//! configGenerationNorm（影响生成的配置投影）、fixRouteDeadReferences（route 死引用兜底）。
//! planHotSwitch/canSkipRestartForAddedUnreferenced 见 `hotswitch.rs`（H6-⑤）。
//! generateSingBoxConfig 编排见 `generate.rs`（H6-④）。
//!
//! 所有函数纯逻辑无 I/O，实例态由参数注入。

#![forbid(unsafe_code)]

use crate::user_config::app_config::UserConfig;
use crate::user_config::rule::RuleType;
use crate::user_config::rules::rule_conditions;
use std::collections::BTreeSet;

/// 递归按 key 排序后序列化——使深比较对对象属性插入顺序不敏感。
///
/// 数组顺序保留（customRules/appRules 顺序具语义）。undefined 键丢弃（与 JSON.stringify 一致）。
/// 上游 `ProxyManager.stableStringify`。
pub fn stable_stringify(v: &serde_json::Value) -> String {
    let canonical = canonicalize(v);
    serde_json::to_string(&canonical).unwrap_or_else(|_| "null".to_string())
}

/// 递归把 serde_json::Value 转为键排序的规范形式（Object → BTreeMap-backed）。
fn canonicalize(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            // 收集 (key, canonical_value) 并按 key 排序，丢弃 null 值（对齐 Polaris 丢 undefined）。
            // 注意：Polaris stableStringify 丢 undefined，但 JSON 里 null 是合法值。
            // Polaris 的 `v[k] !== undefined` 在 JS 对象里 undefined = 不存在的键，null = 存在。
            // serde_json Value::Object 不含 undefined（JSON 无），故只需排序，null 保留。
            let mut pairs: Vec<(String, serde_json::Value)> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonicalize(v)))
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = serde_json::Map::new();
            for (k, v) in pairs {
                sorted.insert(k, v);
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(canonicalize).collect())
        }
        other => other.clone(),
    }
}

/// 节点生成指纹（剔时间戳、键序无关）。
///
/// canSkip③、runningServersFingerprint 快照、待应用差集、dirty 判定共用单一真值。
/// 剔除 updatedAt/createdAt/providerName（归属元数据不改连接内容）。
/// 上游 `ProxyManager.serverFingerprint`。
pub fn server_fingerprint(server: &crate::user_config::server_config::ServerConfig) -> String {
    // 序列化为 Value，剔除时间戳/归属元数据，再 stableStringify。
    let mut value = serde_json::to_value(server).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = value.as_object_mut() {
        obj.remove("updatedAt");
        obj.remove("createdAt");
        obj.remove("providerName");
    }
    stable_stringify(&value)
}

/// 影响生成的配置投影 → 键序无关序列化字符串。
///
/// 热切换判定基础：norm(old) === norm(new) ⟹ 结构等价（仅 selectedServerId/targetServerId 值变）。
/// 排除所有不影响 sing-box 生成的字段（UI 偏好/调度偏好/元数据）。
/// 上游 `ProxyManager.configGenerationNorm`。
///
/// `server_ids`：P2-A 传 Some 时仅保留被引用节点（canSkipRestart 用），None = 全量。
pub fn config_generation_norm(
    config: &UserConfig,
    server_ids: Option<&BTreeSet<String>>,
) -> String {
    let proxy_mode = config.proxy_mode.as_str();
    let user_routing_active = proxy_mode.eq_ignore_ascii_case("smart");

    // 被启用 ruleSet 规则引用的本地资源 id 集（非 smart → 空）。
    let mut ids: BTreeSet<String> = BTreeSet::new();
    if user_routing_active {
        for r in &config.custom_rules {
            if !r.enabled {
                continue;
            }
            for cond in rule_conditions(r) {
                if cond.type_field == RuleType::RuleSet {
                    for v in &cond.values {
                        if let Some(rest) = v.strip_prefix("res:") {
                            ids.insert(rest.to_string());
                        }
                    }
                }
            }
        }
    }

    // 构建投影对象（对齐 Polaris 的字段排除/投影规则）。
    let mut proj = serde_json::Map::new();

    // 全量 config 序列化后投影（而非 spread + null 覆盖——Rust 无 spread）。
    let full = serde_json::to_value(config).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = full.as_object() {
        for (k, v) in obj {
            // 排除不影响生成的字段（Polaris 置 null 的键 = 此处跳过不进投影）。
            //
            // **只剩 `selectedServerId` 一项，这是全集不是节选**：真实判据是「该键是不是 `UserConfig`
            // 的序列化字段」——`UserConfig` 零 `#[serde(flatten)]`，故 `full` 的键集 ⊆
            // `UserConfig::FIELD_NAMES`（由 `field_names_equals_serde_projection` 钉死）。排除一个
            // 不在该结构里的键是**空操作**，`continue` 永不触发。
            //
            // 2026-07-29 清理：此处原有 15 项，其中 14 项（`ghProxyPrefix` / `language` /
            // `hardwareAcceleration` / `windowEffects` / `subscriptions` / `restartOnNodeChange` /
            // `mainSessionViaProxy` / `meshLoginFallbackDirect` / `builtinGeoMeta` /
            // `ruleResourceAutoUpdate` / `ruleResourceUpdateIntervalHours` / `helper*PromptDismissed` ×3）
            // 都不是 `UserConfig` 字段 ⇒ 全是死分支。它们唯一的存在理由是**与 上游的同名排除表逐行
            // 对拍**（那边 config 形状更宽），而该判据已于 2026-07-29 退役（改为「原型 ↔ 后端双向对拍」，
            // 见 `polaris-oracle-retirement-2026-07-29`）⇒ 理由消失，一并删除。
            //
            // 删除**同时消掉了它们曾带来的风险**：死键留在表里时，谁把 `language` 之类升成真字段，
            // 排除就会从空操作静默变成「让该字段不参与生成判等」（改它不再触发重启内核）。键不在表里，
            // 这条路径不复存在。剩下这一项的生效面仍由 `exclusion_table_live_entries_are_pinned` 钉住。
            if k.as_str() == "selectedServerId" {
                continue;
            }
            // dnsConfig 子投影：剔除迁移元数据标记。
            if k == "dnsConfig" {
                if let Some(dns_obj) = v.as_object() {
                    let mut dns_proj = serde_json::Map::new();
                    for (dk, dv) in dns_obj {
                        if matches!(
                            dk.as_str(),
                            "fakeIpToggleMigrated" | "fakeIpTunAutoEnable" | "nodeResolverMigrated"
                        ) {
                            continue;
                        }
                        dns_proj.insert(dk.clone(), dv.clone());
                    }
                    proj.insert(k.clone(), serde_json::Value::Object(dns_proj));
                }
                continue;
            }
            // customRules / app_rules / rule_resources / servers 单独投影（下方处理）。
            // 注：UserConfig serde——customRules 显式 rename；app_rules/rule_resources 未 rename（Rust snake_case）。
            //   排除键须匹配【实际序列化键名】而非 Rust 字段名/Polaris camelCase，否则原始字段泄漏进 norm
            //   （其 targetServerId 会随切节点翻转 → norm 不等 → 热切换误退回重启）。
            if matches!(
                k.as_str(),
                "customRules" | "app_rules" | "rule_resources" | "servers"
            ) {
                continue;
            }
            proj.insert(k.clone(), v.clone());
        }
    }

    // customRules 投影。
    let custom_rules_proj = if user_routing_active {
        let arr: Vec<serde_json::Value> = config
            .custom_rules
            .iter()
            .filter(|r| r.enabled)
            .map(|r| {
                use crate::builder::custom_rule_files::plan_custom_rule;
                if matches!(
                    plan_custom_rule(r),
                    crate::builder::custom_rule_files::RulePlan::Inline
                ) {
                    // smart inline：保留全量结构，剔 remarks + targetServerId（值热切换）。
                    let mut v = serde_json::to_value(r).unwrap_or(serde_json::Value::Null);
                    if let Some(o) = v.as_object_mut() {
                        o.remove("remarks");
                        o.remove("targetServerId");
                    }
                    v
                } else {
                    // smart ext：结构位保留，值移出 norm。
                    let conds: Vec<serde_json::Value> = rule_conditions(r)
                        .into_iter()
                        .map(|cd| {
                            let ok = crate::builder::custom_rule_files::cond_matcher_fields(&cd)
                                .is_some();
                            serde_json::json!({
                                "t": cd.type_field,
                                "ok": ok,
                            })
                        })
                        .collect();
                    serde_json::json!({
                        "__ext": 1,
                        "id": r.id,
                        "action": r.action,
                        "targetServerId": null,
                        "combineMode": r.combine_mode,
                        "bypassFakeIP": r.bypass_fakeip.unwrap_or(false),
                        "conds": conds,
                    })
                }
            })
            .collect();
        serde_json::Value::Array(arr)
    } else {
        serde_json::Value::Array(vec![])
    };
    proj.insert("customRules".into(), custom_rules_proj);

    // appRules 投影：仅 smart 生效。targetServerId 移出 norm。
    let app_rules_proj = if user_routing_active {
        let arr: Vec<serde_json::Value> = config
            .app_rules
            .iter()
            .map(|a| {
                serde_json::json!({
                    "appId": a.app_id,
                    "action": a.action,
                    "enabled": a.enabled,
                    "targetServerId": null,
                })
            })
            .collect();
        serde_json::Value::Array(arr)
    } else {
        serde_json::Value::Array(vec![])
    };
    proj.insert("appRules".into(), app_rules_proj);

    // ruleResources 投影：仅被启用 ruleSet 引用的资源 id，排序。
    let rule_resources_proj: Vec<serde_json::Value> = config
        .rule_resources
        .iter()
        .filter(|rr| ids.contains(&rr.id))
        .map(|rr| serde_json::Value::String(rr.id.clone()))
        .collect();
    let mut sorted_rr = rule_resources_proj;
    sorted_rr.sort_by(|a, b| a.as_str().unwrap_or("").cmp(b.as_str().unwrap_or("")));
    proj.insert("ruleResources".into(), serde_json::Value::Array(sorted_rr));

    // servers 投影：server_ids 过滤 + id 排序 + server_fingerprint。
    let mut servers_proj: Vec<serde_json::Value> = config
        .servers
        .iter()
        .filter(|s| server_ids.map(|ids| ids.contains(&s.id)).unwrap_or(true))
        .map(|s| serde_json::Value::String(server_fingerprint(s)))
        .collect();
    servers_proj.sort_by(|a, b| {
        // server_fingerprint 已含 id，但 Polaris 按 server.id 排序后再 fingerprint。
        // 此处直接对 fingerprint 串排序（等价：fingerprint 内含 id，排序结果一致）。
        a.as_str().unwrap_or("").cmp(b.as_str().unwrap_or(""))
    });
    // 注意：Polaris 先按 server.id.localeCompare 排序再 map fingerprint。
    // 为字节精确对齐，需先按 id 排序。修正：
    let mut servers_with_id: Vec<(&str, String)> = config
        .servers
        .iter()
        .filter(|s| server_ids.map(|ids| ids.contains(&s.id)).unwrap_or(true))
        .map(|s| (s.id.as_str(), server_fingerprint(s)))
        .collect();
    servers_with_id.sort_by(|a, b| a.0.cmp(b.0));
    let servers_final: Vec<serde_json::Value> = servers_with_id
        .into_iter()
        .map(|(_, fp)| serde_json::Value::String(fp))
        .collect();
    proj.insert("servers".into(), serde_json::Value::Array(servers_final));
    // 消除未使用警告（servers_proj 被 servers_final 替代）。
    let _ = servers_proj;

    stable_stringify(&serde_json::Value::Object(proj))
}

/// route 死引用兜底：route 规则的 outbound 指向不存在的 tag → 改写为 proxy-selector。
///
/// 任何 action='route' 的规则，其 outbound 不在 outbounds[].tag ∪ endpoints[].tag 集合中 → 改写。
/// 上游 `ProxyManager.fixRouteDeadReferences`。
pub fn fix_route_dead_references(
    outbounds: &[crate::singbox::Outbound],
    endpoints: &[crate::singbox::Endpoint],
    rules: &mut [crate::singbox::RouteRule],
) {
    let valid_tags: BTreeSet<String> = outbounds
        .iter()
        .map(|o| o.tag.clone())
        .chain(endpoints.iter().map(|e| e.tag.clone()))
        .filter(|t| !t.is_empty())
        .collect();
    for rule in rules.iter_mut() {
        // action='route' 且 outbound 不在有效 tag 集合 → 改写 proxy-selector。
        // RouteRule 的 action/outbound 字段需确认。
        let is_route = rule
            .action
            .as_deref()
            .map(|a| a == "route")
            .unwrap_or(false);
        if is_route {
            if let Some(outbound) = &rule.outbound {
                if !valid_tags.contains(outbound) {
                    rule.outbound = Some("proxy-selector".to_string());
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    /// 排除表里**哪些是活的**必须被钉死。
    ///
    /// 表里现只有 `selectedServerId` 一项，且它真是 `UserConfig` 的序列化字段（2026-07-29 前另有
    /// 14 个非 `FIELD_NAMES` 的死键，是与 上游 对拍的留痕，随该判据退役一并删除）。
    ///
    /// 本测防的是**一次静默的语义变化**：谁往排除表里加一个真字段（或让 `selectedServerId` 掉出
    /// `FIELD_NAMES` / 掉出排除表），该字段就会不参与生成判等 —— 即改它不会被判为需要重启内核。
    /// 那可能是对的，但必须是一次睁着眼的决定。
    ///
    /// 牙（两向）：
    ///  1. 把 `selectedServerId` 从生产排除分支删掉 → 下方自检 `arm.contains` 转红；
    ///  2. 把 `selectedServerId` 从 `FIELD_NAMES` 删掉 → `live` 断言转红。
    ///
    /// **本测守不住的方向**（与 2026-07-29 缩表前一致，缩表未新增此洞）：往生产分支加一个新键而不改
    /// 本表 —— `live` 是从本表算的，加在生产侧看不见。真正兜住它的是「排除即空操作」这条结构性质：
    /// 非 `UserConfig` 字段排了也白排，而真字段一旦被排会让 `norm` 少一维，由热切换/重启的行为测发现。
    #[test]
    fn exclusion_table_live_entries_are_pinned() {
        use crate::user_config::app_config::UserConfig;
        // 与 `config_generation_norm` 里那份排除分支逐行同源（改一处必须改另一处，
        // 不同步会让本测守着一张不存在的表 —— 故下面另有一条自检）。
        const EXCLUDED: [&str; 1] = ["selectedServerId"];
        let fields: std::collections::BTreeSet<&str> =
            UserConfig::FIELD_NAMES.iter().copied().collect();
        let live: Vec<&str> = EXCLUDED
            .iter()
            .copied()
            .filter(|k| fields.contains(k))
            .collect();
        assert_eq!(
            live,
            ["selectedServerId"],
            "排除表的**生效面**变了。它从来只对 UserConfig 的真实字段起作用；\
             多出来的键意味着某个字段被悄悄排除出生成判等（改它不再触发重启内核），\
             少了则意味着 selectedServerId 不再被排除。两个方向都必须是显式决定"
        );

        // 自检：上面那份常量表必须与实现里的排除分支逐字同源，否则本测在守一张幽灵表。
        // 扫描面必须**排除本测自己**：`include_str!` 读的是本文件，而上面那份 EXCLUDED 常量就写在
        // 这里 —— 扫全文的话表里的键永远「找得到」，自检恒绿（试过，正是这么栽的）。
        // 故只取 `#[cfg(test)]` 之前的生产段。用结构性锚点而非注释文本：注释会被改，模块属性不会。
        let src = include_str!("orchestration.rs");
        let arm = src
            .split("#[cfg(test)]")
            .next()
            .expect("split 至少产出一段");
        assert!(
            arm.contains("fn config_generation_norm"),
            "生产段里找不到 config_generation_norm —— 切分锚点漂了，下面的断言在扫一段空文本"
        );
        // 反自引用：锚点一旦匹配不上，`split(..).next()` 会**返回整份文件**（不是 None），
        // 于是本测自己的 EXCLUDED 常量也进了扫描面 ⇒ 键永远找得到 ⇒ 自检恒绿。
        // 常量名只出现在测试模块里，故用它判「扫过界了」。
        assert!(
            !arm.contains("const EXCLUDED"),
            "扫描面把测试模块也扫进来了（切分锚点失配）—— 自检会拿本测自己的常量表自我印证"
        );
        for k in EXCLUDED {
            assert!(
                arm.contains(&format!("\"{k}\"")),
                "常量表里的 {k} 在 config_generation_norm 的排除分支里找不到 —— 两处已分叉"
            );
        }
    }

    use super::*;

    #[test]
    fn stable_stringify_sorts_keys() {
        let v: serde_json::Value = serde_json::from_str(r#"{"c":3,"a":1,"b":2}"#).unwrap();
        let s = stable_stringify(&v);
        // 键应按字母序：a,b,c
        assert_eq!(s, r#"{"a":1,"b":2,"c":3}"#);
    }

    #[test]
    fn stable_stringify_preserves_array_order() {
        let v: serde_json::Value = serde_json::from_str(r#"[3,1,2]"#).unwrap();
        let s = stable_stringify(&v);
        assert_eq!(s, "[3,1,2]");
    }

    #[test]
    fn stable_stringify_recursive_nested() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"z":{"b":2,"a":1},"y":[1,2]}"#).unwrap();
        let s = stable_stringify(&v);
        assert_eq!(s, r#"{"y":[1,2],"z":{"a":1,"b":2}}"#);
    }

    #[test]
    fn stable_stringify_equal_despite_key_order() {
        let a: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        assert_eq!(stable_stringify(&a), stable_stringify(&b));
    }

    #[test]
    fn config_generation_norm_excludes_ui_fields() {
        let mut config = UserConfig::default();
        config.servers = vec![crate::user_config::server_config::ServerConfig {
            id: "s1".into(),
            name: "s1".into(),
            protocol: crate::user_config::server_config::Protocol::Shadowsocks,
            address: "1.1.1.1".into(),
            port: 443,
            ..Default::default()
        }];
        config.selected_server_id = Some("s1".into());
        let norm1 = config_generation_norm(&config, None);
        // 切换 selectedServerId → norm 不变（已排除）
        config.selected_server_id = Some("s2".into());
        let norm2 = config_generation_norm(&config, None);
        assert_eq!(norm1, norm2, "selectedServerId 变化不应翻转 norm");
    }

    #[test]
    fn config_generation_norm_global_ignores_user_routing() {
        use crate::user_config::proxy_mode::ProxyMode;
        use crate::user_config::rule::{Rule, RuleAction, RuleType};
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Global;
        config.servers = vec![crate::user_config::server_config::ServerConfig {
            id: "s1".into(),
            name: "s1".into(),
            protocol: crate::user_config::server_config::Protocol::Shadowsocks,
            address: "1.1.1.1".into(),
            port: 443,
            ..Default::default()
        }];
        config.custom_rules = vec![Rule {
            id: "r1".into(),
            type_field: RuleType::Domain,
            values: vec!["x.com".into()],
            conditions: None,
            combine_mode: None,
            action: RuleAction::Proxy,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        }];
        let norm = config_generation_norm(&config, None);
        // global 模式 → customRules 投影为 []
        assert!(norm.contains(r#""customRules":[]"#));
    }
}
