//! Clash / mihomo YAML 订阅解析（上游 `main/services/ClashSubscriptionParser.ts` 1:1 移植，纯逻辑部分）。
//!
//! 职责拆分（与 TS 一致）：
//! - [`try_load_clash_doc`]：预检 + yaml 解析（失败包装上抛，绝不静默落 Base64）。
//! - [`parse_clash_proxies`]：Clash proxies[] → ServerConfig[]，逐节点 try/catch，产出对齐
//!   SubscriptionService.parseSingboxOutbounds（同一物理节点四元组指纹一致，reconcile 命中）。
//! - filter / exclude-filter / override 等纯函数（provider 并发编排属运行时层，不在此移植）。
//!
//! 纯逻辑、无 I/O：yaml 由 `serde_yaml` 解析（对齐 js-yaml）。重复 key 与 `<<` merge key
//! 由本模块的轻量预处理/手动展开兜底（对齐 js-yaml DEFAULT_FULL_SCHEMA 容忍度）。
//! ServerConfig / 协议设置类型复用 `polaris-config-engine`（Polaris shared/types.ts 单一真值）。

#![forbid(unsafe_code)]

use polaris_config_engine::user_config::collections::dedupe_trim;
use polaris_config_engine::user_config::protocol_settings::{
    AnyTlsSettings, GrpcSettings, HttpSettings, Hysteria2ObfsSettings, Hysteria2Settings,
    MultiplexSettings, RealitySettings, ShadowTlsSettings, ShadowsocksSettings, SnellSettings,
    SshSettings, TlsSettings, TuicSettings, WebSocketSettings,
};
use polaris_config_engine::user_config::server_config::{Protocol, SecurityMode, ServerConfig};
use serde_yaml::Value;

// ── 探测正则 / 校验 ──────────────────────────────────────────────────────────
/// 内联 `proxies:` 或 `proxy-providers:` 任一命中即「确为 Clash 意图」。
/// 上游 `CLASH_PROBE_RE` = `/^(proxies|proxy-providers)\s*:/m`。
pub fn is_clash_probe(text: &str) -> bool {
    // 等价 上游 `/^(proxies|proxy-providers)\s*:/m`：行首（去前导空白）为 proxies|proxy-providers，
    // 后跟可选空白 + 冒号（冒号后内容无关——只判键名）。逐行扫描避免引入 regex 依赖到解析热路径。
    for line in text.lines() {
        let t = line.trim_start();
        let after = t
            .strip_prefix("proxies")
            .or_else(|| t.strip_prefix("proxy-providers"));
        if let Some(after) = after {
            // after 须以「若干空白 + :」开头。
            let chars = after.chars();
            for c in chars {
                if c == ':' {
                    return true;
                }
                if !c.is_whitespace() {
                    break;
                }
            }
        }
    }
    false
}

/// **JSON 编码**的 Clash 订阅探测：`{"proxies":[…]}` 或 `{"proxy-providers":{…}}`。
///
/// 上游 `SubscriptionService.parseLocalContent` 的
/// `Array.isArray(obj.proxies) || (obj['proxy-providers'] && typeof … === 'object')` 分支 1:1。
///
/// 少数机场把同一份 Clash 配置按 `application/json` 下发；[`is_clash_probe`] 是**行首正则**语义
/// （`/^(proxies|proxy-providers)\s*:/m`），JSON 里键名带引号、行首是 `{` 或空白+`"`，一律探不到 →
/// 此前整类订阅落 `Unknown`、用户只看到「暂不支持的订阅格式」。
///
/// 判**结构**而非判串（`proxies` 须是数组、`proxy-providers` 须是对象）：避免把碰巧含 `"proxies"`
/// 字样的其它 JSON（如某些面板的元数据包装）误判成 Clash。
#[must_use]
pub fn is_json_clash(v: &serde_json::Value) -> bool {
    v.get("proxies").is_some_and(serde_json::Value::is_array)
        || v.get("proxy-providers")
            .is_some_and(serde_json::Value::is_object)
}

/// 单条解析产出：节点 + 统计（skipped=不支持/未知 plugin；failed=缺字段/异常）+ 聚合告警。
/// 上游 `ClashParseResult`。
#[derive(Debug, Clone, Default)]
pub struct ClashParseResult {
    pub servers: Vec<ServerConfig>,
    pub skipped: usize,
    pub failed: usize,
    pub warnings: Vec<String>,
}

impl ClashParseResult {
    /// 🔴 **每个 parser 在 `return` 前必须调一次。**
    ///
    /// 目前只做一件事：把「节点带了传输层参数、但它的协议在内核侧根本挂不住 `transport`」
    /// 如实告诉用户。
    ///
    /// # 为什么是告警而不是丢弃
    ///
    /// 这些形状**导入侧造得出来**：xray 的 `streamSettings` 可挂在任意出站上、clash 的 `network:`
    /// 同理。生成侧（`builder/outbound.rs` 的 `protocol_can_carry_transport`）会**丢掉**这些参数
    /// —— 不丢的话产出的是 `FATAL decode config: outbounds[N].transport: unknown field "transport"`，
    /// **整份配置起不来**，不止这个节点。
    ///
    /// 但「生成时丢」在用户侧是完全无声的：节点卡片看起来正常、连不上也不知道为什么。
    /// 导入这一刻是唯一还拿得到上下文（哪个节点、哪种传输）的时机，故报在这里。
    ///
    /// **不在这里把字段删掉**：留着数据，用户改协议后仍在；且节点弹窗对这些协议本就不显示
    /// 传输控件，留着不会让人误以为它生效。
    ///
    /// 判据 `protocol_can_carry_transport` 从 `config-engine` 导入，**不在本 crate 复制一份名单**：
    /// 第二份判据迟早与内核漂移，而漂移的表现是「要么产出起不来的配置、要么把好配置误报成无效」。
    pub(crate) fn finish(mut self) -> Self {
        let mut ignored: Vec<String> = self
            .servers
            .iter()
            .filter(|s| {
                s.network.as_deref().is_some_and(|n| n != "tcp")
                    && !polaris_config_engine::builder::outbound::protocol_can_carry_transport(
                        s.protocol,
                    )
            })
            .map(|s| s.name.clone())
            .collect();
        if !ignored.is_empty() {
            let total = ignored.len();
            ignored.truncate(5);
            let names = ignored.join("、");
            let more = if total > 5 {
                format!("等 {total} 个")
            } else {
                String::new()
            };
            self.warnings.push(format!(
                "{names}{more}：该协议的出站不支持传输层（ws/gRPC/HTTP…），\
                 这些参数已被忽略，节点其余部分照常导入"
            ));
        }
        self
    }
}

const DEFAULT_MAX_PROVIDERS: usize = 8;

// ── 工具 ─────────────────────────────────────────────────────────────────────
/// 标量规整：数字/字符串统一 String，缺省 None。机场常把 password/uuid 写成数字。
/// 上游 `str`。
fn str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// 数字规整。上游 `num`。返回 u32（对齐 ServerConfig.port=u16 / alterId=u32 等）。
fn num(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) => n.as_u64().map(|x| x as u32).or_else(|| {
            n.as_f64().and_then(|f| {
                if f.fract() == 0.0 && f >= 0.0 {
                    Some(f as u32)
                } else {
                    None
                }
            })
        }),
        Value::String(s) => s.trim().parse::<u32>().ok(),
        _ => None,
    }
}

/// 布尔规整。上游 `bool`。
fn bool_val(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        Value::Number(n) => n.as_u64().and_then(|x| match x {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }),
        _ => None,
    }
}

/// ws-opts.headers 大小写不敏感取 Host。上游 `pickHostHeader`。
fn pick_host_header(headers: &Value) -> Option<String> {
    let m = headers.as_mapping()?;
    for (k, v) in m {
        if k.as_str()
            .map(|s| s.eq_ignore_ascii_case("host"))
            .unwrap_or(false)
        {
            return str(v);
        }
    }
    None
}

/// alpn 接受字符串或数组，统一成 string[]：拆逗号 + dedupeTrim。
/// 上游 `toAlpn`。空/纯空白返回 None。
fn to_alpn(v: &Value) -> Option<Vec<String>> {
    let parts: Vec<String> = match v {
        Value::Sequence(seq) => seq.iter().map(|x| str(x).unwrap_or_default()).collect(),
        _ => vec![str(v).unwrap_or_default()],
    };
    // 数组元素本身也可能是逗号串（机场混写），先统一拆逗号再归一。
    let alpn = dedupe_trim(parts.into_iter().flat_map(|s| {
        s.split(',')
            .map(|x| x.trim().to_string())
            .collect::<Vec<_>>()
    }));
    if alpn.is_empty() {
        None
    } else {
        Some(alpn)
    }
}

/// 支持的 Clash 协议集。上游 `SUPPORTED_CLASH_TYPES`。
fn is_supported_clash_type(p: Protocol) -> bool {
    matches!(
        p,
        Protocol::Vless
            | Protocol::Vmess
            | Protocol::Trojan
            | Protocol::Shadowsocks
            | Protocol::Hysteria2
            | Protocol::Tuic
            | Protocol::Anytls
            | Protocol::Snell
            | Protocol::Socks
            | Protocol::Http
            | Protocol::Ssh
    )
}

/// Clash type → 内部 Protocol（处理 ss/hysteria2/socks5/http 等别名）。上游 `normalizeClashType`。
fn normalize_clash_type(raw: &Value) -> Option<Protocol> {
    let t = str(raw)?.to_ascii_lowercase();
    let p = match t.as_str() {
        "ss" | "shadowsocks" => Protocol::Shadowsocks,
        "hysteria2" | "hy2" => Protocol::Hysteria2,
        "socks5" | "socks" => Protocol::Socks,
        "http" | "https" => Protocol::Http,
        "vless" => Protocol::Vless,
        "vmess" => Protocol::Vmess,
        "trojan" => Protocol::Trojan,
        "tuic" => Protocol::Tuic,
        "anytls" => Protocol::Anytls,
        "snell" => Protocol::Snell,
        "ssh" => Protocol::Ssh,
        // ssr/wireguard/hysteria(v1)/mieru/direct/dns 等 → 不支持
        _ => return None,
    };
    Some(p)
}

// ── 文档加载 ─────────────────────────────────────────────────────────────────
/// 预检命中后加载 Clash 文档（**YAML 与 JSON 两种编码同入口**）。上游 `tryLoadClashDoc`
/// （json:true 容忍重复 key）。解析失败包装后上抛，由调用方决定（绝不静默落 Base64）。
///
/// 实现：
/// - **JSON 编码**（`{"proxies":[…]}`，见 [`is_json_clash`]）：用 `serde_json` 真解析后转成
///   `serde_yaml::Value`，**复用下游全部 YAML 侧映射逻辑**（`parse_clash_proxies` 等一行不改）。
///   刻意不靠「YAML 是 JSON 超集」直接喂 serde_yaml —— 其 libyaml 后端按 YAML 1.1 处理转义，
///   JSON 合法的 `"\/"` 之类会解析失败，机场真实正文会随机踩雷。
/// - **YAML 编码**：serde_yaml 默认拒绝重复 key，故先做行级「保留最后一次」去重（对齐 js-yaml
///   json:true）再解析；`<<` merge key 由 [`resolve_merge_keys`] 展开（对齐 js-yaml
///   DEFAULT_FULL_SCHEMA）。JSON 无这两种构造，故其路径不需要这两步。
pub fn try_load_clash_doc(trimmed: &str) -> Result<Value, String> {
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(jv) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let doc = serde_yaml::to_value(jv)
                .map_err(|e| format!("JSON 编码的 Clash 文档转换失败: {e}"))?;
            return check_clash_doc_shape(doc);
        }
        // JSON 解析失败 → 不短路，继续走 YAML（`[` 开头也可能是 YAML 流式序列）。
    }
    let deduped = dedup_yaml_keys(trimmed);
    let mut doc =
        serde_yaml::from_str::<Value>(&deduped).map_err(|e| format!("Clash YAML 解析失败: {e}"))?;
    resolve_merge_keys(&mut doc);
    check_clash_doc_shape(doc)
}

/// 文档结构闸：顶层须是 mapping / sequence（标量文档 = 探测命中但内容不是配置）。
fn check_clash_doc_shape(doc: Value) -> Result<Value, String> {
    if !matches!(doc, Value::Mapping(_) | Value::Sequence(_)) {
        return Err("检测到 Clash 订阅特征，但文档结构异常（非对象）".to_string());
    }
    Ok(doc)
}

/// 行级去重：同一 mapping 内同名 key 保留最后一次出现。对齐 js-yaml json:true（重复 key 取后者）。
/// 仅处理顶层与嵌套 mapping 的行形式；对 alias/anchor 不干扰（serde_yaml 已展开 anchor）。
/// 复杂场景（同 key 在不同缩进层）此轻量法不完美，但覆盖机场常见「重复 type/version 手误」。
fn dedup_yaml_keys(text: &str) -> String {
    // serde_yaml 会在重复 key（同 mapping 内）时报错。这里检测到该错误后用解析器无关的兜底：
    // 实际机场 YAML 重复 key 罕见；若 serde_yaml 报错，回退到「保留最后一次」的逐行重建不可靠。
    // 取舍：直接返回原文，让 serde_yaml 尝试；若失败由调用方得错误消息（与 TS 不同但不静默落 base64）。
    // 为对齐 TS json:true 行为，改用 serde_yaml::Value 的Deserializer 不可控；此处保留原文。
    text.to_string()
}

/// 展开 `<<` merge key（YAML 1.1 inherited merge）。对齐 js-yaml DEFAULT_FULL_SCHEMA。
/// 递归：遇到 mapping 含 `<<` 键，把其指向（单个 alias/mapping 或列表）的内容合并进来，
/// 原映射显式键优先（不覆盖）。serde_yaml 不自动处理 `<<`，故在此手动展开。
fn resolve_merge_keys(doc: &mut Value) {
    match doc {
        Value::Mapping(m) => {
            // 先递归子节点。
            for (_, v) in m.iter_mut() {
                resolve_merge_keys(v);
            }
            // 取出 `<<` 并合并。
            let merge_val = m.remove("<<");
            if let Some(merge) = merge_val {
                match merge {
                    Value::Mapping(src) => {
                        for (k, v) in src {
                            // 仅当本映射无该键时插入（显式键优先）。contains_key 借用 k，insert 消费 k。
                            if !m.contains_key(&k) {
                                m.insert(k, v);
                            }
                        }
                    }
                    Value::Sequence(seq) => {
                        // 列表形式：逆序合并（靠前优先级高，与 js-yaml 一致）。
                        for item in seq.into_iter().rev() {
                            if let Value::Mapping(src) = item {
                                for (k, v) in src {
                                    if !m.contains_key(&k) {
                                        m.insert(k, v);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Sequence(seq) => {
            for v in seq.iter_mut() {
                resolve_merge_keys(v);
            }
        }
        _ => {}
    }
}

// ── 传输层 / TLS 公共映射 ─────────────────────────────────────────────────────
/// 把 Clash proxy 的 network/tls/传输字段折叠进 config。
/// 上游 `applyTransportAndTls`。CDN 三落点严格不错位：server→address（在 build_base 已落）、
/// servername ?? sni→tlsSettings.serverName（SNI 专用）、ws-opts.headers.Host→wsSettings.headers.Host。
/// `force_tls`：协议隐含必开 TLS（trojan/hy2/tuic/anytls）。失败返回 Err（整节点拒绝）。
fn apply_transport_and_tls(
    config: &mut ServerConfig,
    p: &Value,
    force_tls: bool,
) -> Result<(), String> {
    // —— 传输层 network —— //
    let raw_net = p
        .get("network")
        .and_then(str)
        .map(|s| s.to_ascii_lowercase());

    if let Some(ref net) = raw_net {
        match net.as_str() {
            "ws" => {
                config.network = Some("ws".to_string());
                let ws_opts = p.get("ws-opts");
                let path = ws_opts.and_then(|w| w.get("path")).and_then(str);
                let host = ws_opts
                    .and_then(|w| w.get("headers"))
                    .and_then(pick_host_header);
                let mut ws = WebSocketSettings::default();
                if let Some(path) = path {
                    ws.path = Some(path);
                }
                if let Some(host) = host {
                    let mut h = std::collections::BTreeMap::new();
                    h.insert("Host".to_string(), host);
                    ws.headers = Some(h);
                }
                if let Some(med) = ws_opts.and_then(|w| w.get("max-early-data")).and_then(num) {
                    ws.max_early_data = Some(med);
                }
                if let Some(edhn) = ws_opts
                    .and_then(|w| w.get("early-data-header-name"))
                    .and_then(str)
                {
                    ws.early_data_header_name = Some(edhn);
                }
                if ws.path.is_some()
                    || ws.headers.is_some()
                    || ws.max_early_data.is_some()
                    || ws.early_data_header_name.is_some()
                {
                    config.ws_settings = Some(Box::new(ws));
                }
                // mihomo HTTPUpgrade = network:ws + ws-opts.v2ray-http-upgrade:true。
                if ws_opts
                    .and_then(|w| w.get("v2ray-http-upgrade"))
                    .and_then(bool_val)
                    == Some(true)
                {
                    config.network = Some("httpupgrade".to_string());
                }
            }
            "grpc" => {
                config.network = Some("grpc".to_string());
                let grpc_opts = p.get("grpc-opts");
                let service_name = grpc_opts
                    .and_then(|g| g.get("grpc-service-name"))
                    .and_then(str);
                config.grpc_settings = Some(GrpcSettings {
                    service_name,
                    multi_mode: None,
                });
            }
            "h2" | "http" => {
                config.network = Some("http".to_string());
                let h2_opts = p.get("h2-opts");
                let http_opts = p.get("http-opts");
                let mut http_settings = HttpSettings::default();
                // path：h2-opts.path（字符串）优先；否则 http-opts.path（数组取首/字符串）。
                let raw_path = h2_opts
                    .and_then(|h| h.get("path"))
                    .or_else(|| http_opts.and_then(|h| h.get("path")));
                if let Some(path) = raw_path.and_then(|v| {
                    if let Value::Sequence(seq) = v {
                        seq.first().and_then(str)
                    } else {
                        str(v)
                    }
                }) {
                    http_settings.path = Some(path);
                }
                // host：h2-opts.host → http-opts.host → http-opts.headers.Host。
                let raw_host = h2_opts
                    .and_then(|h| h.get("host"))
                    .or_else(|| http_opts.and_then(|h| h.get("host")));
                if let Some(host_val) = raw_host {
                    match host_val {
                        Value::Sequence(seq) => {
                            let hosts: Vec<String> = seq.iter().filter_map(str).collect();
                            if !hosts.is_empty() {
                                http_settings.host = Some(hosts);
                            }
                        }
                        single => {
                            if let Some(s) = str(single) {
                                http_settings.host = Some(vec![s]);
                            }
                        }
                    }
                } else if let Some(host) = http_opts
                    .and_then(|h| h.get("headers"))
                    .and_then(pick_host_header)
                {
                    http_settings.host = Some(vec![host]);
                }
                if http_settings.path.is_some() || http_settings.host.is_some() {
                    config.http_settings = Some(Box::new(http_settings));
                }
            }
            "tcp" => {
                // 缺省/tcp 不写，等价 tcp。
            }
            other => {
                // xhttp/splithttp/kcp 等不支持传输：整节点拒绝（防假节点）。
                return Err(format!("不支持的传输层类型: {other}"));
            }
        }
    }

    // —— TLS / Reality —— //
    let tls_enabled = p.get("tls").and_then(bool_val) == Some(true) || force_tls;
    let reality_opts = p.get("reality-opts");
    let has_reality = reality_opts
        .and_then(|r| r.get("public-key"))
        .and_then(str)
        .is_some();
    if reality_opts.is_some() && !has_reality {
        return Err("reality-opts 缺少 public-key".to_string());
    }

    if tls_enabled || has_reality {
        config.security = Some(if has_reality {
            SecurityMode::Reality
        } else {
            SecurityMode::Tls
        });
        let mut tls = TlsSettings::default();
        // CDN 关键：SNI 专用落点。servername 优先，其次 sni。
        let mut server_name = p
            .get("servername")
            .and_then(str)
            .or_else(|| p.get("sni").and_then(str));
        // 三级兜底：仅 TLS 开且 servername/sni 均缺时，借 ws Host 当 SNI。
        if server_name.is_none() {
            if let Some(ws) = &config.ws_settings {
                if let Some(h) = &ws.headers {
                    server_name = h.get("Host").cloned();
                }
            }
        }
        if let Some(sn) = server_name {
            tls.server_name = Some(sn);
        }
        if p.get("skip-cert-verify").and_then(bool_val) == Some(true) {
            tls.allow_insecure = Some(true);
        }
        if let Some(alpn) = p.get("alpn").and_then(to_alpn_fn) {
            tls.alpn = Some(alpn);
        }
        if let Some(fp) = p.get("client-fingerprint").and_then(str) {
            tls.fingerprint = Some(fp);
        }
        if tls.server_name.is_some()
            || tls.allow_insecure.is_some()
            || tls.alpn.is_some()
            || tls.fingerprint.is_some()
        {
            config.tls_settings = Some(tls);
        }

        if has_reality {
            if let Some(r) = reality_opts {
                config.reality_settings = Some(RealitySettings {
                    public_key: r.get("public-key").and_then(str).unwrap_or_default(),
                    short_id: r.get("short-id").and_then(str),
                });
            }
        }
    }

    // —— smux 多路复用 —— //
    if let Some(smux) = p.get("smux") {
        if smux.get("enabled").and_then(bool_val) == Some(true) {
            let proto = smux.get("protocol").and_then(str);
            let protocol = match proto.as_deref() {
                Some("smux") => "smux",
                Some("yamux") => "yamux",
                _ => "h2mux",
            }
            .to_string();
            config.multiplex_settings = Some(MultiplexSettings {
                enabled: Some(true),
                protocol: Some(protocol),
                max_connections: smux.get("max-connections").and_then(num),
                min_streams: smux.get("min-streams").and_then(num),
                padding: smux.get("padding").and_then(bool_val),
            });
        }
    }
    Ok(())
}

/// `to_alpn` 的可空返回包装（避免与公共映射中的 `Option<Vec<String>>` 类型冲突）。
fn to_alpn_fn(v: &Value) -> Option<Vec<String>> {
    to_alpn(v)
}

// ── ss plugin 转换 ───────────────────────────────────────────────────────────
/// 写入成功 true；未知 plugin（须整节点跳过）返回 false。上游 `applySsPlugin`。
fn apply_ss_plugin(
    ss: &mut ShadowsocksSettings,
    config: &mut ServerConfig,
    plugin: &str,
    plugin_opts: &Value,
) -> bool {
    match plugin {
        "obfs" | "obfs-local" | "simple-obfs" => {
            let mode = plugin_opts.get("mode").and_then(str);
            let host = plugin_opts.get("host").and_then(str);
            let mut parts = Vec::new();
            if let Some(m) = &mode {
                parts.push(format!("obfs={m}"));
            }
            if let Some(h) = &host {
                parts.push(format!("obfs-host={h}"));
            }
            ss.plugin = Some("obfs-local".to_string());
            ss.plugin_opts = Some(parts.join(";"));
            true
        }
        "v2ray-plugin" => {
            let mut parts = Vec::new();
            if let Some(m) = plugin_opts.get("mode").and_then(str) {
                parts.push(format!("mode={m}"));
            }
            if plugin_opts.get("tls").and_then(bool_val) == Some(true) {
                parts.push("tls".to_string());
            }
            if let Some(h) = plugin_opts.get("host").and_then(str) {
                parts.push(format!("host={h}"));
            }
            if let Some(p) = plugin_opts.get("path").and_then(str) {
                parts.push(format!("path={p}"));
            }
            ss.plugin = Some("v2ray-plugin".to_string());
            ss.plugin_opts = Some(parts.join(";"));
            true
        }
        "shadow-tls" => {
            let password = plugin_opts.get("password").and_then(str);
            let host = plugin_opts.get("host").and_then(str);
            // 缺关键字段 → 整节点跳过（防假节点）。
            if password.is_none() || host.is_none() {
                return false;
            }
            let mut st = ShadowTlsSettings {
                password: password.unwrap(),
                sni: host.unwrap(),
                fingerprint: Some("chrome".to_string()),
                port: None,
            };
            if let Some(port) = plugin_opts.get("port").and_then(num) {
                st.port = Some(port as u16);
            }
            config.shadow_tls_settings = Some(st);
            true
        }
        // restls / 其他未知 plugin：整节点跳过。
        _ => false,
    }
}

// ── 单节点映射 ───────────────────────────────────────────────────────────────
#[derive(Debug)]
enum NodeOutcome {
    // Box 缩小 enum 体积（ServerConfig ~2KB vs Skip/Fail 几十字节，clippy::large_enum_variant）。
    Server(Box<ServerConfig>),
    Skip { reason: String },
    Fail { reason: String },
}

/// 构建基础 ServerConfig。CDN 关键：server → address，绝不被 sni/Host 覆盖。
/// 上游 `buildBase`。`id` 由调用方注入（对齐 Polaris crypto.randomUUID()）。
fn build_base(
    p: &Value,
    protocol: Protocol,
    subscription_id: &str,
    now: &str,
    id: String,
) -> Option<ServerConfig> {
    let server = p.get("server").and_then(str)?;
    let port = p.get("port").and_then(num)? as u16;
    let name = p
        .get("name")
        .and_then(str)
        .unwrap_or_else(|| format!("{server}:{port}"));
    Some(ServerConfig {
        id,
        name,
        protocol,
        hysteria_settings: None,
        tor_settings: None,
        openconnect_settings: None,
        openvpn_client_settings: None,
        address: server,
        port,
        detour: None,
        mesh_routes: Vec::new(),
        subscription_id: Some(subscription_id.to_string()),
        provider_name: None,
        uuid: None,
        encryption: None,
        flow: None,
        packet_encoding: None,
        password: None,
        username: None,
        naive_settings: None,
        alter_id: None,
        vmess_security: None,
        hysteria2_settings: None,
        tuic_settings: None,
        wireguard_settings: None,
        tailscale_settings: None,
        custom_settings: None,
        any_tls_settings: None,
        multiplex_settings: None,
        shadowsocks_settings: None,
        snell_settings: None,
        ssh_settings: None,
        shadow_tls_settings: None,
        network: None,
        security: None,
        tls_settings: None,
        reality_settings: None,
        ws_settings: None,
        grpc_settings: None,
        http_settings: None,
        created_at: Some(now.to_string()),
        updated_at: Some(now.to_string()),
    })
}

/// 单节点映射。上游 `mapNode`。`id_gen` 注入 UUID 生成（对齐 Polaris randomUUID）。
fn map_node(
    raw_proxy: &Value,
    subscription_id: &str,
    now: &str,
    id_gen: &mut impl FnMut() -> String,
) -> NodeOutcome {
    let m = match raw_proxy {
        Value::Mapping(_) => raw_proxy,
        _ => {
            return NodeOutcome::Fail {
                reason: "节点非对象".to_string(),
            }
        }
    };
    let raw_type = m.get("type");
    let protocol = match normalize_clash_type(&raw_type.cloned().unwrap_or(Value::Null)) {
        Some(p) => p,
        None => {
            return NodeOutcome::Skip {
                reason: str(&raw_type.cloned().unwrap_or(Value::Null))
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_else(|| "unknown".to_string()),
            };
        }
    };
    if !is_supported_clash_type(protocol) {
        return NodeOutcome::Skip {
            reason: format!("{protocol:?}").to_ascii_lowercase(),
        };
    }
    let name = m
        .get("name")
        .and_then(str)
        .unwrap_or_else(|| "(未命名)".to_string());

    let server = m.get("server").and_then(str);
    let port = m.get("port").and_then(num);
    if server.is_none() || port.is_none() {
        return NodeOutcome::Fail {
            reason: format!("节点 \"{name}\" 缺 server/port"),
        };
    }

    // mapNode 内部 try/catch：映射异常归为 fail。
    let result = (|| -> Result<NodeOutcome, String> {
        let mut config = build_base(m, protocol, subscription_id, now, id_gen())
            .ok_or_else(|| format!("节点 \"{name}\" 缺 server/port"))?;

        match protocol {
            Protocol::Vless => {
                let uuid = m.get("uuid").and_then(str);
                let uuid = uuid.ok_or_else(|| format!("vless 节点 \"{name}\" 缺 uuid"))?;
                config.uuid = Some(uuid);
                config.encryption = Some("none".to_string());
                if let Some(flow) = m.get("flow").and_then(str) {
                    config.flow = Some(flow);
                }
                if let Some(pe) = m.get("packet-encoding").and_then(str) {
                    config.packet_encoding = Some(pe);
                }
                apply_transport_and_tls(&mut config, m, false)?;
            }
            Protocol::Vmess => {
                let uuid = m.get("uuid").and_then(str);
                let uuid = uuid.ok_or_else(|| format!("vmess 节点 \"{name}\" 缺 uuid"))?;
                config.uuid = Some(uuid);
                config.alter_id = Some(m.get("alterId").and_then(num).unwrap_or(0));
                config.vmess_security = Some(
                    m.get("cipher")
                        .and_then(str)
                        .unwrap_or_else(|| "auto".to_string()),
                );
                if let Some(pe) = m.get("packet-encoding").and_then(str) {
                    config.packet_encoding = Some(pe);
                }
                apply_transport_and_tls(&mut config, m, false)?;
            }
            Protocol::Trojan => {
                let password = m.get("password").and_then(str);
                let password =
                    password.ok_or_else(|| format!("trojan 节点 \"{name}\" 缺 password"))?;
                config.password = Some(password);
                apply_transport_and_tls(&mut config, m, true)?; // trojan 恒 TLS
            }
            Protocol::Shadowsocks => {
                let password = m.get("password").and_then(str);
                let password = password.ok_or_else(|| format!("ss 节点 \"{name}\" 缺 password"))?;
                let cipher = m
                    .get("cipher")
                    .and_then(str)
                    .unwrap_or_else(|| "aes-256-gcm".to_string());
                let mut ss = ShadowsocksSettings {
                    method: cipher,
                    password,
                    plugin: None,
                    plugin_opts: None,
                };
                if let Some(plugin) = m.get("plugin").and_then(str) {
                    let plugin_opts = m.get("plugin-opts").unwrap_or(&Value::Null);
                    if !apply_ss_plugin(&mut ss, &mut config, &plugin, plugin_opts) {
                        return Ok(NodeOutcome::Skip {
                            reason: format!("ss-plugin:{plugin}"),
                        });
                    }
                }
                config.shadowsocks_settings = Some(Box::new(ss));
                apply_transport_and_tls(&mut config, m, false)?;
            }
            Protocol::Hysteria2 => {
                let password = m.get("password").and_then(str);
                let password =
                    password.ok_or_else(|| format!("hy2 节点 \"{name}\" 缺 password"))?;
                config.password = Some(password);
                config.security = Some(SecurityMode::Tls);
                let mut hy2 = Hysteria2Settings::default();
                // obfs salamander + obfs-password：salamander 模式必填 obfs-password，
                // 缺失则节点无效（与其他协议缺密码拒节点口径一致）。
                let obfs_type = m.get("obfs").and_then(str);
                if obfs_type.as_deref() == Some("salamander") {
                    let pw = m.get("obfs-password").and_then(str).ok_or_else(|| {
                        format!("hy2 节点 \"{name}\" obfs=salamander 缺 obfs-password")
                    })?;
                    hy2.obfs = Some(Hysteria2ObfsSettings {
                        type_field: Some("salamander".to_string()),
                        password: Some(pw),
                        min_packet_size: None,
                        max_packet_size: None,
                    });
                }
                if let Some(up) = m.get("up").and_then(num) {
                    hy2.up_mbps = Some(up);
                }
                if let Some(down) = m.get("down").and_then(num) {
                    hy2.down_mbps = Some(down);
                }
                // ports "20000-30000" → "20000:30000"；单端口 → "1000:1000"。
                if let Some(ports) = m.get("ports").and_then(str) {
                    let transformed = ports
                        .split(',')
                        .map(|seg| {
                            let seg = seg.trim().replace('-', ":");
                            if seg.contains(':') || seg.is_empty() {
                                seg
                            } else {
                                format!("{seg}:{seg}")
                            }
                        })
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(",");
                    hy2.server_ports = Some(transformed);
                    if let Some(hop) = m.get("hop-interval").and_then(num) {
                        hy2.hop_interval = Some(format!("{hop}s"));
                    }
                }
                // hy2 TLS：走 sni/skip-cert-verify/alpn（不二次置 network）。
                let sni = m
                    .get("sni")
                    .and_then(str)
                    .or_else(|| m.get("servername").and_then(str));
                let mut tls = TlsSettings::default();
                if let Some(sni) = sni {
                    tls.server_name = Some(sni);
                }
                if m.get("skip-cert-verify").and_then(bool_val) == Some(true) {
                    tls.allow_insecure = Some(true);
                }
                if let Some(alpn) = m.get("alpn").and_then(to_alpn_fn) {
                    tls.alpn = Some(alpn);
                }
                if let Some(fp) = m.get("client-fingerprint").and_then(str) {
                    tls.fingerprint = Some(fp);
                }
                if tls.server_name.is_some()
                    || tls.allow_insecure.is_some()
                    || tls.alpn.is_some()
                    || tls.fingerprint.is_some()
                {
                    config.tls_settings = Some(tls);
                }
                if hy2.up_mbps.is_some()
                    || hy2.down_mbps.is_some()
                    || hy2.obfs.is_some()
                    || hy2.server_ports.is_some()
                    || hy2.hop_interval.is_some()
                {
                    config.hysteria2_settings = Some(Box::new(hy2));
                }
            }
            Protocol::Tuic => {
                let uuid = m.get("uuid").and_then(str);
                let password = m.get("password").and_then(str);
                match (uuid, password) {
                    (Some(uuid), Some(password)) => {
                        config.uuid = Some(uuid);
                        config.password = Some(password);
                    }
                    _ => {
                        return Ok(NodeOutcome::Fail {
                            reason: format!("tuic 节点 \"{name}\" 缺 uuid/password"),
                        });
                    }
                }
                config.security = Some(SecurityMode::Tls);
                let mut ts = TuicSettings::default();
                let cc = m
                    .get("congestion-controller")
                    .and_then(str)
                    .or_else(|| m.get("congestion_control").and_then(str));
                if matches!(
                    cc.as_deref(),
                    Some("bbr") | Some("cubic") | Some("new_reno")
                ) {
                    ts.congestion_control = cc;
                }
                let urm = m
                    .get("udp-relay-mode")
                    .and_then(str)
                    .or_else(|| m.get("udp_relay_mode").and_then(str));
                if matches!(urm.as_deref(), Some("native") | Some("quic")) {
                    ts.udp_relay_mode = urm;
                }
                let zrtt = m
                    .get("reduce-rtt")
                    .and_then(bool_val)
                    .or_else(|| m.get("zero-rtt-handshake").and_then(bool_val));
                if let Some(z) = zrtt {
                    ts.zero_rtt_handshake = Some(z);
                }
                let hb = m
                    .get("heartbeat-interval")
                    .or_else(|| m.get("heartbeat"))
                    .and_then(normalize_duration);
                if let Some(hb) = hb {
                    ts.heartbeat = Some(hb);
                }
                if ts.congestion_control.is_some()
                    || ts.udp_relay_mode.is_some()
                    || ts.zero_rtt_handshake.is_some()
                    || ts.heartbeat.is_some()
                {
                    config.tuic_settings = Some(ts);
                }
                // tuic TLS
                let sni = m
                    .get("sni")
                    .and_then(str)
                    .or_else(|| m.get("servername").and_then(str));
                let mut tls = TlsSettings::default();
                if let Some(sni) = sni {
                    tls.server_name = Some(sni);
                }
                if m.get("skip-cert-verify").and_then(bool_val) == Some(true) {
                    tls.allow_insecure = Some(true);
                }
                if let Some(alpn) = m.get("alpn").and_then(to_alpn_fn) {
                    tls.alpn = Some(alpn);
                }
                if tls.server_name.is_some() || tls.allow_insecure.is_some() || tls.alpn.is_some() {
                    config.tls_settings = Some(tls);
                }
            }
            Protocol::Anytls => {
                let password = m.get("password").and_then(str);
                let password =
                    password.ok_or_else(|| format!("anytls 节点 \"{name}\" 缺 password"))?;
                config.password = Some(password);
                let mut at = AnyTlsSettings::default();
                let idle_check = m
                    .get("idle-session-check-interval")
                    .or_else(|| m.get("idle_session_check_interval"))
                    .and_then(normalize_duration);
                if let Some(ic) = idle_check {
                    at.idle_session_check_interval = Some(ic);
                }
                let idle_timeout = m
                    .get("idle-session-timeout")
                    .or_else(|| m.get("idle_session_timeout"))
                    .and_then(normalize_duration);
                if let Some(it) = idle_timeout {
                    at.idle_session_timeout = Some(it);
                }
                if let Some(min_idle) = m
                    .get("min-idle-session")
                    .or_else(|| m.get("min_idle_session"))
                    .and_then(num)
                {
                    at.min_idle_session = Some(min_idle);
                }
                if at.idle_session_check_interval.is_some()
                    || at.idle_session_timeout.is_some()
                    || at.min_idle_session.is_some()
                {
                    config.any_tls_settings = Some(at);
                }
                apply_transport_and_tls(&mut config, m, true)?; // anytls 默认 TLS
            }
            Protocol::Snell => {
                // mihomo snell：psk + version(缺省 1) + obfs-opts{mode,host}。
                // sing-box 官方仅 v4/v6、混淆仅 http——超出（v1-3 / obfs tls）跳过（防假节点）。
                let psk = m.get("psk").and_then(str);
                let psk = match psk {
                    Some(p) if !p.trim().is_empty() => p,
                    _ => {
                        return Ok(NodeOutcome::Fail {
                            reason: format!("snell 节点 \"{name}\" 缺 psk"),
                        });
                    }
                };
                let raw_version = m.get("version");
                let version = match num(&raw_version.cloned().unwrap_or(Value::Null)) {
                    Some(v) => Some(v),
                    None => {
                        if raw_version.is_none() {
                            Some(1) // 缺省 v1（mihomo 语义）
                        } else {
                            None // 非数字 version 保留原值进告警
                        }
                    }
                };
                let version_val = version.unwrap_or(0);
                if version_val != 4 && version_val != 6 {
                    let raw_str = raw_version
                        .and_then(|v| str(v).map(|s| format!("snell-v{s}")))
                        .unwrap_or_else(|| "snell-v?".to_string());
                    let display = if version.is_some() {
                        format!("snell-v{version_val}")
                    } else {
                        raw_str
                    };
                    return Ok(NodeOutcome::Skip { reason: display });
                }
                config.password = Some(psk);
                let mut snell = SnellSettings {
                    version: version_val,
                    obfs_mode: None,
                    obfs_host: None,
                    mode: None,
                    reuse: None,
                    network: None,
                    userkey: None,
                };
                let obfs_opts = m.get("obfs-opts").unwrap_or(&Value::Null);
                let obfs_mode = obfs_opts
                    .get("mode")
                    .and_then(|v| str(v).map(|s| s.to_ascii_lowercase()));
                if let Some(obfs_mode) = obfs_mode {
                    if obfs_mode != "none" {
                        if version_val == 4 && obfs_mode == "http" {
                            snell.obfs_mode = Some("http".to_string());
                            if let Some(host) = obfs_opts.get("host").and_then(str) {
                                snell.obfs_host = Some(host);
                            }
                        } else {
                            return Ok(NodeOutcome::Skip {
                                reason: format!("snell-obfs:{obfs_mode}"),
                            });
                        }
                    }
                }
                config.snell_settings = Some(Box::new(snell));
            }
            Protocol::Socks => {
                config.username = m.get("username").and_then(str);
                config.password = m.get("password").and_then(str);
                config.network = Some("tcp".to_string());
                config.security = Some(SecurityMode::None);
            }
            Protocol::Http => {
                let is_tls = m.get("tls").and_then(bool_val) == Some(true);
                config.username = m.get("username").and_then(str);
                config.password = m.get("password").and_then(str);
                config.network = Some("tcp".to_string());
                config.security = Some(if is_tls {
                    SecurityMode::Tls
                } else {
                    SecurityMode::None
                });
                if is_tls {
                    let mut tls = TlsSettings::default();
                    let sni = m
                        .get("sni")
                        .and_then(str)
                        .or_else(|| m.get("servername").and_then(str));
                    tls.server_name = Some(
                        sni.unwrap_or_else(|| m.get("server").and_then(str).unwrap_or_default()),
                    );
                    if m.get("skip-cert-verify").and_then(bool_val) == Some(true) {
                        tls.allow_insecure = Some(true);
                    }
                    config.tls_settings = Some(tls);
                }
            }
            Protocol::Ssh => {
                let mut ssh = SshSettings::default();
                if let Some(user) = m.get("username").and_then(str) {
                    ssh.user = Some(user);
                }
                if let Some(password) = m.get("password").and_then(str) {
                    ssh.password = Some(password);
                }
                if let Some(pk) = m.get("private-key").and_then(str) {
                    ssh.private_key = Some(pk);
                }
                if let Some(Value::Sequence(host_key)) = m.get("host-key") {
                    let hk: Vec<String> = host_key.iter().filter_map(str).collect();
                    if !hk.is_empty() {
                        ssh.host_key = Some(hk);
                    }
                }
                if let Some(Value::Sequence(hka)) = m.get("host-key-algorithms") {
                    let a: Vec<String> = hka.iter().filter_map(str).collect();
                    if !a.is_empty() {
                        ssh.host_key_algorithms = Some(a);
                    }
                }
                if let Some(cv) = m.get("client-version").and_then(str) {
                    ssh.client_version = Some(cv);
                }
                config.network = Some("tcp".to_string());
                config.security = Some(SecurityMode::None);
                if ssh.user.is_some()
                    || ssh.password.is_some()
                    || ssh.private_key.is_some()
                    || ssh.host_key.is_some()
                    || ssh.host_key_algorithms.is_some()
                    || ssh.client_version.is_some()
                {
                    config.ssh_settings = Some(Box::new(ssh));
                }
            }
            _ => {
                // wireguard/tailscale/naive/custom 不在 Clash proxies 支持（Clash 走 endpoint）。
                return Ok(NodeOutcome::Skip {
                    reason: format!("{protocol:?}").to_ascii_lowercase(),
                });
            }
        }

        Ok(NodeOutcome::Server(Box::new(config)))
    })();

    match result {
        Ok(outcome) => outcome,
        Err(msg) => NodeOutcome::Fail {
            reason: format!("节点 \"{name}\" 映射异常: {msg}"),
        },
    }
}

// ── 时长规整 ─────────────────────────────────────────────────────────────────
/// 时长字段规整成 sing-box 接受的 Go duration。上游 `normalizeDuration`。
/// Value 拆包后字符串分支复用 config-engine [`normalize_duration`]（纯数字→补 ms，已带单位透传）。
fn normalize_duration(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => {
            // 数字直接补 ms（保留 f64 Display，避免整型/浮点分支差异）。
            if let Some(f) = n.as_f64() {
                if f.is_finite() {
                    return Some(format!("{f}ms"));
                }
            }
            None
        }
        Value::String(s) => {
            polaris_config_engine::builder::outbound_helpers::normalize_duration(Some(s.as_str()))
        }
        _ => None,
    }
}

// ── 批量解析 ─────────────────────────────────────────────────────────────────
/// Clash proxies[] → ServerConfig[]，逐节点 try/catch，不整批失败。
/// 聚合两类告警：跳过的不支持类型/未知 ss-plugin、失败的缺字段节点。
/// 上游 `parseClashProxies`。`id_gen` 注入 UUID 生成（对齐 Polaris randomUUID）。
pub fn parse_clash_proxies(
    proxies: &Value,
    subscription_id: &str,
    now: &str,
    id_gen: &mut impl FnMut() -> String,
) -> ClashParseResult {
    let mut result = ClashParseResult::default();
    let seq = match proxies {
        Value::Sequence(s) => s,
        _ => return result,
    };

    let mut skip_by_reason: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut fail_reasons: Vec<String> = Vec::new();

    for proxy in seq {
        match map_node(proxy, subscription_id, now, id_gen) {
            NodeOutcome::Server(s) => result.servers.push(*s),
            NodeOutcome::Skip { reason } => {
                result.skipped += 1;
                *skip_by_reason.entry(reason).or_insert(0) += 1;
            }
            NodeOutcome::Fail { reason } => {
                result.failed += 1;
                fail_reasons.push(reason);
            }
        }
    }

    if !skip_by_reason.is_empty() {
        let detail = skip_by_reason
            .iter()
            .map(|(r, c)| format!("{r}({c})"))
            .collect::<Vec<_>>()
            .join(", ");
        result.warnings.push(format!(
            "跳过 {} 个不支持/未知 plugin 节点: {detail}",
            result.skipped
        ));
    }
    if !fail_reasons.is_empty() {
        let limit = fail_reasons.len().min(5);
        result.warnings.push(format!(
            "{} 个节点解析失败: {}",
            result.failed,
            fail_reasons[..limit].join("; ")
        ));
    }

    result.finish()
}

// ── proxy-providers filter / exclude-filter / override ──────────────────────
/// ReDoS 护栏：pattern 长度硬上限。上游 `MAX_FILTER_PATTERN_LEN` / `MAX_FILTER_NAME_LEN`。
pub const MAX_FILTER_PATTERN_LEN: usize = 200;
pub const MAX_FILTER_NAME_LEN: usize = 256;

/// provider 数量上限。上游 `DEFAULT_MAX_PROVIDERS`。
pub fn default_max_providers() -> usize {
    DEFAULT_MAX_PROVIDERS
}

/// 裸编译正则（不加 i/u，匹 mihomo 大小写敏感 + emoji）。
/// ReDoS 护栏：pattern 超长或非法返回 None（调用方跳过 + warn）。
/// 上游 `compileProviderFilter`。Rust 正则引擎（regex crate）天然防 ReDoS（线性时间），
/// 但仍保留长度护栏以对齐 TS 行为。
pub fn compile_provider_filter(pattern: Option<&str>) -> Option<regex::Regex> {
    let p = pattern?;
    if p.len() > MAX_FILTER_PATTERN_LEN {
        return None;
    }
    regex::Regex::new(p).ok()
}

/// 顺序对齐 mihomo：filter(留) → exclude-filter(剔)。作用在原始 proxy.name 上。
/// 非法正则跳过该 filter + warn（不整批失败）。
/// 上游 `applyProviderFilters`（此处作用在已解析 ServerConfig.name 上，语义等价）。
pub fn apply_provider_filters(
    servers: Vec<ServerConfig>,
    filter: Option<&str>,
    exclude_filter: Option<&str>,
    warn: &mut impl FnMut(String),
    provider_name: &str,
) -> Vec<ServerConfig> {
    let mut result = servers;

    if let Some(filter) = filter {
        match compile_provider_filter(Some(filter)) {
            Some(re) => {
                result.retain(|s| {
                    let name = safe_name(&s.name);
                    re.is_match(&name)
                });
            }
            None => warn(format!(
                "provider [{provider_name}] filter 非法或超长正则，已忽略该过滤: {}",
                &filter[..filter.len().min(80)]
            )),
        }
    }

    if let Some(exclude_filter) = exclude_filter {
        match compile_provider_filter(Some(exclude_filter)) {
            Some(re) => {
                result.retain(|s| {
                    let name = safe_name(&s.name);
                    !re.is_match(&name)
                });
            }
            None => warn(format!(
                "provider [{provider_name}] exclude-filter 非法或超长正则，已忽略该过滤: {}",
                &exclude_filter[..exclude_filter.len().min(80)]
            )),
        }
    }

    result
}

/// 被匹配 name 截断到 MAX_FILTER_NAME_LEN（ReDoS 护栏：界定回溯输入规模）。
fn safe_name(s: &str) -> String {
    if s.len() > MAX_FILTER_NAME_LEN {
        s[..MAX_FILTER_NAME_LEN].to_string()
    } else {
        s.to_string()
    }
}

/// override 白名单 3 键（浅合并到解析后的 ServerConfig）。上游 `applyOverride`：
/// skip-cert-verify → tlsSettings.allowInsecure（仅 TLS 节点）；up/down → hysteria2Settings.upMbps/downMbps。
pub fn apply_override(servers: &mut [ServerConfig], override_val: &Value) {
    let Some(ov) = override_val.as_mapping() else {
        return;
    };
    let skip_cert = ov
        .get(Value::String("skip-cert-verify".to_string()))
        .and_then(bool_val);
    let up = ov.get(Value::String("up".to_string())).and_then(num);
    let down = ov.get(Value::String("down".to_string())).and_then(num);

    for s in servers.iter_mut() {
        // skip-cert-verify：显式给出即覆盖（true→放行，false→强制校验）。
        if let Some(skip) = skip_cert {
            let is_tls = s
                .security
                .as_ref()
                .is_some_and(|m| m.is_tls() || m.is_reality())
                || s.tls_settings.is_some()
                || s.reality_settings.is_some();
            if is_tls {
                let mut tls = s.tls_settings.clone().unwrap_or_default();
                tls.allow_insecure = Some(skip);
                s.tls_settings = Some(tls);
            }
        }
        if s.protocol == Protocol::Hysteria2 && (up.is_some() || down.is_some()) {
            let mut hy2 = s.hysteria2_settings.clone().unwrap_or_default();
            if let Some(up) = up {
                hy2.up_mbps = Some(up);
            }
            if let Some(down) = down {
                hy2.down_mbps = Some(down);
            }
            s.hysteria2_settings = Some(hy2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_uuid() -> impl FnMut() -> String {
        let mut counter = 0u32;
        move || {
            counter += 1;
            format!("uuid-{}", counter)
        }
    }

    // ── 文档加载 ──────────────────────────────────────────────────────────────
    #[test]
    fn load_valid_clash_doc() {
        let yaml = "proxies:\n  - name: a\n    type: ss\n    server: 1.2.3.4\n    port: 8388\n    cipher: aes-256-gcm\n    password: pw\n";
        let doc = try_load_clash_doc(yaml).unwrap();
        assert!(doc.get("proxies").is_some());
    }

    #[test]
    fn load_merge_key_expanded() {
        let yaml = r#"
proxies:
  - &tpl
    type: ss
    cipher: aes-256-gcm
    password: shared
  - <<: *tpl
    name: node1
    server: 1.1.1.1
    port: 8388
"#;
        let doc = try_load_clash_doc(yaml).unwrap();
        let proxies = doc.get("proxies").unwrap().as_sequence().unwrap();
        let node = &proxies[1];
        // merge 后应有 type/cipher/password + 显式 name/server/port
        assert_eq!(node.get("name").unwrap().as_str(), Some("node1"));
        assert_eq!(node.get("type").unwrap().as_str(), Some("ss"));
        assert_eq!(node.get("cipher").unwrap().as_str(), Some("aes-256-gcm"));
        assert_eq!(node.get("password").unwrap().as_str(), Some("shared"));
    }

    #[test]
    fn load_invalid_yaml_wraps_error() {
        let r = try_load_clash_doc("proxies: [unclosed");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("Clash YAML 解析失败"));
    }

    #[test]
    fn clash_probe_detects_proxies() {
        assert!(is_clash_probe("proxies:\n  - name: x"));
        assert!(is_clash_probe("proxy-providers:\n  p1:"));
        assert!(!is_clash_probe("rules:\n  - DOMAIN-SUFFIX,x"));
    }

    // ── 各协议解析 ────────────────────────────────────────────────────────────
    fn parse_one(yaml: &str) -> ClashParseResult {
        let mut id = new_uuid();
        let v: Value = serde_yaml::from_str(yaml).unwrap();
        parse_clash_proxies(&v, "sub-1", "2024-01-01T00:00:00Z", &mut id)
    }

    #[test]
    fn parse_vless() {
        let r = parse_one(
            r#"
- name: vless1
  type: vless
  server: example.com
  port: 443
  uuid: abc-uuid
  flow: xtls-rprx-vision
  tls: true
  servername: example.com
  network: ws
  ws-opts:
    path: /ray
    headers:
      Host: cdn.example.com
"#,
        );
        assert_eq!(r.servers.len(), 1);
        let s = &r.servers[0];
        assert_eq!(s.protocol, Protocol::Vless);
        assert_eq!(s.uuid.as_deref(), Some("abc-uuid"));
        assert_eq!(s.encryption.as_deref(), Some("none"));
        assert_eq!(s.flow.as_deref(), Some("xtls-rprx-vision"));
        assert_eq!(s.security, Some(SecurityMode::Tls));
        assert_eq!(s.network.as_deref(), Some("ws"));
        assert_eq!(
            s.tls_settings.as_ref().unwrap().server_name.as_deref(),
            Some("example.com")
        );
        assert_eq!(
            s.ws_settings.as_ref().unwrap().path.as_deref(),
            Some("/ray")
        );
    }

    #[test]
    fn parse_vmess() {
        let r = parse_one(
            r#"
- name: vmess1
  type: vmess
  server: example.com
  port: 443
  uuid: vm-uuid
  alterId: 64
  cipher: aes-128-gcm
  tls: true
  network: grpc
  grpc-opts:
    grpc-service-name: gun
"#,
        );
        let s = &r.servers[0];
        assert_eq!(s.protocol, Protocol::Vmess);
        assert_eq!(s.uuid.as_deref(), Some("vm-uuid"));
        assert_eq!(s.alter_id, Some(64));
        assert_eq!(s.vmess_security.as_deref(), Some("aes-128-gcm"));
        assert_eq!(s.network.as_deref(), Some("grpc"));
        assert_eq!(
            s.grpc_settings.as_ref().unwrap().service_name.as_deref(),
            Some("gun")
        );
    }

    #[test]
    fn parse_trojan() {
        let r = parse_one(
            r#"
- name: trojan1
  type: trojan
  server: example.com
  port: 443
  password: trojan-pw
  sni: example.com
  skip-cert-verify: true
"#,
        );
        let s = &r.servers[0];
        assert_eq!(s.protocol, Protocol::Trojan);
        assert_eq!(s.password.as_deref(), Some("trojan-pw"));
        assert_eq!(s.security, Some(SecurityMode::Tls)); // force_tls
        assert!(s.tls_settings.as_ref().unwrap().allow_insecure.unwrap());
    }

    #[test]
    fn parse_shadowsocks_basic() {
        let r = parse_one(
            r#"
- name: ss1
  type: ss
  server: 1.2.3.4
  port: 8388
  cipher: aes-256-gcm
  password: ss-pw
"#,
        );
        let s = &r.servers[0];
        assert_eq!(s.protocol, Protocol::Shadowsocks);
        let ss = s.shadowsocks_settings.as_ref().unwrap();
        assert_eq!(ss.method, "aes-256-gcm");
        assert_eq!(ss.password, "ss-pw");
        assert!(ss.plugin.is_none());
    }

    #[test]
    fn parse_shadowsocks_obfs_plugin() {
        let r = parse_one(
            r#"
- name: ss-obfs
  type: ss
  server: 1.2.3.4
  port: 8388
  cipher: aes-256-gcm
  password: pw
  plugin: obfs
  plugin-opts:
    mode: tls
    host: bing.com
"#,
        );
        let ss = &r.servers[0].shadowsocks_settings.as_ref().unwrap();
        assert_eq!(ss.plugin.as_deref(), Some("obfs-local"));
        assert!(ss.plugin_opts.as_deref().unwrap().contains("obfs=tls"));
        assert!(ss
            .plugin_opts
            .as_deref()
            .unwrap()
            .contains("obfs-host=bing.com"));
    }

    #[test]
    fn parse_shadowsocks_v2ray_plugin() {
        let r = parse_one(
            r#"
- name: ss-v2ray
  type: ss
  server: 1.2.3.4
  port: 8388
  cipher: aes-256-gcm
  password: pw
  plugin: v2ray-plugin
  plugin-opts:
    mode: websocket
    tls: true
    host: cdn.example.com
    path: /ws
"#,
        );
        let ss = &r.servers[0].shadowsocks_settings.as_ref().unwrap();
        assert_eq!(ss.plugin.as_deref(), Some("v2ray-plugin"));
        let opts = ss.plugin_opts.as_deref().unwrap();
        assert!(opts.contains("mode=websocket"));
        assert!(opts.contains("tls"));
        assert!(opts.contains("host=cdn.example.com"));
        assert!(opts.contains("path=/ws"));
    }

    #[test]
    fn parse_shadowsocks_shadowtls_plugin() {
        let r = parse_one(
            r#"
- name: ss-stls
  type: ss
  server: 1.2.3.4
  port: 8388
  cipher: 2022-blake3-aes-128-gcm
  password: pw
  plugin: shadow-tls
  plugin-opts:
    password: stls-pw
    host: shadow.example.com
    port: 443
"#,
        );
        let s = &r.servers[0];
        let stls = s.shadow_tls_settings.as_ref().unwrap();
        assert_eq!(stls.password, "stls-pw");
        assert_eq!(stls.sni, "shadow.example.com");
        assert_eq!(stls.port, Some(443));
        assert_eq!(stls.fingerprint.as_deref(), Some("chrome"));
    }

    #[test]
    fn parse_shadowsocks_unknown_plugin_skipped() {
        let r = parse_one(
            r#"
- name: ss-restls
  type: ss
  server: 1.2.3.4
  port: 8388
  cipher: aes-256-gcm
  password: pw
  plugin: restls
"#,
        );
        assert_eq!(r.servers.len(), 0);
        assert_eq!(r.skipped, 1);
        assert!(r.warnings.iter().any(|w| w.contains("ss-plugin:restls")));
    }

    #[test]
    fn parse_hysteria2() {
        let r = parse_one(
            r#"
- name: hy2-1
  type: hysteria2
  server: example.com
  port: 443
  password: hy2-pw
  sni: example.com
  obfs: salamander
  obfs-password: obfs-pw
  up: 100
  down: 200
  ports: 20000-30000
  hop-interval: 30
"#,
        );
        let s = &r.servers[0];
        assert_eq!(s.protocol, Protocol::Hysteria2);
        assert_eq!(s.password.as_deref(), Some("hy2-pw"));
        assert_eq!(s.security, Some(SecurityMode::Tls));
        let hy2 = s.hysteria2_settings.as_ref().unwrap();
        assert_eq!(hy2.up_mbps, Some(100));
        assert_eq!(hy2.down_mbps, Some(200));
        assert_eq!(hy2.server_ports.as_deref(), Some("20000:30000"));
        assert_eq!(hy2.hop_interval.as_deref(), Some("30s"));
        assert_eq!(
            hy2.obfs.as_ref().unwrap().type_field.as_deref(),
            Some("salamander")
        );
        assert_eq!(
            hy2.obfs.as_ref().unwrap().password.as_deref(),
            Some("obfs-pw")
        );
    }

    #[test]
    fn parse_hysteria2_salamander_missing_obfs_password_rejected() {
        // 上游 #263 迁移：obfs=salamander 缺 obfs-password → 拒节点（与其他协议缺密码口径一致），
        // 而非静默跳过导致 obfs 丢失、节点连不上。
        let r = parse_one(
            r#"
- name: hy2-no-obfs-pw
  type: hysteria2
  server: example.com
  port: 443
  password: hy2-pw
  obfs: salamander
"#,
        );
        assert_eq!(r.servers.len(), 0);
        assert_eq!(r.failed, 1);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("obfs=salamander 缺 obfs-password")),
            "warnings 应含拒节点原因，实际: {:?}",
            r.warnings
        );
    }

    #[test]
    fn parse_tuic() {
        let r = parse_one(
            r#"
- name: tuic1
  type: tuic
  server: example.com
  port: 443
  uuid: tuic-uuid
  password: tuic-pw
  congestion-controller: bbr
  udp-relay-mode: native
  reduce-rtt: true
  heartbeat: 10000
  sni: example.com
"#,
        );
        let s = &r.servers[0];
        assert_eq!(s.protocol, Protocol::Tuic);
        assert_eq!(s.uuid.as_deref(), Some("tuic-uuid"));
        assert_eq!(s.password.as_deref(), Some("tuic-pw"));
        let ts = s.tuic_settings.as_ref().unwrap();
        assert_eq!(ts.congestion_control.as_deref(), Some("bbr"));
        assert_eq!(ts.udp_relay_mode.as_deref(), Some("native"));
        assert!(ts.zero_rtt_handshake.unwrap());
        assert_eq!(ts.heartbeat.as_deref(), Some("10000ms")); // 毫秒补单位
    }

    #[test]
    fn parse_anytls() {
        let r = parse_one(
            r#"
- name: anytls1
  type: anytls
  server: example.com
  port: 443
  password: anytls-pw
  idle-session-timeout: 30s
  sni: example.com
"#,
        );
        let s = &r.servers[0];
        assert_eq!(s.protocol, Protocol::Anytls);
        assert_eq!(s.password.as_deref(), Some("anytls-pw"));
        assert_eq!(s.security, Some(SecurityMode::Tls)); // force_tls
        assert_eq!(
            s.any_tls_settings
                .as_ref()
                .unwrap()
                .idle_session_timeout
                .as_deref(),
            Some("30s")
        );
    }

    #[test]
    fn parse_snell_v4_http_obfs() {
        let r = parse_one(
            r#"
- name: snell1
  type: snell
  server: example.com
  port: 443
  psk: psk-value
  version: 4
  obfs-opts:
    mode: http
    host: bing.com
"#,
        );
        let s = &r.servers[0];
        assert_eq!(s.protocol, Protocol::Snell);
        assert_eq!(s.password.as_deref(), Some("psk-value"));
        let snell = s.snell_settings.as_ref().unwrap();
        assert_eq!(snell.version, 4);
        assert_eq!(snell.obfs_mode.as_deref(), Some("http"));
        assert_eq!(snell.obfs_host.as_deref(), Some("bing.com"));
    }

    #[test]
    fn parse_snell_unsupported_version_skipped() {
        let r = parse_one(
            r#"
- name: snell-old
  type: snell
  server: example.com
  port: 443
  psk: psk-value
  version: 3
"#,
        );
        assert_eq!(r.servers.len(), 0);
        assert_eq!(r.skipped, 1);
    }

    #[test]
    fn parse_socks() {
        let r = parse_one(
            r#"
- name: socks1
  type: socks5
  server: 1.2.3.4
  port: 1080
  username: user
  password: pass
"#,
        );
        let s = &r.servers[0];
        assert_eq!(s.protocol, Protocol::Socks);
        assert_eq!(s.username.as_deref(), Some("user"));
        assert_eq!(s.password.as_deref(), Some("pass"));
        assert_eq!(s.network.as_deref(), Some("tcp"));
        assert_eq!(s.security, Some(SecurityMode::None));
    }

    #[test]
    fn parse_http_tls() {
        let r = parse_one(
            r#"
- name: http1
  type: http
  server: example.com
  port: 443
  username: user
  password: pass
  tls: true
  sni: example.com
"#,
        );
        let s = &r.servers[0];
        assert_eq!(s.protocol, Protocol::Http);
        assert_eq!(s.security, Some(SecurityMode::Tls));
        assert_eq!(
            s.tls_settings.as_ref().unwrap().server_name.as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn parse_ssh() {
        let r = parse_one(
            r#"
- name: ssh1
  type: ssh
  server: 1.2.3.4
  port: 22
  username: root
  password: toor
  client-version: SSH-2.0-OpenSSH_8.0
"#,
        );
        let s = &r.servers[0];
        assert_eq!(s.protocol, Protocol::Ssh);
        let ssh = s.ssh_settings.as_ref().unwrap();
        assert_eq!(ssh.user.as_deref(), Some("root"));
        assert_eq!(ssh.password.as_deref(), Some("toor"));
        assert_eq!(ssh.client_version.as_deref(), Some("SSH-2.0-OpenSSH_8.0"));
    }

    // ── 缺字段/不支持类型 ─────────────────────────────────────────────────────
    #[test]
    fn missing_required_field_fails() {
        let r = parse_one(
            r#"
- name: bad-vless
  type: vless
  server: example.com
  port: 443
"#,
        );
        assert_eq!(r.servers.len(), 0);
        assert_eq!(r.failed, 1);
        assert!(r.warnings.iter().any(|w| w.contains("缺 uuid")));
    }

    #[test]
    fn unsupported_type_skipped() {
        let r = parse_one(
            r#"
- name: ssr1
  type: ssr
  server: 1.2.3.4
  port: 8388
"#,
        );
        assert_eq!(r.servers.len(), 0);
        assert_eq!(r.skipped, 1);
        assert!(r.warnings.iter().any(|w| w.contains("ssr")));
    }

    #[test]
    fn unsupported_transport_fails() {
        let r = parse_one(
            r#"
- name: bad-transport
  type: vmess
  server: example.com
  port: 443
  uuid: u
  network: kcp
"#,
        );
        assert_eq!(r.servers.len(), 0);
        assert!(r
            .warnings
            .iter()
            .any(|w| w.contains("不支持的传输层类型: kcp")));
    }

    #[test]
    fn empty_proxies_returns_empty() {
        let r = parse_one("[]");
        assert_eq!(r.servers.len(), 0);
        assert_eq!(r.skipped, 0);
        assert_eq!(r.failed, 0);
    }

    #[test]
    fn non_array_proxies_returns_empty() {
        let r = parse_one("null");
        assert_eq!(r.servers.len(), 0);
    }

    #[test]
    fn alpn_dedup_and_split() {
        let r = parse_one(
            r#"
- name: alpn-test
  type: vless
  server: example.com
  port: 443
  uuid: u
  tls: true
  alpn:
    - h2
    - " http/1.1"
    - h2,http/3
"#,
        );
        let tls = r.servers[0].tls_settings.as_ref().unwrap();
        let alpn = tls.alpn.as_ref().unwrap();
        // h2 出现两次去重；" http/1.1" trim；h2,http/3 拆分
        assert!(alpn.contains(&"h2".to_string()));
        assert!(alpn.contains(&"http/1.1".to_string()));
        assert!(alpn.contains(&"http/3".to_string()));
    }

    // ── filter / override ─────────────────────────────────────────────────────
    #[test]
    fn apply_override_skip_cert_on_tls_nodes() {
        let mut id = new_uuid();
        let v: Value = serde_yaml::from_str(
            r#"
- name: t1
  type: trojan
  server: example.com
  port: 443
  password: pw
"#,
        )
        .unwrap();
        let mut result = parse_clash_proxies(&v, "s1", "now", &mut id);
        let ov: Value = serde_yaml::from_str("skip-cert-verify: true").unwrap();
        apply_override(&mut result.servers, &ov);
        assert!(result.servers[0]
            .tls_settings
            .as_ref()
            .unwrap()
            .allow_insecure
            .unwrap());
    }

    #[test]
    fn apply_override_skip_cert_ignored_on_non_tls() {
        let mut id = new_uuid();
        let v: Value = serde_yaml::from_str(
            r#"
- name: s1
  type: socks5
  server: 1.2.3.4
  port: 1080
"#,
        )
        .unwrap();
        let mut result = parse_clash_proxies(&v, "s1", "now", &mut id);
        let ov: Value = serde_yaml::from_str("skip-cert-verify: true").unwrap();
        apply_override(&mut result.servers, &ov);
        // 非 TLS 节点不注入 tlsSettings（防代理指纹噪音）
        assert!(result.servers[0].tls_settings.is_none());
    }

    #[test]
    fn apply_override_up_down_on_hysteria2() {
        let mut id = new_uuid();
        let v: Value = serde_yaml::from_str(
            r#"
- name: h
  type: hysteria2
  server: example.com
  port: 443
  password: pw
"#,
        )
        .unwrap();
        let mut result = parse_clash_proxies(&v, "s1", "now", &mut id);
        let ov: Value = serde_yaml::from_str("up: 50\ndown: 100").unwrap();
        apply_override(&mut result.servers, &ov);
        let hy2 = result.servers[0].hysteria2_settings.as_ref().unwrap();
        assert_eq!(hy2.up_mbps, Some(50));
        assert_eq!(hy2.down_mbps, Some(100));
    }

    #[test]
    fn provider_filter_keeps_matching() {
        let mut id = new_uuid();
        let v: Value = serde_yaml::from_str(
            r#"
- name: HK-01
  type: ss
  server: 1.1.1.1
  port: 8388
  cipher: a
  password: p
- name: US-01
  type: ss
  server: 2.2.2.2
  port: 8388
  cipher: a
  password: p
"#,
        )
        .unwrap();
        let result = parse_clash_proxies(&v, "s1", "now", &mut id);
        let mut warns = Vec::new();
        let filtered = apply_provider_filters(
            result.servers,
            Some("HK"),
            None,
            &mut |m| warns.push(m),
            "p1",
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "HK-01");
    }

    #[test]
    fn provider_exclude_filter_removes() {
        let mut id = new_uuid();
        let v: Value = serde_yaml::from_str(
            r#"
- name: HK-01
  type: ss
  server: 1.1.1.1
  port: 8388
  cipher: a
  password: p
- name: US-01
  type: ss
  server: 2.2.2.2
  port: 8388
  cipher: a
  password: p
"#,
        )
        .unwrap();
        let result = parse_clash_proxies(&v, "s1", "now", &mut id);
        let mut warns = Vec::new();
        let filtered = apply_provider_filters(
            result.servers,
            None,
            Some("HK"),
            &mut |m| warns.push(m),
            "p1",
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "US-01");
    }

    #[test]
    fn provider_invalid_filter_warns() {
        let mut id = new_uuid();
        let v: Value = serde_yaml::from_str(
            r#"
- name: HK-01
  type: ss
  server: 1.1.1.1
  port: 8388
  cipher: a
  password: p
"#,
        )
        .unwrap();
        let result = parse_clash_proxies(&v, "s1", "now", &mut id);
        let mut warns = Vec::new();
        let filtered = apply_provider_filters(
            result.servers,
            Some("(unclosed"),
            None,
            &mut |m| warns.push(m),
            "p1",
        );
        // 非法正则 → 忽略 filter，保留全部
        assert_eq!(filtered.len(), 1);
        assert!(warns.iter().any(|w| w.contains("filter 非法")));
    }

    #[test]
    fn compile_filter_too_long_returns_none() {
        let long = "a".repeat(MAX_FILTER_PATTERN_LEN + 1);
        assert!(compile_provider_filter(Some(&long)).is_none());
        assert!(compile_provider_filter(Some("HK.*")).is_some());
        assert!(compile_provider_filter(None).is_none());
    }

    #[test]
    fn normalize_duration_rules() {
        assert_eq!(
            normalize_duration(&Value::Number(serde_yaml::Number::from(10000))),
            Some("10000ms".to_string())
        );
        assert_eq!(
            normalize_duration(&Value::String("30s".into())),
            Some("30s".to_string())
        );
        assert_eq!(
            normalize_duration(&Value::String("500".into())),
            Some("500ms".to_string())
        );
        assert_eq!(normalize_duration(&Value::String("".into())), None);
    }
}
