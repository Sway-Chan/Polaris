//! UserConfig 投影（上游 `shared/types.ts UserConfig` 子集）。
//!
//! 增量定义：仅 builder 所需字段。随各 builder 移植扩展。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

use crate::user_config::dns_config::DnsConfig;
use crate::user_config::proxy_mode::{ProxyMode, ProxyModeType};
use crate::user_config::region_routing::RegionRoutingConfig;
use crate::user_config::rule::{AppRule, CustomAppPreset, Rule, RuleResource};
use crate::user_config::server_config::ServerConfig;
use crate::user_config::tun_config::TunModeConfig;

/// 用户配置（增量子集）。上游 `UserConfig`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserConfig {
    pub servers: Vec<ServerConfig>,
    #[serde(rename = "selectedServerId")]
    pub selected_server_id: Option<String>,
    #[serde(rename = "proxyMode", default = "default_proxy_mode")]
    pub proxy_mode: ProxyMode,
    #[serde(rename = "proxyModeType", default = "default_proxy_mode_type")]
    pub proxy_mode_type: ProxyModeType,
    #[serde(rename = "tunConfig")]
    pub tun_config: Option<TunModeConfig>,
    #[serde(rename = "customRules", default)]
    pub custom_rules: Vec<Rule>,
    // rename 不可省：本结构**无** `rename_all`，逐字段 rename。缺了它 serde 找 `app_rules` 键，
    // 而 config.json 里是 `appRules` → `default` 静默给空 Vec → 应用分流整条在运行期不存在。
    #[serde(rename = "appRules", default)]
    pub app_rules: Vec<AppRule>,
    #[serde(rename = "appRoutingEnabled")]
    pub app_routing_enabled: Option<bool>,
    #[serde(rename = "customAppPresets", default)]
    pub custom_app_presets: Vec<CustomAppPreset>,
    #[serde(rename = "allowLan")]
    pub allow_lan: Option<bool>,
    #[serde(rename = "bypassLAN")]
    pub bypass_lan: Option<bool>,
    #[serde(rename = "bypassLANList")]
    pub bypass_lan_list: Option<Vec<String>>,
    #[serde(rename = "enableIPv6")]
    pub enable_ipv6: Option<bool>,
    #[serde(rename = "mixedPort")]
    pub mixed_port: Option<u16>,
    #[serde(rename = "httpPort")]
    pub http_port: Option<u16>,
    #[serde(rename = "dnsConfig")]
    pub dns_config: Option<DnsConfig>,
    // 同 app_rules：config.json 键是 `ruleResources`（store/src/sanitize.rs:62 亦按此名清洗）。
    #[serde(rename = "ruleResources", default)]
    pub rule_resources: Vec<RuleResource>,
    #[serde(rename = "tlsFragment", skip_serializing_if = "Option::is_none")]
    pub tls_fragment: Option<bool>,
    #[serde(
        rename = "interruptConnectionsOnSwitch",
        skip_serializing_if = "Option::is_none"
    )]
    pub interrupt_connections_on_switch: Option<bool>,
    /// 拨号前把目的域名解析成真实 IP 再交给节点（sing-box route action `resolve`）。**默认关。**
    ///
    /// # 默认关的理由（按确定性排序，非偏好）
    ///
    /// 1. `resolve` 失败在上游 `route/route.go:664-667` 是 **fatalErr**，没有「退回发域名」的兜底
    ///    ⇒ 连接直接终止。默认开等于把远端 DNS 从「只影响 endpoint 拨号解析」升级成
    ///    **每条经代理连接的硬前置**，给所有人加一个新单点。
    /// 2. 节点侧按域名 / SNI 做的分流与解锁，在 IP 交付形态下无法工作 —— 机制上确定，
    ///    实际影响面视机场而定。
    ///
    /// # 射程：**不与 FakeIP 联动**（2026-08-11 判定，与上游的做法不同）
    ///
    /// 「FakeIP 关 ⇒ 本开关无效」只在 TUN 语境成立，而 `mixed` 入站在本仓是**无条件生成**的
    /// （`builder/inbounds.rs` 里它在任何模式判断之前 push）。浏览器手配代理 / 终端 `http_proxy` /
    /// 其他 app 指向本机代理端口的流量，目的地恒以 `CONNECT host:port` 的域名形态交付，与 FakeIP 无关。
    /// ⇒ **不存在「本开关必然无效」的配置**，故不置灰；灰掉会在它确实有效的场景把它锁死。
    /// （上游 用 `usesFakeIp` 做门，其 PR 自陈「理由只在 TUN 语境成立」并标为已知窄口。）
    #[serde(rename = "resolveBeforeDial", skip_serializing_if = "Option::is_none")]
    pub resolve_before_dial: Option<bool>,
    #[serde(rename = "regionRouting", skip_serializing_if = "Option::is_none")]
    pub region_routing: Option<RegionRoutingConfig>,
    /// fakeip-filter 总开关：false = 完全关（不生成 captive/ntp filter 规则）。上游 `fakeIpFilter`。
    #[serde(rename = "fakeIpFilter", skip_serializing_if = "Option::is_none")]
    pub fake_ip_filter: Option<bool>,
    /// 用户编辑过的 fakeip-filter 域名清单（未编辑=undefined → 用默认 captive+ntp）。上游 `fakeIpFilterList`。
    #[serde(rename = "fakeIpFilterList", skip_serializing_if = "Option::is_none")]
    pub fake_ip_filter_list: Option<Vec<String>>,
    /// 拦截浏览器内置 DoH（Chrome/Firefox 的「安全 DNS」）：对清单内域名的 443/853 与 UDP443 发 reject。
    /// **默认关**。开启前请读 [`Self::browser_doh_list`] 的取舍说明。
    ///
    /// # 为什么需要它
    ///
    /// 浏览器自带 DoH 会绕开本应用的 DNS 接管（hijack-dns / FakeIP）⇒ 基于域名的分流与 FakeIP 路由
    /// 对那部分查询**不生效**，且查询内容直接送到第三方 DoH 提供商。
    ///
    /// # 为什么默认关、且不内置成恒开
    ///
    /// 屏蔽浏览器行为不是代理客户端该替用户做的决定；2026-08-13 之前这里是一张**用户关不掉**的
    /// 硬编码黑名单，已整块移除（见 `builder::route` 的删除说明）。现在它是一个默认关的开关。
    #[serde(rename = "blockBrowserDoh", skip_serializing_if = "Option::is_none")]
    pub block_browser_doh: Option<bool>,
    /// 被拦的 DoH 端点域名清单（`domain_suffix` 语义）。未编辑 = `None` → 用
    /// [`crate::builder::route::DEFAULT_BROWSER_DOH_SUFFIXES`] 的内置起点。
    ///
    /// # 为什么是 suffix 而不是 keyword
    ///
    /// 旧实现用 `domain_keyword`，匹配面宽（`dns.google` 会命中 `foo.dns.google.evil.com`）。
    /// 这是一张**用户可编辑**的清单：用户填个短词就会误伤一大片，而误伤的后果他看不见。
    /// suffix 的代价是「填不全」，那一格已由清单本身可编辑 + 批量导入解决。
    #[serde(rename = "browserDohList", skip_serializing_if = "Option::is_none")]
    pub browser_doh_list: Option<Vec<String>>,
    /// 阻止 QUIC（对代理向 UDP 443 执行 reject，逼浏览器回退 TCP）；默认关；节点无关。
    /// 上游 `blockQuic`。
    #[serde(rename = "blockQuic", skip_serializing_if = "Option::is_none")]
    pub block_quic: Option<bool>,
    /// WebRTC 防泄露：off=不注入 / proxy=STUN 经代理 / block=reject STUN。上游 `webrtcLeakProtection`。
    #[serde(
        rename = "webrtcLeakProtection",
        skip_serializing_if = "Option::is_none"
    )]
    pub webrtc_leak_protection: Option<String>,
    /// 兼容旧配置的兜底排除进程（新数据已迁移为 customRules 的 processName+direct 规则）。上游 `bypassProcesses`。
    #[serde(rename = "bypassProcesses", skip_serializing_if = "Option::is_none")]
    pub bypass_processes: Option<Vec<String>>,
    /// clash_api/management api 鉴权 secret。上游 `clashApiSecret`。generateSingBoxConfig 注入 services[0].secret。
    #[serde(rename = "clashApiSecret", skip_serializing_if = "Option::is_none")]
    pub clash_api_secret: Option<String>,
    /// sing-box 1.14 官方面板 opt-in 开关。上游 `singboxDashboard`。on 时注入 services[0].dashboard。
    #[serde(rename = "singboxDashboard", skip_serializing_if = "Option::is_none")]
    pub singbox_dashboard: Option<bool>,
    // ── 日志两轴：只为 norm 可见性入列，值的解释权不在这里 ──────────────────────────────
    //
    // **为什么必须在册**：两键被 `runtime/proxy.rs::log_axes_from_config` 从裸 JSON 读走喂 sing-box
    // `log.*` —— 改了要重启内核才生效。不在册则 `config_generation_norm` 恒相等 ⇒ 落 NoOp 腿 ⇒
    // 永不进 pending 差集：核在跑时关「关闭日志写盘」，sing-box 照旧写盘且全程无提示
    // （`ui/src/domain/app-restart-keys.ts` 记的「第四类重启」）。上游的同名排除表**不含**这两键，
    // 即那边本就会进重启判定 —— 本仓此前的行为是一处迁移回归，不是取舍。
    //
    // **为什么是 `Value` 而不是 `LogLevel` / `bool`**：`UserConfig` 的解析是全有全无的
    // （`from_value::<UserConfig>` 一旦 `Err`，起核腿整个放弃），而 `logLevel` 的取值域不由本仓独占
    // （sing-box 有 `trace`，手改/旧版配置还可能写进别的东西）。收紧类型 = 把「提示得晚」的缺陷换成
    // 「起不了核」的缺陷。用 `Value`：任何 JSON 值都进得来、都进投影、变了就判不等，而值怎么解释仍归
    // `log_axes_from_config`（非法值退化 `Info`，行为一字未改）。
    //
    // 「宽容强类型」（`Option<LogLevel>` + 解析失败落 `None`）也不行：那会让 `"trace"` 与 `"bogus"`
    // 归一成同一个 `None` ⇒ 两者互改**看不见**，等于在窄一点的取值域上重犯同一个错。`Value` 无此洞。
    /// 核日志级别。上游 `logLevel`。
    #[serde(rename = "logLevel", skip_serializing_if = "Option::is_none")]
    pub log_level: Option<serde_json::Value>,
    /// 禁用日志写盘。上游 `disableLogFile`。
    #[serde(rename = "disableLogFile", skip_serializing_if = "Option::is_none")]
    pub disable_log_file: Option<serde_json::Value>,
}

impl UserConfig {
    /// `UserConfig` 的**序列化键集**（= `config_generation_norm` 投影面），按声明序。
    ///
    /// # 这是干什么用的
    ///
    /// 渲染端「配置暂存」的豁免谓词是一行：`豁免(key) := key ∉ UserConfigFieldSet`。
    /// 判据**不是**查 `builder/orchestration.rs` 的排除表——`config_generation_norm` 的入参就是
    /// `&UserConfig`，投影里只可能出现本结构声明的键，排除一个本就不存在的键是空操作。该表
    /// 2026-07-29 前有 15 项、其中 14 项正是这种死键（随 上游 逐行对拍判据退役已删，现只剩
    /// `selectedServerId`）。真正决定「改了这个键核会不会重新生成配置」的，就是「它在不在本结构里」。
    ///
    /// 于是本常量是那条谓词的**唯一真值源**，导出给渲染端
    /// （`ui/src/contracts/user-config-fields.ts`，双向锁见同名 `.test.ts`）。
    ///
    /// # 增删字段时必须同步这里
    ///
    /// 下方 `fully_populated()` 用**穷尽结构字面量**构造实例：给 `UserConfig` 加字段而不改它 → E0063
    /// 编译失败；改了它却忘了本表 → `field_names_equals_serde_projection` 转红。两道门合起来，
    /// 「Rust 加了字段而字段表没跟上」在本 crate 内就被拦住，不必等前端那条跨语言锁。
    pub const FIELD_NAMES: &'static [&'static str] = &[
        "servers",
        "selectedServerId",
        "proxyMode",
        "proxyModeType",
        "tunConfig",
        "customRules",
        "appRules",
        "appRoutingEnabled",
        "customAppPresets",
        "allowLan",
        "bypassLAN",
        "bypassLANList",
        "enableIPv6",
        "mixedPort",
        "httpPort",
        "dnsConfig",
        "ruleResources",
        "tlsFragment",
        "interruptConnectionsOnSwitch",
        "resolveBeforeDial",
        "regionRouting",
        "fakeIpFilter",
        "fakeIpFilterList",
        "blockBrowserDoh",
        "browserDohList",
        "blockQuic",
        "webrtcLeakProtection",
        "bypassProcesses",
        "clashApiSecret",
        "singboxDashboard",
        "logLevel",
        "disableLogFile",
    ];
}

fn default_proxy_mode() -> ProxyMode {
    ProxyMode::Smart
}

fn default_proxy_mode_type() -> ProxyModeType {
    ProxyModeType::SystemProxy
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_config::region_routing::RegionRoutingConfig;
    use std::collections::BTreeSet;

    /// 全字段就位的实例。
    ///
    /// 两条约束，缺一这道门就没牙：
    ///  1. **穷尽结构字面量**（禁 `..Default::default()`）—— 新增字段 → E0063 编译失败 → 作者必到此一游。
    ///  2. 所有 `Option` 一律 `Some` —— 带 `skip_serializing_if = "Option::is_none"` 的 10 个字段若为 `None`
    ///     就**不出现在投影里**，相等断言会静默退化成「只测了 17 项」。
    fn fully_populated() -> UserConfig {
        UserConfig {
            servers: Vec::new(),
            selected_server_id: Some(String::new()),
            proxy_mode: default_proxy_mode(),
            proxy_mode_type: default_proxy_mode_type(),
            tun_config: Some(TunModeConfig::default()),
            custom_rules: Vec::new(),
            app_rules: Vec::new(),
            app_routing_enabled: Some(false),
            custom_app_presets: Vec::new(),
            allow_lan: Some(false),
            bypass_lan: Some(false),
            bypass_lan_list: Some(Vec::new()),
            enable_ipv6: Some(false),
            mixed_port: Some(0),
            http_port: Some(0),
            dns_config: Some(DnsConfig::default()),
            rule_resources: Vec::new(),
            tls_fragment: Some(false),
            interrupt_connections_on_switch: Some(false),
            resolve_before_dial: Some(false),
            region_routing: Some(RegionRoutingConfig::default()),
            fake_ip_filter: Some(false),
            fake_ip_filter_list: Some(Vec::new()),
            block_browser_doh: Some(false),
            browser_doh_list: Some(Vec::new()),
            block_quic: Some(false),
            webrtc_leak_protection: Some(String::new()),
            bypass_processes: Some(Vec::new()),
            clash_api_secret: Some(String::new()),
            singbox_dashboard: Some(false),
            log_level: Some(serde_json::json!("info")),
            disable_log_file: Some(serde_json::json!(false)),
        }
    }

    /// `FIELD_NAMES` ≡ serde 投影的键集。
    ///
    /// 牙：改一个 `#[serde(rename = ...)]` 而不改表（或反之）→ 两侧集合不等 → 转红。
    #[test]
    fn field_names_equals_serde_projection() {
        let value = serde_json::to_value(fully_populated()).expect("UserConfig 必须可序列化");
        let projected: BTreeSet<&str> = value
            .as_object()
            .expect("UserConfig 序列化必须是 object")
            .keys()
            .map(String::as_str)
            .collect();
        let declared: BTreeSet<&str> = UserConfig::FIELD_NAMES.iter().copied().collect();
        assert_eq!(
            declared, projected,
            "UserConfig::FIELD_NAMES 与实际序列化键集不符（加/删/改名字段后忘了同步常量表）"
        );
    }

    /// 表内不得有重复项 —— 重复会让上面那条集合断言在「漏了一项 + 抄重了一项」时假绿。
    #[test]
    fn field_names_has_no_duplicates() {
        let unique: BTreeSet<&str> = UserConfig::FIELD_NAMES.iter().copied().collect();
        assert_eq!(unique.len(), UserConfig::FIELD_NAMES.len());
    }
}
