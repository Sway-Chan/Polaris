//! 节点域名解析上游模型 —— 上游 `shared/node-resolver-upstreams.ts` 1:1 移植。
//!
//! 纯逻辑、无 I/O、可逐项单测：
//! - 内置上游单一真值（`ali` / `dnspod` / `system`），IP 取自 [`DOH_ALIDNS_IP`] / [`DOH_DNSPOD_IP`]，
//!   并由单测护栏钉死二者 ∈ `BOOTSTRAP_DIRECT_DNS_IPS`（否则其 :443 DoH 不被 route 直连放行 → TUN 下回环）。
//! - 自定义上游**强制纯 IP**（`parse_dns_server_spec().is_domain` 拒绝）：零 bootstrap + 直连放行确定。
//! - Tier1（加密 DoH）抢跑、上限 3；Tier2（明文 UDP / system）兜底，不占额度、不与 Tier1 抢跑。
//! - canonical 去重：内置与等价自定义合并（**先去重再数上限**，否则重复项会挤掉真上游）。

#![forbid(unsafe_code)]

use polaris_config_engine::user_config::dns_config::{CustomDnsUpstream, DnsConfig};
use polaris_config_engine::user_config::dns_spec::{parse_dns_server_spec, DnsServerType};
use polaris_config_engine::user_config::proxy_mode::ProxyModeType;

/// AliDNS IP-DoH 上游地址。**不变量**：∈ `BOOTSTRAP_DIRECT_DNS_IPS`（单测护栏）。上游 `DOH_ALIDNS_IP`。
pub const DOH_ALIDNS_IP: &str = "223.5.5.5";
/// DNSPod IP-DoH 上游地址。**不变量**：同上。上游 `DOH_DNSPOD_IP`。
pub const DOH_DNSPOD_IP: &str = "1.12.12.12";

/// Tier1 抢跑上游上限（设计 §9.1：2 见顶、第 3 冗余；只数 Tier1，Tier2 不占额度）。
pub const MAX_TIER1_UPSTREAMS: usize = 3;

/// 竞速 on 的默认上游池。上游 `DEFAULT_POOL_IDS`。
pub const DEFAULT_POOL_IDS: &[&str] = &["ali", "dnspod"];
/// 竞速 off 的默认单上游 id。上游 `DEFAULT_SINGLE_ID`。
pub const DEFAULT_SINGLE_ID: &str = "ali";

/// 上游解析方式。上游 `ResolveUpstream.kind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamKind {
    /// DoH（https，POST application/dns-message）/ DoT（tls，见 [`ResolveUpstream::dot`]）。
    Doh,
    /// 明文 UDP:53。
    Udp,
    /// 系统解析器（无 IP，走 OS resolver）。
    System,
}

/// 一个解析上游 = 一种解析方式 + 其 Tier。上游 `ResolveUpstream`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveUpstream {
    /// `ali` / `dnspod` / `system` 或自定义 id。
    pub id: String,
    pub kind: UpstreamKind,
    /// 纯 IP（doh/udp 有；system 无）。
    pub ip: Option<String>,
    pub port: Option<u16>,
    /// 仅 DoH(https) 有。
    pub path: Option<String>,
    /// `kind == Doh` 下是否为 DoT(tls)。**当前恒 false**：DoT 二期，[`parse_custom_upstream`] 直接拒
    /// `tls://`（见该函数注释：接受但永远 FAIL 比拒绝更糟）。
    pub dot: bool,
    /// 1 = 抢跑（加密）；2 = 兜底（明文 UDP / system）。
    pub tier: u8,
}

/// 内置上游单一真值（id → 上游）。上游 `BUILTIN_UPSTREAMS`。
#[must_use]
pub fn builtin_upstream(id: &str) -> Option<ResolveUpstream> {
    match id {
        "ali" => Some(ResolveUpstream {
            id: "ali".into(),
            kind: UpstreamKind::Doh,
            ip: Some(DOH_ALIDNS_IP.into()),
            port: Some(443),
            path: Some("/dns-query".into()),
            dot: false,
            tier: 1,
        }),
        "dnspod" => Some(ResolveUpstream {
            id: "dnspod".into(),
            kind: UpstreamKind::Doh,
            ip: Some(DOH_DNSPOD_IP.into()),
            port: Some(443),
            path: Some("/dns-query".into()),
            dot: false,
            tier: 1,
        }),
        "system" => Some(ResolveUpstream {
            id: "system".into(),
            kind: UpstreamKind::System,
            ip: None,
            port: None,
            path: None,
            dot: false,
            tier: 2,
        }),
        _ => None,
    }
}

/// 自定义上游 spec → [`ResolveUpstream`]；**强制纯 IP**，非法 / 域名 / `tls://` → `None`。
/// 上游 `parseCustomUpstream`。
///
/// - `https://` → Tier1 加密抢跑；`udp://` / 裸 IP → Tier2 明文兜底。
/// - `tls://`（DoT）二期未实现：查询侧对 `dot` 直接 Err（永远 FAIL）。**此处拒绝**，避免 UI 接受
///   `tls://` 上游、用户以为生效却静默全 FAIL。待 DoT 落地后改回 `dot: type == Tls`。
#[must_use]
pub fn parse_custom_upstream(c: &CustomDnsUpstream) -> Option<ResolveUpstream> {
    if c.id.is_empty() || c.spec.is_empty() {
        return None;
    }
    let p = parse_dns_server_spec(Some(&c.spec))?;
    if p.is_domain {
        return None; // 纯 IP 强制
    }
    match p.server_type {
        DnsServerType::Udp => Some(ResolveUpstream {
            id: c.id.clone(),
            kind: UpstreamKind::Udp,
            ip: Some(p.server),
            port: Some(p.port),
            path: None,
            dot: false,
            tier: 2,
        }),
        DnsServerType::Tls => None, // DoT 二期，见函数文档
        DnsServerType::Https => Some(ResolveUpstream {
            id: c.id.clone(),
            kind: UpstreamKind::Doh,
            ip: Some(p.server),
            port: Some(p.port),
            path: Some(p.path.unwrap_or_else(|| "/dns-query".into())),
            dot: false,
            tier: 1,
        }),
    }
}

/// UI 校验：自定义 spec 是否合法（纯 IP DoH / UDP）。上游 `isValidCustomUpstreamSpec`。
#[must_use]
pub fn is_valid_custom_upstream_spec(spec: &str) -> bool {
    parse_custom_upstream(&CustomDnsUpstream {
        id: "_probe".into(),
        spec: spec.to_string(),
    })
    .is_some()
}

/// canonical 去重 key：`system` 唯一；其余按 `(kind, IP, port, path)`。
/// udp 与 doh 即便同 IP 也不同（协议/端口不同）。上游 `upstreamCanonicalKey`。
#[must_use]
pub fn upstream_canonical_key(u: &ResolveUpstream) -> String {
    if u.kind == UpstreamKind::System {
        return "system".into();
    }
    let kind = match u.kind {
        UpstreamKind::Doh => "doh",
        UpstreamKind::Udp => "udp",
        UpstreamKind::System => "system",
    };
    format!(
        "{kind}:{}:{}:{}",
        u.ip.as_deref().unwrap_or(""),
        u.port.map(|p| p.to_string()).unwrap_or_default(),
        u.path.as_deref().unwrap_or("")
    )
}

/// 分桶后的上游集。上游 `ResolvedUpstreams`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedUpstreams {
    /// 抢跑层（去重 + 上限 [`MAX_TIER1_UPSTREAMS`]）。
    pub tier1: Vec<ResolveUpstream>,
    /// 兜底层（不抢跑、不占额度）。
    pub tier2: Vec<ResolveUpstream>,
    /// 全部纯 IP —— 喂 `GenerateConfigDeps::race_upstream_ips`，供 route 直连放行防 TUN 回环。
    pub direct_ips: Vec<String>,
    /// 上面这些上游**实际在用的端口**（去重）—— 喂 `GenerateConfigDeps::race_upstream_ports`。
    ///
    /// issue #147：直连放行规则的端口集若只有恒定的 `[53, 443]`，自定义 DoH 的非标端口
    /// （`https://9.9.9.9:8443/q`）与自定义 UDP 的非 53 口就**只放行了 IP、没放行端口** ⇒ 规则匹配不上
    /// ⇒ TUN 下该上游的流量落到通用路由经代理出站 ⇒ 起核自举窗内恒 FAIL 或回环（sidecar 本就是要给
    /// 内核解析节点域名的，却得先经内核出网）。上游 同缺口，属上游继承 bug。
    ///
    /// **与 [`direct_ips`](Self::direct_ips) 是同一次遍历的两路投影，各自去重**：route 的直连放行是
    /// 一条 `ip_cidr × port` 的叉乘规则，本就不成对匹配，故两个**集合**才是它要的形态。
    ///
    /// **必须从这里下发、不许在 route 侧照着配置复算**：真实上游集是 Tier 分桶 + canonical 去重 +
    /// Tier1 上限 + INV-1 过滤（外加「Tier1 空则回退默认池」）之后的结果 —— 那是一条只在本 crate 里
    /// 完整存在的选择链，复算必然是它的近似（当时的实现取的是超集）。两份真值源迟早会分叉，而分叉的
    /// 代价不对称：多放行一个端口只是无害的宽松，少放行一个正在用的端口 = 该上游恒死。
    pub direct_ports: Vec<u16>,
}

/// 池里一个 id → 上游（内置优先，回退同 id 的自定义项）。`None` = 无效 id / 自定义 spec 非纯 IP。
///
/// 抽成具名函数是为了让 [`plan_upstreams`] 的 INV-1 过滤与 [`resolve_upstreams`] 的分桶**用同一套
/// 解析口径**：过滤若自己按 id 字符串猜 kind（如 `id == "system"`），将来多一个产 `System` 的来源
/// 就会漏筛 —— 而漏筛的后果是 TUN 下的递归放大，不是少一个上游。
fn lookup_upstream(id: &str, custom: &[CustomDnsUpstream]) -> Option<ResolveUpstream> {
    builtin_upstream(id).or_else(|| {
        custom
            .iter()
            .find(|c| c.id == id)
            .and_then(parse_custom_upstream)
    })
}

/// 上游 id 列表 → Tier1/Tier2 分桶 + canonical 去重 + Tier1 上限。上游 `resolveUpstreams`。
///
/// 无效 id / 自定义解析失败 / 重复 → 跳过。
/// **空 Tier1 → 回退默认 `[ali, dnspod]`**（全不勾 / 全无效 / 只勾了 Tier2 时防「无抢跑上游」全断，
/// 设计 §9.3 校验闸）。
#[must_use]
pub fn resolve_upstreams(ids: &[String], custom: &[CustomDnsUpstream]) -> ResolvedUpstreams {
    let mut seen: Vec<String> = Vec::new();
    let mut tier1: Vec<ResolveUpstream> = Vec::new();
    let mut tier2: Vec<ResolveUpstream> = Vec::new();
    for id in ids {
        let Some(up) = lookup_upstream(id, custom) else {
            continue; // 无效 id / 自定义非纯 IP → 跳过
        };
        let key = upstream_canonical_key(&up);
        if seen.contains(&key) {
            continue; // 去重（内置与等价自定义合并）
        }
        seen.push(key);
        if up.tier == 1 {
            if tier1.len() < MAX_TIER1_UPSTREAMS {
                tier1.push(up); // 上限（去重后才数）
            }
        } else {
            tier2.push(up);
        }
    }
    if tier1.is_empty() {
        // 竞速至少要有一个抢跑上游；去重防与已选 Tier2 重复（system 在 Tier2，不会撞）。
        for id in DEFAULT_POOL_IDS {
            if let Some(up) = builtin_upstream(id) {
                let key = upstream_canonical_key(&up);
                if !seen.contains(&key) {
                    seen.push(key);
                    tier1.push(up);
                }
            }
        }
    }
    // 直连放行的两路投影（同一次遍历、各自去重）：IP 进 `ip_cidr`、端口进 `port`。
    // `system` 两者皆 None ⇒ 一路都不进（它没有可放行的目的地，正是 INV-1 摘它的理由）。
    let mut direct_ips: Vec<String> = Vec::new();
    let mut direct_ports: Vec<u16> = Vec::new();
    for u in tier1.iter().chain(tier2.iter()) {
        if let Some(ip) = &u.ip {
            if !direct_ips.contains(ip) {
                direct_ips.push(ip.clone());
            }
        }
        if let Some(port) = u.port {
            if !direct_ports.contains(&port) {
                direct_ports.push(port);
            }
        }
    }
    ResolvedUpstreams {
        tier1,
        tier2,
        direct_ips,
        direct_ports,
    }
}

/// 起 sidecar 前的**唯一决策点**：读 `dnsConfig` → 该起哪些上游，还是根本不起。
///
/// `None` ⟺ 竞速关（`resolveNodeDomainsAhead === false`）⟹ 调用方不起 sidecar ⟹
/// `race_server_port` 恒 0 ⟹ config-engine `with_race_off` 强制单上游路径（`nodeResolverSingle`）。
/// 缺省 / `true` 均视为开（对齐 上游 `!== false` 语义：老配置无此字段 = 开）。
///
/// 抽成具名纯函数（而不是内联进起核流程）是为了让「竞速 off 不走池」这条不变式**本身可单测** ——
/// 内联进 `start_inner` 就只能靠真起核才测得到，而那是真机门。
///
/// # INV-1（TUN 接管期 `system` 不得入池）
///
/// 非竞速路径早就为这条链立了不变量：`config-engine` 的 `helpers.rs::get_node_resolver_tag` ——
/// **「TUN + rule ctx + single=system → 强制走 dns-node（IP-DoH）防递归」**。竞速路径把节点域名统一
/// 指向 `dns-node-race` 之后，这条不变量在**池里勾了 `system`** 时一度没有对应实现，放大链是：
///
/// ```text
/// 内核查节点域名 → dns-node-race → sidecar 的 system 腿 → OS resolver → 明文 :53 发往 LAN DNS
///   → route 的 `hijack-dns`（先于 LAN bypass）抓走 → 内核按域名规则又指回 dns-node-race → …
/// ```
/// Tier1 全 FAIL（离线 / DoH 被封）时每一层都再放一轮齐射，逐级放大。
///
/// **为什么摘 `system` 就够**：TUN 下所有上游的**出网** IP 都由 `direct_ips` →
/// `GenerateConfigDeps::race_upstream_ips` → route 的「DNS 直连放行」规则放行（`:53`/`:443` 及自定义
/// 端口），故 DoH / 自定义 UDP 上游的查询根本走不到 `hijack-dns`。唯独 `system` **没有 IP**
/// （`kind == System ⇒ ip == None`）—— 它把目的地交给 OS resolver 决定，Polaris 无从放行，这才是
/// 唯一能掉进劫持链的上游形态。
///
/// **摘在 `resolve_upstreams` 之前**（对 id 列表过滤，而不是对结果切）：这样「摘完 Tier1 空」会自动
/// 落进既有的默认池回退闸（`[ali, dnspod]`），不会产出「无抢跑上游」的死配置。
#[must_use]
pub fn plan_upstreams(
    dns: Option<&DnsConfig>,
    proxy_mode_type: ProxyModeType,
) -> Option<ResolvedUpstreams> {
    if dns.and_then(|d| d.resolve_node_domains_ahead) == Some(false) {
        return None;
    }
    let owned_default: Vec<String>;
    let mut ids: &[String] = match dns.and_then(|d| d.node_resolver_pool.as_deref()) {
        Some(p) => p,
        None => {
            owned_default = DEFAULT_POOL_IDS.iter().map(|s| (*s).to_string()).collect();
            &owned_default
        }
    };
    let custom: &[CustomDnsUpstream] = dns
        .and_then(|d| d.node_resolver_custom.as_deref())
        .unwrap_or(&[]);
    let tun_filtered: Vec<String>;
    if proxy_mode_type.is_tun() {
        // INV-1：TUN 接管期把 `system` 从池里摘除（见本函数文档）。
        tun_filtered = ids
            .iter()
            .filter(|id| {
                !matches!(
                    lookup_upstream(id, custom).map(|u| u.kind),
                    Some(UpstreamKind::System)
                )
            })
            .cloned()
            .collect();
        ids = &tun_filtered;
    }
    Some(resolve_upstreams(ids, custom))
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_config_engine::user_config::dns_constants::is_bootstrap_direct_dns;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn custom(id: &str, spec: &str) -> CustomDnsUpstream {
        CustomDnsUpstream {
            id: id.into(),
            spec: spec.into(),
        }
    }

    #[test]
    fn builtin_doh_ips_are_direct_allowed_by_route() {
        // 不变量护栏：两个内置 DoH IP 必须 ∈ BOOTSTRAP_DIRECT_DNS_IPS，否则 TUN 下它们的 :443
        // 不被直连放行 → sidecar 的 DoH 请求经 TUN 回到内核 → 内核又等 sidecar 解析 = 死环。
        assert!(is_bootstrap_direct_dns(DOH_ALIDNS_IP));
        assert!(is_bootstrap_direct_dns(DOH_DNSPOD_IP));
    }

    #[test]
    fn default_pool_is_two_tier1_doh() {
        let r = resolve_upstreams(&ids(DEFAULT_POOL_IDS), &[]);
        assert_eq!(r.tier1.len(), 2);
        assert!(r.tier2.is_empty());
        assert_eq!(r.direct_ips, vec![DOH_ALIDNS_IP, DOH_DNSPOD_IP]);
    }

    #[test]
    fn system_goes_to_tier2_and_does_not_consume_tier1_quota() {
        let r = resolve_upstreams(&ids(&["system", "ali", "dnspod"]), &[]);
        assert_eq!(r.tier1.len(), 2, "system 不占 Tier1 额度");
        assert_eq!(r.tier2.len(), 1);
        assert_eq!(r.tier2[0].kind, UpstreamKind::System);
        assert!(
            !r.direct_ips.iter().any(|s| s.is_empty()),
            "system 无 IP，不进 direct_ips"
        );
    }

    #[test]
    fn tier1_capped_at_three_after_dedupe() {
        let cs = vec![
            custom("c1", "https://9.9.9.9/dns-query"),
            custom("c2", "https://8.8.8.8/dns-query"),
            custom("c3", "https://1.1.1.1/dns-query"),
        ];
        let r = resolve_upstreams(&ids(&["ali", "dnspod", "c1", "c2", "c3"]), &cs);
        assert_eq!(r.tier1.len(), MAX_TIER1_UPSTREAMS);
        assert_eq!(
            r.tier1.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            vec!["ali", "dnspod", "c1"],
            "按池顺序取前 3"
        );
    }

    #[test]
    fn duplicate_of_builtin_is_merged_before_counting_cap() {
        // 自定义写成与内置等价（同 kind/IP/port/path）→ 去重合并，不占第二个额度。
        let cs = vec![
            custom("dup", "https://223.5.5.5/dns-query"),
            custom("c2", "https://8.8.8.8/dns-query"),
            custom("c3", "https://9.9.9.9/dns-query"),
        ];
        let r = resolve_upstreams(&ids(&["ali", "dup", "c2", "c3"]), &cs);
        assert_eq!(
            r.tier1.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            vec!["ali", "c2", "c3"],
            "dup 被合并，额度留给真正不同的上游"
        );
    }

    #[test]
    fn empty_tier1_falls_back_to_defaults() {
        // 只勾 system / 全是无效 id → Tier1 空 → 回退 [ali, dnspod]（不留「无抢跑上游」的死配置）。
        let r = resolve_upstreams(&ids(&["system"]), &[]);
        assert_eq!(
            r.tier1.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            vec!["ali", "dnspod"]
        );
        assert_eq!(r.tier2.len(), 1);
        let r2 = resolve_upstreams(&ids(&["nope", "gone"]), &[]);
        assert_eq!(r2.tier1.len(), 2);
    }

    #[test]
    fn custom_domain_and_dot_specs_are_rejected() {
        assert!(parse_custom_upstream(&custom("d", "https://dns.google/dns-query")).is_none());
        assert!(parse_custom_upstream(&custom("d", "tls://1.1.1.1:853")).is_none());
        assert!(parse_custom_upstream(&custom("", "https://1.1.1.1/dns-query")).is_none());
        assert!(parse_custom_upstream(&custom("d", "")).is_none());
        assert!(!is_valid_custom_upstream_spec(
            "https://dns.google/dns-query"
        ));
        assert!(is_valid_custom_upstream_spec("https://1.1.1.1/dns-query"));
        assert!(
            is_valid_custom_upstream_spec("8.8.4.4"),
            "裸 IP → Tier2 udp"
        );
    }

    #[test]
    fn bare_ip_custom_is_tier2_udp() {
        let u = parse_custom_upstream(&custom("x", "8.8.4.4")).expect("裸 IP 合法");
        assert_eq!(u.kind, UpstreamKind::Udp);
        assert_eq!(u.tier, 2);
        assert_eq!(u.port, Some(53));
    }

    /// 非 TUN（系统代理）—— 绝大多数既有断言的基线口径。
    const NON_TUN: ProxyModeType = ProxyModeType::SystemProxy;

    #[test]
    fn plan_returns_none_when_race_disabled() {
        // 【不变式：竞速 off 不走池】关掉总开关 → None，即便池里有一堆上游。
        let dns = DnsConfig {
            resolve_node_domains_ahead: Some(false),
            node_resolver_pool: Some(ids(&["ali", "dnspod", "system"])),
            ..Default::default()
        };
        assert!(plan_upstreams(Some(&dns), NON_TUN).is_none());
        assert!(
            plan_upstreams(Some(&dns), ProxyModeType::Tun).is_none(),
            "总开关优先于 INV-1 过滤：竞速 off 一律不起 sidecar"
        );
    }

    // ── INV-1：TUN 接管期 system 不得入竞速池 ────────────────────────────────

    fn pool_with_system() -> DnsConfig {
        DnsConfig {
            node_resolver_pool: Some(ids(&["ali", "dnspod", "system"])),
            ..Default::default()
        }
    }

    /// 【INV-1 正向】TUN 下 `system` 必须被摘除；【反向】非 TUN 下仍可入池（Tier2 兜底）。
    ///
    /// 两侧一起断言才是不变量：只测 TUN 会被「无论什么模式都摘掉 system」这种过度修法蒙混过关，
    /// 而那会白白砍掉系统代理模式下唯一的本地兜底腿（那里没有 `hijack-dns`，不存在递归）。
    ///
    /// **变异锁**：① 删掉 `plan_upstreams` 里的 `if proxy_mode_type.is_tun()` 过滤块 → TUN 断言转红；
    /// ② 把过滤条件写成恒真（不判模式）→ 非 TUN 断言转红。
    #[test]
    fn plan_drops_system_under_tun_and_keeps_it_otherwise() {
        let dns = pool_with_system();
        let tun = plan_upstreams(Some(&dns), ProxyModeType::Tun).expect("竞速开");
        assert!(
            !tun.tier1
                .iter()
                .chain(tun.tier2.iter())
                .any(|u| u.kind == UpstreamKind::System),
            "TUN 接管期 system 必须离池（INV-1）：它没有 IP ⇒ 无法被 route 直连放行 ⇒ \
             OS resolver 的明文 :53 被 hijack-dns 抓回内核 → 又指回 dns-node-race，逐级放大"
        );
        assert_eq!(
            tun.tier1.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            vec!["ali", "dnspod"],
            "摘 system 不得影响 Tier1"
        );

        let sysproxy = plan_upstreams(Some(&dns), NON_TUN).expect("竞速开");
        assert!(
            sysproxy
                .tier2
                .iter()
                .any(|u| u.kind == UpstreamKind::System),
            "非 TUN 无 hijack-dns、不存在自递归 → system 仍是合法的 Tier2 兜底"
        );
        assert_eq!(sysproxy.tier1.len(), 2);
        // `system` 无 IP ⇒ 两种模式的直连放行集逐字节相同（金样零变化的根据）。
        assert_eq!(tun.direct_ips, sysproxy.direct_ips);
    }

    /// 只勾了 `system` 的池 + TUN：摘完 Tier1 空 → 必须落进既有的默认池回退闸，而不是产出空计划。
    ///
    /// 这正是「摘在 `resolve_upstreams` 之前」的理由 —— 若改成对结果切 Tier，这里会得到一个
    /// **零上游**的 sidecar：所有节点域名解析恒 SERVFAIL，且日志上看不出是被谁摘的。
    ///
    /// **变异锁**：把过滤挪到 `resolve_upstreams` 之后（对 `tier2` retain）→ 本测仍绿但
    /// `plan_drops_system_under_tun_and_keeps_it_otherwise` 不受影响；真正钉死的是下面这条
    /// 「Tier1 非空」断言在「过滤 + 不回退」写法下会转红。
    #[test]
    fn plan_falls_back_to_defaults_when_tun_filter_empties_the_pool() {
        let dns = DnsConfig {
            node_resolver_pool: Some(ids(&["system"])),
            ..Default::default()
        };
        let r = plan_upstreams(Some(&dns), ProxyModeType::Tun).expect("竞速开");
        assert_eq!(
            r.tier1.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            vec!["ali", "dnspod"],
            "摘空后须回退默认池（否则 TUN + 只勾 system = 零上游，节点域名全 SERVFAIL）"
        );
        assert!(r.tier2.is_empty(), "system 已被 INV-1 摘除，Tier2 应为空");
    }

    /// 过滤按**解析出来的 kind** 走，不按 id 字符串：同 id 的自定义项遮不住内置 `system`，
    /// 而合法的自定义 UDP 上游（有 IP ⇒ 能被直连放行）在 TUN 下必须留下。
    ///
    /// **变异锁**：把过滤条件换成 `id == "system"` 的字符串比对 → 本测的「自定义 UDP 仍在」这条仍绿，
    /// 但一旦将来多一个产 `System` kind 的来源即静默漏筛；把条件写成「摘掉所有 Tier2」→ 转红。
    #[test]
    fn tun_filter_keeps_custom_udp_tier2_upstreams() {
        let dns = DnsConfig {
            node_resolver_pool: Some(ids(&["ali", "system", "my-udp"])),
            node_resolver_custom: Some(vec![custom("my-udp", "8.8.4.4")]),
            ..Default::default()
        };
        let r = plan_upstreams(Some(&dns), ProxyModeType::Tun).expect("竞速开");
        assert_eq!(
            r.tier2.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            vec!["my-udp"],
            "自定义 UDP 有 IP → 进 direct_ips → 被 route 直连放行 ⇒ 不掉进 hijack-dns，可留"
        );
        assert!(
            r.direct_ips.contains(&"8.8.4.4".to_string()),
            "留下的 Tier2 上游必须同时进直连放行集（这正是它与 system 的分界）"
        );
    }

    #[test]
    fn plan_uses_pool_when_race_on_and_defaults_when_absent() {
        let on = DnsConfig {
            resolve_node_domains_ahead: Some(true),
            node_resolver_pool: Some(ids(&["dnspod", "system"])),
            ..Default::default()
        };
        let r = plan_upstreams(Some(&on), NON_TUN).expect("on → Some");
        assert_eq!(
            r.tier1.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
            vec!["dnspod"]
        );
        assert_eq!(r.tier2.len(), 1);

        // 缺省（老配置无 resolveNodeDomainsAhead / 无 pool）→ 开 + 默认池。
        let r2 = plan_upstreams(Some(&DnsConfig::default()), NON_TUN).expect("缺省视为开");
        assert_eq!(r2.tier1.len(), 2);
        let r3 = plan_upstreams(None, NON_TUN).expect("无 dnsConfig 也视为开");
        assert_eq!(r3.tier1.len(), 2);
    }

    // ── direct_ports：直连放行的端口轴（issue #147）────────────────────────────

    /// 【不变式：自定义上游的**非标端口**必须随 IP 一起进直连放行集】
    ///
    /// 端口集若只有恒定的 `[53,443]`，`https://9.9.9.9:8443/q` 与 `udp://9.9.9.9:5353` 的流量匹配不上
    /// route 的直连规则 → TUN 下经代理出站 → 起核自举窗内该上游恒 FAIL/回环。
    ///
    /// **变异锁**：删掉 `resolve_upstreams` 里 `direct_ports` 的 push → 转红；
    /// 把 `direct_ports` 写成恒 `vec![53,443]` → 8443/5353 两条断言转红。
    #[test]
    fn direct_ports_carry_custom_nonstandard_ports() {
        let cs = vec![
            custom("my-doh", "https://9.9.9.9:8443/q"),
            custom("my-udp", "udp://9.9.9.9:5353"),
        ];
        let r = resolve_upstreams(&ids(&["ali", "my-doh", "my-udp"]), &cs);
        assert!(r.direct_ports.contains(&443), "内置 DoH 的 :443 须在");
        assert!(
            r.direct_ports.contains(&8443),
            "自定义 DoH 非标端口须在，实得 {:?}",
            r.direct_ports
        );
        assert!(
            r.direct_ports.contains(&5353),
            "自定义 UDP 非 53 口须在，实得 {:?}",
            r.direct_ports
        );
        // 两轴同源：IP 也必须在（route 的规则是 ip_cidr × port 叉乘，缺一匹配不上）。
        assert!(r.direct_ips.contains(&"9.9.9.9".to_string()));
    }

    /// 【不变式：端口集是**真实上游集**的投影，不是配置池的复算】
    ///
    /// 这是「端口随 IP 一起下发」相对「route 侧照 `nodeResolverPool` 复算」的可观测差别：被 Tier1 上限
    /// 挤掉的条目**根本不会被 sidecar 查询**，它的端口不该出现在直连放行集里（复算版会放进去 —— 方向
    /// 安全但那是超集，且两份口径迟早分叉）。
    ///
    /// **变异锁**：把 `direct_ports` 改成遍历入参 `ids` / `custom`（即复算口径）→ `9999` 断言转红。
    #[test]
    fn direct_ports_exclude_upstreams_dropped_by_tier1_cap() {
        let cs = vec![
            custom("c1", "https://8.8.8.8:8443/q"),
            custom("c2", "https://1.1.1.1:8444/q"),
            custom("c3", "https://9.9.9.9:9999/q"), // 第 4 个 Tier1 → 被上限挤掉
        ];
        let r = resolve_upstreams(&ids(&["ali", "c1", "c2", "c3"]), &cs);
        assert_eq!(r.tier1.len(), MAX_TIER1_UPSTREAMS, "前提：c3 被上限挤掉");
        assert!(r.direct_ports.contains(&8443) && r.direct_ports.contains(&8444));
        assert!(
            !r.direct_ports.contains(&9999),
            "被上限挤掉的上游从不被查询 → 其端口不该进放行集，实得 {:?}",
            r.direct_ports
        );
        assert!(
            !r.direct_ips.contains(&"9.9.9.9".to_string()),
            "IP 轴同理（既有口径，一并钉住）"
        );
    }

    /// `system` 两轴皆无（无 IP ⇒ 无可放行的目的地）——端口集不得因它多出条目，也不得混入 0。
    #[test]
    fn system_contributes_neither_ip_nor_port() {
        let with_sys = resolve_upstreams(&ids(&["ali", "system"]), &[]);
        let without = resolve_upstreams(&ids(&["ali"]), &[]);
        assert_eq!(
            with_sys.direct_ports, without.direct_ports,
            "system 不得改变端口轴"
        );
        assert_eq!(with_sys.direct_ips, without.direct_ips);
        assert!(!with_sys.direct_ports.contains(&0));
    }

    /// 端口集去重：同端口的多个上游只留一份（route 侧 `dedupe` 之外的第一道，保证下发形态稳定）。
    #[test]
    fn direct_ports_are_deduped() {
        let cs = vec![
            custom("a", "https://8.8.8.8:8443/q"),
            custom("b", "https://1.1.1.1:8443/q"),
        ];
        let r = resolve_upstreams(&ids(&["a", "b"]), &cs);
        assert_eq!(r.direct_ports, vec![8443], "同端口不同 IP → 端口只留一份");
        assert_eq!(r.direct_ips.len(), 2, "IP 轴仍是两个");
    }

    #[test]
    fn plan_consumes_custom_pool_entry_end_to_end() {
        // 配置字段消费面闭环：nodeResolverCustom 的 spec 真的变成 Tier1 上游 + direct_ips。
        let dns = DnsConfig {
            node_resolver_pool: Some(ids(&["ali", "my-doh"])),
            node_resolver_custom: Some(vec![custom("my-doh", "https://9.9.9.9:8443/q")]),
            ..Default::default()
        };
        let r = plan_upstreams(Some(&dns), NON_TUN).expect("on");
        let mine = r
            .tier1
            .iter()
            .find(|u| u.id == "my-doh")
            .expect("自定义生效");
        assert_eq!(mine.ip.as_deref(), Some("9.9.9.9"));
        assert_eq!(mine.port, Some(8443));
        assert_eq!(mine.path.as_deref(), Some("/q"));
        assert!(
            r.direct_ips.contains(&"9.9.9.9".to_string()),
            "自定义 IP 须进直连放行"
        );
    }
}
