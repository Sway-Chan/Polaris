//! 自定义路由规则生成（上游 `singbox-custom-rules.ts` 1:1 移植）。
//!
//! 用户自定义分流规则 → sing-box route 规则 + rule_set 定义。
//! 含 L3 外化 headless、geosite/geoip、res:<id> 本地资源、logical AND/OR、fail-closed 缺失跳过。
//!
//! 依赖注入：文件系统检查（isValidSrsFile/路径）经 CustomRulesDeps 传入（对拍 fixture 注入固定假值）。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use crate::builder::custom_rule_files::{
    cond_matcher_fields, custom_rule_file_base, is_ext_type, plan_custom_rule, RulePlan,
};
use crate::singbox::{RouteRule, RuleSet};
use crate::user_config::builtin_geo_rulesets::resolve_builtin_rule_set_ref_meta;
use crate::user_config::log_level::LogLevel;
use crate::user_config::neighbor::{
    is_source_device_match_supported, is_valid_mac_address, is_valid_source_hostname,
};
use crate::user_config::rule::{CombineMode, Rule, RuleAction, RuleCondition, RuleType};
use crate::user_config::rules::rule_conditions;
use crate::user_config::validate_tls_spoof_default;

/// 目的地 OR 组（单条 default rule 内原生 OR）。上游 `OR_GROUP`。
fn is_or_group(t: RuleType) -> bool {
    matches!(
        t,
        RuleType::Domain
            | RuleType::DomainSuffix
            | RuleType::DomainKeyword
            | RuleType::DomainRegex
            | RuleType::IpCidr
    )
}

/// applyConditionFields 的目标累积器（route rule 字段并集）。
type RouteFields = BTreeMap<String, serde_json::Value>;

/// 把一个条件的 type→字段累积到 target（值并集；geosite/geoip→rule_set tag；ruleSet→注册定义+tag）。
/// 返回 hasMatcher（是否产出有效匹配字段）。上游 `applyConditionFields`。
///
/// platform + rule_resources_deps 注入处理 FS 相关分支（ruleSet 资源解析）。
fn apply_condition_fields(
    cond: &RuleCondition,
    target: &mut RouteFields,
    platform: &str,
    rule_resources: &[crate::user_config::rule::RuleResource],
    rule_sets_out: &mut Vec<RuleSet>,
    deps: &CustomRulesDeps,
) -> bool {
    // EXT 类型委托 cond_matcher_fields。
    if is_ext_type(cond.type_field) {
        let fields = match cond_matcher_fields(cond) {
            Some(f) => f,
            None => return false,
        };
        for (k, v) in fields {
            let entry = target
                .entry(k)
                .or_insert_with(|| serde_json::Value::Array(vec![]));
            if let serde_json::Value::Array(arr) = entry {
                if let serde_json::Value::Array(new_arr) = serde_json::Value::Array(v) {
                    arr.extend(new_arr);
                }
            }
        }
        return true;
    }

    let vals: Vec<String> = cond
        .values
        .iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();
    if vals.is_empty() {
        return false;
    }

    match cond.type_field {
        RuleType::SourceMac => {
            // 仅 Linux/macOS；脏 MAC 过滤。
            if !is_source_device_match_supported(platform) {
                return false;
            }
            let macs: Vec<&String> = vals
                .iter()
                .filter(|v| is_valid_mac_address(Some(v)))
                .collect();
            if macs.is_empty() {
                return false;
            }
            push_string_array(
                target,
                "source_mac_address",
                macs.into_iter().cloned().collect(),
            );
            true
        }
        RuleType::SourceHostname => {
            if !is_source_device_match_supported(platform) {
                return false;
            }
            let hosts: Vec<&String> = vals
                .iter()
                .filter(|v| is_valid_source_hostname(Some(v)))
                .collect();
            if hosts.is_empty() {
                return false;
            }
            push_string_array(
                target,
                "source_hostname",
                hosts.into_iter().cloned().collect(),
            );
            true
        }
        RuleType::Geosite => {
            let tags: Vec<String> = vals
                .iter()
                .map(|t| format!("geosite-{}", t.to_ascii_lowercase()))
                .collect();
            push_string_array(target, "rule_set", tags);
            true
        }
        RuleType::Geoip => {
            let tags: Vec<String> = vals
                .iter()
                .map(|t| format!("geoip-{}", t.to_ascii_lowercase()))
                .collect();
            push_string_array(target, "rule_set", tags);
            true
        }
        RuleType::RuleSet => {
            // res:<id> → 本地 rule_set；远程 URL 已不再支持（fail-closed 跳过）。
            let mut seen = existing_rule_set_tags(target);
            for v in &vals {
                if let Some(res_id) = v.strip_prefix("res:") {
                    let tag =
                        resolve_resource_rule_set(res_id, rule_resources, rule_sets_out, deps);
                    if let Some(t) = tag {
                        seen.insert(t);
                    }
                } else {
                    deps.log_warn(&format!(
                        "ruleSet 规则的远程 URL 已不再支持，请改用「规则资源」下载后引用，已跳过: {v}"
                    ));
                }
            }
            if !seen.is_empty() {
                target.insert(
                    "rule_set".into(),
                    serde_json::Value::Array(
                        seen.into_iter().map(serde_json::Value::from).collect(),
                    ),
                );
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

fn push_string_array(target: &mut RouteFields, key: &str, vals: Vec<String>) {
    let entry = target
        .entry(key.to_string())
        .or_insert_with(|| serde_json::Value::Array(vec![]));
    if let serde_json::Value::Array(arr) = entry {
        for v in vals {
            arr.push(serde_json::Value::from(v));
        }
    }
}

fn existing_rule_set_tags(target: &mut RouteFields) -> std::collections::BTreeSet<String> {
    match target.get("rule_set") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(serde_json::Value::String(s)) => std::collections::BTreeSet::from([s.clone()]),
        _ => std::collections::BTreeSet::new(),
    }
}

/// res:<id> 资源解析：内置 res:builtin:<tag> 或本地资源 id → rule_set tag + 注册定义。
/// 返回 None = 资源缺失/损坏跳过。FS 检查经 deps 注入。
fn resolve_resource_rule_set(
    res_id: &str,
    rule_resources: &[crate::user_config::rule::RuleResource],
    rule_sets_out: &mut Vec<RuleSet>,
    deps: &CustomRulesDeps,
) -> Option<String> {
    // 内置资源（res:builtin:<tag>）。tag/fileName 取自 catalog 单一真值表，**绝不由 tag 拼 `<tag>.srs`**：
    // `geosite-category-ai` 的落盘名是 `geosite-category-ai-!cn.srs`（MetaCubeX 无裸 category-ai .srs），
    // 拼名会让该条恒缺失。上游 `singbox-custom-rules.ts:119` resolveBuiltinRuleSetRefMeta。
    if let Some((tag, file_name)) = resolve_builtin_rule_set_ref_meta(res_id) {
        let file_path = format!("{}/{}", deps.runtime_rules_dir, file_name);
        if !deps.is_valid_srs_file(&file_path) {
            deps.log_warn(&format!(
                "ruleSet 规则引用的内置资源文件缺失/损坏，已跳过: {file_name}"
            ));
            return None;
        }
        if !rule_sets_out.iter().any(|rs| rs.tag == tag) {
            rule_sets_out.push(RuleSet {
                tag: tag.clone(),
                type_field: "local".into(),
                format: "binary".into(),
                path: Some(file_path),
                url: None,
                download_detour: None,
                update_interval: None,
            });
        }
        return Some(tag);
    }

    // 本地资源 id。**未知 `builtin:<tag>` 也落到这里**（catalog 未命中 → 按普通 id 再查一次用户资源，
    // 查不到才报「资源不存在」）——忠实 上游 `singbox-custom-rules.ts:135-140` 的 if/else 结构；
    // 早退会让「catalog 漏收的 tag」与「用户自建同名资源」两种情形都静默消失。
    let res = match rule_resources.iter().find(|r| r.id == res_id) {
        Some(r) => r,
        None => {
            // 上游 打的是原始值 `${v}`（含 `res:` 前缀），此处 res_id 已剥前缀 → 补回，日志与 上游 逐字一致。
            deps.log_warn(&format!(
                "ruleSet 规则引用的资源不存在，已跳过: res:{res_id}"
            ));
            return None;
        }
    };
    let file_path = format!("{}/{}", deps.rule_resources_path, res.file_name);
    if !deps.is_valid_srs_file(&file_path) {
        deps.log_warn(&format!(
            "ruleSet 规则引用的资源文件缺失/损坏，已跳过: {}",
            res.file_name
        ));
        return None;
    }
    let tag = format!("local-rs-{res_id}");
    if !rule_sets_out.iter().any(|rs| rs.tag == tag) {
        rule_sets_out.push(RuleSet {
            tag: tag.clone(),
            type_field: "local".into(),
            format: match res.format {
                crate::user_config::rule::RuleResourceFormat::Binary => "binary",
                crate::user_config::rule::RuleResourceFormat::Source => "source",
            }
            .into(),
            path: Some(file_path),
            url: None,
            download_detour: None,
            update_interval: None,
        });
    }
    Some(tag)
}

/// 应用规则动作到 sing-box 规则（action/outbound + tls_spoof）。上游 `applyRuleAction`。
/// 参数对齐 TS applyRuleAction 签名（8 入参），Rust 加 rule_fields 共 9 → allow too_many_arguments。
///
/// # 阻断走规则级 `action:"reject"`，不再指向 `block` 出站
///
/// sing-box 自 1.11 起把 legacy special outbound（`block` / `dns`）标记废弃，规则级 `action`
/// 是官方替代。**被规则引用的 `block` 出站正是废弃面**，故此处改发 `action:"reject"` 且
/// **不写 `outbound`**（reject 是规则级动作，没有对应出站可指）。
///
/// ⚠️ `block` **出站定义仍然保留**（见 `builder/outbounds.rs`）——「出口选阻断」的
/// proxy-selector `default`/成员只能填 outbound tag，不存在「reject 出站」。那不是漏改。
///
/// `method` 刻意不写（= 官方默认 `default`，语义「connection reset」），与本仓既有 5 处
/// `action:"reject"`（udp443 阻 QUIC / STUN 阻断 / DNS 防泄露…）保持同一形态。
/// 与 legacy `block` 出站（返回 `EPERM`）的唯一实质差异：默认 `no_drop=false` 下，
/// 30s 内 >50 次拒绝会临时降级为静默丢包（`drop`）——见提交说明。
#[allow(clippy::too_many_arguments)]
pub fn apply_rule_action(
    rule_fields: &mut RouteFields,
    action: RuleAction,
    target_server_id: Option<&str>,
    id_to_tag_map: &BTreeMap<String, String>,
    selected_server_tag: &str,
    rule_id: Option<&str>,
    tls_spoof: Option<&str>,
    tls_spoof_method: Option<&str>,
    arch: &str,
) {
    // outbound 设置。`None` = 该动作是规则级的、没有出站（见函数文档）。
    let outbound: Option<String> = match action {
        RuleAction::Proxy => Some({
            // anti-drift：指定目标节点的规则 → 指向独立 rule-sel-<ruleId> selector，绝不直绑节点。
            if let Some(rid) = rule_id {
                format!("rule-sel-{rid}")
            } else if let Some(tid) = target_server_id {
                id_to_tag_map
                    .get(tid)
                    .cloned()
                    .unwrap_or_else(|| format!("proxy-{tid}"))
            } else {
                selected_server_tag.to_string()
            }
        }),
        RuleAction::Direct => Some("direct".into()),
        RuleAction::Block => None,
    };
    match outbound {
        Some(o) => {
            rule_fields.insert("outbound".into(), serde_json::Value::from(o));
        }
        // 覆盖调用方预置的 `action:"route"`（三处 `insert("action", "route")` 都在本函数之前跑）。
        None => {
            rule_fields.insert("action".into(), serde_json::Value::from("reject"));
            // `no_drop:true` 关掉 sing-box 的 50 次/30s 泛洪降级 —— 不加就与 legacy `block`
            // 出站不等价，且退化恰好落在阻断规则最主要的用途（广告/遥测域名高频命中）上。
            // 判据与实证见 `singbox::RouteRule::no_drop` 字段文档。
            rule_fields.insert("no_drop".into(), serde_json::Value::from(true));
        }
    }

    // TLS spoof（仅非 block 规则）。
    if action != RuleAction::Block {
        let spoof_sni = tls_spoof.unwrap_or("").trim();
        if validate_tls_spoof_default(Some(spoof_sni), tls_spoof_method, Some(arch), None, None) {
            rule_fields.insert("tls_spoof".into(), serde_json::Value::from(spoof_sni));
            if let Some(m) = tls_spoof_method {
                rule_fields.insert("tls_spoof_method".into(), serde_json::Value::from(m));
            }
        }
    }
}

/// buildCustomRules 依赖注入（FS 检查 + 路径 + 日志）。
#[derive(Debug, Clone)]
pub struct CustomRulesDeps {
    /// 运行时 rules 目录（内置 geo .srs 路径前缀）。
    pub runtime_rules_dir: String,
    /// 用户规则资源目录（res:<id> 文件路径前缀）。
    pub rule_resources_path: String,
    /// 自定义规则外化文件目录（L3 ext 文件路径前缀）。
    pub custom_rules_dir: String,
    /// 编译目标 arch（tls_spoof 门控）。
    pub arch: String,
    /// 运行平台（source device match 门控）。
    pub platform: String,
    /// 内置 / res:<id> 二进制 `.srs` 的存在性 + SRS 魔数检查（对拍 fixture 注入固定值）。
    /// 仅 res: 分支用；ext JSON source 走 `exists_fn`。
    pub is_valid_srs_fn: fn(&str) -> bool,
    /// L3 ext 外化 JSON source（`<base>.json` / `<base>.dns.json`）的存在性检查（`existsSync` 等价）。
    /// **绝不复用 `is_valid_srs_fn`**：JSON 无 SRS 魔数，复用会使「落盘后 ext 分支」100% 不可达（恒回落 inline）。
    /// 生产默认注入 [`crate::builder::custom_rule_files::ext_rule_file_exists`]；对拍 fixture 注入固定值。
    pub exists_fn: fn(&str) -> bool,
    /// 日志回调（上游 `CustomRulesDeps.log`）。**规则被剪掉时的唯一线索**：内置/本地资源缺失、
    /// 资源不存在、远程 URL 不再支持三条路径都只在这里发声，config JSON 里看不出任何痕迹。
    /// 签名与 [`crate::builder::route::RouteConfigDeps::log`] 一致 → route 直接透传，生产落
    /// `log::warn!(target: "config-engine", …)`；测试注收集器。
    pub log: fn(LogLevel, &str),
}

impl CustomRulesDeps {
    fn is_valid_srs_file(&self, path: &str) -> bool {
        (self.is_valid_srs_fn)(path)
    }

    /// 剪枝告警（日志不进 config JSON，故金样对拍不受影响）。上游 `deps.log('warn', …)`。
    fn log_warn(&self, msg: &str) {
        (self.log)(LogLevel::Warn, msg);
    }
}

/// buildCustomRules 输出。Polaris 返回 `{rules, ruleSets}`。
#[derive(Debug, Clone, Default)]
pub struct CustomRulesResult {
    pub rules: Vec<RouteRule>,
    pub rule_sets: Vec<RuleSet>,
}

/// 用户自定义规则 → sing-box route 规则 + rule_set 定义。上游 `buildCustomRules`。
///
/// 参数对齐 Polaris buildCustomRules（行 40-49）：
/// - custom_rules: Rule[]
/// - selected_server_id + id_to_tag_map: 选中节点 + id→tag 映射（outbound 解析）
/// - selected_server_tag: 默认代理 tag（通常 'proxy-selector'）
/// - rule_resources: res:<id> 引用的本地资源
/// - register_dns_bypass: FakeIP 启用时为 bypassFakeIP 规则注册 dns rule_set
/// - deps: FS/路径/平台注入
pub fn build_custom_rules(
    custom_rules: &[Rule],
    _selected_server_id: Option<&str>,
    id_to_tag_map: &BTreeMap<String, String>,
    selected_server_tag: &str,
    rule_resources: &[crate::user_config::rule::RuleResource],
    register_dns_bypass: bool,
    deps: &CustomRulesDeps,
) -> CustomRulesResult {
    let mut rules: Vec<RouteRule> = Vec::new();
    let mut rule_sets: Vec<RuleSet> = Vec::new();

    for rule in custom_rules {
        if !rule.enabled {
            continue;
        }

        let plan = plan_custom_rule(rule);
        let ext_base = custom_rule_file_base(&rule.id);

        // DNS 文件注册（ext-skip 跳过前）。
        if register_dns_bypass
            && rule.bypass_fakeip == Some(true)
            && !matches!(plan, RulePlan::Inline)
        {
            if let RulePlan::Ext {
                dns_rules: Some(_), ..
            }
            | RulePlan::ExtSkip { dns_rules: Some(_) } = &plan
            {
                let dns_path = format!(
                    "{deps_custom}/{ext_base}.dns.json",
                    deps_custom = deps.custom_rules_dir
                );
                // ext JSON source：真存在性检查（existsSync 等价），非 SRS 魔数。
                if (deps.exists_fn)(&dns_path) {
                    let dns_tag = format!("{ext_base}-dns");
                    if !rule_sets.iter().any(|rs| rs.tag == dns_tag) {
                        rule_sets.push(RuleSet {
                            tag: dns_tag,
                            type_field: "local".into(),
                            format: "source".into(),
                            path: Some(dns_path),
                            url: None,
                            download_detour: None,
                            update_interval: None,
                        });
                    }
                }
            }
        }

        // ext-skip：全 EXT 但 fail-closed → 无 route 规则。
        if matches!(plan, RulePlan::ExtSkip { .. }) {
            continue;
        }

        // ext：注册 rule_set + 生成 {rule_set:base} 规则。
        if let RulePlan::Ext { .. } = &plan {
            let file_path = format!(
                "{deps_custom}/{ext_base}.json",
                deps_custom = deps.custom_rules_dir
            );
            // ext JSON source：真存在性检查（existsSync 等价），非 SRS 魔数。
            if (deps.exists_fn)(&file_path) {
                if !rule_sets.iter().any(|rs| rs.tag == ext_base) {
                    rule_sets.push(RuleSet {
                        tag: ext_base.clone(),
                        type_field: "local".into(),
                        format: "source".into(),
                        path: Some(file_path),
                        url: None,
                        download_detour: None,
                        update_interval: None,
                    });
                }
                let mut ext_fields = RouteFields::new();
                ext_fields.insert("action".into(), serde_json::Value::from("route"));
                ext_fields.insert(
                    "rule_set".into(),
                    serde_json::Value::Array(vec![serde_json::Value::from(ext_base.clone())]),
                );
                apply_rule_action(
                    &mut ext_fields,
                    rule.action,
                    rule.target_server_id.as_deref(),
                    id_to_tag_map,
                    selected_server_tag,
                    Some(&rule.id),
                    rule.tls_spoof.as_deref(),
                    rule.tls_spoof_method.as_deref(),
                    &deps.arch,
                );
                rules.push(fields_to_route_rule(ext_fields));
                continue;
            }
            // 文件未落盘 → 回落 inline（onDegraded 由调用方处理，此处不注入）。
        }

        // inline 路径：applyConditionFields 累积。
        let raw_conds = rule_conditions(rule);
        let conds: Vec<(&RuleCondition, Vec<String>)> = raw_conds
            .iter()
            .map(|c| {
                (c, {
                    let vals: Vec<String> = c
                        .values
                        .iter()
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                        .collect();
                    vals
                })
            })
            .filter(|(_, v)| !v.is_empty())
            .collect();
        if conds.is_empty() {
            continue;
        }
        // AND 模式任一条件被丢 → 整条跳过（fail-closed）。
        let is_and = rule.combine_mode == Some(CombineMode::And);
        if is_and && conds.len() < raw_conds.len() {
            continue;
        }

        let mergeable =
            conds.len() == 1 || (!is_and && conds.iter().all(|(c, _)| is_or_group(c.type_field)));

        let final_rule_fields: Option<RouteFields> = if mergeable {
            let mut singbox_fields = RouteFields::new();
            singbox_fields.insert("action".into(), serde_json::Value::from("route"));
            let mut has_matcher = false;
            for (c, _) in &conds {
                if apply_condition_fields(
                    c,
                    &mut singbox_fields,
                    &deps.platform,
                    rule_resources,
                    &mut rule_sets,
                    deps,
                ) {
                    has_matcher = true;
                }
            }
            if has_matcher {
                Some(singbox_fields)
            } else {
                None
            }
        } else {
            // logical：每条件一个纯 matcher 子规则。
            let mut sub_rules: Vec<RouteFields> = Vec::new();
            let mut dropped = false;
            for (c, _) in &conds {
                let mut sub = RouteFields::new();
                if apply_condition_fields(
                    c,
                    &mut sub,
                    &deps.platform,
                    rule_resources,
                    &mut rule_sets,
                    deps,
                ) {
                    sub_rules.push(sub);
                } else {
                    dropped = true;
                }
            }
            if is_and && dropped {
                None
            } else if sub_rules.len() == 1 {
                let mut f = sub_rules.into_iter().next().unwrap();
                f.insert("action".into(), serde_json::Value::from("route"));
                Some(f)
            } else if sub_rules.len() > 1 {
                let mut logical = RouteFields::new();
                logical.insert("action".into(), serde_json::Value::from("route"));
                logical.insert("type".into(), serde_json::Value::from("logical"));
                let mode = match rule.combine_mode.unwrap_or_default() {
                    CombineMode::And => "and",
                    CombineMode::Or => "or",
                };
                logical.insert("mode".into(), serde_json::Value::from(mode));
                logical.insert(
                    "rules".into(),
                    serde_json::Value::Array(
                        sub_rules.into_iter().map(fields_to_matcher_rule).collect(),
                    ),
                );
                Some(logical)
            } else {
                None
            }
        };

        let mut final_fields = match final_rule_fields {
            Some(f) => f,
            None => continue,
        };

        apply_rule_action(
            &mut final_fields,
            rule.action,
            rule.target_server_id.as_deref(),
            id_to_tag_map,
            selected_server_tag,
            Some(&rule.id),
            rule.tls_spoof.as_deref(),
            rule.tls_spoof_method.as_deref(),
            &deps.arch,
        );
        rules.push(fields_to_route_rule(final_fields));
    }

    CustomRulesResult { rules, rule_sets }
}

/// RouteFields（BTreeMap<String, Value>）→ RouteRule。
/// RouteRule 是强类型 struct，但 buildCustomRules 产出的字段集开放（geosite/source_mac 等动态），
/// 故经 serde_json::Value 中转反序列化（对拍确定性：BTreeMap 保序）。
fn fields_to_route_rule(fields: RouteFields) -> RouteRule {
    let obj = serde_json::Value::Object(fields.into_iter().collect());
    serde_json::from_value(obj)
        .unwrap_or_else(|e| panic!("buildCustomRules 产出非法 RouteRule: {e}"))
}

/// logical 子规则（纯 matcher，无 action/outbound）。
fn fields_to_matcher_rule(fields: RouteFields) -> serde_json::Value {
    serde_json::Value::Object(fields.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::rule::{RuleAction, RuleType};

    fn deps_default() -> CustomRulesDeps {
        CustomRulesDeps {
            runtime_rules_dir: "/fake/rules".into(),
            rule_resources_path: "/fake/res".into(),
            custom_rules_dir: "/fake/custom-rules".into(),
            arch: "x64".into(),
            platform: "linux".into(),
            is_valid_srs_fn: |_| false, // res: 二进制 .srs 默认不存在
            exists_fn: |_| false,       // ext JSON 未落盘 → 回落 inline
            log: |_, _| {},
        }
    }

    /// 内置 `.srs` 已落盘的世界（`runtime_rules_dir` 下的 `.srs` 全有效）。
    fn deps_builtin_srs_present() -> CustomRulesDeps {
        CustomRulesDeps {
            is_valid_srs_fn: |p| p.starts_with("/fake/rules/") && p.ends_with(".srs"),
            ..deps_default()
        }
    }

    // warn 收集器：`log` 是裸 fn 指针（闭包捕获不了）⇒ thread_local sink（测试单线程内自洽）。
    // 与 route.rs 测试同手法（`route.rs:2087`）。
    thread_local! {
        static WARN_SINK: std::cell::RefCell<Vec<String>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    fn capture_warn(lvl: LogLevel, msg: &str) {
        assert_eq!(
            lvl,
            LogLevel::Warn,
            "剪枝告警必须是 warn 档（会被级别过滤吞掉的 info 等于没打）"
        );
        WARN_SINK.with(|s| s.borrow_mut().push(msg.to_string()));
    }
    fn take_warns() -> Vec<String> {
        WARN_SINK.with(|s| s.borrow_mut().drain(..).collect())
    }

    fn rule_ruleset(id: &str, values: &[&str]) -> Rule {
        rule_single(id, RuleType::RuleSet, values, RuleAction::Proxy)
    }

    fn deps_ext_present() -> CustomRulesDeps {
        // ext JSON 已落盘（exists_fn=true）；is_valid_srs_fn 保持 false 证明 ext 分支不再看它。
        CustomRulesDeps {
            exists_fn: |_| true,
            is_valid_srs_fn: |_| false,
            ..deps_default()
        }
    }

    fn rule_single(id: &str, t: RuleType, values: &[&str], action: RuleAction) -> Rule {
        Rule {
            id: id.into(),
            type_field: t,
            values: values.iter().map(|s| s.to_string()).collect(),
            conditions: None,
            combine_mode: None,
            action,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        }
    }

    fn empty_id_map() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn single_domain_rule_proxy_default() {
        let rules_arr = [rule_single(
            "r1",
            RuleType::Domain,
            &["a.com"],
            RuleAction::Proxy,
        )];
        let id_map = empty_id_map();
        let result = build_custom_rules(
            &rules_arr,
            None,
            &id_map,
            "proxy-selector",
            &[],
            false,
            &deps_default(),
        );
        assert_eq!(result.rules.len(), 1);
        assert_eq!(result.rules[0].domain, Some(vec!["a.com".to_string()]));
        assert_eq!(result.rules[0].outbound.as_deref(), Some("rule-sel-r1"));
        assert_eq!(result.rules[0].action.as_deref(), Some("route"));
    }

    #[test]
    fn direct_action_outbound() {
        let rules_arr = [rule_single(
            "r1",
            RuleType::IpCidr,
            &["10.0.0.0/8"],
            RuleAction::Direct,
        )];
        let id_map = empty_id_map();
        let result = build_custom_rules(
            &rules_arr,
            None,
            &id_map,
            "proxy-selector",
            &[],
            false,
            &deps_default(),
        );
        assert_eq!(result.rules[0].outbound.as_deref(), Some("direct"));
    }

    /// 阻断动作 ⇒ 规则级 `action:"reject"` + **绝不写 outbound**。
    ///
    /// 两条断言各锁一半，缺一不可：
    ///  - 只断 action ⇒ 漏 `outbound:"block"` 残留，配出的规则同时有 reject 与 outbound，
    ///    sing-box 会忽略后者，但下游 `is_proxy_out`（`route.rs` 的 udp443 配对）会按 outbound
    ///    反推成「走代理」，给阻断规则白配一条 udp443 reject。
    ///  - 只断 outbound is_none ⇒ 漏掉 action 覆盖，规则退化成 `action:"route"` 且无出站 ⇒
    ///    落到 `route.final`（= proxy-selector）⇒ **本该阻断的流量被放去代理**，静默失效。
    ///
    /// 变异锁：把 `apply_rule_action` 的 Block 腿改回 `Some("block")` → 两条断言同时红。
    #[test]
    fn block_action_emits_rule_level_reject_without_outbound() {
        let rules_arr = [rule_single(
            "r1",
            RuleType::DomainKeyword,
            &["ads"],
            RuleAction::Block,
        )];
        let id_map = empty_id_map();
        let result = build_custom_rules(
            &rules_arr,
            None,
            &id_map,
            "proxy-selector",
            &[],
            false,
            &deps_default(),
        );
        assert_eq!(result.rules[0].action.as_deref(), Some("reject"));
        assert_eq!(
            result.rules[0].outbound, None,
            "reject 是规则级动作，写 outbound 会让下游把阻断规则误判成走代理"
        );
        // `no_drop:true` 不是可选装饰：缺了它 sing-box 会在 50 次/30s 后把阻断降级成静默丢包，
        // 高频命中的广告/遥测域名于是从「立刻被拒」变成「挂到超时」——与 legacy `block` 不等价。
        assert_eq!(
            result.rules[0].no_drop,
            Some(true),
            "阻断规则必须 no_drop:true 才与 legacy `block` 出站等价（默认会泛洪降级成 drop）"
        );
    }

    #[test]
    fn disabled_rule_skipped() {
        let mut r = rule_single("r1", RuleType::Domain, &["a.com"], RuleAction::Proxy);
        r.enabled = false;
        let id_map = empty_id_map();
        let result = build_custom_rules(
            &[r],
            None,
            &id_map,
            "proxy-selector",
            &[],
            false,
            &deps_default(),
        );
        assert!(result.rules.is_empty());
    }

    #[test]
    fn geosite_emits_rule_set_tag() {
        let rules_arr = [rule_single(
            "r1",
            RuleType::Geosite,
            &["cn", "ads"],
            RuleAction::Proxy,
        )];
        let id_map = empty_id_map();
        let result = build_custom_rules(
            &rules_arr,
            None,
            &id_map,
            "proxy-selector",
            &[],
            false,
            &deps_default(),
        );
        assert_eq!(result.rules.len(), 1);
        // geosite → rule_set: [geosite-cn, geosite-ads]
        let rs = result.rules[0].rule_set.as_ref().expect("应有 rule_set");
        match rs {
            crate::singbox::OneOrMany::Many(arr) => {
                assert_eq!(
                    arr,
                    &vec!["geosite-cn".to_string(), "geosite-ads".to_string()]
                );
            }
            _ => panic!("rule_set 应为数组"),
        }
    }

    #[test]
    fn cross_dimension_or_emits_logical() {
        // domain + port 跨维度 OR → logical。
        let rule = Rule {
            id: "r1".into(),
            type_field: RuleType::Domain,
            values: vec!["a.com".into()],
            conditions: Some(vec![
                RuleCondition {
                    type_field: RuleType::Domain,
                    values: vec!["a.com".into()],
                },
                RuleCondition {
                    type_field: RuleType::Port,
                    values: vec!["443".into()],
                },
            ]),
            combine_mode: None,
            action: RuleAction::Proxy,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        };
        let id_map = empty_id_map();
        let result = build_custom_rules(
            &[rule],
            None,
            &id_map,
            "proxy-selector",
            &[],
            false,
            &deps_default(),
        );
        assert_eq!(result.rules.len(), 1);
        assert_eq!(result.rules[0].type_field.as_deref(), Some("logical"));
        assert_eq!(result.rules[0].mode.as_deref(), Some("or"));
    }

    #[test]
    fn ext_branch_uses_exists_fn_not_srs() {
        // exists_fn=true（ext JSON 已落盘）→ 固化 {rule_set: custom-rule-r1} + 注册 local rule_set。
        // is_valid_srs_fn 保持 false，证明 ext 分支已切到 exists_fn（变异：若仍看 srs_fn → 此测试红）。
        let rules_arr = [rule_single(
            "r1",
            RuleType::Domain,
            &["a.com"],
            RuleAction::Proxy,
        )];
        let id_map = empty_id_map();
        let result = build_custom_rules(
            &rules_arr,
            None,
            &id_map,
            "proxy-selector",
            &[],
            false,
            &deps_ext_present(),
        );
        assert!(
            result.rule_sets.iter().any(|rs| rs.tag == "custom-rule-r1"),
            "应注册 ext local rule_set"
        );
        assert_eq!(result.rules.len(), 1);
        let rule = &result.rules[0];
        assert!(rule.domain.is_none(), "ext 分支不应内联 domain");
        match rule.rule_set.as_ref().expect("应有 rule_set") {
            crate::singbox::OneOrMany::Many(arr) => {
                assert_eq!(arr, &vec!["custom-rule-r1".to_string()])
            }
            crate::singbox::OneOrMany::One(s) => assert_eq!(s, "custom-rule-r1"),
        }
        assert_eq!(rule.outbound.as_deref(), Some("rule-sel-r1"));
        assert_eq!(rule.action.as_deref(), Some("route"));
    }

    #[test]
    fn ext_branch_falls_back_inline_when_file_absent() {
        // exists_fn=false（未落盘）→ 回落 inline（domain 内联，无 rule_set）。
        let rules_arr = [rule_single(
            "r1",
            RuleType::Domain,
            &["a.com"],
            RuleAction::Proxy,
        )];
        let id_map = empty_id_map();
        let result = build_custom_rules(
            &rules_arr,
            None,
            &id_map,
            "proxy-selector",
            &[],
            false,
            &deps_default(),
        );
        assert_eq!(result.rules.len(), 1);
        assert_eq!(result.rules[0].domain, Some(vec!["a.com".to_string()]));
        assert!(result.rules[0].rule_set.is_none());
        assert!(result.rule_sets.is_empty());
    }

    #[test]
    fn ext_dns_registration_uses_exists_fn() {
        // bypassFakeIP + register_dns_bypass + exists_fn=true → 注册 <base>-dns rule_set。
        let mut r = rule_single("r1", RuleType::Domain, &["a.com"], RuleAction::Proxy);
        r.bypass_fakeip = Some(true);
        let id_map = empty_id_map();
        let result = build_custom_rules(
            &[r],
            None,
            &id_map,
            "proxy-selector",
            &[],
            true, // register_dns_bypass
            &deps_ext_present(),
        );
        assert!(
            result
                .rule_sets
                .iter()
                .any(|rs| rs.tag == "custom-rule-r1-dns"),
            "应注册 .dns.json rule_set"
        );
    }

    #[test]
    fn proxy_rule_with_target_server_uses_rule_sel() {
        // 指定 targetServerId 的 proxy 规则 → rule-sel-<id>（anti-drift）。
        let mut r = rule_single(
            "rule42",
            RuleType::Domain,
            &["fixed.com"],
            RuleAction::Proxy,
        );
        r.target_server_id = Some("s2".into());
        let id_map = empty_id_map();
        let result = build_custom_rules(
            &[r],
            None,
            &id_map,
            "proxy-selector",
            &[],
            false,
            &deps_default(),
        );
        assert_eq!(result.rules[0].outbound.as_deref(), Some("rule-sel-rule42"));
    }

    // ── res:builtin:<tag> 内置资源引用 ────────────────────────────────────────────
    //
    // **变异锁**：把 `resolve_resource_rule_set` 的内置分支改回恒 `None`（原 `builtin_rule_set_file_name`
    // 占位实现），下面 `builtin_ref_*` 三条会立刻转红——规则会被整条剪掉、rule_set 注册也消失。

    #[test]
    fn builtin_ref_emits_rule_set_and_definition() {
        // res:builtin:geosite-cn + 文件已落盘 → 规则保留 + 注册 local/binary rule_set。
        let result = build_custom_rules(
            &[rule_ruleset("r1", &["res:builtin:geosite-cn"])],
            None,
            &empty_id_map(),
            "proxy-selector",
            &[],
            false,
            &deps_builtin_srs_present(),
        );
        assert_eq!(result.rules.len(), 1, "内置资源规则不得被静默剪掉");
        match result.rules[0].rule_set.as_ref().expect("应有 rule_set") {
            crate::singbox::OneOrMany::Many(arr) => {
                assert_eq!(arr, &vec!["geosite-cn".to_string()])
            }
            crate::singbox::OneOrMany::One(s) => assert_eq!(s, "geosite-cn"),
        }
        // 引用必须有配套定义，否则 route 末尾的悬空剪枝会把它再剪一次（sing-box 侧则是 FATAL）。
        let rs = result
            .rule_sets
            .iter()
            .find(|rs| rs.tag == "geosite-cn")
            .expect("应注册 geosite-cn rule_set 定义");
        assert_eq!(rs.type_field, "local");
        assert_eq!(rs.format, "binary");
        assert_eq!(rs.path.as_deref(), Some("/fake/rules/geosite-cn.srs"));
    }

    #[test]
    fn builtin_ref_uses_catalog_file_name_not_tag() {
        // geosite-category-ai 的落盘名是 geosite-category-ai-!cn.srs（MetaCubeX 无裸 category-ai .srs）。
        // 若有人把路径改回 `<tag>.srs` 拼接，本条会红 —— 那正是「文件名靠猜」的整类 bug。
        let result = build_custom_rules(
            &[rule_ruleset("r1", &["res:builtin:geosite-category-ai"])],
            None,
            &empty_id_map(),
            "proxy-selector",
            &[],
            false,
            &deps_builtin_srs_present(),
        );
        let rs = result
            .rule_sets
            .iter()
            .find(|rs| rs.tag == "geosite-category-ai")
            .expect("tag 仍是 geosite-category-ai");
        assert_eq!(
            rs.path.as_deref(),
            Some("/fake/rules/geosite-category-ai-!cn.srs")
        );
    }

    #[test]
    fn builtin_ref_dedupes_repeated_tag() {
        // 同一条件里同一 builtin 引用两次 → 定义与引用都只留一份（重复 tag 会让 sing-box FATAL）。
        let result = build_custom_rules(
            &[rule_ruleset(
                "r1",
                &["res:builtin:geoip-cn", "res:builtin:geoip-cn"],
            )],
            None,
            &empty_id_map(),
            "proxy-selector",
            &[],
            false,
            &deps_builtin_srs_present(),
        );
        assert_eq!(
            result
                .rule_sets
                .iter()
                .filter(|rs| rs.tag == "geoip-cn")
                .count(),
            1,
            "rule_set 定义须按 tag 去重"
        );
        match result.rules[0].rule_set.as_ref().expect("应有 rule_set") {
            crate::singbox::OneOrMany::Many(arr) => {
                assert_eq!(arr, &vec!["geoip-cn".to_string()])
            }
            crate::singbox::OneOrMany::One(s) => assert_eq!(s, "geoip-cn"),
        }
    }

    #[test]
    fn builtin_ref_missing_srs_is_skipped_with_warn() {
        // 文件缺失/损坏 → 整条剪掉（不引用不存在的 rule_set），且**必须**留下 warn。
        take_warns();
        let deps = CustomRulesDeps {
            log: capture_warn,
            ..deps_default() // is_valid_srs_fn=false → 内置 .srs 一个都不在
        };
        let result = build_custom_rules(
            &[rule_ruleset("r1", &["res:builtin:geosite-cn"])],
            None,
            &empty_id_map(),
            "proxy-selector",
            &[],
            false,
            &deps,
        );
        assert!(result.rules.is_empty(), "文件缺失须 fail-closed 剪掉规则");
        assert!(result.rule_sets.is_empty());
        let warns = take_warns();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("内置资源文件缺失/损坏") && m.contains("geosite-cn.srs")),
            "规则被剪必须留线索（生产曾是 no-op，剪零告警）：{warns:?}"
        );
    }

    #[test]
    fn unknown_builtin_tag_falls_through_to_resources_then_warns() {
        // catalog 未命中的 builtin: tag → 按普通资源 id 再查一次（上游 if/else 结构），查不到才报
        // 「资源不存在」。早退实现下这条 warn 永不出现。
        take_warns();
        let deps = CustomRulesDeps {
            log: capture_warn,
            ..deps_builtin_srs_present()
        };
        let result = build_custom_rules(
            &[rule_ruleset("r1", &["res:builtin:no-such-tag"])],
            None,
            &empty_id_map(),
            "proxy-selector",
            &[],
            false,
            &deps,
        );
        assert!(result.rules.is_empty());
        let warns = take_warns();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("资源不存在") && m.contains("res:builtin:no-such-tag")),
            "未知 builtin tag 须落到「资源不存在」告警：{warns:?}"
        );
    }

    #[test]
    fn remote_url_rule_set_warns() {
        // 远程 URL 已不再支持 → 跳过 + warn（第三条静音路径）。
        take_warns();
        let deps = CustomRulesDeps {
            log: capture_warn,
            ..deps_default()
        };
        let result = build_custom_rules(
            &[rule_ruleset("r1", &["https://example.com/foo.srs"])],
            None,
            &empty_id_map(),
            "proxy-selector",
            &[],
            false,
            &deps,
        );
        assert!(result.rules.is_empty());
        let warns = take_warns();
        assert!(
            warns.iter().any(|m| m.contains("远程 URL 已不再支持")),
            "远程 URL 跳过须留线索：{warns:?}"
        );
    }
}
