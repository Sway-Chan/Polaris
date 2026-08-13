//! mini 更新弹窗的 Tauri 窗口装配（`updater::popup` 纯逻辑的宿主实现）。
//!
//! 移植自 上游 `UpdateService.createUpdatePopup`（`UpdateService.ts:541-631`），
//! 但**建窗路径的初始态改由文档注入**而非建窗后 push IPC —— 理由见 `updater::popup` 模块文档
//! 「#300/#301 的不变式需要一个『结构上无法违反』的落点」。
//!
//! # 窗口参数对照（Polaris → Tauri）
//!
//! | 上游 `BrowserWindow` | Tauri `WebviewWindowBuilder` | 说明 |
//! |---|---|---|
//! | `frame: false` | `.decorations(false)` | 无边框 |
//! | `alwaysOnTop: true` | `.always_on_top(true)` | 置顶 toast |
//! | `skipTaskbar: true` | `.skip_taskbar(true)` | 不进任务栏 |
//! | `show: false` + `showInactive()` | `.visible(false)` + `show()` | 不抢焦点 |
//! | `transparent: false` | `.transparent(false)` | 上游注释：Win/Linux 透明窗有鼠标穿透 bug |
//! | `backgroundColor: dark?'#161C24':'#FFFFFF'` | `.background_color(...)` | 防白闪 |
//! | `resizable/minimizable/maximizable: false` | `.resizable(false)` 等 | 固定尺寸 |
//! | `setWindowOpenHandler(deny)` | 无子窗入口（Tauri 默认不开） | 拒绝子窗 |
//! | `did-finish-load` 重放 | `.on_page_load(Finished)` 重放 | 崩溃/reload 自愈 |
//!
//! # 与主窗的隔离
//!
//! 弹窗用**独立 label**（[`POPUP_LABEL`]）与独立页面入口（`update-popup.html`），
//! 不复用主窗的 `index.html` —— 否则 380×184 的 toast 里会挂载整个 React 应用（含 i18n/路由/
//! 全部 provider），既慢又会让主窗的白屏自愈门（`window_health.rs` 只认 label=="main"）误判。

use std::sync::Mutex;

use polaris_updater::popup::{
    PopupBootstrap, PopupSession, PopupTransport, UpdatePopupState, POPUP_WIDTH,
};
use tauri::{AppHandle, Emitter, Listener, Manager, WebviewUrl, WebviewWindowBuilder};

/// 弹窗窗口 label（Tauri 内唯一标识；主窗为 `"main"`）。
pub const POPUP_LABEL: &str = "update-popup";

/// 状态下发事件名（主 → 弹窗 renderer）。
///
/// 注：这**不是** `ipc-channels.ts` 里的 command 名（`update_popup_state` 是 renderer→主 的 command）；
/// 本常量是反向的 event 名。二者名字相近但方向相反，勿混。
pub const EVENT_POPUP_STATE: &str = "event:updatePopupState";

/// 弹窗页面入口（vite 多入口产物；dev 态由 devUrl 提供）。
const POPUP_PAGE: &str = "update-popup.html";

/// Tauri 实现的 [`PopupTransport`]：经 `emit_to(label, ...)` 推状态、经窗口句柄改窗高。
pub struct TauriPopupTransport {
    app: AppHandle,
}

impl std::fmt::Debug for TauriPopupTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TauriPopupTransport")
            .finish_non_exhaustive()
    }
}

impl TauriPopupTransport {
    #[must_use]
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl PopupTransport for TauriPopupTransport {
    fn send_state(&self, state: &UpdatePopupState) -> Result<(), String> {
        // emit_to 只投给弹窗 label —— 主窗不该收到弹窗态（避免主窗 listener 误响应）。
        self.app
            .emit_to(POPUP_LABEL, EVENT_POPUP_STATE, state)
            .map_err(|e| format!("推送弹窗状态失败: {e}"))
    }

    fn set_content_height(&self, height: u32) -> Result<(), String> {
        let Some(win) = self.app.get_webview_window(POPUP_LABEL) else {
            // 窗口已销毁：对齐上游 `sendPopupState` 的 destroyed 检查（静默跳过，不报错升级）。
            return Ok(());
        };
        win.set_size(tauri::LogicalSize::new(
            f64::from(POPUP_WIDTH),
            f64::from(height),
        ))
        .map_err(|e| format!("调整弹窗高度失败: {e}"))
    }
}

/// 建窗（或复用）并下发状态。
///
/// 移植自 `UpdateService.createUpdatePopup:541-631`，两条路径逐条对齐：
///
///  1. **复用分支**（`:542-545`）：窗口已存在 → `session.reuse(state)`（= 上游 `sendPopupState` + 早返回）。
///  2. **新建分支**（`:552-631`）：`session.open(state)` 产出 bootstrap → 建窗（注入 init script）→ 显示。
///     **初始态随文档注入，不经 IPC** → #300 的「建窗但从未下发初始态」在此不可达。
///
/// # Errors
///
/// 建窗失败（Tauri builder / webview 创建）。
pub fn show_update_popup(
    app: &AppHandle,
    session_slot: &Mutex<Option<PopupSession<TauriPopupTransport>>>,
    state: UpdatePopupState,
) -> Result<(), String> {
    let mut slot = session_slot
        .lock()
        .map_err(|e| format!("popup 锁中毒: {e}"))?;

    // ── 复用分支 ── 窗口仍在 → 只下发状态（= 上游早返回，不重建）。
    if app.get_webview_window(POPUP_LABEL).is_some() {
        if let Some(session) = slot.as_mut() {
            return session.reuse(state);
        }
        // 窗口在、会话没了（理论上不可达：二者同生命周期）。窗口无人可驱动 = #300 的白屏形态。
        // 与其带着一个驱动不了的窗继续跑，不如关掉它走新建分支重建（自愈优于挂死）。
        log::warn!("弹窗窗口存在但会话缺失：关窗重建（避免无人驱动的挂死窗）");
        if let Some(w) = app.get_webview_window(POPUP_LABEL) {
            let _ = w.close();
        }
    }

    // ── 新建分支 ──
    // open() 是 bootstrap 的唯一产地，且必然 seed last_state：拿不到 script 就建不出窗，
    // 于是「建窗但没初始态」写不出来（#300 不变式的结构性落点）。
    let mut session = PopupSession::new(TauriPopupTransport::new(app.clone()));
    let boot: PopupBootstrap = session.open(state);

    build_popup_window(app, &boot)?;
    *slot = Some(session);
    Ok(())
}

/// 按 bootstrap 建出弹窗窗口。
fn build_popup_window(app: &AppHandle, boot: &PopupBootstrap) -> Result<(), String> {
    // 主题接线（B）：读 `config.uiTheme`（显式 light/dark 直接定；`system`/未设跟随 OS，由任一现存窗口
    // 的 `Window::theme()` 探得——Tauri 2.11 无 app 级 theme getter）。
    // 此前是 `let dark = true;` 硬编码：浅色用户每次弹更新提示都先闪一格 #161C24 深底再被 webview 盖掉，
    // 而这个窗**恰恰**是为「防白闪」才设实底色的 —— 防错了方向的那一半人。
    //
    // ⚠️ **同一个 `dark` 必须同时喂给原生底色与页面主题**。只折原生底色、页面 CSS 留死深色的那一版
    // （2026-07-28 复审抓出）只是把闪烁**方向反转**：浅色用户从「深→深无闪」变成「白底闪一格→深色卡片」。
    // 页面档由 `theme_boot_script` 注入 `<html data-theme>` 驱动（与主窗同款注入手法，不受页面
    // CSP `script-src 'self'` 限制），`ui/src/update-popup/style.css` 两档取值与这里的
    // `surface_color` 同源。**别把页面改成只跟 `prefers-color-scheme`**：那是 OS 明暗，而这里的真值是
    // `config.uiTheme` —— `uiTheme=light` + OS 深色时两者相反，闪烁会原样复发。
    let dark = crate::tray::native_dark(app);
    let bg = crate::tray::surface_color(dark);

    let win = WebviewWindowBuilder::new(app, POPUP_LABEL, WebviewUrl::App(POPUP_PAGE.into()))
        .title("Polaris Update")
        // 初始态注入文档：页面 boot 时同步可读 window.__POLARIS_UPDATE_POPUP_INITIAL__。
        // **这一行是 #300 整类 bug 的结构性解**：无 script 即无页面，无页面即无窗。
        .initialization_script(&boot.init_script)
        // 页面主题种子：与上面的 `bg` 出自同一个 `dark`（见该绑定上方注释）。
        .initialization_script(crate::tray::theme_boot_script(dark))
        .inner_size(f64::from(boot.width), f64::from(boot.height))
        .resizable(false)
        .minimizable(false)
        .maximizable(false)
        .decorations(false)
        // 上游注释：透明窗在 Win/Linux 有鼠标穿透 bug → 恒不透明 + 主题化底色防白闪。
        .transparent(false)
        .background_color(bg)
        .always_on_top(true)
        .skip_taskbar(true)
        // 先隐藏：定位完再 show，避免「先出现在错位置再跳」。
        .visible(false)
        .build()
        .map_err(|e| format!("建更新弹窗失败: {e}"))?;

    position_popup_bottom_right(&win, boot);

    // did-finish-load 等价：页面每次加载完成（含 reload / renderer 崩溃重建）重放 last_state。
    // 用 `.on(...)`（非 once）语义：崩溃自愈靠的就是重复重放。
    // 注：本移植下初始态已随文档注入，重放是**第二层**兜底（上游那里它是唯一一层，故 #300 一击致命）。
    let app_for_load = app.clone();
    win.listen("tauri://webview-created", move |_| {
        if let Some(rt) = app_for_load.try_state::<crate::runtime::AppRuntime>() {
            if let Ok(slot) = rt.updater().popup().lock() {
                if let Some(s) = slot.as_ref() {
                    match s.replay() {
                        Ok(true) => log::debug!("弹窗重放 last_state 完成"),
                        // 不可达：open 必然 seed。真出现说明不变式被破坏 → 大声报（这正是 #300 的现场）。
                        Ok(false) => log::error!("弹窗重放无状态可放 —— #300 不变式被破坏！"),
                        Err(e) => log::warn!("弹窗重放失败 {e}"),
                    }
                }
            }
        }
    });

    win.show().map_err(|e| format!("显示更新弹窗失败: {e}"))?;
    Ok(())
}

/// 定位到工作区右下角（= 上游 `positionPopup` 的角落 toast 定位）。
///
/// 取当前显示器工作区（排除任务栏/Dock），留 [`POPUP_MARGIN`] 边距。
/// 拿不到显示器信息 → 保持默认居中（**不猜坐标**：错位的窗比居中的窗好不到哪去）。
fn position_popup_bottom_right(win: &tauri::WebviewWindow, boot: &PopupBootstrap) {
    const POPUP_MARGIN: i32 = 16;
    let Ok(Some(monitor)) = win.current_monitor() else {
        log::debug!("取不到显示器信息：弹窗保持默认位置");
        return;
    };
    let scale = monitor.scale_factor();
    let size = monitor.size().to_logical::<f64>(scale);
    let pos = monitor.position().to_logical::<f64>(scale);
    #[allow(clippy::cast_possible_truncation)]
    let x = (pos.x + size.width) as i32 - boot.width as i32 - POPUP_MARGIN;
    #[allow(clippy::cast_possible_truncation)]
    let y = (pos.y + size.height) as i32 - boot.height as i32 - POPUP_MARGIN;
    if let Err(e) = win.set_position(tauri::LogicalPosition::new(f64::from(x), f64::from(y))) {
        log::warn!("弹窗定位失败 {e}：保持默认位置");
    }
}

/// 关闭弹窗并清会话（= 上游 `closed` 事件路径）。
///
/// # Errors
///
/// 锁中毒。关窗本身失败被吞（窗口可能已销毁）。
pub fn close_update_popup(
    app: &AppHandle,
    session_slot: &Mutex<Option<PopupSession<TauriPopupTransport>>>,
) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(POPUP_LABEL) {
        let _ = w.close();
    }
    let mut slot = session_slot
        .lock()
        .map_err(|e| format!("popup 锁中毒: {e}"))?;
    *slot = None;
    Ok(())
}
