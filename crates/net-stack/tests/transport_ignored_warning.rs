//! 🔴 **导入侧必须告诉用户「这个节点的传输层参数不会生效」** —— 四个 parser 一个都不许漏。
//!
//! # 背景
//!
//! 内核只允许 trojan / vless / vmess 三种出站挂 `transport`（随包核 beta.7 schema 的 20 支 oneOf
//! 里只有这三支有该属性，其余 17 支 `additionalProperties:false`）。而**导入侧造得出**别的组合：
//! xray 的 `streamSettings` 可挂在任意出站上、clash 的 `network:` 同理、`naive://…?type=ws` 亦然。
//!
//! 生成侧（`builder/outbound.rs::protocol_can_carry_transport`）会把这些参数**丢掉** —— 不丢的话
//! 产出的是 `FATAL decode config: outbounds[N].transport: unknown field "transport"`，
//! **整份配置起不来**，不止这个节点。
//!
//! 但「生成时丢」在用户侧完全无声：卡片看起来正常、连不上也不知道为什么。导入这一刻是唯一还拿得到
//! 上下文（哪个节点、哪种传输）的时机，故报在这里。
//!
//! # 这道门守的是什么
//!
//! 告警由 `ClashParseResult::finish()` 统一产出，而 `finish()` 靠**每个 parser 记得在 return 前调**。
//! 「记得调」不是结构保证 —— 新加一个 parser 的人不会知道有这回事。所以这里给**每个公开入口**
//! 各一条：漏调的那个入口会立刻红，并在失败信息里直接说出该去补什么。
//!
//! 每条都带**正向对照**（同一入口、换成 transport-capable 的协议 ⇒ 不许告警），
//! 否则「有告警」既可能是判据对，也可能是它对谁都告警。

use polaris_net_stack::clash_parser::{parse_clash_proxies, ClashParseResult};
use polaris_net_stack::share_link::parse_url_list;
use polaris_net_stack::singbox_import::{parse_singbox_outbounds, ImportOrigin};
use polaris_net_stack::xray_import::parse_xray_outbounds;

fn ids() -> impl FnMut() -> String {
    let mut n = 0u32;
    move || {
        n += 1;
        format!("id-{n}")
    }
}

/// 告警里必然出现的那句（`finish()` 的措辞），用来把本条与其它告警区分开。
const NEEDLE: &str = "不支持传输层";

fn warned(r: &ClashParseResult) -> bool {
    r.warnings.iter().any(|w| w.contains(NEEDLE))
}

/// 出现告警的同时，节点本身必须照常入库 —— 本条是「告警」不是「拒收」。
fn assert_warned_but_kept(r: &ClashParseResult, who: &str) {
    assert!(
        warned(r),
        "{who}：解析出的节点带了挂不住的传输层参数，却没有任何告警。\
         多半是该 parser 的 return 漏了 `.finish()` —— 生成侧会静默丢掉这些参数，\
         用户只会看到「节点连不上」。实得 warnings={:?}",
        r.warnings
    );
    assert_eq!(
        r.servers.len(),
        1,
        "{who}：本条是告警不是拒收，节点必须照常导入"
    );
}

fn assert_not_warned(r: &ClashParseResult, who: &str) {
    assert!(
        !warned(r),
        "{who}（正向对照）：transport-capable 的协议不该被告警 —— \
         判据把内核允许的组合也报了，门会变成噪音。实得 warnings={:?}",
        r.warnings
    );
    assert_eq!(r.servers.len(), 1, "{who}（正向对照）：节点应正常解析");
}

#[test]
fn singbox_import_warns_when_transport_cannot_apply() {
    let bad: serde_json::Value = serde_json::from_str(
        r#"[{"type":"shadowsocks","tag":"ss-ws","server":"a.example.com","server_port":8388,
             "method":"aes-256-gcm","password":"pw",
             "transport":{"type":"ws","path":"/w"}}]"#,
    )
    .unwrap();
    let r = parse_singbox_outbounds(&bad, "sub", "now", &mut ids(), ImportOrigin::LocalFile);
    assert_warned_but_kept(&r, "singbox_import");

    // 正向对照：同一个入口、同一种传输，换成内核认的 vless ⇒ 不许告警。
    let ok: serde_json::Value = serde_json::from_str(
        r#"[{"type":"vless","tag":"vless-ws","server":"a.example.com","server_port":443,
             "uuid":"6ba7b810-9dad-11d1-80b4-00c04fd430c8",
             "transport":{"type":"ws","path":"/w"}}]"#,
    )
    .unwrap();
    let r = parse_singbox_outbounds(&ok, "sub", "now", &mut ids(), ImportOrigin::LocalFile);
    assert_not_warned(&r, "singbox_import");
}

#[test]
fn xray_import_warns_when_transport_cannot_apply() {
    let bad: serde_json::Value = serde_json::from_str(
        r#"[{"protocol":"shadowsocks","tag":"ss-ws",
             "settings":{"servers":[{"address":"a.example.com","port":8388,
                 "method":"aes-256-gcm","password":"pw"}]},
             "streamSettings":{"network":"ws","wsSettings":{"path":"/w"}}}]"#,
    )
    .unwrap();
    let r = parse_xray_outbounds(&bad, "sub", "now", &mut ids());
    assert_warned_but_kept(&r, "xray_import");

    let ok: serde_json::Value = serde_json::from_str(
        r#"[{"protocol":"vless","tag":"vless-ws",
             "settings":{"vnext":[{"address":"a.example.com","port":443,
                 "users":[{"id":"6ba7b810-9dad-11d1-80b4-00c04fd430c8"}]}]},
             "streamSettings":{"network":"ws","wsSettings":{"path":"/w"}}}]"#,
    )
    .unwrap();
    let r = parse_xray_outbounds(&ok, "sub", "now", &mut ids());
    assert_not_warned(&r, "xray_import");
}

#[test]
fn clash_parser_warns_when_transport_cannot_apply() {
    let bad: serde_yaml::Value = serde_yaml::from_str(
        "- name: ss-ws\n  type: ss\n  server: a.example.com\n  port: 8388\n  \
         cipher: aes-256-gcm\n  password: pw\n  network: ws\n  ws-opts:\n    path: /w\n",
    )
    .unwrap();
    let r = parse_clash_proxies(&bad, "sub", "now", &mut ids());
    assert_warned_but_kept(&r, "clash_parser");

    let ok: serde_yaml::Value = serde_yaml::from_str(
        "- name: vless-ws\n  type: vless\n  server: a.example.com\n  port: 443\n  \
         uuid: 6ba7b810-9dad-11d1-80b4-00c04fd430c8\n  network: ws\n  ws-opts:\n    path: /w\n",
    )
    .unwrap();
    let r = parse_clash_proxies(&ok, "sub", "now", &mut ids());
    assert_not_warned(&r, "clash_parser");
}

#[test]
fn share_link_warns_when_transport_cannot_apply() {
    // naive 是 share-link 里唯一一个「会走 `apply_transport_settings`、但内核挂不住 transport」的
    // scheme（另两个调用点是 vless / trojan，都在白名单内）。
    let r = parse_url_list(
        "naive+https://u:pw@a.example.com:443?type=ws&path=/w#naive-ws",
        "sub",
        "now",
        &mut ids(),
    );
    assert_warned_but_kept(&r, "share_link");

    let r = parse_url_list(
        "vless://6ba7b810-9dad-11d1-80b4-00c04fd430c8@a.example.com:443?type=ws&path=/w#vless-ws",
        "sub",
        "now",
        &mut ids(),
    );
    assert_not_warned(&r, "share_link");
}

/// 🔴 **告警不许把节点名列成一整屏。**
///
/// 机场一次下发几百个同型节点是常态；不截断的话这条告警会把导入结果面板挤爆，
/// 而面板上其余告警（缺字段、不支持的协议）会被顶出视野 —— 那才是用户更需要看到的。
#[test]
fn warning_truncates_the_node_list() {
    let mut arr = Vec::new();
    for i in 0..9 {
        arr.push(serde_json::json!({
            "type": "shadowsocks", "tag": format!("ss-{i}"),
            "server": "a.example.com", "server_port": 8388,
            "method": "aes-256-gcm", "password": "pw",
            "transport": { "type": "ws", "path": "/w" }
        }));
    }
    let r = parse_singbox_outbounds(
        &serde_json::Value::Array(arr),
        "sub",
        "now",
        &mut ids(),
        ImportOrigin::LocalFile,
    );
    let w = r
        .warnings
        .iter()
        .find(|w| w.contains(NEEDLE))
        .expect("应有告警");
    assert!(w.contains("等 9 个"), "9 个节点应报总数；实得 {w}");
    assert!(
        !w.contains("ss-5"),
        "只列前 5 个，第 6 个起不该出现在告警里；实得 {w}"
    );
    assert_eq!(r.servers.len(), 9, "九个节点都要照常导入");
}
