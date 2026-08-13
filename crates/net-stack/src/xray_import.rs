//! Xray / v2ray JSON 配置 → [`ServerConfig`] 解析（上游 `main/services/xray-import.ts` 1:1 移植，纯逻辑）。
//!
//! 与 sing-box JSON 的差异：xray outbound 用 `protocol` + `settings`（vnext/servers）+ `streamSettings`，
//! 而 sing-box 用扁平 `type` + 同级字段。本模块只覆盖 Polaris 已建模的主流协议（vmess/vless/trojan/
//! shadowsocks）；其余协议跳过并报告（**不透传 custom**：custom 落点是 sing-box outbound schema，
//! xray schema 不兼容）。
//!
//! 逐 outbound 处理，单条失败不影响其它（对齐 [`crate::clash_parser::parse_clash_proxies`] 与
//! `ClashSubscriptionParser.parseClashProxies` 的容错分层）。
//!
//! **与 Polaris 边界归一（R4）协同**：`flow` / `vmessSecurity` / `fingerprint` 直接构造 `ServerConfig`
//! 时不经 serde `de_opt_token` 钩子，故此处显式过 [`normalize_token`]（与 [`crate::share_link`] /
//! [`crate::clash_parser`] 同口径），否则 `"Chrome"` / `"AES-128-GCM"` 大小写变体会让 sing-box FATAL
//! （上游 #298 修的正是此类）。`security` 走 [`SecurityMode`] 类型化归一，天然消除大小写变体。
//!
//! **未知传输 → tcp（DESIGN-REVIEW(xray-transport-fallback)）**：忠实迁移 上游 `applyStreamSettings`
//! 的 `else { network = 'tcp' }`——xray 专属传输（kcp/quic/xhttp 等）静默降级为 tcp。这与 Polaris
//! 分享链接族的 #263「未知传输=整节点拒绝」纪律相反；此分歧源自 oracle，见模块尾部 review 注记。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use serde_json::Value;

use polaris_config_engine::user_config::normalize::normalize_token;
use polaris_config_engine::user_config::protocol_settings::{
    GrpcSettings, HttpSettings, RealitySettings, ShadowsocksSettings, TlsSettings,
    WebSocketSettings,
};
use polaris_config_engine::user_config::server_config::{Protocol, SecurityMode, ServerConfig};

use crate::clash_parser::ClashParseResult;

/// 本模块可映射的 xray `outbound.protocol`（对齐 上游 `XRAY_SUPPORTED`）。
const XRAY_SUPPORTED: &[&str] = &["vmess", "vless", "trojan", "shadowsocks"];
/// xray 内部/非节点 outbound（忽略，**不计入 skipped**；对齐 上游 `XRAY_INTERNAL`）。
const XRAY_INTERNAL: &[&str] = &["freedom", "blackhole", "dns", "loopback"];

// ── 标量规整（对齐 上游 xray-import 的 str/num；str 含 boolean→String）────────────────

/// 上游 `str(v)`：字符串/数字/布尔 → `String`，其余（含 null/缺席）→ `None`。
fn str_val(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// 上游的「truthy 字符串」形态（`str(v) || 'x'` 里 `""` 视作缺席）：空串归 `None`。
fn str_ne(v: Option<&Value>) -> Option<String> {
    str_val(v).filter(|s| !s.is_empty())
}

/// 上游 `num(v)`：数字（整数值）/ 数字串 → `u32`，其余 → `None`（与 [`crate::clash_parser`] 同口径）。
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

/// 端口规整：上游 `!port`（0/NaN → 拒）+ Polaris `u16` 上界（>65535 → 拒，防 `as u16` 静默截断）。
fn port_val(v: Option<&Value>) -> Option<u16> {
    let p = num_val(v)?;
    (1..=u32::from(u16::MAX)).contains(&p).then_some(p as u16)
}

/// 取 `parent.key` 数组的首元素（上游 `parent.key?.[0]`）。
fn first<'a>(parent: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    parent?.get(key)?.as_array()?.first()
}

// ── streamSettings → 传输/安全层 ─────────────────────────────────────────────────

/// xray `streamSettings` → `ServerConfig` 传输/安全层字段（上游 `applyStreamSettings`）。
fn apply_stream_settings(server: &mut ServerConfig, ss: Option<&Value>) {
    let Some(ss) = ss.filter(|v| v.is_object()) else {
        return;
    };

    // —— 传输层 —— （上游: (str(ss.network) || 'tcp').toLowerCase()）
    let network = str_ne(ss.get("network"))
        .unwrap_or_else(|| "tcp".to_string())
        .to_ascii_lowercase();
    match network.as_str() {
        "ws" => {
            server.network = Some("ws".to_string());
            let ws = ss.get("wsSettings");
            let headers = ws.and_then(|w| w.get("headers"));
            let host = str_ne(headers.and_then(|h| h.get("Host")))
                .or_else(|| str_ne(headers.and_then(|h| h.get("host"))));
            server.ws_settings = Some(WebSocketSettings {
                path: Some(
                    str_ne(ws.and_then(|w| w.get("path"))).unwrap_or_else(|| "/".to_string()),
                ),
                headers: host.map(host_header),
                ..Default::default()
            });
        }
        "grpc" => {
            server.network = Some("grpc".to_string());
            let grpc = ss.get("grpcSettings");
            server.grpc_settings = Some(GrpcSettings {
                // 上游: serviceName: str(grpc.serviceName) || ''（恒置，缺省空串）。
                service_name: Some(
                    str_val(grpc.and_then(|g| g.get("serviceName"))).unwrap_or_default(),
                ),
                ..Default::default()
            });
        }
        "h2" | "http" => {
            server.network = Some("http".to_string());
            let h2 = ss.get("httpSettings");
            let host_val = h2.and_then(|h| h.get("host"));
            server.http_settings = Some(HttpSettings {
                path: Some(
                    str_ne(h2.and_then(|h| h.get("path"))).unwrap_or_else(|| "/".to_string()),
                ),
                // 上游: Array.isArray(host) ? host : str(host) ? [str(host)] : undefined
                host: match host_val {
                    Some(Value::Array(a)) => {
                        Some(a.iter().filter_map(|x| str_val(Some(x))).collect())
                    }
                    other => str_ne(other).map(|h| vec![h]),
                },
                ..Default::default()
            });
        }
        "httpupgrade" => {
            server.network = Some("httpupgrade".to_string());
            let hu = ss.get("httpupgradeSettings");
            let host = str_ne(hu.and_then(|h| h.get("host")));
            server.ws_settings = Some(WebSocketSettings {
                path: Some(
                    str_ne(hu.and_then(|h| h.get("path"))).unwrap_or_else(|| "/".to_string()),
                ),
                headers: host.map(host_header),
                ..Default::default()
            });
        }
        // DESIGN-REVIEW(xray-transport-fallback)：未知传输静默降级 tcp（忠实 上游；与 #263 相反）。
        _ => server.network = Some("tcp".to_string()),
    }

    // —— 安全层 —— （上游: (str(ss.security) || 'none').toLowerCase()）
    let security = str_ne(ss.get("security"))
        .unwrap_or_else(|| "none".to_string())
        .to_ascii_lowercase();
    match security.as_str() {
        "tls" | "xtls" => {
            server.security = Some(SecurityMode::Tls);
            let tls = ss.get("tlsSettings");
            let alpn = tls
                .and_then(|t| t.get("alpn"))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| str_val(Some(x))).collect());
            server.tls_settings = Some(TlsSettings {
                server_name: str_val(tls.and_then(|t| t.get("serverName"))),
                allow_insecure: Some(
                    tls.and_then(|t| t.get("allowInsecure"))
                        .and_then(Value::as_bool)
                        == Some(true),
                ),
                alpn,
                fingerprint: normalize_token(
                    &str_ne(tls.and_then(|t| t.get("fingerprint")))
                        .unwrap_or_else(|| "chrome".to_string()),
                ),
                ..Default::default()
            });
        }
        "reality" => {
            server.security = Some(SecurityMode::Reality);
            let reality = ss.get("realitySettings");
            server.tls_settings = Some(TlsSettings {
                server_name: str_val(reality.and_then(|r| r.get("serverName"))),
                fingerprint: normalize_token(
                    &str_ne(reality.and_then(|r| r.get("fingerprint")))
                        .unwrap_or_else(|| "chrome".to_string()),
                ),
                ..Default::default()
            });
            server.reality_settings = Some(RealitySettings {
                // 上游: str(reality.publicKey) || ''（缺省空串，不拒节点——xray 分支的 oracle 选择）。
                public_key: str_val(reality.and_then(|r| r.get("publicKey"))).unwrap_or_default(),
                short_id: str_val(reality.and_then(|r| r.get("shortId"))),
            });
        }
        _ => server.security = Some(SecurityMode::None),
    }
}

/// 单 Host 头 → `{ "Host": value }`（sing-box ws/httpupgrade transport headers 形态）。
fn host_header(host: String) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("Host".to_string(), host);
    m
}

// ── 单 outbound → ServerConfig ───────────────────────────────────────────────────

/// 新建基础节点（id 由注入闭包生成，对齐 上游 `randomUUID()`；仅校验通过后调用，失败节点不耗 id）。
fn new_server(
    id_gen: &mut impl FnMut() -> String,
    tag: &Option<String>,
    protocol: Protocol,
    address: String,
    port: u16,
    sub_id: &str,
    now: &str,
) -> ServerConfig {
    let name = tag.clone().unwrap_or_else(|| format!("{address}:{port}"));
    ServerConfig {
        id: id_gen(),
        name,
        protocol,
        address,
        port,
        // 与 parse_url_list 同：统一挂订阅 id（本地导入传空串后由命令层剥离该字段）。
        subscription_id: Some(sub_id.to_string()),
        created_at: Some(now.to_string()),
        updated_at: Some(now.to_string()),
        ..Default::default()
    }
}

/// 单条 xray outbound → [`ServerConfig`]（字段缺失 → `None`，对齐 上游 `mapXrayOutbound` 的 return null）。
fn map_xray_outbound(
    o: &Value,
    proto: &str,
    sub_id: &str,
    now: &str,
    id_gen: &mut impl FnMut() -> String,
) -> Option<ServerConfig> {
    let settings = o.get("settings");
    let tag = str_ne(o.get("tag"));

    match proto {
        "vmess" | "vless" => {
            // settings.vnext[0] → { address, port, users[0] { id, alterId/security | flow } }
            let vnext = first(settings, "vnext")?;
            let address = str_ne(vnext.get("address"))?;
            let port = port_val(vnext.get("port"))?;
            let user = first(Some(vnext), "users");
            let uuid = str_ne(user.and_then(|u| u.get("id")))?;

            let protocol = if proto == "vmess" {
                Protocol::Vmess
            } else {
                Protocol::Vless
            };
            let mut server = new_server(id_gen, &tag, protocol, address, port, sub_id, now);
            server.uuid = Some(uuid);
            if proto == "vmess" {
                server.alter_id = Some(num_val(user.and_then(|u| u.get("alterId"))).unwrap_or(0));
                // R4 归一（`AES-128-GCM` → `aes-128-gcm`），缺省 auto。
                server.vmess_security = normalize_token(
                    &str_ne(user.and_then(|u| u.get("security")))
                        .unwrap_or_else(|| "auto".to_string()),
                );
            } else {
                // R4 归一 flow（`XTLS-RPRX-Vision` → `xtls-rprx-vision`），缺省 None。
                server.flow =
                    str_ne(user.and_then(|u| u.get("flow"))).and_then(|f| normalize_token(&f));
            }
            apply_stream_settings(&mut server, o.get("streamSettings"));
            Some(server)
        }
        "trojan" => {
            let srv = first(settings, "servers")?;
            let address = str_ne(srv.get("address"))?;
            let port = port_val(srv.get("port"))?;
            let password = str_ne(srv.get("password"))?;
            let mut server = new_server(id_gen, &tag, Protocol::Trojan, address, port, sub_id, now);
            server.password = Some(password);
            apply_stream_settings(&mut server, o.get("streamSettings"));
            Some(server)
        }
        "shadowsocks" => {
            let srv = first(settings, "servers")?;
            let address = str_ne(srv.get("address"))?;
            let port = port_val(srv.get("port"))?;
            let method = str_ne(srv.get("method"))?;
            let password = str_ne(srv.get("password"))?;
            let mut server = new_server(
                id_gen,
                &tag,
                Protocol::Shadowsocks,
                address,
                port,
                sub_id,
                now,
            );
            server.shadowsocks_settings = Some(ShadowsocksSettings {
                method,
                password,
                ..Default::default()
            });
            apply_stream_settings(&mut server, o.get("streamSettings"));
            Some(server)
        }
        _ => None,
    }
}

// ── 入口 ─────────────────────────────────────────────────────────────────────────

/// 是否「xray 形态」的 outbounds（上游 `looksXray`）：任一 outbound 有字符串 `protocol` 且**无** `type`。
///
/// sing-box outbound 用扁平 `type`；xray 用 `protocol`+`settings`。二者共用 `outbounds` 键 → 靠此区分。
#[must_use]
pub fn looks_like_xray(outbounds: &[Value]) -> bool {
    outbounds.iter().any(|o| {
        o.is_object()
            && o.get("protocol").and_then(Value::as_str).is_some()
            && o.get("type").is_none()
    })
}

/// xray `outbounds[]` → 节点集 + 统计（上游 `parseXrayOutbounds`）。
///
/// 逐条处理：`XRAY_INTERNAL` 忽略；不支持协议计 `skipped` + 归并到 warning；受支持但字段缺失计 `failed`。
/// `sub_id` 挂到每个节点（本地导入传空串，命令层剥离）；`id_gen` 注入 UUID（对齐 `randomUUID`）。
pub fn parse_xray_outbounds(
    outbounds: &Value,
    sub_id: &str,
    now: &str,
    id_gen: &mut impl FnMut() -> String,
) -> ClashParseResult {
    let mut r = ClashParseResult::default();
    let Some(arr) = outbounds.as_array() else {
        return r;
    };

    // 保持插入序（对齐 上游 Map 迭代序）：跳过协议 → 计数，供 warning 拼装。
    let mut skip_by_proto: Vec<(String, usize)> = Vec::new();

    for ob in arr {
        if !ob.is_object() {
            r.failed += 1;
            continue;
        }
        let proto = str_val(ob.get("protocol"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !XRAY_SUPPORTED.contains(&proto.as_str()) {
            if !XRAY_INTERNAL.contains(&proto.as_str()) {
                r.skipped += 1;
                let key = if proto.is_empty() {
                    "(empty)".to_string()
                } else {
                    proto.clone()
                };
                match skip_by_proto.iter_mut().find(|(p, _)| *p == key) {
                    Some((_, c)) => *c += 1,
                    None => skip_by_proto.push((key, 1)),
                }
            }
            continue;
        }
        match map_xray_outbound(ob, &proto, sub_id, now, id_gen) {
            Some(server) => r.servers.push(server),
            None => r.failed += 1,
        }
    }

    if !skip_by_proto.is_empty() {
        let detail = skip_by_proto
            .iter()
            .map(|(p, c)| format!("{p}({c})"))
            .collect::<Vec<_>>()
            .join(", ");
        r.warnings
            .push(format!("跳过 {} 个不支持的 Xray 协议: {detail}", r.skipped));
    }
    r.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 递增 id 生成器（可复现，便于断言）。
    fn id_gen() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("id-{n}")
        }
    }

    fn parse(v: Value) -> ClashParseResult {
        let mut g = id_gen();
        parse_xray_outbounds(
            v.get("outbounds").unwrap(),
            "sub1",
            "2026-07-18T00:00:00Z",
            &mut g,
        )
    }

    // ── looks_like_xray（区分 xray/sing-box）─────────────────────────────────────
    #[test]
    fn looks_like_xray_distinguishes_schemas() {
        // xray：protocol 存在、无 type。
        let xray = json!([{ "protocol": "vless", "settings": {} }]);
        assert!(looks_like_xray(xray.as_array().unwrap()));
        // sing-box：有 type。
        let singbox = json!([{ "type": "vless", "server": "a.com" }]);
        assert!(!looks_like_xray(singbox.as_array().unwrap()));
        // protocol 存在但同时有 type（歧义）→ 非 xray（对齐 `type === undefined`）。
        let both = json!([{ "protocol": "vless", "type": "vless" }]);
        assert!(!looks_like_xray(both.as_array().unwrap()));
        // 空 / 非对象。
        assert!(!looks_like_xray(&[]));
        assert!(!looks_like_xray(json!(["x", 1]).as_array().unwrap()));
    }

    // ── vless + ws + tls：全字段映射（打断任一映射 → 断言转红）──────────────────────
    #[test]
    fn vless_ws_tls_full_mapping() {
        let out = parse(json!({ "outbounds": [{
            "protocol": "vless",
            "tag": "HK-01",
            "settings": { "vnext": [{
                "address": "a.example.com",
                "port": 443,
                "users": [{ "id": "uuid-1", "flow": "XTLS-RPRX-Vision" }]
            }] },
            "streamSettings": {
                "network": "ws",
                "wsSettings": { "path": "/ray", "headers": { "Host": "cdn.example.com" } },
                "security": "tls",
                "tlsSettings": { "serverName": "sni.example.com", "allowInsecure": true, "fingerprint": "Chrome", "alpn": ["h2", "http/1.1"] }
            }
        }] }));
        assert_eq!(out.servers.len(), 1, "一个合法 vless 节点");
        let s = &out.servers[0];
        assert_eq!(s.name, "HK-01", "tag → name");
        assert_eq!(s.protocol, Protocol::Vless);
        assert_eq!(s.address, "a.example.com");
        assert_eq!(s.port, 443);
        assert_eq!(s.uuid.as_deref(), Some("uuid-1"));
        assert_eq!(
            s.flow.as_deref(),
            Some("xtls-rprx-vision"),
            "flow R4 归一小写"
        );
        assert_eq!(s.network.as_deref(), Some("ws"));
        let ws = s.ws_settings.as_ref().expect("ws_settings");
        assert_eq!(ws.path.as_deref(), Some("/ray"));
        assert_eq!(
            ws.headers
                .as_ref()
                .and_then(|h| h.get("Host"))
                .map(String::as_str),
            Some("cdn.example.com")
        );
        assert_eq!(s.security, Some(SecurityMode::Tls));
        let tls = s.tls_settings.as_ref().expect("tls_settings");
        assert_eq!(tls.server_name.as_deref(), Some("sni.example.com"));
        assert_eq!(tls.allow_insecure, Some(true));
        assert_eq!(
            tls.fingerprint.as_deref(),
            Some("chrome"),
            "fingerprint R4 归一小写"
        );
        assert_eq!(
            tls.alpn.as_deref(),
            Some(&["h2".to_string(), "http/1.1".to_string()][..])
        );
        assert_eq!(s.subscription_id.as_deref(), Some("sub1"), "挂订阅 id");
    }

    // ── vmess + grpc：alterId/security/serviceName ───────────────────────────────
    #[test]
    fn vmess_grpc_mapping() {
        let out = parse(json!({ "outbounds": [{
            "protocol": "vmess",
            "settings": { "vnext": [{
                "address": "b.example.com",
                "port": "8443",
                "users": [{ "id": "uuid-2", "alterId": 4, "security": "AES-128-GCM" }]
            }] },
            "streamSettings": { "network": "grpc", "grpcSettings": { "serviceName": "gsvc" } }
        }] }));
        let s = &out.servers[0];
        assert_eq!(s.protocol, Protocol::Vmess);
        assert_eq!(s.port, 8443, "字符串端口规整");
        assert_eq!(s.alter_id, Some(4));
        assert_eq!(
            s.vmess_security.as_deref(),
            Some("aes-128-gcm"),
            "vmessSecurity R4 归一"
        );
        assert_eq!(s.name, "b.example.com:8443", "无 tag → address:port");
        assert_eq!(s.network.as_deref(), Some("grpc"));
        assert_eq!(
            s.grpc_settings
                .as_ref()
                .and_then(|g| g.service_name.as_deref()),
            Some("gsvc")
        );
    }

    // ── trojan + reality ─────────────────────────────────────────────────────────
    #[test]
    fn trojan_reality_mapping() {
        let out = parse(json!({ "outbounds": [{
            "protocol": "trojan",
            "settings": { "servers": [{ "address": "c.example.com", "port": 443, "password": "pw" }] },
            "streamSettings": {
                "network": "tcp",
                "security": "reality",
                "realitySettings": { "serverName": "r.example.com", "publicKey": "PBK", "shortId": "sid" }
            }
        }] }));
        let s = &out.servers[0];
        assert_eq!(s.protocol, Protocol::Trojan);
        assert_eq!(s.password.as_deref(), Some("pw"));
        assert_eq!(s.security, Some(SecurityMode::Reality));
        let r = s.reality_settings.as_ref().expect("reality_settings");
        assert_eq!(r.public_key, "PBK");
        assert_eq!(r.short_id.as_deref(), Some("sid"));
        assert_eq!(
            s.tls_settings
                .as_ref()
                .and_then(|t| t.server_name.as_deref()),
            Some("r.example.com")
        );
    }

    // ── shadowsocks ──────────────────────────────────────────────────────────────
    #[test]
    fn shadowsocks_mapping() {
        let out = parse(json!({ "outbounds": [{
            "protocol": "shadowsocks",
            "settings": { "servers": [{ "address": "d.example.com", "port": 8388, "method": "aes-256-gcm", "password": "sspw" }] }
        }] }));
        let s = &out.servers[0];
        assert_eq!(s.protocol, Protocol::Shadowsocks);
        let ss = s
            .shadowsocks_settings
            .as_ref()
            .expect("shadowsocks_settings");
        assert_eq!(ss.method, "aes-256-gcm");
        assert_eq!(ss.password, "sspw");
    }

    // ── skipped / internal / failed 统计（打断分类 → 计数断言转红）──────────────────
    #[test]
    fn skip_internal_and_count_unsupported() {
        let out = parse(json!({ "outbounds": [
            { "protocol": "freedom" },                                   // internal → 不计
            { "protocol": "blackhole" },                                 // internal → 不计
            { "protocol": "socks", "settings": {} },                     // unsupported → skipped
            { "protocol": "http", "settings": {} },                      // unsupported → skipped
            { "protocol": "vless", "settings": { "vnext": [{ "address": "a.com", "port": 443, "users": [{ "id": "u" }] }] } }
        ] }));
        assert_eq!(out.servers.len(), 1, "仅 vless 入库");
        assert_eq!(
            out.skipped, 2,
            "socks + http 计 skipped（freedom/blackhole 不计）"
        );
        assert_eq!(out.failed, 0);
        assert_eq!(out.warnings.len(), 1);
        assert!(
            out.warnings[0].contains("socks(1)") && out.warnings[0].contains("http(1)"),
            "warning 列明协议计数: {}",
            out.warnings[0]
        );
    }

    #[test]
    fn incomplete_nodes_count_failed() {
        let out = parse(json!({ "outbounds": [
            { "protocol": "vless", "settings": { "vnext": [{ "address": "a.com", "port": 443, "users": [{}] }] } }, // 缺 uuid
            { "protocol": "vmess", "settings": { "vnext": [{ "address": "", "port": 443, "users": [{ "id": "u" }] }] } }, // 空 address
            { "protocol": "trojan", "settings": { "servers": [{ "address": "c.com", "port": 443 }] } }, // 缺 password
            { "protocol": "vless", "settings": {} } // 缺 vnext
        ] }));
        assert_eq!(out.servers.len(), 0);
        assert_eq!(out.failed, 4, "四个受支持但字段缺失 → failed");
        assert_eq!(out.skipped, 0);
    }

    #[test]
    fn port_zero_and_overflow_rejected() {
        let out = parse(json!({ "outbounds": [
            { "protocol": "vless", "settings": { "vnext": [{ "address": "a.com", "port": 0, "users": [{ "id": "u" }] }] } },      // port 0 → 拒
            { "protocol": "vless", "settings": { "vnext": [{ "address": "a.com", "port": 70000, "users": [{ "id": "u" }] }] } }   // >65535 → 拒
        ] }));
        assert_eq!(
            out.servers.len(),
            0,
            "port 0 / 越界必须拒（不 as u16 静默截断）"
        );
        assert_eq!(out.failed, 2);
    }

    // ── DESIGN-REVIEW(xray-transport-fallback)：未知传输 → tcp（锁 上游 现行行为）──────
    #[test]
    fn unknown_transport_falls_back_to_tcp() {
        let out = parse(json!({ "outbounds": [{
            "protocol": "vless",
            "settings": { "vnext": [{ "address": "a.com", "port": 443, "users": [{ "id": "u" }] }] },
            "streamSettings": { "network": "kcp" }
        }] }));
        assert_eq!(
            out.servers[0].network.as_deref(),
            Some("tcp"),
            "忠实 上游：未知传输降级 tcp"
        );
    }

    // ── h2 host 数组 / 单串两形态 ─────────────────────────────────────────────────
    #[test]
    fn http_host_array_and_scalar() {
        let out = parse(json!({ "outbounds": [
            { "protocol": "vless", "settings": { "vnext": [{ "address": "a.com", "port": 443, "users": [{ "id": "u" }] }] },
              "streamSettings": { "network": "h2", "httpSettings": { "host": ["h1.com", "h2.com"], "path": "/p" } } },
            { "protocol": "vless", "settings": { "vnext": [{ "address": "b.com", "port": 443, "users": [{ "id": "u" }] }] },
              "streamSettings": { "network": "http", "httpSettings": { "host": "single.com" } } }
        ] }));
        let a = &out.servers[0];
        assert_eq!(a.network.as_deref(), Some("http"), "h2 → http");
        let ha = a.http_settings.as_ref().expect("http_settings");
        assert_eq!(
            ha.host.as_deref(),
            Some(&["h1.com".to_string(), "h2.com".to_string()][..])
        );
        assert_eq!(ha.path.as_deref(), Some("/p"));
        let b = &out.servers[1];
        let hb = b.http_settings.as_ref().expect("http_settings");
        assert_eq!(
            hb.host.as_deref(),
            Some(&["single.com".to_string()][..]),
            "标量 host → 单元素数组"
        );
        assert_eq!(hb.path.as_deref(), Some("/"), "缺 path 缺省 /");
    }

    #[test]
    fn non_array_outbounds_is_empty() {
        let mut g = id_gen();
        let r = parse_xray_outbounds(&json!({}), "s", "now", &mut g);
        assert_eq!(r.servers.len(), 0);
        assert_eq!((r.skipped, r.failed), (0, 0));
    }
}
