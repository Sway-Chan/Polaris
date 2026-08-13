//! sing-box 配置顶层结构 + log/experimental/api service（1:1 映射 Polaris singbox-config-types.ts）。
//!
//! 字段全 snake_case（sing-box JSON schema 约定，Rust 字段本就 snake_case → 无需 rename）。
//! Optional 字段用 `#[serde(skip_serializing_if = "Option::is_none")]`：与 TS `undefined` 不序列化等价，
//! 对 B1 金样对拍逐字节 diff 至关重要（TS 侧 undefined 键不进 JSON，Rust 侧 None 也不进）。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

use super::{
    dns::DnsConfig, endpoint::Endpoint, inbound::Inbound, outbound::Outbound, route::RouteConfig,
};

/// sing-box 顶层配置（`singbox-config-types.ts:388 SingBoxConfig`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SingBoxConfig {
    pub log: LogConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dns: Option<DnsConfig>,
    pub inbounds: Vec<Inbound>,
    pub outbounds: Vec<Outbound>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<Vec<Endpoint>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<Experimental>,
    /// sing-box 1.14 管理 API（仅 1.14 核注入；clash_api 已移除，管理面统一走此 gRPC service）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<ApiService>>,
}

/// `log`（`singbox-config-types.ts:6 SingBoxLogConfig`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogConfig {
    pub level: String,
    pub timestamp: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
}

/// `experimental`（`singbox-config-types.ts:361`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Experimental {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_file: Option<CacheFile>,
}

/// `experimental.cache_file`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheFile {
    pub enabled: bool,
    pub path: String,
    /// 缓存命名空间（bump 即令旧条目不可达，逻辑清库）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_fakeip: Option<bool>,
    /// sing-box 1.14：取代 1.13 的 store_rdrc（全量 DNS 缓存持久化）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_dns: Option<bool>,
}

/// sing-box 1.14 services[]（管理 API，`singbox-config-types.ts:375 SingBoxApiService`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiService {
    #[serde(rename = "type")]
    pub type_field: String,
    pub listen: String,
    pub listen_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// sing-box 1.14 官方面板（opt-in）：enabled 时 serve 本地资源。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dashboard: Option<ApiDashboard>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiDashboard {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// **显式 HTTP client 声明**（sing-box 1.14 新增 `http_client`）。见 [`HttpClient`]。
    ///
    /// dashboard 是本仓当前**唯一**触碰「隐式默认 HTTP client」的消费点：核在
    /// `service/api/dashboard.go:start()` 里无条件 `resolveTransport()`，`http_client` 缺省时
    /// 落到 `httpClientManager.DefaultTransport()`，从而命中 1.14 的弃用回落。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_client: Option<HttpClient>,
}

/// sing-box 1.14 显式 HTTP Client 声明（`http_client`，since 1.14.0）。
///
/// **为什么必须显式**：1.14.0 起「隐式默认 HTTP client（经默认出站）」被标弃用，
/// 计划在 **1.16.0 移除**（上游 `experimental/deprecated/constants.go`
/// `OptionImplicitDefaultHTTPClient`：DeprecatedVersion 1.14.0 / ScheduledVersion 1.16.0）。
/// 移除后 `DefaultTransport()` 返回 nil，消费点拿不到 transport 即报错——对 dashboard 而言是
/// `create dashboard http client` 起服务失败，不是「静默降级」。
///
/// **为什么只声明 `detour` 一个键**：上游 `HTTPClientOptions` 还带 engine/version/tls/headers/
/// dial 全套，但隐式回落等价的只有「走默认出站」这一条语义（`box.go` 的回落工厂只设
/// `DefaultOutbound = true`，其余全取核默认）。多写一个键就是多一条与核默认漂移的风险面，
/// 故此处只固定 detour，其余留给核。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpClient {
    /// 上游出站 tag。取 `route.final`（= 核的默认出站）以逐字保持隐式回落时的拨号路径。
    pub detour: String,
}
