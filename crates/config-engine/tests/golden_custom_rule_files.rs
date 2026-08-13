//! B1 金样对拍 harness —— custom_rule_files（planCustomRule + condMatcherFields）。
//!
//! 读 fixtures/custom-rule-files.json（TS 导出的 plan + cond cases），
//! 逐条调 Rust plan_custom_rule / cond_matcher_fields，与 TS output 逐字节 diff。

use polaris_config_engine::builder::custom_rule_files::{
    cond_matcher_fields, plan_custom_rule, RulePlan,
};
use polaris_config_engine::user_config::rule::{
    CombineMode, Rule, RuleAction, RuleCondition, RuleType,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Fixture {
    #[serde(rename = "planCases")]
    plan_cases: Vec<PlanCase>,
    #[serde(rename = "condCases")]
    cond_cases: Vec<CondCase>,
}

#[derive(Debug, Deserialize)]
struct PlanCase {
    name: String,
    input: serde_json::Value,
    output: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct CondCase {
    name: String,
    input: serde_json::Value,
    output: serde_json::Value,
}

fn parse_rule_type(s: &str) -> RuleType {
    match s {
        "domain" => RuleType::Domain,
        "domainSuffix" => RuleType::DomainSuffix,
        "domainKeyword" => RuleType::DomainKeyword,
        "domainRegex" => RuleType::DomainRegex,
        "ipCidr" => RuleType::IpCidr,
        "sourceIpCidr" => RuleType::SourceIpCidr,
        "port" => RuleType::Port,
        "sourcePort" => RuleType::SourcePort,
        "sourceMac" => RuleType::SourceMac,
        "sourceHostname" => RuleType::SourceHostname,
        "processName" => RuleType::ProcessName,
        "processPath" => RuleType::ProcessPath,
        "geosite" => RuleType::Geosite,
        "geoip" => RuleType::Geoip,
        "ruleSet" => RuleType::RuleSet,
        _ => panic!("未知 RuleType: {s}"),
    }
}

fn parse_rule(v: &serde_json::Value) -> Rule {
    let obj = v.as_object().expect("rule 应为 object");
    let type_str = obj
        .get("type")
        .and_then(|t| t.as_str())
        .expect("rule.type 必填");
    let type_field = parse_rule_type(type_str);
    let values: Vec<String> = obj
        .get("values")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let conditions: Option<Vec<RuleCondition>> =
        obj.get("conditions").and_then(|c| c.as_array()).map(|arr| {
            arr.iter()
                .map(|c| {
                    let co = c.as_object().unwrap();
                    RuleCondition {
                        type_field: parse_rule_type(co.get("type").unwrap().as_str().unwrap()),
                        values: co
                            .get("values")
                            .unwrap()
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect(),
                    }
                })
                .collect()
        });
    let action = match obj
        .get("action")
        .and_then(|a| a.as_str())
        .unwrap_or("proxy")
    {
        "proxy" => RuleAction::Proxy,
        "direct" => RuleAction::Direct,
        "block" => RuleAction::Block,
        _ => RuleAction::Proxy,
    };
    let combine_mode = obj
        .get("combineMode")
        .and_then(|c| c.as_str())
        .map(|s| match s {
            "and" => CombineMode::And,
            _ => CombineMode::Or,
        });
    let bypass_fakeip = obj.get("bypassFakeIP").and_then(|b| b.as_bool());
    Rule {
        id: obj
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("r1")
            .to_string(),
        type_field,
        values,
        conditions,
        combine_mode,
        action,
        enabled: obj.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true),
        bypass_fakeip,
        target_server_id: obj
            .get("targetServerId")
            .and_then(|t| t.as_str())
            .map(String::from),
        remarks: None,
        tls_spoof: obj
            .get("tlsSpoof")
            .and_then(|t| t.as_str())
            .map(String::from),
        tls_spoof_method: obj
            .get("tlsSpoofMethod")
            .and_then(|t| t.as_str())
            .map(String::from),
    }
}

fn parse_condition(v: &serde_json::Value) -> RuleCondition {
    let obj = v.as_object().expect("condition 应为 object");
    RuleCondition {
        type_field: parse_rule_type(obj.get("type").unwrap().as_str().unwrap()),
        values: obj
            .get("values")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
    }
}

/// RulePlan → 与 TS 对齐的 serde_json::Value（kind + fileRules/dnsRules）。
fn plan_to_json(plan: RulePlan) -> serde_json::Value {
    match plan {
        RulePlan::Inline => serde_json::json!({ "kind": "inline" }),
        RulePlan::ExtSkip { dns_rules } => {
            serde_json::json!({ "kind": "ext-skip", "dnsRules": dns_rules })
        }
        RulePlan::Ext {
            file_rules,
            dns_rules,
        } => {
            serde_json::json!({ "kind": "ext", "fileRules": file_rules, "dnsRules": dns_rules })
        }
    }
}

#[test]
fn plan_custom_rule_matches_polaris() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/custom-rule-files.json"
    );
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("读 fixture 失败: {e}"));
    let fixture: Fixture = serde_json::from_str(&raw).expect("fixture 解析失败");

    let mut failures = Vec::new();
    for case in &fixture.plan_cases {
        let rule = parse_rule(&case.input);
        let rust_plan = plan_custom_rule(&rule);
        let rust_json = plan_to_json(rust_plan);
        if rust_json != case.output {
            failures.push(format!(
                "[{}] plan diff\n  TS: {}\n  Rust: {}",
                case.name, case.output, rust_json
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{}/{} plan cases 对拍失败:\n{}",
        failures.len(),
        fixture.plan_cases.len(),
        failures.join("\n")
    );
}

#[test]
fn cond_matcher_fields_matches_polaris() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/custom-rule-files.json"
    );
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("读 fixture 失败: {e}"));
    let fixture: Fixture = serde_json::from_str(&raw).expect("fixture 解析失败");

    let mut failures = Vec::new();
    for case in &fixture.cond_cases {
        let cond = parse_condition(&case.input);
        let rust_fields = cond_matcher_fields(&cond);
        // Rust 返回 Option<BTreeMap>，TS 返回 Record|null。
        // None → null；Some(map) → object（map 键序经 BTreeMap 排序，TS Record 键序是插入序，对拍用 Value 比较=忽略键序）。
        let rust_json: serde_json::Value = match rust_fields {
            None => serde_json::Value::Null,
            Some(map) => {
                let mut obj = serde_json::Map::new();
                for (k, v) in map {
                    obj.insert(k, serde_json::Value::Array(v));
                }
                serde_json::Value::Object(obj)
            }
        };
        if rust_json != case.output {
            failures.push(format!(
                "[{}] cond diff\n  TS: {}\n  Rust: {}",
                case.name, case.output, rust_json
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{}/{} cond cases 对拍失败:\n{}",
        failures.len(),
        fixture.cond_cases.len(),
        failures.join("\n")
    );
}
