//! 系统代理 bypass 纯逻辑（上游 `shared/system-proxy-bypass.ts` 1:1 移植）。
//!
//! 仅作用于系统代理模式（OS proxy 例外列表）；TUN 模式直连由 sing-box route 规则负责。
//! bypassLanCidrs / effectiveBypassLan 先行（buildInbounds 依赖）；formatBypassForWindows/mac/Linux
//! 后续 H2（系统代理写入侧）补。

#![forbid(unsafe_code)]

use crate::user_config::collections::dedupe_trim;

/// 默认 bypass 清单（业内聚合：私网/保留段 + Apple 连通性 + 国内 App/网银）。
/// 上游 `DEFAULT_BYPASS_LAN`。
pub const DEFAULT_BYPASS_LAN: &[&str] = &[
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.88.99.0/24",
    "192.168.0.0/16",
    "224.0.0.0/4",
    "233.252.0.0/24",
    "240.0.0.0/4",
    "fc00::/7",
    "fe80::/10",
    "localhost",
    "*.local",
    "sequoia.apple.com",
    "seed-sequoia.siri.apple.com",
    "captive.apple.com",
    "e.crashlytics.com",
    "www.baidu.com",
    "passenger.t3go.cn",
    "yunbusiness.ccb.com",
    "wxh.wo.cn",
    "gate.lagou.com",
    "www.abchina.com.cn",
    "login-service.mobile-bank.psbc.com",
    "mobile-bank.psbc.com",
];

/// bypass 配置投影。
pub trait BypassConfig {
    fn bypass_lan(&self) -> Option<bool>;
    fn bypass_lan_list(&self) -> Option<&[String]>;
}

/// 「绕过局域网」生效清单：开关关→[]，开→用户清单/缺省 DEFAULT_BYPASS_LAN。
/// 上游 `effectiveBypassLan`。
pub fn effective_bypass_lan<C: BypassConfig>(config: &C) -> Vec<String> {
    if config.bypass_lan() == Some(false) {
        return vec![];
    }
    match config.bypass_lan_list() {
        Some(list) => list.to_vec(),
        None => DEFAULT_BYPASS_LAN.iter().map(|s| s.to_string()).collect(),
    }
}

/// **配置读取边界补齐 `bypassLANList`（F1 防默认坍塌）**。
///
/// # 为什么必须在边界注入
///
/// `bypassLANList` 缺省时，内核侧由 [`effective_bypass_lan`] 补 27 条 `DEFAULT_BYPASS_LAN`
/// （私网/CGNAT/组播/Apple 连通性/国内网银 …）。但 UI 的旁路 / route_exclude 编辑器直接绑
/// `config.bypassLANList`，缺省时只能退到前端硬编码兜底 —— **首个按键（ListEditor 逐字符 onChange）
/// 就把这份错误兜底当成用户清单持久化，静默丢弃 24 条真实默认**（win32 TUN route_exclude 丢
/// 10/8+172.16/12+CGNAT；route 直连规则 & 系统代理旁路丢网银/Apple 域名）。
///
/// 修法：在 `config:get` 唯一读取边界，把 UI 收到的 `bypassLANList` **补成其生效值**，使前端永远
/// 编辑真实清单、兜底成为死代码。语义与 [`effective_bypass_lan`] 严格对齐（由 `mirrors_effective_*`
/// 测试锁死），故对 builder 完全透明：注入后再交给 builder，`effective_bypass_lan` 拿到同一份清单，
/// 生成结果不变。
///
/// 幂等且尊重用户意图：字段**已是具体数组**（含用户清空后的 `[]`）→ 用户拥有，原样保留；仅
/// 缺省 / `null` 才注入。`bypassLAN == Some(false)`（用户显式关旁路）→ 注入 `[]`，避免 UI 展示
/// 一份看似生效实则被总开关否决的清单。
pub fn ensure_bypass_lan_list(cfg: &mut serde_json::Value) {
    let Some(obj) = cfg.as_object_mut() else {
        return;
    };
    // 已是具体数组（含用户清空的 []）→ 用户拥有，不覆盖。
    if matches!(obj.get("bypassLANList"), Some(serde_json::Value::Array(_))) {
        return;
    }
    // 缺省 / null → 注入生效默认（严格镜像 effective_bypass_lan 的 None 分支 + 总开关分支）。
    let effective: Vec<serde_json::Value> =
        if obj.get("bypassLAN").and_then(serde_json::Value::as_bool) == Some(false) {
            vec![]
        } else {
            DEFAULT_BYPASS_LAN
                .iter()
                .map(|s| serde_json::Value::from(*s))
                .collect()
        };
    obj.insert(
        "bypassLANList".to_string(),
        serde_json::Value::Array(effective),
    );
}

/// 是否 IPv4 CIDR 字面量（`\d{1,3}.\d{1,3}.\d{1,3}.\d{1,3}/\d{1,2}`）。
/// 上游 `isIpv4Cidr`。
pub fn is_ipv4_cidr(s: &str) -> bool {
    let t = s.trim();
    let Some((addr, prefix)) = t.split_once('/') else {
        return false;
    };
    if addr.is_empty() || prefix.is_empty() || prefix.len() > 2 {
        return false;
    }
    if !prefix.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let octets: Vec<&str> = addr.split('.').collect();
    octets.len() == 4 && octets.iter().all(|o| is_cidr_octet(o))
}

fn is_cidr_octet(o: &str) -> bool {
    !o.is_empty() && o.len() <= 3 && o.bytes().all(|b| b.is_ascii_digit())
}

/// 是否 IPv6 CIDR 字面量（粗判：hex+冒号地址 + /0-128 前缀）。上游 `isIpv6Cidr`。
pub fn is_ipv6_cidr(s: &str) -> bool {
    let t = s.trim();
    let Some((addr, prefix)) = t.split_once('/') else {
        return false;
    };
    if prefix.is_empty() || prefix.len() > 3 {
        return false;
    }
    if !prefix.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let prefix_n: u32 = prefix.parse().unwrap_or(999);
    if prefix_n > 128 {
        return false;
    }
    addr.contains(':')
        && !addr.is_empty()
        && addr.bytes().all(|b| b.is_ascii_hexdigit() || b == b':')
}

/// 是否 IP CIDR（v4 或 v6）。上游 `isIpCidr`。
pub fn is_ip_cidr(s: &str) -> bool {
    is_ipv4_cidr(s) || is_ipv6_cidr(s)
}

/// 从 bypass 清单筛 IP CIDR 条目（滤掉域名/通配/localhost）。
/// 上游 `bypassLanCidrs`。
pub fn bypass_lan_cidrs(list: &[String]) -> Vec<String> {
    list.iter()
        .map(|s| s.trim().to_string())
        .filter(|s| is_ip_cidr(s))
        .collect()
}

/// IPv4 CIDR → Windows ProxyOverride 通配（/8/16/24/12 枚举）。上游 `ipv4CidrToWindowsPatterns`。
pub fn ipv4_cidr_to_windows_patterns(cidr: &str) -> Vec<String> {
    let t = cidr.trim();
    let Some((addr, prefix)) = t.split_once('/') else {
        return vec![];
    };
    let octets: Vec<&str> = addr.split('.').collect();
    if octets.len() != 4 || !octets.iter().all(|o| is_cidr_octet(o)) {
        return vec![];
    }
    let o: Vec<u32> = octets
        .iter()
        .map(|s| s.parse::<u32>().unwrap_or(999))
        .collect();
    if o.iter().any(|&x| x > 255) {
        return vec![];
    }
    let prefix_n: u32 = prefix.parse().unwrap_or(999);
    match prefix_n {
        8 => vec![format!("{}.*", o[0])],
        16 => vec![format!("{}.{}.*", o[0], o[1])],
        24 => vec![format!("{}.{}.{}.*", o[0], o[1], o[2])],
        12 => {
            // /12 第二段对齐到 16 倍数，覆盖 base..base+15。
            let base = o[1] & 0xf0;
            (base..=base + 15)
                .take_while(|&i| i <= 255)
                .map(|i| format!("{}.{i}.*", o[0]))
                .collect()
        }
        _ => vec![],
    }
}

/// macOS networksetup 参数（CIDR + 域名 + 通配原样去重）。上游 `formatBypassForMac`。
pub fn format_bypass_for_mac(list: &[String]) -> Vec<String> {
    dedupe_trim(list.iter().cloned())
}

/// Linux gsettings ignore-hosts（CIDR + 域名原样去重）。上游 `formatBypassForLinux`。
pub fn format_bypass_for_linux(list: &[String]) -> Vec<String> {
    dedupe_trim(list.iter().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Cfg {
        on: Option<bool>,
        list: Option<Vec<String>>,
    }
    impl BypassConfig for Cfg {
        fn bypass_lan(&self) -> Option<bool> {
            self.on
        }
        fn bypass_lan_list(&self) -> Option<&[String]> {
            self.list.as_deref()
        }
    }

    #[test]
    fn effective_default_when_unset() {
        let list = effective_bypass_lan(&Cfg {
            on: None,
            list: None,
        });
        assert!(list.contains(&"192.168.0.0/16".to_string()));
        assert!(list.contains(&"localhost".to_string()));
    }

    #[test]
    fn effective_off_when_false() {
        assert!(effective_bypass_lan(&Cfg {
            on: Some(false),
            list: None
        })
        .is_empty());
    }

    #[test]
    fn effective_user_list() {
        let list = effective_bypass_lan(&Cfg {
            on: Some(true),
            list: Some(vec!["10.0.0.0/8".into()]),
        });
        assert_eq!(list, vec!["10.0.0.0/8".to_string()]);
    }

    #[test]
    fn cidr_detection() {
        assert!(is_ipv4_cidr("192.168.0.0/16"));
        assert!(is_ipv4_cidr("10.0.0.0/8"));
        assert!(!is_ipv4_cidr("localhost"));
        assert!(is_ipv6_cidr("fc00::/7"));
        assert!(is_ipv6_cidr("fe80::/10"));
        assert!(!is_ipv6_cidr("192.168.0.0/16"));
        assert!(is_ip_cidr("10.0.0.0/8"));
        assert!(is_ip_cidr("fc00::/7"));
        assert!(!is_ip_cidr("*.local"));
    }

    #[test]
    fn bypass_lan_cidrs_filters_domains() {
        let list = vec![
            "10.0.0.0/8".to_string(),
            "localhost".to_string(),
            "*.local".to_string(),
            "fc00::/7".to_string(),
            "192.168.0.0/16".to_string(),
        ];
        let cidrs = bypass_lan_cidrs(&list);
        assert_eq!(cidrs.len(), 3);
        assert!(cidrs.contains(&"10.0.0.0/8".to_string()));
        assert!(cidrs.contains(&"fc00::/7".to_string()));
    }

    // ── F1: ensure_bypass_lan_list（配置读取边界补齐，防默认坍塌）──

    /// **F1 no-collapse 门**：缺 `bypassLANList` 的配置，经边界补齐 + 编辑器追加一条后，
    /// 27 条 `DEFAULT_BYPASS_LAN` 一条不丢（复现「首个按键坍塌」并证明已修）。
    #[test]
    fn ensure_undefined_then_append_does_not_drop_defaults() {
        // 新用户配置：store 不 seed bypassLANList → 字段缺省。
        let mut cfg = serde_json::json!({ "proxyMode": "global", "mixedPort": 7890 });
        assert!(
            cfg.get("bypassLANList").is_none(),
            "前提：新配置不含 bypassLANList（store 未 seed）"
        );

        // 边界补齐（config:get 对 UI 下发的那一步）。
        ensure_bypass_lan_list(&mut cfg);

        let injected: Vec<String> = cfg["bypassLANList"]
            .as_array()
            .expect("补齐后应为数组")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        // 收到的正是全部 27 条默认，而非前端 3 条兜底。
        assert_eq!(
            injected.len(),
            DEFAULT_BYPASS_LAN.len(),
            "UI 应收到全部默认，实收 {injected:?}"
        );
        for d in DEFAULT_BYPASS_LAN {
            assert!(injected.contains(&d.to_string()), "缺默认项 {d}");
        }

        // 模拟 ListEditor 首个按键：在收到的清单尾部追加一条，写回。
        let mut edited = injected.clone();
        edited.push("198.18.0.0/15".to_string());

        // 追加后，24 条会被前端兜底丢弃的关键默认仍在（回归锚点）。
        for critical in ["10.0.0.0/8", "172.16.0.0/12", "100.64.0.0/10", "*.local"] {
            assert!(
                edited.contains(&critical.to_string()),
                "追加一条后默认项 {critical} 被丢弃（坍塌回归）"
            );
        }
        assert_eq!(edited.len(), DEFAULT_BYPASS_LAN.len() + 1);
    }

    /// 幂等 + 尊重用户意图：已有具体数组（含用户清空的 `[]`）不被覆盖。
    #[test]
    fn ensure_preserves_existing_and_empty_user_list() {
        let mut with_list = serde_json::json!({ "bypassLANList": ["10.0.0.0/8"] });
        ensure_bypass_lan_list(&mut with_list);
        assert_eq!(
            with_list["bypassLANList"],
            serde_json::json!(["10.0.0.0/8"])
        );

        // 用户清空 → [] 是显式意图，不得被默认覆盖。
        let mut cleared = serde_json::json!({ "bypassLANList": [] });
        ensure_bypass_lan_list(&mut cleared);
        assert_eq!(cleared["bypassLANList"], serde_json::json!([]));
    }

    /// **镜像锁**：`ensure_bypass_lan_list`（作用于 JSON 缺省态）与 `effective_bypass_lan`
    /// （作用于 typed 配置）逐条对齐 —— 任一侧改语义未同步即转红，杜绝双真相漂移。
    #[test]
    fn ensure_mirrors_effective_bypass_lan() {
        // 缺省（bypassLAN 未设）→ DEFAULT，与 effective(None,None) 一致。
        let mut absent = serde_json::json!({});
        ensure_bypass_lan_list(&mut absent);
        let via_ensure: Vec<String> = absent["bypassLANList"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let via_effective = effective_bypass_lan(&Cfg {
            on: None,
            list: None,
        });
        assert_eq!(via_ensure, via_effective, "缺省分支须与 effective 一致");

        // 总开关关（bypassLAN=false）→ []，与 effective(Some(false),None) 一致。
        let mut off = serde_json::json!({ "bypassLAN": false });
        ensure_bypass_lan_list(&mut off);
        assert_eq!(off["bypassLANList"], serde_json::json!([]));
        assert!(effective_bypass_lan(&Cfg {
            on: Some(false),
            list: None
        })
        .is_empty());
    }

    #[test]
    fn windows_patterns_align() {
        assert_eq!(
            ipv4_cidr_to_windows_patterns("10.0.0.0/8"),
            vec!["10.*".to_string()]
        );
        assert_eq!(
            ipv4_cidr_to_windows_patterns("192.168.0.0/16"),
            vec!["192.168.*".to_string()]
        );
        assert_eq!(
            ipv4_cidr_to_windows_patterns("192.168.1.0/24"),
            vec!["192.168.1.*".to_string()]
        );
        // /12 枚举 16 个。
        let p12 = ipv4_cidr_to_windows_patterns("172.16.0.0/12");
        assert_eq!(p12.len(), 16);
        assert!(p12[0].starts_with("172.16."));
        assert!(p12[15].starts_with("172.31."));
        // 不对齐前缀 → 空。
        assert!(ipv4_cidr_to_windows_patterns("100.64.0.0/10").is_empty());
    }
}
