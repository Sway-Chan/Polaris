//! sing-box route 类型（`singbox-config-types.ts:297-359`）。
//! route rules / rule_set / RouteConfig。logical 规则（多条件 OR/AND）递归。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

use super::dns::OneOrMany;

/// `route`（`singbox-config-types.ts:353 SingBoxRouteConfig`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_set: Option<Vec<RuleSet>>,
    pub rules: Vec<RouteRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_domain_resolver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_detect_interface: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "final")]
    pub final_outbound: Option<String>,
}

/// `route.rule_set[]`（`singbox-config-types.ts:342`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleSet {
    pub tag: String,
    #[serde(rename = "type")]
    pub type_field: String,
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_detour: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_interval: Option<String>,
}

/// `route.rules[]`（`singbox-config-types.ts:297`）。logical 规则递归（rules 字段）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_set: Option<OneOrMany<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_suffix: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_keyword: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_regex: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geosite: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_cidr: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ip_cidr: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<OneOrMany<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port_range: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_port: Option<OneOrMany<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_port_range: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_mac_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_hostname: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_name: Option<OneOrMany<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_path: Option<OneOrMany<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_name_not: Option<OneOrMany<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inbound: Option<OneOrMany<String>>,
    /// logical 子规则为纯 matcher 无 action；default/logical 外层显式设 'route'。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outbound: Option<String>,
    /// `action:"reject"` 的抗泛洪开关（sing-box `no_drop`）。`Some(true)` = 关掉降级。
    ///
    /// **为什么阻断规则必须显式置 true**：sing-box 的 reject 默认 `no_drop=false`，30s 滑窗内
    /// 超过 50 次拒绝就把 `method` 临时降级成 `drop`（静默丢包），应用于是从「立刻被拒」变成
    /// 「挂到超时」。上游那道保护是**服务端语义**（防不可信对端泛洪），而本地代理客户端的
    /// 「泛洪」就是用户自己的浏览器在请求广告/遥测 —— 一个页面轻易越过 50 次/30s，越界后页面
    /// 会等在被阻断的请求上。故用户可见的阻断动作一律 `no_drop:true`，与 legacy `block` 出站
    /// （返 `EPERM`，无降级）逐字等价。
    ///
    /// 实证：`no_drop` 在 1.14.0-beta.3 与 1.14.0-alpha.45 都被绑到 `RejectActionOptions.NoDrop`
    /// —— 判据是 `sing-box format` 往返后该字段**存活**，而故意拼错的 `no_dropp` 被丢掉
    /// （`sing-box check` 对未知键静默放行，rc=0 证明不了字段被识别）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_drop: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_by: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sniffer: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewrite_target: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_resolver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub override_address: Option<String>,
    /// TLS spoof（1.14 route action rule）：per-rule 抗审查。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_spoof: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_spoof_method: Option<String>,
    /// logical 规则（多条件跨维度 OR/AND）：type:'logical' + mode + rules。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub type_field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<RouteRule>>,
}
