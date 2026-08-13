//! 协议设置类型（上游 `shared/types/protocol-settings.ts` 1:1 移植）。
//!
//! 各协议专用 Settings struct，ServerConfig 按需引用。buildOutbounds 消费。
//! 纯类型，serde camelCase rename 对齐 Polaris JSON。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// TLS 设置。上游 `TlsSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsSettings {
    #[serde(rename = "serverName", skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    #[serde(rename = "allowInsecure", skip_serializing_if = "Option::is_none")]
    pub allow_insecure: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alpn: Option<Vec<String>>,
    /// uTLS 指纹（`chrome`/`firefox`/`none`/...）。取值集由 sing-box/uTLS 拥有且随版本增长
    /// → 保留 String + 边界归一（R4；上游 #298 修的正是此字段大小写）。
    #[serde(
        default,
        deserialize_with = "crate::user_config::normalize::de_opt_token",
        skip_serializing_if = "Option::is_none"
    )]
    pub fingerprint: Option<String>,
    /// ECH（隐藏 SNI）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ech: Option<bool>,
    #[serde(rename = "echConfig", skip_serializing_if = "Option::is_none")]
    pub ech_config: Option<String>,
    /// TLS ClientHello 分片（抗 SNI-DPI）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fragment: Option<bool>,
    /// TLS 栈引擎（go/windows/apple）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// TLS spoof 诱饵 SNI。
    #[serde(rename = "spoofSni", skip_serializing_if = "Option::is_none")]
    pub spoof_sni: Option<String>,
    #[serde(rename = "spoofMethod", skip_serializing_if = "Option::is_none")]
    pub spoof_method: Option<String>,
}

/// Reality 设置。上游 `RealitySettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealitySettings {
    #[serde(rename = "publicKey")]
    pub public_key: String,
    #[serde(rename = "shortId", skip_serializing_if = "Option::is_none")]
    pub short_id: Option<String>,
}

/// WebSocket 传输。上游 `WebSocketSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSocketSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(rename = "maxEarlyData", skip_serializing_if = "Option::is_none")]
    pub max_early_data: Option<u32>,
    #[serde(
        rename = "earlyDataHeaderName",
        skip_serializing_if = "Option::is_none"
    )]
    pub early_data_header_name: Option<String>,
}

/// gRPC 传输。上游 `GrpcSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrpcSettings {
    #[serde(rename = "serviceName", skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    #[serde(rename = "multiMode", skip_serializing_if = "Option::is_none")]
    pub multi_mode: Option<bool>,
}

/// HTTP 传输。上游 `HttpSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, Vec<String>>>,
}

/// Hysteria2 混淆设置。上游 `Hysteria2ObfsSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hysteria2ObfsSettings {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(rename = "minPacketSize", skip_serializing_if = "Option::is_none")]
    pub min_packet_size: Option<u32>,
    #[serde(rename = "maxPacketSize", skip_serializing_if = "Option::is_none")]
    pub max_packet_size: Option<u32>,
}

/// Hysteria2 设置。上游 `Hysteria2Settings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hysteria2Settings {
    #[serde(rename = "upMbps", skip_serializing_if = "Option::is_none")]
    pub up_mbps: Option<u32>,
    #[serde(rename = "downMbps", skip_serializing_if = "Option::is_none")]
    pub down_mbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub obfs: Option<Hysteria2ObfsSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(rename = "serverPorts", skip_serializing_if = "Option::is_none")]
    pub server_ports: Option<String>,
    #[serde(rename = "hopInterval", skip_serializing_if = "Option::is_none")]
    pub hop_interval: Option<String>,
    #[serde(rename = "bbrProfile", skip_serializing_if = "Option::is_none")]
    pub bbr_profile: Option<String>,
    /// 关闭 Chrome QUIC 握手拟态（sing-box 1.14.0-beta.7 outbound `disable_chrome_parrot`）。
    /// beta.7 起 Hysteria2 客户端默认拟态 Chrome 握手抗指纹识别，而 Chrome 不声明支持 Ed25519 ⇒
    /// **服务端证书是 Ed25519 的节点会直接握手失败**（用户只看到「连不上」）。这是该回归的逃生舱。
    /// 核心默认 false（拟态开）⇒ 仅 `Some(true)` 才下发键，`None`/`Some(false)` 都不发。
    #[serde(
        rename = "disableChromeParrot",
        skip_serializing_if = "Option::is_none"
    )]
    pub disable_chrome_parrot: Option<bool>,
}

/// Snell 版本（4=v4 obfs 系 / 6=v6 mode 系）。
pub type SnellVersion = u32;

/// Snell 设置。上游 `SnellSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnellSettings {
    pub version: SnellVersion,
    #[serde(rename = "obfsMode", skip_serializing_if = "Option::is_none")]
    pub obfs_mode: Option<String>,
    #[serde(rename = "obfsHost", skip_serializing_if = "Option::is_none")]
    pub obfs_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reuse: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub userkey: Option<String>,
}

/// Multiplex 设置。上游 `MultiplexSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiplexSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(rename = "maxConnections", skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
    #[serde(rename = "minStreams", skip_serializing_if = "Option::is_none")]
    pub min_streams: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding: Option<bool>,
}

/// TUIC 设置。上游 `TuicSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuicSettings {
    #[serde(rename = "congestionControl", skip_serializing_if = "Option::is_none")]
    pub congestion_control: Option<String>,
    #[serde(rename = "udpRelayMode", skip_serializing_if = "Option::is_none")]
    pub udp_relay_mode: Option<String>,
    #[serde(rename = "zeroRttHandshake", skip_serializing_if = "Option::is_none")]
    pub zero_rtt_handshake: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<String>,
}

/// Naive 设置。上游 `NaiveSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NaiveSettings {
    #[serde(rename = "useHttp3", skip_serializing_if = "Option::is_none")]
    pub use_http3: Option<bool>,
}

/// Shadowsocks 设置。上游 `ShadowsocksSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowsocksSettings {
    pub method: String,
    pub password: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(rename = "pluginOptions", skip_serializing_if = "Option::is_none")]
    pub plugin_opts: Option<String>,
}

/// AnyTLS 设置。上游 `AnyTlsSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnyTlsSettings {
    #[serde(
        rename = "idleSessionCheckInterval",
        skip_serializing_if = "Option::is_none"
    )]
    pub idle_session_check_interval: Option<String>,
    #[serde(rename = "idleSessionTimeout", skip_serializing_if = "Option::is_none")]
    pub idle_session_timeout: Option<String>,
    #[serde(rename = "minIdleSession", skip_serializing_if = "Option::is_none")]
    pub min_idle_session: Option<u32>,
}

/// Hysteria **v1** 设置（2026-08-11）。
///
/// 与 `Hysteria2Settings` 是**两个协议**而不是同一协议的版本字段：v1 的 `obfs` 是裸字符串、
/// 认证走 `auth_str`（或 base64 的 `auth`），且 `up_mbps`/`down_mbps` 是必填带宽语义。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HysteriaSettings {
    /// 明文认证串。与 `auth`（base64）二选一，两者都给时内核以 `auth_str` 为准。
    #[serde(rename = "authStr", skip_serializing_if = "Option::is_none")]
    pub auth_str: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(rename = "upMbps", skip_serializing_if = "Option::is_none")]
    pub up_mbps: Option<u32>,
    #[serde(rename = "downMbps", skip_serializing_if = "Option::is_none")]
    pub down_mbps: Option<u32>,
    /// v1 的 obfs 是**裸字符串**（混淆口令），不是 v2 的 `{type,password}` 对象。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub obfs: Option<String>,
    /// 端口跳跃范围（如 `20000:30000`）。**抗封锁常用**，故进表单而非透传袋。
    #[serde(rename = "serverPorts", skip_serializing_if = "Option::is_none")]
    pub server_ports: Option<String>,
    /// 跳跃间隔（如 `30s`）。同上。
    #[serde(rename = "hopInterval", skip_serializing_if = "Option::is_none")]
    pub hop_interval: Option<String>,
    /// **透传袋**：本结构未建模的键原样存放，生成时合并回下发内容。
    ///
    /// 表单是**精选子集**（内核这支有几十个键，多数是调优旋钮，铺成控件收益递减且有两类不该给：
    /// 格式苛刻的证书指纹、厂商专有的外部认证钩子）。没有本袋子时，「导入 → 编辑 → 保存」会把
    /// 未建模的键**静默丢掉** —— 配置从能连变成连不上且无任何提示。
    /// 有了它，可编辑与无损不再互斥，importer 才能安全映射到建模协议。
    /// 同型先例：`singbox::Outbound::extra` / `singbox::Endpoint::extra`。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 内嵌 Tor 设置（2026-08-11）。
///
/// ⚠️ **无 server/server_port**：实测给随包核传 `server` 得
/// `outbounds[0].server: json: unknown field "server"`。它自带 Tor 客户端
/// （随包二进制里 `cretz/bine` 与 `torrc` 符号共 320 处），与 `Tailscale` 同属「无地址协议」。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TorSettings {
    #[serde(rename = "executablePath", skip_serializing_if = "Option::is_none")]
    pub executable_path: Option<String>,
    #[serde(rename = "dataDirectory", skip_serializing_if = "Option::is_none")]
    pub data_directory: Option<String>,
    #[serde(rename = "extraArgs", default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    /// 直接注入 torrc 的键值对。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub torrc: std::collections::BTreeMap<String, String>,
    /// **透传袋**：本结构未建模的键原样存放，生成时合并回下发内容。
    ///
    /// 表单是**精选子集**（内核这支有几十个键，多数是调优旋钮，铺成控件收益递减且有两类不该给：
    /// 格式苛刻的证书指纹、厂商专有的外部认证钩子）。没有本袋子时，「导入 → 编辑 → 保存」会把
    /// 未建模的键**静默丢掉** —— 配置从能连变成连不上且无任何提示。
    /// 有了它，可编辑与无损不再互斥，importer 才能安全映射到建模协议。
    /// 同型先例：`singbox::Outbound::extra` / `singbox::Endpoint::extra`。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// OpenConnect 设置（2026-08-11）。
///
/// ⚠️ **本结构的 serde 名 = sing-box 的键名**：生成侧把它整体序列化后 flatten 进
/// `Endpoint::extra`，而不是给 wire struct 加 61 个具名字段。类型化留在这里，wire 保持精简。
/// 改名即改下发内容 —— 动字段名前先对随包核的 `$defs/Endpoint` 那支 `openconnect` 分支。
///
/// 一个类型覆盖六家商用 VPN，由 `flavor` 区分。**不进表单**的字段（`csd` / `hip` / `tncc` /
/// `fortinet_host_check` / `form_entries`）是各家专有的外部认证脚本钩子，语义强绑定厂商，
/// 做成通用控件只会误导 —— 需要它们的用户走 custom 直通。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenconnectSettings {
    /// `host:port` 单串（内核这支的 `server` 就是带端口的整串，不拆两个键）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// anyconnect(Cisco) / gp(GlobalProtect) / fortinet / f5 / pulse / nc(Juniper)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flavor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_udp: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pfs: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_insecure_crypto: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_os: Option<String>,
    /// 绑内核接口 —— 与 WG 的 `system:true` 同义，属「独立承载流量」语义键
    /// （已在 `CARRY_TRAFFIC_KEYS` 里）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<bool>,
    /// **透传袋**：本结构未建模的键原样存放，生成时合并回下发内容。
    ///
    /// 表单是**精选子集**（内核这支有几十个键，多数是调优旋钮，铺成控件收益递减且有两类不该给：
    /// 格式苛刻的证书指纹、厂商专有的外部认证钩子）。没有本袋子时，「导入 → 编辑 → 保存」会把
    /// 未建模的键**静默丢掉** —— 配置从能连变成连不上且无任何提示。
    /// 有了它，可编辑与无损不再互斥，importer 才能安全映射到建模协议。
    /// 同型先例：`singbox::Outbound::extra` / `singbox::Endpoint::extra`。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// OpenVPN 客户端设置（2026-08-11）。serde 名 = sing-box 键名，理由同 [`OpenconnectSettings`]。
///
/// `tls` **必填**：缺了内核判 `initialize endpoint[0]: missing \`tls\` options`。
/// `tls.peer_fingerprint` 也是合法材料但必须是**规范小写十六进制**（非规范格式内核硬报错），
/// 做成输入框会制造一类只有真连才暴露的错误 ⇒ 表单只给 `certificate`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenvpnClientSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// tcp / udp（内核键名就叫 `network`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cipher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    /// OpenVPN 的全隧道开关（等价 WG 的 `allowed_ips: 0.0.0.0/0`）。
    /// 已在 `CARRY_TRAFFIC_KEYS` 里 —— 见 `builder/endpoint_routes.rs`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_gateway: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<bool>,
    /// TLS 材料。**必填**，见结构头注。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<OpenvpnTlsSettings>,
    /// **透传袋**：本结构未建模的键原样存放，生成时合并回下发内容。
    ///
    /// 表单是**精选子集**（内核这支有几十个键，多数是调优旋钮，铺成控件收益递减且有两类不该给：
    /// 格式苛刻的证书指纹、厂商专有的外部认证钩子）。没有本袋子时，「导入 → 编辑 → 保存」会把
    /// 未建模的键**静默丢掉** —— 配置从能连变成连不上且无任何提示。
    /// 有了它，可编辑与无损不再互斥，importer 才能安全映射到建模协议。
    /// 同型先例：`singbox::Outbound::extra` / `singbox::Endpoint::extra`。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// OpenVPN 的 TLS 材料（内核 `$defs/OpenVPNOutboundTLSOptions` 的子集）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenvpnTlsSettings {
    /// CA 证书（PEM 逐行）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub certificate: Vec<String>,
    /// 双向认证：客户端证书与私钥。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_certificate: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_key: Vec<String>,
    /// 嵌套透传袋。父结构的袋子救不了这一层：`tls` 整体在父结构里算「已建模」而被排除出父袋，
    /// 但本结构只建模了三个子键 —— `peer_fingerprint` 之类会从**嵌套层**漏掉。
    /// 2026-08-11 由导入往返测试当场判红发现（内核唯一的 TLS 材料被丢 ⇒ 节点从能连变成起不来）。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// SSH 设置。上游 `SshSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(rename = "privateKey", skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    #[serde(rename = "privateKeyPath", skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
    #[serde(
        rename = "privateKeyPassphrase",
        skip_serializing_if = "Option::is_none"
    )]
    pub private_key_passphrase: Option<String>,
    #[serde(rename = "hostKey", skip_serializing_if = "Option::is_none")]
    pub host_key: Option<Vec<String>>,
    #[serde(rename = "hostKeyAlgorithms", skip_serializing_if = "Option::is_none")]
    pub host_key_algorithms: Option<Vec<String>>,
    #[serde(rename = "clientVersion", skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cipher: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<Vec<String>>,
    #[serde(rename = "kexAlgorithm", skip_serializing_if = "Option::is_none")]
    pub kex_algorithm: Option<Vec<String>>,
}

/// Shadow-TLS 插件设置。上游 `ShadowTlsSettings`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowTlsSettings {
    pub password: String,
    pub sni: String,
    /// uTLS 指纹。同 `TlsSettings::fingerprint`，边界归一（R4）。
    #[serde(
        default,
        deserialize_with = "crate::user_config::normalize::de_opt_token",
        skip_serializing_if = "Option::is_none"
    )]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// 自定义协议（raw-JSON 透传）。上游 `CustomSettings`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomSettings {
    pub outbound: serde_json::Value,
    #[serde(rename = "isEndpoint", skip_serializing_if = "Option::is_none")]
    pub is_endpoint: Option<bool>,
    #[serde(rename = "secretKeys", skip_serializing_if = "Option::is_none")]
    pub secret_keys: Option<Vec<String>>,
}

/// custom 逃生舱 outbound JSON 的**形状判据** —— probe 与生成路径的单一真值。
///
/// 返回 `Some(type)` ⟺ 它是一个带 string `type` 的 JSON 对象。
///
/// # 为什么必须共用一个谓词
///
/// C10「测试内核兼容性」按钮（`src-tauri/src/commands/proxy.rs`）把用户 JSON **原样**包成最小 config
/// 送 `sing-box check`；生成路径（`builder/outbound.rs` / `builder/outbounds.rs`）把**同一份** JSON
/// 原样下发。两条路径此前的判据不同——按钮走 raw `Value`、生成走窄 struct——于是按钮可以报
/// `{ok:true}` 而真正生成的运行配置根本不是同一个东西。透传修好之后，剩下的唯一判据就是本函数，
/// 两边同调它，分叉面为零。
///
/// **不校验 `type` 的取值**（空串也算合法形状）：「这个 type 内核认不认」是 `sing-box check` 的活，
/// 在此复刻一份协议白名单只会与内核版本漂移，且会把逃生舱重新变成白名单——那正是它要逃的东西。
#[must_use]
pub fn custom_outbound_type(outbound: &serde_json::Value) -> Option<&str> {
    outbound.as_object()?.get("type")?.as_str()
}

/// WARP 设备凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarpDevice {
    #[serde(rename = "deviceId")]
    pub device_id: String,
    pub token: String,
}
