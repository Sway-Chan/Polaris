//! FakeIP 例外域名（走真实解析、绕过 FakeIP）单一真值。
//! 上游 `shared/fakeip-filter.ts` 常量部分（FS/seed 不属纯逻辑层，不移植）。
//!
//! 这些域名用假 IP 会坏：连通性探测/Captive Portal（误判断网、锁屏登录卡死）、NTP 校时（拿不到真实 IP）。
//! dns-builder 生成 DNS 规则时消费这些清单。

#![forbid(unsafe_code)]

/// 连通性探测 / Captive Portal 域名：解析须走「内网解析器」反映真实本地网络（exact domain 匹配）。
/// 上游 `FAKEIP_FILTER_CAPTIVE_DOMAINS`。
pub const FAKEIP_FILTER_CAPTIVE_DOMAINS: &[&str] = &[
    "captive.apple.com",
    "connectivitycheck.gstatic.com",
    "connectivitycheck.android.com",
    "msftconnecttest.com",
    "www.msftconnecttest.com",
    "msftncsi.com",
    "www.msftncsi.com",
    "dns.msftncsi.com",
    "detectportal.firefox.com",
    "network-test.debian.org",
    "connect.rom.miui.com",
];

/// NTP 校时域名：走真实 DNS（domain_suffix 匹配 pool.ntp.org 等区域子域）。
/// 上游 `FAKEIP_FILTER_NTP_SUFFIXES`。
pub const FAKEIP_FILTER_NTP_SUFFIXES: &[&str] = &[
    "ntp.org",
    "time.windows.com",
    "time.apple.com",
    "time.cloudflare.com",
    "time.nist.gov",
    "time.android.com",
];

/// NTP/STUN 关键字（裸子串匹配；始终生效的兜底，非用户可编辑域名清单项）。
/// 误伤面极小的 ntp/stun，刻意不含 turn。上游 `FAKEIP_FILTER_NTP_STUN_KEYWORDS`。
pub const FAKEIP_FILTER_NTP_STUN_KEYWORDS: &[&str] = &["ntp", "stun"];

/// 设置页可编辑清单的默认 seed / 恢复默认源：captive + ntp 域名（关键字另由 dns-builder 始终兜底）。
/// 上游 `DEFAULT_FAKEIP_FILTER_DOMAINS`。
pub fn default_fakeip_filter_domains() -> Vec<String> {
    FAKEIP_FILTER_CAPTIVE_DOMAINS
        .iter()
        .chain(FAKEIP_FILTER_NTP_SUFFIXES.iter())
        .map(|s| (*s).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captive_includes_apple_android_msft() {
        let all = FAKEIP_FILTER_CAPTIVE_DOMAINS.join(" ");
        assert!(all.contains("captive.apple.com"));
        assert!(all.contains("connectivitycheck.gstatic.com"));
        assert!(all.contains("msftconnecttest.com"));
    }

    #[test]
    fn ntp_suffixes_include_pool_org() {
        assert!(FAKEIP_FILTER_NTP_SUFFIXES.contains(&"ntp.org"));
        assert!(FAKEIP_FILTER_NTP_SUFFIXES.contains(&"time.apple.com"));
    }

    #[test]
    fn keywords_are_ntp_stun_only() {
        assert_eq!(FAKEIP_FILTER_NTP_STUN_KEYWORDS, &["ntp", "stun"]);
    }

    #[test]
    fn default_filter_is_captive_plus_ntp() {
        let d = default_fakeip_filter_domains();
        assert!(d.contains(&"captive.apple.com".to_string()));
        assert!(d.contains(&"ntp.org".to_string()));
        assert_eq!(
            d.len(),
            FAKEIP_FILTER_CAPTIVE_DOMAINS.len() + FAKEIP_FILTER_NTP_SUFFIXES.len()
        );
    }
}
