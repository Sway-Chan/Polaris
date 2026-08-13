//! 规则工具函数（上游 `shared/rules.ts` 子集 1:1 移植）。
//!
//! ruleConditions（Rule → conditions[] 规范化）+ parsePortValues（端口/端口范围解析）。
//! custom-rule-files + buildCustomRules 共用。

#![forbid(unsafe_code)]

use crate::user_config::rule::{Rule, RuleCondition, RuleType};

/// 从 Rule 提取 conditions 数组（规范化：有 conditions 用之，否则用首条件镜像 type/values）。
/// 上游 `ruleConditions`。
pub fn rule_conditions(rule: &Rule) -> Vec<RuleCondition> {
    if let Some(conds) = &rule.conditions {
        if !conds.is_empty() {
            return conds.clone();
        }
    }
    vec![RuleCondition {
        type_field: rule.type_field,
        values: rule.values.clone(),
    }]
}

/// 端口 token 校验：`^\d{1,5}(-\d{1,5})?$` + 每段 1..=65535 + 范围 a<=b。
/// 上游 `validPortToken`（PORT_RE + 段校验）。
fn valid_port_token(v: &str) -> bool {
    // PORT_RE = /^\d{1,5}(-\d{1,5})?$/
    let parts: Vec<&str> = v.split('-').collect();
    let parsed: Vec<u32> = match parts.len() {
        1 => {
            if parts[0].len() > 5
                || parts[0].is_empty()
                || !parts[0].bytes().all(|b| b.is_ascii_digit())
            {
                return false;
            }
            vec![parts[0].parse::<u32>().unwrap_or(0)]
        }
        2 => {
            for p in &parts {
                if p.len() > 5 || p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return false;
                }
            }
            vec![
                parts[0].parse::<u32>().unwrap_or(0),
                parts[1].parse::<u32>().unwrap_or(0),
            ]
        }
        _ => return false,
    };
    parsed.iter().all(|&n| (1..=65535).contains(&n))
        && (parsed.len() == 1 || parsed[0] <= parsed[1])
}

/// 端口值解析：单值 → ports，范围 "a-b" → ranges（"a:b" 格式，sing-box port_range）。
/// 上游 `parsePortValues`。
pub fn parse_port_values(values: &[String]) -> (Vec<u32>, Vec<String>) {
    let mut ports = Vec::new();
    let mut ranges = Vec::new();
    for raw in values {
        let v = raw.trim();
        if !valid_port_token(v) {
            continue;
        }
        if v.contains('-') {
            let parts: Vec<&str> = v.split('-').collect();
            let a: u32 = parts[0].parse().unwrap_or(0);
            let b: u32 = parts[1].parse().unwrap_or(0);
            ranges.push(format!("{a}:{b}"));
        } else {
            ports.push(v.parse::<u32>().unwrap_or(0));
        }
    }
    (ports, ranges)
}

/// 端口/范围校验便捷封装（condMatcherFields 用）。
pub fn is_valid_port_value(v: &str) -> bool {
    valid_port_token(v.trim())
}

/// 提取规则所有条件中的 ipCidr 值（扁平化、trim、去空）。上游 `ruleIpCidrs`。
pub fn rule_ip_cidrs(rule: &Rule) -> Vec<String> {
    rule_conditions(rule)
        .into_iter()
        .filter(|c| c.type_field == RuleType::IpCidr)
        .flat_map(|c| c.values)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::rule::RuleType;

    fn rule_with_conditions(conditions: Vec<RuleCondition>) -> Rule {
        Rule {
            id: "r1".into(),
            type_field: RuleType::Domain,
            values: vec!["first.com".into()],
            conditions: Some(conditions),
            combine_mode: None,
            action: crate::user_config::rule::RuleAction::Proxy,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        }
    }

    #[test]
    fn rule_conditions_uses_conditions_when_present() {
        let rule = rule_with_conditions(vec![
            RuleCondition {
                type_field: RuleType::DomainSuffix,
                values: vec![".com".into()],
            },
            RuleCondition {
                type_field: RuleType::IpCidr,
                values: vec!["1.2.3.0/24".into()],
            },
        ]);
        let conds = rule_conditions(&rule);
        assert_eq!(conds.len(), 2);
        assert_eq!(conds[0].type_field, RuleType::DomainSuffix);
    }

    #[test]
    fn rule_conditions_falls_back_to_mirror() {
        let rule = Rule {
            id: "r1".into(),
            type_field: RuleType::Domain,
            values: vec!["single.com".into()],
            conditions: None,
            combine_mode: None,
            action: crate::user_config::rule::RuleAction::Direct,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        };
        let conds = rule_conditions(&rule);
        assert_eq!(conds.len(), 1);
        assert_eq!(conds[0].values, vec!["single.com".to_string()]);
    }

    #[test]
    fn rule_conditions_empty_conditions_falls_back() {
        let rule = Rule {
            id: "r1".into(),
            type_field: RuleType::Domain,
            values: vec!["fallback.com".into()],
            conditions: Some(vec![]),
            combine_mode: None,
            action: crate::user_config::rule::RuleAction::Direct,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        };
        let conds = rule_conditions(&rule);
        assert_eq!(conds.len(), 1);
        assert_eq!(conds[0].values, vec!["fallback.com".to_string()]);
    }

    #[test]
    fn parse_port_single() {
        let (ports, ranges) = parse_port_values(&["443".into(), "80".into()]);
        assert_eq!(ports, vec![443, 80]);
        assert!(ranges.is_empty());
    }

    #[test]
    fn parse_port_range() {
        let (ports, ranges) = parse_port_values(&["1000-2000".into()]);
        assert!(ports.is_empty());
        assert_eq!(ranges, vec!["1000:2000".to_string()]);
    }

    #[test]
    fn parse_port_mixed() {
        let (ports, ranges) = parse_port_values(&["443".into(), "8000-9000".into()]);
        assert_eq!(ports, vec![443]);
        assert_eq!(ranges, vec!["8000:9000".to_string()]);
    }

    #[test]
    fn parse_port_invalid_filtered() {
        let (ports, ranges) = parse_port_values(&[
            "0".into(),         // < 1
            "99999".into(),     // > 65535
            "abc".into(),       // 非数字
            "2000-1000".into(), // 范围倒序
            "443".into(),       // 合法
        ]);
        assert_eq!(ports, vec![443]);
        assert!(ranges.is_empty());
    }

    #[test]
    fn port_token_valid() {
        assert!(is_valid_port_value("443"));
        assert!(is_valid_port_value("1-65535"));
        assert!(!is_valid_port_value("0"));
        assert!(!is_valid_port_value("70000"));
        assert!(!is_valid_port_value("abc"));
        assert!(!is_valid_port_value("100-50")); // 倒序
    }

    #[test]
    fn rule_ip_cidrs_extracts_only_ipcidr() {
        let rule = rule_with_conditions(vec![
            RuleCondition {
                type_field: RuleType::DomainSuffix,
                values: vec![".com".into()],
            },
            RuleCondition {
                type_field: RuleType::IpCidr,
                values: vec!["1.2.3.0/24".into(), " 10.0.0.0/8 ".into(), "".into()],
            },
        ]);
        let cidrs = rule_ip_cidrs(&rule);
        assert_eq!(
            cidrs,
            vec!["1.2.3.0/24".to_string(), "10.0.0.0/8".to_string()]
        );
    }
}
