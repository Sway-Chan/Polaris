//! 窗口控制类 command（Polaris window:minimize/maximizeToggle/close/isMaximized + app 排序）。
//!
//! 映射 channel：
//! - `window:minimize` → [`window_minimize`]
//! - `window:maximizeToggle` → [`window_maximize_toggle`]
//! - `window:close` → [`window_close`]
//! - `window:isMaximized` → [`window_is_maximized`]
//! - `app:restart` → [`app_restart`]（U-7「需重启 App 才生效」的设置改动后由用户确认触发）
//! - `app:startupConfigFlags` → [`app_startup_config_flags`]（U-7 判据基线：本次进程启动时读到的三键值）
//! - `renderer:ready` → [`renderer_ready`]（renderer mount 健康门信号）
//! - `fatal:retry` → [`fatal_retry`]（终局错误页「重新加载」按钮）
//! - `renderer:log` → [`renderer_log`]（renderer 错误转发到 Rust 日志）
//!
//! Linux 嵌入式标题栏自绘 min/max/close；Mac 原生红绿灯 / Win titleBarOverlay 系统按钮无需。
//! 最大化态变更广播 event:windowMaximizeChanged（标题栏跟随）。

#![allow(clippy::needless_pass_by_value)]

use std::sync::atomic::Ordering;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use crate::events::channel::EVENT_WINDOW_MAXIMIZE_CHANGED;
use crate::response::{ok_void, ApiResponse};
use crate::window_health::MountGateEvent;

/// 上游 `WINDOW_MINIMIZE`：最小化主窗口。
#[tauri::command]
pub fn window_minimize(window: WebviewWindow) -> ApiResponse<()> {
    let _ = window.minimize();
    ok_void()
}

/// 上游 `WINDOW_MAXIMIZE_TOGGLE`：切换最大化/还原 + 广播 event:windowMaximizeChanged。
#[tauri::command]
pub fn window_maximize_toggle(app: AppHandle, window: WebviewWindow) -> ApiResponse<()> {
    let maximized = window.is_maximized().unwrap_or(false);
    if maximized {
        let _ = window.unmaximize();
    } else {
        let _ = window.maximize();
    }
    // 广播新最大化态（标题栏图标跟随）。
    let new_max = !maximized;
    let _ = app.emit(
        EVENT_WINDOW_MAXIMIZE_CHANGED,
        json!({ "maximized": new_max }),
    );
    ok_void()
}

/// 上游 `WINDOW_CLOSE`：关闭主窗口。
#[tauri::command]
pub fn window_close(window: WebviewWindow) -> ApiResponse<()> {
    let _ = window.close();
    ok_void()
}

/// 上游 `WINDOW_IS_MAXIMIZED`：是否最大化。
#[tauri::command]
pub fn window_is_maximized(window: WebviewWindow) -> ApiResponse<bool> {
    ApiResponse::ok(window.is_maximized().unwrap_or(false))
}

/// `app:restart` —— 重启 Polaris 本体（U-7「第三类重启」：改了**进程启动期才读**的设置后，
/// 用户在弹窗里点「立即重启」才走到这里）。
///
/// # 为什么是 `request_restart()` 而不是 `AppHandle::restart()`
///
/// `restart()` 在**主线程**上调用会**跳过** `RunEvent::ExitRequested` / `Exit`
/// （tauri 2.11.5 `app.rs:588-592`：主线程分支直接 `cleanup_before_exit()` + `process::restart()`；
/// 该函数只清 tray icon / resources table，**不碰任何子进程**）。而本仓**唯一的停核腿**挂在
/// `ExitRequested`（`main.rs::run_exit_cleanup` → `proxy.stop()` + 清系统代理 + 收在飞测速临时核）。
/// 跳过它 = sing-box 子进程活着进入新进程的生命周期：**孤儿核占住 mixedPort/TUN**，
/// 且系统代理仍指向一个不再归本应用管的核 —— 新进程起核必撞端口，用户全网走一个没人能停的代理。
/// `request_restart()`（`app.rs:615`）不分线程，恒走 `ExitRequested` → `Exit` 事件腿，故用它。
///
/// # 为什么必须先置 `QuitState`
///
/// `main.rs` 的 `ExitRequested` arm 有 C16 轻量模式守卫：`lightweight && !quitting && 托盘在`
/// → `api.prevent_exit()` + **早退，不跑停核清理**。而 tauri 对 `RESTART_EXIT_CODE` 的
/// `prevent_exit` 是**空操作**（`app.rs:89-93`：`if self.code != Some(RESTART_EXIT_CODE)`）
/// ⇒ 走到那条早退分支时，应用照样重启、核却没停 = 上面那个孤儿态。置 `QuitState` 让 `!quitting`
/// 落空 → 恒落到 `run_exit_cleanup`，把这条缝堵死。（顺带对齐 `tray_quit`：任何经窗口关闭的
/// 退出路径都不被 `prevent_close` 卡住。）
///
/// # 为什么还要置 `RestartState`（Q1-b ④）
///
/// 上面那个 `QuitState` 让本路径与**真退出**在 `ExitRequested` 里完全同形，而退出腿会在那儿落
/// 「用户主动结束了这次使用」的标记、下次启动据此清掉暂存的编辑。重启不是那件事：用户几秒内就
/// 回来、心智完全连续（本命令的主要用途正是 U-7「改了 hardwareAcceleration，重启生效」），
/// 在这条路径上清 staged = App 自己吃掉了用户的工作（NFR-1）。
/// 判据只有发起方知道 —— 从「`QuitState` 是谁置的」反推是把两个语义压进一个布尔。
/// 必须在 `request_restart()` **之前**置：之后置就再也执行不到了。见 [`crate::clean_exit`]。
///
/// **用户可见后果**：重启连同内核一起停 ⇒ **代理会断开**，重启后按「启动时自动连接」恢复。
/// 这一条必须在弹窗文案里如实写明（见 `settings.restartApp.proxyNote`），不许让用户在断网后才发现。
#[tauri::command]
pub fn app_restart(app: AppHandle) -> ApiResponse<()> {
    app.state::<crate::QuitState>()
        .0
        .store(true, Ordering::SeqCst);
    app.state::<crate::RestartState>()
        .0
        .store(true, Ordering::SeqCst);
    log::info!("app:restart —— 用户确认重启应用（经 ExitRequested 停核 + 清系统代理后重启）");
    app.request_restart();
    ok_void()
}

/// `app:startupConfigFlags`：本次进程**启动时**读到的「需重启 App 才生效」三键的生效值（U-7 判据基线）。
///
/// 只读、无副作用。渲染端拿它当基线判「重启到底会不会改变什么」——拿磁盘现值当基线会在
/// 「改走又改回」时误报一次重启（而重启会断代理），详见 `main.rs` 的 [`crate::StartupConfigFlags`]。
///
/// 值在 `setup` 里定格，进程生命周期内不变；webview 自愈重载后重新拉取拿到的仍是同一份，
/// 这正是**不能**在渲染端自行快照的原因（重载会让渲染端的"启动值"漂移到重载那一刻的磁盘值）。
#[tauri::command]
pub fn app_startup_config_flags(app: AppHandle) -> ApiResponse<crate::StartupConfigFlags> {
    ApiResponse::ok(*app.state::<crate::StartupConfigFlags>())
}

/// `app:takeCleanExitFlag`：上次进程是不是**正常退出**的？—— **读即清**（spec §2.5 Q1-b 清除时机 ④）。
///
/// 真 ⇒ 上次走完了退出腿（托盘「退出」/ ⌘Q / 末窗关闭 / `app:restart`），渲染端据此在 hydrate **之前**
/// 清掉持久化的暂存；假 ⇒ 强杀 / 崩溃 / 断电，或者进程压根没退（webview 自愈重载、C16 轻量模式销毁
/// 重建）—— 照常恢复。为什么是「留标记 + 下次读」而不是退出时通知 webview，见 [`crate::clean_exit`]。
///
/// **每个进程只有第一次调用会返回真**：标记在读的同一次系统调用里被消费掉。这是不变式而非实现细节 ——
/// 不清的话，正常退出一次之后每次启动都会清 staged，强杀那条恢复腿永远走不到。
///
/// 运行时未装配（极早期）⇒ 返 `false`（保守：恢复而非清除，方向与 Q1-b「宁可多恢复一次」一致）。
#[tauri::command]
pub fn app_take_clean_exit_flag(app: AppHandle) -> ApiResponse<bool> {
    let clean = app
        .try_state::<crate::runtime::AppRuntime>()
        .is_some_and(|rt| crate::clean_exit::take(rt.config.dir()));
    ApiResponse::ok(clean)
}

/// 上游 `RENDERER_READY`：renderer 成功 mount 信号（主进程 mount 健康门）。
///
/// 经此确认 renderer 进程活着且当前真实页面已越过 Suspense fallback、DOM 真的挂上 —— C 类白屏
/// （进程活着但 DOM 空）不发任何平台事件，「约定回发 ready + 主进程超时」是唯一侦测手段。正常发出点
/// 见 `ui/src/components/screens/ScreenRouter.tsx` 的内容提交边界；根 ErrorBoundary / 同步静态 fallback
/// 各自也会回报可交互兜底已挂上，抑制无谓的 3s 上屏等待与终局升级。
#[tauri::command]
pub fn renderer_ready(app: AppHandle) -> ApiResponse<()> {
    crate::window_health::dispatch(&app, MountGateEvent::RendererReady);
    ok_void()
}

/// 上游 `FATAL_RETRY`：终局错误页「重新加载」——**复位 mount 门 + 导航回真实应用**。
///
/// 不变式 6「真恢复」：原实现只 `window.eval("location.reload()")`，门仍停在 `finalized`
/// → 恢复后的页面若再白屏就彻底无兜底，按钮沦为一次性假承诺。现经
/// [`crate::window_health::retry_from_fatal`] 先 `reset()` 门再导航，重载后的新文档会重新武装。
#[tauri::command]
pub fn fatal_retry(app: AppHandle) -> ApiResponse<()> {
    crate::window_health::retry_from_fatal(&app);
    ok_void()
}

/// 上游 `RENDERER_LOG`：renderer 错误转发到 Rust 日志（限频 + 截断）。
///
/// Tauri 没有 Electron `console-message` 那样的主进程事件，故由 renderer 侧主动上报
/// （`ui/src/main.tsx` 钩 console.error / window.onerror / unhandledrejection）。
/// 这是 C 类白屏「零可观测」的根治 —— 在此之前白屏时日志里一行痕迹都没有。
#[tauri::command]
pub fn renderer_log(app: AppHandle, level: String, message: String) -> ApiResponse<()> {
    crate::window_health::forward_renderer_log(&app, &level, &message);
    ok_void()
}
