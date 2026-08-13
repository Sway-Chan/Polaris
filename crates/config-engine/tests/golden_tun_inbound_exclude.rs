//! TUN「连入排除清单」(`tunConfig.inboundExcludeCidrs`) 金样对拍 —— buildInbounds 派生的
//! `route_exclude_address` 内容。
//!
//! 对位 上游 `singbox-inbounds-builder.ts` L297-358 + `shared/tun-route-exclude.ts`
//! `computeUserTunExclude`。**变异锁**：本文件的每条 case 都被设计成「删掉一步减法就转红」——
//! 三条减法（mesh / fakeip / macOS 物理 LAN）各有一条只会被那一步剔掉的输入段，规范化（裸 IP 补掩码、
//! 拒非法/过宽）同理。expected 是**全量** `route_exclude_address` 数组（含顺序），不是 contains 断言：
//! 顺序漂移、多出一段、少一段都会失败。
//!
//! 为什么不进 `fixtures/inbounds.json`：那份是 TS 侧导出的对拍金样，Polaris 侧不该手工往里塞 case。

use polaris_config_engine::builder::inbounds::{build_inbounds, InboundsDeps};
use polaris_config_engine::user_config::UserConfig;

/// 本机物理 LAN（macOS 反向路由 guard 的输入；含主机位，走 CIDR 网络地址比较）。
const OWN_LAN: &str = "192.168.1.23/24";

/// 用户在 UI「连入排除清单」里填的原始条目。每条对应一条独立的判定路径：
///  · `172.16.5.0/24`   —— 与任何减法都不相交 → **必须**保留（否则整个特性又变装饰控件）。
///  · `10.66.7.0/24`    —— 落在 WG `allowedIPs` 10.66.0.0/16 内 → mesh 减法剔除（删 mesh 减法即转红）。
///  · `198.18.9.0/24`   —— 落在 FAKEIP_INET4_RANGE 198.18.0.0/15 内 → fakeip 减法剔除（删 fakeip 减法即转红）。
///  · `192.168.1.0/24`  —— 与本机物理 LAN 相交 → **仅 macOS** 剔除（删 darwin LAN 减法 / 误扩到 win32 均转红）。
///  · `203.0.113.7`     —— 裸 IP，须补 `/32`（不补则 sing-box `netip.ParsePrefix: no '/'` 启动 FATAL）。
///  · `256.1.1.1/24`    —— 八位组越界，须被严格校验剔除（松校验会放行 → 内核 FATAL）。
///  · `0.0.0.0/0`       —— catch-all，须被过宽下限剔除（放行 = 整个地址空间排出 TUN、代理静默失效）。
const USER_CIDRS: &str = r#"[
    "172.16.5.0/24",
    "10.66.7.0/24",
    "198.18.9.0/24",
    "192.168.1.0/24",
    "203.0.113.7",
    "256.1.1.1/24",
    "0.0.0.0/0"
]"#;

/// 组网 + FakeIP 场景的 UserConfig：一个 WG endpoint（`alwaysRouteSubnets` 缺省 true → engaged），
/// `dnsConfig` 缺省 → `usesFakeIp` 为 true → 非 Linux 平台 fakeip 段非空。
/// `selectedServerId` 留空是刻意的：避免节点 IP 排除块往 `route_exclude_address` 里追加无关条目，
/// 让金样只反映「连入排除」这一条派生链。
fn config_with(bypass_lan: bool) -> UserConfig {
    let json = format!(
        r#"{{
            "servers": [
                {{
                    "id": "wg-1",
                    "name": "mesh",
                    "protocol": "wireguard",
                    "address": "wg.example.com",
                    "port": 51820,
                    "wireguardSettings": {{ "allowedIPs": ["10.66.0.0/16"] }}
                }}
            ],
            "selectedServerId": null,
            "proxyModeType": "tun",
            "bypassLAN": {bypass_lan},
            "tunConfig": {{ "inboundExcludeCidrs": {USER_CIDRS} }}
        }}"#
    );
    serde_json::from_str(&json).expect("测试 config 反序列化失败")
}

fn deps(platform: &str) -> InboundsDeps {
    InboundsDeps {
        probe_direct_port: None,
        probe_proxy_port: None,
        update_in_port: None,
        probe_pool_ports: vec![],
        platform: platform.into(),
        own_lan_cidrs: vec![OWN_LAN.into()],
        log: |_, _| {},
    }
}

/// 取 TUN inbound 的 `route_exclude_address`（缺省 → None 表示字段整体不下发）。
fn tun_exclude(config: &UserConfig, deps: &InboundsDeps) -> Option<Vec<String>> {
    let inbounds = build_inbounds(config, None, deps);
    let tun = inbounds
        .iter()
        .find(|i| i.type_field == "tun")
        .expect("TUN 模式必须产出 tun inbound");
    tun.route_exclude_address.clone()
}

#[test]
fn darwin_inbound_exclude_golden() {
    // macOS：回环两条（平台基线）+ 用户段减 mesh / fakeip / 物理 LAN 后的两条。
    // 顺序 = 平台基线在前、用户段按声明顺序追加（对位 上游 excludeAddr.push(...userExclude.extra)）。
    let got = tun_exclude(&config_with(true), &deps("darwin"));
    let expected = ["127.0.0.0/8", "::1/128", "172.16.5.0/24", "203.0.113.7/32"].map(String::from);
    assert_eq!(
        got.as_deref(),
        Some(expected.as_slice()),
        "macOS route_exclude_address 金样不符"
    );
}

#[test]
fn win32_keeps_own_lan_but_still_subtracts_mesh_and_fakeip() {
    // Windows：物理 LAN **不**减（NE 反向路由是 macOS 专属约束；Win 侧 WinTun 的网关保护走 bypassLAN carve
    // 那条独立链路）。这里关 bypassLAN 让基线退化为回环两条 + Win DNS 回流护栏，金样才聚焦在用户段上。
    // mesh / fakeip / 规范化三条减法在 Win 上照旧生效。
    let got = tun_exclude(&config_with(false), &deps("win32")).expect("win32 应有排除段");
    let user_tail = &got[got.len() - 3..];
    assert_eq!(
        user_tail,
        ["172.16.5.0/24", "192.168.1.0/24", "203.0.113.7/32"].map(String::from),
        "win32 用户段派生不符：物理 LAN 段应保留，mesh/fakeip/非法段应被剔除"
    );
    // 前缀是平台基线（回环 + DNS 回流护栏），不含任何用户段派生物。
    assert!(
        !got[..got.len() - 3].iter().any(|c| c.starts_with("172.16.")
            || c.starts_with("10.66.")
            || c.starts_with("198.18.")),
        "win32 基线段不应混入用户段：{got:?}"
    );
}

#[test]
fn linux_emits_no_route_exclude_address_at_all() {
    // Linux 加法态：`route_exclude_address` 非空即触发策略路由表 2022 两族分解 → 服务端连入/allowLan 回包全断。
    // 「连入排除」在 Linux 上既不需要（内核策略路由天然保护回包）又是毒丸 → **恒不生成字段**。
    // 这条断言就是 UI「Linux 无效」说明与实现的一致性锁：任何往 Linux 分支塞排除段的改动都会转红。
    assert_eq!(
        tun_exclude(&config_with(true), &deps("linux")),
        None,
        "Linux 必须不下发 route_exclude_address（加法态毒丸）"
    );
}

#[test]
fn empty_list_is_byte_identical_to_absent() {
    // 空清单 / 未设 → 与今日字节完全一致（不得因本特性引入 `route_exclude_address: []` 或多余条目）。
    let baseline: UserConfig =
        serde_json::from_str(r#"{"servers":[],"selectedServerId":null,"proxyModeType":"tun"}"#)
            .unwrap();
    let empty: UserConfig = serde_json::from_str(
        r#"{"servers":[],"selectedServerId":null,"proxyModeType":"tun",
            "tunConfig":{"inboundExcludeCidrs":[]}}"#,
    )
    .unwrap();
    for platform in ["darwin", "win32", "linux"] {
        assert_eq!(
            tun_exclude(&empty, &deps(platform)),
            tun_exclude(&baseline, &deps(platform)),
            "[{platform}] 空清单必须与未设字段字节等价"
        );
    }
}

#[test]
fn all_dropped_leaves_platform_baseline_untouched() {
    // 全部条目都被减法/校验剔除 → 排除表须与「没填过」完全相同（不得留空洞或残渣）。
    let all_dropped: UserConfig = serde_json::from_str(
        r#"{
            "servers":[{"id":"wg-1","name":"mesh","protocol":"wireguard",
                        "address":"wg.example.com","port":51820,
                        "wireguardSettings":{"allowedIPs":["10.66.0.0/16"]}}],
            "selectedServerId": null,
            "proxyModeType":"tun",
            "tunConfig":{"inboundExcludeCidrs":["10.66.7.0/24","198.18.9.0/24","0.0.0.0/0"]}
        }"#,
    )
    .unwrap();
    let d = deps("darwin");
    assert_eq!(
        tun_exclude(&all_dropped, &d),
        Some(vec!["127.0.0.0/8".to_string(), "::1/128".to_string()]),
        "全剔除后应只剩 macOS 平台基线"
    );
}
