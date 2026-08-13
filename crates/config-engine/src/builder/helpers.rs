//! config-gen 共享纯 helper（上游 `singbox-config-helpers.ts` 1:1 移植）。
//!
//! 全部纯函数：只读 config / host 字符串。各 builder 共用，避免跨模块依赖私有方法。
//! 含 buildIdToTagMap/host helpers/effectiveRules/geoCategories/ruleSetPrune +
//! node/domestic resolver tag（issue #147 race）+ custom domestic DNS endpoint。

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use crate::singbox::{DomainResolver, DomainStrategy, RouteRule, SingBoxConfig};
use crate::user_config::app_rules_preset::get_app_preset;
use crate::user_config::ip::{is_ipv4, is_ipv6_literal};
use crate::user_config::rule::{Rule, RuleType};
use crate::user_config::rules::rule_conditions;

/// 内置出站/inbound 保留 tag：节点显示名撞这些会致 sing-box tag 冲突 FATAL。
/// 上游 `RESERVED_OUTBOUND_TAGS`。
pub const RESERVED_OUTBOUND_TAGS: &[&str] = &[
    "proxy-selector",
    "direct",
    "block",
    "direct-loopback",
    "probe-direct-in",
    "probe-proxy-in",
];

/// 主核测速探测池入站 tag 前缀（§15，`probe-in-{k}`）。
///
/// **单一真值**：生成端（[`inbounds`](crate::builder::inbounds) 建入站、
/// [`route`](crate::builder::route) 钉死路由、[`dns`](crate::builder::dns) 钉死解析）与消费端
/// （`polaris-stats-engine` 的连接表探测流量过滤）全部经 [`probe_pool_inbound_tag`] / 本常量取值。
/// 谁把它抄成字面量，改名那天就会静默失配——生成端改了、消费端还在按老前缀比，过滤变成永假且无人报错。
pub const PROBE_POOL_INBOUND_TAG_PREFIX: &str = "probe-in-";

/// 第 `k` 槽探测池入站 tag（`probe-in-{k}`）。见 [`PROBE_POOL_INBOUND_TAG_PREFIX`]。
pub fn probe_pool_inbound_tag(k: usize) -> String {
    format!("{PROBE_POOL_INBOUND_TAG_PREFIX}{k}")
}

/// 该入站 tag 是否属于主核测速探测池（`probe-in-{k}`）。
///
/// 消费端判据：探测池连接是**应用自己的测速流量**，不是用户流量。
pub fn is_probe_pool_inbound_tag(tag: &str) -> bool {
    tag.starts_with(PROBE_POOL_INBOUND_TAG_PREFIX)
}

/// 节点显示名缺省值（name 为空时）。
const UNNAMED_SERVER: &str = "未命名节点";

/// 节点最小投影（buildIdToTagMap 入参）。
pub trait ServerLike {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
}

/// serverId → 唯一 tag（=节点显示名，撞名/撞保留 tag 追加 (n)）。
/// 上游 `buildIdToTagMap`。
pub fn build_id_to_tag_map<S: ServerLike>(
    servers: &[S],
) -> std::collections::BTreeMap<String, String> {
    let mut id_to_tag = std::collections::BTreeMap::new();
    let mut used_tags: BTreeSet<String> = RESERVED_OUTBOUND_TAGS
        .iter()
        .map(|s| s.to_string())
        .collect();
    for s in servers {
        let base_tag = {
            let trimmed = s.name().trim();
            if trimmed.is_empty() {
                UNNAMED_SERVER.to_string()
            } else {
                trimmed.to_string()
            }
        };
        let mut tag = base_tag.clone();
        let mut count = 1;
        while used_tags.contains(&tag) {
            tag = format!("{base_tag} ({count})");
            count += 1;
        }
        used_tags.insert(tag.clone());
        id_to_tag.insert(s.id().to_string(), tag);
    }
    id_to_tag
}

/// 主机是否 IPv4 字面量。上游 `isIpv4Host` = isIpv4。
pub fn is_ipv4_host(host: &str) -> bool {
    is_ipv4(host)
}

/// 主机是否 IPv6 字面量（去方括号 + ≥2 冒号，含 IPv4-mapped）。上游 `isIpv6Host` = isIpv6Literal。
pub fn is_ipv6_host(host: &str) -> bool {
    is_ipv6_literal(host)
}

/// 去 IPv6 字面量方括号（仅配对时脱）。上游 `stripHostBrackets`。
pub fn strip_host_brackets(host: &str) -> &str {
    let bytes = host.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'[' && bytes[bytes.len() - 1] == b']' {
        &host[1..host.len() - 1]
    } else {
        host
    }
}

/// IP 字面量 host → 单 IP 排除 CIDR（v6→/128，v4→/32）；非 IP/空/畸形 → None。上游 `hostToExcludeCidr`。
pub fn host_to_exclude_cidr(host: &str) -> Option<String> {
    let bare = strip_host_brackets(host);
    if is_ipv6_host(bare) {
        return Some(format!("{bare}/128"));
    }
    if is_ipv4_host(bare) {
        return Some(format!("{bare}/32"));
    }
    None
}

/// 国内银行/证券域名（DNS 银行规则 + route 强制直连共用）。上游 `DOMESTIC_BANK_AND_STOCK_DOMAINS`。
pub const DOMESTIC_BANK_AND_STOCK_DOMAINS: &[&str] = &[
    ".microdone.cn",
    ".icbc.com.cn",
    ".boc.cn",
    ".ccb.com",
    ".abchina.com",
    ".abchina.com.cn",
    ".bankcomm.com",
    ".cmbchina.com",
    ".psbc.com",
    ".spdb.com.cn",
    ".cebbank.com",
    ".citicbank.com",
    ".pingan.com",
    ".cib.com.cn",
    ".hxb.com.cn",
    ".cmbc.com.cn",
    ".hzbank.com.cn",
    ".10jqka.com.cn",
    ".thsi.cn",
    ".eastmoney.com",
    ".1234567.com.cn",
    ".gw.com.cn",
    ".tdx.com.cn",
];

/// 用户路由规则 gate：自定义规则仅 smart 模式生效。上游 `effectiveCustomRules`。
/// 调用方传 proxy_mode + custom_rules，返回 smart 时全量规则，否则空。
pub fn effective_custom_rules(proxy_mode: &str, custom_rules: &[Rule]) -> Vec<Rule> {
    if !proxy_mode.eq_ignore_ascii_case("smart") {
        return vec![];
    }
    custom_rules.to_vec()
}

/// 应用分流 gate：appRoutingEnabled && smart 才生效。上游 `effectiveAppRules`。
pub fn effective_app_rules(
    app_routing_enabled: bool,
    proxy_mode: &str,
    app_rules: &[crate::user_config::rule::AppRule],
) -> Vec<crate::user_config::rule::AppRule> {
    if !app_routing_enabled || !proxy_mode.eq_ignore_ascii_case("smart") {
        return vec![];
    }
    app_rules.to_vec()
}

/// 收集自定义规则中使用的 geosite/geoip 类别。上游 `getRequiredGeoCategories`。
/// appRules 的 preset geositeTags/geoipTags 经 app_preset_lookup 解析。
pub fn get_required_geo_categories(
    custom_rules: &[Rule],
    app_rules: &[crate::user_config::rule::AppRule],
    custom_app_presets: &[crate::user_config::rule::CustomAppPreset],
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut geosite: BTreeSet<String> = BTreeSet::new();
    let mut geoip: BTreeSet<String> = BTreeSet::new();

    // 自定义规则扫描（含多条件 logical 内的 geosite/geoip）。
    for rule in custom_rules {
        if !rule.enabled {
            continue;
        }
        for cond in rule_conditions(rule) {
            match cond.type_field {
                RuleType::Geosite => {
                    for t in &cond.values {
                        let tag = t.trim().to_ascii_lowercase();
                        if !tag.is_empty() {
                            geosite.insert(tag);
                        }
                    }
                }
                RuleType::Geoip => {
                    for t in &cond.values {
                        let tag = t.trim().to_ascii_lowercase();
                        if !tag.is_empty() {
                            geoip.insert(tag);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // 应用分流扫描（preset geositeTags/geoipTags）。
    //
    // 走 `get_app_preset`（内置表 + 自定义，单一真值）。**此前走的是本文件私有的 `app_preset_lookup`**
    // ——它只查 customAppPresets，对 16 条内置预设一律返回 None（`TODO(H1): 内置 APP_PRESETS 表移植后
    // 合并查找`）。而内置表早已移植到位，TODO 的前置条件已满足，只是没人回来接线。
    //
    // 漏报的后果（窄但真实）：内置预设的 geo tag 进不了 required 集 → route.rs:979 的
    // `add_local_geo_rule_set` 不会为它找「规则资源页的本地副本」。随包 .srs 在位时无感（route.rs:937
    // 那条路径已注入）；**随包 .srs 缺失/损坏、而用户恰好在规则资源页下过同名 geo 时**，本该由本地副本
    // 兜底，实际却拿不到 → tag 悬空 → fail-closed 剪枝（route.rs:998）把该应用的域名兜底规则剪掉。
    for app_rule in app_rules {
        if !app_rule.enabled {
            continue;
        }
        if let Some(preset) = get_app_preset(&app_rule.app_id, custom_app_presets) {
            for tag in &preset.geosite_tags {
                let t = tag.trim().to_ascii_lowercase();
                if !t.is_empty() {
                    geosite.insert(t);
                }
            }
            for tag in &preset.geoip_tags {
                let t = tag.trim().to_ascii_lowercase();
                if !t.is_empty() {
                    geoip.insert(t);
                }
            }
        }
    }

    (geosite, geoip)
}

/// fail-closed 剪枝：剪掉引用未定义/不可达 rule_set tag 的路由规则及定义。
/// 上游 `applyRuleSetPrune`。三态递归（string/array/logical）。
/// 返回被丢弃的 rule_set tag。
pub fn apply_rule_set_prune(
    config: &mut SingBoxConfig,
    unreachable: &BTreeSet<String>,
) -> Vec<String> {
    if unreachable.is_empty() {
        return vec![];
    }
    let mut dropped = vec![];
    if let Some(route) = config.route.as_mut() {
        if let Some(rule_sets) = route.rule_set.as_mut() {
            let before = rule_sets.len();
            rule_sets.retain(|rs| {
                if unreachable.contains(&rs.tag) {
                    dropped.push(rs.tag.clone());
                    false
                } else {
                    true
                }
            });
            let _ = before;
        }
        let rules = std::mem::take(&mut route.rules);
        route.rules = prune_rules(rules, unreachable);
    }
    dropped
}

/// issue #147 本地 race DNS server tag（race on 时节点域名/国内内容解析指它）。上游 `DNS_NODE_RACE_TAG`。
pub const DNS_NODE_RACE_TAG: &str = "dns-node-race";

/// race 总开关：resolveNodeDomainsAhead !== false。上游 `isNodeRaceOn`。
pub fn is_node_race_on(resolve_node_domains_ahead: Option<bool>) -> bool {
    resolve_node_domains_ahead != Some(false)
}

/// issue #147 race off 单上游 id（优先新模型，回退 legacy）。上游 `effectiveSingleResolverId`。
fn effective_single_resolver_id(
    node_resolver_single: Option<&str>,
    node_domain_resolver: Option<&str>,
) -> String {
    if let Some(s) = node_resolver_single {
        if !s.is_empty() {
            return s.to_string();
        }
    }
    match node_domain_resolver.unwrap_or("auto") {
        "dnspod" => "dnspod".to_string(),
        "system" => "system".to_string(),
        _ => "ali".to_string(),
    }
}

/// 节点域名解析器 → DNS server tag。上游 `getNodeResolverTag`。
/// race on → dns-node-race；off → 按 single id（dnspod→dns-node / system→dns-local[TUN rule→dns-node] / ali→dial/rule 各基线）。
pub fn get_node_resolver_tag(
    resolve_node_domains_ahead: Option<bool>,
    node_resolver_single: Option<&str>,
    node_domain_resolver: Option<&str>,
    proxy_mode_type: &str,
    ctx: NodeResolverCtx,
) -> String {
    if is_node_race_on(resolve_node_domains_ahead) {
        return DNS_NODE_RACE_TAG.to_string();
    }
    let single = effective_single_resolver_id(node_resolver_single, node_domain_resolver);
    match single.as_str() {
        "dnspod" => "dns-node".to_string(),
        "system" => {
            // INV-1: TUN rule ctx 强制 dns-node（IP-DoH）防递归。
            if ctx == NodeResolverCtx::Rule && proxy_mode_type.eq_ignore_ascii_case("tun") {
                "dns-node".to_string()
            } else {
                "dns-local".to_string()
            }
        }
        _ => {
            // ali/自定义/缺省：两路径各自基线。
            match ctx {
                NodeResolverCtx::Dial => "dns-bootstrap".to_string(),
                NodeResolverCtx::Rule => "dns-domestic".to_string(),
            }
        }
    }
}

/// 解析器上下文（dial = outbound domain_resolver，rule = DNS rule1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeResolverCtx {
    Dial,
    Rule,
}

/// 节点 **dial 侧** `domain_resolver` 的下发形态。上游 `getNodeDialDomainResolver`（`a942c60`，#335）。
///
/// - `enable_ipv6 = false`：顶层 `dns.strategy` 是 `ipv4_only`（为 #57 抑制目标站点 AAAA），
///   它会连**节点域名**一起掐掉 AAAA ⇒ AAAA-only 的节点域名解析为空、整条代理不可用。
///   故此处下发结构化形态，**逐载体**把 dial 侧策略覆盖成 `prefer_ipv4`（v4 优先、可回落 v6）。
///   为什么不能只改 `route.default_domain_resolver` 一处：见 [`DomainResolver`] 的 loopback 对照。
/// - `enable_ipv6 = true`：顶层已是 `prefer_ipv4`，无需覆盖 → 保持纯 tag 字符串，
///   **该分支生成的 config 字节零变化**（这是本次修复的硬约束，金样 delta 不得落到这支上）。
///
/// 顶层 `dns.strategy` 本身一字未动：#57 对目标站点的 AAAA 抑制原样保留。
pub fn get_node_dial_domain_resolver(tag: &str, enable_ipv6: bool) -> DomainResolver {
    if enable_ipv6 {
        DomainResolver::Tag(tag.to_string())
    } else {
        DomainResolver::Detailed {
            // server 恒填：`{strategy}` 无 server 会被内核解析期硬拒（`empty domain_resolver.server`）。
            server: tag.to_string(),
            strategy: DomainStrategy::PreferIpv4,
        }
    }
}

/// 国内内容解析器 → server tag。上游 `getDomesticResolverTag`。
pub fn get_domestic_resolver_tag(
    resolve_node_domains_ahead: Option<bool>,
    fallback: &str,
) -> String {
    if is_node_race_on(resolve_node_domains_ahead) {
        DNS_NODE_RACE_TAG.to_string()
    } else {
        fallback.to_string()
    }
}

/// 用户自定义国内 DNS 若为 IP → {ip, port}（TUN 直连放行/排除集用）。上游 `getCustomDomesticDnsEndpoint`。
pub fn get_custom_domestic_dns_endpoint(domestic_dns: Option<&str>) -> Option<(String, u16)> {
    let parsed = crate::user_config::dns_spec::parse_dns_server_spec(domestic_dns)?;
    if parsed.is_domain {
        return None;
    }
    Some((parsed.server, parsed.port))
}

fn prune_rules(rules: Vec<RouteRule>, unreachable: &BTreeSet<String>) -> Vec<RouteRule> {
    rules
        .into_iter()
        .filter_map(|mut rule| {
            // logical 递归。
            if let Some(sub_rules) = rule.rules.take() {
                let before = sub_rules.len();
                let pruned = prune_rules(sub_rules, unreachable);
                // logical 子条件剪空 → 整条丢。
                if pruned.is_empty() {
                    return None;
                }
                // AND logical 任一子条件被剪 → 整条丢（fail-closed）。
                if rule.type_field.as_deref() == Some("logical")
                    && rule.mode.as_deref() == Some("and")
                    && pruned.len() < before
                {
                    return None;
                }
                rule.rules = Some(pruned);
            }
            // rule_set 引用检查。
            match &rule.rule_set {
                Some(crate::singbox::OneOrMany::One(t)) => {
                    if unreachable.contains(t) {
                        return None;
                    }
                }
                Some(crate::singbox::OneOrMany::Many(arr)) => {
                    let kept: Vec<String> = arr
                        .iter()
                        .filter(|t| !unreachable.contains(*t))
                        .cloned()
                        .collect();
                    if kept.is_empty() {
                        return None;
                    }
                    if kept.len() != arr.len() {
                        rule.rule_set = if kept.len() == 1 {
                            Some(crate::singbox::OneOrMany::One(
                                kept.into_iter().next().unwrap(),
                            ))
                        } else {
                            Some(crate::singbox::OneOrMany::Many(kept))
                        };
                    }
                }
                None => {}
            }
            Some(rule)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::rule::{AppRule, CustomAppPreset, RuleAction};

    struct Srv {
        id: String,
        name: String,
    }
    impl ServerLike for Srv {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            &self.name
        }
    }

    #[test]
    fn id_to_tag_unique() {
        let servers = [
            Srv {
                id: "s1".into(),
                name: "HK".into(),
            },
            Srv {
                id: "s2".into(),
                name: "HK".into(),
            },
            Srv {
                id: "s3".into(),
                name: "JP".into(),
            },
        ];
        let map = build_id_to_tag_map(&servers);
        assert_eq!(map.get("s1").unwrap(), "HK");
        assert_eq!(map.get("s2").unwrap(), "HK (1)"); // 撞名追加
        assert_eq!(map.get("s3").unwrap(), "JP");
    }

    #[test]
    fn id_to_tag_reserved_collision() {
        let servers = [Srv {
            id: "s1".into(),
            name: "direct".into(),
        }];
        let map = build_id_to_tag_map(&servers);
        assert_eq!(map.get("s1").unwrap(), "direct (1)"); // 撞保留 tag
    }

    #[test]
    fn id_to_tag_empty_name() {
        let servers = [Srv {
            id: "s1".into(),
            name: "  ".into(),
        }];
        let map = build_id_to_tag_map(&servers);
        assert_eq!(map.get("s1").unwrap(), UNNAMED_SERVER);
    }

    #[test]
    fn host_exclude_cidr() {
        assert_eq!(host_to_exclude_cidr("1.2.3.4"), Some("1.2.3.4/32".into()));
        assert_eq!(host_to_exclude_cidr("::1"), Some("::1/128".into()));
        assert_eq!(host_to_exclude_cidr("[::1]"), Some("::1/128".into()));
        assert_eq!(host_to_exclude_cidr("example.com"), None);
        assert_eq!(host_to_exclude_cidr(""), None);
    }

    #[test]
    fn effective_rules_smart_only() {
        let rule = Rule {
            id: "r1".into(),
            type_field: RuleType::Domain,
            values: vec!["a.com".into()],
            conditions: None,
            combine_mode: None,
            action: RuleAction::Proxy,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        };
        assert_eq!(
            effective_custom_rules("smart", std::slice::from_ref(&rule)).len(),
            1
        );
        assert!(effective_custom_rules("global", std::slice::from_ref(&rule)).is_empty());
        assert!(effective_custom_rules("direct", std::slice::from_ref(&rule)).is_empty());
    }

    #[test]
    fn geo_categories_from_rules() {
        let rule = Rule {
            id: "r1".into(),
            type_field: RuleType::Geosite,
            values: vec!["CN".into(), "ADS".into()],
            conditions: None,
            combine_mode: None,
            action: RuleAction::Proxy,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        };
        let (geosite, geoip) = get_required_geo_categories(&[rule], &[], &[]);
        assert!(geosite.contains("cn")); // lowercase
        assert!(geosite.contains("ads"));
        assert!(geoip.is_empty());
    }

    #[test]
    fn geo_categories_from_custom_app_preset() {
        // 自定义预设（id 以 custom- 起，不与内置撞 —— seed_default_app_rules 也按此前缀保留）。
        //
        // 本测试原用 `id: "youtube"` 当自定义预设，且断言 geoip 含 youtube。它当时能过，是因为
        // `get_required_geo_categories` 走的私有 `app_preset_lookup` **只查 custom**（内置恒 None）。
        // 接上内置表后 `get_app_preset` 按「先内置、后自定义」的既定优先级返回**内置** youtube
        // （其 geoipTags 为空）→ 原断言失效。**原 fixture 是在验证一个坏实现的行为**：它靠「内置查不到」
        // 才让 custom 影子生效，而真实优先级下自定义永远盖不住内置 id。故改用不撞名的 custom-*。
        let app_rule = AppRule {
            app_id: "custom-mytube".into(),
            action: RuleAction::Proxy,
            enabled: true,
            target_server_id: None,
        };
        let preset = CustomAppPreset {
            id: "custom-mytube".into(),
            name: "MyTube".into(),
            emoji: "📺".into(),
            icon_url: None,
            geosite_tags: vec!["mytube".into()],
            geoip_tags: vec!["mytube".into()],
            process_names: None,
            category: None,
        };
        let (geosite, geoip) = get_required_geo_categories(&[], &[app_rule], &[preset]);
        assert!(geosite.contains("mytube"));
        assert!(geoip.contains("mytube"));
    }

    #[test]
    fn geo_categories_from_builtin_app_preset() {
        // 回归门：内置预设的 geo tag 必须进 required 集。
        // 此前 `app_preset_lookup` 对全部 16 条内置预设返回 None → 本断言恒假，且**无任何测试覆盖**
        // （原测试只喂 custom preset，从没验过内置这条路）。这正是「门开在别处」的形态：
        // get_required_geo_categories 有测试、内置表有测试，两者的组合（生产路径）无门。
        let app_rule = AppRule {
            app_id: "telegram".into(),
            action: RuleAction::Proxy,
            enabled: true,
            target_server_id: None,
        };
        let (geosite, geoip) = get_required_geo_categories(&[], &[app_rule], &[]);
        assert!(
            geosite.contains("telegram"),
            "内置预设 telegram 的 geosite tag 未进 required 集"
        );
        assert!(
            geoip.contains("telegram"),
            "内置预设 telegram 的 geoip tag 未进 required 集"
        );
    }

    #[test]
    fn geo_categories_builtin_wins_over_custom_same_id() {
        // 优先级锁死：自定义预设不得影子内置 id（get_app_preset 文档「先内置，后自定义」）。
        let app_rule = AppRule {
            app_id: "youtube".into(),
            action: RuleAction::Proxy,
            enabled: true,
            target_server_id: None,
        };
        let shadow = CustomAppPreset {
            id: "youtube".into(),
            name: "Shadow".into(),
            emoji: "📺".into(),
            icon_url: None,
            geosite_tags: vec!["evil".into()],
            geoip_tags: vec!["evil".into()],
            process_names: None,
            category: None,
        };
        let (geosite, geoip) = get_required_geo_categories(&[], &[app_rule], &[shadow]);
        assert!(geosite.contains("youtube"), "内置 youtube 应生效");
        assert!(!geosite.contains("evil"), "自定义预设影子了内置 id");
        assert!(!geoip.contains("evil"), "自定义预设影子了内置 id");
    }

    #[test]
    fn geo_categories_skip_disabled_app_rule() {
        let app_rule = AppRule {
            app_id: "telegram".into(),
            action: RuleAction::Proxy,
            enabled: false,
            target_server_id: None,
        };
        let (geosite, geoip) = get_required_geo_categories(&[], &[app_rule], &[]);
        assert!(geosite.is_empty(), "禁用的 appRule 不该贡献 geo tag");
        assert!(geoip.is_empty());
    }

    #[test]
    fn node_resolver_race_on() {
        // race on（resolve !== false）→ dns-node-race，无论 single/ctx。
        let tag = get_node_resolver_tag(
            Some(true),
            Some("ali"),
            None,
            "systemProxy",
            NodeResolverCtx::Dial,
        );
        assert_eq!(tag, "dns-node-race");
        let tag = get_node_resolver_tag(None, None, None, "tun", NodeResolverCtx::Rule);
        assert_eq!(tag, "dns-node-race"); // 缺省 = on
    }

    #[test]
    fn node_resolver_race_off_single() {
        // race off（=false）→ 按 single。
        let tag = get_node_resolver_tag(
            Some(false),
            Some("dnspod"),
            None,
            "systemProxy",
            NodeResolverCtx::Dial,
        );
        assert_eq!(tag, "dns-node");
    }

    #[test]
    fn node_resolver_race_off_ali_split_dial_rule() {
        // ali 缺省：dial=dns-bootstrap / rule=dns-domestic（两路径基线）。
        let dial = get_node_resolver_tag(
            Some(false),
            Some("ali"),
            None,
            "systemProxy",
            NodeResolverCtx::Dial,
        );
        assert_eq!(dial, "dns-bootstrap");
        let rule = get_node_resolver_tag(
            Some(false),
            Some("ali"),
            None,
            "systemProxy",
            NodeResolverCtx::Rule,
        );
        assert_eq!(rule, "dns-domestic");
    }

    #[test]
    fn node_resolver_system_tun_rule_forces_dns_node() {
        // INV-1: TUN + system + rule → dns-node（防递归）。
        let tag = get_node_resolver_tag(
            Some(false),
            Some("system"),
            None,
            "tun",
            NodeResolverCtx::Rule,
        );
        assert_eq!(tag, "dns-node");
        // 非 TUN → dns-local。
        let tag = get_node_resolver_tag(
            Some(false),
            Some("system"),
            None,
            "systemProxy",
            NodeResolverCtx::Rule,
        );
        assert_eq!(tag, "dns-local");
    }

    #[test]
    fn node_resolver_legacy_fallback() {
        // 旧 nodeDomainResolver 单选档位迁移读取。
        let tag = get_node_resolver_tag(
            Some(false),
            None,
            Some("dnspod"),
            "systemProxy",
            NodeResolverCtx::Dial,
        );
        assert_eq!(tag, "dns-node");
        let tag = get_node_resolver_tag(
            Some(false),
            None,
            Some("system"),
            "systemProxy",
            NodeResolverCtx::Dial,
        );
        assert_eq!(tag, "dns-local");
    }

    #[test]
    fn domestic_resolver_race() {
        assert_eq!(
            get_domestic_resolver_tag(Some(true), "dns-bootstrap"),
            "dns-node-race"
        );
        assert_eq!(
            get_domestic_resolver_tag(Some(false), "dns-bootstrap"),
            "dns-bootstrap"
        );
        assert_eq!(get_domestic_resolver_tag(None, "fallback"), "dns-node-race");
        // 缺省 on
    }

    #[test]
    fn custom_domestic_dns_ip() {
        let ep = get_custom_domestic_dns_endpoint(Some("https://223.5.5.5/dns-query"));
        assert_eq!(ep, Some(("223.5.5.5".to_string(), 443)));
        // 域名 → None。
        assert!(get_custom_domestic_dns_endpoint(Some("https://doh.pub/dns-query")).is_none());
        assert!(get_custom_domestic_dns_endpoint(None).is_none());
    }
}
