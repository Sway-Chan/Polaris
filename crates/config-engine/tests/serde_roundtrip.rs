//! serde 序列化往返测试：验证 sing-box 类型的 skip_serializing_if / rename / OneOrMany 行为。
//!
//! 这是对拍正确性的前置保证：TS 侧 undefined 键不进 JSON，Rust 侧 None 也不进；
//! 关键字字段（final）正确 rename；OneOrMany 单值出裸 string 多值出数组。
//! 若这些行为错，金样对拍 diff 永远≠0。

use polaris_config_engine::singbox::*;
use serde_json::json;

#[test]
fn none_fields_are_skipped() {
    // 仅必填字段 → 输出不含任何 optional 键（与 TS undefined 不序列化等价）。
    let ob = Outbound {
        type_field: "direct".into(),
        tag: "direct".into(),
        detour: None,
        server: None,
        server_port: None,
        override_address: None,
        method: None,
        password: None,
        username: None,
        plugin: None,
        plugin_opts: None,
        uuid: None,
        security: None,
        alter_id: None,
        flow: None,
        packet_encoding: None,
        up_mbps: None,
        down_mbps: None,
        obfs: None,
        auth_str: None,
        executable_path: None,
        data_directory: None,
        extra_args: None,
        torrc: None,
        bbr_profile: None,
        disable_chrome_parrot: None,
        network: None,
        quic: None,
        congestion_control: None,
        udp_relay_mode: None,
        zero_rtt_handshake: None,
        heartbeat: None,
        version: None,
        psk: None,
        userkey: None,
        reuse: None,
        obfs_mode: None,
        obfs_host: None,
        mode: None,
        idle_session_check_interval: None,
        idle_session_timeout: None,
        min_idle_session: None,
        path: None,
        headers: None,
        tls: None,
        transport: None,
        multiplex: None,
        server_ports: None,
        hop_interval: None,
        domain_resolver: None,
        udp_over_tcp: None,
        udp_fragment: None,
        user: None,
        private_key: None,
        private_key_path: None,
        private_key_passphrase: None,
        host_key: None,
        host_key_algorithms: None,
        client_version: None,
        cipher: None,
        mac: None,
        kex_algorithm: None,
        outbounds: None,
        default: None,
        interrupt_exist_connections: None,
        // 建模协议一律留空 ⇒ flatten 不产生任何键。下面那句逐字符断言就是「加 `extra` 不改变既有
        // 序列化字节」的门：`flatten` 让 derive 从 `serialize_struct` 换成 `serialize_map`，若这个
        // 换法对 serde_json 的输出**不**等价，这里立刻转红（而不是等 37 例金样对拍去发现）。
        extra: serde_json::Map::new(),
    };
    let s = serde_json::to_string(&ob).unwrap();
    assert_eq!(s, r#"{"type":"direct","tag":"direct"}"#);
}

#[test]
fn one_or_many_serializes_single_as_bare() {
    // OneOrMany::One → 裸值（非单元素数组），与 sing-box schema 的 Listable 语义一致。
    let r = RouteRule {
        rule_set: Some(OneOrMany::One("geosite-cn".into())),
        inbound: Some(OneOrMany::One("mixed-in".into())),
        protocol: None,
        network: None,
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
        action: Some("route".into()),
        outbound: Some("direct".into()),
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
    };
    let v: serde_json::Value = serde_json::to_value(&r).unwrap();
    assert_eq!(v["rule_set"], json!("geosite-cn"));
    assert_eq!(v["inbound"], json!("mixed-in"));
}

#[test]
fn one_or_many_serializes_many_as_array() {
    let r = RouteRule {
        rule_set: Some(OneOrMany::Many(vec![
            "geosite-cn".into(),
            "geoip-cn".into(),
        ])),
        inbound: None,
        protocol: None,
        network: None,
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
        action: Some("route".into()),
        outbound: Some("proxy-selector".into()),
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
    };
    let v: serde_json::Value = serde_json::to_value(&r).unwrap();
    assert_eq!(v["rule_set"], json!(["geosite-cn", "geoip-cn"]));
}

#[test]
fn dns_final_keyword_field_renamed() {
    // Rust 关键字 `final` → 字段 final_server 经 serde rename 序列化为 "final"。
    let dns = DnsConfig {
        servers: vec![],
        rules: None,
        final_server: Some("dns-proxy".into()),
        strategy: None,
        fakeip: None,
        reverse_mapping: None,
        optimistic: None,
        timeout: None,
    };
    let v: serde_json::Value = serde_json::to_value(&dns).unwrap();
    assert_eq!(v["final"], json!("dns-proxy"));
    assert!(
        v.get("final_server").is_none(),
        "Rust 字段名不应出现在 JSON"
    );
}

#[test]
fn full_config_roundtrip() {
    // 最小顶层 config 序列化 → 反序列化 → 相等（验证 derive 一致性）。
    let cfg = SingBoxConfig {
        log: LogConfig {
            level: "info".into(),
            timestamp: true,
            output: None,
            disabled: None,
        },
        dns: None,
        inbounds: vec![],
        outbounds: vec![],
        endpoints: None,
        route: None,
        experimental: None,
        services: None,
    };
    let s = serde_json::to_string(&cfg).unwrap();
    let back: SingBoxConfig = serde_json::from_str(&s).unwrap();
    assert_eq!(cfg, back);
}

/// `DomainResolver` 联合类型两种形态的双向往返（#335）。
///
/// 复用本文件既有的 untagged 验证位（`OneOrMany` 同款机制、同款风险）：untagged 枚举的**反序列化
/// 方向**没有别的门盯着——`build_proxy_outbound` 的 Custom 分支会把用户 raw-JSON `serde_json::from_value`
/// 成 `Outbound`，若这里解不出来，整个自定义节点会静默回落成默认 Outbound（不 panic、不报错）。
#[test]
fn domain_resolver_union_roundtrips_both_shapes() {
    // 纯 tag ⇒ 裸字符串（enableIPv6=true 分支的形态，金样字节零变化靠它）。
    let tag = DomainResolver::Tag("dns-bootstrap".into());
    assert_eq!(serde_json::to_value(&tag).unwrap(), json!("dns-bootstrap"));
    assert_eq!(
        serde_json::from_value::<DomainResolver>(json!("dns-bootstrap")).unwrap(),
        tag
    );

    // 结构化 ⇒ {server, strategy}（enableIPv6=false 分支；键名/值须与上游 逐字一致）。
    let detailed = DomainResolver::Detailed {
        server: "dns-bootstrap".into(),
        strategy: DomainStrategy::PreferIpv4,
    };
    assert_eq!(
        serde_json::to_value(&detailed).unwrap(),
        json!({"server": "dns-bootstrap", "strategy": "prefer_ipv4"})
    );
    assert_eq!(
        serde_json::from_value::<DomainResolver>(
            json!({"server": "dns-bootstrap", "strategy": "prefer_ipv4"})
        )
        .unwrap(),
        detailed
    );

    // 四个 strategy 变体的线上拼写（枚举 → snake_case）：写错一个就是内核解析期才发现。
    for (variant, wire) in [
        (DomainStrategy::PreferIpv4, "prefer_ipv4"),
        (DomainStrategy::PreferIpv6, "prefer_ipv6"),
        (DomainStrategy::Ipv4Only, "ipv4_only"),
        (DomainStrategy::Ipv6Only, "ipv6_only"),
    ] {
        assert_eq!(serde_json::to_value(variant).unwrap(), json!(wire));
    }
}
