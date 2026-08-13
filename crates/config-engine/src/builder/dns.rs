//! sing-box DNS 配置生成 —— 上游 `singbox-dns-builder.ts` 1:1 移植。
//!
//! 纯函数：只读 config + 注入实例态（lanResolverForDns / log 回调 / 路径 / FS 检查 / 平台），
//! 不持有任何 ProxyManager 引用。原文件 659 行，每行 Rust 严格对应 Polaris TS 语义。
//!
//! config 字节等价由 config-snapshot 网验证（DNS 进阶分支：自定义上游 / nodeResolver 档位 /
//! bypassFakeIP / enableIPv6 / dns-lan / win32 死环 已锁基线）。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use crate::builder::helpers::probe_pool_inbound_tag;
use crate::singbox::endpoint::Endpoint;
use crate::singbox::DnsConfig;
use crate::singbox::DnsRule;
use crate::singbox::DnsServer;
use crate::singbox::OneOrMany;
use crate::user_config::app_config::UserConfig;
use crate::user_config::collections::dedupe;
use crate::user_config::dns_spec::{parse_dns_server_spec, DnsServerType, ParsedDnsServer};
use crate::user_config::fakeip_filter::{
    FAKEIP_FILTER_CAPTIVE_DOMAINS, FAKEIP_FILTER_NTP_STUN_KEYWORDS, FAKEIP_FILTER_NTP_SUFFIXES,
};
use crate::user_config::log_level::LogLevel;
use crate::user_config::neighbor::{is_source_device_match_supported, normalize_neighbor_domain};
use crate::user_config::proxy_mode::ProxyModeType;
use crate::user_config::region_routing::{effective_region_routing, region_local_geo};
use crate::user_config::rule::{RuleAction, RuleType};
use crate::user_config::rules::rule_conditions;
use crate::user_config::tun_config::{FAKEIP_INET4_RANGE, FAKEIP_INET6_RANGE};

use super::custom_rule_files::{custom_rule_file_base, plan_custom_rule, uses_fake_ip, RulePlan};
use super::helpers::{
    effective_custom_rules, get_domestic_resolver_tag, get_node_resolver_tag, is_ipv4_host,
    is_ipv6_host, NodeResolverCtx, DNS_NODE_RACE_TAG, DOMESTIC_BANK_AND_STOCK_DOMAINS,
};
use crate::user_config::builtin_geo_rulesets::find_builtin;

// ───────────────────────── 常量（Polaris 顶部常量 1:1） ─────────────────────────

/// FakeIP 合成应答下发的 TTL（秒）。默认不设时 sing-box 用 `DefaultDNSTTL = 600`。
///
/// # 为什么要压
///
/// sing-box 的 fakeip 计数器持久化有缺陷：异步写者（`experimental/cachefile/fakeip.go`）的闭包只
/// 捕获**本进程首次分配**的快照，之后定时器只重排期不换数据 ⇒ 落盘的恒是首次分配点；而
/// 准确终值只由 `fakeip.Store.Close()` 写。**Windows 上 Polaris 停核走 `TerminateProcess` 硬杀**
/// （`platform/windows/mod.rs` 的 session-0 地雷那节：`GenerateConsoleCtrlEvent` 在服务模式是
/// no-op），`Close()` 基本不执行 ⇒ 重启后计数器回退、而地址→域名映射表完整保留 ⇒ 新域名覆盖旧
/// 地址的映射，**客户端手里那份旧映射还没过期**，于是浏览器拿着 A 的假 IP 连到了 B。
///
/// # 它压的到底是什么（**不是**「错配窗口」，这点曾被写错）
///
/// **服务端那条错映射不会过期**：`experimental/cachefile/fakeip.go` 全文没有任何 TTL / LRU /
/// 逐出机制，`FakeIPStore` 只 `Put`、`FakeIPLoad` 只 `Get`；一条映射一直有效，直到那个地址被
/// 再次分配、或整表 `FakeIPReset`。所以本常量**削不掉错误本身的寿命**。
///
/// 它削的是**客户端相信旧映射的时长**：错配成立的条件是「内核重新发放了地址 X，而此时仍有
/// 客户端相信 X 属于旧域名」。TTL 越短，客户端越早回来复查、越早改信新映射。600 秒意味着最长
/// 十分钟（且 `ipconfig /flushdns` 够不到 Chrome 自有的 HostResolver，用户无法自救）。
///
/// **对不重新解析的流量零收益**：已建立的长连接（WebSocket / HTTP2 keep-alive / IM）不会再发
/// DNS 查询，本常量对它们完全够不到。这是它的天花板，不是实现缺陷。
///
/// # 为什么是这个量级而不是 1 或 60
///
/// fakeip 应答是**本地合成**，重查不产生任何外部往返，成本只有一次 TUN → sing-box 的本地查询；
/// 所以下界不由成本决定，而由「别把客户端解析器打成忙等」决定。5 秒在两侧都留了余量。
///
/// # 射程边界（未验的那一格）
///
/// 两级证据，别混用：
/// - **固定内核接受该字段**：本仓实测，正负对照俱全（见 `singbox/dns.rs` 的 `rewrite_ttl` 文档）。
/// - **下发路径已在源码层走通**：`dns/router.go:318-319`/`:397-399` 对 fakeip 强制
///   `options.DisableCache = true`，而 `dns/client.go:304` 的 `applyResponseOptions` 在
///   `disableCache` 为真时**仍然执行** ⇒ TTL 改写不会被「不进缓存」这条旁路吃掉。
/// - **仍未验**：运行期实际下发的 TTL 是不是 5。要真机 `dig`/`nslookup` 看应答里的 TTL 才算数。
///
/// 这条只削客户端的陈旧信念时长，**不修根因**。根因是「Windows 硬杀 ⇒ `Store.Close()` 不执行 ⇒
/// 计数器回退」。曾试过在 Windows 上关 `store_fakeip` 来「结构性消灭错配」，**已证伪并回退**：
/// 陈旧信念在客户端，清空内核表清不掉它；反而毁掉 `dns/transport/fakeip/store.go:107-118` 里
/// `Create` 的地址复用（命中已有域名即返回原地址、不推进计数器），以及未被重发地址的正确反查。
/// 取证见 `~/docs/polaris/design/polaris-fakeip-347-crosscheck-2026-08-10.md`。
///
/// # 取值 60（2026-08-11 由 5 上调，用户裁定）
///
/// 上调**放宽**了错配暴露窗口（客户端最长 60 秒仍持有旧映射，原为 5 秒），换来的是客户端重查频率
/// 降到 1/12。两侧都不是安全性判断：合成应答是本地生成、重查零外部往返，所以 5 与 60 的差别只在
/// 「本地查询次数」与「陈旧信念时长」之间取舍，不影响碰撞发生率本身（那由内核侧地址复用决定）。
/// **上界仍远低于内核默认的 600s**，方向没变。
const FAKEIP_REWRITE_TTL: u32 = 60;

/// 域名 → `[精确域名, ".域名"(后缀匹配)]`：同时覆盖 exact 与 subdomain（sing-box domain_suffix 语义）。
/// 上游 `withDotPrefix`。
fn with_dot_prefix(d: &str) -> Vec<String> {
    vec![d.to_string(), format!(".{d}")]
}

/// P4b 内部预留 tag：tailscale 按名解析 DNS server 的固定 tag。不与节点 tag 撞车。
/// 上游 `TS_NAME_DNS_TAG`。
const TS_NAME_DNS_TAG: &str = "dns-tailscale";

/// 内网 / 反向解析后缀（非 .local 组播）：内网域 .lan / .home.arpa + 反查 .arpa。
/// 上游 `INTERNAL_DNS_SUFFIXES`。
const INTERNAL_DNS_SUFFIXES: &[&str] = &[".arpa", ".lan", ".home.arpa"];

// ───────────────────────── 依赖注入 ─────────────────────────

/// buildDnsConfig 依赖注入（实例态 / 路径 / FS 检查 / 平台）。
///
/// 对齐 上游 `buildDnsConfig` 入参中所有非 config、非 idToTagMap 的运行时态：
/// - `lan_resolver_for_dns`：lanResolverForDns 值（决定是否建 dns-lan）。
/// - `pending_endpoints`：本轮实际发射的 endpoint（tailnet 按名解析 gate 用）。
/// - `log`：日志回调（注入 ProxyManager.logToManager）。
/// - `selected_server_tag`：当前选中节点 tag（dns-remote detour / dns-probe-exit-proxy detour）。
/// - `race_server_port`：本地 race DNS server 端口（>0 = race 就绪）。
/// - `probe_pool_ports`：主核测速探测池端口数（K 个 dns-probe-exit-k）。
/// - `probe_proxy_port`：出口伴测 / 出口 IP 探测端口（>0 = 就绪）。
/// - `platform`：运行平台（process.platform；neighbor match + win32 死环判断）。
/// - `custom_rules_dir`：getCustomRulesDir 路径（对拍固定假路径）。
/// - `runtime_rules_dir`：getRuntimeRulesDir 路径（对拍固定假路径）。
/// - `is_valid_srs_fn`：FS 存在性 + SRS 魔数检查（对拍 fixture 注入固定值；兼 .dns.json existsSync）。
#[derive(Debug, Clone)]
pub struct DnsConfigDeps {
    pub lan_resolver_for_dns: Option<String>,
    pub pending_endpoints: Vec<Endpoint>,
    pub log: fn(LogLevel, &str),
    pub selected_server_tag: String,
    pub race_server_port: u16,
    pub probe_pool_ports: Vec<u16>,
    pub probe_proxy_port: Option<u16>,
    pub platform: String,
    pub custom_rules_dir: String,
    pub runtime_rules_dir: String,
    /// 内置 geo 二进制 `.srs` 的存在性 + SRS 魔数检查（builtin rule_set fail-closed）。
    pub is_valid_srs_fn: fn(&str) -> bool,
    /// L3 ext 外化 `<base>.dns.json`（JSON source）存在性检查（`existsSync` 等价）。
    /// **绝不复用 `is_valid_srs_fn`**：JSON 无 SRS 魔数，复用会使「落盘后 DNS ext 分支」100% 不可达。
    /// 生产默认注入 [`crate::builder::custom_rule_files::ext_rule_file_exists`]；对拍 fixture 注入固定值。
    pub exists_fn: fn(&str) -> bool,
}

impl DnsConfigDeps {
    fn log_warn(&self, msg: &str) {
        (self.log)(LogLevel::Warn, msg);
    }
    fn log_info(&self, msg: &str) {
        (self.log)(LogLevel::Info, msg);
    }
}

/// 默认国内 ParsedDnsServer（上游 `DEFAULT_DOMESTIC`）。doh.pub DoH。
fn default_domestic() -> ParsedDnsServer {
    ParsedDnsServer {
        server_type: DnsServerType::Https,
        server: "doh.pub".to_string(),
        port: 443,
        path: Some("/dns-query".to_string()),
        is_domain: true,
    }
}

/// 默认境外 ParsedDnsServer（上游 `DEFAULT_FOREIGN`）。dns.google DoH。
fn default_foreign() -> ParsedDnsServer {
    ParsedDnsServer {
        server_type: DnsServerType::Https,
        server: "dns.google".to_string(),
        port: 443,
        path: Some("/dns-query".to_string()),
        is_domain: true,
    }
}

/// 取 proxy_mode 小写字符串。上游 `(config.proxyMode || 'smart').toLowerCase()`。
/// ProxyMode 默认 Smart，故恒非空。
fn proxy_mode_str(config: &UserConfig) -> &'static str {
    match config.proxy_mode {
        crate::user_config::ProxyMode::Smart => "smart",
        crate::user_config::ProxyMode::Global => "global",
        crate::user_config::ProxyMode::Direct => "direct",
    }
}

/// 由解析结果构造用户 DNS server（上游 `buildUserDns`）。
/// 域名型 DNS 需 domain_resolver 引导解析；DoH 带 path；remote 走代理 detour。
fn build_user_dns(tag: &str, p: &ParsedDnsServer, detour: Option<&str>) -> DnsServer {
    DnsServer {
        tag: tag.to_string(),
        type_field: Some(dns_type_str(p.server_type).to_string()),
        server: Some(p.server.clone()),
        server_port: Some(p.port),
        // 仅 https 带 path（与 上游 `p.type === 'https' ? { path }` 一致）。
        path: if matches!(p.server_type, DnsServerType::Https) {
            Some(p.path.clone().unwrap_or_else(|| "/dns-query".to_string()))
        } else {
            None
        },
        // 域名型 → dns-bootstrap 引导（isDomain）。
        domain_resolver: if p.is_domain {
            Some("dns-bootstrap".to_string())
        } else {
            None
        },
        detour: detour.map(|s| s.to_string()),
        endpoint: None,
        accept_search_domain: None,
        accept_default_resolvers: None,
        neighbor_domain: None,
        address: None,
        address_resolver: None,
        inet4_range: None,
        inet6_range: None,
    }
}

/// DnsServerType → sing-box type 字符串。
fn dns_type_str(t: DnsServerType) -> &'static str {
    match t {
        DnsServerType::Https => "https",
        DnsServerType::Tls => "tls",
        DnsServerType::Udp => "udp",
    }
}

// ───────────────────────── 主函数 ─────────────────────────

/// sing-box DNS 配置生成。上游 `buildDnsConfig`（659 行）1:1 移植。
///
/// 纯函数 + 依赖注入：所有实例态（lanResolverForDns/pendingEndpoints/路径/FS）经 `deps` 传入。
/// 输出 DnsConfig（servers + rules + final + strategy + 可选 reverse_mapping/optimistic/timeout）。
pub fn build_dns_config(
    config: &UserConfig,
    id_to_tag_map: &BTreeMap<String, String>,
    deps: &DnsConfigDeps,
) -> DnsConfig {
    let proxy_mode = proxy_mode_str(config);

    // 获取用户 DNS 配置，不存在则使用默认值（上游 `userDnsConfig = config.dnsConfig || {...}`）。
    // 仅读用到的字段；缺省值与 Polaris 内联对象一致。
    let (domestic_dns, foreign_dns, optimistic_cache, dns_timeout_ms, resolve_node_domains_ahead) =
        match &config.dns_config {
            Some(d) => (
                d.domestic_dns.as_deref(),
                d.foreign_dns.as_deref(),
                d.optimistic_cache,
                d.dns_timeout_ms,
                d.resolve_node_domains_ahead,
            ),
            None => (None, None, None, None, None),
        };

    // 决定是否开启 FakeIP（单一真值：custom-rule-files.usesFakeIp）。
    // enable_fake_ip 字段缺省 = true。
    let enable_fake_ip_field = config.dns_config.as_ref().and_then(|d| d.enable_fake_ip);
    let enable_fake_ip = uses_fake_ip(enable_fake_ip_field);

    // 用户自定义 DNS：解析 domesticDns/foreignDns（https DoH / tls DoT / udp / 裸 IP），
    // 非法或空回退默认并告警。
    let domestic = parse_dns_server_spec(domestic_dns).unwrap_or_else(default_domestic);
    let foreign = parse_dns_server_spec(foreign_dns).unwrap_or_else(default_foreign);
    if domestic_dns.is_some() && parse_dns_server_spec(domestic_dns).is_none() {
        deps.log_warn(&format!(
            "国内 DNS 无法解析，已回退默认 doh.pub: {}",
            domestic_dns.unwrap_or("")
        ));
    }
    if foreign_dns.is_some() && parse_dns_server_spec(foreign_dns).is_none() {
        deps.log_warn(&format!(
            "境外 DNS 无法解析，已回退默认 dns.google: {}",
            foreign_dns.unwrap_or("")
        ));
    }

    // sing-box 1.13+ 新格式：每个 server 必须有显式 type 字段。
    //
    // 关键架构说明（Polaris 注释保留意图）：
    // - 在 TUN 下，Windows 的系统 DNS (svchost) 发出的解析请求会被 TUN 劫持。如果该系统 DNS 配置为公共 IP，
    //   此时 type: 'local' 就会进入死循环。
    // - 引入 DoH IP Bootstrap (dns-bootstrap)：向 223.5.5.5:443 直接发 DoH 包，免疫 UDP 53 限速/劫持/投毒。
    let mut dns_servers: Vec<DnsServer> = vec![
        // 引导解析（DoH over IP）：关键路径的解析器。server 已是 IP，无需 domain_resolver。
        DnsServer {
            tag: "dns-bootstrap".into(),
            type_field: Some("https".into()),
            server: Some("223.5.5.5".into()),
            server_port: Some(443),
            path: Some("/dns-query".into()),
            domain_resolver: None,
            detour: None,
            endpoint: None,
            accept_search_domain: None,
            accept_default_resolvers: None,
            neighbor_domain: None,
            address: None,
            address_resolver: None,
            inet4_range: None,
            inet6_range: None,
        },
        // 节点域名解析器可选档（#57，DNSPod IP-DoH）：1.12.12.12:443。恒加进 servers（未被引用零成本）。
        DnsServer {
            tag: "dns-node".into(),
            type_field: Some("https".into()),
            server: Some("1.12.12.12".into()),
            server_port: Some(443),
            path: Some("/dns-query".into()),
            domain_resolver: None,
            detour: None,
            endpoint: None,
            accept_search_domain: None,
            accept_default_resolvers: None,
            neighbor_domain: None,
            address: None,
            address_resolver: None,
            inet4_range: None,
            inet6_range: None,
        },
        // 兼容性和兜底的系统 DNS。
        DnsServer {
            tag: "dns-local".into(),
            type_field: Some("local".into()),
            server: None,
            server_port: None,
            path: None,
            domain_resolver: None,
            detour: None,
            endpoint: None,
            accept_search_domain: None,
            accept_default_resolvers: None,
            neighbor_domain: None,
            address: None,
            address_resolver: None,
            inet4_range: None,
            inet6_range: None,
        },
        // 国内直连 DNS（用户可自定义，默认 doh.pub DoH）。
        build_user_dns("dns-domestic", &domestic, None),
        // 远程代理 DNS（默认 dns.google DoH）。必须走代理 detour，否则境内直接发起会被 GFW 拦截/污染。
        build_user_dns("dns-remote", &foreign, Some(&deps.selected_server_tag)),
    ];

    // §15 主核测速探测池 DNS server：K 个 dns-probe-exit-k（223.5.5.5 over DoH:443，detour=probe-selector-k）。
    for (k, _port) in deps.probe_pool_ports.iter().enumerate() {
        dns_servers.push(DnsServer {
            tag: format!("dns-probe-exit-{k}"),
            type_field: Some("https".into()),
            server: Some("223.5.5.5".into()),
            server_port: Some(443),
            path: Some("/dns-query".into()),
            domain_resolver: None,
            detour: Some(format!("probe-selector-{k}")),
            endpoint: None,
            accept_search_domain: None,
            accept_default_resolvers: None,
            neighbor_domain: None,
            address: None,
            address_resolver: None,
            inet4_range: None,
            inet6_range: None,
        });
    }

    // V37 出口伴测 / 出口 IP 探测专用出口 DNS（§17.5）：223.5.5.5 over DoH:443，detour=selectedServerTag。
    // 仅 probe-proxy-in 就绪时注入（probeProxyPort > 0）。
    if deps.probe_proxy_port.unwrap_or(0) > 0 {
        dns_servers.push(DnsServer {
            tag: "dns-probe-exit-proxy".into(),
            type_field: Some("https".into()),
            server: Some("223.5.5.5".into()),
            server_port: Some(443),
            path: Some("/dns-query".into()),
            domain_resolver: None,
            detour: Some(deps.selected_server_tag.clone()),
            endpoint: None,
            accept_search_domain: None,
            accept_default_resolvers: None,
            neighbor_domain: None,
            address: None,
            address_resolver: None,
            inet4_range: None,
            inet6_range: None,
        });
    }

    // issue #147：race on + server 就绪 → 本地 race DNS server（dns-node-race）。
    // raceServerPort=0（off/未就绪/snapshot）不生成，getNodeResolverTag 同步走单上游、不悬空引用。
    if deps.race_server_port > 0 && resolve_node_domains_ahead != Some(false) {
        dns_servers.push(DnsServer {
            tag: DNS_NODE_RACE_TAG.into(),
            type_field: Some("udp".into()),
            server: Some("127.0.0.1".into()),
            server_port: Some(deps.race_server_port),
            path: None,
            domain_resolver: None,
            detour: None,
            endpoint: None,
            accept_search_domain: None,
            accept_default_resolvers: None,
            neighbor_domain: None,
            address: None,
            address_resolver: None,
            inet4_range: None,
            inet6_range: None,
        });
    }

    // P6 局域网网关：local DNS server 邻居解析（sing-box 1.14 neighbor_domain）。
    // 仅当用户配置了 neighborDomains 且平台支持时附到 dns-local。
    if is_source_device_match_supported(&deps.platform) {
        let neighbor_domains: Vec<String> = dedupe(
            config
                .tun_config
                .as_ref()
                .and_then(|t| t.neighbor_domains.as_ref())
                .map(|v| v.as_slice())
                .unwrap_or(&[])
                .iter()
                .filter_map(|d| normalize_neighbor_domain(Some(d)))
                .collect::<Vec<_>>(),
        );
        if !neighbor_domains.is_empty() {
            if let Some(local) = dns_servers.iter_mut().find(|s| s.tag == "dns-local") {
                local.neighbor_domain = Some(neighbor_domains.clone());
                deps.log_info(&format!(
                    "local DNS 邻居解析后缀: {}",
                    neighbor_domains.join(", ")
                ));
            }
        }
    }

    if enable_fake_ip {
        // §B：开 IPv6 才给 FakeIP 分配 v6 段；关 IPv6 则不分配。
        let enable_ipv6 = config.enable_ipv6.unwrap_or(false);
        dns_servers.push(DnsServer {
            tag: "fakeip".into(),
            type_field: Some("fakeip".into()),
            server: None,
            server_port: None,
            path: None,
            domain_resolver: None,
            detour: None,
            endpoint: None,
            accept_search_domain: None,
            accept_default_resolvers: None,
            neighbor_domain: None,
            address: None,
            address_resolver: None,
            inet4_range: Some(FAKEIP_INET4_RANGE.to_string()),
            inet6_range: if enable_ipv6 {
                Some(FAKEIP_INET6_RANGE.to_string())
            } else {
                None
            },
        });
    }

    // 方案B + Q1：dns-lan（type:dhcp）——读到内网 LAN 解析器(私网 IPv4)时建，把内网域名重定向到它解析。
    let lan_resolver = deps.lan_resolver_for_dns.as_deref();
    if lan_resolver.is_some() {
        dns_servers.push(DnsServer {
            tag: "dns-lan".into(),
            type_field: Some("dhcp".into()),
            server: None,
            server_port: None,
            path: None,
            domain_resolver: None,
            detour: None,
            endpoint: None,
            accept_search_domain: None,
            accept_default_resolvers: None,
            neighbor_domain: None,
            address: None,
            address_resolver: None,
            inet4_range: None,
            inet6_range: None,
        });
    }

    // Q1 死循环防护（仅 Windows）：Win TUN strict_route(WFP) 把所有 :53 逼进 TUN；type:local 经 svchost → 进 TUN → ∞。
    // winLoopRisk 解耦（T2）：死环源于 Win strict_route(WFP) + type:local 本身 → 改为「Win + TUN」恒判。
    let win_loop_risk =
        deps.platform == "win32" && matches!(config.proxy_mode_type, ProxyModeType::Tun);
    // 内网/反查/captive 解析器：优先 dns-lan；无则 Win 退 dns-domestic，非 Win 退 dns-local。
    let internal_resolver_tag = if lan_resolver.is_some() {
        "dns-lan"
    } else if win_loop_risk {
        "dns-domestic"
    } else {
        "dns-local"
    };
    // 银行/U盾公网域名解析器：仅 Win 改 dns-domestic 绕死环；其余 dns-local。
    let bank_resolver_tag = if win_loop_risk {
        "dns-domestic"
    } else {
        "dns-local"
    };

    let enable_ipv6 = config.enable_ipv6.unwrap_or(false);
    let mut dns_config = DnsConfig {
        servers: dns_servers,
        rules: None,
        // 默认使用国内 DNS 解析。
        final_server: Some("dns-domestic".to_string()),
        // §B strategy 随 enableIPv6 收敛：开→prefer_ipv4 / 关→ipv4_only。
        strategy: Some(
            if enable_ipv6 {
                "prefer_ipv4"
            } else {
                "ipv4_only"
            }
            .to_string(),
        ),
        fakeip: None,
        // 关 FakeIP：补 reverse_mapping；开 FakeIP 时不加。
        reverse_mapping: if enable_fake_ip { None } else { Some(true) },
        // P2b 乐观 DNS 缓存：仅开关 true 时下发。
        optimistic: if optimistic_cache == Some(true) {
            Some(true)
        } else {
            None
        },
        // P2c DNS 查询超时（Go duration 字符串）：仅有效正毫秒时下发 "<n>ms"。
        // Number.isFinite + > 0 防御性校验，避免 emit "0ms"/NaN。
        timeout: dns_timeout_ms
            .filter(|n| n.is_finite() && *n > 0.0)
            .map(|n| format!("{}ms", n.round() as i64)),
    };
    let mut dns_rules: Vec<DnsRule> = vec![];

    // ── rule1：代理服务器域名必须用真实 DNS 解析（避免 FakeIP 劫持死循环）。
    // #57 全量化：遍历【全部】节点的 address + tlsSettings.serverName（过滤 IP 字面量）。
    let node_domains_set: std::collections::BTreeSet<String> = config
        .servers
        .iter()
        .flat_map(|s| {
            let mut ds: Vec<String> = Vec::new();
            if !s.address.is_empty() {
                ds.push(s.address.clone());
            }
            if let Some(sn) = s.tls_settings.as_ref().and_then(|t| t.server_name.as_ref()) {
                ds.push(sn.clone());
            }
            ds
        })
        .collect();
    let node_domains: Vec<String> = node_domains_set
        .into_iter()
        .filter(|d| !d.is_empty() && !is_ipv4_host(d) && !is_ipv6_host(d))
        .collect();
    if !node_domains.is_empty() {
        // 观测：超大订阅下 rule1 域名规模。
        deps.log_info(&format!(
            "DNS rule1 节点域名规则: {} 个域名",
            node_domains.len()
        ));
        let node_resolver_single = config
            .dns_config
            .as_ref()
            .and_then(|d| d.node_resolver_single.as_deref());
        let node_domain_resolver = config
            .dns_config
            .as_ref()
            .and_then(|d| d.node_domain_resolver.as_deref());
        let proxy_mode_type_str = match config.proxy_mode_type {
            ProxyModeType::Tun => "tun",
            ProxyModeType::SystemProxy => "systemProxy",
            ProxyModeType::Manual => "manual",
        };
        let node_suffix: Vec<String> = node_domains
            .iter()
            .flat_map(|d| vec![d.clone(), format!(".{d}")])
            .collect();
        dns_rules.push(DnsRule {
            rule_set: None,
            query_type: None,
            domain: Some(node_domains.clone()),
            domain_suffix: Some(node_suffix),
            domain_keyword: None,
            source_mac_address: None,
            source_hostname: None,
            preferred_by: None,
            type_field: None,
            action: None,
            server: Some(get_node_resolver_tag(
                resolve_node_domains_ahead,
                node_resolver_single,
                node_domain_resolver,
                proxy_mode_type_str,
                NodeResolverCtx::Rule,
            )),
            inbound: None,
            disable_cache: None,
            rewrite_ttl: None,
        });
    }

    // ── 引导解析器域名：确保基础 DNS 服务（含用户自定义的 DoH 域名）走 dns-bootstrap。
    let mut bootstrap_domains: Vec<String> = vec![
        "doh.pub".into(),
        "dns.google".into(),
        "cloudflare-dns.com".into(),
        "one.one.one.one".into(),
    ];
    if domestic.is_domain {
        bootstrap_domains.push(domestic.server.clone());
    }
    if foreign.is_domain {
        bootstrap_domains.push(foreign.server.clone());
    }
    // 根治 §3.6：DoH server 自身域名解析统一用 dns-bootstrap（IP-DoH 抗 UDP53 劫持）。
    dns_rules.push(DnsRule {
        rule_set: None,
        query_type: None,
        domain: Some(dedupe(bootstrap_domains)),
        domain_suffix: None,
        domain_keyword: None,
        source_mac_address: None,
        source_hostname: None,
        preferred_by: None,
        type_field: None,
        action: None,
        server: Some("dns-bootstrap".into()),
        inbound: None,
        disable_cache: None,
        rewrite_ttl: None,
    });

    // ── mDNS / 本地反向解析 / 银行。
    // 三者全为 dns-local → 合并回原单条规则（byte-diff 零变化）；否则拆三条。
    if internal_resolver_tag == "dns-local" && bank_resolver_tag == "dns-local" {
        let mut suffixes: Vec<String> = vec![".local".into()];
        suffixes.extend(INTERNAL_DNS_SUFFIXES.iter().map(|s| s.to_string()));
        suffixes.extend(
            DOMESTIC_BANK_AND_STOCK_DOMAINS
                .iter()
                .map(|s| s.to_string()),
        );
        dns_rules.push(DnsRule {
            rule_set: None,
            query_type: None,
            domain: None,
            domain_suffix: Some(suffixes),
            domain_keyword: None,
            source_mac_address: None,
            source_hostname: None,
            preferred_by: None,
            type_field: None,
            action: None,
            server: Some("dns-local".into()),
            inbound: None,
            disable_cache: None,
            rewrite_ttl: None,
        });
    } else {
        dns_rules.push(DnsRule {
            rule_set: None,
            query_type: None,
            domain: None,
            domain_suffix: Some(vec![".local".into()]),
            domain_keyword: None,
            source_mac_address: None,
            source_hostname: None,
            preferred_by: None,
            type_field: None,
            action: None,
            server: Some("dns-local".into()),
            inbound: None,
            disable_cache: None,
            rewrite_ttl: None,
        });
        dns_rules.push(DnsRule {
            rule_set: None,
            query_type: None,
            domain: None,
            domain_suffix: Some(
                DOMESTIC_BANK_AND_STOCK_DOMAINS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            domain_keyword: None,
            source_mac_address: None,
            source_hostname: None,
            preferred_by: None,
            type_field: None,
            action: None,
            server: Some(bank_resolver_tag.into()),
            inbound: None,
            disable_cache: None,
            rewrite_ttl: None,
        });
        dns_rules.push(DnsRule {
            rule_set: None,
            query_type: None,
            domain: None,
            domain_suffix: Some(
                INTERNAL_DNS_SUFFIXES
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            domain_keyword: None,
            source_mac_address: None,
            source_hostname: None,
            preferred_by: None,
            type_field: None,
            action: None,
            server: Some(internal_resolver_tag.into()),
            inbound: None,
            disable_cache: None,
            rewrite_ttl: None,
        });
    }

    // ── fake-ip-filter 默认清单（仅 FakeIP 开启且有义；config.fakeIpFilter === false 可关）。
    if enable_fake_ip && config.fake_ip_filter != Some(false) {
        match &config.fake_ip_filter_list {
            None => {
                // 默认（未编辑）：与历史逐字节一致。captive→internalResolverTag；ntp/stun→dns-domestic。
                dns_rules.push(DnsRule {
                    rule_set: None,
                    query_type: None,
                    domain: Some(
                        FAKEIP_FILTER_CAPTIVE_DOMAINS
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                    ),
                    domain_suffix: None,
                    domain_keyword: None,
                    source_mac_address: None,
                    source_hostname: None,
                    preferred_by: None,
                    type_field: None,
                    action: None,
                    server: Some(internal_resolver_tag.into()),
                    inbound: None,
                    disable_cache: None,
                    rewrite_ttl: None,
                });
                dns_rules.push(DnsRule {
                    rule_set: None,
                    query_type: None,
                    domain: None,
                    domain_suffix: Some(
                        FAKEIP_FILTER_NTP_SUFFIXES
                            .iter()
                            .flat_map(|d| with_dot_prefix(d))
                            .collect(),
                    ),
                    // FAKEIP_FILTER_NTP_STUN_KEYWORDS 是裸子串匹配（domain_keyword）。
                    domain_keyword: Some(
                        FAKEIP_FILTER_NTP_STUN_KEYWORDS
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                    ),
                    source_mac_address: None,
                    source_hostname: None,
                    preferred_by: None,
                    type_field: None,
                    action: None,
                    server: Some("dns-domestic".into()),
                    inbound: None,
                    disable_cache: None,
                    rewrite_ttl: None,
                });
            }
            Some(fake_ip_filter_list) => {
                // 用户编辑过的清单：内置 captive 域名仍→internalResolverTag；其余→dns-domestic。
                // ntp/stun 关键字（裸子串、非域名清单项）始终兜底保留。
                let captive_set: std::collections::HashSet<&str> =
                    FAKEIP_FILTER_CAPTIVE_DOMAINS.iter().copied().collect();
                let captive: Vec<String> = fake_ip_filter_list
                    .iter()
                    .filter(|d| captive_set.contains(d.as_str()))
                    .cloned()
                    .collect();
                let others: Vec<String> = fake_ip_filter_list
                    .iter()
                    .filter(|d| !captive_set.contains(d.as_str()))
                    .cloned()
                    .collect();
                if !captive.is_empty() {
                    dns_rules.push(DnsRule {
                        rule_set: None,
                        query_type: None,
                        domain: Some(captive),
                        domain_suffix: None,
                        domain_keyword: None,
                        source_mac_address: None,
                        source_hostname: None,
                        preferred_by: None,
                        type_field: None,
                        action: None,
                        server: Some(internal_resolver_tag.into()),
                        inbound: None,
                        disable_cache: None,
                        rewrite_ttl: None,
                    });
                }
                if !others.is_empty() {
                    dns_rules.push(DnsRule {
                        rule_set: None,
                        query_type: None,
                        domain: None,
                        domain_suffix: Some(
                            others.iter().flat_map(|d| with_dot_prefix(d)).collect(),
                        ),
                        domain_keyword: None,
                        source_mac_address: None,
                        source_hostname: None,
                        preferred_by: None,
                        type_field: None,
                        action: None,
                        server: Some("dns-domestic".into()),
                        inbound: None,
                        disable_cache: None,
                        rewrite_ttl: None,
                    });
                }
                dns_rules.push(DnsRule {
                    rule_set: None,
                    query_type: None,
                    domain: None,
                    domain_suffix: None,
                    domain_keyword: Some(
                        FAKEIP_FILTER_NTP_STUN_KEYWORDS
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                    ),
                    source_mac_address: None,
                    source_hostname: None,
                    preferred_by: None,
                    type_field: None,
                    action: None,
                    server: Some("dns-domestic".into()),
                    inbound: None,
                    disable_cache: None,
                    rewrite_ttl: None,
                });
            }
        }
    }

    // ── 自定义规则中的 bypassFakeIP（仅 domain/domainSuffix/domainKeyword 三类域名规则有效）。
    // 用户路由仅 smart 生效（effectiveCustomRules）。可外化规则 → 引用其 <base>-dns rule_set。
    let dns_custom_rules = effective_custom_rules(proxy_mode, &config.custom_rules);
    if !dns_custom_rules.is_empty() && enable_fake_ip {
        /// 一个解析器去向下攒到的 bypass 域名（三种匹配形态 + 外化 rule_set tag）。
        #[derive(Default)]
        struct BypassBucket {
            domains: Vec<String>,  // type 'domain'
            suffixes: Vec<String>, // type 'domainSuffix'
            keywords: Vec<String>, // type 'domainKeyword'
            dns_tags: Vec<String>, // 外化规则的 <base>-dns rule_set
        }

        // 按**规则自己的去向**分桶：走代理的域名必须用境外解析器拿真实 IP。
        //
        // # 为什么不能像原来那样一律送 dns-bootstrap
        //
        // `bypassFakeIP` 的语义是「这个域名别用假 IP、给我真实 IP」，用户会打开它的域名，恰恰是
        // 那些**必须绕开境内 DNS 才拿得到正确解析**的域名（否则他也不需要 bypass）。而
        // `dns-bootstrap` = 223.5.5.5（见本文件 DoH IP Bootstrap 那节），是**境内**解析器。
        // 于是旧写法把最需要境外解析的域名精准地送进了最可能被污染的那条路 —— 逃生口朝里开。
        //
        // 这条缺陷从 上游 逐字继承（`singbox-dns-builder.ts` 的 bypass 分支同样写死
        // `dns-bootstrap`），由 上游 issue #347 的取证暴露：那位用户唯一可用的规避手段只剩
        // 「全局关 FakeIP」，因为按域名的细粒度逃生口在实现上是坏的。
        //
        // # 去向 → 解析器
        //
        // - `proxy` → `dns-remote`（detour 走当前出口，隧道内解析）
        // - `direct` / `block` → `dns-bootstrap`（保持原样：直连的域名本就该境内解析）
        //
        // 同一域名被两条去向不同的规则同时 bypass 时，**代理桶先 emit** ⇒ sing-box 首命中取
        // 境外解析器。这是刻意的 fail-open：对一个用户明确要求「别给假 IP」的域名，拿到可用的
        // 境外解析结果，比拿到一个可能被污染的境内结果更接近他的意图。
        let mut via_proxy = BypassBucket::default();
        let mut via_direct = BypassBucket::default();
        // direct 不外化（route 侧 generateCustomRules 不执行）；此处 dnsCustomRules 已 smart-only。
        let externalize = proxy_mode != "direct";
        for rule in &dns_custom_rules {
            if !rule.enabled || rule.bypass_fakeip != Some(true) {
                continue;
            }
            let bucket = if rule.action == RuleAction::Proxy {
                &mut via_proxy
            } else {
                &mut via_direct
            };
            if externalize {
                let plan = plan_custom_rule(rule);
                // 上游 `if (plan.kind !== 'inline' && plan.dnsRules)`：可外化且 dnsRules 存在。
                let has_dns_rules = match &plan {
                    RulePlan::Ext { dns_rules, .. } | RulePlan::ExtSkip { dns_rules, .. } => {
                        dns_rules.is_some()
                    }
                    RulePlan::Inline => false,
                };
                if has_dns_rules {
                    let base = custom_rule_file_base(&rule.id);
                    let dns_path = format!("{}/{base}.dns.json", deps.custom_rules_dir);
                    // ext JSON source：真存在性检查（existsSync 等价），非 SRS 魔数。
                    if (deps.exists_fn)(&dns_path) {
                        let dns_tag = format!("{base}-dns");
                        if !bucket.dns_tags.contains(&dns_tag) {
                            bucket.dns_tags.push(dns_tag);
                        }
                        continue; // 已外化：域名值走文件，不在此提取。
                    }
                }
            }
            // inline / direct / 文件缺失降级：取所有 domain/domainSuffix/domainKeyword 条件的值并集。
            for cond in rule_conditions(rule) {
                let vals: Vec<String> = cond
                    .values
                    .iter()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect();
                if vals.is_empty() {
                    continue;
                }
                match cond.type_field {
                    RuleType::Domain => bucket.domains.extend(vals),
                    RuleType::DomainSuffix => {
                        bucket.suffixes.extend(vals.into_iter().map(|d| {
                            if let Some(rest) = d.strip_prefix("*.") {
                                rest.to_string()
                            } else {
                                d
                            }
                        }));
                    }
                    RuleType::DomainKeyword => bucket.keywords.extend(vals),
                    _ => {}
                }
            }
        }

        /// 把一个桶 emit 成至多两条 DNS 规则（inline 域名一条、外化 rule_set 一条），都指向 `server`。
        ///
        /// 桶为空则一条都不出 —— 这保证「没有该去向的 bypass 规则」时生成产物与改动前逐字节相同。
        fn push_bypass_bucket(out: &mut Vec<DnsRule>, bucket: BypassBucket, server: &str) {
            if !bucket.domains.is_empty()
                || !bucket.suffixes.is_empty()
                || !bucket.keywords.is_empty()
            {
                let suffix_flat: Vec<String> = bucket
                    .suffixes
                    .iter()
                    .flat_map(|d| with_dot_prefix(d))
                    .collect();
                out.push(DnsRule {
                    rule_set: None,
                    query_type: None,
                    domain: if bucket.domains.is_empty() {
                        None
                    } else {
                        Some(bucket.domains)
                    },
                    domain_suffix: if suffix_flat.is_empty() {
                        None
                    } else {
                        Some(suffix_flat)
                    },
                    domain_keyword: if bucket.keywords.is_empty() {
                        None
                    } else {
                        Some(bucket.keywords)
                    },
                    source_mac_address: None,
                    source_hostname: None,
                    preferred_by: None,
                    type_field: None,
                    action: None,
                    server: Some(server.to_string()),
                    inbound: None,
                    disable_cache: None,
                    rewrite_ttl: None,
                });
            }
            // 外化规则的域名匹配走 rule_set 引用（与 inline 合并规则相邻，OR 语义等价）。
            // 上游 `rule_set: dnsTags`（单 tag 出裸 string，多 tag 出数组 → OneOrMany）。
            if !bucket.dns_tags.is_empty() {
                let mut tags = bucket.dns_tags;
                out.push(DnsRule {
                    rule_set: Some(if tags.len() == 1 {
                        OneOrMany::One(tags.pop().unwrap())
                    } else {
                        OneOrMany::Many(tags)
                    }),
                    query_type: None,
                    domain: None,
                    domain_suffix: None,
                    domain_keyword: None,
                    source_mac_address: None,
                    source_hostname: None,
                    preferred_by: None,
                    type_field: None,
                    action: None,
                    server: Some(server.to_string()),
                    inbound: None,
                    disable_cache: None,
                    rewrite_ttl: None,
                });
            }
        }

        // 代理桶先 emit（首命中优先，理由见上）。
        push_bypass_bucket(&mut dns_rules, via_proxy, "dns-remote");
        push_bypass_bucket(&mut dns_rules, via_direct, "dns-bootstrap");
    }

    // ── 智能分流/全局代理模式下的 DNS 规则。
    if proxy_mode == "smart" || proxy_mode == "global" {
        if enable_fake_ip {
            // [Clash-style 全局 FakeIP]：所有 A/AAAA 无脑走 FakeIP。
            dns_rules.push(DnsRule {
                rule_set: None,
                query_type: Some(vec!["A".into(), "AAAA".into()]),
                domain: None,
                domain_suffix: None,
                domain_keyword: None,
                source_mac_address: None,
                source_hostname: None,
                preferred_by: None,
                type_field: None,
                action: None,
                server: Some("fakeip".into()),
                inbound: None,
                disable_cache: None,
                rewrite_ttl: Some(FAKEIP_REWRITE_TTL),
            });
            // R1（§14.4）：FakeIP 分支补回境内/境外分类的「影子规则」（仅作用于 endpoint dial-time 解析）。
            //
            // # 它们不是死规则 —— 源码级链条（2026-08-10 核对）
            //
            // 首次核于 v1.14.0-beta.7（`3001f038`），抬核到 **v1.14.0-beta.12**（`426c5faf`）后逐条复核：
            // `dns/router.go` 与 `common/dialer/dialer.go` 在两版之间**逐字未变**（`router.go` 只在
            // `ResetNetwork` 少了一行 `ClearCache()`，位置远在下方，本节引用的行号全部不受影响），
            // `protocol/wireguard/endpoint.go` 未变；只有 `protocol/tailscale/endpoint.go` 有位移，
            // 下面已按 beta.12 更新。
            //
            // 这两条排在上面那条 `{query_type:[A,AAAA], server:"fakeip"}` 兜底**之后**，且 matcher 更宽
            // 或相同，看上去恒不可达。实际有第三条路径走到它们，判据是 `dns/router.go:311-314`：
            //
            // ```go
            // isFakeIP := transport.Type() == C.DNSTypeFakeIP
            // if isFakeIP && !allowFakeIP { continue }   // ← continue，不是 return
            // ```
            //
            // fakeip 不可用时**跳过该规则继续往下找**，这两条正是接它的。三条路径分别落到不同终点：
            //
            // | 路径 | `options.Transport` | `allowFakeIP` | 结果 |
            // |---|---|---|---|
            // | 客户端 DNS 查询（`exchangeLegacy`） | nil | **true** | fakeip 兜底命中，本两条不可达 |
            // | dialer 的 dial-time 解析 | **非 nil** | — | `Lookup:1249` 短路，规则链一次都不过 |
            // | **endpoint 内部解析** | nil | **false** | fakeip 被跳过 ⇒ **本两条命中** |
            //
            // 第二行之所以恒非 nil：`common/dialer/dialer.go:77-116` 里 per-outbound 的
            // `domain_resolver.server` 与 `route.default_domain_resolver` 任一非空都会设
            // `dnsQueryOptions.Transport`，而本仓两者都下发（#335 起逐载体给 `domain_resolver`）。
            //
            // 第三行的具体调用点：`protocol/wireguard/endpoint.go:270`（`DialContext`）与 `:287`
            // （`ListenPacketWithDestination`）、`protocol/tailscale/endpoint.go:737`/`:826`，四处都传
            // **裸 `adapter.DNSQueryOptions{}`**。`lookupWithRulesType:1018` 与 legacy 的
            // `matchDNS(..., false, ...):1267` 两条分支都传 `allowFakeIP=false`，行为一致。
            //
            // **FakeIP 恰恰是让这条路径被走到的原因**：开 FakeIP 时 `route/route.go` 的反查把
            // `metadata.Destination` 改回域名，endpoint 收到的就是域名 ⇒ `destination.IsDomain()` 为真
            // ⇒ 触发上面那次 `Lookup`。影子规则与 fakeip 是配套的，不是冗余。
            //
            // 删掉它们的后果：`matchDNS` 走完全部规则后落到 `:354` 的 `r.transport.Default()` = `dns.final`
            // ⇒ WG / Tailscale endpoint 内的域名全部挤到同一个解析器，境内/境外分流在该路径上整个丢失。
            let fakeip_region = effective_region_routing(config.region_routing.as_ref());
            let fakeip_runtime_dir = &deps.runtime_rules_dir;
            // fail-closed：引用 region geo rule_set 前镜像 else 分支——本地 .srs 缺失即跳过。
            let fakeip_local_geo = local_geo_tags(&fakeip_region.region, fakeip_runtime_dir, deps);
            let fakeip_local_resolver = if fakeip_region.reverse {
                "dns-remote".to_string()
            } else {
                get_domestic_resolver_tag(resolve_node_domains_ahead, "dns-domestic")
            };
            let fakeip_fallthrough_resolver = if fakeip_region.reverse {
                "dns-domestic"
            } else {
                "dns-remote"
            };
            // S0 境内内容（geosite-cn 等）→ 境内解析器。
            if !fakeip_local_geo.is_empty() {
                let rule_set = if fakeip_local_geo.len() == 1 {
                    OneOrMany::One(fakeip_local_geo.into_iter().next().unwrap())
                } else {
                    OneOrMany::Many(fakeip_local_geo)
                };
                dns_rules.push(DnsRule {
                    rule_set: Some(rule_set),
                    query_type: Some(vec!["A".into(), "AAAA".into()]),
                    domain: None,
                    domain_suffix: None,
                    domain_keyword: None,
                    source_mac_address: None,
                    source_hostname: None,
                    preferred_by: None,
                    type_field: None,
                    action: None,
                    server: Some(fakeip_local_resolver),
                    inbound: None,
                    disable_cache: None,
                    rewrite_ttl: None,
                });
            }
            // S1 境外（A/AAAA catch-all）→ dns-remote（经当前出口隧道解析）。
            dns_rules.push(DnsRule {
                rule_set: None,
                query_type: Some(vec!["A".into(), "AAAA".into()]),
                domain: None,
                domain_suffix: None,
                domain_keyword: None,
                source_mac_address: None,
                source_hostname: None,
                preferred_by: None,
                type_field: None,
                action: None,
                server: Some(fakeip_fallthrough_resolver.into()),
                inbound: None,
                disable_cache: None,
                rewrite_ttl: None,
            });
        } else {
            // 没开 FakeIP（系统代理模式等）：用 geosite 规则各自拿正确 IP。
            if proxy_mode == "smart" {
                // 地区分流的 DNS 解析器划分，镜像 route 侧 reverse 翻转。
                let region = effective_region_routing(config.region_routing.as_ref());
                let runtime_dir = &deps.runtime_rules_dir;
                // #7 fail-closed：引用 region geo rule_set 前镜像「本地 .srs 缺失即跳过」。
                let local_geo = local_geo_tags(&region.region, runtime_dir, deps);
                // region-local 与其余侧各自的解析器（reverse 翻转）。
                let local_resolver = if region.reverse {
                    "dns-remote".to_string()
                } else {
                    get_domestic_resolver_tag(resolve_node_domains_ahead, "dns-domestic")
                };
                let fallthrough_resolver = if region.reverse {
                    "dns-domestic"
                } else {
                    "dns-remote"
                };
                if !local_geo.is_empty() {
                    let rule_set = if local_geo.len() == 1 {
                        OneOrMany::One(local_geo.into_iter().next().unwrap())
                    } else {
                        OneOrMany::Many(local_geo)
                    };
                    dns_rules.push(DnsRule {
                        rule_set: Some(rule_set),
                        query_type: None,
                        domain: None,
                        domain_suffix: None,
                        domain_keyword: None,
                        source_mac_address: None,
                        source_hostname: None,
                        preferred_by: None,
                        type_field: None,
                        action: None,
                        server: Some(local_resolver),
                        inbound: None,
                        disable_cache: None,
                        rewrite_ttl: None,
                    });
                }
                // 移除 geosite-geolocation-!cn（1.12 dns block 跑 rule_set 会失效/报错），一律 fallthrough。
                dns_rules.push(DnsRule {
                    rule_set: None,
                    query_type: None,
                    domain: None,
                    domain_suffix: None,
                    domain_keyword: None,
                    source_mac_address: None,
                    source_hostname: None,
                    preferred_by: None,
                    type_field: None,
                    action: None,
                    server: Some(fallthrough_resolver.into()),
                    inbound: None,
                    disable_cache: None,
                    rewrite_ttl: None,
                });
                // final 兜底解析器同步翻转：反向时未命中任何规则的查询应走 dns-remote（回国节点）。
                // 仅 smart 非-FakeIP + region.enabled + reverse 才翻。
                if region.enabled && region.reverse {
                    dns_config.final_server = Some("dns-remote".to_string());
                }
            } else {
                dns_rules.push(DnsRule {
                    rule_set: None,
                    query_type: Some(vec!["A".into(), "AAAA".into()]),
                    domain: None,
                    domain_suffix: None,
                    domain_keyword: None,
                    source_mac_address: None,
                    source_hostname: None,
                    preferred_by: None,
                    type_field: None,
                    action: None,
                    server: Some("dns-remote".into()),
                    inbound: None,
                    disable_cache: None,
                    rewrite_ttl: None,
                });
            }
        }
    }

    // ── P4b tailnet 按名解析：注入 tailscale DNS server + preferred_by 规则。
    // gate 放宽到「resolveByName 开 + 该 TS endpoint 已发射（pendingEndpoints）」。
    let ts_resolve_node = find_ts_resolve_node(config, id_to_tag_map, deps);
    if let Some(ts_node) = ts_resolve_node {
        let ep_tag = id_to_tag_map.get(&ts_node.id).cloned();
        if let Some(ep_tag) = ep_tag {
            let accept_default_resolvers = ts_node
                .tailscale_settings
                .as_ref()
                .and_then(|t| t.accept_default_resolvers);
            dns_config.servers.push(DnsServer {
                tag: TS_NAME_DNS_TAG.into(),
                type_field: Some("tailscale".into()),
                server: None,
                server_port: None,
                path: None,
                domain_resolver: None,
                detour: None,
                // tailscale DNS server 必填：引用该 mesh 节点的 endpoint tag（缺失则 sing-box FATAL）。
                endpoint: Some(ep_tag.clone()),
                accept_search_domain: Some(true),
                accept_default_resolvers: if accept_default_resolvers == Some(true) {
                    Some(true)
                } else {
                    None
                },
                neighbor_domain: None,
                address: None,
                address_resolver: None,
                inet4_range: None,
                inet6_range: None,
            });
            // preferred_by 规则须置于全量 catch-all 之前 → unshift 到规则链最前。
            dns_rules.insert(
                0,
                DnsRule {
                    rule_set: None,
                    query_type: None,
                    domain: None,
                    domain_suffix: None,
                    domain_keyword: None,
                    source_mac_address: None,
                    source_hostname: None,
                    preferred_by: Some(vec![TS_NAME_DNS_TAG.into()]),
                    type_field: None,
                    action: Some("route".into()),
                    server: Some(TS_NAME_DNS_TAG.into()),
                    inbound: None,
                    disable_cache: None,
                    rewrite_ttl: None,
                },
            );
            deps.log_info(&format!(
                "Tailscale 按名解析已启用：tailnet 名 → {TS_NAME_DNS_TAG}(endpoint={ep_tag})"
            ));
        } else {
            deps.log_warn("Tailscale 按名解析已开启，但未能定位选中节点的 endpoint tag，已跳过");
        }
    }

    // ── §15 主核测速探测池 DNS 规则（unshift 到 dns.rules 绝对最前，仅匹配探测流量）。
    if !deps.probe_pool_ports.is_empty() {
        let mut probe_rules: Vec<DnsRule> = Vec::new();
        for (k, _port) in deps.probe_pool_ports.iter().enumerate() {
            probe_rules.push(DnsRule {
                rule_set: None,
                query_type: Some(vec!["A".into(), "AAAA".into()]),
                domain: None,
                domain_suffix: None,
                domain_keyword: None,
                source_mac_address: None,
                source_hostname: None,
                preferred_by: None,
                type_field: None,
                action: Some("route".into()),
                server: Some(format!("dns-probe-exit-{k}")),
                inbound: Some(OneOrMany::Many(vec![probe_pool_inbound_tag(k)])),
                disable_cache: Some(true),
                rewrite_ttl: None,
            });
        }
        for (i, r) in probe_rules.into_iter().enumerate() {
            dns_rules.insert(i, r);
        }
    }

    // ── V37 出口伴测 / 出口 IP 探测 inbound 键控 DNS 规则（unshift 到最前）。
    if deps.probe_proxy_port.unwrap_or(0) > 0 {
        dns_rules.insert(
            0,
            DnsRule {
                rule_set: None,
                query_type: Some(vec!["A".into(), "AAAA".into()]),
                domain: None,
                domain_suffix: None,
                domain_keyword: None,
                source_mac_address: None,
                source_hostname: None,
                preferred_by: None,
                type_field: None,
                action: Some("route".into()),
                server: Some("dns-probe-exit-proxy".into()),
                inbound: Some(OneOrMany::Many(vec!["probe-proxy-in".into()])),
                disable_cache: Some(true),
                rewrite_ttl: None,
            },
        );
    }

    dns_config.rules = Some(dns_rules);
    dns_config
}

// ───────────────────────── 辅助函数 ─────────────────────────

/// 计算 region-local geo rule_set tags（fail-closed：本地 .srs 缺失即跳过）。
/// Polaris 两处（fakeip + 非-fakeip）的 `REGION_LOCAL_GEO[region].geosite.filter(tag => isValidSrsFile(...))`
/// 逻辑共用。fileName = findBuiltin(tag)?.fileName ?? `${tag}.srs`；FS 检查经 deps.is_valid_srs_fn 注入。
fn local_geo_tags(region: &str, runtime_dir: &str, deps: &DnsConfigDeps) -> Vec<String> {
    let geo = match region_local_geo(region) {
        Some(g) => g,
        None => return vec![],
    };
    geo.geosite
        .into_iter()
        .filter(|tag| {
            let file_name = find_builtin(tag)
                .map(|b| b.file_name)
                .unwrap_or_else(|| format!("{tag}.srs"));
            let full = format!("{runtime_dir}/{file_name}");
            (deps.is_valid_srs_fn)(&full)
        })
        .collect()
}

/// 定位 tailnet 按名解析目标节点（resolveByName 开 + 该 TS endpoint 已发射）。
/// 上游 `tsResolveNode` IIFE。返回 None = 无候选。
fn find_ts_resolve_node<'a>(
    config: &'a UserConfig,
    id_to_tag_map: &BTreeMap<String, String>,
    deps: &DnsConfigDeps,
) -> Option<&'a crate::user_config::server_config::ServerConfig> {
    use crate::user_config::server_config::Protocol;
    let emitted_tags: std::collections::HashSet<&str> = deps
        .pending_endpoints
        .iter()
        .map(|e| e.tag.as_str())
        .collect();
    let candidates: Vec<&crate::user_config::server_config::ServerConfig> = config
        .servers
        .iter()
        .filter(|s| {
            s.protocol == Protocol::Tailscale
                && s.tailscale_settings
                    .as_ref()
                    .and_then(|t| t.resolve_by_name)
                    == Some(true)
                && id_to_tag_map
                    .get(&s.id)
                    .map(|tag| emitted_tags.contains(tag.as_str()))
                    .unwrap_or(false)
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // 上游 `candidates.find(s => s.id === selectedServerId) ?? candidates[0]`。
    // 优先选中的 TS（若它在候选中），否则候选列表第一个。
    let selected = config.selected_server_id.as_deref();
    candidates
        .iter()
        .find(|s| Some(s.id.as_str()) == selected)
        .copied()
        .or_else(|| candidates.first().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::app_config::UserConfig;
    use crate::user_config::dns_config::DnsConfig as UserDnsConfig;
    use crate::user_config::proxy_mode::{ProxyMode, ProxyModeType};
    use crate::user_config::region_routing::RegionRoutingConfig;
    use crate::user_config::rule::{Rule, RuleAction, RuleType};
    use crate::user_config::server_config::{Protocol, ServerConfig, TailscaleSettings};
    use crate::user_config::tun_config::TunModeConfig;
    use std::collections::BTreeMap;

    /// 构造最小 deps（Linux 平台、无 endpoint、固定假路径、FS 全 false = 所有 .srs 缺失）。
    fn deps_false() -> DnsConfigDeps {
        DnsConfigDeps {
            lan_resolver_for_dns: None,
            pending_endpoints: vec![],
            log: |_, _| {},
            selected_server_tag: "proxy-selector".into(),
            race_server_port: 0,
            probe_pool_ports: vec![],
            probe_proxy_port: None,
            platform: "linux".into(),
            custom_rules_dir: "/fake/custom-rules/".into(),
            runtime_rules_dir: "/fake/runtime-rules/".into(),
            is_valid_srs_fn: |_| false,
            exists_fn: |_| false,
        }
    }

    /// 构造最小 UserConfig（smart + systemProxy + 单节点）。
    fn base_config() -> UserConfig {
        UserConfig {
            servers: vec![ServerConfig {
                id: "s1".into(),
                name: "HK".into(),
                protocol: Protocol::Vless,
                address: "hk.example.com".into(),
                port: 443,
                ..Default::default()
            }],
            selected_server_id: Some("s1".into()),
            proxy_mode: ProxyMode::Smart,
            proxy_mode_type: ProxyModeType::SystemProxy,
            ..Default::default()
        }
    }

    /// 收集 server tag 列表。
    fn server_tags(c: &DnsConfig) -> Vec<String> {
        c.servers.iter().map(|s| s.tag.clone()).collect()
    }

    #[test]
    fn always_present_bootstrap_servers() {
        // 无论配置如何，5 个基础 server 恒在：bootstrap/node/local/domestic/remote。
        let c = build_dns_config(&base_config(), &BTreeMap::new(), &deps_false());
        let tags = server_tags(&c);
        assert!(tags.contains(&"dns-bootstrap".into()));
        assert!(tags.contains(&"dns-node".into()));
        assert!(tags.contains(&"dns-local".into()));
        assert!(tags.contains(&"dns-domestic".into()));
        assert!(tags.contains(&"dns-remote".into()));
        // dns-remote detour = selected_server_tag。
        let remote = c.servers.iter().find(|s| s.tag == "dns-remote").unwrap();
        assert_eq!(remote.detour.as_deref(), Some("proxy-selector"));
    }

    #[test]
    fn fakeip_off_adds_reverse_mapping() {
        // 关 FakeIP → reverse_mapping=true；无 fakeip server。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(false),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        assert_eq!(c.reverse_mapping, Some(true));
        assert!(!server_tags(&c).contains(&"fakeip".into()));
    }

    #[test]
    fn only_the_fakeip_catch_all_rewrites_ttl() {
        // FakeIP 合成应答必须带 rewrite_ttl（压错配窗口，理由见 FAKEIP_REWRITE_TTL）；
        // 其余 DNS 规则一条都不许带 —— 它们的应答来自真实上游，改写 TTL 是纯粹的越权。
        //
        // 判据故意写成「按 server 分区的全量对账」而不是「找到那条断言它有」：后者对
        // 「rewrite_ttl 被顺手撒到别的规则上」这类回归是瞎的。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(true),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        let rules = c.rules.as_ref().expect("开 FakeIP 必有 DNS 规则");
        let with_ttl: Vec<_> = rules.iter().filter(|r| r.rewrite_ttl.is_some()).collect();
        assert_eq!(
            with_ttl.len(),
            1,
            "带 rewrite_ttl 的规则应恰为 1 条，实际 {}",
            with_ttl.len()
        );
        assert_eq!(with_ttl[0].server.as_deref(), Some("fakeip"));
        assert_eq!(with_ttl[0].rewrite_ttl, Some(FAKEIP_REWRITE_TTL));
        // 反向：关 FakeIP 的世界里一条都不该有。
        let mut off = base_config();
        off.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(false),
            ..Default::default()
        });
        let c_off = build_dns_config(&off, &BTreeMap::new(), &deps_false());
        assert!(
            c_off
                .rules
                .as_ref()
                .is_none_or(|rs| rs.iter().all(|r| r.rewrite_ttl.is_none())),
            "关 FakeIP 时不该有任何 rewrite_ttl"
        );
    }

    #[test]
    fn fakeip_on_default_adds_fakeip_server_v4_only() {
        // 开 FakeIP（缺省 true）→ fakeip server 仅 inet4_range（关 IPv6）。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(true),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        let fakeip = c
            .servers
            .iter()
            .find(|s| s.tag == "fakeip")
            .expect("fakeip server present");
        assert_eq!(fakeip.inet4_range.as_deref(), Some("198.18.0.0/15"));
        assert!(fakeip.inet6_range.is_none(), "关 IPv6 不分配 v6 段");
        // 开 FakeIP → 不加 reverse_mapping。
        assert_eq!(c.reverse_mapping, None);
    }

    #[test]
    fn fakeip_on_with_ipv6_adds_v6_range() {
        let mut cfg = base_config();
        cfg.enable_ipv6 = Some(true);
        cfg.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(true),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        let fakeip = c.servers.iter().find(|s| s.tag == "fakeip").unwrap();
        assert_eq!(fakeip.inet6_range.as_deref(), Some("2001:2::/48"));
    }

    #[test]
    fn strategy_follows_ipv6() {
        // 开 IPv6 → prefer_ipv4；关 → ipv4_only。
        let c = build_dns_config(&base_config(), &BTreeMap::new(), &deps_false());
        assert_eq!(c.strategy.as_deref(), Some("ipv4_only"));
        let mut cfg = base_config();
        cfg.enable_ipv6 = Some(true);
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        assert_eq!(c.strategy.as_deref(), Some("prefer_ipv4"));
    }

    #[test]
    fn lan_resolver_adds_dns_lan_dhcp() {
        // lanResolver 注入 → dns-lan(type:dhcp) + internalResolver=dns-lan。
        let mut deps = deps_false();
        deps.lan_resolver_for_dns = Some("192.168.1.1".into());
        let c = build_dns_config(&base_config(), &BTreeMap::new(), &deps);
        let lan = c.servers.iter().find(|s| s.tag == "dns-lan");
        assert!(lan.is_some(), "dns-lan server present");
        assert_eq!(lan.unwrap().type_field.as_deref(), Some("dhcp"));
    }

    #[test]
    fn node_domain_rule1_emits_when_server_has_domain() {
        // 节点 address=域名 → rule1 含 domain + domain_suffix（exact + .suffix）。
        let c = build_dns_config(&base_config(), &BTreeMap::new(), &deps_false());
        let rule1 = c.rules.as_ref().unwrap().iter().find(|r| {
            r.domain
                .as_ref()
                .map(|d| d.contains(&"hk.example.com".to_string()))
                .unwrap_or(false)
        });
        assert!(rule1.is_some(), "rule1 node-domain present");
        let r = rule1.unwrap();
        let suffix = r.domain_suffix.as_ref().unwrap();
        assert!(suffix.contains(&"hk.example.com".into()));
        assert!(suffix.contains(&".hk.example.com".into()));
    }

    #[test]
    fn node_domain_rule1_skips_ip_literals() {
        // address=IPv4 → 不进 rule1。
        let mut cfg = base_config();
        cfg.servers[0].address = "1.2.3.4".into();
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        let has_node_rule = c.rules.as_ref().unwrap().iter().any(|r| {
            r.domain
                .as_ref()
                .map(|d| d.contains(&"1.2.3.4".to_string()))
                .unwrap_or(false)
        });
        assert!(!has_node_rule, "IP 字面量不进 rule1");
    }

    #[test]
    fn bootstrap_rule_includes_doh_domains() {
        // 引导 rule：doh.pub/dns.google/cloudflare-dns.com/one.one.one.one → dns-bootstrap。
        let c = build_dns_config(&base_config(), &BTreeMap::new(), &deps_false());
        let bootstrap_rule = c
            .rules
            .as_ref()
            .unwrap()
            .iter()
            .find(|r| r.server.as_deref() == Some("dns-bootstrap"))
            .unwrap();
        let domains = bootstrap_rule.domain.as_ref().unwrap();
        assert!(domains.contains(&"doh.pub".into()));
        assert!(domains.contains(&"dns.google".into()));
        assert!(domains.contains(&"cloudflare-dns.com".into()));
        assert!(domains.contains(&"one.one.one.one".into()));
    }

    #[test]
    fn merged_local_rule_when_no_win_loop_no_lan() {
        // 非.Win + 无 lanResolver → 三合一单条 dns-local 规则（含 .local/.arpa/.lan/银行）。
        // fakeIpFilter=false 关闭 captive filter（否则 captive→dns-local 会多一条）。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(true),
            ..Default::default()
        });
        cfg.fake_ip_filter = Some(false);
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        let local_rules: Vec<_> = c
            .rules
            .as_ref()
            .unwrap()
            .iter()
            .filter(|r| r.server.as_deref() == Some("dns-local"))
            .collect();
        assert_eq!(local_rules.len(), 1, "合并为单条 dns-local 规则");
        let suffixes = local_rules[0].domain_suffix.as_ref().unwrap();
        assert!(suffixes.contains(&".local".into()));
        assert!(suffixes.contains(&".arpa".into()));
        assert!(suffixes.contains(&".lan".into()));
        assert!(suffixes.contains(&".microdone.cn".into())); // 银行域名
    }

    #[test]
    fn win_tun_splits_three_local_rules() {
        // Win + TUN → 拆三条（.local / 银行 dns-domestic / 内网 dns-domestic）。
        let mut cfg = base_config();
        cfg.proxy_mode_type = ProxyModeType::Tun;
        let mut deps = deps_false();
        deps.platform = "win32".into();
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps);
        let local_rules: Vec<_> = c
            .rules
            .as_ref()
            .unwrap()
            .iter()
            .filter(|r| r.server.as_deref() == Some("dns-local"))
            .collect();
        assert_eq!(local_rules.len(), 1, "Win 死环防护仅 .local 留 dns-local");
        // 银行 + 内网 → dns-domestic（无 lanResolver）。
        let domestic_rules: Vec<_> = c
            .rules
            .as_ref()
            .unwrap()
            .iter()
            .filter(|r| r.server.as_deref() == Some("dns-domestic"))
            .collect();
        // captive filter 不开（enableFakeIp 缺省 true 但... 这里 enableFakeIp=true → 无 captive filter 块需 fakeIpFilter!==false）
        // 至少银行 + 内网两条 dns-domestic。
        assert!(domestic_rules.len() >= 2);
    }

    #[test]
    fn fakeip_default_filter_rules() {
        // 开 FakeIP + 未编辑 filterList → captive + ntp/keyword 两条规则。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(true),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        let rules = c.rules.as_ref().unwrap();
        // captive 规则（domain → internalResolverTag=dns-local，因非 Win 无 lan）。
        let captive = rules.iter().find(|r| {
            r.domain
                .as_ref()
                .map(|d| d.contains(&"captive.apple.com".to_string()))
                .unwrap_or(false)
        });
        assert!(captive.is_some(), "captive filter rule present");
        assert_eq!(captive.unwrap().server.as_deref(), Some("dns-local"));
        // ntp/keyword 规则（domain_suffix + domain_keyword → dns-domestic）。
        let ntp = rules.iter().find(|r| {
            r.domain_keyword
                .as_ref()
                .map(|k| k.contains(&"ntp".to_string()))
                .unwrap_or(false)
        });
        assert!(ntp.is_some(), "ntp/stun keyword rule present");
        let ntp_r = ntp.unwrap();
        assert_eq!(ntp_r.server.as_deref(), Some("dns-domestic"));
        // ntp suffix 含 [ntp.org, .ntp.org] 形态。
        let suf = ntp_r.domain_suffix.as_ref().unwrap();
        assert!(suf.contains(&"ntp.org".into()));
        assert!(suf.contains(&".ntp.org".into()));
    }

    #[test]
    fn fakeip_filter_disabled_no_filter_rules() {
        // fakeIpFilter=false → 完全不生成 captive/ntp filter。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(true),
            ..Default::default()
        });
        cfg.fake_ip_filter = Some(false);
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        let has_captive = c.rules.as_ref().unwrap().iter().any(|r| {
            r.domain
                .as_ref()
                .map(|d| d.contains(&"captive.apple.com".to_string()))
                .unwrap_or(false)
        });
        assert!(!has_captive, "fakeIpFilter=false 关闭 filter");
    }

    #[test]
    fn fakeip_edited_filter_list_splits_captive_and_others() {
        // 用户编辑过 filterList：内置 captive 仍走 internalResolver；其余走 dns-domestic。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(true),
            ..Default::default()
        });
        cfg.fake_ip_filter_list = Some(vec![
            "captive.apple.com".into(),  // 内置 captive
            "custom.example.com".into(), // 其它
        ]);
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        let rules = c.rules.as_ref().unwrap();
        // captive → dns-local（internalResolver）。
        let captive = rules.iter().find(|r| {
            r.domain
                .as_ref()
                .map(|d| d.contains(&"captive.apple.com".to_string()))
                .unwrap_or(false)
        });
        assert_eq!(captive.unwrap().server.as_deref(), Some("dns-local"));
        // others → dns-domestic（domain_suffix 含 [custom.example.com, .custom.example.com]）。
        let others = rules.iter().find(|r| {
            r.domain_suffix
                .as_ref()
                .map(|d| d.contains(&"custom.example.com".to_string()))
                .unwrap_or(false)
        });
        assert!(others.is_some(), "others suffix rule present");
        // ntp/stun keyword 始终兜底。
        let has_keyword = rules.iter().any(|r| {
            r.domain_keyword
                .as_ref()
                .map(|k| k.contains(&"stun".to_string()))
                .unwrap_or(false)
        });
        assert!(has_keyword, "ntp/stun keyword 始终保留");
    }

    #[test]
    fn smart_non_fakeip_global_mode_dns_remote_query_type() {
        // global + 非 FakeIP → query_type A/AAAA → dns-remote。
        let mut cfg = base_config();
        cfg.proxy_mode = ProxyMode::Global;
        cfg.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(false),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        let remote_qt = c
            .rules
            .as_ref()
            .unwrap()
            .iter()
            .find(|r| r.query_type.is_some() && r.server.as_deref() == Some("dns-remote"));
        assert!(
            remote_qt.is_some(),
            "global non-fakeip → dns-remote query_type"
        );
    }

    #[test]
    fn smart_non_fakeip_final_stays_domestic_forward() {
        // smart 非-FakeIP + 正向 region（默认 cn reverse=false）→ final 保持 dns-domestic。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(false),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        assert_eq!(c.final_server.as_deref(), Some("dns-domestic"));
    }

    #[test]
    fn smart_non_fakeip_reverse_flips_final_to_remote() {
        // smart 非-FakeIP + reverse=true → final 翻为 dns-remote。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(false),
            ..Default::default()
        });
        cfg.region_routing = Some(RegionRoutingConfig {
            enabled: true,
            region: "cn".into(),
            reverse: true,
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        assert_eq!(c.final_server.as_deref(), Some("dns-remote"));
    }

    #[test]
    fn optimistic_cache_and_timeout_emitted() {
        // optimisticCache=true → optimistic=true；dnsTimeoutMs>0 → timeout="<n>ms"。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            optimistic_cache: Some(true),
            dns_timeout_ms: Some(5000.0),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        assert_eq!(c.optimistic, Some(true));
        assert_eq!(c.timeout.as_deref(), Some("5000ms"));
    }

    #[test]
    fn timeout_zero_or_invalid_omitted() {
        // dnsTimeoutMs=0 / NaN / 负 → 不下发 timeout。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            dns_timeout_ms: Some(0.0),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        assert_eq!(c.timeout, None);
        // NaN
        cfg.dns_config.as_mut().unwrap().dns_timeout_ms = Some(f64::NAN);
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        assert_eq!(c.timeout, None);
    }

    #[test]
    fn timeout_rounds_to_int_ms() {
        // 4999.6 → "5000ms"（round）。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            dns_timeout_ms: Some(4999.6),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        assert_eq!(c.timeout.as_deref(), Some("5000ms"));
    }

    #[test]
    fn race_server_emitted_when_port_positive_and_race_on() {
        // raceServerPort>0 + resolveNodeDomainsAhead!==false → dns-node-race server。
        let mut deps = deps_false();
        deps.race_server_port = 5353;
        let c = build_dns_config(&base_config(), &BTreeMap::new(), &deps);
        let race = c.servers.iter().find(|s| s.tag == DNS_NODE_RACE_TAG);
        assert!(race.is_some());
        assert_eq!(race.unwrap().server_port, Some(5353));
        assert_eq!(race.unwrap().type_field.as_deref(), Some("udp"));
    }

    #[test]
    fn race_server_skipped_when_resolve_disabled() {
        // raceServerPort>0 + resolveNodeDomainsAhead=false → 不生成 race server。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            resolve_node_domains_ahead: Some(false),
            ..Default::default()
        });
        let mut deps = deps_false();
        deps.race_server_port = 5353;
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps);
        assert!(c.servers.iter().all(|s| s.tag != DNS_NODE_RACE_TAG));
    }

    #[test]
    fn probe_pool_emits_servers_and_leading_rules() {
        // probePoolPorts=[1,2] → 2 个 dns-probe-exit-{0,1} server + 2 条 inbound probe-in 规则置顶。
        let mut deps = deps_false();
        deps.probe_pool_ports = vec![5354, 5355];
        let c = build_dns_config(&base_config(), &BTreeMap::new(), &deps);
        let tags = server_tags(&c);
        assert!(tags.contains(&"dns-probe-exit-0".into()));
        assert!(tags.contains(&"dns-probe-exit-1".into()));
        // 规则置顶：前两条是 probe-in-{0,1}。
        let rules = c.rules.as_ref().unwrap();
        assert_eq!(
            rules[0].inbound,
            Some(OneOrMany::Many(vec!["probe-in-0".into()]))
        );
        assert_eq!(
            rules[1].inbound,
            Some(OneOrMany::Many(vec!["probe-in-1".into()]))
        );
        assert_eq!(rules[0].disable_cache, Some(true));
    }

    #[test]
    fn probe_proxy_emits_server_and_leading_rule() {
        // probeProxyPort>0 → dns-probe-exit-proxy server + probe-proxy-in 规则置顶。
        let mut deps = deps_false();
        deps.probe_proxy_port = Some(5356);
        let c = build_dns_config(&base_config(), &BTreeMap::new(), &deps);
        let proxy_srv = c.servers.iter().find(|s| s.tag == "dns-probe-exit-proxy");
        assert!(proxy_srv.is_some());
        assert_eq!(proxy_srv.unwrap().detour.as_deref(), Some("proxy-selector"));
        // 规则[0] = probe-proxy-in。
        assert_eq!(
            c.rules.as_ref().unwrap()[0].inbound,
            Some(OneOrMany::Many(vec!["probe-proxy-in".into()]))
        );
    }

    #[test]
    fn neighbor_domains_attached_to_dns_local_on_linux() {
        // Linux + neighborDomains → dns-local.neighbor_domain 归一化（.lan）。
        let mut cfg = base_config();
        cfg.tun_config = Some(TunModeConfig {
            neighbor_domains: Some(vec!["lan".into(), "home.arpa".into()]),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        let local = c.servers.iter().find(|s| s.tag == "dns-local").unwrap();
        let nd = local.neighbor_domain.as_ref().unwrap();
        assert!(nd.contains(&".lan".into()));
        assert!(nd.contains(&".home.arpa".into()));
    }

    #[test]
    fn neighbor_domains_skipped_on_win32() {
        // win32 不支持 source device match → 不附 neighbor_domain。
        let mut cfg = base_config();
        cfg.tun_config = Some(TunModeConfig {
            neighbor_domains: Some(vec!["lan".into()]),
            ..Default::default()
        });
        let mut deps = deps_false();
        deps.platform = "win32".into();
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps);
        let local = c.servers.iter().find(|s| s.tag == "dns-local").unwrap();
        assert!(local.neighbor_domain.is_none());
    }

    #[test]
    fn tailscale_resolve_by_name_emits_server_and_preferred_by_rule() {
        // resolveByName=true + endpoint 已发射 → dns-tailscale server + preferred_by 规则。
        let mut cfg = base_config();
        cfg.servers.push(ServerConfig {
            id: "ts1".into(),
            name: "TS".into(),
            protocol: Protocol::Tailscale,
            address: "".into(),
            port: 0,
            tailscale_settings: Some(TailscaleSettings {
                resolve_by_name: Some(true),
                accept_default_resolvers: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut id_map = BTreeMap::new();
        id_map.insert("ts1".into(), "TS".into());
        let mut deps = deps_false();
        deps.pending_endpoints = vec![Endpoint {
            type_field: "tailscale".into(),
            tag: "TS".into(),
            ..Default::default()
        }];
        let c = build_dns_config(&cfg, &id_map, &deps);
        // dns-tailscale server。
        let ts_srv = c.servers.iter().find(|s| s.tag == TS_NAME_DNS_TAG);
        assert!(ts_srv.is_some());
        assert_eq!(ts_srv.unwrap().endpoint.as_deref(), Some("TS"));
        assert_eq!(ts_srv.unwrap().accept_default_resolvers, Some(true));
        // preferred_by 规则（命中 preferred_by）。
        let preferred = c.rules.as_ref().unwrap().iter().find(|r| {
            r.preferred_by
                .as_ref()
                .map(|p| p.contains(&TS_NAME_DNS_TAG.to_string()))
                .unwrap_or(false)
        });
        assert!(preferred.is_some(), "preferred_by rule present");
    }

    #[test]
    fn tailscale_resolve_skipped_when_endpoint_not_emitted() {
        // resolveByName=true 但 endpoint 未在 pendingEndpoints → 不生成。
        let mut cfg = base_config();
        cfg.servers.push(ServerConfig {
            id: "ts1".into(),
            name: "TS".into(),
            protocol: Protocol::Tailscale,
            address: "".into(),
            port: 0,
            tailscale_settings: Some(TailscaleSettings {
                resolve_by_name: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut id_map = BTreeMap::new();
        id_map.insert("ts1".into(), "TS".into());
        let deps = deps_false(); // 无 pendingEndpoints
        let c = build_dns_config(&cfg, &id_map, &deps);
        assert!(c.servers.iter().all(|s| s.tag != TS_NAME_DNS_TAG));
    }

    #[test]
    fn direct_mode_no_geo_rules() {
        // direct 模式 → 不生成 smart/global 分流规则（无 fakeip/geo catch-all）。
        let mut cfg = base_config();
        cfg.proxy_mode = ProxyMode::Direct;
        cfg.dns_config = Some(UserDnsConfig {
            enable_fake_ip: Some(false),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        // 不应有 query_type→fakeip 或 query_type→dns-remote(global) 的 catch-all。
        let has_qt_fakeip = c
            .rules
            .as_ref()
            .unwrap()
            .iter()
            .any(|r| r.query_type.is_some() && r.server.as_deref() == Some("fakeip"));
        assert!(!has_qt_fakeip);
    }

    #[test]
    fn custom_bypass_fakeip_inline_rule() {
        // bypassFakeIP=true + 文件缺失（FS=false）→ inline 合并 rule。
        //
        // 判据是「解析器与该规则的去向一致」，不是某个固定 tag：走代理的域名必须拿境外解析器，
        // 否则 bypassFakeIP 这个逃生口就是朝里开的（上游 #347 暴露的正是这一点）。
        // 本测试此前断言 proxy 规则 → dns-bootstrap，即把缺陷本身当成了基线。
        let bypass_server_for = |action: RuleAction| -> String {
            let mut cfg = base_config();
            cfg.dns_config = Some(UserDnsConfig {
                enable_fake_ip: Some(true),
                ..Default::default()
            });
            cfg.custom_rules = vec![Rule {
                id: "r1".into(),
                type_field: RuleType::Domain,
                values: vec!["blocked.example.com".into()],
                conditions: None,
                combine_mode: None,
                action,
                enabled: true,
                bypass_fakeip: Some(true),
                target_server_id: None,
                remarks: None,
                tls_spoof: None,
                tls_spoof_method: None,
            }];
            let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
            c.rules
                .as_ref()
                .unwrap()
                .iter()
                .find(|r| {
                    r.domain
                        .as_ref()
                        .is_some_and(|d| d.contains(&"blocked.example.com".to_string()))
                })
                .and_then(|r| r.server.clone())
                .expect("bypassFakeIP 规则必须 emit 一条带该域名的 DNS 规则")
        };
        assert_eq!(
            bypass_server_for(RuleAction::Proxy),
            "dns-remote",
            "走代理的 bypass 域名必须用境外解析器（境内解析器正是要绕开的那条路）"
        );
        assert_eq!(
            bypass_server_for(RuleAction::Direct),
            "dns-bootstrap",
            "直连的 bypass 域名保持境内解析器"
        );
    }

    #[test]
    fn invalid_domestic_dns_falls_back_default() {
        // 非法 domesticDns → 回退 doh.pub（server=doh.pub）。
        let mut cfg = base_config();
        cfg.dns_config = Some(UserDnsConfig {
            domestic_dns: Some("garbage text".into()),
            ..Default::default()
        });
        let c = build_dns_config(&cfg, &BTreeMap::new(), &deps_false());
        let domestic = c.servers.iter().find(|s| s.tag == "dns-domestic").unwrap();
        assert_eq!(domestic.server.as_deref(), Some("doh.pub"));
    }

    #[test]
    fn rules_always_set_non_empty() {
        // rules 恒非空（至少 bootstrap + local 规则）。
        let c = build_dns_config(&base_config(), &BTreeMap::new(), &deps_false());
        assert!(!c.rules.as_ref().unwrap().is_empty());
    }
}
