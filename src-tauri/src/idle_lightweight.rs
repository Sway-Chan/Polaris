//! C16 自动轻量模式的闲置巡检（`autoLightweightMode` 的**唯一**计时腿，主进程侧）。
//!
//! 到点即走托盘那条同一个 [`crate::tray::tray_enter_lightweight`]：销毁主窗 webview 释放内存，
//! 保托盘 + 核活。本模块只负责「什么时候该回收」，销毁本体一行都不重写。
//!
//! # 为什么计时器必须在主进程，而不是 renderer 里
//!
//! 原实现把 10 分钟的 `setTimeout` 放在主窗 renderer 里（`ui/src/lib/use-idle-lightweight.ts`），
//! 判据是 `document.visibilityState`。那是**让被判断的对象自己做判断** —— 计时器活在那个正要被
//! 回收的 webview 里。同一个结构问题派生出两条互相独立的死因（2026-07-30 mac 真机反馈）：
//!
//!  1. 隐藏窗的 `visibilityState` 是否真的转 `hidden` **依平台**（WebKitGTK / WebView2 / WKWebView
//!     各不相同）。不转 → 每次到点都判「可见」重新计时 = 开关开着也永不触发。
//!  2. 即便转了，WKWebView 会对隐藏窗的定时器 throttle / suspend，那个 10 分钟的 `setTimeout`
//!     未必真到点。
//!
//! 两条都不是能靠调参数绕过的 bug，只有把计时与被计时的对象解耦才根治。主进程的 tokio 定时器
//! 不受任何 webview 生命周期 / 节流策略影响，且窗口销毁后它仍在（下次唤出无需重新武装）。
//!
//! # 判据是「用户空闲」，不是「窗口不可见」
//!
//! 窗口收进托盘 ≠ 用户离开：他可能正在别的 App 里干活、随时切回来，此刻销毁 webview 只会让他
//! 下次唤出白等一次完整重建 —— 省下的内存还不如这一次等待值钱。故每一拍求的是「距上次**有用户
//! 存在的证据**过去了多久」，三个来源按强弱合一，见 [`next_idle_secs`]。
//!
//! 可见性真值不自造：复用降流门那份（`is_visible() && !is_minimized()` 回读 + 缓存，见
//! [`crate::runtime::stats`]）。两处各自回读只会分叉出「降流门说不可见、轻量巡检说可见」。

use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::runtime::stats::{probe_main_window_visible, MAIN_WINDOW_LABEL};
use crate::runtime::AppRuntime;

/// 闲置阈值：10 分钟。与设置页文案「闲置 10 分钟后关闭界面释放内存」是同一个数 —— 改这里必须
/// 一并改文案（前端 `settings-promise-wiring.test.ts` 的腿 A 守的正是「文案的数字有等值实现」）。
const IDLE_THRESHOLD_SECS: u64 = 10 * 60;

/// 巡检节拍：30 秒。**刻意粗**——判据是 10 分钟量级，秒级精度没有任何价值，而每一拍都要投一次
/// 主线程做可见性回读。用「最晚 10.5 分钟触发」这点误差换掉 20 倍的主线程投递量是划算的。
const TICK_SECS: u64 = 30;

/// 启动本模块的后台巡检腿（进程全程一条，随 app 生命周期存活）。
///
/// 不做 started 去重：唯一调用点在 `main.rs` 的 `setup`，跑且只跑一次。
pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // 「距上次有用户存在证据」的秒数。只有无系统空闲源的平台真的用它累加（见 next_idle_secs）。
        let mut idle_secs: u64 = 0;
        loop {
            // 用 sleep 而非 `interval`：`interval` 默认 Burst 会在系统休眠唤醒后把欠下的拍一次性
            // 补齐，累加腿的闲置随之瞬间冲过阈值 —— 用户刚掀开笔记本盖子就被收走界面。sleep 天然
            // 是「上一拍做完再等一个周期」，跨休眠只会少算，方向上偏保守（宁可晚回收）。
            tokio::time::sleep(Duration::from_secs(TICK_SECS)).await;

            // 主窗不在（尚未建 / 已在轻量态）→ 没有可回收的对象。闲置归零：下次唤出是重建出的新窗，
            // 该从那一刻重新起算，不能拿轻量期间攒的时长把它当场再收一次。
            if app.get_webview_window(MAIN_WINDOW_LABEL).is_none() {
                idle_secs = 0;
                continue;
            }
            // 开关**每拍动态读**（不在启动时快照）：用户在设置页改完最迟下一拍生效，无需重启。
            if !auto_lightweight_enabled(&app) {
                // 关着的时候不攒闲置。否则「关了一整天、刚打开开关」的下一拍就会把用户眼前正在看的
                // 窗口销毁 —— 他打开开关的意思是「从现在起闲置 10 分钟后回收」。
                idle_secs = 0;
                continue;
            }

            idle_secs =
                next_idle_secs(idle_secs, TICK_SECS, window_visible(&app), sys_idle::secs());
            if !idle_reached_threshold(idle_secs) {
                continue;
            }
            enter_lightweight_if_still_hidden(&app);
            // 已投递 → 归零。回收成功的话下一拍走上面的「窗不在」腿；被复核否决（用户恰好在这一
            // 瞬唤出了窗）的话也该重新起算，而不是每 30s 重投一次。
            idle_secs = 0;
        }
    });
}

/// 纯判定：本拍过后「距上次有用户存在的证据」是多少秒。
///
/// 三个来源合一，优先级从强到弱：
///
///  1. **窗口可见 → 归零**。可见 ≠ 一定有人在看，但**可能**有人在看，而误销毁的代价（眼前的界面
///     当场消失 + 下次唤出等一次完整重建）远大于少省一次内存。这条不依赖任何平台 API，故「用户
///     正在用界面」这个最常见的场景在**所有平台**都不会被回收。
///  2. **系统空闲秒数**（`Some`，macOS / Windows）。系统级「距上次键鼠输入」，**含其他 App**——
///     有它才谈得上「用户空闲」：窗口收进了托盘但用户正在别处敲键盘时这个值很小 → 不回收。
///     这正是本次改动要修的语义（原实现只知道「窗口藏了多久」）。
///  3. **`None`（无系统空闲源的平台，见 [`sys_idle`]）→ 累加节拍**，退化成「不可见时长」。这是
///     **弱**判据（分不出「用户离开」与「用户在别处忙」），但它是该平台能免依赖拿到的唯一的量，
///     且不比原实现更弱 —— 原实现在所有平台上就只有这一个量，还挂在会被节流的 renderer 里。
#[must_use]
fn next_idle_secs(prev: u64, tick_secs: u64, visible: bool, system_idle_secs: Option<u64>) -> u64 {
    if visible {
        return 0;
    }
    system_idle_secs.unwrap_or_else(|| prev.saturating_add(tick_secs))
}

/// 纯判定：闲置是否已达回收阈值（含等号 —— 累加腿恰好第 20 拍到 600s，差一个等号就永远晚一拍）。
#[must_use]
const fn idle_reached_threshold(idle_secs: u64) -> bool {
    idle_secs >= IDLE_THRESHOLD_SECS
}

/// 运行期**动态**读 `config.autoLightweightMode`。
///
/// 走 `with_current` 投影而非 `current()`：这是每 30s 一次的常驻腿，`current()` 每次要整份深拷贝
/// 配置（含全部节点与规则），而这里只要一个 bool。
///
/// 运行时未装配（启动早期）/ 读不到 / 非 bool 一律 **false**：这条腿的动作是销毁用户界面，
/// 读不到配置时只能按「没开」处理（失败安全方向 = 不回收）。
fn auto_lightweight_enabled(app: &AppHandle) -> bool {
    let Some(rt) = app.try_state::<AppRuntime>() else {
        return false;
    };
    rt.config()
        .with_current(|c| c.get("autoLightweightMode").and_then(Value::as_bool) == Some(true))
        .unwrap_or(false)
}

/// 主窗可见性（复用降流门那份缓存，非阻塞）。
///
/// 运行时未装配 → **true**（= 当作有人在看，不回收），与本模块其余失败分支同一个保守方向。
fn window_visible(app: &AppHandle) -> bool {
    let Some(rt) = app.try_state::<AppRuntime>() else {
        return true;
    };
    rt.stats().window_visible(app)
}

/// 投主线程：**最终复核**仍不可见 → 走既有的 [`crate::tray::tray_enter_lightweight`]。
///
/// **为什么要复核**：巡检读的是降流门的可见性**缓存**（非阻塞，最多落后一拍 = 30s），而这 30s 里
/// 用户完全可能刚从托盘把窗唤出来 —— 拿陈旧值直接销毁就是「点开界面它当场消失」。复核调的是降流门
/// 同一个 [`probe_main_window_visible`]（`is_visible() && !is_minimized()`），不另立一套判据。
///
/// **为什么必须在主线程**：`destroy()` 与窗口 getter 都要往主事件循环投消息等回包，从后台线程直接
/// 调会把 tokio worker 挂在 `recv` 上（理由见 `runtime::stats` 的 `VisibilityCache` 头注）。
fn enter_lightweight_if_still_hidden(app: &AppHandle) {
    let app_for_main = app.clone();
    let post = app.run_on_main_thread(move || {
        // 窗在这一瞬没了（另一条腿已进轻量 / 正在退出）→ 什么都别做。让它走下去只会重复置一次
        // `LightweightState`、白清一遍订阅账。
        if app_for_main.get_webview_window(MAIN_WINDOW_LABEL).is_none() {
            return;
        }
        match probe_main_window_visible(&app_for_main) {
            Ok(false) => {}      // 确认不可见 —— 这才是要回收的状态
            Ok(true) => return,  // 用户恰好在这一瞬唤出了窗 → 撤销本次回收
            Err(e) => {
                // 回读失败 → 失败安全方向：不回收。宁可这次内存不省，也绝不销毁一个可能有人在看的窗。
                log::warn!("自动轻量模式：可见性复核失败（{e}），本轮不回收");
                return;
            }
        }
        log::info!(
            "自动轻量模式：主窗不可见且用户已闲置 ≥{IDLE_THRESHOLD_SECS}s → 销毁主窗 webview 释放内存"
        );
        // 走托盘浮层那条**同一个** command 本体：置 LightweightState → clear_window 释放 stats
        // 订阅账 → 收浮层 → destroy。四步顺序有理由（见 `tray_enter_lightweight` 文档），尤其
        // `clear_window` 漏掉会让 gRPC poller 永续轮询 = 轻量白做。故这里绝不另写一份销毁逻辑。
        let _ = crate::tray::tray_enter_lightweight(app_for_main.clone());
    });
    if let Err(e) = post {
        log::warn!("自动轻量模式：投递主线程失败（{e}），本轮不回收");
    }
}

/// 系统级「距上次用户输入」秒数 —— 三平台数据源与取舍。
///
/// 这是 上游（本项目的 Electron 前身）里 `powerMonitor.getSystemIdleTime()` 的对应物。Tauri 没有
/// 等价的跨平台 API，故按平台各取一条**零新增 crate**的路：
///
///  - **macOS**：CoreGraphics 的 `CGEventSourceSecondsSinceLastEventType`。系统框架恒在，直接声明
///    外部符号即可，不引 `objc2-core-graphics`（为一个函数拉一个 crate 不划算）。
///  - **Windows**：user32 的 `GetLastInputInfo` + kernel32 的 `GetTickCount`，同样直接声明外部符号。
///    不为这两个函数把 `windows-sys` 拉进 `src-tauri`（它目前只是 `crates/helper` 的 Windows 腿依赖）：
///    `LASTINPUTINFO` 是两个 `u32` 的冻结 ABI，声明它的成本远低于给主进程新挂一层 FFI 绑定 crate。
///  - **其余（Linux）→ `None`**，判据退化成「不可见时长」（见 [`next_idle_secs`] 第 3 条）。刻意
///    不做：X11 的 XScreenSaver 要在构建环境装 libXss 且**在 Wayland 下恒返 0**（拿到就是错的），
///    D-Bus 的 `org.freedesktop.ScreenSaver` / Mutter IdleMonitor 要引一个 D-Bus 客户端 crate 且
///    各桌面环境实现不一。为一个「省内存」的锦上添花功能背上构建依赖 + 一个已知给错数的数据源，
///    代价远超收益。
mod sys_idle {
    /// macOS：`CGEventSourceSecondsSinceLastEventType(kCGEventSourceStateCombinedSessionState,
    /// kCGAnyInputEventType)`，即整个登录会话里距上次任意输入事件（键盘 / 鼠标 / 触控板）的秒数。
    #[cfg(target_os = "macos")]
    pub fn secs() -> Option<u64> {
        // CoreGraphics 是 macOS 系统框架（恒在，无需随包分发）。
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            /// `CFTimeInterval CGEventSourceSecondsSinceLastEventType(CGEventSourceStateID, CGEventType)`
            /// —— `CGEventSourceStateID` 是含负值的枚举（`int32_t`），`CGEventType` 是 `uint32_t`。
            fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
        }
        /// `kCGEventSourceStateCombinedSessionState` = 0：合并整个登录会话的事件源（要的就是
        /// 「用户在这台机器上有没有动」，不是本进程自己合成的事件）。
        const COMBINED_SESSION_STATE: i32 = 0;
        /// `kCGAnyInputEventType` = `(uint32_t)~0`：任意输入事件。
        const ANY_INPUT_EVENT: u32 = u32::MAX;

        // SAFETY: 纯值语义的 C 函数——两个入参都是常量枚举值，无指针、无所有权转移、无回调。
        let secs = unsafe {
            CGEventSourceSecondsSinceLastEventType(COMBINED_SESSION_STATE, ANY_INPUT_EVENT)
        };
        // 非有限 / 负数 = 拿到了讲不通的值 → 当作没有数据源（退回累加腿），不拿它做销毁决策。
        if secs.is_finite() && secs >= 0.0 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Some(secs as u64)
        } else {
            None
        }
    }

    /// Windows：`GetLastInputInfo` 给出最后一次输入的 tick，与 `GetTickCount` 求差即空闲时长。
    #[cfg(windows)]
    pub fn secs() -> Option<u64> {
        /// `LASTINPUTINFO`（winuser.h）：两个 `DWORD` 的冻结 ABI，`cbSize` 是版本自述字段。
        #[repr(C)]
        struct LastInputInfo {
            cb_size: u32,
            dw_time: u32,
        }
        #[link(name = "user32")]
        extern "system" {
            fn GetLastInputInfo(plii: *mut LastInputInfo) -> i32;
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetTickCount() -> u32;
        }

        let mut info = LastInputInfo {
            // 填错 `cb_size` 是这个 API 唯一会拒绝的输入，故用 try_from 而非裸 as。
            cb_size: u32::try_from(std::mem::size_of::<LastInputInfo>()).ok()?,
            dw_time: 0,
        };
        // SAFETY: 传的是本地栈上、`cb_size` 已按结构体真实大小填好的有效可写指针 —— 这正是该 API
        // 的全部契约（无所有权转移，函数只回填 `dw_time`）。
        if unsafe { GetLastInputInfo(&mut info) } == 0 {
            return None; // 极少见（会话锁定等）；当作没有数据源，而不是当作「空闲 0 秒」
        }
        // SAFETY: 无参数、无副作用的计数器读取。
        let now = unsafe { GetTickCount() };
        // `dw_time` 与 `GetTickCount` 同属 32 位毫秒计数域，49.7 天回绕 —— `wrapping_sub` 正是为此。
        // 换 `GetTickCount64` 反而要手工把 32 位的 `dw_time` 对齐到 64 位高半区，出错面更大。
        Some(u64::from(now.wrapping_sub(info.dw_time)) / 1000)
    }

    /// Linux 及其余平台：无免依赖的系统空闲源（理由见模块头注）。
    #[cfg(not(any(target_os = "macos", windows)))]
    pub const fn secs() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_window_never_accumulates_idle_on_any_platform() {
        // 「可见即豁免」：窗口在屏上时，系统空闲再大也归零 —— 可能正有人盯着它。
        // 这条不依赖平台 API，是所有平台共同的地板。
        assert_eq!(next_idle_secs(590, TICK_SECS, true, Some(9_999)), 0);
        assert_eq!(next_idle_secs(590, TICK_SECS, true, None), 0);
    }

    #[test]
    fn system_idle_overrides_how_long_the_window_has_been_hidden() {
        // 本次改动的核心语义：窗口已藏了 10 分钟，但用户 5 秒前还在**别的 App** 里敲键盘
        // → 判 5 秒，不回收。若这里退回 600，就是「窗口不可见时长」冒充「用户空闲」的老 bug。
        assert_eq!(next_idle_secs(600, TICK_SECS, false, Some(5)), 5);
        // 反向：系统真的空闲了 → 即便刚藏起来（prev=0）也直接采信系统值。
        assert_eq!(next_idle_secs(0, TICK_SECS, false, Some(1_200)), 1_200);
    }

    #[test]
    fn without_system_source_idle_accumulates_by_tick() {
        // 无系统空闲源（Linux）→ 退化成累加「不可见时长」。
        assert_eq!(next_idle_secs(0, TICK_SECS, false, None), TICK_SECS);
        assert_eq!(next_idle_secs(570, TICK_SECS, false, None), 600);
        // 不溢出（进程长跑 + 常年不可见）。
        assert_eq!(next_idle_secs(u64::MAX, TICK_SECS, false, None), u64::MAX);
    }

    #[test]
    fn threshold_is_ten_minutes_and_the_boundary_is_inclusive() {
        // 阈值与设置页文案「闲置 10 分钟」同源，改一处必须改另一处。
        assert_eq!(IDLE_THRESHOLD_SECS, 10 * 60);
        assert!(!idle_reached_threshold(IDLE_THRESHOLD_SECS - 1));
        assert!(idle_reached_threshold(IDLE_THRESHOLD_SECS));
    }

    #[test]
    fn accumulating_leg_fires_at_exactly_ten_minutes() {
        // 端到端跑累加腿：30s 一拍，第 20 拍（= 600s）刚好到点，不早不晚。
        // 上限断言是自曝腿——节拍或阈值被改成永远够不着时，这里炸而不是静默变成死开关。
        let mut idle = 0;
        let mut ticks = 0_u32;
        while !idle_reached_threshold(idle) {
            idle = next_idle_secs(idle, TICK_SECS, false, None);
            ticks += 1;
            assert!(
                ticks <= 1_000,
                "累加腿永远到不了阈值 —— 该平台的自动轻量模式是死开关"
            );
        }
        assert_eq!(ticks, 20);
    }

    #[test]
    fn a_single_visible_tick_restarts_the_whole_countdown() {
        // 用户中途瞄了一眼窗口（哪怕只有一拍可见）→ 倒计时必须从头起算，而不是接着上次攒的。
        let idle = next_idle_secs(590, TICK_SECS, true, None);
        assert!(!idle_reached_threshold(next_idle_secs(
            idle, TICK_SECS, false, None
        )));
    }
}
