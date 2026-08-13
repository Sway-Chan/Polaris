//! Cloudflare `cdn-cgi/trace` 解析 + IPv4/IPv6 合法性校验。
//!
//! 1:1 移植自 上游 `IpInfoService.parseTrace` + Node `net.isIP`（仅检测 unlock 用到的字段）。
//! - `parse_trace`：逐行 `key=value`，取 `ip`（经 `is_valid_ip` 校验）+ `loc`（=countryCode，2 字母大写、非 "XX"）。
//! - 失败返 `None`（对应 Polaris 返 `null`）—— 防 portal/截断响应的假数据污染 egress key。

/// 合法 IPv4/IPv6 字符串校验。对应 Node `net.isIP`（!==0 即合法）。
///
/// 只校验语法（非连通性）：用纯字符串解析。IPv4 = 4 个 0-255 八位组；IPv6 = 标准冒号十六进制。
/// 覆盖 Polaris 用到的全部形态（Cloudflare trace 的 ip= 字段）。
pub fn is_valid_ip(s: &str) -> bool {
    is_valid_ipv4(s) || is_valid_ipv6(s)
}

/// IPv4 dotted-quad 校验。
fn is_valid_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.len() <= 3
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u32>().is_ok_and(|n| n <= 255)
    })
}

/// IPv6 校验（标准含冒号形式，支持 `::` 省略；不支持 IPv4-mapped 末段 dotted-quad，因 trace 不产该形态）。
fn is_valid_ipv6(s: &str) -> bool {
    if !s.contains(':') || s.is_empty() {
        return false;
    }
    // 不含 `::` → 恰好 8 组。
    // 含 `::` → 至多 7 组（省略段补零），且 `::` 仅出现一次。
    let (groups, has_double_colon) = if let Some(idx) = s.find("::") {
        // 确保仅一次 `::`
        if s[idx + 2..].contains("::") {
            return false;
        }
        (s.replace("::", ":"), true)
    } else {
        (s.to_string(), false)
    };
    let parts: Vec<&str> = groups.split(':').collect();
    // replace 后首/尾可能产生空串（如 "::1" -> ":1" -> ["", "1"]；"1::" -> "1:" -> ["1",""]）。
    let non_empty: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    let max_groups = if has_double_colon { 7 } else { 8 };
    if non_empty.len() > max_groups {
        return false;
    }
    if !has_double_colon && non_empty.len() != 8 {
        return false;
    }
    non_empty
        .iter()
        .all(|p| p.len() <= 4 && !p.is_empty() && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// 解析后的 trace 信息（unlock 只用 ip + countryCode）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceInfo {
    pub ip: String,
    pub country_code: Option<String>,
}

/// 解析 Cloudflare `cdn-cgi/trace` body。对应 上游 `parseTrace`。
///
/// 行格式 `key=value`；取 `ip`（经 `is_valid_ip` 校验）+ `loc`（大写、恰好 2 字母、非 "XX"）。
/// 任一缺失/非法 → `None`（防 portal 假响应污染 egress 缓存 key）。
pub fn parse_trace(body: &str) -> Option<TraceInfo> {
    let mut kv: BTreeMap<&str, String> = BTreeMap::new();
    for line in body.split('\n') {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let i = match t.find('=') {
            Some(i) if i > 0 => i,
            _ => continue, // 无 '=' 或 key 为空 → 跳过
        };
        let key = &t[..i];
        let val = t[i + 1..].trim();
        kv.insert(key, val.to_string());
    }
    let ip = kv.get("ip")?;
    if !is_valid_ip(ip) {
        return None;
    }
    let country_code = kv
        .get("loc")
        .filter(|loc| {
            let up = loc.to_uppercase();
            up.len() == 2 && up != "XX"
        })
        .map(|loc| loc.to_uppercase());
    Some(TraceInfo {
        ip: ip.clone(),
        country_code,
    })
}

use std::collections::BTreeMap;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_trace() {
        let body = "fl=123f1\nh=cloudflare.com\nip=1.2.3.4\nloc=US\nuag=Mozilla\n";
        let info = parse_trace(body).unwrap();
        assert_eq!(info.ip, "1.2.3.4");
        assert_eq!(info.country_code.as_deref(), Some("US"));
    }

    #[test]
    fn parse_lowercase_loc_uppercased() {
        let body = "ip=10.0.0.1\nloc=jp\n";
        let info = parse_trace(body).unwrap();
        assert_eq!(info.country_code.as_deref(), Some("JP"));
    }

    #[test]
    fn parse_rejects_xx_loc() {
        // XX = Cloudflare 未知地区，对齐 Polaris：当作无 countryCode
        let body = "ip=1.1.1.1\nloc=XX\n";
        let info = parse_trace(body).unwrap();
        assert_eq!(info.country_code, None);
    }

    #[test]
    fn parse_rejects_invalid_ip() {
        // 劫持页/截断响应假响应 → ip 非法 → None
        let body = "ip=not-an-ip\nloc=US\n";
        assert!(parse_trace(body).is_none());
    }

    #[test]
    fn parse_missing_ip_returns_none() {
        let body = "loc=US\nuag=x\n";
        assert!(parse_trace(body).is_none());
    }

    #[test]
    fn parse_trims_line_whitespace() {
        // Cloudflare trace 实际格式：行内 `key=value` 无空格，但行首/尾可能有空白/CR。
        // 对齐 Polaris：trim 整行 + trim value；key 不去 inner 空格（trace 无此形态）。
        let body = "  ip=8.8.8.8  \r\n loc=DE \r\n";
        let info = parse_trace(body).unwrap();
        assert_eq!(info.ip, "8.8.8.8");
        assert_eq!(info.country_code.as_deref(), Some("DE"));
    }

    #[test]
    fn parse_three_letter_loc_kept_as_is() {
        // Polaris 原正则只放行恰好 2 字母；loc=USA 不应进 countryCode
        let body = "ip=1.1.1.1\nloc=USA\n";
        let info = parse_trace(body).unwrap();
        assert_eq!(info.country_code, None);
    }

    #[test]
    fn ipv4_validity() {
        assert!(is_valid_ipv4("1.2.3.4"));
        assert!(is_valid_ipv4("255.255.255.255"));
        assert!(is_valid_ipv4("0.0.0.0"));
        assert!(!is_valid_ipv4("256.1.1.1"));
        assert!(!is_valid_ipv4("1.2.3"));
        assert!(!is_valid_ipv4("1.2.3.4.5"));
        assert!(!is_valid_ipv4("a.b.c.d"));
        assert!(!is_valid_ipv4(""));
    }

    #[test]
    fn ipv6_validity() {
        assert!(is_valid_ipv6("::1"));
        assert!(is_valid_ipv6("2001:db8::1"));
        assert!(is_valid_ipv6("2001:db8:0:0:0:0:0:1"));
        assert!(is_valid_ipv6("fe80::1"));
        assert!(!is_valid_ipv6("2001:db8::1::2")); // 双 ::
        assert!(!is_valid_ipv6("1.2.3.4")); // 无冒号
        assert!(!is_valid_ipv6(""));
    }

    #[test]
    fn is_valid_ip_dispatch() {
        assert!(is_valid_ip("1.2.3.4"));
        assert!(is_valid_ip("::1"));
        assert!(!is_valid_ip("garbage"));
    }
}
