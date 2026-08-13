//! 自定义规则 L3 外化决策（上游 `custom-rule-files.ts` 1:1 移植）。
//!
//! 把「全条件可 headless 表达」的启用 customRule 外化为 per-rule local rule_set 文件，
//! route 规则固化为 `{rule_set:<base>}`；编辑值 → 原子替换文件 → sing-box fswatch 热重载零重启。
//!
//! 关键不变量：本模块的可外化判定/值翻译/mergeable·fail-closed·logical 结构必须与
//! buildCustomRules（route 侧）和 buildDnsConfig 的 bypassFakeIP 块（DNS 侧）逐字等价。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use sha1::{Digest, Sha1};

use crate::user_config::app_config::UserConfig;
use crate::user_config::proxy_mode::ProxyMode;
use crate::user_config::rule::{CombineMode, Rule, RuleCondition, RuleType};
use crate::user_config::rules::{parse_port_values, rule_conditions};

/// 可外化的条件类型：均有 headless source 等价字段。
/// geosite/geoip/ruleSet 不可（headless 不能嵌套 rule_set）。上游 `EXT_TYPES`。
pub fn is_ext_type(t: RuleType) -> bool {
    matches!(
        t,
        RuleType::Domain
            | RuleType::DomainSuffix
            | RuleType::DomainKeyword
            | RuleType::DomainRegex
            | RuleType::IpCidr
            | RuleType::SourceIpCidr
            | RuleType::Port
            | RuleType::SourcePort
            | RuleType::ProcessName
            | RuleType::ProcessPath
    )
}

/// 目的地 OR 组：单条 default rule 内原生 OR。上游 `OR_GROUP`。
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

fn trim_vals(cond: &RuleCondition) -> Vec<String> {
    cond.values
        .iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// 单个 EXT 条件 → headless matcher 字段（BTreeMap 保序，对拍确定性）。
/// 无有效值（含端口全非法）→ None（= hasMatcher=false）。非 EXT 类型 → None。
/// 上游 `condMatcherFields`。
pub fn cond_matcher_fields(
    cond: &RuleCondition,
) -> Option<BTreeMap<String, Vec<serde_json::Value>>> {
    let vals = trim_vals(cond);
    if vals.is_empty() {
        return None;
    }
    let mut o = BTreeMap::new();
    match cond.type_field {
        RuleType::Domain => {
            o.insert("domain".into(), into_json_array(vals));
        }
        RuleType::DomainSuffix => {
            // domain_suffix 匹配域名及子域名；剥 *. 前缀（与 route 侧一致）。
            let stripped: Vec<String> = vals
                .iter()
                .map(|d| {
                    if let Some(rest) = d.strip_prefix("*.") {
                        rest.to_string()
                    } else {
                        d.clone()
                    }
                })
                .collect();
            o.insert("domain_suffix".into(), into_json_array(stripped));
        }
        RuleType::DomainKeyword => {
            o.insert("domain_keyword".into(), into_json_array(vals));
        }
        RuleType::DomainRegex => {
            o.insert("domain_regex".into(), into_json_array(vals));
        }
        RuleType::IpCidr => {
            o.insert("ip_cidr".into(), into_json_array(vals));
        }
        RuleType::SourceIpCidr => {
            o.insert("source_ip_cidr".into(), into_json_array(vals));
        }
        RuleType::Port | RuleType::SourcePort => {
            let (ports, ranges) = parse_port_values(&vals);
            if ports.is_empty() && ranges.is_empty() {
                return None;
            }
            let (port_key, range_key) = if matches!(cond.type_field, RuleType::Port) {
                ("port", "port_range")
            } else {
                ("source_port", "source_port_range")
            };
            if !ports.is_empty() {
                o.insert(
                    port_key.into(),
                    ports.into_iter().map(serde_json::Value::from).collect(),
                );
            }
            if !ranges.is_empty() {
                o.insert(
                    range_key.into(),
                    ranges.into_iter().map(serde_json::Value::from).collect(),
                );
            }
        }
        RuleType::ProcessName => {
            o.insert("process_name".into(), into_json_array(vals));
        }
        RuleType::ProcessPath => {
            o.insert("process_path".into(), into_json_array(vals));
        }
        // 非 EXT 类型 → None。
        _ => return None,
    }
    Some(o)
}

fn into_json_array(vals: Vec<String>) -> Vec<serde_json::Value> {
    vals.into_iter().map(serde_json::Value::from).collect()
}

/// 合并字段（值并集）。上游 `mergeFields`。
fn merge_fields(
    target: &mut BTreeMap<String, Vec<serde_json::Value>>,
    src: BTreeMap<String, Vec<serde_json::Value>>,
) {
    for (k, v) in src {
        target.entry(k).or_default().extend(v);
    }
}

/// 规则外化计划。上游 `RulePlan`。
#[derive(Debug, Clone, PartialEq)]
pub enum RulePlan {
    /// 任一条件 ∉ EXT_TYPES（geo/ruleSet/混合）→ inline 生成。
    Inline,
    /// 全 EXT 但 fail-closed 跳过 route；DNS 侧仍可能消费。
    ExtSkip {
        dns_rules: Option<Vec<serde_json::Value>>,
    },
    /// 可外化：fileRules = headless 文件内容；dnsRules = bypass DNS 域名规则。
    Ext {
        file_rules: Vec<serde_json::Value>,
        dns_rules: Option<Vec<serde_json::Value>>,
    },
}

/// bypassFakeIP 规则的 DNS 域名 headless 规则。
/// domain_suffix 用 flatMap [d, ".d"] 形态（保留 DNS 侧今日编码，与 route 侧裸后缀刻意不同）。
/// 上游 `dnsHeadlessRules`。
fn dns_headless_rules(rule: &Rule) -> Option<Vec<serde_json::Value>> {
    if rule.bypass_fakeip != Some(true) {
        return None;
    }
    let mut domain = Vec::new();
    let mut suffix = Vec::new();
    let mut keyword = Vec::new();
    for cond in &rule_conditions(rule) {
        let vals = trim_vals(cond);
        if vals.is_empty() {
            continue;
        }
        match cond.type_field {
            RuleType::Domain => domain.extend(vals),
            RuleType::DomainSuffix => {
                suffix.extend(vals.into_iter().map(|d| {
                    if let Some(rest) = d.strip_prefix("*.") {
                        rest.to_string()
                    } else {
                        d
                    }
                }));
            }
            RuleType::DomainKeyword => keyword.extend(vals),
            _ => {}
        }
    }
    if domain.is_empty() && suffix.is_empty() && keyword.is_empty() {
        return None;
    }
    let mut m = serde_json::Map::new();
    if !domain.is_empty() {
        m.insert("domain".into(), into_json_array(domain).into());
    }
    if !suffix.is_empty() {
        // flatMap [d, ".d"]。
        let flat: Vec<serde_json::Value> = suffix
            .into_iter()
            .flat_map(|d| {
                vec![
                    serde_json::Value::from(d.clone()),
                    serde_json::Value::from(format!(".{d}")),
                ]
            })
            .collect();
        m.insert("domain_suffix".into(), serde_json::Value::Array(flat));
    }
    if !keyword.is_empty() {
        m.insert("domain_keyword".into(), into_json_array(keyword).into());
    }
    Some(vec![serde_json::Value::Object(m)])
}

/// 规则外化计划（mergeable/fail-closed/logical 判定）。上游 `planCustomRule`。
pub fn plan_custom_rule(rule: &Rule) -> RulePlan {
    let raw_conds = rule_conditions(rule);
    if raw_conds.iter().any(|c| !is_ext_type(c.type_field)) {
        return RulePlan::Inline;
    }

    let dns_rules = dns_headless_rules(rule);
    let conds: Vec<(&RuleCondition, Vec<String>)> = raw_conds
        .iter()
        .map(|c| (c, trim_vals(c)))
        .filter(|(_, v)| !v.is_empty())
        .collect();
    if conds.is_empty() {
        return RulePlan::ExtSkip { dns_rules };
    }
    // AND 模式任一条件值全空被丢 → 整条跳过（fail-closed）。
    let is_and = rule.combine_mode == Some(CombineMode::And);
    if is_and && conds.len() < raw_conds.len() {
        return RulePlan::ExtSkip { dns_rules };
    }

    let mergeable =
        conds.len() == 1 || (!is_and && conds.iter().all(|(c, _)| is_or_group(c.type_field)));

    let file_rules: Option<Vec<serde_json::Value>> = if mergeable {
        let mut merged = BTreeMap::new();
        let mut has = false;
        for (c, _) in &conds {
            if let Some(f) = cond_matcher_fields(c) {
                merge_fields(&mut merged, f);
                has = true;
            }
        }
        if has {
            Some(vec![fields_to_object(merged)])
        } else {
            None
        }
    } else {
        let mut sub_rules: Vec<serde_json::Value> = Vec::new();
        let mut dropped = false;
        for (c, _) in &conds {
            match cond_matcher_fields(c) {
                Some(f) => sub_rules.push(fields_to_object(f)),
                None => dropped = true,
            }
        }
        if is_and && dropped {
            None // fail-closed
        } else if sub_rules.len() == 1 {
            Some(vec![sub_rules.into_iter().next().unwrap()])
        } else if sub_rules.len() > 1 {
            let mode = rule.combine_mode.unwrap_or(CombineMode::Or);
            let mode_str = match mode {
                CombineMode::And => "and",
                CombineMode::Or => "or",
            };
            let mut logical = serde_json::Map::new();
            logical.insert("type".into(), "logical".into());
            logical.insert("mode".into(), mode_str.into());
            logical.insert("rules".into(), serde_json::Value::Array(sub_rules));
            Some(vec![serde_json::Value::Object(logical)])
        } else {
            None
        }
    };

    match file_rules {
        Some(file_rules) => RulePlan::Ext {
            file_rules,
            dns_rules,
        },
        None => RulePlan::ExtSkip { dns_rules },
    }
}

/// BTreeMap<String, Vec<Value>> → serde_json::Value::Object（保序）。
fn fields_to_object(fields: BTreeMap<String, Vec<serde_json::Value>>) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (k, v) in fields {
        m.insert(k, serde_json::Value::Array(v));
    }
    serde_json::Value::Object(m)
}

/// 文件名安全 base：id 仅 [A-Za-z0-9_-] → custom-rule-<id>；否则 sha1 派生唯一名。
/// 上游 `customRuleFileBase`：`/^[A-Za-z0-9_-]+$/` → `custom-rule-<id>`；
/// 否则 `custom-rule-h${createHash('sha1').update(id).digest('hex').slice(0,12)}`。
///
/// 落盘后非法 id 会以此名暴露到磁盘（build_custom_rule_files / 孤儿对账），
/// 故 hash 分支必须真派生唯一名（撞车即两条规则共用一文件，值互相覆盖）。
pub fn custom_rule_file_base(id: &str) -> String {
    if !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return format!("custom-rule-{id}");
    }
    // 非法 id（空 / 含特殊字符）→ sha1(id) 十六进制前 12 位。
    let mut hasher = Sha1::new();
    hasher.update(id.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("custom-rule-h{}", &hex[..12])
}

/// 是否启用 FakeIP（纯看开关，不分模式，缺省 true）。上游 `usesFakeIp`。
pub fn uses_fake_ip(enable_fake_ip: Option<bool>) -> bool {
    enable_fake_ip.unwrap_or(true)
}

/// 当前配置「应存在的外化文件全集」：fileName → JSON 内容。上游 `buildCustomRuleFiles`。
///
/// 起核前落盘与启动孤儿对账清扫都以此为期望集：非 smart 模式返空集
///（global/direct 下 route 侧 generateCustomRules 不执行 → rule_set 无消费者 → 已存在的外化文件被当孤儿清扫）。
///
/// - ext 计划 → `<base>.json`（`{version:1, rules: fileRules}`）。
/// - ext 与 ext-skip 都可能有 `<base>.dns.json`（route 跳过但 DNS 仍消费 bypass 域名值），仅 FakeIP 开时。
///
/// 纯函数（不触 FS）：实际写盘 / mkdir / 孤儿 unlink 由运行时层（起核前）编排，
/// 消费本函数的期望集 + [`is_custom_rule_orphan_file`] 谓词。JSON 缩进 2 空格（对齐 TS `JSON.stringify(x, null, 2)`）。
pub fn build_custom_rule_files(config: &UserConfig) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    // Polaris: `(config.proxyMode || 'smart').toLowerCase() !== 'smart'` → 空集。
    if config.proxy_mode != ProxyMode::Smart {
        return out;
    }
    let fake_ip = uses_fake_ip(config.dns_config.as_ref().and_then(|d| d.enable_fake_ip));
    for rule in &config.custom_rules {
        if !rule.enabled {
            continue;
        }
        let plan = plan_custom_rule(rule);
        if matches!(plan, RulePlan::Inline) {
            continue;
        }
        let base = custom_rule_file_base(&rule.id);
        if let RulePlan::Ext { file_rules, .. } = &plan {
            out.insert(format!("{base}.json"), rule_file_json(file_rules));
        }
        // DNS 文件：ext 与 ext-skip 都可能要（route 跳过但 DNS 仍消费 bypass 域名值）。
        let dns_rules = match &plan {
            RulePlan::Ext { dns_rules, .. } | RulePlan::ExtSkip { dns_rules } => dns_rules.as_ref(),
            RulePlan::Inline => None,
        };
        if fake_ip {
            if let Some(dns_rules) = dns_rules {
                out.insert(format!("{base}.dns.json"), rule_file_json(dns_rules));
            }
        }
    }
    out
}

/// `{version:1, rules:[...]}` 序列化（2 空格缩进，对齐 TS `JSON.stringify(x, null, 2)`）。
fn rule_file_json(rules: &[serde_json::Value]) -> String {
    serde_json::to_string_pretty(&serde_json::json!({ "version": 1, "rules": rules }))
        .expect("外化规则文件 JSON 序列化不应失败")
}

/// 外化规则目录里的「孤儿文件」判定（单一真值）。上游 `isCustomRuleOrphanFile`：
/// 正则 `^custom-rule-.+\.json(?:(?:\.[^.]+)*\.tmp)?$`——裸 `custom-rule-<id>.json`（删规则/禁用/
/// 转 inline/改 id/direct 切换的遗留）+ 原子写残留 `.json(.<seg>)*.tmp`（唯一后缀 `.<pid>.<rand>.tmp` 或裸 `.tmp`）。
///
/// 起核前清扫：`is_custom_rule_orphan_file(name) && !expected.contains_key(name)` → unlink。
pub fn is_custom_rule_orphan_file(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("custom-rule-") else {
        return false;
    };
    // 分支 1：裸 `.+\.json`（`.+` 需 ≥1 字符在 `.json` 前）。
    if let Some(pre) = rest.strip_suffix(".json") {
        if !pre.is_empty() {
            return true;
        }
    }
    // 分支 2：`.+\.json(?:\.[^.]+)*\.tmp`。
    if let Some(head) = rest.strip_suffix(".tmp") {
        // `.json`（ASCII needle）出现位置均在字符边界，match_indices 切片 UTF-8 安全。
        // idx>0 = `.json` 前 ≥1 字符（对齐 `.+`）；其后缀须匹配 `(?:\.[^.]+)*`。
        for (idx, m) in head.match_indices(".json") {
            if idx == 0 {
                continue;
            }
            if is_tmp_dot_segments(&head[idx + m.len()..]) {
                return true;
            }
        }
    }
    false
}

/// `(?:\.[^.]+)*` 匹配：空串，或以 `.` 开头且每个点分段非空且无内嵌点。
fn is_tmp_dot_segments(s: &str) -> bool {
    s.is_empty() || (s.starts_with('.') && s[1..].split('.').all(|seg| !seg.is_empty()))
}

/// 外化 JSON 文件存在性检查（`existsSync` 等价，非 SRS 魔数）。
///
/// L3 ext 文件是 headless **JSON source**（非二进制 `.srs`）：用真存在性判定，
/// 绝不复用 `is_valid_srs_file`（读 3 字节 `SRS` 魔数）——JSON 永不含该魔数，
/// 复用会使「落盘后 ext 分支」100% 不可达（恒回落 inline）。route/DNS 侧 ext 分支的
/// `exists_fn` 生产默认注入此函数；对拍 fixture 注入固定值。
pub fn ext_rule_file_exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::rule::{RuleAction, RuleType};
    use serde_json::json;

    fn cond(t: RuleType, values: &[&str]) -> RuleCondition {
        RuleCondition {
            type_field: t,
            values: values.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn rule_single(t: RuleType, values: &[&str]) -> Rule {
        Rule {
            id: "r1".into(),
            type_field: t,
            values: values.iter().map(|s| s.to_string()).collect(),
            conditions: None,
            combine_mode: None,
            action: RuleAction::Proxy,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        }
    }

    #[test]
    fn ext_type_classification() {
        assert!(is_ext_type(RuleType::Domain));
        assert!(is_ext_type(RuleType::IpCidr));
        assert!(is_ext_type(RuleType::ProcessName));
        assert!(!is_ext_type(RuleType::Geosite));
        assert!(!is_ext_type(RuleType::Geoip));
        assert!(!is_ext_type(RuleType::RuleSet));
        assert!(!is_ext_type(RuleType::SourceMac));
    }

    #[test]
    fn cond_matcher_domain() {
        let c = cond(RuleType::Domain, &["a.com", "b.com"]);
        let f = cond_matcher_fields(&c).unwrap();
        assert_eq!(f["domain"], vec![json!("a.com"), json!("b.com")]);
    }

    #[test]
    fn cond_matcher_domain_suffix_strips_wildcard() {
        let c = cond(RuleType::DomainSuffix, &["*.example.com", "test.com"]);
        let f = cond_matcher_fields(&c).unwrap();
        assert_eq!(
            f["domain_suffix"],
            vec![json!("example.com"), json!("test.com")]
        );
    }

    #[test]
    fn cond_matcher_port_single_and_range() {
        let c = cond(RuleType::Port, &["443", "1000-2000"]);
        let f = cond_matcher_fields(&c).unwrap();
        assert_eq!(f["port"], vec![json!(443)]);
        assert_eq!(f["port_range"], vec![json!("1000:2000")]);
    }

    #[test]
    fn cond_matcher_port_all_invalid_returns_none() {
        let c = cond(RuleType::Port, &["abc", "0", "99999"]);
        assert!(cond_matcher_fields(&c).is_none());
    }

    #[test]
    fn cond_matcher_non_ext_returns_none() {
        let c = cond(RuleType::Geosite, &["cn"]);
        assert!(cond_matcher_fields(&c).is_none());
    }

    #[test]
    fn plan_inline_when_has_geosite() {
        let rule = Rule {
            id: "r1".into(),
            type_field: RuleType::Domain,
            values: vec!["a.com".into()],
            conditions: Some(vec![
                cond(RuleType::Domain, &["a.com"]),
                cond(RuleType::Geosite, &["cn"]),
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
        assert!(matches!(plan_custom_rule(&rule), RulePlan::Inline));
    }

    #[test]
    fn plan_ext_single_domain() {
        let rule = rule_single(RuleType::Domain, &["a.com"]);
        let plan = plan_custom_rule(&rule);
        match plan {
            RulePlan::Ext {
                file_rules,
                dns_rules,
            } => {
                assert!(dns_rules.is_none());
                assert_eq!(file_rules.len(), 1);
                assert_eq!(file_rules[0]["domain"], json!(["a.com"]));
            }
            _ => panic!("expected Ext, got {plan:?}"),
        }
    }

    #[test]
    fn plan_ext_or_group_mergeable() {
        let rule = Rule {
            id: "r1".into(),
            type_field: RuleType::Domain,
            values: vec!["a.com".into()],
            conditions: Some(vec![
                cond(RuleType::Domain, &["a.com"]),
                cond(RuleType::IpCidr, &["1.2.3.0/24"]),
            ]),
            combine_mode: None, // OR
            action: RuleAction::Proxy,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        };
        let plan = plan_custom_rule(&rule);
        match plan {
            RulePlan::Ext { file_rules, .. } => {
                assert_eq!(file_rules.len(), 1); // mergeable → 单 rule
                assert_eq!(file_rules[0]["domain"], json!(["a.com"]));
                assert_eq!(file_rules[0]["ip_cidr"], json!(["1.2.3.0/24"]));
            }
            _ => panic!("expected Ext, got {plan:?}"),
        }
    }

    #[test]
    fn plan_ext_logical_when_cross_dimension_or() {
        // domain + port 跨维度 OR → logical rule。
        let rule = Rule {
            id: "r1".into(),
            type_field: RuleType::Domain,
            values: vec!["a.com".into()],
            conditions: Some(vec![
                cond(RuleType::Domain, &["a.com"]),
                cond(RuleType::Port, &["443"]),
            ]),
            combine_mode: None, // OR
            action: RuleAction::Proxy,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        };
        let plan = plan_custom_rule(&rule);
        match plan {
            RulePlan::Ext { file_rules, .. } => {
                assert_eq!(file_rules.len(), 1);
                assert_eq!(file_rules[0]["type"], json!("logical"));
                assert_eq!(file_rules[0]["mode"], json!("or"));
                assert_eq!(file_rules[0]["rules"].as_array().unwrap().len(), 2);
            }
            _ => panic!("expected Ext, got {plan:?}"),
        }
    }

    #[test]
    fn plan_ext_skip_when_all_values_empty() {
        let rule = rule_single(RuleType::Domain, &["", "  "]);
        let plan = plan_custom_rule(&rule);
        assert!(matches!(plan, RulePlan::ExtSkip { .. }));
    }

    #[test]
    fn file_base_legal_id() {
        assert_eq!(custom_rule_file_base("r1"), "custom-rule-r1");
        assert_eq!(
            custom_rule_file_base("abc-123_def"),
            "custom-rule-abc-123_def"
        );
    }

    #[test]
    fn uses_fake_ip_default_true() {
        assert!(uses_fake_ip(None));
        assert!(uses_fake_ip(Some(true)));
        assert!(!uses_fake_ip(Some(false)));
    }

    // ── Fix 3：非法字符 rule id → sha1 派生唯一名（对齐 createHash('sha1').slice(0,12)）──
    #[test]
    fn file_base_illegal_id_sha1() {
        // sha1("a/b") = 3ec69c85a4ff...；前 12 位十六进制。
        assert_eq!(custom_rule_file_base("a/b"), "custom-rule-h3ec69c85a4ff");
        // sha1("has space") = e42b2b98cbee...
        assert_eq!(
            custom_rule_file_base("has space"),
            "custom-rule-he42b2b98cbee"
        );
        // 空 id 也走 hash（`.+` 不匹配空）：sha1("") = da39a3ee5e6b...
        assert_eq!(custom_rule_file_base(""), "custom-rule-hda39a3ee5e6b");
    }

    #[test]
    fn file_base_illegal_ids_no_collision() {
        // 落盘后暴露撞车：不同非法 id 派生不同文件名。
        assert_ne!(custom_rule_file_base("a/b"), custom_rule_file_base("a\\b"));
        assert_ne!(custom_rule_file_base("x.y"), custom_rule_file_base("x*y"));
    }

    // ── Fix 1a：build_custom_rule_files 期望集 ──
    fn user_config(proxy_mode: ProxyMode, rules: Vec<Rule>, fake_ip: Option<bool>) -> UserConfig {
        let mut c = UserConfig {
            proxy_mode,
            custom_rules: rules,
            ..Default::default()
        };
        if let Some(v) = fake_ip {
            c.dns_config = Some(crate::user_config::dns_config::DnsConfig {
                enable_fake_ip: Some(v),
                ..Default::default()
            });
        }
        c
    }

    #[test]
    fn build_files_non_smart_returns_empty() {
        let rules = vec![rule_single(RuleType::Domain, &["a.com"])];
        assert!(
            build_custom_rule_files(&user_config(ProxyMode::Global, rules.clone(), None))
                .is_empty()
        );
        assert!(build_custom_rule_files(&user_config(ProxyMode::Direct, rules, None)).is_empty());
    }

    #[test]
    fn build_files_smart_ext_json() {
        let cfg = user_config(
            ProxyMode::Smart,
            vec![rule_single(RuleType::Domain, &["a.com"])],
            None,
        );
        let out = build_custom_rule_files(&cfg);
        let content = out
            .get("custom-rule-r1.json")
            .expect("ext 计划应落盘 <base>.json");
        // 2 空格缩进（对齐 JSON.stringify(x, null, 2)）。
        assert!(content.contains("\n  \""), "应为 2 空格缩进 pretty");
        let v: serde_json::Value = serde_json::from_str(content).unwrap();
        assert_eq!(v["version"], json!(1));
        assert_eq!(v["rules"], json!([{ "domain": ["a.com"] }]));
        // 无 bypassFakeIP → 无 .dns.json。
        assert!(!out.contains_key("custom-rule-r1.dns.json"));
    }

    #[test]
    fn build_files_disabled_rule_skipped() {
        let mut r = rule_single(RuleType::Domain, &["a.com"]);
        r.enabled = false;
        assert!(build_custom_rule_files(&user_config(ProxyMode::Smart, vec![r], None)).is_empty());
    }

    #[test]
    fn build_files_inline_rule_skipped() {
        // geosite → inline（不可 headless）→ 无外化文件。
        let cfg = user_config(
            ProxyMode::Smart,
            vec![rule_single(RuleType::Geosite, &["cn"])],
            None,
        );
        assert!(build_custom_rule_files(&cfg).is_empty());
    }

    #[test]
    fn build_files_bypass_fakeip_emits_dns_json() {
        let mut r = rule_single(RuleType::Domain, &["a.com"]);
        r.bypass_fakeip = Some(true);
        let out = build_custom_rule_files(&user_config(ProxyMode::Smart, vec![r], Some(true)));
        assert!(out.contains_key("custom-rule-r1.json"));
        let dns = out
            .get("custom-rule-r1.dns.json")
            .expect("bypassFakeIP + fakeIp 开 → 应落盘 .dns.json");
        let v: serde_json::Value = serde_json::from_str(dns).unwrap();
        assert_eq!(v["version"], json!(1));
        assert_eq!(v["rules"], json!([{ "domain": ["a.com"] }]));
    }

    #[test]
    fn build_files_bypass_fakeip_off_no_dns_json() {
        // FakeIP 关 → 不落 .dns.json（route 侧 ext .json 仍落）。
        let mut r = rule_single(RuleType::Domain, &["a.com"]);
        r.bypass_fakeip = Some(true);
        let out = build_custom_rule_files(&user_config(ProxyMode::Smart, vec![r], Some(false)));
        assert!(out.contains_key("custom-rule-r1.json"));
        assert!(!out.contains_key("custom-rule-r1.dns.json"));
    }

    // ── Fix 1b：孤儿文件谓词 ──
    #[test]
    fn orphan_file_matches() {
        // 裸 .json + .tmp 变体。
        assert!(is_custom_rule_orphan_file("custom-rule-r1.json"));
        assert!(is_custom_rule_orphan_file("custom-rule-r1.dns.json"));
        assert!(is_custom_rule_orphan_file("custom-rule-r1.json.tmp"));
        assert!(is_custom_rule_orphan_file(
            "custom-rule-r1.json.12345.a1b2c3.tmp"
        ));
        assert!(is_custom_rule_orphan_file("custom-rule-hdeadbeef1234.json"));
    }

    #[test]
    fn orphan_file_rejects() {
        assert!(!is_custom_rule_orphan_file("custom-rule-.json")); // `.+` 需 ≥1 字符
        assert!(!is_custom_rule_orphan_file("custom-rule-r1.txt"));
        assert!(!is_custom_rule_orphan_file("custom-rule-r1.jsontmp"));
        assert!(!is_custom_rule_orphan_file("other-r1.json"));
        assert!(!is_custom_rule_orphan_file("custom-rule-r1.json.")); // 空 seg 结尾
        assert!(!is_custom_rule_orphan_file("custom-rule-r1.json..tmp")); // 空 seg
        assert!(!is_custom_rule_orphan_file("r1.json"));
    }
}
