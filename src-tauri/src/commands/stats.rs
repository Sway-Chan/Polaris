//! stats 订阅类 command（上游 `stats-subscription-handlers.ts`）。
//!
//! 映射 channel：
//! - `stats:subscribe` → [`stats_subscribe`]（topic = stats | aggregate | detail | closed）
//! - `stats:unsubscribe` → [`stats_unsubscribe`]
//!
//! Polaris 按 webContents.sender 记账；Tauri 按 webview label（window label）记账。

#![allow(clippy::needless_pass_by_value)]

use tauri::{AppHandle, State, WebviewWindow};

use crate::events::{broadcast, channel::EVENT_CONNECTIONS_CLOSED};
use crate::response::{ok_void, ApiResponse};
use crate::runtime::AppRuntime;
use polaris_stats_engine::{ConnectionsClosedSnapshot, ConnectionsClosedUpdate};

/// 上游 `STATS_SUBSCRIBE`：订阅某 topic（main 挂订阅 + 即回初始帧）。
///
/// `aggregate` topic 需 app/proxy/config 起后台 relay poller（emit `EVENT_CONNECTIONS_AGGREGATE`）。
#[tauri::command]
pub fn stats_subscribe(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppRuntime>,
    topic: String,
) -> ApiResponse<()> {
    state.stats().subscribe(
        &app,
        state.proxy.clone(),
        state.config.clone(),
        window.label(),
        &topic,
    );
    ok_void()
}

/// 上游 `STATS_UNSUBSCRIBE`：退订某 topic（无订阅者 → worker 逐级停机）。
#[tauri::command]
pub fn stats_unsubscribe(
    window: WebviewWindow,
    state: State<'_, AppRuntime>,
    topic: String,
) -> ApiResponse<()> {
    state.stats().unsubscribe(window.label(), &topic);
    ok_void()
}

/// 清空独立的已结束连接历史。水位由 runtime 记录，后续 gRPC reset 不会把已清的旧历史重新灌回。
#[tauri::command]
pub fn stats_closed_clear(
    app: AppHandle,
    state: State<'_, AppRuntime>,
) -> ApiResponse<ConnectionsClosedSnapshot> {
    let snapshot = state.stats().clear_closed_history();
    broadcast(
        &app,
        EVENT_CONNECTIONS_CLOSED,
        ConnectionsClosedUpdate {
            reset: true,
            connections: Vec::new(),
            removed_ids: Vec::new(),
            at: snapshot.at,
        },
    );
    ApiResponse::ok(snapshot)
}
