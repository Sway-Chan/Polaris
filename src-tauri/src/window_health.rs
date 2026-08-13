//! 主窗白屏自愈：mount 健康门 + 终局兜底页 + renderer 错误转发限频。
//!
//! 迁自 上游 PR #302（Electron），**按 Tauri 2 架构重新落地而非照搬代码**。三处关键差异：
//!
//! 1. **武装点：窗口创建时（`main.rs` setup）+ 每次 [`PageLoadEvent::Started`]，而非 上游的
//!    `did-finish-load`**。Tauri 2 的 `PageLoadEvent` 只有 `Started`/`Finished` 两个变体
//!    （`tauri-runtime-2.11.3/src/webview.rs:83-91` 实证），**没有 did-fail-load 等价物**。更糟的是
//!    加载失败时三平台行为还不一致（均为 wry 0.55.1 源码实证）：
//!    - **Windows**：`NavigationCompleted` 的 `IsSuccess`/`WebErrorStatus` 被 wry 丢弃
//!      （`wry/src/webview2/mod.rs:659-670` 第二参数 `_`）→ **加载失败照样上报 `Finished`**。
//!      故 `Finished` **不可用作加载成功的判据**——沿用 上游的 Finished 武装在 Windows 上直接失效。
//!    - **macOS / Linux**：`Started` 挂在 `didCommitNavigation` / `LoadEvent::Committed` 上，加载失败
//!      根本 commit 不了 → **`Started` 和 `Finished` 双双不触发，零信号**。
//!
//!    结论：任何「靠页面事件判加载成败」的设计在 Tauri 都不成立。故武装点前移到**窗口创建那一刻**
//!    （setup 内，无条件），页面事件只作为「新文档开始加载 → 重新武装」的补充。这样 B 类（load 失败）
//!    无论哪个平台、有无信号，都退化成「门武装了但 ready 永不到达」→ 超时兜底。**一门通吃
//!    A(启动期)/B/C 三类**，且不依赖任何平台特定的失败信号。
//!
//! 2. **终局兜底页用 eval 注入而非 `navigate(data:)`**。上游 走 `loadURL(data:)` 后按钮只能经
//!    preload 的 ipcRenderer 回主进程（data: 页是 opaque origin，自身 `location.reload()` 只重渲错误页
//!    本身）。Tauri 的 data: 页同样是 opaque origin，且 **Tauri IPC 按 origin 放行**——data: 页拿不到
//!    可用的 `__TAURI_INTERNALS__`，`fatal_retry` 根本发不出，按钮必成死键。改用 `eval` 往**当前文档**
//!    注入静态 DOM，保住原 origin。
//!
//!    **真机实测结论（2026-07-16，Linux/WebKitGTK，勿凭直觉推翻）**：
//!    - **C 类**（HTML 加载成功、只是 `#root` 空 —— oracle 判定的三平台通杀真凶）：注入可达，
//!      **`__TAURI_INTERNALS__` 存活**，按钮 → `fatal_retry` → 门复位 → 应用真恢复，**端到端实测通过**。
//!    - **B 类**（load 彻底失败）：兜底页**画得出来**（实测截图确认，不是空窗），但
//!      **`__TAURI_INTERNALS__` 不存在** —— Tauri 的 IPC 注入脚本随页面加载执行，页面都没加载成功它就没跑。
//!      故此时 `invoke` 必抛 → 回退 `location.reload()`。**这正是 `PageStarted` 必须能解除 `finalized`
//!      的原因**（见 [`reduce_mount_gate`] 文档）：B 类下门的复活不能依赖 IPC。
//!
//!    即：逃生门在「它要救的东西已经全坏」时仍须有效 —— 不能把恢复能力建在 IPC 还活着的假设上。
//!
//! 3. **`reloading` 不变式保留**（上游 L1，单测背书）：timeout→reload 后到达的 ready 可能来自 reload
//!    前的旧文档，采信会把门置 ready → 重载页若再 C 类失败则无兜底（失明）。Started 武装虽消除了
//!    上游「ready 早于 load 事件」的竞态（Started 必早于本文档任何 JS 执行），但**跨文档的在途 ready**
//!    仍存在，故该位不可省。
//!
//! A 类（webview 渲染进程崩溃）的等价物**按平台分裂**（tauri 2.11.5 / wry 0.55.1 源码实证）：
//! - **macOS/iOS**：有 `Builder::on_web_content_process_terminate`（`tauri/src/app.rs:1791-1806`），
//!   底层是 WKWebView 的 `webViewWebContentProcessDidTerminate:`。**且 Tauri 在你不注册时会自动装一个
//!   默认 handler 去 `webview.reload()`**（`tauri-runtime-wry/src/lib.rs:5119-5140` 的 else 分支）——
//!   即 macOS 的崩溃自愈**开箱即有**。故本仓**刻意不注册**该 handler：注册会走 if 分支、把内置自愈顶掉，
//!   本想加强反而削弱。崩溃 → 内置 reload → 新文档 `Started` → 本门重新武装 → 端到端闭合。
//! - **Windows / Linux**：wry **完全没有 hook** WebView2 的 `ProcessFailed` / WebKitGTK 的
//!   `web-process-terminated`（两处 grep 零命中）→ **无任何等价事件**。补法只有 `with_webview` 逃生舱挂
//!   原生回调，需引入 `webview2-com`+`windows` / `webkit2gtk` 直接依赖 = 新增依赖，超出本批纪律，登记为
//!   follow-up。故本门只覆盖 Windows/Linux 的「启动期就崩」（无 ready → 超时兜底），**运行期崩溃未覆盖**。
//!
//! 注：上游的 A 类触发场景（关窗 hide 保活 → renderer 崩 → 托盘唤出白屏）此前在 Polaris 结构性不存在
//! （无托盘、`window_close` 即真关窗），当时留了「待托盘/hide 保活落地时须补『show 前存活探针』」。托盘 +
//! close-to-tray + C16 轻量模式落地后该场景已成立，**探针即本模块的 [`show_timing`]**：任何把主窗推上屏的
//! 路径都先问一句「当前文档 mount 成功了吗」，没成功就扣在隐藏态等 `renderer:ready`（[`defer_show`]）。
//!
//! 同一条判定顺带修掉一个**与崩溃无关、但成因同构**的缺陷：主窗此前是 `builder.build()` 返回即上屏，
//! 而 webview 那时才开始加载文档 → 用户先看到一段空白窗（mac 真机实测 345–2467ms，长尾就是用户报的「白屏」）。
//! 根因同为「上屏时机由建窗时刻决定，而非由内容可绘决定」，故一处判定通吃两者。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Manager, Url};

/// renderer 未在此期限内回发 `renderer:ready` → 判 mount 失败。
///
/// 12s 对齐 上游 `RENDERER_READY_TIMEOUT_MS`：需覆盖冷启动最慢的 bundle 解析 + 首次 render，
/// 又不能长到让用户对着空窗干等。首次超时 reload，再一个 12s 仍无 ready → 终局页（合计 24s）。
const MOUNT_READY_TIMEOUT_MS: u64 = 12_000;

/// 「等就绪再上屏」的兜底期限：主窗被扣在隐藏态等 `renderer:ready`，超过它仍无信号就先把窗口放出来。
///
/// 必须有界：**「点了图标什么都没发生」比「短暂空窗」更糟** —— 用户会以为没点上、反复点，甚至再拉起
/// 一份进程。超期兜底后的观感恰好**退化成本改动之前的行为**（空窗先上屏、内容随后补），不会更差。
///
/// 3s 的依据是真机实测而非拍脑袋：mac 5.238 日志里 12 次冷启动的「建窗 → `renderer:ready`」区间为
/// **345–2467ms**（中位 ~430ms，长尾 1466/2154/2467ms 三次 —— 正是用户「不是每次复现」的那几次），
/// 3s 覆盖实测最慢档仍有余量。
const MOUNT_SHOW_DEADLINE_MS: u64 = 3_000;

/// console 转发限频窗口（滑动窗口长度）。
const CONSOLE_WINDOW_MS: u64 = 1_000;
/// 每窗口最多转发条数（够看清风暴征兆，又不至刷爆日志）。
const CONSOLE_MAX_PER_WINDOW: usize = 10;
/// 单条日志截断上限（防超长串撑爆单行）。
const CONSOLE_MAX_CHARS: usize = 2_000;

// ─────────────────────────── 纯 reducer（可单测，无 Tauri 依赖）───────────────────────────

/// 门接收的事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountGateEvent {
    /// 新文档开始加载（Tauri `PageLoadEvent::Started`）——**唯一武装点**。
    PageStarted,
    /// renderer mount 成功回发的就绪信号。
    RendererReady,
    /// 武装的计时器到点。
    Timeout,
}

/// 门决策出的动作，交接线层执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountGateAction {
    /// 武装（先作废旧计时器再起新计时器）。
    Arm,
    /// 作废计时器（mount 已确认，门满足）。
    Clear,
    /// 作废计时器 + 重载一次（覆盖瞬态 mount 失败）。
    Reload,
    /// 作废计时器 + 终局兜底（注入静态错误页），门自此停用。
    Fatal,
    /// 无操作。
    None,
}

/// 门状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MountGateState {
    /// `renderer:ready` 已收到（App 成功 render+commit）。
    pub ready: bool,
    /// 已因 mount 超时 reload 过一次（首次 vs 终局的区分位）。
    pub reloaded: bool,
    /// 已进入终局兜底 → 门彻底停用（只有 `fatal_retry` 能复位）。
    pub finalized: bool,
    /// 一次 timeout 触发的 reload **在途**：此窗口内到达的 ready 一律丢弃（见模块头 §3）。
    pub reloading: bool,
}

/// 纯状态转移。
///
/// 要点：
/// 1. `PageStarted` 是唯一武装点——reload / `fatal_retry` 的 navigate 后它必再触发，天然重新武装。
/// 2. `PageStarted` 无条件重置 `ready`：新文档必须重新证明自己能 mount，旧文档的 ready 一律作废。
/// 3. **`PageStarted` 同时解除 `finalized`**（与 上游 相反，理由见下），但 `reloaded` **保持粘滞**。
/// 4. `reloading`：丢弃 pre-reload 旧文档的在途 ready（L1 不变式，见模块头 §3）。
///
/// ## 为什么 `PageStarted` 能安全解除 `finalized`（Tauri 特有，上游 不成立）
///
/// 上游的终局页走 `loadURL(data:)` = **一次真导航**，故终局页自己会触发 `did-finish-load`；若那能重新
/// 武装，就是 `loadURL→timeout→loadURL` 无限死循环 —— 所以 上游 必须让 `finalized` 永久停用门。
///
/// Polaris 的终局页走 **`eval` 注入当前文档**（见模块头 §2），**不产生任何导航** → 终局后的
/// `PageStarted` 只可能来自「主进程 `fatal_retry` 的 navigate」或「用户在终局页按了重载」，**永远不会是
/// 终局页自身的副作用**。故解除 `finalized` 不存在自动死循环风险。
///
/// 这条不是理论优化，是**真机实测逼出来的必需品**：实测证明 load 失败（B 类）的页面上
/// `window.__TAURI_INTERNALS__` **不存在**（Tauri 的 IPC 注入脚本随页面加载执行，页面没加载成功它就没跑）
/// → 终局页按钮的 `invoke('fatal_retry')` 必然抛错 → 回退 `location.reload()`。若 `finalized` 永久停用，
/// 那条回退路径恢复出来的页面就**再无任何兜底**。让 `PageStarted` 解除 finalized 后，即便 IPC 全程不可用，
/// 用户点一次重载也能让门完整复活 —— 逃生门不依赖它想救的那套东西还活着。
///
/// `reloaded` 保持粘滞是防自动重载死循环：否则 `PageStarted` 重置它 → timeout → Reload → navigate →
/// `PageStarted` 又重置 → 无限自动 reload。粘滞后，用户手动重载失败 → 12s 直接进终局页（不再自动 reload），
/// 只有显式 `fatal_retry`（用户主动点、语义=「我要重来一遍」）才 `reset()` 全量复位。
pub fn reduce_mount_gate(
    state: MountGateState,
    event: MountGateEvent,
) -> (MountGateState, MountGateAction) {
    // 终局后只有「新文档开始加载」能复活门；其余事件一律 no-op。
    if state.finalized && event != MountGateEvent::PageStarted {
        return (state, MountGateAction::None);
    }
    match event {
        MountGateEvent::RendererReady => {
            if state.reloading {
                // reload 在途：这条 ready 来自旧文档，采信会让重载页失明。
                (state, MountGateAction::None)
            } else {
                (
                    MountGateState {
                        ready: true,
                        ..state
                    },
                    MountGateAction::Clear,
                )
            }
        }
        MountGateEvent::PageStarted => (
            MountGateState {
                ready: false,
                reloading: false,
                finalized: false,
                // reloaded 粘滞：不重置，否则 timeout→Reload→navigate→PageStarted 是无限自动重载。
                reloaded: state.reloaded,
            },
            MountGateAction::Arm,
        ),
        MountGateEvent::Timeout => {
            if state.ready {
                // 安全网：ready 后不该还有计时器在跑。
                (state, MountGateAction::None)
            } else if state.reloaded {
                (
                    MountGateState {
                        finalized: true,
                        ..state
                    },
                    MountGateAction::Fatal,
                )
            } else {
                (
                    MountGateState {
                        reloaded: true,
                        reloading: true,
                        ..state
                    },
                    MountGateAction::Reload,
                )
            }
        }
    }
}

/// 主窗此刻该**立刻上屏**还是**等首帧可绘再上屏**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowTiming {
    /// 立刻 show。
    Now,
    /// 扣在隐藏态，等 `renderer:ready`（或 [`MOUNT_SHOW_DEADLINE_MS`] 兜底）再 show。
    WhenReady,
}

/// 上屏时机判定（纯函数，可单测）。
///
/// # 为什么需要这条判定
///
/// 主窗此前是 `builder.build()` 返回那一刻就在屏上（conf 未声明 `visible` ⇒ 默认 true），而 webview 此时
/// 才刚开始加载文档、解析 bundle、挂 React —— 中间那段**空白窗**在 mac 真机实测是 345–2467ms。窗口有没有
/// 内容可绘，唯一的权威信号就是本模块已有的 `renderer:ready`，故上屏时机应当由它决定，而不是由建窗时刻决定。
///
/// # 三条「不许扣窗」的例外（任一成立即 [`ShowTiming::Now`]）
///
/// - `!gate_enabled`：门没武装（dev 档默认关）⇒ **等不到 `renderer:ready`**（`dispatch` 直接早退）。
///   此时扣窗 = 窗口永不出现 = 死界面，比空窗糟得多。
/// - `ready`：当前文档已 mount 成功 ⇒ 窗里本就有内容，没有可等的东西（托盘/dock 反复唤出走这条，零延迟）。
/// - `currently_visible`：窗口已经在屏上 ⇒ 扣它只能靠先 `hide()`，那是把「有内容的窗」变成「窗突然消失
///   又回来」的闪烁，纯倒退。已上屏的窗只能往前走。
///
/// 三条都不成立才 [`ShowTiming::WhenReady`]：窗口要么刚建、要么隐藏着，且当前文档尚未证明自己能 mount。
pub fn resolve_show_timing(gate_enabled: bool, ready: bool, currently_visible: bool) -> ShowTiming {
    if !gate_enabled || ready || currently_visible {
        return ShowTiming::Now;
    }
    ShowTiming::WhenReady
}

/// console 转发限频（纯函数）。剪掉窗口外的旧时刻；未超上限则纳入本次并放行，超上限则**不纳入**、
/// 不放行（丢弃本条且不占额度，以免风暴期计数无界增长）。
pub fn admit_console_message(
    timestamps: &[u64],
    now: u64,
    window_ms: u64,
    max_per_window: usize,
) -> (Vec<u64>, bool) {
    let mut recent: Vec<u64> = timestamps
        .iter()
        .copied()
        .filter(|t| now.saturating_sub(*t) < window_ms)
        .collect();
    if recent.len() >= max_per_window {
        return (recent, false);
    }
    recent.push(now);
    (recent, true)
}

/// 单条日志截断（按 char 边界切，避免 UTF-8 多字节被劈裂 panic）。
pub fn truncate_console_message(message: &str, max_chars: usize) -> String {
    let total = message.chars().count();
    if total <= max_chars {
        return message.to_string();
    }
    let head: String = message.chars().take(max_chars).collect();
    format!("{head}…(+{} chars)", total - max_chars)
}

// ─────────────────────────── 终局兜底页（零依赖静态 DOM）───────────────────────────

/// 生成「注入终局兜底页」的 JS。
///
/// 为什么必须零依赖：本页在「React/i18n/theme bundle 已证实起不来」时注入，故不能依赖任何应用脚本、
/// 外链资源或主题 token——纯内联样式 + 硬编码双语文案（i18n 本身可能正是坏的那一环）。
///
/// 按钮经 `window.__TAURI_INTERNALS__.invoke('fatal_retry')` 回主进程（`@tauri-apps/api` 的 `invoke`
/// 也正是转调这个内部对象，见 `node_modules/@tauri-apps/api/core.js:202`；此处不能 import ES 模块，
/// 因为坏掉的可能正是 bundle 本身）。invoke 不可用则回退 `location.reload()`（降级：应用可能恢复，
/// 但门保持 finalized）。
///
/// 注入用 `innerHTML` + `addEventListener` 而非内联 `onclick`/`<script>`：innerHTML 插入的 `<script>`
/// 按 HTML 规范**不执行**，且页面 CSP 为 `script-src 'self'` 会拦内联脚本；`eval` 走的是平台原生
/// evaluateJavaScript（特权上下文），不受页面 CSP 约束。
fn fatal_page_script() -> String {
    // 深色硬编码 hex：终局页不得依赖 tokens.css（--bg 可能正是没加载上的那一环）。色值取 Polaris
    // 深色 --bg (220 40% 6%) 同系，避免刺眼白闪。
    let html = concat!(
        r#"<div style="max-width:520px">"#,
        r#"<h1 style="font-size:18px;font-weight:600;margin:0 0 12px">界面初始化失败 / Failed to initialize</h1>"#,
        r#"<p style="font-size:13px;line-height:1.6;color:#9DA7B3;margin:0 0 20px;word-break:break-word">"#,
        r#"界面加载遇到问题，请尝试重新加载。网络代理不受影响。<br>"#,
        r#"The interface failed to load. Please try reloading. Your proxy connection is unaffected."#,
        r#"</p>"#,
        r#"<button id="polaris-fatal-reload" type="button" style="font:inherit;font-size:13px;color:#fff;background:#2F81F7;border:0;border-radius:8px;padding:9px 20px;cursor:pointer">重新加载 / Reload</button>"#,
        r#"</div>"#
    );
    let body_style = concat!(
        "margin:0;min-height:100vh;background:#0B0F14;color:#E6EDF3;",
        r#"font-family:system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;"#,
        "display:flex;align-items:center;justify-content:center;text-align:center;",
        "padding:24px;box-sizing:border-box"
    );
    // serde_json::to_string 产出的 JSON 字符串字面量是合法 JS 字符串字面量（转义完备），
    // 用它把 HTML 安全嵌进 JS，杜绝引号/换行破坏结构。
    let html_lit = serde_json::to_string(html).unwrap_or_else(|_| "\"\"".to_string());
    let body_lit = serde_json::to_string(body_style).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(function(){{try{{
var d=document;if(!d||!d.body){{return}}
d.documentElement.style.background='#0B0F14';
d.body.style.cssText={body_lit};
d.body.innerHTML={html_lit};
var b=d.getElementById('polaris-fatal-reload');
if(b){{b.addEventListener('click',function(){{
try{{window.__TAURI_INTERNALS__.invoke('fatal_retry',{{}})}}catch(e){{location.reload()}}
}})}}
}}catch(e){{}}}})();"#
    )
}

// ─────────────────────────── 接线层（计时器 / 副作用）───────────────────────────

/// 主窗健康态（Tauri managed state）。
pub struct WindowHealth {
    state: Mutex<MountGateState>,
    /// 计时器代次：每次 Arm/Clear/Reload/Fatal 递增，到点的计时器代次不符即自行作废
    /// （替代 Electron 侧 `clearTimeout`——无需持有 JoinHandle，也不怕竞态重复取消）。
    epoch: AtomicU64,
    /// 应用真实 URL（启动时捕获），供 reload / `fatal_retry` 导航回真实应用。
    app_url: Mutex<Option<Url>>,
    /// console 转发限频的滑动窗口时间戳。
    console_ts: Mutex<Vec<u64>>,
    /// 「待上屏」意图：置位 = 主窗正被**刻意扣在隐藏态**等首帧可绘（见 [`resolve_show_timing`]）。
    /// 兑现腿有二：`renderer:ready`（正常）与 [`MOUNT_SHOW_DEADLINE_MS`] 兜底计时器；`swap` 保证只有
    /// 一条能拿到上屏权，另一条自动退化成 no-op。
    pending_show: AtomicBool,
    /// `pending_show` 兜底计时器的代次（与 mount 门的 `epoch` 各管各的：两者作废条件不同）。
    /// 到点的计时器代次不符即自行作废 —— 与 `epoch` 同一套手法，不引入新机制。
    show_epoch: AtomicU64,
    /// mount 门是否武装。dev 下关闭（vite overlay + devtools 已足够，且 HMR 会频繁触发页面事件）；
    /// 可经 `POLARIS_MOUNT_GATE=1` 强制打开以便 dev 态真机验证。
    gate_enabled: bool,
}

impl WindowHealth {
    /// 新建。`gate_enabled` 由编译档 + `POLARIS_MOUNT_GATE` 环境变量共同决定。
    pub fn new() -> Self {
        let gate_enabled = if cfg!(debug_assertions) {
            std::env::var("POLARIS_MOUNT_GATE").as_deref() == Ok("1")
        } else {
            std::env::var("POLARIS_MOUNT_GATE").as_deref() != Ok("0")
        };
        Self {
            state: Mutex::new(MountGateState::default()),
            epoch: AtomicU64::new(0),
            app_url: Mutex::new(None),
            console_ts: Mutex::new(Vec::new()),
            gate_enabled,
            pending_show: AtomicBool::new(false),
            show_epoch: AtomicU64::new(0),
        }
    }

    /// 消费「待上屏」意图。`true` = 本次调用**赢得**上屏权。
    ///
    /// `swap` 是单赢家闸门：`renderer:ready` 腿与兜底计时器腿并发时只有一个拿到 `true`，另一个必得 `false`
    /// 并退化成 no-op —— 窗口绝不会被 show 两次。顺带递增 `show_epoch` 作废任何在途兜底计时器。
    fn take_pending_show(&self) -> bool {
        self.show_epoch.fetch_add(1, Ordering::SeqCst);
        self.pending_show.swap(false, Ordering::SeqCst)
    }

    /// 记录应用真实 URL（启动时，任何导航发生前）。
    pub fn set_app_url(&self, url: Url) {
        if let Ok(mut slot) = self.app_url.lock() {
            *slot = Some(url);
        }
    }

    /// mount 门是否武装。
    pub fn gate_enabled(&self) -> bool {
        self.gate_enabled
    }

    /// 复位门到初始态（`fatal_retry` 用——不变式 6：重试后门必须能真正复位，否则「重新加载」只是
    /// 一次性假承诺，恢复后的页面再白屏就再无兜底）。
    pub fn reset(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut s) = self.state.lock() {
            *s = MountGateState::default();
        }
    }

    /// 应用 URL（`fatal_retry` / reload 用）。
    fn app_url(&self) -> Option<Url> {
        self.app_url.lock().ok().and_then(|u| u.clone())
    }
}

impl Default for WindowHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// 主窗上屏时机（[`resolve_show_timing`] 的 app 侧薄封装，只负责取三个输入）。
///
/// `window` = 当前主窗；传 `None` 表示**正在新建/重建**主窗 —— 此时新文档定义上还没 mount 过、窗口也
/// 还没上过屏，故 `ready`/`currently_visible` 恒 false。**建窗路径必须传 `None` 而不是查门状态**：轻量
/// 模式重建时门里躺着的还是被销毁那个旧文档的 `ready=true`，照它判会直接把空窗放上屏（正是本次要修的）。
///
/// 门未装配（理论态：state 还没 manage）→ [`ShowTiming::Now`]，绝不因为拿不到健康态就把窗口扣死。
pub fn show_timing(app: &AppHandle, window: Option<&tauri::WebviewWindow>) -> ShowTiming {
    let Some(health) = app.try_state::<WindowHealth>() else {
        return ShowTiming::Now;
    };
    let ready = window.is_some() && health.state.lock().is_ok_and(|s| s.ready);
    let currently_visible = window.is_some_and(|w| w.is_visible().unwrap_or(false));
    resolve_show_timing(health.gate_enabled(), ready, currently_visible)
}

/// 把主窗扣在隐藏态、等首帧可绘再上屏（[`ShowTiming::WhenReady`] 的执行腿）。
///
/// 调用方负责让窗口此刻**不可见**（建窗传 `visible(false)` / 窗本就隐藏着）；本函数只登记意图 + 起兜底
/// 计时器，**不 hide 任何窗**——把已上屏的窗扣回去是闪烁，那条已由 [`resolve_show_timing`] 在判定层挡掉。
pub fn defer_show(app: &AppHandle) {
    let Some(health) = app.try_state::<WindowHealth>() else {
        return;
    };
    health.pending_show.store(true, Ordering::SeqCst);
    let epoch = health.show_epoch.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(MOUNT_SHOW_DEADLINE_MS)).await;
        let Some(health) = app.try_state::<WindowHealth>() else {
            return;
        };
        // 代次已变 = 期间 ready 到达或又发起了一次 defer → 本计时器已作废。
        if health.show_epoch.load(Ordering::SeqCst) != epoch {
            return;
        }
        if health.take_pending_show() {
            log::warn!(
                "首帧上屏兜底：{MOUNT_SHOW_DEADLINE_MS}ms 未收到 renderer:ready → 先显示主窗（内容可能仍在加载）"
            );
            crate::present_main_window(&app);
        }
    });
}

/// 向门投递一个事件并执行决策出的动作。
pub fn dispatch(app: &AppHandle, event: MountGateEvent) {
    let Some(health) = app.try_state::<WindowHealth>() else {
        return;
    };
    if !health.gate_enabled() {
        return;
    }
    // 决策与副作用分离：锁只包住状态转移，绝不跨副作用/await 持有。
    let action = {
        let Ok(mut guard) = health.state.lock() else {
            return;
        };
        let (next, action) = reduce_mount_gate(*guard, event);
        *guard = next;
        action
    };
    apply(app, &health, action);
}

fn apply(app: &AppHandle, health: &WindowHealth, action: MountGateAction) {
    match action {
        MountGateAction::None => {}
        MountGateAction::Clear => {
            health.epoch.fetch_add(1, Ordering::SeqCst);
            log::debug!("mount 健康门：renderer:ready 已收到，门满足");
            // 首帧可绘 → 兑现建窗/唤出时刻登记的「等就绪再上屏」。窗口是在这一刻**第一次**出现在用户
            // 眼前，因此它出现即有内容——不再有 build() 到 mount 之间那段空白窗。
            if health.take_pending_show() {
                crate::present_main_window(app);
            }
        }
        MountGateAction::Arm => {
            let epoch = health.epoch.fetch_add(1, Ordering::SeqCst) + 1;
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_millis(MOUNT_READY_TIMEOUT_MS)).await;
                // 代次已变 = 期间 ready/reload/fatal 发生过 → 本计时器已作废。
                let stale = app
                    .try_state::<WindowHealth>()
                    .is_none_or(|h| h.epoch.load(Ordering::SeqCst) != epoch);
                if !stale {
                    dispatch(&app, MountGateEvent::Timeout);
                }
            });
        }
        MountGateAction::Reload => {
            health.epoch.fetch_add(1, Ordering::SeqCst);
            log::error!(
                "mount 健康门：{MOUNT_READY_TIMEOUT_MS}ms 未收到 renderer:ready（renderer 活着但 DOM 空？）→ 重载一次"
            );
            navigate_to_app(app, health);
        }
        MountGateAction::Fatal => {
            health.epoch.fetch_add(1, Ordering::SeqCst);
            log::error!("mount 健康门：重载后仍未收到 renderer:ready → 注入终局兜底页");
            if let Some(window) = app.get_webview_window("main") {
                if let Err(e) = window.eval(fatal_page_script()) {
                    log::error!("终局兜底页注入失败：{e}（窗口可能已不可用）");
                }
                // 门升级到终局时窗口必须可见——否则用户对着托盘/任务栏毫无反馈。
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
    }
}

/// 导航回应用真实 URL。取不到记录的 URL 时回退 `reload()`（当前文档原地重载）。
fn navigate_to_app(app: &AppHandle, health: &WindowHealth) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    match health.app_url() {
        Some(url) => {
            if let Err(e) = window.navigate(url) {
                log::error!("导航回应用 URL 失败：{e}，回退 reload()");
                let _ = window.reload();
            }
        }
        None => {
            let _ = window.reload();
        }
    }
}

/// 复位门 + 导航回真实应用（`fatal_retry` 接线）。
pub fn retry_from_fatal(app: &AppHandle) {
    let Some(health) = app.try_state::<WindowHealth>() else {
        return;
    };
    log::warn!("fatal:retry —— 用户点击终局页「重新加载」，复位 mount 门并重载应用");
    health.reset();
    navigate_to_app(app, &health);
}

/// 转发一条 renderer 日志到 Rust 日志（限频 + 截断）。
///
/// 这是 C 类白屏「零可观测」的根治：Tauri 没有 Electron 的 `console-message` 主进程事件，故由 renderer
/// 侧主动 invoke 上报（见 `ui/src/main.tsx`）。限频放在 Rust 侧 = 单一权威点、纯函数可测，且与 上游
/// 主进程侧限频同形。
pub fn forward_renderer_log(app: &AppHandle, level: &str, message: &str) {
    let Some(health) = app.try_state::<WindowHealth>() else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let admitted = {
        let Ok(mut ts) = health.console_ts.lock() else {
            return;
        };
        let (recent, admit) =
            admit_console_message(&ts, now, CONSOLE_WINDOW_MS, CONSOLE_MAX_PER_WINDOW);
        *ts = recent;
        admit
    };
    if !admitted {
        return;
    }
    let text = truncate_console_message(message, CONSOLE_MAX_CHARS);
    match level {
        "error" => log::error!("[renderer] {text}"),
        "warn" => log::warn!("[renderer] {text}"),
        _ => log::info!("[renderer] {text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initial() -> MountGateState {
        MountGateState::default()
    }

    // ── 上屏时机（[`resolve_show_timing`]）────────────────────────────────────
    //
    // 变异对照（改坏哪一处 → 哪条转红）：
    //   · 整个函数恒返回 `Now`（= 退回本次改动前「建窗即上屏」）→ `blank_window_is_never_shown_before_ready` 红
    //   · `!gate_enabled` 写成 `gate_enabled`                    → `gate_disabled_must_never_hold_the_window` 红
    //     （外加 `blank_window_is_never_shown_before_ready` 一并红）
    //   · 去掉 `|| ready`                                        → `ready_window_shows_immediately` 红
    //   · 去掉 `|| currently_visible`                            → `visible_window_is_never_pulled_back` 红

    /// 本次要修的那条：门武装 + 当前文档没 mount 成功 + 窗还没上屏 → 必须扣住，等首帧可绘。
    #[test]
    fn blank_window_is_never_shown_before_ready() {
        assert_eq!(
            resolve_show_timing(true, false, false),
            ShowTiming::WhenReady
        );
    }

    /// 门没武装（dev 档默认关）⇒ `renderer:ready` 永远不会被投递到门里 ⇒ 扣窗 = 窗口永不出现。
    /// 空窗只是难看，死界面是坏掉，故此处必须让路。
    #[test]
    fn gate_disabled_must_never_hold_the_window() {
        assert_eq!(resolve_show_timing(false, false, false), ShowTiming::Now);
    }

    /// 当前文档已 mount 成功 → 窗里本就有内容，没有可等的东西。托盘/dock 反复唤出走这条，必须零延迟。
    #[test]
    fn ready_window_shows_immediately() {
        assert_eq!(resolve_show_timing(true, true, false), ShowTiming::Now);
    }

    /// 窗口已在屏上 → 扣它只能先 hide，那是「窗突然消失又回来」的闪烁，比空窗更糟。已上屏只能往前走。
    #[test]
    fn visible_window_is_never_pulled_back() {
        assert_eq!(resolve_show_timing(true, false, true), ShowTiming::Now);
    }

    #[test]
    fn page_started_arms_the_gate() {
        let (s, a) = reduce_mount_gate(initial(), MountGateEvent::PageStarted);
        assert_eq!(a, MountGateAction::Arm);
        assert!(!s.ready);
    }

    #[test]
    fn ready_clears_the_gate() {
        let (s, _) = reduce_mount_gate(initial(), MountGateEvent::PageStarted);
        let (s, a) = reduce_mount_gate(s, MountGateEvent::RendererReady);
        assert_eq!(a, MountGateAction::Clear);
        assert!(s.ready);
    }

    #[test]
    fn timeout_without_ready_reloads_once_then_finalizes() {
        let (s, _) = reduce_mount_gate(initial(), MountGateEvent::PageStarted);
        let (s, a) = reduce_mount_gate(s, MountGateEvent::Timeout);
        assert_eq!(a, MountGateAction::Reload);
        assert!(s.reloaded && s.reloading);

        // reload 后的新文档：PageStarted → 重新武装
        let (s, a) = reduce_mount_gate(s, MountGateEvent::PageStarted);
        assert_eq!(a, MountGateAction::Arm);
        assert!(!s.reloading);

        // 二次超时 → 终局
        let (s, a) = reduce_mount_gate(s, MountGateEvent::Timeout);
        assert_eq!(a, MountGateAction::Fatal);
        assert!(s.finalized);
    }

    /// L1 不变式：reload 在途时到达的**陈旧 ready**（来自 pre-reload 旧文档）必须被丢弃。
    /// 若被采信 → ready=true → 重载页再 C 类失败时 Timeout 走 `ready` 分支 no-op → 门失明、无终局兜底。
    #[test]
    fn stale_ready_during_reload_does_not_blind_the_gate() {
        let (s, _) = reduce_mount_gate(initial(), MountGateEvent::PageStarted);
        let (s, a) = reduce_mount_gate(s, MountGateEvent::Timeout);
        assert_eq!(a, MountGateAction::Reload);

        // 旧文档的在途 ready 到达 → 必须丢弃
        let (s, a) = reduce_mount_gate(s, MountGateEvent::RendererReady);
        assert_eq!(a, MountGateAction::None);
        assert!(!s.ready, "陈旧 ready 被采信 → 门失明");

        // 重载页 mount 又失败 → 必须能升级到终局
        let (s, _) = reduce_mount_gate(s, MountGateEvent::PageStarted);
        let (_, a) = reduce_mount_gate(s, MountGateEvent::Timeout);
        assert_eq!(a, MountGateAction::Fatal, "reload 后仍须能升级终局");
    }

    /// 新文档开始加载必须作废上一文档的 ready，否则重载页天然被判「已就绪」→ 门失明。
    #[test]
    fn page_started_resets_stale_ready_from_previous_document() {
        let (s, _) = reduce_mount_gate(initial(), MountGateEvent::PageStarted);
        let (s, _) = reduce_mount_gate(s, MountGateEvent::RendererReady);
        assert!(s.ready);
        let (s, a) = reduce_mount_gate(s, MountGateEvent::PageStarted);
        assert_eq!(a, MountGateAction::Arm);
        assert!(!s.ready, "新文档必须重新证明能 mount");
    }

    /// finalized 后，除 PageStarted 外一切事件 no-op（终局页已是末路，ready/timeout 不该再驱动任何动作）。
    #[test]
    fn finalized_gate_is_inert_except_new_page_load() {
        let s = MountGateState {
            finalized: true,
            ..initial()
        };
        for ev in [MountGateEvent::RendererReady, MountGateEvent::Timeout] {
            let (next, a) = reduce_mount_gate(s, ev);
            assert_eq!(a, MountGateAction::None);
            assert_eq!(next, s);
        }
    }

    /// **真机实测逼出的不变式**：B 类（load 失败）页面上 `__TAURI_INTERNALS__` 不存在 → 终局页按钮只能
    /// 回退 `location.reload()`（发不出 fatal_retry）。那条路径必须仍能让门复活，否则恢复出来的页面再白屏
    /// 就彻底无兜底 —— 逃生门不能依赖它要救的那套东西还活着。
    #[test]
    fn new_page_load_revives_finalized_gate_without_ipc() {
        let s = MountGateState {
            finalized: true,
            reloaded: true,
            ..initial()
        };
        // 用户在终局页点重载 → location.reload() → 新文档 PageStarted
        let (s, a) = reduce_mount_gate(s, MountGateEvent::PageStarted);
        assert_eq!(a, MountGateAction::Arm, "门必须复活并重新武装");
        assert!(!s.finalized);
        assert!(s.reloaded, "reloaded 须粘滞（防自动重载死循环）");
        // 恢复后的页面能 mount → 门满足
        let (s2, a) = reduce_mount_gate(s, MountGateEvent::RendererReady);
        assert_eq!(a, MountGateAction::Clear);
        assert!(s2.ready);
        // 若恢复后的页面又 C 类失败 → 因 reloaded 粘滞，直接进终局页（不再自动 reload）
        let (_, a) = reduce_mount_gate(s, MountGateEvent::Timeout);
        assert_eq!(
            a,
            MountGateAction::Fatal,
            "手动重载再失败应直落终局，不得再自动 reload"
        );
    }

    /// 防自动重载死循环：`reloaded` 必须跨 reload→PageStarted 粘滞，否则 timeout→Reload→navigate→
    /// PageStarted 重置 reloaded→timeout→Reload… 无限循环烧 CPU。
    #[test]
    fn reloaded_is_sticky_across_navigation_no_infinite_auto_reload() {
        let (s, _) = reduce_mount_gate(initial(), MountGateEvent::PageStarted);
        let (s, a) = reduce_mount_gate(s, MountGateEvent::Timeout);
        assert_eq!(a, MountGateAction::Reload);
        // Reload 触发 navigate → 新文档 PageStarted
        let (s, _) = reduce_mount_gate(s, MountGateEvent::PageStarted);
        assert!(s.reloaded, "reloaded 不得被 PageStarted 重置");
        // 第二次 timeout 必须是 Fatal 而非又一次 Reload
        let (_, a) = reduce_mount_gate(s, MountGateEvent::Timeout);
        assert_eq!(
            a,
            MountGateAction::Fatal,
            "第二次超时必须终局，不得无限自动 reload"
        );
    }

    /// 不变式 6：`fatal_retry` 的 `reset()` 让门**全量**复位（含 reloaded）——用户显式「我要重来一遍」，
    /// 故连自动 reload 的额度也一并归还，区别于上面「手动 location.reload()」的半复位。
    #[test]
    fn fatal_retry_reset_fully_revives_the_gate() {
        // reset() 等价把状态置回 default
        let s = MountGateState::default();
        let (s, a) = reduce_mount_gate(s, MountGateEvent::PageStarted);
        assert_eq!(a, MountGateAction::Arm);
        assert!(!s.reloaded, "reset 后自动 reload 额度须归还");
        let (_, a) = reduce_mount_gate(s, MountGateEvent::Timeout);
        assert_eq!(
            a,
            MountGateAction::Reload,
            "reset 后应重新享有一次自动 reload"
        );
    }

    #[test]
    fn console_rate_limit_admits_up_to_max_then_drops() {
        let mut ts: Vec<u64> = Vec::new();
        for i in 0..10 {
            let (next, admit) = admit_console_message(&ts, 1_000 + i, 1_000, 10);
            ts = next;
            assert!(admit, "窗口内前 10 条应放行");
        }
        let (next, admit) = admit_console_message(&ts, 1_010, 1_000, 10);
        assert!(!admit, "超上限应丢弃");
        assert_eq!(next.len(), 10, "丢弃的条目不得占额度（否则风暴期无界增长）");
    }

    #[test]
    fn console_rate_limit_prunes_outside_window() {
        let ts: Vec<u64> = (0..10).map(|i| 1_000 + i).collect();
        let (next, admit) = admit_console_message(&ts, 5_000, 1_000, 10);
        assert!(admit, "旧时刻出窗后应重新放行");
        assert_eq!(next, vec![5_000]);
    }

    #[test]
    fn truncate_keeps_short_messages_and_marks_long_ones() {
        assert_eq!(truncate_console_message("abc", 10), "abc");
        assert_eq!(truncate_console_message("abcdef", 3), "abc…(+3 chars)");
    }

    /// 多字节字符必须按 char 边界切（按 byte 切会 panic —— 逃生门自己崩了就没救了）。
    #[test]
    fn truncate_is_utf8_safe() {
        let msg = "渲染进程错误：节点配置解析失败";
        let out = truncate_console_message(msg, 4);
        assert!(out.starts_with(
            "渲染进程错"[..]
                .chars()
                .take(4)
                .collect::<String>()
                .as_str()
        ));
        assert!(out.contains("chars)"));
    }

    /// 终局页脚本必须是自洽的一段 JS：HTML 经 JSON 字面量嵌入，不得裸拼引号。
    #[test]
    fn fatal_page_script_embeds_html_as_js_literal() {
        let js = fatal_page_script();
        assert!(js.contains("polaris-fatal-reload"), "须含重载按钮 id");
        assert!(js.contains("__TAURI_INTERNALS__"), "按钮须能回主进程复位门");
        assert!(js.contains("fatal_retry"), "须调 fatal_retry 命令");
        assert!(js.contains("location.reload()"), "invoke 不可用须有回退");
        // innerHTML 赋值必须是 JSON 字面量（带转义的双引号串），而非裸 HTML 拼接
        assert!(
            js.contains(r#"d.body.innerHTML="<div"#),
            "HTML 须以 JS 字符串字面量嵌入"
        );
    }
}
