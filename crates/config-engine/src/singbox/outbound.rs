//! sing-box outbound 类型（`singbox-config-types.ts:116-241 SingBoxOutbound`）。
//! 覆盖所有协议字段（SS/VLESS/VMess/Trojan/Hysteria2/TUIC/Naive/Snell/AnyTLS/SSH/WireGuard/selector）。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

use super::dns::{DomainResolver, OneOrMany};

/// sing-box outbound `version` 字段动态类型：ShadowTLS/Snell 用裸数字（3/4/6），
/// SOCKS 用字符串（上游 `singbox-outbound-builder.ts:381` `version = '5'`）。
/// Polaris 为 `number | string` 动态类型，此处对齐——序列化为裸 JSON 值（数字 / 带引号字符串）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutboundVersion {
    Num(u32),
    Str(String),
}

/// `outbounds[]`（统一 struct，所有协议字段 Optional——不同协议填不同子集）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outbound {
    #[serde(rename = "type")]
    pub type_field: String,
    pub tag: String,
    /// 代理链（前置代理 tag）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detour: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub override_address: Option<String>,
    // Shadowsocks
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_opts: Option<String>,
    // VLESS / VMess
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alter_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packet_encoding: Option<String>,
    // Hysteria2 specific
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up_mbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub down_mbps: Option<u32>,
    /// Hysteria **v1 是字符串、v2 是对象** —— 同名不同型，故用 untagged 枚举容纳两者。
    /// `Object` 分支的序列化与旧的 `Option<Hysteria2Obfs>` **逐字节相同**（untagged 不加判别键），
    /// 故金样零影响；`Text` 分支是 2026-08-11 补 hysteria v1 时新开的。
    /// 本 struct 头注早已把「建模过但类型不同 ⇒ 整个反序列化失败」列为 custom 逃生舱存在的理由之一，
    /// 举的例子正是这个 `obfs` —— 现在 v1 进了建模协议，那条理由对它不再适用。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub obfs: Option<ObfsField>,
    /// Hysteria **v1** 的明文认证串。本 struct 头注把它列为「没建模 ⇒ 静默丢失（连不上，
    /// 但配置看起来是好的）」的实例；2026-08-11 v1 进建模协议，故补上。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_str: Option<String>,
    /// ── 内嵌 tor 的四键（2026-08-11）──
    /// 同样出自头注的「没建模 ⇒ 静默丢失」清单。tor **没有 server/server_port**
    /// （实测传 `server` 得 `unknown field "server"`），故本组是它仅有的可配面。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torrc: Option<std::collections::BTreeMap<String, String>>,
    /// Hysteria2 BBR 拥塞控制 profile（1.14）：standard/aggressive/conservative。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbr_profile: Option<String>,
    /// Hysteria2 关闭 Chrome QUIC 握手拟态（1.14.0-beta.7 新增 `disable_chrome_parrot`）。
    ///
    /// beta.7 起客户端**默认**拟态 Chrome 的 QUIC 握手（抗指纹识别），而 Chrome 不声明支持 Ed25519 ⇒
    /// 服务端用 Ed25519 证书时握手必然失败，用户侧只表现为「连不上」。此开关是那条回归的唯一逃生舱。
    ///
    /// 核心默认值 = `false`（拟态开），故只有用户显式打开时才下发 `true`；`None` ⇒ 整键不出现，
    /// 存量配置字节不变（金样 `config-snapshot.json` 因此零 diff）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_chrome_parrot: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// naive specific: HTTP/3 (QUIC) 传输。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quic: Option<bool>,
    // TUIC specific
    #[serde(skip_serializing_if = "Option::is_none")]
    pub congestion_control: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub udp_relay_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zero_rtt_handshake: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<String>,
    // ShadowTLS / Snell / SOCKS 共用：协议版本号（snell 4|6 数字、shadowtls 3 数字、socks "5" 字符串）。
    // Polaris 同字段动态 number|string（outbound-builder.ts:270 数字 / :381 字符串），故用枚举对齐。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<OutboundVersion>,
    // Snell specific（1.14.0-alpha.38+ 官方 outbound；无 TLS 块）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub psk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub userkey: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reuse: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub obfs_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub obfs_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    // AnyTLS specific
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_session_check_interval: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_session_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_idle_session: Option<u32>,
    // HTTP 出站的**顶层**伪装键（`path` / `headers`）。
    //
    // 🔴 不要挪进 [`Transport`]：随包 sing-box 1.14.0-beta.7 的 **http 出站 schema 没有 `transport`
    // 键**，且该支 `additionalProperties:false` ⇒ 一旦下发就是
    // `FATAL decode config: outbounds[0].transport: json: unknown field "transport"`（rc=1，整份
    // 配置起不来）。schema 原文（`sing-box schema` → `$defs/Outbound/oneOf[4]` type=http 那支）里
    // 这两键就在**出站顶层**：`"path": {"type":"string"}`、`"headers": {"$ref":"#/$defs/HTTPHeader"}`。
    //
    // `headers` 值类型对齐 `$defs/HTTPHeader` = `map<string, string | string[]>`，故用 [`OneOrMany`]。
    //
    // **`host` / `method` 刻意不建模**：那两键在内核 http 出站 schema 里压根不存在，写顶层同样
    // `unknown field`（实测 rc=1）。建了就是假字段——UI 填了只会造出起不来的节点。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, OneOrMany<String>>>,
    // TLS
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<OutboundTls>,
    // Transport
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    // Multiplex
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiplex: Option<Multiplex>,
    // Hysteria2 端口跳跃
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_ports: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hop_interval: Option<String>,
    /// DNS resolver for outbound server domain（dial 侧）。
    ///
    /// 类型是 [`DomainResolver`] 而非 `String`：纯 tag 会整个覆盖掉 `route.default_domain_resolver`
    /// 而**不继承**其 strategy，节点域名因此被顶层 `ipv4_only` 卡死（#335）。因果链与 loopback
    /// 对照实验见 [`DomainResolver`] 的类型注释。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_resolver: Option<DomainResolver>,
    // UDP over TCP (UoT)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub udp_over_tcp: Option<UdpOverTcp>,
    /// Direct outbound: UDP fragmentation（亦标记 outbound 非空以满足 1.13+ 校验）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub udp_fragment: Option<bool>,
    // SSH specific
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key_passphrase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_key: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_key_algorithms: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    /// SSH 算法协商（1.14）：cipher / mac / kex_algorithm。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cipher: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kex_algorithm: Option<Vec<String>>,
    // selector specific（clash_api 热切换：default=当前选中）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outbounds: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interrupt_exist_connections: Option<bool>,
    /// **custom 逃生舱的原样透传载荷**（15 个建模协议一律留空 ⇒ 不产生任何键）。
    ///
    /// # 为什么需要它
    ///
    /// custom 分支的契约是「用户给什么就下发什么」（上游 `{...userOutbound, tag}`，运行时零约束）。
    /// 把 raw JSON `serde_json::from_value` 成本 struct 会把这条契约悄悄换成「只下发本 struct 建模过的
    /// 那些具名字段」。实测（`builder/outbound.rs` 的四组变异锁单测逐条钉住）三种坏法：
    ///  - **建模过但类型不同** —— hysteria v1 的 `obfs` 是字符串，本 struct 是 [`Hysteria2Obfs`] 对象
    ///    ⇒ **整个反序列化失败** ⇒ 回落成 `{"type":"custom","tag":…}` 空壳，而随包 sing-box
    ///    1.14.0-beta.7 对它的判决是 `unknown outbound type: custom`（rc=1，整份配置起不来）；
    ///  - **没建模** —— tor 的 `executable_path`/`data_directory`/`extra_args`/`torrc` 四键 **静默丢失**；
    ///  - **没建模** —— hysteria v1 的 `auth_str` **静默丢失**（连不上，但配置「看起来是好的」）。
    ///
    /// 故 custom 分支**不再走 `from_value`**：`type` 进 `type_field`、`tag` 由调用方覆盖、其余键原封
    /// 不动装进本字段，序列化时 flatten 回顶层 ⇒ 下发内容与用户输入逐键一致。
    ///
    /// # 为什么金样零影响
    ///
    /// 建模协议的构造点一律留空 map ⇒ flatten 不产生任何键；`flatten` 让 derive 从 `serialize_struct`
    /// 换成 `serialize_map`，对 serde_json 的输出字节等价。这条不是推断——`tests/serde_roundtrip.rs`
    /// 的逐字符断言（`{"type":"direct","tag":"direct"}`）与 `tests/golden_config_snapshot.rs` 37 例
    /// 全量对拍就是它的门。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `obfs` 的两种线上形态：hysteria v1 是裸字符串，v2 是 `{type,password}` 对象。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ObfsField {
    Text(String),
    Object(Hysteria2Obfs),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hysteria2Obfs {
    #[serde(rename = "type")]
    pub type_field: String,
    pub password: String,
    /// gecko obfs 随机填充包长（1.14）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_packet_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_packet_size: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboundTls {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insecure: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alpn: Option<Vec<String>>,
    /// TLS 栈引擎（1.14）：go（默认/省略）/ windows(Schannel) / apple(Network.framework)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// TLS spoof（1.14 抗审查）：spoof=伪造 ClientHello SNI；spoof_method=wrong-ack/wrong-md5/wrong-timestamp。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spoof: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spoof_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utls: Option<Utls>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reality: Option<Reality>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ech: Option<Ech>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fragment: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Utls {
    pub enabled: bool,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reality {
    pub enabled: bool,
    pub public_key: String,
    pub short_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ech {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transport {
    #[serde(rename = "type")]
    pub type_field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<OneOrMany<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, OneOrMany<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_early_data: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub early_data_header_name: Option<String>,
}

impl Outbound {
    /// 构造仅含 type+tag 的空壳，其余字段全 None（shadow-tls 后处理等场景用）。
    pub fn shell(type_field: &str, tag: &str) -> Self {
        Self {
            type_field: type_field.to_string(),
            tag: tag.to_string(),
            detour: None,
            server: None,
            server_port: None,
            override_address: None,
            method: None,
            password: None,
            username: None,
            plugin: None,
            plugin_opts: None,
            uuid: None,
            security: None,
            alter_id: None,
            flow: None,
            packet_encoding: None,
            up_mbps: None,
            down_mbps: None,
            obfs: None,
            auth_str: None,
            executable_path: None,
            data_directory: None,
            extra_args: None,
            torrc: None,
            bbr_profile: None,
            disable_chrome_parrot: None,
            network: None,
            quic: None,
            congestion_control: None,
            udp_relay_mode: None,
            zero_rtt_handshake: None,
            heartbeat: None,
            version: None,
            psk: None,
            userkey: None,
            reuse: None,
            obfs_mode: None,
            obfs_host: None,
            mode: None,
            idle_session_check_interval: None,
            idle_session_timeout: None,
            min_idle_session: None,
            path: None,
            headers: None,
            tls: None,
            transport: None,
            multiplex: None,
            server_ports: None,
            hop_interval: None,
            domain_resolver: None,
            udp_over_tcp: None,
            udp_fragment: None,
            user: None,
            private_key: None,
            private_key_path: None,
            private_key_passphrase: None,
            host_key: None,
            host_key_algorithms: None,
            client_version: None,
            cipher: None,
            mac: None,
            kex_algorithm: None,
            outbounds: None,
            default: None,
            interrupt_exist_connections: None,
            extra: serde_json::Map::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Multiplex {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_streams: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UdpOverTcp {
    pub enabled: bool,
    pub version: u32,
}
