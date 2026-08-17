//! 三张协议登记表必须彼此一致 —— 否则同一个协议在链路的两端叫不同的名字。
//!
//! # 缺陷原型（2026-08-12 实测，`openvpn-client`）
//!
//! 协议名在本仓有三处独立登记：
//!
//! | 登记表 | 位置 | 谁读它 |
//! |---|---|---|
//! | Rust `Protocol` 的 **serde 名** | `config-engine/user_config/server_config.rs` | 落盘 JSON ⇄ 内存结构 |
//! | `ALLOWED_PROTOCOLS` | `store/validate.rs` | sanitize：决定一个节点**能不能落盘** |
//! | `NodeProto` | `ui/components/dialogs/node-spec.ts` | 节点对话框：决定**写出什么字符串** |
//!
//! `Protocol` 枚举挂的是 `#[serde(rename_all = "lowercase")]`，它会把 `OpenvpnClient` 折成
//! `openvpnclient`；另外两张表写的都是内核类型名 `openvpn-client`。三处不一致，**两个方向都炸**：
//!
//! - **UI → 落盘**：对话框写 `"openvpn-client"` → `UserConfig` 反序列化 `unknown variant` →
//!   **整份配置解析失败**。实测过：一个坏节点带走全部节点连同全部设置，不是只丢它自己。
//! - **导入 → 落盘**：导入侧构造 `Protocol::OpenvpnClient` → 序列化成 `"openvpnclient"` →
//!   不在 `ALLOWED_PROTOCOLS` 里 → sanitize 那条腿是 `continue`，**节点静默消失**，无任何提示。
//!
//! # 为什么此前一条测试都没红
//!
//! 该协议的每一条既有测试都在 Rust 里**直接构造枚举**（`protocol: Protocol::OpenvpnClient`），
//! 于是全程不经过那个字符串。唯一走字符串的是「协议 × 传输」交叉门，而它的协议清单是
//! **手写夹具**，只有 12 条，新协议不在其中 —— 夹具驱动的门，覆盖面不会跟着代码长。
//!
//! 故本门的判据一律**从源码/类型推导**，不写第二份清单：
//! 变体集合从 `Protocol` 的源码解析，UI 侧从 `NodeProto` 的源码解析，落盘侧从 `ALLOWED_PROTOCOLS`
//! 本体读。三边任意一处新增或改名而另两处没跟上，本门即红。

use polaris_config_engine::user_config::server_config::Protocol;
use polaris_store::validate::ALLOWED_PROTOCOLS;

/// 全部变体。完整性不靠人盯 —— 下面 `all_protocols_array_is_complete` 拿源码对差。
const ALL_PROTOCOLS: &[Protocol] = &[
    Protocol::Vless,
    Protocol::Trojan,
    Protocol::Hysteria2,
    Protocol::Shadowsocks,
    Protocol::Anytls,
    Protocol::Tuic,
    Protocol::Vmess,
    Protocol::Naive,
    Protocol::Snell,
    Protocol::Socks,
    Protocol::Http,
    Protocol::Ssh,
    Protocol::Wireguard,
    Protocol::Tailscale,
    Protocol::Hysteria,
    Protocol::Tor,
    Protocol::Openconnect,
    Protocol::OpenvpnClient,
    Protocol::Custom,
];

/// 该协议该不该出现在**节点对话框**的协议下拉里。
///
/// 用穷尽 `match` 而不是清单：加了新变体，这里编译不过 —— 强制作者表态，
/// 而不是让新协议悄悄落在「两张表都没提到」的缝里。
enum UiClaim {
    /// 用户在节点对话框里能建 ⇒ 必须在 `NodeProto` 里。
    InDialog,
    /// 刻意不进节点对话框 + 理由 ⇒ 必须**不**在 `NodeProto` 里。
    NotInDialog(&'static str),
}

fn ui_claim(p: Protocol) -> UiClaim {
    use Protocol::*;
    match p {
        Vless | Trojan | Hysteria2 | Shadowsocks | Anytls | Tuic | Vmess | Naive | Snell
        | Socks | Http | Ssh | Hysteria | Tor | Openconnect | OpenvpnClient | Custom => {
            UiClaim::InDialog
        }
        // 组网节点走「组网」页签的专属流程（要 auth_key / 对端配置 / 单例约束），
        // 不从通用节点对话框建；`is_mesh_protocol` 的消费点也按这个前提分组。
        Wireguard | Tailscale => UiClaim::NotInDialog("组网节点，由组网页签建，不进通用节点对话框"),
    }
}

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/store 之上应有仓根")
        .to_path_buf()
}

/// 协议的 serde 名（= 落盘 JSON 里那个字符串）。
fn wire(p: Protocol) -> String {
    match serde_json::to_value(p).expect("Protocol 可序列化") {
        serde_json::Value::String(s) => s,
        other => panic!("Protocol 序列化成了非字符串：{other:?}"),
    }
}

/// 从源码解析 `pub enum Protocol { .. }` 的变体标识符。
fn protocol_variants_from_source() -> Vec<String> {
    let path = repo_root().join("crates/config-engine/src/user_config/server_config.rs");
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let start = src
        .find("pub enum Protocol {")
        .expect("找不到 `pub enum Protocol {` —— 枚举改名/挪窝了，先确认再动本门");
    let body = &src[start..];
    let end = body.find("\n}").expect("枚举没有收口");
    body[..end]
        .lines()
        .skip(1)
        .map(str::trim)
        // 文档注释 / 行注释 / 属性行一律不是变体
        .filter(|l| !l.is_empty() && !l.starts_with("//") && !l.starts_with("#["))
        .filter_map(|l| l.strip_suffix(','))
        .filter(|l| {
            l.chars().next().is_some_and(char::is_uppercase)
                && l.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .map(str::to_string)
        .collect()
}

/// 从一段 TS 源码里解析 `export type <名> = 'a' | 'b' | …;` 的成员。
///
/// 逐行剔掉 `//` 之后的内容再取引号内容。**今天两个联合类型的注释里没有带引号的协议名，
/// 故这一步现在剔不掉任何东西** —— 它是给将来准备的：本仓这两处注释本来就在讨论协议
/// （例如说明 shadowtls 为什么不在列），有人补一句 `'shadowtls' 走插件形态` 就会被当成成员，
/// 把真实缺口盖绿。这条剔除逻辑本身由 `parser_drops_names_quoted_inside_comments` 用合成语料验红。
fn ts_union_members_from(src: &str, type_name: &str) -> Vec<String> {
    let needle = format!("export type {type_name}");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("找不到 `{needle}` —— 类型改名了，先确认再动本门"));
    let body = &src[start..];
    let end = body
        .find(';')
        .unwrap_or_else(|| panic!("{type_name} 没有以 `;` 收口"));
    let stripped: String = body[..end]
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    stripped
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

fn ts_union_members(rel: &str, type_name: &str) -> Vec<String> {
    let path = repo_root().join(rel);
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    ts_union_members_from(&src, type_name)
}

/// 节点对话框的协议下拉（`NodeProto`）—— 落盘协议集的**子集**。
fn node_proto_members() -> Vec<String> {
    ts_union_members("ui/src/components/dialogs/node-spec.ts", "NodeProto")
}

/// 前端对**落盘形状**的镜像（`contracts/types.ts::Protocol`）—— 应与 Rust 侧精确相等。
fn contracts_protocol_members() -> Vec<String> {
    ts_union_members("ui/src/contracts/types.ts", "Protocol")
}

/// 🔴 解析器自检：注释里被引号括起来的名字**不得**算成员。
///
/// 本条用合成语料，不依赖仓里当下的注释长什么样 —— 真实源码今天恰好没有这种注释，
/// 拿真实源码验这一步只会得到一个无信息量的绿。
#[test]
fn parser_drops_names_quoted_inside_comments() {
    let src =
        "export type Fake =\n  | 'alpha'\n  // 'beta' 走的是插件形态，不在此列\n  | 'gamma';\n";
    assert_eq!(
        ts_union_members_from(src, "Fake"),
        vec!["alpha".to_string(), "gamma".to_string()],
        "注释里的 'beta' 被当成了成员 —— 这类漏网会让「某协议缺席」的缺口显示为绿"
    );
}

/// 🔴 变体数组必须覆盖枚举全部变体 —— 否则漏掉的那个协议本文件一条也判不到。
#[test]
fn all_protocols_array_is_complete() {
    let mut from_src = protocol_variants_from_source();
    let mut from_arr: Vec<String> = ALL_PROTOCOLS.iter().map(|p| format!("{p:?}")).collect();
    from_src.sort();
    from_arr.sort();
    assert_eq!(
        from_arr, from_src,
        "ALL_PROTOCOLS 与 `Protocol` 源码的变体集合对不上 —— \
         数组漏了谁，本文件下面所有断言就对谁完全失明"
    );
}

/// 🔴 serde 名必须能原样读回来，且全小写。
///
/// 小写这条不是洁癖：sanitize 用 `to_ascii_lowercase()` 之后再比 `ALLOWED_PROTOCOLS`，
/// 一个带大写的 serde 名会**永远匹配不上**，症状同样是「节点静默消失」。
#[test]
fn every_protocol_round_trips_through_its_wire_name() {
    for &p in ALL_PROTOCOLS {
        let w = wire(p);
        assert_eq!(
            w,
            w.to_ascii_lowercase(),
            "{p:?} 的 serde 名 `{w}` 含大写 —— sanitize 小写比对后必然匹配不上"
        );
        let back: Protocol = serde_json::from_str(&format!("\"{w}\""))
            .unwrap_or_else(|e| panic!("{p:?} 的 serde 名 `{w}` 读不回来：{e}"));
        assert_eq!(back, p, "{p:?} ⇄ `{w}` 往返不闭合");
    }
}

/// 🔴 落盘白名单与 serde 名双向一致。
///
/// 正向漏 → 导入侧产出的节点被 sanitize 静默丢掉；
/// 反向多 → 白名单里躺着一条谁也产不出的死条目（放行一个不存在的协议名）。
#[test]
fn allowed_protocols_matches_serde_names_both_ways() {
    let mut dupes = ALLOWED_PROTOCOLS.to_vec();
    dupes.sort_unstable();
    let before = dupes.len();
    dupes.dedup();
    assert_eq!(before, dupes.len(), "ALLOWED_PROTOCOLS 里有重复条目");

    for &p in ALL_PROTOCOLS {
        let w = wire(p);
        assert!(
            ALLOWED_PROTOCOLS.contains(&w.as_str()),
            "{p:?} 的 serde 名 `{w}` 不在 ALLOWED_PROTOCOLS 里 —— \
             该协议的节点会在 sanitize 那条 `continue` 上**静默消失**"
        );
    }
    for entry in ALLOWED_PROTOCOLS {
        assert!(
            serde_json::from_str::<Protocol>(&format!("\"{entry}\"")).is_ok(),
            "ALLOWED_PROTOCOLS 里的 `{entry}` 反序列化不成任何 Protocol —— \
             死条目：它放行的字符串，配置层根本读不了"
        );
    }
}

/// 🔴 前端的落盘形状镜像（`contracts/types.ts::Protocol`）与 Rust serde 名**精确相等**。
///
/// 这张表比 `NodeProto` 更权威：它是前端对「落盘 JSON 里 protocol 字段能是什么」的完整声明，
/// 组网协议也在其中。任一侧新增/改名而另一侧没跟，本条即红 —— 少一个是「前端读到的节点
/// 被当成未知类型」，多一个是「前端以为能写、后端读不了」。
#[test]
fn contracts_protocol_union_matches_serde_names() {
    let mut ts = contracts_protocol_members();
    let mut rs: Vec<String> = ALL_PROTOCOLS.iter().map(|&p| wire(p)).collect();
    ts.sort();
    rs.sort();
    assert_eq!(
        ts, rs,
        "contracts/types.ts 的 Protocol 与 Rust `Protocol` 的 serde 名对不上 —— \
         这是前端对落盘形状的镜像，两边必须逐字相等"
    );
}

/// 🔴 UI 写出来的每个协议字符串，Rust 都必须读得回来。
///
/// 这条是本轮缺陷的**直接判据**。反向也锁：声明「进对话框」的变体必须真在 `NodeProto` 里，
/// 声明「不进」的必须真不在 —— 否则豁免表会和现实脱节，变成一句无人核对的注释。
#[test]
fn node_proto_and_protocol_agree() {
    let members = node_proto_members();
    assert!(
        members.len() >= 10,
        "NodeProto 只解析出 {} 个成员，解析器多半瞎了：{members:?}",
        members.len()
    );

    let mirror = contracts_protocol_members();
    for m in &members {
        assert!(
            mirror.contains(m),
            "NodeProto 有 `{m}`，但 contracts/types.ts 的 Protocol 没有 —— \
             对话框能选的协议必须是落盘协议集的子集"
        );
        assert!(
            serde_json::from_str::<Protocol>(&format!("\"{m}\"")).is_ok(),
            "UI 的 NodeProto 有 `{m}`，但 Rust 的 Protocol 读不了它 —— \
             用户在对话框里建这个协议的节点，落盘后**整份 UserConfig 反序列化失败**\
             （不是丢这一个节点，是全部节点连同设置一起没了）"
        );
        assert!(
            ALLOWED_PROTOCOLS.contains(&m.as_str()),
            "UI 的 NodeProto 有 `{m}`，但它不在 ALLOWED_PROTOCOLS 里 —— sanitize 会静默丢掉该节点"
        );
    }

    for &p in ALL_PROTOCOLS {
        let w = wire(p);
        match ui_claim(p) {
            UiClaim::InDialog => assert!(
                members.contains(&w),
                "{p:?} 声明进节点对话框，但 `{w}` 不在 NodeProto 里 —— 用户建不出这个协议的节点"
            ),
            UiClaim::NotInDialog(why) => {
                assert!(!why.is_empty(), "{p:?} 的豁免没写理由");
                assert!(
                    !members.contains(&w),
                    "{p:?} 声明不进节点对话框（{why}），但 `{w}` 出现在 NodeProto 里 —— \
                     豁免表与现实脱节了，改一处即可，但要先确认哪边才是对的"
                );
            }
        }
    }
}

/// 每个协议归入三档之一，两个判据谓词与该档**逐条对拍**。
///
/// # 为什么需要这道门
///
/// `is_mesh_protocol` 从前叫 `is_endpoint_protocol`：**名字说的是「落 sing-box `endpoints[]`」，
/// 成员集给的却是「组网」**。两者在 openconnect / openvpn-client 上不重合，而消费点是按名字挑
/// 谓词的，于是同一个根因下三处各错各的 —— 临时测速核把它们塞进 `outbounds[]`（整核 FATAL）、
/// detour 指向它们不被丢弃（悬空 tag）、承流播种漏掉它们（该重启时不重启）。
///
/// 拆成两个谓词只治了已知的三处。**加新协议时没有任何东西提醒作者它属哪档**，缝会重新长出来。
/// 故此处穷尽 `match`：新增变体不归档就编译不过。两档的归属都不是自由填空，各有外部验证：
///  - `EndpointLeg` ⇒ 塞进 `outbounds[]` 内核报 `unknown outbound type`
///    （`config-engine/tests/kernel_accepts_outbounds.rs` 的真核门验这条）；
///  - `Mesh` ⇒ `endpoint_forced_route_cidrs` 里有该协议的网段来源分支（下一条门验这条）。
#[test]
fn every_protocol_is_filed_under_exactly_one_routing_class() {
    use polaris_config_engine::user_config::server_config::{is_mesh_protocol, lands_in_endpoints};

    #[derive(Debug, Clone, Copy)]
    enum Class {
        /// 组网：配置期可声明可达网段，且落 `endpoints[]`。
        Mesh,
        /// 仅 endpoint 腿：落 `endpoints[]`，但网段由服务端运行期 push、配置期不可知。
        EndpointLeg,
        /// 普通出站：落 `outbounds[]`。
        Outbound,
    }
    use Protocol::*;

    for &p in ALL_PROTOCOLS {
        let class = match p {
            Wireguard | Tailscale => Class::Mesh,
            Openconnect | OpenvpnClient => Class::EndpointLeg,
            Vless | Vmess | Trojan | Shadowsocks | Hysteria2 | Tuic | Socks | Http | Anytls
            | Naive | Snell | Ssh | Hysteria | Tor | Custom => Class::Outbound,
        };
        let want = match class {
            Class::Mesh => (true, true),
            Class::EndpointLeg => (false, true),
            Class::Outbound => (false, false),
        };
        assert_eq!(
            (is_mesh_protocol(p), lands_in_endpoints(p)),
            want,
            "{p:?} 归档 {class:?}，但 (is_mesh_protocol, lands_in_endpoints) 给的组合对不上"
        );
    }
}

/// 上一条门里 `Mesh` 那一档的外部判据：**组网协议必须真有网段来源**。
///
/// 判据取 `endpoint_forced_route_cidrs` 的实际产出而不是读它的源码分支 —— 后者是复刻，会漂移。
/// 反向也钉：普通出站协议即使填了 `meshRoutes` 也不该产出网段（那个字段只对 endpoint 腿有意义）。
#[test]
fn mesh_protocols_actually_have_a_cidr_source() {
    use polaris_config_engine::builder::endpoint_routes::endpoint_forced_route_cidrs;
    use polaris_config_engine::user_config::server_config::{
        is_mesh_protocol, ServerConfig, WireGuardSettings,
    };

    // WireGuard 的段来自 allowedIPs。
    let wg = ServerConfig {
        id: "wg".into(),
        protocol: Protocol::Wireguard,
        wireguard_settings: Some(Box::new(WireGuardSettings {
            allowed_ips: vec!["10.0.0.0/24".into()],
            ..Default::default()
        })),
        ..Default::default()
    };
    assert!(
        is_mesh_protocol(Protocol::Wireguard) && !endpoint_forced_route_cidrs(&wg).is_empty(),
        "WireGuard 声明为组网协议却拿不出网段 —— 判据与实现脱节"
    );

    // Tailscale 的段是协议固定的 tailnet 两族，连设置都不用填。
    let ts = ServerConfig {
        id: "ts".into(),
        protocol: Protocol::Tailscale,
        ..Default::default()
    };
    assert!(
        is_mesh_protocol(Protocol::Tailscale) && !endpoint_forced_route_cidrs(&ts).is_empty(),
        "Tailscale 声明为组网协议却拿不出网段"
    );

    // 反向：普通出站协议没有这套概念。
    let vless = ServerConfig {
        id: "v".into(),
        protocol: Protocol::Vless,
        ..Default::default()
    };
    assert!(endpoint_forced_route_cidrs(&vless).is_empty());
}
