//! 主窗口驻留治理器（`autoLightweightMode` 的唯一自动回收腿）。
//!
//! 手动关闭始终由 [`crate::tray::tray_enter_lightweight`] 立即销毁主 WebView；本模块只处理窗口被
//! 隐藏或最小化后长期无人使用的情况。倒计时在 Rust 主进程中运行，不受 WKWebView / WebView2 /
//! WebKitGTK 对后台 renderer 定时器的节流影响。
//!
//! 三个平台使用同一判据：主窗口连续隐藏或最小化 10 分钟后回收。系统全局空闲时间不是合适的
//! 所有权信号——用户在别的应用中持续操作，并不意味着 Polaris 的隐藏 WebView 仍应无限驻留。

use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::runtime::stats::{probe_main_window_visible, MAIN_WINDOW_LABEL};
use crate::runtime::AppRuntime;

/// 主窗口持续隐藏 / 最小化 10 分钟后回收。
const HIDDEN_RECLAIM_SECS: u64 = 10 * 60;

/// 10 分钟量级无需秒级精度；粗粒度巡检也减少主线程可见性回读。
const TICK_SECS: u64 = 30;

/// 启动进程级驻留巡检。唯一调用点在 `main.rs::setup`，因此不另设 started 闸。
pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut hidden_secs = 0_u64;
        loop {
            // `sleep` 不会像 burst interval 一样在机器唤醒后补齐全部欠拍，避免刚开盖就回收窗口。
            tokio::time::sleep(Duration::from_secs(TICK_SECS)).await;

            // 没有主窗就没有可回收对象；下次重建从零开始计时。
            if app.get_webview_window(MAIN_WINDOW_LABEL).is_none() {
                hidden_secs = 0;
                continue;
            }
            // 配置每拍动态读取。开关关闭期间不累计，启用后从当下重新计时。
            if !auto_lightweight_enabled(&app) {
                hidden_secs = 0;
                continue;
            }

            hidden_secs = next_hidden_secs(hidden_secs, TICK_SECS, window_visible(&app));
            if hidden_secs < HIDDEN_RECLAIM_SECS {
                continue;
            }

            enter_lightweight_if_still_hidden(&app);
            // 无论回收成功还是被最终复核否决，都重新计时，避免每拍重复投递。
            hidden_secs = 0;
        }
    });
}

/// 可见立即归零；隐藏 / 最小化按节拍饱和累加。
#[must_use]
fn next_hidden_secs(previous: u64, tick_secs: u64, visible: bool) -> u64 {
    if visible {
        0
    } else {
        previous.saturating_add(tick_secs)
    }
}

/// 运行期动态读 `config.autoLightweightMode`，只投影所需 bool，避免周期性深拷贝整份配置。
/// 读取失败按关闭处理：回收用户界面的失败安全方向是不动作。
fn auto_lightweight_enabled(app: &AppHandle) -> bool {
    let Some(rt) = app.try_state::<AppRuntime>() else {
        return false;
    };
    rt.config()
        .with_current(|c| c.get("autoLightweightMode").and_then(Value::as_bool) == Some(true))
        .unwrap_or(false)
}

/// 复用统计降流门的窗口可见性缓存；运行时尚未装配时按可见处理。
fn window_visible(app: &AppHandle) -> bool {
    let Some(rt) = app.try_state::<AppRuntime>() else {
        return true;
    };
    rt.stats().window_visible(app)
}

/// 在主线程最终复核窗口仍不可见，再走唯一的轻量态销毁实现。
fn enter_lightweight_if_still_hidden(app: &AppHandle) {
    let app_for_main = app.clone();
    let post = app.run_on_main_thread(move || {
        if app_for_main.get_webview_window(MAIN_WINDOW_LABEL).is_none() {
            return;
        }
        match probe_main_window_visible(&app_for_main) {
            Ok(false) => {}
            Ok(true) => return,
            Err(error) => {
                log::warn!("自动轻量模式：可见性复核失败（{error}），本轮不回收");
                return;
            }
        }
        log::info!("自动轻量模式：主窗口已隐藏或最小化 ≥{HIDDEN_RECLAIM_SECS}s，释放主 WebView");
        let _ = crate::tray::tray_enter_lightweight(app_for_main.clone());
    });
    if let Err(error) = post {
        log::warn!("自动轻量模式：投递主线程失败（{error}），本轮不回收");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_window_resets_countdown() {
        assert_eq!(next_hidden_secs(570, TICK_SECS, true), 0);
    }

    #[test]
    fn hidden_window_accumulates_and_saturates() {
        assert_eq!(next_hidden_secs(0, TICK_SECS, false), 30);
        assert_eq!(next_hidden_secs(570, TICK_SECS, false), 600);
        assert_eq!(next_hidden_secs(u64::MAX, TICK_SECS, false), u64::MAX);
    }

    #[test]
    fn reclaim_boundary_is_exactly_twenty_ticks() {
        assert_eq!(HIDDEN_RECLAIM_SECS, 10 * 60);
        let mut elapsed = 0;
        for tick in 1..=20 {
            elapsed = next_hidden_secs(elapsed, TICK_SECS, false);
            assert_eq!(elapsed >= HIDDEN_RECLAIM_SECS, tick == 20);
        }
    }

    #[test]
    fn one_visible_tick_restarts_full_countdown() {
        let elapsed = next_hidden_secs(590, TICK_SECS, true);
        assert!(next_hidden_secs(elapsed, TICK_SECS, false) < HIDDEN_RECLAIM_SECS);
    }
}
