//! 代理 Outbound 构造（上游 `buildProxyOutbound` + `generateTransportConfig` +
//! `applyAntiCensorshipOptions` 1:1 移植）。20 协议字段映射 + TLS/Reality/传输层 + 抗封后处理。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use crate::builder::outbound_helpers::{
    is_quic_managed_tls, normalize_duration, parse_ws_early_data, should_emit_tls_engine,
};
use crate::singbox::{
    DomainResolver, Ech, Hysteria2Obfs, Multiplex, OneOrMany, Outbound, OutboundTls, Reality,
    Transport, Utls,
};
use crate::user_config::normalize::normalize_token;
use crate::user_config::protocol_settings::custom_outbound_type;
use crate::user_config::server_config::{Protocol, SecurityMode, ServerConfig};
use crate::user_config::tls_spoof::validate_tls_spoof_default;

/// TLS 协议集（恒需 TLS 块即使无 tlsSettings）。
///
/// `hysteria`（**v1**）2026-08-11 补入：随包核对缺 TLS 的 hysteria v1 出站判
/// `initialize outbound[0]: TLS required` —— 是 **initialize 阶段**硬失败，不是「少个可选块」。
/// 这条不是从文档推的，是新加协议时被 `bundled_core_accepts_hysteria_v1_and_tor` 当场判红逼出来的。
const TLS_PROTOCOLS: &[&str] = &["trojan", "anytls", "hysteria2", "tuic", "hysteria"];

/// 内核允许挂 `transport` 的出站类型 —— **白名单，判据取自内核 schema**。
///
/// 随包核 beta.7 `sing-box schema` → `$defs/Outbound` 的 20 支 oneOf 里，只有这三支有
/// `transport` 属性；其余 17 支一律 `additionalProperties:false` 且无该键。
const TRANSPORT_CAPABLE: &[Protocol] = &[Protocol::Trojan, Protocol::Vless, Protocol::Vmess];

/// 该协议的出站能不能带 `transport`（ws/grpc/http/httpupgrade 那一层）。
///
/// **导出给 `net-stack` 复用** —— 导入侧要据此告诉用户「你这个节点上的传输层参数不会生效」。
/// 那边不许复制一份自己的名单：复制出来的第二份判据迟早与内核漂移，而漂移的表现是
/// 「要么产出起不来的配置、要么把好配置误报成无效」。
pub fn protocol_can_carry_transport(protocol: Protocol) -> bool {
    TRANSPORT_CAPABLE.contains(&protocol)
}

/// 生成代理 Outbound。上游 `buildProxyOutbound`。
/// arch/platform 注入。
///
/// `node_resolver` = 节点域名 dial 解析器，**纯透传**：本函数不构造、不给默认值，由调用方经
/// [`get_node_dial_domain_resolver`](crate::builder::helpers::get_node_dial_domain_resolver)
/// 备好后传入。这是 #335 修复的一部分——参数类型是 [`DomainResolver`] 而非 `&str`，未来新增
/// call site 若图省事直接塞一个 tag 字符串会是**编译错误**，而不是静默回落到「纯 tag 覆盖顶层
/// strategy」的未修形态（那个形态在 loopback 上表现为节点域名 `lookup failed: empty result`）。
pub fn build_proxy_outbound(
    server: &ServerConfig,
    tag: &str,
    node_resolver: &DomainResolver,
    arch: &str,
    platform: &str,
) -> Outbound {
    let protocol = protocol_str(server.protocol);

    // 自定义协议（逃生舱）：**真透传** —— 用户给什么就下发什么，只做两处既有且有理由的改写
    // （覆盖 `tag`、剥内层 `detour`，见 [`custom_passthrough_parts`]）。
    //
    // 此前这里是 `serde_json::from_value::<Outbound>(val)`：注释写「原样下发」，实现却是「只下发
    // 本 struct 建模过的字段」，且类型对不上时整份反序列化失败、回落成 `{"type":"custom"}` 空壳。
    // 完整的三档坏法与实测判决见 [`Outbound::extra`] 的头注。
    if server.protocol == Protocol::Custom {
        if let Some((type_field, extra)) = server
            .custom_settings
            .as_ref()
            .and_then(|cs| custom_passthrough_parts(&cs.outbound))
        {
            let mut ob = Outbound::shell(&type_field, tag);
            ob.extra = extra;
            return ob;
        }
        // 形状非法（非对象 / 无 string `type`）。装配层 `builder/outbounds.rs` 用**同一条判据**
        // （`custom_outbound_type`）把这种节点剔除并记进 `invalid_nodes`，故主生成路径到不了这里；
        // 直调本函数的第二个 call site（`runtime/speedtest.rs` 的临时测速核）会走到。
        //
        // 此时保留 `{"type":"custom","tag":…}` 这颗**毒丸**是刻意的：随包 sing-box 对它的判决是
        // `unknown outbound type: custom`（实测 rc=1），临时核起不来 = 该节点测速失败，如实。
        // 换成「编一个像样的 outbound」反而会把「用户 JSON 写坏了」伪装成「这节点就是慢」。
        return Outbound::shell(&protocol, tag);
    }

    let mut ob = Outbound {
        type_field: protocol.clone(),
        tag: tag.to_string(),
        detour: None,
        server: Some(server.address.clone()),
        server_port: Some(server.port),
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
        domain_resolver: Some(node_resolver.clone()),
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
        extra: serde_json::Map::new(),
    };

    let packet_encoding = server
        .packet_encoding
        .clone()
        .unwrap_or_else(|| "xudp".to_string());

    match server.protocol {
        Protocol::Vless => {
            ob.uuid = server.uuid.clone();
            // 消费点归一：serde 边界已归一，但 net-stack 的 clash_parser 直接字段赋值
            // （`config.flow = Some(raw_yaml)`）绕过 serde → 此处兜底。
            // 未归一的 `"XTLS-RPRX-Vision"` 会让 sing-box `unsupported flow` FATAL。
            ob.flow = server.flow.as_deref().and_then(normalize_token);
            if !packet_encoding.is_empty() {
                ob.packet_encoding = Some(packet_encoding);
            }
        }
        Protocol::Vmess => {
            ob.uuid = server.uuid.clone();
            ob.security = Some(
                server
                    .vmess_security
                    .clone()
                    .unwrap_or_else(|| "auto".into()),
            );
            ob.alter_id = Some(server.alter_id.unwrap_or(0));
            if !packet_encoding.is_empty() {
                ob.packet_encoding = Some(packet_encoding);
            }
        }
        Protocol::Trojan => {
            ob.password = server.password.clone();
        }
        Protocol::Hysteria2 => {
            ob.password = server.password.clone();
            if let Some(h) = &server.hysteria2_settings {
                // `0` 与「不下发」在内核侧**语义等价**，故过滤掉而不是原样写出去。
                //
                // 判据是内核源码不是猜：`sing-quic v0.6.4 hysteria2/client.go:573-590`（= 随包
                // beta.7 `go.mod` 的精确 pin）里是
                // `if !authResponse.RxAuto && actualTx > 0 { NewBrutalSender(actualTx) } else { NewBbrSender... }`
                // —— `> 0` 这一支把 `0` 明确划进 BBR 腿。官方文档同口径：「If empty, the BBR
                // congestion control algorithm will be used instead of Hysteria CC.」
                // loopback A/B 复核（随包 beta.7，200MB×3）：不设 = 3287/3282/3124 Mbps，
                // `up:0 down:0` = 2868/2879/2842 Mbps —— 同为 BBR 量级，`0` 不会 stall。
                //
                // 那为什么还要过滤：写出去会让每份存量配置凭空多一个 `"up_mbps": 0` 键，
                // 与 上游（`if (server.hysteria2Settings?.upMbps)` 的 truthy 判断）产生纯字节分歧，
                // 而本仓的金样对拍是逐字节的。行为等价、字节不等价 = 无谓的漂移。
                //
                // **刻意不做的事**：不过滤非零值、不加「忽略订阅带宽」开关。
                // 用户 2026-08-06 定：**遵循订阅下发**。代价是知情的——非零 `up_mbps`/`down_mbps`
                // 会让内核改用 Brutal 固定速率而非 BBR 自适应（VM185 真机实测：声明 30 → 实测
                // 29.5 Mbps = 1GbE 线速的 3.1%），机场在订阅里填保守值时吞吐会被钉死。
                // 详见 vault `design/networking/` 下的 hy2 自建验证记录。
                ob.up_mbps = h.up_mbps.filter(|v| *v > 0);
                ob.down_mbps = h.down_mbps.filter(|v| *v > 0);
                if let Some(obfs) = &h.obfs {
                    if let (Some(t), Some(pw)) = (&obfs.type_field, &obfs.password) {
                        let mut o = Hysteria2Obfs {
                            type_field: t.clone(),
                            password: pw.clone(),
                            min_packet_size: None,
                            max_packet_size: None,
                        };
                        if t == "gecko" {
                            o.min_packet_size = obfs.min_packet_size;
                            o.max_packet_size = obfs.max_packet_size;
                        }
                        ob.obfs = Some(crate::singbox::outbound::ObfsField::Object(o));
                    }
                }
                ob.bbr_profile = h.bbr_profile.clone();
                // 只有用户显式打开才下发 `true`（核心默认 false=拟态开）。`Some(false)` 与 `None` 一样
                // 不下发 —— 下发 `false` 与省略语义等价，却会让每份存量配置多出一个键（金样字节漂移）。
                if h.disable_chrome_parrot == Some(true) {
                    ob.disable_chrome_parrot = Some(true);
                }
                ob.network = h.network.clone();
            }
        }
        Protocol::Snell => {
            // 🔴 `snellSettings` 缺席时**不能整段跳过** —— 跳过就一个 `version`/`psk` 都不发，
            // 而内核在 **decode 阶段**判 `snell: missing version` ⇒ 整份配置起不来，不止这个节点
            // （随包核 beta.7 实测；由 `tests/kernel_accepts_outbounds.rs` 的协议×传输交叉门发现）。
            //
            // 缺席按全默认处理，且 `version` 归一到 4/6：`SnellVersion = u32` 且 `Default` 派生 ⇒
            // 缺省值是 **0**，而 0 同样不是内核认的版本。归一判据取自 UI 侧既有的那一条
            // （`proto-codec.ts:778` 的 `version === 6 ? '6' : '4'`）—— 两侧同判据，不另立第二份。
            //
            // 生产可达性：UI 的 `toConfig` 与三个 importer 都恒写 `snellSettings`，故这是**防御**
            // 而非已复现的线上缺陷。但落点是「整核起不来」，与 `Protocol::Http` 那次同级，
            // 且修法是纯收窄（4/6 之外的值本就会被内核拒），故不留着。
            {
                let owned = server.snell_settings.clone().unwrap_or_default();
                let s = &owned;
                let version: u32 = if s.version == 6 { 6 } else { 4 };
                ob.version = Some(crate::singbox::OutboundVersion::Num(version));
                ob.psk = server.password.clone();
                ob.userkey = s.userkey.clone();
                if s.reuse == Some(true) {
                    ob.reuse = Some(true);
                }
                ob.network = s.network.clone();
                if version == 4 {
                    if let Some(mode) = &s.obfs_mode {
                        if mode != "none" {
                            ob.obfs_mode = Some(mode.clone());
                            ob.obfs_host =
                                Some(s.obfs_host.clone().unwrap_or_else(|| "bing.com".into()));
                        }
                    }
                } else {
                    if let Some(m) = &s.mode {
                        if m != "default" {
                            ob.mode = Some(m.clone());
                        }
                    }
                }
            }
        }
        Protocol::Anytls => {
            ob.password = server.password.clone();
            if let Some(a) = &server.any_tls_settings {
                ob.idle_session_check_interval =
                    normalize_duration(a.idle_session_check_interval.as_deref());
                ob.idle_session_timeout = normalize_duration(a.idle_session_timeout.as_deref());
                ob.min_idle_session = a.min_idle_session;
            }
        }
        Protocol::Shadowsocks => {
            if let Some(ss) = &server.shadowsocks_settings {
                ob.method = Some(ss.method.clone());
                ob.password = Some(ss.password.clone());
                ob.plugin = ss.plugin.clone();
                ob.plugin_opts = ss.plugin_opts.clone();
            }
        }
        Protocol::Tuic => {
            ob.uuid = server.uuid.clone();
            ob.password = server.password.clone();
            if let Some(t) = &server.tuic_settings {
                ob.congestion_control = t.congestion_control.clone();
                ob.udp_relay_mode = t.udp_relay_mode.clone();
                ob.zero_rtt_handshake = t.zero_rtt_handshake;
                ob.heartbeat = normalize_duration(t.heartbeat.as_deref());
            }
        }
        Protocol::Naive => {
            ob.username = server.username.clone();
            ob.password = server.password.clone();
            // naive TLS 由 Cronet 自管：仅 server_name（alpn/insecure 会 FATAL）。
            ob.tls = Some(OutboundTls {
                enabled: true,
                server_name: Some(
                    server
                        .tls_settings
                        .as_ref()
                        .and_then(|t| t.server_name.clone())
                        .unwrap_or_else(|| server.address.clone()),
                ),
                insecure: None,
                alpn: None,
                engine: None,
                spoof: None,
                spoof_method: None,
                utls: None,
                reality: None,
                ech: None,
                fragment: None,
            });
            if let Some(n) = &server.naive_settings {
                if n.use_http3 == Some(true) {
                    ob.quic = Some(true);
                }
            }
        }
        Protocol::Socks => {
            ob.username = server.username.clone();
            ob.password = server.password.clone();
            // SOCKS 默认版本：上游 `version = '5'`（字符串，outbound-builder.ts:381），非裸数字。
            ob.version = Some(crate::singbox::OutboundVersion::Str("5".to_string()));
        }
        Protocol::Http => {
            ob.username = server.username.clone();
            ob.password = server.password.clone();
            // HTTP 伪装的 headers/path 走**出站顶层**，不是 `transport`。
            //
            // 此前这里 1:1 移植了 上游 `singbox-outbound-builder.ts:391-398` 的「塞进 ob.transport」，
            // 而随包 sing-box 1.14.0-beta.7 的 http 出站 schema **没有 `transport` 键**且
            // `additionalProperties:false` ⇒ 只要用户在 http 节点上填过 headers/path，产出的就是一份
            // `FATAL decode config: outbounds[0].transport: json: unknown field "transport"` 的死配置
            // （整个核起不来，不止这一个节点）。正反对照与 schema 原文见 [`Outbound::path`] 的头注。
            //
            // `h.host` / `h.method` **无处可去**：内核 http 出站没有这两键（写顶层同样 FATAL），故此处
            // 刻意不读——它们只在 h2 **传输**那条腿（`generate_transport_config` 的 "http"|"h2" 分支）
            // 有意义，那里的容器是 `transport`，schema 允许。
            if let Some(h) = &server.http_settings {
                if let Some(headers) = &h.headers {
                    let mut m = BTreeMap::new();
                    for (k, v) in headers {
                        m.insert(k.clone(), OneOrMany::Many(v.clone()));
                    }
                    ob.headers = Some(m);
                }
                if let Some(path) = &h.path {
                    ob.path = Some(path.clone());
                }
            }
        }
        // ── Hysteria v1（2026-08-11）──
        // 与 Hysteria2 是两个协议：v1 的 obfs 是**裸字符串**、认证走 auth_str/auth、
        // 带宽 up_mbps/down_mbps 是必填语义（缺了内核不报错但拥塞控制无从工作）。
        Protocol::Hysteria => {
            if let Some(h) = &server.hysteria_settings {
                // 透传袋：先铺，再把本臂会写的具名键从袋里剔掉。
                // 顺序不够 —— `extra` 是 `#[serde(flatten)]`，序列化时**袋里的键胜出**，
                // 所以「先铺后写」并不能让具名字段赢。必须显式移除冲突键。
                // 判据：具名字段是**表单的真值**，否则用户改过的项会被导入时留下的原值盖回去。
                ob.extra.extend(h.extra.clone());
                for k in [
                    "auth_str",
                    "auth",
                    "up_mbps",
                    "down_mbps",
                    "obfs",
                    "server_ports",
                    "hop_interval",
                ] {
                    ob.extra.remove(k);
                }
                ob.auth_str = h.auth_str.clone();
                ob.up_mbps = h.up_mbps;
                ob.down_mbps = h.down_mbps;
                if let Some(o) = &h.obfs {
                    ob.obfs = Some(crate::singbox::outbound::ObfsField::Text(o.clone()));
                }
                // 端口跳跃：内核这两个键与 hy2 同名同义，直接复用 Outbound 上已有的字段。
                if let Some(ports) = &h.server_ports {
                    if !ports.trim().is_empty() {
                        ob.server_ports = Some(vec![ports.clone()]);
                    }
                }
                ob.hop_interval = h.hop_interval.clone();
            }
        }
        // ── 内嵌 Tor（2026-08-11）──
        // **没有 server/server_port**：实测传 server 得 `unknown field "server"`。
        // 上面的通用构造已经无条件填了这两个键，故此处必须显式清掉，否则整份配置 decode 失败
        // ——这不是「多发一个没用的键」，是**整个内核起不来**。
        Protocol::Tor => {
            ob.server = None;
            ob.server_port = None;
            if let Some(t) = &server.tor_settings {
                ob.extra.extend(t.extra.clone());
                for k in ["executable_path", "data_directory", "extra_args", "torrc"] {
                    ob.extra.remove(k);
                }
                ob.executable_path = t.executable_path.clone();
                ob.data_directory = t.data_directory.clone();
                if !t.extra_args.is_empty() {
                    ob.extra_args = Some(t.extra_args.clone());
                }
                if !t.torrc.is_empty() {
                    ob.torrc = Some(t.torrc.clone());
                }
            }
            // Tor 自带传输层，不叠 TLS/transport。
            return ob;
        }
        Protocol::Ssh => {
            if let Some(s) = &server.ssh_settings {
                ob.user = s.user.clone();
                ob.password = s.password.clone();
                ob.private_key = s.private_key.clone();
                ob.private_key_path = s.private_key_path.clone();
                ob.private_key_passphrase = s.private_key_passphrase.clone();
                ob.host_key = s.host_key.clone();
                ob.host_key_algorithms = s.host_key_algorithms.clone();
                ob.client_version = s.client_version.clone();
                ob.cipher = s.cipher.clone();
                ob.mac = s.mac.clone();
                ob.kex_algorithm = s.kex_algorithm.clone();
            }
            // SSH 不需 TLS/transport，直接返回。
            return ob;
        }
        _ => {}
    }

    // TLS（非 naive）。
    // security 是 SecurityMode 枚举 → 大小写变体在反序列化边界已归一，此处不可能漏判。
    if server.protocol != Protocol::Naive
        && (server.security.as_ref().is_some_and(SecurityMode::is_tls)
            || server.tls_settings.is_some()
            || TLS_PROTOCOLS.contains(&protocol.as_str()))
    {
        let mut final_alpn = server.tls_settings.as_ref().and_then(|t| t.alpn.clone());
        if final_alpn.is_none() && server.protocol == Protocol::Trojan {
            final_alpn = Some(vec!["http/1.1".into()]);
        }

        ob.tls = Some(OutboundTls {
            enabled: true,
            server_name: Some(
                server
                    .tls_settings
                    .as_ref()
                    .and_then(|t| t.server_name.clone())
                    .unwrap_or_else(|| server.address.clone()),
            ),
            insecure: Some(
                server
                    .tls_settings
                    .as_ref()
                    .and_then(|t| t.allow_insecure)
                    .unwrap_or(false),
            ),
            alpn: final_alpn,
            engine: None,
            spoof: None,
            spoof_method: None,
            utls: None,
            reality: None,
            ech: None,
            fragment: None,
        });

        let tls_engine = server
            .tls_settings
            .as_ref()
            .and_then(|t| t.engine.as_deref());
        if !is_quic_managed_tls(&protocol) && should_emit_tls_engine(tls_engine, platform) {
            ob.tls.as_mut().unwrap().engine = tls_engine.map(String::from);
        }

        // uTLS fingerprint（非 QUIC）。消费点归一（理由同 flow：绕过 serde 的字段赋值兜底）。
        // 未归一的 `"Chrome"` / `"NONE"` 会让 sing-box `unknown uTLS fingerprint` FATAL；
        // 尤其 `"None"` 本意是禁用 utls，不归一则反而下发非法指纹 → 核起不来。
        let fingerprint = server
            .tls_settings
            .as_ref()
            .and_then(|t| t.fingerprint.as_deref())
            .and_then(normalize_token);
        let final_fp = fingerprint.unwrap_or_else(|| {
            if server.protocol == Protocol::Vless || server.protocol == Protocol::Anytls {
                "chrome".to_string()
            } else {
                "none".to_string()
            }
        });
        if !is_quic_managed_tls(&protocol) && final_fp != "none" {
            ob.tls.as_mut().unwrap().utls = Some(Utls {
                enabled: true,
                fingerprint: final_fp,
            });
        }
    }

    // Reality。
    if server
        .security
        .as_ref()
        .is_some_and(SecurityMode::is_reality)
    {
        if let Some(r) = &server.reality_settings {
            // 🔴 `engine` 在本段**必须写死 `None`**，别改成「把上面 TLS 段装好的那个搬过来」。
            //
            // 曾按「schema 里 engine 与 reality 是平级属性、无互斥约束」判定这是本仓 builder 的缺口
            // 并动手搬运，**那是错的**：schema 只表达键的形状，reality 与平台 engine 的互斥发生在
            // `initialize outbound` 阶段，schema 与 `sing-box check` 在 Linux 上都看不到。
            //
            // 判据（随包核 beta.7 四个平台二进制的字符串在场矩阵，`strings -n 6 | grep -c`）：
            // ```
            // "reality is unsupported in "   linux=0  win=1  mac-x64=1  mac-arm64=1
            // "utls is unsupported in "      linux=0  win=1  mac-x64=1  mac-arm64=1
            // "ech is unsupported in "       linux=0  win=1  mac-x64=1  mac-arm64=1
            // ```
            // 这三条只编进「有真实平台 engine 客户端」的那几个构建；Linux 里 Windows/Apple 引擎是
            // **提前返回的桩**（报 `... TLS engine is not available on non-Windows platforms`），
            // 于是 Linux 上任何 `reality × engine` 对照都测不到真判决 —— 那种实验的检出力是 **0**，
            // 「四组报错逐字相同 ⇒ 与 reality 无关」是桩的必然输出，不是证据。
            //
            // 而 `should_emit_tls_engine` 只在 `(windows,win32)`/`(apple,darwin)` 放行 ⇒ 一旦搬运，
            // 落到真机上的恰好就是「平台 engine 客户端 + reality」这一组，判决是
            // `FATAL initialize outbound[N]: reality is unsupported in <engine>`，
            // **整份配置起不来**（不止这个节点）。且本替换体无条件发 `utls{enabled:true}`，
            // 即使 reality 那条不先触发，`utls is unsupported in ` 也会触发 —— 双重致命。
            //
            // ⇒ 前端 `whenTlsEngine` 上那条 `!whenReality` 不是止血门，是**正确的**：reality 下
            // 这一档在任何平台都不可用，显示它就是一个拨了必然炸核的控件。
            ob.tls = Some(OutboundTls {
                enabled: true,
                server_name: server
                    .tls_settings
                    .as_ref()
                    .and_then(|t| t.server_name.clone()),
                insecure: Some(
                    server
                        .tls_settings
                        .as_ref()
                        .and_then(|t| t.allow_insecure)
                        .unwrap_or(false),
                ),
                alpn: None,
                engine: None,
                spoof: None,
                spoof_method: None,
                utls: Some(Utls {
                    enabled: true,
                    fingerprint: server
                        .tls_settings
                        .as_ref()
                        .and_then(|t| t.fingerprint.as_deref())
                        .and_then(normalize_token)
                        .unwrap_or_else(|| "chrome".into()),
                }),
                reality: Some(Reality {
                    enabled: true,
                    public_key: r.public_key.clone(),
                    short_id: r.short_id.clone().unwrap_or_default(),
                }),
                ech: None,
                fragment: None,
            });
        }
    }

    // 传输层 —— **白名单**，判据取自内核 schema 而非「排掉几个已知不行的」。
    //
    // 随包核 beta.7 `sing-box schema` → `$defs/Outbound` 的 20 支 oneOf 里，**只有 trojan / vless /
    // vmess 三支有 `transport` 属性**，其余 17 支（http/socks/shadowsocks/tuic/shadowtls/anytls/…）
    // 一律 `additionalProperties:false` 且无该键 ⇒ 给它们挂 transport 的产物是
    // `FATAL decode config: outbounds[N].transport: json: unknown field "transport"`，
    // **整份配置起不来**，不止这个节点。
    //
    // 此处此前是黑名单（`!matches!(Hysteria2|Anytls|Naive)`），与内核判据方向相反：内核说「只有这三个
    // 可以」，本仓说「只有这三个不可以」。中间那 14 个协议只要拿到 `network != "tcp"` 就产出死配置。
    // 而它们**拿得到**：UI 侧只有 vless/vmess/trojan 暴露传输选择器（`ND_SPEC` 里只有这三支带
    // `F_TRANSPORT`，与内核白名单精确一致），但**导入侧不受这个限制** —— xray 的 `streamSettings`
    // 挂在任意出站上、clash 的 `network:` 同理，`net-stack` 那几个 parser 会照单写进 `server.network`。
    //
    // 改成白名单后，非白名单协议带进来的传输参数被**丢弃**（而不是让整份配置炸）。这是有意的取舍：
    // 二者都丢信息，但前者只影响该节点、后者影响全部节点。丢弃这件事今天没有上报通道
    // （builder 无 diagnostics 出口），登记为债务：真正该报的位置在导入侧，那里有 `unsupported` 计数。
    if protocol_can_carry_transport(server.protocol) {
        if let Some(net) = &server.network {
            if net != "tcp" {
                ob.transport = generate_transport_config(server);
            }
        }
    }

    // 抗封后处理。
    apply_anti_censorship_options(&mut ob, server, arch);

    ob
}

/// custom 逃生舱 raw JSON → `(type, 其余键)`，供 outbound / endpoint 两条腿共用。
///
/// 形状不合法（非对象 / 无 string `type`）→ `None`，判据与 C10 probe 共用
/// [`custom_outbound_type`]（那条注释解释了为什么必须是同一个谓词）。
///
/// 只做三处键改写，**每处都有既有理由，不是新策略**：
///  - `type` 取出来进 `type_field` 具名字段 —— 留在 map 里会与具名字段撞成重复键；
///  - `tag` 丢弃 —— 节点 tag 是 Polaris 的拓扑真值（selector 成员、detour 目标、路由规则全指它），
///    由调用方覆盖，用户在 JSON 里自填的那个不作数；
///  - `detour` 丢弃 —— 内层 detour 会绕过 Polaris 自己的 detour 死引用/成环检测
///    （`builder/outbounds.rs::prune_detour_dead_references`），是本仓一直在剥的东西。
///
/// 其余键**一律原样保留**：这正是「逃生舱」三个字的全部内容。
pub(crate) fn custom_passthrough_parts(
    raw: &serde_json::Value,
) -> Option<(String, serde_json::Map<String, serde_json::Value>)> {
    let type_field = custom_outbound_type(raw)?.to_string();
    let mut extra = raw.as_object()?.clone();
    extra.remove("type");
    extra.remove("tag");
    extra.remove("detour");
    Some((type_field, extra))
}

/// 生成传输层配置。上游 `generateTransportConfig`。
fn generate_transport_config(server: &ServerConfig) -> Option<Transport> {
    let net = server.network.as_deref()?;
    match net {
        "ws" => {
            let ws = server.ws_settings.as_ref();
            let raw_path = ws.and_then(|w| w.path.as_deref()).unwrap_or("/");
            let ed = parse_ws_early_data(raw_path);
            Some(Transport {
                type_field: "ws".into(),
                path: Some(ed.path),
                host: None,
                method: None,
                headers: ws.and_then(|w| w.headers.as_ref()).map(|h| {
                    let mut m = BTreeMap::new();
                    for (k, v) in h {
                        m.insert(k.clone(), OneOrMany::One(v.clone()));
                    }
                    m
                }),
                service_name: None,
                max_early_data: ed
                    .max_early_data
                    .or_else(|| ws.and_then(|w| w.max_early_data)),
                early_data_header_name: ed
                    .early_data_header_name
                    .or_else(|| ws.and_then(|w| w.early_data_header_name.clone())),
            })
        }
        "grpc" => {
            let g = server.grpc_settings.as_ref();
            Some(Transport {
                type_field: "grpc".into(),
                service_name: Some(g.and_then(|g| g.service_name.clone()).unwrap_or_default()),
                path: None,
                host: None,
                method: None,
                headers: None,
                max_early_data: None,
                early_data_header_name: None,
            })
        }
        "http" | "h2" => {
            let h = server.http_settings.as_ref();
            Some(Transport {
                type_field: "http".into(),
                host: h.and_then(|h| h.host.clone()).map(|hosts| {
                    if hosts.len() == 1 {
                        OneOrMany::One(hosts[0].clone())
                    } else {
                        OneOrMany::Many(hosts)
                    }
                }),
                path: Some(h.and_then(|h| h.path.clone()).unwrap_or_else(|| "/".into())),
                method: h.and_then(|h| h.method.clone()),
                headers: h.and_then(|h| h.headers.as_ref()).map(|hdrs| {
                    let mut m = BTreeMap::new();
                    for (k, v) in hdrs {
                        m.insert(k.clone(), OneOrMany::Many(v.clone()));
                    }
                    m
                }),
                service_name: None,
                max_early_data: None,
                early_data_header_name: None,
            })
        }
        "httpupgrade" => Some(Transport {
            type_field: "httpupgrade".into(),
            path: Some(
                server
                    .ws_settings
                    .as_ref()
                    .and_then(|w| w.path.clone())
                    .unwrap_or_else(|| "/".into()),
            ),
            host: server
                .ws_settings
                .as_ref()
                .and_then(|w| w.headers.as_ref().and_then(|h| h.get("Host").cloned()))
                .or_else(|| {
                    server
                        .tls_settings
                        .as_ref()
                        .and_then(|t| t.server_name.clone())
                })
                .map(OneOrMany::One),
            method: None,
            headers: None,
            service_name: None,
            max_early_data: None,
            early_data_header_name: None,
        }),
        _ => None,
    }
}

/// 抗封后处理（ECH/fragment/spoof/multiplex/hy2 端口跳跃）。上游 `applyAntiCensorshipOptions`。
fn apply_anti_censorship_options(ob: &mut Outbound, server: &ServerConfig, arch: &str) {
    let protocol_lower = protocol_str(server.protocol);
    let fragment_unsupported =
        is_quic_managed_tls(&protocol_lower) || server.protocol == Protocol::Naive;

    // ECH + fragment + spoof（需 tls 块）。
    if let Some(tls) = ob.tls.as_mut() {
        if let Some(tls_s) = &server.tls_settings {
            if tls_s.ech == Some(true) {
                let ech_cfg = tls_s.ech_config.as_deref().map(|s| s.trim()).unwrap_or("");
                let lines: Vec<String> = if ech_cfg.is_empty() {
                    vec![]
                } else {
                    ech_cfg
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                };
                tls.ech = Some(if lines.is_empty() {
                    Ech {
                        enabled: true,
                        config: None,
                    }
                } else {
                    Ech {
                        enabled: true,
                        config: Some(lines),
                    }
                });
            }
            if tls_s.fragment == Some(true) && !fragment_unsupported {
                tls.fragment = Some(true);
            }
            // TLS spoof。
            let spoof_sni = tls_s.spoof_sni.as_deref().map(|s| s.trim()).unwrap_or("");
            let real_sni = tls.server_name.as_deref();
            if validate_tls_spoof_default(
                Some(spoof_sni),
                tls_s.spoof_method.as_deref(),
                Some(arch),
                Some(protocol_lower.as_str()),
                real_sni,
            ) {
                tls.spoof = Some(spoof_sni.to_string());
                tls.spoof_method = tls_s.spoof_method.clone();
            }
        }
    }

    // Multiplex（vless/trojan/vmess/ss；vision flow 跳过）。
    if let Some(mux) = &server.multiplex_settings {
        if mux.enabled == Some(true)
            && matches!(
                server.protocol,
                Protocol::Vless | Protocol::Trojan | Protocol::Vmess | Protocol::Shadowsocks
            )
        {
            let has_vision = server
                .flow
                .as_deref()
                .map(|f| f.to_ascii_lowercase().contains("vision"))
                .unwrap_or(false);
            if !has_vision {
                ob.multiplex = Some(Multiplex {
                    enabled: true,
                    protocol: Some(mux.protocol.clone().unwrap_or_else(|| "h2mux".into())),
                    max_connections: mux.max_connections,
                    min_streams: mux.min_streams,
                    padding: mux.padding,
                });
            }
        }
    }

    // Hysteria2 端口跳跃。
    if server.protocol == Protocol::Hysteria2 {
        if let Some(h) = &server.hysteria2_settings {
            if let Some(ports_str) = &h.server_ports {
                let ports: Vec<String> = ports_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !ports.is_empty() {
                    ob.server_ports = Some(ports);
                    ob.hop_interval = h.hop_interval.clone();
                }
            }
        }
    }
}

/// 协议的内核 type 字符串。**导出给 outbounds.rs 的端点族腿复用** —— 那里要拿它当
/// `Endpoint::type_field`，复制一份必然与本表漂移。
pub(crate) fn protocol_str(p: Protocol) -> String {
    match p {
        Protocol::Vless => "vless",
        Protocol::Trojan => "trojan",
        Protocol::Hysteria2 => "hysteria2",
        Protocol::Shadowsocks => "shadowsocks",
        Protocol::Anytls => "anytls",
        Protocol::Tuic => "tuic",
        Protocol::Vmess => "vmess",
        Protocol::Naive => "naive",
        Protocol::Snell => "snell",
        Protocol::Socks => "socks",
        Protocol::Http => "http",
        Protocol::Ssh => "ssh",
        Protocol::Wireguard => "wireguard",
        Protocol::Tailscale => "tailscale",
        Protocol::Hysteria => "hysteria",
        Protocol::Tor => "tor",
        Protocol::Openconnect => "openconnect",
        Protocol::OpenvpnClient => "openvpn-client",
        Protocol::Custom => "custom",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::protocol_settings as ps;

    fn server(protocol: Protocol, addr: &str) -> ServerConfig {
        ServerConfig {
            id: "s1".into(),
            name: "test".into(),
            protocol,
            address: addr.into(),
            port: 443,
            ..Default::default()
        }
    }

    // ── custom 逃生舱：真透传（P0 回归锁）────────────────────────────────────────────
    //
    // 修复前这里是 `serde_json::from_value::<Outbound>(val)` —— 一个约 70 个具名字段的强类型
    // struct，无 flatten 兜底。注释写「原样下发」，实现是「只下发建模过的字段」。下面四组是
    // **真跑测出来的**四档形态（随包 sing-box 1.14.0-beta.7 逐条 `check` 过），不是推演。

    /// 造一个 custom 节点（raw JSON 逐字进 `customSettings.outbound`）。
    fn custom_server(raw: serde_json::Value) -> ServerConfig {
        let mut s = server(Protocol::Custom, "unused.example");
        s.custom_settings = Some(ps::CustomSettings {
            outbound: raw,
            is_endpoint: None,
            secret_keys: None,
        });
        s
    }

    fn custom_outbound_json(raw: serde_json::Value) -> serde_json::Value {
        let ob = build_proxy_outbound(
            &custom_server(raw),
            "proxy-c1",
            &test_dial_resolver(),
            "x64",
            "linux",
        );
        serde_json::to_value(&ob).unwrap()
    }

    /// 🔴 **变异锁：custom = 逐键真透传**。四组场景 = 修复前四种不同的坏法。
    ///
    /// 断言方式是「输出 == 输入 + tag 覆盖」的**整对象相等**，不是逐键点名：后者对「多吃掉一个
    /// 没被点名的键」不转红，而静默丢字段正是本缺陷的形态。把实现改回 `from_value::<Outbound>`
    /// ⇒ 第 2 组（整份解析失败 → `{"type":"custom"}`）、第 3/4 组（静默丢字段）立刻转红。
    #[test]
    fn custom_outbound_is_verbatim_passthrough() {
        for (name, raw) in [
            // ① 字段恰好都在 `Outbound` 里 —— 修复前也过，是本组的**阴性对照**：
            //    没有它，「四组全绿」可能只是因为透传把什么都不做当成了成功。
            (
                "shadowtls（全字段已建模）",
                serde_json::json!({"type":"shadowtls","server":"s.example.com","server_port":443,
                    "version":3,"password":"pw","tls":{"enabled":true,"server_name":"sni.example"}}),
            ),
            // ② 建模过但**类型不同**：hysteria v1 的 `obfs` 按真实 schema 是**字符串**，本 struct 是
            //    `Option<Hysteria2Obfs>` 对象 ⇒ 修复前**整个反序列化失败**，回落成
            //    `{"type":"custom","tag":…}`，而 `sing-box check` 对它判 `unknown outbound type:
            //    custom`（rc=1）——一个坏节点炸掉整份配置。
            (
                "hysteria v1（obfs 是字符串）",
                serde_json::json!({"type":"hysteria","server":"h1.example.com","server_port":443,
                    "up_mbps":100,"down_mbps":500,"obfs":"salamander-secret","auth_str":"mypass",
                    "tls":{"enabled":true,"server_name":"h1.example.com"}}),
            ),
            // ③ 没建模：hysteria v1 的 `auth_str` ⇒ 修复前解析成功但该键**静默丢失**（= 无凭证，
            //    连不上，可配置「看起来是好的」）。
            (
                "hysteria v1（auth_str）",
                serde_json::json!({"type":"hysteria","server":"h1.example.com","server_port":443,
                    "auth_str":"mypass"}),
            ),
            // ④ 没建模：tor 的四个键 ⇒ 修复前全丢，只剩 `{"type":"tor","tag":…}`。
            (
                "tor（executable_path 等四键）",
                serde_json::json!({"type":"tor","executable_path":"/usr/bin/tor",
                    "data_directory":"/tmp/tordata","extra_args":["--HTTPTunnelPort","0"],
                    "torrc":{"UseBridges":"1"}}),
            ),
        ] {
            let mut expected = raw.clone();
            expected["tag"] = serde_json::json!("proxy-c1");
            assert_eq!(
                custom_outbound_json(raw),
                expected,
                "{name}：custom 必须逐键原样下发（多一键少一键都是把逃生舱改回白名单）"
            );
        }
    }

    /// 唯二的两处改写：`tag` 强制覆盖、内层 `detour` 剥离 —— 两条都是既有的、有理由的
    /// （tag 是 Polaris 的拓扑真值；内层 detour 会绕过本仓的 detour 死引用/成环检测）。
    #[test]
    fn custom_outbound_overrides_tag_and_strips_inner_detour() {
        let v = custom_outbound_json(serde_json::json!({
            "type":"socks","tag":"用户自己写的tag","detour":"某个内层出站","server":"s.example.com"
        }));
        assert_eq!(v["tag"], serde_json::json!("proxy-c1"));
        assert!(v.get("detour").is_none(), "内层 detour 必须剥掉：{v}");
        assert_eq!(v["server"], serde_json::json!("s.example.com"));
    }

    /// 形状非法（非对象 / 无 string `type`）⇒ 保留 `{"type":"custom"}` **毒丸**。
    ///
    /// 这不是「兜底成一个能用的 outbound」：随包 sing-box 对 `custom` 判 `unknown outbound type`
    /// 立刻拒。主生成路径根本到不了这里（`builder/outbounds.rs` 用同一条判据先把节点剔除并上报），
    /// 到得了的是 `runtime/speedtest.rs` 的临时测速核 —— 那里「测速失败」正是如实的结论。
    #[test]
    fn custom_malformed_shape_stays_a_poison_pill() {
        for raw in [
            serde_json::json!([1, 2, 3]),
            serde_json::json!("hysteria"),
            serde_json::json!({"server": "no-type.example"}),
            serde_json::json!({"type": 4}),
        ] {
            let v = custom_outbound_json(raw.clone());
            assert_eq!(
                v,
                serde_json::json!({"type":"custom","tag":"proxy-c1"}),
                "形状非法的 custom 不得被编成一个像样的 outbound：{raw}"
            );
        }
    }

    /// 本模块各测试断言的是**协议字段映射**，与 dial 解析器形态无关；取生产默认分支
    /// （enableIPv6 关 → 结构化）即可，形态本身的门在 `builder/outbounds.rs` 的 #335 三连测里。
    fn test_dial_resolver() -> DomainResolver {
        crate::builder::helpers::get_node_dial_domain_resolver("dns-bootstrap", false)
    }

    #[test]
    fn vless_basic() {
        let mut s = server(Protocol::Vless, "a.com");
        s.uuid = Some("uuid-1".into());
        s.security = Some(SecurityMode::Tls); // vless 需显式 security=tls 才生成 TLS 块
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        assert_eq!(ob.type_field, "vless");
        assert_eq!(ob.uuid.as_deref(), Some("uuid-1"));
        assert_eq!(ob.packet_encoding.as_deref(), Some("xudp")); // 默认 xudp
        assert_eq!(ob.server.as_deref(), Some("a.com"));
        // vless security=tls 默认 chrome utls。
        assert!(ob.tls.is_some());
        assert_eq!(
            ob.tls.as_ref().unwrap().utls.as_ref().unwrap().fingerprint,
            "chrome"
        );
    }

    #[test]
    fn trojan_default_alpn() {
        let mut s = server(Protocol::Trojan, "t.com");
        s.password = Some("pw".into());
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        assert_eq!(
            ob.tls.as_ref().unwrap().alpn.as_ref().unwrap(),
            &vec!["http/1.1".to_string()]
        );
    }

    #[test]
    fn shadowsocks_method_password() {
        let mut s = server(Protocol::Shadowsocks, "ss.com");
        s.shadowsocks_settings = Some(ps::ShadowsocksSettings {
            method: "aes-256-gcm".into(),
            password: "secret".into(),
            ..Default::default()
        });
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        assert_eq!(ob.method.as_deref(), Some("aes-256-gcm"));
        assert_eq!(ob.password.as_deref(), Some("secret"));
    }

    #[test]
    fn hysteria2_obfs_gecko() {
        let mut s = server(Protocol::Hysteria2, "h.com");
        s.password = Some("pw".into());
        s.hysteria2_settings = Some(ps::Hysteria2Settings {
            obfs: Some(ps::Hysteria2ObfsSettings {
                type_field: Some("gecko".into()),
                password: Some("obfspw".into()),
                min_packet_size: Some(100),
                max_packet_size: Some(200),
            }),
            ..Default::default()
        });
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        let obfs = ob.obfs.as_ref().unwrap();
        let crate::singbox::outbound::ObfsField::Object(obfs) = obfs else {
            panic!("hysteria2 的 obfs 必须是对象形态（v1 才是裸字符串）");
        };
        assert_eq!(obfs.type_field, "gecko");
        assert_eq!(obfs.min_packet_size, Some(100));
    }

    /// 默认（用户没碰这个开关）⇒ 生成的 hysteria2 outbound **不含** `disable_chrome_parrot` 键。
    /// 核心默认值就是 `false`，下发它等于给每份存量配置凭空加一个键（金样字节漂移），语义却没变。
    #[test]
    fn hysteria2_no_chrome_parrot_key_by_default() {
        let mut s = server(Protocol::Hysteria2, "h.com");
        s.password = Some("pw".into());
        s.hysteria2_settings = Some(ps::Hysteria2Settings::default());
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        assert_eq!(ob.disable_chrome_parrot, None);
        let json = serde_json::to_value(&ob).unwrap();
        assert!(json.get("disable_chrome_parrot").is_none());
        // 显式关（Some(false)）与没填一样不下发——`false` 与省略在核心侧等价。
        s.hysteria2_settings = Some(ps::Hysteria2Settings {
            disable_chrome_parrot: Some(false),
            ..Default::default()
        });
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        assert!(serde_json::to_value(&ob)
            .unwrap()
            .get("disable_chrome_parrot")
            .is_none());
    }

    /// 🟡 **变异锁：`up_mbps`/`down_mbps` 的 `0` 不下发，非零值原样下发。**
    ///
    /// 两条断言方向相反，缺一不可：
    /// - 只断言「0 不下发」→ 把整个赋值删掉也绿（那会静默丢掉用户真填的带宽）；
    /// - 只断言「非零下发」→ 退回 `= h.up_mbps` 也绿（那正是本次要改掉的形态）。
    ///
    /// 断言落在**序列化后的 JSON 键集**而非 `Option` 字段：`skip_serializing_if` 若被删，
    /// 结构体断言照绿而 JSON 里会多出 `"up_mbps": null`。
    #[test]
    fn hysteria2_zero_bandwidth_is_omitted_but_nonzero_is_kept() {
        let mut s = server(Protocol::Hysteria2, "h.com");
        s.password = Some("pw".into());

        // 0 ≡ 不设（内核 `actualTx > 0` 才走 Brutal，否则 BBR）⇒ 整键不出现。
        s.hysteria2_settings = Some(ps::Hysteria2Settings {
            up_mbps: Some(0),
            down_mbps: Some(0),
            ..Default::default()
        });
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        let json = serde_json::to_value(&ob).unwrap();
        assert!(json.get("up_mbps").is_none(), "0 不该下发：{json}");
        assert!(json.get("down_mbps").is_none(), "0 不该下发：{json}");

        // 非零是用户/订阅的真实意图（遵循订阅下发，2026-08-06 定），必须原样带上。
        s.hysteria2_settings = Some(ps::Hysteria2Settings {
            up_mbps: Some(100),
            down_mbps: Some(500),
            ..Default::default()
        });
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        let json = serde_json::to_value(&ob).unwrap();
        assert_eq!(json["up_mbps"], 100);
        assert_eq!(json["down_mbps"], 500);
    }

    /// 显式开启 ⇒ 下发 `"disable_chrome_parrot": true`（服务端 Ed25519 证书握手失败时的逃生舱）。
    #[test]
    fn hysteria2_chrome_parrot_disabled_when_opted_in() {
        let mut s = server(Protocol::Hysteria2, "h.com");
        s.password = Some("pw".into());
        s.hysteria2_settings = Some(ps::Hysteria2Settings {
            disable_chrome_parrot: Some(true),
            ..Default::default()
        });
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        assert_eq!(ob.disable_chrome_parrot, Some(true));
        assert_eq!(
            serde_json::to_value(&ob).unwrap()["disable_chrome_parrot"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn naive_only_server_name_tls() {
        let mut s = server(Protocol::Naive, "n.com");
        s.username = Some("u".into());
        s.password = Some("p".into());
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        // naive TLS 仅 server_name，无 alpn/insecure。
        let tls = ob.tls.as_ref().unwrap();
        assert!(tls.alpn.is_none());
        assert!(tls.insecure.is_none());
    }

    #[test]
    fn ssh_no_tls() {
        let mut s = server(Protocol::Ssh, "ssh.com");
        s.ssh_settings = Some(ps::SshSettings {
            user: Some("root".into()),
            ..Default::default()
        });
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        assert!(ob.tls.is_none());
        assert_eq!(ob.user.as_deref(), Some("root"));
    }

    // ── 静默 TLS/Reality 降级回归（安全）────────────────────────────────────
    //
    // 锁死的事故形态：`security` 大小写变体 → 分支不命中 → TLS 不启用且无报错
    // → 用户以为加密，实际明文出站。断言落在**生成的 sing-box JSON** 上，
    // 而非归一函数本身：光测归一函数不能证明 config 真的启用了 TLS。

    /// 从 JSON 反序列化建节点 → 走完整生成链 → 返回 sing-box outbound JSON。
    /// 必须经 serde 入口，才覆盖「存量/订阅脏数据进来」的真实路径。
    fn outbound_json_from(server_json: &str) -> serde_json::Value {
        let s: ServerConfig = serde_json::from_str(server_json).expect("节点必须能反序列化");
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        serde_json::to_value(&ob).unwrap()
    }

    #[test]
    fn uppercase_tls_still_enables_tls_in_generated_json() {
        // 端到端核心断言：大写 "TLS" → 生成的 JSON 里 TLS 必须真启用。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"TLS"}"#,
        );
        assert_eq!(
            v["tls"]["enabled"],
            serde_json::json!(true),
            "大写 TLS 必须启用 TLS —— 否则即为明文出站事故"
        );
        assert_eq!(v["tls"]["server_name"], serde_json::json!("a.com"));
    }

    #[test]
    fn tls_case_variants_produce_identical_outbound_json() {
        // 全大小写变体 → 生成结果逐字节一致（含 utls 指纹等一切下游影响）。
        let baseline = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"tls"}"#,
        );
        for raw in ["TLS", "Tls", "tLs", " tls "] {
            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                    "uuid":"u-1","security":"{raw}"}}"#
            ));
            assert_eq!(v, baseline, "security={raw:?} 必须与小写 tls 生成完全一致");
        }
    }

    #[test]
    fn uppercase_reality_still_enables_reality_in_generated_json() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"REALITY",
                "realitySettings":{"publicKey":"pk-abc","shortId":"01ab"}}"#,
        );
        assert_eq!(v["tls"]["enabled"], serde_json::json!(true));
        assert_eq!(
            v["tls"]["reality"]["enabled"],
            serde_json::json!(true),
            "大写 REALITY 必须启用 Reality —— 否则 Reality 静默失效"
        );
        assert_eq!(
            v["tls"]["reality"]["public_key"],
            serde_json::json!("pk-abc")
        );
        assert_eq!(v["tls"]["reality"]["short_id"], serde_json::json!("01ab"));
    }

    #[test]
    fn reality_case_variants_produce_identical_outbound_json() {
        let baseline = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"reality",
                "realitySettings":{"publicKey":"pk","shortId":"01"}}"#,
        );
        for raw in ["REALITY", "Reality", "ReAlItY", "  reality "] {
            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                    "uuid":"u-1","security":"{raw}",
                    "realitySettings":{{"publicKey":"pk","shortId":"01"}}}}"#
            ));
            assert_eq!(v, baseline, "security={raw:?} 必须与小写 reality 生成一致");
        }
    }

    #[test]
    fn unknown_security_does_not_fabricate_tls() {
        // 未知 security（且无 tlsSettings）→ 不凭空造 TLS 块（语义即"未请求 TLS"）。
        // vless 不在 TLS_PROTOCOLS 里，故此处 tls 必须缺席。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"bogus"}"#,
        );
        assert!(v.get("tls").is_none(), "未知 security 不得生成 TLS 块");
    }

    #[test]
    fn security_none_does_not_enable_tls_for_vless() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"NONE"}"#,
        );
        assert!(v.get("tls").is_none(), "security=none 不得启用 TLS");
    }

    // ── hy2/tuic 的 tls.server_name / tls.insecure 端到端（UI 补 sni/insecure 控件的后端侧门）──────
    //
    // 这两个协议在 `TLS_PROTOCOLS` 里 ⇒ 恒有 TLS 块，`allow_insecure` 走的是和 trojan/anytls
    // 同一段装配（本文件 `insecure: Some(...allow_insecure.unwrap_or(false))`）。
    // 断言落在**序列化后的 JSON** 而不是结构体字段：`OutboundTls::insecure` 带
    // `skip_serializing_if = "Option::is_none"`，只断言 `ob.tls.insecure == Some(true)` 的话，
    // 哪天这个键被漏出配置也照样绿。

    #[test]
    fn hysteria2_allow_insecure_true_emits_tls_insecure_true() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"hysteria2","address":"h.com","port":443,
                "password":"pw","tlsSettings":{"serverName":"hy2.sni","allowInsecure":true}}"#,
        );
        assert_eq!(v["tls"]["enabled"], serde_json::json!(true));
        assert_eq!(v["tls"]["insecure"], serde_json::json!(true));
        assert_eq!(v["tls"]["server_name"], serde_json::json!("hy2.sni"));
    }

    #[test]
    fn hysteria2_without_tls_settings_emits_insecure_false_and_address_sni() {
        // 未填（UI 开关默认关、SNI 留空）：`insecure` 仍**显式**下发 `false`，`server_name` 回落节点地址。
        // 这不是「不下发」——金样 `fixtures/config-snapshot.json` 的 hy2/tuic 条目逐字节就是这个形状
        // （与 上游 对齐），改成省略键会让金样对拍转红。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"hysteria2","address":"h.com","port":443,
                "password":"pw"}"#,
        );
        assert_eq!(v["tls"]["insecure"], serde_json::json!(false));
        assert_eq!(v["tls"]["server_name"], serde_json::json!("h.com"));
    }

    #[test]
    fn tuic_allow_insecure_true_emits_tls_insecure_true() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"tuic","address":"t.com","port":443,
                "uuid":"u-1","password":"pw",
                "tlsSettings":{"serverName":"tuic.sni","allowInsecure":true}}"#,
        );
        assert_eq!(v["tls"]["enabled"], serde_json::json!(true));
        assert_eq!(v["tls"]["insecure"], serde_json::json!(true));
        assert_eq!(v["tls"]["server_name"], serde_json::json!("tuic.sni"));
    }

    #[test]
    fn tuic_without_tls_settings_emits_insecure_false_and_address_sni() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"tuic","address":"t.com","port":443,
                "uuid":"u-1","password":"pw"}"#,
        );
        assert_eq!(v["tls"]["insecure"], serde_json::json!(false));
        assert_eq!(v["tls"]["server_name"], serde_json::json!("t.com"));
    }

    // ── http 的 tls.server_name / tls.insecure 端到端（UI 补 sni/insecure 控件的后端侧门）─────────
    //
    // http **不在** `TLS_PROTOCOLS` 里，TLS 由 `security` 决定；一旦 `security='tls'`，走的就是
    // 与 trojan/vless 同一段装配。断言同样落在序列化后的 JSON（理由见上一组注释）。

    #[test]
    fn http_tls_allow_insecure_true_emits_tls_insecure_true() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"http","address":"p.com","port":8080,
                "username":"u","password":"pw","security":"tls",
                "tlsSettings":{"serverName":"http.sni","allowInsecure":true}}"#,
        );
        assert_eq!(v["tls"]["enabled"], serde_json::json!(true));
        assert_eq!(v["tls"]["insecure"], serde_json::json!(true));
        assert_eq!(v["tls"]["server_name"], serde_json::json!("http.sni"));
    }

    #[test]
    fn http_tls_without_tls_settings_emits_insecure_false_and_address_sni() {
        // 开了 HTTPS 但两颗控件都没填：`insecure` 仍**显式**下发 `false`（不是省略键），
        // `server_name` 回落节点地址 —— 与 hy2/tuic/trojan 同一段代码，同一形状。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"http","address":"p.com","port":8080,
                "security":"tls"}"#,
        );
        assert_eq!(v["tls"]["insecure"], serde_json::json!(false));
        assert_eq!(v["tls"]["server_name"], serde_json::json!("p.com"));
    }

    #[test]
    fn http_security_none_does_not_enable_tls() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"http","address":"p.com","port":8080,
                "security":"none"}"#,
        );
        assert!(v.get("tls").is_none(), "明文 http 不得生成 TLS 块");
    }

    /// 前端 HIGH-1 清除门（关 TLS 时整块删 `tlsSettings`）的**后端侧理由**：
    /// 装配条件是 `security.is_tls() || tls_settings.is_some()` —— 残留的 phantom `tlsSettings`
    /// 会绕过 `security='none'` 把 TLS 打开，用户以为是明文代理，实际握手失败、静默失联。
    #[test]
    fn http_phantom_tls_settings_enable_tls_despite_security_none() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"http","address":"p.com","port":8080,
                "security":"none","tlsSettings":{"serverName":"stale.sni"}}"#,
        );
        assert_eq!(
            v["tls"]["enabled"],
            serde_json::json!(true),
            "残留 tlsSettings 会对明文口误开 TLS —— 故前端关 TLS 时必须整块清除"
        );
        assert_eq!(v["tls"]["server_name"], serde_json::json!("stale.sni"));
    }

    #[test]
    fn tls_protocols_keep_tls_regardless_of_security_case() {
        // trojan 恒需 TLS 块（TLS_PROTOCOLS）——不因 security 变体而丢。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"trojan","address":"t.com","port":443,
                "password":"pw","security":"TLS"}"#,
        );
        assert_eq!(v["tls"]["enabled"], serde_json::json!(true));
    }

    // ── R4：指纹 / flow 归一（上游 #298）────────────────────────────────────

    #[test]
    fn uppercase_fingerprint_normalized_in_generated_json() {
        // 实测：sing-box 对 "Chrome" 报 `unknown uTLS fingerprint` FATAL → 核起不来。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"tls","tlsSettings":{"fingerprint":"Firefox"}}"#,
        );
        assert_eq!(
            v["tls"]["utls"]["fingerprint"],
            serde_json::json!("firefox")
        );
        assert_eq!(v["tls"]["utls"]["enabled"], serde_json::json!(true));
    }

    #[test]
    fn uppercase_fingerprint_none_disables_utls() {
        // "None" 本意是禁用 utls；不归一则反而下发非法指纹 "None" → sing-box FATAL。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"tls","tlsSettings":{"fingerprint":"NONE"}}"#,
        );
        assert!(
            v["tls"].get("utls").is_none(),
            "fingerprint=none（任意大小写）必须禁用 utls，而非下发非法指纹"
        );
    }

    #[test]
    fn reality_fingerprint_normalized_in_generated_json() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"reality","tlsSettings":{"fingerprint":"SAFARI"},
                "realitySettings":{"publicKey":"pk","shortId":"01"}}"#,
        );
        assert_eq!(v["tls"]["utls"]["fingerprint"], serde_json::json!("safari"));
    }

    #[test]
    fn uppercase_flow_normalized_in_generated_json() {
        // 实测：sing-box 对 "XTLS-RPRX-Vision" 报 `unsupported flow` FATAL。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"tls","flow":"XTLS-RPRX-Vision"}"#,
        );
        assert_eq!(v["flow"], serde_json::json!("xtls-rprx-vision"));
    }

    #[test]
    fn uppercase_flow_still_suppresses_multiplex() {
        // vision flow 必须跳过 mux —— 大小写变体不得让该判定失效。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"tls","flow":"XTLS-RPRX-VISION",
                "multiplexSettings":{"enabled":true}}"#,
        );
        assert!(
            v.get("multiplex").is_none(),
            "vision flow 必须跳过 multiplex"
        );
    }

    #[test]
    fn uppercase_vmess_security_normalized_in_generated_json() {
        // 实测：sing-box 对 "AES-128-GCM" 报 `unsupported security type` FATAL。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vmess","address":"v.com","port":443,
                "uuid":"u-1","vmessSecurity":"AES-128-GCM"}"#,
        );
        assert_eq!(v["security"], serde_json::json!("aes-128-gcm"));
    }

    #[test]
    fn uppercase_network_still_generates_transport() {
        // "WS" 不归一 → generate_transport_config 落 `_ => None` → 静默丢传输层。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"w.com","port":443,
                "uuid":"u-1","security":"tls","network":"WS",
                "wsSettings":{"path":"/ws"}}"#,
        );
        assert_eq!(v["transport"]["type"], serde_json::json!("ws"));
        assert_eq!(v["transport"]["path"], serde_json::json!("/ws"));
    }

    // ── 传输层 / Reality / 指纹 / ALPN 的**后端侧门**（UI 补 ws·grpc·anytls-reality·fp·alpn 控件那批）──
    //
    // 这批断言全部落在**序列化后的 JSON**：`Transport` 的每个字段都带
    // `skip_serializing_if = "Option::is_none"`，只断言结构体字段的话，哪天某个键被漏出配置也照样绿。

    /// 🔴 **「选了就废」的证据**：选了 ws 传输但 `wsSettings` 缺席 ⇒ path 落默认 `/`。
    /// 机场节点的 ws path 绝大多数不是 `/` ⇒ 该节点必然连不上。前端补 path/Host 控件的全部理由。
    #[test]
    fn ws_without_settings_falls_back_to_root_path() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"w.com","port":443,
                "uuid":"u-1","security":"tls","network":"ws"}"#,
        );
        assert_eq!(v["transport"]["type"], serde_json::json!("ws"));
        assert_eq!(v["transport"]["path"], serde_json::json!("/"));
        assert!(
            v["transport"].get("headers").is_none(),
            "没填 Host 时不得凭空造 headers"
        );
    }

    /// ws：`path` 原样下发，`headers` 整份透传（Host 是其中一个键，值为单值形态）。
    #[test]
    fn ws_path_and_host_header_reach_transport_json() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"w.com","port":443,
                "uuid":"u-1","security":"tls","network":"ws",
                "wsSettings":{"path":"/ray","headers":{"Host":"cdn.example.com"}}}"#,
        );
        assert_eq!(v["transport"]["type"], serde_json::json!("ws"));
        assert_eq!(v["transport"]["path"], serde_json::json!("/ray"));
        assert_eq!(
            v["transport"]["headers"]["Host"],
            serde_json::json!("cdn.example.com")
        );
    }

    /// httpupgrade **与 ws 同读 `wsSettings`**，但形态不同构：Host 落在顶层 `host` 而不是 `headers`，
    /// 且缺席时回落 `tlsSettings.serverName`。前端因此可以共用 path/Host 两个控件（同一份 wsSettings），
    /// 但 ws 独有的 `?ed=` 早数据解析在这条腿上**不发生** —— 那部分不属于本批。
    #[test]
    fn httpupgrade_reads_ws_settings_host_into_top_level_host() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"trojan","address":"t.com","port":443,
                "password":"pw","network":"httpupgrade",
                "wsSettings":{"path":"/hu","headers":{"Host":"hu.example.com"}}}"#,
        );
        assert_eq!(v["transport"]["type"], serde_json::json!("httpupgrade"));
        assert_eq!(v["transport"]["path"], serde_json::json!("/hu"));
        assert_eq!(v["transport"]["host"], serde_json::json!("hu.example.com"));
        assert!(v["transport"].get("headers").is_none());
    }

    #[test]
    fn httpupgrade_without_host_header_falls_back_to_tls_server_name() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"trojan","address":"t.com","port":443,
                "password":"pw","network":"httpupgrade",
                "tlsSettings":{"serverName":"sni.example.com"}}"#,
        );
        assert_eq!(v["transport"]["path"], serde_json::json!("/"));
        assert_eq!(v["transport"]["host"], serde_json::json!("sni.example.com"));
    }

    /// grpc：`service_name` **恒下发**（`unwrap_or_default()`）⇒ 前端留空与不建 `grpcSettings` 逐字节同结果。
    #[test]
    fn grpc_service_name_emitted_and_defaults_to_empty_string() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vmess","address":"g.com","port":443,
                "uuid":"u-1","security":"tls","network":"grpc",
                "grpcSettings":{"serviceName":"GunService"}}"#,
        );
        assert_eq!(v["transport"]["type"], serde_json::json!("grpc"));
        assert_eq!(
            v["transport"]["service_name"],
            serde_json::json!("GunService")
        );

        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vmess","address":"g.com","port":443,
                "uuid":"u-1","security":"tls","network":"grpc"}"#,
        );
        assert_eq!(v["transport"]["service_name"], serde_json::json!(""));
    }

    /// trojan 的 `httpupgrade` / `http` 传输一直可用（分派只看 `network`，不按协议门控）——
    /// 缺的只是前端下拉档位。
    #[test]
    fn trojan_supports_httpupgrade_and_http_transports() {
        for (net, want) in [("httpupgrade", "httpupgrade"), ("http", "http")] {
            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"trojan","address":"t.com","port":443,
                    "password":"pw","network":"{net}"}}"#
            ));
            assert_eq!(
                v["transport"]["type"],
                serde_json::json!(want),
                "trojan network={net} 必须生成传输层"
            );
        }
    }

    /// 🔴 **Reality 不按协议门控**：判据是 `security.is_reality()`，anytls 与 vless 走同一段装配
    /// ⇒ anytls 一直支持 reality，缺的只是前端的 sec 选择器与 pbk/sid 控件。
    #[test]
    fn anytls_reality_is_assembled_like_vless() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"anytls","address":"a.com","port":443,
                "password":"pw","security":"reality",
                "tlsSettings":{"serverName":"at.sni","fingerprint":"firefox","allowInsecure":true},
                "realitySettings":{"publicKey":"at-pub","shortId":"cd34"}}"#,
        );
        assert_eq!(v["tls"]["enabled"], serde_json::json!(true));
        assert_eq!(v["tls"]["reality"]["enabled"], serde_json::json!(true));
        assert_eq!(
            v["tls"]["reality"]["public_key"],
            serde_json::json!("at-pub")
        );
        assert_eq!(v["tls"]["reality"]["short_id"], serde_json::json!("cd34"));
        // reality 版 TLS 块仍从 tlsSettings 取 sni/insecure/utls ⇒ 那三颗控件在 reality 下照样有效。
        assert_eq!(v["tls"]["server_name"], serde_json::json!("at.sni"));
        assert_eq!(v["tls"]["insecure"], serde_json::json!(true));
        assert_eq!(
            v["tls"]["utls"]["fingerprint"],
            serde_json::json!("firefox")
        );
    }

    /// anytls 选了 reality 却没填公钥（`realitySettings` 缺席）⇒ 不造 reality 块，但 TLS 块仍在
    /// （anytls ∈ `TLS_PROTOCOLS`）—— 前端「pbk 为空即整块不下发」不会造出半成品节点。
    #[test]
    fn anytls_reality_without_settings_keeps_plain_tls_block() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"anytls","address":"a.com","port":443,
                "password":"pw","security":"reality"}"#,
        );
        assert_eq!(v["tls"]["enabled"], serde_json::json!(true));
        assert!(v["tls"].get("reality").is_none());
        assert_eq!(v["tls"]["server_name"], serde_json::json!("a.com"));
    }

    /// vmess / trojan 的 uTLS 指纹：**缺省是 `none`**（与 vless/anytls 的 `chrome` 不同）
    /// ⇒ 没填时整个 `utls` 块不下发；填了才有。前端的 fp 首档因此必须是空串而非 chrome。
    #[test]
    fn vmess_trojan_fingerprint_defaults_to_none_and_is_emitted_when_set() {
        for (proto, extra) in [
            ("vmess", r#""uuid":"u-1","security":"tls""#),
            ("trojan", r#""password":"pw""#),
        ] {
            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"{proto}","address":"x.com","port":443,{extra}}}"#
            ));
            assert!(
                v["tls"].get("utls").is_none(),
                "{proto} 没填指纹时不得下发 utls（缺省 none）"
            );

            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"{proto}","address":"x.com","port":443,{extra},
                    "tlsSettings":{{"fingerprint":"safari"}}}}"#
            ));
            assert_eq!(
                v["tls"]["utls"]["fingerprint"],
                serde_json::json!("safari"),
                "{proto} 填了指纹必须下发"
            );
        }
    }

    /// trojan 的 ALPN：**留空 ≠ 空数组** —— 缺省专属回落 `["http/1.1"]`，填了才覆盖。
    /// 故前端空值必须是「不下发 alpn 键」；写 `alpn: []` 会把这条缺省顶掉。
    #[test]
    fn trojan_alpn_default_is_overridden_only_when_set() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"trojan","address":"t.com","port":443,
                "password":"pw"}"#,
        );
        assert_eq!(v["tls"]["alpn"], serde_json::json!(["http/1.1"]));

        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"trojan","address":"t.com","port":443,
                "password":"pw","tlsSettings":{"alpn":["h3","h2"]}}"#,
        );
        assert_eq!(v["tls"]["alpn"], serde_json::json!(["h3", "h2"]));

        // 空数组是**用户真的清空了 ALPN**，不等于「没填」——不得被缺省顶回 http/1.1。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"trojan","address":"t.com","port":443,
                "password":"pw","tlsSettings":{"alpn":[]}}"#,
        );
        assert_eq!(v["tls"]["alpn"], serde_json::json!([]));
    }

    /// vmess `security` 是开放 String，`zero` 原样透传（内核合法档，上游 下拉里也有）。
    #[test]
    fn vmess_zero_security_passes_through() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vmess","address":"v.com","port":443,
                "uuid":"u-1","vmessSecurity":"zero"}"#,
        );
        assert_eq!(v["security"], serde_json::json!("zero"));
    }

    #[test]
    fn ws_transport_ed_parse() {
        let mut s = server(Protocol::Vless, "w.com");
        s.uuid = Some("u".into());
        s.network = Some("ws".into());
        s.ws_settings = Some(ps::WebSocketSettings {
            path: Some("/ws?ed=2560".into()),
            ..Default::default()
        });
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), "x64", "linux");
        let t = ob.transport.as_ref().unwrap();
        assert_eq!(t.type_field, "ws");
        assert_eq!(t.path.as_deref(), Some("/ws")); // ed 剥离
        assert_eq!(t.max_early_data, Some(2560));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // 批 B（TLS 高级三件套 · multiplex · tuic 0-RTT/心跳 · ssh 算法协商 · ss 插件 ·
    // ws 早数据）的**后端侧门**。生产代码一行未改，这些断言只是把「Rust 本来就会下发」
    // 这个前提钉死 —— 它一旦不成立，前端那批控件就退化成假控件。
    //
    // 与上一批同一纪律：断言全部落在**序列化后的 JSON**（`OutboundTls`/`Transport`/`Multiplex`
    // 的字段几乎都带 `skip_serializing_if`，只断言结构体字段的话，键被漏出配置也照样绿）。
    // ══════════════════════════════════════════════════════════════════════════

    /// `outbound_json_from` 把 arch/platform 钉死成 `("x64","linux")`；TLS engine 与 spoof 的门
    /// **恰恰读这两个参数**，故本批需要一个能改这两维的版本。
    fn outbound_json_on(server_json: &str, arch: &str, platform: &str) -> serde_json::Value {
        let s: ServerConfig = serde_json::from_str(server_json).expect("节点必须能反序列化");
        let ob = build_proxy_outbound(&s, "proxy-s1", &test_dial_resolver(), arch, platform);
        serde_json::to_value(&ob).unwrap()
    }

    /// TLS engine 是**平台门控**（`should_emit_tls_engine`）：只有 windows/win32、apple/darwin
    /// 两种组合才下发；`go` 与缺席都不下发 ⇒ 前端 engine 下拉的首档必须是空串，且跨平台选错档
    /// 不会造出会 FATAL 的配置（这正是 Polaris 不像 上游 那样按平台隐藏选项的安全依据）。
    #[test]
    fn tls_engine_is_platform_gated() {
        let node = |engine: &str| {
            format!(
                r#"{{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                    "uuid":"u-1","security":"tls","tlsSettings":{{"engine":"{engine}"}}}}"#
            )
        };
        assert_eq!(
            outbound_json_on(&node("windows"), "x64", "win32")["tls"]["engine"],
            serde_json::json!("windows")
        );
        assert_eq!(
            outbound_json_on(&node("apple"), "arm64", "darwin")["tls"]["engine"],
            serde_json::json!("apple")
        );
        // 平台不匹配 / go / 缺席：一律不下发该键。
        for (engine, platform) in [
            ("windows", "darwin"),
            ("apple", "win32"),
            ("windows", "linux"),
            ("go", "win32"),
        ] {
            let v = outbound_json_on(&node(engine), "x64", platform);
            assert!(
                v["tls"].get("engine").is_none(),
                "engine={engine} platform={platform} 不得下发 tls.engine"
            );
        }
    }

    /// 🔴 **Reality 下不发 `tls.engine`，且这不是缺口——是内核的硬约束。**
    ///
    /// 机制：TLS 段先按 `should_emit_tls_engine` 把 engine 装进 `ob.tls`，reality 段随后用一个
    /// 新造的 `OutboundTls`（`engine: None`）**整体替换**掉它。spoof/ech 由
    /// `apply_anti_censorship_options` 在替换**之后**补，故照常生效 —— 它们是本测试的阴性对照，
    /// 用来证明「reality 段替换掉了整个块」这个机制描述本身是对的，而不是随便丢了几个键。
    ///
    /// 判据不是 schema：`$defs/OutboundTLSOptions` 里 `engine` 与 `reality` 确实是平级、无互斥约束，
    /// 但那只表达键的形状。真正的拒绝发生在 `initialize outbound` 阶段，四个随包二进制的字符串
    /// 在场矩阵是判决书（生产注释里有完整表）：`"reality is unsupported in "` 与
    /// `"utls is unsupported in "` 只编进 win / mac 那三个构建，linux 一条都没有。
    /// ⇒ Linux 上做的任何 `reality × engine` 对照都只能碰到桩，检出力为 0。
    ///
    /// ⇒ 前端 `whenTlsEngine` 上那条 `!whenReality` **有依据、必须留着**：reality 下这一档在任何
    /// 平台都不可用，显示它就是一个「拨了必然让整核起不来」的控件。
    ///
    /// 2026-08-07 曾按「schema 平级 ⇒ 是本仓缺口」把 engine 搬进 reality 块，本测试同批被改成
    /// 断言 `engine == "windows"`。那次改动把「静默丢一个键、核照常起」换成了「核起不来」，
    /// 已回退。别再来一次。
    #[test]
    fn reality_branch_drops_tls_engine_but_keeps_spoof_and_ech() {
        let node = |security: &str| {
            format!(
                r#"{{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                    "uuid":"u-1","security":"{security}",
                    "realitySettings":{{"publicKey":"pk","shortId":"ab"}},
                    "tlsSettings":{{"serverName":"s.com","engine":"windows",
                        "spoofMethod":"wrong-ack","spoofSni":"decoy.com","ech":true}}}}"#
            )
        };
        // 正向对照：同一份 tlsSettings 在 security=tls 下 engine **是**下发的 ——
        // 没有这一条，下面那句 `is_none()` 可能只是因为平台门没放行，与 reality 无关。
        let plain = outbound_json_on(&node("tls"), "x64", "win32");
        assert_eq!(plain["tls"]["engine"], serde_json::json!("windows"));

        let reality = outbound_json_on(&node("reality"), "x64", "win32");
        assert_eq!(
            reality["tls"]["reality"]["public_key"],
            serde_json::json!("pk")
        );
        assert!(
            reality["tls"].get("engine").is_none(),
            "reality 下发了 tls.engine ⇒ 真机 win32/darwin 上内核会 \
             `FATAL initialize outbound: reality is unsupported in <engine>`，整份配置起不来"
        );
        // 阴性对照：spoof / ech 在 reality 下**照常生效**（它们在替换之后才补），
        // 故「reality 不发 engine」不是「reality 把 TLS 相关的都丢了」。
        assert_eq!(reality["tls"]["spoof"], serde_json::json!("decoy.com"));
        assert_eq!(
            reality["tls"]["spoof_method"],
            serde_json::json!("wrong-ack")
        );
        assert_eq!(reality["tls"]["ech"]["enabled"], serde_json::json!(true));
    }

    /// 同一条约束在 darwin 上也成立 —— 上面那条只跑了 win32。
    ///
    /// 为什么值得单列：`should_emit_tls_engine` 是 `(engine, platform)` 二元门，win32 绿不蕴含
    /// darwin 绿；而 `"reality is unsupported in "` 在 mac-x64 / mac-arm64 两个二进制里都在场，
    /// 即 darwin 上的判决与 win32 同型。
    #[test]
    fn reality_drops_tls_engine_on_darwin_too() {
        let node = |security: &str| {
            format!(
                r#"{{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                    "uuid":"u-1","security":"{security}",
                    "realitySettings":{{"publicKey":"pk","shortId":"ab"}},
                    "tlsSettings":{{"serverName":"s.com","engine":"apple"}}}}"#
            )
        };
        // 正向对照先行：apple × darwin 这一组在 security=tls 下确实过得了平台门。
        assert_eq!(
            outbound_json_on(&node("tls"), "arm64", "darwin")["tls"]["engine"],
            serde_json::json!("apple")
        );
        let v = outbound_json_on(&node("reality"), "arm64", "darwin");
        assert_eq!(v["tls"]["reality"]["public_key"], serde_json::json!("pk"));
        assert!(v["tls"].get("engine").is_none());
    }

    /// QUIC 自管 TLS 的两个协议**永远拿不到 engine**（`is_quic_managed_tls` 前置门）——
    /// 覆盖矩阵把「hy2/tuic 不出 engine」列为有意排除，依据就是这一句。
    #[test]
    fn tls_engine_never_emitted_for_quic_protocols() {
        for (proto, extra) in [
            ("hysteria2", r#""password":"pw""#),
            ("tuic", r#""uuid":"u-1","password":"pw""#),
        ] {
            let v = outbound_json_on(
                &format!(
                    r#"{{"id":"s1","name":"n","protocol":"{proto}","address":"q.com","port":443,{extra},
                        "tlsSettings":{{"engine":"windows"}}}}"#
                ),
                "x64",
                "win32",
            );
            assert!(
                v["tls"].get("engine").is_none(),
                "{proto} 的 TLS 在 QUIC 内自管，不得下发 engine"
            );
        }
    }

    /// TLS spoof 的下发要**同时**满足：方法合法 + 非 ARM + 诱饵 SNI 非空非 IP + 协议非 QUIC/naive
    /// + 诱饵 ≠ 真 server_name 且真 server_name 非 IP。任一不满足即整对不下发（不 FATAL）。
    #[test]
    fn tls_spoof_emitted_only_when_every_gate_passes() {
        let node = |method: &str, spoof: &str, sni: &str| {
            format!(
                r#"{{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                    "uuid":"u-1","security":"tls",
                    "tlsSettings":{{"serverName":"{sni}","spoofMethod":"{method}","spoofSni":"{spoof}"}}}}"#
            )
        };
        let v = outbound_json_on(&node("wrong-ack", "decoy.com", "real.com"), "x64", "linux");
        assert_eq!(v["tls"]["spoof"], serde_json::json!("decoy.com"));
        assert_eq!(v["tls"]["spoof_method"], serde_json::json!("wrong-ack"));

        // ARM64：内核只在 amd64 实现 ⇒ 整对不下发（前端因此把 ARM64 限制写进控件说明）。
        let v = outbound_json_on(
            &node("wrong-ack", "decoy.com", "real.com"),
            "arm64",
            "linux",
        );
        assert!(v["tls"].get("spoof").is_none());
        assert!(v["tls"].get("spoof_method").is_none());

        // 诱饵 == 真 server_name / 诱饵是 IP 字面量 / 诱饵为空 / 方法不在三档白名单：都不下发。
        for (method, spoof, sni, why) in [
            ("wrong-ack", "same.com", "same.com", "诱饵不得等于真 SNI"),
            ("wrong-ack", "1.2.3.4", "real.com", "诱饵不得是 IP 字面量"),
            ("wrong-ack", "", "real.com", "诱饵为空"),
            (
                "wrong-sequence",
                "decoy.com",
                "real.com",
                "内核 schema 有这档但本仓门控只放行三档",
            ),
        ] {
            let v = outbound_json_on(&node(method, spoof, sni), "x64", "linux");
            assert!(v["tls"].get("spoof").is_none(), "{why}");
            assert!(v["tls"].get("spoof_method").is_none(), "{why}");
        }
    }

    /// 真 server_name 是 IP 字面量（节点地址是 IP 且没填 SNI 的回落形态）同样堵死 spoof ——
    /// 这条是「填了却没生效」的最常见成因，前端说明里点了名。
    #[test]
    fn tls_spoof_blocked_when_real_server_name_is_ip() {
        let v = outbound_json_on(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"1.2.3.4","port":443,
                "uuid":"u-1","security":"tls",
                "tlsSettings":{"spoofMethod":"wrong-md5","spoofSni":"decoy.com"}}"#,
            "x64",
            "linux",
        );
        assert_eq!(v["tls"]["server_name"], serde_json::json!("1.2.3.4"));
        assert!(v["tls"].get("spoof").is_none());
    }

    /// ECH 对**任何有 TLS 块的协议**一视同仁（`apply_anti_censorship_options` 无协议门）——
    /// hy2/tuic 早已做过控件，vless/vmess/trojan/anytls 缺的只是控件。
    /// `echConfig` 留空 = 只发 `{enabled:true}`（内核从 DNS HTTPS RR 自取），填了才带 config 数组。
    #[test]
    fn ech_is_assembled_for_every_tcp_tls_protocol() {
        for (proto, extra) in [
            ("vless", r#""uuid":"u-1","security":"tls""#),
            ("vmess", r#""uuid":"u-1","security":"tls""#),
            ("trojan", r#""password":"pw""#),
            ("anytls", r#""password":"pw""#),
            ("http", r#""username":"u","password":"pw","security":"tls""#),
        ] {
            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"{proto}","address":"e.com","port":443,{extra},
                    "tlsSettings":{{"ech":true}}}}"#
            ));
            assert_eq!(
                v["tls"]["ech"]["enabled"],
                serde_json::json!(true),
                "{proto} 的 ECH 没装配"
            );
            assert!(
                v["tls"]["ech"].get("config").is_none(),
                "{proto}: echConfig 留空时不得下发 config 键"
            );

            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"{proto}","address":"e.com","port":443,{extra},
                    "tlsSettings":{{"ech":true,"echConfig":"line-a\nline-b"}}}}"#
            ));
            assert_eq!(
                v["tls"]["ech"]["config"],
                serde_json::json!(["line-a", "line-b"]),
                "{proto}: echConfig 按行拆成数组"
            );
        }
    }

    /// 🔴 Multiplex 的协议面就是那句 `matches!` —— 四个协议下发、其余**静默丢弃**。
    /// 前端 `F_MUX` 只挂这四个，依据即此；给别的协议加控件就是假控件。
    #[test]
    fn multiplex_only_for_the_four_protocols_in_matches() {
        let node = |proto: &str, extra: &str| {
            format!(
                r#"{{"id":"s1","name":"n","protocol":"{proto}","address":"m.com","port":443,{extra},
                    "multiplexSettings":{{"enabled":true,"protocol":"yamux","maxConnections":4,
                    "minStreams":2,"padding":true}}}}"#
            )
        };
        for (proto, extra) in [
            ("vless", r#""uuid":"u-1""#),
            ("vmess", r#""uuid":"u-1""#),
            ("trojan", r#""password":"pw""#),
            (
                "shadowsocks",
                r#""shadowsocksSettings":{"method":"aes-256-gcm","password":"p"}"#,
            ),
        ] {
            let v = outbound_json_from(&node(proto, extra));
            assert_eq!(
                v["multiplex"]["enabled"],
                serde_json::json!(true),
                "{proto}"
            );
            assert_eq!(v["multiplex"]["protocol"], serde_json::json!("yamux"));
            assert_eq!(v["multiplex"]["max_connections"], serde_json::json!(4));
            assert_eq!(v["multiplex"]["min_streams"], serde_json::json!(2));
            assert_eq!(v["multiplex"]["padding"], serde_json::json!(true));
        }
        // 阴性对照：不在 `matches!` 里的协议，同一份 multiplexSettings 一个字节都到不了配置。
        for (proto, extra) in [
            ("anytls", r#""password":"pw""#),
            ("hysteria2", r#""password":"pw""#),
            ("socks", r#""username":"u""#),
        ] {
            let v = outbound_json_from(&node(proto, extra));
            assert!(
                v.get("multiplex").is_none(),
                "{proto} 不在 matches! 里，multiplex 必须被丢弃"
            );
        }
    }

    /// multiplex 的可选三键留空 → 不下发；`protocol` 缺席 → 后端补 `h2mux`
    /// （故前端下拉的 h2mux 档与「不写」逐字节同结果）。
    #[test]
    fn multiplex_optional_keys_omitted_and_protocol_defaults_to_h2mux() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"m.com","port":443,
                "uuid":"u-1","multiplexSettings":{"enabled":true}}"#,
        );
        assert_eq!(v["multiplex"]["protocol"], serde_json::json!("h2mux"));
        for k in ["max_connections", "min_streams", "padding"] {
            assert!(v["multiplex"].get(k).is_none(), "留空的 {k} 不得下发");
        }
        // `enabled:false` 与整块缺席同结果 —— 前端「关开关即整块不下发」不会改变生成产物。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"m.com","port":443,
                "uuid":"u-1","multiplexSettings":{"enabled":false,"protocol":"smux"}}"#,
        );
        assert!(v.get("multiplex").is_none());
    }

    /// vision flow 跳过 multiplex —— 判据是 `flow.to_ascii_lowercase().contains("vision")`
    /// （**子串**匹配，不是相等），前端 `whenMuxAvail` 逐字镜像了这一点。
    #[test]
    fn multiplex_skipped_for_any_vision_flow_variant() {
        for flow in [
            "xtls-rprx-vision",
            "XTLS-RPRX-VISION",
            "xtls-rprx-vision-udp443",
        ] {
            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"vless","address":"m.com","port":443,
                    "uuid":"u-1","flow":"{flow}","multiplexSettings":{{"enabled":true}}}}"#
            ));
            assert!(
                v.get("multiplex").is_none(),
                "flow={flow} 必须跳过 multiplex"
            );
        }
    }

    /// tuic 的 0-RTT 与心跳都是真下发；`heartbeat` 走 `normalize_duration`（裸数字补 `ms`）。
    #[test]
    fn tuic_zero_rtt_and_heartbeat_reach_json() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"tuic","address":"t.com","port":443,
                "uuid":"u-1","password":"pw",
                "tuicSettings":{"zeroRttHandshake":true,"heartbeat":"10s"}}"#,
        );
        assert_eq!(v["zero_rtt_handshake"], serde_json::json!(true));
        assert_eq!(v["heartbeat"], serde_json::json!("10s"));

        // 裸数字 → 补 ms（前端因此原样存用户输入，不在 UI 侧补单位）。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"tuic","address":"t.com","port":443,
                "uuid":"u-1","password":"pw","tuicSettings":{"heartbeat":"3000"}}"#,
        );
        assert_eq!(v["heartbeat"], serde_json::json!("3000ms"));

        // 两键缺席 → 都不下发（前端「关=删键、空=删键」与之逐字节一致）。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"tuic","address":"t.com","port":443,
                "uuid":"u-1","password":"pw","tuicSettings":{"congestionControl":"bbr"}}"#,
        );
        assert!(v.get("zero_rtt_handshake").is_none());
        assert!(v.get("heartbeat").is_none());
    }

    /// ssh 的四个算法协商列表都是真下发，键名以内核 schema 为准（单数 `cipher`/`mac`/`kex_algorithm`）。
    #[test]
    fn ssh_algorithm_lists_reach_json() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"ssh","address":"s.com","port":22,
                "sshSettings":{"user":"root","hostKey":["ssh-ed25519 AAAA"],
                "hostKeyAlgorithms":["ssh-ed25519","rsa-sha2-256"],
                "cipher":["aes128-ctr"],"mac":["hmac-sha2-256"],
                "kexAlgorithm":["curve25519-sha256"]}}"#,
        );
        assert_eq!(
            v["host_key_algorithms"],
            serde_json::json!(["ssh-ed25519", "rsa-sha2-256"])
        );
        assert_eq!(v["cipher"], serde_json::json!(["aes128-ctr"]));
        assert_eq!(v["mac"], serde_json::json!(["hmac-sha2-256"]));
        assert_eq!(v["kex_algorithm"], serde_json::json!(["curve25519-sha256"]));

        // 缺席 → 不下发（前端留空必须删键：空数组等于「一个算法都不接受」，不是「用默认集」）。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"ssh","address":"s.com","port":22,
                "sshSettings":{"user":"root"}}"#,
        );
        for k in ["host_key_algorithms", "cipher", "mac", "kex_algorithm"] {
            assert!(v.get(k).is_none(), "{k} 缺席时不得下发");
        }
    }

    /// shadowsocks 的 SIP003 插件两键原样透传。
    #[test]
    fn shadowsocks_plugin_and_opts_reach_json() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"shadowsocks","address":"ss.com","port":8388,
                "shadowsocksSettings":{"method":"aes-256-gcm","password":"p",
                "plugin":"obfs-local","pluginOptions":"obfs=http;obfs-host=bing.com"}}"#,
        );
        assert_eq!(v["plugin"], serde_json::json!("obfs-local"));
        assert_eq!(
            v["plugin_opts"],
            serde_json::json!("obfs=http;obfs-host=bing.com")
        );

        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"shadowsocks","address":"ss.com","port":8388,
                "shadowsocksSettings":{"method":"aes-192-gcm","password":"p"}}"#,
        );
        assert_eq!(v["method"], serde_json::json!("aes-192-gcm")); // T5：表外档位是内核合法值
        assert!(v.get("plugin").is_none());
        assert!(v.get("plugin_opts").is_none());
    }

    /// 🔴 ws 早数据两键的**归属与优先级**（前端控件语义的全部依据）：
    ///  ① 两键只在 `ws` 腿下发，`httpupgrade` 腿根本不读 ⇒ 前端用 `whenWs` 而非 `whenWsLike`；
    ///  ② `path` 里的 `?ed=` **赢过** `wsSettings.maxEarlyData`（`ed.or_else(|| ws.max_early_data)`），
    ///     且 `ed`/`eh` 会从 path 上被摘掉。
    #[test]
    fn ws_early_data_belongs_to_ws_leg_and_path_ed_wins() {
        // ① 只填 settings、path 无 ed：两键按填的走。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"w.com","port":443,
                "uuid":"u-1","network":"ws",
                "wsSettings":{"path":"/ray","maxEarlyData":1024,"earlyDataHeaderName":"X-Ed"}}"#,
        );
        assert_eq!(v["transport"]["path"], serde_json::json!("/ray"));
        assert_eq!(v["transport"]["max_early_data"], serde_json::json!(1024));
        assert_eq!(
            v["transport"]["early_data_header_name"],
            serde_json::json!("X-Ed")
        );

        // ② path 的 ?ed=/?eh= 覆盖 settings（前端因此在控件说明里写明「路径赢」）。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"w.com","port":443,
                "uuid":"u-1","network":"ws",
                "wsSettings":{"path":"/ray?ed=2560&eh=X-Path","maxEarlyData":1024,
                "earlyDataHeaderName":"X-Settings"}}"#,
        );
        assert_eq!(v["transport"]["path"], serde_json::json!("/ray"));
        assert_eq!(v["transport"]["max_early_data"], serde_json::json!(2560));
        assert_eq!(
            v["transport"]["early_data_header_name"],
            serde_json::json!("X-Path")
        );

        // ③ httpupgrade 腿：同一份 wsSettings 里的这两键**一个都不下发**。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"w.com","port":443,
                "uuid":"u-1","security":"tls","network":"httpupgrade",
                "wsSettings":{"path":"/hu","maxEarlyData":1024,"earlyDataHeaderName":"X-Ed"}}"#,
        );
        assert_eq!(v["transport"]["type"], serde_json::json!("httpupgrade"));
        assert!(v["transport"].get("max_early_data").is_none());
        assert!(v["transport"].get("early_data_header_name").is_none());
    }

    /// 🔴 **`GrpcSettings.multiMode` 永远到不了内核** —— 这是「不该补控件」那条裁定的证据。
    /// `generate_transport_config` 的 grpc 腿只造 `type` + `service_name`，`Transport` 结构体里
    /// 压根没有这个字段；随包核 beta.7 的 grpc 传输 schema 同样没有（`additionalProperties:false`，
    /// 真下发反而是 FATAL）。它只活在 share-link 往返里（`net-stack/share_link.rs` 的 `mode=multi`）。
    #[test]
    fn grpc_multi_mode_never_reaches_the_kernel() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"g.com","port":443,
                "uuid":"u-1","security":"tls","network":"grpc",
                "grpcSettings":{"serviceName":"GunService","multiMode":true}}"#,
        );
        assert_eq!(
            v["transport"]["service_name"],
            serde_json::json!("GunService")
        );
        let keys: Vec<&str> = v["transport"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        // `serde_json::Map` 无 preserve_order feature ⇒ 键有序，断言按字典序写。
        assert_eq!(
            keys,
            vec!["service_name", "type"],
            "grpc 传输只有这两个键 —— multi_mode 建了模却无处可去，给它加控件即假控件"
        );
    }

    // ── 批 D 的后端侧门（UI 补 h2 四件套 · alpn×5 · http 指纹 · hy2 network · naive ECH · fragment×5）──
    //
    // 同上一批：断言全部落在**序列化后的 JSON**，只断言结构体字段的话，某个键被 `skip_serializing_if`
    // 漏出配置也照样绿。

    /// h2 传输的四个键**全都被读**（`generate_transport_config` 的 `"http" | "h2"` 腿）。
    /// `host` 是 `Vec<String>`：长度 1 序列化成裸串、>1 成数组（`OneOrMany`），两种形态内核都认。
    #[test]
    fn h2_transport_reads_all_four_http_settings_keys() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"h.com","port":443,
                "uuid":"u-1","security":"tls","network":"http",
                "httpSettings":{"path":"/h2","host":["a.com","b.com"],"method":"PUT",
                                "headers":{"X-Real-IP":["1.2.3.4"]}}}"#,
        );
        assert_eq!(v["transport"]["type"], serde_json::json!("http"));
        assert_eq!(v["transport"]["path"], serde_json::json!("/h2"));
        assert_eq!(
            v["transport"]["host"],
            serde_json::json!(["a.com", "b.com"])
        );
        assert_eq!(v["transport"]["method"], serde_json::json!("PUT"));
        assert_eq!(
            v["transport"]["headers"]["X-Real-IP"],
            serde_json::json!(["1.2.3.4"])
        );

        // 单元素 host → 裸串（`OneOrMany::One`）。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vmess","address":"h.com","port":443,
                "uuid":"u-1","security":"tls","network":"http",
                "httpSettings":{"host":["only.com"]}}"#,
        );
        assert_eq!(v["transport"]["host"], serde_json::json!("only.com"));
    }

    /// 「选了就废」的第二个实例：选了 h2 却没有 `httpSettings` ⇒ 只落 `path:"/"`，其余三键不下发。
    /// 前端补这四颗控件的全部理由（同 ws 那条 `ws_without_settings_falls_back_to_root_path`）。
    #[test]
    fn h2_without_settings_falls_back_to_root_path_only() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"trojan","address":"h.com","port":443,
                "password":"pw","network":"http"}"#,
        );
        assert_eq!(v["transport"]["type"], serde_json::json!("http"));
        assert_eq!(v["transport"]["path"], serde_json::json!("/"));
        for k in ["host", "method", "headers"] {
            assert!(
                v["transport"].get(k).is_none(),
                "没填 {k} 时不得凭空造该键（前端留空必须是删键）"
            );
        }
    }

    /// `final_alpn` 对**所有**走标准 TLS 栈的协议都读 `tls_settings.alpn` —— 此前只有 trojan/tuic
    /// 的表单给了输入框，其余四个协议的 alpn 是 per-protocol 判据新暴露出来的欠账。
    #[test]
    fn alpn_reaches_tls_json_for_every_standard_tls_protocol() {
        for (proto, cred) in [
            ("vless", r#""uuid":"u-1""#),
            ("vmess", r#""uuid":"u-1""#),
            ("anytls", r#""password":"pw""#),
            ("hysteria2", r#""password":"pw""#),
            ("http", r#""username":"u""#),
        ] {
            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"{proto}","address":"a.com","port":443,
                    {cred},"security":"tls","tlsSettings":{{"alpn":["h2","http/1.1"]}}}}"#
            ));
            assert_eq!(
                v["tls"]["alpn"],
                serde_json::json!(["h2", "http/1.1"]),
                "{proto} 的 alpn 必须原样下发"
            );
        }
    }

    /// 不填 alpn ⇒ **除 trojan 外都不下发该键**（trojan 有专属缺省 `["http/1.1"]`）。
    /// 这条钉的是前端「留空 = 删键」的正确性：写空数组会把 trojan 那条缺省顶掉。
    #[test]
    fn alpn_absent_means_no_key_except_trojan_default() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"tls"}"#,
        );
        assert!(v["tls"].get("alpn").is_none());
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"trojan","address":"a.com","port":443,
                "password":"pw","security":"tls"}"#,
        );
        assert_eq!(v["tls"]["alpn"], serde_json::json!(["http/1.1"]));
    }

    /// http 协议的 uTLS 指纹：http **不在** `is_quic_managed_tls` 里 ⇒ `final_fp != "none"` 时
    /// utls 块照常下发；不填则回落 `none`（非 vless/anytls 的缺省）⇒ 整块不下发。
    /// 后者正是前端必须用 `O_FP_OPT`（带空首项）而不是 `O_FP` 的理由。
    #[test]
    fn http_fingerprint_emits_utls_and_defaults_to_none() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"http","address":"a.com","port":8080,
                "username":"u","security":"tls","tlsSettings":{"fingerprint":"firefox"}}"#,
        );
        assert_eq!(v["tls"]["utls"]["enabled"], serde_json::json!(true));
        assert_eq!(
            v["tls"]["utls"]["fingerprint"],
            serde_json::json!("firefox")
        );

        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"http","address":"a.com","port":8080,
                "username":"u","security":"tls"}"#,
        );
        assert!(
            v["tls"].get("utls").is_none(),
            "http 缺省 final_fp = none ⇒ 整个 utls 块不下发；前端首档必须是空串"
        );
    }

    /// 🔴 **http 协议的 headers/path 必须落在出站顶层，`transport` 键一出现整个核就起不来。**
    ///
    /// 随包核 1.14.0-beta.7 的 http 出站 schema 无 `transport` 且 `additionalProperties:false`：
    /// 实测 `sing-box check` → `FATAL decode config: outbounds[0].transport: json: unknown field
    /// "transport"`（rc=1）；同一份 headers/path 写顶层 → rc=0。
    ///
    /// 这条断言是那次移植错误（上游 `singbox-outbound-builder.ts:391-398` 的 1:1 搬运）的回归锁：
    /// 只要有人把这两键挪回 `transport`，`transport` 键就会重新出现 ⇒ 转红。
    ///
    /// 🔴 **输入必须带 `network`。** 全仓 `http_settings` 的 4 个非测试写入点
    /// （`singbox_import.rs:269` / `xray_import.rs:120` / `clash_parser.rs:365` /
    /// `share_link.rs:280`）**每一处都在写 `http_settings` 的同时写 `network`**，
    /// 没有任何生产路径能造出「有 httpSettings、无 network」。若这里省掉 `network`，
    /// 传输层那段压根不跑，`transport` 恒缺席 ⇒ 断言恒真、对本缺陷零信息量
    /// （本测试第一版正是那个形状：产物在真实链路上照样 FATAL，而门全绿）。
    #[test]
    fn http_protocol_masquerade_goes_to_top_level_never_transport() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"http","address":"a.com","port":8080,
                "username":"u","password":"p","network":"http",
                "httpSettings":{"path":"/tunnel","headers":{"Host":["a.example.com"]}}}"#,
        );
        assert_eq!(v["path"], serde_json::json!("/tunnel"));
        assert_eq!(
            v["headers"],
            serde_json::json!({"Host": ["a.example.com"]}),
            "headers 对齐 schema 的 $defs/HTTPHeader = map<string, string|string[]>"
        );
        assert!(
            v.get("transport").is_none(),
            "http 出站一旦带 transport，内核 decode 阶段就 FATAL（整份配置起不来）"
        );
    }

    /// 🔴 **缺 `snellSettings` 的 snell 节点仍须发出内核认的 `version` + `psk`。**
    ///
    /// 此前整段包在 `if let Some(s) = &server.snell_settings` 里 ⇒ 缺席时一个键都不发，
    /// 内核在 **decode 阶段**判 `snell: missing version`，**整份配置起不来**（不止这个节点）。
    /// 由 `tests/kernel_accepts_outbounds.rs` 的协议 × 传输交叉门发现。
    ///
    /// 归一到 4/6 的第二个理由：`SnellVersion = u32` 且 `Default` 派生 ⇒ 缺省值是 **0**，
    /// 而 0 同样不是内核认的版本 —— 半份 JSON 反序列化就能得到它。
    ///
    /// 本条**不依赖真核**，故在 `ci.yml`（不拉核）上也守得住；交叉门那边是它的真环境复核。
    #[test]
    fn snell_without_settings_still_emits_a_kernel_valid_version_and_psk() {
        // 缺 `snellSettings`：此前整段跳过 ⇒ 出站既无 version 也无 psk ⇒ 内核 **decode 阶段**
        // 判 `snell: missing version`，整份配置起不来。
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"snell","address":"a.com","port":443,
                "password":"pw"}"#,
        );
        assert_eq!(
            v["version"],
            serde_json::json!(4),
            "缺 snellSettings 时必须落到 v4（同 UI proto-codec 的 `version === 6 ? 6 : 4`）"
        );
        assert_eq!(v["psk"], serde_json::json!("pw"), "psk 一并不能漏");

        // `version` 为 0（`SnellVersion = u32` + `Default` 派生的缺省值，半份 JSON 反序列化即得）
        // 同样要归一 —— 0 不是内核认的版本。
        let zero = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"snell","address":"a.com","port":443,
                "password":"pw","snellSettings":{"version":0}}"#,
        );
        assert_eq!(zero["version"], serde_json::json!(4));

        // 正向对照：显式 v6 不受归一影响，且走的是 v6 那条腿（mode 生效、obfs 不生效）。
        let v6 = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"snell","address":"a.com","port":443,
                "password":"pw","snellSettings":{"version":6,"mode":"aes-128-gcm",
                    "obfsMode":"http","obfsHost":"decoy.com"}}"#,
        );
        assert_eq!(v6["version"], serde_json::json!(6));
        assert_eq!(v6["mode"], serde_json::json!("aes-128-gcm"));
        assert!(
            v6.get("obfs_mode").is_none(),
            "v6 不走 obfs 腿 —— 若这条红了说明归一把版本分支也一起改坏了"
        );
        // 反向：显式 v4 时 obfs 生效、mode 不生效。
        let v4 = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"snell","address":"a.com","port":443,
                "password":"pw","snellSettings":{"version":4,"obfsMode":"http",
                    "mode":"aes-128-gcm"}}"#,
        );
        assert_eq!(v4["obfs_mode"], serde_json::json!("http"));
        assert!(v4.get("mode").is_none());
    }

    /// **非白名单协议拿到 `network != tcp` 时不得长出 `transport`** —— 传输层白名单的正面锁。
    ///
    /// 判据是内核 schema：20 支出站 oneOf 里只有 trojan/vless/vmess 有 `transport`。
    /// 这些形状**导入侧造得出来**（xray 的 `streamSettings` 可挂任意出站、clash 的 `network:` 同理），
    /// 而修前的黑名单 `!matches!(Hysteria2|Anytls|Naive)` 会照单放行 ⇒ 整份配置 FATAL。
    #[test]
    fn only_trojan_vless_vmess_may_carry_a_transport() {
        // 正向对照先行：白名单里的协议确实**要**长出 transport，否则下面全是 `is_none()` 的空对照。
        for proto in ["vless", "vmess", "trojan"] {
            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"{proto}","address":"a.com","port":443,
                    "uuid":"u-1","password":"p","network":"ws","wsSettings":{{"path":"/w"}}}}"#
            ));
            assert_eq!(
                v["transport"]["type"],
                serde_json::json!("ws"),
                "{proto} 是内核认的 transport 协议，丢了它等于把用户的传输层配置吞掉"
            );
        }
        // 内核 schema 里没有 transport 的那些：给了 network 也不许长出来。
        for (proto, extra) in [
            ("shadowsocks", r#""method":"aes-256-gcm","password":"p""#),
            ("socks", r#""username":"u","password":"p""#),
            ("http", r#""username":"u","password":"p""#),
            ("tuic", r#""uuid":"u-1","password":"p""#),
            ("snell", r#""password":"p""#),
            ("ssh", r#""username":"u","password":"p""#),
        ] {
            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"{proto}","address":"a.com","port":443,
                    {extra},"network":"ws","wsSettings":{{"path":"/w"}}}}"#
            ));
            assert!(
                v.get("transport").is_none(),
                "{proto} 出站长出了 transport ⇒ `FATAL decode config: outbounds[0].transport: \
                 json: unknown field \"transport\"`，整份配置起不来"
            );
        }
    }

    /// `HttpSettings` 的另两键 **`host` / `method` 在 http 协议下刻意不消费**。
    ///
    /// 判据不是「懒得做」：内核 http 出站 schema 里压根没有这两个键，写到顶层同样是
    /// `unknown field`（实测 rc=1）⇒ 建模/下发它们只会造出起不来的节点。
    /// 它们只在 h2 **传输**那条腿有意义（容器是 `transport`，schema 允许），由
    /// `h2_transport_reads_all_four_http_settings_keys` 那侧守着。
    ///
    /// 只断言 `method`，**不断言 `host`**：`singbox::Outbound` 上根本没有 `host` 字段
    /// （它只在 `Transport` 上，见 `singbox/outbound.rs`）⇒ `v.get("host").is_none()` 是**恒真**断言，
    /// 任何 builder 实现都无法让它红，写进来只会让这道门看起来比实际严。
    /// `method` 则相反：它在 `Outbound` 上真实存在（Shadowsocks 的加密方式），
    /// http 腿一旦借它来装 HTTP 方法就会被这条抓住。
    #[test]
    fn http_protocol_never_emits_method() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"http","address":"a.com","port":8080,
                "username":"u","password":"p","network":"http",
                "httpSettings":{"path":"/x","host":["decoy.com"],"method":"PUT",
                    "headers":{"X-A":["1"]}}}"#,
        );
        // 正向对照：同一份 httpSettings 里能下发的那两键确实下发了，证明分支真的跑到了。
        assert_eq!(v["path"], serde_json::json!("/x"));
        assert_eq!(v["headers"], serde_json::json!({"X-A": ["1"]}));
        assert!(
            v.get("method").is_none(),
            "内核 http 出站没有 method 键，下发即 FATAL；\
             `Outbound::method` 是 Shadowsocks 的加密方式，http 腿不得借用"
        );
        assert!(v.get("transport").is_none());
    }

    /// 没有 `httpSettings` 的 http 节点：两键都不出现（**不写空对象、不写 `path:"/"`**）。
    ///
    /// 这一条同时是金样零影响的依据 —— 金样里那个 http 用例的输入就没有 `httpSettings`
    /// （`fixtures/config-snapshot.json` 全文 `httpSettings` 出现 0 次），故本次修复不动它一个字节。
    #[test]
    fn http_without_settings_emits_neither_path_nor_headers() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"http","address":"a.com","port":8080,
                "username":"u","password":"p"}"#,
        );
        assert!(v.get("path").is_none(), "留空必须删键，不是 path:\"/\"");
        assert!(v.get("headers").is_none());
        assert!(v.get("transport").is_none());
    }

    /// **顶层 path/headers 是 http 协议独占**：h2 **传输**那条腿仍把四键装进 `transport`
    /// （容器不同，schema 各自合法），顶层必须干净。
    ///
    /// 少了这条，「把两键搬到顶层」很容易被误做成全协议通用 ⇒ vless+h2 会同时出现顶层与
    /// transport 两份，而 vless 出站的 schema 没有顶层 `path` ⇒ FATAL。
    #[test]
    fn h2_transport_leg_keeps_using_transport_and_leaves_top_level_clean() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","network":"http",
                "httpSettings":{"path":"/h2","host":["a.com"],"method":"PUT",
                    "headers":{"X-B":["2"]}}}"#,
        );
        assert_eq!(v["transport"]["type"], serde_json::json!("http"));
        assert_eq!(v["transport"]["path"], serde_json::json!("/h2"));
        assert_eq!(v["transport"]["method"], serde_json::json!("PUT"));
        assert_eq!(v["transport"]["host"], serde_json::json!("a.com"));
        assert!(
            v.get("path").is_none() && v.get("headers").is_none(),
            "非 http 协议的出站没有顶层 path/headers（vless schema 里不存在这两键）"
        );
    }

    /// `Hysteria2Settings.network` 真被消费（`ob.network = h.network.clone()`）—— 它此前被覆盖门
    /// 跨协议同名判据（snell 的 `{k:'network'}`）遮成「已覆盖」，债务表记的是零。
    #[test]
    fn hysteria2_network_reaches_outbound_json() {
        for want in ["tcp", "udp"] {
            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"hysteria2","address":"a.com","port":443,
                    "password":"pw","hysteria2Settings":{{"network":"{want}"}}}}"#
            ));
            assert_eq!(v["network"], serde_json::json!(want));
        }
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"hysteria2","address":"a.com","port":443,
                "password":"pw"}"#,
        );
        assert!(
            v.get("network").is_none(),
            "留空必须删键 = 内核缺省 tcp+udp 都走"
        );
    }

    /// 🔴 **naive 的 ECH 到得了内核** —— 批 C 把它记成债务时只推理到「`apply_anti_censorship_options`
    /// 在 `ech: None` 之后运行」，本条把那一步钉成断言：分支里写死的 `None` 确实被覆盖掉了。
    ///
    /// 内核侧同样实测过（随包核 beta.7）：naive 出站对 TLS 选项有一张**显式拒绝名单**
    /// （`… is not supported on naive outbound`：insecure / alpn / uTLS / fragment / reality /
    /// min_version / max_version / disable_sni / cipher_suites / curve_preferences /
    /// client_certificate / client_key / kernel TLS），**`ech` 不在名单里**；且喂一份坏 PEM 时
    /// naive 与 trojan 报同一句 `invalid ECH configs pem` ⇒ 走的是同一条 ECH 装配路径，不是被忽略。
    #[test]
    fn naive_ech_survives_the_branch_writing_none() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"naive","address":"a.com","port":443,
                "username":"u","password":"pw",
                "tlsSettings":{"serverName":"s.com","ech":true,"echConfig":"-----BEGIN ECH CONFIGS-----\nAAAA\n-----END ECH CONFIGS-----"}}"#,
        );
        assert_eq!(v["tls"]["ech"]["enabled"], serde_json::json!(true));
        assert_eq!(
            v["tls"]["ech"]["config"],
            serde_json::json!([
                "-----BEGIN ECH CONFIGS-----",
                "AAAA",
                "-----END ECH CONFIGS-----"
            ])
        );
        // 同一份 tlsSettings 里的其余键仍被 naive 分支挡掉（这才是「只补 ECH」的边界）。
        for k in ["alpn", "insecure", "utls", "engine", "spoof", "fragment"] {
            assert!(
                v["tls"].get(k).is_none(),
                "naive 分支必须继续挡掉 tls.{k}（随包核会点名 FATAL）"
            );
        }
    }

    /// naive 的拒绝名单在**本仓侧**的落点：分支把这几项写死 `None`，故它们进 `NODE_EXEMPT` 而非债务表。
    /// 名单哪天松动（Rust 改成透传），本断言先红 —— 豁免表的依据行就是指着这里。
    #[test]
    fn naive_tls_branch_pins_the_kernel_reject_list() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"naive","address":"a.com","port":443,
                "username":"u","password":"pw",
                "tlsSettings":{"serverName":"s.com","alpn":["h2"],"allowInsecure":true,
                               "fingerprint":"chrome","engine":"windows","fragment":true,
                               "spoofSni":"www.bing.com","spoofMethod":"wrong-ack"}}"#,
        );
        let keys: Vec<&str> = v["tls"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec!["enabled", "server_name"],
            "naive 的 TLS 块只许有这两个键 —— 其余项随包核 beta.7 会 `… is not supported on naive outbound` FATAL"
        );
    }

    /// `fragment` 的下发条件是**严格 `Some(true)`**（`tls_s.fragment == Some(true)`）⇒
    /// `None` 与 `Some(false)` 逐字节同结果，前端「关 = 删键、不写 false」由此而来。
    /// 键名是内核 `tls.fragment`（boolean），**不是 `record_fragment`**（本仓未建模的另一个键）。
    #[test]
    fn fragment_emits_only_on_explicit_true_for_the_five_tcp_tls_protocols() {
        for (proto, cred) in [
            ("vless", r#""uuid":"u-1""#),
            ("vmess", r#""uuid":"u-1""#),
            ("trojan", r#""password":"pw""#),
            ("anytls", r#""password":"pw""#),
            ("http", r#""username":"u""#),
        ] {
            let on = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"{proto}","address":"a.com","port":443,
                    {cred},"security":"tls","tlsSettings":{{"fragment":true}}}}"#
            ));
            assert_eq!(
                on["tls"]["fragment"],
                serde_json::json!(true),
                "{proto} 的 fragment 必须下发"
            );
            assert!(
                on["tls"].get("record_fragment").is_none(),
                "{proto}：本仓建模的是 tls.fragment，不得串到 record_fragment 上"
            );

            for off in ["false", "null"] {
                let v = outbound_json_from(&format!(
                    r#"{{"id":"s1","name":"n","protocol":"{proto}","address":"a.com","port":443,
                        {cred},"security":"tls","tlsSettings":{{"fragment":{off}}}}}"#
                ));
                assert!(
                    v["tls"].get("fragment").is_none(),
                    "{proto} fragment={off} 必须不下发该键（写 false 只是多一个语义等价的键）"
                );
            }
        }
    }

    /// fragment 在 **reality 下照常生效** ⇒ 前端 `fragment` 的门只叠一级、不叠 `!whenReality`。
    ///
    /// 原版这里还捎带断言「engine 被 reality 吞掉」，那句是**错误归因**：`outbound_json_from` 把
    /// platform 钉死成 `"linux"`，`engine:"windows"` 在这条路径上本来就被 `should_emit_tls_engine`
    /// 的平台门拦掉 —— 无论 reality 段吞不吞，此处都是 `None`，那条断言**永远分辨不出两者**。
    /// engine × reality 的真实关系由 `reality_branch_drops_tls_engine_but_keeps_spoof_and_ech`
    /// 与 `reality_drops_tls_engine_on_darwin_too` 两条在 win32/darwin 上分别守着（各带正向对照）。
    #[test]
    fn fragment_survives_the_reality_branch() {
        let v = outbound_json_from(
            r#"{"id":"s1","name":"n","protocol":"vless","address":"a.com","port":443,
                "uuid":"u-1","security":"reality",
                "realitySettings":{"publicKey":"pk"},
                "tlsSettings":{"fragment":true,"engine":"windows"}}"#,
        );
        assert_eq!(v["tls"]["reality"]["enabled"], serde_json::json!(true));
        assert_eq!(v["tls"]["fragment"], serde_json::json!(true));
    }

    /// QUIC 两协议：`fragment_unsupported` 挡在前面 ⇒ 填了也不下发（`NODE_EXEMPT` 的依据）。
    #[test]
    fn fragment_is_dropped_for_quic_managed_protocols() {
        for (proto, cred) in [
            ("hysteria2", r#""password":"pw""#),
            ("tuic", r#""uuid":"u-1","password":"pw""#),
        ] {
            let v = outbound_json_from(&format!(
                r#"{{"id":"s1","name":"n","protocol":"{proto}","address":"a.com","port":443,
                    {cred},"tlsSettings":{{"fragment":true}}}}"#
            ));
            assert!(
                v["tls"].get("fragment").is_none(),
                "{proto} 的 TLS 在 QUIC 内自管，fragment 永不下发 ⇒ 给控件即假控件"
            );
        }
    }
}
