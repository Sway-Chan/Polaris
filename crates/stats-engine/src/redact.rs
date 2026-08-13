//! 诊断报告脱敏 —— 纯函数，零 IO、零网络。
//!
//! Polaris 锚点：`shared/diagnostic-redact.ts`(560) 的 `redactDeep` / `redactUrlValue` /
//! `collectNodeIdentifiers` / `redactIdentifiers`。
//!
//! # 红线（不变量）
//!
//! **诊断报告会被贴到公开 issue，绝不含明文密钥。**
//!
//! 脱敏走「单一真值」：UserConfig 与生成的 sing-box 配置都过同一个 [`redact_deep`]，避免某处漏掉。
//! 这是**安全功能，不是格式化功能** —— 任何「让报告更好看」的想法都不得以放宽打码为代价。
//!
//! # 策略
//!
//! 键名黑名单（命中即整值打码）+ url 仅留 origin + custom 协议 `secretKeys` 叠加；**未命中键原样保留**
//! （诊断需看形态）。
//!
//! 注意：**无值层启发式**（不按 base64 / 熵猜密钥）。custom 协议（raw-JSON 透传）的自定义密钥键若既不在
//! 黑名单、用户又未在表单声明 `secretKeys`，则不会被打码 —— 故 custom 节点务必声明 `secretKeys`
//! （snell psk 等常见键已黑名单兜底）。
//!
//! # 两层脱敏的分工
//!
//! 1. [`redact_deep`]：**键名**层 —— 打码「密钥键」的值（password / uuid / privateKey…）。只管结构化 JSON。
//! 2. [`redact_identifiers`]：**值**层 —— 把节点身份（域名 / IP / SNI / 节点名）在**全报告文本**（配置块
//!    + 日志 tail）统一替换为稳定占位。日志 tail 是原文，第 1 层管不到，非它不可。

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use polaris_config_engine::user_config::ip::is_ipv4;

/// 打码占位符（定长，不泄露原值长度信息）。上游 `REDACTED`。
pub const REDACTED: &str = "<redacted>";

/// 密钥键名黑名单（**已归一**：小写、去 `_`/`-`，故 camelCase 与 snake_case 同时命中：
/// `privateKey` / `private_key` 都归一为 `privatekey`）。命中即整值打码。
///
/// 仅收「凭据 / 密钥」类。**刻意排除**可公开的结构字段：reality `public_key`（公钥本就公开）、`short_id`、
/// `server_name` / `sni`、`method`（SS 加密算法名非密钥）、`fingerprint`、`alpn` —— 这些保留以判形态。
/// `username` 保留（naive 用户名单独不可用，且有助定位），仅 password 类打码。
///
/// **改这张表 = 改红线**。新增协议若引入新密钥键，必须同步加进来 + 补 [`tests`] 里的穷举用例。
pub const SECRET_KEYS: [&str; 15] = [
    "password",
    "uuid",
    "privatekey",
    "privatekeypassphrase",
    "presharedkey",
    "authkey",
    "secret",
    "clashapisecret",
    "token",
    "pluginopts", // ss plugin_opts 常含 host;password
    "pluginoptions",
    "privacypassword",
    "privacypasswordhash", // 隐私密码 salted hash（诊断报告贴公开 issue，hash 可离线爆破 → 打码）
    "psk",                 // snell 等第三方协议主密钥（无 customSettings.secretKeys 时的兜底）
    "userkey",             // snell 多用户服务器鉴权 key
];

/// url 类键名（值按 url 处理：仅保留 origin，path/query 都打码 —— 订阅 token 可能在 path 或 query）。
pub const URL_KEYS: [&str; 1] = ["url"];

/// 归一键名：小写 + 去 `_`/`-`，使 `privateKey` / `private_key` / `private-key` 等价比较。
/// 导出供调用方构造叠加密钥集。上游 `normalizeKey`。
#[must_use]
pub fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_secret_key(nk: &str, extra: &BTreeSet<String>) -> bool {
    SECRET_KEYS.contains(&nk) || extra.contains(nk)
}

/// url 脱敏：仅保留 origin（scheme + host[:port]），path / query / fragment / userinfo 一律丢弃或打码。
/// 上游 `redactUrlValue`。
///
/// 机场订阅 token 既可能在 query(`?token=`) 也可能嵌在 path 段（如 `/abcTOKEN/clash`）→ **宁过勿漏**（红线）；
/// origin 已足够判「订阅源主机是否可达」。
///
/// # 与 TS `new URL()` 的实现差异（均在**更严**方向，已实测锁在 [`tests`]）
///
/// - **userinfo**：`https://user:tok@h/p` → TS 的 `.origin` 天然丢 userinfo；本实现显式取 `@` 之后的
///   authority，等价。
/// - **默认端口**：TS `.origin` 会归一掉 `:443`/`:80`（`https://h:443/` → `https://h`）；本实现保留
///   `:443`。端口非密钥，差异纯属外观。
/// - **fragment**：TS 对 `https://h#frag` 判 `hasPathOrQuery=false` → 回 origin；本实现回
///   `origin/<redacted>`（**更严**，fragment 也可能藏 token）。
/// - **非特殊 scheme**：TS `new URL("vless://uu@h")`.origin = `"null"`（不透明源）；本实现回
///   `vless://h/<redacted>`（丢 uuid、留主机形态）。二者都不泄漏凭据；本实现保留的主机形态另由
///   [`redact_identifiers`] 兜底打码。
/// - **无 scheme 时**（TS 里 `new URL` 抛 → catch 分支）：截断到 `?` 前，与 TS 逐字一致。
#[must_use]
pub fn redact_url_value(raw: &str) -> String {
    let Some(sep) = raw.find("://") else {
        // TS catch 分支：非法 url 退化为截断到 ? 前。
        return match raw.find('?') {
            Some(q) => format!("{}?{REDACTED}", &raw[..q]),
            None => raw.to_string(),
        };
    };
    let scheme = &raw[..sep];
    let after = &raw[sep + 3..];
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let authority = &after[..end];
    // userinfo（user:pass@host）是凭据 → 只留最后一个 '@' 之后的 host[:port]。
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let origin = format!("{scheme}://{host}");
    let rest = &after[end..];
    if rest.is_empty() || rest == "/" {
        origin
    } else {
        format!("{origin}/{REDACTED}")
    }
}

/// 递归脱敏任意 JSON 值。上游 `redactDeep`。不就地修改，返回新副本。
///
/// `extra_secret_keys`：额外打码的「**已归一**」键名（custom 协议 `secretKeys` 叠加用）。
#[must_use]
pub fn redact_deep(value: &Value, extra_secret_keys: &BTreeSet<String>) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| redact_deep(v, extra_secret_keys))
                .collect(),
        ),
        Value::Object(src) => {
            let mut out = Map::new();

            // custom 协议：outbound 内按该节点声明的 secretKeys 额外打码（归一化后并入黑名单传给子层）。
            let mut child_extra = extra_secret_keys.clone();
            if let Some(keys) = src
                .get("customSettings")
                .and_then(|cs| cs.get("secretKeys"))
                .and_then(Value::as_array)
            {
                for k in keys {
                    if let Some(s) = k.as_str() {
                        child_extra.insert(normalize_key(s));
                    }
                }
            }

            for (k, v) in src {
                let nk = normalize_key(k);
                if v.is_null() {
                    out.insert(k.clone(), Value::Null);
                } else if is_secret_key(&nk, extra_secret_keys) {
                    // 命中密钥：标量打码；**对象 / 数组整体打码**（不向下递归，杜绝嵌套泄漏）。
                    out.insert(k.clone(), Value::String(REDACTED.to_string()));
                } else if URL_KEYS.contains(&nk.as_str()) && v.is_string() {
                    out.insert(
                        k.clone(),
                        Value::String(redact_url_value(v.as_str().unwrap_or_default())),
                    );
                } else if v.is_object() || v.is_array() {
                    out.insert(k.clone(), redact_deep(v, &child_extra));
                } else {
                    out.insert(k.clone(), v.clone());
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// 汇总所有 custom 节点声明的 `secretKeys`（**已归一**），供生成配置段的 `extra_secret_keys` 用。
///
/// **为什么必须单独汇总**：custom 协议在生成 sing-box config 时已把 `customSettings.outbound` 展平进
/// outbound 顶层、剥离 `customSettings` 包装 → [`redact_deep`] 在**生成配置**里就地读不到 `secretKeys`。
/// 不预先汇总，第三方协议的自定义密钥键会在生成配置段裸奔（红线：零明文密钥）。
#[must_use]
pub fn collect_custom_secret_keys(config: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(servers) = config.get("servers").and_then(Value::as_array) else {
        return out;
    };
    for s in servers {
        if let Some(keys) = s
            .get("customSettings")
            .and_then(|cs| cs.get("secretKeys"))
            .and_then(Value::as_array)
        {
            for k in keys {
                if let Some(v) = k.as_str() {
                    out.insert(normalize_key(v));
                }
            }
        }
    }
    out
}

/// 节点标识符 → 稳定占位符。上游 `NodeIdentifier`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentifier {
    /// 原值（节点域名 / IP / SNI / 节点名）。
    pub value: String,
    /// 占位符（`<domain-N>` / `<ip-N>` / `<node-N>`）。
    pub placeholder: String,
}

/// 主机类键名（**已归一**：小写去 `_`）：custom 透传 outbound 里这些键的字符串值是节点身份。
const HOST_KEYS: [&str; 5] = ["server", "servername", "sni", "host", "hostname"];

/// IPv4 严格判定（复用 config-engine 单一真值）；IPv6 仅作脱敏**形态粗判**（含 ':' 即归 IP），无需精确
/// —— 判错只是占位符标签选成 `<domain-N>` 而非 `<ip-N>`，**不构成泄漏**。
fn looks_like_ip(s: &str) -> bool {
    is_ipv4(s) || s.contains(':')
}

/// 递归收集对象里主机类键的字符串值。
///
/// custom 协议（raw-JSON 透传）的 outbound 原样下发到生成 config，身份字段可能嵌套
/// （如 `tls.server_name` 伪装 SNI、`transport.headers.Host`），只扫顶层会漏 → **全深度遍历**。
fn collect_hosts_deep(obj: &Value, out: &mut Vec<String>) {
    match obj {
        Value::Array(items) => {
            for x in items {
                collect_hosts_deep(x, out);
            }
        }
        Value::Object(m) => {
            for (k, v) in m {
                if let Some(s) = v.as_str() {
                    if HOST_KEYS.contains(&normalize_key(k).as_str()) {
                        out.push(s.to_string());
                    }
                } else if v.is_object() || v.is_array() {
                    collect_hosts_deep(v, out);
                }
            }
        }
        _ => {}
    }
}

/// 收集 transport headers 里的 Host 值（节点伪装域名）。大小写不敏感匹配 `host` 键；值兼容
/// string（`WebSocketSettings.headers`）与 string[]（`HttpSettings.headers`）。ws / http 共用。
fn add_host_headers(headers: Option<&Value>, out: &mut Vec<String>) {
    let Some(Value::Object(m)) = headers else {
        return;
    };
    for (k, v) in m {
        if !k.eq_ignore_ascii_case("host") {
            continue;
        }
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(items) => {
                for x in items {
                    if let Some(s) = x.as_str() {
                        out.push(s.to_string());
                    }
                }
            }
            _ => {}
        }
    }
}

/// 从 `config.servers` 收集本用户节点标识符 + 稳定占位符。上游 `collectNodeIdentifiers`。
///
/// 涵盖一切会进生成 config / 日志的节点身份字段：地址 / SNI / WS-Host / ShadowTLS-sni /
/// Tailscale-hostname·exitNode / HTTP-host[]·headers.Host / custom outbound 的 server·sni·host。
///
/// 地址类：域名 → `<domain-N>`、IP → `<ip-N>`（保留「域名 vs IP」诊断信号）；节点名 → `<node-N>`。
/// 去重（同值一占位，大小写不敏感）。
///
/// **节点名 < 4 字符跳过**（防误伤日志普通词，如节点名叫 "hk" 会把日志里所有 "hk" 替掉）；地址类不设长度
/// 阈值（靠 [`redact_identifiers`] 的主机边界锚定防误替）。
///
/// `extra_addresses`：#57 resolve-ahead —— 节点域名被预解析成 IP 写进生成 config 的 `outbound.server`，
/// 这些 IP 不在 `config.servers` 里、否则会以明文漏进诊断报告 → 调用方传入，一并按节点 IP 身份打码。
#[must_use]
pub fn collect_node_identifiers(config: &Value, extra_addresses: &[String]) -> Vec<NodeIdentifier> {
    let mut out: Vec<NodeIdentifier> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let (mut domain_n, mut ip_n, mut name_n) = (0u32, 0u32, 0u32);

    let mut add = |raw: &str, is_name: bool| {
        let v = raw.trim();
        if v.is_empty() {
            return;
        }
        // 长度阈值按 **UTF-16 码元**计（`encode_utf16().count()`），逐字对齐 TS `v.length < 4`。
        // 不用 `chars().count()`：机场节点名常带国旗 emoji（如 "🇭🇰🇯🇵"），JS 里每个旗帜 = 4 码元 → length=8 保留，
        // 而 scalar 计数只有 4 → 若阈值判据不同，短 emoji 名会被**漏打码**（错在不安全方向）。
        if is_name && v.encode_utf16().count() < 4 {
            return;
        }
        let key = v.to_lowercase();
        if !seen.insert(key) {
            return;
        }
        let placeholder = if is_name {
            name_n += 1;
            format!("<node-{name_n}>")
        } else if looks_like_ip(v) {
            ip_n += 1;
            format!("<ip-{ip_n}>")
        } else {
            domain_n += 1;
            format!("<domain-{domain_n}>")
        };
        out.push(NodeIdentifier {
            value: v.to_string(),
            placeholder,
        });
    };

    let str_at = |s: &Value, path: &[&str]| -> Option<String> {
        let mut cur = s;
        for p in path {
            cur = cur.get(p)?;
        }
        cur.as_str().map(str::to_owned)
    };

    if let Some(servers) = config.get("servers").and_then(Value::as_array) {
        for s in servers {
            if let Some(v) = str_at(s, &["address"]) {
                add(&v, false);
            }
            if let Some(v) = str_at(s, &["tlsSettings", "serverName"]) {
                add(&v, false);
            }
            // ws/http transport 的 Host 头（伪装域名）：仅匹配精确 `host` 键（**刻意不走 collect_hosts_deep
            // 的 HOST_KEYS 全集**）——transport headers 只有 Host 头承载身份，用全集会把恰好叫 server/sni
            // 的自定义 HTTP 头误收为节点身份；collect_hosts_deep 仅用于 custom raw-JSON outbound（键名不可控、
            // 需广撒网）。
            let mut hosts: Vec<String> = Vec::new();
            add_host_headers(
                s.get("wsSettings").and_then(|w| w.get("headers")),
                &mut hosts,
            );
            add_host_headers(
                s.get("httpSettings").and_then(|h| h.get("headers")),
                &mut hosts,
            );
            for h in &hosts {
                add(h, false);
            }
            if let Some(v) = str_at(s, &["shadowTlsSettings", "sni"]) {
                add(&v, false);
            }
            if let Some(v) = str_at(s, &["tailscaleSettings", "hostname"]) {
                add(&v, false);
            }
            if let Some(v) = str_at(s, &["tailscaleSettings", "exitNode"]) {
                add(&v, false);
            }
            if let Some(arr) = s
                .get("httpSettings")
                .and_then(|h| h.get("host"))
                .and_then(Value::as_array)
            {
                for h in arr {
                    if let Some(v) = h.as_str() {
                        add(v, false);
                    }
                }
            }
            // custom 透传 outbound 原样下发 → 递归收主机类键（含嵌套 tls.server_name / transport.headers.Host）
            if let Some(o) = s.get("customSettings").and_then(|c| c.get("outbound")) {
                let mut deep: Vec<String> = Vec::new();
                collect_hosts_deep(o, &mut deep);
                for h in &deep {
                    add(h, false);
                }
            }
            if let Some(v) = str_at(s, &["name"]) {
                add(&v, true);
            }
        }
    }
    // resolve-ahead 预解析得到的节点 IP（在 config.servers 之外）：按 IP 身份打码。
    for ip in extra_addresses {
        add(ip, false);
    }
    out
}

/// 主机边界字符集：节点标识符前后若是这些字符，说明它只是更长主机名的一段 → 不替。
/// 对齐 TS 正则 `(?<![\w.-])` / `(?![\w.-])`（`\w` = ASCII 字母数字 + `_`）。
///
/// **Rust `regex` crate 不支持 lookbehind/lookahead**，故手写边界判定 —— 顺带免掉正则编译与 ReDoS 面。
fn is_host_boundary_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
}

/// 在任意文本（日志 tail / 序列化后的配置）里把节点标识符替换为占位符。上游 `redactIdentifiers`。
///
/// - **长值优先**（防短值先替坏长值）
/// - **大小写不敏感**（日志域名常小写）
/// - **主机边界锚定**：防节点标识符作为子串误替无关串（节点 `a.com` 不碰 `cdn.a.com`；节点 IP
///   `104.18.8.8` 不把 `104.18.8.83` 切成 `<ip-1>3`）
/// - 占位符为 `<...>` 不含原值，不会自我再匹配
#[must_use]
pub fn redact_identifiers(text: &str, ids: &[NodeIdentifier]) -> String {
    if text.is_empty() || ids.is_empty() {
        return text.to_string();
    }
    let mut sorted: Vec<&NodeIdentifier> = ids.iter().collect();
    sorted.sort_by_key(|id| std::cmp::Reverse(id.value.len()));

    let mut out = text.to_string();
    for id in sorted {
        out = replace_bounded_ci(&out, &id.value, &id.placeholder);
    }
    out
}

/// 大小写不敏感 + 主机边界锚定的全量替换。
fn replace_bounded_ci(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let hay_lower = haystack.to_lowercase();
    let need_lower = needle.to_lowercase();
    // to_lowercase 可能改变字节长度（如 'İ'）→ 会让 lower 上的下标对不回原串。
    // 主机名/IP 是 ASCII，退化到「不替」比错切安全（占位符没打上顶多多留一个已在别处覆盖的主机名，
    // 而错切会破坏报告结构）。
    if hay_lower.len() != haystack.len() || need_lower.len() != needle.len() {
        return haystack.to_string();
    }

    let bytes = haystack.as_bytes();
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0usize;
    while let Some(rel) = hay_lower[cursor..].find(&need_lower) {
        let start = cursor + rel;
        let end = start + needle.len();
        // 前边界：start 前一个字符不得是主机字符
        let prev_ok = start == 0
            || !haystack[..start]
                .chars()
                .next_back()
                .is_some_and(is_host_boundary_char);
        // 后边界：end 处字符不得是主机字符
        let next_ok = end >= bytes.len()
            || !haystack[end..]
                .chars()
                .next()
                .is_some_and(is_host_boundary_char);
        if prev_ok && next_ok {
            out.push_str(&haystack[cursor..start]);
            out.push_str(replacement);
            cursor = end;
        } else {
            // 不满足边界 → 原样保留到 start+1，从 start+1 继续找（避免死循环）
            let step = haystack[start..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&haystack[cursor..start + step]);
            cursor = start + step;
        }
        if cursor >= haystack.len() {
            break;
        }
    }
    out.push_str(&haystack[cursor.min(haystack.len())..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_extra() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn redact(v: &Value) -> Value {
        redact_deep(v, &no_extra())
    }

    /// 断言：序列化后的 JSON 里不含任何一个明文密钥串。
    fn assert_no_plaintext(v: &Value, secrets: &[&str]) {
        let s = serde_json::to_string(v).unwrap();
        for secret in secrets {
            assert!(
                !s.contains(secret),
                "明文泄漏：{secret:?} 出现在脱敏输出里\n{s}"
            );
        }
    }

    // ════════════════════════════════════════════════════════════════════
    // 红线穷举：各类密钥 / 凭据逐个证明不泄漏
    //
    // 每条对应一个真实协议表面（字段名取自 config-engine::user_config::protocol_settings
    // 与 ui/src/shared/types/protocol-settings.ts）。新增协议密钥键 → 必须在此加一条。
    // ════════════════════════════════════════════════════════════════════

    #[test]
    fn 红线_节点密码_trojan_hysteria2_ss_shadowtls() {
        let v = json!({ "servers": [
            { "protocol": "trojan", "password": "TROJAN_PW" },
            { "protocol": "hysteria2", "password": "HY2_PW" },
            { "protocol": "shadowsocks", "password": "SS_PW", "method": "aes-256-gcm" },
            { "protocol": "vless", "shadowTlsSettings": { "password": "STLS_PW", "sni": "a.com" } },
        ]});
        let out = redact(&v);
        assert_no_plaintext(&out, &["TROJAN_PW", "HY2_PW", "SS_PW", "STLS_PW"]);
        assert_eq!(out["servers"][0]["password"], json!(REDACTED));
        assert_eq!(
            out["servers"][2]["method"],
            json!("aes-256-gcm"),
            "算法名非密钥，保留判形态"
        );
        assert_eq!(
            out["servers"][3]["shadowTlsSettings"]["sni"],
            json!("a.com"),
            "sni 保留判形态"
        );
    }

    #[test]
    fn 红线_uuid_vless_vmess_tuic() {
        let v = json!({ "servers": [
            { "protocol": "vless", "uuid": "11111111-2222-3333-4444-555555555555" },
            { "protocol": "vmess", "uuid": "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE" },
            { "protocol": "tuic", "uuid": "TUIC-UUID-XYZ", "password": "TUIC_PW" },
        ]});
        let out = redact(&v);
        assert_no_plaintext(
            &out,
            &[
                "11111111-2222-3333-4444-555555555555",
                "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE",
                "TUIC-UUID-XYZ",
                "TUIC_PW",
            ],
        );
    }

    #[test]
    fn 红线_wireguard_私钥与预共享密钥() {
        let v = json!({ "servers": [{
            "protocol": "wireguard",
            "wireguardSettings": {
                "privateKey": "WG_PRIVATE_KEY_B64",
                "preSharedKey": "WG_PSK_B64",
                "peerPublicKey": "PEER_PUBLIC_KEY_B64",
                "reserved": [1, 2, 3],
            },
        }]});
        let out = redact(&v);
        assert_no_plaintext(&out, &["WG_PRIVATE_KEY_B64", "WG_PSK_B64"]);
        assert_eq!(
            out["servers"][0]["wireguardSettings"]["peerPublicKey"],
            json!("PEER_PUBLIC_KEY_B64"),
            "对端公钥本就公开，保留判形态（刻意不在黑名单）"
        );
    }

    #[test]
    fn 红线_warp_设备凭据_token() {
        // WARP 注册产出的自删凭据随节点落 wireguardSettings.warpDevice；token 与 privateKey 同信任类。
        // 注：WARP `license` 只存在于注册请求/响应与 draft meta（mesh/src/warp.rs），从不写入 UserConfig
        //     → 不在诊断报告的暴露面内；真正落盘的 WARP 凭据是这里的 token。
        let v = json!({ "servers": [{
            "protocol": "wireguard",
            "wireguardSettings": {
                "privateKey": "WARP_PRIV",
                "warpDevice": { "deviceId": "dev-123", "token": "WARP_DEVICE_TOKEN" },
            },
        }]});
        let out = redact(&v);
        assert_no_plaintext(&out, &["WARP_DEVICE_TOKEN", "WARP_PRIV"]);
        assert_eq!(
            out["servers"][0]["wireguardSettings"]["warpDevice"]["deviceId"],
            json!("dev-123"),
            "deviceId 非密钥（无 token 不可用），保留供定位"
        );
    }

    #[test]
    fn 红线_tailscale_authkey() {
        let v = json!({ "servers": [{
            "protocol": "tailscale",
            "tailscaleSettings": {
                "authKey": "tskey-auth-SECRETVALUE",
                "hostname": "my-host",
                "controlUrl": "https://controlplane.tailscale.com",
            },
        }]});
        let out = redact(&v);
        assert_no_plaintext(&out, &["tskey-auth-SECRETVALUE"]);
        assert_eq!(
            out["servers"][0]["tailscaleSettings"]["hostname"],
            json!("my-host"),
            "hostname 是节点身份、由 redact_identifiers 第二层管，此层不动"
        );
    }

    #[test]
    fn 红线_ssh_私钥与密码短语() {
        let v = json!({ "servers": [{
            "protocol": "ssh",
            "sshSettings": {
                "password": "SSH_PW",
                "privateKey": "-----BEGIN OPENSSH PRIVATE KEY-----\nSSH_PRIV_BODY\n-----END-----",
                "privateKeyPassphrase": "SSH_PASSPHRASE",
                "privateKeyPath": "/home/u/.ssh/id_rsa",
                "username": "root",
            },
        }]});
        let out = redact(&v);
        assert_no_plaintext(&out, &["SSH_PW", "SSH_PRIV_BODY", "SSH_PASSPHRASE"]);
        assert_eq!(
            out["servers"][0]["sshSettings"]["privateKeyPath"],
            json!("/home/u/.ssh/id_rsa"),
            "路径非密钥本体（privatekeypath 不在黑名单），保留判形态"
        );
        assert_eq!(
            out["servers"][0]["sshSettings"]["username"],
            json!("root"),
            "username 刻意保留（单独不可用 + 有助定位）"
        );
    }

    #[test]
    fn 红线_snell_psk_与_userkey() {
        let v = json!({ "servers": [{
            "protocol": "snell",
            "snellSettings": { "psk": "SNELL_PSK", "userkey": "SNELL_USERKEY" },
        }]});
        assert_no_plaintext(&redact(&v), &["SNELL_PSK", "SNELL_USERKEY"]);
    }

    #[test]
    fn 红线_shadowsocks_plugin_opts() {
        // plugin_opts 常含 "host=x;password=y" 整串
        let v = json!({ "servers": [{
            "protocol": "shadowsocks",
            "plugin": "obfs-local",
            "plugin_opts": "obfs=http;obfs-host=x.com;password=PLUGIN_PW",
            "pluginOptions": "password=PLUGIN_PW2",
        }]});
        let out = redact(&v);
        assert_no_plaintext(&out, &["PLUGIN_PW", "PLUGIN_PW2"]);
        assert_eq!(
            out["servers"][0]["plugin"],
            json!("obfs-local"),
            "插件名非密钥"
        );
    }

    #[test]
    fn 红线_clash_api_secret_与隐私密码() {
        let v = json!({
            "clashApiSecret": "CLASH_SECRET",
            "privacyPassword": "PRIVACY_PW",
            "privacyPasswordHash": "SALT$PRIVACY_HASH",
            "secret": "BARE_SECRET"
        });
        // salted hash 可离线爆破 → 与明文同级打码（诊断报告贴公开 issue）。
        assert_no_plaintext(
            &redact(&v),
            &["CLASH_SECRET", "PRIVACY_PW", "PRIVACY_HASH", "BARE_SECRET"],
        );
    }

    #[test]
    fn 红线_订阅_url_含_token_query() {
        let v = json!({ "subscriptions": [{ "id": "s1", "url": "https://air.example.com/sub?token=SUBTOKEN123&flag=clash" }]});
        let out = redact(&v);
        assert_no_plaintext(&out, &["SUBTOKEN123"]);
        assert_eq!(
            out["subscriptions"][0]["url"],
            json!("https://air.example.com/<redacted>")
        );
    }

    #[test]
    fn 红线_订阅_url_token_嵌在_path_段() {
        // 机场常把 token 直接嵌进 path（/abcTOKEN/clash）→ 只剥 query 会漏（宁过勿漏）
        let v = json!({ "subscriptions": [{ "url": "https://air.example.com/PATHTOKEN9/clash" }]});
        let out = redact(&v);
        assert_no_plaintext(&out, &["PATHTOKEN9"]);
        assert_eq!(
            out["subscriptions"][0]["url"],
            json!("https://air.example.com/<redacted>")
        );
    }

    #[test]
    fn 红线_url_userinfo_凭据被剥离() {
        let v = json!({ "ruleResources": [{ "url": "https://user:USERINFO_PW@res.example.com/a.srs" }]});
        let out = redact(&v);
        assert_no_plaintext(&out, &["USERINFO_PW"]);
        assert_eq!(
            out["ruleResources"][0]["url"],
            json!("https://res.example.com/<redacted>")
        );
    }

    #[test]
    fn 红线_custom_协议声明的_secret_keys_叠加() {
        let v = json!({ "servers": [{
            "protocol": "custom",
            "customSettings": {
                "secretKeys": ["myCustomToken", "weird_key_name"],
                "outbound": {
                    "type": "snell",
                    "server": "s.example.com",
                    "myCustomToken": "CUSTOM_SECRET_1",
                    "weird_key_name": "CUSTOM_SECRET_2",
                    "nested": { "myCustomToken": "CUSTOM_SECRET_3" },
                },
            },
        }]});
        let out = redact(&v);
        assert_no_plaintext(
            &out,
            &["CUSTOM_SECRET_1", "CUSTOM_SECRET_2", "CUSTOM_SECRET_3"],
        );
        assert_eq!(
            out["servers"][0]["customSettings"]["outbound"]["server"],
            json!("s.example.com"),
            "非密钥键保留判形态"
        );
    }

    #[test]
    fn 红线_custom_secret_keys_归一化匹配() {
        // 声明 "my_custom_token" 应命中 outbound 里的 "myCustomToken"（归一后同为 mycustomtoken）
        let v = json!({ "servers": [{
            "customSettings": {
                "secretKeys": ["my_custom_token"],
                "outbound": { "myCustomToken": "NORMALIZED_SECRET" },
            },
        }]});
        assert_no_plaintext(&redact(&v), &["NORMALIZED_SECRET"]);
    }

    #[test]
    fn 红线_生成配置段靠汇总的_secret_keys_兜底() {
        // 生成 config 已把 customSettings.outbound 展平、剥离包装 → 就地读不到 secretKeys。
        // 必须由 collect_custom_secret_keys 预先汇总后传入，否则第三方密钥在生成配置段裸奔。
        let user_config = json!({ "servers": [{
            "customSettings": { "secretKeys": ["myCustomToken"], "outbound": { "myCustomToken": "X" } },
        }]});
        let extra = collect_custom_secret_keys(&user_config);
        assert!(extra.contains("mycustomtoken"));

        let generated = json!({ "outbounds": [{ "type": "snell", "tag": "n1", "myCustomToken": "FLATTENED_SECRET" }]});
        assert_no_plaintext(&redact_deep(&generated, &extra), &["FLATTENED_SECRET"]);
        // 反证：不传 extra 就会漏 —— 这正是 collect_custom_secret_keys 存在的理由
        let leaked = serde_json::to_string(&redact(&generated)).unwrap();
        assert!(
            leaked.contains("FLATTENED_SECRET"),
            "不传 extra 必漏（锁住这条依赖关系）"
        );
    }

    #[test]
    fn 红线_密钥键下的对象与数组整体打码不递归() {
        // 嵌套泄漏防线：命中密钥键后不向下递归，整个子树打码
        let v = json!({
            "token": { "inner": "NESTED_IN_TOKEN", "deep": { "x": "DEEPER" } },
            "password": ["PW_IN_ARRAY_1", "PW_IN_ARRAY_2"],
        });
        let out = redact(&v);
        assert_no_plaintext(
            &out,
            &[
                "NESTED_IN_TOKEN",
                "DEEPER",
                "PW_IN_ARRAY_1",
                "PW_IN_ARRAY_2",
            ],
        );
        assert_eq!(out["token"], json!(REDACTED));
        assert_eq!(out["password"], json!(REDACTED));
    }

    #[test]
    fn 红线_snake_case_与_camel_case_同时命中() {
        let v = json!({
            "private_key": "SNAKE_PRIV", "privateKey": "CAMEL_PRIV", "private-key": "KEBAB_PRIV",
            "pre_shared_key": "SNAKE_PSK", "preSharedKey": "CAMEL_PSK",
            "auth_key": "SNAKE_AUTH", "authKey": "CAMEL_AUTH",
            "PASSWORD": "UPPER_PW", "UUID": "UPPER_UUID",
        });
        assert_no_plaintext(
            &redact(&v),
            &[
                "SNAKE_PRIV",
                "CAMEL_PRIV",
                "KEBAB_PRIV",
                "SNAKE_PSK",
                "CAMEL_PSK",
                "SNAKE_AUTH",
                "CAMEL_AUTH",
                "UPPER_PW",
                "UPPER_UUID",
            ],
        );
    }

    #[test]
    fn 红线_密钥藏在深层嵌套与数组里也打码() {
        let v = json!({ "a": { "b": [{ "c": { "d": [{ "password": "DEEP_PW", "uuid": "DEEP_UUID" }] } }] } });
        assert_no_plaintext(&redact(&v), &["DEEP_PW", "DEEP_UUID"]);
    }

    #[test]
    fn 红线_全量_secret_keys_逐个覆盖() {
        // 穷举锁：SECRET_KEYS 每一项都必须真打码。新增键忘了实现 → 这条转红。
        for (i, k) in SECRET_KEYS.iter().enumerate() {
            let marker = format!("SECRET_MARKER_{i}");
            let v = json!({ *k: marker.clone() });
            let out = redact(&v);
            assert_eq!(out[*k], json!(REDACTED), "SECRET_KEYS 项 {k:?} 未被打码");
            assert_no_plaintext(&out, &[marker.as_str()]);
        }
    }

    // ── 刻意保留项（打码过度会毁掉诊断价值 → 同样是不变式）──

    #[test]
    fn 刻意保留_可公开的结构字段() {
        let v = json!({ "servers": [{
            "address": "node.example.com", "port": 443, "protocol": "vless",
            "tlsSettings": {
                "serverName": "sni.example.com", "fingerprint": "chrome",
                "alpn": ["h2"], "realitySettings": { "publicKey": "REALITY_PUB", "shortId": "abcd" },
            },
        }]});
        let out = redact(&v);
        assert_eq!(out["servers"][0]["address"], json!("node.example.com"));
        assert_eq!(out["servers"][0]["port"], json!(443));
        assert_eq!(
            out["servers"][0]["tlsSettings"]["serverName"],
            json!("sni.example.com")
        );
        assert_eq!(
            out["servers"][0]["tlsSettings"]["fingerprint"],
            json!("chrome")
        );
        assert_eq!(
            out["servers"][0]["tlsSettings"]["realitySettings"]["publicKey"],
            json!("REALITY_PUB"),
            "reality 公钥本就公开"
        );
        assert_eq!(
            out["servers"][0]["tlsSettings"]["realitySettings"]["shortId"],
            json!("abcd")
        );
    }

    #[test]
    fn null_值原样保留() {
        let v = json!({ "password": null, "uuid": null, "other": null });
        let out = redact(&v);
        assert_eq!(
            out["password"],
            Value::Null,
            "null 不打码（TS: v == null → 原样）"
        );
        assert_eq!(out["uuid"], Value::Null);
    }

    #[test]
    fn redact_deep_不改原值() {
        let v = json!({ "password": "PW" });
        let _ = redact(&v);
        assert_eq!(v["password"], json!("PW"), "输入不被就地修改");
    }

    // ── normalize_key ──

    #[test]
    fn normalize_key_基本形() {
        assert_eq!(normalize_key("privateKey"), "privatekey");
        assert_eq!(normalize_key("private_key"), "privatekey");
        assert_eq!(normalize_key("private-key"), "privatekey");
        assert_eq!(normalize_key("PRIVATE_KEY"), "privatekey");
        assert_eq!(normalize_key(""), "");
    }

    #[test]
    fn secret_keys_表本身已归一() {
        // 自锁：黑名单里若混进未归一的键（如 "privateKey"），它永远匹配不上 → 静默失效
        for k in SECRET_KEYS {
            assert_eq!(
                normalize_key(k),
                k,
                "SECRET_KEYS 项 {k:?} 未归一化，将永不命中"
            );
        }
        for k in URL_KEYS {
            assert_eq!(normalize_key(k), k);
        }
        for k in HOST_KEYS {
            assert_eq!(normalize_key(k), k);
        }
    }

    // ── redact_url_value ──

    #[test]
    fn url_无_path_query_只回_origin() {
        assert_eq!(
            redact_url_value("https://a.example.com"),
            "https://a.example.com"
        );
        assert_eq!(
            redact_url_value("https://a.example.com/"),
            "https://a.example.com"
        );
    }

    #[test]
    fn url_有_path_或_query_则打码() {
        assert_eq!(
            redact_url_value("https://a.example.com/x"),
            "https://a.example.com/<redacted>"
        );
        assert_eq!(
            redact_url_value("https://a.example.com?t=1"),
            "https://a.example.com/<redacted>"
        );
        assert_eq!(
            redact_url_value("https://a.example.com/#f"),
            "https://a.example.com/<redacted>"
        );
    }

    #[test]
    fn url_保留端口() {
        assert_eq!(
            redact_url_value("http://1.2.3.4:8080/sub?t=X"),
            "http://1.2.3.4:8080/<redacted>"
        );
    }

    #[test]
    fn url_非法退化为截断到问号前() {
        // TS catch 分支逐字对齐
        assert_eq!(
            redact_url_value("not a url?token=T"),
            "not a url?<redacted>"
        );
        assert_eq!(redact_url_value("plain-text"), "plain-text");
    }

    #[test]
    fn url_非特殊_scheme_剥掉凭据() {
        // vless://UUID@host:443?sni=... —— UUID 在 userinfo 位
        let out = redact_url_value(
            "vless://11111111-2222-3333-4444-555555555555@n.example.com:443?sni=a",
        );
        assert!(!out.contains("11111111"), "分享链接 uuid 不得泄漏：{out}");
        assert_eq!(out, "vless://n.example.com:443/<redacted>");
    }

    // ── collect_node_identifiers ──

    #[test]
    fn 收集_地址_域名与_ip_分别占位() {
        let c = json!({ "servers": [
            { "address": "d1.example.com", "name": "东京节点甲" },
            { "address": "104.18.8.8", "name": "大阪节点乙" },
        ]});
        let ids = collect_node_identifiers(&c, &[]);
        let find = |v: &str| {
            ids.iter()
                .find(|i| i.value == v)
                .map(|i| i.placeholder.clone())
        };
        assert_eq!(find("d1.example.com").as_deref(), Some("<domain-1>"));
        assert_eq!(find("104.18.8.8").as_deref(), Some("<ip-1>"));
        assert_eq!(find("东京节点甲").as_deref(), Some("<node-1>"));
        assert_eq!(find("大阪节点乙").as_deref(), Some("<node-2>"));
    }

    #[test]
    fn 收集_ipv6_归_ip() {
        let c = json!({ "servers": [{ "address": "2606:4700::1111" }]});
        assert_eq!(collect_node_identifiers(&c, &[])[0].placeholder, "<ip-1>");
    }

    #[test]
    fn 收集_覆盖全部身份字段() {
        let c = json!({ "servers": [{
            "address": "addr.example.com",
            "name": "MyNodeName",
            "tlsSettings": { "serverName": "sni.example.com" },
            "wsSettings": { "headers": { "Host": "ws-host.example.com" } },
            "httpSettings": { "host": ["http-host.example.com"], "headers": { "host": ["hdr-host.example.com"] } },
            "shadowTlsSettings": { "sni": "stls.example.com" },
            "tailscaleSettings": { "hostname": "ts-host", "exitNode": "exit.node.ts.net" },
            "customSettings": { "outbound": {
                "server": "custom.example.com",
                "tls": { "server_name": "nested-sni.example.com" },
                "transport": { "headers": { "Host": "nested-host.example.com" } },
            }},
        }]});
        let ids = collect_node_identifiers(&c, &[]);
        let vals: Vec<&str> = ids.iter().map(|i| i.value.as_str()).collect();
        for want in [
            "addr.example.com",
            "MyNodeName",
            "sni.example.com",
            "ws-host.example.com",
            "http-host.example.com",
            "hdr-host.example.com",
            "stls.example.com",
            "ts-host",
            "exit.node.ts.net",
            "custom.example.com",
            "nested-sni.example.com",
            "nested-host.example.com",
        ] {
            assert!(
                vals.contains(&want),
                "身份字段 {want:?} 未被收集 → 会在报告里裸奔"
            );
        }
    }

    #[test]
    fn 收集_短节点名跳过防误伤日志() {
        let c = json!({ "servers": [{ "name": "hk" }, { "name": "日本节点" }]});
        let ids = collect_node_identifiers(&c, &[]);
        assert!(
            !ids.iter().any(|i| i.value == "hk"),
            "<4 码元节点名跳过（否则日志里所有 hk 被替）"
        );
        assert!(ids.iter().any(|i| i.value == "日本节点"), "4 码元保留");
    }

    #[test]
    fn 收集_节点名长度按_utf16_码元对齐_js() {
        // 锁 JS `v.length` 语义（UTF-16 码元），非 Rust scalar 计数。
        // "🇭🇰" = 2 个 regional indicator = 4 码元 → JS length=4 ≥ 4 → **保留打码**；
        // 若误用 chars().count()（=2）会判 <4 而跳过 → 节点名漏进报告（不安全方向）。
        let c = json!({ "servers": [{ "name": "🇭🇰" }]});
        let ids = collect_node_identifiers(&c, &[]);
        assert!(
            ids.iter().any(|i| i.value == "🇭🇰"),
            "emoji 节点名（4 码元）必须被收集打码"
        );

        // "中" = 1 码元 → 跳过（两种计数在此一致）
        let c2 = json!({ "servers": [{ "name": "中" }]});
        assert!(collect_node_identifiers(&c2, &[]).is_empty());
    }

    #[test]
    fn 收集_去重同值一占位() {
        let c = json!({ "servers": [
            { "address": "same.example.com" },
            { "address": "SAME.example.com" },
        ]});
        let ids = collect_node_identifiers(&c, &[]);
        assert_eq!(ids.len(), 1, "大小写不敏感去重");
    }

    #[test]
    fn 收集_额外预解析_ip() {
        // #57 resolve-ahead：预解析 IP 不在 config.servers 里，不传就会明文漏进报告
        let c = json!({ "servers": [{ "address": "d.example.com" }]});
        let ids = collect_node_identifiers(&c, &["203.0.113.9".to_string()]);
        assert_eq!(
            ids.iter()
                .find(|i| i.value == "203.0.113.9")
                .unwrap()
                .placeholder,
            "<ip-1>"
        );
    }

    #[test]
    fn 收集_transport_headers_只认_host_键() {
        // 恰好叫 server/sni 的自定义 HTTP 头不该被误收为节点身份
        let c = json!({ "servers": [{ "wsSettings": { "headers": { "X-Server": "not-identity.com", "Host": "real.example.com" } } }]});
        let ids = collect_node_identifiers(&c, &[]);
        assert!(ids.iter().any(|i| i.value == "real.example.com"));
        assert!(!ids.iter().any(|i| i.value == "not-identity.com"));
    }

    #[test]
    fn 收集_空配置不panic() {
        assert!(collect_node_identifiers(&json!({}), &[]).is_empty());
        assert!(collect_node_identifiers(&json!({ "servers": "oops" }), &[]).is_empty());
    }

    // ── redact_identifiers（值层）──

    #[test]
    fn 替换_基本与大小写不敏感() {
        let ids = vec![NodeIdentifier {
            value: "A.Example.COM".into(),
            placeholder: "<domain-1>".into(),
        }];
        assert_eq!(
            redact_identifiers("lookup a.example.com SERVFAIL; dial A.EXAMPLE.COM ok", &ids),
            "lookup <domain-1> SERVFAIL; dial <domain-1> ok"
        );
    }

    #[test]
    fn 替换_主机边界锚定_不碰子串() {
        let ids = vec![NodeIdentifier {
            value: "a.com".into(),
            placeholder: "<domain-1>".into(),
        }];
        // cdn.a.com 的 a.com 前面是 '.' → 不替；a.com.evil 后面是 '.' → 不替
        assert_eq!(redact_identifiers("cdn.a.com", &ids), "cdn.a.com");
        assert_eq!(redact_identifiers("a.com.evil", &ids), "a.com.evil");
        assert_eq!(
            redact_identifiers("dial a.com now", &ids),
            "dial <domain-1> now"
        );
    }

    #[test]
    fn 替换_ip_不被切成占位符加数字() {
        let ids = vec![NodeIdentifier {
            value: "104.18.8.8".into(),
            placeholder: "<ip-1>".into(),
        }];
        assert_eq!(
            redact_identifiers("peer 104.18.8.83 up", &ids),
            "peer 104.18.8.83 up",
            "不得把 104.18.8.83 切成 <ip-1>3"
        );
        assert_eq!(
            redact_identifiers("peer 104.18.8.8 up", &ids),
            "peer <ip-1> up"
        );
    }

    #[test]
    fn 替换_长值优先() {
        let ids = vec![
            NodeIdentifier {
                value: "a.com".into(),
                placeholder: "<domain-2>".into(),
            },
            NodeIdentifier {
                value: "sub.a.com".into(),
                placeholder: "<domain-1>".into(),
            },
        ];
        assert_eq!(
            redact_identifiers("host sub.a.com end", &ids),
            "host <domain-1> end",
            "长值先替，短值不该先把它咬碎"
        );
    }

    #[test]
    fn 替换_长值优先_节点名前缀不得漏尾() {
        // 长值优先的**真实防线在节点名**，不在域名：域名/IP 是主机形态，短值撞长值时已被边界锚定挡住
        // （`a.com` 在 `sub.a.com` 里前邻 '.' → 不替）。但节点名是任意串，短名可以是长名的前缀且**边界
        // 恰好合法**（后邻空格非主机字符）→ 边界锚定救不了，只有长值优先能救。
        //
        // 反例（短值优先时）：「东京节点」先替 → "<node-1> IPLC 专线"，尾巴「 IPLC 专线」是节点名残留（泄漏），
        // 且占位符编号张冠李戴（写 node-1 实为 node-2 那个节点）。
        let ids = vec![
            NodeIdentifier {
                value: "东京节点".into(),
                placeholder: "<node-1>".into(),
            },
            NodeIdentifier {
                value: "东京节点 IPLC 专线".into(),
                placeholder: "<node-2>".into(),
            },
        ];
        let out = redact_identifiers("log: 东京节点 IPLC 专线 down", &ids);
        assert_eq!(out, "log: <node-2> down");
        assert!(!out.contains("IPLC"), "节点名尾段不得残留：{out}");
    }

    #[test]
    fn 替换_端口与冒号后仍替() {
        let ids = vec![NodeIdentifier {
            value: "n.example.com".into(),
            placeholder: "<domain-1>".into(),
        }];
        assert_eq!(
            redact_identifiers("connect n.example.com:443", &ids),
            "connect <domain-1>:443"
        );
    }

    #[test]
    fn 替换_空输入与空标识符() {
        assert_eq!(redact_identifiers("", &[]), "");
        assert_eq!(redact_identifiers("text", &[]), "text");
        let ids = vec![NodeIdentifier {
            value: String::new(),
            placeholder: "<x>".into(),
        }];
        assert_eq!(
            redact_identifiers("text", &ids),
            "text",
            "空值不得死循环/全替"
        );
    }

    #[test]
    fn 替换_占位符不自我再匹配() {
        let ids = vec![
            NodeIdentifier {
                value: "a.com".into(),
                placeholder: "<domain-1>".into(),
            },
            NodeIdentifier {
                value: "domain".into(),
                placeholder: "<node-1>".into(),
            },
        ];
        // "domain" 被 <domain-1> 包着，边界是 '<' 和 '-' —— '-' 是主机字符 → 后边界不满足 → 不替
        let out = redact_identifiers("a.com", &ids);
        assert_eq!(out, "<domain-1>");
    }

    #[test]
    fn 替换_跨段占位一致可关联() {
        // 报告价值所在：日志里的 <domain-1> 能对上配置块里的 <domain-1>
        let ids = vec![NodeIdentifier {
            value: "n.example.com".into(),
            placeholder: "<domain-1>".into(),
        }];
        let text = "config: \"server\": \"n.example.com\"\nlog: lookup n.example.com SERVFAIL";
        let out = redact_identifiers(text, &ids);
        assert_eq!(out.matches("<domain-1>").count(), 2);
        assert!(!out.contains("n.example.com"));
    }

    // ── 端到端：两层合起来 ──

    #[test]
    fn 红线_端到端_配置加日志无明文密钥与节点身份() {
        let config = json!({
            "servers": [{
                "id": "s1", "name": "东京节点甲", "address": "tokyo.air.example.com", "port": 443,
                "protocol": "vless", "uuid": "SECRET-UUID-0001",
                "tlsSettings": { "serverName": "cdn.example.com" },
            }],
            "subscriptions": [{ "url": "https://air.example.com/sub?token=SUBTOK" }],
            "clashApiSecret": "CLASHSEC",
        });
        let redacted_cfg = redact_deep(&config, &collect_custom_secret_keys(&config));
        let ids = collect_node_identifiers(&config, &[]);
        let log = "lookup tokyo.air.example.com SERVFAIL\nTLS handshake to cdn.example.com failed\n东京节点甲 down";
        let report = format!(
            "{}\n{}",
            serde_json::to_string_pretty(&redacted_cfg).unwrap(),
            log
        );
        let out = redact_identifiers(&report, &ids);

        for leak in [
            "SECRET-UUID-0001",
            "SUBTOK",
            "CLASHSEC",
            "tokyo.air.example.com",
            "cdn.example.com",
            "东京节点甲",
        ] {
            assert!(!out.contains(leak), "端到端泄漏 {leak:?}：\n{out}");
        }
        assert!(
            out.contains("<domain-1>") && out.contains("<node-1>"),
            "占位符应出现"
        );
    }
}
