//! sing-box JSON `outbounds[]` / `endpoints[]` → [`ServerConfig`] 解析
//! （上游 `SubscriptionService.parseSingboxOutbounds` + `makeCustomNode` 移植，纯逻辑）。
//!
//! 与 xray（[`crate::xray_import`]）的差异：sing-box outbound 用**扁平 `type`** + 同级字段
//! （`server`/`server_port`/`tls`/`transport`/...），xray 用 `protocol`+`settings`+`streamSettings`。
//! 二者共用 `outbounds` 键 → 靠 [`crate::xray_import::looks_like_xray`] 区分。
//!
//! 逐条处理，单条失败不影响其它（对齐 xray/clash 分支容错分层）。**不支持传输 / 缺 server·port**
//! 一律**整节点跳过**（不静默降级裸 TCP 产假节点，与分享链 #263 纪律一致）。**未建模的 type**
//! 按 [`ImportOrigin`] 分流：本机文件透传为 custom 逃生舱、远端订阅跳过。
//!
//! **边界归一（R4）协同**：`flow` / `vmessSecurity` / `fingerprint` / `network` 直接构造 `ServerConfig`
//! 时不经 serde `de_opt_token` 钩子 → 显式过 [`normalize_token`]（与 [`crate::xray_import`] 同口径），
//! 否则 `"Chrome"` / `"AES-128-GCM"` 大小写变体让 sing-box FATAL。`security` 走 [`SecurityMode`] 类型化归一。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use serde_json::Value;

use polaris_config_engine::builder::endpoint_routes::{has_catch_all, strip_catch_all};
use polaris_config_engine::user_config::normalize::normalize_token;
use polaris_config_engine::user_config::protocol_settings::{
    custom_outbound_type, AnyTlsSettings, CustomSettings, GrpcSettings, HttpSettings,
    Hysteria2ObfsSettings, Hysteria2Settings, HysteriaSettings, MultiplexSettings, NaiveSettings,
    OpenconnectSettings, OpenvpnClientSettings, OpenvpnTlsSettings, RealitySettings,
    ShadowsocksSettings, SnellSettings, SshSettings, TlsSettings, TorSettings, TuicSettings,
    WebSocketSettings,
};
use polaris_config_engine::user_config::server_config::{
    Protocol, SecurityMode, ServerConfig, WireGuardSettings,
};

use crate::clash_parser::ClashParseResult;

/// 导入来源 —— 决定**未建模 type 是否透传为 custom 逃生舱**。
///
/// # 为什么这是个类型而不是 `bool`
///
/// custom 逃生舱把**原始 outbound JSON 逐字下发内核**。这在本机文件上是特性（用户自己的配置，
/// 换 fork 内核就能用未建模协议），在**远端订阅**上是一条任意 JSON 注入通道 —— 实测（随包核
/// 1.14.0-beta.7，`sing-box check` rc=0）：
///
/// ```text
/// {"type":"tor","tag":"t","executable_path":"/bin/false","extra_args":["--x"],"data_directory":"/tmp/nope"}
/// ```
///
/// 即内核**按订阅下发的路径拉起任意本机可执行文件**。故两条腿的信任级不同，误把订阅当本机文件
/// 是提权级缺陷 —— 裸 `bool` 参数在调用点看不出方向（`parse_subscription(t, id, now, g, false)`），
/// 枚举让调用点自陈其信任级。
///
/// 上游 同口径：custom 透传只在 `parseLocalContent`（本机导入）里做，
/// `parseSingboxOutbounds` 走订阅时不做（`SubscriptionService.ts:1258-1265`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOrigin {
    /// 远端订阅正文：未建模 type **跳过**（不开任意 JSON 通道）。
    RemoteSubscription,
    /// 用户本机文件 / 粘贴内容：未建模 type 透传为 custom（上游 `makeCustomNode`）。
    LocalFile,
}

/// 可直接映射的 sing-box outbound `type`（对齐 上游 `SINGBOX_SUPPORTED_TYPES`）。
const SINGBOX_SUPPORTED_TYPES: &[&str] = &[
    // 2026-08-11：hysteria(v1) / tor 进建模协议后一并进本表。
    // tor 在 `map_singbox_outbound` 里有**前置分支**（无 server/port），不走公共守卫。
    "hysteria",
    "tor",
    "shadowsocks",
    "vless",
    "trojan",
    "hysteria2",
    "naive",
    "vmess",
    "tuic",
    "anytls",
    "snell",
    "socks",
    "http",
    "ssh",
];
/// sing-box transport.type → ServerConfig.network 可承载的传输（其余整节点跳过）。上游 `SINGBOX_SUPPORTED_TRANSPORTS`。
const SINGBOX_SUPPORTED_TRANSPORTS: &[&str] = &["ws", "grpc", "http", "httpupgrade"];
/// sing-box 非代理内部 outbound type（忽略，不计丢弃噪声）。上游 `SINGBOX_INTERNAL_TYPES`。
const SINGBOX_INTERNAL_TYPES: &[&str] = &["direct", "block", "dns", "selector", "urltest"];

// ── 标量规整（对齐 xray-import 的 str/num）────────────────────────────────────────

/// 字符串/数字/布尔 → `String`，其余 → `None`。
fn str_val(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// 空串归 `None`（上游 `str(v) || 'x'` 语义）。
fn str_ne(v: Option<&Value>) -> Option<String> {
    str_val(v).filter(|s| !s.is_empty())
}

/// 数字（整数值）/ 数字串 → `u32`。
fn num_val(v: Option<&Value>) -> Option<u32> {
    match v? {
        Value::Number(n) => n.as_u64().and_then(|x| u32::try_from(x).ok()).or_else(|| {
            n.as_f64().and_then(|f| {
                (f.fract() == 0.0 && f >= 0.0 && f <= f64::from(u32::MAX)).then_some(f as u32)
            })
        }),
        Value::String(s) => s.trim().parse::<u32>().ok(),
        _ => None,
    }
}

/// 端口规整（0/越界拒，防 `as u16` 静默截断）。
fn port_val(p: u32) -> Option<u16> {
    (1..=u32::from(u16::MAX)).contains(&p).then_some(p as u16)
}

fn bool_true(v: Option<&Value>) -> bool {
    v.and_then(Value::as_bool) == Some(true)
}

/// tuic heartbeat 时长规整（上游 `normalizeDuration`）：纯数字(毫秒)补 `ms`，否则透传；空/缺 → `None`。
/// 防 sing-box `ParseDuration "missing unit"`。
fn normalize_duration(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Number(n) => Some(format!("{n}ms")),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else if t.chars().all(|c| c.is_ascii_digit()) {
                Some(format!("{t}ms"))
            } else {
                Some(t.to_string())
            }
        }
        _ => None,
    }
}

/// 有效端口：`server_port` ?? `server_ports[0]` 低位（Hy2 端口跳跃无 server_port 时从范围首个推导）。
fn effective_port(ob: &Value) -> Option<u16> {
    if let Some(p) = num_val(ob.get("server_port")) {
        return port_val(p);
    }
    // server_ports: ["20000:30000", ...] → 首范围低位端口。
    let first = ob.get("server_ports")?.as_array()?.first()?.as_str()?;
    let low = first.split(':').next()?.trim().parse::<u32>().ok()?;
    port_val(low)
}

/// sing-box transport headers（`{k: v | [v..]}`）→ `{k: v}`（取字符串或数组首元素）。
fn transport_headers(v: Option<&Value>) -> Option<BTreeMap<String, String>> {
    let obj = v?.as_object()?;
    let mut m = BTreeMap::new();
    for (k, val) in obj {
        let s = match val {
            Value::Array(a) => a.first().and_then(|x| str_val(Some(x))),
            other => str_val(Some(other)),
        };
        if let Some(s) = s {
            m.insert(k.clone(), s);
        }
    }
    (!m.is_empty()).then_some(m)
}

fn host_header(host: String) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("Host".to_string(), host);
    m
}

// ── TLS / 传输 / 多路复用 ─────────────────────────────────────────────────────────

/// sing-box `tls.ech.config`（ECHConfigList）→ 归一多行字符串。1.14 schema 为字符串数组
/// （每行一段 PEM），亦容忍单个多行字符串；trim + 去空行后 `\n` 拼接，与导出侧
/// `apply_anti_censorship_options` 的 `lines()` split 对称（数组 ←→ 多行字符串 round-trip 闭合）。
fn ech_config_str(ech: Option<&Value>) -> Option<String> {
    let cfg = ech?.get("config")?;
    let raw: Vec<String> = match cfg {
        Value::Array(a) => a.iter().filter_map(|x| str_val(Some(x))).collect(),
        Value::String(s) => s.lines().map(str::to_string).collect(),
        _ => return None,
    };
    let joined = raw
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

/// TLS/Reality 层（上游：`ob.tls && ob.tls.enabled !== false`）。
fn apply_tls(server: &mut ServerConfig, tls: Option<&Value>) {
    let Some(tls) = tls.filter(|v| v.is_object()) else {
        return;
    };
    // enabled !== false（缺省视作开）。
    if tls.get("enabled").and_then(Value::as_bool) == Some(false) {
        return;
    }
    let reality = tls.get("reality");
    let has_reality = reality
        .map(|r| bool_true(r.get("enabled")))
        .unwrap_or(false)
        && str_ne(reality.and_then(|r| r.get("public_key"))).is_some();

    server.security = Some(if has_reality {
        SecurityMode::Reality
    } else {
        SecurityMode::Tls
    });
    let alpn = tls
        .get("alpn")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| str_val(Some(x))).collect());
    // ECH：enabled 决定开关；开启时一并读 config（ECHConfigList），与导出侧对称（生成什么解析回什么）。
    let ech = tls.get("ech");
    let ech_enabled = ech.map(|e| bool_true(e.get("enabled"))).unwrap_or(false);
    server.tls_settings = Some(TlsSettings {
        server_name: str_val(tls.get("server_name")),
        allow_insecure: Some(bool_true(tls.get("insecure"))),
        alpn,
        fingerprint: str_ne(tls.get("utls").and_then(|u| u.get("fingerprint")))
            .and_then(|f| normalize_token(&f)),
        ech: ech_enabled.then_some(true),
        ech_config: if ech_enabled {
            ech_config_str(ech)
        } else {
            None
        },
        fragment: bool_true(tls.get("fragment")).then_some(true),
        ..Default::default()
    });
    if has_reality {
        if let Some(r) = reality {
            server.reality_settings = Some(RealitySettings {
                public_key: str_val(r.get("public_key")).unwrap_or_default(),
                short_id: str_val(r.get("short_id")),
            });
        }
    }
}

/// 传输层（ws/grpc/http/httpupgrade）。调用前已过 [`SINGBOX_SUPPORTED_TRANSPORTS`] 闸。
fn apply_transport(server: &mut ServerConfig, transport: &Value) {
    let Some(t) = str_ne(transport.get("type")) else {
        return;
    };
    let host = str_ne(transport.get("host"));
    let headers = transport.get("headers");
    match t.as_str() {
        "ws" | "httpupgrade" => {
            server.network = Some(t.clone());
            server.ws_settings = Some(WebSocketSettings {
                path: str_val(transport.get("path")),
                headers: host.map(host_header).or_else(|| transport_headers(headers)),
                ..Default::default()
            });
        }
        "grpc" => {
            server.network = Some("grpc".to_string());
            server.grpc_settings = Some(GrpcSettings {
                service_name: str_val(transport.get("service_name")),
                ..Default::default()
            });
        }
        "http" => {
            server.network = Some("http".to_string());
            server.http_settings = Some(HttpSettings {
                path: str_val(transport.get("path")),
                ..Default::default()
            });
        }
        _ => {}
    }
}

/// Multiplex（vless/trojan/vmess/ss）。
fn apply_multiplex(server: &mut ServerConfig, mux: Option<&Value>) {
    let Some(mux) = mux.filter(|v| bool_true(v.get("enabled"))) else {
        return;
    };
    server.multiplex_settings = Some(MultiplexSettings {
        enabled: Some(true),
        protocol: Some(str_ne(mux.get("protocol")).unwrap_or_else(|| "h2mux".to_string())),
        max_connections: num_val(mux.get("max_connections")),
        min_streams: num_val(mux.get("min_streams")),
        padding: mux.get("padding").and_then(Value::as_bool),
    });
}

// ── 单 outbound → ServerConfig ───────────────────────────────────────────────────

fn new_server(
    id_gen: &mut impl FnMut() -> String,
    ob: &Value,
    protocol: Protocol,
    address: String,
    port: u16,
    sub_id: &str,
    now: &str,
) -> ServerConfig {
    let name = str_ne(ob.get("tag")).unwrap_or_else(|| format!("{address}:{port}"));
    let mut s = ServerConfig {
        id: id_gen(),
        name,
        protocol,
        address,
        port,
        subscription_id: Some(sub_id.to_string()),
        created_at: Some(now.to_string()),
        updated_at: Some(now.to_string()),
        ..Default::default()
    };
    // vless/vmess UDP 封装：显式携带时透传（缺省不写，由生成侧默认 xudp）。
    if ob.get("packet_encoding").is_some() {
        s.packet_encoding = str_val(ob.get("packet_encoding"));
    }
    s
}

/// 未建模 type 能否包成 custom 逃生舱 —— 形状判据**复用生成侧单一真值**
/// [`custom_outbound_type`]，额外只加一条 上游 就有的「type 非空白」。
///
/// # 为什么不在这里另写一份
///
/// 生成侧两条腿（`builder/outbound.rs` / `builder/outbounds.rs`）与「测试内核兼容性」按钮
/// （`commands/proxy.rs::validate_probe_outbound`）已统一到 [`custom_outbound_type`]。导入侧
/// 若自己判「有 type 就行」，`{"type": 42}` 这种就会**导得进、生成时被剔**（数字不是 string）
/// —— 第三份判据 = 第三种分叉。
///
/// # 为什么额外要求 type 非空白（这不是白名单）
///
/// 生成侧刻意放行 `{"type":""}`（“这个 type 内核认不认”是 `sing-box check` 的活，不在生成侧
/// 复刻协议白名单）。导入侧多这一条的理由不是协议判定，而是**落盘门**：
/// `store::validate::protocol_requirement_ok("custom")` 要求 `customSettings.outbound.type`
/// 非空 ⇒ 空 type 的节点造出来也会被 `sanitize_servers` 剔掉，用户只看见「导入了却没有」。
/// 上游 同口径（`if (typeof t !== 'string' || !t.trim()) continue;`）。
/// **仍然没有任何协议名白名单** —— 认不认那个 type 依旧由内核 probe 说了算。
fn custom_wrappable(ob: &Value) -> bool {
    custom_outbound_type(ob).is_some_and(|t| !t.trim().is_empty())
}

/// 未建模 type → custom 透传节点（上游 `makeCustomNode`，`SubscriptionService.ts:1398-1411`）。
/// 调用前须过 [`custom_wrappable`]。
///
/// **原始 JSON 逐字进 `customSettings.outbound`** —— 这是逃生舱的全部价值：换 fork 内核 /
/// 内核后续版本支持该 type 时，节点无需重导即可用；用户也能在节点弹窗里直接编辑那份 JSON。
/// 生成侧已改为 `#[serde(flatten)] extra` 真透传（`098b41e`），故这里保住的字段
/// （hy2-v1 的 `auth_str`、tor 的 `executable_path`…）是真能原样下发内核的，不再被窄 struct 吃掉。
///
/// 字段口径逐条对齐 上游：
/// - `name` = `tag`（trim 后非空）→ 否则 `type` → 否则 `"custom"`；
/// - `address` = `server`（**仅字符串**，非串按缺省空）；`port` = `server_port`（仅数字，否则 0）。
///   空 address / port 0 **不是坏数据**：`crates/store/src/sanitize.rs` 对 `custom` 显式豁免
///   address/port 校验，落点在 `customSettings.outbound` 里那份原文，顶层两字段只作列表展示。
///
/// `is_endpoint` = 该 type 属顶层 `endpoints[]`（openconnect / openvpn-*）而非 `outbounds[]`；
/// 生成侧据此决定把原文塞进哪个数组（`builder/outbounds.rs` 的 custom 两条腿）。
fn make_custom_node(
    ob: &Value,
    sub_id: &str,
    now: &str,
    id_gen: &mut impl FnMut() -> String,
    is_endpoint: bool,
) -> ServerConfig {
    // 已过 `custom_wrappable` ⇒ type 必是非空白 string。
    let ty = custom_outbound_type(ob)
        .unwrap_or_default()
        .trim()
        .to_string();
    let name = str_ne(ob.get("tag"))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(|| (!ty.is_empty()).then(|| ty.clone()))
        .unwrap_or_else(|| "custom".to_string());
    ServerConfig {
        id: id_gen(),
        name,
        protocol: Protocol::Custom,
        // 上游 `typeof ob.server === 'string' ? ob.server : ''`：数字 server 不当地址用
        // （与建模腿的 `str_ne` 有意不同 —— 这里只是展示用回显，真值在 outbound 原文里）。
        address: ob
            .get("server")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        port: ob
            .get("server_port")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(0),
        custom_settings: Some(CustomSettings {
            outbound: ob.clone(),
            is_endpoint: is_endpoint.then_some(true),
            secret_keys: None,
        }),
        subscription_id: Some(sub_id.to_string()),
        created_at: Some(now.to_string()),
        updated_at: Some(now.to_string()),
        ..Default::default()
    }
}

/// 单条 outbound 映射结果。
enum MapOutcome {
    Server(Box<ServerConfig>),
    /// 受支持 type 但字段缺失/配置非法（计 failed）。
    Fail,
    /// 不支持传输（计 skipped，带 transport 名）。
    SkipTransport(String),
}

/// 单条 sing-box outbound → [`ServerConfig`]（已确认 type 受支持）。
fn map_singbox_outbound(
    ob: &Value,
    ty: &str,
    sub_id: &str,
    now: &str,
    id_gen: &mut impl FnMut() -> String,
    origin: ImportOrigin,
) -> MapOutcome {
    // ── tor：**无地址协议**，必须排在下面的 server/port 前置守卫之前（2026-08-11）──
    // 实测给内核传 `server` 得 `outbounds[0].server: json: unknown field "server"`，
    // 故它天生没有这两个键，走公共前置只会被判 Fail 而静默丢掉整个节点。
    // address/port 落空由 `store::sanitize` 的 `addressless` 豁免接住（tailscale 同族）。
    if ty == "tor" {
        let mut s = new_server(id_gen, ob, Protocol::Tor, String::new(), 0, sub_id, now);
        const MODELED_TOR: &[&str] = &[
            "type",
            "tag",
            "executable_path",
            "data_directory",
            "extra_args",
            "torrc",
            "domain_resolver",
            "detour",
        ];
        let mut extra = serde_json::Map::new();
        if let Some(obj) = ob.as_object() {
            for (k, v) in obj {
                if !MODELED_TOR.contains(&k.as_str()) {
                    extra.insert(k.clone(), v.clone());
                }
            }
        }
        s.tor_settings = Some(TorSettings {
            executable_path: str_ne(ob.get("executable_path")),
            data_directory: str_ne(ob.get("data_directory")),
            extra_args: str_array(ob.get("extra_args")).unwrap_or_default(),
            torrc: ob
                .get("torrc")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| str_val(Some(v)).map(|s| (k.clone(), s)))
                        .collect()
                })
                .unwrap_or_default(),
            extra,
        });
        return MapOutcome::Server(Box::new(s));
    }

    // 公共前置：server + 有效端口。
    let (Some(server_addr), Some(port)) = (str_ne(ob.get("server")), effective_port(ob)) else {
        return MapOutcome::Fail;
    };
    // 传输闸：不支持的 transport（quic 等）整节点跳过（防裸 TCP 假节点）。
    if let Some(tt) = str_ne(ob.get("transport").and_then(|t| t.get("type"))) {
        if !SINGBOX_SUPPORTED_TRANSPORTS.contains(&tt.as_str()) {
            return MapOutcome::SkipTransport(tt);
        }
    }

    let protocol = match ty {
        "shadowsocks" => Protocol::Shadowsocks,
        "vless" => Protocol::Vless,
        "trojan" => Protocol::Trojan,
        "hysteria2" => Protocol::Hysteria2,
        "hysteria" => Protocol::Hysteria,
        "naive" => Protocol::Naive,
        "vmess" => Protocol::Vmess,
        "tuic" => Protocol::Tuic,
        "anytls" => Protocol::Anytls,
        "snell" => Protocol::Snell,
        "socks" => Protocol::Socks,
        "http" => Protocol::Http,
        "ssh" => Protocol::Ssh,
        _ => return MapOutcome::Fail,
    };
    let mut s = new_server(id_gen, ob, protocol, server_addr, port, sub_id, now);

    // 通用 TLS / 传输 / 多路复用（ssh 显式覆盖 security/network，见下）。
    apply_tls(&mut s, ob.get("tls"));
    if let Some(transport) = ob.get("transport").filter(|v| v.is_object()) {
        apply_transport(&mut s, transport);
    }
    apply_multiplex(&mut s, ob.get("multiplex"));

    // Protocol-specific。
    match ty {
        "shadowsocks" => {
            s.shadowsocks_settings = Some(ShadowsocksSettings {
                method: str_ne(ob.get("method")).unwrap_or_else(|| "aes-256-gcm".to_string()),
                password: str_val(ob.get("password")).unwrap_or_default(),
                plugin: str_ne(ob.get("plugin")),
                plugin_opts: str_ne(ob.get("plugin_opts")),
            });
        }
        "vless" => {
            s.uuid = Some(str_val(ob.get("uuid")).unwrap_or_default());
            s.flow = str_ne(ob.get("flow")).and_then(|f| normalize_token(&f));
        }
        "trojan" => {
            s.password = Some(str_val(ob.get("password")).unwrap_or_default());
        }
        // Hysteria **v1**（2026-08-11）：与 hy2 同名不同义的两处必须各写各的 ——
        // obfs 是裸口令串（不是 {type,password} 对象）、认证走 auth_str（不是 password）。
        //
        // 未建模的键**原样进透传袋**：表单是精选子集，没有袋子时「导入 → 编辑 → 保存」
        // 会把 recv_window 之类静默丢掉，配置从能连变成连不上且无提示。
        "hysteria" => {
            s.security = Some(SecurityMode::Tls); // v1 恒 TLS（后端 TLS_PROTOCOLS 含它）
            let mut hy = HysteriaSettings {
                auth_str: str_ne(ob.get("auth_str")),
                auth: str_ne(ob.get("auth")),
                up_mbps: num_val(ob.get("up_mbps")),
                down_mbps: num_val(ob.get("down_mbps")),
                obfs: str_ne(ob.get("obfs")),
                hop_interval: str_ne(ob.get("hop_interval")),
                ..Default::default()
            };
            hy.server_ports = ob
                .get("server_ports")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| str_ne(Some(v)));
            // 透传袋 = 原文减去「本结构建模过的键」与「已落在 ServerConfig 别处的公共键」。
            //
            // 🔴 **只对本地文件填充**（2026-08-11）：袋子把原文里**任意**键带进下发配置，
            // 而 sing-box 对未知字段是 **decode 阶段拒收** ⇒ 远端订阅塞一个乱键就能让**整个核起不来**，
            // 不止坏掉那个节点。袋子的语义是「用户自己的文件，原样保全」；远端输入只收我们看得懂的字段。
            // 与 tor/openconnect 的「命令执行向量只许本地文件」同一条信任判据，不是两套规则。
            const MODELED: &[&str] = &[
                "type",
                "tag",
                "server",
                "server_port",
                "auth_str",
                "auth",
                "up_mbps",
                "down_mbps",
                "obfs",
                "server_ports",
                "hop_interval",
                "tls",
                "detour",
            ];
            if origin == ImportOrigin::LocalFile {
                if let Some(map) = ob.as_object() {
                    for (k, v) in map {
                        if !MODELED.contains(&k.as_str()) {
                            hy.extra.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            s.hysteria_settings = Some(hy);
            apply_tls(&mut s, ob.get("tls"));
        }
        "hysteria2" => {
            s.password = Some(str_val(ob.get("password")).unwrap_or_default());
            s.security = Some(SecurityMode::Tls);
            let mut hy2 = Hysteria2Settings::default();
            // obfs：salamander（type+password）/ gecko（+min/max_packet_size 随机填充，仅 gecko）。
            // 与导出侧 buildOutbound 对称：需 type+password 双备；未知 type 不设（graceful #263）。
            let obfs = ob.get("obfs");
            if let (Some(ty), Some(pw)) = (
                str_ne(obfs.and_then(|o| o.get("type"))),
                str_ne(obfs.and_then(|o| o.get("password"))),
            ) {
                match ty.as_str() {
                    "salamander" => {
                        hy2.obfs = Some(Hysteria2ObfsSettings {
                            type_field: Some("salamander".to_string()),
                            password: Some(pw),
                            ..Default::default()
                        });
                    }
                    "gecko" => {
                        hy2.obfs = Some(Hysteria2ObfsSettings {
                            type_field: Some("gecko".to_string()),
                            password: Some(pw),
                            min_packet_size: num_val(obfs.and_then(|o| o.get("min_packet_size"))),
                            max_packet_size: num_val(obfs.and_then(|o| o.get("max_packet_size"))),
                        });
                    }
                    _ => {}
                }
            }
            // bbr_profile（1.14）：仅 standard/aggressive/conservative 合法，空/未知不设（对齐导出侧枚举域）。
            if let Some(bp) = str_ne(ob.get("bbr_profile")) {
                if matches!(bp.as_str(), "standard" | "aggressive" | "conservative") {
                    hy2.bbr_profile = Some(bp);
                }
            }
            if let Some(ports) = ob.get("server_ports").and_then(Value::as_array) {
                if !ports.is_empty() {
                    let joined = ports
                        .iter()
                        .filter_map(|x| str_val(Some(x)))
                        .collect::<Vec<_>>()
                        .join(",");
                    hy2.server_ports = Some(joined);
                    hy2.hop_interval = str_ne(ob.get("hop_interval"));
                }
            }
            if hy2 != Hysteria2Settings::default() {
                s.hysteria2_settings = Some(hy2);
            }
        }
        "naive" => {
            s.username = Some(str_val(ob.get("username")).unwrap_or_default());
            s.password = Some(str_val(ob.get("password")).unwrap_or_default());
            // quic:true → HTTP/3 传输。
            if bool_true(ob.get("quic")) {
                s.naive_settings = Some(NaiveSettings {
                    use_http3: Some(true),
                });
            }
        }
        "vmess" => {
            s.uuid = Some(str_val(ob.get("uuid")).unwrap_or_default());
            s.alter_id = num_val(ob.get("alter_id"));
            s.vmess_security = str_ne(ob.get("security")).and_then(|v| normalize_token(&v));
        }
        "tuic" => {
            s.uuid = Some(str_val(ob.get("uuid")).unwrap_or_default());
            s.password = Some(str_val(ob.get("password")).unwrap_or_default());
            let ts = TuicSettings {
                congestion_control: str_ne(ob.get("congestion_control")),
                udp_relay_mode: str_ne(ob.get("udp_relay_mode")),
                zero_rtt_handshake: ob.get("zero_rtt_handshake").and_then(Value::as_bool),
                heartbeat: normalize_duration(ob.get("heartbeat")),
            };
            if ts != TuicSettings::default() {
                s.tuic_settings = Some(ts);
            }
        }
        "anytls" => {
            s.password = Some(str_val(ob.get("password")).unwrap_or_default());
            let a = AnyTlsSettings {
                idle_session_check_interval: str_ne(ob.get("idle_session_check_interval")),
                idle_session_timeout: str_ne(ob.get("idle_session_timeout")),
                min_idle_session: num_val(ob.get("min_idle_session")),
            };
            if a != AnyTlsSettings::default() {
                s.any_tls_settings = Some(a);
            }
        }
        "socks" => {
            s.username = str_ne(ob.get("username"));
            s.password = str_ne(ob.get("password"));
        }
        "http" => {
            s.username = str_ne(ob.get("username"));
            s.password = str_ne(ob.get("password"));
        }
        "ssh" => {
            // ssh：固定 tcp/none（private_key_path 是本机路径、跨设备订阅无意义，刻意不映射）。
            s.network = Some("tcp".to_string());
            s.security = Some(SecurityMode::None);
            let ssh = SshSettings {
                user: str_ne(ob.get("user")),
                password: str_ne(ob.get("password")),
                private_key: str_ne(ob.get("private_key")),
                private_key_passphrase: str_ne(ob.get("private_key_passphrase")),
                host_key: str_array(ob.get("host_key")),
                host_key_algorithms: str_array(ob.get("host_key_algorithms")),
                client_version: str_ne(ob.get("client_version")),
                cipher: str_array(ob.get("cipher")),
                mac: str_array(ob.get("mac")),
                kex_algorithm: str_array(ob.get("kex_algorithm")),
                ..Default::default()
            };
            if ssh != SshSettings::default() {
                s.ssh_settings = Some(ssh);
            }
        }
        "snell" => {
            // 官方 snell（1.14.0-alpha.38+）：version 仅 4/6；psk 复用 password 落点。
            let version = num_val(ob.get("version"));
            if version != Some(4) && version != Some(6) {
                return MapOutcome::Fail; // 版本不受支持 → 跳过（validateConfig 会拒坏节点连累整份订阅）
            }
            let Some(psk) = str_ne(ob.get("psk")) else {
                return MapOutcome::Fail; // 缺 psk
            };
            let version = version.unwrap();
            let mut snell = SnellSettings {
                version,
                ..Default::default()
            };
            // obfs 仅 v4。
            if version == 4 && str_ne(ob.get("obfs_mode")).as_deref() == Some("http") {
                snell.obfs_mode = Some("http".to_string());
                snell.obfs_host = str_ne(ob.get("obfs_host"));
            }
            if version == 6 {
                if let Some(m) = str_ne(ob.get("mode")) {
                    if m == "unshaped" || m == "unsafe-raw" {
                        snell.mode = Some(m);
                    }
                }
            }
            if bool_true(ob.get("reuse")) {
                snell.reuse = Some(true);
            }
            if let Some(net) = str_ne(ob.get("network")) {
                if net == "tcp" || net == "udp" {
                    snell.network = Some(net);
                }
            }
            snell.userkey = str_ne(ob.get("userkey"));
            s.password = Some(psk);
            s.snell_settings = Some(snell);
        }
        _ => return MapOutcome::Fail,
    }

    MapOutcome::Server(Box::new(s))
}

/// 字符串数组 → `Vec<String>`（空数组 → None，对齐 上游 `x.length > 0`）。
fn str_array(v: Option<&Value>) -> Option<Vec<String>> {
    let arr = v?.as_array()?;
    let out: Vec<String> = arr.iter().filter_map(|x| str_val(Some(x))).collect();
    (!out.is_empty()).then_some(out)
}

// ── 入口 ─────────────────────────────────────────────────────────────────────────

/// sing-box `outbounds[]` → 节点集 + 统计（上游 `parseSingboxOutbounds` + `parseLocalContent`
/// 的 custom 透传腿合并）。
///
/// 逐条：`SINGBOX_INTERNAL_TYPES` 忽略；不支持 transport → skipped；缺 server/port / 配置非法 →
/// failed；**未建模 type** 按 `origin` 分流 —— [`ImportOrigin::LocalFile`] 且过 [`custom_wrappable`]
/// 则透传为 custom（计入 `servers`），[`ImportOrigin::RemoteSubscription`] → skipped。
/// `type` 空 / 缺 / 非字符串 **两条腿都 skipped**（理由见 [`custom_wrappable`]）。
///
/// `sub_id` 挂到每个节点（本地导入传空串，命令层剥离）。
pub fn parse_singbox_outbounds(
    outbounds: &Value,
    sub_id: &str,
    now: &str,
    id_gen: &mut impl FnMut() -> String,
    origin: ImportOrigin,
) -> ClashParseResult {
    let mut r = ClashParseResult::default();
    let Some(arr) = outbounds.as_array() else {
        return r;
    };

    let mut skip_by_type: Vec<(String, usize)> = Vec::new();
    let mut skip_by_transport: Vec<(String, usize)> = Vec::new();
    let mut missing_fields = 0usize;

    for ob in arr {
        if !ob.is_object() {
            r.failed += 1;
            continue;
        }
        let ty = str_val(ob.get("type"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        // ── 信任级分流：命令执行向量类协议**只许本地文件**（2026-08-11）──
        //
        // `tor` 收 `executable_path` / `extra_args` ⇒ 能造出「起任意本机程序」的配置。
        // 该向量此前由「远端订阅绝不产 custom」那条闸挡住（见
        // `remote_subscription_never_wraps_custom`）；把 tor 改成**建模协议**会让向量
        // 从 custom 路径**转移到建模路径**，绕过原闸 —— 这不是新洞，是同一个洞换了条路。
        // 故在此显式复用同一条信任判据，而不是依赖「它现在是建模协议了」。
        if ty == "tor" && origin != ImportOrigin::LocalFile {
            bump(&mut skip_by_type, ty.clone());
            r.skipped += 1;
            continue;
        }
        if !SINGBOX_SUPPORTED_TYPES.contains(&ty.as_str()) {
            // direct/block/selector 等内部 outbound 不计噪声。
            if SINGBOX_INTERNAL_TYPES.contains(&ty.as_str()) {
                continue;
            }
            // 本机文件 + 形状合法 → custom 逃生舱（原文逐字保留、可编辑、换核即用）。
            // 形状判据与生成侧同源，见 [`custom_wrappable`]。
            if origin == ImportOrigin::LocalFile && custom_wrappable(ob) {
                r.servers
                    .push(make_custom_node(ob, sub_id, now, id_gen, false));
                continue;
            }
            let key = if ty.is_empty() {
                "(empty)".to_string()
            } else {
                ty.clone()
            };
            bump(&mut skip_by_type, key);
            r.skipped += 1;
            continue;
        }
        match map_singbox_outbound(ob, &ty, sub_id, now, id_gen, origin) {
            MapOutcome::Server(s) => r.servers.push(*s),
            MapOutcome::Fail => {
                missing_fields += 1;
                r.failed += 1;
            }
            MapOutcome::SkipTransport(tt) => {
                bump(&mut skip_by_transport, tt);
                r.skipped += 1;
            }
        }
    }

    if !skip_by_type.is_empty() {
        r.warnings.push(format!(
            "跳过不支持的 outbound 类型: {}",
            fmt_counts(&skip_by_type)
        ));
    }
    if !skip_by_transport.is_empty() {
        r.warnings.push(format!(
            "跳过不支持的传输层类型: {}",
            fmt_counts(&skip_by_transport)
        ));
    }
    if missing_fields > 0 {
        r.warnings.push(format!(
            "跳过 {missing_fields} 个缺 server/port 或配置非法的 outbound"
        ));
    }
    r.finish()
}

// ── endpoints[] ─────────────────────────────────────────────────────────────────

/// sing-box `endpoints[]` → 节点集 + 统计。
///
/// # 内核 type 域（实测，随包核 `resources/linux/sing-box` = 1.14.0-beta.7）
///
/// `sing-box check` 对 `endpoints[0].type` 逐个探得**恰五种**可解码：`wireguard` / `tailscale` /
/// `openconnect` / `openvpn-client` / `openvpn-server`（余者 `unknown endpoint type: <t>`），
/// 与 `outbounds[]` 的 type 域**不相交**（`wireguard` 作 outbound 已于 1.13 移除、`tailscale`
/// 作 outbound 即 unknown）。故本函数与 [`parse_singbox_outbounds`] 各管一个数组，无重复计数。
/// Polaris 建模其中两种（[`Protocol::Wireguard`] / `Tailscale`），另三种无落点 → custom 逃生舱。
///
/// # 分流
///
/// - `wireguard` → [`Protocol::Wireguard`] 建模映射（见 [`map_wireguard_endpoint`]）。
/// - `tailscale` → **恒 skipped**（不建模、也不透传 custom），四条理由见函数体内该 match 臂的注释。
/// - 其余三种 + 未知 type → 按 `origin` 走 custom 逃生舱（`isEndpoint = true`）/ skipped，
///   与 [`parse_singbox_outbounds`] 同一条信任级判据。
pub fn parse_singbox_endpoints(
    endpoints: &Value,
    sub_id: &str,
    now: &str,
    id_gen: &mut impl FnMut() -> String,
    origin: ImportOrigin,
) -> ClashParseResult {
    let mut r = ClashParseResult::default();
    let Some(arr) = endpoints.as_array() else {
        return r;
    };

    let mut skip_by_type: Vec<(String, usize)> = Vec::new();
    let mut tailscale_skipped = 0usize;
    let mut missing_fields = 0usize;

    for ep in arr {
        if !ep.is_object() {
            r.failed += 1;
            continue;
        }
        let ty = str_val(ep.get("type"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ty.as_str() {
            "wireguard" => match map_wireguard_endpoint(ep, sub_id, now, id_gen) {
                Some(s) => r.servers.push(s),
                None => {
                    missing_fields += 1;
                    r.failed += 1;
                }
            },
            // ── tailscale endpoint 恒不导入（判断依据，勿散落）─────────────────────────
            // 1. **凭据归属反转**：该 endpoint 的唯一实质内容是 `auth_key`（tailnet 预授权密钥）。
            //    导入它 = 把**本机**加进**下发方的 tailnet**（对方可见本机、可路由到本机）。
            //    节点列表不是这个意图的载体。
            // 2. **不可移植**：`state_directory` 由 Polaris 生成时注入本机路径
            //    （`builder/endpoints.rs` 的 `build_tailscale_endpoint`），文件里那份对本机无意义。
            // 3. **单例硬限**：Polaris 全局只许一个 Tailscale 节点（`store/src/sanitize.rs` 的
            //    `first_tailscale` + 前端 `tailscaleSlotTaken`）。批量导入至多贡献 1 个，
            //    且会与用户自己那个抢槽。
            // 4. **实测无内容可导**：`{"type":"tailscale","tag":"x"}`（零字段）`sing-box check`
            //    rc=0 —— 没有任何必填的、可移植的、非凭据字段。
            // **也不走 custom 逃生舱**：custom 节点 protocol 是 `custom`，前端单例闸门
            // `meshSingletonConflict` 只认 `protocol === 'tailscale'`
            // （`ui/src/domain/endpoint-routes.ts`）⇒ 包成 custom 等于替下发方绕过单例闸门，比跳过更坏。
            "tailscale" => {
                tailscale_skipped += 1;
                r.skipped += 1;
            }
            // ── 端点族 VPN 客户端（2026-08-11）──
            // 与 tailscale 相反：它们的凭据归属**不反转**（是「本机连出去」而不是「把本机加进
            // 对方的网」），也没有单例限制，故正常导入。
            // 未建模的键原样进透传袋 —— 表单是精选子集（openconnect 61 键 / openvpn 78 键，
            // 多数是调优旋钮），没有袋子时「导入 → 编辑 → 保存」会静默丢掉它们。
            // 同一条信任判据：openconnect 的 `csd`（Cisco CSD 脚本）/ `tncc`（Juniper）是
            // **执行外部脚本**的键，透传袋会把它们从远端订阅原样带进下发配置 ⇒ 与 tor 同族的
            // 命令执行向量。故只许本地文件；远端订阅仍按「不支持类型」跳过并告警。
            "openconnect" | "openvpn-client" if origin != ImportOrigin::LocalFile => {
                bump(&mut skip_by_type, ty.clone());
                r.skipped += 1;
            }
            "openconnect" | "openvpn-client" => {
                match map_endpoint_vpn_client(&ty, ep, sub_id, now, id_gen) {
                    Some(s) => r.servers.push(s),
                    None => {
                        missing_fields += 1;
                        r.failed += 1;
                    }
                }
            }
            _ => {
                if origin == ImportOrigin::LocalFile && custom_wrappable(ep) {
                    r.servers
                        .push(make_custom_node(ep, sub_id, now, id_gen, true));
                    continue;
                }
                let key = if ty.is_empty() {
                    "(empty)".to_string()
                } else {
                    ty.clone()
                };
                bump(&mut skip_by_type, key);
                r.skipped += 1;
            }
        }
    }

    if tailscale_skipped > 0 {
        r.warnings.push(format!(
            "跳过 {tailscale_skipped} 个 tailscale endpoint：账号制组网需本机登录，\
             订阅/配置文件里的 auth_key 属他人 tailnet 凭据，不导入"
        ));
    }
    if !skip_by_type.is_empty() {
        r.warnings.push(format!(
            "跳过不支持的 endpoint 类型: {}",
            fmt_counts(&skip_by_type)
        ));
    }
    if missing_fields > 0 {
        r.warnings.push(format!(
            "跳过 {missing_fields} 个缺 private_key / peers 必填字段的 wireguard endpoint"
        ));
    }
    r.finish()
}

/// `endpoints[].{type:"wireguard"}` → [`Protocol::Wireguard`] 节点。必填缺失 → `None`（计 failed）。
///
/// # 字段对位（逆向 [`build_wireguard_endpoint`]，单 peer 模型）
///
/// | sing-box endpoint | ServerConfig |
/// |---|---|
/// | `tag` | `name`（缺省 `addr:port`，与 [`new_server`] 同口径） |
/// | `peers[0].address` / `.port` | `address` / `port` |
/// | `private_key` | `wireguardSettings.privateKey` |
/// | `address[]` | `wireguardSettings.localAddress` |
/// | `peers[0].public_key` | `wireguardSettings.peerPublicKey` |
/// | `peers[0].pre_shared_key` | `wireguardSettings.preSharedKey` |
/// | `peers[0].allowed_ips` | catch-all → `allowInternet`；具体段 → `allowedIPs` |
/// | `peers[0].persistent_keepalive_interval` | `persistentKeepalive`（>0 才写） |
/// | `peers[0].reserved` | `reserved`（**恰 3 项**才写，与生成侧 `s.reserved.len() == 3` 对称） |
/// | `mtu` | `mtu`（>0 才写） |
///
/// `allowed_ips` 的拆分口径与粘贴 wg-quick `.conf` 那条腿逐字同源
/// （`ui/src/components/dialogs/wg-logic.ts#draftFromParsed`）：全网段是「全隧道意图」、由
/// `allowInternet` 承载，`allowedIPs` 只留具体段 —— 生成侧 `wireguard_peer_allowed_ips` 会按
/// `allowInternet` 把 0/0 加回去，不这样拆就会双份。
///
/// # 刻意不映射（非遗漏）
///
/// - **`system` → `reverseMesh`**：`system:true` 要抢内核 utun（需提权、与主 TUN 冲突，
///   `builder/endpoint_routes.rs:112-128` 记有 WARP 恒否决与 `resource busy` FATAL 实证）。
///   外部文件不该能翻这个开关；且 wg-quick 导入腿同样恒 false（`wg-logic.ts:170`）。
///   丢它**不产假节点**：用户态 gVisor 就是客户端常态，只是不可被反向接入。
/// - **`listen_port` / `udp_timeout` / `workers` / `name`**：`WireGuardSettings` 无落点。
/// - **`detour`**：`ServerConfig.detour` 存的是**本地节点 id**，外部 tag 无从解析。
///
/// [`build_wireguard_endpoint`]: polaris_config_engine::builder::endpoints::build_wireguard_endpoint
/// `openconnect` / `openvpn-client` endpoint → [`ServerConfig`]。
///
/// 载荷形态与生成侧**对称**：生成时把设置结构整体序列化 flatten 进 endpoint，
/// 故导入时反过来 —— 建模键各归各位，其余原样进透传袋。
/// 两侧共用同一份「哪些键建模了」的清单（下面的 `MODELED_*`），复制第二份必然漂移。
///
/// 地址/端口：openconnect 的 `server` 是 `host:port` **单串**，openvpn 才有独立的 `server_port`。
/// `ServerConfig` 的 address/port 是落盘门 `sanitize_servers` 的必填项，故从各自形态里拆出来。
fn map_endpoint_vpn_client(
    ty: &str,
    ep: &Value,
    sub_id: &str,
    now: &str,
    id_gen: &mut impl FnMut() -> String,
) -> Option<ServerConfig> {
    const MODELED_OC: &[&str] = &[
        "type",
        "tag",
        "server",
        "username",
        "password",
        "flavor",
        "auth_group",
        "token",
        "mtu",
        "no_udp",
        "pfs",
        "allow_insecure_crypto",
        "user_agent",
        "reported_os",
        "system",
        "domain_resolver",
        "detour",
    ];
    const MODELED_OV: &[&str] = &[
        "type",
        "tag",
        "server",
        "server_port",
        "username",
        "password",
        "network",
        "cipher",
        "auth",
        "mtu",
        "redirect_gateway",
        "system",
        "tls",
        "domain_resolver",
        "detour",
    ];
    let bag = |modeled: &[&str]| -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        if let Some(obj) = ep.as_object() {
            for (k, v) in obj {
                if !modeled.contains(&k.as_str()) {
                    m.insert(k.clone(), v.clone());
                }
            }
        }
        m
    };

    let raw_server = str_ne(ep.get("server"))?;
    let (addr, port) = if ty == "openconnect" {
        // `host:port` 单串：末个冒号后是端口则拆开，否则默认 443。
        match raw_server.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && p.parse::<u16>().is_ok() => {
                (h.to_string(), p.parse::<u16>().ok()?)
            }
            _ => (raw_server.clone(), 443u16),
        }
    } else {
        (
            raw_server.clone(),
            port_val(num_val(ep.get("server_port"))?)?,
        )
    };

    let protocol = if ty == "openconnect" {
        Protocol::Openconnect
    } else {
        Protocol::OpenvpnClient
    };
    let mut s = new_server(id_gen, ep, protocol, addr, port, sub_id, now);

    if ty == "openconnect" {
        s.openconnect_settings = Some(OpenconnectSettings {
            server: Some(raw_server),
            username: str_ne(ep.get("username")),
            password: str_ne(ep.get("password")),
            flavor: str_ne(ep.get("flavor")),
            auth_group: str_ne(ep.get("auth_group")),
            token: str_ne(ep.get("token")),
            mtu: num_val(ep.get("mtu")),
            no_udp: ep.get("no_udp").map(|v| bool_true(Some(v))),
            pfs: ep.get("pfs").map(|v| bool_true(Some(v))),
            allow_insecure_crypto: ep.get("allow_insecure_crypto").map(|v| bool_true(Some(v))),
            user_agent: str_ne(ep.get("user_agent")),
            reported_os: str_ne(ep.get("reported_os")),
            system: ep.get("system").map(|v| bool_true(Some(v))),
            extra: bag(MODELED_OC),
        });
    } else {
        let pem = |k: &str| -> Vec<String> {
            ep.get("tls")
                .and_then(|t| t.get(k))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| str_val(Some(x))).collect())
                .unwrap_or_default()
        };
        s.openvpn_client_settings = Some(OpenvpnClientSettings {
            server: Some(raw_server),
            server_port: Some(port),
            username: str_ne(ep.get("username")),
            password: str_ne(ep.get("password")),
            network: str_ne(ep.get("network")),
            cipher: str_ne(ep.get("cipher")),
            auth: str_ne(ep.get("auth")),
            mtu: num_val(ep.get("mtu")),
            redirect_gateway: ep.get("redirect_gateway").map(|v| bool_true(Some(v))),
            system: ep.get("system").map(|v| bool_true(Some(v))),
            tls: Some(OpenvpnTlsSettings {
                certificate: pem("certificate"),
                client_certificate: pem("client_certificate"),
                client_key: pem("client_key"),
                // 嵌套袋：tls 下未建模的子键（peer_fingerprint / server_name / version_* …）
                extra: ep
                    .get("tls")
                    .and_then(Value::as_object)
                    .map(|m| {
                        m.iter()
                            .filter(|(k, _)| {
                                !matches!(
                                    k.as_str(),
                                    "certificate" | "client_certificate" | "client_key"
                                )
                            })
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            extra: bag(MODELED_OV),
        });
    }
    Some(s)
}

fn map_wireguard_endpoint(
    ep: &Value,
    sub_id: &str,
    now: &str,
    id_gen: &mut impl FnMut() -> String,
) -> Option<ServerConfig> {
    let peer = ep.get("peers")?.as_array()?.first()?;
    // 必填：落盘门 `validate::protocol_requirement_ok("wireguard")` 要 privateKey + peerPublicKey
    // + 非空 localAddress；`sanitize_servers` 另要非空 address + port∈1..=65535。缺任一造出来也会
    // 被剔除 —— 与其静默入库再消失，不如此处计 failed 并聚合告警。
    let private_key = str_ne(ep.get("private_key"))?;
    let peer_public_key = str_ne(peer.get("public_key"))?;
    let local_address = str_array(ep.get("address"))?;
    let server_addr = str_ne(peer.get("address"))?;
    let port = port_val(num_val(peer.get("port"))?)?;

    let allowed = str_array(peer.get("allowed_ips")).unwrap_or_default();
    let specific = strip_catch_all(&allowed);
    let mut wg = WireGuardSettings {
        private_key: Some(private_key),
        local_address,
        peer_public_key: Some(peer_public_key),
        pre_shared_key: str_ne(peer.get("pre_shared_key")),
        allowed_ips: specific,
        allow_internet: Some(has_catch_all(&allowed)),
        ..Default::default()
    };
    if let Some(k) = num_val(peer.get("persistent_keepalive_interval")).filter(|k| *k > 0) {
        wg.persistent_keepalive = Some(k);
    }
    if let Some(m) = num_val(ep.get("mtu")).filter(|m| *m > 0) {
        wg.mtu = Some(m);
    }
    // reserved 恰 3 项才承载（与生成侧 `if s.reserved.len() == 3` 对称；残值等价缺席）。
    if let Some(rs) = peer.get("reserved").and_then(Value::as_array) {
        let nums: Vec<u32> = rs.iter().filter_map(|v| num_val(Some(v))).collect();
        if nums.len() == 3 && rs.len() == 3 {
            wg.reserved = nums;
        }
    }

    let name = str_ne(ep.get("tag")).unwrap_or_else(|| format!("{server_addr}:{port}"));
    Some(ServerConfig {
        id: id_gen(),
        name,
        protocol: Protocol::Wireguard,
        address: server_addr,
        port,
        wireguard_settings: Some(wg),
        subscription_id: Some(sub_id.to_string()),
        created_at: Some(now.to_string()),
        updated_at: Some(now.to_string()),
        ..Default::default()
    })
}

fn bump(counts: &mut Vec<(String, usize)>, key: String) {
    match counts.iter_mut().find(|(k, _)| *k == key) {
        Some((_, c)) => *c += 1,
        None => counts.push((key, 1)),
    }
}

fn fmt_counts(counts: &[(String, usize)]) -> String {
    counts
        .iter()
        .map(|(k, c)| format!("{k}({c})"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id_gen() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("id-{n}")
        }
    }

    /// 订阅腿（不透传 custom）—— 既有建模映射用例全部走这条，语义与改动前逐字相同。
    fn parse(v: Value) -> ClashParseResult {
        parse_with(v, ImportOrigin::RemoteSubscription)
    }

    /// 本机导入腿（未建模 type 透传 custom）。
    fn parse_local(v: Value) -> ClashParseResult {
        parse_with(v, ImportOrigin::LocalFile)
    }

    fn parse_with(v: Value, origin: ImportOrigin) -> ClashParseResult {
        let mut g = id_gen();
        parse_singbox_outbounds(
            v.get("outbounds").unwrap(),
            "sub1",
            "2026-07-18T00:00:00Z",
            &mut g,
            origin,
        )
    }

    fn parse_eps(v: Value, origin: ImportOrigin) -> ClashParseResult {
        let mut g = id_gen();
        parse_singbox_endpoints(
            v.get("endpoints").unwrap(),
            "sub1",
            "2026-07-18T00:00:00Z",
            &mut g,
            origin,
        )
    }

    #[test]
    fn vless_ws_tls_full_mapping() {
        let out = parse(json!({ "outbounds": [{
            "type": "vless",
            "tag": "HK-01",
            "server": "a.example.com",
            "server_port": 443,
            "uuid": "uuid-1",
            "flow": "XTLS-RPRX-Vision",
            "packet_encoding": "packetaddr",
            "tls": { "enabled": true, "server_name": "sni.example.com", "insecure": true,
                     "alpn": ["h2", "http/1.1"], "utls": { "fingerprint": "Chrome" } },
            "transport": { "type": "ws", "path": "/ray", "host": "cdn.example.com" }
        }] }));
        assert_eq!(out.servers.len(), 1);
        let s = &out.servers[0];
        assert_eq!(s.name, "HK-01");
        assert_eq!(s.protocol, Protocol::Vless);
        assert_eq!(s.address, "a.example.com");
        assert_eq!(s.port, 443);
        assert_eq!(s.uuid.as_deref(), Some("uuid-1"));
        assert_eq!(s.flow.as_deref(), Some("xtls-rprx-vision"), "flow R4 归一");
        assert_eq!(s.packet_encoding.as_deref(), Some("packetaddr"));
        assert_eq!(s.network.as_deref(), Some("ws"));
        let ws = s.ws_settings.as_ref().unwrap();
        assert_eq!(ws.path.as_deref(), Some("/ray"));
        assert_eq!(
            ws.headers
                .as_ref()
                .and_then(|h| h.get("Host"))
                .map(String::as_str),
            Some("cdn.example.com")
        );
        assert_eq!(s.security, Some(SecurityMode::Tls));
        let tls = s.tls_settings.as_ref().unwrap();
        assert_eq!(tls.server_name.as_deref(), Some("sni.example.com"));
        assert_eq!(tls.allow_insecure, Some(true));
        assert_eq!(
            tls.fingerprint.as_deref(),
            Some("chrome"),
            "fingerprint R4 归一"
        );
        assert_eq!(
            tls.alpn.as_deref(),
            Some(&["h2".to_string(), "http/1.1".to_string()][..])
        );
        assert_eq!(s.subscription_id.as_deref(), Some("sub1"));
    }

    #[test]
    fn vless_reality_mapping() {
        let out = parse(json!({ "outbounds": [{
            "type": "vless", "server": "r.com", "server_port": 443, "uuid": "u",
            "tls": { "enabled": true, "server_name": "sni",
                     "reality": { "enabled": true, "public_key": "PBK", "short_id": "sid" },
                     "utls": { "fingerprint": "chrome" } }
        }] }));
        let s = &out.servers[0];
        assert_eq!(s.security, Some(SecurityMode::Reality));
        let r = s.reality_settings.as_ref().unwrap();
        assert_eq!(r.public_key, "PBK");
        assert_eq!(r.short_id.as_deref(), Some("sid"));
    }

    #[test]
    fn reality_without_public_key_falls_to_tls() {
        // reality.enabled 但缺 public_key → 非 reality（回落 tls）。
        let out = parse(json!({ "outbounds": [{
            "type": "vless", "server": "r.com", "server_port": 443, "uuid": "u",
            "tls": { "enabled": true, "reality": { "enabled": true } }
        }] }));
        assert_eq!(out.servers[0].security, Some(SecurityMode::Tls));
        assert!(out.servers[0].reality_settings.is_none());
    }

    #[test]
    fn tls_disabled_leaves_security_unset() {
        let out = parse(json!({ "outbounds": [{
            "type": "trojan", "server": "t.com", "server_port": 443, "password": "pw",
            "tls": { "enabled": false }
        }] }));
        assert_eq!(out.servers[0].security, None, "tls.enabled:false → 不启用");
        assert!(out.servers[0].tls_settings.is_none());
    }

    #[test]
    fn vmess_grpc_mapping() {
        let out = parse(json!({ "outbounds": [{
            "type": "vmess", "server": "b.com", "server_port": "8443", "uuid": "u2",
            "alter_id": 4, "security": "AES-128-GCM",
            "transport": { "type": "grpc", "service_name": "gsvc" }
        }] }));
        let s = &out.servers[0];
        assert_eq!(s.protocol, Protocol::Vmess);
        assert_eq!(s.port, 8443);
        assert_eq!(s.alter_id, Some(4));
        assert_eq!(
            s.vmess_security.as_deref(),
            Some("aes-128-gcm"),
            "vmessSecurity R4 归一"
        );
        assert_eq!(s.network.as_deref(), Some("grpc"));
        assert_eq!(
            s.grpc_settings
                .as_ref()
                .and_then(|g| g.service_name.as_deref()),
            Some("gsvc")
        );
    }

    #[test]
    fn hysteria2_obfs_and_port_hopping() {
        let out = parse(json!({ "outbounds": [{
            "type": "hysteria2", "server": "hy.com", "server_ports": ["20000:30000"],
            "password": "pw", "hop_interval": "30s",
            "obfs": { "type": "salamander", "password": "obfspw" }
        }] }));
        let s = &out.servers[0];
        assert_eq!(s.protocol, Protocol::Hysteria2);
        assert_eq!(s.port, 20000, "无 server_port → server_ports 首范围低位");
        assert_eq!(s.security, Some(SecurityMode::Tls));
        let hy2 = s.hysteria2_settings.as_ref().unwrap();
        assert_eq!(hy2.server_ports.as_deref(), Some("20000:30000"));
        assert_eq!(hy2.hop_interval.as_deref(), Some("30s"));
        let obfs = hy2.obfs.as_ref().unwrap();
        assert_eq!(obfs.type_field.as_deref(), Some("salamander"));
        assert_eq!(obfs.password.as_deref(), Some("obfspw"));
    }

    #[test]
    fn hysteria2_gecko_obfs_and_bbr_profile() {
        // 订阅下发 gecko 混淆 + min/max_packet_size + bbr_profile（导出侧生成的抗审查/性能字段）。
        let out = parse(json!({ "outbounds": [{
            "type": "hysteria2", "server": "hy.com", "server_port": 443, "password": "pw",
            "obfs": { "type": "gecko", "password": "obfspw",
                      "min_packet_size": 100, "max_packet_size": 1200 },
            "bbr_profile": "aggressive"
        }] }));
        let s = &out.servers[0];
        let hy2 = s.hysteria2_settings.as_ref().unwrap();
        let obfs = hy2.obfs.as_ref().unwrap();
        assert_eq!(obfs.type_field.as_deref(), Some("gecko"));
        assert_eq!(obfs.password.as_deref(), Some("obfspw"));
        assert_eq!(obfs.min_packet_size, Some(100), "gecko 才读 packet_size");
        assert_eq!(obfs.max_packet_size, Some(1200));
        assert_eq!(hy2.bbr_profile.as_deref(), Some("aggressive"));
    }

    #[test]
    fn hysteria2_bbr_profile_unknown_and_salamander_no_packet_size() {
        // 未知 bbr_profile 值不设（graceful）；salamander 不带 packet_size。
        let out = parse(json!({ "outbounds": [{
            "type": "hysteria2", "server": "hy.com", "server_port": 443, "password": "pw",
            "obfs": { "type": "salamander", "password": "s",
                      "min_packet_size": 100, "max_packet_size": 1200 },
            "bbr_profile": "turbo"
        }] }));
        let hy2 = out.servers[0].hysteria2_settings.as_ref().unwrap();
        assert_eq!(hy2.bbr_profile, None, "非枚举域值不设");
        let obfs = hy2.obfs.as_ref().unwrap();
        assert_eq!(obfs.type_field.as_deref(), Some("salamander"));
        assert_eq!(obfs.min_packet_size, None, "salamander 不读 packet_size");
        assert_eq!(obfs.max_packet_size, None);
    }

    #[test]
    fn tls_ech_config_imported() {
        // tls.ech.config（ECHConfigList）为字符串数组（sing-box 1.14）→ 归一多行字符串。
        let out = parse(json!({ "outbounds": [{
            "type": "vless", "server": "e.com", "server_port": 443, "uuid": "u",
            "tls": { "enabled": true, "server_name": "sni",
                     "ech": { "enabled": true, "config": ["ECHLINE1", "ECHLINE2"] } }
        }] }));
        let tls = out.servers[0].tls_settings.as_ref().unwrap();
        assert_eq!(tls.ech, Some(true));
        assert_eq!(
            tls.ech_config.as_deref(),
            Some("ECHLINE1\nECHLINE2"),
            "数组 → \\n 拼接"
        );
    }

    #[test]
    fn tls_ech_config_multiline_string_and_enabled_gates_config() {
        // config 亦容忍单个多行字符串（trim + 去空行）；ech 未开启时不读 config。
        let out = parse(json!({ "outbounds": [
            { "type": "vless", "server": "e.com", "server_port": 443, "uuid": "u",
              "tls": { "enabled": true,
                       "ech": { "enabled": true, "config": "  L1 \n\n L2 " } } },
            { "type": "vless", "server": "e2.com", "server_port": 443, "uuid": "u",
              "tls": { "enabled": true,
                       "ech": { "enabled": false, "config": ["L1"] } } }
        ] }));
        assert_eq!(
            out.servers[0]
                .tls_settings
                .as_ref()
                .unwrap()
                .ech_config
                .as_deref(),
            Some("L1\nL2"),
            "多行字符串 trim + 去空行"
        );
        let t2 = out.servers[1].tls_settings.as_ref().unwrap();
        assert_eq!(t2.ech, None, "enabled:false 不启用");
        assert_eq!(t2.ech_config, None, "ech 未开启不读 config");
    }

    #[test]
    fn round_trip_hysteria2_anti_censorship_fields() {
        // 最强验证：typed 节点 → config-engine 导出 outbound → singbox_import 解析回 → 字段等价。
        use polaris_config_engine::builder::outbound::build_proxy_outbound;
        use polaris_config_engine::singbox::DomainResolver;
        use polaris_config_engine::user_config::protocol_settings::{
            Hysteria2ObfsSettings, Hysteria2Settings, TlsSettings,
        };

        let src = ServerConfig {
            id: "src".into(),
            name: "hy2-anti".into(),
            protocol: Protocol::Hysteria2,
            address: "hy.example.com".into(),
            port: 443,
            password: Some("pw".into()),
            security: Some(SecurityMode::Tls),
            tls_settings: Some(TlsSettings {
                server_name: Some("sni.example.com".into()),
                ech: Some(true),
                ech_config: Some("ECHLINE1\nECHLINE2".into()),
                ..Default::default()
            }),
            hysteria2_settings: Some(Hysteria2Settings {
                obfs: Some(Hysteria2ObfsSettings {
                    type_field: Some("gecko".into()),
                    password: Some("obfspw".into()),
                    min_packet_size: Some(100),
                    max_packet_size: Some(1200),
                }),
                bbr_profile: Some("aggressive".into()),
                server_ports: Some("20000:30000".into()),
                hop_interval: Some("30s".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        // 导出（config-engine）→ sing-box outbound JSON → 导入回。
        // 纯 tag：本用例验的是 outbound 的**导出→导入往返闭合**，dial 侧解析器形态与它无关；
        // 用结构化形态只会给 round-trip 断言引入无关噪声。
        let ob = build_proxy_outbound(
            &src,
            "proxy-1",
            &DomainResolver::Tag("dns-bootstrap".to_string()),
            "x64",
            "linux",
        );
        let wrapped = json!({ "outbounds": [serde_json::to_value(&ob).unwrap()] });
        let out = parse(wrapped);

        assert_eq!(out.servers.len(), 1, "round-trip 节点应闭合");
        let s = &out.servers[0];
        assert_eq!(s.protocol, Protocol::Hysteria2);
        assert_eq!(s.security, Some(SecurityMode::Tls));

        let tls = s.tls_settings.as_ref().unwrap();
        assert_eq!(tls.ech, Some(true), "ech 开关闭合");
        assert_eq!(
            tls.ech_config.as_deref(),
            Some("ECHLINE1\nECHLINE2"),
            "ech.config 多行字符串 ←→ 数组 round-trip 闭合"
        );

        let hy2 = s.hysteria2_settings.as_ref().unwrap();
        let obfs = hy2.obfs.as_ref().unwrap();
        assert_eq!(obfs.type_field.as_deref(), Some("gecko"));
        assert_eq!(obfs.password.as_deref(), Some("obfspw"));
        assert_eq!(obfs.min_packet_size, Some(100), "gecko packet_size 闭合");
        assert_eq!(obfs.max_packet_size, Some(1200));
        assert_eq!(
            hy2.bbr_profile.as_deref(),
            Some("aggressive"),
            "bbr_profile 闭合"
        );
        assert_eq!(hy2.server_ports.as_deref(), Some("20000:30000"));
        assert_eq!(hy2.hop_interval.as_deref(), Some("30s"));
    }

    #[test]
    fn tuic_heartbeat_and_congestion() {
        let out = parse(json!({ "outbounds": [{
            "type": "tuic", "server": "tu.com", "server_port": 443, "uuid": "u", "password": "pw",
            "congestion_control": "bbr", "udp_relay_mode": "native",
            "zero_rtt_handshake": true, "heartbeat": 10000
        }] }));
        let s = &out.servers[0];
        let ts = s.tuic_settings.as_ref().unwrap();
        assert_eq!(ts.congestion_control.as_deref(), Some("bbr"));
        assert_eq!(ts.udp_relay_mode.as_deref(), Some("native"));
        assert_eq!(ts.zero_rtt_handshake, Some(true));
        assert_eq!(ts.heartbeat.as_deref(), Some("10000ms"), "纯数字补 ms 单位");
    }

    #[test]
    fn snell_v4_obfs_and_reject_bad_version() {
        let out = parse(json!({ "outbounds": [
            { "type": "snell", "server": "s.com", "server_port": 443, "psk": "k", "version": 4,
              "obfs_mode": "http", "obfs_host": "h.com" },
            { "type": "snell", "server": "s2.com", "server_port": 443, "psk": "k", "version": 3 }, // 坏版本
            { "type": "snell", "server": "s3.com", "server_port": 443, "version": 4 }              // 缺 psk
        ] }));
        assert_eq!(out.servers.len(), 1, "仅合法 v4 入库");
        let s = &out.servers[0];
        assert_eq!(s.protocol, Protocol::Snell);
        assert_eq!(s.password.as_deref(), Some("k"), "psk → password");
        let snell = s.snell_settings.as_ref().unwrap();
        assert_eq!(snell.version, 4);
        assert_eq!(snell.obfs_mode.as_deref(), Some("http"));
        assert_eq!(snell.obfs_host.as_deref(), Some("h.com"));
        assert_eq!(out.failed, 2, "坏版本 + 缺 psk 计 failed");
    }

    #[test]
    fn shadowsocks_and_ssh_mapping() {
        let out = parse(json!({ "outbounds": [
            { "type": "shadowsocks", "server": "ss.com", "server_port": 8388, "method": "aes-256-gcm", "password": "p" },
            { "type": "ssh", "server": "ssh.com", "server_port": 22, "user": "root", "password": "pw",
              "host_key_algorithms": ["ssh-ed25519"] }
        ] }));
        assert_eq!(out.servers.len(), 2);
        let ss = out.servers[0].shadowsocks_settings.as_ref().unwrap();
        assert_eq!(ss.method, "aes-256-gcm");
        assert_eq!(ss.password, "p");
        let ssh_srv = &out.servers[1];
        assert_eq!(ssh_srv.network.as_deref(), Some("tcp"));
        assert_eq!(ssh_srv.security, Some(SecurityMode::None));
        let ssh = ssh_srv.ssh_settings.as_ref().unwrap();
        assert_eq!(ssh.user.as_deref(), Some("root"));
        assert_eq!(
            ssh.host_key_algorithms.as_deref(),
            Some(&["ssh-ed25519".to_string()][..])
        );
    }

    #[test]
    fn skip_internal_unsupported_type_and_transport() {
        let out = parse(json!({ "outbounds": [
            { "type": "direct" },                                        // internal → 不计
            { "type": "selector", "outbounds": [] },                     // internal → 不计
            { "type": "wireguard", "server": "w.com", "server_port": 1 },// 不支持 type → skipped
            { "type": "vless", "server": "q.com", "server_port": 443, "uuid": "u",
              "transport": { "type": "quic" } },                         // 不支持传输 → skipped
            { "type": "trojan", "server": "t.com", "server_port": 443, "password": "p" }
        ] }));
        assert_eq!(out.servers.len(), 1, "仅 trojan 入库");
        assert_eq!(out.skipped, 2, "wireguard(type) + quic(transport)");
        assert!(out.warnings.iter().any(|w| w.contains("wireguard(1)")));
        assert!(out.warnings.iter().any(|w| w.contains("quic(1)")));
    }

    #[test]
    fn missing_server_or_port_counts_failed() {
        let out = parse(json!({ "outbounds": [
            { "type": "vless", "server_port": 443, "uuid": "u" },   // 缺 server
            { "type": "vless", "server": "a.com", "uuid": "u" },    // 缺 port
            { "type": "vless", "server": "", "server_port": 443, "uuid": "u" } // 空 server
        ] }));
        assert_eq!(out.servers.len(), 0);
        assert_eq!(out.failed, 3);
    }

    #[test]
    fn port_zero_and_overflow_rejected() {
        let out = parse(json!({ "outbounds": [
            { "type": "vless", "server": "a.com", "server_port": 0, "uuid": "u" },
            { "type": "vless", "server": "a.com", "server_port": 70000, "uuid": "u" }
        ] }));
        assert_eq!(out.servers.len(), 0, "port 0 / 越界拒");
        assert_eq!(out.failed, 2);
    }

    #[test]
    fn multiplex_and_naive_http3() {
        let out = parse(json!({ "outbounds": [
            { "type": "vless", "server": "a.com", "server_port": 443, "uuid": "u",
              "multiplex": { "enabled": true, "protocol": "smux", "max_connections": 4 } },
            { "type": "naive", "server": "n.com", "server_port": 443, "username": "x", "password": "y", "quic": true }
        ] }));
        let mux = out.servers[0].multiplex_settings.as_ref().unwrap();
        assert_eq!(mux.enabled, Some(true));
        assert_eq!(mux.protocol.as_deref(), Some("smux"));
        assert_eq!(mux.max_connections, Some(4));
        let naive = &out.servers[1];
        assert_eq!(naive.protocol, Protocol::Naive);
        assert_eq!(
            naive.naive_settings.as_ref().and_then(|n| n.use_http3),
            Some(true)
        );
    }

    #[test]
    fn non_array_outbounds_is_empty() {
        let mut g = id_gen();
        let r = parse_singbox_outbounds(
            &json!({}),
            "s",
            "now",
            &mut g,
            ImportOrigin::RemoteSubscription,
        );
        assert_eq!(r.servers.len(), 0);
        assert_eq!((r.skipped, r.failed), (0, 0));
        let mut g2 = id_gen();
        let e = parse_singbox_endpoints(&json!({}), "s", "now", &mut g2, ImportOrigin::LocalFile);
        assert_eq!(e.servers.len(), 0);
        assert_eq!((e.skipped, e.failed), (0, 0));
    }

    // ── P1：未建模 type → custom 逃生舱 ───────────────────────────────────────────

    #[test]
    fn local_import_wraps_unmodeled_types_as_custom() {
        // 语料 = 真实存在于内核、Polaris **未建模**的 outbound type。
        // ⚠️ 2026-08-11：hysteria(v1) / tor 已进建模协议，故从本语料移出（它们现在走各自的建模腿，
        // 见 `hysteria_and_tor_map_to_modeled_protocols`）。留下的 shadowtls 是**故意**的：
        // 它在本仓以 shadowsocks 插件形态建模，作为**独立 outbound type** 仍无落点 ⇒ 仍该包 custom。
        let out = parse_local(json!({ "outbounds": [
            { "type": "shadowtls", "server": "st.example.com", "server_port": 443, "version": 3,
              "password": "pw", "tls": { "enabled": true, "server_name": "cdn.example.com" } },
            { "type": "direct", "tag": "direct" },
            { "type": "trojan", "tag": "T", "server": "t.com", "server_port": 443, "password": "p" }
        ] }));
        assert_eq!(out.servers.len(), 2, "1 custom + 1 建模 trojan");
        assert_eq!(out.skipped, 0, "透传后不再计 skipped");
        assert_eq!(out.failed, 0);
        assert!(out.warnings.is_empty(), "透传腿不产「跳过不支持类型」告警");

        // 唯一的 custom：shadowtls（独立 outbound type 无落点）。原文逐字透传。
        let st = &out.servers[0];
        assert_eq!(st.protocol, Protocol::Custom);
        assert_eq!(st.address, "st.example.com");
        assert_eq!(st.port, 443);
        let cs = st.custom_settings.as_ref().unwrap();
        assert_eq!(cs.is_endpoint, None, "outbounds[] 腿不置 isEndpoint");
        assert_eq!(cs.outbound.get("type").unwrap(), "shadowtls");
        assert_eq!(cs.outbound.get("version").unwrap(), 3);
        assert_eq!(cs.outbound.get("password").unwrap(), "pw");

        assert_eq!(out.servers[1].protocol, Protocol::Trojan, "建模腿不受影响");
    }

    #[test]
    fn remote_subscription_never_wraps_custom() {
        // 反向对照（信任级分流）：**同一份**输入走订阅腿 → 一个 custom 都不产，仍旧 skipped + 告警。
        // 这条是安全不变量：custom 逃传把原文逐字下发内核，而 `tor` outbound 收
        // `executable_path`/`extra_args`（随包核 1.14.0-beta.7 `sing-box check` rc=0）
        // ⇒ 远端订阅一旦能造 custom 即等于任意本机命令执行。
        let doc = json!({ "outbounds": [
            { "type": "hysteria", "tag": "HY1-JP", "server": "hy1.example.com", "server_port": 8443 },
            { "type": "tor", "tag": "Tor", "executable_path": "/usr/bin/tor" },
            { "type": "trojan", "tag": "T", "server": "t.com", "server_port": 443, "password": "p" }
        ] });
        let out = parse(doc);
        // 2026-08-11：hysteria(v1) 已建模 ⇒ 订阅腿正常收它（它没有命令执行向量，与 tor 不同）。
        // tor 仍被跳过：`executable_path`/`extra_args` = 起任意本机程序，只许本地文件。
        assert_eq!(
            out.servers.len(),
            2,
            "建模 trojan + 建模 hysteria；tor 被信任级分流挡下"
        );
        assert!(out.servers.iter().all(|s| s.protocol != Protocol::Custom));
        assert!(
            out.servers.iter().all(|s| s.protocol != Protocol::Tor),
            "tor 从订阅入库了 —— executable_path 等于任意本机命令执行"
        );
        // 🔴 订阅腿**不填透传袋**：袋里任意键会随配置下发，而未知字段是 decode 阶段拒收
        // ⇒ 远端塞个乱键就能让整个核起不来。
        let hy = out
            .servers
            .iter()
            .find(|s| s.protocol == Protocol::Hysteria)
            .expect("hysteria 没入库");
        assert!(
            hy.hysteria_settings.as_ref().unwrap().extra.is_empty(),
            "订阅腿填了透传袋 —— 远端可借未知字段让整个核 decode 失败"
        );
        assert_eq!(out.skipped, 1);
        assert!(out.warnings.iter().any(|w| w.contains("tor(1)")));
    }

    /// **逃生舱的价值验在这里**：导入产出的 custom 节点经 config-engine 生成回 sing-box outbound，
    /// 未建模字段一个不少 —— 否则 P1 只是「从直接丢弃变成留下来但被吃掉」，不值得做。
    ///
    /// 锁的是与 `098b41e`（`Outbound::extra` 真透传）的接线：那条修复回退成窄 struct
    /// `from_value` 的话，`up_mbps`/`auth_str`/`executable_path` 会静默消失而本例转红。
    #[test]
    fn round_trip_custom_wrapped_node_survives_generation() {
        use polaris_config_engine::builder::outbound::build_proxy_outbound;
        use polaris_config_engine::singbox::DomainResolver;

        let out = parse_local(json!({ "outbounds": [
            { "type": "hysteria", "tag": "HY1", "server": "hy1.example.com", "server_port": 8443,
              "up_mbps": 100, "down_mbps": 500, "auth_str": "pw",
              "tls": { "enabled": true, "server_name": "hy1.example.com" } },
            { "type": "tor", "tag": "Tor", "executable_path": "/usr/bin/tor",
              "extra_args": ["--x"], "data_directory": "/var/lib/tor" }
        ] }));
        assert_eq!(out.servers.len(), 2);

        let hy = serde_json::to_value(build_proxy_outbound(
            &out.servers[0],
            "proxy-1",
            &DomainResolver::Tag("dns-bootstrap".to_string()),
            "x64",
            "linux",
        ))
        .unwrap();
        assert_eq!(
            hy.get("type").unwrap(),
            "hysteria",
            "type 原样，非 \"custom\""
        );
        assert_eq!(
            hy.get("tag").unwrap(),
            "proxy-1",
            "tag 被生成侧覆盖（既有语义）"
        );
        assert_eq!(hy.get("up_mbps").unwrap(), 100, "未建模字段必须活着下发");
        assert_eq!(hy.get("down_mbps").unwrap(), 500);
        assert_eq!(hy.get("auth_str").unwrap(), "pw");
        assert_eq!(hy.get("server").unwrap(), "hy1.example.com");
        assert_eq!(hy.get("server_port").unwrap(), 8443);

        let tor = serde_json::to_value(build_proxy_outbound(
            &out.servers[1],
            "proxy-2",
            &DomainResolver::Tag("dns-bootstrap".to_string()),
            "x64",
            "linux",
        ))
        .unwrap();
        assert_eq!(tor.get("type").unwrap(), "tor");
        assert_eq!(tor.get("executable_path").unwrap(), "/usr/bin/tor");
        assert_eq!(tor.get("extra_args").unwrap(), &json!(["--x"]));
        assert_eq!(tor.get("data_directory").unwrap(), "/var/lib/tor");
    }

    #[test]
    fn malformed_type_never_wrapped_even_locally() {
        // 形状判据与生成侧 `custom_outbound_type` 同源 + 上游的「type 非空白」：
        //  - 缺 type / 空串 / 纯空白 → 落盘门 `protocol_requirement_ok("custom")` 会剔，别造；
        //  - **非字符串 type**（数字）→ 生成侧 `custom_outbound_type` 返 None ⇒ 导得进也会被剔
        //    （这正是「导入侧另写一份判据」会产生的第三种分叉）。
        let doc = json!({ "outbounds": [
            { "tag": "x", "server": "a.com" },
            { "type": "", "server": "b.com" },
            { "type": "   ", "server": "c.com" },
            { "type": 42, "server": "d.com" }
        ] });
        for out in [parse_local(doc.clone()), parse(doc)] {
            assert_eq!(out.servers.len(), 0);
            assert!(out.servers.iter().all(|s| s.protocol != Protocol::Custom));
            assert_eq!(out.skipped, 4);
        }
    }

    #[test]
    fn internal_types_never_wrapped() {
        // direct/block/dns/selector/urltest 是内核内部 outbound，不是节点 —— 两条腿都静默忽略。
        let doc = json!({ "outbounds": [
            { "type": "direct", "tag": "d" }, { "type": "block", "tag": "b" },
            { "type": "dns", "tag": "dns" }, { "type": "selector", "tag": "s", "outbounds": [] },
            { "type": "urltest", "tag": "u", "outbounds": [] }
        ] });
        let out = parse_local(doc);
        assert_eq!(out.servers.len(), 0, "内部 type 不得被包成 custom 节点");
        assert_eq!((out.skipped, out.failed), (0, 0));
        assert!(out.warnings.is_empty());
    }

    // ── P2：endpoints[] ─────────────────────────────────────────────────────────

    /// 与 `sing-box check` 验过的那份 endpoints 语料同形（见本批交付说明）。
    fn wg_endpoint_doc() -> Value {
        json!({ "endpoints": [{
            "type": "wireguard",
            "tag": "WG-HK",
            "system": true,
            "mtu": 1408,
            "address": ["172.16.0.2/32", "fd00::2/128"],
            "private_key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEk=",
            "listen_port": 51820,
            "udp_timeout": "5m",
            "workers": 2,
            "peers": [{
                "address": "wg.example.com",
                "port": 2408,
                "public_key": "bmXOC+F1FxEMF9dyiK2H5/1SUtzH0JuVo51h2wPfgyo=",
                "pre_shared_key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEk=",
                "allowed_ips": ["0.0.0.0/0", "::/0", "10.8.0.0/24"],
                "persistent_keepalive_interval": 25,
                "reserved": [1, 2, 3]
            }]
        }] })
    }

    #[test]
    fn wireguard_endpoint_full_mapping() {
        let out = parse_eps(wg_endpoint_doc(), ImportOrigin::RemoteSubscription);
        assert_eq!(out.servers.len(), 1, "endpoints-only 订阅不再恒 0 节点");
        assert_eq!((out.skipped, out.failed), (0, 0));
        let s = &out.servers[0];
        assert_eq!(s.protocol, Protocol::Wireguard);
        assert_eq!(s.name, "WG-HK", "tag → name");
        assert_eq!(s.address, "wg.example.com", "peers[0].address → address");
        assert_eq!(s.port, 2408, "peers[0].port → port");
        assert_eq!(s.subscription_id.as_deref(), Some("sub1"));
        let wg = s.wireguard_settings.as_ref().unwrap();
        assert_eq!(
            wg.private_key.as_deref(),
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEk=")
        );
        assert_eq!(wg.local_address, vec!["172.16.0.2/32", "fd00::2/128"]);
        assert_eq!(
            wg.peer_public_key.as_deref(),
            Some("bmXOC+F1FxEMF9dyiK2H5/1SUtzH0JuVo51h2wPfgyo=")
        );
        assert_eq!(
            wg.pre_shared_key.as_deref(),
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEk=")
        );
        assert_eq!(
            wg.allowed_ips,
            vec!["10.8.0.0/24"],
            "catch-all 抽进 allowInternet，allowedIPs 只留具体段（与 wg-quick 导入腿同源）"
        );
        assert_eq!(wg.allow_internet, Some(true));
        assert_eq!(wg.persistent_keepalive, Some(25));
        assert_eq!(wg.mtu, Some(1408));
        assert_eq!(wg.reserved, vec![1, 2, 3]);
        assert_eq!(
            wg.reverse_mesh, None,
            "`system:true` 刻意不映射 reverseMesh —— 外部文件不该能翻抢内核 utun 的开关"
        );
    }

    #[test]
    fn wireguard_endpoint_without_catch_all_is_mesh_only() {
        let out = parse_eps(
            json!({ "endpoints": [{
                "type": "wireguard", "address": ["10.0.0.2/32"], "private_key": "pk",
                "peers": [{ "address": "1.2.3.4", "port": 51820, "public_key": "pub",
                            "allowed_ips": ["10.0.0.0/24"] }]
            }] }),
            ImportOrigin::RemoteSubscription,
        );
        let s = &out.servers[0];
        assert_eq!(s.name, "1.2.3.4:51820", "无 tag → addr:port 兜底");
        let wg = s.wireguard_settings.as_ref().unwrap();
        assert_eq!(wg.allow_internet, Some(false), "无 catch-all → 仅组网");
        assert_eq!(wg.allowed_ips, vec!["10.0.0.0/24"]);
        assert_eq!(wg.persistent_keepalive, None, "缺省不写");
        assert_eq!(wg.mtu, None);
        assert!(wg.reserved.is_empty());
        assert_eq!(wg.pre_shared_key, None);
    }

    #[test]
    fn wireguard_endpoint_missing_required_counts_failed() {
        // 逐条对应落盘门：`protocol_requirement_ok("wireguard")`（privateKey / peerPublicKey /
        // 非空 localAddress）+ `sanitize_servers` 的 address / port∈1..=65535。
        let out = parse_eps(
            json!({ "endpoints": [
                { "type": "wireguard", "address": ["10.0.0.2/32"],
                  "peers": [{ "address": "1.2.3.4", "port": 1, "public_key": "pub" }] },   // 缺 private_key
                { "type": "wireguard", "private_key": "pk", "address": ["10.0.0.2/32"],
                  "peers": [{ "address": "1.2.3.4", "port": 1 }] },                        // 缺 public_key
                { "type": "wireguard", "private_key": "pk",
                  "peers": [{ "address": "1.2.3.4", "port": 1, "public_key": "pub" }] },   // 缺 address[]
                { "type": "wireguard", "private_key": "pk", "address": ["10.0.0.2/32"] },  // 缺 peers
                { "type": "wireguard", "private_key": "pk", "address": ["10.0.0.2/32"],
                  "peers": [{ "port": 1, "public_key": "pub" }] },                         // peer 缺 address
                { "type": "wireguard", "private_key": "pk", "address": ["10.0.0.2/32"],
                  "peers": [{ "address": "1.2.3.4", "port": 0, "public_key": "pub" }] }    // port 0
            ] }),
            ImportOrigin::RemoteSubscription,
        );
        assert_eq!(out.servers.len(), 0);
        assert_eq!(out.failed, 6);
        assert_eq!(out.skipped, 0);
        assert!(out
            .warnings
            .iter()
            .any(|w| w.contains("6 个缺 private_key")));
    }

    #[test]
    fn wireguard_endpoint_reserved_must_be_exactly_three() {
        // 与生成侧 `if s.reserved.len() == 3` 对称：残值等价缺席，不留半截数组。
        let out = parse_eps(
            json!({ "endpoints": [{
                "type": "wireguard", "address": ["10.0.0.2/32"], "private_key": "pk",
                "peers": [{ "address": "1.2.3.4", "port": 1, "public_key": "pub",
                            "reserved": [1, 2] }]
            }] }),
            ImportOrigin::RemoteSubscription,
        );
        assert!(out.servers[0]
            .wireguard_settings
            .as_ref()
            .unwrap()
            .reserved
            .is_empty());
    }

    #[test]
    fn tailscale_endpoint_always_skipped_never_custom() {
        for origin in [ImportOrigin::RemoteSubscription, ImportOrigin::LocalFile] {
            let out = parse_eps(
                json!({ "endpoints": [{
                    "type": "tailscale", "tag": "TS", "auth_key": "tskey-auth-SECRET",
                    "hostname": "victim", "control_url": "https://ctl.example.com"
                }] }),
                origin,
            );
            assert_eq!(out.servers.len(), 0, "{origin:?}：账号制凭据不导入");
            assert!(
                out.servers.iter().all(|s| s.protocol != Protocol::Custom),
                "{origin:?}：也不得包成 custom（会绕过前端 tailscale 单例闸门）"
            );
            assert_eq!(out.skipped, 1);
            assert!(out
                .warnings
                .iter()
                .any(|w| w.contains("tailscale endpoint")));
        }
    }

    #[test]
    fn unmodeled_endpoint_types_wrap_as_custom_endpoint() {
        // openconnect / openvpn-client（+ openvpn-server，同一条 `_` 臂）：内核 `$defs/Endpoint`
        // 认这些 type，Polaris 无落点 → custom + isEndpoint。本语料 `sing-box check` rc=0
        // （openvpn-client 的 `tls.peer_fingerprint` 是自签证书的 sha256，凑够内核的必填 TLS 材料；
        // openvpn-server 需本机证书文件路径，不适合进单测语料，故不入本例）。
        let doc = json!({ "endpoints": [
            { "type": "openconnect", "tag": "OC", "server": "vpn.example.com:443",
              "username": "u", "password": "p" },
            { "type": "openvpn-client", "server": "ovpn.example.com", "server_port": 1194,
              "username": "u", "password": "p",
              "tls": { "peer_fingerprint":
                       "e0593c478275d2bd1722039e5b7ba37fd39cd75cacff0a81bc66b46b5628f9bf" } }
        ] });
        let out = parse_eps(doc.clone(), ImportOrigin::LocalFile);
        assert_eq!(out.servers.len(), 2);
        assert_eq!((out.skipped, out.failed), (0, 0));
        // 2026-08-11：二者已进建模协议 ⇒ 不再包 custom，而是各归各的设置结构。
        assert_eq!(out.servers[0].protocol, Protocol::Openconnect);
        assert_eq!(out.servers[1].protocol, Protocol::OpenvpnClient);
        assert_eq!(out.servers[0].name, "OC");
        // openconnect 的 server 是 host:port 单串 ⇒ address/port 由它拆出（落盘门要这两项）。
        assert_eq!(out.servers[0].address, "vpn.example.com");
        assert_eq!(out.servers[0].port, 443);
        let oc = out.servers[0].openconnect_settings.as_ref().unwrap();
        assert_eq!(
            oc.server.as_deref(),
            Some("vpn.example.com:443"),
            "原串保留"
        );
        assert_eq!(oc.username.as_deref(), Some("u"));
        // 命名口径随之改变：custom 腿用 type 作名，建模腿走 new_server ⇒ 无 tag 时用 addr:port。
        // 这是形态变化不是回归 —— 与其余 15 个建模协议一致了。
        assert_eq!(
            out.servers[1].name, "ovpn.example.com:1194",
            "无 tag → addr:port"
        );
        assert_eq!(out.servers[1].port, 1194);
        // 未建模的 tls.peer_fingerprint 走透传袋（本地文件才填）—— 逐字保全，
        // 否则「导入 → 编辑 → 保存」会把内核唯一的 TLS 材料丢掉，节点从能连变成起不来。
        let ov = out.servers[1].openvpn_client_settings.as_ref().unwrap();
        assert_eq!(
            ov.tls
                .as_ref()
                .and_then(|t| t.extra.get("peer_fingerprint"))
                .and_then(|v| v.as_str()),
            Some("e0593c478275d2bd1722039e5b7ba37fd39cd75cacff0a81bc66b46b5628f9bf"),
            "未建模的 TLS 材料被丢掉了"
        );

        // 订阅腿：同一份输入一个不导，计 skipped。
        let remote = parse_eps(doc, ImportOrigin::RemoteSubscription);
        assert_eq!(remote.servers.len(), 0);
        assert_eq!(remote.skipped, 2);
        assert!(remote
            .warnings
            .iter()
            .any(|w| w.contains("openconnect(1)") && w.contains("openvpn-client(1)")));
    }
}
