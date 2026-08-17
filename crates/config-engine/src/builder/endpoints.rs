//! WireGuard/Tailscale endpoint 构造（上游 `buildWireGuardEndpoint` + `buildTailscaleEndpoint`）。

#![forbid(unsafe_code)]

use crate::builder::endpoint_routes::{
    mesh_node_carries_full_tunnel, mesh_uses_system_interface, wireguard_peer_allowed_ips,
    TS_SYSTEM_INTERFACE_NAME, WG_SYSTEM_INTERFACE_NAME,
};
use crate::singbox::{DomainResolver, Endpoint, WireGuardPeer};
use crate::user_config::ip::is_ip_literal;
use crate::user_config::server_config::ServerConfig;

/// Taildrop 收件目录相对 `state_dir` 的子目录名。取与内核默认值相同的字面量（`"Taildrop"`），
/// 差别只在**我们把它锚成绝对路径**、不让它跟着 CWD 漂 —— 见
/// [`crate::singbox::Endpoint::taildrop_directory`]。
const TAILDROP_SUBDIR: &str = "Taildrop";

/// WireGuard endpoint 构造。上游 `buildWireGuardEndpoint`。
/// domain_resolver + platform + tailscale_state_dir（路径）注入。
///
/// `domain_resolver` **纯透传**（#335）：本函数不构造也不给默认值，调用方用
/// [`get_node_dial_domain_resolver`](crate::builder::helpers::get_node_dial_domain_resolver) 备好。
/// 类型是 [`DomainResolver`] 而非 `&str`，新增 call site 塞裸 tag 会编译失败而非静默回落未修形态。
/// `None` 仍表示「不下发」（IP 直拨节点、以及 `endpoint_routes` 的可构造性预检）。
///
/// `detour_tag` = 前置代理的 **outbound tag**（已由调用方经 id→tag 映射解析 + 排除 endpoint 目标，
/// 见 `builder/outbounds.rs#resolve_detour_tag`；本函数不做解析，也不接受 server id）。
/// 这是对 上游的**有意偏离**（上游的 WG 表单与 `SingBoxEndpoint` 都没有 detour），
/// 语义实测与「前置代理必须支持 UDP 转发」这条硬约束见 `singbox/endpoint.rs` 的 `Endpoint::detour`。
pub fn build_wireguard_endpoint(
    server: &ServerConfig,
    tag: &str,
    domain_resolver: Option<&DomainResolver>,
    platform: &str,
    detour_tag: Option<&str>,
) -> Result<Endpoint, String> {
    let s = server
        .wireguard_settings
        .as_ref()
        .ok_or("WireGuard 配置缺失 wireguardSettings")?;
    let private_key = s.private_key.clone().ok_or("WireGuard 缺少 privateKey")?;
    let peer_public_key = s
        .peer_public_key
        .clone()
        .ok_or("WireGuard 缺少 peerPublicKey")?;
    if s.local_address.is_empty() {
        return Err("WireGuard 缺少 localAddress".into());
    }

    let allowed_ips = wireguard_peer_allowed_ips(server).ok_or_else(|| {
        "WireGuard 节点无可路由网段（关外网或 system 内核接口且无具体段）：空 allowed_ips 致 FATAL".to_string()
    })?;

    // 域名 server 才需 domain_resolver（IP 直拨无需）。
    let needs_resolver = domain_resolver.is_some() && !is_ip_literal(&server.address);
    let uses_system = mesh_uses_system_interface(server);

    let mut ep = Endpoint {
        type_field: "wireguard".into(),
        tag: tag.to_string(),
        domain_resolver: None,
        detour: detour_tag.map(String::from),
        extra: serde_json::Map::new(),
        system: None,
        mtu: None,
        address: None,
        private_key: None,
        listen_port: None,
        peers: None,
        udp_timeout: None,
        workers: None,
        auth_key: None,
        state_directory: None,
        control_url: None,
        hostname: None,
        exit_node: None,
        exit_node_allow_lan_access: None,
        accept_routes: None,
        ephemeral: None,
        advertise_routes: None,
        system_interface: None,
        system_interface_name: None,
        name: None,
        advertise_tags: None,
        ssh_server: None,
        relay_server_port: None,
        taildrop_directory: None,
    };

    if needs_resolver {
        ep.domain_resolver = domain_resolver.cloned();
    }
    ep.system = Some(uses_system);
    if uses_system && platform != "darwin" {
        ep.name = Some(WG_SYSTEM_INTERFACE_NAME.to_string());
    }
    let default_mtu = if crate::warp::is_warp_server(server) {
        crate::warp::WARP_MTU
    } else {
        1408
    };
    ep.mtu = Some(s.mtu.filter(|mtu| *mtu > 0).unwrap_or(default_mtu));
    ep.address = Some(s.local_address.clone());
    ep.private_key = Some(private_key);
    let mut peer = WireGuardPeer {
        address: server.address.clone(),
        port: server.port,
        public_key: peer_public_key,
        pre_shared_key: s.pre_shared_key.clone(),
        allowed_ips,
        // 缺省按 Polaris 既有策略回落 25 秒；显式 0 遵循 WireGuard 语义关闭保活。
        persistent_keepalive_interval: Some(s.persistent_keepalive.unwrap_or(25)),
        reserved: None,
    };
    if s.reserved.len() == 3 {
        peer.reserved = Some(s.reserved.clone());
    }
    ep.peers = Some(vec![peer]);

    Ok(ep)
}

/// Tailscale endpoint 构造。上游 `buildTailscaleEndpoint`。
/// state_dir 注入（生产 = UserData/tailscale/<id>，对拍 = 固定假路径）。
///
/// `detour_tag` 同 [`build_wireguard_endpoint`]：已解析好的 outbound tag，本函数不做解析。
/// 对 上游的有意偏离；TS 侧经前置代理的是**控制面 / DERP 的 TCP 拨号**（异于 WG 的 UDP），
/// 实测见 `singbox/endpoint.rs` 的 `Endpoint::detour`。
pub fn build_tailscale_endpoint(
    server: &ServerConfig,
    tag: &str,
    state_dir: &str,
    platform: &str,
    detour_tag: Option<&str>,
) -> Endpoint {
    let ts = server.tailscale_settings.clone().unwrap_or_default();
    let mut ep = Endpoint {
        type_field: "tailscale".into(),
        tag: tag.to_string(),
        domain_resolver: None,
        detour: detour_tag.map(String::from),
        extra: serde_json::Map::new(),
        system: None,
        mtu: None,
        address: None,
        private_key: None,
        listen_port: None,
        peers: None,
        udp_timeout: None,
        workers: None,
        auth_key: ts
            .auth_key
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        state_directory: Some(state_dir.to_string()),
        control_url: ts
            .control_url
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        hostname: ts
            .hostname
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        exit_node: None,
        exit_node_allow_lan_access: None,
        accept_routes: None,
        ephemeral: None,
        advertise_routes: None,
        system_interface: None,
        system_interface_name: None,
        name: None,
        advertise_tags: None,
        ssh_server: None,
        relay_server_port: None,
        // 恒填绝对路径，绝不留给内核默认值 —— 默认是相对的 `Taildrop`，按核进程 CWD 解析后
        // 无条件 mkdir。为什么这是硬约束（含 Windows helper 那条 CWD 腿）见
        // [`crate::singbox::Endpoint::taildrop_directory`]。
        // 落在 state_dir 之下而不是与之并列：state_dir 已按节点 id 分好、随节点删除一起清理，
        // 收件目录跟着走即天然隔离；同时它是 state_dir 的**子目录**，peer 送来的文件名不可能
        // 撞上 `tailscaled.state` 这类密钥文件。
        taildrop_directory: Some(format!("{state_dir}/{TAILDROP_SUBDIR}")),
    };

    // exit_node 仅承载全隧道时下发。
    if mesh_node_carries_full_tunnel(server) {
        if let Some(en) = &ts.exit_node {
            let en = en.trim();
            if !en.is_empty() {
                ep.exit_node = Some(en.to_string());
                ep.exit_node_allow_lan_access = if ts.exit_node_allow_lan_access == Some(true) {
                    Some(true)
                } else {
                    None
                };
            }
        }
    }
    ep.accept_routes = if ts.accept_routes == Some(true) {
        Some(true)
    } else {
        None
    };
    ep.ephemeral = if ts.ephemeral == Some(true) {
        Some(true)
    } else {
        None
    };
    let adv: Vec<String> = ts
        .advertise_routes
        .iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    if !adv.is_empty() {
        ep.advertise_routes = Some(adv);
    }
    let adv_tags: Vec<String> = ts
        .advertise_tags
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if !adv_tags.is_empty() {
        ep.advertise_tags = Some(adv_tags);
    }
    ep.ssh_server = if ts.ssh_server == Some(true) {
        Some(true)
    } else {
        None
    };
    if let Some(p) = ts.relay_server_port {
        if p > 0 {
            ep.relay_server_port = Some(p);
        }
    }
    // `0` = 内核语义里的「自动选端口」（= 不设）。与 relay_server_port 同一口径：不把等价于默认值的
    // 显式 0 写进配置，免得日后上游改默认时磁盘上躺着一份冻结的旧默认。
    if let Some(p) = ts.listen_port {
        if p > 0 {
            ep.listen_port = Some(p);
        }
    }
    // Phase 2 reverseMesh → system_interface。
    if mesh_uses_system_interface(server) {
        ep.system_interface = Some(true);
        if platform != "darwin" {
            ep.system_interface_name = Some(TS_SYSTEM_INTERFACE_NAME.to_string());
        }
    }

    ep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::server_config::{Protocol, ServerConfig, WireGuardSettings};

    #[test]
    fn wg_endpoint_basic() {
        let mut s = ServerConfig {
            id: "w1".into(),
            name: "WG".into(),
            protocol: Protocol::Wireguard,
            address: "1.2.3.4".into(),
            port: 51820,
            ..Default::default()
        };
        s.wireguard_settings = Some(Box::new(WireGuardSettings {
            private_key: Some("priv".into()),
            peer_public_key: Some("pub".into()),
            local_address: vec!["10.0.0.2/32".into()],
            allow_internet: Some(true),
            ..Default::default()
        }));
        let dial = crate::builder::helpers::get_node_dial_domain_resolver("dns-bootstrap", false);
        let ep = build_wireguard_endpoint(&s, "tag-w1", Some(&dial), "linux", None).unwrap();
        assert_eq!(ep.type_field, "wireguard");
        assert_eq!(ep.mtu, Some(1408));
        assert_eq!(ep.peers.as_ref().unwrap()[0].address, "1.2.3.4");
        // allowInternet=on → allowed_ips 含 0/0。
        assert!(ep.peers.as_ref().unwrap()[0]
            .allowed_ips
            .contains(&"0.0.0.0/0".to_string()));
    }

    #[test]
    fn wg_unroutable_errors() {
        let mut s = ServerConfig {
            id: "w1".into(),
            name: "WG".into(),
            protocol: Protocol::Wireguard,
            address: "1.2.3.4".into(),
            port: 51820,
            ..Default::default()
        };
        s.wireguard_settings = Some(Box::new(WireGuardSettings {
            private_key: Some("priv".into()),
            peer_public_key: Some("pub".into()),
            local_address: vec!["10.0.0.2/32".into()],
            allow_internet: Some(false), // 关外网
            allowed_ips: vec![],         // 无具体段
            ..Default::default()
        }));
        assert!(build_wireguard_endpoint(&s, "tag", None, "linux", None).is_err());
    }

    #[test]
    fn ts_endpoint_exit_node_when_full_tunnel() {
        let mut s = ServerConfig {
            id: "t1".into(),
            name: "TS".into(),
            protocol: Protocol::Tailscale,
            ..Default::default()
        };
        s.tailscale_settings = Some(Box::new(
            crate::user_config::server_config::TailscaleSettings {
                exit_node: Some("exit-peer".into()),
                ..Default::default()
            },
        ));
        let ep = build_tailscale_endpoint(&s, "tag-t1", "/fake/ts/t1", "linux", None);
        // exit_node 设 → mesh_allows_internet=true → exit_node 下发。
        assert_eq!(ep.exit_node.as_deref(), Some("exit-peer"));
    }

    /// `taildrop_directory` **恒下发且恒绝对**（1.14.0-beta.15）。
    ///
    /// 这条不是「多测一个字段」：金样快照里**一个 tailscale endpoint 都没有**，
    /// 整套 golden/`sing-box check` 对拍对本字段的检出力恒为 0 —— 缺了这条断言，
    /// 把它改回 `None`（= 回落到内核那个跟着 CWD 漂的相对默认值）不会红任何门。
    #[test]
    fn ts_endpoint_always_pins_taildrop_directory_under_state_dir() {
        let mut s = ServerConfig {
            id: "t1".into(),
            name: "TS".into(),
            protocol: Protocol::Tailscale,
            ..Default::default()
        };
        // 用户一个 tailscale 设置都没填的最小形态：本字段仍须下发。
        s.tailscale_settings = Some(Default::default());
        let ep = build_tailscale_endpoint(&s, "tag-t1", "/fake/ts/t1", "linux", None);
        let dir = ep
            .taildrop_directory
            .as_deref()
            .expect("taildrop_directory 必须下发，不得留给内核相对默认值");
        assert_eq!(dir, "/fake/ts/t1/Taildrop");
        // 绝对性是本字段存在的**唯一理由**：相对路径会被内核按核进程 CWD 解析。
        assert!(
            dir.starts_with('/') || dir.as_bytes().get(1) == Some(&b':'),
            "必须是绝对路径（unix `/…` 或 Windows `X:\\…`），实得 {dir}"
        );
        assert!(
            dir.starts_with("/fake/ts/t1"),
            "须落在该节点自己的 state_dir 之下，随节点一起清理，实得 {dir}"
        );
    }

    /// `listen_port`：填了才下发，`0` 与未填一律不下发。
    ///
    /// 同 `taildrop_directory` 那条的理由 —— 金样里零个 tailscale endpoint，对拍抓不到这条接线；
    /// 而 `Endpoint.listen_port` 是 WG 腿也在用的**共用字段**，接错了不会编译失败，只会静默不发。
    #[test]
    fn ts_endpoint_emits_listen_port_only_when_set_nonzero() {
        fn ep_with(port: Option<u16>) -> Endpoint {
            let mut s = ServerConfig {
                id: "t1".into(),
                name: "TS".into(),
                protocol: Protocol::Tailscale,
                ..Default::default()
            };
            s.tailscale_settings = Some(Box::new(
                crate::user_config::server_config::TailscaleSettings {
                    listen_port: port,
                    ..Default::default()
                },
            ));
            build_tailscale_endpoint(&s, "tag-t1", "/fake/ts/t1", "linux", None)
        }
        assert_eq!(ep_with(Some(41641)).listen_port, Some(41641));
        // 0 = 内核的「自动选端口」，等价未设 ⇒ 不写进配置。
        assert_eq!(ep_with(Some(0)).listen_port, None);
        assert_eq!(ep_with(None).listen_port, None);
    }

    /// WireGuard 腿**不得**下发 `taildrop_directory`（该键只属 tailscale endpoint）。
    #[test]
    fn wg_endpoint_never_sets_taildrop_directory() {
        let s = ServerConfig {
            id: "w1".into(),
            name: "WG".into(),
            protocol: Protocol::Wireguard,
            address: "1.2.3.4".into(),
            port: 51820,
            wireguard_settings: Some(Box::new(WireGuardSettings {
                private_key: Some("priv".into()),
                peer_public_key: Some("pub".into()),
                local_address: vec!["10.0.0.2/32".into()],
                ..Default::default()
            })),
            ..Default::default()
        };
        let ep = build_wireguard_endpoint(&s, "tag-w1", None, "linux", None).unwrap();
        assert_eq!(ep.taildrop_directory, None);
    }

    /// 落盘态直达生成器：`reverseMesh:true` 的 WARP 节点**不得**发出 `system:true` / 接口名。
    ///
    /// 谓词单测（`endpoint_routes.rs`）只钉判据；这条钉的是**发射面** —— 判据与 `ep.system`
    /// 之间的接线断了（比如有人把 `ep.system = Some(uses_system)` 改成 `Some(reverse_mesh)`），
    /// 谓词测试照绿，而磁盘上的 WARP 节点照样 FATAL。
    #[test]
    fn warp_endpoint_policy_differs_from_plain_wireguard() {
        fn wg(address: &str) -> ServerConfig {
            ServerConfig {
                id: "w1".into(),
                name: "WARP".into(),
                protocol: Protocol::Wireguard,
                address: address.into(),
                port: 2408,
                wireguard_settings: Some(Box::new(WireGuardSettings {
                    private_key: Some("priv".into()),
                    peer_public_key: Some("pub".into()),
                    local_address: vec!["172.16.0.2/32".into()],
                    reverse_mesh: Some(true),
                    ..Default::default()
                })),
                ..Default::default()
            }
        }

        // 非 darwin 才会下发接口名 → 用 linux 让「接口名也没漏出去」这条断言有意义。
        let warp = build_wireguard_endpoint(
            &wg("engage.cloudflareclient.com"),
            "tag",
            None,
            "linux",
            None,
        )
        .expect("WARP endpoint 应能构建");
        assert_eq!(warp.system, Some(false), "WARP 不得发 system:true");
        assert_eq!(warp.name, None, "WARP 不得占用内核接口名");
        assert_eq!(warp.mtu, Some(crate::warp::WARP_MTU));

        // 反向对照：同样的 reverseMesh:true，普通 WG 仍应发 system:true + 接口名。
        let plain = build_wireguard_endpoint(&wg("vpn.example.com"), "tag", None, "linux", None)
            .expect("普通 WG endpoint 应能构建");
        assert_eq!(plain.system, Some(true));
        assert_eq!(plain.name.as_deref(), Some(WG_SYSTEM_INTERFACE_NAME));
        assert_eq!(plain.mtu, Some(1408));

        // 用户显式设置始终优先于协议缺省值。
        let mut custom = wg("engage.cloudflareclient.com");
        let settings = custom.wireguard_settings.as_mut().unwrap();
        settings.mtu = Some(1360);
        settings.persistent_keepalive = Some(0);
        let custom = build_wireguard_endpoint(&custom, "tag", None, "linux", None)
            .expect("显式 MTU 的 WARP endpoint 应能构建");
        assert_eq!(custom.mtu, Some(1360));
        assert_eq!(
            custom.peers.unwrap()[0].persistent_keepalive_interval,
            Some(0),
            "显式 0 应关闭保活，不能被改写回 25 秒"
        );
    }
}
