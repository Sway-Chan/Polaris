//! 组网 endpoint 路由纯逻辑（上游 `shared/endpoint-routes.ts` 1:1 移植）。
//!
//! endpointForcedRouteCidrs / meshAllowsInternet / meshAlwaysRoutesSubnets /
//! shouldForceRouteSubnets / collectRuleTargetedServerIds / meshForceRoutedServers /
//! meshForcedRouteCidrs（buildInbounds 依赖的核心子集）。

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use crate::user_config::app_config::UserConfig;
use crate::user_config::collections::{dedupe, dedupe_trim};
use crate::user_config::dns_constants::is_sentinel_selection;
use crate::user_config::rule::{Rule, RuleAction};
use crate::user_config::server_config::{is_mesh_node, lands_in_endpoints, Protocol, ServerConfig};

/// 全网段（catch-all）。上游 `FULL_TUNNEL_CIDRS`。
pub const FULL_TUNNEL_CIDRS: &[&str] = &["0.0.0.0/0", "::/0"];

/// Tailscale tailnet v4 段（CGNAT）。上游 `TAILNET_CGNAT`。
pub const TAILNET_CGNAT: &str = "100.64.0.0/10";

/// Tailscale v6 tailnet 段（ULA 前缀）。上游 `TAILNET_ULA_V6`。
pub const TAILNET_ULA_V6: &str = "fd7a:115c:a1e0::/48";

/// System 模式内核接口固定名（TS）。原 上游 `polaris-ts`，改名 `polaris-ts`（§D.2 品牌改名）。
pub const TS_SYSTEM_INTERFACE_NAME: &str = "polaris-ts";

/// System 模式内核接口固定名（WG）。原 上游 `polaris-wg`，改名 `polaris-wg`。
pub const WG_SYSTEM_INTERFACE_NAME: &str = "polaris-wg";

fn is_catch_all(c: &str) -> bool {
    FULL_TUNNEL_CIDRS.contains(&c.trim())
}

/// 剥离全网段（catch-all），仅留具体段。上游 `stripCatchAll`。
pub fn strip_catch_all(cidrs: &[String]) -> Vec<String> {
    cidrs.iter().filter(|c| !is_catch_all(c)).cloned().collect()
}

/// CIDR 列表是否含任一全网段。上游 `hasCatchAll`。
pub fn has_catch_all(cidrs: &[String]) -> bool {
    cidrs.iter().any(|c| is_catch_all(c))
}

/// 该组网节点应被「强制路由到自身 tag」的具体 CIDR。上游 `endpointForcedRouteCidrs`。
///
/// 三个来源，都是**配置期已知**的段（这正是 `is_mesh_protocol` / `is_mesh_node` 的判据）：
///  - WireGuard：`allowedIPs` 去 catch-all；
///  - Tailscale：tailnet 两族段 + `routes` 去 catch-all；
///  - openconnect / openvpn-client：用户在 `meshRoutes` 里显式声明的段（这两个协议的段本由服务端
///    运行期 push、配置期不可知，故只认用户手填的那份）。
///
/// 非组网协议 → `[]`。
pub fn endpoint_forced_route_cidrs(server: &ServerConfig) -> Vec<String> {
    let raw: Vec<String> = match server.protocol {
        Protocol::Wireguard => {
            let allowed = server
                .wireguard_settings
                .as_ref()
                .map(|w| w.allowed_ips.clone())
                .unwrap_or_default();
            strip_catch_all(&allowed)
        }
        Protocol::Tailscale => {
            let routes = server
                .tailscale_settings
                .as_ref()
                .map(|t| t.routes.clone())
                .unwrap_or_default();
            let mut raw = vec![TAILNET_CGNAT.to_string(), TAILNET_ULA_V6.to_string()];
            raw.extend(strip_catch_all(&routes));
            raw
        }
        // 用户手填的内网段。去 catch-all 与另两支同理：0/0 属「全隧道」意图，由各自的出网开关
        // 表达（OpenVPN 是 `redirect_gateway`），混进 force-route 会绕过那个开关。
        Protocol::Openconnect | Protocol::OpenvpnClient => strip_catch_all(&server.mesh_routes),
        _ => return vec![],
    };
    dedupe_trim(raw)
}

/// 组网节点是否允许作外网出口（缺省 true）。上游 `meshAllowsInternet`。
/// WG：allowInternet !== false；TS：!!exitNode（allowInternet 由 exit_node 派生）。
pub fn mesh_allows_internet(server: &ServerConfig) -> bool {
    match server.protocol {
        Protocol::Wireguard => server
            .wireguard_settings
            .as_ref()
            .and_then(|w| w.allow_internet)
            .unwrap_or(true),
        Protocol::Tailscale => server
            .tailscale_settings
            .as_ref()
            .and_then(|t| t.exit_node.as_deref())
            .map(|e| !e.trim().is_empty())
            .unwrap_or(false),
        // OpenVPN 的全隧道开关。**缺省判 true**（同 WG 的 `allow_internet` 那支）：判 false 的后果是
        // 用户选了该节点作出口、流量却被兜底回 direct —— 静默走明文，比多一次黑洞更坏。故只在用户
        // **显式**关掉时才认为它不承载全隧道，而那恰是「只走公司内网段」的表达。
        // OpenConnect 无对应开关（本就是全隧道），落 `_ => true`。
        Protocol::OpenvpnClient => server
            .openvpn_client_settings
            .as_ref()
            .and_then(|o| o.redirect_gateway)
            .unwrap_or(true),
        _ => true,
    }
}

/// 组网节点是否「始终路由其内网段」（缺省 true）。上游 `meshAlwaysRoutesSubnets`。
pub fn mesh_always_routes_subnets(server: &ServerConfig) -> bool {
    match server.protocol {
        Protocol::Wireguard => server
            .wireguard_settings
            .as_ref()
            .and_then(|w| w.always_route_subnets)
            .unwrap_or(true),
        Protocol::Tailscale => server
            .tailscale_settings
            .as_ref()
            .and_then(|t| t.always_route_subnets)
            .unwrap_or(true),
        _ => true,
    }
}

/// 组网节点是否启用 system 内核接口（reverseMesh）。上游 `meshUsesSystemInterface`。
/// WG：reverseMesh（WARP 否决）；TS：reverseMesh。
pub fn mesh_uses_system_interface(server: &ServerConfig) -> bool {
    match server.protocol {
        Protocol::Wireguard => {
            // WARP 恒否决：它是 anycast 出口、不是子网路由器，不可被反向访问，system 对它无意义；
            // 而 `system:true` 会与主 TUN / 另一 System 接口抢内核 utun →
            // `post-start endpoint/wireguard[Cloudflare WARP]: Connect: resource busy` **FATAL**。
            // 判据与前端 `isWarpServer` 同源（见 crate::warp）——导入配置 / 手改 config.json /
            // 上游 迁移这三条腿不经渲染端，前端那道否决在这里挡不住。
            if crate::warp::is_warp_server(server) {
                return false;
            }
            server
                .wireguard_settings
                .as_ref()
                .and_then(|w| w.reverse_mesh)
                .unwrap_or(false)
        }
        Protocol::Tailscale => server
            .tailscale_settings
            .as_ref()
            .and_then(|t| t.reverse_mesh)
            .unwrap_or(false),
        _ => false,
    }
}

/// 组网节点是否承载全隧道默认出口（= 允许外网）。上游 `meshNodeCarriesFullTunnel`。
pub fn mesh_node_carries_full_tunnel(server: &ServerConfig) -> bool {
    mesh_allows_internet(server)
}

/// WireGuard peer.allowed_ips（Layer A cryptokey）。上游 `wireguardPeerAllowedIps`。
/// allowInternet=on → specific ∪ {0/0,::/0}；off → specific（空则 None=FATAL）。
pub fn wireguard_peer_allowed_ips(server: &ServerConfig) -> Option<Vec<String>> {
    let specific = endpoint_forced_route_cidrs(server);
    if mesh_node_carries_full_tunnel(server) {
        let mut all = specific;
        all.extend(FULL_TUNNEL_CIDRS.iter().map(|s| s.to_string()));
        Some(crate::user_config::collections::dedupe(all))
    } else if specific.is_empty() {
        None
    } else {
        Some(specific)
    }
}

/// 组网节点是否「关外网且无可路由网段」→ 不可发射。上游 `isMeshNodeUnroutable`。
pub fn is_mesh_node_unroutable(server: &ServerConfig) -> bool {
    if server.protocol == Protocol::Wireguard {
        wireguard_peer_allowed_ips(server).is_none()
    } else {
        false
    }
}

/// 平台是否支持组网 System 内核接口（Windows 禁）。上游 `meshSystemSupportedOnPlatform`。
pub fn mesh_system_supported_on_platform(platform: &str) -> bool {
    !platform.eq_ignore_ascii_case("win32")
}

/// 该组网节点的 force-route 段本轮是否应发射。上游 `shouldForceRouteSubnets`。
/// alwaysRouteSubnets ON → 恒发；OFF → 仅 engaged（选中/被规则指向）时发。
pub fn should_force_route_subnets(
    server: &ServerConfig,
    selected_server_id: Option<&str>,
    rule_targeted_server_ids: &BTreeSet<String>,
) -> bool {
    if mesh_always_routes_subnets(server) {
        return true;
    }
    if Some(server.id.as_str()) == selected_server_id {
        return true;
    }
    rule_targeted_server_ids.contains(&server.id)
}

/// 收集「显式指向某节点」的规则目标 id（enabled && proxy && targetServerId）。
/// 上游 `collectRuleTargetedServerIds`。接受 Rule + AppRule 混合。
pub fn collect_rule_targeted_server_ids(rules: &[Rule]) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for r in rules {
        if r.enabled && r.action == RuleAction::Proxy {
            if let Some(tid) = &r.target_server_id {
                ids.insert(tid.clone());
            }
        }
    }
    ids
}

/// 本轮「实际会发射 force-route」的组网节点。上游 `meshForceRoutedServers`。
pub fn mesh_force_routed_servers(
    servers: &[ServerConfig],
    selected_server_id: Option<&str>,
    rule_targeted_server_ids: &BTreeSet<String>,
) -> Vec<ServerConfig> {
    servers
        .iter()
        .filter(|s| is_mesh_node(s))
        .filter(|s| should_force_route_subnets(s, selected_server_id, rule_targeted_server_ids))
        .cloned()
        .collect()
}

/// 全部节点的 mesh force-route 段并集（去重）。上游 `meshForcedRouteCidrs`。
pub fn mesh_forced_route_cidrs(servers: &[ServerConfig]) -> Vec<String> {
    let all: Vec<String> = mesh_force_routed_servers(servers, None, &BTreeSet::new())
        .iter()
        .flat_map(endpoint_forced_route_cidrs)
        .collect();
    dedupe(all)
}

/// custom-endpoint 的 raw JSON（`customSettings.outbound`）是否含「独立承载流量」语义键。
///
/// 深度扫（递归任意嵌套，含 peers[].allowed_ips），命中任一即真。
/// 上游 `customEndpointCarriesTraffic`（endpoint-routes.ts L163-172）。
fn custom_endpoint_carries_traffic(raw: &serde_json::Value) -> bool {
    match raw {
        serde_json::Value::Array(arr) => arr.iter().any(custom_endpoint_carries_traffic),
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if CARRY_TRAFFIC_KEYS.contains(&k.as_str()) {
                    return true;
                }
                if custom_endpoint_carries_traffic(v) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// custom-endpoint 承载流量的语义键集合。上游 `CARRY_TRAFFIC_KEYS`。
///
/// ⚠️ **这个集合的覆盖面由「内核支持哪些端点类型」决定，不由「写它时手边有哪些类型」决定。**
/// 2026-08-11 补 OpenVPN 三键前，全表都是 WireGuard / Tailscale 的词汇 —— 而随包核
/// （1.14.0-beta.12，tags 含 `with_openvpn` / `with_openconnect`）的 `$defs/Endpoint` 有 5 支：
/// wireguard / tailscale / openconnect / openvpn-client / openvpn-server。逐支对过：
///   · openconnect —— 路由类键只有 `system`，**原表已覆盖**；
///   · openvpn-client —— 另有 `redirect_gateway`（OpenVPN 的全隧道开关，等价 WG 的
///     `allowed_ips: 0.0.0.0/0`）、`redirect_private`、`route_no_pull`，**原表一个都不认**。
///
/// 补进来**不要求**先证明「`redirect_gateway:true` 且 `system:false` 时内核是否真独立导流」：
/// 本判据的既定安全方向是「过度纳入只多一次重启、绝不错跳」，而漏纳入的后果写在调用点
/// （`can_skip_restart_for_added_unreferenced` 那段）—— 走 defer 腿不重启、核继续用旧参数出网
/// 且无任何提示。不确定时按方向站队，不是按证据强弱站队。
const CARRY_TRAFFIC_KEYS: &[&str] = &[
    "system",
    "system_interface",
    "allowed_ips",
    "routes",
    "route_address",
    "route_exclude_address",
    "accept_routes",
    "advertise_routes",
    "exit_node",
    // ── OpenVPN（2026-08-11）──
    "redirect_gateway",
    "redirect_private",
    "route_no_pull",
];

/// 生成期该节点是否**必定**被发射为 outbound/endpoint。
///
/// **Sound under-approximation**：返回 `true` ⇒ 一定发射；返回 `false` ⇒ **不确定**（可能发射、
/// 也可能被跳过），调用方须按保守方向处理。逐条对应 `builder/outbounds.rs` 发射循环（:127-234）：
/// - naive 缺 libcronet（:136-138）：libcronet 是**运行期**能力，UserConfig 看不见 → 一律不确定；
/// - WireGuard（:147-161）：无可路由段、或缺 privateKey/peerPublicKey/localAddress → 不发射。
///   这些判据全是 `ServerConfig` 的纯函数，故**直接调 `build_wireguard_endpoint` 取真判据**，
///   不在此复刻条件清单——复刻会随构建腿改动静默漂移，而漂移方向恰好是「误判必定发射」＝错跳。
///   tag / resolver / platform 三个参数不影响 Ok/Err，传占位值；
/// - custom（两条腿）：`customSettings.outbound` 不是**带 string `type` 的对象**就会被发射循环剔除
///   并记进 `invalid_nodes`（`INVALID_REASON_CUSTOM_MALFORMED`）→ 故此处同判
///   [`custom_outbound_type`](crate::user_config::protocol_settings::custom_outbound_type)。
///   注意这条从前写的是「endpoint 腿反序列化可能失败 → 恒不确定」——那个失败腿已随 raw 透传消失，
///   取而代之的是形状判据；而**非** endpoint 的 custom 从前恒记「必定发射」，那在补上形状 gate
///   之后是**不成立**的（形状坏的 custom outbound 现在会被剔），故必须一并收紧，否则这个
///   sound under-approximation 就朝「误判必定发射」＝错跳重启的方向破了；
/// - Tailscale（:165-175）与普通代理 outbound（:199-233）无失败腿 → 必定发射。
///
/// `gate_invalid_nodes`（:139-141）**刻意不建模**：两个注入点（`generate.rs:261` /
/// `runtime/proxy.rs:3048`）都传空集，且它只在发射循环**之后**由 detour 剪枝填充 ⇒ 发射循环恒见空集。
fn certainly_emitted(s: &ServerConfig) -> bool {
    match s.protocol {
        Protocol::Naive => false,
        Protocol::Wireguard => {
            crate::builder::endpoints::build_wireguard_endpoint(s, "", None, "", None).is_ok()
        }
        Protocol::Tailscale => true,
        Protocol::Custom => s.custom_settings.as_ref().is_some_and(|c| {
            crate::user_config::protocol_settings::custom_outbound_type(&c.outbound).is_some()
        }),
        _ => true,
    }
}

/// `proxy-selector` 的 default 是否**可能**落到「非选中节点」的兜底节点上。
///
/// `build_outbounds`（`outbounds.rs:262-271`）在「选中节点的 tag 不在本轮已发射 tag 集合里」时，
/// 把 default 落到 `node_tags.first()`——那个节点随即承载**全部**代理流量，但它的 id 无法从
/// `UserConfig` 静态算出（取决于生成期跳过了谁，而那依赖运行期能力）。本谓词只回答
/// 「是否处于该状态」，把「是谁」交给调用方按保守方向兜。
///
/// 返回 `false` 仅两条：① 直连哨兵（default 恒 = `direct` 出站，无节点承载）；
/// ② 选中节点存在**且** [`certainly_emitted`]（此时 default 恒 = 选中节点 tag）。
/// 其余一律 `true`——含「未选节点」（`selected_tag` 是字面量 `"proxy"`，匹配不到任何节点）
/// 与「悬空选中」（id→tag 解析不到）。
///
/// **同型第二处一并覆盖**：`prune_detour_dead_references` 经 `pruned_selector_default`
/// 重算 default（`outbounds.rs:568-578`）只在「被剔 tag == 当前 default」时触发；而 default ==
/// 选中节点 tag 时该路径返回 Err（`outbounds.rs:558`）而非静默重算 ⇒ 静默重算必然发生在本谓词
/// 已为 `true` 的状态下，无需第二道判据。
pub fn selector_default_may_fall_back(config: &UserConfig) -> bool {
    let Some(sid) = config.selected_server_id.as_deref() else {
        return true; // 未选节点 → selected_tag 恒为字面量 "proxy" → 必落兜底
    };
    if is_sentinel_selection(Some(sid)) {
        return false; // direct / block 哨兵 → default 恒 = 内置出站（direct / block），无节点承载
    }
    match config.servers.iter().find(|s| s.id == sid) {
        None => true, // 悬空选中 → id→tag 解析不到 → 必落兜底
        Some(s) => !certainly_emitted(s),
    }
}

/// 「被引用节点」id 集——其定义变化会影响运行核实际行为、故必须随之重启。
///
/// = {选中节点} ∪ {所有启用规则(custom/app)目标}，按 detour（前置代理链）传递闭包展开
/// ＋ 保守纳入全部 endpoint 协议节点（WireGuard/Tailscale 可能 force-route 子网/mesh）
/// ＋ [`selector_default_may_fall_back`] 成立时纳入**全部**节点（兜底 default 承载全部流量、
/// 但它是谁静态算不出）。
/// 安全方向：过度纳入只多一次重启、绝不错跳。上游 `referencedServerIds`。
pub fn referenced_server_ids(config: &UserConfig) -> BTreeSet<String> {
    let by_id: std::collections::BTreeMap<&str, &ServerConfig> =
        config.servers.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut result: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();

    let seed = |id: Option<&str>, stack: &mut Vec<String>| {
        if let Some(id) = id {
            // direct / block 哨兵不是节点 id：进了引用集就会当成「悬空选中」被 detour 闭包展开，
            // 白白把全部节点纳入 → 每次配置改动都误判需重启。
            if !is_sentinel_selection(Some(id)) {
                stack.push(id.to_string());
            }
        }
    };
    seed(config.selected_server_id.as_deref(), &mut stack);
    for r in &config.custom_rules {
        if r.enabled {
            seed(r.target_server_id.as_deref(), &mut stack);
        }
    }
    for a in &config.app_rules {
        if a.enabled {
            seed(a.target_server_id.as_deref(), &mut stack);
        }
    }
    for s in &config.servers {
        // 判据是 `lands_in_endpoints` 而非组网资格：本播种问的是「谁独立承载流量」，而 endpoint 腿的
        // 节点无论有没有声明网段都自成一条出网路径。漏纳入的后果写在 `CARRY_TRAFFIC_KEYS` 的调用点 ——
        // 走 defer 腿不重启、核继续用旧参数出网且无任何提示。
        if lands_in_endpoints(s.protocol) {
            stack.push(s.id.clone());
        } else if let Some(cs) = &s.custom_settings {
            if cs.is_endpoint.unwrap_or(false) && custom_endpoint_carries_traffic(&cs.outbound) {
                stack.push(s.id.clone());
            }
        }
    }
    // selector default 兜底节点（`outbounds.rs:262-271`）：它承载**全部**代理流量，却不在上面任何
    // 一条播种里——它是「生成期第一个成功发射的节点」，id 取决于生成期跳过了谁（naive 缺 cronet /
    // WG 构建失败 / custom-endpoint 解析失败），`UserConfig` 静态算不出。
    // 漏纳入的后果不是「少一次重启」而是**静默失效**：编辑它会被 `can_skip_restart_for_added_unreferenced`
    // 第③步判「未引用 → 放行」→ 走 defer 腿不重启 → 核继续用旧参数出网且无任何提示
    // （热切腿有 `is_server_dirty` 闸门，defer 腿没有）。
    // 故按本函数的既定安全方向（过度纳入只多一次重启、绝不错跳）：兜底**可能**触发时全员纳入。
    // 该状态本身是降级态（用户选中的出口没进核 / 还没选出口），常态（选中节点必定被发射）不受影响。
    if selector_default_may_fall_back(config) {
        for s in &config.servers {
            stack.push(s.id.clone());
        }
    }
    while let Some(id) = stack.pop() {
        if result.contains(&id) {
            continue; // 成环/重复保护
        }
        result.insert(id.clone());
        if let Some(s) = by_id.get(id.as_str()) {
            if let Some(detour) = &s.detour {
                if by_id.contains_key(detour.as_str()) {
                    stack.push(detour.clone());
                }
            }
        }
    }
    result
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::user_config::server_config::{
        Protocol, ServerConfig, TailscaleSettings, WireGuardSettings,
    };

    fn wg_server(id: &str, allowed: &[&str], allow_internet: Option<bool>) -> ServerConfig {
        ServerConfig {
            id: id.into(),
            name: id.into(),
            protocol: Protocol::Wireguard,
            address: "1.2.3.4".into(),
            port: 443,
            wireguard_settings: Some(WireGuardSettings {
                allowed_ips: allowed.iter().map(|s| s.to_string()).collect(),
                allow_internet,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn ts_server(id: &str, exit_node: Option<&str>, routes: &[&str]) -> ServerConfig {
        ServerConfig {
            id: id.into(),
            name: id.into(),
            protocol: Protocol::Tailscale,
            tailscale_settings: Some(TailscaleSettings {
                exit_node: exit_node.map(String::from),
                routes: routes.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn wg_forced_route_strips_catch_all() {
        let s = wg_server("w1", &["10.0.0.0/24", "0.0.0.0/0"], Some(true));
        assert_eq!(
            endpoint_forced_route_cidrs(&s),
            vec!["10.0.0.0/24".to_string()]
        );
    }

    #[test]
    fn ts_forced_route_includes_tailnet() {
        let s = ts_server("t1", None, &["192.168.10.0/24"]);
        let cidrs = endpoint_forced_route_cidrs(&s);
        assert!(cidrs.contains(&TAILNET_CGNAT.to_string()));
        assert!(cidrs.contains(&TAILNET_ULA_V6.to_string()));
        assert!(cidrs.contains(&"192.168.10.0/24".to_string()));
    }

    #[test]
    fn mesh_allows_internet_works() {
        assert!(mesh_allows_internet(&wg_server(
            "w",
            &["10.0.0.0/24"],
            Some(true)
        )));
        assert!(!mesh_allows_internet(&wg_server(
            "w",
            &["10.0.0.0/24"],
            Some(false)
        )));
        assert!(mesh_allows_internet(&wg_server(
            "w",
            &["10.0.0.0/24"],
            None
        ))); // 缺省 true
        assert!(mesh_allows_internet(&ts_server("t", Some("exit"), &[])));
        assert!(!mesh_allows_internet(&ts_server("t", None, &[])));
        assert!(!mesh_allows_internet(&ts_server("t", Some("  "), &[])));
    }

    #[test]
    fn force_route_always_on() {
        let s = wg_server("w", &["10.0.0.0/24"], None); // alwaysRoute 缺省 true
        let targeted = BTreeSet::new();
        assert!(should_force_route_subnets(&s, None, &targeted));
    }

    #[test]
    fn force_route_off_engaged_by_selection() {
        let mut s = wg_server("w", &["10.0.0.0/24"], None);
        s.wireguard_settings.as_mut().unwrap().always_route_subnets = Some(false);
        let targeted = BTreeSet::new();
        assert!(!should_force_route_subnets(&s, Some("other"), &targeted));
        assert!(should_force_route_subnets(&s, Some("w"), &targeted)); // 选中
    }

    #[test]
    fn force_route_off_engaged_by_rule() {
        let mut s = wg_server("w", &["10.0.0.0/24"], None);
        s.wireguard_settings.as_mut().unwrap().always_route_subnets = Some(false);
        let mut targeted = BTreeSet::new();
        targeted.insert("w".into());
        assert!(should_force_route_subnets(&s, None, &targeted));
    }

    #[test]
    fn collect_targeted_from_rules() {
        use crate::user_config::rule::{CombineMode, Rule, RuleType};
        let rules = vec![
            Rule {
                id: "r1".into(),
                type_field: RuleType::Domain,
                values: vec!["a.com".into()],
                conditions: None,
                combine_mode: None,
                action: RuleAction::Proxy,
                enabled: true,
                bypass_fakeip: None,
                target_server_id: Some("s2".into()),
                remarks: None,
                tls_spoof: None,
                tls_spoof_method: None,
            },
            Rule {
                id: "r2".into(),
                type_field: RuleType::Domain,
                values: vec!["b.com".into()],
                conditions: None,
                combine_mode: Some(CombineMode::And),
                action: RuleAction::Direct, // 非 proxy，不含
                enabled: true,
                bypass_fakeip: None,
                target_server_id: Some("s3".into()),
                remarks: None,
                tls_spoof: None,
                tls_spoof_method: None,
            },
        ];
        let ids = collect_rule_targeted_server_ids(&rules);
        assert!(ids.contains("s2"));
        assert!(!ids.contains("s3")); // direct 不算
    }

    #[test]
    fn mesh_forced_route_union() {
        let servers = [
            wg_server("w1", &["10.0.0.0/24"], None),
            wg_server("w2", &["10.0.0.0/24", "172.16.0.0/24"], None), // 10.0 重复
        ];
        let cidrs = mesh_forced_route_cidrs(&servers);
        assert_eq!(cidrs.len(), 2); // 去重
        assert!(cidrs.contains(&"10.0.0.0/24".to_string()));
        assert!(cidrs.contains(&"172.16.0.0/24".to_string()));
    }

    #[test]
    fn catch_all_detection() {
        assert!(has_catch_all(&["0.0.0.0/0".into()]));
        assert!(has_catch_all(&["::/0".into(), "10.0.0.0/8".into()]));
        assert!(!has_catch_all(&["10.0.0.0/8".into()]));
        assert_eq!(
            strip_catch_all(&["0.0.0.0/0".into(), "10.0.0.0/8".into()]),
            vec!["10.0.0.0/8".to_string()]
        );
    }

    #[test]
    fn referenced_ids_includes_selected_and_endpoint() {
        use crate::user_config::app_config::UserConfig;
        let mut config = UserConfig::default();
        config.servers = vec![
            ServerConfig {
                id: "s1".into(),
                name: "普通节点".into(),
                protocol: Protocol::Shadowsocks,
                address: "1.1.1.1".into(),
                port: 443,
                ..Default::default()
            },
            wg_server("wg1", &["10.0.0.0/24"], None),
        ];
        config.selected_server_id = Some("s1".into());
        let refs = referenced_server_ids(&config);
        // s1 选中 + wg1 是 endpoint（保守纳入）
        assert!(refs.contains("s1"));
        assert!(refs.contains("wg1"));
    }

    #[test]
    fn referenced_ids_detour_transitive_closure() {
        use crate::user_config::app_config::UserConfig;
        // s1 经 s2 代理链（detour），s2 经 s3 → 全闭包 {s1,s2,s3}
        let mut s1 = ServerConfig {
            id: "s1".into(),
            name: "s1".into(),
            protocol: Protocol::Shadowsocks,
            address: "1.1.1.1".into(),
            port: 443,
            detour: Some("s2".into()),
            ..Default::default()
        };
        let mut s2 = ServerConfig {
            id: "s2".into(),
            name: "s2".into(),
            protocol: Protocol::Shadowsocks,
            address: "2.2.2.2".into(),
            port: 443,
            detour: Some("s3".into()),
            ..Default::default()
        };
        let s3 = ServerConfig {
            id: "s3".into(),
            name: "s3".into(),
            protocol: Protocol::Shadowsocks,
            address: "3.3.3.3".into(),
            port: 443,
            ..Default::default()
        };
        let config = UserConfig {
            servers: vec![s1.clone(), s2.clone(), s3],
            selected_server_id: Some("s1".into()),
            ..Default::default()
        };
        let refs = referenced_server_ids(&config);
        assert!(refs.contains("s1"));
        assert!(refs.contains("s2"));
        assert!(refs.contains("s3"));
        // 恢复（避免 borrow 问题，此处不再用）
        s1.detour = None;
        s2.detour = None;
    }

    #[test]
    fn referenced_ids_direct_sentinel_excluded() {
        use crate::user_config::app_config::UserConfig;
        let config = UserConfig {
            servers: vec![ServerConfig {
                id: "s1".into(),
                name: "s1".into(),
                protocol: Protocol::Shadowsocks,
                address: "1.1.1.1".into(),
                port: 443,
                ..Default::default()
            }],
            selected_server_id: Some("__direct__".into()),
            ..Default::default()
        };
        let refs = referenced_server_ids(&config);
        // direct 哨兵剔除，s1 未被选中/规则引用 → 仅 endpoint 保守纳入（s1 非 endpoint）
        assert!(!refs.contains("__direct__"));
    }

    #[test]
    fn referenced_ids_rule_target_included() {
        use crate::user_config::app_config::UserConfig;
        use crate::user_config::rule::{Rule, RuleAction, RuleType};
        let config = UserConfig {
            servers: vec![
                ServerConfig {
                    id: "s1".into(),
                    name: "s1".into(),
                    protocol: Protocol::Shadowsocks,
                    address: "1.1.1.1".into(),
                    port: 443,
                    ..Default::default()
                },
                ServerConfig {
                    id: "s2".into(),
                    name: "s2".into(),
                    protocol: Protocol::Shadowsocks,
                    address: "2.2.2.2".into(),
                    port: 443,
                    ..Default::default()
                },
            ],
            selected_server_id: Some("s1".into()),
            custom_rules: vec![Rule {
                id: "r1".into(),
                type_field: RuleType::Domain,
                values: vec!["example.com".into()],
                conditions: None,
                combine_mode: None,
                action: RuleAction::Proxy,
                enabled: true,
                bypass_fakeip: None,
                target_server_id: Some("s2".into()),
                remarks: None,
                tls_spoof: None,
                tls_spoof_method: None,
            }],
            ..Default::default()
        };
        let refs = referenced_server_ids(&config);
        assert!(refs.contains("s1")); // 选中
        assert!(refs.contains("s2")); // 规则目标
    }

    // === selector default 兜底（proxy-selector 的 default 落到「非选中节点」）===

    fn ss(id: &str, addr: &str) -> ServerConfig {
        ServerConfig {
            id: id.into(),
            name: id.into(),
            protocol: Protocol::Shadowsocks,
            address: addr.into(),
            port: 8388,
            ..Default::default()
        }
    }

    fn three_plain_nodes() -> Vec<ServerConfig> {
        vec![
            ss("s1", "1.1.1.1"),
            ss("s2", "2.2.2.2"),
            ss("s3", "3.3.3.3"),
        ]
    }

    /// 【常态不得被拖成恒重启】正常选中了一个必定被发射的普通代理节点 ⇒ 兜底不可能触发 ⇒
    /// 引用集只含选中节点，未引用节点仍可 defer。
    /// 本用例红 = 修复过度保守，把「正常选了节点」也拖成了「任何节点编辑都重启」。
    #[test]
    fn referenced_ids_normal_selection_stays_minimal() {
        use crate::user_config::app_config::UserConfig;
        let config = UserConfig {
            servers: three_plain_nodes(),
            selected_server_id: Some("s2".into()),
            ..Default::default()
        };
        let refs = referenced_server_ids(&config);
        assert!(!selector_default_may_fall_back(&config));
        assert_eq!(refs, ["s2".to_string()].into_iter().collect());
    }

    /// 【缺陷复现①：未选节点】`selectedServerId=None` ⇒ `build_outbounds`（outbounds.rs:262-271）
    /// 的 `selected_tag` 是字面量 `"proxy"`，匹配不到任何节点 tag → default 落 `node_tags.first()`。
    /// 那个节点承载**全部**代理流量，却不在任何一条播种里。
    /// 本用例红 = 兜底节点又漏出引用集 → 编辑它会被判「未引用」走 defer 腿静默不重启。
    #[test]
    fn referenced_ids_without_selection_includes_all_nodes() {
        use crate::user_config::app_config::UserConfig;
        let config = UserConfig {
            servers: three_plain_nodes(),
            selected_server_id: None,
            ..Default::default()
        };
        assert!(selector_default_may_fall_back(&config));
        let refs = referenced_server_ids(&config);
        // 「哪个节点会被首先发射」取决于生成期跳过了谁（运行期能力），静态算不出 ⇒ 全部纳入。
        for id in ["s1", "s2", "s3"] {
            assert!(refs.contains(id), "{id} 未纳入引用集");
        }
    }

    /// 【缺陷复现②：悬空选中】选中 id 不在 servers 里（节点被删/订阅换了 id）⇒ id→tag 解析不到
    /// → 同样落 `node_tags.first()` 兜底。
    #[test]
    fn referenced_ids_dangling_selection_includes_all_nodes() {
        use crate::user_config::app_config::UserConfig;
        let config = UserConfig {
            servers: three_plain_nodes(),
            selected_server_id: Some("ghost".into()),
            ..Default::default()
        };
        assert!(selector_default_may_fall_back(&config));
        let refs = referenced_server_ids(&config);
        for id in ["s1", "s2", "s3"] {
            assert!(refs.contains(id), "{id} 未纳入引用集");
        }
    }

    /// 【缺陷复现③：选中节点生成期可能被跳过】naive 缺 libcronet 时被 `outbounds.rs:136-138` 跳过。
    /// libcronet 是**运行期**能力，UserConfig 看不见 ⇒ 必须保守当作「可能没发射」→ 兜底可能触发。
    #[test]
    fn referenced_ids_selected_naive_includes_all_nodes() {
        use crate::user_config::app_config::UserConfig;
        let mut servers = three_plain_nodes();
        servers[1].protocol = Protocol::Naive;
        let config = UserConfig {
            servers,
            selected_server_id: Some("s2".into()),
            ..Default::default()
        };
        assert!(selector_default_may_fall_back(&config));
        let refs = referenced_server_ids(&config);
        for id in ["s1", "s2", "s3"] {
            assert!(refs.contains(id), "{id} 未纳入引用集");
        }
    }

    /// 【直连哨兵不触发全纳入】`__direct__` ⇒ default 恒 = `direct` 出站，没有节点承载
    /// （outbounds.rs:262-263 的 `is_direct` 腿）⇒ 引用集不得被撑成全体。
    #[test]
    fn referenced_ids_direct_sentinel_no_blanket_inclusion() {
        use crate::user_config::app_config::UserConfig;
        let config = UserConfig {
            servers: three_plain_nodes(),
            selected_server_id: Some("__direct__".into()),
            ..Default::default()
        };
        assert!(!selector_default_may_fall_back(&config));
        assert!(referenced_server_ids(&config).is_empty());
    }

    /// 【WG 复用真判据】选中 WG 节点时，「会不会被发射」直接问 `build_wireguard_endpoint`：
    /// 配置完整 ⇒ 必定发射（不触发兜底）；缺 privateKey ⇒ Err ⇒ 不发射 ⇒ 兜底可能触发。
    /// 本用例红 = 判据与真正的发射腿漂移了。
    #[test]
    fn selector_fallback_tracks_wireguard_buildability() {
        use crate::user_config::app_config::UserConfig;
        let mut wg = wg_server("wg1", &["10.0.0.0/24"], Some(true));
        let s = wg.wireguard_settings.as_mut().unwrap();
        s.private_key = Some("k".into());
        s.peer_public_key = Some("p".into());
        s.local_address = vec!["10.0.0.2/32".into()];
        let mut config = UserConfig {
            servers: vec![wg.clone(), ss("s2", "2.2.2.2")],
            selected_server_id: Some("wg1".into()),
            ..Default::default()
        };
        assert!(
            !selector_default_may_fall_back(&config),
            "配置完整的 WG 必定发射"
        );
        assert!(!referenced_server_ids(&config).contains("s2"));

        config.servers[0]
            .wireguard_settings
            .as_mut()
            .unwrap()
            .private_key = None;
        assert!(
            selector_default_may_fall_back(&config),
            "缺 privateKey 的 WG 构建失败 → 不发射 → 兜底可能触发"
        );
        assert!(referenced_server_ids(&config).contains("s2"));
    }

    #[test]
    fn custom_endpoint_carries_traffic_detects_keys() {
        use serde_json::json;
        // system 键命中
        assert!(custom_endpoint_carries_traffic(&json!({"system": true})));
        // allowed_ips 嵌套命中
        assert!(custom_endpoint_carries_traffic(&json!({
            "peers": [{"allowed_ips": ["0.0.0.0/0"]}]
        })));
        // 无语义键
        assert!(!custom_endpoint_carries_traffic(
            &json!({"tag": "x", "type": "wireguard"})
        ));
        // 数组递归
        assert!(custom_endpoint_carries_traffic(
            &json!([{"exit_node": true}])
        ));
    }

    /// OpenVPN 全隧道必须被判为承流 —— 语料取**真实可用**的 `openvpn-client` 端点
    /// （对随包核 1.14.0-beta.12 跑 `sing-box check` rc=0 的形状：`tls` 必填，缺了报
    /// `missing 'tls' options`），不是手捏一个只有目标键的空壳。
    ///
    /// 变异靶：把 `redirect_gateway` 从 `CARRY_TRAFFIC_KEYS` 里删掉 → 第一条 assert 转红。
    /// 这条**不能**只写「含 routes 的那份命中」——OpenVPN 表达全隧道的常见写法就是只给
    /// `redirect_gateway: true` 而不写 `routes`，那正是原表漏掉的那一半。
    #[test]
    fn openvpn_full_tunnel_counts_as_carrying_traffic() {
        use serde_json::json;
        let ovpn = |extra: serde_json::Value| {
            let mut base = json!({
                "type": "openvpn-client",
                "server": "1.2.3.4",
                "server_port": 1194,
                "username": "u",
                "password": "p",
                "tls": { "certificate": ["-----BEGIN CERTIFICATE-----"] }
            });
            let map = base.as_object_mut().unwrap();
            for (k, v) in extra.as_object().unwrap() {
                map.insert(k.clone(), v.clone());
            }
            base
        };
        // 只给 redirect_gateway（不写 routes）—— 补键之前这条是 false
        assert!(custom_endpoint_carries_traffic(&ovpn(
            json!({"redirect_gateway": true})
        )));
        assert!(custom_endpoint_carries_traffic(&ovpn(
            json!({"redirect_private": true})
        )));
        assert!(custom_endpoint_carries_traffic(&ovpn(
            json!({"route_no_pull": true})
        )));
        // 不过度纳入：纯拨号型 openvpn-client（无任何路由语义键）仍判 false，
        // 否则「过度纳入只多一次重启」会退化成「每个 OpenVPN 节点必重启」。
        assert!(!custom_endpoint_carries_traffic(&ovpn(json!({}))));
        // openconnect 的路由语义键只有 system —— 原表已覆盖，这条钉住别在补键时把它漏掉。
        assert!(custom_endpoint_carries_traffic(&json!({
            "type": "openconnect", "server": "vpn.example.com:443",
            "username": "u", "password": "p", "flavor": "anyconnect", "system": true
        })));
        assert!(!custom_endpoint_carries_traffic(&json!({
            "type": "openconnect", "server": "vpn.example.com:443",
            "username": "u", "password": "p", "flavor": "anyconnect"
        })));
    }

    /// WG `reverseMesh:true` 的三态：普通 WG 放行 / WARP（带凭据）否决 / WARP（仅域名）否决。
    ///
    /// 一个测试里放三条是**故意**的：把「否决」和「不过度否决」钉在同一处，
    /// 免得后人只看到 WARP 那两条 assert 就把整条 WG 腿改成恒 false —— 那会让正常 WG 的
    /// System 接入模式（子网路由 / 反向可达）整体失效，是个只有真机才暴露的静默回归。
    #[test]
    fn wg_reverse_mesh_system_vetoed_only_for_warp() {
        fn wg_reverse_mesh(address: &str, warp_device: bool) -> ServerConfig {
            ServerConfig {
                id: "w".into(),
                name: "w".into(),
                protocol: Protocol::Wireguard,
                address: address.into(),
                port: 2408,
                wireguard_settings: Some(WireGuardSettings {
                    reverse_mesh: Some(true),
                    warp_device: warp_device.then(|| {
                        crate::user_config::protocol_settings::WarpDevice {
                            device_id: "d".into(),
                            token: "t".into(),
                        }
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        // 反向对照（**先写**）：普通 WG 的 System 接入模式必须照旧生效，否决不得收得过宽。
        assert!(
            mesh_uses_system_interface(&wg_reverse_mesh("vpn.example.com", false)),
            "非 WARP 的 WG reverseMesh:true 必须仍返 true —— 收宽了就是把 System 接入模式整体废掉"
        );

        // 新注册的 WARP：带自删凭据，address 是注册响应给的裸 IP。
        assert!(
            !mesh_uses_system_interface(&wg_reverse_mesh("162.159.192.1", true)),
            "WARP（warpDevice 标记）reverseMesh:true 必须被否决 —— 抢 utun ⇒ resource busy FATAL"
        );

        // 旧 / 导入 / 上游 迁移来的 WARP：无 warpDevice，只能靠端点域名兜底。
        // 这三条腿都不经渲染端，前端的否决在此无效 —— 本用例守的就是那道口子。
        assert!(
            !mesh_uses_system_interface(&wg_reverse_mesh("engage.cloudflareclient.com", false)),
            "无 warpDevice 标记的旧 WARP 必须按域名兜底否决（导入/手改/迁移绕过前端）"
        );
    }
}
