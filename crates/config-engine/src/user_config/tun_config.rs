//! TUN 配置类型 + Windows 接口名解析 + FakeIP 段常量。
//!
//! 上游 `shared/types.ts TunModeConfig` + `shared/tun-interface.ts` + `shared/fakeip-filter.ts` 合并移植。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

use crate::user_config::neighbor::TunMacFilterMode;
use crate::user_config::tun_stack::TunStack;

/// FakeIP IPv4 段（benchmarking 保留）。上游 `FAKEIP_INET4_RANGE`。
pub const FAKEIP_INET4_RANGE: &str = "198.18.0.0/15";

/// FakeIP IPv6 段。上游 `FAKEIP_INET6_RANGE`。
pub const FAKEIP_INET6_RANGE: &str = "2001:2::/48";

/// Windows TUN 接口名（缺省）。与外部 sing-box 默认 tun0 / 其它 VPN 网卡区分。
/// issue #327：起核后「适配器真建出来了没」正向探测的锚定名，须与探测侧同源
///（`polaris-` 前缀落在 `polaris_helper::platform::windows::wintun::PROBE_PREFIXES` 的可枚举面内，
/// 用户改成自定义名时探测转「不可断言」而非误判失败）。
pub const WIN_TUN_INTERFACE: &str = "polaris-tun0";

/// UDP NAT 类型档（用户语汇）。缺席 = 跟随内核默认。
///
/// # 为什么是「一个 NAT 类型档」，而不是把 `udp_mapping` / `udp_filtering` 两个原始字段各配一个控件
///
/// 内核那两个字段各有 3 个取值 ⇒ 9 种组合，其中**只有 4 种对应真实存在的 NAT 语义**（RFC 3489 的
/// 三种锥形 + 对称），其余 5 种是「过滤比映射还松」之类的无意义组合 —— 逐字段暴露等于把 5 个必然
/// 无意义的格子摆到用户面前，而用户认得的词是「NAT 类型」，不是 `udp_mapping`。这与本页既有的
/// `macFilterMode` 同形：那也是**一个**下拉（关闭/仅允许/排除）映射到内核**两个**互斥字段
/// (`include_mac_address` / `exclude_mac_address`)，不是把两个清单并排摆出来。
///
/// 另一条路（逐字段暴露）唯一的好处是「和内核 schema 一一对应、日后加值不用改 UI」；本页并不追求
/// 这条 —— `stack` 已经是 auto 档 + 平台解析，`mtu` 已经是「留空即按栈×平台推导」，语汇一直是
/// **意图级**而非字段级。
///
/// # 为什么没有「对称 NAT」档
///
/// 对称（mapping = `address_and_port_dependent`）比端口受限锥更严，但它对**本机出网**没有额外收益：
/// 端口受限锥已经把「只接受曾发过包的地址+端口」这条收紧做满，对称多出来的那一半是「每个目的地换一个
/// 源端口」，代价是彻底堵死一切打洞、收益只在「防端口预测」这种本机 TUN 上不存在的威胁模型里。
/// YAGNI：没有真实场景就不加档（加档的成本是 5 语文案 + 一格映射表 + 一条用户永远选错的路）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UdpNatType {
    /// 全锥：任何远端都能往回发（打洞最容易）。
    FullCone,
    /// 受限锥：只接受**曾发过包的地址**来的包。
    RestrictedCone,
    /// 端口受限锥：只接受**曾发过包的地址+端口**来的包（本档里最严，P2P/联机最容易失败）。
    PortRestrictedCone,
}

/// TUN 模式配置（上游 `TunModeConfig`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TunModeConfig {
    /// TUN MTU。**`None` = 自动**（按最终栈 × 平台取 `tun_stack::default_mtu_for`）。
    ///
    /// # 为什么是 `Option` 而不是「默认值 + 哨兵」
    ///
    /// 此前是 `u32` + 默认 1350，且 `builder/inbounds.rs` 把 **`Some(9000)` 当「未设置」**回落平台默认
    /// （上游 `singbox-inbounds-builder.ts:393` 的同款哨兵）。哨兵有两个后果：① 用户一旦真想要 9000，
    /// 会被静默改写成 1350/1400，属「设了但不生效」；② 默认值随平台/栈变化时，无从区分「持久化的 1350
    /// 是旧默认」还是「用户就要 1350」。`Option` 两者都没有：缺席即自动，在场即用户意图，逐字下发。
    ///
    /// 存量配置的 `mtu` 一律由 `polaris-store` 的 `migrate_tun_mtu` 清成缺席 —— 判据是**本项在此之前
    /// 从未有过 UI 入口**，故磁盘上的任何值都是程序写的默认，没有一个承载用户意图。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    #[serde(default)]
    pub stack: TunStack,
    #[serde(default = "default_true", rename = "autoRoute")]
    pub auto_route: bool,
    #[serde(default = "default_true", rename = "strictRoute")]
    pub strict_route: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_name: Option<String>,
    #[serde(rename = "inet4Address", skip_serializing_if = "Option::is_none")]
    pub inet4_address: Option<String>,
    #[serde(rename = "inet6Address", skip_serializing_if = "Option::is_none")]
    pub inet6_address: Option<String>,
    #[serde(rename = "macFilterMode", skip_serializing_if = "Option::is_none")]
    pub mac_filter_mode: Option<TunMacFilterMode>,
    #[serde(
        default,
        rename = "macFilterList",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub mac_filter_list: Vec<String>,
    #[serde(rename = "neighborDomains", skip_serializing_if = "Option::is_none")]
    pub neighbor_domains: Option<Vec<String>>,
    #[serde(
        rename = "inboundExcludeCidrs",
        skip_serializing_if = "Option::is_none"
    )]
    pub inbound_exclude_cidrs: Option<Vec<String>>,
    /// 「NAT 类型」档 → TUN inbound 的 `udp_mapping` × `udp_filtering`（映射表见
    /// `builder::inbounds::udp_nat_behaviors`）。
    ///
    /// **`None` = 两个键一个都不下发 = 跟随内核默认**（beta.7 上游默认两项均 `endpoint_independent`，
    /// 即全锥）。这条不变量是本项的默认安全性所在：存量配置与金样 `fixtures/config-snapshot.json`
    /// 零 delta，且「没设过 NAT 类型的用户」拿到的仍是打洞最容易的那一档。
    ///
    /// 与 `mac_filter_mode` 同形（`Option` 而非带 `Auto` 变体的枚举）：那颗下拉的「关闭」档同样落成
    /// `None`、同样不发键。不学 `stack` 的 `Auto` 变体，是因为 `stack` **恒发**（Polaris 始终显式 pin
    /// 具体栈，见 `tun_stack` 模块头），`Auto` 只是「发哪一个由平台决定」；本项恰恰相反 —— 默认档的
    /// 语义就是**不发**。
    #[serde(rename = "udpNatType", skip_serializing_if = "Option::is_none")]
    pub udp_nat_type: Option<UdpNatType>,
}

fn default_true() -> bool {
    true
}

impl Default for TunModeConfig {
    fn default() -> Self {
        Self {
            mtu: None,
            stack: TunStack::Auto,
            auto_route: true,
            strict_route: true,
            interface_name: None,
            inet4_address: None,
            inet6_address: None,
            mac_filter_mode: None,
            mac_filter_list: vec![],
            neighbor_domains: None,
            inbound_exclude_cidrs: None,
            udp_nat_type: None,
        }
    }
}

/// 解析 Windows TUN 接口名：尊重自定义（合法才用），否则回落 Polaris 专属名。
/// 上游 `resolveWinTunInterfaceName`。
pub fn resolve_win_tun_interface_name(interface_name: Option<&str>) -> String {
    let custom = interface_name.unwrap_or("").trim();
    // 仅字母数字/连字符/下划线、1-32 字符（Windows 接口名约束）。
    if !custom.is_empty()
        && custom.len() <= 32
        && custom
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return custom.to_string();
    }
    WIN_TUN_INTERFACE.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win_tun_default() {
        assert_eq!(resolve_win_tun_interface_name(None), WIN_TUN_INTERFACE);
        assert_eq!(resolve_win_tun_interface_name(Some("")), WIN_TUN_INTERFACE);
        assert_eq!(
            resolve_win_tun_interface_name(Some("  ")),
            WIN_TUN_INTERFACE
        );
    }

    #[test]
    fn win_tun_custom_valid() {
        assert_eq!(resolve_win_tun_interface_name(Some("my-tun")), "my-tun");
        assert_eq!(resolve_win_tun_interface_name(Some("wg_0")), "wg_0");
    }

    #[test]
    fn win_tun_custom_invalid_falls_back() {
        assert_eq!(
            resolve_win_tun_interface_name(Some("my tun")),
            WIN_TUN_INTERFACE
        ); // 空格
        assert_eq!(
            resolve_win_tun_interface_name(Some("a".repeat(33).as_str())),
            WIN_TUN_INTERFACE
        ); // 过长
        assert_eq!(
            resolve_win_tun_interface_name(Some("bad!")),
            WIN_TUN_INTERFACE
        ); // 特殊字符
    }

    #[test]
    fn fakeip_ranges() {
        assert_eq!(FAKEIP_INET4_RANGE, "198.18.0.0/15");
        assert_eq!(FAKEIP_INET6_RANGE, "2001:2::/48");
    }
}
