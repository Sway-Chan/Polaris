//! DNS 常量 + 全局直连哨兵（上游 `shared/dns.ts` 常量 + `shared/direct-selection.ts` 1:1 移植）。
//!
//! BOOTSTRAP_DIRECT_DNS_IPS（route-builder 引导直连放行的国内 DNS）+ CONTROLLED_TUN_DNS_IP
//! （TUN 接管系统 DNS 被强制改成的受控 IP）+ isDirectSelection 哨兵。

#![forbid(unsafe_code)]

/// 引导直连放行的国内 DNS IP（含内置 DoH 上游 223.5.5.5/1.12.12.12）。上游 `BOOTSTRAP_DIRECT_DNS_IPS`。
pub const BOOTSTRAP_DIRECT_DNS_IPS: &[&str] = &[
    "223.5.5.5",
    "223.6.6.6",
    "1.12.12.12",
    "119.29.29.29",
    "119.28.28.28",
    "114.114.114.114",
];

/// TUN 接管时系统 DNS 被强制改成的受控 IP（8.8.8.8）。
/// 故意排除出 BOOTSTRAP_DIRECT_DNS_IPS（否则被直连放行、逃逸 hijack）。上游 `CONTROLLED_TUN_DNS_IP`。
pub const CONTROLLED_TUN_DNS_IP: &str = "8.8.8.8";

/// 是否引导直连 DNS IP。上游 `isBootstrapDirectDns`。
pub fn is_bootstrap_direct_dns(ip: &str) -> bool {
    BOOTSTRAP_DIRECT_DNS_IPS.contains(&ip.trim())
}

/// 全局节点选择「直连」哨兵值。上游 `DIRECT_SERVER_ID`。
pub const DIRECT_SERVER_ID: &str = "__direct__";

/// selectedServerId 是否为直连哨兵（全局出口走 direct）。上游 `isDirectSelection`。
pub fn is_direct_selection(selected_server_id: Option<&str>) -> bool {
    selected_server_id == Some(DIRECT_SERVER_ID)
}

/// 全局节点选择「阻断」哨兵值（Polaris 新增，上游 无对应物）。
///
/// **语义边界（写给后来者，别当 bug 修）**：出口选单支配的是「本该走出口的那部分流量」，不是全部流量。
/// 直连规则（LAN/私网、`geosite-cn`/`geoip-cn`、ICMP、`protocol:dns`、DoH 引导、sing-box 自身进程）
/// 都是 `action:route → outbound:direct` **显式命中，压根不经过 proxy-selector**，故阻断影响不到它们。
/// 由此三种模式下的观感差异是出口语义的正确外延，不是缺陷：
///   - `smart`：国内照常直连、只断「本该走代理」的境外流量（≈ 反向 proxifier）；
///   - `global`：断几乎全部，仅剩上面那批豁免；
///   - `direct`：`route.final` 恒 = `direct`、无流量经过 selector ⇒ **本哨兵无效**，故 UI 在该模式下禁用该选项
///     （不留静默 no-op，见 `HomeScreen.tsx` blockDisabledReason）。
///
/// 「全流量 kill switch」是另一个功能（要连 LAN/DNS/管理面一起掐），不走出口选单，本哨兵不承担。
pub const BLOCK_SERVER_ID: &str = "__block__";

/// selectedServerId 是否为阻断哨兵（全局出口走 block 出站）。
pub fn is_block_selection(selected_server_id: Option<&str>) -> bool {
    selected_server_id == Some(BLOCK_SERVER_ID)
}

/// selectedServerId 是否为「非节点哨兵」（direct / block）。
///
/// 收口所有「该 id 不是真实节点 ⇒ 豁免存在性校验 / 不进节点引用集 / 无真实出站」的判据。
/// 分开写两个谓词再逐处 `||` 是此前 `__direct__` 铺开到 ~8 处的成因；新增第三个哨兵时只改这里。
pub fn is_sentinel_selection(selected_server_id: Option<&str>) -> bool {
    is_direct_selection(selected_server_id) || is_block_selection(selected_server_id)
}

/// 全局出口 proxy-selector 的直连成员 tag。上游 `DIRECT_TAG`。
pub const DIRECT_TAG: &str = "direct";

/// 全局出口 proxy-selector 的阻断成员 tag（= `block` 出站的 tag，`outbounds.rs` 无条件生成）。
///
/// **这是 `block` 出站现在仅剩的消费者**：规则级阻断（自定义规则 / 应用分流的 `RuleAction::Block`）
/// 已迁到 sing-box 1.11+ 官方的 `action:"reject"`，不再引用任何出站。而 selector 的
/// `default`/成员表按 schema 只能填 outbound tag，**没有「reject 出站」可填** ⇒ 出口选阻断
/// 只能继续押在这个出站上。两条腿的取舍与实证见 `builder/outbounds.rs` 生成处的注释。
pub const BLOCK_TAG: &str = "block";

/// 全局出口 selector 的 tag —— **生产方与消费方的唯一契约**。
///
/// 这一个字符串同时是：
/// - **生产方**：`builder/outbounds.rs` 生成的 selector 出站的 tag；
/// - **路由方**：`builder/route.rs` 把主代理流量指向的出站；
/// - **消费方**：`builder/hotswitch.rs` 规划 PUT 时的 `selector_tag`，最终由 switch-engine
///   经 gRPC `SelectOutbound` 下发给运行中的核。
///
/// **为什么必须单一真值**：三方一旦拼写漂移，`SelectOutbound` 会 PUT 到一个不存在的 selector →
/// 核返回 `NotFound` → executor 判 `Failed` → **静默退回去抖重启**。用户看到「节点切换了」，
/// 实际发生的是整核重启（断流），而热切换**永久失效且无人报错** —— 兜底把失败伪装成了成功。
/// 此前本仓正是四份拼写（hotswitch.rs 一个私有 const + outbounds.rs / route.rs 多处内联字面量）。
///
/// 与 [`DIRECT_TAG`] 同列：二者都是 proxy-selector 语义域内的出站 tag 常量。
pub const PROXY_SELECTOR_TAG: &str = "proxy-selector";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_dns_check() {
        assert!(is_bootstrap_direct_dns("223.5.5.5"));
        assert!(is_bootstrap_direct_dns("  1.12.12.12  "));
        assert!(!is_bootstrap_direct_dns("8.8.8.8"));
        assert!(!is_bootstrap_direct_dns("1.1.1.1"));
    }

    #[test]
    fn controlled_dns_excluded_from_bootstrap() {
        // 不变量：CONTROLLED_TUN_DNS_IP 不在 BOOTSTRAP_DIRECT_DNS_IPS。
        assert!(!is_bootstrap_direct_dns(CONTROLLED_TUN_DNS_IP));
    }

    #[test]
    fn direct_selection_sentinel() {
        assert!(is_direct_selection(Some(DIRECT_SERVER_ID)));
        assert!(!is_direct_selection(Some("s1")));
        assert!(!is_direct_selection(None));
    }

    #[test]
    fn block_selection_sentinel() {
        assert!(is_block_selection(Some(BLOCK_SERVER_ID)));
        assert!(!is_block_selection(Some(DIRECT_SERVER_ID)));
        assert!(!is_block_selection(Some("s1")));
        assert!(!is_block_selection(None));
    }

    /// 两个哨兵值必须互异且都不是合法节点 id 形状——撞值会让「阻断」静默变成「直连」。
    #[test]
    fn sentinels_are_distinct() {
        assert_ne!(DIRECT_SERVER_ID, BLOCK_SERVER_ID);
        assert!(is_sentinel_selection(Some(DIRECT_SERVER_ID)));
        assert!(is_sentinel_selection(Some(BLOCK_SERVER_ID)));
        assert!(!is_sentinel_selection(Some("s1")));
        assert!(!is_sentinel_selection(None));
    }

    /// BLOCK_TAG 必须与 `outbounds.rs` 无条件生成的 block 出站 tag 同字面 —— 漂了就 PUT 到不存在的成员，
    /// 核返 NotFound → executor 判 Failed → 静默退回重启（与 PROXY_SELECTOR_TAG 同款失效形态）。
    #[test]
    fn block_tag_matches_generated_outbound_tag() {
        assert_eq!(BLOCK_TAG, "block");
        assert_ne!(BLOCK_TAG, DIRECT_TAG);
    }
}
