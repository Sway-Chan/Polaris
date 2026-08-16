//! Polaris IPC 事件 → Tauri 2 event 映射层。
//!
//! Polaris 主进程通过 `webContents.send(channel, data)` 向渲染端推事件（`event:*` / `navigate` 等）。
//! Tauri 2 等价物：[`tauri::AppHandle::emit`]（或 `Emitter::emit_to`）。
//!
//! 本模块收口所有事件通道名常量（单一真值源，防字符串漂移——对齐 上游 `shared/ipc-channels.ts`
//! 的 `EVENT_*` 常量集）+ 统一的广播封装（经 `AppHandle` 广播给所有 webview）。
//!
//! 见系统设计 §B.3（命令/事件映射）。前端经 `@tauri-apps/api/event` 的 `listen(name, cb)` 订阅，
//! 回调签名 `(event: { payload: T }) => void`（payload 即 Polaris 的 `data`）。
//!
//! 注：部分 `EVENT_*` 常量 + `broadcast` / `emit_to_main` 仍为后续批次（stats relay / unlock /
//! speedtest / core-update actor）的预留发射点。已在用：`EVENT_PROXY_STARTED` / `EVENT_PROXY_STOPPED` /
//! `EVENT_CONFIG_CHANGED` / `EVENT_WINDOW_MAXIMIZE_CHANGED`（command 层）+ `EVENT_PROXY_ERROR` /
//! `EVENT_PROXY_INVALID_NODES` / `EVENT_SYSTEM_PROXY_RESIDUAL` / **`EVENT_TAILSCALE_STATUS`（A3 STATUS
//! relay，`proxy.rs::spawn_tailscale_status_relay` 经 `ProxyErrorEmitter::emit_tailscale_status` 发）**
//! 与 **`EVENT_MESH_LOGIN_FALLBACK`（A4 登录期出口让位，`proxy.rs::reconcile_login_fallback` 经
//! `ProxyErrorEmitter::emit_mesh_login_fallback` 发，前端 `App.tsx` 订阅 → toast）**
//! 与 **`EVENT_AUTO_NODE_SWITCHED`（C3 自动换节点，`proxy.rs::do_switch_io` 经
//! `ProxyErrorEmitter::emit_auto_node_switched` 发，前端订阅由 W1-ui 批接）**（runtime 层）
//! 与 **`EVENT_CORE_AUTO_UPDATE_STATUS`（内核自动更新状态，`runtime/core_update_scheduler.rs` 在
//! 检查完成 / 跨带提示 / 暂存 / 落位四个时刻发）** 与 **`EVENT_HELPER_UPGRADEABLE`
//! （启动 T+7s 探测，`runtime/startup_tasks.rs::spawn_helper_upgradeable_probe` 发）**。
//! 仍死（后端零 emit，见 DESIGN-REVIEW）：`EVENT_TAILSCALE_STATE_CLEARED`（无属主归一机制）。预留面保留不删
//! （删后各 actor 批次需重定义通道名，易漂移），故 `dead_code` 全模块放行。

#![forbid(unsafe_code)]
#![allow(dead_code)]

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

/// 上游 `event:*` / 控制类事件通道名（主进程 → 渲染端）。
///
/// 与 `polaris-ipc-channels.ts`（B6 前端移植）保持同名，前端 `listen(EVENT_PROXY_STARTED, ...)` 直连。
/// 这些是「事件推送」单向通道（区别于 `command_*` 双向 invoke），语义不变式见 Polaris ipc-channels.ts 注释。
pub mod channel {
    // 代理生命周期
    pub const EVENT_PROXY_STARTED: &str = "event:proxyStarted";
    pub const EVENT_PROXY_STOPPED: &str = "event:proxyStopped";
    pub const EVENT_PROXY_ERROR: &str = "event:proxyError";
    pub const EVENT_PROXY_INVALID_NODES: &str = "proxy:invalid-nodes";
    // R2 待应用差集 PUSH：`switch_mode` 落盘后单点推 `{added, modified}`（modified 恒空——Polaris 只 defer
    // 新增未引用节点，改/删走重启腿）。前端 Home 待应用操作条据此渲染（`proxy.rs::push_pending_changes` 发）。
    pub const EVENT_PROXY_PENDING_CHANGES: &str = "event:proxyPendingChanges";
    /// **runtime 生命周期结局**（`{phase:'ready'|'stopped'|'failed', errorCode?, message?}`）。
    ///
    /// # 为什么不复用上面那两条，而新开一路
    ///
    /// `EVENT_PROXY_STARTED`/`STOPPED` 的发射点在**命令层**（`commands/proxy.rs` 的
    /// proxy_start/stop/restart）—— 后端**自驱**的核起停（去抖重启 /「立即应用」/ drain 排空 /
    /// 崩溃自愈）一个都不发。把它们收口到 runtime 状态跃迁点确实更正确，但代价不可接受：
    /// `subscription_scheduler` 听 `proxyStarted` 会**真联网**做订阅补更、`core_update_scheduler`
    /// 听 `proxyStopped` 会排换核 —— 收口后每次内部重启都多跑一遍这两件事。
    ///
    /// 故新开一条**只给 UI 的**通道：那两条与两个 scheduler 的语义一字不动，本通道由
    /// `runtime/proxy.rs` 在真状态跃迁点发，订阅方仅待应用操作条 + 连接态显示。
    /// **托盘刻意不订阅**（托盘图标语汇另有待拍板项，不捆进来）。
    pub const EVENT_PROXY_LIFECYCLE: &str = "event:proxyLifecycle";

    // 配置变更
    pub const EVENT_CONFIG_CHANGED: &str = "event:configChanged";

    // 订阅后台自动更新结果（scheduler 每个 due 订阅拉取后发；渲染端仅失败态 toast，对齐 上游
    // 「后台更新只入日志、不弹成功」的静默度——payload={subscriptionId,name,success,error?,
    // addedServers,updatedServers,deletedServers,unchanged}）。手动刷新走 updateServers 命令三态 toast，不经本通道。
    pub const EVENT_SUBSCRIPTION_AUTOUPDATE: &str = "event:subscriptionAutoUpdate";

    /// **单订阅更新的逐阶段进度**（`{subscriptionId, phase, …}`）。
    ///
    /// 与上面那条通道的分工：`AUTOUPDATE` 是**后台腿的结局**（scheduler 专属，只喂失败 toast），
    /// 本通道是**任一腿的过程**——手动刷新与 scheduler 共用 `perform_subscription_update`
    /// （§K7.1 唯一生产路径），故一个发射点覆盖两条腿，订阅信息栏无论谁发起都亮。
    ///
    /// phase 取值与载荷（发射点见 `commands/subscription.rs::perform_subscription_update`）：
    /// - `"fetching"` —— 主正文拉取中（最长 `MAIN_FETCH_TIMEOUT_MS` = 30s，绝大多数时间在这）；
    /// - `"providers"` + `{done, total}` —— Clash proxy-providers 逐个拉取（**串行**，见
    ///   `resolve_proxy_providers` 的声明序执行）；`done` = 已完成数，故首帧是 `0/n`；
    /// - `"applying"` —— 对账 + 落盘 + 广播；
    /// - `"done"` + `{added, updated, deleted}` / `"unchanged"` / `"failed"` + `{error}` —— 终态。
    ///
    /// **终态必达**由结构保证（不是靠时限兜底）：`perform_subscription_update` 是一层薄壳，
    /// 内层无论从哪条 `return` 出来，外层都从它的返回值派生终态帧。新增 early-return 不会漏发。
    ///
    /// **不经 `EmitGate` 节流**：一次更新至多 `1 + min(providers,8) + 1 + 1 ≈ 11` 帧、跨几十秒，
    /// 不是数据流。给它套节流器只会引入一个本不存在的 trailing-edge 问题。
    pub const EVENT_SUBSCRIPTION_UPDATE_PROGRESS: &str = "event:subscriptionUpdateProgress";

    // 日志（批量，~150ms coalesce）
    pub const EVENT_LOG_RECEIVED_BATCH: &str = "event:logReceivedBatch";

    // stats 订阅驱动数据面（topic → 推送通道）
    pub const EVENT_STATS_UPDATED: &str = "event:statsUpdated";
    pub const EVENT_CONNECTIONS_AGGREGATE: &str = "event:connectionsAggregate";
    /// 完整活动表的拓扑相关字段发生变化（小信号，不携带有损 Top-N 投影）。
    /// 首页“连接流向”处于检索态时据此重查后端完整表；常态图仍只消费 aggregate 小载荷。
    pub const EVENT_CONNECTIONS_TOPOLOGY_CHANGED: &str = "event:connectionsTopologyChanged";
    pub const EVENT_CONNECTIONS_DETAIL: &str = "event:connectionsDetail";
    pub const EVENT_CONNECTIONS_CLOSED: &str = "event:connectionsClosed";

    // 隐私模式
    pub const EVENT_ENTER_PRIVACY_MODE: &str = "event:enterPrivacyMode";
    pub const EVENT_EXIT_PRIVACY_MODE: &str = "event:exitPrivacyMode";

    // 路由 / 导航
    //
    // `EVENT_NAVIGATE`（上游 `navigate`）已删除——**架构化石，非漏接**。上游 用**原生** Tray/Menu，
    // 菜单项点击在主进程侧 emit `navigate` 让渲染端 router.push 到某屏（TrayManager.ts:658/687、
    // tray-actions.ts:107/201/237）。Polaris 的托盘改成**同源 webview 浮层**（`tray.rs` + `ui/src/tray/`）：
    // 浮层自己渲染节点列表并 `setView` 切自身子视图，其余动作 `invoke('tray_show_main')` 只是把主窗显示出来
    // ——**无任何路径需要「跨窗口令主窗跳到第 N 屏」**。深链跳转亦不存在（本仓无 deep-link 插件，
    // 仅 `polaris-icon://` 图标 scheme）。两端皆零 emit / 零订阅，保留只会诱导后人为「对齐 上游」重造假接线。
    // 前端 `ui/src/domain/ipc-channels.ts` 的同名声明由主线程一并删除（不在本 crate 范围）。
    //
    // ⚠️ 下面这条**不是** `EVENT_NAVIGATE` 的复活，别把它当通用路由用（见 `tray.rs::normalize_tray_screen`
    // 的选型注释）：上游 那条通道之所以被删，是因为它是「主进程可令渲染端跳任意路由」的开放面，而
    // Polaris 的浮层自渲染子视图、只有**一个**真反例——「打开设置」（设置屏只存在于主窗）。
    // 故这里给的是窄通道：
    //  · 值域由 Rust 侧白名单 `normalize_tray_screen` 钉死（当前仅 `"settings"`），前端传任意串也只能命中登记项；
    //  · 单播主窗（`emit_to_main`），不广播；
    //  · 唯一发射点 = `tray::tray_show_main` + 原生兜底菜单的「打开设置」项。
    // 想加第二个目标屏 → 必须同时改白名单 + 补 `tray.rs` 的白名单单测，成本落在该落的地方。
    pub const EVENT_TRAY_OPEN_SCREEN: &str = "event:trayOpenScreen";

    // 内核版本 / 自动更新状态
    pub const EVENT_CORE_VERSION_CHANGED: &str = "event:coreVersionChanged";
    pub const EVENT_CORE_AUTO_UPDATE_STATUS: &str = "event:coreAutoUpdateStatus";
    pub const EVENT_CORE_BASELINE_WARNING: &str = "event:coreBaselineWarning";

    // helper 可升级
    pub const EVENT_HELPER_UPGRADEABLE: &str = "event:helperUpgradeable";

    // 自动换节点
    pub const EVENT_AUTO_NODE_SWITCHED: &str = "event:autoNodeSwitched";

    // IP 信息
    pub const EVENT_IP_INFO_UPDATED: &str = "event:ipInfoUpdated";

    // 解锁检测
    pub const EVENT_UNLOCK_PROGRESS: &str = "event:unlockProgress";
    pub const EVENT_UNLOCK_INVALIDATED: &str = "event:unlockInvalidated";
    pub const EVENT_UNLOCK_UPDATED: &str = "event:unlockUpdated";

    // 规则资源下载进度
    pub const EVENT_RULE_RESOURCE_PROGRESS: &str = "event:ruleResourceProgress";

    // 测速（逐节点 / 进度）
    //
    // `EVENT_SPEED_TEST_RESULT_LIST`（上游 `speedTestResult`，"全量结果列表"）已删除——**架构化石，非漏接**，
    // 同 `EVENT_NAVIGATE`（见上）的处置先例。上游 用**原生** Tray 菜单：主进程跑完托盘测速后无从让原生菜单
    // 自己刷新，只能把整个结果数组推给渲染端做汇总 toast + 合并写入。Polaris 的托盘是**同源 webview 浮层**
    // （`ui/src/tray/TrayMenu.tsx`），它与主窗**共用** `use-latency-store` 并各自直订逐节点的
    // `EVENT_SPEED_TEST_RESULT`（`api-client.ts:390`）——store 的写入口本就是 `{...latencyMap, ...新值}`
    // 的**合并**语义（`use-latency-store.ts:45/47`）。
    //
    // 故能力契约 L131 的「托盘测速结果**合并非替换**」这条要求**已由逐节点通道结构性满足**，汇总通道在这个
    // 架构里没有承担任何独有语义：补它等于给同一事实造第二个真值源（两条通道谁先到、谁覆盖谁），删它则
    // 零行为变化。两端皆零 emit / 零订阅，保留只会诱导后人为「对齐 上游」重造假接线。
    pub const EVENT_SPEED_TEST_RESULT: &str = "event:speedTestResult";
    pub const EVENT_SPEED_TEST_PROGRESS: &str = "event:speedTestProgress";
    /// **一轮测速的终态**（`{outcome,tested,total,serverIds:[serverId],pending:[serverId]}`）。
    ///
    /// # 为什么必须是事件，而不是继续用 command 的返回值
    ///
    /// `commands/speedtest.rs` 的三条腿（主核池 / 回退 / 临时核）本来就各自返回 `"outcome"`，但那是
    /// **`ApiResponse` 的返回值**：只有**发起那次 invoke 的那个 JS 堆**拿得到。托盘浮层是独立 webview /
    /// 独立 JS 堆 ⇒ 托盘发起的那轮测速，主窗的进度 toast 结构上收不到终态，只能靠「静默 N 秒」去猜
    /// 「是不是被打断了」—— 而后端在 `is_superseded` 命中那一刻**当场就知道**并已经返回了 `interrupted`。
    /// 「断开为什么还要等十几秒才显示中断」的全部根因就在这个信道错配上（陈先生 2026-07-31 指出）。
    ///
    /// 故终态改**广播**：谁发起的都不重要，任何窗口订上就能立刻收敛。前端的静默超时随之降级为
    /// **纯兜底**（防事件丢失 / 后端异常退出），不再是主路径。
    ///
    /// # `pending`：本轮**没拿到值**的节点 id（中断后「继续」的输入）
    ///
    /// 判据 = 本腿**已裁定要测**的集合 − `results` 里已出值的集合。复用的是既有那条诚实性根基
    /// 「让位未测的节点一律缺席、绝不写假 -1」⇒ 缺席的就是没测的。
    ///
    /// **差集必须由后端算**：前端只在自己发起时才知道请求集，托盘发起的那轮主窗根本没有请求集
    /// （同上一节的信道错配）。波前预筛掉的 `notInPool`/`dirty`/`tsNotReady` **不在** `pending` 里
    /// ——它们不是「没轮到测」，而是「本轮结构上就不该测」，各自有独立的返回字段与各自的修法
    /// （重启纳入 / 应用更改 / 去登录），混进续测集合只会让「继续」原地再失败一次。
    pub const EVENT_SPEED_TEST_DONE: &str = "event:speedTestDone";

    // Tailscale
    pub const EVENT_TAILSCALE_AUTH_URL: &str = "event:tailscaleAuthUrl";
    pub const EVENT_TAILSCALE_STATUS: &str = "event:tailscaleStatus";
    pub const EVENT_TAILSCALE_STATE_CLEARED: &str = "event:tailscaleStateCleared";

    // mesh 登录期出口让位
    pub const EVENT_MESH_LOGIN_FALLBACK: &str = "event:meshLoginFallback";

    // 系统代理残留提示
    pub const EVENT_SYSTEM_PROXY_RESIDUAL: &str = "event:systemProxyResidual";

    // 窗口最大化态变更（标题栏跟随）
    pub const EVENT_WINDOW_MAXIMIZE_CHANGED: &str = "event:windowMaximizeChanged";

    // App 更新进度
    pub const EVENT_UPDATE_PROGRESS: &str = "update:progress";

    // App 更新弹窗四态（独立 mini 窗）
    pub const UPDATE_POPUP_STATE: &str = "update:popupState";
}

/// 向所有 webview 广播事件（上游 `ipcEventEmitter.sendToAll` 等价）。
///
/// Polaris 遍历注册的 BrowserWindow 调 `webContents.send`；Tauri 2 下多 webview 由 `emit`
/// 自动 fan-out 给所有监听该事件的窗口。payload 经 serde 序列化，与 上游 `data` 同形。
///
/// 找不到窗口（启动极早期 / 全部关闭）不报错——对齐 上游 `!window.isDestroyed()` 的静默跳过。
pub fn broadcast<T: Serialize + Clone>(handle: &AppHandle, name: &str, payload: T) {
    // emit 对零监听者 / 无窗口都是 no-op（仅记日志，不 panic）。
    if let Err(e) = handle.emit(name, payload) {
        log::warn!("emit event `{name}` failed: {e}");
    }
}

/// 向主窗口（label = "main"）单播事件（上游 `sendToWindow` 等价）。
///
/// 用于仅需主窗口消费的事件（当前 Polaris 单窗口模型下与 [`broadcast`] 等价，保留 API 以备多窗）。
pub fn emit_to_main<T: Serialize + Clone>(handle: &AppHandle, name: &str, payload: T) {
    if let Some(window) = handle.get_webview_window("main") {
        if let Err(e) = window.emit(name, payload) {
            log::warn!("emit event `{name}` to main window failed: {e}");
        }
    }
}
