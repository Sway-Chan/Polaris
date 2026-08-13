//! 路由规则值/聚合校验（上游 `shared/rules.ts` 的 `validateRuleValue` / `validateRule` /
//! `isValidIpCidr` 权威 Rust 实现）——**单一真值**。
//!
//! 历史上本逻辑有两份副本：`user_config/rules.rs`（仅端口 token）与 `store/validate.rs`
//! （完整逐类型校验 + 严格 IP/CIDR，且各自私有一份 `valid_port_token`）。D4 收敛为一份：
//! 完整实现落 config-engine（最底层、持有 `Rule`/`RuleType` 类型），`store/validate.rs`
//! 改为 re-export 本模块（config 落盘 sanitize 复用同一真值），端口 token 复用
//! `rules::is_valid_port_value`（消除第二份 `valid_port_token`）。
//!
//! D4 提交门：rule-dialog 提交经 Tauri `rules_add`/`rules_update` 调 [`validate_rule`]，
//! 权威在 Rust；前端只留 `isValidIpCidr` 做输入内联提示。
//!
//! 语义与 sing-box `netip.ParsePrefix` 对齐（严格 IPv4/IPv6：拒前导零、掩码越界、非法结构），
//! 放过这些值会让内核启动 FATAL。

#![forbid(unsafe_code)]

use crate::user_config::rule::Rule;
use crate::user_config::rules::{is_valid_port_value, rule_conditions};
use crate::user_config::{is_valid_mac_address, is_valid_source_hostname};

/// 规则类型全集（上游 `RULE_TYPE_IDS`，camelCase）。与 [`crate::user_config::rule::RuleType::as_id`]
/// 严格同源（见本模块单测 `rule_type_ids_in_sync`），供 sanitize 阶段按字符串判定已知类型。
pub const RULE_TYPE_IDS: &[&str] = &[
    "domain",
    "domainSuffix",
    "domainKeyword",
    "domainRegex",
    "ipCidr",
    "sourceIpCidr",
    "port",
    "sourcePort",
    "sourceMac",
    "sourceHostname",
    "processName",
    "processPath",
    "geosite",
    "geoip",
    "ruleSet",
];

/// 规则类型是否已知（camelCase 比对）。
#[must_use]
pub fn is_known_rule_type(t: &str) -> bool {
    RULE_TYPE_IDS.contains(&t)
}

/// 严格 IPv4（恰 4 段、每段 0-255、禁前导零）。上游 `isStrictIpv4`。
fn is_strict_ipv4(addr: &str) -> bool {
    let octets: Vec<&str> = addr.split('.').collect();
    octets.len() == 4
        && octets.iter().all(|&o| {
            // (0|[1-9]\d{0,2}) 且 ≤255（禁前导零，对齐 sing-box netip）
            o.parse::<u32>().is_ok_and(|n| n <= 255)
                && (o == "0" || !o.starts_with('0'))
                && o.bytes().all(|b| b.is_ascii_digit())
        })
}

/// 结构化 IPv6 校验（`::` 至多一次、每段 1-4 hex、末段可内嵌 IPv4）。
/// 上游 `isStrictIpv6`，匹配 sing-box netip.ParsePrefix。
fn is_strict_ipv6(addr: &str) -> bool {
    let parts: Vec<&str> = addr.split("::").collect();
    if parts.len() > 2 {
        return false;
    }
    let has_compression = parts.len() == 2;
    let left: Vec<&str> = if parts[0].is_empty() {
        vec![]
    } else {
        parts[0].split(':').collect()
    };
    let right: Vec<&str> = if has_compression {
        if parts[1].is_empty() {
            vec![]
        } else {
            parts[1].split(':').collect()
        }
    } else {
        vec![]
    };
    let mut groups: Vec<&str> = left;
    groups.extend(right);
    let mut ipv4_suffix = false;
    for (i, g) in groups.iter().enumerate() {
        if i == groups.len() - 1 && g.contains('.') {
            if !is_strict_ipv4(g) {
                return false;
            }
            ipv4_suffix = true;
        } else if !(!g.is_empty() && g.len() <= 4 && g.bytes().all(|b| b.is_ascii_hexdigit())) {
            return false;
        }
    }
    let count = groups.len() + if ipv4_suffix { 1 } else { 0 };
    // 无 :: 须恰 8 段；有 :: 时代表 ≥1 零段，显式段须 ≤7。
    if has_compression {
        count <= 7
    } else {
        count == 8
    }
}

/// IPv4/IPv6（可选 CIDR 掩码）严格校验。上游 `isValidIpCidr`。
/// 含范围+结构检查：八位组 ≤255、禁前导零、掩码 v4≤32 / v6≤128、IPv6 结构合法。
#[must_use]
pub fn is_valid_ip_cidr(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    let (addr, mask) = match v.split_once('/') {
        Some((a, m)) => (a, Some(m)),
        None => (v, None),
    };
    let is_v6 = addr.contains(':');
    if let Some(m) = mask {
        if !(m.len() <= 3 && m.bytes().all(|b| b.is_ascii_digit())) {
            return false;
        }
        let prefix: u32 = m.parse().unwrap_or(999);
        let max = if is_v6 { 128 } else { 32 };
        if prefix > max {
            return false;
        }
    }
    if is_v6 {
        is_strict_ipv6(addr)
    } else {
        is_strict_ipv4(addr)
    }
}

/// 单条规则值是否合法（按类型）。上游 `validateRuleValue`（空串一律非法）。
/// 仅做形状校验（sanitize 阶段已过滤非字符串）；生成期再校验。
#[must_use]
pub fn validate_rule_value(rule_type: &str, value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    match rule_type {
        "domain" | "domainSuffix" => is_domain_shape(v),
        // 关键词是对**域名**做子串匹配，而 DNS 名里不可能出现 `:` ⇒ 含冒号的关键词恒不命中。
        // 用户把 IPv6 字面量（`2001:db8::1`、URL 写法 `[2001:db8::1]`）填进关键词条件时，落成的
        // `domain_keyword` 是一条永不命中的死配置，且内核不报错、用户以为配好了 —— 故在提交门硬拒。
        // 判据取「含冒号」而非「是 IPv6 字面量」：前者可证（DNS 名不含冒号），且顺带盖住 `foo:bar`
        // 这类同样恒不命中的值；`domain`/`domainSuffix` 经 `is_domain_shape` 本就拒冒号，此处补齐同族最后一个缺口。
        // **不推广到 IPv4**：`1.2.3.4` 作关键词能命中 `1.2.3.4.nip.io` / `4.3.2.1.in-addr.arpa` 这类
        // 真实域名，拒掉会砍掉合法能力（详见本模块单测 `domain_keyword_*`）。
        "domainKeyword" => !v.contains(':'),
        "domainRegex" => {
            // RE2 拒 lookahead/lookbehind/反向引用；Rust 无 new RegExp，仅做 RE2 禁项粗检。
            // 反向引用 \1..\9 全拒（TS `\\[1-9]`），非仅 \1。
            if v.contains("(?=") || v.contains("(?!") || v.contains("(?<") {
                return false;
            }
            !v.as_bytes()
                .windows(2)
                .any(|w| w[0] == b'\\' && w[1].is_ascii_digit() && w[1] != b'0')
        }
        "ipCidr" | "sourceIpCidr" => is_valid_ip_cidr(v),
        "port" | "sourcePort" => is_valid_port_value(v),
        "sourceMac" => is_valid_mac_address(Some(v)),
        "sourceHostname" => is_valid_source_hostname(Some(v)),
        "processName" => !v.contains('/') && !v.contains('\\'),
        "processPath" => v.starts_with('/') || windows_path(v),
        "geosite" | "geoip" => {
            // 小写字母数字 + ! - _（大小写不敏感，对齐 TS GEO_TAG_RE 的 /i）
            v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'!' || b == b'-' || b == b'_')
        }
        "ruleSet" => {
            if let Some(rest) = v.strip_prefix("res:") {
                !rest.is_empty()
            } else {
                v.starts_with("http://") || v.starts_with("https://")
            }
        }
        _ => false,
    }
}

/// Windows 路径前缀（`C:\` / `D:/`）。上游 `/^[A-Za-z]:[\\/]/`。
fn windows_path(v: &str) -> bool {
    let b = v.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// 域名形状（Polaris DOMAIN_RE 等价手写：标签 1-63、字母数字/连字符/下划线、可 *. 前缀）。
fn is_domain_shape(v: &str) -> bool {
    let s = v.strip_prefix("*.").unwrap_or(v);
    if s.is_empty() {
        return false;
    }
    // 点分标签，每标签 1-63、字母数字开头结尾、中间可 -_
    s.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
            && label
                .bytes()
                .last()
                .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// 一条规则的聚合校验结果（提交门 DTO）。`valid == errors.is_empty()`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleValidation {
    pub valid: bool,
    pub errors: Vec<String>,
}

/// 聚合校验一条规则（上游 `validateRule`）：遍历全部条件（`rule_conditions` 归一——
/// 多条件用 `conditions`，否则退化为镜像单条件），每个条件须有 ≥1 个非空值且全部值合法。
///
/// 类型系统已强制 `RuleType`/`CombineMode` 枚举合法（TS 需运行时校验镜像 `type`/`combineMode`，
/// Rust 由类型保证，故此处不再重复），本函数只判各条件的值。
#[must_use]
pub fn validate_rule(rule: &Rule) -> RuleValidation {
    let mut errors = Vec::new();
    for cond in rule_conditions(rule) {
        let type_id = cond.type_field.as_id();
        let non_empty: Vec<&String> = cond
            .values
            .iter()
            .filter(|v| !v.trim().is_empty())
            .collect();
        if non_empty.is_empty() {
            errors.push(format!("{type_id}: 缺少有效值"));
            continue;
        }
        for v in non_empty {
            if !validate_rule_value(type_id, v) {
                errors.push(format!("{type_id}: 非法值 \"{}\"", v.trim()));
            }
        }
    }
    RuleValidation {
        valid: errors.is_empty(),
        errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::rule::{Rule, RuleAction, RuleCondition, RuleType};

    #[test]
    fn strict_ipv4_rejects_leading_zeros() {
        assert!(is_valid_ip_cidr("10.0.0.0/8"));
        assert!(is_valid_ip_cidr("192.168.1.1"));
        assert!(!is_valid_ip_cidr("010.0.0.1")); // 前导零
        assert!(!is_valid_ip_cidr("10.0.0.0/40")); // 掩码超界
        assert!(!is_valid_ip_cidr("256.1.1.1")); // 八位组超界
    }

    #[test]
    fn strict_ipv6_structure() {
        assert!(is_valid_ip_cidr("fc00::/7"));
        assert!(is_valid_ip_cidr("2001:db8::1"));
        assert!(is_valid_ip_cidr("::ffff:192.168.1.1")); // 末段内嵌 IPv4
        assert!(!is_valid_ip_cidr("12345::1")); // 段>4位
        assert!(!is_valid_ip_cidr("dead::beef::1")); // 多个 ::
        assert!(!is_valid_ip_cidr("2001:db8::1/129")); // v6 掩码超界
    }

    #[test]
    fn rule_type_ids_in_sync() {
        // RULE_TYPE_IDS 与 RuleType::as_id 严格同源（数量 + 逐项）。
        let variants = [
            RuleType::Domain,
            RuleType::DomainSuffix,
            RuleType::DomainKeyword,
            RuleType::DomainRegex,
            RuleType::IpCidr,
            RuleType::SourceIpCidr,
            RuleType::Port,
            RuleType::SourcePort,
            RuleType::SourceMac,
            RuleType::SourceHostname,
            RuleType::ProcessName,
            RuleType::ProcessPath,
            RuleType::Geosite,
            RuleType::Geoip,
            RuleType::RuleSet,
        ];
        assert_eq!(variants.len(), RULE_TYPE_IDS.len());
        for (v, id) in variants.iter().zip(RULE_TYPE_IDS) {
            assert_eq!(v.as_id(), *id);
            assert!(is_known_rule_type(id));
        }
        assert!(!is_known_rule_type("bogusType"));
    }

    /// 15 类规则值：每类至少一个 accept + 一个 reject（含空串一律非法）。
    #[test]
    fn validate_rule_value_accept_reject_per_type() {
        let cases: &[(&str, &str, &str)] = &[
            // (type, accept, reject)
            ("domain", "example.com", "bad!domain"),
            ("domainSuffix", "sub.example.com", "exa mple.com"),
            ("domainKeyword", "anything-goes", "   "),
            ("domainRegex", "^ab.*$", "(?=lookahead)"),
            ("ipCidr", "10.0.0.0/8", "010.0.0.1"),
            ("sourceIpCidr", "192.168.1.0/24", "256.1.1.1"),
            ("port", "443", "70000"),
            ("sourcePort", "1-1024", "0"),
            ("sourceMac", "aa:bb:cc:dd:ee:ff", "zz:bb:cc:dd:ee:ff"),
            ("sourceHostname", "my-host", "-bad"),
            ("processName", "chrome.exe", "bad/name"),
            ("processPath", "/usr/bin/chrome", "relative/path"),
            ("geosite", "geolocation-!cn", "bad tag"),
            ("geoip", "us", "bad@tag"),
            ("ruleSet", "res:my-set", "ftp://x/y"),
        ];
        for (ty, ok, bad) in cases {
            assert!(validate_rule_value(ty, ok), "{ty} should accept {ok:?}");
            assert!(!validate_rule_value(ty, bad), "{ty} should reject {bad:?}");
            assert!(!validate_rule_value(ty, ""), "{ty} must reject empty");
        }
        // 未知类型一律非法
        assert!(!validate_rule_value("bogus", "x"));
        // domainRegex 反向引用 \1..\9 全拒（非仅 \1）
        assert!(!validate_rule_value("domainRegex", r"(a)\2"));
        // processPath Windows 路径
        assert!(validate_rule_value(
            "processPath",
            r"C:\Program Files\app.exe"
        ));
        // ruleSet http(s) URL
        assert!(validate_rule_value("ruleSet", "https://example.com/x.srs"));
    }

    fn rule(
        type_field: RuleType,
        values: Vec<&str>,
        conditions: Option<Vec<RuleCondition>>,
    ) -> Rule {
        Rule {
            id: "r1".into(),
            type_field,
            values: values.into_iter().map(String::from).collect(),
            conditions,
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
    fn validate_rule_accepts_valid_single_condition() {
        let r = rule(RuleType::Domain, vec!["example.com"], None);
        let res = validate_rule(&r);
        assert!(res.valid, "errors: {:?}", res.errors);
        assert!(res.errors.is_empty());
    }

    #[test]
    fn validate_rule_accepts_valid_multi_condition() {
        let r = rule(
            RuleType::Domain,
            vec!["mirror.com"],
            Some(vec![
                RuleCondition {
                    type_field: RuleType::DomainSuffix,
                    values: vec!["example.com".into(), "cdn.example.org".into()],
                },
                RuleCondition {
                    type_field: RuleType::IpCidr,
                    values: vec!["1.2.3.0/24".into()],
                },
            ]),
        );
        let res = validate_rule(&r);
        assert!(res.valid, "errors: {:?}", res.errors);
    }

    #[test]
    fn validate_rule_rejects_bad_value() {
        let r = rule(RuleType::IpCidr, vec!["010.0.0.1"], None);
        let res = validate_rule(&r);
        assert!(!res.valid);
        assert_eq!(res.errors.len(), 1);
        assert!(res.errors[0].contains("ipCidr"));
    }

    #[test]
    fn validate_rule_rejects_empty_values() {
        let r = rule(RuleType::Domain, vec!["", "   "], None);
        let res = validate_rule(&r);
        assert!(!res.valid);
        assert!(res.errors[0].contains("缺少有效值"));
    }

    #[test]
    fn validate_rule_rejects_when_one_condition_bad() {
        let r = rule(
            RuleType::Domain,
            vec!["ok.com"],
            Some(vec![
                RuleCondition {
                    type_field: RuleType::Domain,
                    values: vec!["ok.com".into()],
                },
                RuleCondition {
                    type_field: RuleType::Port,
                    values: vec!["99999".into()], // 越界
                },
            ]),
        );
        let res = validate_rule(&r);
        assert!(!res.valid);
        assert!(res.errors.iter().any(|e| e.contains("port")));
    }

    /// `domain_keyword` 是对**域名**做子串匹配，DNS 名里不可能出现 `:` ⇒ 含冒号的关键词恒不命中，
    /// 且内核不报错 —— 用户填 IPv6 字面量进去得到的是一条静默失效的死规则。前端同源门在
    /// `ui/src/domain/rules.test.ts` 的 `domainKeyword 拒含冒号值`，两侧必须同时改。
    #[test]
    fn domain_keyword_rejects_ipv6_literals() {
        for v in [
            "2001:db8::1",
            "[2001:db8::1]", // URL 写法，方括号形式同样含冒号
            "::1",
            "fe80::1%eth0",
            "::ffff:192.168.1.1",
            "2606:4700::1",
        ] {
            assert!(
                !validate_rule_value("domainKeyword", v),
                "{v} 应被拒：含冒号的关键词永不命中"
            );
        }
        // 判据是「含冒号」而非「像 IPv6」——`foo:bar` 同样永不命中。
        assert!(!validate_rule_value("domainKeyword", "foo:bar"));
        assert!(!validate_rule_value("domainKeyword", "example.com:443"));
    }

    /// 反向对照：闸门不得收得过宽。这条挂了说明把正常关键词误判成了 IP，砍掉了合法能力。
    #[test]
    fn domain_keyword_accepts_normal_keywords_including_ipv4() {
        for v in [
            "ads",
            "googlevideo",
            "example.com",
            "cdn-",
            "1.2.3.4", // `1.2.3.4.nip.io`、`4.3.2.1.in-addr.arpa` 都是真实可命中的域名
            "10.0.0.1",
            "v6", // 名字里带 v6 不等于 IPv6
        ] {
            assert!(
                validate_rule_value("domainKeyword", v),
                "{v} 应被接受：它是合法关键词"
            );
        }
        // 原有语义不变：空 / 纯空白仍然拒。
        assert!(!validate_rule_value("domainKeyword", ""));
        assert!(!validate_rule_value("domainKeyword", "   "));
    }

    /// 同族一致性：域名族三个字面量类型对 IPv6 口径统一（此前只有 keyword 漏）。
    #[test]
    fn domain_family_rejects_ipv6_uniformly() {
        for t in ["domain", "domainSuffix", "domainKeyword"] {
            assert!(!validate_rule_value(t, "2001:db8::1"), "{t} 应拒 IPv6");
            assert!(
                !validate_rule_value(t, "[2001:db8::1]"),
                "{t} 应拒方括号 IPv6"
            );
        }
    }

    /// 端到端：一条「关键词 = IPv6 字面量」的规则过不了提交门（`rules_add`/`rules_update` 走这里）。
    #[test]
    fn validate_rule_rejects_ipv6_keyword_rule() {
        let r = rule(RuleType::DomainKeyword, vec!["2001:db8::1"], None);
        let res = validate_rule(&r);
        assert!(
            !res.valid,
            "IPv6 关键词规则必须被提交门拒掉，而不是静默存成死配置"
        );
        assert!(res.errors.iter().any(|e| e.contains("domainKeyword")));
    }
}
