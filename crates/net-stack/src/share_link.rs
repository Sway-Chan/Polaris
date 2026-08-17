//! 分享链接解析（上游 `main/services/ProtocolParser.ts` 的 parse 侧移植，纯逻辑）。
//!
//! 覆盖 `vless:` / `vmess:` / `ss:` / `trojan:` / `hysteria2:` / `tuic:` / `anytls:` / `snell:` /
//! `naive:` / `socks:` / `http:` 等 scheme → [`ServerConfig`]。多数机场订阅是 base64 / url-list
//! 形态（非 Clash YAML），本模块是那两条路的节点级解析器；Clash YAML 走 [`crate::clash_parser`]。
//!
//! **设计要点**：
//! - **URL 解析用 `url` crate（既有依赖）而非手搓**：`url` 2.5 与 Node `new URL()` 同为 WHATWG URL
//!   实现，实测在 userinfo 百分号编码、IPv6 规范化（`[::ffff:1.2.3.4]` → `[::ffff:102:304]`）、
//!   默认端口省略（`http://h:80` → port 空）上逐项一致 → 金样对拍可比。手搓最小解析必然在这些
//!   边角上与 oracle 漂移。
//! - **传输别名归一收敛为一份**：[`normalize_transport`]（config-engine）。上游 有三份同构表
//!   （URI `type=` / vmess JSON `net` / xray `streamSettings.network`），issue #263 即「一处漏 case
//!   → 一船节点全灭」。本模块两条入参形态共用同一张表；各形态的字段抽取不同，那不是重复。
//! - **未知传输 = 整节点拒绝**，不静默降级成 tcp（假节点「看得见连不上」比丢弃更糟，issue #263）。
//! - **脏值容错分层**：凭据残缺 / 能力缺席（reality 缺 pbk、hy2 salamander 缺密码、未知传输、
//!   snell 版本越界）→ 整节点拒绝（入库也连不上）；仅**枚举 token 脏**（`security=BOGUS`）→
//!   [`SecurityMode::Unknown`] 保留原文按非 TLS 处理，**不让整个节点消失**（§L1）。
//!
//! **不做 SSRF**：节点地址是用户意图连接的代理服务器，内网/link-local 是合法形态（LAN 代理、
//! mesh peer、本机链式代理）。[`crate::ssrf::assert_host_allowed`] 的语义是「订阅/Provider **URL**
//! 的 hostname」（其错误文案即 `订阅地址...`），已在 [`crate::subscription::fetch_subscription_full`]
//! 的拉取层生效；对节点地址套用会拒掉合法 LAN 节点，且与既有 Clash YAML 路径（`clash_parser` 同样
//! 不做）产生「同一订阅 YAML 能进、url-list 不能进」的分裂。上游 两个解析器亦均不做。

#![forbid(unsafe_code)]

use std::net::Ipv6Addr;

use polaris_config_engine::builder::outbound_helpers::normalize_duration;
use polaris_config_engine::user_config::collections::dedupe_trim;
use polaris_config_engine::user_config::normalize::{normalize_token, normalize_transport};
use polaris_config_engine::user_config::protocol_settings::{
    AnyTlsSettings, GrpcSettings, HttpSettings, Hysteria2ObfsSettings, Hysteria2Settings,
    RealitySettings, ShadowTlsSettings, ShadowsocksSettings, SnellSettings, TlsSettings,
    TuicSettings, WebSocketSettings,
};
use polaris_config_engine::user_config::server_config::{Protocol, SecurityMode, ServerConfig};
use url::Url;

/// 分享链接支持的 scheme 白名单。**与前端 `ui/src/shared/protocol-url-schemes.ts` 同源**
/// （`scheme_whitelist_matches_frontend` 测试逐条对差，漂移即红）。
///
/// 前后端各持一份白名单曾撞 issue #191（`naive://` 仅后端支持、前端漏列 → 导入语境不一致）。
/// Rust 侧无法直接 import TS 常量，故以「常量 + 跨语言一致性测试」保证同源：漂移在 `cargo test`
/// 转红，而不是等用户粘贴链接才发现。
pub const SUPPORTED_URL_SCHEMES: &[&str] = &[
    "vless",
    "vmess",
    "trojan",
    "hysteria2",
    "hy2",
    "ss",
    "anytls",
    // snell 无标准 scheme，此为 Surge 生态工具（sub-store 等）的事实形态。
    "snell",
    "tuic",
    "naive",
    "http2",
    "naive+https",
    "socks5",
    "socks",
    "s5",
    "http",
    "https",
];

/// 是否受支持的分享链接。上游 `isSupportedShareUrl`。
///
/// 前缀精确匹配 `<scheme>://`：`http` → `http://` 不会误命中 `https://`（后者不以 `http://`
/// 开头），故列表顺序无关。大小写敏感（与 TS `startsWith` 同）。
pub fn is_supported_share_url(url: &str) -> bool {
    SUPPORTED_URL_SCHEMES.iter().any(|s| {
        url.len() > s.len() + 3
            && url.is_char_boundary(s.len())
            && &url[..s.len()] == *s
            && url[s.len()..].starts_with("://")
    })
}

// ── 通用工具 ─────────────────────────────────────────────────────────────────

/// `decodeURIComponent` 等价：仅解 `%XX`（不把 `+` 当空格），非法序列 / 非 UTF-8 → Err。
///
/// TS 侧 `decodeURIComponent` 对畸形输入抛 URIError → 整节点拒绝；此处 Err 对齐该语义。
fn decode_uri_component(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return Err("URI malformed".to_string());
            }
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).map_err(|_| "URI malformed")?;
            let v = u8::from_str_radix(hex, 16).map_err(|_| "URI malformed")?;
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "URI malformed".to_string())
}

/// JS `parseInt` 语义：跳前导空白 + 可选符号 + 取前导数字段（`"100abc"` → 100），无数字 → None。
///
/// 刻意不用 `str::parse`（对 `"100abc"` 报错）——TS 侧对订阅脏值靠 parseInt 的宽松截断行为，
/// 逐字段对拍要求同语义。
fn js_parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let n: i64 = digits[..end].parse().ok()?;
    Some(if neg { -n } else { n })
}

/// 查询参数（`URLSearchParams` 等价）：保序、允许重复 key，`get` 取首个。
struct Params(Vec<(String, String)>);

impl Params {
    fn from_url(u: &Url) -> Self {
        Self(
            u.query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
    }
    /// 取首个同名参数。缺键 → None（对齐 `URLSearchParams.get` 的 null）。
    fn get(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
    /// 取首个**非空**同名参数（对齐 TS `params.get(a) || params.get(b)` 的 falsy 穿透）。
    fn get_ne(&self, key: &str) -> Option<String> {
        self.get(key).filter(|v| !v.is_empty())
    }
}

/// 去 IPv6 方括号（`[::1]` → `::1`）。上游 `stripIpv6Brackets`。
fn strip_ipv6_brackets(host: &str) -> String {
    if host.starts_with('[') && host.ends_with(']') && host.len() >= 2 {
        host[1..host.len() - 1].to_string()
    } else {
        host.to_string()
    }
}

/// 是否合法 IPv6 字面量（Node `net.isIPv6` 等价，stdlib 解析，无新依赖）。
fn is_ipv6(s: &str) -> bool {
    s.parse::<Ipv6Addr>().is_ok()
}

/// 裸 IPv6（无方括号）预处理：`ss://u@2001:db8::1:8388?..` → `ss://u@[2001:db8::1]:8388?..`。
/// 上游 `preprocessBareIpv6`（全协议通用；无 `@` 的形态如 vmess base64 原样返回）。
///
/// 「地址 vs 地址:端口」是该文法的**固有歧义**（`2001:db8::1:8388` 两种读法都合法），消解启发式
/// （issue #263 定案，15 组边界专项审计通过）：
/// 1. 末段 2-5 位十进制且 ≤65535 且余段是合法 IPv6 → 按 `地址+端口` 拆（分享链几乎恒带端口）；
///    单数字末段（如 `::1`）按地址整段——真实代理端口不用 1-9，而地址末段 `1` 极常见。
/// 2. 否则整段本身合法 IPv6（无端口形态）→ 只加括号，端口缺省交 parse_base（与域名无端口同语义）。
/// 3. 两者皆非 → 原样返回，交 `Url::parse` 报错（**丢弃可见**，不静默入库截断地址的假节点）。
///
/// 注：Clash YAML 路径本就 server/port 分离字段、不产生此歧义，故 [`crate::clash_parser`] 无需本函数。
fn preprocess_bare_ipv6(raw: &str) -> String {
    let Some(at_idx) = raw.find('@') else {
        return raw.to_string();
    };
    let before_at = &raw[..at_idx + 1];
    let after_at = &raw[at_idx + 1..];

    // 切掉 path/query/fragment，只处理 host:port 段。
    // （SIP002 plugin 形态 `…:port/?plugin=…` 的 `/` 在 `?` 前是标准写法，不截断会使 hostPort 携 `/` 双判定皆失败）
    let q_idx = after_at.find(['/', '?', '#']);
    let (host_port, suffix) = match q_idx {
        Some(i) => (&after_at[..i], &after_at[i..]),
        None => (after_at, ""),
    };

    if host_port.starts_with('[') {
        return raw.to_string(); // 已带方括号
    }

    // IPv6 地址含多个冒号；末个冒号是候选端口分隔符。
    if host_port.split(':').count() >= 4 {
        let last_colon = host_port.rfind(':').unwrap_or(0);
        let potential_port = &host_port[last_colon + 1..];
        let remainder = &host_port[..last_colon];
        let port_shaped = (2..=5).contains(&potential_port.len())
            && potential_port.bytes().all(|b| b.is_ascii_digit());
        if port_shaped
            && potential_port.parse::<u32>().is_ok_and(|p| p <= 65535)
            && is_ipv6(remainder)
        {
            return format!("{before_at}[{remainder}]:{potential_port}{suffix}");
        }
        if is_ipv6(host_port) {
            return format!("{before_at}[{host_port}]{suffix}");
        }
    }
    raw.to_string()
}

/// 标准 `scheme://cred@host:port?params#name` 的公共头部。上游 `parseBase`。
///
/// 名称缺省回落 `address:port`。VMess（base64 JSON）/ SS（缺省 `Shadowsocks`）/ TUIC
/// （缺省 `TUIC Node`）名称/凭据形态各异，不走此助手。
struct Base {
    address: String,
    port: u16,
    params: Params,
    name: String,
}

fn parse_base(u: &Url, default_port: u16) -> Result<Base, String> {
    let address = strip_ipv6_brackets(u.host_str().unwrap_or(""));
    let port = u.port().unwrap_or(default_port);
    let params = Params::from_url(u);
    let frag = u.fragment().unwrap_or("");
    let decoded = decode_uri_component(frag)?;
    let name = if decoded.is_empty() {
        format!("{address}:{port}")
    } else {
        decoded
    };
    Ok(Base {
        address,
        port,
        params,
        name,
    })
}

/// 新建基础 ServerConfig（id 由注入闭包生成，对齐 上游 `randomUUID()`）。
fn new_config(
    id_gen: &mut impl FnMut() -> String,
    name: String,
    protocol: Protocol,
    address: String,
    port: u16,
) -> ServerConfig {
    ServerConfig {
        id: id_gen(),
        name,
        protocol,
        address,
        port,
        ..Default::default()
    }
}

// ── 传输层 / TLS ─────────────────────────────────────────────────────────────

/// URI `type=` 传输参数 → config.network + 对应 settings。上游 `parseTransportSettings`。
///
/// 别名映射委托 [`normalize_transport`]（单一真值）；此处只做「本形态（URLSearchParams）的字段抽取」。
/// 未知类型 → Err 整节点拒绝，消息保留 `不支持的传输层类型` 字样（用户定案靠日志搜此关键词）。
fn apply_transport_settings(
    config: &mut ServerConfig,
    params: &Params,
    raw_network: &str,
) -> Result<(), String> {
    let Some(net) = normalize_transport(raw_network) else {
        return Err(format!(
            "不支持的传输层类型: {}",
            raw_network.to_ascii_lowercase()
        ));
    };
    config.network = Some(net.to_string());
    match net {
        "ws" | "httpupgrade" => {
            config.ws_settings = Some(Box::new(parse_websocket_settings(params)))
        }
        "grpc" => config.grpc_settings = Some(parse_grpc_settings(params)),
        "http" => config.http_settings = Some(Box::new(parse_http_settings(params))),
        // tcp 无额外配置。
        _ => {}
    }
    Ok(())
}

fn parse_websocket_settings(params: &Params) -> WebSocketSettings {
    let mut s = WebSocketSettings::default();
    if let Some(path) = params.get_ne("path") {
        s.path = Some(path);
    }
    if let Some(host) = params.get_ne("host") {
        let mut h = std::collections::BTreeMap::new();
        h.insert("Host".to_string(), host);
        s.headers = Some(h);
    }
    if let Some(med) = params
        .get_ne("maxEarlyData")
        .as_deref()
        .and_then(js_parse_int)
    {
        s.max_early_data = Some(med as u32);
    }
    if let Some(n) = params.get_ne("earlyDataHeaderName") {
        s.early_data_header_name = Some(n);
    }
    s
}

fn parse_grpc_settings(params: &Params) -> GrpcSettings {
    let mut s = GrpcSettings::default();
    if let Some(sn) = params.get_ne("serviceName") {
        s.service_name = Some(sn);
    }
    if params.get("mode").as_deref() == Some("multi") {
        s.multi_mode = Some(true);
    }
    s
}

fn parse_http_settings(params: &Params) -> HttpSettings {
    let mut s = HttpSettings::default();
    if let Some(host) = params.get_ne("host") {
        s.host = Some(host.split(',').map(str::to_string).collect());
    }
    if let Some(path) = params.get_ne("path") {
        s.path = Some(path);
    }
    if let Some(m) = params.get_ne("method") {
        s.method = Some(m);
    }
    s
}

/// TLS 参数。上游 `parseTlsSettings`。
/// `fingerprint` 走 [`normalize_token`] 边界归一（R4；上游 #298 修的正是此字段大小写）。
fn parse_tls_settings(params: &Params) -> TlsSettings {
    let mut s = TlsSettings::default();
    if let Some(sni) = params
        .get_ne("sni")
        .or_else(|| params.get_ne("host"))
        .or_else(|| params.get_ne("peer"))
    {
        s.server_name = Some(sni);
    }
    // TS `a || b || c` 后 `!== null` 判定：前两项 falsy 穿透，末项原样取（含空串）。
    let allow_insecure = params
        .get_ne("allowInsecure")
        .or_else(|| params.get_ne("insecure"))
        .or_else(|| params.get("allow_insecure"));
    if let Some(v) = allow_insecure {
        s.allow_insecure = Some(v == "1" || v == "true");
    }
    if let Some(alpn) = params.get_ne("alpn") {
        s.alpn = Some(dedupe_trim(alpn.split(',').map(str::to_string)));
    }
    if let Some(fp) = params.get_ne("fp").or_else(|| params.get_ne("fingerprint")) {
        s.fingerprint = normalize_token(&fp);
    }
    s
}

/// Reality 参数。缺 `pbk` → None（调用方据此整节点拒绝）。上游 `parseRealitySettings`。
fn parse_reality_settings(params: &Params) -> Option<RealitySettings> {
    let public_key = params.get_ne("pbk")?;
    Some(RealitySettings {
        public_key,
        short_id: params.get_ne("sid"),
    })
}

/// `security=` → SecurityMode + 关联 settings（vless/trojan/anytls 共用尾部）。
///
/// **与 §L1 协同**：`security` 是 [`SecurityMode`] 枚举而非裸串 —— `security=TLS` 在 上游 会因
/// `=== 'tls'` 不命中而**静默不启用 TLS**（明文出站），此处归一后照常启用。脏值（`security=bogus`）
/// → [`SecurityMode::Unknown`] 保留原文、按非 TLS 处理、**不拒节点**。
fn apply_security(
    config: &mut ServerConfig,
    params: &Params,
    mode: SecurityMode,
) -> Result<(), String> {
    let is_tls = mode.is_tls();
    let is_reality = mode.is_reality();
    config.security = Some(mode);
    if is_tls || is_reality {
        config.tls_settings = Some(parse_tls_settings(params));
    }
    if is_reality {
        // 缺 pbk 的 reality 节点无法握手（builder 不生成 reality tls 块 → 退化裸 TCP 假节点），整节点拒绝。
        config.reality_settings = Some(
            parse_reality_settings(params)
                .ok_or_else(|| "Reality 节点缺少 pbk（public key）参数".to_string())?,
        );
    }
    Ok(())
}

// ── 各 scheme 解析 ───────────────────────────────────────────────────────────

/// vless://uuid@address:port?encryption=none&security=tls&type=ws&host=..&path=..#name
fn parse_vless(u: &Url, id_gen: &mut impl FnMut() -> String) -> Result<ServerConfig, String> {
    let uuid = u.username();
    let b = parse_base(u, 443)?;
    if uuid.is_empty() {
        return Err("VLESS URL 缺少 UUID".to_string());
    }
    let mut c = new_config(id_gen, b.name, Protocol::Vless, b.address, b.port);
    c.uuid = Some(uuid.to_string());
    c.encryption = Some(b.params.get_ne("encryption").unwrap_or("none".into()));
    // flow 走边界归一（R4）：`XTLS-RPRX-Vision` → `xtls-rprx-vision`，否则 sing-box FATAL。
    c.flow = b.params.get_ne("flow").and_then(|f| normalize_token(&f));
    if let Some(net) = b.params.get_ne("type") {
        apply_transport_settings(&mut c, &b.params, &net)?;
    }
    if let Some(sec) = b.params.get_ne("security") {
        apply_security(&mut c, &b.params, SecurityMode::from_raw(&sec))?;
    }
    Ok(c)
}

/// trojan://password@address:port?security=tls&type=ws#name（默认即 TLS）
fn parse_trojan(u: &Url, id_gen: &mut impl FnMut() -> String) -> Result<ServerConfig, String> {
    let password = decode_uri_component(u.username())?;
    let b = parse_base(u, 443)?;
    let mut c = new_config(id_gen, b.name, Protocol::Trojan, b.address, b.port);
    c.password = Some(password);
    if let Some(net) = b.params.get_ne("type") {
        apply_transport_settings(&mut c, &b.params, &net)?;
    }
    // Trojan 默认即 TLS。
    let mode = b
        .params
        .get_ne("security")
        .map_or(SecurityMode::Tls, |s| SecurityMode::from_raw(&s));
    apply_security(&mut c, &b.params, mode)?;
    Ok(c)
}

/// hysteria2://password@address:port?obfs=salamander&obfs-password=..&sni=..#name（必 TLS）
fn parse_hysteria2(
    u: &Url,
    id_gen: &mut impl FnMut() -> String,
    warn: &mut impl FnMut(String),
) -> Result<ServerConfig, String> {
    let password = decode_uri_component(u.username())?;
    let b = parse_base(u, 443)?;
    if password.is_empty() {
        return Err("Hysteria2 URL 缺少密码".to_string());
    }
    let mut c = new_config(id_gen, b.name, Protocol::Hysteria2, b.address, b.port);
    c.password = Some(password);
    c.security = Some(SecurityMode::Tls); // Hysteria2 协议必须使用 TLS

    let mut hy2 = Hysteria2Settings::default();
    if let Some(up) = b
        .params
        .get_ne("up_mbps")
        .or_else(|| b.params.get_ne("up"))
        .as_deref()
        .and_then(js_parse_int)
    {
        hy2.up_mbps = Some(up as u32);
    }
    if let Some(down) = b
        .params
        .get_ne("down_mbps")
        .or_else(|| b.params.get_ne("down"))
        .as_deref()
        .and_then(js_parse_int)
    {
        hy2.down_mbps = Some(down as u32);
    }
    let obfs = b.params.get_ne("obfs");
    let obfs_password = b.params.get_ne("obfs-password");
    match (obfs.as_deref(), obfs_password) {
        (Some("salamander"), Some(pw)) => {
            hy2.obfs = Some(Hysteria2ObfsSettings {
                type_field: Some("salamander".to_string()),
                password: Some(pw),
                ..Default::default()
            });
        }
        (Some("salamander"), None) => {
            // 声明 salamander 即服务端强制混淆，缺 obfs-password 剥离后裸连必死（假节点）——
            // 与 reality 缺 pbk 同判据（凭据残缺=整节点拒绝）。
            return Err("Hysteria2 obfs=salamander 缺少 obfs-password".to_string());
        }
        (Some(other), _) => {
            // 非 salamander 值（sing-box hy2 无对应混淆类型/参数噪声）：剥离 + warn 留痕。
            warn(format!(
                "Hysteria2 节点 \"{}\" 的 obfs 参数（{other}）不受支持，已忽略混淆配置",
                c.name
            ));
        }
        (None, _) => {}
    }
    if let Some(net) = b.params.get_ne("network") {
        hy2.network = Some(net);
    }
    if hy2 != Hysteria2Settings::default() {
        c.hysteria2_settings = Some(Box::new(hy2));
    }

    let mut tls = parse_tls_settings(&b.params);
    if tls.server_name.is_none() {
        // 必开 TLS：parse_tls_settings 没解析到 SNI 时兜底。
        tls.server_name = Some(
            b.params
                .get_ne("sni")
                .or_else(|| b.params.get_ne("peer"))
                .unwrap_or_else(|| c.address.clone()),
        );
    }
    c.tls_settings = Some(tls);
    Ok(c)
}

/// tuic://uuid:password@address:port?congestion_control=bbr&alpn=h3&sni=..#name
fn parse_tuic(u: &Url, id_gen: &mut impl FnMut() -> String) -> Result<ServerConfig, String> {
    let params = Params::from_url(u);
    let name = decode_uri_component(u.fragment().unwrap_or(""))?;

    // 凭据形态 tuic://uuid:password@...；password 含未编码 ':' 时全部拼回。
    let raw_cred = if u.password().is_some() {
        format!("{}:{}", u.username(), u.password().unwrap_or(""))
    } else {
        u.username().to_string()
    };
    let credentials = decode_uri_component(&raw_cred)?;
    let (uuid, password) = match credentials.split_once(':') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (credentials, String::new()),
    };
    if uuid.is_empty() || password.is_empty() {
        return Err("TUIC 协议缺少 uuid 或 password".to_string());
    }
    // TS 侧 `parseInt(url.port,10)` 无缺省 → 缺端口产出 NaN（JSON 里成 null）的坏节点。
    // 上游 `port: u16` 不可表示 NaN → 显式拒绝（可见丢弃 > 静默坏节点）。
    let port = u.port().ok_or_else(|| "TUIC URL 缺少端口".to_string())?;

    let mut c = new_config(
        id_gen,
        if name.is_empty() {
            "TUIC Node".to_string()
        } else {
            name
        },
        Protocol::Tuic,
        strip_ipv6_brackets(u.host_str().unwrap_or("")),
        port,
    );
    c.uuid = Some(uuid);
    c.password = Some(password);
    c.network = Some("tcp".to_string());
    c.security = Some(SecurityMode::Tls);
    c.tuic_settings = Some(TuicSettings::default());

    let mut tls = parse_tls_settings(&params);
    if tls.server_name.is_none() {
        tls.server_name = Some(
            params
                .get_ne("sni")
                .unwrap_or_else(|| u.host_str().unwrap_or("").to_string()),
        );
    }
    // alpn 逗号串拆分后统一 dedupe_trim（与 VMess/parse_tls_settings 口径一致）。
    if let Some(alpn) = params.get_ne("alpn") {
        tls.alpn = Some(dedupe_trim(alpn.split(',').map(str::to_string)));
    }
    c.tls_settings = Some(tls);

    let t = c.tuic_settings.as_mut().expect("刚置 Some");
    if let Some(cc) = params.get("congestion_control") {
        if matches!(cc.as_str(), "bbr" | "cubic" | "new_reno") {
            t.congestion_control = Some(cc);
        }
    }
    if let Some(m) = params.get("udp_relay_mode") {
        if matches!(m.as_str(), "native" | "quic") {
            t.udp_relay_mode = Some(m);
        }
    }
    // heartbeat 经 normalize_duration：订阅写裸毫秒整数（"10000"）补 ms 单位，防 sing-box "missing unit"。
    if let Some(hb) = normalize_duration(params.get("heartbeat").as_deref()) {
        t.heartbeat = Some(hb);
    }
    if let Some(z) = params.get_ne("zero_rtt_handshake") {
        t.zero_rtt_handshake = Some(z == "1" || z == "true");
    }
    Ok(c)
}

/// anytls://password@address:port?security=tls&sni=..#name（默认即 TLS）
fn parse_anytls(u: &Url, id_gen: &mut impl FnMut() -> String) -> Result<ServerConfig, String> {
    let password = decode_uri_component(u.username())?;
    let b = parse_base(u, 443)?;
    if password.is_empty() {
        return Err("AnyTLS URL 缺少密码".to_string());
    }
    let mut c = new_config(id_gen, b.name, Protocol::Anytls, b.address, b.port);
    c.password = Some(password);

    let a = AnyTlsSettings {
        idle_session_check_interval: normalize_duration(
            b.params.get("idle_session_check_interval").as_deref(),
        ),
        idle_session_timeout: normalize_duration(b.params.get("idle_session_timeout").as_deref()),
        // min_idle_session 口径：min=0 是合法值（须保留），非数字串必须丢弃（不得写成 NaN）。
        min_idle_session: b
            .params
            .get("min_idle_session")
            .filter(|s| !s.trim().is_empty())
            .and_then(|s| s.trim().parse::<u32>().ok()),
    };
    if a != AnyTlsSettings::default() {
        c.any_tls_settings = Some(a);
    }

    match b.params.get_ne("security") {
        Some(sec) => apply_security(&mut c, &b.params, SecurityMode::from_raw(&sec))?,
        // AnyTLS 默认就是 TLS。
        None => apply_security(&mut c, &b.params, SecurityMode::Tls)?,
    }
    Ok(c)
}

/// snell://psk@address:port?version=4&obfs=http&obfs-host=..#name
///
/// version 缺省 4；sing-box 官方 snell 仅支持 4/6（v1-3 为 Surge/mihomo 旧协议版本），
/// 版本/混淆超出能力（如 obfs=tls）→ 整节点拒绝（入库也连不上，防假节点）。
fn parse_snell(u: &Url, id_gen: &mut impl FnMut() -> String) -> Result<ServerConfig, String> {
    // psk 含未编码 ':' 时 URL 引擎会拆成 username:password——两段拼回，避免 psk 静默截断成假节点。
    let raw_psk = match u.password() {
        Some(p) => format!("{}:{}", u.username(), p),
        None => u.username().to_string(),
    };
    let psk = decode_uri_component(&raw_psk)?;
    let b = parse_base(u, 443)?;
    if psk.trim().is_empty() {
        // trim 语义与 completeness 闸门（password?.trim()）对齐。
        return Err("Snell URL 缺少 psk".to_string());
    }
    let raw_version = b
        .params
        .get("version")
        .or_else(|| b.params.get("v"))
        .unwrap_or_else(|| "4".to_string());
    let version = js_parse_int(&raw_version).unwrap_or(i64::MIN);
    if version != 4 && version != 6 {
        return Err(format!(
            "Snell 版本不受支持（sing-box 仅支持 4/6）: {raw_version}"
        ));
    }
    let mut snell = SnellSettings {
        version: version as u32,
        ..Default::default()
    };
    let obfs = b
        .params
        .get("obfs")
        .or_else(|| b.params.get("obfs-mode"))
        .map(|s| s.to_lowercase());
    if let Some(o) = obfs.as_deref().filter(|o| !o.is_empty() && *o != "none") {
        if version == 4 && o == "http" {
            snell.obfs_mode = Some("http".to_string());
            if let Some(host) = b.params.get_ne("obfs-host") {
                snell.obfs_host = Some(host);
            }
        } else {
            // v4 tls 混淆（Surge 旧形态）/ v6 带 obfs：sing-box snell 无对应能力。
            return Err(format!("Snell obfs 不受支持: {o}"));
        }
    }
    if version == 6 {
        if let Some(m) = b.params.get("mode") {
            if matches!(m.as_str(), "unshaped" | "unsafe-raw") {
                snell.mode = Some(m);
            }
        }
    }
    if matches!(b.params.get("reuse").as_deref(), Some("1") | Some("true")) {
        snell.reuse = Some(true);
    }
    if let Some(n) = b.params.get("network") {
        if matches!(n.as_str(), "tcp" | "udp") {
            snell.network = Some(n);
        }
    }
    if let Some(uk) = b.params.get_ne("userkey") {
        snell.userkey = Some(uk);
    }

    let mut c = new_config(id_gen, b.name, Protocol::Snell, b.address, b.port);
    c.password = Some(psk); // psk 复用 password 字段（同 trojan/hysteria2 惯例）
    c.snell_settings = Some(Box::new(snell));
    Ok(c)
}

/// ss://base64(method:password)@server:port#remarks 或 SIP002 ss://method:password@server:port?plugin=..
fn parse_shadowsocks(
    u: &Url,
    id_gen: &mut impl FnMut() -> String,
    warn: &mut impl FnMut(String),
) -> Result<ServerConfig, String> {
    let name = decode_uri_component(u.fragment().unwrap_or(""))?;
    // TS 侧 `parseInt(urlObj.port,10)` 无缺省 → 缺端口产出 NaN 坏节点；u16 不可表示 → 显式拒绝。
    let port = u
        .port()
        .ok_or_else(|| "Shadowsocks URL 缺少端口".to_string())?;
    let mut c = new_config(
        id_gen,
        if name.is_empty() {
            "Shadowsocks".to_string()
        } else {
            name
        },
        Protocol::Shadowsocks,
        strip_ipv6_brackets(u.host_str().unwrap_or("")),
        port,
    );

    let user_info = u.username();
    if user_info.is_empty() {
        return Err("Shadowsocks URL 缺少加密信息".to_string());
    }
    let (method, password) = decode_ss_userinfo(user_info, u.password())
        .map_err(|e| format!("Shadowsocks URL 格式错误: {e}"))?;
    c.shadowsocks_settings = Some(Box::new(ShadowsocksSettings {
        method,
        password,
        ..Default::default()
    }));

    let params = Params::from_url(u);
    if let Some(plugin) = params.get_ne("plugin") {
        let mut parts = plugin.splitn(2, ';');
        let plugin_name = parts.next().unwrap_or("").to_string();
        let opts = parts.next();
        let ss = c.shadowsocks_settings.as_mut().expect("刚置 Some");
        ss.plugin = Some(plugin_name.clone());
        if let Some(o) = opts {
            ss.plugin_opts = Some(o.to_string());
        }
        if plugin_name.contains("shadow-tls") {
            // Shadow-TLS v3 常用参数。
            if let (Some(pw), Some(sni)) = (
                params.get_ne("shadow-tls-password"),
                params.get_ne("shadow-tls-sni"),
            ) {
                c.shadow_tls_settings = Some(ShadowTlsSettings {
                    password: pw,
                    sni,
                    fingerprint: normalize_token(
                        &params.get_ne("shadow-tls-fp").unwrap_or("chrome".into()),
                    ),
                    port: params
                        .get_ne("shadow-tls-port")
                        .as_deref()
                        .and_then(js_parse_int)
                        .map(|p| p as u16),
                });
            }
        }
    }
    // 无 plugin 字段、直接以 shadow-tls-* 查询参数传递的形态。
    if c.shadow_tls_settings.is_none() {
        if let (Some(pw), Some(sni)) = (
            params.get_ne("shadow-tls-password"),
            params.get_ne("shadow-tls-sni"),
        ) {
            c.shadow_tls_settings = Some(ShadowTlsSettings {
                password: pw,
                sni,
                fingerprint: normalize_token(
                    &params.get_ne("shadow-tls-fp").unwrap_or("chrome".into()),
                ),
                port: params
                    .get_ne("shadow-tls-port")
                    .as_deref()
                    .and_then(js_parse_int)
                    .map(|p| p as u16),
            });
        }
    }
    // shadow-tls 参数携 base64(JSON) 配置的形态。
    if let Some(raw) = params.get_ne("shadow-tls") {
        match crate::subscription::base64_decode(&raw)
            .ok()
            .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
        {
            Some(v) => {
                let pw = v.get("password").and_then(|x| x.as_str());
                let host = v.get("host").and_then(|x| x.as_str());
                if let (Some(pw), Some(host)) = (pw, host) {
                    c.shadow_tls_settings = Some(ShadowTlsSettings {
                        password: pw.to_string(),
                        sni: host.to_string(),
                        fingerprint: Some("chrome".to_string()),
                        port: v.get("port").and_then(json_num).map(|p| p as u16),
                    });
                }
            }
            None => warn(format!("解析 shadow-tls Base64 JSON 参数失败: {raw}")),
        }
    }
    Ok(c)
}

/// ss userinfo → (method, password)。先试 base64（传统形态），失败落 SIP002 明文。
/// 上游 `parseShadowsocks` 的 userInfo 分支。
fn decode_ss_userinfo(
    user_info: &str,
    user_password: Option<&str>,
) -> Result<(String, String), String> {
    // ① base64 形态：ss://base64(method:password)@server:port
    if let Ok(decoded_ui) = decode_uri_component(user_info) {
        if let Ok(decoded) = crate::subscription::base64_decode(&decoded_ui) {
            // 解码结果须是可打印 ASCII（避免乱码被当成明文）且含 ':'。
            let printable =
                !decoded.is_empty() && decoded.bytes().all(|b| (0x20..=0x7E).contains(&b));
            if printable {
                if let Some((m, p)) = decoded.split_once(':') {
                    let (m, p) = (m.trim().to_string(), p.trim().to_string());
                    if m.is_empty() {
                        return Err("加密方法为空".to_string());
                    }
                    if p.is_empty() {
                        return Err("密码为空".to_string());
                    }
                    return Ok((m, p));
                }
            }
        }
    }
    // ② SIP002 明文形态：ss://method:password@server:port
    let (method, password) = if let Some(up) = user_password {
        (
            decode_uri_component(user_info)?.trim().to_string(),
            decode_uri_component(up)?.trim().to_string(),
        )
    } else if let Some((a, b)) = user_info.split_once(':') {
        // 有些实现把 method:password 都放在 username 里。
        (
            decode_uri_component(a)?.trim().to_string(),
            decode_uri_component(b)?.trim().to_string(),
        )
    } else {
        return Err("无法解析加密方法和密码：既不是 Base64 编码，也不是明文格式".to_string());
    };
    if method.is_empty() {
        return Err("加密方法为空".to_string());
    }
    if password.is_empty() {
        return Err("密码为空".to_string());
    }
    Ok((method, password))
}

/// http2://username:password@address:port#name（naive 族）
fn parse_naive(u: &Url, id_gen: &mut impl FnMut() -> String) -> Result<ServerConfig, String> {
    let username = decode_uri_component(u.username())?;
    let password = decode_uri_component(u.password().unwrap_or(""))?;
    let b = parse_base(u, 443)?;
    if username.is_empty() || password.is_empty() {
        return Err("NaiveProxy URL 缺少用户名或密码".to_string());
    }
    let mut c = new_config(id_gen, b.name, Protocol::Naive, b.address, b.port);
    c.username = Some(username);
    c.password = Some(password);
    if let Some(net) = b.params.get_ne("type") {
        apply_transport_settings(&mut c, &b.params, &net)?;
    }
    c.security = Some(SecurityMode::Tls);
    let mut tls = parse_tls_settings(&b.params);
    if tls.server_name.is_none() {
        tls.server_name = Some(c.address.clone());
    }
    c.tls_settings = Some(tls);
    Ok(c)
}

/// socks5://username:password@address:port#name
fn parse_socks(u: &Url, id_gen: &mut impl FnMut() -> String) -> Result<ServerConfig, String> {
    let b = parse_base(u, 1080)?;
    let (username, password) = split_userinfo(u)?;
    let mut c = new_config(id_gen, b.name, Protocol::Socks, b.address, b.port);
    c.username = username;
    c.password = password;
    c.network = Some("tcp".to_string());
    c.security = Some(SecurityMode::None);
    Ok(c)
}

/// http://username:password@address:port#name 或 https://...
fn parse_http(
    u: &Url,
    is_https: bool,
    id_gen: &mut impl FnMut() -> String,
) -> Result<ServerConfig, String> {
    let b = parse_base(u, if is_https { 443 } else { 80 })?;
    let (username, password) = split_userinfo(u)?;
    let mut c = new_config(id_gen, b.name, Protocol::Http, b.address, b.port);
    c.username = username;
    c.password = password;
    c.network = Some("tcp".to_string());
    c.security = Some(if is_https {
        SecurityMode::Tls
    } else {
        SecurityMode::None
    });
    if is_https {
        let mut tls = parse_tls_settings(&b.params);
        if tls.server_name.is_none() {
            tls.server_name = Some(c.address.clone());
        }
        c.tls_settings = Some(tls);
    }
    Ok(c)
}

/// socks/http 共用：`user:pass` 拆分（含未编码 ':' 时 password 段全部拼回）。
/// 缺 username → 两者皆 None（对齐 TS `urlObj.username ? ... : undefined`）。
fn split_userinfo(u: &Url) -> Result<(Option<String>, Option<String>), String> {
    let raw = match u.password() {
        Some(p) => format!("{}:{}", u.username(), p),
        None => u.username().to_string(),
    };
    let cred = decode_uri_component(&raw)?;
    let username = if u.username().is_empty() {
        None
    } else {
        Some(cred.split(':').next().unwrap_or("").to_string())
    };
    let password = if u.password().is_some() {
        Some(cred.split(':').skip(1).collect::<Vec<_>>().join(":"))
    } else {
        None
    };
    Ok((username, password))
}

/// serde_json 数值规整（数字或数字串）。
fn json_num(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        serde_json::Value::String(s) => js_parse_int(s),
        _ => None,
    }
}

/// serde_json 字符串规整（字符串或数字 → String）。
fn json_str(v: Option<&serde_json::Value>) -> Option<String> {
    match v? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// vmess://base64(json)（V2RayN 格式）。上游 `parseVmess`。
///
/// 传输别名与 URI 路径共用 [`normalize_transport`]（上游 此处是第二份表 → 已收敛）。
fn parse_vmess(raw_url: &str, id_gen: &mut impl FnMut() -> String) -> Result<ServerConfig, String> {
    let b64 = &raw_url[8..]; // 去 'vmess://'
    let json_str_body = crate::subscription::base64_decode(b64)
        .map_err(|()| "VMess base64 解码失败".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&json_str_body).map_err(|e| format!("VMess JSON 解析失败: {e}"))?;

    let address = json_str(v.get("add")).unwrap_or_default();
    let port = json_str(v.get("port"))
        .as_deref()
        .and_then(js_parse_int)
        .unwrap_or(0);
    let uuid = json_str(v.get("id")).unwrap_or_default();
    if address.is_empty() || port == 0 || uuid.is_empty() {
        return Err("VMess 配置信息不完整".to_string());
    }
    let port = u16::try_from(port).map_err(|_| "VMess 端口越界".to_string())?;
    let name = json_str(v.get("ps")).unwrap_or_default();
    let name = if name.is_empty() {
        format!("{address}:{port}")
    } else {
        name
    };

    let mut c = new_config(id_gen, name, Protocol::Vmess, address.clone(), port);
    c.uuid = Some(uuid);
    c.alter_id = Some(
        json_str(v.get("aid"))
            .as_deref()
            .and_then(js_parse_int)
            .unwrap_or(0) as u32,
    );
    // vmessSecurity 走边界归一（R4）：`AES-128-GCM` → `aes-128-gcm`，否则 sing-box FATAL。
    c.vmess_security = normalize_token(&json_str(v.get("scy")).unwrap_or("auto".into()))
        .or_else(|| Some("auto".to_string()));

    let raw_net = json_str(v.get("net")).unwrap_or("tcp".into());
    let net = normalize_transport(&raw_net)
        .ok_or_else(|| format!("不支持的传输层类型: {}", raw_net.to_lowercase()))?;
    c.network = Some(net.to_string());
    let path = json_str(v.get("path"));
    let host = json_str(v.get("host"));
    match net {
        // httpupgrade 与 ws 同款 path/host 承载（sing-box 支持 vmess+httpupgrade）。
        "ws" | "httpupgrade" => {
            c.ws_settings = Some(Box::new(WebSocketSettings {
                path: Some(path.unwrap_or("/".into())),
                headers: host.map(|h| {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert("Host".to_string(), h);
                    m
                }),
                ..Default::default()
            }));
        }
        "http" => {
            c.http_settings = Some(Box::new(HttpSettings {
                path: Some(path.unwrap_or("/".into())),
                host: host.map(|h| h.split(',').map(str::to_string).collect()),
                ..Default::default()
            }));
        }
        "grpc" => {
            c.grpc_settings = Some(GrpcSettings {
                service_name: Some(path.unwrap_or_default()),
                multi_mode: None,
            });
        }
        // tcp 无额外配置。
        _ => {}
    }

    // TLS：上游 此处是 `vmessData.tls === 'tls'` 裸串严格比较 → `"tls":"TLS"` 会**静默明文**
    // （K3-1 同一事故类）。归一到 SecurityMode 后大小写变体照常启用 TLS。
    let tls_mode = SecurityMode::from_raw(&json_str(v.get("tls")).unwrap_or_default());
    if tls_mode.is_tls() {
        c.security = Some(SecurityMode::Tls);
        let mut tls = TlsSettings {
            server_name: Some(
                json_str(v.get("sni"))
                    .filter(|s| !s.is_empty())
                    .or_else(|| json_str(v.get("host")).filter(|s| !s.is_empty()))
                    .unwrap_or(address),
            ),
            allow_insecure: Some(
                v.get("insecure").and_then(|x| x.as_bool()) == Some(true)
                    || v.get("insecure").and_then(|x| x.as_i64()) == Some(1)
                    || v.get("allowInsecure").and_then(|x| x.as_bool()) == Some(true),
            ),
            fingerprint: normalize_token(&json_str(v.get("fp")).unwrap_or("chrome".into()))
                .or_else(|| Some("chrome".to_string())),
            ..Default::default()
        };
        // alpn 可能是逗号串或已是数组；两路都过 dedupe_trim（保序去重 + 丢空白项）。
        if let Some(alpn_v) = v.get("alpn") {
            let raw: Vec<String> = match alpn_v {
                serde_json::Value::Array(a) => a
                    .iter()
                    .map(|x| json_str(Some(x)).unwrap_or_else(|| x.to_string()))
                    .collect(),
                serde_json::Value::Null => vec![],
                other => json_str(Some(other))
                    .unwrap_or_default()
                    .split(',')
                    .map(str::to_string)
                    .collect(),
            };
            if !raw.is_empty() {
                tls.alpn = Some(dedupe_trim(raw));
            }
        }
        c.tls_settings = Some(tls);
    } else {
        c.security = Some(tls_mode);
    }
    Ok(c)
}

// ── 入口 ─────────────────────────────────────────────────────────────────────

/// 解析单条分享链接 → [`ServerConfig`]。上游 `ProtocolParser.parseUrl`。
///
/// `id_gen` 注入 UUID 生成（对齐 上游 `randomUUID`）；`warn` 收集非致命告警（hy2 obfs 剥离等）。
/// 失败返回 Err（调用方逐行 catch 聚合，单条坏链不影响整份订阅）。
pub fn parse_share_url(
    url: &str,
    id_gen: &mut impl FnMut() -> String,
    warn: &mut impl FnMut(String),
) -> Result<ServerConfig, String> {
    if !is_supported_share_url(url) {
        return Err(format!(
            "不支持的协议: {}",
            url.split("://").next().unwrap_or(url)
        ));
    }
    parse_share_url_inner(url, id_gen, warn).map_err(|e| format!("URL 解析失败: {e}"))
}

fn parse_share_url_inner(
    url: &str,
    id_gen: &mut impl FnMut() -> String,
    warn: &mut impl FnMut(String),
) -> Result<ServerConfig, String> {
    // 裸 IPv6（无方括号）预处理泛化到全协议：vless/trojan/hy2 等裸 IPv6 链接会在 URL 解析直接
    // 报错整节点丢（issue #263）。无 '@' 的形态（vmess base64）原样返回。
    let url = preprocess_bare_ipv6(url);
    let scheme = url.split("://").next().unwrap_or("");

    // vmess 只消费原始串（base64 JSON），不经 Url::parse —— 其 base64 体含 '='/'/'，
    // 让 URL 引擎去解析它既无收益又平添形态依赖。
    if scheme == "vmess" {
        return parse_vmess(&url, id_gen);
    }

    let u = Url::parse(&url).map_err(|e| format!("Invalid URL: {e}"))?;
    match scheme {
        "vless" => parse_vless(&u, id_gen),
        "trojan" => parse_trojan(&u, id_gen),
        "hysteria2" | "hy2" => parse_hysteria2(&u, id_gen, warn),
        "ss" => parse_shadowsocks(&u, id_gen, warn),
        "anytls" => parse_anytls(&u, id_gen),
        "snell" => parse_snell(&u, id_gen),
        "tuic" => parse_tuic(&u, id_gen),
        // naive 别名族：naive:// / http2:// / naive+https://，以及部分客户端的 https://..#naive。
        "naive" | "http2" | "naive+https" => parse_naive(&u, id_gen),
        "https" if u.fragment().unwrap_or("").to_lowercase().contains("naive") => {
            parse_naive(&u, id_gen)
        }
        "socks" | "socks5" | "s5" => parse_socks(&u, id_gen),
        "http" => parse_http(&u, false, id_gen),
        "https" => parse_http(&u, true, id_gen),
        other => Err(format!("不支持的协议: {other}")),
    }
}

/// 解析 url-list 形态订阅正文（每行一条分享链接）→ 节点集 + 统计。
///
/// 逐行 try/catch（对齐 `parse_clash_proxies`）：单条坏链只计 failed + 聚合告警，不影响其余。
/// 非分享链接行（注释/空行/说明文字）静默跳过——机场订阅常夹带 `#` 注释与空行。
pub fn parse_url_list(
    text: &str,
    subscription_id: &str,
    now: &str,
    id_gen: &mut impl FnMut() -> String,
) -> crate::clash_parser::ClashParseResult {
    let mut r = crate::clash_parser::ClashParseResult::default();
    let mut warn = |m: String| r.warnings.push(m);
    let mut servers = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !is_supported_share_url(line) {
            // 非分享链接：不是「解析失败」，是本行不含节点（注释/说明），静默跳过。
            continue;
        }
        match parse_share_url(line, id_gen, &mut warn) {
            Ok(mut c) => {
                c.subscription_id = Some(subscription_id.to_string());
                c.created_at = Some(now.to_string());
                c.updated_at = Some(now.to_string());
                servers.push(c);
            }
            Err(e) => {
                r.failed += 1;
                warn(e);
            }
        }
    }
    r.servers = servers;
    r.finish()
}

// ── 编码（[`parse_share_url`] 的逆）────────────────────────────────────────────
//
// 生成前端 `SERVER_GENERATE_URL` 用的真实 share link。**与上方解析器逐格式对称**：
// `parse_share_url(encode_share_url(cfg))` ≈ cfg（id 重生、name 缺省回落、security 归一等
// 语义等价项除外）。round-trip 单测逐协议锁死（见 `share_link/tests.rs`）。
//
// 编码器不做 SSRF/可达性判定——它只把已在库的 [`ServerConfig`] 序列化成 URI 文法，
// 与解析侧一样把节点地址当合法数据（LAN/mesh 节点亦可分享）。

/// `encodeURIComponent` 等价：仅保留 RFC3986 unreserved（`A-Za-z0-9-_.~`），其余逐字节 `%XX`。
///
/// 与解析侧 [`decode_uri_component`] 对称（后者只解 `%XX`）：本函数产出的每个转义都能被它还原，
/// 且**绝不吐裸 `+`**（查询串走 `URLSearchParams`/form 解码时 `+`=空格，会篡改含 `+` 的密码/参数）。
fn percent_encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(
                char::from_digit(u32::from(b >> 4), 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
            out.push(
                char::from_digit(u32::from(b & 0x0f), 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
        }
    }
    out
}

/// base64 编码（`url_safe`/`pad` 可选，无新依赖）。vmess 用标准+padding（V2RayN 惯例）；
/// ss userinfo 用 URL-safe 无 padding（`+`/`/` 在 userinfo 位破坏 URL 文法 → 必须 `-`/`_`）。
/// 二者均能被 [`crate::subscription::base64_decode`] 还原（它兼容两套字母表 + 自动补 padding）。
fn base64_encode(input: &[u8], url_safe: bool, pad: bool) -> String {
    const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let t = if url_safe { URL } else { STD };
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(t[((n >> 18) & 0x3f) as usize] as char);
        out.push(t[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(t[((n >> 6) & 0x3f) as usize] as char);
        } else if pad {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(t[(n & 0x3f) as usize] as char);
        } else if pad {
            out.push('=');
        }
    }
    out
}

/// host 格式化：裸 IPv6 加方括号（与解析侧 [`strip_ipv6_brackets`] 对称）。
fn format_host(address: &str) -> String {
    if address.contains(':') && !address.starts_with('[') && is_ipv6(address) {
        format!("[{address}]")
    } else {
        address.to_string()
    }
}

/// 有序查询串构造：键为受控 ASCII 字面量（原样），值经 [`percent_encode_component`]。
#[derive(Default)]
struct QueryString(Vec<(&'static str, String)>);

impl QueryString {
    fn new() -> Self {
        Self::default()
    }
    fn push(&mut self, key: &'static str, value: impl Into<String>) {
        self.0.push((key, value.into()));
    }
    /// 仅当 `Some` 且非空时追加（空串参数对解析侧 `get_ne` 等价缺席，省得写噪声）。
    fn push_opt(&mut self, key: &'static str, value: Option<String>) {
        if let Some(v) = value {
            if !v.is_empty() {
                self.0.push((key, v));
            }
        }
    }
    fn finish(&self) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let mut s = String::from("?");
        for (i, (k, v)) in self.0.iter().enumerate() {
            if i > 0 {
                s.push('&');
            }
            s.push_str(k);
            s.push('=');
            s.push_str(&percent_encode_component(v));
        }
        s
    }
}

/// 组装 `scheme://[userinfo@]host:port[?query][#name]`（port 恒显式写入——
/// 对 special scheme（http/https）url crate 会归一掉默认端口，解析侧 `default_port` 兜回，round-trip 无损）。
fn assemble(
    scheme: &str,
    userinfo: Option<String>,
    host: &str,
    port: u16,
    query: &QueryString,
    name: &str,
) -> String {
    let mut s = String::new();
    s.push_str(scheme);
    s.push_str("://");
    if let Some(ui) = userinfo {
        s.push_str(&ui);
        s.push('@');
    }
    s.push_str(&format_host(host));
    s.push(':');
    s.push_str(&port.to_string());
    s.push_str(&query.finish());
    if !name.is_empty() {
        s.push('#');
        s.push_str(&percent_encode_component(name));
    }
    s
}

/// 追加 TLS 查询参数（sni/alpn/fp/allowInsecure），与 [`parse_tls_settings`] 读取项对称。
fn push_tls_params(q: &mut QueryString, tls: &TlsSettings) {
    q.push_opt("sni", tls.server_name.clone());
    if let Some(alpn) = &tls.alpn {
        if !alpn.is_empty() {
            q.push("alpn", alpn.join(","));
        }
    }
    q.push_opt("fp", tls.fingerprint.clone());
    if tls.allow_insecure == Some(true) {
        q.push("allowInsecure", "1");
    }
}

/// 追加传输层查询参数（`type=` + 各传输专属字段），与 [`apply_transport_settings`] 读取项对称。
fn push_transport_params(q: &mut QueryString, c: &ServerConfig) {
    let Some(net) = c.network.as_deref() else {
        return;
    };
    q.push("type", net.to_string());
    match net {
        "ws" | "httpupgrade" => {
            if let Some(ws) = &c.ws_settings {
                q.push_opt("path", ws.path.clone());
                if let Some(h) = ws.headers.as_ref().and_then(|m| m.get("Host")) {
                    q.push("host", h.clone());
                }
                if let Some(med) = ws.max_early_data {
                    q.push("maxEarlyData", med.to_string());
                }
                q.push_opt("earlyDataHeaderName", ws.early_data_header_name.clone());
            }
        }
        "grpc" => {
            if let Some(g) = &c.grpc_settings {
                q.push_opt("serviceName", g.service_name.clone());
                if g.multi_mode == Some(true) {
                    q.push("mode", "multi");
                }
            }
        }
        "http" => {
            if let Some(h) = &c.http_settings {
                if let Some(hosts) = &h.host {
                    if !hosts.is_empty() {
                        q.push("host", hosts.join(","));
                    }
                }
                q.push_opt("path", h.path.clone());
                q.push_opt("method", h.method.clone());
            }
        }
        _ => {}
    }
}

/// 追加 `security=` + 关联 TLS/Reality 参数（vless/trojan/anytls 尾部共用，与 [`apply_security`] 对称）。
fn push_security_params(q: &mut QueryString, c: &ServerConfig, mode: &SecurityMode) {
    q.push("security", mode.as_str().to_string());
    if mode.is_tls() || mode.is_reality() {
        if let Some(tls) = &c.tls_settings {
            push_tls_params(q, tls);
        }
    }
    if mode.is_reality() {
        if let Some(r) = &c.reality_settings {
            q.push("pbk", r.public_key.clone());
            q.push_opt("sid", r.short_id.clone());
        }
    }
}

/// 生成节点分享链接（上游 `ProtocolParser.generateUrl` 等价）。
///
/// 支持 vless/vmess/trojan/ss/hysteria2/tuic/anytls/snell/socks/http(s)/naive。
/// WireGuard/Tailscale/SSH/Custom 无标准 share-link 文法 → `Err`（诚实拒绝，不编造 `polaris://` 假链）。
pub fn encode_share_url(c: &ServerConfig) -> Result<String, String> {
    match c.protocol {
        // hysteria v1 野外确有 `hysteria://` 文法，但**各家不兼容**（auth/upmbps/obfs 的键名与
        // 大小写各写各的），编一个只有本仓认得的链等于制造一条假的互操作承诺。
        // tor 没有 server/port，连「指向某个服务器」这个 share-link 的前提都不成立。
        // 与 WireGuard/Tailscale/SSH/Custom 同口径：诚实拒绝。
        Protocol::Openconnect => Err("openconnect 无标准分享链接文法".into()),
        Protocol::OpenvpnClient => {
            Err("openvpn-client 的凭据是证书材料，不适合塞进分享链接".into())
        }
        Protocol::Hysteria => Err("hysteria (v1) 无标准分享链接文法".into()),
        Protocol::Tor => Err("tor 无 server/port，分享链接的前提不成立".into()),
        Protocol::Vless => encode_vless(c),
        Protocol::Vmess => encode_vmess(c),
        Protocol::Trojan => encode_trojan(c),
        Protocol::Shadowsocks => encode_shadowsocks(c),
        Protocol::Hysteria2 => encode_hysteria2(c),
        Protocol::Tuic => encode_tuic(c),
        Protocol::Anytls => encode_anytls(c),
        Protocol::Snell => encode_snell(c),
        Protocol::Socks => Ok(encode_socks(c)),
        Protocol::Http => Ok(encode_http(c)),
        Protocol::Naive => encode_naive(c),
        Protocol::Wireguard | Protocol::Tailscale | Protocol::Ssh | Protocol::Custom => {
            Err(format!(
                "{:?} 协议无标准分享链接形态，无法生成 share URL",
                c.protocol
            ))
        }
    }
}

fn encode_vless(c: &ServerConfig) -> Result<String, String> {
    let uuid = c
        .uuid
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("VLESS 节点缺少 UUID")?;
    let mut q = QueryString::new();
    q.push(
        "encryption",
        c.encryption.clone().unwrap_or_else(|| "none".into()),
    );
    q.push_opt("flow", c.flow.clone());
    push_transport_params(&mut q, c);
    if let Some(mode) = &c.security {
        push_security_params(&mut q, c, mode);
    }
    Ok(assemble(
        "vless",
        Some(uuid.to_string()),
        &c.address,
        c.port,
        &q,
        &c.name,
    ))
}

fn encode_trojan(c: &ServerConfig) -> Result<String, String> {
    let password = c.password.clone().unwrap_or_default();
    let mut q = QueryString::new();
    push_transport_params(&mut q, c);
    // 默认即 TLS；显式写 security 以 round-trip（解析侧默认 tls，写出即对齐）。
    let mode = c.security.clone().unwrap_or(SecurityMode::Tls);
    push_security_params(&mut q, c, &mode);
    Ok(assemble(
        "trojan",
        Some(percent_encode_component(&password)),
        &c.address,
        c.port,
        &q,
        &c.name,
    ))
}

fn encode_hysteria2(c: &ServerConfig) -> Result<String, String> {
    let password = c
        .password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("Hysteria2 节点缺少密码")?;
    let mut q = QueryString::new();
    if let Some(hy2) = &c.hysteria2_settings {
        if let Some(up) = hy2.up_mbps {
            q.push("up_mbps", up.to_string());
        }
        if let Some(down) = hy2.down_mbps {
            q.push("down_mbps", down.to_string());
        }
        if let Some(obfs) = &hy2.obfs {
            q.push_opt("obfs", obfs.type_field.clone());
            q.push_opt("obfs-password", obfs.password.clone());
        }
        q.push_opt("network", hy2.network.clone());
    }
    if let Some(tls) = &c.tls_settings {
        push_tls_params(&mut q, tls);
    }
    Ok(assemble(
        "hysteria2",
        Some(percent_encode_component(password)),
        &c.address,
        c.port,
        &q,
        &c.name,
    ))
}

fn encode_tuic(c: &ServerConfig) -> Result<String, String> {
    let uuid = c
        .uuid
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("TUIC 节点缺少 uuid")?;
    let password = c
        .password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("TUIC 节点缺少 password")?;
    let mut q = QueryString::new();
    if let Some(t) = &c.tuic_settings {
        q.push_opt("congestion_control", t.congestion_control.clone());
        q.push_opt("udp_relay_mode", t.udp_relay_mode.clone());
        q.push_opt("heartbeat", t.heartbeat.clone());
        if t.zero_rtt_handshake == Some(true) {
            q.push("zero_rtt_handshake", "1");
        }
    }
    if let Some(tls) = &c.tls_settings {
        push_tls_params(&mut q, tls);
    }
    // userinfo = uuid:password（uuid 为 hex+连字符，安全原样；password 转义）。
    let userinfo = format!("{}:{}", uuid, percent_encode_component(password));
    Ok(assemble(
        "tuic",
        Some(userinfo),
        &c.address,
        c.port,
        &q,
        &c.name,
    ))
}

fn encode_anytls(c: &ServerConfig) -> Result<String, String> {
    let password = c
        .password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("AnyTLS 节点缺少密码")?;
    let mut q = QueryString::new();
    if let Some(a) = &c.any_tls_settings {
        q.push_opt(
            "idle_session_check_interval",
            a.idle_session_check_interval.clone(),
        );
        q.push_opt("idle_session_timeout", a.idle_session_timeout.clone());
        if let Some(m) = a.min_idle_session {
            q.push("min_idle_session", m.to_string());
        }
    }
    let mode = c.security.clone().unwrap_or(SecurityMode::Tls);
    push_security_params(&mut q, c, &mode);
    Ok(assemble(
        "anytls",
        Some(percent_encode_component(password)),
        &c.address,
        c.port,
        &q,
        &c.name,
    ))
}

fn encode_snell(c: &ServerConfig) -> Result<String, String> {
    let psk = c
        .password
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or("Snell 节点缺少 psk")?;
    let mut q = QueryString::new();
    if let Some(s) = &c.snell_settings {
        q.push("version", s.version.to_string());
        q.push_opt("obfs", s.obfs_mode.clone());
        q.push_opt("obfs-host", s.obfs_host.clone());
        q.push_opt("mode", s.mode.clone());
        if s.reuse == Some(true) {
            q.push("reuse", "1");
        }
        q.push_opt("network", s.network.clone());
        q.push_opt("userkey", s.userkey.clone());
    }
    Ok(assemble(
        "snell",
        Some(percent_encode_component(psk)),
        &c.address,
        c.port,
        &q,
        &c.name,
    ))
}

fn encode_shadowsocks(c: &ServerConfig) -> Result<String, String> {
    let ss = c
        .shadowsocks_settings
        .as_ref()
        .ok_or("Shadowsocks 节点缺少加密设置")?;
    // userinfo = base64url-nopad(method:password)（传统形态，解析侧 base64 优先分支还原）。
    let cred = format!("{}:{}", ss.method, ss.password);
    let userinfo = base64_encode(cred.as_bytes(), true, false);
    let mut q = QueryString::new();
    if let Some(plugin) = &ss.plugin {
        let full = match &ss.plugin_opts {
            Some(opts) if !opts.is_empty() => format!("{plugin};{opts}"),
            _ => plugin.clone(),
        };
        q.push("plugin", full);
    }
    Ok(assemble(
        "ss",
        Some(userinfo),
        &c.address,
        c.port,
        &q,
        &c.name,
    ))
}

fn encode_socks(c: &ServerConfig) -> String {
    let q = QueryString::new();
    let userinfo = encode_userinfo(c.username.as_deref(), c.password.as_deref());
    assemble("socks5", userinfo, &c.address, c.port, &q, &c.name)
}

fn encode_http(c: &ServerConfig) -> String {
    let is_https = c.security.as_ref().is_some_and(SecurityMode::is_tls);
    let scheme = if is_https { "https" } else { "http" };
    let q = QueryString::new();
    let userinfo = encode_userinfo(c.username.as_deref(), c.password.as_deref());
    assemble(scheme, userinfo, &c.address, c.port, &q, &c.name)
}

fn encode_naive(c: &ServerConfig) -> Result<String, String> {
    let username = c
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("NaiveProxy 节点缺少用户名")?;
    let password = c
        .password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("NaiveProxy 节点缺少密码")?;
    let mut q = QueryString::new();
    push_transport_params(&mut q, c);
    let userinfo = format!(
        "{}:{}",
        percent_encode_component(username),
        percent_encode_component(password)
    );
    Ok(assemble(
        "http2",
        Some(userinfo),
        &c.address,
        c.port,
        &q,
        &c.name,
    ))
}

/// socks/http userinfo：缺 username → None（无 `@` 段，对齐解析侧 `split_userinfo`）。
fn encode_userinfo(username: Option<&str>, password: Option<&str>) -> Option<String> {
    let u = username.filter(|s| !s.is_empty())?;
    Some(match password {
        Some(p) => format!(
            "{}:{}",
            percent_encode_component(u),
            percent_encode_component(p)
        ),
        None => percent_encode_component(u),
    })
}

fn encode_vmess(c: &ServerConfig) -> Result<String, String> {
    use serde_json::json;
    let uuid = c
        .uuid
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or("VMess 节点缺少 uuid")?;
    let net = c.network.clone().unwrap_or_else(|| "tcp".into());
    let mut obj = serde_json::Map::new();
    obj.insert("v".into(), json!("2"));
    obj.insert("ps".into(), json!(c.name));
    obj.insert("add".into(), json!(c.address));
    obj.insert("port".into(), json!(c.port.to_string()));
    obj.insert("id".into(), json!(uuid));
    obj.insert("aid".into(), json!(c.alter_id.unwrap_or(0).to_string()));
    obj.insert(
        "scy".into(),
        json!(c.vmess_security.clone().unwrap_or_else(|| "auto".into())),
    );
    obj.insert("net".into(), json!(net));
    // 传输承载（ws/httpupgrade/http 的 path/host；grpc 的 serviceName 走 path）。
    match net.as_str() {
        "ws" | "httpupgrade" => {
            if let Some(ws) = &c.ws_settings {
                if let Some(p) = &ws.path {
                    obj.insert("path".into(), json!(p));
                }
                if let Some(h) = ws.headers.as_ref().and_then(|m| m.get("Host")) {
                    obj.insert("host".into(), json!(h));
                }
            }
        }
        "http" => {
            if let Some(h) = &c.http_settings {
                if let Some(p) = &h.path {
                    obj.insert("path".into(), json!(p));
                }
                if let Some(hosts) = &h.host {
                    obj.insert("host".into(), json!(hosts.join(",")));
                }
            }
        }
        "grpc" => {
            if let Some(g) = &c.grpc_settings {
                if let Some(sn) = &g.service_name {
                    obj.insert("path".into(), json!(sn));
                }
            }
        }
        _ => {}
    }
    if c.security.as_ref().is_some_and(SecurityMode::is_tls) {
        obj.insert("tls".into(), json!("tls"));
        if let Some(tls) = &c.tls_settings {
            if let Some(sni) = &tls.server_name {
                obj.insert("sni".into(), json!(sni));
            }
            if let Some(fp) = &tls.fingerprint {
                obj.insert("fp".into(), json!(fp));
            }
            if let Some(alpn) = &tls.alpn {
                if !alpn.is_empty() {
                    obj.insert("alpn".into(), json!(alpn.join(",")));
                }
            }
        }
    }
    let body = serde_json::to_string(&serde_json::Value::Object(obj))
        .map_err(|e| format!("VMess JSON 序列化失败: {e}"))?;
    Ok(format!(
        "vmess://{}",
        base64_encode(body.as_bytes(), false, true)
    ))
}

#[cfg(test)]
mod tests;
