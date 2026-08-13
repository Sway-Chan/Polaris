//! sing-box inbound 类型（`singbox-config-types.ts:86-114 SingBoxInbound`）。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// `inbounds[]`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inbound {
    #[serde(rename = "type")]
    pub type_field: String,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen_port: Option<u16>,
    // TUN 模式
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_route: Option<bool>,
    /// auto_redirect（1.10+）：Linux nftables 改善 TUN 路由/性能。P6 LAN 网关按 MAC 过滤时必发。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_redirect: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict_route: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// udp_mapping / udp_filtering（1.14 新增；**只有 tun / tproxy 两个 inbound 变体带这组键** ——
    /// 随包 beta.7 `sing-box schema` 实测：`$defs/Inbound/oneOf[16].properties.type.const == "tun"`、
    /// `oneOf[13] == "tproxy"`，其余 20 个变体没有）。
    ///
    /// 两项**合起来**才是用户感知的「NAT 类型」：mapping 决定同一本地端口对不同目的地复用不复用同一个
    /// 映射，filtering 决定允许哪些远端的回包进来。档位映射表与判据在 `builder::inbounds::udp_nat_behaviors`。
    ///
    /// **缺席即最宽松**：上游两项默认都是 `endpoint_independent`（<https://sing-box.sagernet.org/configuration/shared/udp-nat/>），
    /// 即全锥。故 Polaris 只在用户显式选档时下发（入口见 `user_config::tun_config::UdpNatType`），
    /// 默认一个键都不发 —— 这既保住金样零 delta，也避免把「当前默认值」硬编码进配置、日后上游改默认时
    /// 我们还钉在旧值上（与 `tun_stack` 刻意 pin 协议栈相反：那里 pin 是因为默认随 build-tag 漂且我们有
    /// 实测判据要压住，这里没有任何判据说全锥不该是默认）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub udp_mapping: Option<UdpNatBehavior>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub udp_filtering: Option<UdpNatBehavior>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_exclude_address: Option<Vec<String>>,
    /// include/exclude_mac_address（1.14；P6 LAN 网关，互斥，仅 Linux+auto_route+auto_redirect）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_mac_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_mac_address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<InboundPlatform>,
}

/// sing-box UDP NAT 行为闭集（`udp_mapping` / `udp_filtering` 共用同一个类型 `option.UDPNATBehavior`，
/// 见随包 beta.7 二进制里的结构体 tag）。
///
/// 刻意用**枚举**而非裸 `String`，与 [`super::dns::DomainStrategy`] 同一条理由：值是内核 schema 的闭集，
/// 而 `sing-box check` 对写错的值只在 unmarshal 到该 inbound 时才报 `unknown UDP NAT behavior`，
/// 靠人眼与 check 都不是稳定拦截点；编译期才是。
///
/// **不含 schema 里的空串 `""` 变体**：那一档在内核语义上等价于「键缺席 → 用默认」，而本仓表达「缺席」
/// 一律用 `Option::None` + `skip_serializing_if`（`Inbound` 其余每一个可选字段都是这套）。同一语义留两条
/// 表达路径必然漂：`Some(Empty)` 与 `None` 序列化出**不同 JSON、相同行为**，对拍与 diff 就再也说不清
/// 一处差异是不是回归。故只保留三个有值的档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UdpNatBehavior {
    /// 同一本地端口对所有目的地复用同一映射 / 接受任意远端来的包。
    EndpointIndependent,
    /// 按目的**地址**分映射 / 只接受曾发过包的地址来的包。
    AddressDependent,
    /// 按目的**地址+端口**分映射 / 只接受曾发过包的地址+端口来的包（最严）。
    AddressAndPortDependent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboundPlatform {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_proxy: Option<HttpProxyPlatform>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpProxyPlatform {
    pub enabled: bool,
    pub server: String,
    pub server_port: u16,
}
