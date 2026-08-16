//! sing-box endpoint 类型（`singbox-config-types.ts:243-295`）。
//! WireGuard + Tailscale（1.11+ 顶层 endpoints[]，tag 可被 route/selector 引用）。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

use super::dns::DomainResolver;

/// `endpoints[]`（WireGuard / Tailscale 共用 struct，按 type 区分填字段）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    #[serde(rename = "type")]
    pub type_field: String,
    pub tag: String,
    /// Dial Fields（1.14 起 server 用域名的 endpoint 需 dial 级 domain_resolver）。
    ///
    /// 与 `Outbound::domain_resolver` 同类型同理由：纯 tag 覆盖顶层 strategy 而不继承 ⇒ AAAA-only
    /// 的 endpoint server 域名解析不到（#335）。见 [`DomainResolver`]。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_resolver: Option<DomainResolver>,
    /// Dial Fields 的 `detour` —— 前置代理：本 endpoint 的**底层拨号**经该 outbound tag 出去。
    ///
    /// # 这是对 上游的有意偏离（用户已授权，非移植遗漏）
    ///
    /// 上游的 `SingBoxEndpoint` 类型没有这个键，三个组网表单（WG / WARP / Tailscale）也都没有
    /// 对应控件 —— 它只在**代理 outbound** 上支持链式前置代理。Polaris 把这条能力延伸到 endpoint：
    /// WG/WARP 的 UDP 握手与 Tailscale 的控制面拨号都能经前置代理走。故本仓生成的配置在
    /// 「endpoint 带 detour」这一形态上**与 上游 金样不可比**（金样 37 例里没有这种节点，
    /// 可选字段缺省 `None` ⇒ 现有对拍逐字节不变；日后若给夹具加带 detour 的 WG/TS 节点，
    /// 那条 delta 是本次偏离的预期代价，不是回归）。
    ///
    /// # 语义（2026-07-31 本机 loopback A/B 实测，随包基线 sing-box 1.14.0-beta.3）
    ///
    /// - **WireGuard**（gVisor 用户态，peer 指本地假 peer，detour 指本地假 SOCKS5）：
    ///   无 detour = 3 次 148 字节握手包直达 peer / SOCKS 零连接；有 detour = **零** UDP 直达 /
    ///   15 次 SOCKS5 `UDP_ASSOCIATE`。⇒ **前置代理必须支持 UDP 转发**，只支持 TCP 的前置代理会让
    ///   WG 起不来且表现为**静默不通**（没有回落直连这条腿）。
    /// - **Tailscale**（control_url 指本地假控制面）：无 detour = 16 次控制面直连（`GET /key?v=131`）/
    ///   SOCKS 零连接；有 detour = **零**直连 / 32 次 SOCKS5 `CONNECT`。TCP 前置代理即可。
    ///
    /// 两侧都**不回落直连** —— 前置代理不可用时该 endpoint 直接不通，这正是 UI 提示必须写清的点。
    ///
    /// # 目标限制
    ///
    /// 只填**非 endpoint** 的 outbound tag。endpoint→endpoint 未经验证（`sing-box check` 不校验
    /// detour 的引用解析，指向不存在的 tag 也 rc=0，故 check 给不出阴性对照），沿用
    /// `builder/outbounds.rs` 对代理 outbound 的同一条排除，保守禁掉。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detour: Option<String>,
    // WireGuard
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peers: Option<Vec<WireGuardPeer>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub udp_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers: Option<u32>,
    // Tailscale（账号制 mesh；默认 tsnet 用户态）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_node: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_node_allow_lan_access: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_routes: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advertise_routes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_interface: Option<bool>,
    /// 固定内核接口名（仅 system=true 时）。TS 用 system_interface_name；WG 用 `name`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_interface_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    // 1.14 新增（P4a）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advertise_tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_server: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_server_port: Option<u16>,
    /// Taildrop 收件目录（1.14.0-beta.15 新增）。**必须恒填绝对路径**，不能吃内核默认值。
    ///
    /// 内核侧行为（`protocol/tailscale/endpoint.go:187-192` + `:253-263`，v1.14.0-beta.15 读源）：
    /// 缺省取字面量 `"Taildrop"` —— **相对路径**，经 `filemanager.BasePath` + `filepath.Abs` 按
    /// **核进程 CWD** 解析；随后在 `Start(StartStateInitialize)` 里 `MkdirAll(0o700)`，且该 mkdir
    /// **无条件执行**（唯一豁免 `version.IsAppleTV()`），失败即 `create taildrop directory` 把整个核
    /// 起崩。⇒ 只要配置里有 tailscale endpoint，这个目录就一定会被建出来，且落点由 CWD 决定。
    ///
    /// 本仓四条起核腿的 CWD 曾经三对一错：App 直起 / Linux helper / macOS helper 都设成 config 目录，
    /// 而 Windows helper 的 `CreateProcessW` 没传 `lpCurrentDirectory` ⇒ 继承服务 CWD
    /// `C:\Windows\System32`，tailnet peer 发来的文件会落进 System32。那条腿已单独补上
    /// （`crates/helper/src/platform/windows/winproc/win.rs`），但**两处都要修**：补 CWD 治的是
    /// 「CWD 是什么」，本字段治的是「不依赖 CWD」—— 只做前者，将来任何新的相对路径默认值仍会跟着漂。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taildrop_directory: Option<String>,
    /// **custom（isEndpoint）逃生舱的原样透传载荷**（WG / Tailscale 两条腿一律留空 ⇒ 不产生任何键）。
    ///
    /// 语义、理由与代价见 [`crate::singbox::Outbound::extra`]；endpoint 侧此前的形态**更坏**：
    /// 本 struct 只有 WG/TS 的字段集（没有 `server` / `server_port` / `username` / `password`），
    /// 而 `builder/outbounds.rs` 的失败处理是 `if let Ok(ep) = from_value::<Endpoint>(val)` ——
    /// Err 分支无 push、无 log、无上报，**节点在配置里凭空消失**。实测两档坏法（随包 beta.7）：
    ///  - 未建模字段（`openconnect` 的 `server`/`username`/`password`，check rc=0 的合法端点）→
    ///    解析成功但四键**全丢**，只剩 `{"type":"openconnect","tag":…}`；
    ///  - 与已建模字段**类型冲突**（`address` 给字符串而非数组）→ 反序列化失败 → **整节点静默消失**。
    ///
    /// 这也是「`openvpn-client`/`openconnect` 只能走 endpoint 腿」（实测：塞进 `outbounds[]` 得
    /// `unknown outbound type`）为什么必须把这条腿修好——那是它们唯一的通路。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `endpoints[].peers[]`（WireGuard peer，`singbox-config-types.ts:243`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireGuardPeer {
    pub address: String,
    pub port: u16,
    pub public_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_shared_key: Option<String>,
    pub allowed_ips: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persistent_keepalive_interval: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserved: Option<Vec<u32>>,
}
