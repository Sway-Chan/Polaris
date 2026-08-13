//! sing-box 配置输出类型（1:1 映射 上游 `singbox-config-types.ts`）。
//!
//! 模块边界：本模块**只含 sing-box JSON schema 的 Rust 类型镜像**，零业务逻辑。
//! 字段名、Optionality、序列化行为与 Polaris TS 侧逐字节对齐（B1 金样对拍约束）。
//! builder 逻辑（从 UserConfig 生成这些类型）在 crate 根的 builder 模块。

pub mod config;
pub mod dns;
pub mod endpoint;
pub mod inbound;
pub mod outbound;
pub mod route;

pub use config::{
    ApiDashboard, ApiService, CacheFile, Experimental, HttpClient, LogConfig, SingBoxConfig,
};
pub use dns::{
    DnsConfig, DnsRule, DnsServer, DomainResolver, DomainStrategy, FakeIpConfig, OneOrMany,
};
pub use endpoint::{Endpoint, WireGuardPeer};
pub use inbound::{HttpProxyPlatform, Inbound, InboundPlatform, UdpNatBehavior};
pub use outbound::{
    Ech, Hysteria2Obfs, Multiplex, Outbound, OutboundTls, OutboundVersion, Reality, Transport,
    UdpOverTcp, Utls,
};
pub use route::{RouteConfig, RouteRule, RuleSet};
