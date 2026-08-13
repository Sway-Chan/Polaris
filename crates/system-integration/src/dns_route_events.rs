//! macOS `route -n monitor` 输出行解析（纯函数，无副作用）。
//!
//! 1:1 移植自 上游 `dns-route-events.ts`。
//! 用途：[`crate::dns_watcher::DnsInterfaceWatcher`] 长驻 `route -n monitor`，逐行喂本函数判定
//! 「是否值得触发一次 DNS reconcile 的网络变更」。命中 → 去抖后调 reconcile。

#![forbid(unsafe_code)]

/// route monitor 中「值得触发 reconcile」的消息类型（前缀匹配，覆盖 RTM_IFINFO2 / RTM_NEWADDR2 变体）：
/// - IFINFO：接口 up/down（插拔网卡、Wi-Fi 开关、坞站上下线）。
/// - NEWADDR / DELADDR：接口地址增删（DHCP 续约、IPv6 SLAAC、VPN 虚拟地址）。
/// - ADD / DELETE：路由增删（默认路由切换 = 出口/解析器可能整体易主）。
pub const TRIGGER_RTM_TYPES: &[&str] = &[
    "RTM_IFINFO",
    "RTM_NEWADDR",
    "RTM_DELADDR",
    "RTM_ADD",
    "RTM_DELETE",
];

/// 判定单行 `route -n monitor` 输出是否表示「值得触发 DNS reconcile 的网络变更」。
///
/// 命中：行首 token 为上述 RTM_ 触发类型（或其带数字后缀的变体）。
/// 不命中：统计头（`got message of size ...`）、地址/标志明细行、空行、畸形行。
/// 永不抛（Polaris 不变量）。上游 `isDnsReconcileTriggerLine`。
pub fn is_dns_reconcile_trigger_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    // 统计头高频噪音，显式排除。
    if trimmed.starts_with("got message of size") {
        return false;
    }
    // 首 token（冒号 / 空格分隔）。明细行首 token 是地址族/标志名，不会以 RTM_ 起。
    let first_token: &str = trimmed
        .split(|c: char| c.is_whitespace() || c == ':')
        .next()
        .unwrap_or("");
    if first_token.is_empty() {
        return false;
    }
    TRIGGER_RTM_TYPES
        .iter()
        .any(|t| first_token == *t || first_token.starts_with(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_on_rtm_ifinfo() {
        assert!(is_dns_reconcile_trigger_line(
            "RTM_IFINFO: oscp_route_recv\n"
        ));
        assert!(is_dns_reconcile_trigger_line(
            "  RTM_NEWADDR: address added"
        ));
    }

    #[test]
    fn triggers_on_numbered_variants() {
        assert!(is_dns_reconcile_trigger_line("RTM_IFINFO2 len"));
        assert!(is_dns_reconcile_trigger_line("RTM_NEWADDR2: ..."));
    }

    #[test]
    fn triggers_on_route_add_delete() {
        assert!(is_dns_reconcile_trigger_line("RTM_ADD: default gateway"));
        assert!(is_dns_reconcile_trigger_line("RTM_DELETE: default gateway"));
    }

    #[test]
    fn ignores_stat_header() {
        assert!(!is_dns_reconcile_trigger_line(
            "got message of size 92 on Wed Jul 15 10:00:00 2026"
        ));
    }

    #[test]
    fn ignores_noise_and_empty() {
        assert!(!is_dns_reconcile_trigger_line(""));
        assert!(!is_dns_reconcile_trigger_line("   "));
        assert!(!is_dns_reconcile_trigger_line("lock: 0 flags: 0x1"));
    }

    #[test]
    fn ignores_non_trigger_rtm_types() {
        assert!(!is_dns_reconcile_trigger_line("RTM_GET: query"));
        assert!(!is_dns_reconcile_trigger_line("RTM_LOSING: ..."));
        assert!(!is_dns_reconcile_trigger_line("RTM_MISS: ..."));
    }
}
