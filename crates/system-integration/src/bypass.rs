//! Windows ProxyOverride 串构造（上游 `shared/system-proxy-bypass.ts` `formatBypassForWindows` 1:1 移植）。
//!
//! mac / linux 的 `format_bypass_for_mac` / `format_bypass_for_linux` 已在
//! [`polaris_config_engine::user_config::system_proxy_bypass`] 落地（B1）；Windows 串构造当时标 H2 后补，
//! 即本模块。CIDR 判定 / Windows 通配展开复用 config-engine 单一真值，不重复实现。

#![forbid(unsafe_code)]

use polaris_config_engine::user_config::system_proxy_bypass::{
    ipv4_cidr_to_windows_patterns, is_ipv4_cidr, is_ipv6_cidr,
};

/// Windows ProxyOverride 合法字符白名单：域名 / CIDR / 通配 / `<local>` / IPv6 冒号 / 括号。
/// 含其它字符的 bypass 项整项跳过（fail-safe：其流量走代理，告警可见，不静默篡改主机名）。
/// 上游 `UNSAFE = /[^a-zA-Z0-9.\-*:/<>()]/`。
fn is_windows_bypass_token_safe(token: &str) -> bool {
    token.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'.' | b'-' | b'*' | b':' | b'/' | b'<' | b'>' | b'(' | b')'
            )
    })
}

/// Windows ProxyOverride 串（分号分隔）：IPv4 CIDR→通配、v6 CIDR 跳过、域名/通配/精确项原样、补 `<local>`。
///
/// `on_unsafe`：bypass 项含 cmd 非法字符时整项跳过并回调（与 Polaris 同口径：不静默改写主机名，告警可见）。
///
/// 上游 `formatBypassForWindows`。
pub fn format_bypass_for_windows(
    list: &[String],
    mut on_unsafe: Option<&mut dyn FnMut(&str)>,
) -> String {
    let mut out: Vec<String> = Vec::new();
    for raw in list {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        if !is_windows_bypass_token_safe(t) {
            if let Some(cb) = on_unsafe.as_deref_mut() {
                cb(t);
            }
            continue;
        }
        if is_ipv4_cidr(t) {
            out.extend(ipv4_cidr_to_windows_patterns(t));
        } else if is_ipv6_cidr(t) {
            // IPv6 CIDR：Windows 代理例外跳过。
            continue;
        } else {
            out.push(t.to_string());
        }
    }
    if !out.iter().any(|s| s == "<local>") {
        out.push("<local>".to_string());
    }
    // 去重保序（Polaris dedupe）。
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    for s in out {
        if seen.insert(s.clone()) {
            deduped.push(s);
        }
    }
    deduped.join(";")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_cidr_to_wildcard_and_local_default() {
        let s = format_bypass_for_windows(
            &[
                "10.0.0.0/8".into(),
                "192.168.0.0/16".into(),
                "localhost".into(),
            ],
            None,
        );
        assert_eq!(s, "10.*;192.168.*;localhost;<local>");
    }

    #[test]
    fn skips_ipv6_cidr() {
        let s = format_bypass_for_windows(&["fc00::/7".into(), "localhost".into()], None);
        assert_eq!(s, "localhost;<local>");
    }

    #[test]
    fn preserves_local_if_present() {
        let s = format_bypass_for_windows(&["localhost".into(), "<local>".into()], None);
        assert_eq!(s, "localhost;<local>");
        // 不重复追加
        assert_eq!(s.matches("<local>").count(), 1);
    }

    #[test]
    fn unsafe_token_skipped_and_reported() {
        let mut reported = Vec::new();
        let s = format_bypass_for_windows(
            &["intra;net".into(), "10.0.0.0/8".into()],
            Some(&mut |e: &str| reported.push(e.to_string())),
        );
        assert_eq!(s, "10.*;<local>");
        assert_eq!(reported, vec!["intra;net".to_string()]);
    }

    #[test]
    fn wildcard_domain_preserved() {
        let s = format_bypass_for_windows(&["*.local".into()], None);
        assert_eq!(s, "*.local;<local>");
    }

    #[test]
    fn dedup_preserves_order() {
        let s = format_bypass_for_windows(
            &["10.0.0.0/8".into(), "10.0.0.0/8".into(), "localhost".into()],
            None,
        );
        assert_eq!(s, "10.*;localhost;<local>");
    }
}
