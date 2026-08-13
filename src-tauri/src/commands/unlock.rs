//! 解锁检测类 command（上游 `unlock-handlers.ts`）。
//!
//! 映射 channel：
//! - `unlock:run` → [`unlock_run`]（跑一轮 AI/流媒体检测，force 绕 TTL；返回终态快照 + emit 事件）
//! - `unlock:get` → [`unlock_get`]（纯读最近快照，零网络）
//!
//! 编排（run/get/快照/事件/出口 pin/TTL/归属 bracket/受限收敛/warm 补测）全在
//! [`crate::runtime::unlock::UnlockRuntime`]；本层只做「gating + 出口 pin 客户端注入 + 调度 warm 补测」。

#![allow(clippy::needless_pass_by_value)]

use std::time::Duration;

use tauri::{AppHandle, State};

use polaris_config_engine::user_config::app_config::UserConfig;
use polaris_config_engine::user_config::proxy_mode::ProxyMode;
use polaris_unlock::detector::unix_millis;
use polaris_unlock::{is_restricted_egress_region, UnlockSnapshot, UnlockStatus};

use crate::response::ApiResponse;
use crate::runtime::tailscale_status::{
    is_definitive_logged_out, selected_ts_exit_blocked, TsExitWarningInput,
};
use crate::runtime::unlock::{
    unlock_gate_reason, BroadcastSink, UnlockEventSink, WARM_RECHECK_DELAY_MS,
};
use crate::runtime::AppRuntime;
use polaris_unlock_transport::UnlockClient;

/// item6：选中 TS 出口是否直判无效（`unlock_gate_reason` 的 `exit_blocked` 输入）。
///
/// 组装 [`TsExitWarningInput`]：当前配置的选中出口 + 直连模式 + 主核 running + 该节点 STATUS 末帧
/// （`peers`/`logged_in`）→ 纯谓词 [`selected_ts_exit_blocked`]。config 读失败/无选中 → false（保守，不误挡）。
///
/// **推侧未接线（DESIGN-REVIEW row31 push）**：本函数是**拉侧**（`unlock:run` 时按需求值）。Polaris
/// `reconcileTsExitBlock` 的**每帧 STATUS 翻转对账**（none↔blocked 主动 invalidate + R1/R2 恢复腿）仍缺
/// —— 出口失效→有效的自动重检、EditPrefs 热重设 exit_node 未接线，见任务报告 DESIGN-REVIEW。
fn compute_selected_exit_blocked(state: &AppRuntime, running: bool) -> bool {
    let Ok(value) = state.config.current() else {
        return false;
    };
    let Ok(cfg) = serde_json::from_value::<UserConfig>(value) else {
        return false;
    };
    let Some(sel_id) = cfg.selected_server_id.as_deref() else {
        return false;
    };
    let selected = cfg.servers.iter().find(|s| s.id == sel_id);
    let event = state.mesh.ts_status_event(sel_id);
    let (logged_in, peers, definitive_logged_out) =
        event.as_ref().map_or((false, &[][..], false), |e| {
            (e.logged_in, e.peers.as_slice(), is_definitive_logged_out(e))
        });
    selected_ts_exit_blocked(&TsExitWarningInput {
        selected,
        logged_in,
        proxy_mode_direct: cfg.proxy_mode == ProxyMode::Direct,
        proxy_running: running,
        peers,
        definitive_logged_out,
    })
}

/// **一轮解锁检测的完整编排**（gating → 出口 pin → run → warm 补测调度），**手动与自跑共用**。
///
/// # 为何抽出来
///
/// 触发源有两个：用户点「网络检测」（[`unlock_run`] command，force=true）与 invalidate 后的**主进程侧
/// 去抖自跑**（[`UnlockEventSink::schedule_self_run`](crate::runtime::unlock::UnlockEventSink::schedule_self_run)，
/// force=false）。两者必须走**同一条**编排，否则会重演本批修的缺陷形态：驱动侧和被驱动侧各持一份逻辑、
/// 其中一份漏了某个收口动作，UI 就永远停在「检测中」。
///
/// # 每条返回路径都 emit 终态
///
/// gating 短路 → emit blocked 快照；正常轮 → `UnlockRuntime::run` 内部 emit（TTL 快路 / S-gate / force-min /
/// notReady / commit 各路径均 emit）。唯一不 emit 的是归属校验丢弃腿，那条自带 invalidate → 排下一轮自跑。
/// 故前端拿得到终态是不变式，不靠调用方补。
///
/// `Err` = 建出口 pin 客户端失败（唯一硬失败面）。
pub async fn run_unlock_cycle(app: AppHandle, force: bool) -> Result<UnlockSnapshot, String> {
    // ── 同步段：取 Arc + 算 gating。Tauri `State` 守卫非 Send，**绝不可跨 await**，故限定在此块内。──
    let (unlock, running, mixed_port, exit_blocked) = {
        use tauri::Manager;
        let Some(state) = app.try_state::<AppRuntime>() else {
            // setup 前极早期 / 关停中 → 静默放弃（同 proxy.rs try_state 范式，绝不 panic）。
            return Err("AppRuntime 尚未装配".to_string());
        };
        let status = state.proxy.status();
        let exit_blocked = compute_selected_exit_blocked(&state, status.running);
        (
            state.unlock.clone(),
            status.running,
            status.mixed_port,
            exit_blocked,
        )
    };

    // ── gating（SoT `unlock_gate_reason`）：核未运行 → ProxyNotRunning；选中 TS 出口直判无效 → ExitInvalid
    //    （不空跑死出口检测）。blocked 快照不缓存，emit UPDATED 让前端 spinner 复位 ──
    if let Some(reason) = unlock_gate_reason(running, mixed_port, exit_blocked) {
        // info 而非 warn：这是**预期**短路（没开代理就点检测 / 出口没选好），且前端会显示明确的 blocked 态，
        // 不是「卡住」。真机 logLevel=warn 下不刷屏。
        log::info!("解锁检测短路：{reason:?}（running={running}，mixed_port={mixed_port}，exit_blocked={exit_blocked}）");
        let snap = UnlockSnapshot::blocked(reason);
        BroadcastSink::new(&app).updated(&snap);
        return Ok(snap);
    }

    // ── 出口 pin：经本机 mixed 端口建客户端（检测走当前分流出口）──
    //
    // **传输层 = `UnlockClient`（wreq + Chrome 131 指纹伪装），不是共享的 reqwest `HttpRuntime`**：
    // CF 按 TLS/JA3 指纹判自动化，rustls 形态会吃 1020/403（见 `polaris-unlock-transport` 模块文档）。
    // 出口 pin 语义与原 `HttpRuntime::via_local_proxy` 等价（同一本机 mixed 口的 HTTP CONNECT）。
    let http = UnlockClient::via_local_proxy(mixed_port).map_err(|e| {
        // warn：建不出客户端 = 这一轮**零 emit**，前端停在检测中。属「没有最终结果」的一种，必须可见。
        log::warn!("解锁检测：建出口 pin 客户端失败（mixed_port={mixed_port}）：{e}");
        e
    })?;
    let epoch0 = unlock.epoch();
    let snap = unlock
        .run(&http, &BroadcastSink::new(&app), force, unix_millis)
        .await;

    // ── warm 补测调度（#6）：partial-timeout（含 timeout 但非全超/受限）→ 5s 后定向重打 ──
    let restricted =
        is_restricted_egress_region(snap.egress.as_ref().and_then(|e| e.region.as_deref()));
    let has_timeout = snap
        .results
        .values()
        .any(|r| r.status == UnlockStatus::Timeout);
    let schedule_recheck = snap.checked_at.is_some()
        && has_timeout
        && !restricted
        && snap.low_confidence != Some(true);
    if schedule_recheck {
        let unlock2 = unlock.clone();
        let app2 = app.clone();
        let port = mixed_port;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(WARM_RECHECK_DELAY_MS)).await;
            // epoch 守卫：调度期间有 invalidate（切节点/起停）→ 取消（别测旧出口）。
            if unlock2.epoch() != epoch0 {
                return;
            }
            if let Ok(h) = UnlockClient::via_local_proxy(port) {
                let _ = unlock2
                    .run_recheck(&h, &BroadcastSink::new(&app2), epoch0, unix_millis)
                    .await;
            }
        });
    }

    Ok(snap)
}

/// 上游 `UNLOCK_RUN`：跑一轮解锁检测（返回完整快照 + 广播 progress/updated）。
///
/// async：检测经代理出口发真实 HTTPS（含 8s/请求超时 + 并发齐射），绝不可阻塞命令线程。
/// 编排全在 [`run_unlock_cycle`]（与去抖自跑共用），本函数只做参数解包 + 响应包装。
#[tauri::command]
pub async fn unlock_run(
    app: AppHandle,
    force: Option<bool>,
) -> Result<ApiResponse<UnlockSnapshot>, ()> {
    Ok(match run_unlock_cycle(app, force.unwrap_or(false)).await {
        Ok(snap) => ApiResponse::ok(snap),
        Err(e) => ApiResponse::err(e),
    })
}

/// 上游 `UNLOCK_GET`：纯读最近快照（页面挂载水合，零网络）；无/过期/停代理 → null。
///
/// 停代理即缓存失效（不 serve 陈旧快照）——invalidate 契约的自证腿，无需生命周期接线也成立。
#[tauri::command]
pub fn unlock_get(state: State<'_, AppRuntime>) -> ApiResponse<Option<UnlockSnapshot>> {
    if !state.proxy().status().running {
        return ApiResponse::ok(None);
    }
    ApiResponse::ok(state.unlock().peek(unix_millis()))
}
