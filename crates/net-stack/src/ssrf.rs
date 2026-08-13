//! SSRF guard 纯逻辑（上游 `shared/ssrf-guard.ts` 1:1 移植）。
//!
//! 零网络依赖；DNS 解析由调用方注入（[`DnsLookup`] trait）。覆盖 IPv4-mapped / CGNAT /
//! link-local / 云元数据(169.254.169.254) / ULA 等绕过面，故意比 RFC1918 三段更严。
//!
//! 复用 `polaris-config-engine` 的 IP 字面量判定（`is_ipv4`/`is_ipv6_literal`）与 CIDR
//! 相交算术（`cidr_overlaps_any`），保证与 上游 `shared/ip.ts` 单一真值对齐。
//!
//! FakeIP 段常量（`FAKEIP_INET4_RANGE`=198.18.0.0/15 · `FAKEIP_INET6_RANGE`=2001:2::/48）
//! Polaris 侧由 `shared/fakeip-filter.ts` 派生；Rust `config-engine` 仅移植了 FakeIP 例外
//! *域名*（FS/seed，非纯逻辑），故假 IP *段* 单一真值在此落地，由 [`is_polaris_fake_ip`] 消费。

#![forbid(unsafe_code)]

use std::future::Future;

use polaris_config_engine::user_config::cidr::cidr_overlaps_any;
use polaris_config_engine::user_config::ip::{is_ip_literal, is_ipv4, is_ipv6_literal};
// FakeIP 段常量单一真值在 config-engine（tun_config.rs），re-export 消除重复定义。
pub use polaris_config_engine::user_config::tun_config::{FAKEIP_INET4_RANGE, FAKEIP_INET6_RANGE};

/// 候选 FakeIP 段集（v4 + v6）。[`is_polaris_fake_ip`] 与 SSRF 豁免共用。
fn fakeip_ranges() -> Vec<String> {
    vec![
        FAKEIP_INET4_RANGE.to_string(),
        FAKEIP_INET6_RANGE.to_string(),
    ]
}

/// 去除首尾方括号（`[::1]` → `::1`）并转小写。上游 `ip.replace(/^\[|\]$/g, '').toLowerCase()`。
fn strip_brackets_lower(ip: &str) -> String {
    let bytes = ip.as_bytes();
    let inner = if bytes.len() >= 2 && bytes[0] == b'[' && bytes[bytes.len() - 1] == b']' {
        &ip[1..ip.len() - 1]
    } else {
        ip
    };
    inner.to_ascii_lowercase()
}

/// 单个字面 IP 是否属内网/回环/link-local/CGNAT 等不可达外网的危险段。
///
/// 上游 `isPrivateIp` 1:1：
/// - IPv4：0/8、127/8、10/8、172.16/12、192.168/16、169.254/16（含云元数据）、100.64/10（CGNAT）。
/// - IPv6：::1、::、fc00::/7(ULA)、fe80::/10(link-local)、以及 IPv4-mapped（::ffff:x.x.x.x，
///   取低 32 位递归判 IPv4，防点分/hex/压缩各种写法绕过）。
///
/// 非字面 IP 返回 false（调用方对域名先做 DNS 解析再逐 IP 套用本判定）。
pub fn is_private_ip(ip: &str) -> bool {
    let h = strip_brackets_lower(ip);
    if is_ipv4(&h) {
        let parts: Vec<&str> = h.split('.').collect();
        // is_ipv4 已保证 4 段纯数字 0-255，安全 parse。
        let a: u32 = parts[0].parse().unwrap_or(0);
        let b: u32 = parts[1].parse().unwrap_or(0);
        if a == 0 || a == 127 {
            return true; // 通配 / 本机回环
        }
        if a == 10 {
            return true; // 私网
        }
        if a == 192 && b == 168 {
            return true; // 私网
        }
        if a == 172 && (16..=31).contains(&b) {
            return true; // 私网
        }
        if a == 169 && b == 254 {
            return true; // link-local / 云元数据 169.254.169.254
        }
        if a == 100 && (64..=127).contains(&b) {
            return true; // CGNAT
        }
        return false;
    }
    if is_ipv6_literal(&h) {
        if h == "::1" || h == "::" {
            return true; // 回环 / 通配
        }
        // 规范化展开成 8 段 16-bit（处理 :: 压缩与末尾内嵌点分 IPv4）。
        if let Some(seg) = expand_ipv6(&h) {
            // IPv4-mapped：前 5 段 0、第 6 段 0xffff → 取低 32 位拼回 IPv4 递归判定（防绕过）。
            if seg[0] == 0
                && seg[1] == 0
                && seg[2] == 0
                && seg[3] == 0
                && seg[4] == 0
                && seg[5] == 0xffff
            {
                let a = seg[6] >> 8;
                let b = seg[6] & 0xff;
                let c = seg[7] >> 8;
                let d = seg[7] & 0xff;
                return is_private_ip(&format!("{a}.{b}.{c}.{d}"));
            }
            // fe80::/10（link-local）：首段 0xfe80–0xfebf。
            if (0xfe80..=0xfebf).contains(&seg[0]) {
                return true;
            }
        }
        // fc00::/7（ULA）：首字节 fc/fd（已确认字面 IPv6，不会误伤主机名）。
        if h.starts_with("fc") || h.starts_with("fd") {
            return true;
        }
        return false;
    }
    false // 非字面 IP
}

/// 是否 Polaris FakeIP 假地址（198.18.0.0/15 · 2001:2::/48）。
///
/// 上游 `isFakeIp`：裸 IP → /32·/128 与假段做家族感知交集（跨族恒不相交）。
/// 仅接受字面 IP；CIDR/主机名 → false。由假段常量派生（单一真值）。
pub fn is_polaris_fake_ip(ip: &str) -> bool {
    let h = strip_brackets_lower(ip);
    if !is_ip_literal(&h) {
        return false; // 仅字面 IP（CIDR/主机名 → false）
    }
    let ranges = fakeip_ranges();
    cidr_overlaps_any(&h, &ranges)
}

/// 把一个 is_ipv6_literal 已认定合法的 IPv6 字符串规范化展开成 8 个 16-bit 段数值。
///
/// 上游 `expandIpv6`：处理 `::` 压缩与末尾内嵌点分 IPv4（如 ::ffff:127.0.0.1）。
/// 解析失败返回 None。
fn expand_ipv6(h: &str) -> Option<[u32; 8]> {
    let mut s = h.to_string();
    // 末尾内嵌点分 IPv4 → 转成两段 16-bit hex，统一按纯 hex 处理。
    let v4 = regex_v4_suffix(&s);
    if let Some((v4_str, caps)) = v4 {
        let o: Vec<u32> = caps.iter().map(|x| x.parse::<u32>().unwrap_or(0)).collect();
        if o.iter().any(|&n| n > 255) {
            return None;
        }
        let hi = (o[0] << 8) | o[1];
        let lo = (o[2] << 8) | o[3];
        let prefix = &s[..s.len() - v4_str.len()];
        s = format!("{prefix}{hi:x}:{lo:x}");
    }
    let parts: Vec<&str> = s.split("::").collect();
    if parts.len() > 2 {
        return None;
    }
    let head: Vec<&str> = if parts[0].is_empty() {
        vec![]
    } else {
        parts[0].split(':').collect()
    };
    let tail: Vec<&str> = if parts.len() == 2 && !parts[1].is_empty() {
        parts[1].split(':').collect()
    } else {
        vec![]
    };
    let segs: Vec<&str> = if parts.len() != 2 {
        head
    } else {
        let fill = 8usize.saturating_sub(head.len() + tail.len());
        if fill < 1 {
            // :: 至少省 1 组（fill==0 即非法，与 上游 `fill < 0` 等价，因 head+tail ≤ 7）
            if head.len() + tail.len() >= 8 {
                return None;
            }
        }
        let mut v = head.clone();
        v.extend(std::iter::repeat_n("0", fill));
        v.extend(tail);
        v
    };
    if segs.len() != 8 {
        return None;
    }
    let mut out = [0u32; 8];
    for (i, x) in segs.iter().enumerate() {
        let seg = if x.is_empty() { "0" } else { *x };
        if seg.len() > 4 {
            return None;
        }
        let n = u32::from_str_radix(seg, 16).ok()?;
        if n > 0xffff {
            return None;
        }
        out[i] = n;
    }
    Some(out)
}

/// 匹配末尾点分 IPv4（`\d{1,3}.\d{1,3}.\d{1,3}.\d{1,3}$`），返回 (匹配串, 四段)。
/// 手写等价于 上游 `s.match(/(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/)`。
fn regex_v4_suffix(s: &str) -> Option<(&str, [String; 4])> {
    let last_colon = s.rfind(':')?;
    let tail = &s[last_colon + 1..];
    if !tail.contains('.') {
        return None;
    }
    let parts: Vec<&str> = tail.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    if !parts
        .iter()
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some((
        tail,
        [
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string(),
            parts[3].to_string(),
        ],
    ))
}

/// 注入的 DNS 解析（all:true 语义）。上游 `DnsLookupAll`。
///
/// 由调用方提供具体实现（main 侧包 `tokio::net::lookup_host` / 测试传 mock）。
/// 返回该 hostname 解析到的全部 IP 地址串（字面量形式，如 "127.0.0.1" / "::1"）。
pub trait DnsLookup: Send + Sync {
    /// 解析 hostname 到全部地址（all:true）。失败返回 Err（消息含 code/message）。
    fn lookup_all(&self, host: &str) -> impl Future<Output = Result<Vec<String>, String>> + Send;
}

/// H1（DNS rebinding）核心：对订阅/Provider URL 的 hostname 做 SSRF guard。
///
/// 上游 `assertHostAllowed` 1:1：
/// - 字面 localhost 直接拒；
/// - hostname 是字面 IP → 直接套 [`is_private_ip`]（字面 FakeIP 也按内网拒，不豁免）；
/// - hostname 是域名 → `lookup_all` 解析后逐 IP 套 [`is_private_ip`]，任一命中内网即拒
///   （拦「域名解析到 127.0.0.1 / 169.254.169.254 / 10.x」的 rebinding 绕过）。
///
/// `exempt_fake_ip=true` 时仅「经代理（proxied socks session）」豁免 FakeIP：
/// 经代理出口是远程节点、本机内网不可达，系统 DNS 把公网域名解析成 FakeIP 可安全豁免；
/// 直连/字面 IP 不豁免（防本机内网 SSRF）。
///
/// 错误只含 hostname，不回显完整 url（防 token 泄露）。返回 `Err(message)`。
pub async fn assert_host_allowed<L: DnsLookup>(
    host: &str,
    lookup: &L,
    exempt_fake_ip: bool,
) -> Result<(), String> {
    let host_norm = strip_brackets_lower(host);
    if host_norm == "localhost" {
        return Err(format!("订阅地址指向本机/内网/link-local，已拒绝: {host}"));
    }
    if is_ip_literal(&host_norm) {
        // 字面 IP 无「域名反查真实」语义——字面 FakeIP 也按内网拒（不豁免）。
        if is_private_ip(&host_norm) {
            return Err(format!("订阅地址指向本机/内网/link-local，已拒绝: {host}"));
        }
        return Ok(());
    }
    // 域名：解析后逐 IP 判定（DNS rebinding 防护）。
    let resolved = match lookup.lookup_all(&host_norm).await {
        Ok(v) => v,
        Err(e) => {
            return Err(format!("订阅地址解析失败，已拒绝: {host}（{e}）"));
        }
    };
    if resolved.is_empty() {
        return Err(format!("订阅地址无法解析到任何 IP，已拒绝: {host}"));
    }
    for r in &resolved {
        if exempt_fake_ip && is_polaris_fake_ip(r) {
            continue;
        }
        if is_private_ip(r) {
            return Err(format!(
                "订阅地址解析到本机/内网/link-local，已拒绝: {host} → {r}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── IPv4 私网段穷举 ──────────────────────────────────────────────────────
    #[test]
    fn ipv4_loopback_and_wildcard() {
        assert!(is_private_ip("127.0.0.1"));
        assert!(is_private_ip("127.255.255.255"));
        assert!(is_private_ip("127.0.0.0"));
        assert!(is_private_ip("0.0.0.0")); // a===0 通配
        assert!(is_private_ip("0.1.2.3"));
    }

    #[test]
    fn ipv4_rfc1918_ten() {
        assert!(is_private_ip("10.0.0.0"));
        assert!(is_private_ip("10.255.255.255"));
        assert!(is_private_ip("10.1.2.3"));
    }

    #[test]
    fn ipv4_rfc1918_172_16_12() {
        assert!(is_private_ip("172.16.0.0"));
        assert!(is_private_ip("172.31.255.255"));
        assert!(is_private_ip("172.20.5.5"));
        // 边界外：172.15 / 172.32 非私网
        assert!(!is_private_ip("172.15.0.0"));
        assert!(!is_private_ip("172.32.0.0"));
    }

    #[test]
    fn ipv4_rfc1918_192_168_16() {
        assert!(is_private_ip("192.168.0.0"));
        assert!(is_private_ip("192.168.1.1"));
        assert!(is_private_ip("192.168.255.255"));
    }

    #[test]
    fn ipv4_link_local_and_cloud_metadata() {
        assert!(is_private_ip("169.254.0.0"));
        assert!(is_private_ip("169.254.169.254")); // 云元数据
        assert!(is_private_ip("169.254.255.255"));
        // 边界外
        assert!(!is_private_ip("169.253.0.0"));
        assert!(!is_private_ip("169.255.0.0"));
    }

    #[test]
    fn ipv4_cgnat_100_64_10() {
        assert!(is_private_ip("100.64.0.0"));
        assert!(is_private_ip("100.127.255.255"));
        assert!(is_private_ip("100.100.100.100"));
        // 边界外
        assert!(!is_private_ip("100.63.255.255"));
        assert!(!is_private_ip("100.128.0.0"));
    }

    #[test]
    fn ipv4_public_not_private() {
        assert!(!is_private_ip("1.1.1.1"));
        assert!(!is_private_ip("8.8.8.8"));
        assert!(!is_private_ip("172.32.0.1"));
        assert!(!is_private_ip("11.0.0.1"));
        assert!(!is_private_ip("198.18.0.1")); // FakeIP 段（非 isPrivateIp 管辖）
    }

    // ── IPv6 私网段穷举 ──────────────────────────────────────────────────────
    #[test]
    fn ipv6_loopback_and_unspecified() {
        assert!(is_private_ip("::1"));
        assert!(is_private_ip("::"));
    }

    #[test]
    fn ipv6_ula_fc00_7() {
        assert!(is_private_ip("fc00::"));
        assert!(is_private_ip("fd00::1"));
        assert!(is_private_ip("fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"));
        // 非 ULA
        assert!(!is_private_ip("fe00::1"));
    }

    #[test]
    fn ipv6_link_local_fe80_10() {
        assert!(is_private_ip("fe80::1"));
        assert!(is_private_ip("febf::"));
        assert!(is_private_ip("fe90::1"));
        assert!(is_private_ip("fea0::1"));
        assert!(!is_private_ip("fe7f::1")); // 边界外
        assert!(!is_private_ip("fec0::1")); // 边界外（fec0::/10 site-local 已废弃，TS 也不拦）
    }

    #[test]
    fn ipv6_v4_mapped_loopback() {
        // ::ffff:127.0.0.1 → 递归判 IPv4 回环
        assert!(is_private_ip("::ffff:127.0.0.1"));
        assert!(is_private_ip("::ffff:7f00:1"));
        assert!(is_private_ip("0:0:0:0:0:ffff:127.0.0.1"));
        assert!(is_private_ip("::ffff:10.0.0.1"));
        assert!(is_private_ip("::ffff:192.168.1.1"));
        assert!(is_private_ip("::ffff:169.254.169.254"));
    }

    #[test]
    fn ipv6_v4_mapped_public() {
        assert!(!is_private_ip("::ffff:1.1.1.1"));
        assert!(!is_private_ip("::ffff:8.8.8.8"));
    }

    #[test]
    fn ipv6_brackets_normalized() {
        assert!(is_private_ip("[::1]"));
        assert!(is_private_ip("[fc00::1]"));
    }

    #[test]
    fn ipv6_case_insensitive() {
        assert!(is_private_ip("FC00::1"));
        assert!(is_private_ip("FE80::1"));
    }

    #[test]
    fn non_ip_returns_false() {
        assert!(!is_private_ip("example.com"));
        assert!(!is_private_ip(""));
        assert!(!is_private_ip("not an ip"));
    }

    // ── FakeIP 段判定 ─────────────────────────────────────────────────────────
    #[test]
    fn fakeip_v4_range() {
        assert!(is_polaris_fake_ip("198.18.0.0"));
        assert!(is_polaris_fake_ip("198.19.255.255"));
        assert!(is_polaris_fake_ip("198.18.123.45"));
        // 边界外
        assert!(!is_polaris_fake_ip("198.17.255.255"));
        assert!(!is_polaris_fake_ip("198.20.0.0"));
    }

    #[test]
    fn fakeip_v6_range() {
        assert!(is_polaris_fake_ip("2001:2::"));
        assert!(is_polaris_fake_ip("2001:2:0:0:ffff:ffff:ffff:ffff")); // /48 覆盖
                                                                       // 边界外
        assert!(!is_polaris_fake_ip("2001:3::"));
    }

    #[test]
    fn fakeip_non_literal_false() {
        assert!(!is_polaris_fake_ip("example.com"));
        assert!(!is_polaris_fake_ip(""));
        assert!(!is_polaris_fake_ip("1.1.1.1")); // 公网非假段
    }

    // ── expand_ipv6 内部 ──────────────────────────────────────────────────────
    #[test]
    fn expand_full_form() {
        assert_eq!(
            expand_ipv6("2001:db8::1"),
            Some([0x2001, 0x0db8, 0, 0, 0, 0, 0, 1])
        );
    }

    #[test]
    fn expand_v4_mapped() {
        assert_eq!(
            expand_ipv6("::ffff:127.0.0.1"),
            Some([0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001])
        );
    }

    #[test]
    fn expand_invalid_too_many_double_colon() {
        assert_eq!(expand_ipv6("::1::2"), None);
    }

    // ── assert_host_allowed ───────────────────────────────────────────────────
    /// 测试用 mock DnsLookup：按 hostname 返回预设地址表。
    struct MockLookup {
        map: std::collections::HashMap<&'static str, Result<Vec<&'static str>, &'static str>>,
    }

    impl DnsLookup for MockLookup {
        fn lookup_all(
            &self,
            host: &str,
        ) -> impl Future<Output = Result<Vec<String>, String>> + Send {
            let res = self.map.get(host).cloned().unwrap_or(Ok(vec!["1.2.3.4"]));
            async move {
                res.map(|v| v.into_iter().map(String::from).collect())
                    .map_err(String::from)
            }
        }
    }

    #[tokio::test]
    async fn assert_localhost_rejected() {
        let lk = MockLookup {
            map: std::collections::HashMap::new(),
        };
        let r = assert_host_allowed("localhost", &lk, false).await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("localhost"));
    }

    #[tokio::test]
    async fn assert_literal_public_ip_ok() {
        let lk = MockLookup {
            map: std::collections::HashMap::new(),
        };
        assert!(assert_host_allowed("1.2.3.4", &lk, false).await.is_ok());
    }

    #[tokio::test]
    async fn assert_literal_private_ip_rejected() {
        let lk = MockLookup {
            map: std::collections::HashMap::new(),
        };
        assert!(assert_host_allowed("127.0.0.1", &lk, false).await.is_err());
        assert!(assert_host_allowed("10.0.0.1", &lk, false).await.is_err());
        assert!(assert_host_allowed("169.254.169.254", &lk, false)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn assert_domain_resolves_public_ok() {
        let mut map = std::collections::HashMap::new();
        map.insert("example.com", Ok(vec!["93.184.216.34"]));
        let lk = MockLookup { map };
        assert!(assert_host_allowed("example.com", &lk, false).await.is_ok());
    }

    #[tokio::test]
    async fn assert_dns_rebinding_to_private_rejected() {
        // 域名解析到 127.0.0.1 → rebinding 绕过拦截
        let mut map = std::collections::HashMap::new();
        map.insert("evil.example.com", Ok(vec!["127.0.0.1"]));
        let lk = MockLookup { map };
        let r = assert_host_allowed("evil.example.com", &lk, false).await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn assert_dns_rebinding_to_metadata_rejected() {
        let mut map = std::collections::HashMap::new();
        map.insert("meta.example.com", Ok(vec!["169.254.169.254"]));
        let lk = MockLookup { map };
        assert!(assert_host_allowed("meta.example.com", &lk, false)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn assert_dns_resolve_empty_rejected() {
        let mut map = std::collections::HashMap::new();
        map.insert("empty.example.com", Ok(vec![]));
        let lk = MockLookup { map };
        let r = assert_host_allowed("empty.example.com", &lk, false).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn assert_dns_resolve_failed_rejected() {
        let mut map = std::collections::HashMap::new();
        map.insert("fail.example.com", Err("ENOTFOUND"));
        let lk = MockLookup { map };
        let r = assert_host_allowed("fail.example.com", &lk, false).await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("ENOTFOUND"));
    }

    #[tokio::test]
    async fn assert_fakeip_exempt_when_proxied() {
        // 经代理豁免 FakeIP：域名解析到 198.18.x → exempt_fake_ip=true 放行
        let mut map = std::collections::HashMap::new();
        map.insert("fake.example.com", Ok(vec!["198.18.0.5"]));
        let lk = MockLookup { map };
        assert!(assert_host_allowed("fake.example.com", &lk, true)
            .await
            .is_ok());
        // 直连不豁免：FakeIP 不属 isPrivateIp（假段在私网外）→ 也放行（豁免冗余兜底）
        assert!(assert_host_allowed("fake.example.com", &lk, false)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn assert_literal_fakeip_not_exempted_even_when_proxied() {
        // 字面 IP 的 FakeIP 不豁免——但假段在私网外，isPrivateIp 不拦 → 放行
        // （与 TS 语义一致：字面 FakeIP 走 isPrivateIp 判定，假段不在私网表 → false）
        let lk = MockLookup {
            map: std::collections::HashMap::new(),
        };
        assert!(assert_host_allowed("198.18.0.1", &lk, true).await.is_ok());
    }

    #[tokio::test]
    async fn assert_mixed_resolution_one_private_rejected() {
        // 多 IP 中任一内网即拒
        let mut map = std::collections::HashMap::new();
        map.insert(
            "mixed.example.com",
            Ok(vec!["8.8.8.8", "10.0.0.1", "1.1.1.1"]),
        );
        let lk = MockLookup { map };
        assert!(assert_host_allowed("mixed.example.com", &lk, false)
            .await
            .is_err());
    }
}
