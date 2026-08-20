//! Polaris — Tauri 2 主进程入口。
//!
//! 装配：17 个 domain crate（经 [`runtime::AppRuntime`] 注入真实 I/O 实现）+ Tauri 2 原生插件
//! （single-instance / shell / dialog / notification / autostart / os / process / fs）+ 全部 IPC command 注册。
//!
//! 架构 / 进程模型见 `docs/polaris/design/polaris-system-design.md` §B.1。
//! IPC 命令 / 事件映射见 §B.3（Polaris 136 IPC channel → Tauri command，语义不变）。
//!
//! 命令面：[`commands`] 模块按 上游 `main/ipc/handlers/` 文件划分组织，统一返回
//! [`response::ApiResponse<T>`]（上游 `{ success, data, error, code }` 信封）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_language;
mod clean_exit;
mod commands;
mod events;
mod graphics_compat;
mod i18n;
mod icon_cache;
mod idle_lightweight;
mod logging;
mod response;
mod runtime;
mod tray;
mod window_health;

use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_os = "macos"))]
use std::sync::Arc;

use polaris_helper_proto::Platform;
use tauri::{Manager, WebviewWindowBuilder};
use tauri_plugin_autostart::MacosLauncher;

use crate::commands::*;
use crate::runtime::AppRuntime;
use crate::window_health::{MountGateEvent, WindowHealth};

/// 退出意图标记（app-managed state）：托盘「退出」/ Cmd·Ctrl+Q 置真后，`CloseRequested` 放行窗口真关闭；
/// 未置真时关窗 = 销毁主窗进入轻量驻留（托盘在）或真退出（托盘缺失）。`app.exit` 走 ExitRequested 不经 `CloseRequested`，
/// 该标记保证任何**经窗口关闭**的退出路径都不被 `prevent_close` 卡住（含显式退出收尾与未来路径）。
struct QuitState(AtomicBool);

/// U-7 判据基线：本次进程**启动时**从 config.json 真正读到的「需重启 App 才生效」三键的生效值。
///
/// 存在的理由是「重启到底会不会改变什么」只能相对启动值回答。渲染端能拿到的最新值是磁盘现值，
/// 拿它当基线会在「改走又改回」时误报（详见 `setup` 里 `app.manage` 处的注释）。
///
/// 字段语义一律是 `UserConfig` 口径的**「该功能是否开」**（缺省为开），与渲染端 `effectiveValue`
/// （`v !== false`）同向；不要在此存 `should_disable_*` 这类反相值。
///
/// 只读、进程生命周期内不变：三键的消费点全在 webview / 插件注册之前，启动后再改也不会被本次运行读到。
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupConfigFlags {
    pub hardware_acceleration: bool,
    pub window_effects: bool,
    pub remember_window_size: bool,
}

/// 轻量驻留的末窗销毁标记（C16，app-managed state）：销毁主 WebView，或主窗已销毁后回收最后一个托盘
/// WebView 前置真。若末窗 `destroy()` 触发 `ExitRequested`，`run` 循环据此**阻止退出 + 跳过停核清理**——
/// 轻量模式恒不退出、代理连接不中断（对齐 上游 `markLightweightModeTransition`）。`swap(false)` 在
/// `ExitRequested` 消费；陈旧置位不阻断真实退出（真退出置 `QuitState` → 守卫 `!QuitState` 落空）。
struct LightweightState(AtomicBool);

/// 「本次退出是 `app:restart` 发起的」（app-managed state）：[`commands::app_restart`] 在
/// `request_restart()` **之前**置真，退出腿 [`mark_clean_exit`] 读到即**跳过落正常退出标记**。
///
/// # 为什么需要它，而不是从 `QuitState` 反推
///
/// `app_restart` 与真退出在进程层面**完全一样**（都置 `QuitState`、都走 `ExitRequested`），
/// 标记点上不可区分。而 Q1-b ④ 要的判据不是「进程有没有真的退」，是「用户还记不记得那批编辑」——
/// `app:restart` 是用户几秒内就回来的一次重启（主要用途正是 U-7「改了 hardwareAcceleration，重启生效」），
/// 在那儿清 staged 就是 App 自己吃掉用户的工作（NFR-1）。这只有发起方知道，故由发起方显式置位。
/// 语义见 [`clean_exit`] 模块注释「标记的语义是『用户主动结束了这次使用』」那节。
///
/// **纯进程内存态、不落盘**：它只需活到本进程的退出腿读它那一刻。`swap(false)` 在
/// `mark_clean_exit` 消费（与 `LightweightState` 同款），陈旧置位不会跨进程存活。
struct RestartState(AtomicBool);

/// C15 启动模式判定（纯函数，可单测）：据进程 argv + 是否有图形显示决定 CLI 早退 / 隐藏启动。
/// 迁移自 上游 `resolveCliEarlyExit`（`cli-early-exit.ts:26-38`）+ `index.ts:895/1494` 的 `--hidden` 处理。
#[derive(Debug, PartialEq, Eq)]
enum StartupAction {
    /// `-V/--version`：打印版本 + exit(0)（CLI 查询，根本不起 GUI）。
    Version,
    /// `-h/--help`：打印用法 + exit(0)。
    Help,
    /// 无图形环境（Linux 无 DISPLAY/WAYLAND_DISPLAY）：GUI app 必崩 → 提示 + exit(1)，规避 segfault。
    HeadlessExit,
    /// 正常起 GUI。`hidden`=argv 含 `--hidden`（启动不显主窗、只驻托盘；再与 config.silentStart 在 setup 合并）。
    Run { hidden: bool },
}

/// 纯判定：`-V/--version` > `-h/--help` > Linux headless > 正常（hidden 由 `--hidden` 决定）。
/// `has_display` 由调用方按平台注入（Linux 查 DISPLAY/WAYLAND_DISPLAY；mac/win 恒 true——无等价简单信号，
/// 与 上游 一致：headless 早退仅 Linux，CLI flag 早退三平台通用）。
fn resolve_startup(args: &[String], has_display: bool) -> StartupAction {
    let has = |flags: &[&str]| args.iter().any(|a| flags.contains(&a.as_str()));
    if has(&["-V", "--version"]) {
        return StartupAction::Version;
    }
    if has(&["-h", "--help"]) {
        return StartupAction::Help;
    }
    if !has_display {
        return StartupAction::HeadlessExit;
    }
    StartupAction::Run {
        hidden: has(&["--hidden"]),
    }
}

/// `-h/--help` 用法文本（CLI 惯例英文，与 i18n 无关——CLI 帮助按惯例英文）。迁自 上游 `cliHelpText`。
fn cli_help_text() -> String {
    format!(
        "Polaris {}\n\nUsage: polaris [options]\n  -V, --version   Show version and exit\n  -h, --help      Show this help and exit\n  --hidden        Start hidden to the system tray\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// 从 config.json 原文本判 `silentStart`（静默启动 = 启动即隐藏；与 `--hidden` 合并成 start_hidden）。
/// 读原文本而非 store：与 `graphics_compat` 判定同源、不依赖 store 具体 API；任何异常/缺失 → false（默认显示）。
fn config_silent_start(raw: Option<&str>) -> bool {
    raw.and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
        .and_then(|v| v.get("silentStart").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

/// 从 config.json 原文本判 `rememberWindowSize`（记忆窗口大小 → 是否注册 window-state 插件）。
///
/// **正向语义 + 缺省为 true**：对齐前端 `config.rememberWindowSize !== false`。与
/// [`config_silent_start`]（缺省 false）方向相反是**刻意**的——静默启动缺省关是「不改变可见行为」，
/// 而记忆窗口大小缺省开才与 UI 上那个默认打开的开关一致。
///
/// 读原文本而非 store：与 `graphics_compat` / `config_silent_start` 同源，且判定发生在 store 装配之后
/// 但必须早于建窗，用同一份 `raw_config` 最省。
fn config_remember_window_size(raw: Option<&str>) -> bool {
    raw.and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
        .and_then(|v| {
            v.get("rememberWindowSize")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(true)
}

/// 提交一次原生窗口最大化观测；仅状态真变化时返回 `true`，普通 resize 不产生事件风暴。
#[cfg(not(target_os = "macos"))]
fn commit_maximized_observation(state: &AtomicBool, current: bool) -> bool {
    state.swap(current, Ordering::SeqCst) != current
}

/// 关主窗时该做什么（#10）。
#[derive(Debug, PartialEq, Eq)]
enum CloseAction {
    /// 放行真关闭（显式退出进行中）。
    AllowClose,
    /// prevent_close + 销毁主窗 WebView，保留托盘与内核。
    EnterLightweight,
    /// 真退出进程。
    QuitApp,
}

/// 纯判定关窗语义（#10）。此前 `CloseRequested` 只看 `QuitState` + 托盘在否，**完全没读**
/// `config.minimizeToTray` → UI 那个「关闭主窗口时：收进托盘 / 退出应用」分段控件是死装饰。
///
/// 语义：
/// - `quitting` → `AllowClose`：显式退出（托盘「退出」/ ⌘Q）进行中，绝不 hide，否则退不掉。
/// - `minimize_to_tray && tray_present` → `EnterLightweight`：用户明确关闭主窗后销毁 renderer，
///   保托盘与内核；与“最小化”只缩起窗口的语义分开，也不依赖自动轻量开关。
/// - `minimize_to_tray && !tray_present` → `QuitApp`：用户虽要收纳，但托盘整体缺失（Linux 无
///   StatusNotifier）→ 销毁后无处唤出 = 僵尸进程，只能真退出。
/// - `!minimize_to_tray` → `QuitApp`：用户明确选了「退出应用」，托盘在不在都退。
fn resolve_close_action(quitting: bool, tray_present: bool, minimize_to_tray: bool) -> CloseAction {
    if quitting {
        return CloseAction::AllowClose;
    }
    if minimize_to_tray && tray_present {
        return CloseAction::EnterLightweight;
    }
    CloseAction::QuitApp
}

/// 事件发生时**动态**读 `config.minimizeToTray`（不在建窗时快照）——用户改完设置立刻生效、无需重启。
/// 缺省 / 运行时未装配 / 读失败一律 **true**：保持现行「关窗收进托盘」的默认，存量行为不变。
fn config_minimize_to_tray(app: &tauri::AppHandle) -> bool {
    app.try_state::<AppRuntime>()
        .and_then(|rt| rt.config().load_full().ok())
        .and_then(|v| v.get("minimizeToTray").and_then(serde_json::Value::as_bool))
        .unwrap_or(true)
}

/// 主窗显隐变化 → 刷新 stats 降流门的可见性缓存。
///
/// 真值一律由 stats 侧回读窗口实况（`is_visible() && !is_minimized()`）派生，本函数只是「显隐可能
/// 刚变」的**即时触发器**：变了即唤醒 park 中的三条 poller，恢复不等整拍。
/// 两个主动写入点：`WindowEvent::Focused` 与 [`show_main_window`] 的 `show()` 之后；显式关窗现在直接
/// 销毁主 WebView并由 `tray_enter_lightweight` 清订阅，不再需要 `hide()` 后单独刷新。
/// 运行时未装配（启动早期）→ no-op。
fn refresh_stats_visibility(app: &tauri::AppHandle) {
    if let Some(rt) = app.try_state::<AppRuntime>() {
        rt.stats().refresh_window_visible(app);
    }
}

/// macOS 只驻托盘时隐藏 Dock 图标；主窗重新呈现前恢复。其它平台保持 no-op，调用点无需散落 cfg。
#[cfg(target_os = "macos")]
pub(crate) fn set_macos_dock_visible(app: &tauri::AppHandle, visible: bool) {
    if let Err(e) = app.set_dock_visibility(visible) {
        log::warn!("macOS Dock 图标显隐切换失败（visible={visible}，非致命）：{e}");
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_macos_dock_visible(_app: &tauri::AppHandle, _visible: bool) {}

/// 把**已存在**的主窗真正推上屏：unminimize + show + focus。
///
/// 失败静默——窗可能已析构，非致命。unminimize 先行：窗若只被最小化而仍存在，只 show 不够，
/// 得先出最小化态才会真正可见（dock/任务栏重开路径尤其需要）；未最小化时 unminimize 是 no-op。
///
/// **只负责「呈现」，绝不建窗**（建窗归 [`show_main_window`]）：本函数还被 `window_health` 的兑现腿
/// （ready / 兜底期限）异步调用，而那时窗口可能已被轻量模式销毁 —— 那种情况下必须安静地什么都不做，
/// 绝不能凭一个几秒前的上屏意图凭空重建一个用户没要的窗。
fn present_main_window(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    // macOS 收托盘时会隐藏 Dock 图标；先恢复再上屏，避免窗口已出现而 Dock 仍缺席的一帧错位。
    set_macos_dock_visible(app, true);
    let _ = w.unminimize();
    let show_result = w.show();
    let _ = w.set_focus();
    // 显隐写入点：主窗刚变可见 → 立刻刷 stats 降流门（park 中的三条 poller 由此即刻恢复，
    // 不必等它们各自的 1s 兜底拍）。`Focused` 事件在部分平台/路径上不保证跟着 show 发。
    refresh_stats_visibility(app);
    window_health::log_show_probe(
        app,
        if show_result.is_ok() {
            "shown"
        } else {
            "show-failed"
        },
        true,
    );
}

/// 唤出主窗（托盘浮层的明确入口 / Linux 托盘「显示」菜单 / macOS dock 重开 共用）。
///
/// 上屏时机交 [`window_health::show_timing`] 判：窗在且内容已就绪 → 立刻呈现（常态，零延迟）；
/// 窗在但当前文档还没 mount 成功（启动期 / webview 崩溃后 Tauri 内置 reload 在途）→ **不把空窗推给
/// 用户**，扣在隐藏态等 `renderer:ready`（超期有兜底，见 `window_health::defer_show`）。
///
/// 所有调用先统一投到主线程，再由 [`show_main_window_on_main_thread`] 执行。这个边界不能只包
/// `apply_vibrancy`：重建窗的 builder、原生材质和窗口事件装配是一项不可拆的主线程事务。托盘 WebView
/// command 会从异步 IPC 线程进入；若直接在那条线程重建，macOS 会拒绝 vibrancy，而前端仍按“材质已开”
/// 让侧栏透明，最终露出桌面。首建在 setup 主线程、重建在 IPC 线程的分叉必须在入口处消掉。
fn show_main_window(app: &tauri::AppHandle) {
    // W18 第二层（2026-08-20 真机翻案）：本函数的调用方会从**主线程的消息分发帧**进来——
    // single-instance 插件的 WM_COPYDATA WndProc（跨进程 `SendMessageW` 同步栈内）、托盘
    // 浮层 command 的 IPC 分发栈。`run_on_main_thread` 从主线程调用是**内联直执**——帧内
    // 直接重建主窗（WebView2 创建）会把同步对端卡死（真机实证：关窗后双击 = 第二实例
    // 永不退出 + 首实例重建卡在 WndProc 里）。故先跳 async 线程（脱离分发帧）再排回主
    // 线程执行；从非主线程进来的调用方只多一跳（µs 级），行为不变。
    window_health::begin_show_probe(app, app.get_webview_window("main").is_none());
    let app_for_main = app.clone();
    tauri::async_runtime::spawn(async move {
        let h = app_for_main.clone();
        if let Err(error) = app_for_main.run_on_main_thread(move || {
            window_health::log_show_probe(&h, "main-thread", false);
            show_main_window_on_main_thread(&h);
        }) {
            log::error!("主窗唤出投递主线程失败：{error}");
            window_health::log_show_probe(&app_for_main, "dispatch-failed", true);
        }
    });
}

/// 主线程内完成“复用现有主窗或完整重建”的唯一实现。
fn show_main_window_on_main_thread(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        match window_health::show_timing(app, Some(&w)) {
            window_health::ShowTiming::Now => present_main_window(app),
            window_health::ShowTiming::WhenReady => window_health::defer_show(app),
        }
    } else {
        // C16 轻量模式已**销毁**主窗 webview（`get_webview_window` 返 None）→ 重建（可见）。所有 per-window
        // 装配（特效 / 白屏自愈门 / 关闭进轻量事件）都在 `create_main_window` 一处，故重建与首建等价。
        // 失败仅记日志（托盘 / 核仍在，用户可重试唤出）；`start_hidden=false`——用户显式唤出即要可见。
        window_health::log_show_probe(app, "build-start", false);
        if let Err(e) = create_main_window(app, false) {
            log::error!("主窗重建失败（轻量模式返回）：{e}");
            window_health::log_show_probe(app, "build-failed", true);
        }
    }
}

/// C1 退出清理：任何 `ExitRequested`（托盘/菜单「退出」、末窗关闭、`app.exit`）都**阻塞**跑
/// [`ProxyRuntime::stop`](crate::runtime::proxy::ProxyRuntime::stop)（停核 + 清系统代理，marker 门控幂等）。
///
/// **为什么安全关键**：`systemProxy` 模式 start 成功后会把 OS 系统代理指向本地 mixedPort（A1）。若退出不清，
/// 系统代理仍指向刚被杀的死端口 → 用户全网断连、需手动改回。这与 start 失败腿 / 主动 stop 的清理同一
/// marker 门控收口点（`ProxyRuntime::clear_system_proxy`），不误清用户自配的第三方代理。
///
/// **覆盖面与兜底**：正常退出（含 OS 关机/logout 若经窗口关闭）走这里。**崩溃 / 强杀 / panic 不经此路径**
/// → 靠启动期 [`recover_system_proxy_on_startup`](crate::runtime::proxy::ProxyRuntime::recover_system_proxy_on_startup)
/// 在下次启动清残留 marker。**刻意不加清系统代理的 panic hook**：本仓 `panic=unwind`，任一后台 tokio task
/// 的 panic 都会触发进程级 hook，会误清一个仍在服务的活代理 → 见 review-queue `DESIGN-REVIEW(c1-panic-hook)`。
///
/// `block_on` 在 RunEvent 回调（主线程、非 tokio worker）内安全；退出路径慢一点可接受，但绝不能带着
/// 死端口系统代理离开。
fn run_exit_cleanup(app: &tauri::AppHandle) {
    // 在飞的**测速临时核**先收：它不在 `ProxyRuntime` 的任何生命周期槽里（刻意隔离），`proxy.stop()`
    // 碰不到它；而测速在飞时退出，那条 tokio task 不会被 drop ⇒ child 的 Drop 守卫也够不着 ⇒ 留下
    // 持有 N 个回环端口 + WG peer 会话的孤儿 sing-box（Windows 无 stale sweep，永不被清）。
    // 不依赖 `AppRuntime`（进程级 pid 表），故放在 try_state 早退之前。
    let killed = crate::runtime::speedtest::kill_inflight_temp_cores();
    if killed > 0 {
        log::warn!("退出清理：强杀了 {killed} 个在飞测速临时核");
    }
    let Some(rt) = app.try_state::<AppRuntime>() else {
        return; // 运行时未装配（极早期退出）→ 无可清。
    };
    let proxy = rt.proxy.clone();
    tauri::async_runtime::block_on(async move {
        if let Err(e) = proxy.stop().await {
            log::error!("退出清理：停核失败（不阻断退出）: {e}");
        }
    });
}

/// 按代理连接态切托盘图标；连接/断开靠**形态**区分（实心 vs 空心），三平台**全单色自适应**（不再彩色）。
/// 图标在编译期内嵌（`include_image!`，走 `image-png` feature），运行期零文件 IO。托盘可能整体缺失
/// （Linux 无 StatusNotifier）→ `tray_by_id` 返 None 时静默跳过。
///
/// # 图标策略（R15：用户已裁决「全自适应、丢彩色」）
///
/// 「彩色品牌」与「系统自适应」在 macOS template 机制下不可兼得（template 只吃 alpha、由系统自动反色，
/// 保不住彩色），用户拍板**全自适应**。连接/断开不靠颜色、靠**形态**区分：连接=**实心星**、断开=**空心
/// 描边星**（都单色、随明暗反色），实心/空心一眼区分——macOS 惯例（VPN app 连=实心盾 / 断=空心盾）。
/// 素材从 `icons/polaris-logo.svg` 星形派生（`tray-star-{filled,outline}.svg` → 四张
/// `tray-{on,off}-{black,white}.png`，alpha 即形状，无外部素材）。
///   · **macOS**：`template=true`（conf `iconAsTemplate:true` + 此处），系统按菜单栏明暗**自动反色**
///     （深=白、浅=黑）。template 只取 alpha 忽略 RGB → 用黑色变体即可（连=on-black / 断=off-black）。
///   · **Win/Linux**：**无** template 自动反色机制 → 靠 [`tauri::Window::theme`] 检测系统明暗，深色任务栏
///     用白变体、浅色用黑变体；并监听 `WindowEvent::ThemeChanged` 实时换（见主窗 `on_window_event`）。
///     检测取不到 → 默认深色任务栏用白，避免深底黑星融入。
///
/// # 四态（A2：此前只有 connected 二态）
///
/// 形态轴从「实心 / 空心」扩到四种**轮廓可辨**的图形（16px 下二值可分，不靠粗细微差）：
/// 连接=实心星+厚环 / 起核中=**实心星无环** / 未连接=空心星+细环+**单斜杠** / 异常=空心星+细环+**双斜杠**。
/// 未连接那道斜杠是 2026-07-29 真机加的：「实心 vs 空心」在 22pt 菜单栏几乎不可分，斜杠才是二值特征；
/// 异常态随之从单斜杠升双斜杠，否则两态撞形（22px 实测比选，见 `icons/tray-star-error.svg` 头注）。
/// 素材同源派生（`tray-star-{connecting,error}.svg` → `tray-{connecting,error}-{black,white}.png`）。
/// 对齐 上游 `TrayManager.ts:54` 的三态 + `:265` 的 `hasError` 分支（Polaris 侧此前二者都缺）。
///
/// ⚠️ macOS 反色、Windows 任务栏主题、Linux portal 主题检测本机（Linux）均验不全 → 待真机（R15）。
/// Win/Linux 托盘图标黑/白变体的**系统真值**读取（W13 正解，复审修法②）。
///
/// - **Windows**：直读注册表 `Personalize`（任务栏跟随系统主题，故取 `SystemUsesLightTheme`，
///   缺失时退应用档 `AppsUseLightTheme`）。零窗口依赖——主窗/浮层窗全销毁的轻量态恒可用，
///   且不受显式 uiTheme 的 `set_theme` 钉窗失真影响（复审 Med-1：窗口 `theme()` 读的是应用外观）。
///   实时性：无窗时收不到 `WM_SETTINGCHANGE`，由既有 30s 自愈轮询（[`TRAY_ICON_POLL`]）兜住。
/// - **Linux**：portal 读法需要窗口在场（tao 实现），无窗口时返回 `None` 落回窗口探测链——
///   已知缺口如实记录（Linux 侧本就标 R15 待真机）。
#[cfg(target_os = "windows")]
fn system_dark_bg() -> Option<bool> {
    use std::os::windows::ffi::OsStrExt;
    const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    fn read_dword(value: &str) -> Option<u32> {
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
        };
        let subkey = wide(PERSONALIZE);
        let val = wide(value);
        let mut data: u32 = 0;
        let mut size: u32 = 4;
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                val.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                &mut data as *mut u32 as *mut std::ffi::c_void,
                &mut size,
            )
        };
        (rc == ERROR_SUCCESS).then_some(data)
    }
    let light = read_dword("SystemUsesLightTheme").or_else(|| read_dword("AppsUseLightTheme"))?;
    Some(light != 1) // 1 = 浅色；0（或它值）= 深色
}

/// 非 Windows 侧（仅 Linux；mac 走 template 反色不进本链）：未引入零窗口真值源（portal/gsettings
/// 直读需新依赖），返回 `None` 落回窗口探测链。门控与调用点 `not(macos)` 同构——若写成
/// `not(windows)`，mac 展开下本 fn 零引用，clippy -D warnings 的 macos CI 腿必红（二审 High-1）。
#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn system_dark_bg() -> Option<bool> {
    None
}

/// Win/Linux 托盘图标黑/白变体的明暗探测（W13 抽出的纯函数）。
///
/// `primary` 依序取第一个 `Some`：注册表真值（Win，未引入时为 None）→ 主窗（显式 uiTheme 下被
/// `set_theme` 钉住、读到的是应用外观而非任务栏明暗——已知失真，Linux 侧至今靠本句记录）。
/// `fallback` = 托盘浮层窗（限时存活：轻量转场与 120s 空闲回收都会销毁它，聊胜于无的末位兜底）。
/// 全部取不到 → 默认深色任务栏用白（沿用原取向：深底黑星融入不可辨）。
#[cfg(not(target_os = "macos"))]
fn dark_bg_from_probe(primary: Option<bool>, fallback: Option<bool>) -> bool {
    primary.or(fallback).unwrap_or(true)
}

fn set_tray_state(app: &tauri::AppHandle, state: crate::tray::TrayState) {
    let Some(tray) = app.tray_by_id("main") else {
        return; // 托盘整体缺失（Linux 无 StatusNotifier / appindicator 不可用）→ 静默跳过
    };
    // macOS：template 由系统按菜单栏明暗**自动反色** ⇒ 明暗根本不是输入，恒 false 占位（不进视觉态）。
    #[cfg(target_os = "macos")]
    let dark_bg = false;
    // Win/Linux：无 template 自动反色 → 探测链（W13）：注册表真值（Win）→ 主窗（显式 uiTheme
    // 下被钉、读到应用外观）→ 浮层窗（限时存活兜底）。
    // 旧实现只探主窗，主窗一关取不到就回落白变体，浅色任务栏上图标直接隐身。
    #[cfg(not(target_os = "macos"))]
    let dark_bg = dark_bg_from_probe(
        system_dark_bg().or_else(|| {
            app.get_webview_window("main")
                .and_then(|w| w.theme().ok())
                .map(|t| t == tauri::Theme::Dark)
        }),
        app.get_webview_window(crate::tray::TRAY_LABEL)
            .and_then(|w| w.theme().ok())
            .map(|t| t == tauri::Theme::Dark),
    );

    // tooltip 语言：config.language（`ConfigManager` 缓存读），auto 回落系统 locale。
    let next = TrayVisual {
        state,
        dark_bg,
        lang: crate::i18n::app_lang(app),
    };

    // 幂等闸门：托盘上真正要落的字节完全由 `next` 决定 ⇒ 未变即不碰托盘（见 `reconcile_tray_visual`）。
    // 锁跨越 apply 是刻意的：多驱动源（事件监听 / 自愈轮询 / 主题变化）并发调本函数时，串行化避免两次
    // set_icon 交错落成陈旧终态。中毒锁不致命（托盘不是安全边界）→ `into_inner` 继续用，不 panic。
    let mut cache = TRAY_VISUAL.lock().unwrap_or_else(|e| e.into_inner());
    reconcile_tray_visual(&mut cache, next, |v| {
        // macOS：原子设「图标+template」免闪烁（先 set_icon 再 set_icon_as_template 会二次渲染，
        // 见 tauri `tray/mod.rs` set_icon_with_as_template）；template 只取 alpha → 黑变体即可。
        #[cfg(target_os = "macos")]
        let icon_res = {
            let icon = match v.state {
                crate::tray::TrayState::Connected => {
                    tauri::include_image!("icons/tray-on-black.png")
                }
                crate::tray::TrayState::Connecting => {
                    tauri::include_image!("icons/tray-connecting-black.png")
                }
                crate::tray::TrayState::Error => {
                    tauri::include_image!("icons/tray-error-black.png")
                }
                crate::tray::TrayState::Idle => tauri::include_image!("icons/tray-off-black.png"),
            };
            tray.set_icon_with_as_template(Some(icon), true)
        };
        #[cfg(not(target_os = "macos"))]
        let icon_res = {
            use crate::tray::TrayState;
            let icon = match (v.state, v.dark_bg) {
                (TrayState::Connected, true) => tauri::include_image!("icons/tray-on-white.png"),
                (TrayState::Connected, false) => tauri::include_image!("icons/tray-on-black.png"),
                (TrayState::Connecting, true) => {
                    tauri::include_image!("icons/tray-connecting-white.png")
                }
                (TrayState::Connecting, false) => {
                    tauri::include_image!("icons/tray-connecting-black.png")
                }
                (TrayState::Error, true) => tauri::include_image!("icons/tray-error-white.png"),
                (TrayState::Error, false) => tauri::include_image!("icons/tray-error-black.png"),
                (TrayState::Idle, true) => tauri::include_image!("icons/tray-off-white.png"),
                (TrayState::Idle, false) => tauri::include_image!("icons/tray-off-black.png"),
            };
            tray.set_icon(Some(icon))
        };
        if let Err(e) = &icon_res {
            log::warn!("托盘图标切换失败（{v:?}）：{e}");
        }

        // 托盘 tooltip 随连接态动态刷新（审查 MED）：tauri.conf 静态 "Polaris" → hover 恒固定文案；此处按
        // 连接态 + 语言设。与图标切换同源（本函数在 init/start/stop/主题变化都调），tooltip 与图标态天然
        // 同步。mac/win 显示；Linux appindicator 无 tooltip = 静默 Ok(()) no-op（`tray-icon` gtk 后端
        // `set_tooltip` 直接返 Ok）→ 不会把 Linux 的缓存永久打成 None（真机门：呈现只在 mac/win 验得到）。
        let tip_res = tray.set_tooltip(Some(crate::tray::tooltip_text(v.lang, v.state)));
        icon_res.is_ok() && tip_res.is_ok()
    });
}

/// 落到托盘上的**全部**视觉输入 —— 汇流点幂等短路的比较键。
///
/// - `state`：图标形态（四态，见 [`crate::tray::TrayState`]）+ tooltip 文案分支
/// - `dark_bg`：Win/Linux 的黑 / 白变体选择（macOS 走 template 由系统反色 ⇒ 恒 `false`，不参与）
/// - `lang`：tooltip 文案语言
///
/// 这三者之外没有任何输入能改变托盘上的字节 ⇒ 键相等 ⇒ 重设是纯浪费（见 [`reconcile_tray_visual`]）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TrayVisual {
    state: crate::tray::TrayState,
    dark_bg: bool,
    lang: crate::i18n::Lang,
}

/// 上次**成功落到托盘上**的视觉态。`None` = 未知（进程刚起 / 上次落盘失败）→ 下次无条件重设。
static TRAY_VISUAL: std::sync::Mutex<Option<TrayVisual>> = std::sync::Mutex::new(None);

/// 托盘视觉态的**幂等闸门**（纯函数，副作用经 `apply` 注入 → 可单测）：与上次成功落盘的态相同则
/// **不碰托盘**并返 `false`；否则 apply 并按结果更新缓存，返 `true`。
///
/// # 为什么必须短路
///
/// 汇流点被 30s 自愈轮询（[`TRAY_ICON_POLL`]）无条件叫醒，而绝大多数轮次状态根本没变（代理长期未
/// 运行 = 每一轮都没变）。Linux 侧代价最实：`tray-icon` 的 gtk 后端 `set_icon`
/// （`platform_impl/gtk/mod.rs:52-71`）每次都**删旧临时 PNG → counter+1 → 往
/// `$XDG_RUNTIME_DIR/tray-icon/` 写一张新 PNG → `set_icon_theme_path` + `set_icon_full`** ——
/// 磁盘写 + indicator 重载每 30s 一次、伴随整个进程生命周期，多数 StatusNotifier host 上表现为
/// 图标周期性闪一下。轮询本身不动（自愈网要留着），把「叫醒」与「重画」解耦即可。
///
/// # 为什么落盘失败要作废缓存，而不是照存
///
/// 存进去就等于宣称「托盘上现在长这样」。`set_icon` 失败时托盘上其实是**旧图**，若照存，之后每一
/// 轮自愈轮询都会短路、再也不重试 —— 自愈网被自己的缓存关掉，恰好在最需要它的时候。故失败置
/// `None`，下一轮无条件重设。
///
/// # 短路不会被绕过
///
/// 落盘动作（`set_icon` / `set_tooltip`）只存在于传进来的 `apply` 闭包里，[`set_tray_connected`] 自身
/// 没有第二条通往托盘的路径 ⇒ 想跳过闸门必须重写该函数体，而不是漏调一行。
fn reconcile_tray_visual(
    cache: &mut Option<TrayVisual>,
    next: TrayVisual,
    apply: impl FnOnce(TrayVisual) -> bool,
) -> bool {
    if *cache == Some(next) {
        return false;
    }
    *cache = apply(next).then_some(next);
    true
}

/// 托盘图标 / tooltip 的**唯一汇流点**：回读 proxy 真值 → 刷新。所有驱动源（setup 初始化、代理生命
/// 周期事件、自愈轮询、系统明暗切换）一律经此，不再有第二处决定「图标该显示什么」。
///
/// # 为什么必须回读真值，而不是由事件携带布尔
///
/// 图标此前只订阅 `EVENT_PROXY_STARTED` / `EVENT_PROXY_STOPPED` 并各自传 `true`/`false` 字面量，于是
/// 「哪些腿会改变连接态」与「哪些腿会发这两个事件」被绑成了同一个问题 —— 而它们并不相等：
///
/// | 终态腿 | 发的事件 | 旧图标 |
/// |---|---|---|
/// | 用户主动断开 | `STOPPED` | ✅ |
/// | 核异常退出 / 自动重启失败 | 仅 `ERROR` | ❌ 停在实心 |
/// | `proxy_restart` 失败（核已停） | **零 emit** | ❌ 停在实心 |
/// | updater 换核前停核 | **零 emit** | ❌ 停在实心 |
/// | 休眠唤醒后失效 | **零 emit** | ❌ |
///
/// 回读真值把问题收敛回「当下核在不在跑」这一个可直接观测的事实（`ProxyRuntime::status().running`，
/// 与主窗 `refreshProxyStatus()` / 托盘浮层 `hydrate()` 同一真值源），于是**零 emit 的腿也能被兜住**
/// —— 只要有任何一个触发点把汇流点叫醒。触发点清单见 [`wire_tray_icon_sync`]。
///
/// # 四态从哪读（A2）
///
/// 三个位全部出自**同一个** `ProxyStatus` 快照，不新造任何 latch：
/// - `running` / `starting` —— 快照现成字段（`starting` 是读时投影，正是浮层用来判「起核中」的那个）。
/// - `errored` = `error_code.is_some()`（`set_error` 落值时与 `EVENT_PROXY_ERROR` **同点**写，见
///   `ProxyStatus::error_code` 文档「快照与事件同源，错过事件的 UI 仍能从状态读到码」）。
///
/// **刻意不用「收到 ERROR 事件就置个 flag」**：那等于给同一事实造第二个真值源，且必须自己想清楚何时清
/// 标记（start 成功？stop？超时？）——每一个都是新的漏清风险。读快照则天然自洽：`start()` 成功会整体
/// 覆写 status（error 归 None）、`stop()` 写 `ProxyStatus::default()`（同样归 None），清除路径**已经**
/// 由 runtime 层保证，托盘不必也不该复述一遍。这与本函数「回读真值而非信事件」的整体取向是同一条理由。
///
/// 便宜（一次 `RwLock` 读快照，无 IO / 无 syscall），故可放心让轮询按秒级频率调。
fn reconcile_tray_icon(app: &tauri::AppHandle) {
    let state = app
        .try_state::<AppRuntime>()
        .map(|rt| {
            let s = rt.proxy().status();
            crate::tray::resolve_tray_state(s.running, s.starting, s.error_code.is_some())
        })
        .unwrap_or(crate::tray::TrayState::Idle);
    set_tray_state(app, state);
}

/// 触发托盘汇流点（[`reconcile_tray_icon`] + [`reconcile_tray_menu`]）的事件全集。
///
/// `ERROR` 是补上的那条边：`runtime/proxy.rs` 的 `set_error()` 会把 `running=false` 落盘、却只发
/// `EVENT_PROXY_ERROR`（`ProxyErrorEmitter` trait 结构上就没有 `emit_proxy_stopped`）→ 崩溃腿此前对
/// 托盘完全不可见。对齐 上游 `index.ts:1895-1902`（`ProxyManager` `emit('error')` → 汇流点）。
///
/// `CONFIG_CHANGED` 是随 A7 原生菜单补上的：菜单要显示当前的**接管方式 / 分流策略勾选**与**语言**，
/// 而这三样只随配置变，不随代理生命周期变。少了它，用户在主窗切完分流策略，右键托盘看到的还是旧勾选
/// —— 且最长要等 30s 轮询才回正（Linux 上原生菜单是主交互面，那 30s 是实打实的错误信息）。
const TRAY_SYNC_EVENTS: [&str; 4] = [
    crate::events::channel::EVENT_PROXY_STARTED,
    crate::events::channel::EVENT_PROXY_STOPPED,
    crate::events::channel::EVENT_PROXY_ERROR,
    crate::events::channel::EVENT_CONFIG_CHANGED,
];

/// 托盘图标自愈轮询周期。
///
/// 存在的理由是**已知有腿零 emit**（restart 失败 / updater 停核 / 休眠唤醒），事件订阅无论补多全都
/// 只覆盖「已知会发事件的腿」；轮询覆盖的是**未知缺口**。主窗 `App.tsx:210-213` 正是靠同款 30s 轮询
/// 兜住这些腿才没出现同类 bug，托盘图标此前一道网都没有 → 对齐取 30s。
///
/// 30s ≠ 用户可感延迟的上限：有事件的腿仍是即时的（事件腿先到），轮询只负责封顶「最坏多久回正」。
const TRAY_ICON_POLL: std::time::Duration = std::time::Duration::from_secs(30);

/// 装配托盘图标汇流点的**全部驱动源**——两道网，缺一道就会退回「图标卡在实心」。
///
/// 副作用经 `subscribe` / `spawn_poll` 两个闭包注入，装配逻辑本身成纯函数 → 可在无 `AppHandle` 的
/// 单测里断言「三条终态事件全订 + 轮询网确实挂上」（见本模块 tests）。这是本修复唯一可自动验的部分：
/// 图标像素只能真机看，但「哪些源被接上」是纯装配决策，必须自动断言，否则又是一条无测试的腿。
///
/// # 为什么选「回读真值 + 自愈网」，而不是在 runtime 层补 emit
///
/// 另一条路是照 上游 在核状态每次变化处补 `emit_proxy_stopped`（`runtime/proxy.rs` 的 `set_error`、
/// `commands/proxy.rs` 的 restart 失败腿、`commands/updater.rs` 的停核腿各补一处）。不选它，因为那是
/// **逐腿补 emit**：正确性取决于「有没有漏掉某条腿」，而本 bug 的成因恰恰就是漏了一条 —— 同一类错误
/// 会随新增终态腿反复发生（updater 那两处就是 started/stopped 搬全之后新长出来的）。回读真值把
/// 正确性条件从「所有腿都记得发事件」降级为「任一触发点叫醒汇流点」，是**结构上**更难写错的形态。
fn wire_tray_icon_sync(
    mut subscribe: impl FnMut(&'static str),
    mut spawn_poll: impl FnMut(std::time::Duration),
) {
    for ev in TRAY_SYNC_EVENTS {
        subscribe(ev);
    }
    spawn_poll(TRAY_ICON_POLL);
}

// ── 托盘交互策略：macOS/Windows 直派点击，Linux 由原生菜单承接 ─────────────────────
//
// 这不是视觉偏好分支，而是平台能力边界：Tauri 的 tray-icon 在 macOS/Windows 会派发左右键事件；
// Linux AppIndicator 明确不派发 `TrayIconEvent`，且菜单一旦挂上也不能移除。故把平台差异收敛成一个
// 策略判据，调用方只消费「主窗口 / 自绘浮层 / 原生菜单」三种既定所有权，不再靠散落的 cfg 和注释猜。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayInteractionMode {
    /// 应用直接接收鼠标事件：左/右键都切换自绘浮层。
    DirectClicks,
    /// 桌面托盘宿主接管点击并展示原生菜单（Linux AppIndicator）。
    NativeMenu,
}

#[must_use]
const fn tray_interaction_mode(platform: Platform) -> TrayInteractionMode {
    match platform {
        Platform::Mac | Platform::Win => TrayInteractionMode::DirectClicks,
        Platform::Linux | Platform::Other => TrayInteractionMode::NativeMenu,
    }
}

/// macOS/Windows 的左/右键是否应切换托盘浮层。只在按键抬起时执行，
/// 避免一次点击的 down/up 两帧各触发一次；macOS 双指辅助点按由系统归为 Right，与左键同语义。
/// Linux/未知平台由原生菜单持有事件，任何偶发派发都忽略，防止两个菜单叠开。
#[must_use]
fn tray_click_toggles_overlay(
    platform: Platform,
    button: tauri::tray::MouseButton,
    state: tauri::tray::MouseButtonState,
) -> bool {
    if tray_interaction_mode(platform) != TrayInteractionMode::DirectClicks
        || state != tauri::tray::MouseButtonState::Up
    {
        return false;
    }
    matches!(
        button,
        tauri::tray::MouseButton::Left | tauri::tray::MouseButton::Right
    )
}

// ── A7：Linux 原生兜底菜单（AppIndicator 不递送点击时唯一够得着功能面的入口）───────────
//
// AppIndicator 下 `set_show_menu_on_left_click(false)` 是 **no-op**，Tauri 也明确不派发 Linux
// `TrayIconEvent`；因此 Linux 的稳定入口只有桌面宿主展示的原生菜单。它此前只有「显示 / 退出」两项：
// 连接开关、接管方式、分流策略、设置、检查更新**全部够不着**。
//
// macOS/Windows 不再装这棵菜单：右键的唯一所有者是自绘浮层，若同时挂原生菜单，系统会先消费右键并
// 弹 NSMenu/HMENU，应用即使收到事件也只能得到两个重叠表面。菜单代码仍跨平台可编译和单测，但运行期
// 仅 [`TrayInteractionMode::NativeMenu`] 会装载。

/// 落到**原生托盘菜单**上的全部输入 —— 菜单幂等重建的比较键（与 [`TrayVisual`] 同款闸门思路）。
///
/// 菜单项文案随 `lang` 变、连接项文案随 `running` 变、两个子菜单的勾选随 `mode` / `mode_type` 变。
/// 这四者之外没有任何输入能改变菜单上的字节。
#[derive(Clone, PartialEq, Eq, Debug)]
struct TrayMenuModel {
    running: bool,
    mode: String,
    mode_type: String,
    lang: crate::i18n::Lang,
}

/// 上次**成功装到托盘上**的菜单模型。`None` = 未知 → 下次无条件重建。
static TRAY_MENU: std::sync::Mutex<Option<TrayMenuModel>> = std::sync::Mutex::new(None);

/// 菜单幂等闸门（纯函数，副作用经 `apply` 注入 → 可单测）。与 [`reconcile_tray_visual`] 逐字同构，
/// 理由也同构：汇流点被 30s 轮询无条件叫醒，而 GTK 侧每次 `set_menu` 都要重建整棵 widget 树 ——
/// 用户正把菜单**打开着**时重建，多数 StatusNotifier host 上表现为菜单闪一下甚至收起。
///
/// 失败置 `None`（不照存）的理由同 [`reconcile_tray_visual`]：存了就等于宣称托盘上现在长这样，
/// 之后每一轮都短路、再也不重试 = 自愈网被自己的缓存关掉。
fn reconcile_tray_menu_model(
    cache: &mut Option<TrayMenuModel>,
    next: TrayMenuModel,
    apply: impl FnOnce(&TrayMenuModel) -> bool,
) -> bool {
    if cache.as_ref() == Some(&next) {
        return false;
    }
    *cache = apply(&next).then_some(next);
    true
}

/// 菜单项 id → 前缀常量。子菜单项 id 形如 `tray_takeover:tun` / `tray_routing:global`，
/// 由 [`parse_menu_action`] 解析回动作（纯函数，可单测）。
const MENU_ID_TAKEOVER: &str = "tray_takeover:";
const MENU_ID_ROUTING: &str = "tray_routing:";

/// 原生菜单项点击 → 动作（纯函数：id 字符串是菜单与 handler 之间唯一的契约面，解析必须可单测，
/// 否则「子菜单 id 拼错 → 点了没反应」这类错只能真机撞）。
#[derive(Debug, PartialEq, Eq)]
enum MenuAction {
    Show,
    Quit,
    ToggleProxy,
    OpenSettings,
    CheckUpdate,
    /// 切接管方式（`config.proxyModeType`）。载荷已由 [`crate::tray::TAKEOVER_KINDS`] 白名单归一。
    Takeover(&'static str),
    /// 切分流策略（`config.proxyMode`）。载荷已由 [`crate::tray::ROUTING_MODES`] 白名单归一。
    Routing(&'static str),
}

/// 菜单 id → [`MenuAction`]。未登记 id 返 `None`（handler 静默忽略，不猜）。
///
/// 子菜单载荷**回查白名单常量**再返 `'static` 串，而不是把 id 里的尾巴直接透传去写配置：
/// 写进 `config.proxyMode` 的值域必须由本文件钉死，不能取决于「谁拼的这个菜单 id」。
fn parse_menu_action(id: &str) -> Option<MenuAction> {
    match id {
        "tray_show" => return Some(MenuAction::Show),
        "tray_quit" => return Some(MenuAction::Quit),
        "tray_toggle" => return Some(MenuAction::ToggleProxy),
        "tray_settings" => return Some(MenuAction::OpenSettings),
        "tray_check_update" => return Some(MenuAction::CheckUpdate),
        _ => {}
    }
    if let Some(kind) = id.strip_prefix(MENU_ID_TAKEOVER) {
        return crate::tray::TAKEOVER_KINDS
            .into_iter()
            .find(|k| *k == kind)
            .map(MenuAction::Takeover);
    }
    if let Some(mode) = id.strip_prefix(MENU_ID_ROUTING) {
        return crate::tray::ROUTING_MODES
            .into_iter()
            .find(|m| *m == mode)
            .map(MenuAction::Routing);
    }
    None
}

/// 按模型建整棵原生托盘菜单（项集对齐 上游 `TrayManager.ts:392-441` 的 contextMenu）。
///
/// 项序：连接/断开 ─┈─ 接管方式▸ 分流策略▸ ─┈─ 打开设置 检查更新 ─┈─ 显示 Polaris 退出。
/// 两个子菜单用 `CheckMenuItem` 显当前档（上游 用 `type:'radio'` + `checked`，Tauri 等价物即 check 项）。
fn build_tray_menu(
    app: &tauri::AppHandle,
    m: &TrayMenuModel,
) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};

    let lang = m.lang;
    let toggle = MenuItem::with_id(
        app,
        "tray_toggle",
        crate::i18n::t(
            lang,
            if m.running {
                crate::i18n::key::TRAY_DISCONNECT
            } else {
                crate::i18n::key::TRAY_CONNECT
            },
        ),
        true,
        None::<&str>,
    )?;

    let takeover_items = crate::tray::TAKEOVER_KINDS
        .iter()
        .map(|k| {
            CheckMenuItem::with_id(
                app,
                format!("{MENU_ID_TAKEOVER}{k}"),
                crate::i18n::t(lang, crate::tray::takeover_key(k)),
                true,
                *k == m.mode_type,
                None::<&str>,
            )
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let takeover = Submenu::with_items(
        app,
        crate::i18n::t(lang, crate::i18n::key::TRAY_GROUP_TAKEOVER),
        true,
        &takeover_items
            .iter()
            .map(|i| i as &dyn tauri::menu::IsMenuItem<tauri::Wry>)
            .collect::<Vec<_>>(),
    )?;

    let routing_items = crate::tray::ROUTING_MODES
        .iter()
        .map(|v| {
            CheckMenuItem::with_id(
                app,
                format!("{MENU_ID_ROUTING}{v}"),
                crate::i18n::t(lang, crate::tray::routing_key(v)),
                true,
                *v == m.mode,
                None::<&str>,
            )
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let routing = Submenu::with_items(
        app,
        crate::i18n::t(lang, crate::i18n::key::TRAY_GROUP_MODE),
        true,
        &routing_items
            .iter()
            .map(|i| i as &dyn tauri::menu::IsMenuItem<tauri::Wry>)
            .collect::<Vec<_>>(),
    )?;

    let settings = MenuItem::with_id(
        app,
        "tray_settings",
        crate::i18n::t(lang, crate::i18n::key::TRAY_OPEN_SETTINGS),
        true,
        None::<&str>,
    )?;
    let check_update = MenuItem::with_id(
        app,
        "tray_check_update",
        crate::i18n::t(lang, crate::i18n::key::TRAY_CHECK_UPDATE),
        true,
        None::<&str>,
    )?;
    let show = MenuItem::with_id(
        app,
        "tray_show",
        crate::i18n::t(lang, crate::i18n::key::TRAY_OPEN_MAIN),
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(
        app,
        "tray_quit",
        crate::i18n::t(lang, crate::i18n::key::TRAY_QUIT),
        true,
        None::<&str>,
    )?;

    Menu::with_items(
        app,
        &[
            &toggle,
            &PredefinedMenuItem::separator(app)?,
            &takeover,
            &routing,
            &PredefinedMenuItem::separator(app)?,
            &settings,
            &check_update,
            &PredefinedMenuItem::separator(app)?,
            &show,
            &quit,
        ],
    )
}

/// Linux 原生托盘菜单的**唯一汇流点**（与 [`reconcile_tray_icon`] 并列，同一批驱动源叫醒）：
/// 回读 proxy / config 真值 → 模型变了才重建菜单。macOS/Windows 不调用本函数，右键由自绘浮层独占。
fn reconcile_tray_menu(app: &tauri::AppHandle) {
    let Some(tray) = app.tray_by_id("main") else {
        return; // 托盘整体缺失 → 无菜单可装
    };
    let rt = app.try_state::<AppRuntime>();
    // 只投影要用的两个字段（`ConfigManager::with_current` 持读锁投影，不产整份 owned `Value`）：本汇流点
    // 挂着 30s 自愈轮询，用 `current()` 等于每 30s 白拷一份 200 节点级配置去读两个字符串。
    // ⚠️ `app_lang(app)` 自己也要读配置 —— 必须留在闭包**外**（嵌套读锁是 `with_current` 的禁忌）。
    let (mode, mode_type) = rt
        .as_ref()
        .and_then(|r| {
            r.config()
                .with_current(|c| {
                    (
                        c.get("proxyMode")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                        c.get("proxyModeType")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                    )
                })
                .ok()
        })
        .unwrap_or((None, None));
    let next = TrayMenuModel {
        running: rt.as_ref().is_some_and(|r| r.proxy().status().running),
        // 缺省与前端一致：`TrayMenu.tsx` 的 `config?.proxyMode ?? 'smart'` /
        // `config?.proxyModeType ?? 'systemProxy'`（两个入口显示同一档，不许分叉）。
        mode: mode.unwrap_or_else(|| "smart".to_string()),
        mode_type: mode_type.unwrap_or_else(|| "systemProxy".to_string()),
        lang: crate::i18n::app_lang(app),
    };
    let mut cache = TRAY_MENU.lock().unwrap_or_else(|e| e.into_inner());
    reconcile_tray_menu_model(&mut cache, next, |m| match build_tray_menu(app, m) {
        Ok(menu) => match tray.set_menu(Some(menu)) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("托盘原生菜单装载失败（{m:?}）：{e}");
                false
            }
        },
        Err(e) => {
            log::warn!("托盘原生菜单构建失败（{m:?}）：{e}");
            false
        }
    });
}

/// 托盘汇流点的统一叫醒入口：三平台都刷新图标；仅 Linux 刷新原生菜单。两者各自幂等短路，多叫无害。
fn reconcile_tray(app: &tauri::AppHandle) {
    reconcile_tray_icon(app);
    if tray_interaction_mode(Platform::current()) == TrayInteractionMode::NativeMenu {
        reconcile_tray_menu(app);
    }
}

/// 主进程侧系统通知（`tauri-plugin-notification`）。
///
/// **只给「没有任何 UI 表面可回显」的腿用** —— 目前唯一调用点是托盘**原生兜底菜单**的「检查更新」：
/// 浮层有 notice 行、主窗有 toast，唯独原生菜单什么都没有，而 Linux 上它恰恰是主交互面。
/// 别把它当通用提示出口：应用内能看见的地方一律走 toast，系统通知会进通知中心/锁屏，成本高得多。
///
/// 失败静默（只记日志）：用户没给通知权限 / 平台不支持时，**通知发不出去不该反过来影响业务动作**
/// ——检查更新本身已经跑完了。
fn notify_user(app: &tauri::AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        log::warn!("系统通知发送失败（已忽略）: {e}");
    }
}

/// Linux 原生菜单动作执行（副作用腿）。
///
/// 业务动作**复用 `commands::*` 里那几个 `#[tauri::command]` 函数本体**，不另写一份：它们同时也是浮层
/// 与主窗走的那条路径（`proxy_start` 的「只在核真起来了才广播 proxyStarted」、`config_save` 的
/// 隐私 hash 回填 + 后端权威字段兜底都在里面）。绕过它们直接调 runtime 就会得到一条**语义不同**的
/// 第二实现 —— 那正是本仓反复出现的分叉源。
fn run_menu_action(app: &tauri::AppHandle, action: MenuAction) {
    match action {
        MenuAction::Show => show_main_window(app),
        MenuAction::Quit => {
            app.state::<QuitState>().0.store(true, Ordering::SeqCst);
            app.exit(0);
        }
        MenuAction::OpenSettings => {
            // 与浮层「打开设置」逐字节同一条路径（含轻量模式重建时的首帧种子腿）。
            let _ = tray::tray_show_main(app.clone(), Some("settings".into()));
        }
        MenuAction::ToggleProxy => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let state = app.state::<AppRuntime>();
                if state.proxy().status().running {
                    let _ = commands::proxy::proxy_stop(app.clone(), state).await;
                } else {
                    // 不再在此读配置：起核载荷由 `proxy_start` 自己读盘（见其头注）。
                    // 读失败的诊断也收口在那里 —— 本处 `let _` 丢弃返回值，留在这里会随之丢掉。
                    let _ = commands::proxy::proxy_start(app.clone(), state).await;
                }
            });
        }
        // Routing / Takeover 都要**落盘 + 触发配置评估**（可能连带重启内核）。菜单事件回调跑在
        // **主线程**（Linux 上就是 GTK 主线程）⇒ 同步跑完整条链会把 UI 卡一拍：菜单项按下去不弹回、
        // 托盘图标不重绘。同文件的 ToggleProxy / CheckUpdate 早就 spawn 了，这两条是漏网的
        // （2026-07-28 复审 LOW）。
        //
        // 用 `spawn_blocking` 而非 `spawn`：这两个 command 是**同步阻塞**函数（文件写 + 规则评估），
        // 丢进异步 worker 会占着 tokio 的协作式线程不还。reviewer 写的是 `spawn`——这里的出入只在
        // 「丢到哪个池」，「离开主线程」这个根因两者相同，而 blocking 池才是同步阻塞工作的正确去处。
        MenuAction::Routing(mode) => {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let state = app.state::<AppRuntime>();
                let _ = commands::config::config_update_mode(
                    app.clone(),
                    state,
                    serde_json::Value::String(mode.to_string()),
                );
            });
        }
        MenuAction::Takeover(kind) => {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let state = app.state::<AppRuntime>();
                let Ok(mut cfg) = state.config().current() else {
                    log::warn!("托盘菜单切接管方式：读配置失败 → 跳过");
                    return;
                };
                cfg["proxyModeType"] = serde_json::Value::String(kind.to_string());
                // 与浮层 / 主窗同源：切到 TUN 时消费「FakeIP-TUN 待纠正」快照（见 tray::apply_fake_ip_tun_entry）。
                // 走**全量 config_save**（而非 set_value 单键）正是为了让这次纠正也落盘 —— 单键写等于把
                // 三个入口里的这一个悄悄降级成不纠正版。
                if crate::tray::apply_fake_ip_tun_entry(&mut cfg) {
                    log::info!(
                        "托盘原生菜单进入 TUN：已自动回填 enableFakeIp=true（消费迁移期待纠正快照）"
                    );
                }
                // `defer_restart=None`：托盘切接管方式是**用户此刻要它生效**的动作，不是「保存」，
                // 不得降级到待应用差集（降了 = 点了切 TUN 却什么都没发生）。
                // `base_version=None`：托盘不产生 staged（spec §Q8-b 闸 2），它永远是「被合并方」
                // 而非「冲突方」——挂上乐观并发只会让托盘操作在别人写盘时莫名失败。
                let _ = commands::config::config_save(app.clone(), state, cfg, None, None);
            });
        }
        MenuAction::CheckUpdate => {
            let app = app.clone();
            // 与浮层「检查更新」**同一个** command 本体（tray::tray_check_update）：两个入口共用一条链，
            // 不会出现「菜单查到的和浮层查到的不一样」。
            //
            // 结果经系统通知回显（2026-07-28 复审 MED）：此前「已是最新」与「失败」都只入日志 ⇒
            // 用户零反馈。而 **Linux 上原生菜单就是主交互面**（左键递送不可靠，`set_show_menu_on_left_click`
            // 在 appindicator 下是 no-op），点了没动静与按钮坏了完全不可分辨。浮层有 notice 行、
            // 主窗有 toast，原生菜单**没有任何 UI 表面** → `tauri-plugin-notification`（已在 builder
            // 注册 + capability `notification:default` 已授权）是唯一送达路径。
            //
            // `hasUpdate == true` 那一支**刻意不发通知**：提醒窗已经弹在屏幕上了，再叠一条系统通知
            // 就是同一件事说两遍。
            tauri::async_runtime::spawn(async move {
                let lang = i18n::app_lang(&app);
                let r = tray::tray_check_update(app.clone()).await;
                let body = if r.success {
                    if r.data == Some(true) {
                        return; // 有更新 → 提醒窗自己就是反馈
                    }
                    i18n::t(lang, i18n::key::TRAY_UP_TO_DATE)
                } else {
                    let why = r
                        .error
                        .unwrap_or_else(|| i18n::t(lang, i18n::key::NATIVE_UNKNOWN_ERROR));
                    log::warn!("托盘原生菜单检查更新失败: {why}");
                    format!(
                        "{}: {why}",
                        i18n::t(lang, i18n::key::TRAY_UPDATE_CHECK_FAILED)
                    )
                };
                notify_user(
                    &app,
                    &i18n::t(lang, i18n::key::NATIVE_UPDATE_NOTIFY_TITLE),
                    &body,
                );
            });
        }
    }
}

/// 建（或**重建**）主窗：conf 声明 + per-platform 窗口铬（transparent/decorations）+ vibrancy/Mica 特效
/// + mount 健康门武装 + 窗口事件接线（可见性 → stats 门控 / 关窗语义 / 主题跟随）。
///
/// **两处调用**：① `setup` 首次建窗；② `show_main_window_on_main_thread` 在 **C16 轻量模式销毁 webview** 后 `get_webview_window("main")`
/// 返 None 时**重建**。故所有 per-window 装配必须收在**这一处**——重建才与首建逐字节等价（否则重建窗少了
/// 关闭进轻量 / 白屏自愈 / 主题跟随，成半残窗）。`on_page_load` / 图标 scheme / 托盘监听是 app 级（builder /
/// setup 一次装），不在此、重建自动覆盖同 `label=="main"`。
/// 两个调用点都在 Tauri 主线程；macOS 的 `apply_vibrancy` 会硬性校验这一点。
///
/// `start_hidden`：true → 建成**隐藏**窗（`--hidden` / `silentStart`；托盘作唤出锚点）。重建路径恒传 false
/// （用户显式唤出即要可见）。返回 `Box<dyn Error>` 与 `setup` 同型，`?` 直冒泡（`.first()` None 仅理论态：conf 恒声明主窗）。
fn create_main_window(
    app: &tauri::AppHandle,
    start_hidden: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // 窗口特效总门控（与首建同源：读 config.json 原文本，不依赖 store 具体 API）。
    // `windowEffects` / `hardwareAcceleration` 任一显式 false → 不上特效。判定收在 graphics_compat 的纯函数里
    // （可单测）；**别在这儿重写这个 OR** —— 建窗代码在 cfg 门内，单测够不着，逻辑放这儿等于没门。
    let config_dir = app
        .path()
        .app_config_dir()
        .map(|p| p.join("polaris"))
        .unwrap_or_else(|_| std::path::PathBuf::from("./polaris"));
    let raw_config = graphics_compat::read_config_raw(&config_dir);
    let apply_effects = graphics_compat::should_apply_window_effects(raw_config.as_deref());

    // 用官方文档化的 `WebviewWindowBuilder::from_config` 复用同一份 conf 声明（`tauri-utils/src/config.rs`
    // 的 doc 示例即此模式），零字段重复、行为与 conf 直建逐字节相同。保留建窗模式（而非 conf create:true）是为
    // **B6 窗口铬**：mac `hiddenInset`+vibrancy / Windows Mica 需要 per-platform 的 `transparent`（builder-only、
    // 运行期不可改），单条 conf window 声明表达不了。
    let window_config = app
        .config()
        .app
        .windows
        .first()
        .cloned()
        .ok_or_else(|| "tauri.conf.json 未声明主窗".to_string())?;
    // per-platform 窗口铬（B6）：先 from_config 复用 conf 声明，再按平台覆盖 transparent，最后 build，再挂特效。
    //   · mac/win：开 transparent（配合前端 `.win` 圆角 + 半透底，让圆角内容成为可见轮廓）。
    //   · Linux：**恒不开** transparent、**不调用任何特效**——transparent:false 是白屏逃生门路径，绝不翻转。
    #[allow(unused_mut)]
    let mut builder = WebviewWindowBuilder::from_config(app, &window_config)?;

    // ── B：主题接线（此前后端零读 `uiTheme`，三处原生面全硬编码深色）──
    //
    // ① **首帧预解析脚本**：`initialization_script` 先于页面任何脚本执行、且不受页面 CSP `script-src 'self'`
    //    限制（与 `tray::TRAY_BLUR_DISMISS_JS` / `update_popup` 的 init_script 同款手法）。它把 `data-theme`
    //    在第一帧之前就播种到 `<html>` 上 —— 而**能同步读到 `uiTheme` 真值的只有主进程**（它在 config.json
    //    里，前端拿到它已经是 IPC 之后）。只播种不接管，运行期真值仍归 `AppShell.tsx` 那个 effect。
    // ② **窗口原生底色**：`from_config` 用的是 conf 里写死的 `#0B0F14`。显式选浅色的用户，窗口原生底
    //    （webview 出图之前就在屏上的那一层）会先闪一格深色。按 `uiTheme` 覆写掉。
    //
    // ⚠️ `uiTheme='system'` 且**当前一个窗都没有**（首次建主窗）时探不到系统明暗（Tauri 2.11 无 app 级
    //    theme getter，只有 `Window::theme()`）→ `resolve_native_dark` 回落深色 = 与本改动前逐字节相同。
    //    这条缺口只影响 system 档的**冷启动首帧**；显式 light 档（真正会抱怨闪深色的那批人）已完整修好。
    let dark = tray::native_dark(app);
    builder = builder
        .initialization_script(tray::theme_boot_script(dark))
        // **必须在下面 mac/win 特效分支之前**：那一支要把底色覆写成全透明，builder 是后写胜出。
        // 放这里也让 **Linux** 覆盖到 —— 特效分支整个在 `cfg(any(macos, windows))` 门内，
        // Linux 根本进不去，若把主题底色写进那个 else，Linux 主窗会继续吃 conf 的写死深色。
        .background_color(tray::window_bg_color(dark));
    // A1 首帧种子腿：轻量模式销毁主窗后经托盘「打开设置」重建时，事件腿必丢（订阅还没挂上）→
    // 目标屏随文档注入。take 语义 ⇒ 消费一次即清，不会在后续重建里反复跳屏。
    if let Some(screen) = tray::take_pending_screen(app) {
        builder = builder.initialization_script(tray::tray_screen_boot_script(screen));
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if apply_effects {
        // mac/win：真透明窗。transparent(true) 覆盖 conf 的 transparent:false；并把 conf 的方形不透明
        // backgroundColor(#0B0F14) 覆盖成**全透明**——否则该纯色底铺满方形 rect，会在前端 `.win` 14px 圆角**外**
        // 露方角。清背景后圆角轮廓由 vibrancy/Mica（radius=14）+ 前端 `.win`（--r-lg:14px）共同构成。
        builder = builder
            .transparent(true)
            .background_color(tauri::window::Color(0, 0, 0, 0));
    } else {
        // 特效关（windowEffects=false 或图形逃生门开）→ 保持 conf 的 transparent:false 实色底，但底色
        // **按主题覆写**（conf 里写死的是深色 #0B0F14）——不做上面的透明覆盖。
        // 这一支是必需的、不是可省的优化：前端 `.side`（mac）与 `.win`/`.stage`（mac 且特效开）在 CSS 里是
        // `background:transparent`，指望原生特效当底。若这里仍建透明窗而不上特效，
        // 侧栏会直接透出桌面 = 半透明穿透窗，与开关文案承诺的「纯色背景」相反。透明只为透出特效而存在，
        // 没特效就不该透明。此时不透明窗的方角由 mac 原生 decorations(true) 圆角收边；Windows 无原生圆角，
        // 关特效即方角窗——这正是 Mica 关掉后该有的样子，不再自绘伪圆角。
        log::info!(
            "窗口特效已关（windowEffects=false 或 hardwareAcceleration=false）→ 建不透明窗，实色底按 uiTheme 折算（dark={dark}）"
        );
    }
    // ── macOS 原生窗口铬（P1：交通灯点击无反应 + 窗口拖不动 根因修复）──
    // conf 的 `decorations:false` 会剥掉 mac 原生窗口控制的**功能**（styleMask 全被剥），此处翻回 true：
    // 重挂交通灯功能 + 原生标题栏拖动 + 四角原生圆角；titleBarStyle:Overlay + hiddenTitle:true 仍由 conf 套上。
    #[cfg(target_os = "macos")]
    {
        // ⚠️ 这里**不再重复设尺寸**：`from_config`（:942）已把 conf 的 width/height/minWidth/minHeight
        // 套上，mac 分支曾另写一份 `inner_size(925,740)` + `min_inner_size(760,560)` 覆盖掉它 ——
        // 于是 conf 里那四个值在 mac 上恒为死值，改 conf 不生效（陈先生 2026-07-29 真机报「没有锁定
        // 最小限制」，实测最小可缩到 760×560 而非 conf 写的值）。尺寸单一真值收回 conf。
        builder = builder
            .decorations(true)
            .resizable(true)
            // 交通灯 inset 到侧栏 .side-chrome(36px 净空)内，别贴窗角（默认 ~7,7 太贴边）。真机微调。
            .traffic_light_position(tauri::LogicalPosition::new(13.0, 18.0));
    }
    // C15/C16：start_hidden（--hidden / silentStart / 轻量前的隐藏建窗）→ 建成隐藏窗，覆盖 conf 的可见默认。
    // 靠托盘浮层的明确入口/Dock 唤出（`show_main_window`）；托盘缺失时 setup 末尾兜底显示（见 setup 无锚点分支）。
    //
    // **非 start_hidden 也一律建成隐藏窗**（门武装时）：conf 未声明 `visible` ⇒ 默认 true ⇒ 此前
    // `builder.build()` 返回那一刻窗口就在屏上，而 webview 那时才刚开始加载文档、解析 bundle、挂 React
    // —— 中间那段**空白窗**在 mac 真机实测 345–2467ms（用户报的「点图标先白屏一会儿」正是长尾那几次）。
    // 改由 `renderer:ready` 决定上屏时机（`defer_show`，超期有兜底），窗口出现即有内容。
    // 传 `None` 而非查门状态：轻量模式重建时门里还躺着被销毁那个旧文档的 ready=true。
    let defer_show = !start_hidden
        && window_health::show_timing(app, None) == window_health::ShowTiming::WhenReady;
    if start_hidden || defer_show {
        builder = builder.visible(false);
    }
    let window = builder.build()?;
    window_health::log_show_probe(app, "window-built", false);
    // Tauri 的窗口 registry 在 destroy/create 过渡期不等同于“可用 WebView”。把建窗成功作为明确的
    // 生命周期提交点，供 stats/logs 的非阻塞可见性门共享；窗口此刻仍隐藏，renderer ready 后再翻可见。
    if let Some(rt) = app.try_state::<AppRuntime>() {
        rt.stats().mark_main_window_created();
    }

    // ── 原生窗口外观跟随 `config.uiTheme`（不是跟随系统）──
    // vibrancy/Mica 的明暗由 **NSWindow/HWND 的 appearance** 决定，而不是网页里的 `data-theme`。
    // 此前从未设过窗口外观 ⇒ 原生面恒跟系统：系统深色 + 应用内选浅色时，`NSVisualEffectMaterial::Sidebar`
    // 渲染深色变体，表现为「浅色模式下侧栏透明效果是黑的」（陈先生 2026-07-29 真机报）。
    // 判定复用 `tray::resolve_native_dark` —— 托盘原生面已在用同一条判据，两处各写一份必然分叉。
    // `uiTheme=system` 时显式传 `None`：交回给系统跟随，而不是把当下探到的值钉死（否则用户改系统
    // 明暗后窗口外观不跟）。
    {
        let ui_theme = tray::ui_theme(app);
        let native_theme = match ui_theme.as_deref().map(str::trim) {
            Some("dark") => Some(tauri::Theme::Dark),
            Some("light") => Some(tauri::Theme::Light),
            _ => None,
        };
        if let Err(e) = window.set_theme(native_theme) {
            log::warn!("主窗原生外观设置失败（vibrancy 明暗可能与应用内主题不一致）：{e}");
        }
    }

    // 特效仅 mac/win；任何失败绝不 fatal——窗背景已在建窗时清成全透明，失败即无 blur，可见底改由前端 `.win`
    // 自绘兜底。特效门控：hardwareAcceleration=false（图形逃生门开启）→ 跳过，与逃生门联动一致。
    #[cfg(target_os = "macos")]
    {
        if !apply_effects {
            log::info!("窗口特效已关 → 跳过 macOS vibrancy；窗口已建为不透明实色底（见上）");
        } else {
            use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial, NSVisualEffectState};
            if let Err(e) = apply_vibrancy(
                &window,
                NSVisualEffectMaterial::Sidebar,
                Some(NSVisualEffectState::Active),
                Some(14.0),
            ) {
                log::warn!("macOS vibrancy 失败，降级纯色底：{e}");
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        if !apply_effects {
            log::info!("窗口特效已关 → 跳过 Windows Mica；窗口已建为不透明实色底（见上）");
        } else {
            use window_vibrancy::apply_mica;
            if let Err(e) = apply_mica(&window, None) {
                log::warn!("Windows Mica 失败（非 Win11），降级纯色底：{e}");
            }
        }
    }
    // Linux：不 transparent、不调用任何特效 —— WebKitGTK 无 vibrancy/Mica 等价物，特效分支根本不编译进
    // Linux 目标。故 windowEffects 在 Linux 是结构性 no-op，UI 侧同步隐藏该行（不留死开关）。
    let _ = apply_effects; // Linux：apply_effects 仅 mac/win 特效分支读

    // ── 武装 mount 健康门 ──
    // 记录应用真实 URL（任何导航发生前），供超时 reload / fatal_retry 导航回真实应用。
    if let Some(health) = app.try_state::<WindowHealth>() {
        match window.url() {
            Ok(url) => health.set_app_url(url),
            Err(e) => log::warn!("主窗 URL 捕获失败 {e}：超时重载将回退 reload()"),
        }
        if health.gate_enabled() {
            log::info!("mount 健康门已武装（等待 renderer:ready）");
        }
    }
    // 登记「等就绪再上屏」**必须早于** PageStarted 武装：兑现腿挂在 `renderer:ready` 上，意图晚于 ready
    // 到达就再也没人来兑现 = 窗口永不出现。二者都在主线程同步段内、webview 的 JS 此刻还跑不起来，本无
    // 竞态可言；这里排在前面是把「不可能」写成「结构上不可能」。
    if defer_show {
        window_health::defer_show(app);
    }
    // 窗口创建即武装：这是唯一不依赖任何平台信号的武装点（macOS/Linux 加载失败时 Started 也不触发）。
    window_health::dispatch(app, MountGateEvent::PageStarted);

    // 窗口可见性 → stats relay 门控（stats-worker 据此降流）+ 关窗语义（放行退出 / 收托盘 / 真退出）。
    // Win/Linux 另桥接原生 maximize 变化：双击拖动带 / 系统菜单 / 拖顶不会经过自绘按钮 command，
    // 只能从 WindowEvent::Resized 回读真值；AtomicBool 只在值变化时发事件，普通 resize 零噪音。
    #[cfg(not(target_os = "macos"))]
    let maximized_state = Arc::new(AtomicBool::new(window.is_maximized().unwrap_or(false)));
    #[cfg(not(target_os = "macos"))]
    let event_window = window.clone();
    let app_handle = app.clone();
    window.on_window_event(move |event| match event {
        tauri::WindowEvent::Focused(_) => {
            // **不取 focused 的值**：失焦 ≠ 隐藏。窗口失焦但仍在屏上时依然有 UI 消费者，按 focused
            // 降流会让用户看着的首页拓扑/连接明细直接冻住。Tauri 2 的 `WindowEvent` 又没有 show/hide
            // 变体，故这里只把 Focused 当「显隐可能刚变」的**即时触发器**：真值由 stats 侧回读窗口实况
            //（`is_visible() && !is_minimized()`）派生，并在变化时立刻唤醒降流中的 poller（恢复不等整拍）。
            // 不发 Focused 的显隐（如托盘在窗口本就失焦时隐藏它）由 poller 每拍的实况回读兜底。
            refresh_stats_visibility(&app_handle);
        }
        #[cfg(not(target_os = "macos"))]
        tauri::WindowEvent::Resized(_) => match event_window.is_maximized() {
            Ok(maximized) => {
                if commit_maximized_observation(&maximized_state, maximized) {
                    commands::window::emit_window_maximize_changed(&app_handle, maximized);
                }
            }
            Err(e) => log::warn!("回读主窗最大化状态失败，标题栏图标可能暂时不同步：{e}"),
        },
        // 关闭主窗语义：判定收在纯函数 [`resolve_close_action`]（含 #10 的 `config.minimizeToTray`
        // 门控），此处只执行。托盘存在与否 + minimizeToTray **都动态查**：本闭包在托盘 setup 前也可能
        // 装上（首建），且设置改完须即时生效——不捕获任何陈旧快照。
        tauri::WindowEvent::CloseRequested { api, .. } => {
            let quitting = app_handle.state::<QuitState>().0.load(Ordering::SeqCst);
            let tray_present = app_handle.tray_by_id("main").is_some();
            match resolve_close_action(quitting, tray_present, config_minimize_to_tray(&app_handle))
            {
                CloseAction::AllowClose => {}
                CloseAction::EnterLightweight => {
                    api.prevent_close();
                    // 明确点“关闭”与最小化分流：前者进入轻量驻留，后者仍只 minimize。暂存层已经
                    // 持久化到 localStorage，可跨 WebView 重建恢复；正在编辑但尚未提交的弹窗草稿按
                    // 关闭窗口语义丢弃。自动轻量开关只控制 idle 触发，不控制本条显式关闭腿。
                    //
                    // W18（2026-08-19 真机）：**不得在本回调帧内同步销毁 WebView2**。Windows 上
                    // CloseRequested 跑在窗口自身 close 消息的分发栈里，帧内 `destroy()` 主窗 +
                    // 托盘浮层两个 WebView 会把消息泵楔死——症状：首实例托盘全无响应，双击桌面
                    // 再起一个进程、双托盘图标、主窗谁也弹不出，只能任务管理器全杀。帧内只做两件
                    // 轻事：挡关闭 + 立即隐藏（视觉即时关闭）；转场销毁排到帧外——先跳 async
                    // 线程再 `run_on_main_thread` 排回事件循环（注意：从主线程调
                    // `run_on_main_thread` 是内联直执，不跳线程等于没排）。排队失败只 warn：主窗
                    // 已隐藏、可经托盘唤出重建，不比不修差。
                    if let Some(win) = app_handle.get_webview_window("main") {
                        let _ = win.hide();
                    }
                    let h = app_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        let h2 = h.clone();
                        if let Err(e) = h.run_on_main_thread(move || {
                            // F2 复核（评审）：排队期间（极端调度饥饿下）用户可能已把主窗唤回
                            // （show_main_window 对未销毁的窗走 present）。此刻可见 = 用户意图
                            // 已翻盘，放弃本轮销毁——LightweightState 由转场本体置位，跳过即
                            // 无需回滚。idle 巡检腿对同一函数有同款复核，这里补齐对称。
                            if let Some(win) = h2.get_webview_window("main") {
                                if win.is_visible().unwrap_or(false) {
                                    log::info!(
                                        "轻量转场复核：主窗在排队期间被重新唤出，放弃本轮销毁"
                                    );
                                    return;
                                }
                            }
                            crate::tray::enter_lightweight_transition(h2);
                        }) {
                            log::warn!("轻量转场排队失败（主窗已隐藏，可经托盘唤出重建）：{e}");
                        }
                    });
                }
                CloseAction::QuitApp => {
                    // 置 QuitState 再退：这条腿现在也会在**托盘在**时触发（用户选了「退出应用」），
                    // 而 `ExitRequested` 的 C16 轻量守卫判据是 `lightweight && !quitting && 托盘在`
                    // —— 不置位的话，一个陈旧的 lightweight 置位会把用户的真退出 `prevent_exit` 掉。
                    app_handle
                        .state::<QuitState>()
                        .0
                        .store(true, Ordering::SeqCst);
                    app_handle.exit(0);
                }
            }
        }
        // 系统明暗切换 → Win/Linux 按新主题重选黑/白托盘图标（无 template 自动反色机制）。
        // 经汇流点（回读真值），故主题切换顺带也修正一次可能已漂移的连接态图标。
        #[cfg(not(target_os = "macos"))]
        tauri::WindowEvent::ThemeChanged(_) => {
            reconcile_tray(&app_handle);
        }
        _ => {}
    });
    Ok(())
}

/// Tauri command 注册表：Polaris 136 IPC channel → `#[tauri::command]`。
///
/// 语法：Tauri 2 的 `generate_handler![]`（与 Tauri 1 一致；插件 API 改了，handler 宏未变）。
/// 命令名 = Rust fn 名（snake_case），前端经 `invoke('proxy_start', { config })` 调用（camelCase 自动转换）。
fn main() {
    // ── C15 CLI 早退 + 启动模式（在起 Tauri GUI 之前）──
    // version/help/headless 早退在 Tauri GUI 初始化之前跑（对齐 上游：早于 requestSingleInstanceLock/whenReady），
    // 避免 headless 环境（SSH/CI 无 DISPLAY）走到 WebKitGTK 初始化崩溃/segfault。
    let args: Vec<String> = std::env::args().collect();
    #[cfg(target_os = "linux")]
    let has_display =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    #[cfg(not(target_os = "linux"))]
    let has_display = true;
    let arg_hidden = match resolve_startup(&args, has_display) {
        StartupAction::Version => {
            println!("Polaris {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        StartupAction::Help => {
            print!("{}", cli_help_text());
            return;
        }
        StartupAction::HeadlessExit => {
            eprintln!(
                "Polaris requires a graphical display (no DISPLAY/WAYLAND_DISPLAY found). Start it from a desktop session."
            );
            std::process::exit(1);
        }
        StartupAction::Run { hidden } => hidden,
    };

    // ── 原生对话框语言对账（macOS-only；**必须早于 `tauri::Builder`**）──
    //
    // 把 `config.language` 写进本应用 UserDefaults 域的 `AppleLanguages`，让 NSOpenPanel /
    // NSAlert 等 AppKit 自绘的原生 UI 跟随**应用内**语言而非系统语言。为什么这个位置不可挪、
    // 挪到 `setup` 会让用户要重启两次 —— 见 `app_language` 模块文档；顺序由 `main.rs` 的
    // `native_dialog_language_is_applied_before_appkit_boots` 守卫钉住。
    //
    // `generate_context!()` 提到这里（原先内联在 `.build()` 的实参位）**只为拿 identifier**：
    // 配置路径要用它，而在 `AppHandle` 存在之前只有 context 认得这个值。写死一份
    // "com.polaris.app" 也能跑，但 identifier 一改就静默读空 —— 那正是本模块最难发现的失效形态。
    // 放在 CLI 早退**之后**：`--version` / `--help` / headless 不该为此多做任何事。
    let ctx = tauri::generate_context!();
    app_language::apply_process_language(&ctx.config().identifier);

    tauri::Builder::default()
        // ── C2 单实例锁 ──
        // **必须第一个注册**：第二次启动的进程要在其它插件初始化前把 argv 交给首实例并自退，避免双开
        // 双核抢 TUN/端口。回调在**已存在的首实例**里触发（不在第二实例里）→ 召回并聚焦主窗，
        // 让「再点一次图标」表现为「把窗拉回前台」。
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--autostart"]),
        ))
        // ── 自定义应用图标 scheme（polaris-icon://）──
        //
        // 缓存路由 `c/<file>` 读 `<userData>/icons/` 本地副本（正常渲染零出站，隐私第一性）；
        // 远端路由 `i/<enc-url>` 经传输层单点拉取（URL 面板预览 / 未迁移旧 remote iconUrl，一次性）。
        // 见 `icon_cache` 模块文档。内置 / 解锁图标是随包 SVG，不经此 scheme。
        .register_asynchronous_uri_scheme_protocol(
            icon_cache::ICON_PROXY_SCHEME,
            |ctx, request, responder| {
                icon_cache::handle_scheme_request(
                    ctx.app_handle().clone(),
                    request.uri().clone(),
                    responder,
                );
            },
        )
        // ── mount 健康门的页面事件接线（C 类白屏侦测）──
        //
        // 只用 `Started`（= 新文档开始加载 → 重新武装），**刻意不用 `Finished`**：Windows 上 wry 丢弃了
        // NavigationCompleted 的 IsSuccess/WebErrorStatus（`wry/src/webview2/mod.rs:659-670`），加载失败
        // 照样上报 Finished → 用它判成功会误判。武装的**主**入口在 setup 内（窗口创建即武装），因为
        // macOS/Linux 上加载失败连 Started 都不触发（挂在 didCommitNavigation / LoadEvent::Committed）。
        .on_page_load(|webview, payload| {
            if webview.label() == "main"
                && payload.event() == tauri::webview::PageLoadEvent::Started
            {
                window_health::dispatch(webview.app_handle(), MountGateEvent::PageStarted);
                // 导航开始 = 旧 JS 上下文连同它的 stats / logs 页面订阅一起作废，但它已经没机会再发
                // unsubscribe 了。而 registry 按 **webview label** 记账，reload 后 label 仍是
                // "main" → 旧 token 无人退订 → 订阅计数永远 ≥1 → `stop_*_poller` 的计数闸门恒拦 →
                // poller 永久 1s gRPC 轮询、日志 emitter 永续拉环。故在此主动清两类账；新上下文
                // mount 后会按当前页面自行重订。
                // 触发面：白屏自愈 reload / 用户手动刷新 / dev 热重载。
                if let Some(rt) = webview.app_handle().try_state::<AppRuntime>() {
                    rt.stats().clear_window("main");
                }
                commands::misc::clear_log_stream_window("main");
            }
        })
        .setup(move |app| {
            // 配置目录：<app_config_dir>/polaris/（对齐 上游 `app.getPath('userData')`）。
            let config_dir = app
                .path()
                .app_config_dir()
                .map(|p| p.join("polaris"))
                .unwrap_or_else(|e| {
                    log::warn!("app_config_dir 解析失败 {e}，回落 cwd/polaris");
                    std::path::PathBuf::from("./polaris")
                });
            // 确保目录存在（首次启动）。
            let _ = std::fs::create_dir_all(&config_dir);
            // 日志 sink 必须最先装：在此之前所有 log::* 都是静默 no-op（`log` 只是门面）。
            logging::init(&config_dir);
            log::info!(
                "polaris 启动：config_dir={}, platform={:?}",
                config_dir.display(),
                Platform::current()
            );
            // 原生对话框语言对账的结果补报（它跑在 `logging::init` 之前，那时 `log::*` 还是
            // 静默 no-op）。它的失败形态全是「悄悄什么都没做」，没有这一句真机上就没有任何证据。
            app_language::log_startup_outcome();

            // ── 图形兼容逃生门（D 类合成层白屏自救）──
            // 必须在**首个 webview 创建之前**：各平台 runtime 只在创建 webview 那一刻读 GPU 环境变量。
            // 读 config.json 原文本而非走 store：此刻 store 尚未装配，且逃生门必须在「配置损坏到 store
            // 都加载不了」时仍能工作。容错第一 —— 任何异常一律回落「默认全开 = 行为不变」。
            let raw_config = graphics_compat::read_config_raw(&config_dir);
            // ── U-7 判据基线：本次进程**启动时真正读到的**三个值 ──
            // 必须在这里定格，而不是让渲染端拿「上一次保存值」当基线。反例：进程以 hardwareAcceleration=true
            // 起来 → 用户关掉（弹窗，点「稍后」）→ 又打开 ⇒ 若与上次保存值比就再弹一次，可此刻磁盘值已等于
            // 启动值、重启什么都不会变。用户要么白重启一次（**会断代理**），要么学会无视这个弹窗 —— 后者
            // 直接废掉 U-7 的全部价值。
            // 语义方向统一为 `UserConfig` 的「该功能是否开」（与渲染端 `effectiveValue` 同口径），
            // 而非各自判定函数的「是否禁用/是否上特效」，避免两侧各记一次反相。
            app.manage(StartupConfigFlags {
                hardware_acceleration: !graphics_compat::should_disable_hardware_acceleration(
                    raw_config.as_deref(),
                ),
                window_effects: graphics_compat::should_apply_window_effects(raw_config.as_deref()),
                remember_window_size: config_remember_window_size(raw_config.as_deref()),
            });
            // 图形逃生门：hardwareAcceleration=false → 设 GPU 环境变量（软件渲染）。必须在**首个 webview 创建
            // 之前**：各平台 runtime 只在建 webview 那一刻读 GPU 环境变量。窗口 vibrancy/Mica 的同一判定在
            // `create_main_window` 内按同源 raw config 重算（特效关时**不** apply，避免与逃生门叠加合成负担）。
            graphics_compat::apply_hardware_acceleration_escape(
                graphics_compat::should_disable_hardware_acceleration(raw_config.as_deref()),
            );

            // ── 可写现役核基目录注入（**必须早于任何起核路径**）──
            // `resolve_core_binary()` 是自由函数（无 AppHandle），故基目录经进程级 OnceLock 注入。
            // 未注入时它恒回落随包种子 —— 行为安全，但换核/回滚会报 CORE_DIR_UNAVAILABLE。
            runtime::core_paths::init_base_dir(config_dir.clone());

            // ── 内置 geo 规则集播种（调用点 1/2：应用启动；对齐 上游 `index.ts:1834`）──
            // 不种 → `<userData>/rules` 恒空 → route builder 一个 rule_set 都不注入 → 全部 geo 规则被
            // fail-closed 剪掉。**必须早于任何起核路径**（自动连接就在 setup 尾巴上）。
            // 幂等 + best-effort：已有有效副本跳过，失败只记日志不阻断启动。
            //
            // **启动这次（且只有这次）开出厂态刷新**（`refresh_out_of_box`，对齐 上游
            // `index.ts:1834` 的 `refreshOutOfBox: true`）：此刻无并发的规则资源更新，刷新落地无竞态。
            // 不开这条，「装 v1 → 播种 → 升 v2（随包带新 geo 数据）」的出厂态用户会永久冻结在 v1。
            // 出厂态判据取自同一份 raw config 的 `builtinGeoMeta`（上面图形逃生门已读过，不重复 IO）。
            runtime::geo_seed::seed_builtin_rule_sets_into(
                &config_dir,
                "启动",
                &runtime::geo_seed::SeedOptions {
                    network_updated_tags: runtime::geo_seed::network_updated_tags_from_raw(
                        raw_config.as_deref(),
                    ),
                    refresh_out_of_box: true,
                },
            );

            // 装配 17 crate 运行时（注入 tokio / std::fs / 真 socket / 真 HTTP client）。
            // 传输层 client 建不起来 = 网络栈残缺 → 报错退出（? 冒泡给 setup），不带病硬跑。
            let app_runtime = AppRuntime::new(config_dir)?;

            // ── 版本感知 reseed：随包核 → 可写现役核（幂等；**失败不 fatal**）──
            // 失败即回落随包种子照常起核（`resolve_core_binary` 第 3 级）⇒ 首启/迁移永不 brick。
            // 覆盖判据是纯函数 `decide_reseed`（fork/unknown/更新的核**绝不覆盖**），见 core_paths。
            match runtime::core_paths::ensure_writable_core(
                app_runtime.updater().bundled_core_version(),
            ) {
                Ok(p) => {
                    // 把现役核路径注入 UpdaterRuntime（版本双读法的探测目标；此前只认
                    // POLARIS_SINGBOX_PATH，导致非开发态恒报「未知版本」）。
                    app_runtime.updater().with_core_binary(p);
                }
                Err(e) => log::warn!("可写现役核播种失败（{e}）：回落随包核，换核功能将不可用"),
            }
            // ── C1 启动期系统代理崩溃恢复 ──
            // 上次若带系统代理退出却未清（崩溃/强杀/panic → marker 残留），早期清掉「仍指向上个已死端口的
            // 系统代理」，防本次启动前用户全网断连。marker 门控：正常 fresh start 无 marker → 零系统调用、
            // 即时返回；只有崩溃恢复路径付 exec 代价。**阻塞**跑在 UI 加载前，确保用户不带残留断网态入场。
            tauri::async_runtime::block_on(app_runtime.proxy.recover_system_proxy_on_startup());
            // ── C4 WARP 待注销队列 drain ──
            // 启动清上次会话遗留的孤儿 WARP 设备 + 定时 drain，防孤儿计费。装配点即此（AppRuntime::new
            // 之后、manage 之前）；`mesh`/`http` 是 pub Arc 字段。
            app_runtime
                .mesh
                .clone()
                .spawn_warp_drain(app_runtime.http.clone());
            // `event:proxyError` 接线：崩溃自愈跑在后台 task（无人 await），失败只能靠事件告知渲染端；
            // 而 `AppHandle` 要到此刻才有 → 运行时「先构造、后接线」（见 ProxyRuntime::error_emitter）。
            // **必须在 manage 之前**：manage 移走所有权后就只能经 State 再借，绕一圈无谓。
            app_runtime.proxy.set_error_emitter(Box::new(
                runtime::proxy::AppHandleProxyErrorEmitter {
                    app: app.handle().clone(),
                },
            ));
            app.manage(app_runtime);
            app.manage(WindowHealth::new());
            // 退出意图标记（关窗语义分流用），默认 false = 关窗按 hide/兜底走。
            app.manage(QuitState(AtomicBool::new(false)));
            // C16 轻量模式转场标记，默认 false = 非轻量销毁；进轻量前置真，供 ExitRequested 守卫保核。
            app.manage(LightweightState(AtomicBool::new(false)));
            // Q1-b ④：「本次退出是 app:restart 发起的」，默认 false = 真退出（照落正常退出标记）。
            app.manage(RestartState(AtomicBool::new(false)));
            // 托盘运行期状态（自绘浮层去抖 + 轻量重建时的待导航目标；Linux 虽不建浮层仍要后者）。
            app.manage(tray::TrayOverlay::default());

            // ── 订阅自动更新调度器（启动补更 8s + 周期巡检 30min + 代理就绪补更）──
            // 装在 AppRuntime manage 之后（运行期经 State 取 config/proxy/http）。managed 保活；
            // 内部定时器/事件接线为薄壳，决策逻辑纯函数单测覆盖。UI autoUpdate 开关据此生效（否则死装饰）。
            let scheduler =
                std::sync::Arc::new(runtime::subscription_scheduler::SubscriptionScheduler::new());
            scheduler.start(app.handle().clone());
            app.manage(scheduler);

            // ── 规则资源自动更新调度器（启动补更 12s + 周期巡检 30min）──
            // 装法与订阅调度器同构；12s 刻意错开订阅的 8s 启动高峰。UI 的
            // ruleResourceAutoUpdate / ruleResourceUpdateIntervalHours 据此生效（此前零消费者 = 死开关）。
            let rule_res_scheduler =
                std::sync::Arc::new(runtime::rule_resource_scheduler::RuleResourceScheduler::new());
            rule_res_scheduler.start(app.handle().clone());
            app.manage(rule_res_scheduler);

            // ── 内核自动更新调度器（启动 30s + 6h 巡检 + 24h due + 代理停止后 5s 落位）──
            // 装法与上面两个调度器同构。**30s 启动延迟刻意最靠后**：错开 startup_tasks 的
            // 2s 自动连接 / 3s 出口 IP / 5s App 更新检查 / 6s 内核基线 / 7s helper 可升级，
            // 以及订阅 8s、规则资源 12s（= 上游 `CoreUpdateScheduler.STARTUP_DELAY_MS` 的原始理由）。
            // 它是唯一会**替换内核二进制**的后台腿，总开关 `autoUpdateCore` **缺省关**；
            // 落位只在代理未运行时发生（绝不主动断流），跨带只提示不自动更新。
            let core_update_scheduler =
                std::sync::Arc::new(runtime::core_update_scheduler::CoreUpdateScheduler::new());
            core_update_scheduler.start(app.handle().clone());
            app.manage(core_update_scheduler);

            // ── 自动轻量模式窗口驻留巡检（隐藏 / 最小化 10 分钟，30s 一拍）──
            // 计时**必须在主进程**：原实现挂在主窗 renderer 里，等于让那个正要被回收的 webview
            // 自己判断自己该不该被回收 —— 隐藏窗的 visibilityState 依平台、定时器又被 WKWebView
            // 节流，mac 上因此两条腿全断（根因见 `idle_lightweight` 头注）。
            // 无需 manage：它不持外部可见状态，句柄只在自己的 task 里。
            idle_lightweight::start(app.handle().clone());

            // ── 记忆窗口大小（#11：config.rememberWindowSize）──
            // **按配置 gate 的运行期插件注册**（`AppHandle::plugin`，tauri 2.11 支持）：开启才注册，
            // 关闭时**完全不注册** → 窗口尺寸行为逐字节保持现状（而不是注册后再想办法让它别生效）。
            // 必须在 `create_main_window` 之前：插件靠 `on_window_ready` 钩子恢复尺寸，窗口建完才注册就赶不上。
            // denylist 排掉全部非主窗（托盘自绘浮层 / 更新 mini 弹窗 / sing-box 面板外链窗）——它们的
            // 尺寸位置由各自逻辑精确控制（浮层要贴托盘图标、弹窗按四态高度自适应），被插件恢复会错位。
            if config_remember_window_size(raw_config.as_deref()) {
                use tauri_plugin_window_state::StateFlags;
                let plugin = tauri_plugin_window_state::Builder::new()
                    .with_state_flags(StateFlags::SIZE | StateFlags::POSITION)
                    .with_denylist(&[
                        tray::TRAY_LABEL,
                        runtime::update_popup::POPUP_LABEL,
                        // `commands::misc` 里的 DASHBOARD_WINDOW_LABEL 是私有常量，此处按字面同步；
                        // 改那边的 label 需一并改这里（两处都只此一份引用）。
                        "singbox-dashboard",
                    ])
                    .build();
                if let Err(e) = app.handle().plugin(plugin) {
                    log::warn!("window-state 插件注册失败，窗口尺寸不记忆（非致命）：{e}");
                }
            }

            // ── 建主窗（C15 start_hidden）──
            // 建窗全流程（per-platform 窗口铬 / vibrancy·Mica 特效 / 白屏自愈门 / 可见性·关窗事件接线）收在
            // `create_main_window` 一处——供 C16 轻量模式**销毁 webview 后重建**复用（重建与首建逐字节等价）。
            // start_hidden = `--hidden`（argv）或 `config.silentStart`（读原文本，与逃生门同源）：启动即隐藏、
            // 只驻托盘，靠托盘浮层明确入口/原生菜单/Dock 唤出；托盘缺失时 setup 末尾兜底显示（见下方分支）。
            let start_hidden = arg_hidden || config_silent_start(raw_config.as_deref());
            if start_hidden {
                log::info!(
                    "start_hidden 启动（--hidden 或 silentStart）：主窗建成隐藏，靠托盘浮层入口/Dock 唤出"
                );
            }
            create_main_window(app.handle(), start_hidden)?;

            // ── 启动期延迟任务（#9 自动连接 2s / 自动检查更新 5s + #17 内核基线提醒 6s）──
            // 挂在建窗**之后**（对齐 上游 whenReady 内的两个 setTimeout：窗口/服务先就位再连）。
            // 三条腿全 fire-and-forget，任何失败只记日志，绝不阻断启动。
            runtime::startup_tasks::spawn(app.handle().clone());

            // ── 应用菜单：唯一目的是一条**不依赖托盘**的退出快捷键 Cmd/Ctrl+Q ──
            // 托盘可能整体缺失（Linux 无 StatusNotifier host / appindicator 不可用）→ 必须保留非托盘退出路径，
            // 否则关窗 hide 后无处退出 = 僵尸进程。用官方 muda 菜单挂 `CmdOrCtrl+Q`（mac=⌘Q / win·linux=Ctrl+Q）
            // accelerator，事件先置 QuitState 再 app.exit(0)。
            //   · macOS：菜单栏在系统顶栏（无边框窗内不显），是预期形态；并补 Edit 子菜单，保住文本框
            //     ⌘Z/⌘X/⌘C/⌘V/⌘A（set_menu 替换了 Tauri 默认 mac 菜单，不补则复制粘贴丢失）。
            //   · win/linux：无边框自绘标题栏不要可见菜单栏 → 建好后 hide_menu；accelerator 仍生效
            //     （muda 隐藏只 hide 菜单栏 widget / SetMenu(null)，不移除 GTK accel_group / Win 子类 accel 表）。
            {
                use tauri::menu::{Menu, MenuItem, Submenu};
                let h = app.handle();
                // 应用菜单在 `setup` 里**只建一次**、不随语言重建（托盘菜单有 30s 汇流点，它没有）。
                // 改语言后这一项要下次启动才跟上 —— 与 `app_language.rs` 承诺的「改语言重启一次」
                // 同一档语义，不另立一条更强的承诺。
                let quit = MenuItem::with_id(
                    h,
                    "app_quit",
                    crate::i18n::t(crate::i18n::app_lang(h), crate::i18n::key::TRAY_QUIT),
                    true,
                    Some("CmdOrCtrl+Q"),
                )?;
                let app_menu = Submenu::with_items(h, "Polaris", true, &[&quit])?;
                #[cfg(target_os = "macos")]
                let menu = {
                    use tauri::menu::PredefinedMenuItem;
                    let edit = Submenu::with_items(
                        h,
                        "Edit",
                        true,
                        &[
                            &PredefinedMenuItem::undo(h, None)?,
                            &PredefinedMenuItem::redo(h, None)?,
                            &PredefinedMenuItem::separator(h)?,
                            &PredefinedMenuItem::cut(h, None)?,
                            &PredefinedMenuItem::copy(h, None)?,
                            &PredefinedMenuItem::paste(h, None)?,
                            &PredefinedMenuItem::select_all(h, None)?,
                        ],
                    )?;
                    Menu::with_items(h, &[&app_menu, &edit])?
                };
                #[cfg(not(target_os = "macos"))]
                let menu = Menu::with_items(h, &[&app_menu])?;
                h.set_menu(menu)?;
                // 无边框自绘标题栏不需要可见菜单栏；隐藏后 accelerator 仍触发（macOS 顶栏不隐、也无需隐）。
                #[cfg(not(target_os = "macos"))]
                let _ = h.hide_menu();
                h.on_menu_event(|app, event| {
                    if event.id.as_ref() == "app_quit" {
                        app.state::<QuitState>().0.store(true, Ordering::SeqCst);
                        app.exit(0);
                    }
                });
            }

            // ── 系统托盘（mac/win：左/右键统一切换自绘浮层；Linux：完整原生菜单）──
            // conf.trayIcon 已在 setup 前自动建好**单个**托盘（id "main" / 默认图标 tray-off-black.png=断开态空心星+单斜杠 /
            // iconAsTemplate:true=mac 断开态首帧即走系统自适应反色·Win/Linux 忽略 / tooltip）；
            // 此处取回它挂接点击行为与原生菜单，而非再 build 第二个——Tauri 每次 build 各向 OS 推一枚
            // 图标（tray/mod.rs push 非按 id 覆盖），双 build 会出现两枚。
            // `tray_present` 决定关窗语义：托盘在 → hide 收纳；托盘缺失 → 关窗即真退出（不留僵尸）。
            let handle = app.handle();
            let tray_present = if let Some(tray) = handle.tray_by_id("main") {
                use tauri::tray::TrayIconEvent;

                match tray_interaction_mode(Platform::current()) {
                    TrayInteractionMode::NativeMenu => {
                        // Linux AppIndicator 不派发可靠的左右键事件，完整原生菜单是唯一稳定功能面。
                        // 菜单树只由 reconcile_tray_menu 构建，状态/语言变化仍走统一汇流点，setup 不另造副本。
                        reconcile_tray_menu(handle);
                        tray.on_menu_event(|app, event| {
                            if let Some(action) = parse_menu_action(event.id.as_ref()) {
                                run_menu_action(app, action);
                            }
                        });
                    }
                    TrayInteractionMode::DirectClicks => {
                        // macOS/Windows 的左/右键必须只归自绘浮层所有。Tauri 没暴露「禁用右键菜单」开关，
                        // 但底层在 menu=None 时不会弹 NSMenu/HMENU，右键事件仍照常派发；同时关闭 mac 默认的
                        // 左键菜单行为。两步都做，避免未来配置误挂菜单后重新抢走事件。
                        if let Err(e) = tray.set_menu(None::<tauri::menu::Menu<tauri::Wry>>) {
                            log::warn!("移除非 Linux 托盘原生菜单失败（自绘浮层可能被抢占）：{e}");
                        }
                        if let Err(e) = tray.set_show_menu_on_left_click(false) {
                            log::warn!("关闭托盘左键原生菜单失败（自绘浮层点击可能被抢占）：{e}");
                        }
                    }
                }

                // macOS/Windows：左键、右键（mac 双指辅助点按同样归为 Right）抬起都
                // toggle 自绘浮层，不再把托盘图标点击解读为突然唤出主窗。主窗只由浮层里的明确入口唤出。
                // Linux 即使某个 host 偶发派发事件，判定也会拒绝，避免与原生菜单叠开。
                // `rect` 是图标真实屏幕矩形，用于浮层定位。
                //
                // 「拖动托盘图标 → 浮层跟隐藏」为何不在此接：`TrayIconEvent` 只有 Click/DoubleClick/Enter/
                // Move/Leave，**无专门的拖动事件**；Move/Leave 在**普通 hover**（鼠标从图标移到浮层）时也照
                // 触发，且不带按钮状态无法区分「拖动」与「划过」→ 拿来 hide 会误关浮层。故不接（避免又一个
                // 不可靠通道）。点窗外的隐藏由浮层 `Focused(false)` 覆盖（见 tray::build_overlay）；「mac 上
                // Cmd 拖动菜单栏图标时浮层是否失焦」本机（Linux）验不了 → 列入真机待验（见 review-queue）。
                tray.on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button,
                        button_state,
                        rect,
                        ..
                    } = event
                    {
                        if tray_click_toggles_overlay(
                            Platform::current(),
                            button,
                            button_state,
                        ) {
                            crate::tray::toggle_overlay(tray.app_handle(), Some(rect));
                        }
                    }
                });

                // ── 托盘图标 + Linux 原生菜单随状态刷新（图标四态见 set_tray_state）──
                // 全部驱动源收敛到 `reconcile_tray` 这一个叫醒入口（内部两个汇流点各自回读真值 + 幂等短路，
                // 不信事件携带的布尔）——见 `reconcile_tray_icon` / `wire_tray_icon_sync` 的文档：此前只订
                // STARTED/STOPPED 并传字面量，崩溃腿（只发 ERROR）与零 emit 腿（restart 失败 / updater 停核 /
                // 休眠唤醒）会把图标永久卡在实心，必须等用户下一次手动启停才回正。
                //
                // 三个入口：① setup 初始化一次（autostart 已起核则纠正为实心）；② 四条同步事件
                // （三条代理终态 + CONFIG_CHANGED，后者喂菜单的勾选与语言）；③ 30s 自愈轮询（兜未知缺口，
                // 对齐主窗 App.tsx:210-213 的同款网）。
                {
                    use tauri::Listener;
                    // macOS 菜单栏位置持久化（#313b）：给 NSStatusItem 钉 autosaveName。
                    // 必须在托盘已存在之后、且**只做一次** —— 放这里而不是 `reconcile_tray` 里面，
                    // 因为那两个汇流点挂着 30s 自愈轮询与四条事件，每次都重设是纯浪费；
                    // 而这个属性一旦设上就跟着 NSStatusItem 活到进程退出，没有被谁改回去的路径。
                    crate::tray::pin_tray_autosave_name(handle);
                    reconcile_tray(handle);
                    wire_tray_icon_sync(
                        // `listen_any` 捕获 `emit` 广播（不限 target），任何发射点都触发。
                        |ev| {
                            let h = handle.clone();
                            handle.listen_any(ev, move |_| reconcile_tray(&h));
                        },
                        |every| {
                            let h = handle.clone();
                            // 常驻自愈任务：随 app 生命周期存活（无退出信号——进程退出即随之消亡）。
                            tauri::async_runtime::spawn(async move {
                                loop {
                                    tokio::time::sleep(every).await;
                                    reconcile_tray(&h);
                                }
                            });
                        },
                    );
                }
                true
            } else {
                log::warn!(
                    "系统托盘未创建（conf.trayIcon 缺失 / StatusNotifier 或 appindicator 不可用）"
                );
                false
            };

            // 自绘浮层不在启动期预建：首次托盘点击由 `toggle_overlay` 按需创建，隐藏 2 分钟后自行回收。
            // Linux 的点击归原生菜单所有，不会创建这块 WebView；macOS/Windows 因而也不再为一个尚未
            // 用过的菜单常驻 renderer 内存。建窗失败只记日志，不曲解用户意图为显示主窗。

            // C15：start_hidden 但托盘缺失（Linux 无 StatusNotifier）→ 无唤出锚点，**必须**显示主窗，否则
            // 窗口永远隐藏且无处唤起 = 死界面。托盘在则保持隐藏（靠主激活/原生菜单/dock 唤出）。窗口可见性 → stats
            // 门控 + 关窗语义的接线已在 `create_main_window::on_window_event`（首建/重建同一处）。
            if start_hidden && !tray_present {
                log::info!("start_hidden 但托盘缺失 → 显示主窗（无隐藏唤出锚点，否则死界面）");
                // 走 `show_main_window` 而非直接 show：与托盘/dock 唤出同一条上屏时机判定（内容没就绪
                // 就等 `renderer:ready`），别在这条兜底腿上把空窗漏出去。窗此刻必存在，不会触发重建腿。
                show_main_window(app.handle());
            } else if start_hidden {
                // 有托盘唤出锚点时才进入真正的「只驻托盘」形态；否则上方兜底必须保留 Dock。
                set_macos_dock_visible(app.handle(), false);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // ── 配置类（config:get/save/getValue/setValue/updateMode + privacy）──
            config_get,
            config_save,
            config_classify_staged,
            config_update_mode,
            config_get_value,
            config_set_value,
            config_get_privacy_mode,
            config_set_privacy_mode,
            privacy_has_password,
            privacy_set_password,
            privacy_unlock,
            // ── 节点类（server:add/update/delete/deleteBatch/getAll/switch/generateUrl）──
            server_add,
            server_add_bulk,
            server_update,
            server_delete,
            server_delete_batch,
            server_get_all,
            server_switch,
            server_generate_url,
            // ── mesh 节点（warp + tailscale）──
            warp_register,
            warp_apply_license,
            tailscale_login,
            tailscale_login_cancel,
            tailscale_logout,
            tailscale_state_exists,
            tailscale_get_status,
            // ── Taildrop 收件箱（sing-box 1.14.0-beta.15）──────────────────────
            taildrop_list,
            taildrop_mark_read,
            taildrop_delete,
            taildrop_cancel,
            taildrop_save,
            // ── 代理控制（proxy:start/stop/restart/getStatus + pending + connections）──
            proxy_start,
            proxy_stop,
            proxy_restart,
            proxy_get_status,
            proxy_get_pending_changes,
            proxy_apply_pending_changes,
            kernel_probe_outbound,
            connections_close,
            connections_close_all,
            system_proxy_disable,
            system_proxy_get_status,
            // ── 订阅（subscription:add/update/delete/updateServers/preview + localImport）──
            subscription_add,
            subscription_update,
            subscription_delete,
            subscription_update_servers,
            subscription_preview,
            local_import_parse,
            local_import_pick_file,
            // ── 路由规则（rules:getAll/add/update/delete/reorder）──
            rules_get_all,
            rules_add,
            rules_update,
            rules_delete,
            rules_reorder,
            // ── 应用分流预设（内置表 Rust SoT 下发）──
            app_presets_list,
            // ── 自定义应用图标缓存（设定即下载到 userData，渲染零出站）──
            cache_app_icon,
            // ── 规则资源（ruleResources:*）──
            rule_resources_list,
            rule_resources_download,
            rule_resources_redownload,
            rule_resources_cancel,
            rule_resources_delete,
            rule_resources_get_catalog,
            rule_resources_refresh_catalog,
            rule_resources_get_cached_catalog,
            rule_resources_set_auto_update,
            rule_resources_update_all,
            rule_resources_reset_builtin,
            rule_resources_update_builtin,
            rule_resources_icon_galleries,
            rule_resources_refresh_icon_galleries,
            // ── stats 订阅（stats:subscribe/unsubscribe）──
            stats_subscribe,
            stats_unsubscribe,
            stats_project_topology,
            stats_closed_clear,
            // ── 系统能力（system:listProcesses）──
            system_list_processes,
            // ── helper（helper:getStatus/install/uninstall）──
            helper_get_status,
            helper_install,
            helper_uninstall,
            // ── 解锁检测（unlock:run/get）──
            unlock_run,
            unlock_get,
            // ── 测速（server:speedTest）──
            server_speed_test,
            // ── 更新（version + app update + core update）──
            version_get_info,
            update_check,
            update_download,
            update_install,
            update_skip,
            update_open_releases,
            update_popup_state,
            update_popup_action,
            update_popup_show,
            core_update_check,
            core_update_run,
            core_get_version_info,
            core_rollback,
            core_replace_manual,
            core_update_get_auto_status,
            core_update_apply_staged,
            core_update_ack_version_change,
            core_reset_factory,
            app_uninstall_all,
            // ── 窗口控制（window:* + app + renderer/fatal）──
            window_minimize,
            window_maximize_toggle,
            window_close,
            window_is_maximized,
            app_restart,
            app_startup_config_flags,
            app_take_clean_exit_flag,
            // ── 托盘自绘浮层（独立窗口生命周期 + 显示主窗 + 退出）──
            tray::tray_renderer_ready,
            tray::tray_resize,
            tray::tray_hide,
            tray::tray_show_main,
            tray::tray_take_pending_screen,
            tray::tray_quit,
            tray::tray_enter_lightweight,
            tray::tray_check_update,
            renderer_ready,
            fatal_retry,
            // ── 杂项（logs/shell/singbox-dashboard/backup/diagnostic/autostart/ipinfo）──
            logs_get,
            logs_search,
            logs_unsubscribe,
            logs_clear,
            logs_runtime_level,
            logs_diagnostic_state,
            logs_set_diagnostic,
            logs_export,
            logs_open_dir,
            logs_legacy_info,
            logs_archive_legacy,
            shell_open_external,
            open_singbox_dashboard,
            refresh_singbox_dashboard,
            get_singbox_dashboard_connection,
            backup_export,
            backup_import_pick,
            backup_import_apply,
            backup_get_info,
            diagnostic_export,
            auto_start_set,
            auto_start_get_status,
            ipinfo_get,
            // ── 主窗白屏自愈（mount 健康门 / 终局页重试 / renderer 日志转发）──
            // 注：renderer_ready / fatal_retry 已在上方「窗口控制」段注册，此处不重复列。
            renderer_log
        ])
        .build(ctx)
        .expect("error while building Polaris")
        // RunEvent 循环：① macOS dock 图标重开 ② C1 退出清理（停核 + 清系统代理）。关窗语义仍由
        // on_window_event + QuitState 决定，未改动；本回调只在**进程级退出请求**时兜安全清理。
        .run(|app_handle, event| match event {
            // macOS：点 dock 图标（NSApplicationDelegate applicationShouldHandleReopen）→ RunEvent::Reopen。
            // 主窗关闭进入轻量驻留后，Dock 重开是 macOS 上召回/重建窗口的路径；Windows 靠任务栏
            // 或托盘浮层的明确入口，Linux 靠原生菜单「显示」。show_main_window 会按存在性选择呈现或重建。
            // Reopen 是 **macOS-only** 的 RunEvent 变体 → cfg 门控该 arm；Linux/Windows 上 cargo check 覆盖
            // 不到它（需 mac 编译验证）。
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => show_main_window(app_handle),
            // C1：任何退出请求（托盘/菜单「退出」→ app.exit、末窗关闭时托盘缺失 → exit、OS 关机/logout）
            // → 阻塞清理。不 `prevent_exit`（清完照常退出）。安全关键：见 `run_exit_cleanup` 文档。
            //
            // C16 守卫：轻量驻留中有意销毁**末窗**（主 WebView，或主窗已销毁后的空闲托盘 WebView）若触发
            // spurious ExitRequested，则必须保核——轻量语义恒不退出、代理连接不中断（对齐 上游）。判据：
            // LightweightState 由销毁方前置真（swap 消费）且非显式退出（`!QuitState`）且托盘在（有唤出锚点）
            // → `prevent_exit` + **跳过停核清理**。陈旧置位不阻断真实退出：真退出置 QuitState → 落到清理。
            tauri::RunEvent::ExitRequested { api, .. } => {
                let lightweight = app_handle
                    .state::<LightweightState>()
                    .0
                    .swap(false, Ordering::SeqCst);
                let quitting = app_handle.state::<QuitState>().0.load(Ordering::SeqCst);
                if lightweight && !quitting && app_handle.tray_by_id("main").is_some() {
                    api.prevent_exit();
                    return;
                }
                // Q1-b ④：落「上次是正常退出」标记，供下次启动的渲染端据以清 staged（见 `clean_exit`）。
                // **必须在 C16 守卫之后**：被 `prevent_exit` 的那条腿进程根本没退（轻量模式销毁主窗
                // 而已），在那儿落标记会让重建出来的 webview 把自己的编辑当「上次退出过」清掉。
                // **必须在 `run_exit_cleanup` 之前**：那里面是阻塞停核，卡住 / panic 都会让标记落不下去。
                mark_clean_exit(app_handle);
                run_exit_cleanup(app_handle);
            }
            _ => {}
        });
}

/// Q1-b ④：正常退出腿落标记（目录 = `<userData>/`，与 `system-proxy.marker.json` 同处）。
///
/// 目录取自 `AppRuntime`（唯一持有 config dir 的地方）。运行时未装配 = 极早期退出，
/// 那时任何 webview 都还没起、不可能有 staged ⇒ 无标记可落，直接跳过。
///
/// `RestartState` 置位（`app:restart` 发起）⇒ **不落标记**：那不是「用户结束了这次使用」，
/// 而是用户几秒内就回来的一次重启，清掉 staged = 吃掉用户的工作。判定腿在
/// [`clean_exit::mark_unless_restarting`]（有单测），本函数只负责把那个 bit 取出来喂给它。
/// `try_state` 而非 `state`：极早期退出时 `RestartState` 还没 manage，`state` 会 panic 在退出路径上。
fn mark_clean_exit(app: &tauri::AppHandle) {
    let restarting = app
        .try_state::<RestartState>()
        .is_some_and(|s| s.0.swap(false, Ordering::SeqCst));
    if let Some(rt) = app.try_state::<AppRuntime>() {
        clean_exit::mark_unless_restarting(rt.config.dir(), restarting);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn maximize_observation_only_commits_real_transitions() {
        let state = AtomicBool::new(false);
        assert!(!commit_maximized_observation(&state, false));
        assert!(commit_maximized_observation(&state, true));
        assert!(!commit_maximized_observation(&state, true));
        assert!(commit_maximized_observation(&state, false));
    }

    /// 双击拖动层 / 系统菜单最大化不经过 `window_maximize_toggle`，必须由原生 resize 事件回读并广播。
    #[test]
    fn main_window_native_maximize_is_bridged_to_renderer() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            include_str!("main.rs"),
            "fn create_main_window(",
        );
        for required in [
            "tauri::WindowEvent::Resized(_)",
            "event_window.is_maximized()",
            "commit_maximized_observation(&maximized_state, maximized)",
            "emit_window_maximize_changed(&app_handle, maximized)",
        ] {
            assert!(
                body.contains(required),
                "主窗原生最大化同步链缺少 `{required}`"
            );
        }
    }

    /// W13：托盘图标黑/白变体的探测链（Win/Linux）。前两格是纯函数语义；后两格是源扫描守卫
    /// （探测窗必须先于主窗被探、且 setup 必须真建它）——顺序翻回去 = 显式 uiTheme 下的
    /// 读应用外观失真（复审 Med-1）复活，不建它 = 探测链退回旧缺陷。
    /// W13：托盘图标黑/白变体的探测链（Win/Linux）。前三格是纯函数语义；后两格是守卫——
    /// 注册表真值必须先于（被钉的）主窗被读；Windows 上注册表读法必须真的给出答案（CI win 腿上跑）。
    #[cfg(not(target_os = "macos"))]
    mod tray_dark_bg_probe {
        use super::dark_bg_from_probe;

        #[test]
        fn primary_wins_when_present() {
            assert!(dark_bg_from_probe(Some(true), Some(false)));
            assert!(!dark_bg_from_probe(Some(false), Some(true)));
        }

        /// W13 的核心格：主信号（注册表→主窗）取不到时 fallback（浮层窗）接管——
        /// 旧实现这格恒白，正是浅色任务栏图标隐身的真机缺陷本体。
        #[test]
        fn fallback_takes_over_when_primary_is_gone() {
            assert!(!dark_bg_from_probe(None, Some(false)));
            assert!(dark_bg_from_probe(None, Some(true)));
        }

        #[test]
        fn all_missing_falls_back_to_dark_assumption() {
            assert!(dark_bg_from_probe(None, None));
        }

        #[test]
        fn registry_truth_is_probed_before_the_pinned_main_window() {
            let src = include_str!("main.rs");
            let body = crate::commands::guard_scan::top_level_fn_body(src, "fn set_tray_state(");
            assert!(
                !body.is_empty(),
                "set_tray_state 函数体取不到——判据失效需同步更新"
            );
            let reg = body
                .find("system_dark_bg()")
                .expect("set_tray_state 不再读注册表真值（W13 回潮）");
            let main = body
                .find("get_webview_window(\"main\")")
                .expect("set_tray_state 的主窗探测形态变了");
            assert!(
                reg < main,
                "注册表真值又排到了主窗之后：显式 uiTheme 下主窗被钉、读到应用外观而非任务栏明暗"
            );
        }

        /// Windows CI 腿上跑：Personalize 键自 Win10 1809 起恒在，读不出 Some 说明读法坏了。
        #[cfg(target_os = "windows")]
        #[test]
        fn registry_probe_answers_on_real_windows() {
            assert!(super::super::system_dark_bg().is_some());
        }
    }

    fn argv(rest: &[&str]) -> Vec<String> {
        // 首元素恒是程序名（std::env::args 的 argv[0]），判定须跳过它——测试连同 argv[0] 一起喂。
        std::iter::once("polaris")
            .chain(rest.iter().copied())
            .map(String::from)
            .collect()
    }

    /// **结构门，不是行为门** —— 它证的是「写 `AppleLanguages` 的那一句排在建 `NSApplication`
    /// 的那一句之前」，证不了「macOS 上原生对话框真的换了语言」。后者要一台 mac
    /// （本仓 CI / 开发机是 Linux，AppKit 根本不存在），已列真机判据。
    ///
    /// # 为什么这条顺序值一个门
    ///
    /// `app_language::apply_process_language` 的效果**只在下次启动兑现**，所以挪错位置**当场
    /// 什么都不会变**：不 panic、不报错、单测全绿、Linux/Windows 完全无感，连 macOS 用户也只是
    /// 「改完语言重启一次没生效，重启第二次才生效」。这种缺陷不会有人报 bug，只会被当成「这软件
    /// 就这样」。因果链见 `app_language` 模块文档的那张表：Tauri 2 的 `setup` 由
    /// `Builder::build()` 在建完 runtime（= tao 建 `NSApplication`）之后才调
    /// （`tauri-2.11.5/src/app.rs:2344` 建、`:2531` 才 setup），故写在 `.build(ctx)` 之后 =
    /// AppKit 本次已经读过旧值 = 用户要重启两次。
    ///
    /// **变异锁**：把 `apply_process_language(...)` 挪到 `.build(ctx)` 之后 ⇒ 顺序断言转红；
    /// 整句删掉 ⇒ 锚点消失、`expect` 转红；把 `.build(ctx)` 改回内联 `generate_context!()`
    /// ⇒ 锚点消失转红（那意味着 identifier 又拿不到了）。
    ///
    /// 三平台同源：非 macOS 上 `apply_process_language` 是空函数，但调用点照样在，
    /// 故本门在 Linux/Windows 的 CI 上一样有判据 —— 不会出现「只有 mac 跑得到的门」。
    #[test]
    fn native_dialog_language_is_applied_before_appkit_boots() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(include_str!("main.rs"), "fn main() {");
        let apply = body.find("app_language::apply_process_language(").expect(
            "锚点消失：原生对话框语言对账的调用点没了 —— macOS 上原生对话框会退回跟随系统语言",
        );
        let build = body
            .find(".build(ctx)")
            .expect("锚点消失：`.build(ctx)` —— 守卫已失去「AppKit 何时起来」的判据");
        assert!(
            apply < build,
            "`apply_process_language` 排到了 `.build(ctx)` 之后 —— Tauri 在 build 里就建好了 \
             NSApplication，AppKit 本次已按旧值解析完本地化，用户改语言后要重启**两次**才生效。"
        );
    }

    /// Q1-b ④：正常退出标记在 `ExitRequested` 里的**落点**必须夹在两个锚之间。行为断言够不着
    /// （要一个跑起来的 Tauri 事件循环），而挪错任何一边都是静默的正确性缺陷：
    ///
    /// - 挪到 C16 `prevent_exit` 早退**之前** ⇒ 轻量模式销毁主窗（**进程没退**）也落标记 ⇒
    ///   用户唤出、webview 重建后，暂存的编辑被当成「上次退出过」清掉。这正是 NFR-1 禁止的
    ///   「App 自己吃掉用户的工作」，且由 idle 计时器驱动，会反复发生。
    /// - 挪到 `run_exit_cleanup` **之后** ⇒ 那里面是**阻塞**停核（`block_on(proxy.stop())`），
    ///   卡住 / panic 都会让标记落不下去 ⇒ 每次正常退出都被下次启动当成强杀 ⇒ ④ 整条腿失效。
    ///
    /// **变异锁**：把 `mark_clean_exit(app_handle);` 挪到 `api.prevent_exit();` 之前或
    /// `run_exit_cleanup(app_handle);` 之后 ⇒ 转红；整句删掉 ⇒ 锚点消失、转红。
    #[test]
    fn clean_exit_marker_is_written_only_on_the_real_exit_leg() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(include_str!("main.rs"), "fn main() {");
        let prevent = body
            .find("api.prevent_exit();")
            .expect("锚点消失：C16 轻量模式的 prevent_exit 早退，守卫已失去判据");
        let mark = body
            .find("mark_clean_exit(app_handle);")
            .expect("锚点消失：正常退出标记的落点，Q1-b ④ 已无人守");
        let cleanup = body
            .find("run_exit_cleanup(app_handle);")
            .expect("锚点消失：退出清理调用点，守卫已失去判据");
        assert!(
            prevent < mark,
            "正常退出标记落在了 C16 `prevent_exit` 早退之前 —— 轻量模式销毁 webview（进程没退）\
             也会落标记，重建后用户的暂存编辑会被当成「上次退出过」清掉（NFR-1）。"
        );
        assert!(
            mark < cleanup,
            "正常退出标记落在了 `run_exit_cleanup` 之后 —— 那里面是阻塞停核，卡住/panic 就落不下标记，\
             每次正常退出都会被下次启动当成强杀，Q1-b ④ 整条腿失效。"
        );
    }

    /// Q1-b ④：`app:restart` 这条腿**不落**正常退出标记 —— 的**接线**那一半。
    ///
    /// # 本条证不了行为，写清楚它证的是什么
    ///
    /// 判定腿（「bit 为真就不落、为假照落」）是真行为，由 `clean_exit` 的
    /// `restart_leg_does_not_leave_a_marker` / `real_exit_leg_still_leaves_a_marker` 两条**真跑 FS**
    /// 的单测钉住。本条只钉**接线**：那个 bit 确实由 `app_restart` 置上、且确实被退出腿取出来喂进去。
    /// 端到端（真跑 Tauri 事件循环，点重启 → 看标记文件没出现）本环境构造不出，已列真机项。
    ///
    /// 没有本条，接线断了两侧都不会红：`app_restart` 不置位 ⇒ 判定腿恒收到 `false`（照落标记 =
    /// 缺陷本体），而它自己的单测传的是自己造的 bit，全绿。
    ///
    /// **变异锁**：删掉 `app_restart` 里的 `RestartState` 置位 ⇒ 转红；把它挪到 `request_restart()`
    /// 之后（永远执行不到）⇒ 顺序断言转红；`mark_clean_exit` 改回直接调 `clean_exit::mark` ⇒ 转红。
    #[test]
    fn restart_leg_is_wired_to_skip_the_clean_exit_marker() {
        use crate::commands::guard_scan::top_level_fn_body;

        let restart = top_level_fn_body(
            include_str!("commands/window.rs"),
            "pub fn app_restart(app: AppHandle) -> ApiResponse<()> {",
        );
        let set = restart.find("RestartState").expect(
            "`app_restart` 不再置 `RestartState` —— 重启会被当成「用户结束了这次使用」，\
                     回来后暂存的编辑被清掉（NFR-1）",
        );
        let restart_call = restart
            .find("app.request_restart();")
            .expect("锚点消失：`request_restart()` 调用点，守卫已失去判据");
        assert!(
            set < restart_call,
            "`RestartState` 置位落在 `request_restart()` 之后 —— 那行永远执行不到，等于没置。"
        );

        let exit_leg = top_level_fn_body(
            include_str!("main.rs"),
            "fn mark_clean_exit(app: &tauri::AppHandle) {",
        );
        assert!(
            exit_leg.contains("RestartState"),
            "退出腿不再读 `RestartState` —— `app:restart` 会照落标记，重启回来暂存被清。"
        );
        assert!(
            exit_leg.contains("mark_unless_restarting"),
            "退出腿绕开了 `mark_unless_restarting`（那条 `if restarting` 是 Q1-b ④ 语义的全部实现，\
             也是唯一有单测的那一处）—— 直接调 `clean_exit::mark` 就把重启腿的豁免整个丢了。"
        );
    }

    #[test]
    fn version_and_help_win_over_everything() {
        // CLI 查询优先级最高：即便无显示 / 带 --hidden 也先返 Version/Help（三平台通用）。
        assert_eq!(
            resolve_startup(&argv(&["--version", "--hidden"]), false),
            StartupAction::Version
        );
        assert_eq!(
            resolve_startup(&argv(&["-V"]), true),
            StartupAction::Version
        );
        assert_eq!(
            resolve_startup(&argv(&["--help"]), false),
            StartupAction::Help
        );
        assert_eq!(resolve_startup(&argv(&["-h"]), true), StartupAction::Help);
    }

    #[test]
    fn version_precedes_help_when_both_present() {
        // -V 在 -h 之前判定（上游 同序）。
        assert_eq!(
            resolve_startup(&argv(&["-h", "-V"]), true),
            StartupAction::Version
        );
    }

    #[test]
    fn headless_exit_only_when_no_display_and_no_cli_query() {
        // 无显示 + 非 CLI 查询 → HeadlessExit（规避无 GUI 崩溃）。
        assert_eq!(
            resolve_startup(&argv(&[]), false),
            StartupAction::HeadlessExit
        );
        // 有显示 → 正常起 GUI，不 headless。
        assert_eq!(
            resolve_startup(&argv(&[]), true),
            StartupAction::Run { hidden: false }
        );
    }

    #[test]
    fn hidden_flag_only_affects_run_variant() {
        assert_eq!(
            resolve_startup(&argv(&["--hidden"]), true),
            StartupAction::Run { hidden: true }
        );
        // --hidden 但无显示 → 仍 headless 早退（headless 先于 hidden）。
        assert_eq!(
            resolve_startup(&argv(&["--hidden"]), false),
            StartupAction::HeadlessExit
        );
        // 无 --hidden → hidden:false。
        assert_eq!(
            resolve_startup(&argv(&["--autostart"]), true),
            StartupAction::Run { hidden: false }
        );
    }

    #[test]
    fn silent_start_parsed_from_raw_config() {
        assert!(config_silent_start(Some(r#"{"silentStart":true}"#)));
        assert!(!config_silent_start(Some(r#"{"silentStart":false}"#)));
        // 缺字段 / 非 bool / 坏 JSON / None → 默认 false（显示）。
        assert!(!config_silent_start(Some(r#"{"autoStart":true}"#)));
        assert!(!config_silent_start(Some(r#"{"silentStart":"yes"}"#)));
        assert!(!config_silent_start(Some("not json")));
        assert!(!config_silent_start(None));
    }

    #[test]
    fn remember_window_size_defaults_to_true() {
        // 正向语义 + 缺省 true（对齐 UI `config.rememberWindowSize !== false`）。
        assert!(config_remember_window_size(Some(
            r#"{"rememberWindowSize":true}"#
        )));
        assert!(!config_remember_window_size(Some(
            r#"{"rememberWindowSize":false}"#
        )));
        // 缺字段 / 非 bool / 坏 JSON / None → true（开）。
        assert!(config_remember_window_size(Some(r#"{"silentStart":true}"#)));
        assert!(config_remember_window_size(Some(
            r#"{"rememberWindowSize":"yes"}"#
        )));
        assert!(config_remember_window_size(Some("not json")));
        assert!(config_remember_window_size(None));
    }

    #[test]
    fn close_action_allows_close_while_quitting() {
        // 显式退出进行中 → 恒放行，托盘 / minimizeToTray 组合一概不影响（否则退不掉）。
        for tray in [true, false] {
            for m2t in [true, false] {
                assert_eq!(
                    resolve_close_action(true, tray, m2t),
                    CloseAction::AllowClose,
                    "quitting=true, tray={tray}, minimizeToTray={m2t}"
                );
            }
        }
    }

    #[test]
    fn close_action_enters_lightweight_only_when_wanted_and_tray_present() {
        // 用户选「收进托盘」+ 托盘在 → 唯一销毁 renderer、保核驻托盘的组合。
        assert_eq!(
            resolve_close_action(false, true, true),
            CloseAction::EnterLightweight
        );
        // 想收纳但托盘缺失 → 销毁即僵尸，改真退出。
        assert_eq!(
            resolve_close_action(false, false, true),
            CloseAction::QuitApp
        );
    }

    #[test]
    fn close_action_quits_when_user_chose_exit_app() {
        // 用户选「退出应用」→ 托盘在不在都退（#10 之前这里恒 hide，开关是死装饰）。
        assert_eq!(
            resolve_close_action(false, true, false),
            CloseAction::QuitApp
        );
        assert_eq!(
            resolve_close_action(false, false, false),
            CloseAction::QuitApp
        );
    }

    #[test]
    fn cli_help_text_lists_hidden_flag() {
        let text = cli_help_text();
        assert!(text.contains("--hidden"));
        assert!(text.contains("--version"));
        assert!(text.starts_with("Polaris "));
    }

    // ── 托盘图标汇流点（P1「中断后不回落」）──────────────────────────────────────
    //
    // 图标本身只能真机看；能自动断言的是**装配决策**：哪些源被接上汇流点。此前这条腿零测试，
    // 于是「崩溃腿没接」这种缺失从未被任何门发现。下面三测穷举本 bug 的逃逸面：
    //   ① 某条终态事件没订（本 bug 的原形：ERROR 缺失）
    //   ② 事件订阅退化成只订一条 / 只订部分（补一个 ERROR 监听就收工的半修）
    //   ③ 轮询自愈网没挂，或挂了但周期长到形同没有（零 emit 腿仍无人兜）

    /// 收集一次装配实际接上的驱动源。
    fn wired_sources() -> (Vec<&'static str>, Vec<std::time::Duration>) {
        let mut events = Vec::new();
        let mut polls = Vec::new();
        wire_tray_icon_sync(|ev| events.push(ev), |d| polls.push(d));
        (events, polls)
    }

    #[test]
    fn tray_icon_subscribes_every_terminal_event() {
        let (events, _) = wired_sources();
        // ERROR 是本 bug 的原形：`set_error()` 把 running=false 落盘却只发它，图标此前收不到。
        for want in [
            crate::events::channel::EVENT_PROXY_STARTED,
            crate::events::channel::EVENT_PROXY_STOPPED,
            crate::events::channel::EVENT_PROXY_ERROR,
        ] {
            assert!(
                events.contains(&want),
                "终态事件 {want} 未接入托盘图标汇流点 → 该腿触发时图标会卡住"
            );
        }
        // 无重复订阅（同一事件订两次 = 每次终态刷两遍图标）。
        let mut uniq = events.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), events.len(), "存在重复订阅：{events:?}");
    }

    #[test]
    fn tray_icon_has_polling_self_heal_net() {
        let (_, polls) = wired_sources();
        assert_eq!(
            polls.len(),
            1,
            "自愈轮询网必须且只须挂一条；缺它则 restart 失败 / updater 停核 / 休眠唤醒这些\
             **零 emit** 的腿无人兜，图标照样卡死（只补 ERROR 监听是半修）"
        );
        let every = polls[0];
        assert!(!every.is_zero(), "周期 0 = 忙循环，不是自愈网");
        assert!(
            every <= std::time::Duration::from_secs(30),
            "自愈周期不得慢于主窗 App.tsx:210-213 的 30s 网（{every:?}），否则托盘比主窗还晚回正"
        );
    }

    /// 🟡 **图标汇流点必须保持无 IO**（2026-07-28 复审 LOW-2 的登记闸）。
    ///
    /// `reconcile_tray_icon` 被四个事件源 + 30s 轮询随便叫醒，其可以这样被随便叫醒的**前提**就是
    /// 「一次 `RwLock` 读快照，无 IO / 无 syscall」（见该函数文档末段）。往里塞任何现查
    /// —— 最可能的诱因是给图标补 systemProxy 降级态，那个位只能靠 exec `networksetup`/`gsettings`/
    /// `reg` 现查 —— 就等于每次代理状态变化、每次 configChanged 都拖一次子进程。
    ///
    /// 为什么原生图标**刻意不做** degraded 第五态、以及要做的前置条件是什么，成文记在
    /// `tray.rs::TrayState` 上方那段决策登记里。本条是它的牙。
    ///
    /// **变异探针**：在 `reconcile_tray_icon` 里加 `system_proxy` 现查 / `spawn_blocking` /
    /// `std::process::Command` ⇒ 逐条转红。
    /// 🟡 **托盘两个汇流点读配置必须走投影**（与 [`tray_icon_reconcile_stays_io_free`] 同源的理由：
    /// 它们被 4 个事件源 + **30s 自愈轮询**随便叫醒）。
    ///
    /// `reconcile_tray_menu` 要 `proxyMode` / `proxyModeType`，`app_lang` 要 `language` —— 都是取一两个
    /// 字符串。用 `config.current()` 则每次叫醒都为此深拷贝整份配置（含 200 节点级 `servers`），而
    /// 「随便叫醒」正是这两个汇流点的设计前提。且 `set_tray_state` 与 `reconcile_tray_menu` 各调一次
    /// `app_lang`，一轮轮询实际是**三次**整份拷贝。
    ///
    /// 与 `periodic_legs_read_config_by_projection_not_full_clone`（`runtime/proxy.rs`）同类：纯性能
    /// 不变式，行为断言看不出来，只能源码型锁。
    ///
    /// ⚠️ 顺带锁住**嵌套禁忌**：`app_lang(app)` 自己要读配置，故它绝不能出现在
    /// `reconcile_tray_menu` 的 `with_current` 闭包里 —— 闭包内持着 `ConfigManager` 的读锁，递归读在
    /// 有写者排队时会永久阻塞（见 `ConfigManager::with_current` 文档）。debug 构型有重入探针兜底，
    /// 但那要真跑到才炸；这里在源码层面就要求两次读是**平铺**的。
    ///
    /// **变异锁**：任一函数体里换回 `.current()` ⇒ 转红；把 `app_lang(app)` 挪进 `with_current`
    /// 闭包 ⇒ 「平铺」断言转红。
    #[test]
    fn tray_reconcile_reads_config_by_projection_not_full_clone() {
        use crate::commands::guard_scan::top_level_fn_body;
        for (src, head, who) in [
            (
                include_str!("main.rs"),
                "fn reconcile_tray_menu(app: &tauri::AppHandle) {",
                "菜单汇流点",
            ),
            (
                include_str!("i18n.rs"),
                "pub fn app_lang(app: &AppHandle) -> Lang {",
                "原生文案语言",
            ),
        ] {
            let body = top_level_fn_body(src, head);
            assert!(
                !body.contains(".current()"),
                "{who}（`{head}`）出现了 `config.current()` —— 它挂在 30s 自愈轮询上，\
                 每次叫醒都会整份深拷贝配置。改用 `with_current(|c| …)` 只投影要用的字段。"
            );
            assert!(
                body.contains(".with_current("),
                "{who} 里连 `with_current` 都没有了 —— 负面断言会因此恒真（门被抽空）"
            );
        }

        // 嵌套禁忌：`app_lang(app)` 必须在 `with_current` 闭包**之后**（平铺），不得被包进闭包里。
        let menu = top_level_fn_body(
            include_str!("main.rs"),
            "fn reconcile_tray_menu(app: &tauri::AppHandle) {",
        );
        let close = menu
            .find("            .ok()")
            .expect("锚点消失：`with_current` 投影段的收尾，守卫已失去判据");
        let lang = menu
            .find("app_lang(app)")
            .expect("锚点消失：菜单语言读取，守卫已失去判据");
        assert!(
            lang > close,
            "`app_lang(app)` 落进了 `with_current` 闭包内 —— 闭包里持着 ConfigManager 的读锁，\
             而 app_lang 自己还要再读一次配置：递归读在有写者排队时永久阻塞。两次读必须平铺。"
        );
    }

    /// 主窗尺寸只能有一个真值源：`tauri.conf.json`。
    ///
    /// 建窗走 `WebviewWindowBuilder::from_config`（conf 的 `create:false`），conf 里的
    /// width/height/minWidth/minHeight 由它套上。mac 分支曾在其后另写一份 `inner_size` +
    /// `min_inner_size`，把 conf 那四个值在 mac 上变成死值 —— 改 conf 不生效、且**没有任何门会红**
    /// （2026-07-29 真机才发现最小尺寸不是 conf 写的值）。这条锁住「建主窗的代码里不得再出现尺寸设置」。
    ///
    /// 射程刻意收在 `create_main_window`：Dashboard 窗（`commands/misc.rs`）是独立窗、有自己的尺寸，
    /// 不在此列。
    #[test]
    fn main_window_size_comes_only_from_conf() {
        let src = include_str!("main.rs");
        let body = crate::commands::guard_scan::top_level_fn_body(src, "fn create_main_window(");
        assert!(
            !body.is_empty(),
            "抓不到 create_main_window 的函数体 —— 判据面塌了（改名了？），本门会恒绿"
        );
        for forbidden in ["inner_size(", "min_inner_size("] {
            assert!(
                !body.contains(forbidden),
                "建主窗的代码里出现 `{forbidden}` —— 它会覆盖 from_config 套上的 conf 尺寸，\
                 使 tauri.conf.json 的 width/height/minWidth/minHeight 变成死值。\
                 尺寸改动请改 conf，不要在这里再设一份。"
            );
        }
    }

    /// 首建天然发生在 setup 主线程，轻量态重建则可能由托盘 WebView IPC 线程触发。两条路径必须在
    /// `show_main_window` 入口合流到主线程，否则 macOS `apply_vibrancy` 会拒绝重建窗，透明侧栏背后
    /// 没有原生材质，表现为整个左侧导航直接透出桌面。
    #[test]
    fn main_window_rebuild_is_dispatched_to_main_thread() {
        let src = include_str!("main.rs");
        let entry = crate::commands::guard_scan::top_level_fn_body(src, "fn show_main_window(");
        let on_main = crate::commands::guard_scan::top_level_fn_body(
            src,
            "fn show_main_window_on_main_thread(",
        );

        assert!(
            entry.contains("run_on_main_thread"),
            "主窗唤出入口必须先投主线程；托盘 IPC 线程不得直接重建原生窗口"
        );
        // W18 第二层：主线程帧内调用（WM_COPYDATA WndProc / IPC 分发栈）时 run_on_main_thread
        // 是内联直执——必须先跳线程脱离分发帧再排回，否则重建把同步对端卡死在 SendMessageW 上。
        let spawn_at = entry
            .find("async_runtime::spawn")
            .expect("主窗唤出必须先跳 async 线程脱离消息分发帧");
        let queue_at = entry
            .find("run_on_main_thread")
            .expect("跳线程后必须排回主线程");
        assert!(
            spawn_at < queue_at,
            "次序必须是 spawn 脱帧 → run_on_main_thread 排回 → 重建/呈现"
        );
        let probe_at = entry
            .find("window_health::begin_show_probe")
            .expect("帧外调度前必须先登记唤出起点，否则真机时延漏掉排队段");
        assert!(
            probe_at < spawn_at,
            "主窗时延起点必须早于 spawn，不能把消息帧逃逸/线程排队从数据里剪掉"
        );
        assert!(
            entry.contains("show_main_window_on_main_thread("),
            "主线程闭包必须调用唯一的建窗/呈现实现"
        );
        assert!(
            !entry.contains("create_main_window("),
            "跨线程入口不得绕过主线程边界直接建窗"
        );
        assert!(
            on_main.contains("create_main_window(app, false)"),
            "轻量态重建必须留在主线程实现内"
        );
        let create = crate::commands::guard_scan::top_level_fn_body(src, "fn create_main_window(");
        assert!(
            create.contains("rt.stats().mark_main_window_created()"),
            "builder 成功后必须提交主窗口生命周期，供三平台 stats/logs 可见性门共享"
        );
        assert!(
            create.contains("window_health::log_show_probe(app, \"window-built\", false)"),
            "builder 成功点必须记录 window-built，B9 才能区分原生建窗与 renderer 加载耗时"
        );
        let present =
            crate::commands::guard_scan::top_level_fn_body(src, "fn present_main_window(");
        assert!(
            present.contains("window_health::log_show_probe(")
                && present.contains("\"shown\"")
                && present.contains("\"show-failed\""),
            "唯一呈现漏斗必须消费 shown/show-failed 终态探针"
        );
    }

    /// 四个托盘态必须是四张**互不相同**的图，且黑白变体成对。
    ///
    /// 图标是这四个态在菜单栏里唯一的出口 —— 撞图＝该态对用户彻底不可见，而这种撞不会让任何门变红：
    /// PNG 是二进制素材、`include_image!` 只管文件在不在。2026-07-29 断开态加斜杠后 off 与 error
    /// 的构图只差「一道 vs 两道」，正是最容易在下次改图时被复制粘贴弄成同一张的形态。
    ///
    /// 只比字节（不解 PNG）：能抓住的是「同一张图挂了两个态」这个真实回归形态；
    /// 「两张图渲染出来很像」需要像素级判据，那属于设计评审，不在这条门的射程内。
    #[test]
    fn tray_state_icons_are_all_distinct() {
        let icons: [(&str, &[u8]); 4] = [
            ("on", include_bytes!("../icons/tray-on-black.png")),
            (
                "connecting",
                include_bytes!("../icons/tray-connecting-black.png"),
            ),
            ("off", include_bytes!("../icons/tray-off-black.png")),
            ("error", include_bytes!("../icons/tray-error-black.png")),
        ];
        for i in 0..icons.len() {
            for j in (i + 1)..icons.len() {
                assert_ne!(
                    icons[i].1, icons[j].1,
                    "托盘 `{}` 与 `{}` 用的是同一张图 —— 这两个态在菜单栏里无法区分",
                    icons[i].0, icons[j].0
                );
            }
            // 白变体只换 RGB、不动 alpha ⇒ 与黑变体等大、必然不同字节。缺一半会让 Win/Linux 某个明暗下无图。
            let white: &[u8] = match icons[i].0 {
                "on" => include_bytes!("../icons/tray-on-white.png"),
                "connecting" => include_bytes!("../icons/tray-connecting-white.png"),
                "off" => include_bytes!("../icons/tray-off-white.png"),
                _ => include_bytes!("../icons/tray-error-white.png"),
            };
            assert_ne!(
                white, icons[i].1,
                "托盘 `{}` 的黑白变体是同一张图",
                icons[i].0
            );
        }
    }

    #[test]
    fn tray_icon_reconcile_stays_io_free() {
        let src = include_str!("main.rs");
        let body = crate::commands::guard_scan::top_level_fn_body(src, "fn reconcile_tray_icon(");
        for forbidden in [
            "system_proxy",
            "spawn_blocking",
            "Command::new",
            "block_on",
            ".await",
        ] {
            assert!(
                !body.contains(forbidden),
                "图标汇流点里出现了 `{forbidden}` —— 它被 4 个事件源 + 30s 轮询叫醒，\
                 无 IO 是它能被这样叫醒的前提（补 degraded 态的正确前置是后端先有低成本活态，\
                 见 tray.rs::TrayState 上方的决策登记）"
            );
        }
    }

    // ── 托盘汇流点幂等闸门（30s 轮询 × 全程 = 每 30s 一次磁盘写 + indicator 重载）────────
    //
    // 下面这组锁的是 `reconcile_tray_visual` 的**全部逃逸面**：短路被删（恒重画）、短路过度（变了不
    // 重画）、只比部分字段、缓存不更新、失败还照存缓存 —— 任一形态都必须有一条转红。

    /// 测试用视觉态构造（字段全给 → 漏比某个字段的变异一定会被下面逐字段的用例抓到）。
    fn vis(state: crate::tray::TrayState, dark_bg: bool, lang: crate::i18n::Lang) -> TrayVisual {
        TrayVisual {
            state,
            dark_bg,
            lang,
        }
    }

    /// 记录 apply 是否被调用的探针（`applied_ok` 模拟托盘落盘成功/失败）。
    fn run(cache: &mut Option<TrayVisual>, next: TrayVisual, applied_ok: bool) -> bool {
        let mut called = false;
        reconcile_tray_visual(cache, next, |v| {
            assert_eq!(v, next, "apply 必须拿到本次要落的态，不是别的");
            called = true;
            applied_ok
        });
        called
    }

    #[test]
    fn tray_visual_first_paint_always_applies() {
        use crate::i18n::Lang;
        use crate::tray::TrayState;
        // 缓存空（进程刚起）→ 必须画一次，否则托盘停在 conf 里的静态初值。
        let mut cache = None;
        assert!(run(
            &mut cache,
            vis(TrayState::Idle, true, Lang::ZhCN),
            true
        ));
        assert_eq!(
            cache,
            Some(vis(TrayState::Idle, true, Lang::ZhCN)),
            "画完要记下来"
        );
    }

    #[test]
    fn tray_visual_unchanged_state_is_short_circuited() {
        use crate::i18n::Lang;
        use crate::tray::TrayState;
        // 本条就是 B1 的正题：代理长期未运行时，30s 轮询每一轮拿到的都是同一个态。
        let mut cache = None;
        let same = vis(TrayState::Idle, true, Lang::ZhCN);
        assert!(run(&mut cache, same, true), "第一次要画");
        for round in 0..3 {
            assert!(
                !run(&mut cache, same, true),
                "第 {round} 轮轮询：视觉态未变仍重设 = 每 30s 一次 PNG 落盘 + indicator 重载（图标闪）"
            );
        }
    }

    #[test]
    fn tray_visual_every_field_change_repaints() {
        use crate::i18n::Lang;
        use crate::tray::TrayState;
        let base = vis(TrayState::Idle, true, Lang::ZhCN);
        // 逐字段单独翻转：任何一个字段被漏出比较键，对应这条就会因为「该画却短路了」而红。
        for (label, changed) in [
            (
                "state（四态 → 四种图标形态 + tooltip 文案）",
                vis(TrayState::Connected, true, Lang::ZhCN),
            ),
            (
                "dark_bg（任务栏明暗 → 黑/白变体）",
                vis(TrayState::Idle, false, Lang::ZhCN),
            ),
            (
                "lang（tooltip 文案语言）",
                vis(TrayState::Idle, true, Lang::EnUS),
            ),
        ] {
            let mut cache = Some(base);
            assert!(
                run(&mut cache, changed, true),
                "{label} 变了却没重画 —— 短路过度，图标/tooltip 停在旧态"
            );
        }
    }

    #[test]
    fn tray_visual_cache_tracks_latest_applied_state() {
        use crate::i18n::Lang;
        use crate::tray::TrayState;
        // 缓存只在首次写入、之后不更新（一种典型写错法）→ 第三步会误判「变了」而重画，本条转红。
        let mut cache = None;
        let a = vis(TrayState::Idle, true, Lang::ZhCN);
        let b = vis(TrayState::Connected, true, Lang::ZhCN);
        assert!(run(&mut cache, a, true));
        assert!(run(&mut cache, b, true), "a → b 该画");
        assert!(
            !run(&mut cache, b, true),
            "b → b 该短路（缓存必须跟到最新态）"
        );
    }

    #[test]
    fn tray_visual_failed_apply_is_retried_next_round() {
        use crate::i18n::Lang;
        use crate::tray::TrayState;
        // 落盘失败还照存缓存 → 之后每轮自愈都短路，托盘永久停在旧图：自愈网被自己的缓存关掉。
        let mut cache = None;
        let want = vis(TrayState::Connected, false, Lang::EnUS);
        assert!(run(&mut cache, want, false), "第一次尝试");
        assert_eq!(cache, None, "落盘失败不得记成「已落」");
        assert!(
            run(&mut cache, want, true),
            "上次落盘失败 → 下一轮自愈必须重试，而不是被缓存短路掉"
        );
        assert!(!run(&mut cache, want, true), "重试成功后才轮到短路");
    }

    // ── dialog 插件 ACL（真机 'plugin:dialog|confirm not allowed by ACL'）───────────
    //
    // 病灶不是「忘了配」，是 `dialog:default` **文案骗人**：它自称 "All dialog types are enabled"，
    // 实际 permissions = [allow-message, allow-save, allow-open]，**不含 allow-confirm/allow-ask**。
    // 而 tauri-plugin-dialog 的 init 脚本无条件把 `window.alert`/`window.confirm` 覆写成
    // `plugin:dialog|message` / `|confirm` ⇒ 任何一句 `window.confirm(...)` 都会撞 ACL。
    //
    // 「读 default.json 断言含 allow-confirm」这种测只是把配置抄一遍（改配置时会顺手改测，没牙）。
    // 下面改成断言**调用面 ⊆ 授权面**：扫前端源码里真实用到的 dialog 命令，逐个要求对应权限存在。

    /// 判定紧接 `out` 之后的 `/` 是否**可能**是正则字面量起点（而非除法 / JSX 斜杠）。
    ///
    /// 用白名单（只有这些前驱字符/关键字之后才允许正则）而非黑名单：宁可漏判成除法（= 今天的行为），
    /// 也不能把 `</div>`、`<img … />` 这类 JSX 斜杠误当正则起点 —— 那会在 .tsx 里新造注释泄漏。
    fn regex_can_start(out: &str) -> bool {
        let t = out.trim_end();
        let Some(last) = t.chars().last() else {
            return true; // 文件开头
        };
        if matches!(
            last,
            '=' | '(' | ',' | ':' | '[' | '!' | '&' | '|' | '?' | ';' | '{'
        ) {
            return true;
        }
        // `return /re/.test(x)` 一类：关键字之后同样允许正则。
        const REGEX_PREFIX_KW: [&str; 14] = [
            "return",
            "typeof",
            "instanceof",
            "in",
            "of",
            "new",
            "delete",
            "void",
            "throw",
            "case",
            "do",
            "else",
            "yield",
            "await",
        ];
        let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
        REGEX_PREFIX_KW.iter().any(|kw| {
            t.strip_suffix(kw)
                .is_some_and(|head| head.chars().last().is_none_or(|c| !ident(c)))
        })
    }

    /// 剥 TS/TSX 注释（`//` 与 `/* */`），**不碰字符串/模板字面量内部**（`"https://x"` 里的 `//`
    /// 不是注释起点 —— 否则同行后续真代码会被吃掉，扫描面出洞）。换行原样保留。
    ///
    /// 守卫扫的是**代码**：本仓注释里到处在讲 `window.confirm` 这个坑（`settings-logic.ts:252/254`、
    /// `SettingsHelper.tsx:101`），不剥注释的话「前端还在用 confirm」这个前提永远为真 —— 反向哨兵
    /// 退化成空转，而它恰恰是写来防这个形态的。
    ///
    /// 与前端 `settings-logic.test.ts` 的 `stripComments` 同职责（那边正则、这边多认字符串状态）。
    ///
    /// 正则字面量（`const re = /['"]/g;`）单列一档：不认的话里面的 `'` 会被当成字符串起点，引号状态
    /// **一路挂到文件后面某个落单引号**，中间所有注释都不再被剥 ⇒ 反向哨兵被注释里的
    /// `window.confirm` 喂绿。识别到就**原样拷贝**（绝不删除），故判错的最坏后果 = 退化成今天的行为，
    /// 不可能吃掉真代码造成扫描面出洞。起点判定用**白名单**（`= ( , : [ ! & | ? ; {` + 关键字）而非
    /// 「不是标识符就算正则」：后者会把 JSX 的 `</div>`、`<img … />` 误当正则起点，在 .tsx 里
    /// 反倒制造新的注释泄漏。
    ///
    /// 另有兜底：`'` / `"` 字符串**不能跨行**（只有模板串可以），故撞见换行即判定「刚才解析错了」
    /// 并复位。任何残余的引号态误判（正则、JSX 文本里的 `don't` …）都被限制在**一行内**，
    /// 不再顺着文件级联。
    fn strip_ts_comments(src: &str) -> String {
        let mut out = String::with_capacity(src.len());
        let mut it = src.chars().peekable();
        let mut quote: Option<char> = None; // Some(引号字符) = 字符串/模板内
        let mut escaped = false;
        while let Some(c) = it.next() {
            if let Some(q) = quote {
                out.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                } else if c == '\n' && q != '`' {
                    quote = None; // 单/双引号串不跨行 ⇒ 走到这儿说明刚才判错了，就地收手
                }
                continue;
            }
            match c {
                '\'' | '"' | '`' => {
                    quote = Some(c);
                    out.push(c);
                }
                '/' if it.peek() == Some(&'/') => {
                    for n in it.by_ref() {
                        if n == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                '/' if it.peek() == Some(&'*') => {
                    it.next(); // 吃掉 '*'
                    let mut prev = '\0';
                    for n in it.by_ref() {
                        if prev == '*' && n == '/' {
                            break;
                        }
                        if n == '\n' {
                            out.push('\n');
                        }
                        prev = n;
                    }
                }
                // 正则字面量：原样拷贝，只为屏蔽里面的引号，不改变任何字符。
                '/' if regex_can_start(&out) => {
                    out.push('/');
                    let mut in_class = false; // `[...]` 内的 `/` 不结束字面量
                    let mut esc = false;
                    for n in it.by_ref() {
                        out.push(n);
                        if esc {
                            esc = false;
                            continue;
                        }
                        match n {
                            '\\' => esc = true,
                            '[' => in_class = true,
                            ']' => in_class = false,
                            '/' if !in_class => break,
                            '\n' => break, // 正则不跨行：真跨行说明判错了，立刻收手
                            _ => {}
                        }
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }

    /// 递归收集目录下所有**生产** .ts/.tsx 源码（已剥注释、已排除 `*.test.*` / `*.spec.*`；
    /// 测试期读盘，无新增依赖）。
    ///
    /// 排除测试文件：vitest 跑在 node 环境、根本不经 Tauri ACL，测试文本里出现 `window.confirm`
    /// 既不需要授权，也不该让「前端还在用 confirm」这个前提为真（`settings-logic.test.ts` 此前正是
    /// 这么把反向哨兵喂绿的）。
    fn collect_sources(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_sources(&p, out);
                continue;
            }
            if !matches!(
                p.extension().and_then(|s| s.to_str()),
                Some("ts") | Some("tsx")
            ) {
                continue;
            }
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_owned();
            if name.contains(".test.") || name.contains(".spec.") {
                continue;
            }
            if let Ok(s) = std::fs::read_to_string(&p) {
                out.push((p, strip_ts_comments(&s)));
            }
        }
    }

    fn ui_src() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ui/src")
    }

    /// 直接 invoke 命令名的**裸串**形态 → 所需权限 id。
    ///
    /// 光有这张表守不住 import 形态：`import { save } from '@tauri-apps/plugin-dialog'` 的源码里
    /// 一个 `plugin:dialog|save` 字样都不会出现 —— 见 [`dialog_import`]，三条检测面并联。
    const DIALOG_INVOKE_TO_PERM: [(&str, &str); 5] = [
        ("plugin:dialog|confirm", "dialog:allow-confirm"),
        ("plugin:dialog|message", "dialog:allow-message"),
        ("plugin:dialog|ask", "dialog:allow-ask"),
        ("plugin:dialog|open", "dialog:allow-open"),
        ("plugin:dialog|save", "dialog:allow-save"),
    ];

    /// 被插件 init 脚本**覆写的全局函数** → 所需权限 id。插件无条件把 `window.confirm` / `window.alert`
    /// 换成 `plugin:dialog|confirm` / `|message`，所以调用点写不写 `window.` 前缀都一样走 ACL。
    const DIALOG_GLOBAL_TO_PERM: [(&str, &str); 2] = [
        ("confirm", "dialog:allow-confirm"),
        ("alert", "dialog:allow-message"),
    ];

    /// 源码里是否调用了全局函数 `name`。
    ///
    /// 只匹配字面量 `window.confirm(` 会漏掉一整片等价形态 —— 它们撞的是**同一条** ACL：
    /// 裸调 `await confirm('x')`、括号前带空格 `window.confirm ('x')`、计算成员
    /// `window['confirm'](…)`、`globalThis.confirm(…)`。漏了它们不仅是 ACL 洞（一旦收回
    /// `allow-confirm` 就逃逸），更让反向哨兵「confirm 只许出现在 nativeConfirm 一处」对别处裸用不转红。
    ///
    /// `foo.confirm(` 这类**他人成员**不算全局调用（`window` / `globalThis` 才是），否则会把无关对象
    /// 的同名方法误判成 dialog 调用。
    fn calls_global(src: &str, name: &str) -> bool {
        let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
        let is_global_owner = |head: &str| {
            ["window", "globalThis"].iter().any(|o| {
                head.strip_suffix(o)
                    .is_some_and(|rest| rest.chars().last().is_none_or(|c| !ident(c)))
            })
        };
        for (i, _) in src.match_indices(name) {
            // token 左边界：`reconfirm(` 不算。右边界不必单独校验 —— 多出的标识符字符必然让下面
            // 「后面是 `(`」的判据落空（`confirmDelete(` 的 rest 以 `D` 开头），单写一条杀不掉任何变异。
            if src[..i].ends_with(ident) {
                continue;
            }
            // 括号前允许空白：`window.confirm ('x')`。
            let rest = &src[i + name.len()..];
            if !rest.trim_start().starts_with('(') {
                continue;
            }
            let head = src[..i].trim_end();
            if let Some(owner) = head.strip_suffix('.') {
                if !is_global_owner(owner.trim_end()) {
                    continue; // `dialog.confirm(` 之类：不是被覆写的那个全局
                }
            }
            return true;
        }
        // 计算成员形态：`window['confirm'](…)` —— 上面的标识符扫描看不见（名字在字符串里）。
        ["window", "globalThis"].iter().any(|owner| {
            ['\'', '"']
                .iter()
                .any(|q| src.contains(&format!("{owner}[{q}{name}{q}]")))
        })
    }

    /// dialog 插件的 JS 模块说明符（具名 import 形态的入口）。
    const DIALOG_MODULE: &str = "@tauri-apps/plugin-dialog";

    /// 插件导出的**命令面全集** → 所需权限 id（导出名与 `plugin:dialog|<name>` 同名）。
    ///
    /// `save` 这行是本批补的洞：`allow-save` 刚被收回，而旧表里 `plugin:dialog|save` 压根不存在
    /// ⇒ 有人写 `import { save } ...` 时守卫全绿、运行期真机抛
    /// `Command plugin:dialog|save not allowed by ACL` —— 正是本批要根治的病灶原型复发。
    const DIALOG_API_TO_PERM: [(&str, &str); 5] = [
        ("confirm", "dialog:allow-confirm"),
        ("message", "dialog:allow-message"),
        ("ask", "dialog:allow-ask"),
        ("open", "dialog:allow-open"),
        ("save", "dialog:allow-save"),
    ];

    /// 一份源码对 [`DIALOG_MODULE`] 的使用形态。
    #[derive(Debug, PartialEq, Eq)]
    enum DialogImport {
        /// 没引用该模块。
        Absent,
        /// 解析出静态具名列表（可能为空：纯类型 import）→ 可精确映射到权限。
        /// 保存的是**导出名**（`open as pick` 取 `open`），因为 ACL 认的是命令名不是本地别名。
        Named(Vec<String>),
        /// 引用了模块但取不到静态具名列表（`import * as` / `await import(...)` / side-effect import）
        /// → 调用面不可判 ⇒ **失败关闭**，让人来判，而不是默默放行成第二个洞。
        Opaque,
    }

    /// 找出 `src` 里所有落在 token 边界上的 `kw` 关键字位置（避开 `important` / `reimport` 之类子串命中）。
    fn keyword_positions<'a>(src: &'a str, kw: &'a str) -> impl Iterator<Item = usize> + 'a {
        let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
        src.match_indices(kw)
            .filter(move |(i, _)| {
                !src[..*i].ends_with(ident) && !src[*i + kw.len()..].starts_with(ident)
            })
            .map(|(i, _)| i)
    }

    /// 从一条 import/export 语句的**正文**（关键字之后 → 下一条语句关键字为止）里取模块说明符，
    /// 返回 `(关键字与说明符之间的 clause, 模块名)`。撞见 `;` 说明本语句压根没有说明符。
    fn module_specifier(body: &str) -> Option<(&str, &str)> {
        let mut it = body.char_indices();
        while let Some((off, c)) = it.next() {
            match c {
                ';' => return None,
                '\'' | '"' | '`' => {
                    let start = off + c.len_utf8();
                    let mut escaped = false;
                    for (end, n) in it.by_ref() {
                        if escaped {
                            escaped = false;
                        } else if n == '\\' {
                            escaped = true;
                        } else if n == c {
                            return Some((&body[..off], &body[start..end]));
                        }
                    }
                    return None;
                }
                _ => {}
            }
        }
        None
    }

    /// 解析源码对 `@tauri-apps/plugin-dialog` 的使用形态。
    ///
    /// **不从模块名回看整份文件**：回看会一路切到*上一条* import 的关键字，于是上一条 import 的花括号
    /// 就把「有没有具名列表」这个判据满足掉 —— 真实源文件几乎必有前置 import，`export … from` /
    /// `export *` / 动态 `import(M)` / `require()` 因此全部被误判成 `Named(上一条 import 的名字)`，
    /// 失败关闭形同虚设（甚至吞掉本条真正的具名项）。
    ///
    /// 改为**先按语句切分再解析**：每条语句的射程 = 本关键字 → 下一条 import/export 关键字，越不了界。
    /// 模块名若出现在任何 import/export 语句之外（`require('…')`、`const M = '…'`、子路径…），
    /// 直接判不可判。
    ///
    /// 两条独立的网：①「本语句 clause 必须整好是具名花括号组」②「模块名出现次数必须全被 import/export
    /// 语句认领」。变异验证显示二者互为兜底 —— 单独敲掉 `export` 分支或语句射程上界，逃逸构造仍被 ②
    /// 拦成 `Opaque`（失败关闭方向），不产生假绿。保留 ① 是为了**精度**（少报无谓的不可判），不是安全属性。
    fn dialog_import(src: &str) -> DialogImport {
        let total = src.matches(DIALOG_MODULE).count();
        if total == 0 {
            return DialogImport::Absent;
        }
        let mut stmts: Vec<(usize, &str)> = ["import", "export"]
            .iter()
            .flat_map(|kw| keyword_positions(src, kw).map(move |i| (i, *kw)))
            .collect();
        stmts.sort_unstable();

        let mut names = Vec::new();
        let mut matched = 0usize;
        for (n, &(pos, kw)) in stmts.iter().enumerate() {
            let end = stmts.get(n + 1).map_or(src.len(), |(next, _)| *next);
            let Some((clause, module)) = module_specifier(&src[pos + kw.len()..end]) else {
                continue;
            };
            if module != DIALOG_MODULE {
                continue;
            }
            matched += 1;
            // `export … from '…'`：再导出。调用面转移到下游消费方，而消费方 import 的是**本地**模块名
            // （扫描时看不出它连着 dialog 插件）⇒ 不可判，失败关闭。
            if kw == "export" {
                return DialogImport::Opaque;
            }
            // clause 必须**整好**是一个具名花括号组 + `from`。`import * as d from` / `import d from` /
            // `import d, { x } from`（默认绑定没被覆盖）/ 动态 `import(` / side-effect import 一律不可判。
            let Some(spec) = clause.trim().strip_suffix("from") else {
                return DialogImport::Opaque;
            };
            let Some(inner) = spec
                .trim()
                .strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
            else {
                return DialogImport::Opaque;
            };
            for raw in inner.split(',') {
                // `type Foo` → 剥 type 前缀；`open as pick` → 取导出名 `open`。
                let n = raw.trim().trim_start_matches("type ").trim();
                if let Some(exported) = n.split_whitespace().next() {
                    if !exported.is_empty() {
                        names.push(exported.to_owned());
                    }
                }
            }
        }
        if matched != total {
            return DialogImport::Opaque;
        }
        DialogImport::Named(names)
    }

    /// 一份源码需要的 dialog 权限全集（裸串形态 ∪ 具名 import 形态）。
    /// `Err` = 调用面不可判，守卫按失败关闭处理。
    fn required_dialog_perms(src: &str) -> Result<Vec<&'static str>, &'static str> {
        let mut need: Vec<&'static str> = DIALOG_INVOKE_TO_PERM
            .iter()
            .filter(|(call, _)| src.contains(call))
            .map(|(_, perm)| *perm)
            .chain(
                DIALOG_GLOBAL_TO_PERM
                    .iter()
                    .filter(|(name, _)| calls_global(src, name))
                    .map(|(_, perm)| *perm),
            )
            .collect();
        match dialog_import(src) {
            DialogImport::Absent => {}
            DialogImport::Opaque => return Err(
                "引用了 @tauri-apps/plugin-dialog 却取不到静态具名列表（namespace / 动态 import / \
                     side-effect import）→ 调用面不可判。请改成具名 import，否则 ACL 守卫失效",
            ),
            DialogImport::Named(names) => {
                for n in names {
                    if let Some((_, perm)) = DIALOG_API_TO_PERM.iter().find(|(api, _)| *api == n) {
                        need.push(perm);
                    }
                }
            }
        }
        Ok(need)
    }

    fn granted(capability_json: &str) -> Vec<String> {
        let v: serde_json::Value =
            serde_json::from_str(capability_json).expect("capability 非法 JSON");
        v["permissions"]
            .as_array()
            .expect("capability 缺 permissions")
            .iter()
            .filter_map(|p| p.as_str().map(str::to_owned))
            .collect()
    }

    /// `capabilities/*.json` → `window label → 该 window 的授权面`（一份 capability 可覆盖多个 window）。
    ///
    /// **测试期读盘**而非手写清单：手写「次级窗有哪些」正是漏掉 `update-popup` 的成因 —— 新增一份
    /// capability / 新增一个 window 必须自动进扫描面，不能指望有人记得回来改这张表。
    fn capabilities_by_window() -> std::collections::BTreeMap<String, Vec<String>> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
        let mut out: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for entry in std::fs::read_dir(&dir)
            .expect("capabilities/ 读不到")
            .flatten()
        {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let raw = std::fs::read_to_string(&p).expect("capability 读不到");
            let v: serde_json::Value = serde_json::from_str(&raw).expect("capability 非法 JSON");
            let windows: Vec<String> = v["windows"]
                .as_array()
                .expect("capability 缺 windows")
                .iter()
                .filter_map(|w| w.as_str().map(str::to_owned))
                .collect();
            let perms = granted(&raw);
            for w in windows {
                out.entry(w).or_default().extend(perms.iter().cloned());
            }
        }
        out
    }

    /// 次级窗的前端入口目录名（= window label）。约定：`ui/src/<label>/main.ts(x)`，与
    /// `ui/vite.config.ts` 的多页 `rollupOptions.input`、Rust 侧建窗 label 一一对应。
    /// 主窗入口是 `ui/src/main.tsx`（顶层文件，不是目录），故天然不在此列。
    fn secondary_window_dirs() -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(ui_src())
            .expect("ui/src 读不到")
            .flatten()
            .map(|e| e.path())
            .filter(|p| ["main.ts", "main.tsx"].iter().any(|f| p.join(f).is_file()))
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn dialog_acl_covers_every_frontend_dialog_call() {
        let mut files = Vec::new();
        collect_sources(&ui_src(), &mut files);
        assert!(
            !files.is_empty(),
            "没扫到前端源码，测试形同虚设（路径漂了？）"
        );

        let perms = granted(include_str!("../capabilities/default.json"));
        // `dialog:default` 缺 confirm/ask —— 它不能替代逐条授权，撞见即判缺。
        let has = |perm: &str| perms.iter().any(|p| p == perm);

        for (path, src) in &files {
            let need = required_dialog_perms(src)
                .unwrap_or_else(|why| panic!("{}：{why}", path.display()));
            for perm in need {
                assert!(
                    has(perm),
                    "{} 用到 dialog 命令，但 capabilities/default.json 未授 {perm} \
                     → 运行期抛 'not allowed by ACL' 的未捕获 promise rejection",
                    path.display()
                );
            }
        }
        // 前身是「生产代码确实还在用 window.confirm」的反向哨兵，用来防本测空转。
        // 2026-07-29 破坏性操作的二次确认改走原地二次点击（`ui/src/lib/confirm-twice.ts`）后
        // 生产代码已无 confirm 调用，该哨兵按它自己写的退役条件退役，换成下面那条**正向不变式**：
        // 生产代码不得再回退到 window.confirm。防空转的职责由本函数开头的 `!files.is_empty()`
        // 与 `comment_stripper_has_teeth` 共同承担。
    }

    /// 生产代码不得再调用 `window.confirm` —— 破坏性操作一律走原地二次点击
    /// （`ui/src/lib/confirm-twice.ts` 的 `useConfirmTwice`，对齐原型 confirmTwice L3211）；
    /// 需要成段解释的确认走 App 自绘 `ConfirmDialog`。
    ///
    /// 为什么这是一条**产品不变式**而不只是风格偏好：插件 init 脚本把 `window.confirm` 覆写成
    /// `plugin:dialog|confirm`，于是「二次确认」这道闸门的成立与否取决于该窗口的 capability 有没有授
    /// `dialog:allow-confirm`。漏授时闸门不是降级成「无确认」，而是整条腿抛 rejection ⇒ 用户看到的是
    /// 「卸载失败」。真机上就是这么表现的（2026-07-29 于 5.238 复现）。原地二次点击把二次确认从一项
    /// 运行期授权变回一段普通渲染，没有这条失败模式。
    ///
    /// 与前端 `settings-logic.test.ts` 的同名约束互为两侧：那边扫 settings 子树，这边扫整个 `ui/src`。
    #[test]
    fn production_code_never_calls_global_confirm() {
        let mut files = Vec::new();
        collect_sources(&ui_src(), &mut files);
        assert!(
            !files.is_empty(),
            "没扫到前端源码，测试形同虚设（路径漂了？）"
        );
        let hits: Vec<String> = files
            .iter()
            .filter(|(_, s)| calls_global(s, "confirm"))
            .map(|(p, _)| {
                p.strip_prefix(ui_src())
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(
            hits.is_empty(),
            "这些文件又用回了 window.confirm：{hits:?} —— 二次确认会退化成一项 ACL 授权，\
             漏授时整条腿抛 rejection（用户看到的是「操作失败」）。请改用 useConfirmTwice"
        );
    }

    /// 收回的授权不许悄悄回来：`dialog:allow-confirm` 已随自绘弹窗退役。
    ///
    /// 单独一条而不是并进上面那测：两者失败的含义不同 —— 上面红 = 有人写回了 confirm 调用；
    /// 这条红 = 授权面在没有调用点的情况下被重新放开（纯多余授权面，且会让上面那条的后果重新变得隐蔽）。
    #[test]
    fn dialog_confirm_permission_stays_revoked() {
        let perms = granted(include_str!("../capabilities/default.json"));
        assert!(
            !perms.iter().any(|p| p == "dialog:allow-confirm"),
            "capabilities/default.json 又授了 dialog:allow-confirm —— \
             生产代码已无 window.confirm 调用（见 production_code_never_calls_global_confirm），\
             该授权是多余授权面。若确有新增调用点，请先说明为什么不能走 useConfirmTwice"
        );
    }

    #[test]
    fn comment_stripper_has_teeth() {
        // 哨兵的地基自检：剥注释必须**真的**吃掉注释里的 window.confirm，又**不能**吃掉代码。
        // 剥过头（比如把整份源码吃空）→ 命中面恒空 → 哨兵恒红；剥不动 → 哨兵恒绿。两侧都锁。
        assert!(
            !strip_ts_comments("/* window.confirm(x) */\nlet a = 1;").contains("window.confirm(")
        );
        assert!(!strip_ts_comments("// window.confirm(x)\nlet a = 1;").contains("window.confirm("));
        assert!(
            !strip_ts_comments("/**\n * window.confirm(x)\n */\nlet a = 1;")
                .contains("window.confirm(")
        );
        assert!(
            strip_ts_comments("if (window.confirm(\"x\")) drop(); // 尾注释")
                .contains("window.confirm(")
        );
        // 字符串里的 `//` 不是注释起点 —— 否则同行后续真调用被吃掉 = 扫描面出洞（假绿）。
        assert!(
            strip_ts_comments("const u = \"https://x\"; if (window.confirm(\"y\")) drop();")
                .contains("window.confirm(")
        );
        assert!(
            strip_ts_comments("const t = `a // b`; window.confirm(\"y\");")
                .contains("window.confirm(")
        );
        // 剥完不塌行：注释吃掉的换行必须补回，否则剥后文本与真实文件行号错位（断言消息里的
        // file:line 就成了假话），且相邻两行会被拼到一起。块注释与行注释两侧都锁。
        assert_eq!(strip_ts_comments("a;\n/* x\ny */\nb;").lines().count(), 4);
        assert_eq!(strip_ts_comments("a; // c\nb;").lines().count(), 2);
        // 正则字面量里的引号不得污染字符串状态：`/['"]/` 若被当成字符串起点，引号态会一路挂到文件
        // 后面某个落单引号，中间的注释全不再被剥 ⇒ 哨兵被注释文本喂绿。原样拷贝 + 后续注释照剥。
        let re = strip_ts_comments("const r = /['\"]/g; // window.confirm(x)\nlet a = 1;");
        assert!(re.contains("/['\"]/g"), "正则字面量必须原样保留");
        assert!(!re.contains("window.confirm("), "正则之后的行注释仍须被剥");
        // JSX 的 `/` **不是**正则起点（`</div>`、`<img … />`）：误判成正则会吃掉同行后续注释边界，
        // 在 .tsx 里反倒新造泄漏。两种形态都必须让紧随其后的注释照常被剥。
        assert!(!strip_ts_comments("<div />; // window.confirm(x)\n").contains("window.confirm("));
        assert!(!strip_ts_comments("</div>; // window.confirm(x)\n").contains("window.confirm("));
        // 除法不得被当成正则：`a / b` 之后的注释照剥。
        assert!(
            !strip_ts_comments("const q = a / b; // window.confirm(x)\n")
                .contains("window.confirm(")
        );
        // 兜底：单/双引号串不跨行 —— 未闭合的引号（JSX 文本里的 `don't` 等）只准污染一行。
        assert!(
            !strip_ts_comments("<p>don't</p>\n// window.confirm(x)\n").contains("window.confirm("),
            "未闭合引号必须在换行处复位，否则污染一路级联到文件尾"
        );
        // 测试文件不进扫描面（此前 settings-logic.test.ts 的测试文本就是哨兵的假绿来源之一）。
        let mut files = Vec::new();
        collect_sources(&ui_src(), &mut files);
        // 非空兜底：`collect_sources` 在 read_dir 失败时静默 return，files 为空则下面的
        // `!any(...)` 恒真 —— 空集上的「不存在」断言没牙。
        assert!(
            !files.is_empty(),
            "没扫到前端源码，测试形同虚设（路径漂了？）"
        );
        assert!(
            !files
                .iter()
                .any(|(p, _)| p.to_string_lossy().contains(".test.")),
            "测试文件混进了 ACL 扫描面"
        );
    }

    #[test]
    fn dialog_import_form_is_in_the_detection_surface() {
        // B2：插件 JS API 的具名 import 形态。源码里**不会**出现 `plugin:dialog|save` 字样，
        // 旧守卫（只认裸串、且表里根本没有 save）对它全绿 → 真机 'plugin:dialog|save not allowed by ACL'。
        let src = "import { save } from '@tauri-apps/plugin-dialog';\nawait save({});";
        assert_eq!(
            required_dialog_perms(src).unwrap(),
            ["dialog:allow-save"],
            "具名 import 的 save 必须被识别为需要 dialog:allow-save"
        );
        // 别名与类型 import：ACL 认的是**导出名**，不是本地别名；纯类型 import 不产生权限需求。
        assert_eq!(
            required_dialog_perms(
                "import { open as pick, type OpenDialogOptions } from '@tauri-apps/plugin-dialog';"
            )
            .unwrap(),
            ["dialog:allow-open"]
        );
        // 多具名 + 双引号 + 换行排版。
        let multi = "import {\n  ask,\n  message,\n} from \"@tauri-apps/plugin-dialog\";";
        let mut got = required_dialog_perms(multi).unwrap();
        got.sort_unstable();
        assert_eq!(got, ["dialog:allow-ask", "dialog:allow-message"]);
        // 裸串形态没被这次改动挤掉。
        assert_eq!(
            required_dialog_perms("await invoke('plugin:dialog|save')").unwrap(),
            ["dialog:allow-save"]
        );
        assert_eq!(
            required_dialog_perms("if (window.confirm('x')) {}").unwrap(),
            ["dialog:allow-confirm"]
        );
        // 不引用该模块 ⇒ 零需求（守卫不能对无关文件乱要权限）。
        assert_eq!(
            required_dialog_perms("import { useState } from 'react';").unwrap(),
            Vec::<&str>::new()
        );
        // `import` 只算 token，不算子串 —— 两侧边界都锁：
        // ① 前边界：`save as reimport` 里的 `reimport` 落在真关键字与模块名**之间**，若不校验前边界，
        //    回看会停在它身上 ⇒ stmt 里没有 `{` ⇒ 误判 Opaque，把一条本可精确判定的 import 打成噪声。
        assert_eq!(
            dialog_import("import { save as reimport } from '@tauri-apps/plugin-dialog';"),
            DialogImport::Named(vec!["save".into()])
        );
        // ② 后边界：`importantThing` 同理（且它证明 rfind 不会被前文的同形子串带偏）。
        assert_eq!(
            dialog_import(
                "const important = { save };\nimport { ask } from '@tauri-apps/plugin-dialog';"
            ),
            DialogImport::Named(vec!["ask".into()])
        );
        assert_eq!(
            dialog_import("import { importantFlag, ask } from '@tauri-apps/plugin-dialog';"),
            DialogImport::Named(vec!["importantFlag".into(), "ask".into()])
        );
        // 失败关闭：取不到具名列表的三种形态一律判「不可判」，不许静默放行。
        for opaque in [
            "import * as dialog from '@tauri-apps/plugin-dialog';",
            "const d = await import('@tauri-apps/plugin-dialog');",
            "import '@tauri-apps/plugin-dialog';",
        ] {
            assert!(
                required_dialog_perms(opaque).is_err(),
                "{opaque} 的调用面不可判，守卫必须失败关闭而不是放行"
            );
        }
        // 当前前端确实没人用 import 形态（现状安全）—— 变了要么这条红、要么 ACL 测红。
        let mut files = Vec::new();
        collect_sources(&ui_src(), &mut files);
        assert!(
            !files.iter().any(|(_, s)| s.contains(DIALOG_MODULE)),
            "前端开始直接 import @tauri-apps/plugin-dialog 了 —— 复核 capabilities 授权面后更新本条"
        );
    }

    #[test]
    fn dialog_import_detection_survives_multi_statement_files() {
        // A1 根因：旧实现从模块名 `rfind("import")` **回看整份文件**，stmt 于是从*上一条* import 的
        // 关键字一路切到本处 —— 只要文件里此前有任何一条带花括号的 import（真实源文件几乎必有），
        // `find('{')` / `rfind('}')` 就被上一条 import 的花括号满足 ⇒ 永远进不了 Opaque 分支，反而把
        // 上一条 import 的名字当成 dialog 的具名列表（需求集为空 ⇒ **静默全绿**，真机才炸 ACL）。
        //
        // 旧单测全绿只是因为用例都是**单行、无前置 import 的玩具输入** —— 这正是本缺陷藏住的直接原因。
        // 下面每条都带真实前置 import。
        const HEAD: &str = "import { useState } from 'react';\nimport { clsx } from 'clsx';\n";

        // ① 具名 import 仍须精确：前置 import 的名字不得混进 dialog 的需求集。
        assert_eq!(
            dialog_import(&format!(
                "{HEAD}import {{ save }} from '@tauri-apps/plugin-dialog';\nawait save({{}});"
            )),
            DialogImport::Named(vec!["save".into()]),
            "前置 import 的具名列表混进了 dialog 的需求集"
        );
        // ② 逃逸面穷举：旧实现在这些构造上一律返回 Named([\"useState\"]) ⇒ 需求为空 ⇒ 静默全绿。
        //    新实现一律失败关闭（不可判就让人来判，绝不默默放行）。
        for escape in [
            "export { save } from '@tauri-apps/plugin-dialog';",
            "export * from '@tauri-apps/plugin-dialog';",
            "export { confirm, save } from '@tauri-apps/plugin-dialog';",
            "const M = '@tauri-apps/plugin-dialog';\nconst d = await import(M);",
            "const d = require('@tauri-apps/plugin-dialog');",
            "import * as d from '@tauri-apps/plugin-dialog';",
            "const d = await import('@tauri-apps/plugin-dialog');",
            "import '@tauri-apps/plugin-dialog';",
            "import d, { save } from '@tauri-apps/plugin-dialog';",
            "import { save } from '@tauri-apps/plugin-dialog/foo';",
        ] {
            let src = format!("{HEAD}{escape}");
            assert_eq!(
                dialog_import(&src),
                DialogImport::Opaque,
                "{escape}（带前置 import）必须失败关闭"
            );
            assert!(
                required_dialog_perms(&src).is_err(),
                "{escape} 的调用面不可判，守卫必须失败关闭而不是放行"
            );
        }
        // ③ 多条 dialog import 并存：名字合并，不能只认其中一条。
        let two = format!(
            "{HEAD}import {{ save }} from '@tauri-apps/plugin-dialog';\n\
             import {{ ask }} from '@tauri-apps/plugin-dialog';"
        );
        let mut got = required_dialog_perms(&two).unwrap();
        got.sort_unstable();
        assert_eq!(got, ["dialog:allow-ask", "dialog:allow-save"]);
        // ④ 真实排版：dialog import 夹在中间，后面还有 export / 别的 import。
        let sandwich = format!(
            "{HEAD}import {{ message }} from '@tauri-apps/plugin-dialog';\n\
             import type {{ Foo }} from './foo';\nexport function go() {{}}\n"
        );
        assert_eq!(
            required_dialog_perms(&sandwich).unwrap(),
            ["dialog:allow-message"]
        );
    }

    #[test]
    fn dialog_global_call_forms_are_in_the_detection_surface() {
        // 插件 init 脚本覆写的是**全局对象**，以下形态撞的是同一条 `plugin:dialog|confirm` ACL。
        // 只认字面量 `window.confirm(` 会全部漏掉：今天不成洞只因 allow-confirm 已授 —— 收回即逃逸；
        // 且反向哨兵「confirm 只许出现在 nativeConfirm 一处」对「别处裸用」根本不转红。
        for form in [
            "if (window.confirm('x')) {}",
            "if (await confirm('x')) {}",
            "if (window.confirm ('x')) {}",
            "if (window['confirm']('x')) {}",
            "if (window[\"confirm\"]('x')) {}",
            "if (globalThis.confirm('x')) {}",
        ] {
            assert_eq!(
                required_dialog_perms(form).unwrap(),
                ["dialog:allow-confirm"],
                "{form} 走 plugin:dialog|confirm，必须进检测面"
            );
        }
        assert_eq!(
            required_dialog_perms("alert('x')").unwrap(),
            ["dialog:allow-message"]
        );
        // 反向：不得过度命中 —— 他人成员 / 同前缀标识符不是被覆写的那个全局。过度命中会把无关文件
        // 拖进需求集，更会让反向哨兵的唯一命中面失真。
        for benign in [
            "dialog.confirm('x')",
            "reconfirm('x')",
            "confirmDelete('x')",
            "const confirmed = true;",
            "type Alerts = { alert: string };",
        ] {
            assert_eq!(
                required_dialog_perms(benign).unwrap(),
                Vec::<&str>::new(),
                "{benign} 不是 dialog 调用，守卫不许乱要权限"
            );
        }
    }

    #[test]
    fn secondary_windows_dialog_calls_are_also_covered() {
        // 能力集按 window label 生效（default.json 只覆盖 "main"）。次级窗是**独立 window**，用了
        // dialog 却没授权同样会撞 ACL。
        //
        // 窗口清单**不再手写**：原来写死 `[("tray", …)]`，于是 `update-popup` 整个窗漏在射程外 ——
        // 它有前端入口目录、有 Rust 建窗路径（`runtime::update_popup::POPUP_LABEL`），却一份
        // capability 都没有 ⇒ 零权限，`listen()` 真机被 ACL 拒，而调用点是 `.catch(() => {})`
        // ⇒ 静默失效，连报错都看不到。改为「前端入口目录 × capabilities/*.json」双向驱动。
        let by_window = capabilities_by_window();
        let dirs = secondary_window_dirs();
        // 清单本身要**钉死**：只断言「清单里的窗都被覆盖」是杀不掉「有人把清单写回硬编码」的 ——
        // 原缺陷正是这个形态（写死 [("tray", …)] ⇒ update-popup 整个窗不在射程内，永远绿）。
        // 新增次级窗时本条先红，逼人当场确认 capability 与扫描面都跟上了。
        assert_eq!(
            dirs,
            ["tray", "update-popup"],
            "次级窗前端入口目录清单变了（ui/src/<label>/main.ts(x)）—— 新窗必须同时有 capability 覆盖；\
             若这里为空说明扫描面塌了，本测将退化成恒绿"
        );

        for label in &dirs {
            let perms = by_window.get(label).unwrap_or_else(|| {
                panic!(
                    "window \"{label}\" 有前端入口目录 ui/src/{label}/ 却没有任何 capability 覆盖它 \
                     ⇒ 该窗零权限：listen / 插件命令真机一律被 ACL 拒（前端常写 .catch(() => {{}}) \
                     ⇒ 静默失效）。请在 capabilities/ 下补一份 windows 含 \"{label}\" 的能力集"
                )
            });
            let mut files = Vec::new();
            collect_sources(&ui_src().join(label), &mut files);
            assert!(
                !files.is_empty(),
                "window \"{label}\" 一份前端源码都没扫到 —— 空扫描面 = 恒绿，本条形同虚设"
            );
            for (path, src) in &files {
                // 与主窗同一条检测面（invoke 裸串 ∪ 全局覆写 ∪ 具名 import），否则次级窗会漏形态。
                let need = required_dialog_perms(src)
                    .unwrap_or_else(|why| panic!("{}：{why}", path.display()));
                for perm in need {
                    assert!(
                        perms.iter().any(|p| p == perm),
                        "{} 属 window \"{label}\"，用了 dialog 命令但该 window 的 capability 未授 {perm}",
                        path.display()
                    );
                }
            }
        }
    }

    #[test]
    fn tray_icon_events_are_the_proxy_lifecycle_channels() {
        use crate::events::channel as ch;
        // 订的必须是既有通道常量本身（防有人把常量改成拼错的字面量：listen_any 对不存在的
        // 事件名不会报错，只会**静默永不触发**——与本 bug 同型的静默失败）。
        // 逐条列举而非只查前缀：CONFIG_CHANGED 不带 `event:proxy` 前缀，旧的前缀断言会把它误判成非法，
        // 而「只查前缀」本身也放得过 `event:proxyFoo` 这种拼错的近似名。
        assert_eq!(
            TRAY_SYNC_EVENTS,
            [
                ch::EVENT_PROXY_STARTED,
                ch::EVENT_PROXY_STOPPED,
                ch::EVENT_PROXY_ERROR,
                ch::EVENT_CONFIG_CHANGED,
            ],
            "三条代理终态 + 一条配置变更（后者喂原生菜单的勾选/语言，少了它 Linux 上要等 30s 轮询才回正）"
        );
    }

    // ── A2：四态派生（`resolve_tray_state` 的优先级）───────────────────────────────
    //
    // 断言全部走 `crate::tray::resolve_tray_state`（生产装配里 `reconcile_tray_icon` 调的就是它），
    // 不在测试里另写一份判定 —— 否则删掉生产那行 `resolve_tray_state` 测试照样绿。

    #[test]
    fn tray_state_error_is_distinguishable_from_idle() {
        use crate::tray::{resolve_tray_state, TrayState};
        // 这条正是 A2 要修的缺口：`set_error()` 写 running=false + error_code，只发 ERROR 事件。
        // 修之前托盘回读到的就是 running=false ⇒ 与用户主动断开**完全同形**。
        assert_eq!(
            resolve_tray_state(false, false, true),
            TrayState::Error,
            "核崩溃/起核失败必须与主动断开可辨"
        );
        assert_eq!(resolve_tray_state(false, false, false), TrayState::Idle);
    }

    #[test]
    fn tray_state_running_wins_over_stale_error() {
        use crate::tray::{resolve_tray_state, TrayState};
        // `set_nonfatal_error`（如 A1 的 SYSTEM_PROXY_FAILED）在**活核**上留 error_code。
        // 那不是「没连上」，托盘不该翻红叉。
        assert_eq!(resolve_tray_state(true, false, true), TrayState::Connected);
        assert_eq!(resolve_tray_state(true, true, true), TrayState::Connected);
    }

    #[test]
    fn tray_state_starting_wins_over_stale_error() {
        use crate::tray::{resolve_tray_state, TrayState};
        // 新一轮起核已在飞 ⇒ 上一轮的失败不该盖住「正在重试」这个更新的事实。
        assert_eq!(resolve_tray_state(false, true, true), TrayState::Connecting);
        assert_eq!(
            resolve_tray_state(false, true, false),
            TrayState::Connecting
        );
    }

    #[test]
    fn tray_state_four_states_map_to_four_distinct_visuals() {
        use crate::i18n::Lang;
        use crate::tray::TrayState;
        // 变异锁：把某两个态映射到同一张图/同一句 tooltip（例如「connecting 先复用 idle 图标」这种
        // 常见的偷懒实现）必须转红 —— 否则 A2 的「起核中有反馈、错误态可辨」就成了空话。
        let states = [
            TrayState::Idle,
            TrayState::Connecting,
            TrayState::Connected,
            TrayState::Error,
        ];
        let mut tips: Vec<String> = states
            .iter()
            .map(|s| crate::tray::tooltip_text(Lang::ZhCN, *s))
            .collect();
        tips.sort_unstable();
        let n = tips.len();
        tips.dedup();
        assert_eq!(tips.len(), n, "四态 tooltip 必须两两不同");
        // 视觉键同理：TrayVisual 以 state 为键 ⇒ 四态必须产出四个互不相等的键（否则幂等闸门会把
        // 「态变了」误判成「没变」而不重画）。
        let mut keys: Vec<TrayVisual> = states.iter().map(|s| vis(*s, true, Lang::ZhCN)).collect();
        keys.dedup();
        assert_eq!(keys.len(), 4, "四态必须产出四个不同的视觉键");
    }

    // ── 托盘点击所有权：mac/win 直派，Linux/未知平台交给原生菜单 ─────────────────────

    #[test]
    fn tray_interaction_mode_is_direct_only_on_mac_and_windows() {
        assert_eq!(
            tray_interaction_mode(Platform::Mac),
            TrayInteractionMode::DirectClicks
        );
        assert_eq!(
            tray_interaction_mode(Platform::Win),
            TrayInteractionMode::DirectClicks
        );
        assert_eq!(
            tray_interaction_mode(Platform::Linux),
            TrayInteractionMode::NativeMenu
        );
        assert_eq!(
            tray_interaction_mode(Platform::Other),
            TrayInteractionMode::NativeMenu
        );
    }

    #[test]
    fn direct_tray_clicks_toggle_overlay_for_left_and_right() {
        use tauri::tray::{MouseButton, MouseButtonState};

        for platform in [Platform::Mac, Platform::Win] {
            assert!(
                tray_click_toggles_overlay(platform, MouseButton::Left, MouseButtonState::Up),
                "{platform:?} 左键抬起必须切换自绘浮层"
            );
            assert!(
                tray_click_toggles_overlay(platform, MouseButton::Right, MouseButtonState::Up),
                "{platform:?} 右键抬起必须切换自绘浮层"
            );
            assert!(
                !tray_click_toggles_overlay(platform, MouseButton::Middle, MouseButtonState::Up),
                "中键没有产品动作，不得猜测"
            );
            for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
                assert!(
                    !tray_click_toggles_overlay(platform, button, MouseButtonState::Down),
                    "按下帧不得执行，避免 down/up 重复触发"
                );
            }
        }
    }

    #[test]
    fn native_menu_platforms_ignore_all_tray_click_events() {
        use tauri::tray::{MouseButton, MouseButtonState};

        for platform in [Platform::Linux, Platform::Other] {
            for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
                for state in [MouseButtonState::Down, MouseButtonState::Up] {
                    assert!(
                        !tray_click_toggles_overlay(platform, button, state),
                        "{platform:?} 点击归原生菜单所有，不得叠开自绘浮层"
                    );
                }
            }
        }
    }

    // ── A7：原生菜单 id ↔ 动作解析（菜单与 handler 之间唯一的契约面）─────────────────

    #[test]
    fn menu_ids_parse_to_actions() {
        assert_eq!(parse_menu_action("tray_show"), Some(MenuAction::Show));
        assert_eq!(parse_menu_action("tray_quit"), Some(MenuAction::Quit));
        assert_eq!(
            parse_menu_action("tray_toggle"),
            Some(MenuAction::ToggleProxy)
        );
        assert_eq!(
            parse_menu_action("tray_settings"),
            Some(MenuAction::OpenSettings)
        );
        assert_eq!(
            parse_menu_action("tray_check_update"),
            Some(MenuAction::CheckUpdate)
        );
    }

    #[test]
    fn submenu_ids_roundtrip_for_every_declared_value() {
        // **每一个**声明出来的档都必须能解析回去：菜单是按 TAKEOVER_KINDS/ROUTING_MODES 生成 id 的，
        // 少了任何一档的解析 = 那一项点了没反应（且没有任何报错，纯静默）。
        for k in crate::tray::TAKEOVER_KINDS {
            assert_eq!(
                parse_menu_action(&format!("{MENU_ID_TAKEOVER}{k}")),
                Some(MenuAction::Takeover(k)),
                "接管方式 {k} 的菜单项点了会没反应"
            );
        }
        for m in crate::tray::ROUTING_MODES {
            assert_eq!(
                parse_menu_action(&format!("{MENU_ID_ROUTING}{m}")),
                Some(MenuAction::Routing(m)),
                "分流策略 {m} 的菜单项点了会没反应"
            );
        }
    }

    #[test]
    fn unknown_menu_ids_are_rejected_not_guessed() {
        // 载荷必须回查白名单，不能把 id 尾巴透传去写配置（写进 config.proxyMode 的值域由本文件钉死）。
        assert_eq!(parse_menu_action("tray_routing:evil"), None);
        assert_eq!(parse_menu_action("tray_takeover:"), None);
        assert_eq!(parse_menu_action("tray_takeover:TUN"), None, "大小写不放行");
        assert_eq!(
            parse_menu_action("app_quit"),
            None,
            "应用菜单的 id 不该被托盘 handler 认领"
        );
        assert_eq!(parse_menu_action(""), None);
    }

    #[test]
    fn menu_model_gate_repaints_on_every_field_and_only_then() {
        use crate::i18n::Lang;
        let base = TrayMenuModel {
            running: false,
            mode: "smart".into(),
            mode_type: "systemProxy".into(),
            lang: Lang::ZhCN,
        };
        // 未变 → 不重建（GTK 每次 set_menu 重建整棵 widget 树；菜单开着时重建会闪/收起）。
        let mut cache = Some(base.clone());
        let mut called = false;
        reconcile_tray_menu_model(&mut cache, base.clone(), |_| {
            called = true;
            true
        });
        assert!(!called, "模型未变不得重建菜单");

        // 四个字段逐个变 → 每一个都必须触发重建（漏比某字段 = 菜单显示陈旧且无人发现）。
        for (why, next) in [
            (
                "running（连接项文案）",
                TrayMenuModel {
                    running: true,
                    ..base.clone()
                },
            ),
            (
                "mode（分流勾选）",
                TrayMenuModel {
                    mode: "global".into(),
                    ..base.clone()
                },
            ),
            (
                "mode_type（接管勾选）",
                TrayMenuModel {
                    mode_type: "tun".into(),
                    ..base.clone()
                },
            ),
            (
                "lang（全部项文案）",
                TrayMenuModel {
                    lang: Lang::EnUS,
                    ..base.clone()
                },
            ),
        ] {
            let mut cache = Some(base.clone());
            let mut called = false;
            reconcile_tray_menu_model(&mut cache, next, |_| {
                called = true;
                true
            });
            assert!(called, "{why} 变了必须重建菜单");
        }
    }

    #[test]
    fn menu_model_gate_invalidates_cache_when_apply_fails() {
        use crate::i18n::Lang;
        // 失败照存 = 之后每一轮都短路、再也不重试 —— 自愈网被自己的缓存关掉（同 reconcile_tray_visual）。
        let mut cache = None;
        let next = TrayMenuModel {
            running: true,
            mode: "smart".into(),
            mode_type: "tun".into(),
            lang: Lang::EnUS,
        };
        reconcile_tray_menu_model(&mut cache, next.clone(), |_| false);
        assert_eq!(cache, None, "装载失败必须作废缓存，下一轮无条件重建");
        reconcile_tray_menu_model(&mut cache, next.clone(), |_| true);
        assert_eq!(cache, Some(next), "成功才记账");
    }
}
