//! Tailscale STATUS 帧解码：sing-box 管理 API `TailscaleStatusUpdate`（proto）→ 前端契约事件。
//!
//! # 为什么解码在 src-tauri 而不在 mesh crate
//!
//! `polaris-mesh` 是纯逻辑 crate，**不依赖** `polaris-singbox-grpc`（tonic/prost）——让它依赖会把
//! gRPC 传输层拖进 mesh 决策层。而本解码的输入就是 proto 生成类型（`daemon::TailscaleStatusUpdate`），
//! 故落在 src-tauri（既定的「proto → domain 投影」注入点，同 `runtime/management_api.rs` 把
//! `daemon::Connection → ConnectionSnapshot` 的手法）。
//!
//! # 数据链
//!
//! `SubscribeTailscaleStatus` 流每帧 = **全量端点快照**（所有 tailscale endpoint）。本模块把它逐端点投影成
//! [`TailscaleStatusEvent`]（前端 `contracts/tailscale-status.ts` 的 1:1 镜像，serde 字段名对齐 camelCase）：
//! - `endpointTag → serverId`：经 `tag_to_id`（`build_id_to_tag_map` 的逆，仅当前运行配置在册的 tailscale 节点）。
//!   **不在册的端点（幽灵/历史）直接丢弃**（前端契约「幽灵条目已过滤」）。
//! - `loggedIn = (backendState ∈ {Running, Starting}) 且 self 未过期`（1.14 登录成功信号，对齐前端契约）。
//! - `peers`：摊平 `userGroups[].peers[]` + 按 hostName 去重，投影成 UI lean 形态。
//!
//! 契约唯一真值 = `ui/src/contracts/tailscale-status.ts`（跨层三方共享）。改字段先改那份。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use polaris_singbox_grpc::daemon;

/// 对端节点 lean 形态（`contracts/tailscale-status.ts` `TailscaleStatusPeer` 镜像）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TailscaleStatusPeer {
    /// 主机名。
    pub host_name: String,
    /// 内网 IP：首个 IPv4（tailnet 100.x），无则首个 IP。
    pub ip: String,
    /// 该 peer 在 tailnet 上是否 up。
    pub online: bool,
    /// 是否当前被本节点选中作出口。
    pub exit_node: bool,
    /// 是否广告了可当出口（出口下拉候选判据）。
    pub exit_node_option: bool,
    /// 近期是否有活跃直连/流量。
    pub active: bool,
    /// tailnet stableID（主进程热重设 exit_node 用；UI 不消费，旧核/无 ID → None）。
    #[serde(rename = "stableID", skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
}

/// 单个 Tailscale endpoint 的状态事件（`contracts/tailscale-status.ts` `TailscaleStatusEvent` 镜像）。
///
/// 既是 `EVENT_TAILSCALE_STATUS` 的推送载荷（逐 endpoint 发一条），也是 [`TailscaleStatusSnapshot`]
/// 的成员（`TAILSCALE_GET_STATUS` 拉末帧）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TailscaleStatusEvent {
    /// 节点 id（由 `endpointTag` 经 tag→id 逆映射得到）。
    pub server_id: String,
    /// NoState | NeedsLogin | Starting | Running | …
    pub backend_state: String,
    /// loggedIn =（Running||Starting）且 key 未过期。
    pub logged_in: bool,
    /// NeedsLogin 时的交互登录 URL（主核路径带；空 → None）。
    #[serde(rename = "authURL", skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
    /// 本节点自身内网 IP（self.tailscaleIPs）。
    #[serde(rename = "tailscaleIPs")]
    pub tailscale_ips: Vec<String>,
    /// key 是否过期。
    pub expired: bool,
    /// 对端列表（摊平 userGroups 各组 + 去重）。
    pub peers: Vec<TailscaleStatusPeer>,
    /// Taildrop **能力位**：tailnet 是否授了 `https://tailscale.com/cap/file-sharing`。
    ///
    /// 这是 UI 的门，不是用户开关 —— 未授时内核照常跑，只是收发不成立。不拿它当门就会做出
    /// 「点了没反应」的界面（本仓为 `allowInternet` / `resolveByName` 两次记过同一条教训）。
    ///
    /// 旧核（< 1.14.0-beta.15）没有这个字段 ⇒ prost 给 proto3 标量的缺省 `false`，UI 落到
    /// 「此 tailnet 未启用文件共享」那一档。**这个降级是对的**：换了没有 Taildrop 的核，
    /// 收发本来也不成立。
    pub can_share_files: bool,
    /// 已落盘待处理的文件数。
    pub waiting_file_count: i32,
    /// 正在接收中的文件数。
    pub receiving_file_count: i32,
    /// 未读数（`MarkTaildropInboxRead` 清零）。角标取它而不是 waiting：读过但没删的文件
    /// 仍在 waiting 里，拿 waiting 当角标会让角标永远消不掉。
    pub unread_file_count: i32,
}

/// `TAILSCALE_GET_STATUS` 返回：缓存末帧 + 新鲜度（`contracts/tailscale-status.ts` `TailscaleStatusSnapshot`）。
///
/// `connected` = 主核是否在运行（=状态流是否 live）。false → `statuses` 为上次已知/空（renderer 灰显动态位）。
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TailscaleStatusSnapshot {
    pub connected: bool,
    pub statuses: Vec<TailscaleStatusEvent>,
}

/// 取对端/自身的展示 IP：首个 IPv4（不含 `:`，tailnet 100.x），无则首个，全无则空串。
/// 对齐前端契约「首个 IPv4(100.x)，无则首个 IP」。
fn pick_ip(ips: &[String]) -> String {
    ips.iter()
        .find(|ip| !ip.contains(':'))
        .or_else(|| ips.first())
        .cloned()
        .unwrap_or_default()
}

/// proto peer → UI lean peer。`stable_id` 空串 → None（旧核/无 ID）。
fn lean_peer(p: &daemon::TailscalePeer) -> TailscaleStatusPeer {
    TailscaleStatusPeer {
        host_name: p.host_name.clone(),
        ip: pick_ip(&p.tailscale_i_ps),
        online: p.online,
        exit_node: p.exit_node,
        exit_node_option: p.exit_node_option,
        active: p.active,
        stable_id: if p.stable_id.is_empty() {
            None
        } else {
            Some(p.stable_id.clone())
        },
    }
}

/// 摊平 `userGroups[].peers[]` + 按 hostName 去重（首见保留，对齐前端出口下拉 `seen.has(hostName)` 去重）。
fn flatten_peers(groups: &[daemon::TailscaleUserGroup]) -> Vec<TailscaleStatusPeer> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for g in groups {
        for p in &g.peers {
            if seen.insert(p.host_name.clone()) {
                out.push(lean_peer(p));
            }
        }
    }
    out
}

/// 解码一帧全量端点快照 → 前端事件集。
///
/// `tag_to_id` = 当前运行配置的 `tag → serverId`（`build_id_to_tag_map` 的逆）。**端点 tag 不在其中 → 丢弃**
/// （幽灵/历史端点过滤，前端契约要求）。`loggedIn` 判定 = backendState ∈ {Running, Starting} 且 self 未过期。
#[must_use]
pub fn decode_tailscale_status(
    update: &daemon::TailscaleStatusUpdate,
    tag_to_id: &BTreeMap<String, String>,
) -> Vec<TailscaleStatusEvent> {
    update
        .endpoints
        .iter()
        .filter_map(|ep| {
            // 幽灵过滤：端点 tag 不对应任何在册节点 → 丢弃（不 emit、不进缓存）。
            let server_id = tag_to_id.get(&ep.endpoint_tag)?.clone();
            let expired = ep.self_.as_ref().is_some_and(|s| s.expired);
            let logged_in = matches!(ep.backend_state.as_str(), "Running" | "Starting") && !expired;
            let auth_url = if ep.auth_url.is_empty() {
                None
            } else {
                Some(ep.auth_url.clone())
            };
            let tailscale_ips = ep
                .self_
                .as_ref()
                .map(|s| s.tailscale_i_ps.clone())
                .unwrap_or_default();
            Some(TailscaleStatusEvent {
                server_id,
                backend_state: ep.backend_state.clone(),
                logged_in,
                auth_url,
                tailscale_ips,
                expired,
                peers: flatten_peers(&ep.user_groups),
                can_share_files: ep.can_share_files,
                waiting_file_count: ep.waiting_file_count,
                receiving_file_count: ep.receiving_file_count,
                unread_file_count: ep.unread_file_count,
            })
        })
        .collect()
}

// ── item6 / row31：选中 TS 出口无效直判（`ProxyExitBlock` 信号源的纯谓词核）─────────────────────
//
// 1:1 移植自 上游 `shared/tailscale-exit-warning.ts`。解锁 gating（`unlock_gate_reason`）与状态栏
// 出口角标共用此谓词判「选中 TS 出口是否失效」，避免死出口检测空转就绪门数十秒。

use polaris_config_engine::user_config::server_config::{Protocol, ServerConfig};

/// TS 出口告警（前三态 1:1 上游 `TsExitWarning`；`NeedsAuth` 为本仓新增）。`None` = 无告警。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsExitWarning {
    /// 出口有效或不适用（未选 TS / 直连 / 非终局的未登录）。
    None,
    /// 选中 TS 作出口，但控制面明说这份凭据不能用（NeedsLogin / NeedsMachineAuth / 已过期）
    /// ⇒ 该 endpoint 从未认证成功，**永不承载流量**。**上游 无此态**（其渲染层拿不到
    /// `backendState`，只有一次性登录 toast），2026-07-31 真机上正是这一格静默：
    /// 日志 `Waiting for authentication` ×6、`Running` ×0、tailscale outbound 仅 2 条计数，
    /// 而 UI 全绿。判据是控制面终局否定，不是超时猜测。
    NeedsAuth,
    /// 选中 TS 但无 exit_node（公网不经 TS）。
    NoExitDevice,
    /// exit_node 对应 peer 离线。
    ExitDeviceOffline,
    /// exit_node 对应 peer 在线但未广告可当出口 → 流量出不去。
    ExitDeviceNotAdvertised,
}

/// 这一帧 STATUS 是否**终局否定**（definitive-out：控制面明说凭据不能用）。
///
/// 与前端 `domain/tailscale-conn-state.ts::isDefinitiveTsLoginFrame` 的 definitive-out 分支同口径
/// （那边多一条 `loggedIn → true` 的 definitive-in，是登录态判决门用的；此处只问「否定得算数吗」）。
/// **启动过渡帧不算**：`NoState` / `Stopped` 折叠出的 `logged_in=false` 说的是「核还没启完」。
#[must_use]
pub fn is_definitive_logged_out(ev: &TailscaleStatusEvent) -> bool {
    !ev.logged_in
        && (ev.expired || matches!(ev.backend_state.as_str(), "NeedsLogin" | "NeedsMachineAuth"))
}

/// [`derive_ts_exit_warning`] 输入（1:1 上游 `TsExitWarningInput`）。
pub struct TsExitWarningInput<'a> {
    /// 当前选中的出口节点（`selectedServerId` 对应；None = 未选中）。
    pub selected: Option<&'a ServerConfig>,
    /// 选中 TS 节点是否已登录（STATUS backendState ∈ {Running,Starting} 且 self 未过期）。
    pub logged_in: bool,
    /// 是否显式全直连模式（direct）。
    pub proxy_mode_direct: bool,
    /// 主核是否运行（= STATUS 流 live；离线/未认证判定均须新鲜帧，防据陈旧帧误判）。
    pub proxy_running: bool,
    /// 选中 TS 节点末帧 peers（STATUS 缓存）。
    pub peers: &'a [TailscaleStatusPeer],
    /// 该帧是否**终局否定**（[`is_definitive_logged_out`]；无帧 → false）。与 `peers`/`logged_in`
    /// 必须取自**同一帧**，调用方一次 `map_or` 一并投影。
    pub definitive_logged_out: bool,
}

/// 选中 TS 出口无效判定（纯谓词，1:1 上游 `deriveTsExitWarning`）。判定顺序即 §G 方向反转口径：
/// 未选 TS / 直连 / 未登录 → 永不告警；有 TS 但无 exit_node → NoExitDevice（配置态，断开也提示）；
/// 有 exit_node 但需新鲜 STATUS 才判 peer 离线/未广告（`proxy_running=false` 时保守返 None 防陈旧误报）。
#[must_use]
pub fn derive_ts_exit_warning(i: &TsExitWarningInput) -> TsExitWarning {
    let Some(s) = i.selected else {
        return TsExitWarning::None; // 未选中 → 永不告警
    };
    if s.protocol != Protocol::Tailscale {
        return TsExitWarning::None; // 非 TS 出口 → 方向反转不适用
    }
    if i.proxy_mode_direct {
        return TsExitWarning::None; // 显式全直连
    }
    // 认证态优先：endpoint 没认证成功就根本不承载流量，此时报「没选出口设备」是指错方向。
    // 三重门：核在跑（帧新鲜）+ 该帧终局否定（无帧 → definitive_logged_out=false，不猜）。
    if i.proxy_running && i.definitive_logged_out {
        return TsExitWarning::NeedsAuth;
    }
    if !i.logged_in {
        return TsExitWarning::None; // 其余未登录（启动过渡/无帧）：登录角标/toast 已 own，不叠加
    }
    let exit_node = s
        .tailscale_settings
        .as_ref()
        .and_then(|t| t.exit_node.as_deref())
        .map(str::trim)
        .filter(|e| !e.is_empty());
    let Some(exit_node) = exit_node else {
        return TsExitWarning::NoExitDevice; // 无 exit_node → 公网不经 TS（不信 allowInternet）
    };
    if !i.proxy_running {
        return TsExitWarning::None; // offline/未广告判定须新鲜 STATUS，陈旧 snapshot 会误报
    }
    // exit_node 值与 peer 匹配（ip / hostName 口径）；匹配到才判，自定义值不匹配 → 不误报。
    let peer = i
        .peers
        .iter()
        .find(|p| p.ip == exit_node || p.host_name == exit_node);
    if let Some(p) = peer {
        if !p.online {
            return TsExitWarning::ExitDeviceOffline; // 离线优先（离线态 exit_node_option 可能陈旧）
        }
        if !p.exit_node_option {
            return TsExitWarning::ExitDeviceNotAdvertised; // 在线但未广告出口 → 流量出不去
        }
    }
    TsExitWarning::None
}

/// 「选中 TS 出口是否失效」布尔（上游 `selectedTsExitBlock` 的 bool 投影；供 [`crate::runtime::unlock::
/// unlock_gate_reason`] 的 `exit_blocked` 输入）。新鲜度守卫已内建于 [`derive_ts_exit_warning`]
/// （offline/not-advertised 在 `proxy_running=false` 时已提前返 None），故此处 `warning != None` 即为失效。
#[must_use]
pub fn selected_ts_exit_blocked(i: &TsExitWarningInput) -> bool {
    !matches!(derive_ts_exit_warning(i), TsExitWarning::None)
}

#[cfg(test)]
mod exit_warning_tests {
    //! §H.2 出口无效谓词矩阵（协议 × proxyMode × loggedIn × exitNode × running × peer 三态）。
    use super::*;
    use polaris_config_engine::user_config::server_config::TailscaleSettings;

    fn ts_server(exit_node: Option<&str>) -> ServerConfig {
        ServerConfig {
            id: "ts1".into(),
            name: "ts".into(),
            protocol: Protocol::Tailscale,
            tailscale_settings: Some(TailscaleSettings {
                exit_node: exit_node.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn peer(host: &str, ip: &str, online: bool, advertises: bool) -> TailscaleStatusPeer {
        TailscaleStatusPeer {
            host_name: host.into(),
            ip: ip.into(),
            online,
            exit_node: false,
            exit_node_option: advertises,
            active: false,
            stable_id: None,
        }
    }

    fn base<'a>(
        selected: Option<&'a ServerConfig>,
        peers: &'a [TailscaleStatusPeer],
    ) -> TsExitWarningInput<'a> {
        TsExitWarningInput {
            selected,
            logged_in: true,
            proxy_mode_direct: false,
            proxy_running: true,
            peers,
            definitive_logged_out: false,
        }
    }

    /// `NeedsAuth` 优先于其余各条，且**只认终局否定 + 核在跑**。
    ///
    /// 变异表（逐条真跑过）：
    /// - 把 `proxy_running && definitive_logged_out` 的 `proxy_running` 删 → 停核 case 转红；
    /// - 把该判据整段挪到 `!logged_in` 守卫**之后** → 第一个断言拿到 `None`（被守卫吞掉）转红；
    /// - 把它挪到 `no_exit_node` 之后 → 第二个断言拿到 `NoExitDevice` 转红；
    /// - 把 `definitive_logged_out` 换成 `!logged_in` → 「启动过渡帧」case 从 `None` 变 `NeedsAuth` 转红。
    #[test]
    fn needs_auth_is_definitive_only_and_outranks_exit_device_faults() {
        let s = ts_server(Some("exit-host"));
        let peers = [peer("exit-host", "100.64.0.5", true, true)];

        // 终局否定 + 核在跑 → NeedsAuth（即便出口设备本身完全健康）。
        let mut i = base(Some(&s), &peers);
        i.logged_in = false;
        i.definitive_logged_out = true;
        assert_eq!(derive_ts_exit_warning(&i), TsExitWarning::NeedsAuth);
        assert!(selected_ts_exit_blocked(&i));

        // 同为终局否定，但**未配 exit_node** → 仍报 NeedsAuth（根因先行，不指错方向）。
        let no_exit = ts_server(None);
        let mut i2 = base(Some(&no_exit), &[]);
        i2.logged_in = false;
        i2.definitive_logged_out = true;
        assert_eq!(derive_ts_exit_warning(&i2), TsExitWarning::NeedsAuth);

        // 核没跑 → 帧陈旧，不据其报未认证（浏览器里补完的登录我们收不到）。
        let mut stale = base(Some(&s), &peers);
        stale.logged_in = false;
        stale.definitive_logged_out = true;
        stale.proxy_running = false;
        assert_eq!(derive_ts_exit_warning(&stale), TsExitWarning::None);

        // 启动过渡帧（NoState/Stopped 折叠出的 logged_in=false，非终局）→ 静默。
        let mut transitional = base(Some(&s), &peers);
        transitional.logged_in = false;
        transitional.definitive_logged_out = false;
        assert_eq!(derive_ts_exit_warning(&transitional), TsExitWarning::None);

        // 直连模式在认证态之前短路（用户显式全直连，TS 出口不适用）。
        let mut direct = base(Some(&s), &peers);
        direct.logged_in = false;
        direct.definitive_logged_out = true;
        direct.proxy_mode_direct = true;
        assert_eq!(derive_ts_exit_warning(&direct), TsExitWarning::None);
    }

    /// [`is_definitive_logged_out`] 的取值面：只有 NeedsLogin / NeedsMachineAuth / expired 算数，
    /// 且 `logged_in=true` 一律不算（后端已确认在跑）。变异：去掉 `!ev.logged_in` 前置 → 首条转红；
    /// 把 `NeedsMachineAuth` 删 → 第三条转红；把 `expired` 删 → 第四条转红。
    #[test]
    fn definitive_logged_out_matrix() {
        let ev = |backend: &str, logged_in: bool, expired: bool| TailscaleStatusEvent {
            server_id: "ts1".into(),
            backend_state: backend.into(),
            logged_in,
            auth_url: None,
            tailscale_ips: vec![],
            expired,
            peers: vec![],
            // Taildrop 四位在本用例无关，取「无能力、无文件」的中性值；不给 Default 是刻意的：
            // 日后再加字段时，这些构造点必须重新被人看一眼，而不是被 `..Default::default()` 静默补齐。
            can_share_files: false,
            waiting_file_count: 0,
            receiving_file_count: 0,
            unread_file_count: 0,
        };
        assert!(!is_definitive_logged_out(&ev("Running", true, false)));
        // `logged_in=true` 一票否决，**即便**同帧带着否定信号。这两格 [`decode_tailscale_status`]
        // 造不出来（那里 `logged_in = backendState ∈ {Running,Starting} && !expired`），但谓词是
        // pub、判据独立于解码器：没有这两条，删掉 `!ev.logged_in` 前置的变异会**存活**（实测如此）。
        assert!(!is_definitive_logged_out(&ev("NeedsLogin", true, false)));
        assert!(!is_definitive_logged_out(&ev("Running", true, true)));
        assert!(is_definitive_logged_out(&ev("NeedsLogin", false, false)));
        assert!(is_definitive_logged_out(&ev(
            "NeedsMachineAuth",
            false,
            false
        )));
        // 过期与 backendState 正交：Running 但 key 过期，后端已折叠成 logged_in=false。
        assert!(is_definitive_logged_out(&ev("Running", false, true)));
        // 启动过渡态：不知道 ≠ 否定。
        assert!(!is_definitive_logged_out(&ev("NoState", false, false)));
        assert!(!is_definitive_logged_out(&ev("Starting", false, false)));
        assert!(!is_definitive_logged_out(&ev("Stopped", false, false)));
    }

    /// 有 TS 出口但无 exit_node → NoExitDevice（断开态也报）。
    #[test]
    fn no_exit_node_is_no_exit_device() {
        let s = ts_server(None);
        assert_eq!(
            derive_ts_exit_warning(&base(Some(&s), &[])),
            TsExitWarning::NoExitDevice
        );
        // 断开态（proxy_running=false）仍报（配置态，与 running 无关）。
        let mut i = base(Some(&s), &[]);
        i.proxy_running = false;
        assert_eq!(derive_ts_exit_warning(&i), TsExitWarning::NoExitDevice);
        assert!(selected_ts_exit_blocked(&base(Some(&s), &[])));
    }

    /// 未选中 / 非 TS / 直连 / 未登录 → 永不告警（四条抑制路径，逐条变异删任一 → 该 case 转红）。
    #[test]
    fn suppressed_paths_never_warn() {
        let s = ts_server(None);
        // 未选中
        assert_eq!(
            derive_ts_exit_warning(&base(None, &[])),
            TsExitWarning::None
        );
        // 非 TS
        let vless = ServerConfig {
            protocol: Protocol::Vless,
            ..ts_server(None)
        };
        assert_eq!(
            derive_ts_exit_warning(&base(Some(&vless), &[])),
            TsExitWarning::None
        );
        // 直连
        let mut d = base(Some(&s), &[]);
        d.proxy_mode_direct = true;
        assert_eq!(derive_ts_exit_warning(&d), TsExitWarning::None);
        // 未登录
        let mut nl = base(Some(&s), &[]);
        nl.logged_in = false;
        assert_eq!(derive_ts_exit_warning(&nl), TsExitWarning::None);
    }

    /// exit_node 匹配到离线 peer → ExitDeviceOffline；在线但未广告 → ExitDeviceNotAdvertised；
    /// 在线且广告 → None。新鲜度守卫：proxy_running=false → None（防陈旧）。
    /// 变异：把 offline 分支删 → 离线 case 落到 not-advertised 或 None → 转红。
    #[test]
    fn peer_state_drives_offline_and_not_advertised() {
        let s = ts_server(Some("exit-host"));
        // 离线
        let offline = [peer("exit-host", "100.64.0.5", false, true)];
        assert_eq!(
            derive_ts_exit_warning(&base(Some(&s), &offline)),
            TsExitWarning::ExitDeviceOffline
        );
        // 在线未广告
        let not_adv = [peer("exit-host", "100.64.0.5", true, false)];
        assert_eq!(
            derive_ts_exit_warning(&base(Some(&s), &not_adv)),
            TsExitWarning::ExitDeviceNotAdvertised
        );
        // 在线且广告 → 有效
        let ok = [peer("exit-host", "100.64.0.5", true, true)];
        assert_eq!(
            derive_ts_exit_warning(&base(Some(&s), &ok)),
            TsExitWarning::None
        );
        assert!(!selected_ts_exit_blocked(&base(Some(&s), &ok)));
        // 新鲜度守卫：流断 → 保守 None（不据陈旧 peers 报离线）。
        let mut stale = base(Some(&s), &offline);
        stale.proxy_running = false;
        assert_eq!(derive_ts_exit_warning(&stale), TsExitWarning::None);
    }

    /// exit_node 自定义值不匹配任何 peer → 不误报（None）。
    #[test]
    fn unmatched_exit_node_does_not_false_warn() {
        let s = ts_server(Some("custom-value"));
        let peers = [peer("other-host", "100.64.0.9", true, true)];
        assert_eq!(
            derive_ts_exit_warning(&base(Some(&s), &peers)),
            TsExitWarning::None
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag_map() -> BTreeMap<String, String> {
        // tag "东京 03" → serverId "srv-tokyo"（build_id_to_tag_map 逆映射的一条）。
        BTreeMap::from([("东京 03".to_string(), "srv-tokyo".to_string())])
    }

    fn peer(host: &str, ips: &[&str]) -> daemon::TailscalePeer {
        daemon::TailscalePeer {
            host_name: host.to_string(),
            tailscale_i_ps: ips.iter().map(|s| s.to_string()).collect(),
            online: true,
            exit_node_option: true,
            stable_id: "sid".to_string(),
            ..Default::default()
        }
    }

    fn running_endpoint(tag: &str) -> daemon::TailscaleEndpointStatus {
        daemon::TailscaleEndpointStatus {
            endpoint_tag: tag.to_string(),
            backend_state: "Running".to_string(),
            auth_url: String::new(),
            self_: Some(daemon::TailscalePeer {
                host_name: "self".to_string(),
                tailscale_i_ps: vec!["100.64.0.9".to_string()],
                expired: false,
                ..Default::default()
            }),
            user_groups: vec![daemon::TailscaleUserGroup {
                peers: vec![peer("box-a", &["100.64.0.1"])],
            }],
            exit_node: None,
            // stateText / networkName / magicDNSSuffix / keyAuth：1.14 beta 期真核新增，本解码器
            // 不消费（`decode_tailscale_status` 只取 tag/state/self/userGroups），故 fixture 留缺省。
            ..Default::default()
        }
    }

    /// 幽灵过滤：tag 不在 tag_to_id → 丢弃。打断（不过滤 / 用空串兜底 id）→ 转红。
    #[test]
    fn ghost_endpoint_filtered_out() {
        let update = daemon::TailscaleStatusUpdate {
            endpoints: vec![running_endpoint("不在册的节点")],
        };
        let out = decode_tailscale_status(&update, &tag_map());
        assert!(out.is_empty(), "tag 不在册 → 端点必须被丢弃（幽灵过滤）");
    }

    /// 在册端点 → 映射 serverId + backendState Running → loggedIn=true + self IP + peers 摊平。
    #[test]
    fn running_endpoint_maps_to_logged_in_event() {
        let update = daemon::TailscaleStatusUpdate {
            endpoints: vec![running_endpoint("东京 03")],
        };
        let out = decode_tailscale_status(&update, &tag_map());
        assert_eq!(out.len(), 1);
        let ev = &out[0];
        assert_eq!(ev.server_id, "srv-tokyo");
        assert_eq!(ev.backend_state, "Running");
        assert!(ev.logged_in, "Running 且未过期 → loggedIn");
        assert_eq!(ev.tailscale_ips, vec!["100.64.0.9".to_string()]);
        assert_eq!(ev.peers.len(), 1);
        assert_eq!(ev.peers[0].host_name, "box-a");
        assert_eq!(ev.peers[0].ip, "100.64.0.1");
        assert_eq!(ev.peers[0].stable_id.as_deref(), Some("sid"));
    }

    /// loggedIn 判定：key 过期 → 即使 Running 也 loggedIn=false。打断「且未过期」→ 转红。
    #[test]
    fn expired_key_forces_logged_out_even_when_running() {
        let mut ep = running_endpoint("东京 03");
        ep.self_.as_mut().unwrap().expired = true;
        let update = daemon::TailscaleStatusUpdate {
            endpoints: vec![ep],
        };
        let ev = &decode_tailscale_status(&update, &tag_map())[0];
        assert!(ev.expired);
        assert!(!ev.logged_in, "key 过期 → 不算登录（防陈旧绿标）");
    }

    /// NeedsLogin + authURL → loggedIn=false + authUrl 携带。打断「Running/Starting 才 loggedIn」（如恒 true）→ 转红。
    #[test]
    fn needs_login_carries_auth_url_and_not_logged_in() {
        let mut ep = running_endpoint("东京 03");
        ep.backend_state = "NeedsLogin".to_string();
        ep.auth_url = "https://login.tailscale.com/a/abc".to_string();
        let update = daemon::TailscaleStatusUpdate {
            endpoints: vec![ep],
        };
        let ev = &decode_tailscale_status(&update, &tag_map())[0];
        assert!(!ev.logged_in);
        assert_eq!(
            ev.auth_url.as_deref(),
            Some("https://login.tailscale.com/a/abc")
        );
    }

    /// peers 去重（同 hostName 只留一条）+ IPv4 优先取 IP。打断去重 → len 转红；打断 pick_ip → ip 转红。
    #[test]
    fn peers_dedup_by_hostname_and_prefer_ipv4() {
        let mut ep = running_endpoint("东京 03");
        ep.user_groups = vec![
            daemon::TailscaleUserGroup {
                peers: vec![peer("dup", &["fd7a::1", "100.64.0.5"])],
            },
            daemon::TailscaleUserGroup {
                peers: vec![peer("dup", &["100.64.0.6"])], // 同名 → 去重丢弃
            },
        ];
        let ev = &decode_tailscale_status(&update_of(ep), &tag_map())[0];
        assert_eq!(ev.peers.len(), 1, "同 hostName 去重");
        assert_eq!(ev.peers[0].ip, "100.64.0.5", "IPv4 优先于 v6");
    }

    fn update_of(ep: daemon::TailscaleEndpointStatus) -> daemon::TailscaleStatusUpdate {
        daemon::TailscaleStatusUpdate {
            endpoints: vec![ep],
        }
    }

    /// serde 字段名对齐前端契约（authURL / tailscaleIPs / stableID / serverId / backendState / loggedIn）。
    /// 打断任一 rename → JSON key 变 → 前端 duck-typing 读不到 → 此断言转红。
    #[test]
    fn serialized_field_names_match_frontend_contract() {
        let update = daemon::TailscaleStatusUpdate {
            endpoints: vec![running_endpoint("东京 03")],
        };
        let ev = &decode_tailscale_status(&update, &tag_map())[0];
        let v = serde_json::to_value(ev).unwrap();
        assert!(v.get("serverId").is_some());
        assert!(v.get("backendState").is_some());
        assert!(v.get("loggedIn").is_some());
        assert!(v.get("tailscaleIPs").is_some());
        let peer = &v["peers"][0];
        assert!(peer.get("hostName").is_some());
        assert!(peer.get("exitNodeOption").is_some());
        assert_eq!(peer.get("stableID").and_then(|x| x.as_str()), Some("sid"));
    }
}
