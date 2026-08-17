//! sing-box Inbound 配置生成（上游 `singbox-inbounds-builder.ts` 1:1 移植）。
//!
//! 装配 mixed inbound（HTTP+SOCKS 同口）+ 探针 inbound（probe-direct/proxy-in/update-in）+
//! TUN inbound（平台相关排除段/MTU/stack/IPv6/macOS http_proxy platform）。

#![forbid(unsafe_code)]

use crate::builder::endpoint_routes::{
    collect_rule_targeted_server_ids, mesh_force_routed_servers, mesh_forced_route_cidrs,
};
use crate::builder::helpers::{
    effective_app_rules, effective_custom_rules, get_custom_domestic_dns_endpoint,
    host_to_exclude_cidr, is_ipv4_host, is_ipv6_host, probe_pool_inbound_tag,
};
use crate::builder::tun_route_exclude::{compute_user_tun_exclude, UserTunExcludeInput};
use crate::singbox::{HttpProxyPlatform, Inbound, InboundPlatform, UdpNatBehavior};
use crate::user_config::cidr::cidr_overlaps_any;
use crate::user_config::collections::dedupe;
use crate::user_config::dns_constants::{BOOTSTRAP_DIRECT_DNS_IPS, CONTROLLED_TUN_DNS_IP};
use crate::user_config::log_level::LogLevel;
use crate::user_config::neighbor::{is_tun_mac_filter_supported, is_valid_mac_address};
use crate::user_config::proxy_ports::{local_proxy_port, PortConfig};
use crate::user_config::rule::RuleAction;
use crate::user_config::rules::rule_ip_cidrs;
use crate::user_config::system_proxy_bypass::{
    bypass_lan_cidrs, effective_bypass_lan, BypassConfig,
};
use crate::user_config::tun_config::{
    resolve_win_tun_interface_name, UdpNatType, FAKEIP_INET4_RANGE, FAKEIP_INET6_RANGE,
};
use crate::user_config::tun_stack::{default_mtu_for, resolve_tun_stack};
use crate::user_config::ProxyModeType;
use crate::user_config::UserConfig;

/// buildInbounds 依赖注入（实例态 + FS）。上游 `InboundsDeps`。
pub struct InboundsDeps {
    pub probe_direct_port: Option<u16>,
    pub probe_proxy_port: Option<u16>,
    pub update_in_port: Option<u16>,
    pub probe_pool_ports: Vec<u16>,
    pub platform: String,
    /// 本机所有非回环接口 CIDR（os.networkInterfaces 注入；对拍固定假值）。上游 `getOwnLanCidrs`。
    pub own_lan_cidrs: Vec<String>,
    /// 日志回调（静默剔除告警：非法段/组网重叠/macOS 物理 LAN 重叠/非直连自定义规则重叠）。
    /// 上游 `deps.log`（`singbox-inbounds-builder.ts` L308-355）。
    pub log: fn(LogLevel, &str),
}

/// 生成 sing-box inbounds。上游 `buildInbounds`。
pub fn build_inbounds(
    config: &UserConfig,
    resolved_ips: Option<&std::collections::BTreeMap<String, String>>,
    deps: &InboundsDeps,
) -> Vec<Inbound> {
    let mut inbounds: Vec<Inbound> = Vec::new();
    let is_tun = matches!(config.proxy_mode_type, ProxyModeType::Tun);
    let listen_addr = if config.allow_lan == Some(true) {
        "::"
    } else {
        "127.0.0.1"
    };

    // mixed inbound（HTTP+SOCKS 同口）。
    inbounds.push(Inbound {
        type_field: "mixed".into(),
        tag: "mixed-in".into(),
        listen: Some(listen_addr.into()),
        listen_port: Some(local_proxy_port(config)),
        interface_name: None,
        address: None,
        mtu: None,
        auto_route: None,
        auto_redirect: None,
        strict_route: None,
        stack: None,
        udp_mapping: None,
        udp_filtering: None,
        route_exclude_address: None,
        include_mac_address: None,
        exclude_mac_address: None,
        platform: None,
    });

    // 探针 inbound（probe-direct/proxy-in）。
    if let (Some(dp), Some(pp)) = (deps.probe_direct_port, deps.probe_proxy_port) {
        inbounds.push(http_loopback("probe-direct-in", dp));
        inbounds.push(http_loopback("probe-proxy-in", pp));
    }

    // §15 探测池 probe-in-k。
    for (k, port) in deps.probe_pool_ports.iter().enumerate() {
        inbounds.push(http_loopback(&probe_pool_inbound_tag(k), *port));
    }

    // update-in（socks）。
    if let Some(up) = deps.update_in_port {
        inbounds.push(Inbound {
            type_field: "socks".into(),
            tag: "update-in".into(),
            listen: Some("127.0.0.1".into()),
            listen_port: Some(up),
            interface_name: None,
            address: None,
            mtu: None,
            auto_route: None,
            auto_redirect: None,
            strict_route: None,
            stack: None,
            udp_mapping: None,
            udp_filtering: None,
            route_exclude_address: None,
            include_mac_address: None,
            exclude_mac_address: None,
            platform: None,
        });
    }

    // TUN inbound。
    if is_tun {
        if let Some(tun) = build_tun_inbound(config, resolved_ips, deps) {
            inbounds.push(tun);
        }
    }

    inbounds
}

fn http_loopback(tag: &str, port: u16) -> Inbound {
    Inbound {
        type_field: "http".into(),
        tag: tag.into(),
        listen: Some("127.0.0.1".into()),
        listen_port: Some(port),
        interface_name: None,
        address: None,
        mtu: None,
        auto_route: None,
        auto_redirect: None,
        strict_route: None,
        stack: None,
        udp_mapping: None,
        udp_filtering: None,
        route_exclude_address: None,
        include_mac_address: None,
        exclude_mac_address: None,
        platform: None,
    }
}

/// 用户「NAT 类型」档 → sing-box `(udp_mapping, udp_filtering)` 取值对。
///
/// # 表本身（RFC 3489 §5 的锥形分类，逐档钉在 `udp_nat_type_maps_each_tier`）
///
/// | 档 | `udp_mapping` | `udp_filtering` |
/// |---|---|---|
/// | 全锥 | `endpoint_independent` | `endpoint_independent` |
/// | 受限锥 | `endpoint_independent` | `address_dependent` |
/// | 端口受限锥 | `endpoint_independent` | `address_and_port_dependent` |
///
/// **三档的 mapping 全是 `endpoint_independent`，这不是复制粘贴漏改**：锥形（cone）的定义就是
/// 「同一本地端口对所有目的地共用同一个外部映射」，三种锥的差别**只在 filtering**。mapping 一旦
/// 收紧就不再是锥形而是对称 NAT —— 那个档本仓刻意不提供（理由见 [`UdpNatType`] 的文档注释）。
///
/// # 为什么两个键一起发，而不是「只发变化的那个 filtering」
///
/// 只发 filtering 的话，「全锥」这一档会退化成「filtering 跟默认一样、mapping 听天由命」：档位名对
/// 用户的承诺（这是全锥）就依赖于上游默认恰好也是 `endpoint_independent`。上游改默认，档位名当场
/// 变成谎话且没有任何门会红。选了档 = 用户要一个**确定**的 NAT 形态，两个键一起钉死才兑现得了。
/// 反过来「不选档」仍是一个键都不发（见 [`build_tun_inbound`] 的 `None` 腿），那才是「跟随内核」。
fn udp_nat_behaviors(nat: UdpNatType) -> (UdpNatBehavior, UdpNatBehavior) {
    match nat {
        UdpNatType::FullCone => (
            UdpNatBehavior::EndpointIndependent,
            UdpNatBehavior::EndpointIndependent,
        ),
        UdpNatType::RestrictedCone => (
            UdpNatBehavior::EndpointIndependent,
            UdpNatBehavior::AddressDependent,
        ),
        UdpNatType::PortRestrictedCone => (
            UdpNatBehavior::EndpointIndependent,
            UdpNatBehavior::AddressAndPortDependent,
        ),
    }
}

fn build_tun_inbound(
    config: &UserConfig,
    resolved_ips: Option<&std::collections::BTreeMap<String, String>>,
    deps: &InboundsDeps,
) -> Option<Inbound> {
    let should_bypass_lan = config.bypass_lan != Some(false);
    let fakeip_ranges: Vec<String> = if uses_fake_ip(config) && deps.platform != "linux" {
        let mut r = vec![FAKEIP_INET4_RANGE.to_string()];
        if config.enable_ipv6 == Some(true) {
            r.push(FAKEIP_INET6_RANGE.to_string());
        }
        r
    } else {
        vec![]
    };

    // engaged mesh force-route 段：rule_targeted 含 custom + app 规则指向的节点。
    let mut rule_targeted = collect_rule_targeted_server_ids(&effective_custom_rules_proxy(config));
    for app in &effective_app_rules_proxy(config) {
        if app.enabled && app.action == RuleAction::Proxy {
            if let Some(tid) = &app.target_server_id {
                rule_targeted.insert(tid.clone());
            }
        }
    }
    let engaged_mesh = mesh_forced_route_cidrs(&mesh_force_routed_servers(
        &config.servers,
        config.selected_server_id.as_deref(),
        &rule_targeted,
    ));

    let mut exclude_addr: Vec<String> = if deps.platform == "win32" && should_bypass_lan {
        let bypass = bypass_lan_cidrs(&effective_bypass_lan(&UConfigBypass(config)));
        let win = crate::builder::tun_route_exclude::compute_win_bypass_exclude(
            &crate::builder::tun_route_exclude::WinBypassExcludeInput {
                bypass_cidrs: &bypass,
                engaged_mesh_cidrs: &engaged_mesh,
                own_lan_cidrs: &deps.own_lan_cidrs,
                fakeip_ranges: &fakeip_ranges,
            },
        );
        win.exclude
    } else if deps.platform == "linux" {
        // Linux 加法态：route_exclude 恒空（VM185 实证）。
        vec![]
    } else {
        // mac/其它：回环排除。
        vec!["127.0.0.0/8".into(), "::1/128".into()]
    };

    // Windows 额外排除 DNS IP（防回流死循环）。
    if deps.platform == "win32" {
        let mut dns_ips: Vec<String> = BOOTSTRAP_DIRECT_DNS_IPS
            .iter()
            .map(|s| s.to_string())
            .collect();
        dns_ips.push(CONTROLLED_TUN_DNS_IP.to_string());
        dns_ips = dedupe(dns_ips);
        for ip in &dns_ips {
            exclude_addr.push(format!("{ip}/32"));
        }
        if let Some((ip, _port)) = get_custom_domestic_dns_endpoint(
            config
                .dns_config
                .as_ref()
                .and_then(|d| d.domestic_dns.as_deref()),
        ) {
            if let Some(cidr) = host_to_exclude_cidr(&ip) {
                exclude_addr.push(cidr);
            }
        }
    }

    // 节点 IP 排除（非 Linux）。
    if deps.platform != "linux" {
        let mut all_server_ids: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        if let Some(sid) = &config.selected_server_id {
            all_server_ids.insert(sid.clone());
        }
        for r in &effective_app_rules_proxy(config) {
            if let Some(tid) = &r.target_server_id {
                all_server_ids.insert(tid.clone());
            }
        }
        for sid in &all_server_ids {
            if let Some(server) = config.servers.iter().find(|s| &s.id == sid) {
                if is_ipv4_host(&server.address) || is_ipv6_host(&server.address) {
                    if let Some(cidr) = host_to_exclude_cidr(&server.address) {
                        exclude_addr.push(cidr);
                    }
                } else if let Some(ips) = resolved_ips {
                    if let Some(ip) = ips.get(sid) {
                        if let Some(cidr) = host_to_exclude_cidr(ip) {
                            exclude_addr.push(cidr);
                        }
                    }
                }
            }
        }
    }

    // 「连入来源排除」（上游 singbox-inbounds-builder.ts L297-358）：本机作服务端被 **off-subnet** 私网连入
    // （如经 ZeroTier→路由器 DNAT）时，回包目的地落在本机直连子网外 → 被 TUN 捕获 → 用户态栈误当新连接重拨 →
    // 连接断。把用户显式声明的来源网段追加进 `route_exclude_address`，内核层就不把该段交给 TUN、回包走物理网卡。
    // ⚠️ 双向语义：排除一个段会让该段出/入两个方向都绕过 TUN，故该段不再能经代理/自定义规则出网。
    //
    // 减法（顺序即 compute_user_tun_exclude 内部顺序）：先规范化（裸 IP 补 /32|/128、拒非法/过宽，否则
    // sing-box `netip.ParsePrefix` FATAL 或半个地址空间被排出 TUN），再减 engaged 组网 force-route 段
    // （mesh 优先，否则声明段把组网架空），再减 fakeip 段（假 IP 被排出 TUN → 绕过 fakeip 反查、服务端收不到域名），
    // macOS 额外减本机物理 LAN 段（排除物理 LAN 触发 NetworkExtension 反向路由拦截、drop 从 TUN 发回该段的回包）。
    // own_lan_cidrs 无条件传入，平台判定在 compute_user_tun_exclude 内（非 darwin 忽略该参数）。
    //
    // **Linux 恒忽略、不发射**（上游 L304-311 同款短路）：Linux 加法态下 route_exclude 非空即触发策略路由表
    // 2022 两族分解 → 表 9001 全抓 → same/off-subnet 服务端连入与 allowLan 回包全断；而 Linux 服务端回包本就由
    // 内核策略路由天然保护（表 9002 main 具体路由优先），此项对它既不需要、又是毒丸。故这里**不生成无效字段**，
    // 与 UI「Linux 无效」说明一致 —— 上面 exclude_addr 初始化的 linux 分支恒 [] 也是同一不变量的另一半。
    // 注：mesh/fakeip 减法**不能**在 Linux 上"算了但不发射"——engaged_mesh/fakeip_ranges 在 Linux 下本就是
    // 空/未算（fakeip_ranges 有 linux 守卫），进来只会白算。
    let user_inbound_cidrs: &[String] = config
        .tun_config
        .as_ref()
        .and_then(|t| t.inbound_exclude_cidrs.as_deref())
        .unwrap_or(&[]);
    if !user_inbound_cidrs.is_empty() && deps.platform == "linux" {
        // Linux 忽略腿也必须**出声**（上游 L304-311 有、移植时丢了）：用户在 UI 里填了段、
        // 生成侧整块跳过，日志里一个字都没有 = 与「填了但没生效」不可区分。与本批「静默剔除必告警」
        // 的其余三类（非法/过宽、组网重叠、macOS 物理 LAN）同档 warn，口径一致。
        (deps.log)(
            LogLevel::Warn,
            &format!(
                "Linux：服务端回包已由内核策略路由天然保护，「连入来源排除」不生效且会重新触发路由表分解引入回归，已忽略 {} 条声明段。",
                user_inbound_cidrs.len()
            ),
        );
    } else if !user_inbound_cidrs.is_empty() {
        let user_exclude = compute_user_tun_exclude(&UserTunExcludeInput {
            platform: &deps.platform,
            user_cidrs: user_inbound_cidrs,
            mesh_cidrs: &engaged_mesh,
            fakeip_ranges: &fakeip_ranges,
            own_lan_cidrs: &deps.own_lan_cidrs,
        });

        // 静默剔除告警（上游 L324-355）：4 类，用户填的段被剔时零线索，须逐类 warn。
        if user_exclude.dropped_invalid > 0 {
            (deps.log)(
                LogLevel::Warn,
                &format!(
                    "「连入来源排除」剔除 {} 条非法/过宽网段（须合法 CIDR、不含 0.0.0.0/0 等过宽段）。",
                    user_exclude.dropped_invalid
                ),
            );
        }
        if !user_exclude.dropped_mesh_overlap.is_empty() {
            (deps.log)(
                LogLevel::Warn,
                &format!(
                    "「连入来源排除」{} 段与生效组网(WG/Tailscale)路由段重叠，已跳过排除（该段经组网节点）：{}",
                    user_exclude.dropped_mesh_overlap.len(),
                    user_exclude.dropped_mesh_overlap.join(", ")
                ),
            );
        }
        if !user_exclude.dropped_own_lan_mac.is_empty() {
            (deps.log)(
                LogLevel::Warn,
                &format!(
                    "macOS：「连入来源排除」{} 段与本机物理 LAN 相交，已跳过（排除物理 LAN 会触发 NetworkExtension 反向路由丢包）：{}",
                    user_exclude.dropped_own_lan_mac.len(),
                    user_exclude.dropped_own_lan_mac.join(", ")
                ),
            );
        }

        // 与非直连（走代理/拦截）自定义规则段重叠告警（上游 L342-356，双向语义副作用）：被排除的段出/入
        // 均绕过 TUN，若某 enabled 的非直连（proxy/block）custom rule 想把该段走代理/拦截，会被静默架空。
        // 刻意**不减**（排除=用户显式声明意图更明确），仅告警。
        let overridable_rule_cidrs: Vec<String> = effective_custom_rules_proxy(config)
            .iter()
            .filter(|r| r.enabled && r.action != RuleAction::Direct)
            .flat_map(rule_ip_cidrs)
            .collect();
        if !overridable_rule_cidrs.is_empty() {
            let conflict: Vec<String> = user_exclude
                .extra
                .iter()
                .filter(|c| cidr_overlaps_any(c, &overridable_rule_cidrs))
                .cloned()
                .collect();
            if !conflict.is_empty() {
                (deps.log)(
                    LogLevel::Warn,
                    &format!(
                        "「连入来源排除」{} 段与非直连（走代理/拦截）自定义规则段重叠：排除使其出/入均绕过 TUN 走直连，该自定义规则对这些段将不生效：{}",
                        conflict.len(),
                        conflict.join(", ")
                    ),
                );
            }
        }

        exclude_addr.extend(user_exclude.extra);
    }

    // TUN 地址。
    let tun_cfg = config.tun_config.as_ref();
    let mut tun_address = vec![tun_cfg
        .and_then(|t| t.inet4_address.clone())
        .unwrap_or_else(|| {
            if deps.platform == "darwin" {
                "172.19.0.1/30".into()
            } else {
                "172.19.0.1/16".into()
            }
        })];
    if config.enable_ipv6 == Some(true) {
        tun_address.push(
            tun_cfg
                .and_then(|t| t.inet6_address.clone())
                .unwrap_or_else(|| "fdfe:dcba:9876::1/126".into()),
        );
    }

    // stack。**必须先于 MTU 解析** —— 默认 MTU 是栈的函数（gvisor 吃得下大 MTU，system/mixed
    // 在 65535 下会塌到 11 Mbps），倒过来算就只能退回平台单维度，那正是此前 1350/1400 的形态。
    let user_stack = tun_cfg.map(|t| t.stack);
    let effective_stack = resolve_tun_stack(user_stack, &deps.platform);

    // MTU：用户显式值逐字下发；缺席则按「最终栈 × 平台」取默认（判据见 `tun_stack::default_mtu_for`）。
    // 此处**没有**任何哨兵值 —— 旧实现把 `Some(9000)` 当「未设置」，导致真想要 9000 的用户被静默改写。
    let effective_mtu = tun_cfg
        .and_then(|t| t.mtu)
        .unwrap_or_else(|| default_mtu_for(effective_stack, &deps.platform));

    let auto_route = tun_cfg.map(|t| t.auto_route).unwrap_or(true);
    let strict_route = tun_cfg.map(|t| t.strict_route).unwrap_or(true);

    // NAT 类型：**缺席就一个键都不发**（`unwrap_or((None, None))`，不是「缺席回落成全锥」）。
    // 上游两项默认已是 endpoint_independent（全锥），回落写死等价值只会把「当前默认」冻进每一份
    // 生成的配置 —— 金样 config-snapshot.json 当场 delta，且上游日后改默认时我们钉在旧值上却没有
    // 任何判据支撑（对比 stack/MTU：那两处的显式 pin 有 §0.6 实测撑着，这里没有）。
    let (udp_mapping, udp_filtering) = tun_cfg
        .and_then(|t| t.udp_nat_type)
        .map(|nat| {
            let (m, f) = udp_nat_behaviors(nat);
            (Some(m), Some(f))
        })
        .unwrap_or((None, None));

    let mut tun = Inbound {
        type_field: "tun".into(),
        tag: "tun-in".into(),
        listen: None,
        listen_port: None,
        interface_name: None,
        address: Some(tun_address),
        mtu: Some(effective_mtu),
        auto_route: Some(auto_route),
        auto_redirect: None,
        strict_route: Some(strict_route),
        stack: Some(effective_stack.as_str().to_string()),
        udp_mapping,
        udp_filtering,
        route_exclude_address: None,
        include_mac_address: None,
        exclude_mac_address: None,
        platform: None,
    };
    if !exclude_addr.is_empty() {
        tun.route_exclude_address = Some(exclude_addr);
    }

    // Windows 接口名。
    if deps.platform == "win32" {
        let ifname =
            resolve_win_tun_interface_name(tun_cfg.and_then(|t| t.interface_name.as_deref()));
        tun.interface_name = Some(ifname);
    }

    // P6 LAN MAC 过滤（仅 Linux + auto_route + 合法 MAC）。
    if let Some(mac_mode) = tun_cfg.and_then(|t| t.mac_filter_mode) {
        if is_tun_mac_filter_supported(&deps.platform) && auto_route {
            let macs: Vec<String> = tun_cfg
                .map(|t| {
                    t.mac_filter_list
                        .iter()
                        .map(|m| m.trim().to_string())
                        .filter(|m| is_valid_mac_address(Some(m)))
                        .collect()
                })
                .unwrap_or_default();
            if !macs.is_empty() {
                tun.auto_redirect = Some(true);
                match mac_mode {
                    crate::user_config::neighbor::TunMacFilterMode::Exclude => {
                        tun.exclude_mac_address = Some(macs)
                    }
                    crate::user_config::neighbor::TunMacFilterMode::Include => {
                        tun.include_mac_address = Some(macs)
                    }
                }
            }
        }
    }

    // macOS http_proxy platform。
    if deps.platform == "darwin" {
        tun.platform = Some(InboundPlatform {
            http_proxy: Some(HttpProxyPlatform {
                enabled: true,
                server: "127.0.0.1".into(),
                server_port: local_proxy_port(config),
            }),
        });
    }

    Some(tun)
}

/// usesFakeIp：enableFakeIp 缺省 true。上游 `custom-rule-files.usesFakeIp`。
fn uses_fake_ip(config: &UserConfig) -> bool {
    config
        .dns_config
        .as_ref()
        .and_then(|d| d.enable_fake_ip)
        .unwrap_or(true)
}

/// UserConfig → effectiveCustomRules（smart gate）。复用 helpers。
fn effective_custom_rules_proxy(config: &UserConfig) -> Vec<crate::user_config::rule::Rule> {
    let mode = match config.proxy_mode {
        crate::user_config::ProxyMode::Smart => "smart",
        crate::user_config::ProxyMode::Global => "global",
        crate::user_config::ProxyMode::Direct => "direct",
    };
    effective_custom_rules(mode, &config.custom_rules)
}

/// UserConfig → effectiveAppRules（smart + appRoutingEnabled gate）。
fn effective_app_rules_proxy(config: &UserConfig) -> Vec<crate::user_config::rule::AppRule> {
    let mode = match config.proxy_mode {
        crate::user_config::ProxyMode::Smart => "smart",
        crate::user_config::ProxyMode::Global => "global",
        crate::user_config::ProxyMode::Direct => "direct",
    };
    effective_app_rules(
        config.app_routing_enabled == Some(true),
        mode,
        &config.app_rules,
    )
}

/// BypassConfig 适配器（UserConfig → effective_bypass_lan）。
struct UConfigBypass<'a>(&'a UserConfig);
impl<'a> BypassConfig for UConfigBypass<'a> {
    fn bypass_lan(&self) -> Option<bool> {
        self.0.bypass_lan
    }
    fn bypass_lan_list(&self) -> Option<&[String]> {
        self.0.bypass_lan_list.as_deref()
    }
}

/// PortConfig 适配器。
impl PortConfig for UserConfig {
    fn mixed_port(&self) -> Option<u16> {
        self.mixed_port
    }
    fn http_port(&self) -> Option<u16> {
        self.http_port
    }
    fn control_port(&self) -> Option<u16> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps_linux() -> InboundsDeps {
        InboundsDeps {
            probe_direct_port: None,
            probe_proxy_port: None,
            update_in_port: None,
            probe_pool_ports: vec![],
            platform: "linux".into(),
            own_lan_cidrs: vec![],
            log: |_, _| {},
        }
    }

    // warn 收集器：`log` 是裸 fn 指针（闭包捕获不了）⇒ thread_local sink（与 route.rs/custom_rules.rs 同手法）。
    thread_local! {
        static WARN_SINK: std::cell::RefCell<Vec<String>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    fn capture_warn(lvl: LogLevel, msg: &str) {
        assert_eq!(
            lvl,
            LogLevel::Warn,
            "「连入来源排除」静默剔除告警必须是 warn 档（会被级别过滤吞掉的 info 等于没打）"
        );
        WARN_SINK.with(|s| s.borrow_mut().push(msg.to_string()));
    }
    fn take_warns() -> Vec<String> {
        WARN_SINK.with(|s| s.borrow_mut().drain(..).collect())
    }

    /// 【不变式：静默剔除必告警 —— Linux 忽略腿】
    /// 用户在 Linux 填了「连入来源排除」→ 整块跳过，但必须 warn 出「已忽略 N 条」，
    /// 且**确实不发射** route_exclude_address（Linux 加法态下它是毒丸）。
    /// 变异验证：删掉 linux 分支的 `(deps.log)(...)` → 首个断言转红。
    #[test]
    fn inbound_exclude_on_linux_warns_and_emits_nothing() {
        let config = UserConfig {
            proxy_mode_type: ProxyModeType::Tun,
            tun_config: Some(crate::user_config::tun_config::TunModeConfig {
                inbound_exclude_cidrs: Some(vec!["10.0.0.0/24".into(), "192.168.9.0/24".into()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut deps = deps_linux();
        deps.log = capture_warn;
        let inbounds = build_inbounds(&config, None, &deps);
        let warns = take_warns();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("已忽略 2 条声明段") && m.contains("Linux")),
            "Linux 忽略腿必须逐条自曝（含条数），实际: {warns:?}"
        );
        let tun = inbounds.iter().find(|i| i.tag == "tun-in").expect("有 tun");
        assert!(
            tun.route_exclude_address.is_none(),
            "Linux 恒不发射 route_exclude_address（非空即触发策略路由表分解 → 连入全断）"
        );
    }

    /// Linux 上用户**没填**时不得凭空 warn（告警要与用户动作一一对应，否则日志噪音掩盖真问题）。
    #[test]
    fn inbound_exclude_on_linux_is_silent_when_user_declared_nothing() {
        let config = UserConfig {
            proxy_mode_type: ProxyModeType::Tun,
            ..Default::default()
        };
        let mut deps = deps_linux();
        deps.log = capture_warn;
        let _ = build_inbounds(&config, None, &deps);
        assert!(take_warns().is_empty(), "未声明任何段 → 不得告警");
    }

    #[test]
    fn inbound_exclude_warns_invalid_cidr() {
        let config = UserConfig {
            proxy_mode_type: ProxyModeType::Tun,
            tun_config: Some(crate::user_config::tun_config::TunModeConfig {
                inbound_exclude_cidrs: Some(vec!["not-a-cidr".into()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut deps = deps_linux();
        deps.platform = "darwin".into();
        deps.log = capture_warn;
        let _ = build_inbounds(&config, None, &deps);
        let warns = take_warns();
        assert!(
            warns.iter().any(|m| m.contains("非法/过宽网段")),
            "实际: {warns:?}"
        );
    }

    #[test]
    fn inbound_exclude_warns_mesh_overlap() {
        let mut config = UserConfig {
            proxy_mode_type: ProxyModeType::Tun,
            selected_server_id: Some("w1".into()),
            tun_config: Some(crate::user_config::tun_config::TunModeConfig {
                inbound_exclude_cidrs: Some(vec!["10.0.0.0/24".into()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        config
            .servers
            .push(crate::user_config::server_config::ServerConfig {
                id: "w1".into(),
                name: "WG".into(),
                protocol: crate::user_config::server_config::Protocol::Wireguard,
                address: "1.2.3.4".into(),
                port: 443,
                wireguard_settings: Some(Box::new(
                    crate::user_config::server_config::WireGuardSettings {
                        allowed_ips: vec!["10.0.0.0/24".into()],
                        ..Default::default()
                    },
                )),
                ..Default::default()
            });
        let mut deps = deps_linux();
        deps.platform = "darwin".into();
        deps.log = capture_warn;
        let _ = build_inbounds(&config, None, &deps);
        let warns = take_warns();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("组网(WG/Tailscale)路由段重叠")),
            "实际: {warns:?}"
        );
    }

    #[test]
    fn inbound_exclude_warns_mac_own_lan_overlap() {
        let config = UserConfig {
            proxy_mode_type: ProxyModeType::Tun,
            tun_config: Some(crate::user_config::tun_config::TunModeConfig {
                inbound_exclude_cidrs: Some(vec!["10.0.0.0/24".into()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut deps = deps_linux();
        deps.platform = "darwin".into();
        deps.own_lan_cidrs = vec!["10.0.0.0/24".into()];
        deps.log = capture_warn;
        let _ = build_inbounds(&config, None, &deps);
        let warns = take_warns();
        assert!(
            warns.iter().any(|m| m.contains("本机物理 LAN")),
            "实际: {warns:?}"
        );
    }

    #[test]
    fn inbound_exclude_warns_custom_rule_overlap() {
        let config = UserConfig {
            proxy_mode_type: ProxyModeType::Tun,
            proxy_mode: crate::user_config::ProxyMode::Smart,
            tun_config: Some(crate::user_config::tun_config::TunModeConfig {
                inbound_exclude_cidrs: Some(vec!["10.0.0.0/24".into()]),
                ..Default::default()
            }),
            custom_rules: vec![crate::user_config::rule::Rule {
                id: "r1".into(),
                type_field: crate::user_config::rule::RuleType::IpCidr,
                values: vec!["10.0.0.0/24".into()],
                conditions: None,
                combine_mode: None,
                action: RuleAction::Proxy,
                enabled: true,
                bypass_fakeip: None,
                target_server_id: None,
                remarks: None,
                tls_spoof: None,
                tls_spoof_method: None,
            }],
            ..Default::default()
        };
        let mut deps = deps_linux();
        deps.platform = "darwin".into();
        deps.log = capture_warn;
        let _ = build_inbounds(&config, None, &deps);
        let warns = take_warns();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("非直连（走代理/拦截）自定义规则段重叠")),
            "实际: {warns:?}"
        );
    }

    #[test]
    fn system_proxy_single_mixed_inbound() {
        let config = UserConfig::default();
        let inbounds = build_inbounds(&config, None, &deps_linux());
        assert_eq!(inbounds.len(), 1);
        assert_eq!(inbounds[0].type_field, "mixed");
        assert_eq!(inbounds[0].tag, "mixed-in");
        assert_eq!(inbounds[0].listen.as_deref(), Some("127.0.0.1"));
        assert_eq!(inbounds[0].listen_port, Some(7890)); // 默认 mixed port
    }

    #[test]
    fn system_proxy_allow_lan_listens_all() {
        let config = UserConfig {
            allow_lan: Some(true),
            ..Default::default()
        };
        let inbounds = build_inbounds(&config, None, &deps_linux());
        assert_eq!(inbounds[0].listen.as_deref(), Some("::"));
    }

    #[test]
    fn probe_inbounds_when_ports_set() {
        let mut deps = deps_linux();
        deps.probe_direct_port = Some(12345);
        deps.probe_proxy_port = Some(12346);
        let inbounds = build_inbounds(&UserConfig::default(), None, &deps);
        assert_eq!(inbounds.len(), 3); // mixed + probe-direct + probe-proxy
        assert_eq!(inbounds[1].tag, "probe-direct-in");
        assert_eq!(inbounds[2].tag, "probe-proxy-in");
    }

    #[test]
    fn update_inbound_when_port_set() {
        let mut deps = deps_linux();
        deps.update_in_port = Some(12347);
        let inbounds = build_inbounds(&UserConfig::default(), None, &deps);
        assert_eq!(inbounds.len(), 2); // mixed + update-in
        assert_eq!(inbounds[1].type_field, "socks");
        assert_eq!(inbounds[1].tag, "update-in");
    }

    #[test]
    fn tun_linux_no_exclude_addr() {
        let config = UserConfig {
            proxy_mode_type: ProxyModeType::Tun,
            ..Default::default()
        };
        let inbounds = build_inbounds(&config, None, &deps_linux());
        assert_eq!(inbounds.len(), 2); // mixed + tun
        let tun = &inbounds[1];
        assert_eq!(tun.type_field, "tun");
        assert_eq!(tun.tag, "tun-in");
        assert_eq!(tun.stack.as_deref(), Some("system")); // linux 默认 system
        assert!(
            tun.route_exclude_address.is_none()
                || tun.route_exclude_address.as_ref().unwrap().is_empty()
        );
        assert_eq!(tun.mtu, Some(4064)); // linux auto → system → 4064
    }

    #[test]
    fn tun_mac_has_loopback_exclude() {
        let config = UserConfig {
            proxy_mode_type: ProxyModeType::Tun,
            ..Default::default()
        };
        let mut deps = deps_linux();
        deps.platform = "darwin".into();
        let inbounds = build_inbounds(&config, None, &deps);
        let tun = &inbounds[1];
        assert_eq!(tun.stack.as_deref(), Some("gvisor")); // mac 默认 gvisor
        assert_eq!(tun.mtu, Some(9000)); // mac auto → gvisor → 9000
        let exclude = tun.route_exclude_address.as_ref().unwrap();
        assert!(exclude.contains(&"127.0.0.0/8".to_string()));
        assert!(exclude.contains(&"::1/128".to_string()));
    }

    #[test]
    fn tun_mac_has_http_proxy_platform() {
        let config = UserConfig {
            proxy_mode_type: ProxyModeType::Tun,
            ..Default::default()
        };
        let mut deps = deps_linux();
        deps.platform = "darwin".into();
        let inbounds = build_inbounds(&config, None, &deps);
        let tun = &inbounds[1];
        assert!(tun.platform.is_some());
        assert!(tun.platform.as_ref().unwrap().http_proxy.is_some());
    }

    #[test]
    fn tun_ipv6_address_when_enabled() {
        let config = UserConfig {
            proxy_mode_type: ProxyModeType::Tun,
            enable_ipv6: Some(true),
            ..Default::default()
        };
        let inbounds = build_inbounds(&config, None, &deps_linux());
        let tun = &inbounds[1];
        let addr = tun.address.as_ref().unwrap();
        assert_eq!(addr.len(), 2); // v4 + v6
        assert!(addr[1].contains("::"));
    }

    #[test]
    fn tun_probe_pool_inbounds() {
        let config = UserConfig {
            proxy_mode_type: ProxyModeType::Tun,
            ..Default::default()
        };
        let mut deps = deps_linux();
        deps.probe_pool_ports = vec![20000, 20001];
        let inbounds = build_inbounds(&config, None, &deps);
        // mixed + probe-in-0 + probe-in-1 + tun
        assert_eq!(inbounds.len(), 4);
        assert_eq!(inbounds[1].tag, "probe-in-0");
        assert_eq!(inbounds[2].tag, "probe-in-1");
    }

    // ── NAT 类型（udp_mapping × udp_filtering）─────────────────────────────────

    fn tun_with_nat(nat: Option<UdpNatType>) -> Inbound {
        let config = UserConfig {
            proxy_mode_type: ProxyModeType::Tun,
            tun_config: Some(crate::user_config::tun_config::TunModeConfig {
                udp_nat_type: nat,
                ..Default::default()
            }),
            ..Default::default()
        };
        let inbounds = build_inbounds(&config, None, &deps_linux());
        inbounds
            .into_iter()
            .find(|i| i.type_field == "tun")
            .expect("TUN 模式必产出 tun inbound")
    }

    /// 【不变式：默认零下发】未选档 ⇒ 序列化出的 TUN inbound **一个 udp_* 键都没有**。
    ///
    /// 断言落在**序列化后的 JSON** 而非 `Option` 字段上：`skip_serializing_if` 漏写时字段仍是 `None`、
    /// 结构体断言照绿，而 JSON 里会多出 `"udp_mapping": null` —— 那正是金样 `config-snapshot.json`
    /// 转红的形态。变异锁：把 `unwrap_or((None, None))` 改成回落全锥、或删掉任一 `skip_serializing_if`，
    /// 本条即红。
    #[test]
    fn udp_nat_absent_by_default_emits_no_key() {
        let json = serde_json::to_value(tun_with_nat(None)).unwrap();
        let obj = json.as_object().unwrap();
        for key in ["udp_mapping", "udp_filtering", "udp_nat_max"] {
            assert!(
                !obj.contains_key(key),
                "未选 NAT 类型档时不得下发 `{key}`（金样 config-snapshot.json 依赖这条零 delta），实际: {json}"
            );
        }
        // 同一条不变量的另一半：非 TUN inbound 永远不带这组键（内核 schema 里 mixed/http/socks 没有它们，
        // 发了即 `sing-box check` FATAL）。
        let config = UserConfig::default(); // 默认 systemProxy ⇒ 只有 mixed
        let mixed = serde_json::to_value(&build_inbounds(&config, None, &deps_linux())[0]).unwrap();
        assert!(!mixed.as_object().unwrap().contains_key("udp_mapping"));
    }

    /// 【映射表逐档钉死】三档 → `(udp_mapping, udp_filtering)`，值取**序列化后的字面量**
    /// （枚举 `rename_all` 写错时结构体断言看不出来，内核只认字面量）。
    ///
    /// 变异锁：把任一档的 filtering 挪一格（受限锥 ↔ 端口受限锥）即红；把哪一档的 mapping 收紧成
    /// address_* 即红 —— 那会把锥形变成对称 NAT，档位名对用户的承诺当场作废。
    #[test]
    fn udp_nat_type_maps_each_tier() {
        let cases = [
            (
                UdpNatType::FullCone,
                "endpoint_independent",
                "endpoint_independent",
            ),
            (
                UdpNatType::RestrictedCone,
                "endpoint_independent",
                "address_dependent",
            ),
            (
                UdpNatType::PortRestrictedCone,
                "endpoint_independent",
                "address_and_port_dependent",
            ),
        ];
        for (nat, mapping, filtering) in cases {
            let json = serde_json::to_value(tun_with_nat(Some(nat))).unwrap();
            assert_eq!(
                json.get("udp_mapping").and_then(|v| v.as_str()),
                Some(mapping),
                "{nat:?} 的 udp_mapping 不符"
            );
            assert_eq!(
                json.get("udp_filtering").and_then(|v| v.as_str()),
                Some(filtering),
                "{nat:?} 的 udp_filtering 不符"
            );
        }
    }

    /// `udpNatType` 走用户配置 JSON（camelCase）反序列化 —— 前端 `TunModeConfig.udpNatType` 与
    /// Rust 的键名/值名是同一套字面量，改名只在这里被抓住（前端那侧没有编译期依赖）。
    #[test]
    fn udp_nat_type_deserializes_from_user_config_json() {
        let config: UserConfig = serde_json::from_str(
            r#"{"servers":[],"proxyModeType":"tun","tunConfig":{"udpNatType":"portRestrictedCone"}}"#,
        )
        .expect("用户配置反序列化失败");
        assert_eq!(
            config.tun_config.as_ref().unwrap().udp_nat_type,
            Some(UdpNatType::PortRestrictedCone)
        );
        let tun = build_inbounds(&config, None, &deps_linux())
            .into_iter()
            .find(|i| i.type_field == "tun")
            .unwrap();
        assert_eq!(
            tun.udp_filtering,
            Some(UdpNatBehavior::AddressAndPortDependent)
        );
    }
}
