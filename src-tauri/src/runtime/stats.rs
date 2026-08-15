//! stats 运行时：`polaris-stats-engine` 订阅注册表 + 流 relay。
//!
//! Polaris 锚点：`StatsSubscriptionRegistry`（`main/services/StatsSubscriptionRegistry.ts`）+
//! `StatsService.ts` / `StatsWorkerHost` 的 connections 长驻流 + change-driven 签名去重（issue #227
//! 把「连接风暴」挡在 main 侧：载荷与连接总数解耦，只在聚合内容真变时才推一帧）。
//!
//! renderer 按 topic（stats | aggregate | detail | closed）声明订阅 → main 据订阅集派生 worker demand
//! + 精确 relay 给订阅者。订阅即回初始帧（合并旧 GET 初值路径）。
//!
//! # 连接数据面：一条长驻流 + 三种投影（aggregate 拓扑 / detail 活动 / closed 已结束）
//!
//! 三个连接事件通道由**同一条**
//! `SubscribeConnections` 长驻流供数（[`run_connections_stream`]）：流帧维护一张
//! [`StatsAggregator`] 活动连接表；CLOSED 在删表前另存入有界历史环。三种视图同源且互不污染，
//! 上游只订一次。
//!
//! **此前是两条各自轮询的 poller**（每 250ms / 1s 各拉一次 `first_connection_snapshot` 全量表）。
//! 换流的判据：内核对 NEW/CLOSED 本就是事件驱动即时推送
//! （`daemon/started_service.go:752` 的 `case event := <-subscription`，只有 UPDATE 走 ticker）——
//! 轮询等于把一个推送接口当轮询接口用，既白等半拍，又每拍重付一次含 ≤1000 条死连接的全量表。
//!
//! 帧到达 → 更新活动表与历史环（O(1)/事件）→ 三条 [`polaris_stats_engine::EmitGate`] 各自合并节流。
//! aggregate 另有**签名去重**（`aggregate_signature`，同内容不推，issue #227）；detail 不去重
//! （渲染端靠相邻两帧差分算每条连接的速率，理由见 [`run_connections_stream`]）。
//!
//! 生命周期：订阅时起（单例幂等）、**三个连接投影都**退订/窗口关闭时停；
//! 核未运行时**不碰 gRPC**（推一帧离线态后等核起）。
//!
//! # 流量数据面：`SubscribeStatus` 长驻流（stats topic）
//!
//! `EVENT_STATS_UPDATED`（StatusBar 的上下行速率 + 累计 + 连接数）由 [`run_stats_stream`] 供数：
//! 一条 `SubscribeStatus` 长驻流 → [`StatsAggregator::on_status`] → emit。
//!
//! **此前是一条 1s 轮询**（每拍拉一次 `first_connection_snapshot` 全量表，对整表的
//! `uplink_total` 求和再跨拍差分）。换掉它的判据是**口径**不是性能：那个和**不过滤已关闭连接**，
//! 而内核的死连接历史环有 1000 条上限，环满后每淘汰一条，"累计总量"就**下跌**一截 ——
//! 累计读数会倒退，且 `saturating_sub` 把那一拍的速率吃成 0（连接高频起落时速率系统性偏低）。
//! `SubscribeStatus` 直给 `trafficcontrol.Manager.Total()`：两个只增的 `atomic.Int64`，
//! 关连接时 `leave()` 不减 ⇒ **结构上不可能回退**。
//!
//! ⚠️ 此前登记的两条拦路条**都是错的，勿再据以判断**：
//! - 「`Status.connectionsIn/Out` 内核不填、恒 0，故第五项仍得靠连接表」——`readStatus()`
//!   （`daemon/started_service.go:417`）两个字段都填：`ConnectionsOut = connectionManager.Count()`
//!   （`box.go:233` 无条件注册）、`ConnectionsIn = trafficManager.ConnectionsLen()`
//!   （daemon gRPC 走 `needAPIService`，该 manager 必被构造，`box.go:245`）。
//!   `connectionsIn` 恰是 `SubscribeConnections` 首帧里活连接的条数，是精确 drop-in。
//! - 「消费 tonic 流需 `futures::StreamExt` 而本 crate 只有 dev-dependency」——`recv()` 是
//!   [`polaris_singbox_grpc::ReconnectingStream`] 的固有方法，连接流早就这么用了。
//!
//! ⚠️ **`Status.uplink` / `downlink` 不是速率**，直接拿来用会得到恒 0：内核从不在 `readStatus()`
//! 里给它们赋值，是 `SubscribeStatus` 的循环每拍算一次 `UplinkTotal - uploadTotal` 再写回
//! （:408-413），**首帧在任何 tick 之前就 `Send`，两者恒 0**；而把增量折成速率所需的窗口长度
//! （服务端 ticker 的实际间隔）根本不在 wire 上。故速率一律由 [`StatsAggregator::on_status`]
//! 对累计做差分、除以**客户端实测 Δt**。
//!
//! # 降流门（维度7：无 UI 消费者时不拉取、不 emit）
//!
//! 契约的另一条腿：数据面需求 **不只**由订阅集派生，还乘上窗口可见性
//! （[`SubscriptionRegistry::should_stream`]；全部 topic 口径一致，均受可见性门控——Stats 曾是例外，
//! 该例外为何作废见 `polaris_stats_engine::Topic::gated_by_visibility` 的文档）。
//!
//! 两条腿现在**同一种机制**（[`StreamGate::wait_until`]）：判定为假 → **drop 流**，门再开时重订阅。
//! park 住不读流毫无意义 —— 帧会堆在 tonic 缓冲与内核发送窗口里，反而堵住内核的事件分发。
//! （连接流的重订阅必然收到一帧 `reset=true` 全量表，断流期间消失的连接靠它清掉，那些连接的 CLOSED
//! 永不补发——见 `polaris_stats_engine` 的 `reset帧整表替换而非增量叠加`；Status 流的重订阅则
//! 必须丢掉速率差分基线，理由见 [`StatsAggregator::on_status`]。）
//!
//! 收托盘/最小化后两条腿一起停手：两条上游流断开、逐秒全量明细 JSON 归零，
//! 笔电不再为没人看的画面付电。断流期的兜底实况回读恒按 [`PARK_RECHECK_INTERVAL`]，
//! **不跟随任何 emit 间隔**——隐藏态下高频空转等于把降流的收益吐回去。
//!
//! 可见性真值来源：**窗口实况回读**（`is_visible() && !is_minimized()`，对齐 上游
//! `isUiBroadcastActive`），**不是** `WindowEvent::Focused`——失焦但仍在屏上的窗口依然有 UI 消费者。
//! Tauri 2 的 `WindowEvent` 没有 show/hide 变体，故实况回读按 [`PARK_RECHECK_INTERVAL`] 兜底重跑，
//! `main.rs` 的显隐写入点（`Focused` / 收托盘 / 单实例唤起）只作「显隐可能刚变」的**即时**触发器
//! （[`StatsRelay::refresh_window_visible`]）：门一变即经 `watch` 唤醒等在门上的 relay，
//! 恢复不等兜底周期，用户切回窗口无可感知空窗。
//!
//! ⚠️ **回读本身跑在主线程、relay 只读缓存**（见 [`VisibilityCache`]）：窗口 getter 是「投消息进
//! 主事件循环 + 阻塞等回包」，直接在 relay 里调会在主循环被原生模态（提权框/菜单跟踪）占住时
//! 一次把两条后台腿一起挂死在 `recv` 上。

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tauri::{AppHandle, Manager};
use tokio::sync::watch;

use polaris_config_engine::builder::is_probe_pool_inbound_tag;
use polaris_singbox_grpc::{daemon, Endpoint, ReconnectConfig, SingBoxApiClient};
use polaris_stats_engine::{
    aggregate_connections_with_topn, aggregate_signature, trim_connection, ClosedConnectionEntry,
    ConnectionEntry, ConnectionEventType, ConnectionsAggregate, ConnectionsClosedSnapshot,
    ConnectionsSnapshot, EmitGate, SingBoxConnection, SingBoxConnectionEvent,
    SingBoxConnectionEvents, SingBoxProcessInfo, SingBoxStatus, StatsAggregator,
    SubscriptionRegistry, SubscriptionToken, Topic, TrafficStats, TOPOLOGY_TOP_N,
};

use crate::events::{
    broadcast,
    channel::{
        EVENT_CONNECTIONS_AGGREGATE, EVENT_CONNECTIONS_CLOSED, EVENT_CONNECTIONS_DETAIL,
        EVENT_STATS_UPDATED,
    },
};
use crate::runtime::config::ConfigManager;
use crate::runtime::proxy::ProxyRuntime;

/// `SubscribeStatus` 请求里的 `interval`（纳秒）—— **服务端推 Status 帧的节奏**。
///
/// 取 1s：一帧 Status 就是 StatusBar 上那五个数字，而它们是「秒级平均」的语义。推得更勤只会放大
/// 内核累计字节的采样抖动（速率读数更跳而不是更准），并让渲染端按同样的频率白重渲。
///
/// ⚠️ **本值不参与速率计算**，别把它当分母：服务端 `interval <= 0` 会兜底成 1s，且实际间隔含
/// ticker 调度抖动，wire 上也不回传实际值。速率的分母恒是 [`StatsAggregator::on_status`] 的
/// **实测 Δt**（见该方法文档）。本值改成 500ms 或 2s，速率读数都仍然正确。
const STATS_STREAM_INTERVAL_NS: i64 = 1_000_000_000;

/// aggregate（拓扑）emit 的**下限间隔** —— 注意语义：不是拉取周期。
///
/// # 前身与它为何换了语义
///
/// 本常量的前身是 `AGGREGATE_POLL_INTERVAL`（拓扑轮询周期，同为 250ms）。轮询时代它一身兼两职：
/// **多久拉一次内核**（成本）与**多久推一帧给渲染端**（观感）。改成长驻流后前一职消失
/// —— 内核对 NEW/CLOSED 是 `case event := <-subscription` 事件驱动即时推送
/// （`daemon/started_service.go:752`，只有 UPDATE 走 ticker），我们不再「问」，只是「收」。
///
/// 于是当年那半条成本判据（「每拍新建一条订阅流，服务端构造活跃连接 + ≤1000 条死连接历史环的
/// 全量 protobuf ≈ 200–500 KB/拍，且这段在签名去重的上游、随节拍线性上涨」）**整段作废**：
/// 长驻流一次订阅只付一次首帧全量，此后全是增量。**下界不再由 gRPC 成本决定。**
///
/// # 现在的取值判据
///
/// - **下界 250ms（观感 + 渲染成本，不再是 gRPC 成本）**：每一次 emit 都要 O(n log n) 聚合 + Top-N +
///   过 IPC + 渲染端重排整张拓扑图。而拓扑节点的出现/消失在 250ms 与 100ms 之间没有可分辨差异 ——
///   `.link` / `.node` 的 opacity 过渡本身就是 160ms（`ui/src/styles/components.css:224`），
///   比 100ms 还长。**「实时」不等于越快越好**：再快只是让渲染端多做功，用户一帧都多看不到。
/// - **上界 350ms**：拓扑答的是「此刻有哪些连接、走哪个出口」，用户点开一个网页就等着看新节点冒出来。
///   1s 一拍在交互上是「反应了一下」；250ms 落在 Nielsen 的 0.1s/1s 两道门之间偏 0.1s 一侧，已是「跟手」。
///
/// # 真正的延迟改善来自换流，不来自本常量
///
/// 轮询时代一次变化的可见延迟是「≤一拍 + RTT」（平均半拍 ≈ 125ms 的等待纯属白等）。
/// 长驻流下**事件发生即到达**，本常量只在「上一次 emit 之后不足 250ms 又来了变化」时才生效，
/// 且那种情况下推迟的也只是**合并后的一帧**（见 [`polaris_stats_engine::EmitGate`] 的尾沿保证）。
/// 空闲时一次孤立的连接变化 → 延迟 ≈ RTT，与本常量无关。
///
/// 区间由 `aggregate_emit间隔取值区间` 锁死。
const AGGREGATE_EMIT_MIN_INTERVAL: Duration = Duration::from_millis(250);

/// detail（连接明细）emit 的下限间隔。
///
/// 前身是 detail 那条 poller 的轮询周期（1s）。判据同样只留下与拉取无关的那半条：
/// detail 是两条投影里**载荷最大**的一条（全量连接明细逐帧下发、不做签名去重，见
/// [`run_connections_stream`] 里 detail 分支的说明），而明细表是给人逐行读的，1s 已快于人眼扫表的速度。
///
/// **比 aggregate 慢一档是刻意的**：同一张连接表，拓扑那条推的是几十个计数，明细那条推的是
/// 整张表的 JSON。两者共用一条上游流，但没有理由共用一个 emit 频率。
const DETAIL_EMIT_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// 已结束连接只保留最近 1000 条，对齐 sing-box 重置帧能重放的历史上限。
/// 再高只在本进程期间有效，连接流重订后无法补回，反而会制造不一致。
const MAX_CLOSED_HISTORY: usize = 1_000;

/// 已结束历史的全量快照最多每秒推一次；连接风暴时合并 CLOSED，避免 N 次断连复制 N 次千行 JSON。
const CLOSED_EMIT_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// `SubscribeConnections` 请求里的 `interval`（纳秒）—— **只管服务端 UPDATE 帧的节奏**。
///
/// 对齐 上游 `CONNECTIONS_INTERVAL_NS = 1_000_000_000`（`StatsService.ts:20`）。
///
/// 容易误读，钉清楚：这个值**不影响 NEW / CLOSED 的延迟**。服务端 `SubscribeConnections` 的
/// 事件分支（`case event := <-subscription`）与 ticker 分支是并列的两条腿 ——
/// 连接建立/断开当刻即推，ticker 只驱动 `buildTrafficUpdates`（per-connection 字节增量）。
/// 取 1s 是因为 UPDATE 的唯一消费者是明细表的每条连接速率，而那张表本身按
/// [`DETAIL_EMIT_MIN_INTERVAL`] 每秒推一帧 —— 让内核比我们推得更勤没有意义。
///
/// 服务端 `interval <= 0` 会兜底成 1s，故取值不会退化成忙转。
const CONNECTIONS_STREAM_INTERVAL_NS: i64 = 1_000_000_000;

/// 断流待命期的兜底实况回读周期（**恒 1s，不跟随任何 emit 间隔**）。
///
/// Tauri 2 没有 show/hide 事件，已断流、等在门上的 relay 只能定期回读窗口实况兜底
/// （详见 [`StreamGate::wait_until`]）。隐藏态下每回读一次就要取一次 registry 锁 +
/// 投一次主线程可见性回读，把这个周期调快等于把降流省下的电烧回去。
///
/// 第二个用途（**同一个数字，两条理由**）：两条 relay 流循环里的兜底唤醒周期 ——
/// `ReconnectingStream` 断了自己重连、永不 yield 错误，故「核停了 / 核换端口重启了」这两件事
/// 必须靠定期复核 `proxy.status()` 才发现得了。
///
/// 恢复延迟不受它影响 —— 门变更（`epoch` bump）才是立刻唤醒的那条腿，本常量只是「事件丢了」的兜底。
const PARK_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

/// 主窗 label。渲染端订阅只来自主窗（`commands::stats` 按 `window.label()` 记账）；托盘浮层是
/// 独立 label 的 `tray.html`，不订阅任何 stats topic，故不该把它算作「有 UI 消费者」。
pub(crate) const MAIN_WINDOW_LABEL: &str = "main";

/// 一条运行中的后台 relay 任务（单例槽位的内容物）。
///
/// 名字留作 `AggregatePoller` 是历史沿革（最早只有 aggregate 一条，且是轮询）；现在两个使用者
/// 都是长驻流 —— **连接流**（[`run_connections_stream`]）与 **Status 流**（[`run_stats_stream`]），
/// 结构本身与「轮询」无关 —— 只是「停机标志 + 任务句柄」。
struct AggregatePoller {
    /// 协作停机标志（relay 每轮外循环 top 检查；退订/窗口关即置 true）。
    stop: Arc<AtomicBool>,
    /// 后台任务句柄（abort 作硬兜底，令 sleep/在飞 gRPC 立即取消）。
    handle: tauri::async_runtime::JoinHandle<()>,
}

/// 降流门的共享态：订阅注册表 + 门变更信号。
///
/// 为何要从 [`StatsRelay`] 里拆出来 `Arc` 共享：两条 relay 是 `tauri::async_runtime::spawn` 出的
/// 独立后台任务，须反复读同一份订阅集 × 可见性做降流判定，而 `StatsRelay` 是 `State`-managed、
/// 后台任务拿不到它的引用。这不是新增抽象层，只是把两个已有字段挪进一个可共享的所有者。
struct StreamGateState {
    /// 纯逻辑订阅注册表（topic 计数 + 可见性门控判定，判定本体见 `should_stream`）。
    registry: Mutex<SubscriptionRegistry>,
    /// 门变更代次（订阅 / 退订 / 可见性翻转即 +1）。等在门上的 relay 靠 `watch` 立刻醒——
    /// `watch` 记版本而非边沿信号，故「判定为假」与「开始等」之间发生的 bump 不会丢。
    epoch: watch::Sender<u64>,
    /// 主窗可见性缓存（relay 只读它，**从不**碰窗口 getter）。
    vis: VisibilityCache,
}

/// 主窗可见性的缓存 + 刷新记账。
///
/// # 为什么 relay 不能自己回读窗口
///
/// tauri-runtime-wry 的窗口 getter（`is_visible` / `is_minimized`）是「往主事件循环投一条消息 +
/// `rx.recv()` **阻塞**等回包」（`window_getter!` → `getter!`；非主线程走 `proxy.send_event` 分支）。
/// 主循环被原生模态 / 菜单跟踪 / 提权框（`helper_install` / `helper_uninstall` 都会弹）占住时，
/// 两条 relay 会**同时**把两个 tokio worker 挂死在 `recv` 上，而且每收一帧一次、贯穿整段模态期。
///
/// 故改成两段：
///  - **读**：relay 一次原子 load，永不阻塞；
///  - **写**：`AppHandle::run_on_main_thread` 投一个闭包给主循环（非主线程时只是一次 channel send，
///    不等回包），闭包**在主线程里**跑 getter —— `send_user_message` 对主线程走内联分支，不会自死锁。
///    主循环忙时这次刷新只是排队等，relay 照常用上一份真值继续跑。
struct VisibilityCache {
    /// 最近一次回读到的可见性。**缺省 true**：与 getter 报错时的兜底方向一致
    /// （宁可多流一拍，绝不误把还在屏上的 UI 饿死）。
    visible: AtomicBool,
    /// 是否已有一次刷新在飞（两条 relay 各自反复投递 → 去重成一次）。
    refreshing: AtomicBool,
    /// 连续回读失败次数（限频告警的判据，见 [`should_warn_visibility_failure`]）。
    error_streak: AtomicU64,
}

/// 纯判定：可见性回读连续失败第 `streak` 次该不该 warn。
///
/// 1 / 10 / 100 次各一条，此后每 1000 次一条。
///
/// **不能只发第一条**：平台性持续失败时降流门整体退化成「恒可见」（两条上游流永不断开、
/// 逐秒全量明细 JSON 照发），那条独苗日志早被淹了 —— 于是「降流失效」这件事零可观测。
/// 也不能每次都发：两条 relay 各按自己的帧率投递（合计每秒数条），日志被自己刷爆。
#[must_use]
const fn should_warn_visibility_failure(streak: u64) -> bool {
    matches!(streak, 1 | 10 | 100) || (streak > 100 && streak.is_multiple_of(1000))
}

impl StreamGateState {
    fn new() -> Self {
        Self {
            registry: Mutex::new(SubscriptionRegistry::new()),
            epoch: watch::channel(0).0,
            vis: VisibilityCache {
                visible: AtomicBool::new(true),
                refreshing: AtomicBool::new(false),
                error_streak: AtomicU64::new(0),
            },
        }
    }

    /// 主窗可见性（**非阻塞**）：读缓存，并顺带投递一次主线程刷新。
    ///
    /// 「读的同时投刷新」是刻意的：relay 每次过门（收帧后重入 select、或断流待命期的兜底回读）
    /// 都会调它，刷新节拍便自然跟着数据面走 —— 不必另起一条定时器，也不会因为有两条 relay
    /// 而变成双倍投递（`refreshing` 去重）。
    fn cached_window_visible(self: &Arc<Self>, app: &AppHandle) -> bool {
        self.spawn_visibility_refresh(app);
        self.vis.visible.load(Ordering::Relaxed)
    }

    /// 投递一次主线程可见性回读（已有一次在飞 → no-op）。
    fn spawn_visibility_refresh(self: &Arc<Self>, app: &AppHandle) {
        if self.vis.refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        let app_for_probe = app.clone();
        // 主线程调用时 `run_on_main_thread` 内联执行该闭包（tauri 的 `send_user_message` 对
        // 主线程走内联分支）；非主线程时只是一次 channel send —— 两种情形都不阻塞调用方。
        if app
            .run_on_main_thread(move || {
                let probe = probe_main_window_visible(&app_for_probe);
                this.apply_visibility_probe(probe);
                this.vis.refreshing.store(false, Ordering::SeqCst);
            })
            .is_err()
        {
            // 事件循环已退出（收尾期）→ 必须复位闸，否则此后再也不会有刷新排上队。
            self.vis.refreshing.store(false, Ordering::SeqCst);
        }
    }

    /// 落一次回读结果（成功 → 写缓存 + 门；失败 → 兜底「可见」+ 限频告警）。
    fn apply_visibility_probe(&self, probe: Result<bool, String>) {
        match probe {
            Ok(visible) => {
                self.vis.error_streak.store(0, Ordering::Relaxed);
                self.store_window_visible(visible);
            }
            Err(e) => {
                let streak = self.vis.error_streak.fetch_add(1, Ordering::Relaxed) + 1;
                if should_warn_visibility_failure(streak) {
                    log::warn!(
                        "主窗可见性回读连续失败 {streak} 次（{e}）：降流门已整体退化为「恒可见」\
                         —— 两条长驻流将一直开着并持续 emit，收托盘/最小化不再省电"
                    );
                }
                // 失败安全方向：宁可多流，绝不把还在屏上的 UI 饿死。
                self.store_window_visible(true);
            }
        }
    }

    /// 写可见性缓存 + 同步进降流门（变了才 bump → 等在门上的 relay 立刻醒，恢复不等兜底周期）。
    fn store_window_visible(&self, visible: bool) {
        self.vis.visible.store(visible, Ordering::Relaxed);
        self.set_window_visible(visible);
    }

    /// 门代次 +1 → 唤醒全部等在门上的 relay（无接收者时是纯 no-op）。
    fn bump(&self) {
        self.epoch.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// 某 topic 此刻是否应 emit（订阅集 × 可见性）。锁毒化 → 保守判否（不做无人消费的 I/O）。
    fn should_stream(&self, topic: Topic) -> bool {
        match self.registry.lock() {
            Ok(r) => r.should_stream(topic),
            Err(e) => {
                log::warn!("stats registry lock: {e}");
                false
            }
        }
    }

    /// 写入窗口可见性；**变了才** bump（否则每次兜底实况回读都会白唤醒两条 relay）。
    fn set_window_visible(&self, visible: bool) {
        let changed = match self.registry.lock() {
            Ok(mut r) => {
                let changed = r.window_visible() != visible;
                if changed {
                    r.set_window_visible(visible);
                }
                changed
            }
            Err(e) => {
                log::warn!("stats registry lock: {e}");
                false
            }
        };
        if changed {
            log::debug!("stats 降流门：窗口可见性 → {visible}");
            self.bump();
        }
    }
}

/// 一条长驻流的降流门句柄（共享门态 + 该流的需求判据 + 自己的变更接收端）。
///
/// # 降流的动作是 drop 流，不是 park
///
/// 轮询时代的降流是「这一拍不拉取」——不拉取就等于不产生任何成本。长驻流下没有「拍」，也没有
/// 「不拉取」这个动作：流是**内核在推**。park 住不去读它，帧只会堆在 tonic 的接收缓冲和内核的
/// gRPC 发送窗口里，直到把窗口打满、把内核那条 goroutine 阻塞在 `server.Send` 上 ——
/// 我们非但没省，还给内核的事件分发添了堵。
///
/// 故降流语义是 **drop 流**：判定为假 → 丢掉 `ReconnectingStream`（连同它的重连 future），
/// TCP 连接自然关闭，内核那侧 `server.Context().Done()` 触发、`UnSubscribeEvents` 退订，
/// **整条链路上的成本真正归零**。判定为真 → **重新订阅**。
///
/// ⚠️ 重订阅一律从「一份新的真相」开始：连接流必然收到一帧 `reset=true` 全量表
/// （`daemon/started_service.go:728` 在建 ticker 前无条件 `Send`），断流期间消失的连接只能靠它清掉
/// （见 `polaris_stats_engine::aggregator` 的 `reset帧整表替换而非增量叠加`）；Status 流则必须丢掉
/// 速率差分基线（否则整段断流期的平均吞吐会被当成"此刻的速率"显示一帧）。
/// 两者都由调用方在建流后 `StatsAggregator::reset()` 一次做掉。
struct StreamGate {
    state: Arc<StreamGateState>,
    epoch: watch::Receiver<u64>,
    /// 本条流的**需求判据**。两条流的需求面不同，判定本体都在
    /// [`polaris_stats_engine::SubscriptionRegistry`] 里：
    /// - 连接流 = `should_stream_connections()`（aggregate ∪ detail ∪ closed，共用一条上游流）；
    /// - Status 流 = `should_stream(Topic::Stats)`。
    ///
    /// 存函数指针而非 `Topic`：连接流的需求本就不是单个 topic，写成 `Topic` 会逼着把那条并集
    /// 判据搬到门里重写一遍（判据必须只有一处定义）。
    demand: fn(&SubscriptionRegistry) -> bool,
}

impl StreamGate {
    /// 连接长驻流的门（需求 = aggregate ∪ detail ∪ closed）。
    fn connections(state: Arc<StreamGateState>) -> Self {
        Self {
            epoch: state.epoch.subscribe(),
            state,
            demand: SubscriptionRegistry::should_stream_connections,
        }
    }

    /// Status 长驻流的门（需求 = stats topic 自己）。
    fn stats(state: Arc<StreamGateState>) -> Self {
        Self {
            epoch: state.epoch.subscribe(),
            state,
            demand: |r| r.should_stream(Topic::Stats),
        }
    }

    /// 本条流此刻是否该开着（按 [`Self::demand`] 判）。锁毒化 → 保守判否（不做无人消费的 I/O）。
    fn is_open(&self) -> bool {
        self.state
            .registry
            .lock()
            .map(|r| (self.demand)(&r))
            .unwrap_or(false)
    }

    /// 某条 topic 此刻是否该 emit（流开着 ≠ 该流供数的每条 topic 都该推）。
    fn topic_open(&self, topic: Topic) -> bool {
        self.state.should_stream(topic)
    }

    /// 阻塞到「连接流是否该开」等于 `want` 为止。
    ///
    /// 两个方向共用一个实现是刻意的 —— 它们是**同一个判定**的两侧，分成两个函数写迟早长出
    /// 「开的条件」与「关的条件」不互补的缝（隐藏时不断流、或断了之后醒不过来）。
    ///
    /// 唤醒两条腿：
    /// - **门变更**（订阅/退订/可见性翻转 → `epoch` bump）→ 立刻返回；
    /// - **[`PARK_RECHECK_INTERVAL`] 超时**兜底 → 重新回读窗口实况（Tauri 2 无 show/hide 事件，
    ///   收托盘时窗口本就失焦、连 `Focused` 都不发，只靠事件会永久停在这里）。
    ///
    /// **cancel-safe**：状态全在 `self`（`watch::Receiver` 的 `changed()` 本身即 cancel-safe），
    /// 被 `select!` 丢弃只是停止等待，下次调用续上。流循环正是把它当 `select!` 的一条腿用。
    async fn wait_until<V: Fn() -> bool>(&mut self, want: bool, visible: &V) {
        loop {
            // 顺序要紧：先按实况写可见性（自己这次 bump 随即被 borrow_and_update 吃掉），
            // 再记门代次，最后读判定 —— 判定之后发生的任何 bump 都会让 `changed()` 立刻返回。
            self.state.set_window_visible(visible());
            self.epoch.borrow_and_update();
            if self.is_open() == want {
                return;
            }
            match tokio::time::timeout(PARK_RECHECK_INTERVAL, self.epoch.changed()).await {
                Ok(Ok(())) | Err(_) => {}
                // sender 随 StatsRelay 存活于进程全程；Err 只可能出现在收尾 → 退避防忙转。
                Ok(Err(_)) => tokio::time::sleep(PARK_RECHECK_INTERVAL).await,
            }
        }
    }
}

/// 主窗真实可见性回读（对齐 上游 `isUiBroadcastActive` = `mainWindow.isVisible()`）。
///
/// ⚠️ **必须在主线程调用** —— 两个调用点都在 `run_on_main_thread` 投出去的闭包里：
/// [`StreamGateState::spawn_visibility_refresh`]，与 `crate::idle_lightweight` 销毁主窗前的最终复核。
/// 理由见 [`VisibilityCache`]：从别的线程调会阻塞等主循环回包。
///
/// 之所以让轻量巡检也调**这一个**函数而不是自己 `is_visible()` 一遍：显隐判据只能有一处定义，
/// 否则「降流门说不可见、轻量巡检说可见」这类分叉迟早长出来。
///
/// **不是** `WindowEvent::Focused`：失焦但仍在屏上的窗口依然有 UI 消费者，按 focused 降流会让用户
/// 看着的首页拓扑 / 连接明细直接冻住。最小化一并算不可见（笔电最小化后没人看，正是要省电的场景）。
///
/// - `Ok(false)`：主窗不存在（关窗释放内存 / 轻量模式）或已隐藏 / 最小化。主窗不存在时订阅也已由
///   `clear_window` 清空，两条腿一致。
/// - `Err`：平台 getter 报错 —— 由 [`StreamGateState::apply_visibility_probe`] 兜底成「可见」
///   并限频告警（**兜底方向失败安全，但不能静默**，否则降流整体失效且零可观测）。
pub(crate) fn probe_main_window_visible(app: &AppHandle) -> Result<bool, String> {
    let Some(w) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return Ok(false);
    };
    if !w.is_visible().map_err(|e| format!("is_visible: {e}"))? {
        return Ok(false);
    }
    Ok(!w.is_minimized().map_err(|e| format!("is_minimized: {e}"))?)
}

/// 生产用的可见性取值器（喂给 [`StreamGate::wait_until`]）：只读缓存 + 投递一次主线程刷新。
///
/// 写成吃 owned 参数的自由函数（而非 `StreamGate` 的方法）是为了让返回的闭包不借用 `gate` ——
/// relay 里紧接着就要 `gate.wait_until(.., &visible)`（需 `&mut gate`）。
/// 单测在同一个位置注入可翻转 flag 的替身（见测试模块的 `flag_visibility_source`）。
fn visibility_source(state: Arc<StreamGateState>, app: AppHandle) -> impl Fn() -> bool {
    move || state.cached_window_visible(&app)
}

/// 已结束连接的独立有界历史环。
///
/// 活跃表收到 CLOSED 后会立即删行；历史不能塞回那张表，否则拓扑、活动数和关闭动作都会被幽灵记录
/// 污染。这里最多保留 1000 条，按结束时间新到旧排列。`cutoff_ns` 是用户清空时的水位：连接流重订后
/// 首帧会重放 sing-box 自己的历史环，水位确保已清过的旧记录不会重新出现。
#[derive(Debug, Default)]
struct ClosedHistory {
    entries: Vec<ClosedConnectionEntry>,
    cutoff_ns: i64,
}

impl ClosedHistory {
    fn snapshot(&self, at: u64) -> ConnectionsClosedSnapshot {
        ConnectionsClosedSnapshot {
            connections: self.entries.clone(),
            at,
        }
    }

    fn clear(&mut self, cutoff_ns: i64) {
        self.entries.clear();
        self.cutoff_ns = self.cutoff_ns.max(cutoff_ns);
    }

    /// 在活跃聚合器消费本帧前提取关闭记录，这样 CLOSED 缺少完整 connection 时仍能用活动表兜底。
    /// 返回历史内容是否发生变化。
    fn apply_events(&mut self, events: &SingBoxConnectionEvents, active: &StatsAggregator) -> bool {
        let has_closed_event = events.events.iter().any(|event| {
            event.kind == ConnectionEventType::Closed
                || event.closed_at > 0
                || event
                    .connection
                    .as_ref()
                    .is_some_and(|connection| connection.closed_at > 0)
        });
        if !events.reset && !has_closed_event {
            return false;
        }

        let before = self.entries.clone();
        if events.reset {
            self.entries.clear();
        }

        for event in &events.events {
            let payload_closed_at = event
                .connection
                .as_ref()
                .map_or(0, |connection| connection.closed_at);
            let reported_closed_at = event.closed_at.max(payload_closed_at);
            let closed_at = if reported_closed_at > 0 {
                reported_closed_at
            } else if event.kind == ConnectionEventType::Closed {
                now_ns()
            } else {
                0
            };
            let is_closed = event.kind == ConnectionEventType::Closed || closed_at > 0;
            if !is_closed || closed_at <= self.cutoff_ns {
                continue;
            }

            let entry = event
                .connection
                .as_ref()
                .filter(|connection| !is_probe_pool_inbound_tag(&connection.inbound))
                .map(trim_connection)
                .or_else(|| active.entry(&event.id).cloned());
            let Some(entry) = entry else {
                continue;
            };
            if entry.id.is_empty() {
                continue;
            }

            self.entries.retain(|old| old.entry.id != entry.id);
            self.entries
                .push(ClosedConnectionEntry { entry, closed_at });
        }

        self.entries
            .sort_by_key(|entry| std::cmp::Reverse(entry.closed_at));
        self.entries.truncate(MAX_CLOSED_HISTORY);
        self.entries != before
    }
}

/// stats 运行时（`State`-managed，单实例）。
pub struct StatsRelay {
    /// 降流门态（订阅注册表 + 门变更信号）；与两条 relay `Arc` 共享。
    gate: Arc<StreamGateState>,
    /// 每窗口订阅记账（key = window label + topic，value = Subscription）。
    /// Polaris 按 webContents.sender 记账；Tauri 按 webview label 记账（窗口关闭时清理）。
    subs: Mutex<Vec<(String, Topic, SubscriptionToken)>>,
    /// 连接长驻流 relay（`Some` = 在跑）。**aggregate / detail / closed 共用这一条**
    /// （三者来自同一事件流，见 [`run_connections_stream`]）——
    /// 此前是两个各自轮询的独立槽位。
    connections: Mutex<Option<AggregatePoller>>,
    /// 已结束连接独立历史环；命令清空与连接流写入共享。
    closed_history: Arc<Mutex<ClosedHistory>>,
    /// stats topic（上下行速率 + 累计 + 连接数）的 `SubscribeStatus` 长驻流 relay（`Some` = 在跑）。
    stats_poller: Mutex<Option<AggregatePoller>>,
}

impl Default for StatsRelay {
    fn default() -> Self {
        Self::new()
    }
}

impl StatsRelay {
    pub fn new() -> Self {
        Self {
            gate: Arc::new(StreamGateState::new()),
            subs: Mutex::new(Vec::new()),
            connections: Mutex::new(None),
            closed_history: Arc::new(Mutex::new(ClosedHistory::default())),
            stats_poller: Mutex::new(None),
        }
    }

    /// 订阅某 topic（上游 `stats:subscribe`）。非法 topic 静默忽略（不抛，避免 promise reject 噪音）。
    ///
    /// **非主窗的订阅一律拒绝 + 告警**（见 [`accepts_stats_subscription`]）。
    ///
    /// 订阅任一 topic → 起对应的后台 relay（单例幂等）。
    pub fn subscribe(
        &self,
        app: &AppHandle,
        proxy: Arc<ProxyRuntime>,
        config: Arc<ConfigManager>,
        window_label: &str,
        topic_str: &str,
    ) {
        let Some(topic) = parse_topic(topic_str) else {
            return;
        };
        if !accepts_stats_subscription(window_label) {
            log::warn!(
                "拒绝来自非主窗（label={window_label}）的 stats 订阅（topic={topic_str}）：\
                 降流门的可见性只看主窗，该窗的订阅会在主窗隐藏时被整体 park 掉 = 永远收不到帧。\
                 要给非主窗供数，须先把可见性判据从「主窗可见」改成「任一订阅窗可见」"
            );
            return;
        }
        {
            let mut reg = match self.gate.registry.lock() {
                Ok(g) => g,
                Err(e) => {
                    log::warn!("stats registry lock: {e}");
                    return;
                }
            };
            // subscriber_id 用窗口 label（唯一标识一个 webview）。
            let token = reg.subscribe(topic, window_label.to_string());
            if let Ok(mut subs) = self.subs.lock() {
                subs.push((window_label.to_string(), topic, token));
            }
        }
        // 订阅集变了 → 唤醒该 topic 已在跑但正断流待命的 relay（无订阅时停在门上的那条腿）。
        self.gate.bump();
        // 数据面 relay（订阅即起，内部按核起停自适应）：
        // - aggregate（拓扑）、detail（活动）、closed（已结束）→ **同一条**连接长驻流，
        //   见 [`run_connections_stream`]）；
        // - stats → `SubscribeStatus` 长驻流（EVENT_STATS_UPDATED，见 [`run_stats_stream`]）。
        // 全部 topic 必须覆盖：漏一条即对应视图永不收帧。
        match topic {
            Topic::Connections | Topic::Detail | Topic::Closed => {
                self.ensure_connections_stream(app, proxy, config)
            }
            Topic::Stats => self.ensure_stats_stream(app, proxy, config),
        }
        if topic == Topic::Closed {
            broadcast(
                app,
                EVENT_CONNECTIONS_CLOSED,
                self.closed_snapshot(now_ms()),
            );
        }
    }

    /// 退订某 topic（上游 `stats:unsubscribe`）。无匹配为 no-op。
    /// 该 topic 的订阅者归零 → 停对应的后台 relay。
    pub fn unsubscribe(&self, window_label: &str, topic_str: &str) {
        let Some(topic) = parse_topic(topic_str) else {
            return;
        };
        let token = {
            let mut subs = match self.subs.lock() {
                Ok(g) => g,
                Err(e) => {
                    log::warn!("stats subs lock: {e}");
                    return;
                }
            };
            let pos = subs
                .iter()
                .position(|(label, t, _)| label == window_label && *t == topic);
            pos.map(|i| subs.remove(i).2)
        };
        if let Some(token) = token {
            if let Ok(mut reg) = self.gate.registry.lock() {
                reg.unsubscribe(topic, token);
            }
            self.gate.bump(); // 订阅集变了 → 门重判（下一拍即降流，不空转）
        }
        // 连接流由三个 topic 共用：**三个都归零**才停。
        if matches!(topic, Topic::Connections | Topic::Detail | Topic::Closed)
            && self.connections_subscriber_count() == 0
        {
            self.stop_connections_stream();
        }
        if topic == Topic::Stats && self.stats_subscriber_count() == 0 {
            self.stop_stats_stream();
        }
    }

    /// 窗口关闭：清该窗口全部订阅（Polaris registry 兜底防泄漏）+ aggregate 归零则停 relay。
    pub fn clear_window(&self, window_label: &str) {
        let removed = {
            let mut subs = match self.subs.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let (keep, drop) = subs
                .iter()
                .cloned()
                .partition(|(label, _, _)| label != window_label);
            *subs = keep;
            drop
        };
        if let Ok(mut reg) = self.gate.registry.lock() {
            for (_, topic, token) in removed {
                reg.unsubscribe(topic, token);
            }
        }
        self.gate.bump();
        if self.connections_subscriber_count() == 0 {
            self.stop_connections_stream();
        }
        if self.stats_subscriber_count() == 0 {
            self.stop_stats_stream();
        }
    }

    /// 按窗口实况刷新可见性 → 降流门（Polaris stats-worker 据此门控 connectionsStreamOn）。
    ///
    /// 由 `main.rs` 的三个显隐写入点调（`WindowEvent::Focused` / 收托盘 `hide()` 后 / 单实例唤起
    /// `show()` 后）—— `Focused` 那处**不取 focused 的值**（失焦 ≠ 隐藏），只把它当「显隐可能刚变」
    /// 的即时触发器，真值一律经 [`probe_main_window_visible`] 回读窗口实况；变了即 bump 门代次 →
    /// 等在门上的 relay 立刻醒（恢复不等兜底周期）。
    ///
    /// 回读经 [`StreamGateState::spawn_visibility_refresh`] 投给主线程执行 —— 本方法在主线程被调用时
    /// 该闭包内联跑完，等价于同步回读；从别的线程调也不会阻塞（见 [`VisibilityCache`]）。
    /// 托盘那条显隐路径（`tray.rs` 的 `hide()` / `show()`）没有写入点，靠 relay 的兜底刷新覆盖。
    pub fn refresh_window_visible(&self, app: &AppHandle) {
        self.gate.spawn_visibility_refresh(app);
    }

    /// 主窗可见性缓存的**只读**取值（非阻塞：一次原子 load + 顺带投递一次主线程刷新）。
    ///
    /// 降流门之外的第二个消费方：C16 自动轻量模式的后端闲置巡检（`crate::idle_lightweight`）。
    /// 让它读**这一份**缓存而不是自己回读窗口，一是不必再摊一份「非主线程回读会阻塞」的风险，
    /// 二是两处显隐真值恒一致。代价是最多落后一拍（调用方各自的巡检周期）—— 轻量巡检据此在真正
    /// 销毁前还会在主线程上做一次新鲜复核，见其 `enter_lightweight_if_still_hidden`。
    #[must_use]
    pub fn window_visible(&self, app: &AppHandle) -> bool {
        self.gate.cached_window_visible(app)
    }

    /// 清空已结束历史并设置重放水位，返回应立即广播给当前页面的空快照。
    pub fn clear_closed_history(&self) -> ConnectionsClosedSnapshot {
        match self.closed_history.lock() {
            Ok(mut history) => {
                history.clear(now_ns());
                history.snapshot(now_ms())
            }
            Err(error) => {
                log::warn!("已结束连接历史 lock: {error}");
                ConnectionsClosedSnapshot {
                    connections: Vec::new(),
                    at: now_ms(),
                }
            }
        }
    }

    fn closed_snapshot(&self, at: u64) -> ConnectionsClosedSnapshot {
        self.closed_history
            .lock()
            .map(|history| history.snapshot(at))
            .unwrap_or(ConnectionsClosedSnapshot {
                connections: Vec::new(),
                at,
            })
    }

    /// 连接流的活跃订阅者数 = **三个投影之和**（aggregate + detail + closed）。
    ///
    /// 求和而非取 max/任一：`== 0` 恰好表达三个投影都没人消费。
    fn connections_subscriber_count(&self) -> usize {
        self.gate
            .registry
            .lock()
            .map(|r| {
                r.subscriber_count(Topic::Connections)
                    + r.subscriber_count(Topic::Detail)
                    + r.subscriber_count(Topic::Closed)
            })
            .unwrap_or(0)
    }

    /// 确保连接长驻流 relay 在跑（单例 + TOCTOU 闸门，见 [`should_spawn_poller`]）。
    fn ensure_connections_stream(
        &self,
        app: &AppHandle,
        proxy: Arc<ProxyRuntime>,
        config: Arc<ConfigManager>,
    ) {
        let mut slot = match self.connections.lock() {
            Ok(g) => g,
            Err(e) => {
                log::warn!("连接流 slot lock: {e}");
                return;
            }
        };
        if !should_spawn_poller(slot.is_some(), self.connections_subscriber_count()) {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let handle = tauri::async_runtime::spawn(run_connections_stream(
            app.clone(),
            proxy,
            config,
            stop.clone(),
            StreamGate::connections(self.gate.clone()),
            self.closed_history.clone(),
        ));
        *slot = Some(AggregatePoller { stop, handle });
        log::debug!("连接流 relay 已启动");
    }

    /// 停连接流 relay（set stop + abort；无则 no-op）。
    ///
    /// TOCTOU 闸门：**slot 锁下**重校订阅计数（与 [`Self::ensure_connections_stream`] 互斥）。订阅计数
    /// （registry mutex）与 relay slot（本 mutex）是两把锁，非原子。若最后一个 unsubscribe 读到 count==0
    /// 后、并发 subscribe 又重新计数并见 slot=Some（依赖现有 relay 不重建），此处若无条件 stop 会把仍有
    /// 活订阅的 relay 停掉 → 留活订阅无 relay（拓扑/明细冻结到下次 sub/unsub，liveness gap）。故取 slot 后、
    /// abort 前，在锁内复查计数：仍有订阅则不停。
    fn stop_connections_stream(&self) {
        let mut slot = match self.connections.lock() {
            Ok(g) => g,
            Err(e) => {
                log::warn!("连接流 slot lock: {e}");
                return;
            }
        };
        if self.connections_subscriber_count() != 0 {
            return; // 并发 subscribe 已重新计数并依赖此 relay → 绝不停。
        }
        if let Some(p) = slot.take() {
            p.stop.store(true, Ordering::Relaxed);
            p.handle.abort();
            log::debug!("连接流 relay 已停止");
        }
    }

    /// 当前 stats（[`Topic::Stats`]）活跃订阅者数。
    fn stats_subscriber_count(&self) -> usize {
        self.gate
            .registry
            .lock()
            .map(|r| r.subscriber_count(Topic::Stats))
            .unwrap_or(0)
    }

    /// 确保 stats（Status 流）relay 在跑（单例 + TOCTOU 闸门，见 [`should_spawn_poller`]）。
    fn ensure_stats_stream(
        &self,
        app: &AppHandle,
        proxy: Arc<ProxyRuntime>,
        config: Arc<ConfigManager>,
    ) {
        let mut slot = match self.stats_poller.lock() {
            Ok(g) => g,
            Err(e) => {
                log::warn!("stats relay slot lock: {e}");
                return;
            }
        };
        if !should_spawn_poller(slot.is_some(), self.stats_subscriber_count()) {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let handle = tauri::async_runtime::spawn(run_stats_stream(
            app.clone(),
            proxy,
            config,
            stop.clone(),
            StreamGate::stats(self.gate.clone()),
        ));
        *slot = Some(AggregatePoller { stop, handle });
        log::debug!("stats relay 已启动");
    }

    /// 停 stats relay（set stop + abort；无则 no-op）。
    ///
    /// TOCTOU 闸门：**slot 锁下**重校订阅计数（与 [`Self::ensure_stats_stream`] 互斥）——同连接流的
    /// liveness gap（订阅计数与 relay slot 两把锁非原子）。取 slot 后、abort 前锁内复查：仍有订阅则不停。
    fn stop_stats_stream(&self) {
        let mut slot = match self.stats_poller.lock() {
            Ok(g) => g,
            Err(e) => {
                log::warn!("stats relay slot lock: {e}");
                return;
            }
        };
        if self.stats_subscriber_count() != 0 {
            return; // 并发 subscribe 已重新计数并依赖此 relay → 绝不停。
        }
        if let Some(p) = slot.take() {
            p.stop.store(true, Ordering::Relaxed);
            p.handle.abort();
            log::debug!("stats relay 已停止");
        }
    }
}

/// relay spawn 决策（两条流的 `ensure_*` 共用；**须在 slot 锁下**求值）。
///
/// 两个否决条件：
/// - `slot_occupied`：已在跑 → 幂等 no-op（单例）。
/// - `subscriber_count == 0`：**TOCTOU 闸门**，与 `stop_*_poller` 的锁内复查对称。registry 插入与
///   ensure 分属两把锁、中间无守卫，故存在：T1 subscribe 插入（count=1）→ T2 unsubscribe 跑完
///   （count=0；此刻 slot 仍 None → stop 是 no-op）→ T1 才 ensure → 起一条**零订阅者的 relay**。
///   此后无人再触发 stop（退订路径已走完）→ 上游流永久开着 + 无人消费的 emit。
///   前端实况触发器：`ConnectionsScreen` 的订阅 effect 依赖 `[paused]`，暂停切换即快速退订+重订；
///   React StrictMode 的双挂载同理。
fn should_spawn_poller(slot_occupied: bool, subscriber_count: usize) -> bool {
    !slot_occupied && subscriber_count > 0
}

/// 纯判定：该 window label 的 stats 订阅能否被接受。
///
/// **只有主窗**（[`MAIN_WINDOW_LABEL`]）可订阅。降流门的可见性判据只看主窗
/// （[`probe_main_window_visible`]），故任何非主窗的订阅都会在主窗隐藏时被整体 park 掉 ——
/// 「注册了但永远收不到帧」，而且是**静默**的（订阅计数正常、relay 也在跑，只是门永远关着）。
///
/// 当前托盘浮层（独立 label 的 `tray.html`）不订阅任何 topic，故这条闸今天是空跑。
/// 它存在是为了让**将来**给非主窗接订阅的人立刻撞墙并看到日志，而不是上线后表现为
/// 「浮层数据时有时无」——那种缺陷要从可见性门一路倒推回来才找得到。
///
/// **拒绝而不是「接受 + 告警」**：接受等于把一条结构性饿死的订阅登记进注册表，
/// 表现为间歇性缺数据（主窗可见时又好了），比彻底不出数难查得多。
fn accepts_stats_subscription(label: &str) -> bool {
    label == MAIN_WINDOW_LABEL
}

/// topic 字面量校验：只接受 stats | aggregate | detail | closed。
fn parse_topic(s: &str) -> Option<Topic> {
    match s {
        "stats" => Some(Topic::Stats),
        "aggregate" => Some(Topic::Connections),
        "detail" => Some(Topic::Detail),
        "closed" => Some(Topic::Closed),
        _ => None,
    }
}

/// gRPC `daemon::Connection` → stats-engine `ConnectionEntry`（复用 [`trim_connection`] 的裁剪，
/// 不另写一份 host/IP 拆分逻辑）。
fn daemon_conn_to_entry(c: &daemon::Connection) -> ConnectionEntry {
    trim_connection(&daemon_conn_to_engine(c))
}

/// prost `daemon::Connection` → 纯逻辑层 [`SingBoxConnection`]。
///
/// 从 [`daemon_conn_to_entry`] 里拆出来的**同一段**映射：长驻流要把整条连接喂进
/// [`StatsAggregator`]（它按 id 维护连接表、按 delta 累加字节），不能像轮询那样拿到就 trim ——
/// trim 是有损的（丢 `closed_at` / 只留展示字段），trim 完就没法再判幽灵、也没法累加。
///
/// **刻意不加字段**：这里映射哪些字段决定了 aggregate / detail 的输出，与轮询时代必须逐字一致，
/// 否则「换了数据来源」会顺手变成「换了显示内容」。
///
/// `inbound`（入站 **tag**，非 `inbound_type`）是该规矩下唯一的例外，且不破它：[`trim_connection`]
/// 不读这个字段 ⇒ aggregate / detail 的输出一字不变。它只喂 [`StatsAggregator`] 的准入判据——
/// 主核测速探测池 `probe-in-{k}` 的连接是应用自己的流量，不进连接表（见 aggregator NEW 分支）。
/// 此前它一直落在 `..Default::default()` 里恒为空串，探测连接因而无从识别。
fn daemon_conn_to_engine(c: &daemon::Connection) -> SingBoxConnection {
    let process_path = c
        .process_info
        .as_ref()
        .map(|p| p.process_path.clone())
        .unwrap_or_default();
    SingBoxConnection {
        id: c.id.clone(),
        inbound: c.inbound.clone(),
        inbound_type: c.inbound_type.clone(),
        network: c.network.clone(),
        source: c.source.clone(),
        destination: c.destination.clone(),
        domain: c.domain.clone(),
        created_at: c.created_at,
        closed_at: c.closed_at,
        uplink_total: c.uplink_total,
        downlink_total: c.downlink_total,
        rule: c.rule.clone(),
        chain_list: c.chain_list.clone(),
        process_info: SingBoxProcessInfo {
            process_path,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// prost `daemon::ConnectionEvents` 帧 → 纯逻辑层 [`SingBoxConnectionEvents`]。
///
/// `type` 是 proto enum（`NEW=0 / UPDATE=1 / CLOSED=2`），prost 生成成 `i32`。
/// **未知值兜底成 `New`**：proto3 的开放枚举语义 —— 新核加了事件类型而旧客户端不认时，
/// 当 NEW 处理最多是多一条连接（还会被 `closed_at` 幽灵过滤兜一道），当 CLOSED 处理则会
/// **误删一条活连接**。兜底方向选不伤表的那侧。
fn daemon_events_to_engine(ev: &daemon::ConnectionEvents) -> SingBoxConnectionEvents {
    SingBoxConnectionEvents {
        reset: ev.reset,
        events: ev
            .events
            .iter()
            .map(|e| SingBoxConnectionEvent {
                kind: match e.r#type {
                    x if x == daemon::ConnectionEventType::Update as i32 => {
                        ConnectionEventType::Update
                    }
                    x if x == daemon::ConnectionEventType::Closed as i32 => {
                        ConnectionEventType::Closed
                    }
                    _ => ConnectionEventType::New,
                },
                id: e.id.clone(),
                connection: e.connection.as_ref().map(daemon_conn_to_engine),
                uplink_delta: e.uplink_delta,
                downlink_delta: e.downlink_delta,
                closed_at: e.closed_at,
            })
            .collect(),
    }
}

/// 连接快照 → 拓扑聚合（首帧全量含历史环死连接，按 `closed_at>0` 过滤）。
///
/// relay 的纯数据面核心（无 gRPC / 无 emit），单测直接喂 fixture。
fn build_aggregate(conns: &[daemon::Connection], at: u64) -> ConnectionsAggregate {
    let entries: Vec<ConnectionEntry> = conns
        .iter()
        .filter(|c| c.closed_at <= 0) // 丢弃历史环死连接（快照含之）
        .map(daemon_conn_to_entry)
        .collect();
    aggregate_connections_with_topn(&entries, at, TOPOLOGY_TOP_N)
}

/// 连接快照 → 明细快照（detail topic 载荷）。
///
/// 与 [`build_aggregate`] 同源同裁剪（`daemon_conn_to_entry` → [`trim_connection`]），只是不做 Top-N
/// 聚合、逐条下发。死连接（`closed_at>0`，内核历史环）同样过滤——明细页只展示活跃连接。
///
/// relay 的纯数据面核心（无 gRPC / 无 emit），单测直接喂 fixture。
fn build_detail(conns: &[daemon::Connection], at: u64) -> ConnectionsSnapshot {
    ConnectionsSnapshot {
        connections: conns
            .iter()
            .filter(|c| c.closed_at <= 0)
            .map(daemon_conn_to_entry)
            .collect(),
        at,
    }
}

/// change-driven 去重：聚合内容签名相较上帧变了才返回 `Some(new_sig)`（应 emit）；同签名返回 `None`（去重）。
///
/// issue #227 的核心：载荷与连接总数解耦——连接风暴（大量 UPDATE）但拓扑内容不变时**不推**，
/// 只在 host/outbound 计数或成员真变时推一帧。
fn signature_changed(agg: &ConnectionsAggregate, last: &Option<String>) -> Option<String> {
    let sig = aggregate_signature(agg);
    if last.as_deref() == Some(sig.as_str()) {
        None
    } else {
        Some(sig)
    }
}

/// 核未运行时的 aggregate offline 帧：空聚合经**正常签名去重**推一帧（`emit` 由调用方注入）。
///
/// 返回新签名（推了）/ `None`（去重，本轮不推）。
///
/// 此前 offline 分支只复位签名、**不推帧** → 前端 aggregate state 永远停在停核前的旧值（首页拓扑继续
/// 显示「连接: N」+ 旧 host 列表），而明细页的 offline 空帧已如实归零 → 两页互相矛盾。语义与
/// [`run_connections_stream`] 的离线空帧对齐。
///
/// 走既有签名去重而非另加 flag：空聚合的签名**本身**即「核已停」的基准 —— 进入停核态时签名由旧内容
/// 变空 → 推一帧（天然边沿触发，核停着不逐秒重推）；核回来后首帧内容非空 → 签名再变 → 必推。
fn offline_aggregate_frame(
    last_sig: &Option<String>,
    at: u64,
    emit: impl FnOnce(ConnectionsAggregate),
) -> Option<String> {
    let agg = build_aggregate(&[], at);
    let sig = signature_changed(&agg, last_sig)?;
    emit(agg);
    Some(sig)
}

/// 核未运行时的 stats 清零帧（速率 / 累计 / 连接数全 0）。
///
/// **刻意是个常量帧、不经任何差分状态求值**：清零帧若从聚合器里取，就得先把「停核期的 0」写进
/// 速率基线，核回来后的首帧便会拿它做差分 → 把核重启后的全部历史累计字节一次性算成瞬时速率
/// （天文数字尖峰）。调用方另行 `reset()` 聚合器丢基线，两件事各归各位。
fn offline_stats_frame() -> TrafficStats {
    TrafficStats::zeroed()
}

/// prost `daemon::Status` 帧 → 纯逻辑层 [`SingBoxStatus`]。
///
/// 与 [`daemon_conn_to_engine`] 同型的一段纯映射。字段逐条搬，**不在这里做任何口径加工**
/// （速率推导、可用性判断都在各自该在的层）。
fn daemon_status_to_engine(s: &daemon::Status) -> SingBoxStatus {
    SingBoxStatus {
        memory: s.memory,
        goroutines: s.goroutines,
        connections_in: s.connections_in,
        connections_out: s.connections_out,
        traffic_available: s.traffic_available,
        uplink: s.uplink,
        downlink: s.downlink,
        uplink_total: s.uplink_total,
        downlink_total: s.downlink_total,
    }
}

/// 纯判定：`trafficAvailable` 的当前值是否**相对上一次**变了（`None` = 还没见过任何一帧）。
///
/// # 为什么这件事必须有可观测信号
///
/// `SubscribeStatus` 对 `trafficManager == nil` **不做任何前置校验、不返错**：`readStatus()`
/// 只是跳过那三行赋值，于是流照常每秒推帧，`uplinkTotal` / `downlinkTotal` / `connectionsIn`
/// 安静地全是 0。UI 上的表现是「速率恒 0 B/s、累计恒 0、连接数恒 0，且没有任何错误」——
/// 与「用户真的没在传数据」逐像素一致，无从区分，也无从排查。
///
/// 故必须显式判 `trafficAvailable` 并把它喊出来。判据取**变化沿**而非每帧：值一旦稳定下来
/// （生产里它恒为 true——daemon gRPC 走 `needAPIService`，`trafficManager` 必被构造，见 `box.go:245`），
/// 每帧一条日志就是每秒一条噪音，反而把真信号淹掉；而每次建流后基线复位成 `None`，
/// 故每条新流的第一帧必报一次。
fn traffic_availability_changed(prev: Option<bool>, now: bool) -> bool {
    prev != Some(now)
}

/// 当前 epoch 毫秒（聚合 `at` 采样时刻；签名比对时被剔除，故不影响去重）。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 当前 epoch 纳秒。只用作「清空已结束历史」的重放水位及缺失 closedAt 的保守回落。
fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// currentConfig.clashApiSecret（对齐 proxy.rs `management_api()` 的读法）。
fn read_clash_secret(config: &ConfigManager) -> String {
    config
        .current()
        .ok()
        .and_then(|c| {
            c.get("clashApiSecret")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// 连接长驻流 relay：**一条** `SubscribeConnections` 流同时喂 aggregate（拓扑）与 detail（明细）。
///
/// # 为什么是一条流、两条 emit
///
/// 拓扑与明细从来不是两份数据，是同一张连接表的两种投影（`StatsAggregator::aggregate` /
/// `connections_snapshot`）。轮询时代它们各起一条 poller、各拉一次全量表，是纯粹的重复劳动 ——
/// 而且两次拉取时刻不同，还能给出互相矛盾的两帧（拓扑说 12 条、明细列 13 条）。
/// 一条流 + 一张表 + 两条各自节流的 emit，既省一半上游成本，又让两个页面**恒定自洽**。
///
/// # 相对轮询变了什么
///
/// | | 轮询（旧） | 长驻流（本函数） |
/// |---|---|---|
/// | 延迟 | ≤一拍 + RTT（平均白等半拍） | 事件发生即 ≈RTT |
/// | 上游 | 每秒 4 次(agg) + 1 次(detail) 全量表 | 每次订阅一帧全量，此后只有增量 |
/// | 死连接 | 每拍重新下发 ≤1000 条再由我们过滤 | 只在 reset 帧出现一次 |
/// | 降流 | park 一拍（不拉取） | **drop 流**（见 [`StreamGate`]） |
///
/// # 生命周期（每一轮外循环 = 一条流的一生）
///
/// 1. [`StreamGate::wait_until`] 等门开（无订阅者 / 主窗不可见 → 断流待命，不碰 gRPC）。
/// 2. 核未运行 → 推一帧离线态（拓扑空聚合 + 明细空快照）让两个页面如实归零，等核回来。
/// 3. 建 h2c 客户端 + 订阅流；**连接表与两条 emit 闸门一并复位** —— 新流的首帧是 `reset=true`
///    全量表，旧表在此刻已作废（断流期间断掉的连接不会补发 CLOSED，只有 reset 能清掉它们）。
/// 4. 内循环消费帧，直到门关 / 核停 / 换端口 → 跳出，drop 流，回到 1。
///
/// # 为什么内循环还有一个 1s 的兜底唤醒
///
/// [`ReconnectingStream`] 的语义是**永不向消费方 yield 错误或 None**（断了自己重连），
/// 于是「核停了」「核换端口重启了」这两件事**流本身不会告诉我们** —— 不兜底的话，核换口重启后
/// 这条流会永远重连到旧端口，两个页面静默冻结。故内循环每 [`PARK_RECHECK_INTERVAL`] 至少醒一次
/// 复核 `proxy.status()`。代价是一次进程内 mutex 读，与它替掉的每秒 5 次全量 gRPC 拉取不在一个量级。
async fn run_connections_stream(
    app: AppHandle,
    proxy: Arc<ProxyRuntime>,
    config: Arc<ConfigManager>,
    stop: Arc<AtomicBool>,
    mut gate: StreamGate,
    closed_history: Arc<Mutex<ClosedHistory>>,
) {
    let visible = visibility_source(gate.state.clone(), app.clone());
    // 节流用**单调**时钟，不是 `now_ms()`（墙钟）：NTP 校时会让墙钟跳变，往前跳一小时 =
    // 一次无节制 emit，往后跳 = emit 被饿死一小时。墙钟只用来填帧里的 `at` 字段（那是给渲染端看的时刻）。
    let clock = Instant::now();
    let mut table = StatsAggregator::new();
    let mut agg_emit = EmitGate::new(AGGREGATE_EMIT_MIN_INTERVAL);
    let mut detail_emit = EmitGate::new(DETAIL_EMIT_MIN_INTERVAL);
    let mut closed_emit = EmitGate::new(CLOSED_EMIT_MIN_INTERVAL);
    let mut last_sig: Option<String> = None;
    let mut offline_sent = false;

    while !stop.load(Ordering::Relaxed) {
        // ① 降流门：关着就在这里断流待命。
        gate.wait_until(true, &visible).await;

        // ② 核未运行 → 不碰 gRPC，推一帧离线态（只在进入该态时推一次；核停着重复推相同空帧
        //    只会让渲染端白重渲）。
        let status = proxy.status();
        if !status.running || status.clash_api_port == 0 {
            if !offline_sent {
                if let Some(sig) = offline_aggregate_frame(&last_sig, now_ms(), |agg| {
                    broadcast(&app, EVENT_CONNECTIONS_AGGREGATE, agg);
                }) {
                    last_sig = Some(sig);
                }
                broadcast(&app, EVENT_CONNECTIONS_DETAIL, build_detail(&[], now_ms()));
                offline_sent = true;
            }
            tokio::time::sleep(PARK_RECHECK_INTERVAL).await;
            continue;
        }

        // ③ 建流。
        let port = status.clash_api_port;
        let secret = read_clash_secret(&config);
        let client = match SingBoxApiClient::connect(Endpoint::new("127.0.0.1", port), secret).await
        {
            Ok(c) => c,
            Err(e) => {
                log::debug!("连接流：管理 API 连接失败 {e}");
                tokio::time::sleep(PARK_RECHECK_INTERVAL).await;
                continue;
            }
        };
        let mut stream = client
            .subscribe_connections(CONNECTIONS_STREAM_INTERVAL_NS, ReconnectConfig::default());
        // 新流 = 新的一份真相：旧连接表在此刻作废，等首帧 reset 重建。
        table.reset();
        agg_emit.reset();
        detail_emit.reset();
        closed_emit.reset();
        offline_sent = false;
        log::debug!("连接流已订阅（port={port}）");

        // ④ 流循环。
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            // 下次该醒的时刻：两条 emit 的到期时间与核状态复核周期取最小。
            // 两条都无待推变更（空闲）→ 只剩兜底复核，不设无谓定时器。
            let now = mono_ms(clock);
            let due = [
                agg_emit.wait_for(now),
                detail_emit.wait_for(now),
                closed_emit.wait_for(now),
            ]
            .into_iter()
            .flatten()
            .min()
            .map_or(PARK_RECHECK_INTERVAL, |d| d.min(PARK_RECHECK_INTERVAL));

            tokio::select! {
                frame = stream.recv() => match frame {
                    Some(ev) => {
                        let events = daemon_events_to_engine(&ev);
                        let closed_changed = closed_history
                            .lock()
                            .map(|mut history| history.apply_events(&events, &table))
                            .unwrap_or(false);
                        table.on_connection_events(&events, 0);
                        agg_emit.note_change();
                        detail_emit.note_change();
                        if closed_changed {
                            closed_emit.note_change();
                        }
                    }
                    // ReconnectingStream 正常语义下不返 None；真返了说明它内部终止 → 重建。
                    None => break,
                },
                // 门关（退订 / 主窗隐藏）→ 跳出即 drop 流，整条链路成本归零。
                () = gate.wait_until(false, &visible) => break,
                () = tokio::time::sleep(due) => {}
            }

            // emit：两条投影各按自己的闸门与订阅状态。
            let now = mono_ms(clock);
            if agg_emit.should_emit(now) {
                // 该 topic 没订阅者时**照样 mark**：不消费掉这次待推标志的话，`wait_for` 会恒返回
                // ZERO，select 的定时器分支退化成 0 延迟 → 忙转烧一个 tokio worker。
                if gate.topic_open(Topic::Connections) {
                    let agg = table.aggregate(now_ms());
                    // 签名去重（issue #227）：拓扑载荷是 host/出口计数，连接风暴下内容常不变。
                    // 闸门挡的是频率，去重挡的是「频率之内但内容没变」的那些帧，两者不重叠。
                    if let Some(sig) = signature_changed(&agg, &last_sig) {
                        broadcast(&app, EVENT_CONNECTIONS_AGGREGATE, agg);
                        last_sig = Some(sig);
                    }
                }
                agg_emit.mark_emitted(now);
            }
            if detail_emit.should_emit(now) {
                if gate.topic_open(Topic::Detail) {
                    // detail **不做**签名去重：载荷含每条连接的累计字节，只要有流量就逐帧都变，
                    // 去重恒不命中；且渲染端正是靠相邻两帧的 (at, bytes) 差分算每条连接的实时速率，
                    // 去重掉「内容相同」的帧会让静默连接的速率停在旧值而非归零。
                    broadcast(
                        &app,
                        EVENT_CONNECTIONS_DETAIL,
                        table.connections_snapshot(now_ms()),
                    );
                }
                detail_emit.mark_emitted(now);
            }
            if closed_emit.should_emit(now) {
                if gate.topic_open(Topic::Closed) {
                    if let Ok(history) = closed_history.lock() {
                        broadcast(&app, EVENT_CONNECTIONS_CLOSED, history.snapshot(now_ms()));
                    }
                }
                closed_emit.mark_emitted(now);
            }

            // 核停 / 换端口（换核、重启动态口）→ 断流重来。ReconnectingStream 自己发现不了这两件事。
            let st = proxy.status();
            if !st.running || st.clash_api_port != port {
                break;
            }
        }
        log::debug!("连接流已断开（待重订阅）");
    }
    log::debug!("连接流 relay 已退出");
}

/// 单调毫秒（emit 闸门的时基）。见 [`run_connections_stream`] 里 `clock` 的说明。
fn mono_ms(origin: Instant) -> u64 {
    origin.elapsed().as_millis() as u64
}

/// stats relay：一条 `SubscribeStatus` 长驻流 → [`StatsAggregator::on_status`] → emit
/// `EVENT_STATS_UPDATED`（StatusBar 的上下行速率 + 累计 + 连接数）。
///
/// # 相对 1s 轮询变了什么
///
/// | | 轮询（旧） | 长驻流（本函数） |
/// |---|---|---|
/// | 上游 | 每秒一次 `first_connection_snapshot`（活连接 + ≤1000 条死连接的全量 protobuf） | 一帧 9 个标量 |
/// | 累计口径 | 对**含死连接**的整表 `uplink_total` 求和 → 死连接被历史环淘汰时**下跌** | `Manager.Total()`，两个只增的 `atomic.Int64`，**结构上不回退** |
/// | 速率 | 上述会下跌的和做跨拍差分 → 连接高频起落时被 `saturating_sub` 系统性钳低 | 单调累计做差分 ÷ 实测 Δt |
/// | 活跃连接数 | 快照里 `closed_at <= 0` 的条数 | `Status.connectionsIn`（= `trafficManager.ConnectionsLen()`，同一口径） |
/// | 降流 | park 一拍（不拉取） | **drop 流**（见 [`StreamGate`]） |
///
/// 换流的判据是**口径**不是性能：旧法的累计会倒退（历史环满 1000 条后每淘汰一条就跌一截），
/// 那不是接线问题、修不掉。上游成本下降只是顺带。
///
/// # 生命周期（每一轮外循环 = 一条流的一生）
///
/// 1. [`StreamGate::wait_until`] 等门开（无 stats 订阅者 / 主窗不可见 → 断流待命，不碰 gRPC）。
/// 2. 核未运行 → 推一帧清零态让 StatusBar 如实归零（只在进入该态时推一次），等核回来。
/// 3. 建 h2c 客户端 + 订阅流；**聚合器复位** —— 速率基线在此刻必须作废（断流 / 停核 / 换核跨越的
///    时长不定，沿用旧基线会把整段空档的平均吞吐当成「此刻的速率」显示一帧）。
/// 4. 内循环消费帧，直到门关 / 核停 / 换端口 → 跳出，drop 流，回到 1。
///
/// 内循环那个 [`PARK_RECHECK_INTERVAL`] 兜底唤醒的理由与 [`run_connections_stream`] 逐字相同：
/// `ReconnectingStream` 断了自己重连、永不 yield 错误，故「核停了」「核换端口重启了」这两件事
/// 流本身不会告诉我们。
///
/// # 首帧不必等
///
/// 内核在建 ticker **之前**就无条件 `Send` 一帧当前状态（`daemon/started_service.go:396`），
/// 故订阅即出首帧 —— 轮询时代靠「首拍不睡」换来的那条语义，在流下是白送的。
/// 该帧的速率必然是 0（无基线），累计与连接数即刻真实。
async fn run_stats_stream(
    app: AppHandle,
    proxy: Arc<ProxyRuntime>,
    config: Arc<ConfigManager>,
    stop: Arc<AtomicBool>,
    mut gate: StreamGate,
) {
    let visible = visibility_source(gate.state.clone(), app.clone());
    // 速率差分的时基必须是**单调**时钟：墙钟被 NTP 往回校一秒，Δt 就会算成负数（钳到下限后是个
    // 天文数字速率）；往前校则把速率算低。与 `run_connections_stream` 的 `clock` 同一理由。
    let clock = Instant::now();
    let mut meter = StatsAggregator::new();
    // 核未运行的清零帧是否已推过（边沿触发；同 `run_connections_stream` 的 offline_sent）。
    let mut offline_sent = false;

    while !stop.load(Ordering::Relaxed) {
        // ① 降流门：关着就在这里断流待命。
        gate.wait_until(true, &visible).await;

        // ② 核未运行 → 不碰 gRPC，推一帧清零态（只在进入该态时推一次；核停着重复推相同空帧
        //    只会让渲染端白重渲）。
        let status = proxy.status();
        if !status.running || status.clash_api_port == 0 {
            if !offline_sent {
                broadcast(&app, EVENT_STATS_UPDATED, offline_stats_frame());
                offline_sent = true;
            }
            meter.reset(); // 停核 = 旧基线作废（核回来是新的一条生命线，累计从 0 重来）
            tokio::time::sleep(PARK_RECHECK_INTERVAL).await;
            continue;
        }

        // ③ 建流。
        let port = status.clash_api_port;
        let secret = read_clash_secret(&config);
        let client = match SingBoxApiClient::connect(Endpoint::new("127.0.0.1", port), secret).await
        {
            Ok(c) => c,
            Err(e) => {
                log::debug!("Status 流：管理 API 连接失败 {e}");
                tokio::time::sleep(PARK_RECHECK_INTERVAL).await;
                continue;
            }
        };
        let mut stream =
            client.subscribe_status(STATS_STREAM_INTERVAL_NS, ReconnectConfig::default());
        // 新流 = 新的一份真相：速率基线在此刻作废。
        meter.reset();
        offline_sent = false;
        // 上一次见到的 `trafficAvailable`（`None` = 本条流还没见过帧 → 首帧必报一次）。
        // 随流而生、随流而灭：见 [`traffic_availability_changed`]。
        let mut traffic_available: Option<bool> = None;
        log::debug!("Status 流已订阅（port={port}）");

        // ④ 流循环。
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            tokio::select! {
                frame = stream.recv() => match frame {
                    Some(st) => {
                        // 显式判 `trafficAvailable`：核没有 trafficManager 时本流照推、字段安静全 0，
                        // 不喊出来就是「0 B/s 且零报错」，与真的没流量无从区分。
                        if traffic_availability_changed(traffic_available, st.traffic_available) {
                            traffic_available = Some(st.traffic_available);
                            if st.traffic_available {
                                log::debug!("Status 流：trafficAvailable=true（流量统计可用）");
                            } else {
                                log::warn!(
                                    "sing-box 报 trafficAvailable=false：核内未构造 trafficManager，\
                                     本流的累计/连接数字段将恒为 0 且**不会报任何错** —— \
                                     状态栏的速率、总流量、连接数三个数字全是假 0，别当成「没在传数据」"
                                );
                            }
                        }
                        meter.on_status(&daemon_status_to_engine(&st), mono_ms(clock));
                        // 门关的一瞬可能正好收到一帧（`wait_until(false, ..)` 那条腿还没被调度到）→
                        // emit 前再看一次订阅门，别把帧推给已经没人看的窗口。
                        if gate.topic_open(Topic::Stats) {
                            broadcast(&app, EVENT_STATS_UPDATED, meter.snapshot());
                        }
                    }
                    // ReconnectingStream 正常语义下不返 None；真返了说明它内部终止 → 重建。
                    None => break,
                },
                // 门关（退订 / 主窗隐藏）→ 跳出即 drop 流，整条链路成本归零。
                () = gate.wait_until(false, &visible) => break,
                // 兜底唤醒：复核核状态（流自己发现不了核停 / 换端口）。
                () = tokio::time::sleep(PARK_RECHECK_INTERVAL) => {}
            }

            // 核停 / 换端口（换核、重启动态口）→ 断流重来。ReconnectingStream 自己发现不了这两件事。
            let st = proxy.status();
            if !st.running || st.clash_api_port != port {
                break;
            }
        }
        log::debug!("Status 流已断开（待重订阅）");
    }
    log::debug!("stats relay 已退出");
}

#[cfg(test)]
mod tests {
    use super::*;
    /// impl 内方法的源码切片工具（`guard_scan::top_level_fn_body` 只认列 0 的右花括号，
    /// 对 impl 里的方法会一路切到整个 impl 结束 → 守卫可被「删这里、加那里」骗过）。
    use crate::runtime::core_update_scheduler::method_scan::method_body;

    fn conn(id: &str, domain: &str, chain: &str) -> daemon::Connection {
        daemon::Connection {
            id: id.to_string(),
            domain: domain.to_string(),
            chain_list: vec![chain.to_string()],
            rule: "final".to_string(),
            ..Default::default()
        }
    }

    fn engine_conn(id: &str, closed_at: i64) -> SingBoxConnection {
        SingBoxConnection {
            id: id.to_string(),
            domain: format!("{id}.example"),
            chain_list: vec!["hk".to_string()],
            closed_at,
            ..Default::default()
        }
    }

    #[test]
    fn closed_history_is_newest_first_and_capped_to_singbox_replay_limit() {
        let events = SingBoxConnectionEvents {
            reset: true,
            events: (1..=1_002)
                .map(|n| SingBoxConnectionEvent {
                    kind: ConnectionEventType::New,
                    connection: Some(engine_conn(&format!("c{n}"), n)),
                    ..Default::default()
                })
                .collect(),
        };
        let mut history = ClosedHistory::default();
        assert!(history.apply_events(&events, &StatsAggregator::new()));
        assert_eq!(history.entries.len(), MAX_CLOSED_HISTORY);
        assert_eq!(history.entries.first().unwrap().closed_at, 1_002);
        assert_eq!(history.entries.last().unwrap().closed_at, 3);
    }

    #[test]
    fn clearing_closed_history_blocks_old_reset_replay_but_keeps_new_closes() {
        let mut history = ClosedHistory::default();
        history.clear(500);
        let events = SingBoxConnectionEvents {
            reset: true,
            events: vec![
                SingBoxConnectionEvent {
                    kind: ConnectionEventType::New,
                    connection: Some(engine_conn("old", 499)),
                    ..Default::default()
                },
                SingBoxConnectionEvent {
                    kind: ConnectionEventType::New,
                    connection: Some(engine_conn("new", 501)),
                    ..Default::default()
                },
            ],
        };
        assert!(history.apply_events(&events, &StatsAggregator::new()));
        assert_eq!(history.entries.len(), 1);
        assert_eq!(history.entries[0].entry.id, "new");
    }

    #[test]
    fn closed_event_without_payload_uses_active_entry_before_removal() {
        let mut active = StatsAggregator::new();
        active.on_connection_events(
            &SingBoxConnectionEvents {
                reset: false,
                events: vec![SingBoxConnectionEvent {
                    kind: ConnectionEventType::New,
                    connection: Some(engine_conn("live", 0)),
                    ..Default::default()
                }],
            },
            0,
        );
        let closed = SingBoxConnectionEvents {
            reset: false,
            events: vec![SingBoxConnectionEvent {
                kind: ConnectionEventType::Closed,
                id: "live".to_string(),
                closed_at: 700,
                ..Default::default()
            }],
        };
        let mut history = ClosedHistory::default();
        assert!(history.apply_events(&closed, &active));
        assert_eq!(history.entries[0].entry.id, "live");
        active.on_connection_events(&closed, 0);
        assert_eq!(
            active.conn_count(),
            0,
            "活动表仍按 CLOSED 删除，不被历史污染"
        );
    }

    #[test]
    fn maps_daemon_connection_fields() {
        let c = daemon::Connection {
            id: "c1".into(),
            source: "1.2.3.4:1234".into(),
            destination: "5.6.7.8:443".into(),
            domain: "example.com".into(),
            network: "tcp".into(),
            inbound_type: "Tun".into(),
            rule: "geoip".into(),
            chain_list: vec!["hk".into()],
            uplink_total: 111,
            downlink_total: 222,
            process_info: Some(daemon::ProcessInfo {
                process_path: "/usr/bin/curl".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let e = daemon_conn_to_entry(&c);
        assert_eq!(e.id, "c1");
        assert_eq!(e.chains, vec!["hk"]);
        let m = e.metadata.unwrap();
        assert_eq!(m.host.as_deref(), Some("example.com"));
        assert_eq!(m.destination_ip.as_deref(), Some("5.6.7.8"));
        assert_eq!(m.destination_port.as_deref(), Some("443"));
        assert_eq!(m.process_path.as_deref(), Some("/usr/bin/curl"));
        assert_eq!(e.upload, Some(111));
        assert_eq!(e.download, Some(222));
    }

    #[test]
    fn build_aggregate_counts_and_excludes_dead_connections() {
        let mut dead = conn("dead", "dead.com", "hk");
        dead.closed_at = 1_000_000_000; // 历史环死连接 → 必须被过滤
        let conns = vec![
            conn("c0", "a.com", "hk"),
            conn("c1", "a.com", "hk"),
            conn("c2", "b.com", "us"),
            dead,
        ];
        let agg = build_aggregate(&conns, 0);
        assert_eq!(agg.total, 3, "死连接不计入 total");
        let a = agg.hosts.iter().find(|h| h.name == "a.com").unwrap();
        assert_eq!(a.count, 2);
        assert!(
            agg.hosts.iter().all(|h| h.name != "dead.com"),
            "死连接不建 host 节点"
        );
        let hk = agg.outbounds.iter().find(|o| o.name == "hk").unwrap();
        assert_eq!(hk.count, 2);
    }

    // ── change-driven 去重的变异门（BUG-1 relay 核心）──
    // 打断 emit（signature_changed 恒 None）→ `first_frame_emits` 转红；
    // 打断去重（signature_changed 恒 Some）→ `same_content_deduped` 转红。

    #[test]
    fn first_frame_emits() {
        let agg = build_aggregate(&[conn("c0", "a.com", "hk")], 0);
        // 无上帧签名 → 必推（Some）。
        assert!(signature_changed(&agg, &None).is_some());
    }

    #[test]
    fn same_content_deduped() {
        // 同内容、不同采样时刻 at → 签名相同（at 被剔）→ 去重（None）。
        let agg1 = build_aggregate(&[conn("c0", "a.com", "hk")], 1000);
        let sig = aggregate_signature(&agg1);
        let agg2 = build_aggregate(&[conn("c0", "a.com", "hk")], 9_999_999);
        assert!(
            signature_changed(&agg2, &Some(sig)).is_none(),
            "内容不变（仅 at 变）应去重不推"
        );
    }

    #[test]
    fn content_change_emits_new_signature() {
        let agg1 = build_aggregate(&[conn("c0", "a.com", "hk")], 0);
        let sig = aggregate_signature(&agg1);
        // 多一条连接 → host 计数变 → 签名变 → 推。
        let agg2 = build_aggregate(&[conn("c0", "a.com", "hk"), conn("c1", "a.com", "hk")], 0);
        assert!(signature_changed(&agg2, &Some(sig)).is_some());
    }

    // ── detail topic：build_detail 数据面（EVENT_CONNECTIONS_DETAIL 供数）──

    /// 明细逐条下发（不聚合），死连接过滤，字段经 trim 裁剪后仍在。
    /// 打断死连接过滤 → 本测转红；打断 map（返回空 vec）→ 本测转红。
    #[test]
    fn build_detail_lists_live_connections_and_excludes_dead() {
        let mut dead = conn("dead", "dead.com", "hk");
        dead.closed_at = 1_000_000_000;
        let conns = vec![conn("c0", "a.com", "hk"), conn("c1", "b.com", "us"), dead];
        let snap = build_detail(&conns, 4_242);
        assert_eq!(
            snap.at, 4_242,
            "采样时刻必须原样带出（渲染端靠它算速率差分）"
        );
        let ids: Vec<&str> = snap.connections.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["c0", "c1"], "逐条下发活跃连接，死连接过滤");
        let first = &snap.connections[0];
        assert_eq!(
            first.metadata.as_ref().unwrap().host.as_deref(),
            Some("a.com"),
            "明细须带 metadata（明细表的域名列靠它）"
        );
    }

    /// 无活跃连接 → 空快照（非 error），明细页据此显示「无活动连接」。
    #[test]
    fn build_detail_empty_when_no_live_connections() {
        let mut dead = conn("dead", "dead.com", "hk");
        dead.closed_at = 1;
        assert!(build_detail(&[dead], 0).connections.is_empty());
        assert!(build_detail(&[], 0).connections.is_empty());
    }

    /// 明细含累计字节 —— 渲染端每条连接的速率/累计列全靠它，丢了则表格恒显 0。
    #[test]
    fn build_detail_carries_byte_totals() {
        let mut c = conn("c0", "a.com", "hk");
        c.uplink_total = 111;
        c.downlink_total = 222;
        let snap = build_detail(&[c], 0);
        assert_eq!(snap.connections[0].upload, Some(111));
        assert_eq!(snap.connections[0].download, Some(222));
    }

    // ── BUG-P2-1：停核 offline 帧（首页拓扑 / StatusBar 不得停在旧数据）──

    /// 停核 → 必须推一帧空聚合（total=0 / 无 host），且**只推一帧**（签名去重天然边沿触发）。
    /// 打断 `offline_aggregate_frame` 里的 `emit(agg)` → 本测第一段转红。
    #[test]
    fn offline_aggregate_frame_emits_empty_once_then_dedupes() {
        // 停核前的最后一帧：有连接。
        let live = build_aggregate(&[conn("c0", "a.com", "hk")], 0);
        let last_sig = Some(aggregate_signature(&live));

        // 进入停核态 → 推一帧空聚合。
        let mut emitted: Vec<ConnectionsAggregate> = Vec::new();
        let sig = offline_aggregate_frame(&last_sig, 1_000, |a| emitted.push(a));
        assert!(
            sig.is_some(),
            "停核必须推空帧（否则首页拓扑停在旧 host 列表）"
        );
        assert_eq!(emitted.len(), 1, "恰好一帧");
        assert_eq!(emitted[0].total, 0, "空聚合：连接数归零");
        assert!(emitted[0].hosts.is_empty(), "空聚合：旧 host 列表必须清掉");

        // 核仍停着的后续每一轮 → 去重，不重推（否则渲染端每秒白重渲一次）。
        let mut again: Vec<ConnectionsAggregate> = Vec::new();
        assert!(
            offline_aggregate_frame(&sig, 9_999, |a| again.push(a)).is_none(),
            "停核态逐秒重推内容相同的空帧 = 白重渲"
        );
        assert!(again.is_empty());
    }

    /// 核回来后的首帧内容非空 → 签名与空签名不同 → 必推（停核态不得把核恢复后的首帧吃掉）。
    #[test]
    fn aggregate_emits_again_after_core_returns() {
        let empty_sig = offline_aggregate_frame(&None, 0, |_| {}).expect("首次 offline 必推");
        let live = build_aggregate(&[conn("c0", "a.com", "hk")], 0);
        assert!(
            signature_changed(&live, &Some(empty_sig)).is_some(),
            "核恢复后的真实首帧必须推（否则首页恒空）"
        );
    }

    /// 停核清零帧：速率 / 累计 / 连接数全 0（StatusBar 据此归零而非停格），**且键名是 TS 契约那五个**。
    ///
    /// 键名这一半是本批新长出来的判据：帧载荷从手拼的 `json!` 换成了直接 `Serialize` 的
    /// `TrafficStats`，少了 `rename_all` 就整帧变下划线名而两侧类型系统都不报错
    /// （契约本体的锁在 `polaris_stats_engine` 的 `traffic_stats_json_keys_match_ts_contract`，
    /// 本条锁的是**这条 emit 路径**真送出那份契约）。
    #[test]
    fn offline_stats_frame_is_all_zero() {
        let v = serde_json::to_value(offline_stats_frame()).expect("清零帧应可序列化");
        assert_eq!(v["uploadSpeed"], 0);
        assert_eq!(v["downloadSpeed"], 0);
        assert_eq!(v["totalUpload"], 0);
        assert_eq!(v["totalDownload"], 0);
        assert_eq!(v["activeConnections"], 0);
        assert_eq!(
            v.as_object().map(serde_json::Map::len),
            Some(5),
            "清零帧的键名/键数必须与 TS 契约一致（下划线名前端读不到，且两侧都不会报错）"
        );
    }

    /// 清零帧**不得污染速率基线**：核回来后的首帧速率必须是 0，而不是拿「停核期的 0」当基准，
    /// 把核重启后的全部历史累计字节一次性算成瞬时速率（天文数字尖峰）。
    ///
    /// 这条锁的是 `offline_stats_frame()` 是个**不碰任何差分状态**的常量帧这一签名约束：
    /// 若改成从聚合器里取（`meter.on_status(&Default::default(), t); meter.snapshot()`），
    /// 那次调用就把「停核期的 0」写进了基线，本测第二段转红。
    #[test]
    fn offline_stats_frame_does_not_poison_speed_baseline() {
        // 停核前跑过一帧，留下基线。
        let mut meter = StatsAggregator::new();
        meter.on_status(&status_totals(1_000_000, 1_000_000), 0);

        // 停核：推清零帧（不经聚合器）+ 生产代码紧接着 reset。
        let z = offline_stats_frame();
        assert_eq!(z, TrafficStats::zeroed());
        meter.reset();

        // 核回来：首帧带巨大历史累计 → 速率必须是 0（无基线），不得是尖峰。
        meter.on_status(&status_totals(9_000_000, 9_000_000), 1_000);
        let s = meter.snapshot();
        assert_eq!(
            s.upload_speed, 0,
            "核重启后首帧速率必须 0，不得把历史累计算成尖峰"
        );
        assert_eq!(s.download_speed, 0);
    }

    // ── BUG-P2-3：relay spawn 侧 TOCTOU 闸门 ──

    /// spawn 决策：已在跑 → 不起（单例）；零订阅者 → 不起（TOCTOU：并发 unsubscribe 已退光）。
    /// 打断计数条件（`!slot_occupied`）→ `零订阅者` 用例转红；打断单例条件 → `已在跑` 用例转红。
    #[test]
    fn should_spawn_poller_requires_free_slot_and_live_subscriber() {
        assert!(should_spawn_poller(false, 1), "空 slot + 有订阅 → 起");
        assert!(
            !should_spawn_poller(false, 0),
            "零订阅者绝不起 relay（否则上游流永久开着、无人能停）"
        );
        assert!(!should_spawn_poller(true, 1), "已在跑 → 幂等 no-op");
        assert!(!should_spawn_poller(true, 0));
    }

    // ── BUG-P2-2：clear_window 清账（webview reload 后旧上下文订阅无人退订）──

    /// reload：旧上下文的订阅无人退订，label 仍是 "main" → 必须由 clear_window 清账 + 停 relay，
    /// 否则计数恒 ≥1、停机闸门恒拦 → 上游流永久开着、`subs` 无界累积。
    /// 打断 clear_window 的 registry 退订循环 → 本测转红。
    #[test]
    fn clear_window_drops_all_subs_and_stops_pollers() {
        let relay = StatsRelay::new();
        // 模拟旧 JS 上下文的全部 topic 订阅（经真实记账路径入账）。
        for (topic, slot) in [
            (Topic::Connections, &relay.connections),
            (Topic::Stats, &relay.stats_poller),
            (Topic::Detail, &relay.connections),
            (Topic::Closed, &relay.connections),
        ] {
            let token = relay.gate.registry.lock().unwrap().subscribe(topic, "main");
            relay
                .subs
                .lock()
                .unwrap()
                .push(("main".to_string(), topic, token));
            *slot.lock().unwrap() = Some(dummy_poller());
        }
        assert_eq!(
            relay.connections_subscriber_count(),
            3,
            "连接流的计数是三个投影之和"
        );

        relay.clear_window("main");

        assert_eq!(
            relay.connections_subscriber_count(),
            0,
            "reload 后旧订阅必须清账"
        );
        assert_eq!(relay.stats_subscriber_count(), 0);
        assert!(
            relay.subs.lock().unwrap().is_empty(),
            "subs 记账清空（否则无界累积）"
        );
        assert!(
            relay.connections.lock().unwrap().is_none(),
            "无订阅者 → 连接流必停"
        );
        assert!(relay.stats_poller.lock().unwrap().is_none());
    }

    /// clear_window 只清目标窗口：其它窗口的订阅与 poller 不得被误清。
    #[test]
    fn clear_window_spares_other_windows() {
        let relay = StatsRelay::new();
        let token = relay
            .gate
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Stats, "other");
        relay
            .subs
            .lock()
            .unwrap()
            .push(("other".to_string(), Topic::Stats, token));
        *relay.stats_poller.lock().unwrap() = Some(dummy_poller());

        relay.clear_window("main");

        assert_eq!(relay.stats_subscriber_count(), 1, "别的窗口的订阅不得被清");
        assert!(
            relay.stats_poller.lock().unwrap().is_some(),
            "仍有订阅 → relay 不得停"
        );
    }

    #[test]
    fn parse_topic_maps_aggregate_to_connections() {
        assert_eq!(parse_topic("aggregate"), Some(Topic::Connections));
        assert_eq!(parse_topic("stats"), Some(Topic::Stats));
        assert_eq!(parse_topic("detail"), Some(Topic::Detail));
        assert_eq!(parse_topic("closed"), Some(Topic::Closed));
        assert_eq!(parse_topic("bogus"), None);
    }

    // ── BUG-D：relay start/stop TOCTOU 闸门 ──
    // stop_* 在 slot 锁下复查订阅计数：仍有订阅 → 绝不停（否则留活订阅无 relay，数据面冻结）。
    // 直接装占位 relay 进 slot（不经 ensure，避免真起后台流），断言守卫决策，不依赖时序竞态。

    /// 造一个不驱动任何真实数据面的占位 relay（stop flag + 立即完成的空任务句柄）。
    fn dummy_poller() -> AggregatePoller {
        AggregatePoller {
            stop: Arc::new(AtomicBool::new(false)),
            handle: tauri::async_runtime::spawn(async {}),
        }
    }

    #[test]
    fn stop_stats_stream_keeps_running_while_subscriber_remains() {
        let relay = StatsRelay::new();
        *relay.stats_poller.lock().unwrap() = Some(dummy_poller());
        // 模拟并发 subscribe 已重新计数（见 slot=Some 依赖现有 poller）。
        let token = relay
            .gate
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Stats, "w1");
        assert_eq!(relay.stats_subscriber_count(), 1);

        relay.stop_stats_stream();
        assert!(
            relay.stats_poller.lock().unwrap().is_some(),
            "仍有活订阅 → 闸门必须拦住 stop（否则 liveness gap）"
        );

        // 退订到 0 → stop 正常生效。
        relay
            .gate
            .registry
            .lock()
            .unwrap()
            .unsubscribe(Topic::Stats, token);
        assert_eq!(relay.stats_subscriber_count(), 0);
        relay.stop_stats_stream();
        assert!(
            relay.stats_poller.lock().unwrap().is_none(),
            "无订阅 → 正常停 relay"
        );
    }

    /// 🔴 **TOCTOU 闸门 + 共用槽位：任一条投影还有订阅者，连接流就不许停。**
    ///
    /// 取代了原来分列的 poller —— 三个 topic
    /// 现在共用一条流一个槽位，分开测反而测不到真正的新风险：**只退订其中一条时误停整条流**
    /// （现象是关掉首页拓扑后连接明细页跟着冻住，反之亦然）。
    ///
    /// **变异探针**：`connections_subscriber_count` 改成只数一条 topic ⇒ 转红；
    /// `stop_connections_stream` 里锁内那次复查删掉 ⇒ 第一段断言转红。
    #[test]
    fn stop_connections_stream_keeps_running_while_any_projection_remains() {
        let relay = StatsRelay::new();
        *relay.connections.lock().unwrap() = Some(dummy_poller());
        let t_agg = relay
            .gate
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Connections, "w1");
        let t_detail = relay
            .gate
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Detail, "w1");
        let t_closed = relay
            .gate
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Closed, "w1");
        assert_eq!(relay.connections_subscriber_count(), 3);

        relay.stop_connections_stream();
        assert!(
            relay.connections.lock().unwrap().is_some(),
            "三个投影都还订着 → 闸门必须拦住 stop"
        );

        // 只退订拓扑：明细还在看 → 流必须留着
        relay
            .gate
            .registry
            .lock()
            .unwrap()
            .unsubscribe(Topic::Connections, t_agg);
        relay.stop_connections_stream();
        assert!(
            relay.connections.lock().unwrap().is_some(),
            "只退订拓扑、明细仍订着 → 绝不能停整条流（否则连接明细页冻住）"
        );

        // 活动明细退订，已结束历史仍在看 → 流仍须保留
        relay
            .gate
            .registry
            .lock()
            .unwrap()
            .unsubscribe(Topic::Detail, t_detail);
        relay.stop_connections_stream();
        assert!(relay.connections.lock().unwrap().is_some());

        // 最后一条也退订 → 正常停
        relay
            .gate
            .registry
            .lock()
            .unwrap()
            .unsubscribe(Topic::Closed, t_closed);
        relay.stop_connections_stream();
        assert!(
            relay.connections.lock().unwrap().is_none(),
            "三个投影都无订阅 → 正常停流"
        );
    }

    // ── stats topic：Status 流数据面（EVENT_STATS_UPDATED 供数）──

    /// 只带累计的 Status 帧（速率推导的最小夹具）。
    fn status_totals(up: i64, down: i64) -> SingBoxStatus {
        SingBoxStatus {
            uplink_total: up,
            downlink_total: down,
            traffic_available: true,
            ..Default::default()
        }
    }

    /// 🔴 **prost `daemon::Status` → 纯逻辑 `SingBoxStatus` 必须逐字段搬到，一个不漏。**
    ///
    /// 漏字段在这里是**静默**的：`..Default::default()` 把漏掉的那个填成 0，而 0 恰好是
    /// 「没流量 / 没连接 / 统计不可用」这些完全合理的取值 —— 编译过、测试绿、UI 上只是永远显示 0。
    /// 本批要修的原缺陷（`trafficAvailable` 曾被误 typed 成 `i64`）就是同一族。
    ///
    /// **变异探针**：映射里删任一行（让它落进默认值）⇒ 转红。
    #[test]
    fn daemon_status_to_engine_carries_every_field() {
        let raw = daemon::Status {
            memory: 111,
            goroutines: 22,
            connections_in: 33,
            connections_out: 44,
            traffic_available: true,
            uplink: 55,
            downlink: 66,
            uplink_total: 777,
            downlink_total: 888,
        };
        assert_eq!(
            daemon_status_to_engine(&raw),
            SingBoxStatus {
                memory: 111,
                goroutines: 22,
                connections_in: 33,
                connections_out: 44,
                traffic_available: true,
                uplink: 55,
                downlink: 66,
                uplink_total: 777,
                downlink_total: 888,
            }
        );
    }

    /// 🔴 **`trafficAvailable=false` 必须发出可观测信号，且只在变化沿发。**
    ///
    /// 核内没有 `trafficManager` 时 `SubscribeStatus` **不报错**，只是把累计/连接数三个字段留成 0：
    /// UI 表现是「0 B/s 且零报错」，与「真的没流量」逐像素一致。不判它 = 这条故障永远查不出来。
    ///
    /// **变异探针**：把判据改成恒 false（= 不判、静默）⇒ 第一段转红；改成恒 true（= 每帧一条日志，
    /// 每秒一条噪音把真信号淹掉）⇒ 第二段转红。
    #[test]
    fn traffic_availability_reports_only_on_change() {
        assert!(
            traffic_availability_changed(None, false),
            "新流的第一帧必须报一次（哪怕值一直是 false，也得让人看见一次）"
        );
        assert!(traffic_availability_changed(None, true), "首帧必报");
        assert!(
            !traffic_availability_changed(Some(true), true),
            "值没变就别每秒喊一遍 —— 噪音会把真信号淹掉"
        );
        assert!(!traffic_availability_changed(Some(false), false));
        assert!(
            traffic_availability_changed(Some(true), false),
            "true → false（统计刚失效）必须立刻喊"
        );
        assert!(
            traffic_availability_changed(Some(false), true),
            "false → true（恢复）也该记一笔，否则日志里只有病、没有好"
        );
    }

    /// 首帧：速率 0（无基线），累计 + 活跃连接数即刻真实。
    ///
    /// **变异探针**：把首帧速率改成拿 `uplink_total` 本身（或拿 `Status.uplink`）⇒ 第一段转红。
    #[test]
    fn stats_first_frame_reports_zero_speed_with_real_totals() {
        let mut meter = StatsAggregator::new();
        meter.on_status(
            &SingBoxStatus {
                uplink_total: 100,
                downlink_total: 900,
                connections_in: 3,
                traffic_available: true,
                ..Default::default()
            },
            0,
        );
        let s = meter.snapshot();
        assert_eq!(s.upload_speed, 0, "首帧无基线 → 速率 0");
        assert_eq!(s.download_speed, 0);
        assert_eq!(s.total_upload, 100);
        assert_eq!(s.total_download, 900);
        assert_eq!(s.active_connections, 3, "活跃连接数取 Status.connectionsIn");
    }

    /// 🔴 **速率的分母是实测 Δt，不是请求里那个 `STATS_STREAM_INTERVAL_NS`。**
    ///
    /// 服务端 ticker 的实际间隔含调度抖动、wire 上也不回传，把常量当分母就是拿期望值冒充实测值。
    /// 本例故意让实测 Δt（2s）≠ 请求间隔（1s）：拿常量当分母会实得 4000/8000。
    ///
    /// **变异探针**：分母换成 `STATS_STREAM_INTERVAL_NS / 1_000_000_000` ⇒ 转红；
    /// 直接用 `Status.uplink` 当速率 ⇒ 实得 1 ⇒ 转红。
    #[test]
    fn stats_speed_divides_by_measured_dt_not_the_requested_interval() {
        let mut meter = StatsAggregator::new();
        meter.on_status(&status_totals(1_000, 2_000), 10_000);
        meter.on_status(
            &SingBoxStatus {
                uplink: 1, // 诱饵：内核这个字段不是速率
                downlink: 2,
                ..status_totals(5_000, 10_000)
            },
            12_000, // 实测 Δt = 2s ≠ 请求的 1s
        );
        let s = meter.snapshot();
        assert_eq!(s.upload_speed, 2_000, "(5000-1000)/2s");
        assert_eq!(s.download_speed, 4_000, "(10000-2000)/2s");
    }

    // ══════════════════════════════════════════════════════════════════════════
    // 降流门（维度7）：两条长驻流都过 `should_stream`（订阅集 × 可见性）
    //
    // 被测对象是两条 relay 真正调用的那个点 —— `StreamGate::wait_until`。全部 topic 只有
    // 一种降流机制（drop 流），故门测试只有这一套夹具；`PollGate`（轮询时代的「park 一拍」）
    // 随 stats 换流一并删除，继续拿它测就是在测一个生产里已不存在的形状。
    //
    // 变异锁（下列转红结果均为**实跑**，非推演）：
    //  - 拿掉 `wait_until` 里的判定（无条件放行）→ 4 例转红：`park_gated_topic_when_window_hidden` /
    //    `park_any_topic_without_subscriber` / `park_after_last_subscriber_leaves` /
    //    `可见性翻回true_立刻恢复`。
    //  - 拿掉 `watch` 唤醒（只留超时兜底）→ `可见性翻回true_立刻恢复` 转红（恢复要等满兜底周期）。
    //  - 把 `StreamGate::stats` 的 `demand` 换成 `should_stream_connections`（两条流共用一条判据）
    //    → `三topic各自独立判定` 转红（只订 stats 时 Status 流不开、连接流反被拉起）。
    // 用虚拟时钟（`start_paused`）：兜底回读周期不占真实时间，测试恒毫秒级。
    // ══════════════════════════════════════════════════════════════════════════

    use std::sync::atomic::AtomicBool as GateFlag;

    /// 连接长驻流的门夹具（需求 = aggregate ∪ detail ∪ closed）。
    ///
    /// 必须走生产同一个构造器：测试自己拼 `StreamGate { .. }` 就等于给测试造了一条与生产
    /// 无关的判据，门测试会全部失去判据。
    fn test_stream_gate() -> (Arc<StreamGateState>, StreamGate, Arc<GateFlag>) {
        let state = Arc::new(StreamGateState::new());
        let gate = StreamGate::connections(state.clone());
        (state, gate, Arc::new(GateFlag::new(true)))
    }

    /// Status 长驻流的门夹具（需求 = stats topic）。
    fn test_stats_gate() -> (Arc<StreamGateState>, StreamGate, Arc<GateFlag>) {
        let state = Arc::new(StreamGateState::new());
        let gate = StreamGate::stats(state.clone());
        (state, gate, Arc::new(GateFlag::new(true)))
    }

    /// 门**开着**时 `wait_until(false, ..)` 必须一直不返回（流继续跑），反之亦然。
    /// 用远大于兜底周期的虚拟时限：判定若反了会立刻返回 → 转红。
    async fn assert_gate_holds(
        gate: &mut StreamGate,
        want: bool,
        visible: &Arc<GateFlag>,
        why: &str,
    ) {
        let src = flag_visibility_source(visible.clone());
        assert!(
            tokio::time::timeout(Duration::from_secs(30), gate.wait_until(want, &src))
                .await
                .is_err(),
            "{why}"
        );
    }

    /// 可见性源（替代生产里那个「读缓存 + 投主线程刷新」的 [`visibility_source`]）：
    /// 读一个可随时翻转的 flag，注入点与生产完全同一处（`StreamGate::wait_until` 的入参）。
    fn flag_visibility_source(flag: Arc<GateFlag>) -> impl Fn() -> bool {
        move || flag.load(Ordering::Relaxed)
    }

    /// 可见性 false + 有订阅 → **全部 topic**断流（不收、不 emit）。
    ///
    /// 覆盖面含 Stats：门控口径一致后，隐藏态下一条 gRPC 都不该剩。
    #[tokio::test(start_paused = true)]
    async fn park_gated_topic_when_window_hidden() {
        // stats（Status 流）：门关 = 流不该开着。
        let (state, mut gate, visible) = test_stats_gate();
        state
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Stats, "main");
        visible.store(false, Ordering::Relaxed);
        assert_gate_holds(
            &mut gate,
            true,
            &visible,
            "窗口隐藏 + 有 stats 订阅 → Status 流必须保持断开，绝不再收帧 + emit",
        )
        .await;

        // aggregate / detail / closed（连接流）：门关 = 流不该开着。
        for topic in [Topic::Connections, Topic::Detail, Topic::Closed] {
            let (state, mut gate, visible) = test_stream_gate();
            state.registry.lock().unwrap().subscribe(topic, "main");
            visible.store(false, Ordering::Relaxed);
            assert_gate_holds(
                &mut gate,
                true,
                &visible,
                "窗口隐藏 + 有连接订阅 → 连接流必须保持断开，绝不再收事件 + emit",
            )
            .await;
        }
    }

    /// 无订阅者 → 两条流都不开。
    #[tokio::test(start_paused = true)]
    async fn park_any_topic_without_subscriber() {
        let (_state, mut gate, visible) = test_stats_gate();
        assert_gate_holds(
            &mut gate,
            true,
            &visible,
            "无 stats 订阅者 → 无人消费，Status 流必须保持断开",
        )
        .await;

        let (_state, mut sgate, visible) = test_stream_gate();
        assert_gate_holds(
            &mut sgate,
            true,
            &visible,
            "三个连接视图都没订阅者 → 连接流必须保持断开",
        )
        .await;
    }

    /// 退订到零 → 原本放行的门必须翻成 park（订阅集是门的另一条腿）。
    /// 🔴 **三个投影都退订才断流；任一仍在看时流必须留着。**
    ///
    /// **变异探针**：`should_stream_connections` 改成 `&&`（或 `stop_connections_stream` 的
    /// 计数改成只看一条 topic）⇒ 「关掉首页但连接页还开着」时流被停掉 ⇒ 转红。
    #[tokio::test(start_paused = true)]
    async fn park_after_last_subscriber_leaves() {
        let (state, mut gate, visible) = test_stream_gate();
        let t_agg = state
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Connections, "main");
        let t_detail = state
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Detail, "main");
        let t_closed = state
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Closed, "main");
        let src = flag_visibility_source(visible.clone());
        tokio::time::timeout(Duration::from_secs(5), gate.wait_until(true, &src))
            .await
            .expect("有订阅 + 可见 → 流必须开");

        // 只退订拓扑：明细还在看 → 流必须留着
        state
            .registry
            .lock()
            .unwrap()
            .unsubscribe(Topic::Connections, t_agg);
        assert_gate_holds(
            &mut gate,
            false,
            &visible,
            "只退订拓扑、活动与已结束仍订着 → 连接流绝不能断",
        )
        .await;

        // 活动明细也退订，已结束历史仍在看 → 继续保持
        state
            .registry
            .lock()
            .unwrap()
            .unsubscribe(Topic::Detail, t_detail);
        assert_gate_holds(
            &mut gate,
            false,
            &visible,
            "已结束历史仍订着 → 连接流绝不能断",
        )
        .await;

        // 最后一条也退订 → 断流
        state
            .registry
            .lock()
            .unwrap()
            .unsubscribe(Topic::Closed, t_closed);
        let src = flag_visibility_source(visible.clone());
        tokio::time::timeout(Duration::from_secs(5), gate.wait_until(false, &src))
            .await
            .expect("最后一个订阅者退订 → 必须断流");
    }

    /// 可见性翻回 true → **立刻**恢复（不等下一拍整周期），用户切回窗口无可感知空窗。
    #[tokio::test(start_paused = true)]
    async fn 可见性翻回true_立刻恢复() {
        let (state, mut gate, visible) = test_stream_gate();
        state
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Connections, "main");
        visible.store(false, Ordering::Relaxed);
        assert_gate_holds(&mut gate, true, &visible, "先确认确实断着流").await;

        // 另一条腿（main.rs 的 Focused 触发器）把可见性写回 true 并 bump 门代次。
        let waker = state.clone();
        let flag = visible.clone();
        tokio::spawn(async move {
            flag.store(true, Ordering::Relaxed);
            waker.set_window_visible(true);
        });

        let src = flag_visibility_source(visible.clone());
        let started = tokio::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), gate.wait_until(true, &src))
            .await
            .expect("可见性翻回 true 必须立刻重订阅");
        assert!(
            started.elapsed() < PARK_RECHECK_INTERVAL / 2,
            "恢复必须由门变更立刻唤醒（实测 {:?}），而不是等满一个 PARK_RECHECK_INTERVAL 的兜底回读",
            started.elapsed()
        );
    }

    /// 三 topic 各自独立判定：只订了 stats → Status 流开，连接流仍断着。
    ///
    /// **变异探针**：把 `StreamGate::stats` 的 `demand` 换成 `should_stream_connections`
    /// （两条流共用一条判据）⇒ 第一段转红（只订 stats 时 Status 流打不开）。
    #[tokio::test(start_paused = true)]
    async fn 三topic各自独立判定() {
        let state = Arc::new(StreamGateState::new());
        state
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Stats, "main");
        let visible = Arc::new(GateFlag::new(true));
        let src = flag_visibility_source(visible.clone());

        let mut stats_gate = StreamGate::stats(state.clone());
        tokio::time::timeout(Duration::from_secs(5), stats_gate.wait_until(true, &src))
            .await
            .expect("有 stats 订阅 → Status 流必须开");

        let mut conn_gate = StreamGate::connections(state.clone());
        assert_gate_holds(
            &mut conn_gate,
            true,
            &visible,
            "只订了 stats 不该把连接长驻流拉起来 —— 它不消费连接表",
        )
        .await;
    }

    /// ★ 契约测试（口径一致 · 消费侧）：全部 topic 在同一可见性下**同进同退**。
    ///
    /// 前身是 `stats_topic_不受可见性门控`，断言「Stats 隐藏也放行」。该差异化语义已作废
    /// （理由见 `polaris_stats_engine::Topic::gated_by_visibility`：上游的 status 不门控是
    /// worker demand 握手载体，Polaris 没有 worker、没有该握手；而 上游 广播侧
    /// `StatsService.ts:312` / `StatsWorkerHost.ts:217` 本来就按可见性门控 stats）。
    ///
    /// 本条不是「再测一遍 `park_*`」：它把全部 topic 放在**同一次可见性翻转**下逐条比对，
    /// 任何一条被单独开成「隐藏也流」或「可见也不流」都转红。
    ///
    /// 所有 topic 共用同一种机制（drop 流 + 恢复时重订阅）；
    /// 契约本身（隐藏即停、恢复即刻）逐条不变。
    #[tokio::test(start_paused = true)]
    async fn 全部topic门控口径一致() {
        type Fixture = fn() -> (Arc<StreamGateState>, StreamGate, Arc<GateFlag>);
        for (topic, mk) in [
            (Topic::Stats, test_stats_gate as Fixture),
            (Topic::Connections, test_stream_gate as Fixture),
            (Topic::Detail, test_stream_gate as Fixture),
            (Topic::Closed, test_stream_gate as Fixture),
        ] {
            let (state, mut gate, visible) = mk();
            state.registry.lock().unwrap().subscribe(topic, "main");

            let src = flag_visibility_source(visible.clone());
            tokio::time::timeout(Duration::from_secs(5), gate.wait_until(true, &src))
                .await
                .unwrap_or_else(|_| panic!("{topic:?}：可见 + 有订阅 → 流必须开"));

            visible.store(false, Ordering::Relaxed);
            let src = flag_visibility_source(visible.clone());
            tokio::time::timeout(Duration::from_secs(5), gate.wait_until(false, &src))
                .await
                .unwrap_or_else(|_| panic!("{topic:?}：隐藏 → 流必须断（不是留着白收）"));

            visible.store(true, Ordering::Relaxed);
            let src = flag_visibility_source(visible.clone());
            tokio::time::timeout(Duration::from_secs(5), gate.wait_until(true, &src))
                .await
                .unwrap_or_else(|_| panic!("{topic:?}：窗口回来 → 必须重订阅"));
        }
    }

    /// 🔴 **断流恢复必须丢掉速率基线**（本批换流后，这条不变式换了落点，不是被删了）。
    ///
    /// 轮询时代它靠 `PollGate::next_tick` 返回「本拍前 park 过」、由 poller 手动复位 `last`。
    /// 长驻流下降流的动作是 drop 流，恢复的动作是**重新订阅** —— 于是判据落在建流处那一句
    /// `meter.reset()` 上：只要它在，跨越断流期的旧基线就不可能被沿用。
    ///
    /// 锁的是「隐藏期均速被当成当前速率」这个具体缺陷：用户隐藏窗口期间下过大文件，
    /// 切回来的瞬间状态栏闪一个与此刻无关的高速率。
    ///
    /// **变异探针**（实跑）：删掉 `run_stats_stream` 里建流后那句 `meter.reset()` ⇒ 转红。
    /// 纯逻辑那一半（`reset()` 真的丢基线）由 `polaris_stats_engine` 的
    /// `reset_drops_speed_baseline_so_next_frame_is_zero` 锁。
    #[test]
    fn 断流重订阅必须丢掉速率基线() {
        let src = include_str!("stats.rs");
        let body =
            crate::commands::guard_scan::top_level_fn_body(src, "async fn run_stats_stream(");
        let subscribe_at = body
            .find("client.subscribe_status(")
            .expect("锚点消失：stats relay 已不走 SubscribeStatus，守卫失去判据");
        let reset_at = body[subscribe_at..]
            .find("meter.reset();")
            .map(|i| i + subscribe_at)
            .expect(
                "建流后必须 `meter.reset()` —— 否则断流期跨越的旧基线会把整段空档的平均吞吐\
                 当成「此刻的速率」显示一帧",
            );
        assert!(subscribe_at < reset_at);
    }

    /// 🔴 **变异锁：stats relay 不得退回「拉全量连接表再求和」那条口径已坏的路。**
    ///
    /// 那条路的缺陷不是性能而是口径：`first_connection_snapshot` 返回的表**含内核历史环里的
    /// 死连接**，对它整表求 `uplink_total` 得到的「累计」会在环满（1000 条）后每淘汰一条就**下跌**
    /// 一截 —— 累计倒退，且 `saturating_sub` 把那一拍速率吃成 0。它修不掉，只能换掉。
    ///
    /// 一并锁住 emit 侧的订阅门：门关的一瞬仍可能收到一帧，不看门就会把它推给已经没人看的窗口。
    ///
    /// **变异探针**：把 `subscribe_status` 换回 `first_connection_snapshot` ⇒ 转红；
    /// 删掉 `gate.topic_open(Topic::Stats)` 那道 emit 门 ⇒ 转红。
    #[test]
    fn stats_relay是流驱动且emit过订阅门() {
        let src = include_str!("stats.rs");
        let body =
            crate::commands::guard_scan::top_level_fn_body(src, "async fn run_stats_stream(");
        assert!(
            !body.contains("first_connection_snapshot"),
            "stats relay 里出现了 `first_connection_snapshot` —— 那条路的累计口径是坏的（含死连接、\
             会随历史环淘汰而下跌），本批换掉的正是它"
        );
        assert!(
            body.contains("gate.wait_until(true, &visible).await"),
            "必须先等降流门开才建流 —— 否则无人看也照收帧"
        );
        assert!(
            body.contains("() = gate.wait_until(false, &visible) => break"),
            "门关那条腿必须 `break`（跳出即 drop 流）"
        );
        assert!(
            body.contains("if gate.topic_open(Topic::Stats) {"),
            "emit 前须看订阅门 —— 门关的一瞬仍可能收到一帧"
        );
    }

    /// 「订阅即出首帧」在流下**不需要节拍特判**：门开着就立刻返回、马上建流，而内核在建 ticker
    /// 之前就无条件 `Send` 一帧当前状态（`daemon/started_service.go:396`）。
    ///
    /// 本测取代了旧的 `首拍不睡后续按周期`（那条断言 `PollGate` 首拍不 sleep、第二拍睡满
    /// `POLL_INTERVAL`）。**不是放宽，是判据换了对象**：轮询节拍随 stats 换流一并删除，
    /// 继续断言它等于要求一个已不存在的东西存在。现在该锁的是「门开着时 `wait_until` 不引入
    /// 任何等待」——引入了，用户订阅后就得白等一个周期才看到第一个数字。
    #[tokio::test(start_paused = true)]
    async fn 门开着时不得引入任何等待() {
        let (state, mut gate, visible) = test_stats_gate();
        state
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Stats, "main");
        let src = flag_visibility_source(visible.clone());

        for round in 0..3 {
            let t0 = tokio::time::Instant::now();
            gate.wait_until(true, &src).await;
            assert_eq!(
                t0.elapsed(),
                Duration::ZERO,
                "第 {round} 次：门开着就该立刻返回（订阅即建流、即出首帧），不得有节拍式等待"
            );
        }
    }

    // ══════════════════════════════════════════════════════════════════════════
    // 拓扑节拍独立（本批）：aggregate 单独提频，另两条腿不动，降流门不被高频传染
    // ══════════════════════════════════════════════════════════════════════════

    /// 🔴 **变异锁：aggregate / detail 不再有任何「节拍」—— 它们是流驱动的。**
    ///
    /// 本测取代了旧的 `aggregate节拍快于另两条腿`（那条断言三条 topic 的 `PollGate` 睡了不同时长）。
    /// **不是放宽，是判据换了对象**：那条锁的是「拓扑轮询得比另两条快」，而本批把拓扑与明细的
    /// 轮询整个删掉了 —— 继续断言它们的轮询节拍等于要求一个已不存在的东西存在。
    ///
    /// 现在该锁的不变式有两条，都在这里：
    /// 1. 连接 relay 的主循环里**没有轮询节拍**（不得出现 `PollGate` / `next_tick`）；
    /// 2. 它的两条腿是「等门开」与「等门关」，门关那条**必须 `break`**（drop 流），不是 park。
    ///
    /// **变异探针**：把 `() = gate.wait_until(false, &visible) => break` 改成 `=> continue`
    /// 或整条腿删掉（= 不可见时不 drop 流，留着白收事件）⇒ 转红；
    /// 把外层的 `wait_until(true, ..)` 删掉（= 无人看也照开流）⇒ 转红；
    /// 把 `PollGate` / `next_tick` 引回连接 relay（= 复原成轮询）⇒ 转红。
    #[test]
    fn 连接流是流驱动而非节拍驱动() {
        let src = include_str!("stats.rs");
        let body =
            crate::commands::guard_scan::top_level_fn_body(src, "async fn run_connections_stream(");

        assert!(
            body.contains("gate.wait_until(true, &visible).await"),
            "连接 relay 必须先等降流门开才建流 —— 否则无人看也照收事件"
        );
        assert!(
            body.contains("() = gate.wait_until(false, &visible) => break"),
            "门关那条腿必须 `break`（跳出即 drop 流）。park 住不读流只会把帧堆在 tonic 缓冲和\
             内核发送窗口里，非但不省，还会把内核的连接事件分发堵住"
        );
        for forbidden in ["PollGate", "next_tick", "first_connection_snapshot"] {
            assert!(
                !body.contains(forbidden),
                "连接 relay 里出现了 `{forbidden}` —— 那是轮询的形状，本批换掉的正是它"
            );
        }
        assert!(
            body.contains("subscribe_connections("),
            "连接 relay 必须走长驻流订阅"
        );
    }

    /// 🟡 **变异锁：aggregate 的 emit 比 detail 勤，且两者都由 [`EmitGate`] 而非 sleep 决定。**
    ///
    /// 同一张连接表、同一条上游流，但拓扑推的是几十个计数、明细推的是整张表的 JSON ——
    /// 没有理由共用一个 emit 频率。
    ///
    /// **变异探针**：两个常量调成相等 ⇒ 第一段转红；把 `detail_emit` 也用
    /// [`AGGREGATE_EMIT_MIN_INTERVAL`] 构造（常量本身不动、只是接错线）⇒ 第二段转红。
    ///
    /// ⚠️ 第二段是**变异实测补上的**：只断言两个常量不等，抓不到「常量分得好好的，接线接错了」——
    /// 实测把 `EmitGate::new(DETAIL_EMIT_MIN_INTERVAL)` 换成 `AGGREGATE_EMIT_MIN_INTERVAL` 后
    /// 全测试套仍全绿。常量的判据必须落在**它被用的那一处**，不是它被定义的那一处。
    #[test]
    fn aggregate的emit比detail勤() {
        assert!(
            AGGREGATE_EMIT_MIN_INTERVAL < DETAIL_EMIT_MIN_INTERVAL,
            "拓扑 emit 必须严格勤于明细：前者载荷是几十个计数，后者是整张连接表的 JSON"
        );
        let src = include_str!("stats.rs");
        let body =
            crate::commands::guard_scan::top_level_fn_body(src, "async fn run_connections_stream(");
        assert!(
            body.contains("EmitGate::new(AGGREGATE_EMIT_MIN_INTERVAL)")
                && body.contains("EmitGate::new(DETAIL_EMIT_MIN_INTERVAL)"),
            "两条投影的闸门必须各用各的常量 —— 接成同一个，上面那条区间锁就形同虚设"
        );
    }

    /// 🔴 **变异锁：三个连接投影的 emit 都过闸门，且 emit 后必须 `mark_emitted`。**
    ///
    /// `mark_emitted` 漏掉会有一个很隐蔽的后果：闸门的 `pending` 永不清零 → `wait_for` 恒返回
    /// `ZERO` → select 的定时器分支退化成 `sleep(0)` → **忙转烧掉一个 tokio worker**，
    /// 而 UI 上一切正常（帧照推），没有任何症状指向它。
    ///
    /// **变异探针**：删任一 `mark_emitted` ⇒ 转红；把 `should_emit` 判定去掉改成逐帧 broadcast ⇒ 转红。
    #[test]
    fn 连接流emit走闸门不走裸sleep() {
        let src = include_str!("stats.rs");
        let body =
            crate::commands::guard_scan::top_level_fn_body(src, "async fn run_connections_stream(");
        for probe in [
            "agg_emit.should_emit(now)",
            "detail_emit.should_emit(now)",
            "closed_emit.should_emit(now)",
            "agg_emit.mark_emitted(now)",
            "detail_emit.mark_emitted(now)",
            "closed_emit.mark_emitted(now)",
            "agg_emit.note_change()",
            "detail_emit.note_change()",
            "closed_emit.note_change()",
        ] {
            assert!(
                body.contains(probe),
                "连接 relay 缺 `{probe}` —— emit 必须经闸门合并/记账，漏 mark 会让定时器退化成忙转"
            );
        }
        // 闸门必须在**门关的 topic** 上也 mark（否则 pending 永不清 → 忙转）。
        assert!(
            body.contains("if gate.topic_open(Topic::Connections) {")
                && body.contains("if gate.topic_open(Topic::Detail) {")
                && body.contains("if gate.topic_open(Topic::Closed) {"),
            "每条投影 emit 前须各自看自己的订阅门（只订了拓扑就别推全量明细 JSON）"
        );
    }

    /// 🟡 **变异锁：断流期的兜底实况回读周期恒为 [`PARK_RECHECK_INTERVAL`]。**
    ///
    /// 窗口隐藏时连接流已断开，[`StreamGate::wait_until`] 停在那里靠定期回读窗口实况兜底
    /// （Tauri 2 无 show/hide 事件）。这个周期若被调快（比如顺手改成 [`AGGREGATE_EMIT_MIN_INTERVAL`]
    /// 好让恢复更快），隐藏态下就会按 4Hz 空转：每次一把 registry 锁 + 一次投给主线程的可见性回读
    /// —— 降流门省下的电又烧回去，而这正是最容易顺手做坏的一处。
    ///
    /// 恢复速度**不靠**调快它：门变更（`epoch` bump）才是立刻唤醒的那条腿，本兜底只管「事件丢了」。
    ///
    /// 判据是**回读次数**（每轮循环调一次可见性源），虚拟时钟下确定。
    /// **变异探针**：`timeout(PARK_RECHECK_INTERVAL, ..)` 改成 `timeout(AGGREGATE_EMIT_MIN_INTERVAL, ..)`
    /// ⇒ 10 个周期内从 ~11 次涨到 ~41 次 ⇒ 转红。
    #[tokio::test(start_paused = true)]
    async fn 断流期回读周期不跟随emit间隔() {
        let (state, mut gate, visible) = test_stream_gate();
        state
            .registry
            .lock()
            .unwrap()
            .subscribe(Topic::Connections, "main");
        visible.store(false, Ordering::Relaxed);

        let probes = Arc::new(AtomicU64::new(0));
        let counted = {
            let probes = probes.clone();
            let flag = visible.clone();
            move || {
                probes.fetch_add(1, Ordering::Relaxed);
                flag.load(Ordering::Relaxed)
            }
        };
        // 隐藏 10 个兜底周期：门永不开，`wait_until(true, ..)` 必然超时。
        assert!(
            tokio::time::timeout(PARK_RECHECK_INTERVAL * 10, gate.wait_until(true, &counted))
                .await
                .is_err(),
            "隐藏态必须一直保持断流"
        );
        let n = probes.load(Ordering::Relaxed);
        assert!(
            n <= 12,
            "隐藏 10 个兜底周期内最多 ~11 次实况回读，实得 {n} —— 回读跟随了 emit 间隔即降流失效"
        );
    }

    /// 🟡 **取值区间锁**：钉区间而非具体数字 —— 调参可以，但不许滑回 1s，也不许滑到过激。
    ///
    /// **本批换了下界的判据，区间数字未动，理由如实登记**：
    /// - 旧下界（250ms）撑在「每拍一次含 ≤1000 条死连接的全量表拉取，成本在签名去重上游、
    ///   随节拍线性上涨」上。长驻流下**这段成本整个消失**（一次订阅一帧全量，此后只有增量），
    ///   那条理由随之作废。
    /// - 新下界撑在**渲染侧**：每次 emit 都要 O(n log n) 聚合 + 过 IPC + 渲染端重排整张拓扑图，
    ///   而拓扑节点的出现/消失在 250ms 与 100ms 之间没有可分辨差异 —— `.link` / `.node` 的
    ///   opacity 过渡本身就是 160ms（`ui/src/styles/components.css:224`），比 100ms 还长。
    ///   再快只是让渲染端多做功，用户一帧都多看不到。
    /// - 上界（350ms）判据未变：再慢就退回「反应了一下」的观感。
    ///
    /// **变异探针**：改回 `from_secs(1)` ⇒ 转红；改成 `from_millis(50)` / `from_millis(16)` ⇒ 转红。
    #[test]
    fn aggregate_emit间隔取值区间() {
        assert!(
            AGGREGATE_EMIT_MIN_INTERVAL >= Duration::from_millis(200),
            "拓扑 emit 间隔过激（{AGGREGATE_EMIT_MIN_INTERVAL:?}）：每次 emit 都是一次全表聚合 + IPC + \
             渲染端重排拓扑图，而 200ms 以下对一张离散变化的图没有可感知增量（连线过渡本身就 160ms）"
        );
        assert!(
            AGGREGATE_EMIT_MIN_INTERVAL <= Duration::from_millis(350),
            "拓扑 emit 间隔过慢（{AGGREGATE_EMIT_MIN_INTERVAL:?}）：滑回秒级即退回「反应了一下」的观感"
        );
    }

    /// 可见性未变时不得 bump 门代次（否则每拍的实况回读会白唤醒三条 poller）。
    #[test]
    fn 可见性未变不bump门代次() {
        let state = StreamGateState::new();
        let rx = state.epoch.subscribe();
        state.set_window_visible(true); // 与缺省同值 → no-op
        assert!(!rx.has_changed().unwrap(), "同值写入不得 bump");
        state.set_window_visible(false);
        assert!(
            rx.has_changed().unwrap(),
            "真变化必须 bump（唤醒 park 的 poller）"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════
    // 可见性缓存（M6/L13）：poller 只读缓存，回读跑在主线程
    // ══════════════════════════════════════════════════════════════════════════

    /// 缓存缺省必须是「可见」——与 getter 报错时的兜底方向一致（宁可多流一拍，绝不饿死 UI）。
    /// 首拍发生在第一次主线程刷新落地之前，缺省若是 false，订阅即出首帧那条语义就断了。
    #[test]
    fn visibility_cache_defaults_to_visible() {
        let state = StreamGateState::new();
        assert!(state.vis.visible.load(Ordering::Relaxed));
    }

    /// 回读成功 → 写缓存 + 同步进降流门（变了才 bump，park 中的 poller 由此立刻醒）。
    #[test]
    fn visibility_probe_ok_updates_cache_and_gate() {
        let state = StreamGateState::new();
        let rx = state.epoch.subscribe();

        state.apply_visibility_probe(Ok(false));
        assert!(!state.vis.visible.load(Ordering::Relaxed));
        assert!(
            rx.has_changed().unwrap(),
            "可见性真变化必须 bump 门代次（否则恢复要等满一拍）"
        );
        assert!(
            !state.registry.lock().unwrap().window_visible(),
            "缓存与降流门必须是同一个真值，不能只写缓存"
        );

        state.apply_visibility_probe(Ok(true));
        assert!(state.vis.visible.load(Ordering::Relaxed));
        assert!(state.registry.lock().unwrap().window_visible());
    }

    /// 🟡 **回读报错 → 兜底「可见」+ 计数**（失败方向失败安全，但**不能静默**）。
    ///
    /// **变异探针**：把错误分支改成「保持上一个值」/ 兜底成 false ⇒ 第一条断言转红；
    /// 把 `error_streak` 计数删掉 ⇒ 第二条转红。
    #[test]
    fn visibility_probe_error_falls_back_to_visible_and_counts() {
        let state = StreamGateState::new();
        state.apply_visibility_probe(Ok(false)); // 先进入「不可见」
        assert!(!state.vis.visible.load(Ordering::Relaxed));

        state.apply_visibility_probe(Err("is_visible: boom".into()));
        assert!(
            state.vis.visible.load(Ordering::Relaxed),
            "回读失败必须兜底成「可见」——宁可多流一拍，绝不误把还在屏上的 UI 饿死"
        );
        assert_eq!(state.vis.error_streak.load(Ordering::Relaxed), 1);

        state.apply_visibility_probe(Err("is_minimized: boom".into()));
        assert_eq!(state.vis.error_streak.load(Ordering::Relaxed), 2);
        // 一次成功即复位（连续失败才是「平台性失效」的信号）。
        state.apply_visibility_probe(Ok(true));
        assert_eq!(state.vis.error_streak.load(Ordering::Relaxed), 0);
    }

    /// 限频告警：既不能只发一条（后续被淹 ⇒ 降流整体失效零可观测），也不能每拍都发。
    #[test]
    fn visibility_failure_warns_at_a_decaying_rate() {
        assert!(should_warn_visibility_failure(1), "首次必须告警");
        assert!(!should_warn_visibility_failure(2));
        assert!(!should_warn_visibility_failure(9));
        assert!(should_warn_visibility_failure(10));
        assert!(should_warn_visibility_failure(100));
        assert!(!should_warn_visibility_failure(101));
        assert!(
            should_warn_visibility_failure(1000),
            "持续失效必须周期性再喊 —— 只喊一次等于没监控"
        );
        assert!(should_warn_visibility_failure(5000));
        assert!(!should_warn_visibility_failure(5001));
        // 三条 poller 每秒合计约六拍 ⇒ 若每次都发，一分钟就是 360 条。
        let noisy = (1..=600u64)
            .filter(|n| should_warn_visibility_failure(*n))
            .count();
        assert!(noisy <= 3, "600 次失败内最多 3 条告警，实得 {noisy}");
    }

    /// 🟡 **调用点守卫：两条 relay 都不得直接碰窗口 getter。**
    ///
    /// 窗口 getter 是「投消息进主事件循环 + 阻塞等回包」；主循环被原生模态 / 提权框占住时，
    /// 两条 relay 会同时把两个 tokio worker 挂死在 `recv` 上。
    ///
    /// **变异探针**：在任一 relay 里把 `visibility_source(...)` 换回 `|| main_window_visible(&app)`
    /// 之类的直读 ⇒ 转红；把回读从 `run_on_main_thread` 里挪出来 ⇒ 也转红。
    #[test]
    fn pollers_never_touch_window_getters_directly() {
        let src = include_str!("stats.rs");
        for f in [
            "async fn run_connections_stream(",
            "async fn run_stats_stream(",
        ] {
            let body = crate::commands::guard_scan::top_level_fn_body(src, f);
            assert!(
                body.contains("visibility_source(gate.state.clone()"),
                "{f} 没走缓存式可见性源"
            );
            for getter in ["is_visible(", "is_minimized(", "get_webview_window("] {
                assert!(
                    !body.contains(getter),
                    "{f} 里出现了阻塞式窗口 getter `{getter}` —— 主循环被模态占住时会挂死 tokio worker"
                );
            }
        }
        // 唯一允许调窗口 getter 的地方：投给主线程执行的那个闭包。
        // 只数**生产段**（测试模块里也会出现这个字面量）。
        let prod = &src[..src.find("\n#[cfg(test)]\nmod tests {").expect("锚点消失")];
        assert_eq!(
            prod.matches("probe_main_window_visible(").count(),
            2,
            "窗口回读的调用点应恰为「定义 1 + 主线程闭包 1」——多出来的那个多半是又在别处直读了"
        );
        let refresh = method_body(src, "    fn spawn_visibility_refresh(");
        assert!(
            refresh.contains("run_on_main_thread"),
            "可见性回读必须投给主线程执行（否则调用方要阻塞等主循环回包）"
        );
        assert!(
            refresh.contains("probe_main_window_visible("),
            "回读必须在投给主线程的那个闭包**里面**（挪到闭包外就又是跨线程阻塞了）"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════
    // 订阅来源（L14）：只有主窗可订阅
    // ══════════════════════════════════════════════════════════════════════════

    /// 🟡 **变异锁：非主窗的订阅一律拒绝。**
    ///
    /// 降流门的可见性只看主窗 → 非主窗的订阅会在主窗隐藏时被整体 park 掉（注册了但永远收不到帧，
    /// 且完全静默）。**变异探针**：把判据改成恒 true / 改成「非空即可」⇒ 转红。
    #[test]
    fn only_the_main_window_may_subscribe_to_stats() {
        assert!(accepts_stats_subscription(MAIN_WINDOW_LABEL));
        for other in ["tray", "update-popup", "main2", "", "Main"] {
            assert!(
                !accepts_stats_subscription(other),
                "label={other:?} 的订阅必须被拒绝：它会在主窗隐藏时被饿死，且没有任何信号"
            );
        }
    }

    /// 🟡 **调用点守卫：label 闸必须在真正登记订阅之前。**
    ///
    /// 登记之后再判等于白判（订阅已进注册表、poller 已被起起来）。
    #[test]
    fn subscribe_rejects_foreign_labels_before_registering() {
        let src = include_str!("stats.rs");
        let body = method_body(src, "    pub fn subscribe(");
        let gate_at = body
            .find("accepts_stats_subscription(window_label)")
            .expect("非主窗订阅闸被删了 —— 将来给浮层接订阅会表现为「数据时有时无」而非立刻报错");
        let register_at = body
            .find("reg.subscribe(")
            .expect("锚点消失：守卫已失去判据");
        assert!(
            gate_at < register_at,
            "label 闸必须在 `reg.subscribe(...)` **之前**（登记后再判等于没判）"
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// 真机验证（BUG-1 aggregate relay 数据面）—— `#[ignore]`，需 POLARIS_SINGBOX_PATH。
//
//   POLARIS_SINGBOX_PATH=<某个可用的 sing-box 二进制路径> \
//     cargo test -p polaris --bin polaris -- --ignored --nocapture real_core_aggregate
//
// 走**真核 + 真 h2c gRPC + 真连接**，验证 relay 的实际路径：
//   proxy.start(config)（BUG-2：真配置起真核）
//   → first_connection_snapshot（复用热切换批的首帧快照）
//   → build_aggregate（daemon::Connection → 聚合，死连接过滤）
//   → signature_changed（change-driven 去重）。
//
// 安全硬约束（对齐 proxy.rs 真机测试）：config 恒 manual + 全局直连 + 仅 127.0.0.1 混合入站
// → 不接管系统网络、无 TUN、无系统代理；流量只打本地回显服务器（不出网）。
// ══════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod real_core_tests {
    use super::*;
    use crate::runtime::helper::HelperRuntime;
    use crate::runtime::mesh::MeshRuntime;
    use crate::runtime::proxy::ProxyRuntime;
    use polaris_singbox_grpc::{Endpoint, SingBoxApiClient};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// 真机验证用最小 config：manual + 全局直连 + 仅本地混合入站。
    fn local_only_config(mixed: u16) -> Value {
        serde_json::json!({
            "servers": [],
            "selectedServerId": "__direct__",
            "proxyMode": "direct",
            "proxyModeType": "manual",
            "mixedPort": mixed,
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "真机验证：需 POLARIS_SINGBOX_PATH 指向真实 sing-box；非 CI 门"]
    async fn real_core_aggregate_relay_emits_real_frames() {
        std::env::var("POLARIS_SINGBOX_PATH").expect(
            "真机验证需 POLARIS_SINGBOX_PATH 指向真实 sing-box（前置缺失即失败，不静默跳过）",
        );

        let dir = std::env::temp_dir().join(format!(
            "polaris-agg-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        crate::logging::init(&dir);
        let config = Arc::new(ConfigManager::new(dir.clone()));
        let helper = Arc::new(HelperRuntime::never_installed_for_tests(dir.clone()));
        let mesh = Arc::new(MeshRuntime::new(dir.clone()));
        // 系统代理清理收口器：真实控制器 + 临时目录 marker 路径（无 marker → 门控 1 即返、零系统调用）。
        let proxy_clearer: Box<dyn crate::runtime::proxy::SystemProxyClearer> =
            Box::new(polaris_system_integration::production_proxy_controller(
                dir.join(polaris_system_integration::PROXY_MARKER_FILENAME)
                    .to_string_lossy()
                    .into_owned(),
            ));
        let proxy = Arc::new(ProxyRuntime::new(
            config,
            helper,
            mesh,
            proxy_clearer,
            // C11：真机验证用不到 DoH 竞速（本地 direct config，无节点域名）→ 桩即可。
            Arc::new(crate::runtime::proxy::NoNetworkDoh),
        ));

        let mixed = free_port();

        // ── BUG-2：真配置起真核（proxy.start(config: Value) → running）──────────────
        let st = proxy
            .start(local_only_config(mixed))
            .await
            .expect("[BUG-2] proxy.start(config) 起核应成功");
        println!(
            "[BUG-2] proxy.start(config) → running={} pid={} mixedPort={} apiPort={}",
            st.running, st.pid, st.mixed_port, st.clash_api_port
        );
        assert!(st.running, "[BUG-2] 起核后必须 running");
        assert_ne!(st.pid, 0, "[BUG-2] 必须拿到真实 pid");
        assert_ne!(st.clash_api_port, 0, "[BUG-2] 管理 API 端口必须已解析");

        // ── 造真实连接：本地服务器（仅 127.0.0.1，不出网），**延迟 10s 才响应** →
        //    请求已发、响应未回，连接在整个窗口内确定活跃（对齐「首页有活连接」的稳态场景）。──
        let srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let srv_port = srv.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = srv.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = s.read(&mut buf).await;
                    tokio::time::sleep(Duration::from_secs(10)).await; // 延迟响应 → 连接持续活跃
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                        .await;
                });
            }
        });
        let mut holds = Vec::new();
        for _ in 0..3 {
            let mut c = tokio::net::TcpStream::connect(("127.0.0.1", mixed))
                .await
                .expect("混合入站应可连");
            let req = format!(
                "GET http://127.0.0.1:{srv_port}/ HTTP/1.1\r\nHost: 127.0.0.1:{srv_port}\r\n\r\n"
            );
            c.write_all(req.as_bytes()).await.unwrap();
            holds.push(c); // 持有 + 不等响应 → 连接保持活跃
        }
        // 给 sing-box 一点时间把连接登记进管理面。
        tokio::time::sleep(Duration::from_millis(500)).await;

        // ── BUG-1：走 relay 的真实路径（snapshot → build_aggregate → signature_changed）──
        let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", st.clash_api_port), "")
            .await
            .expect("[BUG-1] 管理 API gRPC 连接应成功");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        let mut agg = None;
        while tokio::time::Instant::now() < deadline {
            let conns = client
                .first_connection_snapshot()
                .await
                .expect("[BUG-1] 连接快照应成功");
            let alive = conns.iter().filter(|c| c.closed_at <= 0).count();
            eprintln!(
                "[poll] first_connection_snapshot → {} conns（活跃 {alive}）",
                conns.len()
            );
            let a = build_aggregate(&conns, now_ms());
            if a.total > 0 {
                agg = Some(a);
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let agg =
            agg.expect("[BUG-1] relay 必须从真核聚合出 total>0 的真实帧（否则数据面仍无供数）");
        println!(
            "[BUG-1] build_aggregate(真核快照) → {}",
            serde_json::to_string(&agg).unwrap()
        );
        assert!(agg.total > 0, "[BUG-1] 真实连接总数必须 > 0");
        assert!(!agg.hosts.is_empty(), "[BUG-1] 至少一个真实 host 节点");

        // change-driven：首帧必推，同内容去重不推。
        let sig1 = signature_changed(&agg, &None).expect("[BUG-1] 首帧必推（emit）");
        assert!(
            signature_changed(&agg, &Some(sig1)).is_none(),
            "[BUG-1] 同内容必须去重不推（change-driven）"
        );
        println!("[BUG-1] change-driven：首帧 emit + 同内容去重 ✓");

        // ── 停核干净 ──────────────────────────────────────────────────────────
        drop(holds);
        let pid = st.pid;
        proxy.stop().await.expect("停核应成功");
        assert!(!proxy.status().running, "停核后 running 必须为 false");
        println!("[done] proxy.stop() → running=false（pid={pid} 已收割）");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
