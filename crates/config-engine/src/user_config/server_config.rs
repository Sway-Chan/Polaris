//! ServerConfig 节点配置（上游 `shared/types.ts ServerConfig` 子集）。
//!
//! 增量定义：仅 endpoint/mesh 相关字段（WG/Tailscale endpoint 路由用）。
//! 协议设置子类型最小投影。随 builder 移植扩展。

#![forbid(unsafe_code)]

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::user_config::normalize::normalize_token;

/// 传输层安全模式（上游 `Security = 'none' | 'tls' | 'reality'`，上游 `shared/types.ts:129`）。
///
/// **为什么是枚举而不是 `Option<String>`**（落地要求 R3）：
/// 裸串 + 严格比较（`security.as_deref() == Some("tls")`）下，任何写入路径塞进 `"TLS"` /
/// `"Reality"` / `"tls "` 变体都会让分支静默不命中 → **TLS/Reality 不启用、无任何报错，
/// 用户以为加密实际明文出站**。这是本类型存在的唯一理由。
///
/// 归一只发生在反序列化边界一次（[`SecurityMode::from_raw`]），之后类型系统保证
/// 不可能再出现大小写变体 —— 不依赖后人记得调归一函数。
///
/// **为什么这个字段类型化、而 `fingerprint`/`flow` 不**：本字段取值集由 Polaris 自身闭合
/// （`none|tls|reality`），类型化无上游漂移风险；且它的误判是**静默**的（分支不命中，
/// sing-box 根本看不到意图）。反观 `fingerprint`/`flow` 取值集由 sing-box 拥有且开放，
/// 且实测误判即 `FATAL`（fail-closed）→ 保留 String + 边界归一即可，见 [`normalize`]。
///
/// [`normalize`]: crate::user_config::normalize
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityMode {
    /// 不启用传输层安全。
    None,
    Tls,
    Reality,
    /// 未知值（订阅脏数据 / 未来新增模式）：保留原文，语义按「非 TLS、非 Reality」处理。
    ///
    /// **刻意不报错**：单个脏字段不应让整个节点反序列化失败而从列表里消失。
    Unknown(String),
}

impl SecurityMode {
    /// 边界归一：trim + ASCII 小写后匹配；未知值保留 trim 后原文。
    ///
    /// 空/缺省视作 [`SecurityMode::None`]（未设置 ≡ 不启用），与订阅里 `security: ""` 的实际语义一致。
    pub fn from_raw(raw: &str) -> Self {
        match normalize_token(raw).as_deref() {
            None | Some("none") => Self::None,
            Some("tls") => Self::Tls,
            Some("reality") => Self::Reality,
            Some(_) => Self::Unknown(raw.trim().to_string()),
        }
    }

    /// 规范文本表示（序列化用）。未知值原样吐回 → 往返不丢用户数据。
    pub fn as_str(&self) -> &str {
        match self {
            Self::None => "none",
            Self::Tls => "tls",
            Self::Reality => "reality",
            Self::Unknown(s) => s,
        }
    }

    /// 是否启用 TLS。**判定唯一入口** —— 禁止在别处写 `== "tls"` 字符串比较。
    pub fn is_tls(&self) -> bool {
        matches!(self, Self::Tls)
    }

    /// 是否启用 Reality。**判定唯一入口** —— 禁止在别处写 `== "reality"` 字符串比较。
    pub fn is_reality(&self) -> bool {
        matches!(self, Self::Reality)
    }
}

impl Serialize for SecurityMode {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SecurityMode {
    /// 大小写不敏感反序列化。
    ///
    /// 不用 `#[serde(rename_all = "lowercase")]`（只管序列化方向，反序列化仍严格匹配），
    /// 也不用 `#[serde(alias)]` 穷举（`"reality"` 需 2^7=128 条别名，不可维护）。
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        Ok(Self::from_raw(&String::deserialize(de)?))
    }
}

/// 节点协议（上游 `Protocol`，子集——仅当前 builder 所需 + endpoint 全集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Vless,
    Trojan,
    Hysteria2,
    Shadowsocks,
    Anytls,
    Tuic,
    Vmess,
    Naive,
    Snell,
    Socks,
    Http,
    Ssh,
    Wireguard,
    Tailscale,
    // ── 2026-08-11 补：随包核支持而本仓此前无表单的三个出站 ──
    // 判据是「随包核 check 收不收」而不是「schema 里有没有」——`sing-box generate schema` 实测
    // 漏了 `snell`（它接受 snell 出站），故全集从实测来。
    //
    // ⚠️ **`shadowtls` 不在这里，且不是遗漏**：它在本仓是 shadowsocks 的**插件设置**
    // （`ShadowTlsSettings`），生成侧自动造外层 `stls-out-<id>` 出站并把主出站的 detour 指过去
    // （`builder/outbounds.rs` 的 Shadow-TLS 后处理段）。那才是它的正确形态 —— 它是传输层不是出口，
    // 建成独立协议只会让用户建完选中它、然后握手得上却出不去网。
    // 判「支不支持」的判据是**生成侧能不能产出该 outbound type**，不是「协议白名单里有没有它」。
    /// Hysteria **v1**（与既有 `Hysteria2` 是两个协议，不是版本字段）。
    Hysteria,
    /// 内嵌 Tor 客户端：**无 server/port**（实测传 `server` 得 `unknown field "server"`），
    /// 与 `Tailscale` 同属「无地址协议」。
    Tor,
    // ── 端点族 VPN 客户端（2026-08-11）──
    // 二者在内核里属 `$defs/Endpoint`，塞进 `outbounds[]` 会 `unknown outbound type`（实测）
    // ⇒ 进 [`lands_in_endpoints`]。
    //
    // 是否进 [`is_mesh_protocol`] 则**不由协议决定，由节点决定**（见 `is_mesh_node`）：那条判据是
    // 「配置期能否声明可达网段」，而这两个协议的网段是服务端在隧道建立后 push 的，配置期不可知。
    // 用户在 `meshRoutes` 里显式声明了段，该节点才具备组网能力。
    // 2026-08-13 前这里写的是「语义上是普通 VPN 出口，不是组网」—— 那是不可验证的主观表述，
    // 已换成上面这条代码可推、可写门的判据。
    /// OpenConnect：一个类型覆盖六家商用 VPN，由 `flavor` 区分
    /// （anyconnect / gp / fortinet / f5 / pulse / nc）。
    Openconnect,
    /// OpenVPN 客户端。`tls` 是**必填**——缺了内核判 `initialize endpoint[0]: missing \`tls\` options`。
    /// 只做 client；server 端不做（用户裁定）。
    ///
    /// **本变体是全枚举里唯一需要 per-variant rename 的**：枚举级 `rename_all = "lowercase"` 会把它
    /// 折成 `openvpnclient`，而**内核类型名、store 白名单、UI `NodeProto` 三处都是 `openvpn-client`**。
    /// 少了这行的后果实测过，两个方向都是用户可见故障：
    /// · UI 建的节点写 `"openvpn-client"` → `UserConfig` 反序列化 `unknown variant` →
    ///   **整份配置解析失败**（不是丢这一个节点，是全部节点连同设置一起没了）；
    /// · 导入侧产出序列化成 `"openvpnclient"` → 不在 `ALLOWED_PROTOCOLS` 里 → sanitize **静默丢节点**。
    /// `alias` 收下折叠拼法，让任何已经落过盘的旧值仍读得回来。
    /// 三处登记表的一致性由 `crates/store/tests/protocol_registries_agree.rs` 钉住。
    #[serde(rename = "openvpn-client", alias = "openvpnclient")]
    OpenvpnClient,
    Custom,
}

/// WireGuard 设置（上游 `WireGuardSettings`）。sing-box 1.11+ endpoint，默认 gVisor 用户态。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireGuardSettings {
    #[serde(rename = "privateKey", skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    #[serde(
        rename = "localAddress",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub local_address: Vec<String>,
    #[serde(rename = "peerPublicKey", skip_serializing_if = "Option::is_none")]
    pub peer_public_key: Option<String>,
    #[serde(rename = "preSharedKey", skip_serializing_if = "Option::is_none")]
    pub pre_shared_key: Option<String>,
    #[serde(rename = "allowedIPs", default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_ips: Vec<String>,
    #[serde(rename = "allowInternet", skip_serializing_if = "Option::is_none")]
    pub allow_internet: Option<bool>,
    #[serde(rename = "alwaysRouteSubnets", skip_serializing_if = "Option::is_none")]
    pub always_route_subnets: Option<bool>,
    #[serde(
        rename = "persistentKeepalive",
        skip_serializing_if = "Option::is_none"
    )]
    pub persistent_keepalive: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reserved: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    #[serde(rename = "reverseMesh", skip_serializing_if = "Option::is_none")]
    pub reverse_mesh: Option<bool>,
    #[serde(rename = "warpDevice", skip_serializing_if = "Option::is_none")]
    pub warp_device: Option<crate::user_config::protocol_settings::WarpDevice>,
}

/// Tailscale 设置（上游 `TailscaleSettings`）。账号制 mesh，sing-box endpoint。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailscaleSettings {
    #[serde(rename = "authKey", skip_serializing_if = "Option::is_none")]
    pub auth_key: Option<String>,
    #[serde(rename = "allowInternet", skip_serializing_if = "Option::is_none")]
    pub allow_internet: Option<bool>,
    #[serde(rename = "alwaysRouteSubnets", skip_serializing_if = "Option::is_none")]
    pub always_route_subnets: Option<bool>,
    #[serde(rename = "exitNode", skip_serializing_if = "Option::is_none")]
    pub exit_node: Option<String>,
    #[serde(
        rename = "exitNodeAllowLanAccess",
        skip_serializing_if = "Option::is_none"
    )]
    pub exit_node_allow_lan_access: Option<bool>,
    #[serde(rename = "acceptRoutes", skip_serializing_if = "Option::is_none")]
    pub accept_routes: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,
    #[serde(rename = "controlUrl", skip_serializing_if = "Option::is_none")]
    pub control_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    #[serde(
        rename = "advertiseRoutes",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub advertise_routes: Vec<String>,
    #[serde(rename = "reverseMesh", skip_serializing_if = "Option::is_none")]
    pub reverse_mesh: Option<bool>,
    #[serde(
        rename = "advertiseTags",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub advertise_tags: Vec<String>,
    #[serde(rename = "sshServer", skip_serializing_if = "Option::is_none")]
    pub ssh_server: Option<bool>,
    #[serde(rename = "relayServerPort", skip_serializing_if = "Option::is_none")]
    pub relay_server_port: Option<u16>,
    #[serde(rename = "resolveByName", skip_serializing_if = "Option::is_none")]
    pub resolve_by_name: Option<bool>,
    #[serde(
        rename = "acceptDefaultResolvers",
        skip_serializing_if = "Option::is_none"
    )]
    pub accept_default_resolvers: Option<bool>,
}

/// 节点配置（上游 `ServerConfig` 全字段）。buildOutbounds 消费。
/// CustomSettings 含 serde_json::Value（非 Eq）→ 不 derive Eq。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    /// 🔴 `default` 不可去（2026-07-31 真机阻断级缺陷）：**账号制协议本就没有服务器地址**。
    ///
    /// 契约在 `crates/store/src/sanitize.rs:271` —— 那里 `tailscale` / `custom` **豁免**
    /// address/port 校验、有意保留这类节点；其余协议缺 address 或 port∉1..=65535 直接剔除。
    /// 此处若必填，就是在下一层用**协议盲**的判据把上一层特意放行的东西再拒一次，两层契约打架。
    ///
    /// 后果不是「那个节点用不了」而是整机不可用：`proxy_start` 反序列化的是**整份 UserConfig**，
    /// 一个无 address 的 TS 节点 ⇒ `missing field \`address\`` ⇒ 127 个节点的配置全体解析失败 ⇒
    /// 连接按钮恒失败。真机日志实证：`[home] connect toggle failed: 配置解析失败（UserConfig）:
    /// missing field \`address\``，而磁盘上那个 TS 节点的键只有
    /// id/name/protocol/tailscaleSettings/createdAt/updatedAt。
    ///
    /// 「那非账号协议缺 address 岂不静默变空串」—— 那道门没丢，只是留在 sanitize（它知道 protocol，
    /// 这里不知道）。**别把它挪回来**：挪回来就重现本缺陷。
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub port: u16,
    /// 代理链（前置代理）ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detour: Option<String>,
    /// 用户声明的「经该节点可达的内网段」（CIDR）。**仅 endpoint 腿的 VPN 客户端
    /// （openconnect / openvpn-client）读它**，是这两个协议获得组网资格的唯一途径
    /// （见 [`is_mesh_node`]）。
    ///
    /// # 为什么它们需要用户手填，而 WG/TS 不用
    ///
    /// WireGuard 的段是用户填的 `allowedIPs`、Tailscale 的是协议固定的 tailnet 段 —— 生成配置那一刻
    /// 就已知。OpenVPN / OpenConnect 的段由**服务端在隧道建立后 push**，配置期不可知，内核侧对应的
    /// 是 `redirect_private` / `route_no_pull` 这类「要不要收下服务端下发的路由」的开关，不是网段本身。
    /// 所以「连公司 VPN，只走公司网段，其余直连」这个用法，WG 用户填个 allowedIPs 就有，而这两个协议
    /// 此前只能去规则页手写 CIDR 指向该节点。本字段补的就是这条不对称。
    ///
    /// # 为什么在 ServerConfig 顶层而不在各自的 settings 结构里
    ///
    /// `OpenconnectSettings` / `OpenvpnClientSettings` 的 **serde 名 = sing-box 键名**，整体序列化后
    /// flatten 进 `Endpoint::extra` 下发。往里加一个内核不认识的键 = 给内核发未知字段（实测硬报错），
    /// 且会破坏那两个结构写在头注里的既定契约。顶层字段不进下发载荷，`detour` 是同型先例。
    ///
    /// 与 WG 的 `allowedIPs` 有一处**语义差别**：`allowedIPs` 兼任栈内 cryptokey 过滤（不在表里的包
    /// 被丢），两处生效；本字段只喂 `route.rules`，OpenVPN/OpenConnect 客户端侧没有对应的过滤层。
    #[serde(rename = "meshRoutes", default, skip_serializing_if = "Vec::is_empty")]
    pub mesh_routes: Vec<String>,
    #[serde(rename = "subscriptionId", skip_serializing_if = "Option::is_none")]
    pub subscription_id: Option<String>,
    #[serde(rename = "providerName", skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    // VLESS
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encryption: Option<String>,
    /// XTLS flow（`xtls-rprx-vision` 等）。取值集由 sing-box 拥有 → 保留 String，边界归一。
    #[serde(
        default,
        deserialize_with = "crate::user_config::normalize::de_opt_token",
        skip_serializing_if = "Option::is_none"
    )]
    pub flow: Option<String>,
    #[serde(rename = "packetEncoding", skip_serializing_if = "Option::is_none")]
    pub packet_encoding: Option<String>,
    // Trojan/Hysteria2 通用
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    // Naive
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(rename = "naiveSettings", skip_serializing_if = "Option::is_none")]
    pub naive_settings: Option<crate::user_config::protocol_settings::NaiveSettings>,
    // VMess
    #[serde(rename = "alterId", skip_serializing_if = "Option::is_none")]
    pub alter_id: Option<u32>,
    /// VMess 加密方式（`auto`/`aes-128-gcm`/...）。sing-box 拥有取值集 → String + 边界归一。
    #[serde(
        rename = "vmessSecurity",
        default,
        deserialize_with = "crate::user_config::normalize::de_opt_token",
        skip_serializing_if = "Option::is_none"
    )]
    pub vmess_security: Option<String>,
    // 协议设置子结构
    #[serde(rename = "hysteria2Settings", skip_serializing_if = "Option::is_none")]
    pub hysteria2_settings: Option<crate::user_config::protocol_settings::Hysteria2Settings>,
    #[serde(rename = "tuicSettings", skip_serializing_if = "Option::is_none")]
    pub tuic_settings: Option<crate::user_config::protocol_settings::TuicSettings>,
    /// Hysteria **v1**（与 `hysteria2_settings` 是两个协议，不是同一协议的版本字段）。
    #[serde(rename = "hysteriaSettings", skip_serializing_if = "Option::is_none")]
    pub hysteria_settings: Option<crate::user_config::protocol_settings::HysteriaSettings>,
    /// 内嵌 Tor（无 server/port）。
    #[serde(rename = "torSettings", skip_serializing_if = "Option::is_none")]
    pub tor_settings: Option<crate::user_config::protocol_settings::TorSettings>,
    /// OpenConnect（端点族，非组网）。
    #[serde(
        rename = "openconnectSettings",
        skip_serializing_if = "Option::is_none"
    )]
    pub openconnect_settings: Option<crate::user_config::protocol_settings::OpenconnectSettings>,
    /// OpenVPN 客户端（端点族，非组网）。
    #[serde(
        rename = "openvpnClientSettings",
        skip_serializing_if = "Option::is_none"
    )]
    pub openvpn_client_settings:
        Option<crate::user_config::protocol_settings::OpenvpnClientSettings>,
    #[serde(rename = "wireguardSettings", skip_serializing_if = "Option::is_none")]
    pub wireguard_settings: Option<WireGuardSettings>,
    #[serde(rename = "tailscaleSettings", skip_serializing_if = "Option::is_none")]
    pub tailscale_settings: Option<TailscaleSettings>,
    #[serde(rename = "customSettings", skip_serializing_if = "Option::is_none")]
    pub custom_settings: Option<crate::user_config::protocol_settings::CustomSettings>,
    #[serde(rename = "anyTlsSettings", skip_serializing_if = "Option::is_none")]
    pub any_tls_settings: Option<crate::user_config::protocol_settings::AnyTlsSettings>,
    #[serde(rename = "multiplexSettings", skip_serializing_if = "Option::is_none")]
    pub multiplex_settings: Option<crate::user_config::protocol_settings::MultiplexSettings>,
    #[serde(
        rename = "shadowsocksSettings",
        skip_serializing_if = "Option::is_none"
    )]
    pub shadowsocks_settings: Option<crate::user_config::protocol_settings::ShadowsocksSettings>,
    #[serde(rename = "snellSettings", skip_serializing_if = "Option::is_none")]
    pub snell_settings: Option<crate::user_config::protocol_settings::SnellSettings>,
    #[serde(rename = "sshSettings", skip_serializing_if = "Option::is_none")]
    pub ssh_settings: Option<crate::user_config::protocol_settings::SshSettings>,
    #[serde(rename = "shadowTlsSettings", skip_serializing_if = "Option::is_none")]
    pub shadow_tls_settings: Option<crate::user_config::protocol_settings::ShadowTlsSettings>,
    // 传输层
    /// 传输层类型（`tcp`/`ws`/`grpc`/`http`/`h2`/`httpupgrade`）。R3 覆盖项 → 边界归一。
    /// 未归一时 `"WS"` 会走到 `generate_transport_config` 的 `_ => None` 分支静默丢传输层。
    #[serde(
        default,
        deserialize_with = "crate::user_config::normalize::de_opt_token",
        skip_serializing_if = "Option::is_none"
    )]
    pub network: Option<String>,
    /// 传输层安全模式。类型化根治静默 TLS/Reality 降级，见 [`SecurityMode`]。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<SecurityMode>,
    #[serde(rename = "tlsSettings", skip_serializing_if = "Option::is_none")]
    pub tls_settings: Option<crate::user_config::protocol_settings::TlsSettings>,
    #[serde(rename = "realitySettings", skip_serializing_if = "Option::is_none")]
    pub reality_settings: Option<crate::user_config::protocol_settings::RealitySettings>,
    #[serde(rename = "wsSettings", skip_serializing_if = "Option::is_none")]
    pub ws_settings: Option<crate::user_config::protocol_settings::WebSocketSettings>,
    #[serde(rename = "grpcSettings", skip_serializing_if = "Option::is_none")]
    pub grpc_settings: Option<crate::user_config::protocol_settings::GrpcSettings>,
    #[serde(rename = "httpSettings", skip_serializing_if = "Option::is_none")]
    pub http_settings: Option<crate::user_config::protocol_settings::HttpSettings>,
    // 元数据
    #[serde(rename = "createdAt", skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(rename = "updatedAt", skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// 组网协议。上游 `isEndpointProtocol`（本仓改名，理由见下）。
///
/// # 判据：配置期就能声明可达网段
///
/// 命中者，生成侧能在**生成配置的那一刻**为它发 force-route 规则，让它的网段常驻可达：
/// WireGuard 的段是用户填的 `allowedIPs`，Tailscale 的是协议固定的 tailnet 两族段 + `routes`。
/// 判据的唯一实现是 [`crate::builder::endpoint_routes::endpoint_forced_route_cidrs`]，
/// 那个函数有来源的协议就该在这里命中，没有的就不该 —— 两者由 `mesh_protocol_matches_cidr_source`
/// 对拍，加协议时漏改一边即红。
///
/// # 这**不是**「落在 `endpoints[]` 里的协议」
///
/// 那是内核的数据模型形态，openconnect / openvpn-client 同样落在 `endpoints[]`（塞
/// `outbounds[]` 得 `unknown outbound type`，实测），但它们的网段由服务端在隧道建立后 push，
/// 配置期不可知 ⇒ 不属本判据。**这个函数从前叫 `is_endpoint_protocol`，名字说的是数据模型、
/// 成员集给的是组网 —— 两者不重合的那两个协议上，消费点按名字选谓词就选错了，实际造成过三处缺陷**
/// （临时测速核把它们塞进 `outbounds[]` 致整核 FATAL、detour 指向它们成悬空引用、
/// 承流播种漏掉它们致该重启时不重启）。要判数据模型形态用 [`lands_in_endpoints`]。
pub fn is_mesh_protocol(p: Protocol) -> bool {
    matches!(p, Protocol::Wireguard | Protocol::Tailscale)
}

/// 落 sing-box 顶层 `endpoints[]`（而非 `outbounds[]`）的协议 —— **内核的数据模型形态**。
///
/// 与 [`is_mesh_protocol`] 是两件事：那个判「能不能声明网段」（产品能力），这个判「JSON 该塞哪个数组」
/// （内核形态）。四个协议命中，前两个两者皆是，后两个只是形态。
///
/// 射程：`custom` 协议的 endpoint 腿（`customSettings.isEndpoint`）也落 `endpoints[]`，但那要看
/// 节点的设置而非协议，本函数看不到 ⇒ 调用点若需覆盖它，须自行并上那一支（`speedtest.rs` 的
/// `build_temp_node` 就是先判 custom-endpoint 再走本判据）。
pub fn lands_in_endpoints(p: Protocol) -> bool {
    matches!(
        p,
        Protocol::Wireguard | Protocol::Tailscale | Protocol::Openconnect | Protocol::OpenvpnClient
    )
}

/// 该**节点**是否具备组网能力 —— [`is_mesh_protocol`] 的节点级形态。
///
/// 判据仍是「配置期能否声明可达网段」，只是对 openconnect / openvpn-client 而言，这件事由**用户填没填
/// [`ServerConfig::mesh_routes`]** 决定，不由协议决定：填了，生成侧就能为它发 force-route 规则，它
/// 与一个填了 `allowedIPs` 的 WireGuard 节点在路由上再无分别；没填，它就只是个普通出口。
///
/// 分组、force-route 发射、热切换判定这些**看能力**的消费点用本函数；判 JSON 该塞哪个数组用
/// [`lands_in_endpoints`]；只在拿不到整个节点时才退回 [`is_mesh_protocol`]。
pub fn is_mesh_node(s: &ServerConfig) -> bool {
    is_mesh_protocol(s.protocol)
        || (matches!(s.protocol, Protocol::Openconnect | Protocol::OpenvpnClient)
            && s.mesh_routes.iter().any(|c| !c.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_protocol_classification() {
        assert!(is_mesh_protocol(Protocol::Wireguard));
        assert!(is_mesh_protocol(Protocol::Tailscale));
        assert!(!is_mesh_protocol(Protocol::Vless));
        assert!(!is_mesh_protocol(Protocol::Trojan));
    }

    /// 判据分离后二者必须是**真子集**关系：组网 ⊂ endpoint 腿。
    /// 全协议逐条归档见 `crates/store/tests/protocol_registries_agree.rs`
    /// （那边的变体清单有源码对差的完整性门，不必在此再写第二份）。
    #[test]
    fn mesh_protocols_are_a_strict_subset_of_the_endpoint_leg() {
        for p in [Protocol::Wireguard, Protocol::Tailscale] {
            assert!(is_mesh_protocol(p) && lands_in_endpoints(p), "{p:?}");
        }
        for p in [Protocol::Openconnect, Protocol::OpenvpnClient] {
            assert!(!is_mesh_protocol(p) && lands_in_endpoints(p), "{p:?}");
        }
    }

    /// endpoint 腿的 VPN 客户端：**声明了内网段才算组网节点**。
    ///
    /// 这条是「组网资格由能力而非协议决定」的唯一判据。空白项不算声明 —— 表单里删干净一行留下的
    /// 空串若算数，用户就会得到一个「是组网但没有任何网段」的节点，分组进组网页签却什么都路由不了。
    #[test]
    fn endpoint_leg_vpn_is_a_mesh_node_only_when_it_declares_routes() {
        let mk = |proto: Protocol, routes: Vec<String>| ServerConfig {
            id: "x".into(),
            protocol: proto,
            mesh_routes: routes,
            ..Default::default()
        };
        for proto in [Protocol::Openconnect, Protocol::OpenvpnClient] {
            assert!(!is_mesh_node(&mk(proto, vec![])), "{proto:?} 未声明");
            assert!(
                !is_mesh_node(&mk(proto, vec!["  ".into()])),
                "{proto:?} 只有空白项 —— 不算声明"
            );
            assert!(
                is_mesh_node(&mk(proto, vec!["10.0.0.0/8".into()])),
                "{proto:?} 已声明"
            );
        }
        // 组网协议与 meshRoutes 无关：WG/TS 的段有自己的来源。
        assert!(is_mesh_node(&mk(Protocol::Wireguard, vec![])));
        assert!(is_mesh_node(&mk(Protocol::Tailscale, vec![])));
        // 普通出站协议即使被塞了 meshRoutes 也不是组网节点（那个字段对它无意义）。
        assert!(!is_mesh_node(&mk(
            Protocol::Vless,
            vec!["10.0.0.0/8".into()]
        )));
    }

    #[test]
    fn server_config_deserialize() {
        let json =
            r#"{"id":"s1","name":"HK","protocol":"wireguard","address":"1.2.3.4","port":443}"#;
        let s: ServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(s.protocol, Protocol::Wireguard);
        assert!(s.wireguard_settings.is_none());
    }

    /// 🔴 账号制节点（tailscale）磁盘上就是没有 address/port —— 键名逐字取自 2026-07-31 真机
    /// `config.json`。把 `#[serde(default)]` 去掉 ⇒ 本条报 `missing field \`address\``。
    #[test]
    fn account_based_node_without_address_or_port_deserializes() {
        let json = r#"{"id":"802f47bd-8c91-47a3-97f6-6ab38964ac20","name":"Sway-Tailscale",
                       "protocol":"tailscale","tailscaleSettings":{},
                       "createdAt":"2026-06-19T17:31:35.564Z","updatedAt":"2026-06-28T07:01:40.490Z"}"#;
        let s: ServerConfig = serde_json::from_str(json).expect("账号制节点必须能反序列化");
        assert_eq!(s.protocol, Protocol::Tailscale);
        assert_eq!(s.address, "");
        assert_eq!(s.port, 0);
    }

    /// 🔴 真正的爆炸半径：整份 `servers[]` 里**一个**无 address 的节点，不许把其余节点一起带走。
    /// 这是真机症状「connect toggle failed: 配置解析失败（UserConfig）」的最小复现 ——
    /// 127 个节点里只有一个 TS 节点缺字段，结果整份配置解析失败、连接按钮恒失败。
    #[test]
    fn one_account_based_node_does_not_break_the_whole_server_list() {
        let json = r#"[
            {"id":"a","name":"VLESS","protocol":"vless","address":"1.2.3.4","port":443},
            {"id":"ts1","name":"Tailscale","protocol":"tailscale","tailscaleSettings":{}},
            {"id":"b","name":"WG","protocol":"wireguard","address":"5.6.7.8","port":51820}
        ]"#;
        let list: Vec<ServerConfig> = serde_json::from_str(json).expect("一个节点不许拖垮整表");
        assert_eq!(list.len(), 3);
        assert_eq!(list[1].address, "");
        // 反向对照：其余节点的 address 必须**原样保留**，不能被 default 抹平 ——
        // 否则这条 default 就从「容忍缺席」滑成「静默丢值」。
        assert_eq!(list[0].address, "1.2.3.4");
        assert_eq!(list[2].port, 51820);
    }

    // ── SecurityMode 归一（R3）──────────────────────────────────────────────
    // 锁死事故形态：大小写变体必须归一到同一枚举，否则 TLS/Reality 静默不启用。

    #[test]
    fn security_tls_case_variants_all_normalize() {
        for raw in ["tls", "TLS", "Tls", "tLs", " tls ", "\tTLS\n"] {
            assert_eq!(
                SecurityMode::from_raw(raw),
                SecurityMode::Tls,
                "{raw:?} 必须归一为 Tls"
            );
            assert!(
                SecurityMode::from_raw(raw).is_tls(),
                "{raw:?} is_tls 必须真"
            );
        }
    }

    #[test]
    fn security_reality_case_variants_all_normalize() {
        for raw in ["reality", "REALITY", "Reality", "ReAlItY", "  Reality  "] {
            assert_eq!(
                SecurityMode::from_raw(raw),
                SecurityMode::Reality,
                "{raw:?} 必须归一为 Reality"
            );
            assert!(SecurityMode::from_raw(raw).is_reality());
        }
    }

    #[test]
    fn security_none_variants_and_empty() {
        for raw in ["none", "NONE", "None", "", "   "] {
            assert_eq!(SecurityMode::from_raw(raw), SecurityMode::None, "{raw:?}");
        }
        // none 既非 tls 也非 reality。
        assert!(!SecurityMode::None.is_tls());
        assert!(!SecurityMode::None.is_reality());
    }

    #[test]
    fn security_unknown_preserved_and_not_tls() {
        // 脏值/未来模式：保留原文（往返不丢），语义按非 TLS 处理，且**不报错**。
        let m = SecurityMode::from_raw(" xtls ");
        assert_eq!(m, SecurityMode::Unknown("xtls".into()));
        assert_eq!(m.as_str(), "xtls");
        assert!(!m.is_tls(), "未知值不得被当作 TLS");
        assert!(!m.is_reality());
    }

    #[test]
    fn security_deserialize_is_case_insensitive() {
        let json = r#"{"id":"s1","name":"HK","protocol":"vless","address":"a.com","port":443,"security":"TLS"}"#;
        let s: ServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(s.security, Some(SecurityMode::Tls));
    }

    #[test]
    fn security_dirty_value_does_not_fail_whole_node() {
        // 回归：单个脏 security 不得让整个节点反序列化失败（否则节点从列表消失）。
        let json = r#"{"id":"s1","name":"HK","protocol":"vless","address":"a.com","port":443,"security":"bogus-mode"}"#;
        let s: ServerConfig = serde_json::from_str(json).expect("脏 security 不得导致解析失败");
        assert_eq!(s.security, Some(SecurityMode::Unknown("bogus-mode".into())));
        assert_eq!(s.name, "HK", "其余字段必须完好");
    }

    #[test]
    fn security_serialize_is_canonical_lowercase() {
        // "TLS" 存入 → 序列化出 "tls"（归一后写回，消除存量变体）。
        let json = r#"{"id":"s1","name":"HK","protocol":"vless","address":"a.com","port":443,"security":"Reality"}"#;
        let s: ServerConfig = serde_json::from_str(json).unwrap();
        let out = serde_json::to_value(&s).unwrap();
        assert_eq!(out["security"], serde_json::json!("reality"));
    }

    #[test]
    fn security_unknown_roundtrips_verbatim() {
        let json = r#"{"id":"s1","name":"HK","protocol":"vless","address":"a.com","port":443,"security":"xtls"}"#;
        let s: ServerConfig = serde_json::from_str(json).unwrap();
        let out = serde_json::to_value(&s).unwrap();
        assert_eq!(out["security"], serde_json::json!("xtls"), "未知值往返不丢");
    }

    #[test]
    fn security_absent_stays_absent() {
        let json = r#"{"id":"s1","name":"HK","protocol":"vless","address":"a.com","port":443}"#;
        let s: ServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(s.security, None);
        let out = serde_json::to_value(&s).unwrap();
        assert!(out.get("security").is_none(), "未设置不得凭空出现");
    }

    // ── R4 指纹 / flow / network / vmessSecurity 边界归一 ────────────────────

    #[test]
    fn r4_tokens_normalized_at_deserialize() {
        let json = r#"{"id":"s1","name":"HK","protocol":"vless","address":"a.com","port":443,
            "flow":"XTLS-RPRX-Vision","network":"WS","vmessSecurity":"AES-128-GCM",
            "tlsSettings":{"fingerprint":"Chrome"}}"#;
        let s: ServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(s.flow.as_deref(), Some("xtls-rprx-vision"));
        assert_eq!(s.network.as_deref(), Some("ws"));
        assert_eq!(s.vmess_security.as_deref(), Some("aes-128-gcm"));
        assert_eq!(
            s.tls_settings.unwrap().fingerprint.as_deref(),
            Some("chrome")
        );
    }

    #[test]
    fn r4_absent_token_fields_stay_none() {
        // 回归 serde 陷阱：deserialize_with 会吃掉 Option 的隐式缺键行为，靠 `default` 兜住。
        let json = r#"{"id":"s1","name":"HK","protocol":"vless","address":"a.com","port":443}"#;
        let s: ServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(s.flow, None);
        assert_eq!(s.network, None);
        assert_eq!(s.vmess_security, None);
    }
}
