//! sing-box 路由配置生成（上游 `singbox-route-builder.ts` 1:1 移植）。
//!
//! route 子系统集成 hub：纯函数，只读 config/id_to_tag_map + 注入实例态依赖（probe 端口 /
//! lan_resolver_for_dns / pending_endpoints 值 + log·on_degraded 回调）。装配 sniff/探针/DNS 直连·劫持/
//! 节点排除/网银 U盾/endpoint 强制路由/私网直连/自定义规则(build_custom_rules)/应用分流/QUIC 阻断/
//! geo rule_set/悬空剪枝。
//!
//! 纯函数 + 依赖注入：所有实例态经 `RouteConfigDeps` 注入，FS 路径经 `RouteConfigDeps.runtime_rules_dir` /
//! `rule_resources_path` 注入（对拍固定假路径），`is_valid_srs_fn` 注入（对拍 fixture 控制）。

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use crate::builder::custom_rule_files::uses_fake_ip;
use crate::builder::custom_rules::{build_custom_rules, CustomRulesDeps};
use crate::builder::endpoint_routes::{
    collect_rule_targeted_server_ids, endpoint_forced_route_cidrs, mesh_force_routed_servers,
    mesh_forced_route_cidrs, mesh_node_carries_full_tunnel, should_force_route_subnets,
};
use crate::builder::helpers::{
    apply_rule_set_prune, effective_app_rules, effective_custom_rules,
    get_custom_domestic_dns_endpoint, get_required_geo_categories, host_to_exclude_cidr,
    is_ipv4_host, is_ipv6_host, probe_pool_inbound_tag, DOMESTIC_BANK_AND_STOCK_DOMAINS,
};
use crate::singbox::{OneOrMany, RouteConfig, RouteRule, RuleSet};
use crate::user_config::app_config::UserConfig;
use crate::user_config::app_rules_preset::get_app_preset;
use crate::user_config::builtin_geo_rulesets::{builtin_geo_rulesets, PRIVATE_DOMAIN_DIRECT_TAG};
use crate::user_config::cidr::{cidr_overlaps_any, partition_cidrs_by_overlap};
use crate::user_config::collections::dedupe;
use crate::user_config::dns_constants::{
    is_block_selection, BOOTSTRAP_DIRECT_DNS_IPS, PROXY_SELECTOR_TAG,
};
use crate::user_config::log_level::LogLevel;
use crate::user_config::region_routing::{
    effective_region_routing, region_foreign_geo, region_local_geo,
};
use crate::user_config::rule::AppRule;
use crate::user_config::rule::RuleAction;
use crate::user_config::rules::rule_ip_cidrs;
use crate::user_config::server_config::Protocol;
use crate::user_config::system_proxy_bypass::{bypass_lan_cidrs, effective_bypass_lan};
use crate::user_config::tun_config::{FAKEIP_INET4_RANGE, FAKEIP_INET6_RANGE};

/// DoH 上游 IP 单一真值（上游 `shared/dns#DOH_UPSTREAM_IPS`）。
/// 223.5.5.5 AliDNS + 1.12.12.12 DNSPod（#57）。
const DOH_UPSTREAM_IPS: &[&str] = &["223.5.5.5", "1.12.12.12"];

/// 浏览器内置 DoH 端点的**内置起点清单**（`domain_suffix` 语义，非全集）。
///
/// 只在 `blockBrowserDoh` 开关**打开**且用户没编辑过清单时使用；用户一旦编辑，以用户的为准。
///
/// # 收录判据
///
/// 「浏览器自带的安全 DNS 下拉里能选到的提供商」+ 广泛使用的公共 DoH 端点。suffix 语义下
/// `cloudflare-dns.com` 已覆盖 `mozilla.` / `chrome.` / `security.` / `family.` 那几个子域，
/// 故不逐个列。
///
/// # 刻意不收的两类（不是遗漏）
///
/// - **国内公共 DoH**（`doh.pub` / `dns.alidns.com` / `doh.360.cn` 等）：它们不是浏览器内置选项，
///   而且**本应用自己的 DNS 上游就用其中两个**（`DOH_UPSTREAM_IPS` 与 bootstrap 的 `doh.pub`）。
///   预填进来等于自伤 —— 用户要拦可以自己加，但那是他知情下的选择。
/// - **各家的门户/官网**：这里列的是**解析端点**，不是站点。列 apex（如 `quad9.net`）会把官网一起拦掉，
///   收益为零。用户若想连官网一起拦，把 apex 加进清单即可。
///
/// # 它必然不全，这是设计而非缺陷
///
/// DoH 端点可以是任意自建域名甚至纯 IP，黑名单原理上不可能穷尽。故本清单只是**起点**，
/// 真正的兜底是 UI 上那个可编辑 + 可批量导入的清单。
pub const DEFAULT_BROWSER_DOH_SUFFIXES: &[&str] = &[
    // Google
    "dns.google",
    // Cloudflare（含 mozilla./chrome./security./family. 子域）
    "cloudflare-dns.com",
    "one.one.one.one",
    // Quad9
    "dns.quad9.net",
    "dns9.quad9.net",
    "dns10.quad9.net",
    "dns11.quad9.net",
    // Cisco OpenDNS
    "doh.opendns.com",
    "doh.familyshield.opendns.com",
    // NextDNS（含 firefox. 子域）
    "dns.nextdns.io",
    // AdGuard（含 family./unfiltered. 子域）
    "adguard-dns.com",
    "dns.adguard.com",
    // CleanBrowsing
    "doh.cleanbrowsing.org",
    // Control D
    "dns.controld.com",
    "freedns.controld.com",
    // Mullvad（含 adblock./base. 子域）
    "dns.mullvad.net",
    "doh.mullvad.net",
    // DNS.SB
    "doh.sb",
    "doh.dns.sb",
    // Comss.one（Firefox 在俄区的默认档之一）
    "dns.comss.one",
    "router.comss.one",
    // 其它广泛使用的公共端点
    "wikimedia-dns.org",
    "dns.digitale-gesellschaft.ch",
    "doh.libredns.gr",
];

/// Tailscale preferred_by 试点开关（上游 `TS_PREFERRED_BY_TRIAL = false`）。
/// sing-box 源码确证 TS 的 routePrefixes 运行时动态+就绪窗口 nil → 组网段不归位，故 TS 必走 ip_cidr 静态。
const TS_PREFERRED_BY_TRIAL: bool = false;

/// 注入依赖：上游 `RouteConfigDeps`。实例态（值 + 回调）由 generateSingBoxConfig 注入。
///
/// 对拍：FS 路径注入固定假路径（如 "/fake/rules/"），`is_valid_srs_fn` 由测试夹具控制。
pub struct RouteConfigDeps<'a> {
    pub probe_direct_port: Option<u16>,
    pub probe_proxy_port: Option<u16>,
    pub update_in_port: Option<u16>,
    /// §15 主核测速探测池：K 个 probe-in-k → probe-selector-k 钉死路由的端口数。
    /// 空/缺省 = 不注入池。
    pub probe_pool_ports: Vec<u16>,
    pub lan_resolver_for_dns: Option<String>,
    pub pending_endpoints: &'a [crate::singbox::Endpoint],
    pub log: fn(LogLevel, &str),
    pub on_degraded: fn(),
    /// issue #147：本地 race server 的【自定义】上游 IP（内置 ali/dnspod 已在 BOOTSTRAP_DIRECT_DNS_IPS）。
    /// 缺省空 = race off / 无自定义上游（零变化）。
    pub race_upstream_ips: Vec<String>,
    /// issue #147：上面那些上游**实际在用的端口**（由 `polaris-dns-race` 的 `ResolvedUpstreams::direct_ports`
    /// 一路下发到此，见该字段文档）。缺省空 = race off（端口集逐字节回 `[53,443]` 基线，金样不动）。
    ///
    /// **本 builder 只消费、不复算**：真实上游集是 Tier 分桶 + canonical 去重 + Tier1 上限 + TUN 下摘
    /// `system` 之后的结果，那条选择链只在 `polaris-dns-race` 里完整存在。此处曾就地从
    /// `config.dns_config` 重新导出一遍端口（只认 `nodeResolverPool` 点名的纯 IP 条目，刻意不复制分桶/
    /// 去重/上限 ⇒ 真实集的**超集**）—— 方向是安全的，但那是**第二份真值源**：它与 sidecar 的选择
    /// 逻辑靠人肉对齐，任何一侧改口径都不会让另一侧转红。改成随 IP 一起注入后，两轴同源、同一次遍历
    /// 产出，结构上不可能分叉。
    pub race_upstream_ports: Vec<u16>,
    /// 运行时 rules 目录（内置 geo .srs 路径前缀）。对拍固定假路径。
    pub runtime_rules_dir: String,
    /// 用户规则资源目录（res:<id> 文件路径前缀）。对拍固定假路径。
    pub rule_resources_path: String,
    /// 自定义规则外化文件目录（L3 ext 文件路径前缀）。对拍固定假路径。
    pub custom_rules_dir: String,
    /// 编译目标 arch（tls_spoof 门控）。
    pub arch: String,
    /// 运行平台（source device match 门控）。
    pub platform: String,
    /// 文件存在性 + SRS 魔数检查（对拍 fixture 注入固定 true/false）。
    pub is_valid_srs_fn: fn(&str) -> bool,
}

/// QUIC(UDP/443) reject 规则工厂：可选叠加域名/进程等匹配器。route 与各处 blockQuic 共用，
/// 保证 network/port/action 字面量始终一致（避免某处漏写 network 导致行为漂移）。
/// 上游 `udp443RejectRule`。
///
/// # ⚠️ 本仓 5 处「裸 reject」仍受 50 次/30s 泛洪降级影响（已知，待另一批处理）
///
/// 这 5 处（本工厂 + 本文件 `:455` DNS 防泄露 domain_keyword 段、`:548` STUN 阻断、
/// `:596` logical udp443、`:887`）都**不带 `no_drop`** ⇒ 落到 sing-box 默认
/// `no_drop=false`：30s 内超 50 次拒绝就把 `method` 临时降级成 `drop`（静默丢包）。
///
/// **本工厂这一条尤其可疑**：它的既定目的是「阻 QUIC 逼浏览器回退 TCP」，而回退依赖拒绝是
/// **立刻**的；一旦降级成 drop，浏览器就等在那里，功能被打掉。
///
/// 阻断类新腿（自定义规则 / 应用分流的 `RuleAction::Block`）已显式置 `no_drop:true`。
/// 这 5 处**没跟着改的唯一原因**是：其中 3 条逐字节写在金样 37 例的期望值里
/// （`fixtures/config-snapshot.json`），改它们会改动金样、与 上游 参考实现分家 ⇒ 属另一批。
/// **此处只留判据，不改行为。**
fn udp443_reject_rule(matcher: RouteRule) -> RouteRule {
    let mut rule = matcher;
    rule.network = Some(vec!["udp".to_string()]);
    rule.port = Some(OneOrMany::Many(vec![443]));
    rule.action = Some("reject".to_string());
    // 清掉与 udp443 reject 冲突的匹配字段（Polaris 仅复制 matcher 字段，本工厂直接覆盖）。
    rule.port_range = None;
    rule
}

/// 提取 RouteRule 的匹配字段（除 action/outbound/network/port/port_range/type/mode/rules 外），
/// 供 udp443 reject 配对用（上游 `UDP443_MATCHER_EXCLUDE`）。返回 None = 无匹配字段。
fn extract_udp443_matcher(cr: &RouteRule) -> Option<RouteRule> {
    // 用 serde_json::Value 中转：序列化 cr → 移除 excluded 键 → 反序列化为 RouteRule。
    let mut val = serde_json::to_value(cr).ok()?;
    let obj = val.as_object_mut()?;
    for k in [
        "action",
        "outbound",
        "network",
        "port",
        "port_range",
        "type",
        "mode",
        "rules",
    ] {
        obj.remove(k);
    }
    // 仅当仍有非空匹配字段才返回（Polaris: Object.keys(matcher).length > 0）。
    if obj.is_empty() {
        return None;
    }
    // 移除值为 null 的字段（上游 `v != null`）。
    obj.retain(|_, v| !v.is_null());
    if obj.is_empty() {
        return None;
    }
    serde_json::from_value(val).ok()
}

/// [`build_route_config_with_report`] 的产物：路由配置 + 本次 fail-closed 剪枝报告。
#[derive(Debug, Clone)]
pub struct RouteConfigOutcome {
    /// 生成的 route 配置（与 [`build_route_config`] 返回值逐字段相同）。
    pub route: RouteConfig,
    /// 因本地 `.srs` 缺失/损坏而被 fail-closed 剪枝的 rule_set tag（**空 = 规则集完整**）。
    ///
    /// 这是「规则被剪枝」的**唯一诚实来源**：只有剪枝点本身知道哪些 tag 悬空。运行时层据此
    /// 决定要不要给用户发可见信号（资源齐全时恒空 → 零噪音）。
    pub pruned_rule_set_tags: Vec<String>,
}

/// buildRouteConfig 入口。上游 `buildRouteConfig`（904 行）。
///
/// 纯函数：只读 config/id_to_tag_map + 注入 deps。返回完整 RouteConfig。
pub fn build_route_config(
    config: &UserConfig,
    id_to_tag_map: &BTreeMap<String, String>,
    deps: &RouteConfigDeps<'_>,
) -> RouteConfig {
    build_route_config_with_report(config, id_to_tag_map, deps).route
}

/// [`build_route_config`] + 剪枝报告。
///
/// **为什么另开入口而非改原签名**：与 [`crate::builder::generate::generate_sing_box_config_with_report`]
/// 同一取舍——原函数有 30+ 处调用方（含 golden 对拍），改返回类型会把「多返回一个副产物」变成全仓
/// 签名 churn。原函数保留为本函数的薄 wrapper（同一条代码路径，绝无第二份生成逻辑）。
#[allow(clippy::too_many_lines)]
pub fn build_route_config_with_report(
    config: &UserConfig,
    id_to_tag_map: &BTreeMap<String, String>,
    deps: &RouteConfigDeps<'_>,
) -> RouteConfigOutcome {
    let mut rules: Vec<RouteRule> = Vec::new();
    let proxy_mode = proxy_mode_str(config);
    // 地区分流（智能分流的 geo 基线层）：None=默认中国大陆正向(=今日行为)，仅 smart 模式生效。
    let region = effective_region_routing(config.region_routing.as_ref());

    // 组网 force-route 的「engaged」判定集（与块 0c shouldForceRouteSubnets 同口径，单一真值）：仅
    // enabled+action==='proxy' 的自定义规则/应用分流 targetServerId 计入。下方重叠 warn 与块 0c 发射端共用，
    // 杜绝对「仅出网且未 engaged」节点虚报。
    let custom_rules_eff = effective_custom_rules(proxy_mode.as_str(), &config.custom_rules);
    let app_rules_eff = effective_app_rules(
        config.app_routing_enabled == Some(true),
        proxy_mode.as_str(),
        &config.app_rules,
    );
    let rule_targeted_server_ids = collect_targeted_mixed(&custom_rules_eff, &app_rules_eff);

    // mesh 重叠提醒（layer-2 兜底，非阻断）。基准只取「本轮实际会发射 force-route」的节点（与块 0c 同 gate）。
    let mesh_cidrs_for_warn = mesh_forced_route_cidrs(&mesh_force_routed_servers(
        &config.servers,
        config.selected_server_id.as_deref(),
        &rule_targeted_server_ids,
    ));
    if !mesh_cidrs_for_warn.is_empty() {
        let mut overlapping: BTreeSet<String> = BTreeSet::new();
        for rule in &custom_rules_eff {
            if !rule.enabled {
                continue;
            }
            for c in rule_ip_cidrs(rule) {
                if cidr_overlaps_any(&c, &mesh_cidrs_for_warn) {
                    overlapping.insert(c);
                }
            }
        }
        if !overlapping.is_empty() {
            let sample_vec: Vec<String> = overlapping.iter().take(5).cloned().collect();
            let sample = sample_vec.join(", ");
            (deps.log)(LogLevel::Warn, &format!(
                "{} 个自定义规则网段（{sample}{}）与组网(WG/Tailscale)路由段重叠：按优先级将覆盖组网路由，该段可能不走组网节点。如非有意请调整规则或组网配置。",
                overlapping.len(),
                if overlapping.len() > 5 { "…" } else { "" }
            ));
        }
    }

    // 主代理出站统一走 selector(proxy-selector)：热切换即改 selector 指向、路由无需重生成。
    // 常量取自 dns_constants 单一真值——与 outbounds.rs 的生成方、hotswitch.rs 的 PUT 消费方同源。
    let selected_server_tag = PROXY_SELECTOR_TAG;

    // 出口选中阻断哨兵（proxy-selector 的 default = block）。只用于管理面豁免判据，**不改 user_exit_tag**：
    // 阻断必须经由 selector 表达（改成直写 block 就退化成不可热切、且切出阻断也要重启）。
    let exit_is_block = is_block_selection(config.selected_server_id.as_deref());

    // D4/D7：主节点是「关外网组网节点」时，「→代理」的用户出口整体回退 direct。
    let exit_fallback = mesh_selected_exit_falls_back_to_direct(config);
    let user_exit_tag = if exit_fallback {
        "direct"
    } else {
        selected_server_tag
    };
    if exit_fallback {
        (deps.log)(
            LogLevel::Warn,
            "选中的组网节点已关闭外网访问：外网流量已回退直连（具体网段仍经组网节点），如需经此节点全隧道请开启该节点「允许访问外网」",
        );
    }

    // blockQuic（节点无关）：开启时对"将走代理"的 QUIC(UDP443) 执行 reject，逼浏览器回退 TCP。
    let block_proxy_quic =
        config.block_quic == Some(true) && proxy_mode != "direct" && !config.servers.is_empty();

    // 给定域名匹配器，返回应配对的 udp443 reject 规则（smart 模式放在每条 →代理 规则之前），否则 None。
    let proxy_udp_reject_for = |matcher: RouteRule| -> Option<RouteRule> {
        if block_proxy_quic {
            Some(udp443_reject_rule(matcher))
        } else {
            None
        }
    };

    // WebRTC 防泄露：off=不注入；proxy=STUN 经代理；block=reject STUN。
    let webrtc_leak = config
        .webrtc_leak_protection
        .as_deref()
        .unwrap_or("off")
        .to_string();

    // A. 嗅探规则（必须在前，用于识别域名）。
    rules.push(RouteRule {
        action: Some("sniff".to_string()),
        ..empty_matcher()
    });
    // WebRTC 防泄露开启时为稳健补一条显式 UDP stun sniffer。
    if webrtc_leak != "off" {
        rules.push(RouteRule {
            network: Some(vec!["udp".to_string()]),
            action: Some("sniff".to_string()),
            sniffer: Some(vec!["stun".to_string()]),
            timeout: Some("300ms".to_string()),
            ..empty_matcher()
        });
    }

    // A2. 出口 IP 探针钉死路由（紧随 sniff、先于一切分流/进程规则）。
    // 上游 `inbound: ['probe-direct-in']`（route-builder.ts:203）恒数组，对齐序列化形态。
    if let (Some(_direct), Some(_proxy)) = (deps.probe_direct_port, deps.probe_proxy_port) {
        rules.push(RouteRule {
            inbound: Some(OneOrMany::Many(vec!["probe-direct-in".to_string()])),
            action: Some("route".to_string()),
            outbound: Some("direct".to_string()),
            ..empty_matcher()
        });
        rules.push(RouteRule {
            inbound: Some(OneOrMany::Many(vec!["probe-proxy-in".to_string()])),
            action: Some("route".to_string()),
            outbound: Some(selected_server_tag.to_string()),
            ..empty_matcher()
        });
    }

    // A2b. 主核测速探测池钉死路由（§15）：probe-in-k → probe-selector-k。
    // 上游 `inbound: ['probe-in-${k}']`（route-builder.ts:212）恒数组。
    for k in 0..deps.probe_pool_ports.len() {
        rules.push(RouteRule {
            inbound: Some(OneOrMany::Many(vec![probe_pool_inbound_tag(k)])),
            action: Some("route".to_string()),
            outbound: Some(format!("probe-selector-{k}")),
            ..empty_matcher()
        });
    }

    // A3. update-in 钉死路由：global/smart → user_exit_tag；direct → direct。
    // 上游 `inbound: ['update-in']`（route-builder.ts:223）恒数组。
    //
    // **阻断出口豁免**：出口选阻断时 user_exit_tag 指向的 proxy-selector 其 default 已是 block ⇒
    // 订阅更新与检查更新会一并被掐死。这条腿必须改走 direct，理由是管理面同类豁免的一致性——
    // LAN/私网、DNS、ICMP、sing-box 自身进程本来就无条件放行直连，订阅/更新属同一类「让用户还能
    // 自救」的管理流量。掐死它之后用户只剩「切回出口」一条路，多一道自锁台阶而毫无收益。
    if deps.update_in_port.is_some() {
        rules.push(RouteRule {
            inbound: Some(OneOrMany::Many(vec!["update-in".to_string()])),
            action: Some("route".to_string()),
            outbound: Some(if proxy_mode == "direct" || exit_is_block {
                "direct".to_string()
            } else {
                user_exit_tag.to_string()
            }),
            ..empty_matcher()
        });
    }

    // 1. 强制放行 sing-box 核心进程：防止流量回流死循环。
    rules.push(RouteRule {
        process_name: Some(OneOrMany::Many(vec![
            "sing-box".to_string(),
            "sing-box.exe".to_string(),
        ])),
        action: Some("route".to_string()),
        outbound: Some("direct".to_string()),
        ..empty_matcher()
    });

    // C. 强制引导核心 DNS 直连（必须在 hijack-dns 之前！）。
    let custom_domestic_dns = get_custom_domestic_dns_endpoint(
        config
            .dns_config
            .as_ref()
            .and_then(|d| d.domestic_dns.as_deref()),
    );
    let mut dns_direct_cidrs: Vec<String> = BOOTSTRAP_DIRECT_DNS_IPS
        .iter()
        .map(|ip| format!("{ip}/32"))
        .collect();
    if let Some((ip, _port)) = &custom_domestic_dns {
        if let Some(c) = host_to_exclude_cidr(ip) {
            dns_direct_cidrs.push(c);
        }
    }
    if let Some(lan) = &deps.lan_resolver_for_dns {
        if let Some(c) = host_to_exclude_cidr(lan) {
            dns_direct_cidrs.push(c);
        }
    }
    for ip in &deps.race_upstream_ips {
        if let Some(c) = host_to_exclude_cidr(ip) {
            dns_direct_cidrs.push(c);
        }
    }
    // :53=UDP / :443=DoH（恒）。DoT(:853) 二期未实现——无 DoT 上游，不为永不工作的协议开无用端口。
    let mut dns_ports: Vec<u32> = vec![53, 443];
    if let Some((_, port)) = &custom_domestic_dns {
        dns_ports.push(u32::from(*port));
    }
    // issue #147：race 上游的**实际端口**必须与它的 IP 一起放行（两轴缺一，规则就匹配不上 ⇒ TUN 下该
    // 上游经代理出站/回环）。端口由 sidecar 侧的真实上游集下发（`race_upstream_ports`，见该字段文档），
    // **本处不复算**。race off 时两轴同为空 ⇒ 端口集逐字节回 `[53,443]` 基线，金样输出不动。
    dns_ports.extend(deps.race_upstream_ports.iter().copied().map(u32::from));
    let dns_ports_dedup: Vec<u32> = dedupe(dns_ports);
    rules.push(RouteRule {
        ip_cidr: Some(dns_direct_cidrs),
        port: Some(ports_to_one_or_many(dns_ports_dedup)),
        action: Some("route".to_string()),
        outbound: Some("direct".to_string()),
        ..empty_matcher()
    });

    // D. DNS 劫持（必须在引导 DNS IP 直连之后）。劫持所有其余 port 53 流量。
    // Polaris route-builder L273-276 用 `port: [53]`（恒数组），非单值裸数字；对齐序列化形态。
    rules.push(RouteRule {
        port: Some(OneOrMany::Many(vec![53])),
        action: Some("hijack-dns".to_string()),
        ..empty_matcher()
    });

    rules.push(RouteRule {
        process_name: Some(OneOrMany::Many(vec![
            "Surge".to_string(),
            "Surge 4".to_string(),
            "Surge 5".to_string(),
            "Clash".to_string(),
            "Clash for Windows".to_string(),
            "ClashX".to_string(),
            "ClashX Pro".to_string(),
            "clash-meta".to_string(),
            "Quantumult X".to_string(),
            "sing-box".to_string(),
            "sing-box.exe".to_string(),
            "mDNSResponder".to_string(),
            "apsd".to_string(),
            "nsurlsessiond".to_string(),
            "airportd".to_string(),
            "syspolicyd".to_string(),
            "trustd".to_string(),
            "ocspd".to_string(),
            "securityd".to_string(),
            "taskgated".to_string(),
            "findmydeviced".to_string(),
            "cloudd".to_string(),
        ])),
        action: Some("route".to_string()),
        outbound: Some("direct".to_string()),
        ..empty_matcher()
    });

    // 构造 routeConfig 主体（final 在此定）。
    // direct 模式或 smart+地区反向（如「回国」：海外应直连）→ final=direct；否则 → user_exit_tag。
    // 两分支同返 'direct' 是 1:1 镜像 Polaris（条件语义不同：模式 vs 地区反向），故 allow if_same_then_else。
    #[allow(clippy::if_same_then_else)]
    let final_outbound = if proxy_mode == "direct" {
        "direct".to_string()
    } else if proxy_mode == "smart" && region.enabled && region.reverse {
        "direct".to_string()
    } else {
        user_exit_tag.to_string()
    };
    let mut route_config = RouteConfig {
        rule_set: None,
        rules: rules.clone(),
        default_domain_resolver: Some("dns-bootstrap".to_string()),
        auto_detect_interface: Some(true),
        final_outbound: Some(final_outbound),
    };

    // 【已删除：内置 DoH 泄漏域名 reject 表】
    // 曾在此处无条件 reject `dns.google` / `cloudflare-dns.com` / `doh.opendns.com` /
    // `dns.quad9.net` / `one.one.one.one` 的 443+853，并在下方再配一条 UDP443 拒 DoH-over-QUIC。
    // 两条都**没有任何开关**，属硬编码域名黑名单，2026-08-13 按用户裁定整块移除。
    //
    // ⚠️ 已知代价（如实记，不是遗漏）：浏览器自带的 DoH（Chrome/Firefox 安全 DNS）不再被强制打断
    // ⇒ 那部分查询绕开本应用的 hijack-dns / FakeIP 体系，基于域名的分流与 FakeIP 路由对它们不生效。
    // 判据是「屏蔽浏览器行为不是代理客户端的职责」——要拦由用户在浏览器侧关掉安全 DNS。
    // **禁止以任何形式重建无条件域名黑名单**，由 `no_builtin_domain_reject_table` 钉住。

    // 【浏览器内置 DoH 拦截】—— 用户开关驱动，默认关（`blockBrowserDoh`）。
    //
    // 与上面那张被删的表的区别只有一个，但那是全部区别：**用户能关**。清单也归用户
    // （`browserDohList`，未编辑则用 `DEFAULT_BROWSER_DOH_SUFFIXES` 起点）。
    //
    // 两条规则一起发、且都排在**自定义规则之前**：开关打开的语义是「这些端点一律不通」，
    // 若 QUIC 那条排在自定义规则之后，一条把该域名路由到代理的自定义规则就会让 DoH-over-QUIC
    // 漏过去 —— 用户开了开关却半通半不通，比不做更坏。
    // （旧实现正是这样：443/853 那条在前、UDP443 那条在后。这里是有意的行为收敛。）
    if config.block_browser_doh == Some(true) {
        let suffixes: Vec<String> = match config.browser_doh_list.as_ref() {
            // 用户编辑过 → 以用户的为准（空清单 = 用户清空了，等于不拦，尊重之）。
            Some(list) => list
                .iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            None => DEFAULT_BROWSER_DOH_SUFFIXES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        };
        if !suffixes.is_empty() {
            // ① DoH(443) + DoT(853) 的 TCP 面。
            rules.push(RouteRule {
                domain_suffix: Some(suffixes.clone()),
                port: Some(OneOrMany::Many(vec![443, 853])),
                action: Some("reject".to_string()),
                ..empty_matcher()
            });
            // ② DoH-over-QUIC（UDP/443）。复用 udp443 工厂，与 blockQuic 同形。
            rules.push(udp443_reject_rule(RouteRule {
                domain_suffix: Some(suffixes),
                ..empty_matcher()
            }));
        }
    }

    // 排除全部代理节点的域名/IP，确保到任一节点的连接走直连（防回流死循环 + 兼容无缝切换/代理链）。
    {
        let mut ip_set: BTreeSet<String> = BTreeSet::new();
        let mut domain_set: BTreeSet<String> = BTreeSet::new();
        for s in &config.servers {
            let mut hosts: Vec<String> = Vec::new();
            if !s.address.is_empty() {
                hosts.push(s.address.clone());
            }
            if let Some(sn) = s
                .tls_settings
                .as_ref()
                .and_then(|t| t.server_name.as_deref())
            {
                if !sn.is_empty() {
                    hosts.push(sn.to_string());
                }
            }
            for host in &hosts {
                if is_ipv4_host(host) || is_ipv6_host(host) {
                    if let Some(cidr) = host_to_exclude_cidr(host) {
                        ip_set.insert(cidr);
                    }
                } else {
                    domain_set.insert(host.clone());
                }
            }
        }

        if !domain_set.is_empty() {
            let domains: Vec<String> = domain_set.into_iter().collect();
            let suffixes: Vec<String> = domains.iter().map(|d| format!(".{d}")).collect();
            rules.push(RouteRule {
                domain: Some(domains.clone()),
                domain_suffix: Some(suffixes),
                action: Some("route".to_string()),
                outbound: Some("direct".to_string()),
                ..empty_matcher()
            });
        }

        if !ip_set.is_empty() {
            rules.push(RouteRule {
                ip_cidr: Some(ip_set.into_iter().collect()),
                action: Some("route".to_string()),
                outbound: Some("direct".to_string()),
                ..empty_matcher()
            });
        }
    }

    // 0a. U盾/安全插件的本地伪域名 → override_address 强制 127.0.0.1。
    let ukey_local_domains: &[&str] = &[".microdone.cn"];
    let ukey_set: BTreeSet<&str> = ukey_local_domains.iter().copied().collect();
    let other_bank_domains: Vec<String> = DOMESTIC_BANK_AND_STOCK_DOMAINS
        .iter()
        .map(|s| s.to_string())
        .filter(|d| !ukey_set.contains(d.as_str()))
        .collect();

    rules.push(RouteRule {
        domain_suffix: Some(ukey_local_domains.iter().map(|s| s.to_string()).collect()),
        action: Some("route".to_string()),
        outbound: Some("direct".to_string()),
        override_address: Some("127.0.0.1".to_string()),
        ..empty_matcher()
    });

    // 0b. 其余银行/证券域名 → 普通 direct。
    if !other_bank_domains.is_empty() {
        rules.push(RouteRule {
            domain_suffix: Some(other_bank_domains),
            action: Some("route".to_string()),
            outbound: Some("direct".to_string()),
            ..empty_matcher()
        });
    }

    // WebRTC 防泄露：对嗅出的 STUN(UDP) 协议精确处理。
    if webrtc_leak == "proxy" && proxy_mode != "direct" && !config.servers.is_empty() {
        rules.push(RouteRule {
            protocol: Some("stun".to_string()),
            action: Some("route".to_string()),
            outbound: Some(selected_server_tag.to_string()),
            ..empty_matcher()
        });
    } else if webrtc_leak == "block" {
        rules.push(RouteRule {
            protocol: Some("stun".to_string()),
            action: Some("reject".to_string()),
            ..empty_matcher()
        });
    }

    // 2b. 拨号前解析（route action `resolve`，默认关）。
    //
    // # 位置为什么在这里
    //
    // 必须排在**探测/更新入站钉死路由（A2/A2b/A3）、节点域名排除、网银强制直连（0a/0b）、
    // bootstrap DNS 直连（C）之后** —— 那几类是终止规则，且它们的目的地绝不能先被解析成 IP
    // （探针要按域名钉出口、网银要按域名判直连）。
    // 又必须排在**自定义规则（块 3）之前** —— 自定义规则命中即 `break match`，排其后则 smart 模式
    // 下永远走不到本条。两侧都是硬约束，不是风格偏好。
    //
    // 无 matcher = 对本条之前未被终止的全部流量生效。裸 `{"action":"resolve"}` 经随包核
    // 1.14.0-beta.14 `check` rc=0（实测）。
    //
    // # 两条排除（2026-08-11 复审加，**不是**风格取舍）
    //
    // ① `exit_fallback`（选中「关外网的组网节点」⇒ 用户出口整体回退 direct，见块 D4/D7）：
    //    此时用户出口已全直连，与 ② 同源 —— 注入只换掉解析器、并丢掉 direct 出站的
    //    happy-eyeballs，收益为零而代价非零（resolve 失败即 fatalErr 断连）。
    //
    //    ⚠️ **本条的理由已更正**：初版依据是「该状态下 `dns-remote` 的 detour 仍指
    //    `proxy-selector`，远程解析打进黑洞，resolve 会把它放大成每条连接断连」。
    //    那个黑洞是真的（实测：route.final=direct 而 selector.default 仍是关外网的组网节点，
    //    DoH 被 cryptokey routing 丢掉），但它是**独立的既存缺陷**，已在同批修掉
    //    （`generate.rs` 的 `selected_server_tag` 改为跟随同一条回退，门见
    //    `dns_remote_detour_follows_the_same_exit_fallback_as_route`）。
    //    根因修掉后旧理由不再成立，故换成上面这条 —— 结论不变、依据换新，别让下一个人
    //    照着一条已经失效的理由去推别的结论。
    // ② `direct` 模式：出口恒直连，插 resolve 只换掉解析器，并丢掉 direct 出站的
    //    happy-eyeballs 并行拨号（拿到的是一组已解析地址，按序尝试）。收益为零、代价非零。
    //
    // 两条都是**静默不注入**而非报错：开关本身仍可开，只是在这两种状态下无效——
    // 与「灰掉开关」不同，状态是会变的（换个节点/换个模式就恢复），锁死开关反而更难理解。
    if config.resolve_before_dial == Some(true) && !exit_fallback && proxy_mode != "direct" {
        rules.push(RouteRule {
            action: Some("resolve".to_string()),
            ..empty_matcher()
        });
    }

    // 3. 自定义规则 + 应用分流（用户路由）——仅 smart 模式。
    if proxy_mode == "smart" {
        let custom_deps = CustomRulesDeps {
            runtime_rules_dir: deps.runtime_rules_dir.clone(),
            rule_resources_path: deps.rule_resources_path.clone(),
            custom_rules_dir: deps.custom_rules_dir.clone(),
            arch: deps.arch.clone(),
            platform: deps.platform.clone(),
            is_valid_srs_fn: deps.is_valid_srs_fn,
            // ext JSON source 存在性走 existsSync 等价（生产真 FS）。RouteConfigDeps 不携带此注入
            // （GenerateConfigDeps→proxy.rs 未加字段，避免动 Round2 owns 的 proxy.rs）→ config-engine 侧默认。
            exists_fn: crate::builder::custom_rule_files::ext_rule_file_exists,
            // 规则被剪时的唯一线索（资源缺失/不存在/远程 URL 已弃用）——直接透传 route 的 logger，
            // 生产即 `log::warn!(target: "config-engine", …)`，无需 GenerateConfigDeps 加字段（不动 proxy.rs）。
            log: deps.log,
        };
        let custom_result = build_custom_rules(
            &custom_rules_eff,
            config.selected_server_id.as_deref(),
            id_to_tag_map,
            selected_server_tag,
            &config.rule_resources,
            uses_fake_ip(config.dns_config.as_ref().and_then(|d| d.enable_fake_ip)),
            &custom_deps,
        );
        let custom_rules = custom_result.rules;
        let custom_rule_sets = custom_result.rule_sets;

        // 走代理的自定义规则同样要配对 udp443 reject。逐条插入：
        for cr in &custom_rules {
            // 阻断规则迁到 `action:"reject"` 后已无 outbound（`apply_rule_action` 是自定义规则
            // outbound 的唯一产地），故此处**不再**排 `"block"` 字面量 —— 留着只会让人以为
            // 规则还能指向 block 出站。`action != "route"` 这一项已把 reject 规则挡在外面。
            let is_proxy_out = cr.action.as_deref() == Some("route")
                && cr
                    .outbound
                    .as_deref()
                    .map(|o| o != "direct")
                    .unwrap_or(false);
            if is_proxy_out && block_proxy_quic {
                if cr.type_field.as_deref() == Some("logical") {
                    // logical 规则顶层不接受 network/port → 再套一层 AND logical。
                    rules.push(RouteRule {
                        action: Some("reject".to_string()),
                        type_field: Some("logical".to_string()),
                        mode: Some("and".to_string()),
                        rules: Some(vec![
                            RouteRule {
                                type_field: Some("logical".to_string()),
                                mode: cr.mode.clone(),
                                rules: cr.rules.clone(),
                                ..empty_matcher()
                            },
                            RouteRule {
                                network: Some(vec!["udp".to_string()]),
                                port: Some(OneOrMany::Many(vec![443])),
                                ..empty_matcher()
                            },
                        ]),
                        ..empty_matcher()
                    });
                } else if let Some(matcher) = extract_udp443_matcher(cr) {
                    rules.push(udp443_reject_rule(matcher));
                }
            }
            rules.push(cr.clone());
        }

        if !custom_rule_sets.is_empty() {
            let rs = route_config.rule_set.get_or_insert_with(Vec::new);
            rs.extend(custom_rule_sets);
        }

        // 排除进程：兼容旧配置的兜底（新数据已由 ConfigManager 迁移为 customRules 的 processName+direct 规则）。
        if let Some(bypass_processes) = config.bypass_processes.as_deref() {
            if !bypass_processes.is_empty() {
                rules.push(RouteRule {
                    process_name: Some(OneOrMany::Many(bypass_processes.to_vec())),
                    action: Some("route".to_string()),
                    outbound: Some("direct".to_string()),
                    ..empty_matcher()
                });
            }
        }

        // 应用分流规则（真·应用分流，基于进程名）。
        for app_rule in &app_rules_eff {
            if !app_rule.enabled {
                continue;
            }
            let preset = match get_app_preset(&app_rule.app_id, &config.custom_app_presets) {
                Some(p) => p,
                None => continue,
            };

            // 确定动作 + 出站方式。阻断走**规则级** `action:"reject"`（sing-box 1.11+ 官方替代
            // legacy `block` 出站，口径与 `custom_rules::apply_rule_action` 同一份，见那里的函数文档）
            // ⇒ 无 outbound 可指。其余走 `action:"route"` + 出站 tag。
            let (rule_action, outbound) = match app_rule.action {
                RuleAction::Proxy => ("route", Some(format!("rule-sel-app-{}", app_rule.app_id))),
                RuleAction::Block => ("reject", None),
                RuleAction::Direct => ("route", Some("direct".to_string())),
            };
            // `no_drop:true` 只给阻断腿（关掉 50 次/30s 泛洪降级，与 legacy `block` 出站等价；
            // 判据见 `singbox::RouteRule::no_drop`）。route 腿带上它是无意义字段，故按 action 分。
            let app_no_drop = matches!(app_rule.action, RuleAction::Block).then_some(true);
            // 「出站是代理」直接判枚举：此前由 outbound 字面量反推（`!= direct && != block`），
            // 迁移后 Block 已无 outbound，反推会把它误判成代理 ⇒ 给阻断规则白配一条 udp443 reject。
            let app_out_is_proxy = matches!(app_rule.action, RuleAction::Proxy);

            // a. 基于进程名的规则（最精准）。
            if !preset.process_names.is_empty() {
                if app_out_is_proxy {
                    if let Some(r) = proxy_udp_reject_for(RouteRule {
                        process_name: Some(OneOrMany::Many(preset.process_names.clone())),
                        ..empty_matcher()
                    }) {
                        rules.push(r);
                    }
                }
                rules.push(RouteRule {
                    process_name: Some(OneOrMany::Many(preset.process_names.clone())),
                    action: Some(rule_action.to_string()),
                    outbound: outbound.clone(),
                    no_drop: app_no_drop,
                    ..empty_matcher()
                });
            }

            // b. 基于原有 rule_set 的规则（兜底，基于域名/IP 识别）。tag 小写对齐。
            let mut rule_sets: Vec<String> = Vec::new();
            for tag in &preset.geosite_tags {
                rule_sets.push(format!("geosite-{}", tag.to_ascii_lowercase()));
            }
            for tag in &preset.geoip_tags {
                rule_sets.push(format!("geoip-{}", tag.to_ascii_lowercase()));
            }

            if !rule_sets.is_empty() {
                if app_out_is_proxy {
                    if let Some(r) = proxy_udp_reject_for(RouteRule {
                        rule_set: Some(OneOrMany::Many(rule_sets.clone())),
                        ..empty_matcher()
                    }) {
                        rules.push(r);
                    }
                }
                rules.push(RouteRule {
                    rule_set: Some(OneOrMany::Many(rule_sets)),
                    action: Some(rule_action.to_string()),
                    outbound: outbound.clone(),
                    no_drop: app_no_drop,
                    ..empty_matcher()
                });
            }
        }
    }

    // ===== 用户规则之后的功能性强制路由（reorder：原在用户规则之上，现下移）=====
    // 0c. endpoint 节点（WireGuard/Tailscale）的「配置路由段」强制路由到该节点自身 tag。
    {
        let emitted_endpoint_tags: BTreeSet<String> = deps
            .pending_endpoints
            .iter()
            .map(|e| e.tag.clone())
            .collect();
        let mut claimed_cidrs: BTreeSet<String> = BTreeSet::new();
        let mut force_route_conflicts = 0u32;
        for s in &config.servers {
            let tag = match id_to_tag_map.get(&s.id) {
                Some(t) => t.clone(),
                None => continue,
            };
            if !emitted_endpoint_tags.contains(&tag) {
                continue;
            }
            if !should_force_route_subnets(
                s,
                config.selected_server_id.as_deref(),
                &rule_targeted_server_ids,
            ) {
                continue;
            }
            // preferred_by 适用：非全隧道 +（WG 恒 | TS 试点开）。
            let use_preferred_by = !mesh_node_carries_full_tunnel(s)
                && (s.protocol == Protocol::Wireguard
                    || (s.protocol == Protocol::Tailscale && TS_PREFERRED_BY_TRIAL));
            if use_preferred_by {
                rules.push(RouteRule {
                    preferred_by: Some(vec![tag.clone()]),
                    action: Some("route".to_string()),
                    outbound: Some(tag),
                    ..empty_matcher()
                });
                continue;
            }
            // 否则（全隧道节点 / TS 试点未开）：手动 ip_cidr force-route（去 0/0 + 跨节点 first-match 去重）。
            let cidrs: Vec<String> = endpoint_forced_route_cidrs(s)
                .into_iter()
                .filter(|c| {
                    if claimed_cidrs.contains(c) {
                        force_route_conflicts += 1;
                        false
                    } else {
                        claimed_cidrs.insert(c.clone());
                        true
                    }
                })
                .collect();
            if !cidrs.is_empty() {
                rules.push(RouteRule {
                    ip_cidr: Some(cidrs),
                    action: Some("route".to_string()),
                    outbound: Some(tag),
                    ..empty_matcher()
                });
            }
        }
        if force_route_conflicts > 0 {
            (deps.log)(LogLevel::Warn, &format!(
                "{force_route_conflicts} 个 endpoint 路由段被多个节点重复声明，已按节点顺序去重（先声明者生效）"
            ));
        }
    }

    // 1. 私有 IP 段直连。仅当用户未关闭"绕过局域网"时添加。
    if config.bypass_lan != Some(false) {
        // FakeIP 护栏：剔除与 fakeip 假 IP 段相交的旁路条目。
        let mut fakeip_ranges: Vec<String> = Vec::new();
        if uses_fake_ip(config.dns_config.as_ref().and_then(|d| d.enable_fake_ip)) {
            fakeip_ranges.push(FAKEIP_INET4_RANGE.to_string());
            if config.enable_ipv6 == Some(true) {
                fakeip_ranges.push(FAKEIP_INET6_RANGE.to_string());
            }
        }
        let bypass_cfg = UConfigBypass(config);
        let bypass_list = effective_bypass_lan(&bypass_cfg);
        let (overlapping, bypass_cidrs) =
            partition_cidrs_by_overlap(&bypass_lan_cidrs(&bypass_list), &fakeip_ranges);
        if !overlapping.is_empty() {
            (deps.log)(
                LogLevel::Warn,
                &format!(
                    "旁路局域网清单含与 FakeIP 段({})相交的条目，已剔除以免假 IP 被当私网直连：{}",
                    fakeip_ranges.join(", "),
                    overlapping.join(", ")
                ),
            );
        }
        if !bypass_cidrs.is_empty() {
            rules.push(RouteRule {
                ip_cidr: Some(bypass_cidrs),
                action: Some("route".to_string()),
                outbound: Some("direct".to_string()),
                ..empty_matcher()
            });
        }
        // 私有/本地域名直连（geosite-private，补 ip_cidr 的域名盲区）。仅在本地 .srs 有效时加规则。
        // 必须 proxyMode !== 'direct'（与 rule_set 定义注入块同门控）。
        if proxy_mode != "direct" {
            let private_path =
                format!("{}/{PRIVATE_DOMAIN_DIRECT_TAG}.srs", deps.runtime_rules_dir);
            if (deps.is_valid_srs_fn)(&private_path) {
                rules.push(RouteRule {
                    rule_set: Some(OneOrMany::One(PRIVATE_DOMAIN_DIRECT_TAG.to_string())),
                    action: Some("route".to_string()),
                    outbound: Some("direct".to_string()),
                    ..empty_matcher()
                });
            }
        }
    }

    // ICMP 兜底：放在 mesh force-route(块 0c) + bypass-LAN 之后，恒走 direct。
    rules.push(RouteRule {
        network: Some(vec!["icmp".to_string()]),
        action: Some("route".to_string()),
        outbound: Some("direct".to_string()),
        ..empty_matcher()
    });

    // 【DNS 死循环防范】：sing-box 本地 DNS 解析器的请求必须强制直连。
    rules.push(RouteRule {
        protocol: Some("dns".to_string()),
        action: Some("route".to_string()),
        outbound: Some("direct".to_string()),
        ..empty_matcher()
    });

    rules.push(RouteRule {
        ip_cidr: Some(
            DOH_UPSTREAM_IPS
                .iter()
                .map(|ip| format!("{ip}/32"))
                .collect(),
        ),
        port: Some(OneOrMany::Many(vec![53, 443])),
        action: Some("route".to_string()),
        outbound: Some("direct".to_string()),
        ..empty_matcher()
    });

    rules.push(RouteRule {
        domain_suffix: Some(vec!["doh.pub".to_string()]),
        action: Some("route".to_string()),
        outbound: Some("direct".to_string()),
        ..empty_matcher()
    });

    // 【已删除：Chrome/Edge 后台 beacon 域名黑名单】
    // 曾无条件（`proxy_mode != "direct"` 即发射）reject 14 个 Google 域名。整块移除，不留缩表版本。
    //
    // 逐条独立成立的删除依据：
    //  ① **注释与代码从第一天就相反**：注释写「强制直连」，代码写 `action: "reject"` —— 该块未被复核。
    //  ② **代价已实测、收益从未证**：`clients2.google.com` 是扩展商店 CRX 的更新与下载端点，被 reject
    //     后「添加至 Chrome」必失败；`update.googleapis.com`（Chrome 永不自升级）、
    //     `oauthaccountmanager.googleapis.com`（账号登录/令牌刷新）、`mtalk.google.com`（FCM 推送）
    //     三处均为静默功能损失。而「耗尽连接池导致全站超时」这个立表理由无复现、无测试。
    //  ③ 严格按「掉了无用户可见损失」筛完只剩两条纯遥测，收益不可感知而策略却是硬编码不可关。
    //  ④ 屏蔽遥测不是代理客户端的职责（用户侧 uBlock/hosts 才是），代客户决定属越界。
    //
    // 删除后这些域名与其它 Google 域名同等对待：smart 落 geosite 分类，global 走 final。
    // 若「过一会就断网」的原始症状再现，那是节点侧（UDP 中继 / mux / DNS）问题，本表此前恰恰掩盖了它。

    // 智能分流的「地区分流」geo 基线层（仅 smart + region.enabled）。
    if proxy_mode == "smart" && region.enabled {
        let local_geo = region_local_geo(&region.region);
        let foreign_geo = region_foreign_geo(&region.region);
        // 正向：本地直连·海外代理；反向（如回国）：本地代理·海外直连。
        let local_out = if region.reverse {
            user_exit_tag
        } else {
            "direct"
        };
        let foreign_out = if region.reverse {
            "direct"
        } else {
            user_exit_tag
        };
        // 「→代理」的那一侧才在其前配对「代理向 UDP reject」（exitFallback 回退 direct 时不配对）。
        let foreign_to_proxy = !region.reverse;
        let local_to_proxy = region.reverse;

        // 海外/Google 一类。Google 关键词兜底对所有地区一致。
        let google_keywords = vec![
            "google".to_string(),
            "gmail".to_string(),
            "youtube".to_string(),
            "gstatic".to_string(),
            "googleapis".to_string(),
            "googlevideo".to_string(),
        ];
        // 海外侧。
        if foreign_to_proxy && !exit_fallback {
            if let Some(r) = proxy_udp_reject_for(RouteRule {
                domain_keyword: Some(google_keywords.clone()),
                ..empty_matcher()
            }) {
                rules.push(r);
            }
        }
        rules.push(RouteRule {
            domain_keyword: Some(google_keywords),
            action: Some("route".to_string()),
            outbound: Some(foreign_out.to_string()),
            ..empty_matcher()
        });
        for tag in &foreign_geo {
            if foreign_to_proxy && !exit_fallback {
                if let Some(r) = proxy_udp_reject_for(RouteRule {
                    rule_set: Some(OneOrMany::One(tag.clone())),
                    ..empty_matcher()
                }) {
                    rules.push(r);
                }
            }
            rules.push(RouteRule {
                rule_set: Some(OneOrMany::One(tag.clone())),
                action: Some("route".to_string()),
                outbound: Some(foreign_out.to_string()),
                ..empty_matcher()
            });
        }

        // 本地侧（geosite + geoip）。
        if let Some(local) = &local_geo {
            for tag in &local.geosite {
                if local_to_proxy && !exit_fallback {
                    if let Some(r) = proxy_udp_reject_for(RouteRule {
                        rule_set: Some(OneOrMany::One(tag.clone())),
                        ..empty_matcher()
                    }) {
                        rules.push(r);
                    }
                }
                rules.push(RouteRule {
                    rule_set: Some(OneOrMany::One(tag.clone())),
                    action: Some("route".to_string()),
                    outbound: Some(local_out.to_string()),
                    ..empty_matcher()
                });
            }
            for tag in &local.geoip {
                if local_to_proxy && !exit_fallback {
                    if let Some(r) = proxy_udp_reject_for(RouteRule {
                        rule_set: Some(OneOrMany::One(tag.clone())),
                        ..empty_matcher()
                    }) {
                        rules.push(r);
                    }
                }
                rules.push(RouteRule {
                    rule_set: Some(OneOrMany::One(tag.clone())),
                    action: Some("route".to_string()),
                    outbound: Some(local_out.to_string()),
                    ..empty_matcher()
                });
            }
        }
    }

    // 添加 rule_set（除非是直连模式）。直连模式下不需要 rule_set，因为全部走 direct。
    if proxy_mode != "direct" {
        let rs = route_config.rule_set.get_or_insert_with(Vec::new);
        let runtime_dir = &deps.runtime_rules_dir;
        // 地区分流：未激活地区的 geo 不注入 rule_set。
        let mut inactive_region_geo_tags: BTreeSet<String> = BTreeSet::new();
        for rid in ["ir", "ru"] {
            if !region.enabled || region.region != rid {
                if let Some(g) = region_local_geo(rid) {
                    for t in &g.geosite {
                        inactive_region_geo_tags.insert(t.clone());
                    }
                    for t in &g.geoip {
                        inactive_region_geo_tags.insert(t.clone());
                    }
                }
            }
        }
        // 随包播种目录里已定义的 tag（供下面的「规则资源页副本」回落腿去重）。
        let mut builtin_defined: BTreeSet<String> = rs.iter().map(|r| r.tag.clone()).collect();
        for rs_entry in builtin_geo_rulesets() {
            if inactive_region_geo_tags.contains(&rs_entry.tag) {
                continue;
            }
            let file_path = format!("{runtime_dir}/{}", rs_entry.file_name);
            // 缺失/损坏即跳过：不引用不存在的本地文件（否则 sing-box initialize rule-set FATAL）。
            if (deps.is_valid_srs_fn)(&file_path) {
                builtin_defined.insert(rs_entry.tag.clone());
                rs.push(RuleSet {
                    tag: rs_entry.tag,
                    type_field: "local".to_string(),
                    format: "binary".to_string(),
                    path: Some(file_path),
                    url: None,
                    download_detour: None,
                    update_interval: None,
                });
                continue;
            }
            // 随包播种缺失/损坏 → **回落「规则资源」页下载的本地副本**（`<userData>/rule-resource/`）。
            //
            // 不做这条，给用户的指引就是死路：剪枝 warn 与 `RULE_RESOURCES_MISSING` 都写「请到「规则资源」
            // 页下载后重连恢复」，而下载腿一律落 `rule-resource/`、内置 geo 基线却只读 `rules/` ⇒ 用户
            // 照着做、下载成功、再连仍被剪。选这条而非「让文案改口叫用户重置内置/重启重新播种」的理由：
            // 播种失败最常见的成因恰是**随包 `.srs` 本身缺失/损坏**（异常打包），那时重播多少次都没用，
            // 只有下载能救；而 catalog id 与 builtin tag **本就同形**（`rule_resource_catalog.rs` 模块头
            // 明记「同 id ⇒ 下载副本与随包项自然去重」），回落走的是既有机制，不新造第二套。
            add_local_geo_rule_set(&rs_entry.tag, rs, &mut builtin_defined, config, deps);
        }
    }

    // 添加自定义规则和应用分流所需的 Geosite/GeoIP rule_set。
    let (custom_geosite_categories, custom_geoip_categories) = get_required_geo_categories(
        &custom_rules_eff,
        &app_rules_eff,
        &config.custom_app_presets,
    );

    // fail-closed：自定义规则 / 应用分流引用的 geo 统一由「规则资源」管理。
    if proxy_mode != "direct"
        && (!custom_geosite_categories.is_empty() || !custom_geoip_categories.is_empty())
    {
        let rs = route_config.rule_set.get_or_insert_with(Vec::new);
        // 已有本地定义（随包内置已在上方注入）→ 跳过；否则用规则资源页的本地副本；再否则缺失（不注入，末尾剪枝）。
        let mut defined_tags: BTreeSet<String> = rs.iter().map(|r| r.tag.clone()).collect();
        let rs_vec = route_config.rule_set.as_mut().unwrap();
        let mut all_tags: Vec<String> = custom_geosite_categories
            .iter()
            .map(|c| format!("geosite-{c}"))
            .chain(custom_geoip_categories.iter().map(|c| format!("geoip-{c}")))
            .collect();
        for tag in &all_tags {
            add_local_geo_rule_set(tag, rs_vec, &mut defined_tags, config, deps);
        }
        all_tags.clear();
        let _ = all_tags;
    }

    // 【代理向 QUIC 兜底】：放在所有直连/分流规则之后，拦截"会落到 final(代理)"的剩余 QUIC(udp443)。
    if block_proxy_quic {
        rules.push(udp443_reject_rule(empty_matcher()));
    }

    // rule_set 按 tag 去重（保留首次=本地 .srs 优先于远程）。
    if let Some(rs) = route_config.rule_set.as_mut() {
        if !rs.is_empty() {
            let mut seen_tags: BTreeSet<String> = BTreeSet::new();
            rs.retain(|r| seen_tags.insert(r.tag.clone()));
        }
    }

    // fail-closed 兜底：剪掉引用「未定义 rule_set tag」的路由规则。
    let mut pruned_rule_set_tags: Vec<String> = Vec::new();
    let mut rules = {
        let defined_tags: BTreeSet<String> = route_config
            .rule_set
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|r| r.tag.clone())
            .collect();
        let mut referenced: BTreeSet<String> = BTreeSet::new();
        collect_refs(&rules, &mut referenced);
        let dangling: Vec<String> = referenced
            .iter()
            .filter(|t| !defined_tags.contains(*t))
            .cloned()
            .collect();
        if dangling.is_empty() {
            rules
        } else {
            // applyRuleSetPrune 操作整个 SingBoxConfig.route；这里构造只含 route 的壳（其它必填字段用最小值）。
            let mut singbox = crate::singbox::SingBoxConfig {
                log: crate::singbox::LogConfig {
                    level: "info".to_string(),
                    timestamp: false,
                    output: None,
                    disabled: None,
                },
                dns: None,
                inbounds: vec![],
                outbounds: vec![],
                endpoints: None,
                route: Some(RouteConfig {
                    rule_set: route_config.rule_set.clone(),
                    rules,
                    default_domain_resolver: None,
                    auto_detect_interface: None,
                    final_outbound: None,
                }),
                experimental: None,
                services: None,
            };
            let dangling_set: BTreeSet<String> = dangling.iter().cloned().collect();
            apply_rule_set_prune(&mut singbox, &dangling_set);
            // 回填剪枝后的 rules/rule_set。
            if let Some(r) = singbox.route.take() {
                route_config.rule_set = r.rule_set;
                // **warn 而非 info**：这是「用户以为在分流、实则整段规则被剪掉」的唯一告知。上游侧同为
                // `deps.log('warn', …)`（`route-builder.ts:895`）；Polaris 早先恒 info，被日志级别过滤吞掉后
                // 真机上「全量直连」只剩 `rule_set=0` 一个裸数字可查。
                (deps.log)(LogLevel::Warn, &format!(
                    "规则资源：{} 缺少本地副本，已跳过引用它的规则以避免代理启动失败（在「规则资源」页下载后自动恢复；应用分流仍按进程名生效）",
                    dangling.join(", ")
                ));
                pruned_rule_set_tags = dangling;
                r.rules
            } else {
                Vec::new()
            }
        }
    };

    // 【T2 fail-safe】资源缺失把「→代理」的腿剪光 → `final` 绝不落 direct。
    //
    // 两条降级各自合理、叠加即 fail-open（真机 2026-07-20 全量明文直连的直接成因）：
    //   - 只有资源缺失：final=proxy-selector，最坏「全走代理」——浪费带宽，不泄露；
    //   - 只有 reverse（回国）：CN 规则把国内流量送代理、海外直连——设计语义；
    //   - 两者叠加：把流量送代理的那两条 rule_set 规则被剪光，final=direct 兜底 ⇒ **全部明文直连**。
    //
    // **判据是「剪枝后还有没有规则指向 `user_exit_tag`」，不是「有没有发生过剪枝」**：
    // 后者对**任意**悬空 tag 生效，会在「28 个内置 geo 全好、只有一条自定义规则引用了未下载的 geo
    // 分类」时误触发 —— 回国模式的 final 从 direct 被翻成 proxy-selector ⇒ **全部海外流量改走国内
    // 节点**，把「海外直连」的语义整体反转，而真正的「→代理」腿（`geosite-cn`/`geoip-cn`）根本完好、
    // 压根没有 fail-open。查「代理腿还在不在」才是这条 fail-safe 想守的东西（因，非果）。
    //
    // 射程另外两道边界：
    //   - `proxy_mode == "direct"` 是用户显式选的全直连，是意图不是降级，**必须保持 direct**；
    //   - `user_exit_tag == "direct"`（D4/D7 组网出口回退）时**无处可退** —— 写 direct 到 direct 是
    //     no-op，此时若照打「已回退为代理」就是日志说谎。故单独分流，改打「无法 fail-safe」。
    if !pruned_rule_set_tags.is_empty()
        && proxy_mode != "direct"
        && route_config.final_outbound.as_deref() == Some("direct")
    {
        if user_exit_tag == "direct" {
            // **必须先判**：出口本身就是 direct 时 `routes_to_exit(rules, "direct")` 问的是「有没有
            // 规则走直连」——恒真且与本判定无关。先分流出去，才不会把这条腿静默吞掉。
            (deps.log)(
                LogLevel::Warn,
                "规则资源缺失已导致分流规则被剪枝，但选中的组网节点已关闭外网访问、用户出口本身就是直连：无法回退为代理，本次流量将明文直连。请下载规则资源，或为该节点开启「允许访问外网」/改选其它节点",
            );
        } else if !routes_to_exit(&rules, user_exit_tag) {
            (deps.log)(
                LogLevel::Warn,
                "规则资源缺失已导致分流规则被剪枝，为避免退化成全量明文直连，默认出口已回退为代理（下载规则资源后自动恢复地区分流语义）",
            );
            route_config.final_outbound = Some(user_exit_tag.to_string());
        }
        // else：「→代理」的腿还活着（剪掉的是别的 tag）→ `final=direct` 仍是设计语义，**不动**。
        // 剪枝本身已在上面 warn 过，不重复告警。
    }

    // ── 出口选阻断：整体改写成规则级 reject ──────────────────────────────────────
    //
    // # 为什么不是「末尾加一条 reject」
    //
    // 所有「→代理」的规则都指向 `proxy-selector`，它们在末尾那条之前就把流量路由走了 ——
    // 只加末尾一条，smart 模式下「海外→代理」照样出网，用户选了阻断却半通。故必须**逐条改写**：
    // 凡出站是 `proxy-selector` 的规则一律变成 `action:"reject"` 且不带 outbound，再补一条
    // 无 matcher 的兜底（实测真核收 matcher-less reject，rc=0）。
    //
    // # 为什么不再走 block 出站
    //
    // 旧形态是 `proxy-selector.default = "block"` + 一个 `{type:"block"}` 出站。它买到的是
    // 「切出阻断可热切」（PUT selector default），代价是**阻断期间核对每条被拦连接打一行 ERROR**
    // （`outbound/block[block]: operation not permitted`），而 `log.level=warn` 过滤不掉它。
    // 本仓核日志是单文件 + 满则轮转一次（`.1`），持续刷 ERROR 会把之前的排障线索**挤出去** ——
    // 丢的不是观感是历史。`action:"reject"` 只在 DEBUG 打一行。
    //
    // 代价如实记：切出阻断（阻断→节点/直连）由热切退化为**整核重启**，因为规则集变了。
    // 切入阻断本来就是重启。这个取舍的依据是「持续吃掉排障历史」比「用户主动改变网络姿态时
    // 断一次连接」更坏。
    if exit_is_block {
        for r in rules.iter_mut() {
            if r.outbound.as_deref() == Some(PROXY_SELECTOR_TAG) {
                r.action = Some("reject".to_string());
                r.outbound = None;
            }
        }
        rules.push(RouteRule {
            action: Some("reject".to_string()),
            ..empty_matcher()
        });
        // final 必须是一个合法出站 tag，且此刻已不可达（上面那条无 matcher 的规则全命中）。
        // 指 direct 而不是 proxy-selector：万一将来有人把兜底那条删了，退化方向是「直连」而不是
        // 「静默走代理」—— 后者与用户选阻断的意图正相反。
        route_config.final_outbound = Some("direct".to_string());
    }

    // 回填最终 rules 到 route_config。
    route_config.rules = rules;
    RouteConfigOutcome {
        route: route_config,
        pruned_rule_set_tags,
    }
}

// ===== 辅助函数 =====

/// 全默认（None）的 RouteRule matcher 骨架，便于 push 时用 `..empty_matcher()`。
fn empty_matcher() -> RouteRule {
    RouteRule {
        protocol: None,
        network: None,
        rule_set: None,
        domain: None,
        domain_suffix: None,
        domain_keyword: None,
        domain_regex: None,
        geosite: None,
        ip_cidr: None,
        source_ip_cidr: None,
        port: None,
        port_range: None,
        source_port: None,
        source_port_range: None,
        source_mac_address: None,
        source_hostname: None,
        process_name: None,
        process_path: None,
        process_name_not: None,
        inbound: None,
        action: None,
        outbound: None,
        no_drop: None,
        preferred_by: None,
        sniffer: None,
        rewrite_target: None,
        timeout: None,
        domain_resolver: None,
        override_address: None,
        tls_spoof: None,
        tls_spoof_method: None,
        type_field: None,
        mode: None,
        rules: None,
    }
}

/// UserConfig.proxy_mode (enum) → 小写字符串（smart/global/direct）。上游 `config.proxyMode.toLowerCase()`。
fn proxy_mode_str(config: &UserConfig) -> String {
    match config.proxy_mode {
        crate::user_config::ProxyMode::Smart => "smart",
        crate::user_config::ProxyMode::Global => "global",
        crate::user_config::ProxyMode::Direct => "direct",
    }
    .to_string()
}

/// addLocalGeo：已定义则跳过；否则查规则资源页本地副本，存在则注入 type:'local' 定义。
/// 缺失 → 不注入、不远程兜底 → 交末尾悬空引用剪枝（fail-closed）。上游 `addLocalGeo`。
fn add_local_geo_rule_set(
    tag: &str,
    rs: &mut Vec<RuleSet>,
    defined_tags: &mut BTreeSet<String>,
    config: &UserConfig,
    deps: &RouteConfigDeps<'_>,
) {
    if defined_tags.contains(tag) {
        return;
    }
    // 已下载进规则资源的本地副本 → 注入 type:'local'；缺失/损坏跳过。
    let local = match config.rule_resources.iter().find(|x| x.id == tag) {
        Some(r) => {
            let p = format!("{}/{}", deps.rule_resources_path, r.file_name);
            if (deps.is_valid_srs_fn)(&p) {
                Some(p)
            } else {
                None
            }
        }
        None => None,
    };
    if let Some(path) = local {
        rs.push(RuleSet {
            tag: tag.to_string(),
            type_field: "local".to_string(),
            format: "binary".to_string(),
            path: Some(path),
            url: None,
            download_detour: None,
            update_interval: None,
        });
        defined_tags.insert(tag.to_string());
    }
}

/// D4/D7：选中的组网节点是否「关外网」→ 整体用户出口回退 direct。
/// 上游 `meshSelectedExitFallsBackToDirect`。
///
/// pub：hotswitch.rs planHotSwitch 的 route 投影 guard 复用（选中节点 mesh 退回 direct 翻转 → 重启）。
pub fn mesh_selected_exit_falls_back_to_direct(config: &UserConfig) -> bool {
    let selected_id = match config.selected_server_id.as_deref() {
        Some(s) => s,
        None => return false,
    };
    let selected = match config.servers.iter().find(|s| s.id == selected_id) {
        Some(s) => s,
        None => return false,
    };
    if !matches!(selected.protocol, Protocol::Wireguard | Protocol::Tailscale) {
        return false;
    }
    !mesh_node_carries_full_tunnel(selected)
}

/// 混合收集 Rule + AppRule 的 targetServerId（enabled && action==proxy && targetServerId）。
/// 上游 `collectRuleTargetedServerIds([...customRules, ...appRules])`。
fn collect_targeted_mixed(
    custom_rules: &[crate::user_config::rule::Rule],
    app_rules: &[AppRule],
) -> BTreeSet<String> {
    let mut ids = collect_rule_targeted_server_ids(custom_rules);
    for r in app_rules {
        if r.enabled && r.action == RuleAction::Proxy {
            if let Some(tid) = &r.target_server_id {
                ids.insert(tid.clone());
            }
        }
    }
    ids
}

/// 剪枝后是否**还有任何规则把流量送去用户出口**（= 代理腿是否幸存）。
///
/// T2 fail-safe 的判据。递归进 logical rules 的子规则（与 [`collect_refs`] 同一套遍历形态：
/// 只查一半就会在逻辑规则里漏判）。
///
/// 只认 `outbound == exit_tag` 这一种「送去代理」：指向**具体节点 tag**（自定义规则 targetServerId）
/// 的规则不算 —— 少算的方向是**多触发一次 fail-safe**（final 改成代理），安全侧；反过来漏触发才是
/// 明文直连。判据宁可保守。
///
/// **钉死内部入站的规则一律不算**（`inbound` 非空 ⇒ `probe-direct-in` / `probe-proxy-in` /
/// `probe-in-<k>` / `update-in`，`:272`–`:311` 四处，全是应用自己的测速与更新流量）。它们恒指向代理、
/// 与用户流量无关；算进来会让「用户流量的代理腿已被剪光」这个事实被自家探针**永久掩盖** ——
/// fail-safe 从此再不触发，正是它要防的那个 fail-open。此处**必须**留在判据里。
fn routes_to_exit(rules: &[RouteRule], exit_tag: &str) -> bool {
    rules.iter().any(|r| {
        if r.inbound.is_some() {
            return false;
        }
        r.outbound.as_deref() == Some(exit_tag)
            || r.rules
                .as_deref()
                .is_some_and(|sub| routes_to_exit(sub, exit_tag))
    })
}

/// 递归收集 rules 中所有 rule_set 引用（string/array/logical 递归）。上游 `collectRefs`。
fn collect_refs(rules: &[RouteRule], referenced: &mut BTreeSet<String>) {
    for rule in rules {
        if let Some(sub) = rule.rules.as_deref() {
            collect_refs(sub, referenced);
        }
        match &rule.rule_set {
            Some(OneOrMany::One(t)) => {
                referenced.insert(t.clone());
            }
            Some(OneOrMany::Many(arr)) => {
                for t in arr {
                    referenced.insert(t.clone());
                }
            }
            None => {}
        }
    }
}

/// u32 端口列表 → OneOrMany（1 个用 One，否则 Many），镜像 sing-box JSON 形态。
fn ports_to_one_or_many(ports: Vec<u32>) -> OneOrMany<u32> {
    if ports.len() == 1 {
        OneOrMany::One(ports.into_iter().next().unwrap())
    } else {
        OneOrMany::Many(ports)
    }
}

/// BypassConfig 适配器（UserConfig → effective_bypass_lan）。
struct UConfigBypass<'a>(&'a UserConfig);
impl<'a> crate::user_config::system_proxy_bypass::BypassConfig for UConfigBypass<'a> {
    fn bypass_lan(&self) -> Option<bool> {
        self.0.bypass_lan
    }
    fn bypass_lan_list(&self) -> Option<&[String]> {
        self.0.bypass_lan_list.as_deref()
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::singbox::Endpoint;
    use crate::user_config::app_config::UserConfig;
    use crate::user_config::proxy_mode::ProxyMode;
    use crate::user_config::rule::{AppRule, CustomAppPreset};

    fn noop_log(_: LogLevel, _: &str) {}
    fn noop_degraded() {}

    fn deps_default<'a>(pending: &'a [Endpoint]) -> RouteConfigDeps<'a> {
        RouteConfigDeps {
            probe_direct_port: Some(7890),
            probe_proxy_port: Some(7891),
            update_in_port: None,
            probe_pool_ports: vec![],
            lan_resolver_for_dns: None,
            pending_endpoints: pending,
            log: noop_log,
            on_degraded: noop_degraded,
            race_upstream_ips: vec![],
            race_upstream_ports: vec![],
            runtime_rules_dir: "/fake/rules".to_string(),
            rule_resources_path: "/fake/res".to_string(),
            custom_rules_dir: "/fake/custom-rules".to_string(),
            arch: "x64".to_string(),
            platform: "linux".to_string(),
            is_valid_srs_fn: |_| false,
        }
    }

    fn empty_id_map() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn direct_mode_final_is_direct() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Direct;
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        assert_eq!(rc.final_outbound.as_deref(), Some("direct"));
        assert_eq!(rc.default_domain_resolver.as_deref(), Some("dns-bootstrap"));
        assert_eq!(rc.auto_detect_interface, Some(true));
        // direct 模式不注入 rule_set。
        assert!(rc.rule_set.is_none());
    }

    #[test]
    fn global_mode_final_is_selector() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Global;
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        assert_eq!(rc.final_outbound.as_deref(), Some("proxy-selector"));
    }

    #[test]
    fn smart_mode_final_is_selector() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Smart;
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        assert_eq!(rc.final_outbound.as_deref(), Some("proxy-selector"));
    }

    #[test]
    fn sniff_is_first_rule() {
        let config = UserConfig::default();
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        assert_eq!(rc.rules[0].action.as_deref(), Some("sniff"));
    }

    #[test]
    fn probe_routes_when_ports_present() {
        let config = UserConfig::default();
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        // 探针钉死路由（probe-direct-in → direct, probe-proxy-in → proxy-selector）紧随 sniff。
        let probe_direct = rc.rules.iter().find(|r| {
            r.inbound.as_ref().map(|o| match o {
                OneOrMany::One(s) => s == "probe-direct-in",
                OneOrMany::Many(v) => v.iter().any(|s| s == "probe-direct-in"),
            }) == Some(true)
        });
        assert!(probe_direct.is_some());
        assert_eq!(probe_direct.unwrap().outbound.as_deref(), Some("direct"));
    }

    #[test]
    fn probe_routes_absent_when_ports_missing() {
        let config = UserConfig::default();
        let mut deps = deps_default(&[]);
        deps.probe_direct_port = None;
        deps.probe_proxy_port = None;
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let has_probe = rc.rules.iter().any(|r| {
            r.inbound.as_ref().map(|o| match o {
                OneOrMany::One(s) => s == "probe-direct-in",
                OneOrMany::Many(v) => v.iter().any(|s| s == "probe-direct-in"),
            }) == Some(true)
        });
        assert!(!has_probe);
    }

    #[test]
    fn hijack_dns_rule_present() {
        let config = UserConfig::default();
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let hijack = rc
            .rules
            .iter()
            .find(|r| r.action.as_deref() == Some("hijack-dns"));
        assert!(hijack.is_some());
        assert_eq!(hijack.unwrap().port, Some(OneOrMany::Many(vec![53])));
    }

    #[test]
    fn core_process_direct_rule_present() {
        let config = UserConfig::default();
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let core = rc.rules.iter().find(|r| match r.process_name.as_ref() {
            Some(OneOrMany::Many(v)) => v == &["sing-box".to_string(), "sing-box.exe".to_string()],
            _ => false,
        });
        assert!(core.is_some());
        assert_eq!(core.unwrap().outbound.as_deref(), Some("direct"));
    }

    #[test]
    fn node_domain_exclusion() {
        let mut config = UserConfig::default();
        config
            .servers
            .push(crate::user_config::server_config::ServerConfig {
                id: "s1".into(),
                name: "HK".into(),
                protocol: crate::user_config::server_config::Protocol::Vless,
                address: "hk.example.com".into(),
                port: 443,
                ..Default::default()
            });
        let mut id_map = BTreeMap::new();
        id_map.insert("s1".to_string(), "HK".to_string());
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &id_map, &deps);
        let domain_rule = rc.rules.iter().find(|r| {
            r.domain
                .as_ref()
                .map(|d| d.contains(&"hk.example.com".to_string()))
                .unwrap_or(false)
        });
        assert!(domain_rule.is_some());
        assert_eq!(domain_rule.unwrap().outbound.as_deref(), Some("direct"));
    }

    #[test]
    fn node_ip_exclusion() {
        let mut config = UserConfig::default();
        config
            .servers
            .push(crate::user_config::server_config::ServerConfig {
                id: "s1".into(),
                name: "HK".into(),
                protocol: crate::user_config::server_config::Protocol::Vless,
                address: "1.2.3.4".into(),
                port: 443,
                ..Default::default()
            });
        let mut id_map = BTreeMap::new();
        id_map.insert("s1".to_string(), "HK".to_string());
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &id_map, &deps);
        let ip_rule = rc.rules.iter().find(|r| {
            r.ip_cidr
                .as_ref()
                .map(|c| c.contains(&"1.2.3.4/32".to_string()))
                .unwrap_or(false)
        });
        assert!(ip_rule.is_some());
        assert_eq!(ip_rule.unwrap().outbound.as_deref(), Some("direct"));
    }

    #[test]
    fn ukey_domain_override_address() {
        let config = UserConfig::default();
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let ukey = rc.rules.iter().find(|r| {
            r.domain_suffix
                .as_ref()
                .map(|d| d.contains(&".microdone.cn".to_string()))
                .unwrap_or(false)
        });
        assert!(ukey.is_some());
        assert_eq!(ukey.unwrap().override_address.as_deref(), Some("127.0.0.1"));
    }

    #[test]
    fn icmp_fallback_direct() {
        let config = UserConfig::default();
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let icmp = rc.rules.iter().find(|r| {
            r.network
                .as_ref()
                .map(|n| n.contains(&"icmp".to_string()))
                .unwrap_or(false)
        });
        assert!(icmp.is_some());
        assert_eq!(icmp.unwrap().outbound.as_deref(), Some("direct"));
    }

    #[test]
    fn dns_protocol_direct() {
        let config = UserConfig::default();
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let dns = rc.rules.iter().find(|r| {
            r.protocol.as_deref() == Some("dns") && r.outbound.as_deref() == Some("direct")
        });
        assert!(dns.is_some());
    }

    /// 🔴 出口选阻断的**行为级**断言：代理流量一条都不许出去，直连规则仍生效。
    ///
    /// 与实现无关地写：不问「selector 的 default 是什么」，只问**产物里还有没有一条能把流量
    /// 送到代理出口的路**。2026-08-13 把阻断从「selector.default = block 出站」改成规则级 reject，
    /// 本条在两种实现下语义相同 —— 这正是它的价值：它钉的是承诺，不是机制。
    #[test]
    fn block_exit_rejects_all_proxy_bound_traffic() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Smart;
        config.selected_server_id = Some("__block__".into());
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);

        assert!(
            !rc.rules
                .iter()
                .any(|r| r.outbound.as_deref() == Some(PROXY_SELECTOR_TAG)),
            "仍有规则把流量路由到 proxy-selector —— 用户选了阻断，这些流量会照常出网"
        );
        assert_eq!(
            rc.final_outbound.as_deref(),
            Some("direct"),
            "final 必须落在 direct：万一兜底那条被删，退化方向应是直连而不是静默走代理"
        );
        let last = rc.rules.last().expect("规则表不该为空");
        assert_eq!(last.action.as_deref(), Some("reject"), "末尾缺兜底 reject");
        assert!(
            last.domain_suffix.is_none()
                && last.domain_keyword.is_none()
                && last.ip_cidr.is_none()
                && last.port.is_none()
                && last.network.is_none()
                && last.protocol.is_none(),
            "兜底那条带了匹配器 ⇒ 匹配不到的流量会落到 final，阻断就漏了"
        );
        // 直连侧仍活着（文案承诺「代理流量已丢弃 · 直连规则仍生效」）。
        assert!(
            rc.rules
                .iter()
                .any(|r| r.outbound.as_deref() == Some("direct")),
            "一条直连规则都没剩 —— 与「直连规则仍生效」这句用户可见文案不符"
        );
    }

    /// 🔴 浏览器 DoH 拦截的**正向对照**：开关打开时必须真发射，且形态正确。
    ///
    /// 与 `no_builtin_domain_reject_table`（守「关的时候没有」）配对。只有反向那条时，
    /// 把发射逻辑整段删掉照样全绿 —— 那就是一个「永远不会红」的假门。
    ///
    /// 钉四件事：① 发两条（TCP 443/853 + UDP443）；② 用 `domain_suffix` **不是** `domain_keyword`
    /// （keyword 面太宽，用户填个短词就误伤一片，而后果他看不见）；③ 未编辑清单时用内置起点；
    /// ④ 两条都排在**自定义规则之前** —— 排在后面的话，一条把该域名路由到代理的自定义规则
    /// 就能让 DoH-over-QUIC 漏过去，用户开了开关却半通半不通。
    #[test]
    fn browser_doh_block_emits_only_when_switched_on() {
        let doh_rules = |cfg: &UserConfig| -> Vec<RouteRule> {
            let deps = deps_default(&[]);
            build_route_config(cfg, &empty_id_map(), &deps)
                .rules
                .into_iter()
                .filter(|r| {
                    r.action.as_deref() == Some("reject")
                        && r.domain_suffix
                            .as_ref()
                            .map(|v| v.iter().any(|d| d == "dns.google"))
                            .unwrap_or(false)
                })
                .collect()
        };

        // 关（默认）：一条都不该有。
        let mut off = UserConfig::default();
        off.proxy_mode = ProxyMode::Smart;
        assert!(
            doh_rules(&off).is_empty(),
            "开关默认关，却发射了 DoH reject"
        );

        // 开 + 未编辑清单 → 内置起点，两条。
        let mut on = UserConfig::default();
        on.proxy_mode = ProxyMode::Smart;
        on.block_browser_doh = Some(true);
        let hits = doh_rules(&on);
        assert_eq!(
            hits.len(),
            2,
            "开关打开应发 TCP+UDP 两条，实为 {}",
            hits.len()
        );
        assert!(
            hits.iter().all(|r| r.domain_keyword.is_none()),
            "用了 domain_keyword —— 本清单是用户可编辑的，keyword 的误伤面用户看不见"
        );
        assert!(
            hits.iter()
                .any(|r| r.port == Some(OneOrMany::Many(vec![443, 853]))),
            "缺 DoH(443)+DoT(853) 那条"
        );
        assert!(
            hits.iter()
                .any(|r| r.network.as_deref() == Some(["udp".to_string()].as_slice())),
            "缺 DoH-over-QUIC(UDP443) 那条"
        );
        assert!(
            hits[0]
                .domain_suffix
                .as_ref()
                .is_some_and(|v| v.len() == DEFAULT_BROWSER_DOH_SUFFIXES.len()),
            "未编辑清单时应当用内置起点全量"
        );

        // 开 + 用户自定清单 → 以用户的为准（并归一化）。
        let mut custom = on.clone();
        custom.browser_doh_list = Some(vec!["  DNS.Google  ".into(), String::new()]);
        let hits = doh_rules(&custom);
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].domain_suffix.as_deref(),
            Some(["dns.google".to_string()].as_slice()),
            "用户清单未被 trim/小写归一化，或没盖过内置起点"
        );

        // 开 + 用户清空清单 → 等于不拦（尊重用户把它清空这个动作）。
        let mut emptied = on.clone();
        emptied.browser_doh_list = Some(vec![]);
        assert!(doh_rules(&emptied).is_empty(), "用户清空了清单却仍在拦");

        // 次序：两条都必须排在自定义规则之前。
        use crate::user_config::rule::{Rule, RuleType};
        let mut ordered = on.clone();
        ordered.custom_rules = vec![Rule {
            id: "r-doh".into(),
            type_field: RuleType::DomainSuffix,
            values: vec!["dns.google".into()],
            conditions: None,
            combine_mode: None,
            action: RuleAction::Proxy,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        }];
        let deps = deps_default(&[]);
        let rules = build_route_config(&ordered, &empty_id_map(), &deps).rules;
        let last_reject = rules
            .iter()
            .rposition(|r| {
                r.action.as_deref() == Some("reject")
                    && r.domain_suffix
                        .as_ref()
                        .map(|v| v.iter().any(|d| d == "dns.google"))
                        .unwrap_or(false)
            })
            .expect("开关打开却找不到 DoH reject");
        let custom_hit = rules
            .iter()
            .position(|r| {
                r.outbound.is_some()
                    && r.domain_suffix
                        .as_ref()
                        .is_some_and(|v| v.iter().any(|d| d == "dns.google"))
            })
            .expect("自定义规则没发射（本用例的前提）");
        assert!(
            last_reject < custom_hit,
            "DoH reject 排到了自定义规则之后（{last_reject} vs {custom_hit}）—— \
             一条把该域名路由到代理的自定义规则会让 DoH-over-QUIC 漏过去"
        );
    }

    /// 🔴 **不得存在任何内置的域名 reject 表**（2026-08-13 用户裁定，整块移除三张）。
    ///
    /// 被移除的三张：DoH 泄漏域名的 443/853 reject、同一批域名的 UDP443 reject、
    /// Chrome/Edge 后台 beacon 的 14 个 Google 域名。共同点是**硬编码 + 无任何用户开关**。
    ///
    /// # 本门守的是「不许重建」，判据取产物
    ///
    /// 判据 = 生成的路由规则里，凡 `action == "reject"` 者**都必须由用户开关或用户自定义规则产生**，
    /// 不得出现「带域名匹配器且无人可关」的 reject。默认配置（全部开关关闭、无自定义规则）下
    /// 一条带域名匹配的 reject 都不该有 —— 这正是重建那三张表时必然违反的那一格。
    ///
    /// 不用「grep 域名字面量」当判据：换一批域名就绕过去了，而问题从来不是**哪些**域名，
    /// 是**有没有一张用户关不掉的表**。
    #[test]
    fn no_builtin_domain_reject_table() {
        for mode in [ProxyMode::Global, ProxyMode::Smart, ProxyMode::Direct] {
            let mut config = UserConfig::default();
            config.proxy_mode = mode;
            // 显式关掉两个会合法产出 reject 的开关，把剩下的任何 reject 都暴露出来。
            config.block_quic = Some(false);
            config.webrtc_leak_protection = Some("off".to_string());
            let deps = deps_default(&[]);
            let rc = build_route_config(&config, &empty_id_map(), &deps);

            let offenders: Vec<_> = rc
                .rules
                .iter()
                .filter(|r| r.action.as_deref() == Some("reject"))
                .filter(|r| {
                    r.domain.is_some()
                        || r.domain_suffix.is_some()
                        || r.domain_keyword.is_some()
                        || r.domain_regex.is_some()
                })
                .collect();

            assert!(
                offenders.is_empty(),
                "{mode:?} 模式下出现了 {} 条带域名匹配器的内置 reject —— \
                 用户关不掉的域名黑名单已被重建：{offenders:#?}",
                offenders.len()
            );
        }
    }

    #[test]
    fn block_quic_emits_fallback_reject() {
        let mut config = UserConfig::default();
        config.block_quic = Some(true);
        config.proxy_mode = ProxyMode::Global;
        config
            .servers
            .push(crate::user_config::server_config::ServerConfig {
                id: "s1".into(),
                name: "HK".into(),
                protocol: crate::user_config::server_config::Protocol::Vless,
                address: "1.2.3.4".into(),
                port: 443,
                ..Default::default()
            });
        let mut id_map = BTreeMap::new();
        id_map.insert("s1".to_string(), "HK".to_string());
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &id_map, &deps);
        // blockProxyQuic 兜底：末尾应有裸 udp443 reject（无 matcher）。
        let bare_udp443 = rc.rules.iter().any(|r| {
            r.action.as_deref() == Some("reject")
                && r.network.as_deref() == Some(["udp".to_string()].as_slice())
                && r.port.as_ref().map(|p| match p {
                    OneOrMany::One(p) => *p == 443,
                    OneOrMany::Many(p) => p.as_slice() == [443],
                }) == Some(true)
        });
        assert!(bare_udp443);
    }

    #[test]
    fn webrtc_proxy_emits_stun_route() {
        let mut config = UserConfig::default();
        config.webrtc_leak_protection = Some("proxy".to_string());
        config.proxy_mode = ProxyMode::Global;
        config
            .servers
            .push(crate::user_config::server_config::ServerConfig {
                id: "s1".into(),
                name: "HK".into(),
                protocol: crate::user_config::server_config::Protocol::Vless,
                address: "1.2.3.4".into(),
                port: 443,
                ..Default::default()
            });
        let mut id_map = BTreeMap::new();
        id_map.insert("s1".to_string(), "HK".to_string());
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &id_map, &deps);
        let stun = rc.rules.iter().find(|r| {
            r.protocol.as_deref() == Some("stun") && r.outbound.as_deref() == Some("proxy-selector")
        });
        assert!(stun.is_some());
    }

    #[test]
    fn webrtc_block_emits_stun_reject() {
        let mut config = UserConfig::default();
        config.webrtc_leak_protection = Some("block".to_string());
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let stun = rc.rules.iter().find(|r| {
            r.protocol.as_deref() == Some("stun") && r.action.as_deref() == Some("reject")
        });
        assert!(stun.is_some());
    }

    #[test]
    fn webrtc_off_no_stun_rule() {
        let config = UserConfig::default();
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let stun = rc
            .rules
            .iter()
            .any(|r| r.protocol.as_deref() == Some("stun"));
        assert!(!stun);
    }

    #[test]
    fn endpoint_force_route_preferred_by_wg() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Global;
        config
            .servers
            .push(crate::user_config::server_config::ServerConfig {
                id: "w1".into(),
                name: "WG".into(),
                protocol: Protocol::Wireguard,
                address: "1.2.3.4".into(),
                port: 443,
                wireguard_settings: Some(crate::user_config::server_config::WireGuardSettings {
                    allowed_ips: vec!["10.0.0.0/24".into()],
                    allow_internet: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            });
        let mut id_map = BTreeMap::new();
        id_map.insert("w1".to_string(), "WG".to_string());
        let endpoint = Endpoint {
            type_field: "wireguard".into(),
            tag: "WG".into(),
            ..Default::default()
        };
        let pending = [endpoint];
        let deps = deps_default(&pending);
        let rc = build_route_config(&config, &id_map, &deps);
        // WG 非全隧道 → preferred_by（allowInternet=true 但 allowed_ips 无 0/0 → carriesFullTunnel=allowInternet=true?）。
        // 注意：mesh_node_carries_full_tunnel = allowInternet（true），故此处 usePreferredBy=false → ip_cidr 路径。
        let force = rc.rules.iter().find(|r| {
            r.ip_cidr
                .as_ref()
                .map(|c| c.contains(&"10.0.0.0/24".to_string()))
                .unwrap_or(false)
        });
        assert!(force.is_some());
        assert_eq!(force.unwrap().outbound.as_deref(), Some("WG"));
    }

    #[test]
    fn endpoint_force_route_skipped_if_not_in_pending() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Global;
        config
            .servers
            .push(crate::user_config::server_config::ServerConfig {
                id: "w1".into(),
                name: "WG".into(),
                protocol: Protocol::Wireguard,
                address: "1.2.3.4".into(),
                port: 443,
                wireguard_settings: Some(crate::user_config::server_config::WireGuardSettings {
                    allowed_ips: vec!["10.0.0.0/24".into()],
                    ..Default::default()
                }),
                ..Default::default()
            });
        let mut id_map = BTreeMap::new();
        id_map.insert("w1".to_string(), "WG".to_string());
        let deps = deps_default(&[]); // 无 pending endpoint
        let rc = build_route_config(&config, &id_map, &deps);
        let force = rc.rules.iter().any(|r| {
            r.ip_cidr
                .as_ref()
                .map(|c| c.contains(&"10.0.0.0/24".to_string()))
                .unwrap_or(false)
        });
        assert!(!force);
    }

    #[test]
    fn local_geo_rule_set_injected_when_srs_valid() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Smart;
        let mut deps = deps_default(&[]);
        deps.is_valid_srs_fn = |_| true; // 所有 .srs 视为存在
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let rs = rc.rule_set.expect("smart 非直连应有 rule_set");
        assert!(rs.iter().any(|r| r.tag == "geosite-cn"));
        assert!(rs.iter().any(|r| r.tag == "geoip-cn"));
        assert!(rs.iter().any(|r| r.tag == "geosite-geolocation-!cn"));
    }

    #[test]
    fn local_geo_rule_set_absent_when_srs_invalid() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Smart;
        let deps = deps_default(&[]); // is_valid_srs_fn 默认 false
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        // 无内置注入、无自定义规则引用 → rule_set 为空（Polaris: [] 数组，所有 srs 被跳过）。
        let rs_len = rc.rule_set.as_deref().map(|v| v.len()).unwrap_or(0);
        assert_eq!(rs_len, 0);
    }

    // ───────── T2：规则资源缺失时 final fail-safe（真机 2026-07-20 全量明文直连的根治点）─────────

    /// 构造「smart + 回国（reverse）」配置。
    fn reverse_cn_config() -> UserConfig {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Smart;
        config.region_routing = Some(crate::user_config::region_routing::RegionRoutingConfig {
            enabled: true,
            region: "cn".into(),
            reverse: true,
        });
        config
    }

    /// **资源齐全 + reverse** → `final=direct` 是设计语义（海外直连），必须原样保留。
    /// 变异锁：把 fail-safe 的 `!pruned.is_empty()` 条件删掉（无条件翻）→ 此测转红。
    #[test]
    fn reverse_final_stays_direct_when_rule_sets_complete() {
        let config = reverse_cn_config();
        let mut deps = deps_default(&[]);
        deps.is_valid_srs_fn = |_| true; // 资源齐全
        let out = build_route_config_with_report(&config, &empty_id_map(), &deps);
        assert!(
            out.pruned_rule_set_tags.is_empty(),
            "资源齐全不该有剪枝：{:?}",
            out.pruned_rule_set_tags
        );
        assert_eq!(
            out.route.final_outbound.as_deref(),
            Some("direct"),
            "reverse 下 final=direct 是设计语义，资源齐全时不得改动"
        );
    }

    /// **资源缺失 + reverse** → 唯一把流量送代理的 geosite-cn/geoip-cn 规则被剪光，
    /// 若 final 仍是 direct 就是**全量明文直连**（fail-open）。必须 fail-safe 回退到 proxy-selector。
    /// 变异锁：删整个 fail-safe 块 / 把回退目标写成 "direct" → 转红。
    #[test]
    fn reverse_final_fails_safe_to_proxy_when_rule_sets_pruned() {
        let config = reverse_cn_config();
        let deps = deps_default(&[]); // is_valid_srs_fn 默认 false = 资源全缺
        let out = build_route_config_with_report(&config, &empty_id_map(), &deps);

        assert!(
            !out.pruned_rule_set_tags.is_empty(),
            "资源全缺必须报告剪枝（否则运行时层收不到信号）"
        );
        assert!(
            out.pruned_rule_set_tags.iter().any(|t| t == "geosite-cn"),
            "回国模式的 →代理 腿 geosite-cn 必须在剪枝清单里：{:?}",
            out.pruned_rule_set_tags
        );
        assert_eq!(
            out.route.final_outbound.as_deref(),
            Some(PROXY_SELECTOR_TAG),
            "资源缺失 + reverse 叠加 = fail-open 全量明文直连；final 必须回退为代理"
        );
        // 兜底断言语义：剪枝后确实没有任何规则再指向代理 —— 正因如此 final 才是唯一防线。
        let to_proxy = out
            .route
            .rules
            .iter()
            .any(|r| r.outbound.as_deref() == Some(PROXY_SELECTOR_TAG) && r.rule_set.is_some());
        assert!(!to_proxy, "剪枝后不该还有 rule_set 规则指向代理");

        // **内部入站排除腿的变异锁**：本场景里 `probe-proxy-in` 规则确实指向代理，若 `routes_to_exit`
        // 不排除钉死内部入站的规则，它就会返回 true ⇒ fail-safe 永不触发 ⇒ 上面那条 final 断言转红。
        // 这条断言把「探针规则存在」这个前提钉住，防后人删掉排除腿时误以为无人覆盖。
        let probe_to_proxy = out
            .route
            .rules
            .iter()
            .any(|r| r.inbound.is_some() && r.outbound.as_deref() == Some(PROXY_SELECTOR_TAG));
        assert!(
            probe_to_proxy,
            "前提：本场景应有钉死内部入站（probe-proxy-in）且指向代理的规则；\
             没有它，`routes_to_exit` 的 inbound 排除腿就没被本用例覆盖"
        );
        assert!(
            !routes_to_exit(&out.route.rules, PROXY_SELECTOR_TAG),
            "用户流量已无代理腿（自家探针不算）—— 这正是 fail-safe 必须触发的判据"
        );
    }

    /// 造一条引用「非内置」geo 类目的自定义规则（`config.rule_resources` 里也没有 ⇒ 必悬空 ⇒ 必被剪）。
    /// 用它模拟「用户装了一条引用未下载 geo 分类的规则」——真实且高频的剪枝来源。
    fn dangling_custom_geo_rule() -> crate::user_config::rule::Rule {
        use crate::user_config::rule::{Rule, RuleType};
        Rule {
            id: "r-dangling".into(),
            type_field: RuleType::Geosite,
            // bilibili 不在 builtin_geo_rulesets() 表内 ⇒ 不会被随包腿注入。
            values: vec!["bilibili".into()],
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

    /// **R4：内置 geo 在随包播种目录缺失时，必须回落「规则资源」页下载的本地副本。**
    ///
    /// 不做这条，给用户的指引就是死路：剪枝 warn 与 `RULE_RESOURCES_MISSING` 都写「到「规则资源」页
    /// 下载后重连恢复」，而下载腿一律落 `rule_resources_path`、内置基线却只读 `runtime_rules_dir`
    /// ⇒ 用户下载成功、再连仍被剪。
    ///
    /// 变异锁：删内置注入腿里的 `add_local_geo_rule_set(...)` 回落调用 → geosite-cn 重新被剪 → 转红。
    #[test]
    fn builtin_geo_falls_back_to_downloaded_rule_resource() {
        use crate::user_config::rule::{RuleResource, RuleResourceFormat};
        let mut config = reverse_cn_config();
        config.rule_resources = vec![RuleResource {
            id: "geosite-cn".into(), // catalog id 与 builtin tag 同形（rule_resource_catalog.rs 模块头）
            name: "geosite-cn".into(),
            category: "geosite".into(),
            source_url: "https://example.invalid/geosite-cn.srs".into(),
            file_name: "geosite-cn.srs".into(),
            format: RuleResourceFormat::Binary,
            size: 1,
            downloaded_at: "2026-07-20T00:00:00Z".into(),
        }];
        let mut deps = deps_default(&[]);
        // 随包播种目录**全空**（异常打包 / 播种失败），规则资源目录里有用户下载的那一份。
        deps.is_valid_srs_fn = |p| p.starts_with("/fake/res/");
        let out = build_route_config_with_report(&config, &empty_id_map(), &deps);

        let injected = out
            .route
            .rule_set
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .find(|r| r.tag == "geosite-cn");
        assert_eq!(
            injected.and_then(|r| r.path.as_deref()),
            Some("/fake/res/geosite-cn.srs"),
            "随包缺失时内置 tag 必须回落规则资源页的本地副本"
        );
        assert!(
            !out.pruned_rule_set_tags.iter().any(|t| t == "geosite-cn"),
            "回落成功就不该再被剪：{:?}",
            out.pruned_rule_set_tags
        );
        // 没下载的那些内置 tag 照常 fail-closed 剪掉（回落不是「无条件放行」）。
        assert!(
            out.pruned_rule_set_tags.iter().any(|t| t == "geoip-cn"),
            "未下载且随包缺失的内置 tag 仍必须被剪：{:?}",
            out.pruned_rule_set_tags
        );
    }

    /// `routes_to_exit` 三条腿的直测（判据本身，不经 builder）。
    ///
    /// 递归腿单列在这里的原因：当前 builder 生成的嵌套 logical 规则（`:568` 的 udp443 配对）
    /// 子规则只有 matcher、不带 `outbound` ⇒ 递归腿**按构造走不到**，只靠 builder 级用例覆盖不了它
    /// （实测变异：删掉递归，builder 那批全绿）。判据是纯函数，直测比留一条无门的分支便宜得多。
    #[test]
    fn routes_to_exit_covers_top_level_nested_and_inbound_exclusion() {
        let plain_proxy = RouteRule {
            action: Some("route".into()),
            outbound: Some(PROXY_SELECTOR_TAG.into()),
            ..empty_matcher()
        };
        let plain_direct = RouteRule {
            action: Some("route".into()),
            outbound: Some("direct".into()),
            ..empty_matcher()
        };
        let probe_pinned = RouteRule {
            inbound: Some(OneOrMany::Many(vec!["probe-proxy-in".into()])),
            action: Some("route".into()),
            outbound: Some(PROXY_SELECTOR_TAG.into()),
            ..empty_matcher()
        };
        let nested_proxy = RouteRule {
            type_field: Some("logical".into()),
            mode: Some("and".into()),
            rules: Some(vec![plain_proxy.clone()]),
            ..empty_matcher()
        };

        assert!(
            routes_to_exit(std::slice::from_ref(&plain_proxy), PROXY_SELECTOR_TAG),
            "顶层腿"
        );
        assert!(
            routes_to_exit(std::slice::from_ref(&nested_proxy), PROXY_SELECTOR_TAG),
            "递归腿"
        );
        assert!(
            !routes_to_exit(std::slice::from_ref(&plain_direct), PROXY_SELECTOR_TAG),
            "指向别处的规则不得算数"
        );
        assert!(
            !routes_to_exit(std::slice::from_ref(&probe_pinned), PROXY_SELECTOR_TAG),
            "钉死内部入站（探针/更新）的规则不承载用户流量，不得算数"
        );
        assert!(
            !routes_to_exit(
                &[
                    RouteRule {
                        inbound: Some(OneOrMany::Many(vec!["probe-proxy-in".into()])),
                        rules: Some(vec![nested_proxy]),
                        ..empty_matcher()
                    },
                    plain_direct,
                    probe_pinned,
                ],
                PROXY_SELECTOR_TAG
            ),
            "内部入站的排除必须先于递归——否则嵌在探针规则里的代理出站会被误算成用户腿"
        );
    }

    /// **R2 反向门：fail-safe 不得被「任意悬空 tag」触发。**
    ///
    /// 场景：smart + 回国 + 28 个内置 geo 全部正常，用户另有一条引用未下载 geo 分类的自定义规则。
    /// 旧判据 `!pruned.is_empty()` 在此为真 ⇒ `final` 从 direct 被翻成 proxy-selector ⇒
    /// **全部海外流量改走国内节点**，把回国模式的「海外直连」语义整体反转。而真正的「→代理」腿
    /// （`geosite-cn`/`geoip-cn`）完好无损、根本没有 fail-open —— 这是纯粹的误伤。
    ///
    /// 变异锁：把 T2 的 `!routes_to_exit(&rules, user_exit_tag)` 删掉（退回「有剪枝就翻」）→ 转红。
    #[test]
    fn reverse_final_stays_direct_when_only_unrelated_tag_pruned() {
        let mut config = reverse_cn_config();
        config.custom_rules = vec![dangling_custom_geo_rule()];
        let mut deps = deps_default(&[]);
        // 随包内置全在（`/fake/rules/...`），规则资源目录空（`/fake/res/...` 全无效）。
        deps.is_valid_srs_fn = |p| p.starts_with("/fake/rules/");
        let out = build_route_config_with_report(&config, &empty_id_map(), &deps);

        assert!(
            out.pruned_rule_set_tags
                .iter()
                .any(|t| t == "geosite-bilibili"),
            "该自定义规则引用的 geo 必须被剪（本用例的前提）：{:?}",
            out.pruned_rule_set_tags
        );
        assert!(
            !out.pruned_rule_set_tags.iter().any(|t| t == "geosite-cn"),
            "回国模式的 →代理 腿必须完好（本用例的另一半前提）：{:?}",
            out.pruned_rule_set_tags
        );
        assert_eq!(
            out.route.final_outbound.as_deref(),
            Some("direct"),
            "「→代理」腿完好时 final=direct 仍是设计语义；翻成代理 = 海外流量被误送国内节点"
        );
        // 判据自证：确实还有规则指向用户出口 —— fail-safe 正是因此才不该触发。
        assert!(
            routes_to_exit(&out.route.rules, PROXY_SELECTOR_TAG),
            "前提失守：剪枝后已无规则指向代理，那本用例就不该期望 final 保持 direct"
        );
    }

    /// **R3：组网出口回退（D4/D7）时 fail-safe 无处可退**，不得静默 no-op、更不得打「已回退为代理」。
    ///
    /// `mesh_selected_exit_falls_back_to_direct` 为真 ⇒ `user_exit_tag == "direct"` ⇒ 把 direct 写成
    /// direct 是 no-op，而旧代码在写之前已经打了「默认出口已回退为代理」—— 日志说谎，比不打更糟。
    ///
    /// 变异锁：删 `user_exit_tag == "direct"` 分流腿 → 走进 `routes_to_exit(rules, "direct")`
    /// （恒真：smart 模式有大量直连规则）→ 一句 warn 都不发 → 下面的日志断言转红。
    #[test]
    fn mesh_exit_fallback_logs_cannot_fail_safe_instead_of_lying() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Smart;
        config.selected_server_id = Some("w1".into());
        config.custom_rules = vec![dangling_custom_geo_rule()];
        config
            .servers
            .push(crate::user_config::server_config::ServerConfig {
                id: "w1".into(),
                name: "WG".into(),
                protocol: Protocol::Wireguard,
                address: "1.2.3.4".into(),
                port: 443,
                wireguard_settings: Some(crate::user_config::server_config::WireGuardSettings {
                    allowed_ips: vec!["10.0.0.0/24".into()],
                    // 关外网 ⇒ carriesFullTunnel=false ⇒ 用户出口整体回退 direct。
                    allow_internet: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            });

        // `RouteConfigDeps.log` 是裸 fn 指针，闭包捕获不了 ⇒ 用 thread_local 收集（测试单线程内自洽）。
        thread_local! {
            static SINK: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        fn capture(_lvl: LogLevel, msg: &str) {
            SINK.with(|s| s.borrow_mut().push(msg.to_string()));
        }
        SINK.with(|s| s.borrow_mut().clear());

        let mut deps = deps_default(&[]);
        deps.is_valid_srs_fn = |p| p.starts_with("/fake/rules/");
        deps.log = capture;
        let out = build_route_config_with_report(&config, &empty_id_map(), &deps);
        let captured = SINK.with(|s| s.borrow().clone());

        assert!(
            mesh_selected_exit_falls_back_to_direct(&config),
            "前提：本场景必须触发组网出口回退"
        );
        assert!(
            !out.pruned_rule_set_tags.is_empty(),
            "前提：本场景必须发生剪枝"
        );
        assert_eq!(
            out.route.final_outbound.as_deref(),
            Some("direct"),
            "出口本身就是 direct，final 只能是 direct（写 direct 到 direct 是 no-op）"
        );
        assert!(
            captured.iter().any(|m| m.contains("无法回退为代理")),
            "必须如实告知「无法 fail-safe」：{captured:?}"
        );
        assert!(
            !captured.iter().any(|m| m.contains("默认出口已回退为代理")),
            "绝不能打「已回退为代理」——什么都没回退，这是日志说谎：{captured:?}"
        );
    }

    /// **`proxy_mode=direct`（用户显式全直连）→ final 必须保持 direct**，那是用户意图不是降级。
    ///
    /// **本用例锁的是「让 fail-safe 在 direct 模式下不可达」的上游不变式**，而不是 fail-safe 的
    /// `proxy_mode != "direct"` 守卫本身——实测确认（变异 M2）：删掉那个守卫，**没有任何测试转红**，
    /// 因为 direct 模式下压根产生不出悬空引用：
    ///   - 内置 geo rule_set 不注入（`:951` 的 `proxy_mode != "direct"` 门），**但也没有规则引用它们**；
    ///   - 自定义规则 / 应用分流整块**仅 smart 模式发**（`:529` `if proxy_mode == "smart"`）；
    ///   - 地区分流 geo 基线同样仅 smart（`:830`）。
    ///
    /// ⟹ direct 模式的 `dangling` 恒空 ⟹ fail-safe 恒不进入 ⟹ 那个守卫是**按构造不可达的
    /// defense-in-depth**，不是被覆盖的分支。**别把它当成有牙的变异锁。** 真正守住用户意图的是本不变式：
    /// 下面这条断言若哪天转红（= 有人让 direct 模式也发引用 geo 的规则），就必须回头重新审视那个守卫
    /// 到底还够不够——那时它才会从「不可达」变成「唯一防线」。
    #[test]
    fn direct_mode_never_prunes_so_final_stays_direct() {
        use crate::user_config::rule::{Rule, RuleType};
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Direct;
        // 尽最大努力制造悬空引用：挂一条引用 geosite 类目的自定义规则 + 开启回国地区分流。
        config.custom_rules = vec![Rule {
            id: "r-geo".into(),
            type_field: RuleType::Geosite,
            values: vec!["youtube".into()],
            conditions: None,
            combine_mode: None,
            action: RuleAction::Proxy,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: None,
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        }];
        config.region_routing = Some(crate::user_config::region_routing::RegionRoutingConfig {
            enabled: true,
            region: "cn".into(),
            reverse: true,
        });
        let deps = deps_default(&[]); // 资源全缺
        let out = build_route_config_with_report(&config, &empty_id_map(), &deps);

        assert!(
            out.pruned_rule_set_tags.is_empty(),
            "不变式失守：direct 模式竟产生了悬空 rule_set 引用（{:?}）——\
             fail-safe 的 `proxy_mode != \"direct\"` 守卫从此是真防线，需重新做变异验证",
            out.pruned_rule_set_tags
        );
        assert_eq!(
            out.route.final_outbound.as_deref(),
            Some("direct"),
            "用户显式选的全直连必须原样保留"
        );
    }

    /// 非 reverse 的 smart（正向）资源缺失：final 本就是 proxy-selector，fail-safe 不该改变它。
    /// 证明 fail-safe 只在「final 已落 direct」时出手，不是无条件覆写。
    #[test]
    fn forward_smart_final_unchanged_when_pruned() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Smart;
        let deps = deps_default(&[]); // 资源全缺
        let out = build_route_config_with_report(&config, &empty_id_map(), &deps);
        assert_eq!(
            out.route.final_outbound.as_deref(),
            Some(PROXY_SELECTOR_TAG)
        );
    }

    #[test]
    fn inactive_region_geo_excluded() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Smart;
        config.region_routing = Some(crate::user_config::region_routing::RegionRoutingConfig {
            enabled: true,
            region: "cn".into(),
            reverse: false,
        });
        let mut deps = deps_default(&[]);
        deps.is_valid_srs_fn = |_| true;
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let rs = rc.rule_set.unwrap();
        // region=cn 时，ir/ru 地区 geo 不注入。
        assert!(!rs.iter().any(|r| r.tag == "geosite-category-ir"));
        assert!(!rs.iter().any(|r| r.tag == "geoip-ru"));
    }

    #[test]
    fn app_rule_process_name_route() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Smart;
        config.app_routing_enabled = Some(true);
        config.app_rules.push(AppRule {
            app_id: "custom-app".into(),
            action: RuleAction::Proxy,
            enabled: true,
            target_server_id: None,
        });
        config.custom_app_presets.push(CustomAppPreset {
            id: "custom-app".into(),
            name: "MyApp".into(),
            emoji: "".into(),
            icon_url: None,
            geosite_tags: vec![],
            geoip_tags: vec![],
            process_names: Some(vec!["myapp".into()]),
            category: None,
        });
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let app_rule = rc.rules.iter().find(|r| match r.process_name.as_ref() {
            Some(OneOrMany::Many(v)) => v == &["myapp".to_string()],
            _ => false,
        });
        assert!(app_rule.is_some());
        assert_eq!(
            app_rule.unwrap().outbound.as_deref(),
            Some("rule-sel-app-custom-app")
        );
    }

    /// 应用分流「阻断」⇒ 规则级 `action:"reject"` + 无 outbound + **不配对 udp443 reject**。
    ///
    /// 三条断言各锁一处（口径同 `custom_rules::block_action_emits_rule_level_reject_without_outbound`）：
    ///  - `action == "reject"`：退回 `"route"` 就是把该阻断的流量交给 `route.final`（proxy-selector）
    ///    ⇒ 静默走代理。
    ///  - `outbound is None`：残留 `"block"` ⇒ 引用一个已被上游废弃的 legacy special outbound。
    ///  - **只有一条命中 `myapp` 的规则**：`app_out_is_proxy` 若仍按 outbound 字面量反推
    ///    （Block 现在没有 outbound ⇒ 反推成「是代理」），就会在前面白插一条
    ///    `network:udp / port:443 / action:reject` 的配对规则 ⇒ 计数变 2 ⇒ 转红。
    #[test]
    fn app_rule_block_emits_rule_level_reject_and_no_udp443_pair() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Smart;
        config.app_routing_enabled = Some(true);
        config.block_quic = Some(true); // 打开 udp443 配对，否则第 3 条断言恒绿
        config.app_rules.push(AppRule {
            app_id: "custom-app".into(),
            action: RuleAction::Block,
            enabled: true,
            target_server_id: None,
        });
        config.custom_app_presets.push(CustomAppPreset {
            id: "custom-app".into(),
            name: "MyApp".into(),
            emoji: "".into(),
            icon_url: None,
            geosite_tags: vec![],
            geoip_tags: vec![],
            process_names: Some(vec!["myapp".into()]),
            category: None,
        });
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let hits: Vec<_> = rc
            .rules
            .iter()
            .filter(|r| match r.process_name.as_ref() {
                Some(OneOrMany::Many(v)) => v == &["myapp".to_string()],
                _ => false,
            })
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "阻断的应用规则不该被配对 udp443 reject（那是「走代理」才需要的）：{hits:?}"
        );
        assert_eq!(hits[0].action.as_deref(), Some("reject"));
        assert_eq!(
            hits[0].outbound, None,
            "reject 是规则级动作，不得再指向 legacy `block` 出站"
        );
        assert_eq!(
            hits[0].no_drop,
            Some(true),
            "阻断规则必须 no_drop:true 才与 legacy `block` 出站等价（默认会泛洪降级成 drop）"
        );
        // 反向：走代理的应用规则**不该**带 no_drop（那条是 route 动作，字段无意义）。
        let mut proxy_cfg = config.clone();
        proxy_cfg.app_rules[0].action = RuleAction::Proxy;
        let rc2 = build_route_config(&proxy_cfg, &empty_id_map(), &deps);
        let route_hit = rc2
            .rules
            .iter()
            .find(|r| {
                r.action.as_deref() == Some("route")
                    && matches!(r.process_name.as_ref(), Some(OneOrMany::Many(v)) if v == &["myapp".to_string()])
            })
            .expect("走代理的应用规则应存在");
        assert_eq!(
            route_hit.no_drop, None,
            "route 动作不该带 no_drop —— 无条件加会把无意义字段撒进每条规则"
        );
    }

    #[test]
    fn custom_domestic_dns_ip_added_to_direct_cidrs() {
        let mut config = UserConfig::default();
        config.dns_config = Some(crate::user_config::dns_config::DnsConfig {
            domestic_dns: Some("https://223.5.5.5/dns-query".into()),
            ..Default::default()
        });
        let deps = deps_default(&[]);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        // 223.5.5.5/32 应出现在 DNS 直连放行规则中（BOOTSTRAP_DIRECT_DNS_IPS 含 223.5.5.5，自定义 IP 重复也无妨）。
        let dns_direct = rc.rules.iter().find(|r| {
            r.ip_cidr
                .as_ref()
                .map(|c| c.contains(&"223.5.5.5/32".to_string()))
                .unwrap_or(false)
        });
        assert!(dns_direct.is_some());
    }

    /// 取 DNS 直连放行规则（`ip_cidr` 含引导 DNS + action=route→direct 的那条）。
    fn dns_direct_ports(rc: &RouteConfig) -> Vec<u32> {
        let rule = rc
            .rules
            .iter()
            .find(|r| {
                r.outbound.as_deref() == Some("direct")
                    && r.ip_cidr
                        .as_ref()
                        .is_some_and(|c| c.contains(&"223.5.5.5/32".to_string()))
            })
            .expect("DNS 直连放行规则必存在");
        match rule.port.as_ref().expect("该规则必带端口集") {
            OneOrMany::One(p) => vec![*p],
            OneOrMany::Many(v) => v.clone(),
        }
    }

    fn dns_config_with_custom_pool(pool: &[&str], custom: &[(&str, &str)]) -> UserConfig {
        let mut config = UserConfig::default();
        config.dns_config = Some(crate::user_config::dns_config::DnsConfig {
            node_resolver_pool: Some(pool.iter().map(|s| (*s).to_string()).collect()),
            node_resolver_custom: Some(
                custom
                    .iter()
                    .map(
                        |(id, spec)| crate::user_config::dns_config::CustomDnsUpstream {
                            id: (*id).to_string(),
                            spec: (*spec).to_string(),
                        },
                    )
                    .collect(),
            ),
            ..Default::default()
        });
        config
    }

    /// 【不变式：race 上游的 IP 与端口必须一起放行】
    ///
    /// 端口集写死 `[53,443]` 时，`https://9.9.9.9:8443/q` 与 `udp://9.9.9.9:5353` 的流量匹配不上
    /// 直连规则 → TUN 下经代理出站 → 起核自举窗内该上游恒 FAIL/回环。
    ///
    /// **变异锁**：删掉 `build_route` 里的 `dns_ports.extend(deps.race_upstream_ports…)` → 转红。
    #[test]
    fn race_custom_upstream_nonstandard_ports_are_direct_allowed() {
        let config = dns_config_with_custom_pool(
            &["ali", "my-doh", "my-udp"],
            &[
                ("my-doh", "https://9.9.9.9:8443/q"),
                ("my-udp", "udp://9.9.9.9:5353"),
            ],
        );
        let mut deps = deps_default(&[]);
        // race 就绪（sidecar 已起）：两轴均由 `polaris-dns-race` 的真实上游集下发。
        deps.race_upstream_ips = vec!["9.9.9.9".to_string()];
        deps.race_upstream_ports = vec![443, 8443, 5353];
        let rc = build_route_config(&config, &empty_id_map(), &deps);

        let ports = dns_direct_ports(&rc);
        assert!(
            ports.contains(&53) && ports.contains(&443),
            "恒定端口不得丢"
        );
        assert!(
            ports.contains(&8443),
            "自定义 DoH 非标端口须放行，实际: {ports:?}"
        );
        assert!(
            ports.contains(&5353),
            "自定义 UDP 非 53 口须放行，实际: {ports:?}"
        );
        // IP 也必须在（两轴缺一不可，否则规则照样匹配不上）。
        let rule_has_ip = rc.rules.iter().any(|r| {
            r.ip_cidr
                .as_ref()
                .is_some_and(|c| c.contains(&"9.9.9.9/32".to_string()))
        });
        assert!(rule_has_ip, "自定义上游 IP 须进直连放行");
        // 端口集仍须去重（`443` 由恒定集与 race 集各贡献一次）。
        let mut sorted = ports.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ports.len(), "端口集不得有重复项: {ports:?}");
    }

    /// race off（两轴皆空）→ 端口集**逐字节回现状**，金样不动。
    ///
    /// 配置里**故意留着**一个声明了非标端口的自定义上游：race off 时它不该有任何影响
    /// （sidecar 都没起，放行它的端口纯属白开口子）。
    #[test]
    fn race_off_leaves_dns_direct_ports_untouched() {
        let config =
            dns_config_with_custom_pool(&["my-doh"], &[("my-doh", "https://9.9.9.9:8443/q")]);
        let deps = deps_default(&[]); // race 两轴默认空
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        assert_eq!(
            dns_direct_ports(&rc),
            vec![53, 443],
            "race off 时不得叠加任何端口"
        );
    }

    /// 【不变式：端口集**只**来自 `deps.race_upstream_ports`，route 不得照 `dnsConfig` 复算】
    ///
    /// 曾经这里有一个 `race_custom_upstream_ports(config)`：就地从 `nodeResolverPool` + `nodeResolverCustom`
    /// 再导出一遍端口。它与 sidecar 侧 `resolve_upstreams` 的选择逻辑（Tier 分桶 / canonical 去重 /
    /// Tier1 上限 / TUN 摘 `system`）**刻意不一致**（取超集），于是同一件事有了两份真值源 —— 两边任
    /// 一侧改口径都不会让另一侧转红。现在端口随 IP 由 sidecar 一路下发，本 builder 只消费。
    ///
    /// 判据构造成「配置说一套、注入说另一套」：`dnsConfig` 里点名的是 `:9443`，注入进来的是 `:8443`。
    /// 只有真的不复算，输出才会跟着注入走。
    ///
    /// **变异锁**：把 `race_custom_upstream_ports(config)` 那行加回去 → `9443` 断言立刻转红。
    #[test]
    fn dns_direct_ports_come_only_from_deps_never_recomputed_from_config() {
        let config = dns_config_with_custom_pool(
            &["ali", "selected"],
            &[
                // 配置层面点名 :9443；但真实上游集（注入）里是 :8443。
                ("selected", "https://8.8.8.8:9443/q"),
                ("domain", "https://dns.google:9444/q"), // 域名 → sidecar 侧拒绝腿
                ("dot", "tls://1.1.1.1:8853"),           // DoT 二期 → sidecar 侧拒绝腿
            ],
        );
        let mut deps = deps_default(&[]);
        deps.race_upstream_ips = vec!["9.9.9.9".to_string()];
        deps.race_upstream_ports = vec![8443];
        let ports = dns_direct_ports(&build_route_config(&config, &empty_id_map(), &deps));
        assert!(
            ports.contains(&8443),
            "注入的端口必须落进规则，实际: {ports:?}"
        );
        for unwanted in [9443u32, 9444, 8853] {
            assert!(
                !ports.contains(&unwanted),
                "{unwanted} 只存在于 dnsConfig、不在注入的真实上游集里 → 不得被放行（复算复活的信号），\
                 实际: {ports:?}"
            );
        }
    }

    #[test]
    fn udp443_reject_rule_factory() {
        let matcher = RouteRule {
            process_name: Some(OneOrMany::One("chrome".into())),
            ..empty_matcher()
        };
        let rule = udp443_reject_rule(matcher);
        assert_eq!(rule.action.as_deref(), Some("reject"));
        assert_eq!(
            rule.network.as_deref(),
            Some(["udp".to_string()].as_slice())
        );
        assert_eq!(rule.port, Some(OneOrMany::Many(vec![443])));
        assert_eq!(rule.process_name, Some(OneOrMany::One("chrome".into())));
    }

    #[test]
    fn proxy_mode_str_mapping() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Smart;
        assert_eq!(proxy_mode_str(&config), "smart");
        config.proxy_mode = ProxyMode::Global;
        assert_eq!(proxy_mode_str(&config), "global");
        config.proxy_mode = ProxyMode::Direct;
        assert_eq!(proxy_mode_str(&config), "direct");
    }

    #[test]
    fn collect_refs_handles_nested_logical() {
        let rules = vec![RouteRule {
            rule_set: Some(OneOrMany::One("geosite-cn".into())),
            rules: Some(vec![RouteRule {
                rule_set: Some(OneOrMany::Many(vec![
                    "geoip-cn".into(),
                    "geosite-private".into(),
                ])),
                ..empty_matcher()
            }]),
            ..empty_matcher()
        }];
        let mut refs = BTreeSet::new();
        collect_refs(&rules, &mut refs);
        assert!(refs.contains("geosite-cn"));
        assert!(refs.contains("geoip-cn"));
        assert!(refs.contains("geosite-private"));
    }

    #[test]
    fn update_in_port_route() {
        let mut config = UserConfig::default();
        config.proxy_mode = ProxyMode::Global;
        let mut deps = deps_default(&[]);
        deps.update_in_port = Some(7892);
        let rc = build_route_config(&config, &empty_id_map(), &deps);
        let update = rc.rules.iter().find(|r| {
            r.inbound.as_ref().map(|o| match o {
                OneOrMany::One(s) => s == "update-in",
                OneOrMany::Many(v) => v.iter().any(|s| s == "update-in"),
            }) == Some(true)
        });
        assert!(update.is_some());
        assert_eq!(update.unwrap().outbound.as_deref(), Some("proxy-selector"));
    }

    /// update-in 那条腿的出站（None = 没生成该规则）。
    fn update_in_outbound(config: &UserConfig) -> Option<String> {
        let mut deps = deps_default(&[]);
        deps.update_in_port = Some(7892);
        let rc = build_route_config(config, &empty_id_map(), &deps);
        rc.rules
            .iter()
            .find(|r| {
                r.inbound.as_ref().map(|o| match o {
                    OneOrMany::One(s) => s == "update-in",
                    OneOrMany::Many(v) => v.iter().any(|s| s == "update-in"),
                }) == Some(true)
            })
            .and_then(|r| r.outbound.clone())
    }

    /// 【阻断出口豁免管理面】选阻断时 update-in 必须走 direct，不能跟着 proxy-selector 一起被 block。
    ///
    /// 变异锁：把 `proxy_mode == "direct" || exit_is_block` 里的 `|| exit_is_block` 删掉 →
    /// 该腿回到 "proxy-selector" → 转红。两种模式都测，因为 exit_is_block 与 proxy_mode 正交。
    #[test]
    fn block_exit_exempts_update_in_from_blocking() {
        for mode in [ProxyMode::Global, ProxyMode::Smart] {
            let mut config = UserConfig::default();
            config.proxy_mode = mode;
            config.selected_server_id = Some("__block__".into());
            assert_eq!(
                update_in_outbound(&config).as_deref(),
                Some("direct"),
                "proxy_mode={} 选阻断时订阅/更新腿必须豁免",
                mode.as_str()
            );
        }
    }

    /// 对照腿：**没选**阻断时 update-in 仍钉在 proxy-selector 上（豁免不得泄漏成无条件 direct）。
    ///
    /// 缺了这条，把 update-in 无条件改成 direct 也能让上面那条绿——订阅更新会永久绕过代理，
    /// 在墙内等于永久失效，且没有任何门会红。
    #[test]
    fn non_block_exit_keeps_update_in_on_proxy_selector() {
        for selected in [None, Some("__direct__"), Some("s1")] {
            let mut config = UserConfig::default();
            config.proxy_mode = ProxyMode::Global;
            config.selected_server_id = selected.map(str::to_string);
            assert_eq!(
                update_in_outbound(&config).as_deref(),
                Some("proxy-selector"),
                "selected={selected:?} 时 update-in 不该被改写"
            );
        }
    }
}
