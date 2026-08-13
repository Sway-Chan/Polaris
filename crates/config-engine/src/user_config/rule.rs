//! 路由规则类型（上游 `shared/types/rules.ts` Rule/RuleCondition/RuleAction/RuleType 1:1 移植）。
//!
//! buildCustomRules + buildRouteConfig 的主输入类型。对齐 sing-box route rule 常用全集。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// 规则动作：proxy=走代理 / direct=直连 / block=拒绝。上游 `RuleAction`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    #[default]
    Proxy,
    Direct,
    Block,
}

/// 自定义规则条件类型（sing-box route rule 常用全集，去冗余）。上游 `RuleType`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleType {
    #[default]
    Domain,
    DomainSuffix,
    DomainKeyword,
    DomainRegex,
    IpCidr,
    SourceIpCidr,
    Port,
    SourcePort,
    /// sing-box 1.14 源设备识别（按 MAC 分流；仅 Linux/macOS）。
    SourceMac,
    SourceHostname,
    ProcessName,
    ProcessPath,
    Geosite,
    Geoip,
    RuleSet,
}

impl RuleType {
    /// 稳定 camelCase id（上游 `RULE_TYPE_IDS` 的元素；与本枚举 `serde rename_all = "camelCase"` 一致）。
    /// 供值校验（按类型字符串分派）与 `rule_validate::RULE_TYPE_IDS` 复用，杜绝手抄字符串漂移。
    #[must_use]
    pub fn as_id(self) -> &'static str {
        match self {
            RuleType::Domain => "domain",
            RuleType::DomainSuffix => "domainSuffix",
            RuleType::DomainKeyword => "domainKeyword",
            RuleType::DomainRegex => "domainRegex",
            RuleType::IpCidr => "ipCidr",
            RuleType::SourceIpCidr => "sourceIpCidr",
            RuleType::Port => "port",
            RuleType::SourcePort => "sourcePort",
            RuleType::SourceMac => "sourceMac",
            RuleType::SourceHostname => "sourceHostname",
            RuleType::ProcessName => "processName",
            RuleType::ProcessPath => "processPath",
            RuleType::Geosite => "geosite",
            RuleType::Geoip => "geoip",
            RuleType::RuleSet => "ruleSet",
        }
    }
}

/// 单个匹配条件（type + values）。上游 `RuleCondition`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleCondition {
    #[serde(rename = "type")]
    pub type_field: RuleType,
    pub values: Vec<String>,
}

/// 多条件组合：or(默认，命中任一) / and(全部命中)。上游 `Rule.combineMode`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CombineMode {
    And,
    #[default]
    Or,
}

/// 自定义路由规则。上游 `Rule`。
///
/// `type`/`values` = 首条件镜像（向后兼容）；`conditions` = 多条件（≥2 时存在）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    /// 首条件镜像（恒与 conditions[0] 一致）。
    #[serde(rename = "type")]
    pub type_field: RuleType,
    pub values: Vec<String>,
    /// 多条件共存（≥2 条件时存在）。空/缺省 = 单条件规则。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<RuleCondition>>,
    #[serde(rename = "combineMode", skip_serializing_if = "Option::is_none")]
    pub combine_mode: Option<CombineMode>,
    pub action: RuleAction,
    pub enabled: bool,
    /// 绕过 FakeIP（仅 domain/domainSuffix/domainKeyword 有效）。
    #[serde(rename = "bypassFakeIP", skip_serializing_if = "Option::is_none")]
    pub bypass_fakeip: Option<bool>,
    /// 目标代理服务器 ID（仅 action=proxy 时有效）。
    #[serde(rename = "targetServerId", skip_serializing_if = "Option::is_none")]
    pub target_server_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remarks: Option<String>,
    /// TLS spoof（P3a 抗审查，sing-box 1.14）：伪造 ClientHello SNI。
    #[serde(rename = "tlsSpoof", skip_serializing_if = "Option::is_none")]
    pub tls_spoof: Option<String>,
    #[serde(rename = "tlsSpoofMethod", skip_serializing_if = "Option::is_none")]
    pub tls_spoof_method: Option<String>,
}

/// 已下载规则资源（.srs）。上游 `RuleResource`。
///
/// **逐字段 rename 不可省**（本结构无 `rename_all`）：config.json 侧是 camelCase
/// （`sourceUrl`/`fileName`/`downloadedAt`，见 `ui/src/shared/types/rules.ts:105`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleResource {
    pub id: String,
    pub name: String,
    pub category: String,
    #[serde(rename = "sourceUrl")]
    pub source_url: String,
    #[serde(rename = "fileName")]
    pub file_name: String,
    pub format: RuleResourceFormat,
    pub size: u64,
    #[serde(rename = "downloadedAt")]
    pub downloaded_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleResourceFormat {
    Binary,
    Source,
}

/// 应用分流规则（映射到内置 geosite/process 规则集）。上游 `AppRule`。
///
/// **`appId` 的 rename 不可省**（本结构无 `rename_all`）：`targetServerId` 有、`app_id` 曾漏 →
/// 整条 appRules 反序列化不出来。见 `app_config.rs` 的 `app_rules` 注释。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppRule {
    #[serde(rename = "appId")]
    pub app_id: String,
    pub action: RuleAction,
    pub enabled: bool,
    #[serde(rename = "targetServerId", skip_serializing_if = "Option::is_none")]
    pub target_server_id: Option<String>,
}

/// 自定义应用分流预设。上游 `CustomAppPreset`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomAppPreset {
    pub id: String,
    pub name: String,
    pub emoji: String,
    #[serde(rename = "iconUrl", skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    #[serde(default, rename = "geositeTags")]
    pub geosite_tags: Vec<String>,
    #[serde(default, rename = "geoipTags", skip_serializing_if = "Vec::is_empty")]
    pub geoip_tags: Vec<String>,
    #[serde(rename = "processNames", skip_serializing_if = "Option::is_none")]
    pub process_names: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_deserializes_from_polaris_json() {
        // 验证能反序列化 Polaris config.json 里的 Rule 结构（camelCase 键）。
        let json = r#"{
            "id": "r1",
            "type": "domain",
            "values": ["example.com"],
            "action": "proxy",
            "enabled": true,
            "targetServerId": "s2"
        }"#;
        let rule: Rule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.type_field, RuleType::Domain);
        assert_eq!(rule.action, RuleAction::Proxy);
        assert_eq!(rule.target_server_id.as_deref(), Some("s2"));
        assert!(rule.conditions.is_none()); // 单条件无 conditions
    }

    #[test]
    fn multi_condition_rule_with_and() {
        let json = r#"{
            "id": "r2",
            "type": "domain",
            "values": ["a.com"],
            "conditions": [
                {"type": "domainSuffix", "values": [".com"]},
                {"type": "ipCidr", "values": ["1.2.3.0/24"]}
            ],
            "combineMode": "and",
            "action": "direct",
            "enabled": true
        }"#;
        let rule: Rule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.combine_mode, Some(CombineMode::And));
        assert_eq!(rule.conditions.as_ref().unwrap().len(), 2);
    }
}
