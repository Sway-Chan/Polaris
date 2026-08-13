//! sing-box DNS 配置类型（`singbox-config-types.ts:13-84`）。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// `dns`（`singbox-config-types.ts:70 SingBoxDnsConfig`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DnsConfig {
    pub servers: Vec<DnsServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<DnsRule>>,
    /// sing-box JSON 字段名是 `final`（Rust 关键字）→ 字段名 final_server + serde rename。
    #[serde(skip_serializing_if = "Option::is_none", rename = "final")]
    pub final_server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fakeip: Option<FakeIpConfig>,
    /// 关 FakeIP 时注入：用 DNS 解析结果反查域名补路由匹配。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverse_mapping: Option<bool>,
    /// sing-box 1.14 顶层 dns.optimistic（仅 true 时下发）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optimistic: Option<bool>,
    /// sing-box 1.14 dns.timeout（Go duration 字符串，如 "5s"；仅 >0 时下发）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

/// `dns.servers[]`（`singbox-config-types.ts:13 SingBoxDnsServer`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DnsServer {
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub type_field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_port: Option<u16>,
    /// DoH path, e.g. "/dns-query"。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Bootstrap resolver tag（sing-box 1.12+，server 为域名时必填）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_resolver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detour: Option<String>,
    /// Tailscale DNS server（1.14；须引用 tailscale endpoint tag）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_search_domain: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_default_resolvers: Option<bool>,
    /// neighbor_domain（1.14；LAN 网关，每条须以 '.' 开头，仅 Linux/macOS）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neighbor_domain: Option<Vec<String>>,
    // Legacy / compat fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address_resolver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inet4_range: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inet6_range: Option<String>,
}

/// `dns.rules[]`（`singbox-config-types.ts:41 SingBoxDnsRule`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DnsRule {
    /// `rule_set`：sing-box DNS rule 是 `string | string[]`（OneOrMany）。
    /// Polaris dns-builder 在 region-local geo（单/多 tag）与外化 dns rule_set（数组）两处 emit。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_set: Option<OneOrMany<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_type: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_suffix: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_keyword: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_mac_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_hostname: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_by: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub type_field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// inbound 键控 DNS 规则（1.13+；string 或 string[]）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inbound: Option<OneOrMany<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_cache: Option<bool>,
    /// 改写应答 TTL（秒）。sing-box `DNSRuleAction.rewrite_ttl`，与 `action` 同层扁平。
    ///
    /// 存在的唯一理由是压 FakeIP 的错配窗口，理由全文见 `builder::dns` 的 `FAKEIP_REWRITE_TTL`。
    /// 本仓实测该字段被固定内核接受（乱写字段名会被 `unknown field` 拒 ⇒ 这道 check 有牙）；
    /// **但「接受」不等于「运行期真的改写了合成应答的 TTL」**，后者属真机项，未验。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewrite_ttl: Option<u32>,
}

/// `dns.fakeip`（`singbox-config-types.ts:64`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FakeIpConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inet4_range: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inet6_range: Option<String>,
}

/// sing-box 多处字段是 `string | string[]`（Listable）。序列化时单值出裸 string、多值出数组。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

/// **dial 侧** `domain_resolver`（`outbounds[]` / `endpoints[]`）：`"<tag>"` 或 `{server, strategy}`。
/// 对齐 上游 `SingBoxDomainResolver`（`a942c60`，#335）。
///
/// # 为什么需要「结构化」这一形态（#335 根因）
///
/// 顶层 `dns.strategy` 在 `enableIPv6=false` 时是 `ipv4_only`（`builder/dns.rs` §B，为 #57 抑制
/// **目标站点** AAAA 而设）。但内核解析**节点自身域名**走的也是同一条 dial 路径 ⇒ AAAA-only 的节点
/// 域名永远解析不出地址，整个代理不可用。本类型让 dial 侧**逐 outbound / endpoint** 用
/// `prefer_ipv4` 覆盖顶层策略：目标站点仍 v4 only（#57 收益原样保留），节点域名则 v4 优先但可回落 v6。
///
/// # 为什么必须逐载体下发，改 `route.default_domain_resolver` 一处不够（loopback 实测定论）
///
/// per-outbound 只要是**纯 tag 字符串**，就整个覆盖掉 `route.default_domain_resolver`，**不继承**
/// 它的 strategy。三组对照（随包 sing-box 1.14.0-beta.5，零包出网）：
/// - default 带 strategy + per-outbound 纯字符串 → `dns: lookup failed for probe.test: empty result`
/// - default 带 strategy + 完全不写 per-outbound → `lookup succeed`
/// - per-outbound 写 `{"server":"h","strategy":"prefer_ipv4"}` → `lookup succeed for probe.test: ::1`
///
/// # 为什么 `server` 恒填
///
/// `{strategy}` 无 `server` 会被**解析期硬拒**（`empty domain_resolver.server`）；`{server, strategy}`
/// 在 outbound / endpoint / `route.default_domain_resolver` 三处都被 `sing-box check` 接受。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DomainResolver {
    /// 纯 tag：继承顶层 `dns.strategy`。
    /// `enableIPv6=true` 分支恒走这支——那时顶层已是 `prefer_ipv4`，无需覆盖，且**保证该分支
    /// config 字节零变化**（金样 delta 只落在 IPv6 关闭的场景上）。
    Tag(String),
    /// 结构化：显式覆盖顶层策略。
    Detailed {
        server: String,
        strategy: DomainStrategy,
    },
}

/// sing-box domain strategy 闭集。
///
/// 刻意用**枚举**而非裸 `String`：`sing-box check` 对 `domain_resolver` 对象内的键名/值**无牙**
/// （上游 实测：写错的 typo 仍 `exit=0`），所以值笔误的唯一拦截点是编译期。四个变体是 sing-box
/// 的完整闭集，用户自定义 outbound 的 raw-JSON 透传（`build_proxy_outbound` 的 Custom 分支）
/// 需要能反序列化任一值，故不裁剪成「只留用得上的那个」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainStrategy {
    PreferIpv4,
    PreferIpv6,
    Ipv4Only,
    Ipv6Only,
}
