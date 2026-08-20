//! 托盘上下文自定义 HTML 浮层（`.tray-menu` 原型的独立 WebviewWindow 宿主）。
//!
//! macOS/Windows 以它替代原生上下文菜单：托盘左键或右键（macOS 双指辅助点按归为右键）都弹出/收起这个
//! 独立窗口渲染的自绘浮层（连接状态卡 + 断开/连接 + 节点切换 + 模式 + 打开主窗 + 退出）。
//! 主窗只由浮层内的明确入口唤出。Linux AppIndicator 不派发可靠点击事件，仍由 `main.rs` 保留完整原生菜单兜底。
//!
//! # 窗口形态（对齐 `runtime::update_popup` 的独立 mini 窗模式）
//!
//! - 独立 `label`（[`TRAY_LABEL`]）+ 独立页面入口（`tray.html`），**不复用主窗 `index.html`**——
//!   否则整个 React 主应用（i18n/路由/全部 provider）会挂进这个小浮层，且主窗白屏自愈门
//!   （`window_health.rs` 只认 `label=="main"`）会对着浮层误判。
//! - frameless（`decorations:false`）+ `always_on_top` + `skip_taskbar` + 不可 resize + 初始 hidden。
//! - **透明**仅 mac/win（配合卡片圆角 + 1px 边框 + 贴菜单栏 native 间隙，无箭头「面板风」、无阴影）；**Linux 恒不透明**（透明窗在无合成器/部分 WM 下
//!   =黑块或鼠标穿透，与主窗 `transparent:false` 白屏逃生门同一顾虑）——用卡片 surface 同色实底兜底，
//!   方角可接受（对齐主窗「Linux 方窗 + 前端小圆角」既定取舍）。
//!
//! # 生命周期
//!
//! 首次托盘点击只登记展示意图并跳出事件帧，随后 [`build_overlay`] 按需创建 → renderer-ready 后
//! 定位+显示+聚焦（再点=隐藏）→
//! 点窗外/切他 app 收起：Rust `Focused(false)` + DOM `window.blur`→`tray_hide` **双路** dismiss（后者
//! 经 `initialization_script` 注入，兜 mac 上次级窗 Focused 递送不可靠，见 [`TRAY_BLUR_DISMISS_JS`]）。
//! `keepTrayMenuWarm` 默认开启：日常隐藏只收起、不自动回收，换取后续点击热开；用户关闭后，隐藏超过
//! [`TRAY_IDLE_RECLAIM_SECS`] 才销毁 WebView。此偏好与主窗口 `autoLightweightMode` 完全独立：主窗进入
//! 轻量态只释放主 WebView，不替用户改变托盘 renderer 的驻留选择。
//!
//! # 与主进程的契约（专用 command 均薄封装，供浮层 React 端 invoke）
//!
//! - [`tray_renderer_ready`]：React 首次 commit 后携冷建代次回执，只有当前代 renderer 可触发展示。
//! - [`tray_resize`]：浮层量出内容高度后回报 → 主进程设窗高（宽固定）并重定位（自适应高）。
//! - [`tray_hide`]：连接/断开/切节点后收起浮层（原生菜单选项即关的等价）。
//! - [`tray_show_main`]：显示主窗（打开主窗口/在主窗口管理）——复用 `crate::show_main_window`。
//! - [`tray_quit`]：置 `QuitState` + `app.exit(0)`——与 `main.rs` 托盘/菜单「退出」路径逐字节相同。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{
    AppHandle, LogicalSize, Manager, PhysicalPosition, PhysicalSize, Position, Size, WebviewUrl,
    WebviewWindowBuilder, WindowEvent,
};

use crate::i18n::{app_lang, key, t, Lang};
use crate::response::{ok_void, ApiResponse};

// ── 托盘原生文案（Linux 原生菜单 + 三平台 tooltip）────────────────────────────────
//
// 浮层（webview）文案走前端 `labels.ts`（`i18n/auxiliary.ts` 的键查找，`locales/auxiliary/*.json` 的 `tray.*`）；
// **原生**托盘图标 tooltip 与 Linux 兜底菜单在 Rust 侧构建，前端 i18n 够不着 —— 故本模块经
// [`crate::i18n`] 读**同一批 `tray.*` 键**：同一个字符串，两个入口想分叉都分叉不了
// （此前靠一句「文案与浮层逐字一致」的散文约束守着）。语言真值源同为 `config.language`，
// 见 [`crate::i18n::app_lang`]。
//
// 2026-07-31 之前这里是一张 zh/en **二态**表（旧 `TrayLang` + `native_menu_*` 一族）：产品出
// 5 语种，俄语 / 波斯语 / 繁中用户的原生菜单与 tooltip 一律落英文（繁中还落简体的对立面——英文）。
// 现已随 [`crate::i18n::Lang`] 五语齐备，那一族常量包装函数随之删除：它们每个只是
// `match lang { Zh => "…", En => "…" }`，改成键查找后再留一层转发没有信息量，
// 调用点直接写 `i18n::t(lang, key::TRAY_X)` 反而让「这条文案是哪个键」在现场可见。

// ── 托盘四态（图标 / tooltip / 浮层状态点共用的单一状态轴）──────────────────────
//
// 此前托盘只有 `connected: bool` 二态（`main.rs::set_tray_connected`），对齐 上游
// `TrayManager.ts:54` 的 `TrayIconState = 'idle' | 'connected' | 'connecting'` 缺一态；且 上游的
// `TrayMenuData.hasError`（`TrayManager.ts:58/265`）在 Polaris 侧完全没有对应物 —— `main.rs` 收到
// `EVENT_PROXY_ERROR` 只是叫醒汇流点，汇流点回读 `running=false` ⇒ **崩溃与用户主动断开在托盘上完全同形**。
// 本枚举把两个缺口一次补齐：起核中可见反馈、异常终态可辨。

/// 托盘视觉状态（四态）。图标形态与 tooltip 都由它单点决定。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayState {
    /// 未连接（用户主动断开 / 从未启动）。
    Idle,
    /// 起核腿在飞（`ProxyStatus.starting`，重试预算内可达数十秒）。
    Connecting,
    /// 核在跑。
    Connected,
    /// 异常终态：核崩溃 / 起核失败（`ProxyStatus.error` 有值且核未跑），**非**用户主动断开。
    Error,
}

// ── 为什么原生图标**没有**浮层那个 `degraded` 第五态（2026-07-28 复审 LOW-2，如实登记）────
//
// 浮层的 `trayStatusTone`（`ui/src/tray/tray-status-tone.ts`）比本枚举多一个 `degraded`
// （核在跑但 systemProxy 被手改 ⇒ 流量没经核）。原生图标**刻意不跟进**，登记在此免得被反复重开。
//
// # 拦路的不是「加一个 enum 分支」，是这个位的真值从哪来
//
// 判据是 `system_proxy_get_status` 的 `pointsToUs` —— 它**只能**靠 exec
// `networksetup` / `gsettings` / `reg` 现查（无内核事件、无缓存，`commands/proxy.rs` 每次调用现造
// 一次性 ops）。而图标汇流点 `main.rs::reconcile_tray_icon` 的立身之本正是「一次 `RwLock` 读快照、
// 无 IO 无 syscall」，故可以被四个事件源 + 30s 轮询随便叫醒。往那条腿里塞 exec ⇒ 每次代理状态
// 变化、每次 configChanged 都拖一次子进程，代价与收益完全不成比例（这条已在上一批复审判过）。
//
// # 那「让前端把已取到的活态回传给 Rust」呢——**也不成立**，理由是数据到不了现场
//
// 前端确实已经有一份活态（`ui/src/store/use-system-proxy-live.ts`），但它的两个产出点都覆盖不到
// 「需要看图标」的那个时刻：
//  - **主窗轮询**（15s 一发）硬门控在 `document.visibilityState === 'visible'`，隐藏即整条链停摆
//    （连 timer 都不留）。而主窗关闭 = `hide()` 收进托盘是本应用的默认关窗语义
//    （`main.rs::resolve_close_action` + `config.minimizeToTray`）⇒ 托盘图标成为唯一状态面的场景，
//    恰恰就是该轮询**一发都不发**的场景。
//  - **浮层 hydrate**（弹出即取一发）发生在用户已经打开浮层之后，而浮层自己那颗点此刻就显示着
//    正确结论 ⇒ 图标晚一步翻过来，对用户是零增量信息。
// 即：这条链能让图标「在浮层/主窗已经说了实话的时候跟着说一遍」，唯独不能在只剩图标的时候说话。
// 代价却是一条新的跨窗状态推送通道（新 command + 契约 + 前端接线）**外加一套陈旧度策略**
// （报文停了之后那个位算真还是算假、多久算陈旧——没有原则性的取值）。
//
// # tooltip 先行也不是更便宜的路
//
// tooltip 与图标共用同一个 `TrayVisual` 输入，缺的同样是那个位的真值 ⇒ 数据源的代价一分不少，
// 只省下了「换个图标」这点几乎为零的成本；且 Linux appindicator 根本没有 tooltip
// （`set_tooltip` 直接返 Ok = no-op），缺口最大的平台一点都补不到。
//
// # 结论与现状
//
// 判定为**不做**：现有形态下没有一条路径能把这个位在「需要它」的时刻送到图标上。缺口不是没有代偿——
// 主窗状态栏琥珀点 + 首页降级横幅 + 托盘浮层琥珀点三处都已如实呈现，独缺不接受输入的原生图标那一格。
// 真要补，前置条件是**后端自己有一份低成本的活态**（例如系统代理接管腿在写侧维护一个带 TTL 的
// `points_to_us` 缓存，由已有的接管/重申动作顺带刷新），那时本枚举加第五态才只是「加一个分支」。
// 在那之前，`reconcile_tray_icon` 的无 IO 不变式由 `main.rs` 的
// `tray_icon_reconcile_stays_io_free` 守住 —— 防的就是有人为了补这一格把 exec 塞进图标腿。

/// 由 proxy 状态快照的三个位折出托盘状态（纯函数，可单测）。
///
/// 优先级 **Connected > Connecting > Error > Idle**，每一级都有理由：
/// - `running` 压过一切：核确实在跑时，任何陈旧的 error 字段都不该把托盘打成红叉（`set_nonfatal_error`
///   会在**活核**上留 `error`，如 A1 的 `SYSTEM_PROXY_FAILED` —— 那不是「没连上」）。
/// - `starting` 压过 `errored`：新一轮起核已经在飞，上一轮的失败不该盖住「正在重试」这个更新的事实。
/// - `errored` 压过 Idle：这正是本轮要补的那条边（崩溃腿此前与主动断开同形）。
#[must_use]
pub fn resolve_tray_state(running: bool, starting: bool, errored: bool) -> TrayState {
    if running {
        TrayState::Connected
    } else if starting {
        TrayState::Connecting
    } else if errored {
        TrayState::Error
    } else {
        TrayState::Idle
    }
}

/// 托盘图标 tooltip 文案（随状态动态刷新；tauri.conf 静态 "Polaris" → hover 恒固定的替代）。
///
/// Linux appindicator 无 tooltip（`tray-icon` gtk 后端 `set_tooltip` 直接返 Ok）→ 那里状态全靠图标形态
/// 与原生菜单，故错误态**必须**在图标上可辨，不能只写进 tooltip（见 [`TrayState`]）。
pub fn tooltip_text(lang: Lang, state: TrayState) -> String {
    // `Polaris — <状态>`：品牌名不进 locale（五语种同名），分隔符是排版而非文案。
    // fa（RTL）下的左右次序由系统 bidi 算法定，不在此处硬编码方向。
    format!("Polaris — {}", t(lang, tooltip_status_key(state)))
}

/// 四态 → `tray.status*` 键。与浮层状态卡取同一批键（同一状态两个入口不得措辞分叉）。
///
/// 浮层比这里多一个 `statusProxyInactive`（degraded 第五态），原生图标刻意不跟进 —— 理由见
/// [`TrayState`] 上方那段登记。
#[must_use]
pub fn tooltip_status_key(state: TrayState) -> &'static str {
    match state {
        TrayState::Connected => key::TRAY_STATUS_CONNECTED,
        TrayState::Connecting => key::TRAY_STATUS_CONNECTING,
        TrayState::Error => key::TRAY_STATUS_ERROR,
        TrayState::Idle => key::TRAY_STATUS_DISCONNECTED,
    }
}

// ── 原生兜底菜单文案（A7：Linux 不递送点击事件时，这是唯一够得着功能面的入口）──────────
//
// Tauri 的 AppIndicator 后端不支持切换菜单点击键，且明确不派发 Linux `TrayIconEvent` ⇒ Linux 用户
// 只能依赖桌面宿主展示的原生菜单。
// 它此前只有「显示 / 退出」两项 ⇒ 模式、接管方式、节点、连接开关**全部够不着**。
// 故原生菜单必须自带完整功能面（对齐 上游 `TrayManager.ts:392-441` 的 contextMenu 项集）。
//
// 菜单项文案由 `main.rs::build_tray_menu` 直接 `i18n::t(lang, key::TRAY_*)` 取；只有下面两个
// **值 → 键**的映射留在此处（它们有真实逻辑：`config` 里的取值域要对到显示序与文案上）。

/// 接管方式三档的文案键。`kind` 取 [`TAKEOVER_KINDS`] 之一（`config.proxyModeType` 值域）。
/// 与浮层 `TrayMenu.tsx` 的 `TAKEOVERS` 表**共用同一批键**（不再是「靠人守的逐字一致」）。
#[must_use]
pub fn takeover_key(kind: &str) -> &'static str {
    match kind {
        "tun" => key::TRAY_TAKEOVER_TUN,
        "manual" => key::TRAY_TAKEOVER_MANUAL,
        _ => key::TRAY_TAKEOVER_SYSTEM_PROXY,
    }
}

/// 分流策略三档的文案键。`mode` 取 [`ROUTING_MODES`] 之一（`config.proxyMode` 值域）。
/// 与浮层 `TrayMenu.tsx` 的 `MODES` 表共用同一批键。
#[must_use]
pub fn routing_key(mode: &str) -> &'static str {
    match mode {
        "global" => key::TRAY_MODE_GLOBAL,
        "direct" => key::TRAY_MODE_DIRECT,
        _ => key::TRAY_MODE_SMART,
    }
}

/// `config.proxyModeType` 值域（顺序 = 菜单显示序，与浮层 `TAKEOVERS` 同序）。
pub const TAKEOVER_KINDS: [&str; 3] = ["systemProxy", "tun", "manual"];
/// `config.proxyMode` 值域（顺序 = 菜单显示序，与浮层 `MODES` 同序）。
pub const ROUTING_MODES: [&str; 3] = ["smart", "global", "direct"];

// ── 跨窗导航（A1「打开设置」）────────────────────────────────────────────────────
//
// # 选型：给 `tray_show_main` 加**受限**目标屏参数 + 一条窄事件，而不是复活 `EVENT_NAVIGATE`
//
// `events.rs:66-72` 已把 上游的 `navigate` 通道删净并写明理由（Polaris 托盘是同源 webview 浮层，
// 自己渲染子视图，**没有**任何路径需要「跨窗令主窗跳到第 N 屏」）。「打开设置」是那条论证的**唯一反例**：
// 设置屏在主窗里，浮层里没有也不该有。但反例只有一个 ⇒ 不该为它重开一条**任意字符串路由**的通用通道
// （那正是 上游 那条通道会长出 `/server` `/settings` `/logs` 一堆消费点、最后没人说得清谁在用的成因）。
//
// 故取窄形态：
//  1. 复用既有 `tray_show_main` command（不新增 command），加一个 `Option<String>` 目标屏参数——
//     缺省不传 = 今天的行为逐字节不变（既有 `invoke('tray_show_main')` 调用点零改动）。
//  2. 参数经 [`normalize_tray_screen`] **白名单**归一，只有登记过的屏名才会被发出去；未知值一律降级为
//     「只显示主窗、不导航」——通道的值域由 Rust 侧枚举钉死，不是「前端传什么就发什么」。
//  3. 事件 `EVENT_TRAY_OPEN_SCREEN` 单播给主窗（`emit_to_main`），不广播。
//
// 想加第二个目标屏必须同时改白名单 + 补测试，成本恰好落在该落的地方。

/// 托盘可导航的目标屏**白名单**（纯函数，可单测）。
///
/// 返回 `'static` 串而非透传入参：发出去的值域被本函数钉死 ⇒ 前端传任意字符串也只能命中登记项，
/// 通道不会退化成通用路由。当前只有 `settings` 一项（A1「打开设置」）。
#[must_use]
pub fn normalize_tray_screen(screen: &str) -> Option<&'static str> {
    match screen.trim() {
        "settings" => Some("settings"),
        _ => None,
    }
}

// ── 原生面主题（B：后端此前零读 `uiTheme`）──────────────────────────────────────

/// 浅色 / 深色的**窗口背景色**（原生面用，webview 首帧之前就已经在屏上）。
///
/// 取值 = `ui/src/styles/tokens.css` 的 `--bg`（深 `220 40% 6%` = #0B0F14，与 `tauri.conf.json` 主窗
/// `backgroundColor` 同值；浅 `210 30% 96%` ≈ #F2F5F8）。
#[must_use]
pub fn window_bg_color(dark: bool) -> tauri::window::Color {
    if dark {
        tauri::window::Color(0x0B, 0x0F, 0x14, 0xFF)
    } else {
        tauri::window::Color(0xF2, 0xF5, 0xF8, 0xFF)
    }
}

/// 浅色 / 深色的**卡片面背景色**（托盘浮层 Linux 实底 / 更新弹窗防白闪）。
/// = tokens 的 `--surface`（深 #161C24，沿用既有取值；浅 #FFFFFF）。
#[must_use]
pub fn surface_color(dark: bool) -> tauri::window::Color {
    if dark {
        tauri::window::Color(0x16, 0x1C, 0x24, 0xFF)
    } else {
        tauri::window::Color(0xFF, 0xFF, 0xFF, 0xFF)
    }
}

/// `config.uiTheme` + 系统明暗 → 原生面该用深色吗（纯函数，与前端 `tray-theme.ts::resolveDark`
/// 逐分支同构：显式 light/dark 直接定，其余跟随系统）。
///
/// `os_dark` 为 `None`（拿不到系统明暗，见 [`os_dark`]）时回落 **true** —— tokens 默认深色，
/// 且这正是本改动之前的既有行为，取不到信号时不制造新的观感跳变。
#[must_use]
pub fn resolve_native_dark(ui_theme: Option<&str>, os_dark: Option<bool>) -> bool {
    match ui_theme.map(str::trim) {
        Some("dark") => true,
        Some("light") => false,
        _ => os_dark.unwrap_or(true),
    }
}

/// 读 `config.uiTheme`（`ConfigManager` 缓存，与 [`crate::i18n::app_lang`] 同款便宜读）。
pub fn ui_theme(app: &AppHandle) -> Option<String> {
    app.try_state::<crate::runtime::AppRuntime>()
        .and_then(|rt| rt.config().current().ok())
        .and_then(|c| c.get("uiTheme").and_then(Value::as_str).map(str::to_string))
}

/// 系统明暗探测。**Tauri 2.11 没有 app 级 theme getter**（只有 `Window::theme()`，见
/// `tauri-runtime/src/lib.rs:787`），故只能借任一现存窗口去问 OS。
///
/// 按 主窗 → 托盘浮层 → 更新弹窗 顺序探（C16 轻量模式**销毁**主窗后仍能从浮层拿到答案）；
/// 一个窗都没有（首建主窗之前）→ `None`，由 [`resolve_native_dark`] 回落深色。
///
/// 与 `main.rs::set_tray_connected` 里那份 `dark_bg` 探测**刻意不合并**：那问的是「**任务栏**底色深浅」
/// （决定托盘图标用黑变体还是白变体），本函数问的是「**UI 主题**该深该浅」。两者今天都由系统明暗回答，
/// 但语义不同轴——合并会让「以后想让托盘图标跟任务栏、UI 跟 uiTheme」这类分叉无处落脚。
pub fn os_dark(app: &AppHandle) -> Option<bool> {
    for label in [
        "main",
        TRAY_LABEL,
        crate::runtime::update_popup::POPUP_LABEL,
    ] {
        if let Some(w) = app.get_webview_window(label) {
            if let Ok(t) = w.theme() {
                return Some(t == tauri::Theme::Dark);
            }
        }
    }
    None
}

/// `config.uiTheme` + 现存窗口探到的系统明暗 → 原生面深色否（[`resolve_native_dark`] 的 app 侧薄封装）。
pub fn native_dark(app: &AppHandle) -> bool {
    resolve_native_dark(ui_theme(app).as_deref(), os_dark(app))
}

/// 主窗 FOUC 预解析脚本（`initialization_script` 注入，**先于页面任何脚本、且不受页面 CSP
/// `script-src 'self'` 限制** —— 与 [`TRAY_BLUR_DISMISS_JS`] / `update_popup` 的 `init_script` 同款手法）。
///
/// # 为什么必须是注入脚本，而不是 `index.html` 里的内联 `<script>`
///
/// `ui/index.html` 的 CSP 是 `script-src 'self'`，内联脚本会被直接拦掉；放宽成 `'unsafe-inline'`
/// 为一句主题赋值换掉整页的脚本注入防线，不划算。而**能同步读到 `uiTheme` 真值的只有主进程**
/// （它在 config.json 里，前端拿到它已经是 IPC 之后、第一帧早过去了）——真值源与执行时机在这里天然重合。
///
/// # 语义：只**播种**，不接管
///
/// `hasAttribute` 守卫 ⇒ 属性已存在就不写。`AppShell.tsx` 的主题 effect 才是运行期真值的持有者
/// （用户在设置里改主题即时生效走它），本脚本只负责把「第一帧之前」这段空窗填上，绝不与它抢。
#[must_use]
pub fn theme_boot_script(dark: bool) -> String {
    let theme = if dark { "dark" } else { "light" };
    format!(
        r#"(function () {{
  var t = '{theme}';
  window.__POLARIS_INITIAL_THEME__ = t;
  function apply() {{
    var el = document.documentElement;
    if (el && !el.hasAttribute('data-theme')) el.setAttribute('data-theme', t);
  }}
  apply();
  document.addEventListener('readystatechange', apply);
  document.addEventListener('DOMContentLoaded', apply);
}})();
"#
    )
}

// ── FakeIP-TUN 待纠正快照（A7 原生菜单切接管方式要用）────────────────────────────

/// 消费「FakeIP-TUN 待纠正」快照 —— `ui/.../home/fakeip-tun-entry.ts::applyFakeIpTunEntry` 的 Rust 同构体。
///
/// 仅当目标模式为 `tun` 且 `dnsConfig.fakeIpTunAutoEnable === true` 时，把迁移期冻结的
/// `enableFakeIp:false` 回 `true` 并**一次性消费** flag（置 false）；其余一律不动。
/// flag 由 `crates/store/src/migrate.rs::migrate_fake_ip_tun_pending` 写入。
///
/// # 为什么要在 Rust 侧也有一份
///
/// 浮层与主窗切接管方式走前端那份；**原生兜底菜单**（A7，Linux 左键不递送时的唯一入口）在 Rust 侧
/// 落盘，够不着前端函数。若这条腿直接写 `proxyModeType` 而跳过纠正，Linux 用户从原生菜单进 TUN 就会
/// 带着 `enableFakeIp:false` 起核 —— 与另两个入口行为分叉。两份实现由**同一组用例**钉住（本模块 tests
/// 与 `fakeip-tun-entry` 的前端单测覆盖同样的四个分支）。
///
/// 返回 `true` 表示**真把 false 改成了 true**（供调用方决定要不要告知用户；flag 开着但值本就是 true
/// 时只消费 flag、返 false）。
pub fn apply_fake_ip_tun_entry(config: &mut Value) -> bool {
    let mode_type = config
        .get("proxyModeType")
        .and_then(Value::as_str)
        .unwrap_or("systemProxy")
        .to_ascii_lowercase();
    if mode_type != "tun" {
        return false;
    }
    let pending = config
        .get("dnsConfig")
        .and_then(|d| d.get("fakeIpTunAutoEnable"))
        .and_then(Value::as_bool)
        == Some(true);
    if !pending {
        return false;
    }
    let Some(dns) = config.get_mut("dnsConfig").and_then(Value::as_object_mut) else {
        return false; // 上面已确认 dnsConfig 里有该 flag ⇒ 不可达；防御式返回，不 panic
    };
    let corrected = dns.get("enableFakeIp").and_then(Value::as_bool) == Some(false);
    if corrected {
        dns.insert("enableFakeIp".into(), Value::Bool(true));
    }
    dns.insert("fakeIpTunAutoEnable".into(), Value::Bool(false));
    corrected
}

/// 浮层窗 label（Tauri 内唯一；主窗为 `"main"`，更新弹窗为 `"update-popup"`）。
pub const TRAY_LABEL: &str = "tray";

/// 浮层页面入口（vite 多入口产物；dev 态由 devUrl 提供 `/tray.html`）。
const TRAY_PAGE: &str = "tray.html";

/// 浮层「点击窗外即收起」的**替代 dismiss**（defect#3a）。
///
/// 根因：mac 上这个 frameless/辅助窗的 Rust 侧 `WindowEvent::Focused(false)` 递送不可靠（Tauri 已知
/// 类：次级窗口 Focused 事件在 macOS 偶发不触发）→ 只靠它则点窗外不收。DOM 层的 `window.blur` 由
/// WKWebView 在宿主 NSWindow resignKey 时可靠派发（与 `TrayMenu` 已依赖的 `focus` 事件对称）→ 作独立
/// 兜底：失焦即 invoke `tray_hide`（内含 `mark_hidden`，与图标点击去抖一致，不会闪关又弹回）。
///
/// 走 `initialization_script`（主进程注入，先于页面脚本、不受页面 CSP `script-src` 限制；与
/// `update_popup` 同款注入手法）——故**无需**改前端 TS。防御式取 `__TAURI_INTERNALS__.invoke`
/// （Tauri v2 注入的 IPC 桥；缺失即静默不动，非 Tauri 预览态不报错）。
const TRAY_BLUR_DISMISS_JS: &str = r#"
(function () {
  window.addEventListener('blur', function () {
    try {
      var i = window.__TAURI_INTERNALS__;
      if (i && typeof i.invoke === 'function') { i.invoke('tray_hide'); }
    } catch (e) {}
  });
})();
"#;

/// 浮层窗逻辑宽度（固定；高度由前端量内容后经 [`tray_resize`] 自适应）。
/// 卡片 `.tray-menu` 宽 246 + 浮层 CSS 左右各 ~11 外边距（让圆角/1px 边框不贴窗沿被裁）≈ 268。
const TRAY_WIDTH: f64 = 268.0;

/// 浮层「刚被隐藏」的去抖窗口：托盘图标点击会先让浮层失焦（→ 自动隐藏），
/// 若紧接着的 Click 事件在此窗口内到达，视为「点击图标关闭」，不再重开（否则闪一下又弹回）。
const REOPEN_DEBOUNCE_MS: u128 = 300;

/// 用户关闭 `keepTrayMenuWarm` 后，托盘浮层隐藏至此时限才自动回收。
const TRAY_IDLE_RECLAIM_SECS: u64 = 120;

/// 应用级偏好键：true/缺失 = 日常隐藏后保持 WebView warm（默认）；false = 120s 后冷态回收。
const KEEP_TRAY_MENU_WARM_KEY: &str = "keepTrayMenuWarm";

/// 冷建后 renderer 最晚应回报 ready 的时间。超时不把空壳漏给用户，而是回收这次坏实例；下一次点击
/// 可重新创建。正常真机冷建约 237ms，这里留出数量级余量只兜白屏/IPC 断路，不参与日常体验时序。
const TRAY_READY_TIMEOUT_SECS: u64 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayOpenAction {
    ShowNow,
    AwaitReady,
    QueueBuild { generation: u64 },
}

/// 托盘浮层的冷建状态机。所有字段都在同一把锁下迁移，避免「窗口已建」与「renderer 已就绪」分别用
/// 原子量表示时读到撕裂组合。`generation` 隔离被销毁旧 WebView 的迟到 ready 回执。
#[derive(Default)]
struct OverlayLifecycle {
    generation: u64,
    build_queued: bool,
    renderer_ready: bool,
    show_requested: bool,
}

impl OverlayLifecycle {
    fn request_open(&mut self, window_exists: bool) -> OverlayOpenAction {
        self.show_requested = true;
        if window_exists && self.renderer_ready {
            OverlayOpenAction::ShowNow
        } else if window_exists || self.build_queued {
            OverlayOpenAction::AwaitReady
        } else {
            self.generation = self.generation.wrapping_add(1);
            self.build_queued = true;
            self.renderer_ready = false;
            OverlayOpenAction::QueueBuild {
                generation: self.generation,
            }
        }
    }

    fn build_finished(&mut self, generation: u64, success: bool) {
        if self.generation != generation {
            return;
        }
        self.build_queued = false;
        if !success {
            self.show_requested = false;
        }
    }

    fn mark_ready(&mut self, generation: u64) -> bool {
        if self.generation != generation {
            return false;
        }
        self.renderer_ready = true;
        self.show_requested
    }

    fn should_show(&self, generation: u64) -> bool {
        self.generation == generation && self.renderer_ready && self.show_requested
    }

    fn hide(&mut self) {
        self.show_requested = false;
    }

    fn reset(&mut self) {
        // 让旧 renderer 的迟到 ready 回执失效；新冷建再递增一次不影响语义。
        self.generation = self.generation.wrapping_add(1);
        self.build_queued = false;
        self.renderer_ready = false;
        self.show_requested = false;
    }
}

#[derive(Clone, Copy)]
struct OverlayOpenProbe {
    started: Instant,
    cold: bool,
}

/// 浮层运行期状态（app-managed）：记录最近一次隐藏时刻（供 [`toggle_overlay`] 去抖）+ 最近一次
/// 托盘图标屏幕矩形（供 [`reposition`] 对齐图标；[`tray_resize`] 改高后重定位也复用它）。
pub struct TrayOverlay {
    last_hidden: Mutex<Option<Instant>>,
    anchor: Mutex<Option<PhysicalRect>>,
    lifecycle: Mutex<OverlayLifecycle>,
    /// 点击到 renderer-ready / show 的真机时延探针。只记录运行期指标，不参与状态机判定。
    open_probe: Mutex<Option<OverlayOpenProbe>>,
    /// A1「打开设置」的**首帧种子腿**：主窗已被 C16 轻量模式销毁时，目标屏存在这里，等
    /// `create_main_window` 重建时注入首帧脚本（事件腿此刻必丢，见 [`tray_show_main`]）。
    /// `'static` 串 = [`normalize_tray_screen`] 的白名单产物。
    pending_screen: Mutex<Option<&'static str>>,
    /// 隐藏回收任务代次：每次 show/hide/destroy 都递增，过期任务只在代次仍匹配时销毁窗口。
    reclaim_generation: AtomicU64,
    /// `config.keepTrayMenuWarm` 的运行期镜像。只由启动同步与 CONFIG_CHANGED 事件更新；hide 热路径
    /// 直接读原子值，不为一次菜单收起克隆整份配置。
    keep_warm: AtomicBool,
    /// mac 全局鼠标按下监听器（NSEvent global monitor）句柄的**原始指针地址**（defect#3）。存 `usize`
    /// 而非 `Retained<AnyObject>`：后者 `!Send`，进不了 Tauri app-managed state（要求 `Send + Sync`）；
    /// monitor 仅在主线程 add/remove，跨线程只传指针地址是安全的。`None` = 未装。
    /// 见 [`install_click_monitor`] / [`remove_click_monitor`]。
    #[cfg(target_os = "macos")]
    click_monitor: Mutex<Option<usize>>,
}

impl Default for TrayOverlay {
    fn default() -> Self {
        Self {
            last_hidden: Mutex::default(),
            anchor: Mutex::default(),
            lifecycle: Mutex::default(),
            open_probe: Mutex::default(),
            pending_screen: Mutex::default(),
            reclaim_generation: AtomicU64::default(),
            // 与 store 的缺省值同口径；启动同步尚未执行时也不能短暂排下一条冷态回收任务。
            keep_warm: AtomicBool::new(true),
            #[cfg(target_os = "macos")]
            click_monitor: Mutex::default(),
        }
    }
}

/// 托盘图标的屏幕物理矩形（左上角 + 尺寸）。`TrayIconEvent::Click` 的 `rect` 原样存这里。
/// `tray-icon` 的三平台事件契约均为物理坐标；尤其 Windows 源自 `Shell_NotifyIconGetRect`，不能再拿
/// 浮层窗当前屏的 DPI 猜一次转换比例。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicalRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// 一块显示器上的物理像素区域，边界采用左闭右开/上闭下开。窗口定位只认它，不把「整屏」与
/// 「扣掉任务栏/Dock/菜单栏后的工作区」混在同一组 `(position, size)` 参数里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScreenArea {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl ScreenArea {
    fn new(position: &PhysicalPosition<i32>, size: &PhysicalSize<u32>) -> Self {
        Self {
            left: position.x,
            top: position.y,
            right: position.x.saturating_add(size.width as i32),
            bottom: position.y.saturating_add(size.height as i32),
        }
    }

    fn is_usable(self) -> bool {
        self.right > self.left && self.bottom > self.top
    }
}

/// 托盘所在的系统边缘；浮层朝相反方向（工作区内部）展开。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayEdge {
    Top,
    Bottom,
    Left,
    Right,
}

impl TrayEdge {
    fn attr(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct OverlayPlacement {
    work_area: ScreenArea,
    edge: TrayEdge,
    scale_factor: f64,
}

fn physical_tray_rect(rect: tauri::Rect) -> Option<PhysicalRect> {
    let (Position::Physical(p), Size::Physical(s)) = (rect.position, rect.size) else {
        return None;
    };
    Some(PhysicalRect {
        x: f64::from(p.x),
        y: f64::from(p.y),
        w: f64::from(s.width),
        h: f64::from(s.height),
    })
}

fn store_anchor(app: &AppHandle, rect: tauri::Rect) {
    let Some(rect) = physical_tray_rect(rect) else {
        return;
    };
    if let Some(state) = app.try_state::<TrayOverlay>() {
        if let Ok(mut guard) = state.anchor.lock() {
            *guard = Some(rect);
        }
    }
}

fn anchor(app: &AppHandle) -> Option<PhysicalRect> {
    app.try_state::<TrayOverlay>()
        .and_then(|state| state.anchor.lock().ok().and_then(|g| *g))
}

fn mark_hidden(app: &AppHandle) {
    if let Some(state) = app.try_state::<TrayOverlay>() {
        if let Ok(mut guard) = state.last_hidden.lock() {
            *guard = Some(Instant::now());
        }
    }
}

fn recently_hidden(app: &AppHandle) -> bool {
    app.try_state::<TrayOverlay>()
        .and_then(|state| state.last_hidden.lock().ok().and_then(|g| *g))
        .is_some_and(|t| t.elapsed().as_millis() < REOPEN_DEBOUNCE_MS)
}

fn begin_open_probe(app: &AppHandle, cold: bool) {
    if let Some(state) = app.try_state::<TrayOverlay>() {
        if let Ok(mut probe) = state.open_probe.lock() {
            probe.get_or_insert(OverlayOpenProbe {
                started: Instant::now(),
                cold,
            });
        }
    }
}

fn clear_open_probe(app: &AppHandle) {
    if let Some(state) = app.try_state::<TrayOverlay>() {
        if let Ok(mut probe) = state.open_probe.lock() {
            *probe = None;
        }
    }
}

fn log_open_probe(app: &AppHandle, stage: &str, take: bool) {
    let probe = app.try_state::<TrayOverlay>().and_then(|state| {
        state
            .open_probe
            .lock()
            .ok()
            .and_then(|mut probe| if take { probe.take() } else { *probe })
    });
    if let Some(probe) = probe {
        log::info!(
            "托盘浮层时延: stage={stage}, cold={}, elapsed_ms={}",
            probe.cold,
            probe.started.elapsed().as_millis()
        );
    }
}

/// 统一收起浮层：隐藏窗口 + 记隐藏时刻（去抖）+（mac）拆掉全局点击监听器。所有「收起」入口
/// （Focused(false) / 点图标 toggle / tray_hide / tray_show_main / tray_enter_lightweight / 全局 monitor
/// handler）都走此函数，保证 monitor 与浮层可见性同生命周期（show 装、任一 hide 拆），不泄漏。
fn hide_overlay(app: &AppHandle) {
    let should_reclaim = app.get_webview_window(TRAY_LABEL).is_some_and(|w| {
        let was_visible = w.is_visible().unwrap_or(false);
        if was_visible {
            let _ = w.hide();
        }
        was_visible
    });
    // 冷建阶段的 Focused(false)/DOM blur 属于宿主装配噪声：窗口从未显示，不能据此取消首击请求，
    // 更不能写 last_hidden 让随后真正的托盘点击落入 300ms 去抖。只有可见→隐藏才是一次菜单 dismiss。
    if !should_reclaim {
        return;
    }
    if let Some(state) = app.try_state::<TrayOverlay>() {
        if let Ok(mut lifecycle) = state.lifecycle.lock() {
            lifecycle.hide();
        }
    }
    clear_open_probe(app);
    mark_hidden(app);
    #[cfg(target_os = "macos")]
    remove_click_monitor(app);
    if should_reclaim && !overlay_keeps_warm(app) {
        schedule_overlay_reclaim(app);
    }
}

/// 创建一代浮层窗。调用方保证它已跳出托盘点击分发帧；`generation` 同时注入 renderer，ready 回执
/// 必须携带同一代次才有资格上屏。**非致命**：失败返回 `None`，不自作主张唤出主窗。
fn build_overlay(app: &AppHandle, generation: u64) -> Option<tauri::WebviewWindow> {
    if let Some(win) = app.get_webview_window(TRAY_LABEL) {
        return Some(win); // 已建（幂等）
    }

    let initial_edge = overlay_placement(app, anchor(app))
        .map(|placement| placement.edge)
        .unwrap_or_else(default_tray_edge);
    let edge_script = tray_edge_boot_script(initial_edge);
    let initialization_script = format!(
        "window.__POLARIS_TRAY_GENERATION__ = {generation};\n{edge_script}\n{TRAY_BLUR_DISMISS_JS}"
    );
    let mut builder = WebviewWindowBuilder::new(app, TRAY_LABEL, WebviewUrl::App(TRAY_PAGE.into()))
        .title("Polaris")
        // DOM `blur` → tray_hide 的替代 dismiss（defect#3a，mac Rust 侧 Focused 递送不可靠时兜底）。
        .initialization_script(initialization_script)
        .inner_size(TRAY_WIDTH, 420.0)
        .resizable(false)
        .minimizable(false)
        .maximizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false);

    // mac/win：透明窗 + **关系统窗口阴影**。阴影沿的是**窗口矩形**（不是卡片圆角），透明窗上就成了卡片外
    // 那圈灰边/「波纹」——真机实拍确认，故恒关。无箭头「面板风」质感改由前端承担：卡片 1px 边框定边 +
    // 贴菜单栏 native 间隙（`tray-overlay.css`），**不再画 CSS box-shadow**（defect#2「不该有的阴影」——
    // 透明窗上的 CSS 阴影会被 body overflow:hidden 裁成硬边=「截断」观感，一并去掉）。
    // 且此处**不设 background_color**：它会给 webview 铺一层实底，压掉 transparent。
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        builder = builder.transparent(true).shadow(false);
    }
    // Linux 恒不透明（透明窗在无合成器/部分 WM 下=黑块或穿透）：卡片 surface 同色实底兜底。
    // 底色按 `config.uiTheme` 折算（B）——此前硬编码深色 surface，浅色用户每次弹浮层都先闪一格深色底
    // （浮层 WebView 在 120s 保温期内复用、show 即上屏，webview 重绘在其后）。运行期改主题由
    // [`toggle_overlay`] 的 `set_background_color` 跟进（同一代窗口不重建，建窗时这一次只管首次）。
    #[cfg(target_os = "linux")]
    {
        builder = builder
            .transparent(false)
            .background_color(surface_color(native_dark(app)));
    }

    let win = match builder.build() {
        Ok(w) => w,
        Err(e) => {
            log::warn!("托盘浮层窗创建失败（主窗仍可从 Dock/任务栏唤出）：{e}");
            return None;
        }
    };

    #[cfg(target_os = "macos")]
    if let Err(e) = configure_nonactivating_overlay(&win) {
        log::warn!("托盘浮层 non-activating 宿主配置失败，本代窗口不展示：{e}");
        if let Err(destroy_err) = destroy_overlay_preserving_tray_residency(app, &win) {
            log::warn!("托盘浮层宿主配置失败后的回收也失败：{destroy_err}");
        }
        return None;
    }

    // 失焦即收起（点窗外 / 切到别的 app）：菜单语义。走 hide_overlay 统一拆 mac 全局监听器（defect#3）。
    // （W13 的明暗信号源不挂这里：本窗限时存活——轻量转场与 120s 空闲回收都会销毁它；
    // Win 直读注册表真值、Linux 留窗口探测链，均见 main.rs 的 system_dark_bg。）
    let app_handle = app.clone();
    win.on_window_event(move |event| {
        if let WindowEvent::Focused(false) = event {
            hide_overlay(&app_handle);
        }
    });
    Some(win)
}

/// 把冷建排到托盘点击回调返回之后：W18 已证实 WebView 建/销不能跑在 OS 消息分发栈内；同一纪律
/// 适用于托盘 `Click` 回调。renderer 未 ready 前窗口保持 hidden，避免空壳和加载期 blur 竞态。
fn queue_overlay_build(app: &AppHandle, generation: u64) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let callback_app = app.clone();
        let _ = app.run_on_main_thread(move || {
            let still_current = callback_app
                .try_state::<TrayOverlay>()
                .is_some_and(|state| {
                    state.lifecycle.lock().ok().is_some_and(|lifecycle| {
                        lifecycle.generation == generation && lifecycle.build_queued
                    })
                });
            if !still_current {
                return;
            }

            let win = build_overlay(&callback_app, generation);
            if let Some(state) = callback_app.try_state::<TrayOverlay>() {
                if let Ok(mut lifecycle) = state.lifecycle.lock() {
                    lifecycle.build_finished(generation, win.is_some());
                }
            }
            if win.is_none() {
                clear_open_probe(&callback_app);
                return;
            }
            schedule_overlay_ready_timeout(&callback_app, generation);
        });
    });
}

fn schedule_overlay_ready_timeout(app: &AppHandle, generation: u64) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(TRAY_READY_TIMEOUT_SECS)).await;
        let callback_app = app.clone();
        let _ = app.run_on_main_thread(move || {
            let timed_out = callback_app
                .try_state::<TrayOverlay>()
                .is_some_and(|state| {
                    state.lifecycle.lock().ok().is_some_and(|lifecycle| {
                        lifecycle.generation == generation && !lifecycle.renderer_ready
                    })
                });
            if timed_out {
                log::warn!(
                    "托盘浮层 renderer 在 {TRAY_READY_TIMEOUT_SECS}s 内未就绪，回收本代 WebView"
                );
                destroy_overlay(&callback_app);
            }
        });
    });
}

fn show_ready_overlay(app: &AppHandle, win: &tauri::WebviewWindow) {
    invalidate_overlay_reclaim(app);
    #[cfg(target_os = "linux")]
    {
        let _ = win.set_background_color(Some(surface_color(native_dark(app))));
    }
    reposition(win);
    if let Err(e) = win.show() {
        log::warn!("托盘浮层显示失败：{e}");
        return;
    }
    reposition(win);
    focus_overlay(win);
    log_open_probe(app, "shown", true);
}

/// macOS/Windows 托盘左/右键入口（由 `main.rs` 的 `on_tray_icon_event` 调）。
///
/// 可见 → 隐藏（toggle off）；不可见 → 定位到托盘所在屏角 + 显示 + 聚焦。
/// 浮层创建失败 → 本次点击 no-op；不把「托盘菜单」意图突然放大成主窗。
pub fn toggle_overlay(app: &AppHandle, rect: Option<tauri::Rect>) {
    // 事件 rect 自身就是物理像素，不依赖浮层窗是否已建、当前落在哪块屏。先存锚点，冷建与热开共用；
    // 即便本次点击是关闭，下次打开也从最新托盘位置起步。
    if let Some(rect) = rect {
        store_anchor(app, rect);
    }
    let existing = app.get_webview_window(TRAY_LABEL);
    if existing
        .as_ref()
        .is_some_and(|win| win.is_visible().unwrap_or(false))
    {
        hide_overlay(app);
        return;
    }
    // 刚因本次点击导致失焦隐藏（<300ms）→ 视为「点击图标关闭」，不重开。
    if recently_hidden(app) {
        return;
    }
    let action = app.try_state::<TrayOverlay>().and_then(|state| {
        state
            .lifecycle
            .lock()
            .ok()
            .map(|mut lifecycle| lifecycle.request_open(existing.is_some()))
    });
    let Some(action) = action else {
        return;
    };
    begin_open_probe(app, !matches!(action, OverlayOpenAction::ShowNow));
    invalidate_overlay_reclaim(app);
    match action {
        OverlayOpenAction::ShowNow => {
            if let Some(win) = existing {
                show_ready_overlay(app, &win);
            }
        }
        OverlayOpenAction::AwaitReady => {}
        OverlayOpenAction::QueueBuild { generation } => {
            queue_overlay_build(app, generation);
        }
    }
}

/// 使所有已排队的隐藏回收任务失效。Relaxed 足够：代次只承担去重，不承载其他内存可见性。
fn invalidate_overlay_reclaim(app: &AppHandle) -> u64 {
    app.try_state::<TrayOverlay>()
        .map(|state| state.reclaim_generation.fetch_add(1, Ordering::Relaxed) + 1)
        .unwrap_or(0)
}

/// 当前托盘浮层是否按用户偏好保持 warm。状态尚未 manage 时回落出厂默认 true。
fn overlay_keeps_warm(app: &AppHandle) -> bool {
    app.try_state::<TrayOverlay>()
        .is_none_or(|state| state.keep_warm.load(Ordering::Relaxed))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayRetentionAction {
    None,
    CancelReclaim,
    ScheduleReclaim,
}

/// 配置切换时该怎样处理已存在的浮层计时器。
///
/// case 矩阵：
/// - 值没变：不动（否则任意配置保存都会把 120s 计时器无限续期）；
/// - 关→开：使已有回收任务失效；
/// - 开→关：若浮层已隐藏则立即重新挂 120s 回收，若仍可见则等本次 hide 再挂。
#[must_use]
fn overlay_retention_action(
    previous: bool,
    next: bool,
    overlay_hidden: bool,
) -> OverlayRetentionAction {
    if previous == next {
        OverlayRetentionAction::None
    } else if next {
        OverlayRetentionAction::CancelReclaim
    } else if overlay_hidden {
        OverlayRetentionAction::ScheduleReclaim
    } else {
        OverlayRetentionAction::None
    }
}

/// 从 ConfigManager 的原始配置缓存同步 `keepTrayMenuWarm`，并即时兑现开关变化。
///
/// 复用 `event:configChanged` 的 Rust 监听，不新增 IPC/第二份持久化状态。调用点只有启动初始化与配置变更
/// 事件；不能挂进 30s 托盘自愈轮询，否则 warm=false 时会不断重排计时器、WebView 永不回收。
pub(crate) fn reconcile_overlay_retention(app: &AppHandle) {
    let next = app
        .try_state::<crate::runtime::AppRuntime>()
        .and_then(|rt| {
            rt.config()
                .with_current(|cfg| {
                    cfg.get(KEEP_TRAY_MENU_WARM_KEY)
                        .and_then(Value::as_bool)
                        .unwrap_or(true)
                })
                .ok()
        })
        .unwrap_or(true);
    let Some(state) = app.try_state::<TrayOverlay>() else {
        return;
    };
    let previous = state.keep_warm.swap(next, Ordering::Relaxed);
    let overlay_hidden = app
        .get_webview_window(TRAY_LABEL)
        .is_some_and(|win| !win.is_visible().unwrap_or(false));
    match overlay_retention_action(previous, next, overlay_hidden) {
        OverlayRetentionAction::None => {}
        OverlayRetentionAction::CancelReclaim => {
            invalidate_overlay_reclaim(app);
            log::debug!("托盘浮层保持 warm：已取消隐藏回收任务");
        }
        OverlayRetentionAction::ScheduleReclaim => {
            schedule_overlay_reclaim(app);
            log::debug!("托盘浮层关闭 warm：已恢复隐藏回收任务");
        }
    }
}

/// 隐藏后延迟回收托盘 WebView。任务到点后回主线程复核「代次未变化 + 仍隐藏」才销毁；期间任何
/// reopen/hide/destroy 都会换代，因此不会出现旧计时器把刚打开的菜单关掉。
fn schedule_overlay_reclaim(app: &AppHandle) {
    let generation = invalidate_overlay_reclaim(app);
    if generation == 0 {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(TRAY_IDLE_RECLAIM_SECS)).await;
        let callback_app = app.clone();
        let _ = app.run_on_main_thread(move || {
            let is_current = callback_app
                .try_state::<TrayOverlay>()
                .is_some_and(|state| {
                    state.reclaim_generation.load(Ordering::Relaxed) == generation
                });
            if !is_current {
                return;
            }
            // 配置事件正常会使 generation 失效；这里再读一次运行期镜像，兜事件递送失败/竞态，
            // 绝不在用户已开启 warm 后销毁浮层。
            if overlay_keeps_warm(&callback_app) {
                return;
            }
            let Some(win) = callback_app.get_webview_window(TRAY_LABEL) else {
                return;
            };
            if win.is_visible().unwrap_or(false) {
                return;
            }
            if destroy_overlay(&callback_app) {
                log::debug!("托盘浮层隐藏超时，已回收 WebView");
            }
        });
    });
}

/// 销毁托盘浮层前，若它已是**最后一个原生窗口**且托盘仍在，则武装一次 C16 退出守卫。
///
/// Tauri 会把末窗 `destroy()` 折成一次 `RunEvent::ExitRequested`。主窗已进入轻量态后，托盘浮层的
/// 2 分钟空闲回收正好会成为「销毁末窗」：若只在主窗销毁前武装 [`crate::LightweightState`]，那次守卫
/// 早已被消费，浮层回收便会把整个应用（连同托盘/代理）一起退出。这里把**每一次有意的末窗回收**都接到
/// 同一条一次性守卫上；显式退出仍先置 `QuitState`，不会被它拦住。
///
/// Polaris 的窗口宿主全部由 `WebviewWindowBuilder` 创建，故按 `webview_windows()` 计数；若还有主窗、更新
/// 提示或仪表盘，本次销毁不会触发退出，提前置位会留下陈旧守卫，可能误拦后续 OS 退出。
fn destroy_overlay_preserving_tray_residency(
    app: &AppHandle,
    win: &tauri::WebviewWindow,
) -> tauri::Result<()> {
    let armed = should_arm_last_overlay_exit_guard(
        app.webview_windows().len(),
        app.tray_by_id("main").is_some(),
    ) && app
        .state::<crate::LightweightState>()
        .0
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();

    let result = win.destroy();
    // destroy 失败不会产生 ExitRequested；只撤销本函数亲自置的那一位，不能误清别的轻量转场。
    if result.is_err() && armed {
        app.state::<crate::LightweightState>()
            .0
            .store(false, Ordering::SeqCst);
    }
    result
}

/// [`destroy_overlay_preserving_tray_residency`] 的纯判据，单测锁住“只为末窗 + 托盘在”武装。
#[must_use]
fn should_arm_last_overlay_exit_guard(window_count: usize, tray_present: bool) -> bool {
    window_count == 1 && tray_present
}

/// 立即销毁浮层。仅 renderer-ready 超时与关闭 warm 后的隐藏超时调用；主窗口轻量转场不得调用，
/// 否则 `keepTrayMenuWarm=true` 会被另一个无关开关静默覆盖。
fn destroy_overlay(app: &AppHandle) -> bool {
    invalidate_overlay_reclaim(app);
    #[cfg(target_os = "macos")]
    remove_click_monitor(app);
    let destroyed = if let Some(win) = app.get_webview_window(TRAY_LABEL) {
        match destroy_overlay_preserving_tray_residency(app, &win) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("托盘浮层 WebView 提前回收失败：{e}");
                false
            }
        }
    } else {
        true
    };
    if destroyed {
        if let Some(state) = app.try_state::<TrayOverlay>() {
            if let Ok(mut lifecycle) = state.lifecycle.lock() {
                lifecycle.reset();
            }
        }
        clear_open_probe(app);
    }
    destroyed
}

/// 把 Tauri 创建的 borderless NSWindow 切成 AppKit 的 non-activating panel 语义。
///
/// 无需另建/重挂 WKWebView：`NSWindowStyleMaskNonactivatingPanel` 可在宿主创建后补入。macOS 26.6.2
/// 真机探针验证了“先 borderless 建窗、再 setStyleMask”这一精确序列：首个按钮点击可交互，同时前台
/// app 保持不变。若配置失败，本代窗口宁可不展示，也不退回会抢焦点的旧语义。
#[cfg(target_os = "macos")]
fn configure_nonactivating_overlay(win: &tauri::WebviewWindow) -> Result<(), String> {
    use objc2_app_kit::{NSWindow, NSWindowStyleMask};

    let raw = win.ns_window().map_err(|e| e.to_string())?;
    if raw.is_null() {
        return Err("NSWindow handle is null".to_string());
    }
    // SAFETY: Tauri 的 `ns_window()` 返回该 WebviewWindow 持有、且当前主线程有效的 NSWindow 指针；
    // 本函数只在 build() 紧接着的主线程调用，不越过窗口生命周期保存引用。
    let ns_window = unsafe { &*raw.cast::<NSWindow>() };
    ns_window.setStyleMask(ns_window.styleMask() | NSWindowStyleMask::NonactivatingPanel);
    Ok(())
}

/// 让 non-activating 浮层取得键盘焦点，但不激活整个 Polaris app。
///
/// Tauri/tao 的 macOS `set_focus()` 在 `makeKeyAndOrderFront:` 后还会无条件调用
/// `activateIgnoringOtherApps:YES`，正是 W25 的抢焦点源。这里绕开那层封装，直接调用原生方法；
/// 全局鼠标 monitor 继续负责窗外收起，无需在 hide 时猜测并恢复旧 app（那会与用户点击第三个 app 竞态）。
#[cfg(target_os = "macos")]
fn focus_overlay(win: &tauri::WebviewWindow) {
    use objc2_app_kit::NSWindow;

    if let Ok(raw) = win.ns_window() {
        if !raw.is_null() {
            // SAFETY: 与 configure_nonactivating_overlay 同一宿主指针；本函数由主线程 show 路径调用，
            // 引用不逃逸。直接调原生方法是为了绕开 tao `set_focus()` 内附带的 app activation。
            let ns_window = unsafe { &*raw.cast::<NSWindow>() };
            ns_window.makeKeyAndOrderFront(None);
        }
    }
    // show 后装全局点击监听器：点其它菜单栏状态项 / 桌面 / 别的窗即收起浮层（defect#3）。
    install_click_monitor(win.app_handle());
}

/// 非 mac：仅聚焦（Win/Linux 辅助窗 `Focused(false)` 递送与 mac borderless-key 坑无关）。
#[cfg(not(target_os = "macos"))]
fn focus_overlay(win: &tauri::WebviewWindow) {
    let _ = win.set_focus();
}

/// 装全局鼠标按下监听器（defect#3：点**另一个菜单栏状态项**不收起浮层的根治）。
///
/// # 根因
/// borderless/辅助浮层在 mac 上，点另一个菜单栏状态项是**系统状态栏**的点击、不切本 app 的 active 态 →
/// 浮层宿主 NSWindow 不 resignKey → `WindowEvent::Focused(false)` 与 DOM `blur` 都不触发 → 浮层赖着不走。
/// non-activating 宿主的 `Focused(false)`/DOM blur 兜的是键窗迁移，兜不住所有状态栏宿主事件。
///
/// # 修法（Apple 文档的状态栏 popover 标准式）
/// show 时装 `NSEvent addGlobalMonitorForEventsMatchingMask:handler:`（Left/Right/OtherMouseDown）——
/// **本 app 之外**任意点击都派发（含点另一状态项、点桌面、点别的窗），handler 里 [`hide_overlay`] 收起。
/// 全局 monitor 只观察不吞事件（不影响被点目标），主线程派发。与 `Focused(false)` 互补并存（切 app 仍
/// 走那条）。hide 时 [`remove_click_monitor`] 拆掉。
///
/// ⚠️ 本机（Linux）编不到、验不了（objc2 NSEvent 首次编译在 mac，H-5）→ 真机（mac）待行为确认。
#[cfg(target_os = "macos")]
fn install_click_monitor(app: &AppHandle) {
    use block2::RcBlock;
    use core::ptr::NonNull;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSEvent, NSEventMask};

    let Some(state) = app.try_state::<TrayOverlay>() else {
        return;
    };
    let Ok(mut guard) = state.click_monitor.lock() else {
        return;
    };
    if guard.is_some() {
        return; // 幂等：已装不重复装（避免多个 monitor 泄漏 / 多次收起）
    }
    let app_handle = app.clone();
    // handler 在主线程派发（AppKit 全局 monitor 契约）：点本 app 之外任意位置即收起浮层。
    let handler: RcBlock<dyn Fn(NonNull<NSEvent>)> =
        RcBlock::new(move |_event: NonNull<NSEvent>| {
            hide_overlay(&app_handle);
        });
    let mask =
        NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown | NSEventMask::OtherMouseDown;
    // 安全 fn（objc2 生成，method_family=none → 返回 +1 owned 的 monitor 对象）。into_raw 存指针地址，
    // 由 remove_click_monitor 在主线程 removeMonitor + from_raw 释放那 +1。addGlobalMonitor 内部会 copy
    // 本 block（RcBlock），故本地 `handler` 随后 drop 无碍——AppKit 持有副本至 removeMonitor。
    if let Some(monitor) = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &handler) {
        *guard = Some(Retained::into_raw(monitor) as usize);
    }
}

/// 拆全局鼠标按下监听器（defect#3）。任一收起路径经 [`hide_overlay`] 调用。`removeMonitor:` 必须在**主
/// 线程**，故经 `run_on_main_thread` 调度（同步 command 在 WebView2 IPC 分发栈=主线程内直跑、
/// `async fn` command 才被 spawn 到异步 runtime；Focused(false)/toggle_overlay/monitor handler 在
/// 主线程跑——统一调度都安全）。`take()` 去重，保证同一 monitor 只 remove
/// 一次。非 mac 不编译（无此函数）。
#[cfg(target_os = "macos")]
fn remove_click_monitor(app: &AppHandle) {
    let raw = app
        .try_state::<TrayOverlay>()
        .and_then(|s| s.click_monitor.lock().ok().and_then(|mut g| g.take()));
    if let Some(raw) = raw {
        let _ = app.run_on_main_thread(move || {
            let ptr = raw as *mut objc2::runtime::AnyObject;
            // SAFETY: ptr 来自 Retained::into_raw（+1 owned 的 monitor）；take() 保证只 remove 一次；
            // removeMonitor / from_raw 都在主线程执行；from_raw 收回 Retained，drop 时释放那 +1。
            unsafe {
                objc2_app_kit::NSEvent::removeMonitor(&*ptr);
                let _ = objc2::rc::Retained::from_raw(ptr);
            }
        });
    }
}

fn valid_anchor(anchor: PhysicalRect) -> bool {
    anchor.x.is_finite()
        && anchor.y.is_finite()
        && anchor.w.is_finite()
        && anchor.h.is_finite()
        && anchor.w > 0.0
        && anchor.h > 0.0
}

fn default_tray_edge() -> TrayEdge {
    if cfg!(target_os = "macos") {
        TrayEdge::Top
    } else {
        TrayEdge::Bottom
    }
}

fn edge_distance(anchor: PhysicalRect, screen: ScreenArea, edge: TrayEdge) -> f64 {
    match edge {
        TrayEdge::Top => (anchor.y - f64::from(screen.top)).abs(),
        TrayEdge::Bottom => (f64::from(screen.bottom) - (anchor.y + anchor.h)).abs(),
        TrayEdge::Left => (anchor.x - f64::from(screen.left)).abs(),
        TrayEdge::Right => (f64::from(screen.right) - (anchor.x + anchor.w)).abs(),
    }
}

fn work_inset(screen: ScreenArea, work: ScreenArea, edge: TrayEdge) -> i32 {
    match edge {
        TrayEdge::Top => work.top.saturating_sub(screen.top),
        TrayEdge::Bottom => screen.bottom.saturating_sub(work.bottom),
        TrayEdge::Left => work.left.saturating_sub(screen.left),
        TrayEdge::Right => screen.right.saturating_sub(work.right),
    }
    .max(0)
}

/// 从托盘锚点与同屏工作区推断系统栏所在边。工作区有保留边时只在这些边中选离锚点最近者：这能在
/// Windows 竖向任务栏的底角处打破“左/右与底边同距”的歧义，也能在 mac 同时存在顶部菜单栏与
/// 底部/侧边 Dock 时仍选中图标实际所在的顶部。自动隐藏使工作区等于整屏时，再退回四边最近距离；
/// 距离完全相同时保持平台默认（mac 顶、其余底）。
fn resolve_tray_edge(
    anchor: Option<PhysicalRect>,
    screen: ScreenArea,
    work: ScreenArea,
    preferred: TrayEdge,
) -> TrayEdge {
    let Some(anchor) = anchor.filter(|anchor| valid_anchor(*anchor)) else {
        return preferred;
    };
    let edges = [
        preferred,
        TrayEdge::Top,
        TrayEdge::Bottom,
        TrayEdge::Left,
        TrayEdge::Right,
    ];
    let has_reserved_edge = edges
        .iter()
        .copied()
        .any(|edge| work_inset(screen, work, edge) > 0);
    let mut best: Option<(TrayEdge, f64)> = None;
    for edge in edges {
        if has_reserved_edge && work_inset(screen, work, edge) == 0 {
            continue;
        }
        let distance = edge_distance(anchor, screen, edge);
        if best.is_none_or(|(_, best_distance)| distance < best_distance) {
            best = Some((edge, distance));
        }
    }
    best.map(|(edge, _)| edge).unwrap_or(preferred)
}

/// 托盘锚点中心点是选屏的唯一权威：不再看浮层窗上一次停留的 `current_monitor()`。Tauri 的
/// `monitor_from_point` 在 Windows 直达 `MonitorFromPoint`，`work_area()` 直达 `GetMonitorInfoW.rcWork`；
/// 因而多屏负坐标、异 DPI 与任务栏保留区都来自同一个 monitor 事实源。无有效锚点才回退主屏。
fn overlay_placement(app: &AppHandle, anchor: Option<PhysicalRect>) -> Option<OverlayPlacement> {
    let anchor = anchor.filter(|anchor| valid_anchor(*anchor));
    let monitor = anchor
        .and_then(|anchor| {
            app.monitor_from_point(anchor.x + anchor.w / 2.0, anchor.y + anchor.h / 2.0)
                .ok()
                .flatten()
        })
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let screen = ScreenArea::new(monitor.position(), monitor.size());
    let monitor_work = monitor.work_area();
    let work = ScreenArea::new(&monitor_work.position, &monitor_work.size);
    let work = if work.is_usable() { work } else { screen };
    Some(OverlayPlacement {
        work_area: work,
        edge: resolve_tray_edge(anchor, screen, work, default_tray_edge()),
        scale_factor: monitor.scale_factor(),
    })
}

/// 首帧前播种托盘边缘，供 CSS 把卡片的透明留白移到**远离**系统栏的一侧；四个方向的外边距总量不变，
/// 所以不会让 `tray_resize` 高度或固定窗宽发生二次抖动。运行期热开到另一块屏时由同一个 setter 更新。
fn tray_edge_boot_script(edge: TrayEdge) -> String {
    format!(
        r#"(function () {{
  window.__POLARIS_TRAY_EDGE__ = '{edge}';
  function apply() {{
    var el = document.documentElement;
    if (el) el.setAttribute('data-tray-edge', window.__POLARIS_TRAY_EDGE__);
  }}
  window.__POLARIS_SET_TRAY_EDGE__ = function (next) {{
    if (window.__POLARIS_TRAY_EDGE__ === next) return;
    window.__POLARIS_TRAY_EDGE__ = next;
    apply();
  }};
  apply();
  document.addEventListener('readystatechange', apply);
  document.addEventListener('DOMContentLoaded', apply);
}})();"#,
        edge = edge.attr()
    )
}

fn apply_tray_edge(win: &tauri::WebviewWindow, edge: TrayEdge) {
    let _ = win.eval(format!(
        "window.__POLARIS_SET_TRAY_EDGE__ && window.__POLARIS_SET_TRAY_EDGE__('{}');",
        edge.attr()
    ));
}

/// 纯几何：由锚点（图标屏幕物理矩形）+ **同屏工作区** + 窗口尺寸 + 系统栏边缘算浮层左上角。
/// 有锚点时沿图标中心对齐并朝工作区内部展开；无/退化锚点时贴该边的右下惯用角。最终只在同一工作区
/// 内 clamp，绝不跨回浮层旧屏或主屏。
fn overlay_xy(
    anchor: Option<PhysicalRect>,
    work: ScreenArea,
    win_size: (u32, u32),
    gap: i32,
    edge: TrayEdge,
) -> (i32, i32) {
    let wsw = i32::try_from(win_size.0).unwrap_or(i32::MAX);
    let wsh = i32::try_from(win_size.1).unwrap_or(i32::MAX);

    let (x, y) = match anchor.filter(|anchor| valid_anchor(*anchor)) {
        Some(a) => {
            let cx = (a.x + a.w / 2.0).round() as i32 - wsw / 2;
            let cy = (a.y + a.h / 2.0).round() as i32 - wsh / 2;
            match edge {
                TrayEdge::Top => (cx, (a.y + a.h).round() as i32 + gap),
                TrayEdge::Bottom => (cx, a.y.round() as i32 - wsh - gap),
                TrayEdge::Left => ((a.x + a.w).round() as i32 + gap, cy),
                TrayEdge::Right => (a.x.round() as i32 - wsw - gap, cy),
            }
        }
        None => match edge {
            TrayEdge::Top => (work.right - wsw - gap, work.top + gap),
            TrayEdge::Bottom => (work.right - wsw - gap, work.bottom - wsh - gap),
            TrayEdge::Left => (work.left + gap, work.bottom - wsh - gap),
            TrayEdge::Right => (work.right - wsw - gap, work.bottom - wsh - gap),
        },
    };
    let x = x.clamp(work.left, work.right.saturating_sub(wsw).max(work.left));
    let y = y.clamp(work.top, work.bottom.saturating_sub(wsh).max(work.top));
    (x, y)
}

/// 把浮层对齐到**托盘图标**并夹回屏内。锚点来自 `TrayIconEvent::Click` 的 `rect`（OS 给的图标屏幕矩形，
/// 本来就是物理像素）。真正的几何在 [`overlay_xy`]（纯函数、可单测）；本函数只负责按锚点中心找
/// monitor/work area、同步 CSS 边缘并下发 `set_position`。取不到显示器信息 → 保持当前位置（不猜坐标）。
fn reposition(win: &tauri::WebviewWindow) {
    let app = win.app_handle();
    let anchor = anchor(app);
    let Some(placement) = overlay_placement(app, anchor) else {
        return;
    };
    let ws = win.outer_size().unwrap_or(PhysicalSize::new(280, 420));
    // gap 是 1 逻辑像素折到**锚点所在屏**的物理像素；卡片近系统栏侧另留 2px CSS 安全边，合计约 3px。
    let gap = placement.scale_factor.round() as i32;
    apply_tray_edge(win, placement.edge);
    let (x, y) = overlay_xy(
        anchor,
        placement.work_area,
        (ws.width, ws.height),
        gap,
        placement.edge,
    );
    let _ = win.set_position(PhysicalPosition::new(x, y));
}

// ── 浮层 React 端 → 主进程的专用薄 command ─────────────────────────────────────

/// 浮层量出内容高度后回报 → 设窗高（宽固定 [`TRAY_WIDTH`]）并重定位（自适应高）。
#[tauri::command]
pub fn tray_resize(app: AppHandle, height: f64) -> ApiResponse<()> {
    if let Some(win) = app.get_webview_window(TRAY_LABEL) {
        let h = height.clamp(80.0, 720.0);
        let _ = win.set_size(LogicalSize::new(TRAY_WIDTH, h));
        reposition(&win);
    }
    ok_void()
}

/// 托盘 renderer 完成 React commit 后的代次化 ready 回执。冷建窗口在此之前始终 hidden；命令声明为
/// async，使兑现腿不在 WebKit IPC 分发栈内直接 show，而是排回下一轮主线程事件循环。
#[tauri::command]
pub async fn tray_renderer_ready(app: AppHandle, generation: u64) -> ApiResponse<()> {
    let should_show = app.try_state::<TrayOverlay>().is_some_and(|state| {
        state
            .lifecycle
            .lock()
            .ok()
            .is_some_and(|mut lifecycle| lifecycle.mark_ready(generation))
    });
    if !should_show {
        return ok_void();
    }
    log_open_probe(&app, "renderer-ready", false);
    let callback_app = app.clone();
    let _ = app.run_on_main_thread(move || {
        let still_requested = callback_app
            .try_state::<TrayOverlay>()
            .is_some_and(|state| {
                state
                    .lifecycle
                    .lock()
                    .ok()
                    .is_some_and(|lifecycle| lifecycle.should_show(generation))
            });
        if !still_requested {
            return;
        }
        if let Some(win) = callback_app.get_webview_window(TRAY_LABEL) {
            show_ready_overlay(&callback_app, &win);
        }
    });
    ok_void()
}

/// 收起浮层（连接/断开/切节点等动作后关闭菜单）。
#[tauri::command]
pub fn tray_hide(app: AppHandle) -> ApiResponse<()> {
    hide_overlay(&app);
    ok_void()
}

/// 显示主窗（「打开主窗口」/「在主窗口管理」/「打开设置」）并收起浮层。复用 `crate::show_main_window`
/// （与托盘图标点击 / 菜单「显示」/ dock 重开同一路径）。
///
/// `screen`：可选目标屏，经 [`normalize_tray_screen`] 白名单归一。**不传 = 今天的行为逐字节不变**
/// （既有 `invoke('tray_show_main')` 无参调用点零改动，Tauri 对 `Option<_>` 形参把缺失键解析成 `None`）。
/// 通道选型理由见 [`normalize_tray_screen`] 上方注释。
///
/// # 三条投递腿，**互补而非互斥**
///
/// 意图的目的地是「主窗的 nav-store」，而它可能处在三种状态，每种只有一条腿够得着：
///
/// 1. **窗在、订阅已挂**（常态）→ 事件腿：`emit_to_main(EVENT_TRAY_OPEN_SCREEN)`，即到即导航。
/// 2. **窗已销毁**（C16 轻量模式）→ 首帧种子腿：`create_main_window` 建窗时把
///    [`TrayOverlay::pending_screen`] 注入 `initialization_script`（`window.__POLARIS_TRAY_SCREEN__`），
///    前端 boot 时同步读一次。事件腿在这里必丢（emit 发生在 webview 装载之前）。
/// 3. **窗在、但 webview 还没挂上订阅**（冷启动/重载后的那一小段）→ **两条都够不着**：
///    种子腿只在建窗那一刻注入（窗已经建好了，不会再注入一次），事件腿 emit 出去没人听 ⇒
///    intent **静默丢失**（窗开了、屏没跳）。这正是 2026-07-28 复审标 NEEDS-REPRO 的那条竞态。
///
/// 修法：**pending 恒写**（不再只在 `!main_alive` 时写），并给前端一条主动取货的通道
/// [`tray_take_pending_screen`]：nav-store 装配时取一发。于是第 3 种状态由「前端就绪后自己来取」覆盖。
///
/// 陈旧意图怎么防：`pending` 是 **take 一次即清**，且事件腿命中的那一路，前端在 `applyTrayScreenIntent`
/// 之后**也会调一次取货**把它清掉（"消费后清"）——不清的话，下次因任何别的原因重建主窗都会被送去设置页。
#[tauri::command]
pub fn tray_show_main(app: AppHandle, screen: Option<String>) -> ApiResponse<()> {
    let target = screen.as_deref().and_then(normalize_tray_screen);
    // 必须在 `show_main_window` **之前**问：它会把销毁的主窗重建出来，之后就分不出是哪条腿了。
    let main_alive = app.get_webview_window("main").is_some();
    let legs = tray_show_main_legs(target.is_some(), main_alive);
    if let Some(t) = target {
        if legs.write_pending {
            set_pending_screen(&app, t);
        }
    }
    hide_overlay(&app);
    crate::show_main_window(&app);
    if let (Some(t), true) = (target, legs.emit_event) {
        crate::events::emit_to_main(&app, crate::events::channel::EVENT_TRAY_OPEN_SCREEN, t);
    }
    ok_void()
}

/// [`tray_show_main`] 该点亮哪几条投递腿。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrayShowMainLegs {
    /// 写 [`TrayOverlay::pending_screen`]（供首帧种子腿注入 / 前端 [`tray_take_pending_screen`] 取货）。
    pub write_pending: bool,
    /// 单播 `EVENT_TRAY_OPEN_SCREEN`（只有主窗已存在时才有意义）。
    pub emit_event: bool,
}

/// 折出投递腿（纯函数，可单测）。
///
/// **`write_pending` 恒真**（只要有目标屏）—— 这正是 2026-07-28 复审那条竞态的修法。
/// 此前是 `if !main_alive { set_pending_screen(...) }`：两条腿互斥 ⇒ 「主窗存在但 webview 还没挂上
/// `EVENT_TRAY_OPEN_SCREEN` 订阅」这一格里，事件腿 emit 出去没人听、种子腿又因为窗已存在而根本没写
/// ⇒ intent 静默丢失（窗开了、屏没跳）。恒写之后这一格由前端的取货腿兜住。
///
/// 陈旧意图由「take 一次即清 + 前端事件腿命中后也调一次取货」防住，不靠这里少写。
#[must_use]
pub fn tray_show_main_legs(has_target: bool, main_alive: bool) -> TrayShowMainLegs {
    TrayShowMainLegs {
        write_pending: has_target,
        emit_event: has_target && main_alive,
    }
}

/// 记下「主窗要跳到哪一屏」（见 [`tray_show_main`] 的三条腿）。
fn set_pending_screen(app: &AppHandle, screen: &'static str) {
    if let Some(state) = app.try_state::<TrayOverlay>() {
        if let Ok(mut g) = state.pending_screen.lock() {
            *g = Some(screen);
        }
    }
}

/// **取走**待导航目标屏（一次性；`create_main_window` 建窗时调）。
///
/// take 语义是刚需：留着的话，用户下次因任何别的原因重建主窗（轻量模式再进再出）都会被送去设置页 ——
/// 一条陈旧意图变成反复发作的跳屏。
pub fn take_pending_screen(app: &AppHandle) -> Option<&'static str> {
    app.try_state::<TrayOverlay>()
        .and_then(|s| s.pending_screen.lock().ok().and_then(|mut g| g.take()))
}

/// 前端主动取货口 —— [`tray_show_main`] 第 3 条腿（窗在、订阅还没挂）的收货端，兼「消费后清」。
///
/// 主窗 nav-store 装配时调一次：
///  - 有值（事件腿丢了 / 冷启动期间点的托盘）→ 前端据此导航，竞态被补上；
///  - 无值（常态）→ 返回 `None`，零成本。
/// 事件腿命中之后前端**也调一次**，把 `tray_show_main` 恒写下的那份余量清掉，避免它以陈旧意图的
/// 形式活到下一次建窗。
///
/// 返回的是 [`normalize_tray_screen`] 白名单里的 `'static` 串，值域与另两条腿逐字相同。
#[tauri::command]
pub fn tray_take_pending_screen(app: AppHandle) -> ApiResponse<Option<&'static str>> {
    ApiResponse::ok(take_pending_screen(&app))
}

/// 主窗首帧「托盘目标屏」种子脚本（与 [`theme_boot_script`] 同款注入手法）。
///
/// 值域已由 [`normalize_tray_screen`] 钉死为白名单里的 `'static` 串 ⇒ 这里拼进 JS 字面量不存在注入面
/// （不是前端传什么就拼什么）。
#[must_use]
pub fn tray_screen_boot_script(screen: &str) -> String {
    format!("window.__POLARIS_TRAY_SCREEN__ = '{screen}';\n")
}

/// 「检查更新」（A1）：托盘浮层与**原生兜底菜单**共用的唯一实现。
///
/// 返回 `true` = 有更新且提醒窗已弹出；`false` = 已是最新。失败返 `Err`（**绝不**把失败伪装成
/// 「已是最新」——那是 B5 反伪造里点名的形态，后端 `update_check` 自己也是这个语义）。
///
/// # 为什么这条链落在 Rust 而不是浮层的 JS 里
///
/// 弹提醒窗要的是 `update_popup_show(version, currentVersion)`，其中 `currentVersion` 的真值是
/// `app.package_info().version` —— 在主进程手里。放前端就得先绕一趟 `version_get_info` 再拼参数，
/// 平白多一条可能与 `startup_tasks` 那条链读出不同值的路径。
///
/// 链本身与 `startup_tasks::spawn_update_check` 逐段相同（check → hasUpdate → version → popup），
/// 含「`hasUpdate:true` 却缺 version = 后端契约破损，宁可不弹也不弹个空版本号」这条边界。
/// 预发布口径同样共用 [`crate::commands::updater::PUSH_UPDATE_INCLUDE_PRERELEASE`]：本腿检出的版本
/// 号会被写进弹窗，而用户点「更新」时的复查读的是同一个常量。
///
/// 错误串随 [`crate::i18n::app_lang`] 分档（浮层把它原样显示在 notice 行、原生菜单腿把它发进
/// 系统通知）—— 2026-07-31 前先是硬编码中文、后是 zh/en 二态，俄语 / 波斯语 / 繁中用户拿到的
/// 都不是自己的语言。
#[tauri::command]
pub async fn tray_check_update(app: AppHandle) -> ApiResponse<bool> {
    let lang = app_lang(&app);
    let resp = match crate::commands::update_check(
        app.clone(),
        app.state::<crate::runtime::AppRuntime>(),
        Some(crate::commands::updater::PUSH_UPDATE_INCLUDE_PRERELEASE),
    )
    .await
    {
        Ok(r) => r,
        Err(()) => return ApiResponse::err(t(lang, key::TRAY_UPDATE_CHECK_FAILED)),
    };
    if !resp.success {
        return ApiResponse::err(
            resp.error
                .unwrap_or_else(|| t(lang, key::TRAY_UPDATE_CHECK_FAILED)),
        );
    }
    let data = resp.data.unwrap_or(Value::Null);
    if data.get("hasUpdate").and_then(Value::as_bool) != Some(true) {
        return ApiResponse::ok(false);
    }
    let Some(version) = data
        .pointer("/updateInfo/version")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        log::warn!("托盘检查更新：hasUpdate 为真但缺 updateInfo.version，跳过弹窗");
        return ApiResponse::err(t(lang, key::NATIVE_UPDATE_INFO_INCOMPLETE));
    };
    let current = app.package_info().version.to_string();
    let r = crate::commands::update_popup_show(
        app.clone(),
        app.state::<crate::runtime::AppRuntime>(),
        version,
        current,
    );
    if r.success {
        ApiResponse::ok(true)
    } else {
        ApiResponse::err(
            r.error
                .unwrap_or_else(|| t(lang, key::NATIVE_UPDATE_POPUP_FAILED)),
        )
    }
}

/// 退出 Polaris：置 `QuitState`（放行 `CloseRequested`，不被 close-to-tray 卡）+ `app.exit(0)`。
/// 与 `main.rs` 托盘原生菜单「退出」/ 应用菜单 ⌘Q 逐字节相同的退出路径。
#[tauri::command]
pub fn tray_quit(app: AppHandle) -> ApiResponse<()> {
    app.state::<crate::QuitState>()
        .0
        .store(true, Ordering::SeqCst);
    app.exit(0);
    ok_void()
}

/// C16 进入轻量模式（command 壳）：**只做排队，不做转场**。
///
/// W18/F1（评审，2026-08-20）：本命令经托盘浮层按钮 invoke。**同步** command 由 tauri-macros 的
/// Blocking 路生成，在 WebView2 `WebResourceRequested` 分发栈（主线程）内直跑——若帧内执行转场，
/// 销毁的恰是**正在处理这条 IPC 的浮层自身**（deferral 未 Complete）+ 主窗，与 W18 真机证实的
/// CloseRequested 帧内销毁死锁同构。`async fn` command 被 tauri spawn 到 tokio worker（帧外，
/// tokio worker 恒非主线程），再经 `run_on_main_thread` 排回主线程**事件循环帧外**执行转场本体
/// （注意：`run_on_main_thread` 从主线程调用是内联直执——async command 保证了调用点不在主线程）。
#[tauri::command]
pub async fn tray_enter_lightweight(app: AppHandle) -> ApiResponse<()> {
    let h = app.clone();
    let h2 = h.clone();
    if let Err(e) = h.run_on_main_thread(move || enter_lightweight_transition(h2)) {
        log::warn!("轻量转场排队失败（事件循环已关闭？主窗/浮层保持原状，托盘唤出可用）：{e}");
    }
    ok_void()
}

/// 轻量转场本体：**销毁主窗 webview 释放内存，保托盘 + 核活**（≠ 关窗到托盘的 `hide()`——那只隐藏、
/// renderer 进程仍活=内存未释放）。三个入口共用：托盘浮层「进入轻量模式」command（排回腿）+
/// 主进程窗口驻留巡检（idle 腿）+ `CloseRequested` 的轻量分流（延后腿）。
/// 对齐 上游 `releaseWindowMemory` + `markLightweightModeTransition`。
///
/// **调用契约**：主线程、且不在任何窗口/WebView2 事件回调分发栈内（close 消息栈 / IPC
/// WebResourceRequested 栈都不行——帧内销毁 = W18 死锁形态）。三个调用点各自负责跳帧后
/// 再调本函数；本函数自身不再排队（主线程内再 `run_on_main_thread` 是内联直执，排了个寂寞）。
///
/// 顺序（以 destroy 成功为事务提交点）：
///  1. 置 `LightweightState`：万一销毁末窗触发 `ExitRequested`，`main.rs` 守卫据此**保核 + 阻退**（轻量恒不停核）。
///  2. 只收起浮层；是否常驻完全由 `keepTrayMenuWarm` 决定，主窗口轻量态不得替它做主。
///  3. 先把主窗生命周期标成“销毁中”，再 `destroy()`（**force**：绕过 `CloseRequested` 的拦截）。这道
///     显式状态挡住 Tauri registry 过渡期仍返回的失效 WebView，stats/logs 不再跨线程探测旧句柄。
///  4. destroy 成功才提交 main 的 stats + logs 订阅清理；失败则回滚生命周期与 LightweightState，保留
///     原页面订阅。webview 销毁不触发 `on_page_load`，成功后不清账会让 gRPC/log emitter 永续工作。
///
/// 用户经托盘浮层内明确入口、Linux 原生菜单或 Dock/任务栏唤出时，`show_main_window` 走
/// `create_main_window` 重建。
pub(crate) fn enter_lightweight_transition(app: AppHandle) {
    app.state::<crate::LightweightState>()
        .0
        .store(true, Ordering::SeqCst);
    hide_overlay(&app);
    if let Some(rt) = app.try_state::<crate::runtime::AppRuntime>() {
        rt.stats().mark_main_window_destroying();
    }
    if let Some(win) = app.get_webview_window("main") {
        match win.destroy() {
            Ok(()) => {
                if let Some(rt) = app.try_state::<crate::runtime::AppRuntime>() {
                    rt.stats().clear_window("main");
                }
                crate::commands::misc::clear_log_stream_window("main");
                crate::set_macos_dock_visible(&app, false);
            }
            Err(e) => {
                app.state::<crate::LightweightState>()
                    .0
                    .store(false, Ordering::SeqCst);
                if let Some(rt) = app.try_state::<crate::runtime::AppRuntime>() {
                    rt.stats().mark_main_window_created();
                    rt.stats().refresh_window_visible(&app);
                }
                log::warn!("轻量模式销毁主窗失败（已回滚窗口与订阅状态；托盘/核不受影响）：{e}");
            }
        }
    } else {
        // 窗已被另一条腿释放：把这次操作视为幂等成功，并兜底清掉按 label 持有的旧订阅账。
        if let Some(rt) = app.try_state::<crate::runtime::AppRuntime>() {
            rt.stats().clear_window("main");
        }
        crate::commands::misc::clear_log_stream_window("main");
        crate::set_macos_dock_visible(&app, false);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// macOS 菜单栏位置持久化（#313b）
// ─────────────────────────────────────────────────────────────────────────────

/// `NSStatusItem.autosaveName` —— **一次性拍定，跨版本永不变更**。
///
/// 系统按 `NSStatusItem Preferred Position <autosaveName>` 这个键把用户拖好的菜单栏位置存进
/// 本 app 的 preferences domain。改这个字面量 = 换一把钥匙 = **所有用户已拖好的位置当场全丢**，
/// 且丢完还不会有任何报错。故它有一道专门的棘轮测试钉着（`tray_autosave_name_is_frozen`）。
#[cfg(target_os = "macos")]
pub const TRAY_AUTOSAVE_NAME: &str = "com.polaris.app.tray";

/// 给托盘的 `NSStatusItem` 钉上 `autosaveName`，让菜单栏位置在**应用更新后**仍然保留（#313b）。
///
/// # 缺陷机制
///
/// AppKit 只有在 `NSStatusItem.autosaveName` 非空时，才把该状态项的位置写进 app 的 preferences
/// domain（键 `NSStatusItem Preferred Position <autosaveName>`）。Polaris 的托盘是**声明式**建的
/// （`tauri.conf.json` 的 `trayIcon` + 运行期 `app.tray_by_id("main")`），而 Tauri **不暴露**这个属性
/// ⇒ 全仓零 `autosaveName` ⇒ 位置没有稳定键可存，用户每次更新完都要重新拖一遍。
///
/// # 与 上游的对照
///
/// 上游 走的是 Electron 的 `new Tray(icon, guid)`：darwin 上 Electron 把 guid 赋给
/// `NSStatusItem.autosaveName`（`electron_api_tray.cc` 的 `SetAutoSaveName`），机制与这里同源，
/// 只是它有现成参数可传。Tauri 没有，只能自己摸到对象。
///
/// # 怎么摸到 NSStatusItem
///
/// `tauri::tray::TrayIcon::with_inner_tray_icon` 是唯一出口（它保证在主线程跑），
/// 拿到底层 `tray_icon::TrayIcon` 后走它的 `ns_status_item()`。**不新增依赖**：闭包返回 `bool`，
/// 故不必在本仓命名 `tray_icon` 那个 crate；`objc2-app-kit` 只是多开一个 `NSStatusItem` feature。
///
/// # 真机实测（2026-08-13，SwayMacBook-Pro / macOS 26.6.1 arm64）—— 两条预判都被证伪
///
/// **① 「不设 autosaveName 就不持久化」是错的。** AppKit 在没有 autosaveName 时会用
/// **按状态项序号编的默认键** `NSStatusItem Preferred Position Item-0`。实测那台机器上该键
/// 值为 549、写入时间 7-31，而 `.app` 在 8-10 被换过一次 —— **位置跨过一次真实更新存活了**。
/// 也就是说 `#313b` 想修的症状（更新后位置重置）在**单状态项**的 app 上根本不复现：
/// 只有一个状态项时 `Item-0` 这个序号天然稳定。
///
/// **② 「老用户会再重置一次」也是错的**（本注释此前就是这么写的，一并更正）。
/// 实测装上带 autosaveName 的版本后：
/// ```text
/// "NSStatusItem Preferred Position Item-0"               = 549;
/// "NSStatusItem Preferred Position com.polaris.app.tray" = 549;   ← 新键，同值
/// ```
/// 新键是带着**当时的实际位置**建的，不是默认值 —— `setAutosaveName` 发生在状态项已按旧键
/// 落好位之后，AppKit 把当前位置存进了新名字。迁移是免费的，没有一次性丢失。
///
/// # 所以这段代码的定位要说准：**保险，不是修复**
///
/// 今天它不解决任何已复现的症状；它买的是「将来若增加第二个状态项，位置不会因序号漂移而串位」，
/// 以及一个显式、可读的键名。代价实测为零，故保留；但**别把它当成 #313b 的『修复』记账**。
///
/// # 验证边界
///
/// 本机（Linux）编不了这条腿，编译验证靠 CI 的 macOS 矩阵腿。上面的键值是真机 SSH 只读取证
/// （起 GUI → 读 `defaults` → 退出，两轮一致）。**没验的一格**：拖动图标后新键是否随之更新
/// —— 那要人手拖，SSH 下做不了。
#[cfg(target_os = "macos")]
pub fn pin_tray_autosave_name(app: &AppHandle) {
    use objc2_foundation::NSString;

    let Some(tray) = app.tray_by_id("main") else {
        return; // 托盘没建出来 → 无对象可设，上游已有告警
    };
    let applied = tray.with_inner_tray_icon(|inner| match inner.ns_status_item() {
        Some(item) => {
            item.setAutosaveName(Some(&NSString::from_str(TRAY_AUTOSAVE_NAME)));
            true
        }
        None => false,
    });
    match applied {
        Ok(true) => {
            log::debug!("托盘 autosaveName 已钉为 {TRAY_AUTOSAVE_NAME}（菜单栏位置将跨更新保留）")
        }
        // 两种失败都不影响任何业务功能，只是位置不再持久 ⇒ 记日志、不打断启动。
        Ok(false) => log::warn!("拿不到 NSStatusItem —— 菜单栏位置不会跨更新保留（#313b）"),
        Err(e) => log::warn!("设置托盘 autosaveName 失败：{e}"),
    }
}

/// 非 macOS：Windows 任务栏与 Linux StatusNotifier 都没有「用户可拖动的状态项位置」这个概念。
#[cfg(not(target_os = "macos"))]
pub fn pin_tray_autosave_name(_app: &AppHandle) {}

#[cfg(test)]
mod autosave_name_gate {
    //! #313b 的接线门。三条断言全部**源码级**，故在 Linux 上也跑得动 ——
    //! 而这恰恰是必须的：这条腿的实现是 `#[cfg(target_os = "macos")]`，本机连编都编不了，
    //! 若判据也只能在 mac 上跑，那本地就是全盲。
    //!
    //! `include_str!` 而非运行期读盘：路径在编译期定死，不依赖 cwd，也不会因为文件被挪走
    //! 而变成一条「读不到 → 跳过」的空转。

    const TRAY_RS: &str = include_str!("tray.rs");
    const MAIN_RS: &str = include_str!("main.rs");
    const CARGO_TOML: &str = include_str!("../Cargo.toml");

    /// 从源码里取 `TRAY_AUTOSAVE_NAME` 的字面量。
    fn autosave_name_in_source() -> &'static str {
        let needle = "pub const TRAY_AUTOSAVE_NAME: &str = \"";
        let i = TRAY_RS
            .find(needle)
            .expect("找不到 TRAY_AUTOSAVE_NAME 的定义 —— 改名或挪窝了，先确认再动本门");
        let rest = &TRAY_RS[i + needle.len()..];
        &rest[..rest.find('"').expect("字面量没收口")]
    }

    /// 🔴 autosaveName 是**用户数据的钥匙**，改了等于把所有人已拖好的位置全丢，且无任何报错。
    ///
    /// 期望值是**拼出来**的，不写成同一个字面量 —— 否则一次全局改名会把常量和判据一起改掉，
    /// 门恒绿。这类「判据被自己污染」是源码级门最典型的失效方式，本门落地时就踩过一次：
    /// 第一版把期望值直接写成 `"com.polaris.app.tray"`，验红时 sed 全局替换同时改了两处，
    /// 变异**没红**。
    #[test]
    fn tray_autosave_name_is_frozen() {
        let frozen = ["com", "polaris", "app", "tray"].join(".");
        assert_eq!(
            autosave_name_in_source(),
            frozen,
            "TRAY_AUTOSAVE_NAME 的字面量变了。系统按 `NSStatusItem Preferred Position <名字>` 存位置，\
             换名字 = 换钥匙 = 所有用户已拖好的菜单栏位置当场全丢，而且丢完不会报错。\
             真要改，先想清楚这个代价再来动本门。"
        );
    }

    /// 🔴 必须在启动时调一次，且**只调一次**。
    ///
    /// 反向也锁：放进 `reconcile_tray` 那两个汇流点里会被 30s 自愈轮询反复重设 —— 不会出错，
    /// 但那是每 30 秒一次的无用功，且会掩盖「首次到底设没设上」这个信息。
    #[test]
    fn wired_once_at_boot() {
        let calls = MAIN_RS.matches("pin_tray_autosave_name(").count();
        assert_eq!(
            calls, 1,
            "main.rs 里 pin_tray_autosave_name 的调用点有 {calls} 处，应恰好 1 处（启动时一次）"
        );
        let i = MAIN_RS
            .find("pin_tray_autosave_name(")
            .expect("main.rs 没有调用 pin_tray_autosave_name —— 实现写了但没接线");
        let j = MAIN_RS
            .find("reconcile_tray(handle);")
            .expect("找不到托盘启动汇流点 —— 启动流程改过了，先确认再动本门");
        assert!(
            i < j,
            "pin_tray_autosave_name 排在托盘启动汇流点之后 —— 位置属性应在首次呈现前就位"
        );
    }

    /// 🔴 `objc2-app-kit` 必须开 `NSStatusItem` 与 `NSWindow` feature。
    ///
    /// 这条门的存在理由很具体：漏了它 **mac 腿编不过，而本机（Linux）完全看不到** ——
    /// 要等 CI 的 macOS 矩阵跑完才暴露，而那条腿是 10x 计费里最慢的一档。
    #[test]
    fn objc2_app_kit_has_nsstatusitem_feature() {
        let line = CARGO_TOML
            .lines()
            .find(|l| l.trim_start().starts_with("objc2-app-kit"))
            .expect("Cargo.toml 里找不到 objc2-app-kit");
        assert!(
            line.contains("\"NSStatusItem\"") && line.contains("\"NSWindow\""),
            "objc2-app-kit 没开 NSStatusItem/NSWindow feature —— 托盘位置或 non-activating 宿主在 mac 上编不过，\
             而本机看不到。当前行：{line}"
        );
    }
}

#[cfg(test)]
mod overlay_lifecycle_gate {
    //! 托盘浮层的结构性契约：启动期懒创建、跳出点击帧冷建、renderer-ready 后才展示、普通隐藏独立
    //! 定时回收、轻量转场提前回收。任一条漂移都会重新引入首击失效、空壳或独立 WebContent 常驻。

    use super::{should_arm_last_overlay_exit_guard, OverlayLifecycle, OverlayOpenAction};

    const TRAY_RS: &str = include_str!("tray.rs");
    const MAIN_RS: &str = include_str!("main.rs");

    #[test]
    fn overlay_is_lazy_not_built_during_setup() {
        assert!(
            !MAIN_RS.contains("tray::build_overlay("),
            "启动 setup 不得预建托盘 WebView；首次托盘点击只能按需排队创建"
        );
        let toggle_body = TRAY_RS
            .split_once("pub fn toggle_overlay")
            .and_then(|(_, rest)| rest.split_once("fn invalidate_overlay_reclaim"))
            .map(|(body, _)| body)
            .expect("must isolate toggle_overlay source");
        assert!(
            !toggle_body.contains("show_main_window"),
            "托盘浮层创建失败时应保持 no-op，不得回退打开主窗"
        );
        assert!(
            toggle_body.contains("queue_overlay_build(app, generation)")
                && !toggle_body.contains("build_overlay("),
            "托盘 Click 帧只许排冷建任务，不能同步 build WebView"
        );
        let queue_body =
            crate::commands::guard_scan::top_level_fn_body(TRAY_RS, "fn queue_overlay_build(");
        let spawn = queue_body
            .find("tauri::async_runtime::spawn")
            .expect("冷建必须先跳离托盘点击线程");
        let main = queue_body
            .find("run_on_main_thread")
            .expect("WebView build 必须排回主线程");
        let build = queue_body
            .find("build_overlay(&callback_app, generation)")
            .expect("排回主线程后必须执行按需 build");
        assert!(
            spawn < main && main < build,
            "冷建次序必须是 spawn → 排回主线程 → build"
        );
    }

    #[test]
    fn cold_overlay_waits_for_matching_renderer_generation() {
        let mut lifecycle = OverlayLifecycle::default();
        assert_eq!(
            lifecycle.request_open(false),
            OverlayOpenAction::QueueBuild { generation: 1 }
        );
        // 加载期间的重复点击合并成同一次打开意图，不让“用户因迟疑再点一下”反向取消首开。
        assert_eq!(lifecycle.request_open(false), OverlayOpenAction::AwaitReady);
        lifecycle.build_finished(1, true);
        assert!(lifecycle.mark_ready(1));
        assert!(lifecycle.should_show(1));

        lifecycle.hide();
        assert!(!lifecycle.should_show(1));
        assert_eq!(lifecycle.request_open(true), OverlayOpenAction::ShowNow);
    }

    #[test]
    fn destroyed_overlay_rejects_stale_renderer_ready() {
        let mut lifecycle = OverlayLifecycle::default();
        let OverlayOpenAction::QueueBuild { generation } = lifecycle.request_open(false) else {
            panic!("首次冷开必须排 build");
        };
        lifecycle.build_finished(generation, true);
        lifecycle.reset();
        let OverlayOpenAction::QueueBuild {
            generation: next_generation,
        } = lifecycle.request_open(false)
        else {
            panic!("销毁后的下一次打开必须创建新一代");
        };
        lifecycle.build_finished(next_generation, true);
        assert!(!lifecycle.mark_ready(generation));
        assert!(!lifecycle.should_show(generation));
        assert_eq!(
            lifecycle.request_open(true),
            OverlayOpenAction::AwaitReady,
            "旧 ready 不得把当前新窗污染成 ready"
        );
    }

    #[test]
    fn mac_overlay_contract_is_nonactivating_and_never_uses_tauri_focus() {
        let configure = crate::commands::guard_scan::top_level_fn_body(
            TRAY_RS,
            "fn configure_nonactivating_overlay(",
        );
        assert!(
            configure.contains("NSWindowStyleMask::NonactivatingPanel")
                && configure.contains("setStyleMask("),
            "mac 托盘宿主必须在展示前补 AppKit non-activating mask"
        );
        let focus = crate::commands::guard_scan::top_level_fn_body(TRAY_RS, "fn focus_overlay(");
        assert!(
            focus.contains("makeKeyAndOrderFront(None)")
                && !focus.contains("activateIgnoringOtherApps")
                && !focus.contains("win.set_focus()"),
            "mac focus 腿必须绕开 tao 附带 app activation 的 set_focus 封装"
        );
    }

    #[test]
    fn renderer_ready_is_the_only_cold_show_commit_point() {
        let build = crate::commands::guard_scan::top_level_fn_body(TRAY_RS, "fn build_overlay(");
        assert!(
            !build.contains(".show()"),
            "冷建函数不得在 renderer ready 前展示窗口"
        );
        let ready = crate::commands::guard_scan::top_level_fn_body(
            TRAY_RS,
            "pub async fn tray_renderer_ready(",
        );
        assert!(
            ready.contains("lifecycle.mark_ready(generation)")
                && ready.contains("run_on_main_thread")
                && ready.contains("show_ready_overlay"),
            "renderer ready 必须代次校验后、跳出 IPC 帧排回主线程再 show"
        );
    }

    #[test]
    fn tray_retention_is_independent_from_main_lightweight_setting() {
        assert!(
            TRAY_RS.contains("schedule_overlay_reclaim(app);")
                && TRAY_RS.contains("TRAY_IDLE_RECLAIM_SECS"),
            "普通 hide 必须自行排程回收，不能只靠主窗轻量模式顺带清理"
        );
        let body = crate::commands::guard_scan::top_level_fn_body(
            TRAY_RS,
            "pub(crate) fn enter_lightweight_transition(",
        );
        assert!(
            body.contains("hide_overlay(&app);") && !body.contains("destroy_overlay(&app);"),
            "主窗口轻量转场只能收起托盘；销毁与否必须继续服从独立 warm 偏好"
        );
    }

    #[test]
    fn main_window_destroy_is_a_transactional_lifecycle_boundary() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            TRAY_RS,
            "pub(crate) fn enter_lightweight_transition(",
        );
        let destroying = body
            .find("mark_main_window_destroying()")
            .expect("destroy 前必须先关闭主窗口生命周期门");
        let destroy = body
            .find("win.destroy()")
            .expect("轻量态必须真正销毁主 WebView");
        let clear = body
            .find("rt.stats().clear_window(\"main\")")
            .expect("destroy 成功后必须释放 stats 订阅");
        assert!(destroying < destroy, "生命周期门必须先于平台 destroy 关闭");
        assert!(
            clear > destroy,
            "订阅清理是 destroy 成功后的事务提交；提前清会让失败回滚后的活页面永久断流"
        );
        assert!(
            body.contains("rt.stats().mark_main_window_created()")
                && body.contains("store(false, Ordering::SeqCst)"),
            "destroy 失败必须同时回滚窗口生命周期与 LightweightState"
        );
    }

    /// W18（2026-08-19 真机）：CloseRequested 帧内不得同步调 `tray_enter_lightweight`（内含
    /// 主窗+浮层两个 WebView 的 `destroy()`）——Windows 上在窗口自身 close 消息分发栈里销毁
    /// WebView2 会楔死消息泵（首实例托盘全死、双击再起第二进程双图标）。帧内只许轻操作
    /// （hide），转场必须「跳离主线程 → run_on_main_thread 排回帧外」。
    ///
    /// 次序即语义：`win.hide()`（帧内即时视觉关闭）→ `async_runtime::spawn`（跳离）→
    /// `run_on_main_thread`（排回）→ `tray_enter_lightweight`（帧外销毁）。任何一步次序倒换
    /// （尤其把 tray_enter_lightweight 挪回 spawn 之前 = 帧内直调）本条转红。
    #[test]
    fn lightweight_transition_is_deferred_out_of_close_frame() {
        let arm = MAIN_RS
            .split_once("CloseAction::EnterLightweight => {")
            .and_then(|(_, rest)| rest.split_once("CloseAction::QuitApp => {"))
            .map(|(arm, _)| arm)
            .expect("必须能切出 CloseRequested 的 EnterLightweight 分支");
        // 针带语法特征（限定路径/点调用/实参形态），避免命中写进 arm 注释里的裸词。
        let hide_at = arm
            .find("win.hide()")
            .expect("帧内必须先隐藏（即时关闭视觉）");
        let spawn_at = arm
            .find("tauri::async_runtime::spawn")
            .expect("必须跳离主线程（run_on_main_thread 从主线程调用是内联直执）");
        let queue_at = arm
            .find(".run_on_main_thread(")
            .expect("必须经 run_on_main_thread 排回主线程");
        let enter_at = arm
            .find("enter_lightweight_transition(h2)")
            .expect("转场入口必须在位（帧外闭包内，以 h2 实参形态）");
        // F2（评审）：排回闭包内、销毁之前必须复核主窗未被重新唤出（排队饥饿时用户可能已唤回）。
        let recheck_at = arm
            .find("win.is_visible()")
            .expect("销毁前必须复核主窗可见性（迟到销毁不得杀用户刚唤出的窗）");
        assert!(
            hide_at < spawn_at
                && spawn_at < queue_at
                && queue_at < recheck_at
                && recheck_at < enter_at,
            "轻量转场次序必须为 hide → spawn → run_on_main_thread → is_visible 复核 → enter（帧内直调即 W18 死锁形态）"
        );
    }

    /// W18/F1（评审）：`tray_enter_lightweight` command 是托盘浮层按钮的 invoke 入口——
    /// 同步 command 跑在 WebView2 IPC 分发栈（主线程）内，帧内销毁浮层自身即 W18 死锁
    /// 同构形态。command 必须是 `async fn`（tauri spawn 到 tokio worker = 帧外）且体内
    /// 只排队（`run_on_main_thread` → `enter_lightweight_transition`），不得直跑销毁。
    /// 变异锁：改回同步直调 / 删排队直调本体 → 本条红。
    #[test]
    fn lightweight_command_defers_out_of_ipc_frame() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            TRAY_RS,
            "pub async fn tray_enter_lightweight(",
        );
        assert!(
            !body.contains("win.destroy()"),
            "command 体内不得直跑销毁（IPC 分发栈内）"
        );
        let queue_at = body
            .find(".run_on_main_thread(")
            .expect("command 必须经 run_on_main_thread 排回主线程帧外");
        let enter_at = body
            .find("enter_lightweight_transition(h")
            .expect("转场必须经本体 fn（帧外闭包内）");
        assert!(queue_at < enter_at, "次序必须是排队在先、转场在排回闭包内");
    }

    #[test]
    fn last_overlay_reclaim_preserves_tray_residency_only_for_the_last_window() {
        assert!(should_arm_last_overlay_exit_guard(1, true));
        assert!(!should_arm_last_overlay_exit_guard(0, true));
        assert!(!should_arm_last_overlay_exit_guard(2, true));
        assert!(!should_arm_last_overlay_exit_guard(1, false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> PhysicalRect {
        PhysicalRect { x, y, w, h }
    }
    // 单显示器 2000×1200@原点，浮层 536×700，gap=4。
    const SCREEN: ScreenArea = ScreenArea {
        left: 0,
        top: 0,
        right: 2000,
        bottom: 1200,
    };
    const WIN: (u32, u32) = (536, 700);
    const GAP: i32 = 4;

    #[test]
    fn overlay_retention_switch_only_changes_the_active_timer_when_needed() {
        use OverlayRetentionAction::{CancelReclaim, None, ScheduleReclaim};

        // 任意无关配置保存都不能续期现有回收计时器。
        assert_eq!(overlay_retention_action(false, false, true), None);
        assert_eq!(overlay_retention_action(true, true, true), None);
        // 开启 warm 必须取消在飞回收，无论浮层当前是否存在/可见。
        assert_eq!(overlay_retention_action(false, true, false), CancelReclaim);
        assert_eq!(overlay_retention_action(false, true, true), CancelReclaim);
        // 关闭 warm：隐藏态立即恢复计时，可见态交给下一次 hide 挂计时。
        assert_eq!(overlay_retention_action(true, false, true), ScheduleReclaim);
        assert_eq!(overlay_retention_action(true, false, false), None);
    }

    #[test]
    fn tray_event_rect_is_already_physical() {
        let event_rect = tauri::Rect {
            position: PhysicalPosition::new(-1180, 2160).into(),
            size: PhysicalSize::new(32, 32).into(),
        };
        assert_eq!(
            physical_tray_rect(event_rect),
            Some(rect(-1180.0, 2160.0, 32.0, 32.0))
        );
    }

    #[test]
    fn top_edge_centers_on_icon_and_hugs_menu_bar() {
        // 图标在菜单栏中右：x=1000 w=44，y=0 h=48。
        let work = ScreenArea { top: 48, ..SCREEN };
        let (x, y) = overlay_xy(
            Some(rect(1000.0, 0.0, 44.0, 48.0)),
            work,
            WIN,
            GAP,
            TrayEdge::Top,
        );
        assert_eq!(x, 1022 - 268); // icon_cx(1022) - win_w/2(268) = 754，水平居中图标
        assert_eq!(y, 48 + GAP); // 图标下沿 + gap，紧贴菜单栏（不是屏顶+28）
    }

    #[test]
    fn bottom_edge_places_above_icon() {
        let work = ScreenArea {
            bottom: 1160,
            ..SCREEN
        };
        let (_, y) = overlay_xy(
            Some(rect(1000.0, 1160.0, 40.0, 40.0)),
            work,
            WIN,
            GAP,
            TrayEdge::Bottom,
        );
        assert_eq!(y, 1160 - 700 - GAP);
    }

    #[test]
    fn left_edge_places_right_of_icon() {
        let work = ScreenArea { left: 48, ..SCREEN };
        let (x, y) = overlay_xy(
            Some(rect(0.0, 500.0, 48.0, 40.0)),
            work,
            WIN,
            GAP,
            TrayEdge::Left,
        );
        assert_eq!(x, 48 + GAP);
        assert_eq!(y, 520 - 350);
    }

    #[test]
    fn right_edge_places_left_of_icon() {
        let work = ScreenArea {
            right: 1952,
            ..SCREEN
        };
        let (x, y) = overlay_xy(
            Some(rect(1952.0, 500.0, 48.0, 40.0)),
            work,
            WIN,
            GAP,
            TrayEdge::Right,
        );
        assert_eq!(x, 1952 - 536 - GAP);
        assert_eq!(y, 520 - 350);
    }

    #[test]
    fn degenerate_anchor_falls_back_to_edge_corner() {
        let (x, y) = overlay_xy(
            Some(rect(0.0, 0.0, 0.0, 0.0)),
            SCREEN,
            WIN,
            GAP,
            TrayEdge::Top,
        );
        assert_eq!(x, 2000 - 536 - GAP);
        assert_eq!(y, GAP);
    }

    #[test]
    fn clamps_to_same_negative_coordinate_work_area() {
        let work = ScreenArea {
            left: -2520,
            top: 40,
            right: -48,
            bottom: 1400,
        };
        let (x, y) = overlay_xy(
            Some(rect(-60.0, 1360.0, 40.0, 40.0)),
            work,
            WIN,
            GAP,
            TrayEdge::Bottom,
        );
        assert_eq!(x, -48 - 536);
        assert_eq!(y, 1360 - 700 - GAP);
    }

    #[test]
    fn reserved_work_edge_breaks_vertical_taskbar_corner_tie() {
        let work = ScreenArea { left: 48, ..SCREEN };
        assert_eq!(
            resolve_tray_edge(
                Some(rect(0.0, 1160.0, 48.0, 40.0)),
                SCREEN,
                work,
                TrayEdge::Bottom,
            ),
            TrayEdge::Left
        );
    }

    #[test]
    fn auto_hidden_taskbar_uses_anchor_with_platform_tie_break() {
        assert_eq!(
            resolve_tray_edge(
                Some(rect(1960.0, 1160.0, 40.0, 40.0)),
                SCREEN,
                SCREEN,
                TrayEdge::Bottom,
            ),
            TrayEdge::Bottom
        );
        assert_eq!(
            resolve_tray_edge(
                Some(rect(1960.0, 0.0, 40.0, 40.0)),
                SCREEN,
                SCREEN,
                TrayEdge::Bottom,
            ),
            TrayEdge::Top
        );
    }

    #[test]
    fn edge_boot_script_sets_stable_css_contract() {
        let script = tray_edge_boot_script(TrayEdge::Right);
        assert!(script.contains("window.__POLARIS_TRAY_EDGE__ = 'right'"));
        assert!(script.contains("data-tray-edge"));
        assert!(script.contains("__POLARIS_SET_TRAY_EDGE__"));
    }

    #[test]
    fn placement_uses_anchor_monitor_and_work_area_not_overlay_current_screen() {
        const TRAY_RS: &str = include_str!("tray.rs");
        let store = crate::commands::guard_scan::top_level_fn_body(TRAY_RS, "fn store_anchor(");
        assert!(!store.contains("current_monitor"));
        assert!(!store.contains("to_physical"));

        let placement =
            crate::commands::guard_scan::top_level_fn_body(TRAY_RS, "fn overlay_placement(");
        assert!(placement.contains("app.monitor_from_point("));
        assert!(placement.contains("monitor.work_area()"));

        let reposition = crate::commands::guard_scan::top_level_fn_body(TRAY_RS, "fn reposition(");
        assert!(!reposition.contains("current_monitor"));
        assert!(reposition.contains("placement.work_area"));
    }

    // ── 托盘原生文案：五语齐备 + 值→键映射 ─────────────────────────────────────
    //
    // 语言**解析**的门在 `crate::i18n`（那是纯函数、与托盘无关）。这里只守托盘自己的两件事：
    // ① tooltip 的拼装形状；② `config` 取值域 → 文案键的映射不得塌成同一档。

    #[test]
    fn tooltip_is_brand_plus_localized_status() {
        assert_eq!(
            tooltip_text(Lang::ZhCN, TrayState::Connected),
            "Polaris — 已连接"
        );
        assert_eq!(
            tooltip_text(Lang::EnUS, TrayState::Idle),
            "Polaris — Disconnected"
        );
        assert_eq!(
            tooltip_text(Lang::Ru, TrayState::Error),
            "Polaris — Ошибка подключения"
        );
        // 繁中此前与简中同归一档（旧 TrayLang 二态），这条钉住它现在是**独立**的一档。
        assert_ne!(
            tooltip_text(Lang::ZhTW, TrayState::Connecting),
            tooltip_text(Lang::EnUS, TrayState::Connecting)
        );
        // 五语种四态：一条都不许回落成键名（回落 = 那一格漏译）。
        for lang in crate::i18n::SUPPORTED {
            for st in [
                TrayState::Idle,
                TrayState::Connecting,
                TrayState::Connected,
                TrayState::Error,
            ] {
                let tip = tooltip_text(lang, st);
                assert!(
                    tip.starts_with("Polaris — ") && !tip.contains("tray.status"),
                    "{lang:?}/{st:?} 的 tooltip 回落成了键名：{tip}"
                );
            }
        }
    }

    /// 四态必须映射到**四个不同**的键：塌成一个的症状是「连接中 / 连接异常 / 未连接」在 tooltip 上
    /// 长得一样，而图标是对的 ⇒ 功能「正常」、纯 UI 撒谎。
    #[test]
    fn tooltip_status_keys_are_four_distinct_keys() {
        let mut keys: Vec<&str> = [
            TrayState::Idle,
            TrayState::Connecting,
            TrayState::Connected,
            TrayState::Error,
        ]
        .into_iter()
        .map(tooltip_status_key)
        .collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), 4, "托盘四态的文案键塌了");
    }

    /// 子菜单三档若有两档文案相同，用户根本分不出点的是哪个（而 id 是对的 ⇒ 功能"正常"、纯 UI 撒谎）。
    /// 五语种逐个查——旧版本只查 zh/en，ru/fa 的漏译在那道门下恒绿。
    #[test]
    fn takeover_and_routing_labels_are_distinct_per_value_in_all_languages() {
        for lang in crate::i18n::SUPPORTED {
            for (name, vals, f) in [
                (
                    "接管方式",
                    TAKEOVER_KINDS,
                    takeover_key as fn(&str) -> &'static str,
                ),
                (
                    "分流策略",
                    ROUTING_MODES,
                    routing_key as fn(&str) -> &'static str,
                ),
            ] {
                let mut labels: Vec<String> = vals.iter().map(|v| t(lang, f(v))).collect();
                let n = labels.len();
                assert!(
                    labels.iter().all(|l| !l.starts_with("tray.")),
                    "{name} 有一档回落成了键名（{lang:?}）：{labels:?}"
                );
                labels.sort_unstable();
                labels.dedup();
                assert_eq!(labels.len(), n, "{name}三档文案必须两两不同（{lang:?}）");
            }
        }
    }

    /// 值域外的取值必须落到与浮层同一个默认档（`smart` / `systemProxy`），**不得**回落成别的档 ——
    /// 那会让托盘显示的当前档与真实配置不符。
    #[test]
    fn unknown_config_values_fall_back_to_the_same_default_as_the_overlay() {
        assert_eq!(takeover_key("no-such-kind"), takeover_key("systemProxy"));
        assert_eq!(routing_key("no-such-mode"), routing_key("smart"));
    }

    // ── A1：跨窗导航白名单 ───────────────────────────────────────────────────────

    #[test]
    fn tray_screen_whitelist_only_admits_registered_targets() {
        assert_eq!(normalize_tray_screen("settings"), Some("settings"));
        assert_eq!(
            normalize_tray_screen(" settings "),
            Some("settings"),
            "容忍空白"
        );
        // 通道**不是**通用路由：未登记值一律拒绝（拒绝 = 只显示主窗、不导航，而不是把串透传出去）。
        for evil in ["", "home", "/settings", "Settings", "nodes", "../settings"] {
            assert_eq!(normalize_tray_screen(evil), None, "{evil} 不该被放行");
        }
    }

    #[test]
    fn tray_screen_boot_script_only_ever_carries_whitelisted_values() {
        // 种子脚本是拼进 JS 字面量的 ⇒ 载荷必须是白名单产物。这条把「有人改成透传入参」钉死：
        // 透传后 `normalize_tray_screen` 的返回类型就不再是 `&'static str`，本断言的形态即失效。
        let s = normalize_tray_screen("settings").expect("白名单里有 settings");
        assert_eq!(
            tray_screen_boot_script(s),
            "window.__POLARIS_TRAY_SCREEN__ = 'settings';\n"
        );
    }

    // ── 投递腿：种子腿与事件腿**不互斥**（2026-07-28 复审的早启动竞态）──────────────

    #[test]
    fn pending_is_always_written_when_a_target_screen_is_given() {
        // 被守的缺陷：原状是 `if !main_alive { set_pending_screen(...) }`。于是
        // 「主窗存在但 webview 还没挂上 EVENT_TRAY_OPEN_SCREEN 订阅」这一格里两条腿都够不着 ——
        // 事件 emit 出去没人听、pending 又没写 ⇒ 窗开了、屏没跳，且**静默**。
        // 把 write_pending 改回 `!main_alive` 这条即转红。
        assert!(tray_show_main_legs(true, true).write_pending);
        assert!(tray_show_main_legs(true, false).write_pending);
    }

    #[test]
    fn event_leg_only_fires_when_the_main_window_already_exists() {
        // 窗不存在时 emit 必丢（emit 发生在 webview 装载之前），发了也只是噪声。
        assert!(tray_show_main_legs(true, true).emit_event);
        assert!(!tray_show_main_legs(true, false).emit_event);
    }

    #[test]
    fn no_target_screen_lights_no_leg() {
        // 无参 `tray_show_main()`（「显示主窗口」）必须与本改动前逐字节相同：不写 pending、不 emit。
        // 少了这条，每次点「显示主窗口」都会往 pending 里塞东西 → 下次建窗被送去设置页。
        assert_eq!(
            tray_show_main_legs(false, true),
            TrayShowMainLegs {
                write_pending: false,
                emit_event: false
            }
        );
        assert_eq!(
            tray_show_main_legs(false, false),
            TrayShowMainLegs {
                write_pending: false,
                emit_event: false
            }
        );
    }

    // ── 「检查更新」结果文案：五语齐备 + 成功/失败不得同形 ────────────────────────

    #[test]
    fn update_result_labels_are_localized_and_distinguishable() {
        use crate::i18n::key as k;
        for lang in crate::i18n::SUPPORTED {
            for key in [
                k::NATIVE_UPDATE_NOTIFY_TITLE,
                k::TRAY_UP_TO_DATE,
                k::TRAY_UPDATE_CHECK_FAILED,
                k::NATIVE_UNKNOWN_ERROR,
                k::NATIVE_UPDATE_INFO_INCOMPLETE,
                k::NATIVE_UPDATE_POPUP_FAILED,
            ] {
                let s = t(lang, key);
                assert!(!s.trim().is_empty(), "{lang:?} 的 {key} 是空串");
                assert_ne!(s, key, "{lang:?} 的 {key} 回落成了键名 = 那一格漏译");
            }
            // B5 反伪造：失败绝不能显示成「已是最新」。
            assert_ne!(
                t(lang, k::TRAY_UP_TO_DATE),
                t(lang, k::TRAY_UPDATE_CHECK_FAILED)
            );
        }
    }

    // ── B：原生面主题折算 ────────────────────────────────────────────────────────

    #[test]
    fn explicit_theme_wins_over_system() {
        // 显式档不看系统（这正是「设置里选了浅色、启动仍闪深色」的修复点）。
        assert!(!resolve_native_dark(Some("light"), Some(true)));
        assert!(resolve_native_dark(Some("dark"), Some(false)));
        assert!(
            !resolve_native_dark(Some(" light "), Some(true)),
            "容忍空白"
        );
    }

    #[test]
    fn system_theme_follows_os_and_falls_back_to_dark() {
        assert!(resolve_native_dark(Some("system"), Some(true)));
        assert!(!resolve_native_dark(Some("system"), Some(false)));
        assert!(!resolve_native_dark(None, Some(false)), "未设 = 跟随系统");
        // 探不到系统明暗（首次建主窗时一个窗都没有）→ 深色，= 本改动前的既有行为，不制造新跳变。
        assert!(resolve_native_dark(Some("system"), None));
        assert!(resolve_native_dark(None, None));
        assert!(resolve_native_dark(Some("weird-value"), None));
    }

    #[test]
    fn theme_colors_differ_between_light_and_dark() {
        // 变异锁：把 light/dark 映射到同一个色（例如"先都用深色，回头再说"）必须转红 —— 那等于 B 没做。
        assert_ne!(window_bg_color(true).0, window_bg_color(false).0);
        assert_ne!(surface_color(true).0, surface_color(false).0);
        // 深色底必须真的比浅色底暗（防把两个色写反）。
        assert!(window_bg_color(true).0 < window_bg_color(false).0);
        assert!(surface_color(true).0 < surface_color(false).0);
    }

    #[test]
    fn theme_boot_script_seeds_but_does_not_override() {
        let dark = theme_boot_script(true);
        assert!(dark.contains("var t = 'dark';"));
        assert!(theme_boot_script(false).contains("var t = 'light';"));
        // 只播种不接管：属性已存在就不写 —— 否则 DOMContentLoaded 那次回调会把 AppShell 刚设的
        // 运行期真值覆盖回启动值（用户在设置里改主题后，切回主窗会闪回旧主题）。
        assert!(
            dark.contains("!el.hasAttribute('data-theme')"),
            "缺 hasAttribute 守卫 = 会与 AppShell 的主题 effect 抢写"
        );
        // 首帧之前就要落属性：只挂 DOMContentLoaded 而不立即执行一次 = FOUC 照旧。
        assert!(
            dark.contains("apply();\n"),
            "必须立即执行一次，不能只挂事件"
        );
    }

    // ── A7：FakeIP-TUN 待纠正快照（与前端 applyFakeIpTunEntry 同一组分支）───────────

    #[test]
    fn fake_ip_tun_entry_corrects_only_when_entering_tun_with_pending_flag() {
        use serde_json::json;
        let mut cfg = json!({
            "proxyModeType": "tun",
            "dnsConfig": { "enableFakeIp": false, "fakeIpTunAutoEnable": true }
        });
        assert!(
            apply_fake_ip_tun_entry(&mut cfg),
            "真把 false 改回 true → 返 true"
        );
        assert_eq!(cfg["dnsConfig"]["enableFakeIp"], json!(true));
        assert_eq!(
            cfg["dnsConfig"]["fakeIpTunAutoEnable"],
            json!(false),
            "flag 一次性消费"
        );
    }

    #[test]
    fn fake_ip_tun_entry_consumes_flag_without_reporting_correction() {
        use serde_json::json;
        // flag 开着但 enableFakeIp 本就是 true → 只消费 flag，不报"纠正过"（不打扰用户）。
        let mut cfg = json!({
            "proxyModeType": "tun",
            "dnsConfig": { "enableFakeIp": true, "fakeIpTunAutoEnable": true }
        });
        assert!(!apply_fake_ip_tun_entry(&mut cfg));
        assert_eq!(cfg["dnsConfig"]["fakeIpTunAutoEnable"], json!(false));
    }

    #[test]
    fn fake_ip_tun_entry_leaves_non_tun_and_unflagged_configs_untouched() {
        use serde_json::json;
        // 非 tun：flag 存续到真进 TUN 才消费（systemProxy→manual→tun 的绕行仍应纠正）。
        let mut cfg = json!({
            "proxyModeType": "manual",
            "dnsConfig": { "enableFakeIp": false, "fakeIpTunAutoEnable": true }
        });
        let before = cfg.clone();
        assert!(!apply_fake_ip_tun_entry(&mut cfg));
        assert_eq!(cfg, before, "非 tun 一律不动（含不得提前消费 flag）");

        // 无 flag（用户手改过 DNS 开关 = 撤销意图）：进 TUN 也不得自动开回来。
        let mut cfg = json!({
            "proxyModeType": "tun",
            "dnsConfig": { "enableFakeIp": false, "fakeIpTunAutoEnable": false }
        });
        let before = cfg.clone();
        assert!(!apply_fake_ip_tun_entry(&mut cfg));
        assert_eq!(cfg, before, "flag 已撤销 → 不得误纠正");

        // 连 dnsConfig 都没有：不 panic、不凭空造字段。
        let mut cfg = json!({ "proxyModeType": "tun" });
        assert!(!apply_fake_ip_tun_entry(&mut cfg));
        assert_eq!(cfg, json!({ "proxyModeType": "tun" }));
    }
}
