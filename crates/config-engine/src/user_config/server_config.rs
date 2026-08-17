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
///
/// 🔴 **反序列化必须保持严格小写、不得加别名或大小写不敏感解析**（同文件的 [`SecurityMode`] 恰好是
/// 宽容解析的活先例，别照抄过来）。`polaris::runtime::proxy` 的 A4 早退闸有一条**廉价判据**
/// （`ProxyRuntime::selected_exit_is_tailscale`）绕过本类型、直接在原始 JSON 上比 `protocol ==
/// "tailscale"`；它与 `login_fallback_eligible` 的等价性正建立在「本类型只认小写」之上。一旦这里
/// 放宽，`"Tailscale"` 会变成 `eligible=true` 而廉价判据为假 ⇒ **engage 帧被闸吃掉**，未登录的
/// Tailscale 出口永远等不到让位。
///
/// 要改这里，先去改那条判据（并在其文档里对着改回来）。`protocol_deserialization_is_case_strict`
/// 是这条契约的绊线。
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
    /// WireGuard / P2P 流量的 UDP 监听端口（sing-box 1.14.0-beta.15 新增 `listen_port`）。
    ///
    /// 缺省由 tsnet 随机选。固定它才能在上游路由/防火墙上做端口映射 —— 这直接决定能不能与对端
    /// 直连打洞，还是恒回落 DERP 中继。与 [`Self::relay_server_port`] 是**两件事**：那个是本机作
    /// peer relay 时的**入站中继**监听口，本字段是自己这条 WireGuard 腿的出/入口。
    #[serde(rename = "listenPort", skip_serializing_if = "Option::is_none")]
    pub listen_port: Option<u16>,
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
///
/// # 为什么少数几个 `*_settings` 是 `Option<Box<T>>` 而其余是 `Option<T>`
///
/// 本结构体是**按值**装进 `UserConfig::servers: Vec<ServerConfig>` 的，故 `size_of::<Self>()`
/// 直接乘以节点数：每次 `from_value::<UserConfig>` 都要为这个 `Vec` 连续分配 `n × size_of`
/// （Vec 增长期峰值再翻一倍），每次 `ServerConfig::clone` 都要按这个宽度 memcpy。而 20+ 个协议
/// 设置子结构**全部内联**时，一个节点无论实际用哪个协议都得背上全部协议的宽度。
///
/// 装箱的判据是 **体积大 × 极少出现**（2026-08-17 实测 `size_of` 逐个量过，牙在本文件的
/// `server_config_stays_narrow`）：openvpnClient 280 B / ssh 264 B / openconnect 232 B /
/// [`WireGuardSettings`] 216 B / [`TailscaleSettings`] 192 B / hysteria2 184 B / hysteria 160 B /
/// tor 120 B —— 合计 1648 B，占装箱前 3096 B 的 53%，而它们对应的协议在真实配置里近乎不出现。
/// 装箱后各占 8 B，只有真正带该协议设置的节点才付一次堆分配。
/// 实测口径：3096 B → 1904 B（前六项）→ 1512 B（补 wireguard / tailscale 两项）。
///
/// **两者的出现率证据不同强度，分开记**（真机 60 节点配置实测均 0/60，但那只是一台机器一份配置）：
///
/// - `tailscaleSettings` 有**近似结构性的上限**：`store/src/sanitize.rs` 的 `first_tailscale`
///   只留第一个 tailscale 节点、其余整条剔除（前端 `tailscaleSlotTaken` 同源）。
///   ⚠️ 该闸**只按 `protocol == "tailscale"` 计数**，且同函数里那段 `tailscaleSettings` 清洗对**所有**
///   保留节点都跑、只洗 CIDR 不剥键 ⇒ 一个协议不是 tailscale 却挂着该键的脏节点既不占槽也不被清掉。
///   所以准确说法是「**tailscale 协议节点**恒 ≤ 1」，不是「该键的出现率恒 ≤ 1/n」。实践上后者仍成立
///   （没有写入路径会给非 TS 节点造这个键），但它是经验而非闸门保证。
///
/// - `wireguardSettings` **能被订阅量产，N 无界** —— 这条别再写成「量产腿产不出它」：
///   三条量产导入腿里，分享链接侧 `wireguard://` 不在 `is_supported_share_url`（实测）、Clash 侧
///   `clash_parser.rs` 恒填 `None`（WG 不在 Clash proxies 支持面），**但 sing-box JSON 订阅这条腿能**：
///   `subscription.rs` 的 `SingboxJson` 分支无条件把 `endpoints[]` 交给 `parse_singbox_endpoints`，
///   那里的 `"wireguard"` 臂**不看 `origin`**（隔壁 `openconnect`/`openvpn-client` 臂才有
///   `if origin != ImportOrigin::LocalFile` 闸），直接进 `map_wireguard_endpoint` 填本字段；而订阅刷新
///   正是以 `ImportOrigin::RemoteSubscription` 调进来的。前端批量准入 `meshSingletonConflict` 也只挡
///   WARP 与 tailscale，普通 WG **不占槽、无上限**。「机场下发 WireGuard 组网」本就是这条腿的立项理由
///   （见 `parse_subscription` 头注）。
///
/// **即便如此仍该装**，因为盈亏平衡点极高：WG 内联 216 B，装箱后在场节点付 `8 B + 224 B` glibc chunk
/// （`align16(216+8)`）= 232 B ⇒ 每节点亏 **16 B**；缺席节点省 `216−8 = 208 B` ⇒
/// `p* = 208/224 ≈ 92.9%`。也就是说**哪怕全库 100% 是 WG 节点，代价也只有 16 B/节点加一次 malloc**，
/// 而常见的「一个 WG 都没有」直接省 208 B/节点。TS 同法：chunk `align16(192+8)` = 208，
/// 在场亏 24 B、缺席省 184 B，`p* = 184/208 ≈ 88.5%`。判据的「罕见」在这两项上不是必要条件，
/// 只是把收益放大 —— 这与 `tlsSettings` 那种 `p*` 只有 87.5–95.5% 却实测 60/60 的情形是两回事。
///
/// **`tlsSettings` 刻意不装箱**（176 B）：它是**最常出现**的那个 —— 绝大多数 vless/trojan/vmess
/// 节点都带它（真机实测 60/60）。装箱后每个 TLS 节点省下 168 B 内联却多付 176 B 堆加一次 malloc，
/// 字节上近乎打平甚至更差，且它的调用面是全部协议设置里最大的一个（2026-08-17 `\btls_settings\b`
/// 全仓实测 80 处，次大的 wireguard 42 / tailscale 32）。判据是「大 × 罕见」，不是「大」。
/// 它在**本文件 + `protocol_settings.rs` 全部 21 个子结构**里排第 7，只在 `protocol_settings.rs`
/// 单独排序时才是第 5 —— 别引用后一个排名做取舍。装箱面补齐 WG/TS 后它仍是**最大的未装箱项**，
/// 也就是「为压数字而装箱」最诱人的那个目标，故门里那条内联保持断言钉的就是它。
///
/// ⚠️ **取样面教训**：wireguard / tailscale 是上一批**漏掉**的，不是权衡后不做 —— 那次按**定义所在
/// 模块**枚举候选，只扫了 `protocol_settings.rs` 里的子结构，而这两个定义在本文件，整个不在取样面内。
/// 后果是「判据没错、清单不全」：漏掉的两项比当时装的任何一个都大，也比刻意不装的 `tlsSettings` 大。
/// 下次增删协议设置时，候选面按**本结构体的字段**枚举，别按定义所在的模块。
///
/// 🔴 **按字段枚举一遍后，桌上还剩 384 B —— 本批没做，是独立立项，不是已补齐**
/// （2026-08-17 `size_of_val` 逐个实测，按宽度降序）：
///
/// | 未装箱字段 | 宽度 | 备注 |
/// |---|---|---|
/// | `tlsSettings` | 176 B | **刻意内联**，账见上文；不是候选 |
/// | `snellSettings` | **128 B** | 比本批之前就已装箱的 `torSettings`（120 B）**还大**，同样 0/60 |
/// | `httpSettings` | 104 B | |
/// | `shadowsocksSettings` | 96 B | |
/// | `wsSettings` | 88 B | |
/// | `tuicSettings` / `shadowTlsSettings` | 80 B | |
/// | 其余（custom 64 / anyTls 56 / multiplex 48 / reality 48 / grpc 32 / naive 1） | ≤64 B | 收益递减 |
///
/// 后四项里 `snell + http + shadowsocks + ws` 合计 416 B，装箱后 32 B ⇒ 可再省 **384 B/节点**，
/// 与本批省下的 392 B 同量级。**按同一条判据，`snellSettings` 严格优于本批之前已装的 `tor`**。
/// 之所以本批不顺手做：那是独立工作量（各自的调用面 + 各自的门），夹带会让这份 diff 失去可审性。
/// ⚠️ 这段前瞻清单**不许在补装后删掉改写成回顾**——上一批就是把它写成回顾性教训、前瞻内容随之消失，
/// 下一个人读到的是「清单已补齐」而不是「还有 384 B 在桌上」，于是同一根因连着复发了三次。
/// 补装某项时**只划掉那一行**，别动整段。
///
/// 装箱**不改变任何序列化产物**：`Box<T>` 的 `Serialize`/`Deserialize` 逐字转发给 `T`，
/// `skip_serializing_if = "Option::is_none"` 语义不变，`Debug`/`PartialEq` 同样转发。
///
/// ⚠️ **钉住这件事的只有本文件那条 `boxed_protocol_settings_serialize_transparently`**，
/// 别以为「反正还有几道既有门兜着」就可以删它 —— 2026-08-17 逐条实测过那三道对**八个**装箱键的射程：
/// `tests/serde_roundtrip.rs` 全文件只碰 `singbox::*`，`UserConfig`/`ServerConfig` **零出现**；
/// `tests/user_config_key_contract.rs` 的夹具是 `"servers": []`，八个键 **0 命中**；
/// `tests/golden_config_snapshot.rs` 的 `fixtures/config-snapshot.json`（37 case）只有
/// `hysteria2Settings` × 1 / `sshSettings` × 1 / `wireguardSettings` × 1，
/// 另外五个键（含 `tailscaleSettings`）**0 命中**。
/// 即：两道射程为零、一道覆盖 8 个里的 3 个，且那一道只走**生成侧**（UserConfig → sing-box 配置），
/// 磁盘往返与订阅导入导出这两条腿一条都不碰。删掉本文件那条 = 保护归零。
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
    pub hysteria2_settings: Option<Box<crate::user_config::protocol_settings::Hysteria2Settings>>,
    #[serde(rename = "tuicSettings", skip_serializing_if = "Option::is_none")]
    pub tuic_settings: Option<crate::user_config::protocol_settings::TuicSettings>,
    /// Hysteria **v1**（与 `hysteria2_settings` 是两个协议，不是同一协议的版本字段）。
    #[serde(rename = "hysteriaSettings", skip_serializing_if = "Option::is_none")]
    pub hysteria_settings: Option<Box<crate::user_config::protocol_settings::HysteriaSettings>>,
    /// 内嵌 Tor（无 server/port）。
    #[serde(rename = "torSettings", skip_serializing_if = "Option::is_none")]
    pub tor_settings: Option<Box<crate::user_config::protocol_settings::TorSettings>>,
    /// OpenConnect（端点族，非组网）。
    #[serde(
        rename = "openconnectSettings",
        skip_serializing_if = "Option::is_none"
    )]
    pub openconnect_settings:
        Option<Box<crate::user_config::protocol_settings::OpenconnectSettings>>,
    /// OpenVPN 客户端（端点族，非组网）。
    #[serde(
        rename = "openvpnClientSettings",
        skip_serializing_if = "Option::is_none"
    )]
    pub openvpn_client_settings:
        Option<Box<crate::user_config::protocol_settings::OpenvpnClientSettings>>,
    #[serde(rename = "wireguardSettings", skip_serializing_if = "Option::is_none")]
    pub wireguard_settings: Option<Box<WireGuardSettings>>,
    #[serde(rename = "tailscaleSettings", skip_serializing_if = "Option::is_none")]
    pub tailscale_settings: Option<Box<TailscaleSettings>>,
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
    pub ssh_settings: Option<Box<crate::user_config::protocol_settings::SshSettings>>,
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

    /// 🔴 **绊线：`Protocol` 的反序列化必须严格小写**（见该枚举文档的跨 crate 契约）。
    ///
    /// 守的不是本 crate 的行为，而是 `polaris::runtime::proxy` 那条绕过本类型、直接在原始 JSON 上
    /// 比 `"tailscale"` 的 A4 早退闸廉价判据。给本类型加宽容解析/别名，会让 `"Tailscale"` 这类写法
    /// 变成「完整判据说符合、廉价判据说不符合」⇒ engage 帧被早退闸吃掉，未登录的 TS 出口永不让位。
    /// 那条链路上**没有任何测试会因此转红**（等价性测试只喂现存形态），所以绊线必须落在这里。
    ///
    /// **变异探针**：给 `Protocol` 换成 `SecurityMode` 那样的大小写不敏感 `Deserialize` 手写实现
    /// （或加 `#[serde(alias = "Tailscale")]`）⇒ 本条转红。
    #[test]
    fn protocol_deserialization_is_case_strict() {
        assert_eq!(
            serde_json::from_value::<Protocol>(serde_json::json!("tailscale")).unwrap(),
            Protocol::Tailscale,
            "线上字面量恒为小写（`rename_all = \"lowercase\"`）"
        );
        for wrong in ["Tailscale", "TAILSCALE", "TailScale"] {
            assert!(
                serde_json::from_value::<Protocol>(serde_json::json!(wrong)).is_err(),
                "`{wrong}` 必须解析失败 —— 一旦被接受，proxy.rs 那条按 `\"tailscale\"` 比对的 A4 \
                 早退闸就会静默假阴性，engage 帧被吃掉（见 Protocol 的文档注释）"
            );
        }
        // 同文件的宽容解析先例就在隔壁，正向对照一下**它确实宽容** —— 免得日后 `SecurityMode` 也收严
        // 时，上面那组断言被误读成「本文件一律严格」而顺手推广。
        assert_eq!(
            serde_json::from_value::<SecurityMode>(serde_json::json!("REALITY")).unwrap(),
            SecurityMode::from_raw("reality"),
            "对照：SecurityMode 是大小写不敏感的（两者刻意不同口径，别统一）"
        );
    }

    // ── 装箱协议设置的两道门（结构体头注给的是判据，这里是它的牙）──────────────

    /// 🟡 **宽度门**：`ServerConfig` 是**按值**装进 `UserConfig::servers: Vec<ServerConfig>` 的，
    /// 它的 `size_of` 直接乘节点数 —— 每次 `from_value::<UserConfig>` 都要为那个 `Vec` 连续分配
    /// `n × size_of`（增长期峰值再翻一倍），每次 `ServerConfig::clone` 都按这个宽度 memcpy。
    /// 于是「又内联进来一个 200 B 的协议设置」这件事的代价是 `200 × 节点数`，而它**不会让任何
    /// 行为测试转红**：序列化产物一个字节不变、全仓功能照常。本门就是补这个盲区。
    ///
    /// 基线（2026-08-17 实测）：装箱前 3096 B，把 8 个「体积大 × 极少出现」的协议设置改
    /// `Option<Box<T>>` 后 1512 B（分两批落地：前 6 项 → 1904 B，补 wireguard / tailscale
    /// 两项再省 `(216−8)+(192−8) = 392 B` → 1512 B）；按真机 119 节点算，单次反序列化的 `Vec`
    /// 底层分配 368,424 B → 179,928 B。
    ///
    /// **红了不等于有 bug，红了等于「该看一眼」**。字段尺寸之和 = 1508 B，`size_of` = 1512 B ⇒
    /// **仍只剩 4 B 尾隙**（装箱不改变尾隙：换掉的两个字段本就 8 字节对齐）。所以两种情形都会
    /// 让它红，处方**完全相反**，先分清是哪一种：
    ///
    /// - **本仓有意新增了一个 ≥8 B 的标量 / 字符串字段**（`meshRoutes` / `disableChromeParrot`
    ///   就是这个形态，也是本结构体最常见的演进方式）：4 B 尾隙吃不下，**必红**。
    ///   2026-08-17 逐档实测：`Option<u32>`（8 B）⇒ 1512 → **1520 红**；
    ///   `Option<String>`（24 B）⇒ **1536 红**。
    ///   处方是**重新实测 `size_of` 并连同日期更新常量** —— 不是把那个 24 B 的 String 装箱
    ///   （那毫无意义，只多一次 malloc）。工具链换版改了布局而本仓一字未动，同理。
    ///   ⚠️ **≤4 B 的小标量塞得进尾隙，门会保持绿，这不是漏网**：同批实测
    ///   `bool` / `Option<bool>`（1 B）与 `Option<u16>`（4 B）加进来后 `size_of` 仍是 1512。
    ///   那 4 B 是真空位（当前 `protocol` 1 + `port` 2 + `naive_settings` 1 = 4 B 已占的另一半），
    ///   而 `Option<u16>` 不是假想形态 —— `TailscaleSettings` 里就有两个端口字段是它。
    ///   这类字段本就**不在本门射程内**：本门量的是「按节点数放大的宽度」，4 B 塞进既有空位
    ///   等于零成本，没有可报的事。
    /// - **有人又内联了一个大结构体**（几十上百 B 的协议设置）：这才是本门要拦的那一类。
    ///   按结构体头注的判据（体积大 × 罕见）决定它该不该装箱，**不得为了让它过门而调大常量**。
    ///
    /// 只在 64 位靶子上断言：指针宽度直接进 `Option<Box<_>>` / `String` / `Vec` 的尺寸，32 位上
    /// 这个常量没有意义。本仓四条打包腿（mac-arm64 / mac-x64 / linux-x64 / win-x64）全是 64 位。
    ///
    /// # 为什么还有第二条断言（上界单独一条是**会奖励错误做法**的）
    ///
    /// 上界只守判据的「大」那一半：把 `tlsSettings` 装箱能让 `size_of` 掉到 1344 B，上界**更绿** ——
    /// 而按判据那恰恰不该做（它 60/60 出现，装箱等于每个节点多一次 malloc 换字节打平）。
    /// 只有上界的门不但不拦这种「压数字」的改法，还给它发绿灯。故第二条钉住高频字段**保持内联**。
    ///
    /// # 为什么第二条只钉 `tlsSettings` 一个字段
    ///
    /// 2026-08-17 补装 WG/TS 后重新问过「要不要扩到别的字段」，结论是**不扩**，两条理由：
    ///
    /// ① **手上的样本面撑不起第二条禁令**。同一份 60 节点真机配置实测：`tlsSettings` 60/60、
    /// `realitySettings` 30/60、`naiveSettings` 30/60、其余全 0/60。数是有的，但那是**一台机器
    /// 一份配置**；把某字段的**内联**冻成禁令，需要的证据强度高于「这份样本里它常见」。
    /// `tlsSettings` 是唯一一个 60/60 且有独立机制解释（几乎所有 vless/trojan/vmess 都带 TLS）的，
    /// 才配得上一条门。
    ///
    /// ② **最像候选的那个算下来其实边缘可做**。`realitySettings` 48 B，glibc 下 48 B 载荷落 64 B
    /// chunk：装箱后在场节点 `8 + 64 = 72 B` vs 内联 48 B ⇒ 每节点亏 24 B，缺席省 40 B，
    /// `p* = 40/64 = 62.5%`；实测 50% ⇒ 装箱它在字节上**是净赚的**，只是多一次 malloc。
    /// 也就是说它是**边缘可做**而非明确不该做 —— 写进断言等于用一条门把一个边缘取舍冻成禁令。
    ///
    /// 要扩这条断言，先把出现率样本面补厚（多机器 / 多订阅源），别照着 `size_of` 排序扩。
    ///
    /// **变异探针**：把任一 `Option<Box<T>>` 改回 `Option<T>` ⇒ 上界转红；
    /// 反过来把 `tls_settings` 改成 `Option<Box<TlsSettings>>` ⇒ 上界照绿（1344 ≤ 1512），
    /// 由第二条转红接住。
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn server_config_stays_narrow() {
        /// 2026-08-17 实测值（装箱前 3096 B；只装前 6 项时 1904 B）。
        const MEASURED: usize = 1512;
        let actual = std::mem::size_of::<ServerConfig>();
        assert!(
            actual <= MEASURED,
            "size_of::<ServerConfig>() = {actual} B > {MEASURED} B。\
             若这是本仓有意新增的小标量/字符串字段 ⇒ 重新实测并连同日期更新 MEASURED；\
             若是又内联了一个大结构体 ⇒ 按 ServerConfig 头注的判据（大 × 罕见）装箱它，\
             不得为此调大常量。两者的分辨方法见本测试的文档注释。"
        );

        // 「罕见」那一半：高频字段必须保持内联，不得靠装箱它们来压上面那个数字。
        // `Option<Box<_>>` 恒为 8 B（指针 + niche），故 `> 8` 精确等价于「没被装箱」，
        // 且不随子结构增删字段漂移 —— 不写死 176 就是为了不要一条会自己过期的断言。
        let s = ServerConfig::default();
        assert!(
            std::mem::size_of_val(&s.tls_settings) > 8,
            "tlsSettings 被装箱了 —— 它 60/60 出现，装箱只是把内联宽度换成每节点一次 malloc，\
             字节上打平甚至更差。上界断言看不见这件事（size 反而变小 ⇒ 更绿），故由本条接住。\
             判据是「体积大 × 极少出现」，不是「体积大」。"
        );
    }

    /// 🔴 **透明门**：`Option<Box<T>>` 的序列化产物必须与 `Option<T>` **逐字节相同**。
    ///
    /// 这是宽度门成立的前提：装箱只是内存布局手术，磁盘配置、订阅导入导出、下发给内核的载荷
    /// 一个字节都不该变。裸 `Box<T>` 的 `Serialize`/`Deserialize` 确实逐字转发给 `T` —— 但那是
    /// **当前**这个类型的性质，不是本仓写下的契约。若日后有人把它换成自己的包装类型（挂懒加载 /
    /// 版本迁移 / 引用计数），转发就没了，磁盘上的配置会**静默**多出一层 `{"inner": …}`：
    /// 老配置读不回来，新配置内核不认，而这条链路上没有任何行为测试会因此转红。
    ///
    /// 断言逐键写死（而非 round-trip 自证）：round-trip 对「两侧一起改坏」是瞎的。
    ///
    /// ⚠️ **首条断言的射程比标题大**：它是**整份 JSON 相等**，于是顺带钉住了 `ServerConfig`
    /// **全部**字段的 skip 行为。将来有人加一个不带 `skip_serializing_if` 的字段，红的会是这一条，
    /// 而它的失败文案说的是装箱 —— 指向错误方向。文案里已就此留了分辨提示。
    ///
    /// **变异探针**：给任一装箱字段套一层非 `transparent` 的 newtype、或删掉它的
    /// `skip_serializing_if` / `rename` ⇒ 下面的整份 JSON 断言转红。
    #[test]
    fn boxed_protocol_settings_serialize_transparently() {
        use crate::user_config::protocol_settings as ps;
        let s = ServerConfig {
            id: "s1".into(),
            name: "n1".into(),
            protocol: Protocol::Vless,
            address: "a.com".into(),
            port: 443,
            hysteria2_settings: Some(Box::new(ps::Hysteria2Settings {
                up_mbps: Some(50),
                ..Default::default()
            })),
            hysteria_settings: Some(Box::new(ps::HysteriaSettings {
                auth_str: Some("hy1".into()),
                ..Default::default()
            })),
            tor_settings: Some(Box::new(ps::TorSettings {
                executable_path: Some("/usr/bin/tor".into()),
                ..Default::default()
            })),
            openconnect_settings: Some(Box::new(ps::OpenconnectSettings {
                server: Some("vpn.example.com:443".into()),
                ..Default::default()
            })),
            openvpn_client_settings: Some(Box::new(ps::OpenvpnClientSettings {
                server_port: Some(1194),
                ..Default::default()
            })),
            ssh_settings: Some(Box::new(ps::SshSettings {
                private_key: Some("KEY".into()),
                ..Default::default()
            })),
            // WG/TS 各多填一个 `Vec` 字段，钉住装箱后**数组仍是数组**、不多包一层。
            // 注意这一态给不了 `skip_serializing_if = "Vec::is_empty"` 任何牙（这里它们恒非空）
            // —— 那半边由下面第三态接住。
            wireguard_settings: Some(Box::new(WireGuardSettings {
                private_key: Some("PRIV".into()),
                allowed_ips: vec!["0.0.0.0/0".into()],
                ..Default::default()
            })),
            tailscale_settings: Some(Box::new(TailscaleSettings {
                auth_key: Some("tskey-auth-X".into()),
                advertise_routes: vec!["192.168.1.0/24".into()],
                ..Default::default()
            })),
            ..Default::default()
        };
        let v = serde_json::to_value(&s).expect("节点应可序列化");
        assert_eq!(
            v,
            serde_json::json!({
                "id": "s1",
                "name": "n1",
                "protocol": "vless",
                "address": "a.com",
                "port": 443,
                "hysteria2Settings": { "upMbps": 50 },
                "hysteriaSettings": { "authStr": "hy1" },
                "torSettings": { "executablePath": "/usr/bin/tor" },
                "openconnectSettings": { "server": "vpn.example.com:443" },
                "openvpnClientSettings": { "server_port": 1194 },
                "sshSettings": { "privateKey": "KEY" },
                "wireguardSettings": { "privateKey": "PRIV", "allowedIPs": ["0.0.0.0/0"] },
                "tailscaleSettings": {
                    "authKey": "tskey-auth-X",
                    "advertiseRoutes": ["192.168.1.0/24"]
                },
            }),
            "装箱字段的键名/嵌套形状必须与未装箱时逐字节一致 —— 多一层包装 = 老配置读不回来。\
             ⚠️ 本条是整份 JSON 相等，射程覆盖全部字段：若红在一个**新增字段**上（左侧多出一个键），\
             那不是装箱问题 —— 先确认该字段该不该带 `skip_serializing_if`，再更新下面的期望 JSON。"
        );
        let back: ServerConfig = serde_json::from_value(v).expect("应能读回");
        assert_eq!(back, s, "反序列化侧同样必须透明");
        // 缺席仍不进 JSON：装箱不该让 `skip_serializing_if = "Option::is_none"` 失效
        // （`Option<Box<T>>` 的 `is_none` 与 `Option<T>` 同义，但这条得有牙）。
        let bare = ServerConfig {
            id: "s2".into(),
            name: "n2".into(),
            protocol: Protocol::Vless,
            address: "b.com".into(),
            port: 443,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&bare).expect("节点应可序列化"),
            serde_json::json!({
                "id": "s2", "name": "n2", "protocol": "vless", "address": "b.com", "port": 443
            }),
            "八个装箱字段缺席时一个键都不该出现"
        );
        // 第三态：装箱字段**在场、但内容全缺省**。前两态都盖不到它，而它才是两个谓词分岔的地方：
        // 字段级的 `Option::is_none` 只看**字段在不在**、与内容无关；一旦有人把它换成内容相关的
        // 谓词（图省事写成「空对象就别发了」），一个只建了没填的节点就会**静默丢键**，
        // 而前两态一条都不红。顺带这也是子结构里那些 `skip_serializing_if = "Vec::is_empty"`
        // 唯一有牙的一态 —— 满字段态里那些 `Vec` 恒非空，碰不到该谓词。
        //
        // 🔴 **八个装箱字段一个都不能少**：本态的判据是「字段级谓词是否与内容无关」，那是**每个**
        // 装箱字段各自的属性，不是可以抽样的共性。少写一个，同一个变异换到那个字段上就一态不红。
        // 本态初版只放了 wireguard/tailscale 两个（补装它俩那批顺手加的），另外六个在三态里的形态
        // 是「填了个标量 / 缺席 / 缺席」—— 恰好绕开本态要拦的那件事，等于门只补到 2/8。
        // 期望值 `{}` 是**实测**来的（八个结构 `Default` 逐个序列化确认），不是「反正全带
        // skip_serializing_if 所以应该是空」的推断：哪天有人给某个结构加一个不带 skip 的必填字段，
        // 该改的是这里的期望值，而本条会先红出来提醒。
        let empty_boxed = ServerConfig {
            id: "s3".into(),
            name: "n3".into(),
            protocol: Protocol::Wireguard,
            address: "c.com".into(),
            port: 51820,
            hysteria2_settings: Some(Box::new(ps::Hysteria2Settings::default())),
            hysteria_settings: Some(Box::new(ps::HysteriaSettings::default())),
            tor_settings: Some(Box::new(ps::TorSettings::default())),
            openconnect_settings: Some(Box::new(ps::OpenconnectSettings::default())),
            openvpn_client_settings: Some(Box::new(ps::OpenvpnClientSettings::default())),
            ssh_settings: Some(Box::new(ps::SshSettings::default())),
            wireguard_settings: Some(Box::new(WireGuardSettings::default())),
            tailscale_settings: Some(Box::new(TailscaleSettings::default())),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&empty_boxed).expect("节点应可序列化"),
            serde_json::json!({
                "id": "s3", "name": "n3", "protocol": "wireguard",
                "address": "c.com", "port": 51820,
                "hysteria2Settings": {}, "hysteriaSettings": {}, "torSettings": {},
                "openconnectSettings": {}, "openvpnClientSettings": {}, "sshSettings": {},
                "wireguardSettings": {}, "tailscaleSettings": {}
            }),
            "在场的装箱字段即使内容全缺省，键也必须在、值恰为 `{{}}` —— 空对象与缺席是两回事：\
             真机上「新建了 TS 节点还没填任何设置」就是这个形态（`tailscaleSettings: {{}}`），\
             丢了它，节点回读时会当成从没配过。子结构里的 `Vec` 字段也不得因为空就冒出 `[]`。"
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
