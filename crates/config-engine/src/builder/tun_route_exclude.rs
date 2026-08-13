//! TUN 排除段计算（上游 `shared/tun-route-exclude.ts` 1:1 移植）。
//!
//! computeUserTunExclude（连入来源排除，减 mesh/fakeip/macOS 物理 LAN）+
//! computeWinBypassExclude（Windows bypassLAN carve，算术差集挖 engaged mesh 段）。

#![forbid(unsafe_code)]

use crate::user_config::cidr::{partition_cidrs_by_overlap, subtract_cidrs};
use crate::user_config::collections::dedupe;
// **必须**用 rule_validate 的严格校验（上游 `rules.isValidIpCidr` 的对位移植）：八位组 ≤255 / 禁前导零 /
// 掩码 v4≤32·v6≤128 / IPv6 结构合法。system_proxy_bypass::is_ip_cidr 是形状粗判（对位 上游 `isIpCidr`，
// 只数点分段数与位数），`256.1.1.1/24`、`10.0.0.0/40` 都能过——这些串进 route_exclude_address 会让
// sing-box `netip.ParsePrefix` 启动 FATAL，正是本函数存在的理由。
use crate::user_config::rule_validate::is_valid_ip_cidr;

const V4_MIN_PREFIX: u32 = 8;
const V6_MIN_PREFIX: u32 = 7;

/// 规范化 + 校验排除条目（裸 IP 补掩码，拒 catch-all/过宽/非法）。上游 `normalizeTunExcludeCidr`。
pub fn normalize_tun_exclude_cidr(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let is_v6 = t.contains(':');
    let cidr = if t.contains('/') {
        t.to_string()
    } else {
        format!("{t}/{}", if is_v6 { 128 } else { 32 })
    };
    if !is_valid_ip_cidr(&cidr) {
        return None;
    }
    let prefix_str = cidr.split('/').nth(1).unwrap_or("32");
    let prefix: u32 = prefix_str.parse().ok()?;
    if prefix < if is_v6 { V6_MIN_PREFIX } else { V4_MIN_PREFIX } {
        return None;
    }
    Some(cidr)
}

/// 用户排除输入。上游 `UserTunExcludeInput`。
pub struct UserTunExcludeInput<'a> {
    pub platform: &'a str,
    pub user_cidrs: &'a [String],
    pub mesh_cidrs: &'a [String],
    pub fakeip_ranges: &'a [String],
    pub own_lan_cidrs: &'a [String],
}

/// 用户排除结果。上游 `UserTunExcludeResult`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserTunExcludeResult {
    pub extra: Vec<String>,
    pub dropped_invalid: usize,
    pub dropped_mesh_overlap: Vec<String>,
    pub dropped_fakeip_overlap: Vec<String>,
    pub dropped_own_lan_mac: Vec<String>,
}

/// 计算用户声明 TUN 排除段的最终生效集。上游 `computeUserTunExclude`。
pub fn compute_user_tun_exclude(input: &UserTunExcludeInput) -> UserTunExcludeResult {
    let mut dropped_invalid = 0;
    let normalized: Vec<String> = input
        .user_cidrs
        .iter()
        .filter_map(|raw| match normalize_tun_exclude_cidr(raw) {
            Some(c) => Some(c),
            None => {
                dropped_invalid += 1;
                None
            }
        })
        .collect();
    let valid = dedupe(normalized);

    let (mesh_overlap, mesh_disjoint) = partition_cidrs_by_overlap(&valid, input.mesh_cidrs);
    let (fakeip_overlap, fakeip_disjoint) =
        partition_cidrs_by_overlap(&mesh_disjoint, input.fakeip_ranges);

    let (extra, dropped_own_lan_mac) = if input.platform == "darwin" {
        let (lan_overlap, lan_disjoint) =
            partition_cidrs_by_overlap(&fakeip_disjoint, input.own_lan_cidrs);
        (lan_disjoint, lan_overlap)
    } else {
        (fakeip_disjoint, vec![])
    };

    UserTunExcludeResult {
        extra,
        dropped_invalid,
        dropped_mesh_overlap: mesh_overlap,
        dropped_fakeip_overlap: fakeip_overlap,
        dropped_own_lan_mac,
    }
}

/// Windows bypassLAN carve 保护段（回环/链路本地/多播）。
const WIN_BYPASS_CARVE_GUARD: &[&str] = &[
    "127.0.0.0/8",
    "::1/128",
    "169.254.0.0/16",
    "fe80::/10",
    "224.0.0.0/4",
];

/// Windows bypassLAN 输入。上游 `WinBypassExcludeInput`。
pub struct WinBypassExcludeInput<'a> {
    pub bypass_cidrs: &'a [String],
    pub engaged_mesh_cidrs: &'a [String],
    pub own_lan_cidrs: &'a [String],
    pub fakeip_ranges: &'a [String],
}

/// Windows bypassLAN 结果。上游 `WinBypassExcludeResult`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WinBypassExcludeResult {
    pub exclude: Vec<String>,
    pub carved_mesh_cidrs: Vec<String>,
    pub mesh_skipped_own_lan: Vec<String>,
}

/// Windows bypassLAN 内核排除表 carve。上游 `computeWinBypassExclude`。
pub fn compute_win_bypass_exclude(input: &WinBypassExcludeInput) -> WinBypassExcludeResult {
    // 1. fakeip 整条剔除。
    let (_fakeip_overlap, after_fakeip) =
        partition_cidrs_by_overlap(input.bypass_cidrs, input.fakeip_ranges);

    // 2. 只考虑落在某 bypass 条目内的 engaged mesh 段。
    let engaged: Vec<String> = dedupe(input.engaged_mesh_cidrs.iter().cloned());
    let relevant_mesh: Vec<String> = engaged
        .into_iter()
        .filter(|m| crate::user_config::cidr::cidr_overlaps_any(m, &after_fakeip))
        .collect();

    // 3. 分流：与保护段（物理子网 + guard）相交的段不 carve。
    let mut guard_with_lan: Vec<String> = input.own_lan_cidrs.to_vec();
    guard_with_lan.extend(WIN_BYPASS_CARVE_GUARD.iter().map(|s| s.to_string()));
    let (mesh_skipped_own_lan, carve_mesh) =
        partition_cidrs_by_overlap(&relevant_mesh, &guard_with_lan);

    // 4. 无可 carve → 原样返回。
    if carve_mesh.is_empty() {
        return WinBypassExcludeResult {
            exclude: after_fakeip,
            carved_mesh_cidrs: vec![],
            mesh_skipped_own_lan,
        };
    }

    // 5. 算术差集。
    WinBypassExcludeResult {
        exclude: subtract_cidrs(&after_fakeip, &carve_mesh),
        carved_mesh_cidrs: carve_mesh,
        mesh_skipped_own_lan,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_bare_ip() {
        assert_eq!(
            normalize_tun_exclude_cidr("192.168.1.1"),
            Some("192.168.1.1/32".into())
        );
        assert_eq!(
            normalize_tun_exclude_cidr("fe80::1"),
            Some("fe80::1/128".into())
        );
    }

    #[test]
    fn normalize_rejects_catch_all() {
        assert_eq!(normalize_tun_exclude_cidr("0.0.0.0/0"), None);
        assert_eq!(normalize_tun_exclude_cidr("::/0"), None);
        assert_eq!(normalize_tun_exclude_cidr("10.0.0.0/7"), None); // 过宽（< 8）
    }

    #[test]
    fn normalize_rejects_invalid() {
        assert_eq!(normalize_tun_exclude_cidr(""), None);
        assert_eq!(normalize_tun_exclude_cidr("  "), None);
        assert_eq!(normalize_tun_exclude_cidr("abc"), None);
    }

    #[test]
    fn normalize_rejects_out_of_range_and_leading_zero() {
        // 这些串**形状**合法（点分四段 + 数字掩码），只有严格校验（rule_validate::is_valid_ip_cidr）能拦。
        // 一旦退回 system_proxy_bypass::is_ip_cidr 的形状粗判，它们会原样进 route_exclude_address →
        // sing-box `netip.ParsePrefix` 启动 FATAL（整个代理起不来），故本用例是校验强度的变异锁。
        assert_eq!(normalize_tun_exclude_cidr("256.1.1.1/24"), None); // 八位组越界
        assert_eq!(normalize_tun_exclude_cidr("192.168.1.1/33"), None); // v4 掩码越界
        assert_eq!(normalize_tun_exclude_cidr("010.0.0.1/24"), None); // 前导零
        assert_eq!(normalize_tun_exclude_cidr("12345::1/64"), None); // v6 段 >4 位
        assert_eq!(normalize_tun_exclude_cidr("fe80::1/129"), None); // v6 掩码越界

        // 合法边界仍须放行（别把校验收紧成一刀切）。
        assert_eq!(
            normalize_tun_exclude_cidr("10.0.0.0/8"),
            Some("10.0.0.0/8".into())
        );
        assert_eq!(
            normalize_tun_exclude_cidr("fc00::/7"),
            Some("fc00::/7".into())
        );
    }

    #[test]
    fn user_exclude_reduces_mesh() {
        let input = UserTunExcludeInput {
            platform: "linux",
            user_cidrs: &["10.0.0.0/8".into(), "100.64.0.0/10".into()],
            mesh_cidrs: &["100.64.0.0/10".into()],
            fakeip_ranges: &[],
            own_lan_cidrs: &[],
        };
        let result = compute_user_tun_exclude(&input);
        assert!(result.extra.contains(&"10.0.0.0/8".to_string()));
        assert!(result
            .dropped_mesh_overlap
            .contains(&"100.64.0.0/10".to_string()));
    }

    #[test]
    fn user_exclude_mac_reduces_own_lan() {
        let input = UserTunExcludeInput {
            platform: "darwin",
            user_cidrs: &["10.0.0.0/8".into()],
            mesh_cidrs: &[],
            fakeip_ranges: &[],
            own_lan_cidrs: &["10.0.0.0/8".into()], // 同段物理 LAN
        };
        let result = compute_user_tun_exclude(&input);
        assert!(result.extra.is_empty()); // 全被物理 LAN guard 剔除
        assert!(result
            .dropped_own_lan_mac
            .contains(&"10.0.0.0/8".to_string()));
    }

    #[test]
    fn win_bypass_no_mesh_returns_original() {
        let input = WinBypassExcludeInput {
            bypass_cidrs: &["10.0.0.0/8".into(), "192.168.0.0/16".into()],
            engaged_mesh_cidrs: &[],
            own_lan_cidrs: &[],
            fakeip_ranges: &[],
        };
        let result = compute_win_bypass_exclude(&input);
        assert_eq!(result.exclude.len(), 2);
        assert!(result.carved_mesh_cidrs.is_empty());
    }

    #[test]
    fn win_bypass_carves_mesh() {
        // 10.0.0.0/8 排除，engaged mesh 10.64.0.0/10 → carve 开洞。
        let input = WinBypassExcludeInput {
            bypass_cidrs: &["10.0.0.0/8".into()],
            engaged_mesh_cidrs: &["10.64.0.0/10".into()],
            own_lan_cidrs: &[],
            fakeip_ranges: &[],
        };
        let result = compute_win_bypass_exclude(&input);
        assert!(result
            .carved_mesh_cidrs
            .contains(&"10.64.0.0/10".to_string()));
        // exclude 应为 10.0.0.0/8 ∖ 10.64.0.0/10（多段）。
        assert!(result.exclude.len() > 1);
    }

    /// **后果锁**：`/0` 一旦漏进 `own_lan_cidrs`，guard 与一切 mesh 段相交 ⇒ 一条都不 carve ⇒
    /// bypassLAN 下组网段整体绕 TUN 静默失效。第二段证明上游 `own_lan_cidr` 拒掉 `prefix=0` 后
    /// carve 恢复正常。
    ///
    /// 变异锁（沿真实后果，不止谓词层）：把 `own_lan::own_lan_cidr` 的 `prefix == 0 → None` 删掉，
    /// 或把 `netinfo::prefix_is_valid` 的下界放回 0 —— 前者让本用例第二段（`own_lan` 应为空、carve
    /// 应发生）转红。第一段不随修复变化，它记录的是「一旦漏进来会怎样」这条因果。
    #[test]
    fn win_bypass_zero_prefix_own_lan_kills_all_carve() {
        use crate::user_config::own_lan::own_lan_cidr;

        // 段一：/0 漏进 own_lan → 全部 mesh 段被 skip，exclude 原样、零 carve。
        let poisoned = WinBypassExcludeInput {
            bypass_cidrs: &["10.0.0.0/8".into()],
            engaged_mesh_cidrs: &["10.64.0.0/10".into()],
            own_lan_cidrs: &["10.0.0.5/0".into()],
            fakeip_ranges: &[],
        };
        let poisoned_result = compute_win_bypass_exclude(&poisoned);
        assert!(
            poisoned_result.carved_mesh_cidrs.is_empty(),
            "/0 guard 与一切段相交，carve 必然全灭"
        );
        assert!(poisoned_result
            .mesh_skipped_own_lan
            .contains(&"10.64.0.0/10".to_string()));
        assert_eq!(poisoned_result.exclude, vec!["10.0.0.0/8".to_string()]);

        // 段二：own_lan_cidr 在汇流点拒掉 prefix=0 ⇒ own_lan 为空 ⇒ carve 正常发生。
        let own_lan: Vec<String> = [("10.0.0.5", 0u8), ("", 24u8)]
            .into_iter()
            .filter_map(|(addr, prefix)| own_lan_cidr(addr, prefix, false))
            .collect();
        assert!(own_lan.is_empty(), "prefix=0 必须在 own_lan_cidr 处被挡住");
        let clean = WinBypassExcludeInput {
            bypass_cidrs: &["10.0.0.0/8".into()],
            engaged_mesh_cidrs: &["10.64.0.0/10".into()],
            own_lan_cidrs: &own_lan,
            fakeip_ranges: &[],
        };
        let clean_result = compute_win_bypass_exclude(&clean);
        assert!(clean_result
            .carved_mesh_cidrs
            .contains(&"10.64.0.0/10".to_string()));
        assert!(clean_result.mesh_skipped_own_lan.is_empty());
        assert!(clean_result.exclude.len() > 1, "差集应把 /8 打成多段");
    }

    #[test]
    fn win_bypass_skips_mesh_on_protected() {
        // mesh 段与保护段（回环）相交 → 不 carve。
        let input = WinBypassExcludeInput {
            bypass_cidrs: &["127.0.0.0/8".into()],
            engaged_mesh_cidrs: &["127.0.0.0/8".into()],
            own_lan_cidrs: &[],
            fakeip_ranges: &[],
        };
        let result = compute_win_bypass_exclude(&input);
        assert!(result.carved_mesh_cidrs.is_empty());
        assert!(result
            .mesh_skipped_own_lan
            .contains(&"127.0.0.0/8".to_string()));
    }
}
