//! 解锁检测运行时：命令编排层（把纯逻辑 `polaris-unlock` crate 接成 run/get/快照/事件的生产路径）。
//!
//! # 这条接线守的是 §K7.1「两扇门之间的缝」
//!
//! `crates/unlock/` 有完整的检测器（`detect_all`/`run_checker`/`probe_egress`，全 mock-testable，
//! 有单测门）；`runtime/http.rs` 的 `UnlockHttp` 生产实现有真 socket 门。**但没有任何生产代码把
//! 「命令 → 编排 → 事件」这条路接起来** —— `commands/unlock.rs` 曾是 stub，`unlock:run` 返回空对象、
//! `unlock:get` 返回 null。检测器再全，前端也恒是灰徽章（与 §O1「数据面 aggregate 无人 emit」同族：
//! 事件常量在、无人 emit → UI 恒收 null）。本模块就是那条缺失的接线，且**必须由组合面门覆盖**：
//! 真 `UnlockHttp` 注入 → `run` 真调 → 快照真存 → 事件真 emit（见 `#[cfg(test)]`）。
//!
//! # 编排职责（上游 `UnlockDetectionService` 剥离 electron 壳后的应用层）
//!
//! 纯逻辑 crate 刻意不移植的编排策略（见 `crates/unlock/src/detector.rs` 模块文档「不移植」清单），
//! 在此重建。四个淬火不变式（`上游-unlock-4bug-fix.md`，registry 维度 7）逐条落：
//!
//! 1. **TTL + warm 补测**（#65/#6）：快照带 TTL（含 timeout 且非受限 → 2min 自然重检兜底；否则 30min）；
//!    partial-timeout 提交后 5s 定向重打 timeout 项（[`UnlockRuntime::run_recheck`]，epoch 守卫，invalidate 取消）。
//! 2. **出口归属 bracket**（#7）：轮首/轮尾各探一次 egress，不符=契约外翻转→丢弃+invalidate；
//!    并行地，commit 前校验 `epoch == epoch0`（有并发 invalidate 则 epoch 已变）→ 丢弃。
//!    **决不把 A 出口的结果标给 B 出口**。丢弃腿排的自跑由 [`MAX_CONSECUTIVE_DRIFT`] 熔断封顶
//!    （连续漂移 N 轮 → 落低置信终态、停止再排程），否则出口持续漂移 = 无界自持循环 + UI 永钉「检测中」。
//! 3. **invalidate 契约**（#7）：[`UnlockRuntime::invalidate`] 递增 epoch + 清缓存 + 广播 `{running,exitBlocked}`。
//!    停代理时 [`UnlockRuntime::peek`] 也自证失效（`unlock_get` 见 command 层）。
//! 4. **受限地区收敛**（#8）：出口 region ∈ `RESTRICTED_EGRESS_REGIONS`（CN）时，全超是结构性预期，
//!    按高置信终态收敛——不置 `low_confidence`、不 warm 补测、用正常 30min TTL（不 2min churn）。
//!
//! # 出口 pin
//!
//! 检测须走**用户当前分流出口**（否则测的是本机直连，无意义）。command 层用
//! [`HttpRuntime::via_local_proxy`](crate::runtime::http::HttpRuntime::via_local_proxy) 建经本机 mixed
//! 端口的客户端注入 [`UnlockRuntime::run`] —— 即 上游 `ensureFetch` 的 socks5 session pin 的等价物。
//! 本模块的 `run` 对 http 是注入无关的（`H: UnlockHttp`），故单测用 mock、生产用 pin 客户端，同一条编排。
//!
//! # headers 透传（CF 挑战判据不丢）
//!
//! HTTP 批给 `UnlockResponse` 加了 `headers`（`cf-mitigated` 是 CF 挑战主判据）。本编排层**不读也不动**
//! headers —— 它只调 `run_checker`/`probe_egress`，headers 由 checker（现在的判定 + #29 的 challenge.rs）
//! 消费。透传是自动的：`HttpRuntime::request` 填 headers → checker 读。本层不在中间截断，故不丢。
//!
//! # Restricted 变体前向兼容
//!
//! #29 可能给 `UnlockStatus` 加 `Restricted` 变体。本模块**不穷举 match** `UnlockStatus`：只用
//! `r.status == UnlockStatus::Timeout` 等值比较判「是否 timeout」，新变体自然落「非 timeout」，
//! 不炸编译、不误计数。加变体无需改本文件。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::AppHandle;

use polaris_unlock::detector::{is_restricted_egress_region, probe_egress};
use polaris_unlock::endpoints::CHECKER_BUDGET_MS;
use polaris_unlock::{
    run_checker, ServiceId, UnlockBlockedReason, UnlockEgress, UnlockHttp, UnlockProgress,
    UnlockResult, UnlockSnapshot, UnlockStatus,
};

use crate::events::broadcast;
use crate::events::channel::{
    EVENT_UNLOCK_INVALIDATED, EVENT_UNLOCK_PROGRESS, EVENT_UNLOCK_UPDATED,
};
use crate::runtime::http::HttpRuntime;

/// 新鲜快照缓存 TTL（30min）。对齐 上游 `EGRESS_CACHE_TTL_MS`。
const FRESH_TTL_MS: u64 = 30 * 60 * 1000;

/// 含 timeout 的快照 TTL（2min）。对齐 上游 `TIMEOUT_TTL_MS`——2min 后自然重检兜底，
/// 不让一次冷隧道 timeout 锁死 30min。**受限地区不走这条**（收敛，用 FRESH_TTL）。
const TIMEOUT_TTL_MS: u64 = 2 * 60 * 1000;

/// warm 补测延时（5s）。对齐 上游 `RECHECK_DELAY_MS`——等隧道热起来再定向重打 timeout 项。
/// **真机需调**（可能 3s 够）；由 command 层调度。
pub const WARM_RECHECK_DELAY_MS: u64 = 5_000;

/// **force 硬下限**（item 5，上游 `FORCE_MIN_MS`）：force 绕 TTL，但仍防手点连发触发对端限频。
/// **FX-ui 已加前端 15s 冷却灰态；本常量是后端硬下限**，双保险对齐（脚本/自动化绕过前端仍受此限）。
const FORCE_MIN_MS: u64 = 15_000;

/// **就绪门退避 schedule**（item 2，上游 `READINESS_BACKOFF_SCHEDULE_MS`）：核刚 running 时 mixed inbound
/// 尚未真正路由 → egress trace 探针会失败。首次即时探（核已就绪则零延迟），失败按此退避重试。前 3 攻 1.2s
/// （冷启动常态 <4s 就绪），后 3 攻拉长（+4/+4/+8s）吸收慢起窗。attempt n 的退避 = `schedule[n-1]`。
const READINESS_BACKOFF_SCHEDULE_MS: &[u64] = &[1200, 1200, 1200, 4000, 4000, 8000];

/// 就绪门最大攻数（item 2，上游 `READINESS_MAX_ATTEMPTS`）= schedule 长度 + 1（首攻即时探 + 6 次退避 = 7）。
const READINESS_MAX_ATTEMPTS: usize = READINESS_BACKOFF_SCHEDULE_MS.len() + 1;

/// **B1 自适应就绪确认**（item 2，上游 `READINESS_CONFIRM_MS`）：疑似 flap（曾失败过）时，成功探测后追加
/// 1 次确认探（此间隔后连续 2 成才判就绪）；首攻即成（健康路径）零代价直接就绪，不伤「连上即点亮」体感。
const READINESS_CONFIRM_MS: u64 = 1200;

/// **轮内 settle-retry 最大轮数**（item 4，上游 `SETTLE_RETRY_MAX_ROUNDS`）。
const SETTLE_RETRY_MAX_ROUNDS: u64 = 2;

/// **轮内 settle-retry 退避基数**（item 4，上游 `SETTLE_RETRY_BACKOFF_MS`）：第 n 轮退避 = n × 此值
/// （2s→4s，隧道进一步热）。首轮个别 checker 撞冷隧道 8s 超时 = 低置信瞬态，不与命中 marker 的高置信结果同权落定。
const SETTLE_RETRY_BACKOFF_MS: u64 = 2_000;

/// **整轮检测 wall-clock 硬上限**（上游 `TOTAL_DETECTION_BUDGET_MS`，`UnlockDetectionService.ts:78`）：
/// **就绪门 + checker 主轮 + settle-retry 共享一条 deadline**，非各段独立预算加法累加。加法累加的旧行为
/// 最坏 ≈ 就绪门 19.6s + checker 15s + settle-retry 6s ≈ 40s+（上游 同形态旧行为 ≈127s），用户实测
/// 「总超时不生效」。deadline 本身即上限：
/// - 就绪门每次退避/探测前判 deadline，单次探测按剩余收紧，耗尽 → notReady（不空等）；
/// - checker 主轮 + settle-retry 的单 checker 截止点 = `min(CHECKER_BUDGET_MS, 剩余)`；
/// - settle-retry 退避若跨 deadline 直接停（保留已有终态）。
///
/// **值经真机反馈定为 10s**（陈先生 2026-07-13：慢节点检测 ≤10s 比较合理）——本仓迁移时漏移植该值，
/// 分段常量按 上游 **修复前**版本抄了回来，等于把用户已反馈过的回归搬了过来。此处照搬 10s，不另定值。
/// 慢隧道超预算落 notReady/timeout，靠后续 invalidate 自跑（[`UnlockEventSink::schedule_self_run`]）恢复。
pub const TOTAL_DETECTION_BUDGET_MS: u64 = 10_000;

/// 单次网络操作在 deadline 逼近时的最小配额（上游 `MIN_OP_BUDGET_MS`）：防「按剩余收紧」算出 0/负值的
/// 退化请求。代价是整轮最多超出 deadline 此值——换来「每个 checker 都拿得到一个真实终态」。
const MIN_OP_BUDGET_MS: u64 = 500;

/// **invalidate 后主进程侧自跑去抖窗**（上游 `UNLOCK_SELF_RUN_DEBOUNCE_MS`，`index.ts:1772`）。
///
/// 起代理会连发多条 invalidate（起核就绪 + 切节点 + 热切换…），去抖把这一串合并成**一轮**检测。
/// 语义与 上游 逐字对齐：**每次 invalidate 重置计时**，只有静默满 1500ms 的那一次真正开跑。
pub const SELF_RUN_DEBOUNCE_MS: u64 = 1_500;

/// **出口漂移连击熔断阈值**：连续 N 轮「轮首/轮尾 egress 不符」丢弃 → 停止再排自跑，改落低置信终态。
///
/// # 没有这道熔断会怎样（本常量存在的唯一理由）
///
/// 漂移丢弃腿调 [`UnlockRuntime::invalidate`]，而 invalidate 会排一轮 [`SELF_RUN_DEBOUNCE_MS`] 后的自跑；
/// 那一轮重新探测、再次漂移、再次丢弃 —— **永不收敛**。每次迭代都是一整个 [`TOTAL_DETECTION_BUDGET_MS`]
/// 预算的真实网络流量（6 个解锁端点 + 2 次 CF trace），且每次 invalidate 广播 `{running:true}` →
/// 前端 `App.tsx` 调 `beginUnlockCheck()` ⇒ **UI 永久钉在「检测中」**。
///
/// 触发条件不是边角：任何负载均衡 / urltest / WARP / 多 IP 出口，只要出口 IP 轮换快过一轮检测即可。
/// 迁移时曾按「与 上游 同构、不加熔断」放行，本轮据上述具体机制推翻——上游 同构不等于 上游 没这个洞。
///
/// # 为什么是「熔断」而不是「放宽漂移判据」
///
/// 放宽判据（按 /24 比对、只比 region…）会削弱 §K7.1 的核心不变式「**决不把 A 出口的结果标给 B 出口**」。
/// 熔断不碰判据：前 N-1 轮照旧丢弃 + 重跑（快速漂移多半是瞬态，一两轮就稳），只有**持续**漂移才承认
/// 「这个出口在本轮时间尺度上没有稳定 IP」并落终态。归属不变式全程不破 —— 终态的 `egress` 置 `None`
/// （不标给任何出口），只如实告诉 UI「测到了这些值，但出口在抖，低置信」。
///
/// # 不是永久闩锁
///
/// 熔断落的终态是 `low_confidence` ⇒ 按既有规则**不入 TTL 缓存**，且落定即把连击计数清零。
/// 下一次真触发（起停 / 切节点 / 用户 force）照常重检，只是不再有「自己排给自己」的自持循环。
const MAX_CONSECUTIVE_DRIFT: u64 = 3;

/// 缓存的快照 + 其 TTL 记账。
struct Cached {
    snapshot: UnlockSnapshot,
    stored_at_ms: u64,
    ttl_ms: u64,
}

/// 解锁 gating 短路判定（**SoT**，命令层唯一入口）。1:1 上游 `UnlockDetectionService.run` 的 gating 段：
/// - 核未运行 / 无 mixed 入站 → `ProxyNotRunning`（不发起检测、不缓存）；
/// - 选中 TS 出口直判无效（`exit_blocked`，见 [`crate::runtime::tailscale_status::selected_ts_exit_blocked`]）
///   → `ExitInvalid`（经死出口检测只会空转就绪门数十秒 → 短路，零网络零就绪门）。
///
/// 优先级 `ProxyNotRunning > ExitInvalid`（无代理谈不上出口有效性），对齐 Polaris gate 顺序
/// （`isRunning` 先于 `getExitBlock`）。返回 `None` = 放行，进真检测。
#[must_use]
pub fn unlock_gate_reason(
    running: bool,
    mixed_port: u16,
    exit_blocked: bool,
) -> Option<UnlockBlockedReason> {
    if !running || mixed_port == 0 {
        return Some(UnlockBlockedReason::ProxyNotRunning);
    }
    if exit_blocked {
        return Some(UnlockBlockedReason::ExitInvalid);
    }
    None
}

/// `EVENT_UNLOCK_INVALIDATED` 载荷（对齐前端 `UnlockInvalidatedPayload`：`{running,exitBlocked}`）。
///
/// 由**主进程**带上核真态，供渲染端决定「显检测中 vs 复位 idle」（invalidate 常先于 STARTED 抵达，
/// 渲染端视图可能陈旧）。`rename_all` 对齐前端 camelCase 契约。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct InvalidatedPayload {
    running: bool,
    exit_blocked: bool,
}

/// 解锁事件出口（注入抽象）。
///
/// 生产实现 [`BroadcastSink`] 经 `AppHandle` 广播 Tauri 事件；测试实现记录到 `Vec` 供断言——
/// 这样组合面门能证「事件真 emit」而无需 Tauri 运行时。对齐本仓「纯逻辑 + 注入 I/O」架构
/// （`events.rs` 是被注入的那一侧）。
pub trait UnlockEventSink {
    /// 单服务 settle 逐个点亮（`EVENT_UNLOCK_PROGRESS`）。
    fn progress(&self, service_id: &str, result: &UnlockResult);
    /// 一轮完成的完整终态快照（`EVENT_UNLOCK_UPDATED`）。
    fn updated(&self, snapshot: &UnlockSnapshot);
    /// 缓存失效（`EVENT_UNLOCK_INVALIDATED`）。
    fn invalidated(&self, running: bool, exit_blocked: bool);

    /// **invalidate 后的主进程侧去抖自跑**（上游 `scheduleUnlockSelfRun`，`index.ts:1774-1789`）。
    ///
    /// # 为何驱动层必须在这一侧
    ///
    /// 上游 源码 `index.ts:1808` 原文警告过这个坑：「GAP-1：invalidate 后主进程侧防抖自跑（**不依赖
    /// home 页挂载着的 renderer hook 发 IPC**）」。本仓迁移时只搬了 invalidate 的「作废 + 广播」半边，
    /// 把重跑责任交给了渲染端 hook，而该 hook 只有手动腿 ⇒ invalidate 把六个徽章置成「检测中」后**无人调
    /// run**，永久转圈。故驱动层落在此处（Rust 侧 = Electron 主进程的等价物），**不是**前端补 `useEffect`。
    ///
    /// # token 与去抖合并
    ///
    /// `token` 由 [`UnlockRuntime::invalidate`] 递增取得。实现方等 [`SELF_RUN_DEBOUNCE_MS`] 后须用
    /// [`UnlockRuntime::self_run_token_current`] 复核：token 已被后续 invalidate 顶掉 → 让位（不跑），
    /// 只有最后一次 invalidate 排的那一轮真正开跑。这就是「短时间内多次 invalidate 只跑一轮」。
    ///
    /// 默认实现 no-op：单测用的记录型 sink 无需真跑网络（也拿不到 `AppHandle`）。
    fn schedule_self_run(&self, token: u64) {
        let _ = token;
    }
}

/// 生产事件出口：经 `AppHandle` 广播给所有 webview。
pub struct BroadcastSink<'a> {
    handle: &'a AppHandle,
}

impl<'a> BroadcastSink<'a> {
    #[must_use]
    pub fn new(handle: &'a AppHandle) -> Self {
        Self { handle }
    }
}

impl UnlockEventSink for BroadcastSink<'_> {
    fn progress(&self, service_id: &str, result: &UnlockResult) {
        broadcast(
            self.handle,
            EVENT_UNLOCK_PROGRESS,
            UnlockProgress {
                service_id: service_id.to_string(),
                result: result.clone(),
            },
        );
    }

    fn updated(&self, snapshot: &UnlockSnapshot) {
        broadcast(self.handle, EVENT_UNLOCK_UPDATED, snapshot.clone());
    }

    fn invalidated(&self, running: bool, exit_blocked: bool) {
        broadcast(
            self.handle,
            EVENT_UNLOCK_INVALIDATED,
            InvalidatedPayload {
                running,
                exit_blocked,
            },
        );
    }

    /// 生产实现：spawn 一个去抖任务，静默满 [`SELF_RUN_DEBOUNCE_MS`] 且 token 未被顶掉 → 真跑一轮。
    ///
    /// gating（核未跑 / 出口直判无效）与出口 pin 全在
    /// [`run_unlock_cycle`](crate::commands::unlock::run_unlock_cycle) 内，与手动 `unlock:run` **同一条
    /// 编排** —— 自跑不是第二套逻辑，只是第二个触发源。`run(force=false)` 幂等：撞 gating 短路 = 零网络
    /// no-op，撞在飞轮 = `run_lock` 单飞串行后走 TTL 快路。
    /// ⚠️ **必须用 `tauri::async_runtime::spawn`，不能用 `tokio::spawn`**（2026-07-21 真机崩溃血证）。
    ///
    /// `tokio::spawn` 要求调用处**已在 Tokio runtime 上下文内**，否则 panic ⇒ Rust panic 在 Tauri IPC
    /// 回调里无处可catch ⇒ `abort()` ⇒ 整个应用崩溃。而 `invalidate` 的调用方**全是同步 command**
    /// （`server_switch` / `server_delete` / `server_delete_batch` / `subscription_delete` /
    /// `config_save` / `config_set_value`），Tauri 对 `pub fn`（非 `async fn`）command 是在**主线程**
    /// 直接调用的，**没有 runtime 上下文** ⇒ 切一次节点必崩，射程覆盖切/删节点、删订阅、存配置、改设置项。
    ///
    /// `tauri::async_runtime::spawn` 持有 Tauri 的全局 runtime handle，任意线程可调，仓内另有 21 处先例。
    ///
    /// **单测抓不到这个**：`#[tokio::test]` 自带 runtime 上下文，两种 spawn 在测试里行为一致、都能过。
    /// 唯一能在本层锁住的判据是源码扫描 —— 见本文件 `mod spawn_guard`。
    fn schedule_self_run(&self, token: u64) {
        let app = self.handle.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(SELF_RUN_DEBOUNCE_MS)).await;
            {
                use tauri::Manager;
                // setup 前极早期 / 关停中取不到 State → 静默放弃（绝不 panic，同 proxy.rs 的 try_state 范式）。
                let Some(rt) = app.try_state::<crate::runtime::AppRuntime>() else {
                    return;
                };
                if !rt.unlock.self_run_token_current(token) {
                    // 去抖合并：窗内又来了 invalidate → 由更晚那一轮负责跑，本轮让位。
                    log::debug!(
                        "解锁自跑：token {token} 已被后续 invalidate 顶掉 → 让位（合并为一轮）"
                    );
                    return;
                }
            } // State 守卫不跨 await（Tauri State 非 Send）。
            log::debug!(
                "解锁自跑：去抖窗静默满 {SELF_RUN_DEBOUNCE_MS}ms → 发起一轮检测（token {token}）"
            );
            if let Err(e) = crate::commands::unlock::run_unlock_cycle(app, false).await {
                // 真机 logLevel=warn ⇒ 此条必须 warn：自跑失败 = 前端「检测中」无人收口，正是卡住的形态。
                log::warn!("解锁自跑失败（前端可能停在检测中，需手动刷新）：{e}");
            }
        });
    }
}

/// 选中出口 identity 是否变化（A7 解锁缓存失效判准，**四写腿共用的唯一权威**）。
///
/// 判准 = `selectedServerId` 变；两侧皆 `Option<&str>`（`None` = 无选中 / 清除选中，如删光节点或订阅刷没了）：
/// - 旧 == 新（含两侧皆 `None`）→ 未变（`false`）：出口不动，旧解锁结果仍有效，不失效（防白刷探测）。
/// - 旧 != 新 → 变（`true`）：出口切走，旧结果作废。含三类变：
///   - 旧 `None` → 新 `Some`（首次选中）；
///   - 旧 `Some` → 新 `Some'`（换节点）；
///   - **旧 `Some` → 新 `None`（→null：删当前选中 / 订阅刷新令选中消失）** —— 也是出口变，必须失效
///     （否则解锁角标最长陈旧 30min，即缓存 `FRESH_TTL_MS`）。
///
/// 曾在 `commands/server.rs`（`exit_node_changed`，新值 `&str`）与 `commands/config.rs`（`selected_exit_changed`，
/// 两侧 `Option`）各有一份；本函数收敛为单一真值源，两处引用它（server 侧调用点包 `Some(new)`）。
#[must_use]
pub fn selected_exit_changed(old_selected: Option<&str>, new_selected: Option<&str>) -> bool {
    old_selected != new_selected
}

/// 解锁检测运行时（`State`-managed 单实例）。
///
/// 持有传输层单点（建出口 pin 客户端用，虽然 `run` 本身注入无关）+ epoch（归属 bracket）+ 快照缓存。
pub struct UnlockRuntime {
    /// 传输层单点（保留引用以备将来直建客户端；出口 pin 由 command 层经 `via_local_proxy` 建）。
    #[allow(dead_code)]
    http: Arc<HttpRuntime>,
    /// 归属世代：invalidate 递增，作废在飞轮的 commit（别把旧出口结果标给新出口）。
    epoch: AtomicU64,
    /// 最近一轮的终态快照（TTL 内 `unlock_get` 零网络水合）。
    cache: Mutex<Option<Cached>>,
    /// **单飞串行**（item 7，上游 `inflight`）：并发 `run`/`run_recheck` 经此互斥串行化——第二者等第一者
    /// commit 后走 TTL 快路（零网络往返），而非各跑一遍 6 checker。Rust 无法像 JS 那样存借用 `http` 的在飞
    /// future（其生命周期借栈），故以「持锁跑整轮」等价实现单飞：第二者阻塞至第一者释放，再命中新鲜缓存。
    run_lock: tokio::sync::Mutex<()>,
    /// 最近一次**提交**的终态快照（含 notReady / lowConfidence；与 TTL `cache` 分离——后者受 TTL 约束且
    /// lowConfidence 不入）。供 S-gate（item 2：notReady 终态非 force 不重扫）+ force 硬下限（item 5）读。
    /// 上游 `lastSnapshot`。invalidate 清空。
    last_snapshot: Mutex<Option<UnlockSnapshot>>,
    /// 最近一次真跑网络（就绪门 / checker 轮）的时刻（Unix ms）。force 硬下限据此判 <15s 连点（item 5）。
    /// 上游 `lastRunAt`。invalidate 归零。
    last_run_at: AtomicU64,
    /// **自跑去抖世代**（上游 `unlockSelfRunTimer` 的等价物）：每次 invalidate 递增，作为该次排程的 token。
    /// 定时器到点时 token 与当前值不符 = 窗内又来过 invalidate → 该次让位。这就是「多次 invalidate 合并成
    /// 一轮」的实现——用世代号取代 JS 的 `clearTimeout`（Rust 侧 spawn 出去的 sleep 无法从外部取消）。
    self_run_seq: AtomicU64,
    /// **出口漂移连击计数**（熔断器状态，见 [`MAX_CONSECUTIVE_DRIFT`]）：轮尾 egress 与轮首不符**且 epoch 未变**
    /// 时递增；任何落定终态（正常 commit / notReady commit / 熔断 commit）或「epoch 真变了」都清零。
    ///
    /// 刻意**不在 [`UnlockRuntime::invalidate`] 里清零** —— 漂移丢弃腿自己就调 invalidate，在那里清零会让
    /// 计数恒为 1、熔断永不触发，即「加了熔断却没有牙」。清零点只放在上面列的那几处。
    drift_streak: AtomicU64,
}

impl UnlockRuntime {
    #[must_use]
    pub fn new(http: Arc<HttpRuntime>) -> Self {
        Self {
            http,
            epoch: AtomicU64::new(0),
            cache: Mutex::new(None),
            run_lock: tokio::sync::Mutex::new(()),
            last_snapshot: Mutex::new(None),
            last_run_at: AtomicU64::new(0),
            self_run_seq: AtomicU64::new(0),
            drift_streak: AtomicU64::new(0),
        }
    }

    /// 当前自跑去抖世代（排程 token 的真值源）。
    #[must_use]
    pub fn self_run_seq(&self) -> u64 {
        self.self_run_seq.load(Ordering::SeqCst)
    }

    /// 该排程 token 是否仍是最新（否 = 去抖窗内又发生过 invalidate，本次排程应让位）。
    ///
    /// **去抖合并的判据单点**：[`UnlockEventSink::schedule_self_run`] 的实现方只调本函数，
    /// 不自己比大小——语义（含「相等才算最新」）由此处收口，单测直接锁这条。
    #[must_use]
    pub fn self_run_token_current(&self, token: u64) -> bool {
        self.self_run_seq() == token
    }

    /// 读最近提交的终态快照（S-gate / force-min 用；与 TTL `cache` 分离，无 TTL 约束）。
    fn last_snapshot(&self) -> Option<UnlockSnapshot> {
        self.last_snapshot.lock().ok().and_then(|g| g.clone())
    }

    /// 写最近提交的终态快照。
    fn set_last_snapshot(&self, snap: Option<UnlockSnapshot>) {
        if let Ok(mut g) = self.last_snapshot.lock() {
            *g = snap;
        }
    }

    /// 当前归属世代。
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// 递增世代，返回新值。
    fn bump_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// `unlock:get` —— 纯读 TTL 内的缓存快照，**零网络**。过期/无缓存 → None（前端水合复位）。
    ///
    /// 无需 epoch 校验：invalidate 已在切节点/起停时清缓存，故非空缓存恒是当前出口的合法结果。
    #[must_use]
    pub fn peek(&self, now_ms: u64) -> Option<UnlockSnapshot> {
        let guard = self.cache.lock().ok()?;
        let cached = guard.as_ref()?;
        if now_ms < cached.stored_at_ms.saturating_add(cached.ttl_ms) {
            Some(cached.snapshot.clone())
        } else {
            None
        }
    }

    /// **invalidate 契约**：切节点/起停 → 递增 epoch（作废在飞轮）+ 清缓存 + 广播 `{running,exitBlocked}`
    /// + **排一轮去抖自跑**。
    ///
    /// 由生命周期事件（proxy start/stop/热切换、server switch、订阅刷新、config 换出口）触发。
    ///
    /// # 这里是自跑的唯一汇聚点
    ///
    /// 自跑排程**不在各调用点逐个接线**，而是收口在本函数（对齐 上游：所有 `invalidate()` → `onInvalidated`
    /// 回调 → `scheduleUnlockSelfRun()`，`index.ts:1806-1809`）。好处是「新增一个 invalidate 触发点」自动获得
    /// 自跑，不会像本批修的缺陷那样出现「广播了失效、没人重跑」的半边移植。含 `run()` 内的出口漂移丢弃腿
    /// （经 [`Self::invalidate_keep_run_at`]）——那一轮结果被丢弃后必须有人重跑，否则前端停在检测中。
    ///
    /// # 自跑不会无界自持
    ///
    /// 「丢弃 → 排自跑 → 再丢弃」这条边是有界的：漂移丢弃腿由 [`MAX_CONSECUTIVE_DRIFT`] 熔断，连续 N 轮后
    /// 改落低置信终态且**不再经过本函数**（不排新的自跑）。本函数自身不设限流，边界由调用侧的丢弃腿承担。
    pub fn invalidate<S: UnlockEventSink>(&self, sink: &S, running: bool, exit_blocked: bool) {
        self.bump_epoch();
        if let Ok(mut guard) = self.cache.lock() {
            *guard = None;
        }
        // S-gate / force-min 内存态一并复位（切节点/起停 = 一切真状态变化的解除通道，对齐 上游 invalidate：
        // 清 lastSnapshot + lastRunAt）——否则 notReady 终态会锁死 S-gate、旧 lastRunAt 会误挡新出口的首次 force。
        self.set_last_snapshot(None);
        self.last_run_at.store(0, Ordering::SeqCst);
        sink.invalidated(running, exit_blocked);
        // 去抖自跑：先递增世代取 token，再交给 sink 排程。递增必须在 `schedule_self_run` **之前**——否则
        // 并发 invalidate 可能拿到相同 token，两轮都判「最新」而双跑。
        let token = self.self_run_seq.fetch_add(1, Ordering::SeqCst) + 1;
        sink.schedule_self_run(token);
    }

    /// 丢弃腿专用的失效：语义同 [`Self::invalidate`]，但**保留 `last_run_at`**。
    ///
    /// # 为什么丢弃腿不能沿用裸 `invalidate`
    ///
    /// `invalidate` 把 `last_run_at` 置 0，那是为「起停 / 切节点」设计的：真状态变了，旧的限流记账不该
    /// 再挡新出口的首次 force。但**丢弃腿不是状态变化，而是本轮真跑过一整轮网络**（就绪门 + 6 个 checker
    /// + 2 次 trace）。在这条腿上置 0 会同时击穿两道防连点闸门：
    ///  - 后端 [`FORCE_MIN_MS`] 硬下限的 `force && last_at != 0` 守卫失效 ⇒ 连点 force 全部放行；
    ///  - 丢弃腿不 emit UPDATED ⇒ 前端 `unlock.lastRunAt` 停在陈旧/null ⇒ `unlockCooldown`
    ///    （`HomeScreen.tsx` 由 `lastRunAt` 派生的 15s 灰态）永不武装。
    ///
    /// 于是在漂移出口上刷新按钮**两侧都不受限流**，而这**恰好是后端已在自跑的时候** —— 对端限频风险最高
    /// 的那一刻反而门户大开。保留 `last_run_at` 即恢复后端那道闸门；前端那道由熔断落终态时的 UPDATED
    /// （带 `checkedAt` ⇒ store 的 `lastRunAt` 得到更新）收口，见 [`MAX_CONSECUTIVE_DRIFT`]。
    fn invalidate_keep_run_at<S: UnlockEventSink>(
        &self,
        sink: &S,
        running: bool,
        exit_blocked: bool,
    ) {
        let ran_at = self.last_run_at.load(Ordering::SeqCst);
        self.invalidate(sink, running, exit_blocked);
        self.last_run_at.store(ran_at, Ordering::SeqCst);
    }

    /// 写缓存（commit 后）。
    fn store(&self, snapshot: UnlockSnapshot, stored_at_ms: u64, ttl_ms: u64) {
        if let Ok(mut guard) = self.cache.lock() {
            *guard = Some(Cached {
                snapshot,
                stored_at_ms,
                ttl_ms,
            });
        }
    }

    /// **编排核心**（注入 http/sink/clock，天然可单测；生产由 command 注入 `via_local_proxy` 出口 pin 客户端）。
    ///
    /// 流程：单飞持锁（item 7）→ TTL 快路（非 force）→ S-gate（item 2 notReady 终态非 force 不重扫）→
    /// force 硬下限（item 5）→ 就绪门退避（item 2：核起→路由前探针重试 7 次 + B1 flap）→ checker 主轮
    /// （item 3 每 checker `CHECKER_BUDGET_MS` 封顶，逐 settle emit progress）→ 轮内 settle-retry
    /// （item 4：timeout 项退避补测 ≤2 轮）→ 轮尾 egress 确认 → 归属 bracket（epoch + egress）→
    /// commit（受限收敛 + TTL 挂置信度 + 维护 lastSnapshot）→ emit UPDATED。
    ///
    /// **整轮共享 deadline** = [`TOTAL_DETECTION_BUDGET_MS`]（10s），见
    /// [`Self::run_with_budget`]。
    pub async fn run<H, S>(
        &self,
        http: &H,
        sink: &S,
        force: bool,
        now: impl Fn() -> u64,
    ) -> UnlockSnapshot
    where
        H: UnlockHttp + ?Sized,
        S: UnlockEventSink,
    {
        self.run_with_budget(http, sink, force, now, TOTAL_DETECTION_BUDGET_MS)
            .await
    }

    /// [`Self::run`] 的预算参数化版本。
    ///
    /// `budget_ms` = 整轮 wall-clock 硬上限，**就绪门 + checker 主轮 + settle-retry 共享**（非各段加法累加）。
    /// 生产恒用 [`TOTAL_DETECTION_BUDGET_MS`]；单测用它把预算调大/调小，分别验「预算足时全 7 攻退避可达」
    /// 与「预算耗尽时写终态而非挂着」——对应 上游「单测以冻结注入时钟绕过 deadline」的等价手法。
    ///
    /// # deadline 用 `tokio::time::Instant` 而非注入的 `now`
    ///
    /// 注入的 `now`（生产 `unix_millis`）是**打戳用的墙钟**，单测里常冻结成常量（`|| 1_000`）。而所有真正
    /// 耗时的动作（退避 sleep、checker 超时）走的是 `tokio::time`，`start_paused` 下是虚拟时钟。deadline 必须
    /// 跟这些动作同一条时间轴，否则单测里 deadline 永不到点（虚拟时钟推进了，墙钟没动）= 假绿。
    ///
    /// # 「到点必须写终态」
    ///
    /// deadline 不是「到点就撒手」：每个 checker 的截止点取 `min(CHECKER_BUDGET_MS, 剩余)` 但**不低于**
    /// [`MIN_OP_BUDGET_MS`]，超时落 [`UnlockStatus::Timeout`] —— 即 deadline 到点时每个服务都拿得到一个真实
    /// 终态、快照照常 commit + emit。就绪门耗尽则提交 `notReady` 终态。**绝不留「检测中」挂着**，那正是本批修的缺陷形态。
    pub async fn run_with_budget<H, S>(
        &self,
        http: &H,
        sink: &S,
        force: bool,
        now: impl Fn() -> u64,
        budget_ms: u64,
    ) -> UnlockSnapshot
    where
        H: UnlockHttp + ?Sized,
        S: UnlockEventSink,
    {
        // ── item 7 单飞：串行化并发 run（第二者等第一者 commit 后走下方 TTL 快路，避免双网络往返）──
        let _run_guard = self.run_lock.lock().await;

        // ── TTL 快路：非 force 且缓存未过期 → 直接返回（零网络），并广播 UPDATED 让新监听者点亮 ──
        if !force {
            if let Some(cached) = self.peek(now()) {
                sink.updated(&cached);
                return cached;
            }
        }

        // ── item 2 S-gate：已提交 notReady 失败终态 → 非 force 直接返终态（防 mount/切 tab 反复重扫死出口
        //    就绪门数十秒）。解除通道 = invalidate（起停/切节点，清 last_snapshot）+ force。──
        if !force {
            if let Some(last) = self.last_snapshot() {
                if last.not_ready == Some(true) {
                    sink.updated(&last);
                    return last;
                }
            }
        }

        // ── item 5 force 硬下限：force 也不得 <15s 重打（连点触发对端限频）→ 返上次快照 ──
        //
        // **「限流」与「返什么」是两件事**：`last_snapshot` 只决定返回值，不该决定是否限流。此前二者绑在
        // 一起（无快照 ⇒ 落空、照常重跑），而「有 lastRunAt 但无快照」恰恰是**漂移丢弃轮**的形态
        // （丢弃腿经 invalidate 清了 last_snapshot）—— 后端正在自跑、对端限频风险最高的那一刻，闸门反而
        // 门户大开。故限流只看 `last_at`；无快照时返空快照：前端 `applyUnlockSnapshot` 的 no-op 守卫
        // （空 results + 无终态标记）识得它、不动现有显示，收口交给自跑那一轮的 UPDATED（其排程由漂移
        // 熔断封顶，见 [`MAX_CONSECUTIVE_DRIFT`]，故不会等一个永不到来的终态）。
        let last_at = self.last_run_at.load(Ordering::SeqCst);
        if force && last_at != 0 && now().saturating_sub(last_at) < FORCE_MIN_MS {
            let last = self.last_snapshot();
            if let Some(snap) = &last {
                sink.updated(snap);
            }
            return last.unwrap_or_default();
        }

        let epoch0 = self.epoch();
        // 整轮 deadline 从此刻起算（gating/TTL/S-gate/force-min 四条早退路径是零网络的，不吃预算）。
        let deadline = tokio::time::Instant::now() + Duration::from_millis(budget_ms);

        // ── item 2 就绪门退避：egress trace 兼作「inbound 已就绪」探针（首次即时探 + 失败退避重试 7 次 +
        //    B1 flap 确认）。拿到有效 egress = 就绪，兼作轮首出口锚（bracket）。──
        let egress0 = match self.probe_ready(http, epoch0, deadline).await {
            Some(e) => e,
            None => {
                // 退避期/探测期被 invalidate（epoch 变）→ 丢弃本轮（陈旧，不提交 notReady 污染新出口）。
                if self.epoch() != epoch0 {
                    log::debug!("解锁检测：就绪门期间被 invalidate → 丢弃本轮（由自跑重跑）");
                    return UnlockSnapshot::default();
                }
                // 真机 logLevel=warn ⇒ warn：这是「一个 checker 都没跑成」的降级终态，正是用户报「没有最终
                // 结果」时最需要在日志里看见的一条。
                log::warn!(
                    "解锁检测：就绪门未过（{READINESS_MAX_ATTEMPTS} 攻退避或 {budget_ms}ms 整轮预算耗尽）→ 提交 notReady 终态"
                );
                // 就绪门耗尽 → 提交 notReady 终态（checkedAt=null，不伪造；S-gate 兜住不重扫）。lastRunAt 置位
                // （本轮真跑了整轮就绪门网络 → force 15s 硬下限据此生效）。egress=null → 天然不入 TTL 缓存。
                self.last_run_at.store(now(), Ordering::SeqCst);
                let snap = UnlockSnapshot {
                    not_ready: Some(true),
                    ..Default::default()
                };
                // 落定终态 → 漂移连击清零（本轮连 checker 都没跑，谈不上漂移；且已有终态收口，无自持循环）。
                self.drift_streak.store(0, Ordering::SeqCst);
                self.set_last_snapshot(Some(snap.clone()));
                sink.updated(&snap);
                return snap;
            }
        };

        self.last_run_at.store(now(), Ordering::SeqCst);
        // 受限出口（CN）：海外服务 timeout 是结构性预期、非低置信瞬态 → 跳过 settle-retry + 用正常 30min TTL
        // + 不标 low_confidence（就绪门已过 → egress 必非空，此值贯穿本轮）。
        let restricted = is_restricted_egress_region(egress0.region.as_deref());

        // ── item 3 checker 主轮（单 checker 截止点 = min(CHECKER_BUDGET_MS, 整轮剩余)）：逐 settle emit progress ──
        let mut results =
            run_checkers_budgeted(http, ServiceId::ALL, deadline, |id, r| sink.progress(id, r))
                .await;

        // ── item 4 轮内 settle-retry：就绪门只证「单点连通」非「各端点已热」→ 首轮个别 checker 撞冷隧道 8s
        //    超时。commit 前仅对 timeout 项退避补测 ≤2 轮（保留高置信结果、只重打灰的，对端友好）。受限出口
        //    跳过（timeout 是结构性终态、补测无意义）。──
        if !restricted {
            for round in 1..=SETTLE_RETRY_MAX_ROUNDS {
                let timeout_ids: Vec<ServiceId> = ServiceId::ALL
                    .iter()
                    .copied()
                    .filter(|id| {
                        results
                            .get(id.as_str())
                            .is_some_and(|r| r.status == UnlockStatus::Timeout)
                    })
                    .collect();
                if timeout_ids.is_empty() {
                    break; // 全部高置信 → 快路径零额外开销
                }
                if self.epoch() != epoch0 {
                    break; // 本轮已作废 → 下方 bracket 守卫会丢弃
                }
                // deadline 判在**发 checking 之前**：跨界就直接停、保留已有 timeout 终态。若先发了 checking
                // 再停，那几个服务会永远停在「补测中」——正是本批修的「徽章转圈不落地」形态。
                let backoff = Duration::from_millis(SETTLE_RETRY_BACKOFF_MS * round);
                if tokio::time::Instant::now() + backoff >= deadline {
                    log::debug!("解锁检测：settle-retry 第 {round} 轮退避跨整轮 deadline → 停止补测，保留已有终态");
                    break;
                }
                // 灰点翻回 checking（视觉诚实：补测中，非终态）。
                for id in &timeout_ids {
                    sink.progress(id.as_str(), &UnlockResult::new(UnlockStatus::Checking));
                }
                tokio::time::sleep(backoff).await;
                if self.epoch() != epoch0 {
                    break; // 退避期间被 invalidate → 放弃本轮补测
                }
                let fresh = run_checkers_budgeted(http, &timeout_ids, deadline, |id, r| {
                    sink.progress(id, r)
                })
                .await;
                for (id, r) in fresh {
                    results.insert(id, r);
                }
            }
        }

        // ── 出口归属 bracket 确认：轮尾 egress ──
        // 同样受整轮 deadline 约束（floor MIN_OP_BUDGET_MS）：轮尾探测若无界，一次挂死的确认探就能把整轮拖成
        // 「永远不 commit」——即用户报的「一直在检测中」。超时按 None 处理，语义同「confirm 失败 ≠ 出口不符」。
        let egress1 =
            tokio::time::timeout_at(op_deadline(deadline, CHECKER_BUDGET_MS), probe_egress(http))
                .await
                .unwrap_or(None);
        // confirm 失败(None) ≠ 不符：网络瞬态不误触发丢弃（Polaris F-B）。两端都拿到但 IP 不同 = 契约外翻转。
        let egress_moved = match &egress1 {
            Some(b) => b.ip != egress0.ip,
            None => false,
        };

        // ── 归属校验：epoch 变了（并发 invalidate）或出口漂移 → 丢弃，不 commit，广播失效自动重跑 ──
        // **这是「决不把 A 出口的结果标给 B 出口」的门**。
        let epoch_changed = self.epoch() != epoch0;
        if epoch_changed || egress_moved {
            // epoch 变 = 外部真状态变化（起停/切节点），不是漂移 → 连击清零，别让「用户切了三次节点」
            // 被误算成「出口在抖」而错误熔断。
            if epoch_changed {
                self.drift_streak.store(0, Ordering::SeqCst);
            }
            // ── 漂移熔断（见 [`MAX_CONSECUTIVE_DRIFT`]）：连续 N 轮纯漂移 → 停止自持循环，落低置信终态 ──
            // 只有「纯漂移」（epoch 未变）才计数：epoch 变那条腿本就有外部触发源，不会自持。
            if egress_moved && !epoch_changed {
                let streak = self.drift_streak.fetch_add(1, Ordering::SeqCst) + 1;
                if streak >= MAX_CONSECUTIVE_DRIFT {
                    // 真机 logLevel=warn ⇒ warn：这是「为什么徽章突然不转了、且标着低置信」的唯一线索。
                    log::warn!(
                        "解锁检测：出口连续漂移 {streak} 轮（≥{MAX_CONSECUTIVE_DRIFT}）→ 熔断，落低置信终态并停止自跑排程（出口 IP 轮换快过一轮检测：负载均衡/urltest/WARP/多 IP 出口）"
                    );
                    // 归属不变式仍守住：`egress=None` —— 结果不标给**任何**出口，只如实说「测到了，但出口在抖」。
                    let snapshot = UnlockSnapshot {
                        results,
                        checked_at: Some(now()),
                        egress: None,
                        blocked_reason: None,
                        not_ready: None,
                        low_confidence: Some(true),
                    };
                    // 落定即清零：熔断掐断的是自持循环，不是把检测永久闩死。
                    self.drift_streak.store(0, Ordering::SeqCst);
                    self.set_last_snapshot(Some(snapshot.clone()));
                    // low_confidence 不入 TTL 缓存（沿用既有规则）→ 下一次真触发即重检。
                    // **必须 emit UPDATED**：这是 UI 脱离「检测中」的唯一出口（丢弃腿本身从不 emit 终态）。
                    sink.updated(&snapshot);
                    return snapshot;
                }
            }
            // warn：本轮**不产出终态**（不 commit、不 emit UPDATED），前端停在「检测中」直到 invalidate 排的
            // 自跑落地。真机 logLevel=warn 下这条是判「为什么这一轮没结果」的唯一线索。
            log::warn!(
                "解锁检测：归属校验失败（epoch 变={epoch_changed}，出口漂移={egress_moved}）→ 丢弃本轮结果，排自跑重测"
            );
            // 保留 `last_run_at`：本轮真跑过整轮网络，force 15s 硬下限必须继续生效（见 `invalidate_keep_run_at`）。
            self.invalidate_keep_run_at(sink, true, false);
            return UnlockSnapshot::default();
        }
        // 归属校验通过 → 本轮出口稳定，漂移连击中断。
        self.drift_streak.store(0, Ordering::SeqCst);

        // ── commit ──
        let egress = egress1.or(Some(egress0));
        let has_timeout = results.values().any(|r| r.status == UnlockStatus::Timeout);
        let all_timeout =
            !results.is_empty() && results.values().all(|r| r.status == UnlockStatus::Timeout);
        // **受限地区收敛**：CN 出口全超是结构性预期，不置 low_confidence（高置信终态）。
        let low_confidence = all_timeout && !restricted;

        let checked_at = now();
        let snapshot = UnlockSnapshot {
            results,
            checked_at: Some(checked_at),
            egress,
            blocked_reason: None,
            not_ready: None,
            low_confidence: low_confidence.then_some(true),
        };

        // lastSnapshot 恒记（含 lowConfidence，供 S-gate/force-min 读）；TTL `cache` 仅高置信入。
        self.set_last_snapshot(Some(snapshot.clone()));
        // TTL 挂置信度：含 timeout 且非受限 → 2min；否则（含受限全超）→ 30min（受限不 churn）。
        let ttl = if has_timeout && !restricted {
            TIMEOUT_TTL_MS
        } else {
            FRESH_TTL_MS
        };
        // low_confidence（全超瞬态、非受限）不写缓存：避免垃圾快照锁 30min（Polaris：未写 egressIp 缓存）。
        // 下一真触发即重检。仍返回 + emit UPDATED（UI 如实显、但不入缓存）。
        if !low_confidence {
            self.store(snapshot.clone(), checked_at, ttl);
        }
        // info 级：正常收口。真机 logLevel=warn 看不到本条 —— 刻意如此，「成功落终态」不是排查线索；
        // 排查靠上面那几条 warn（没落终态的路径）+ 「没有 warn」这个事实本身。
        log::info!(
            "解锁检测：一轮完成（{} 项，含 timeout={has_timeout}，低置信={low_confidence}，出口={}）",
            snapshot.results.len(),
            snapshot.egress.as_ref().map_or("-", |e| e.ip.as_str())
        );
        sink.updated(&snapshot);
        snapshot
    }

    /// **就绪门退避探测**（item 2，上游 `probeReady`）：egress trace 兼作「inbound 已就绪」探针。attempt 0
    /// 立即探（核已就绪则零延迟，如手动刷新），失败退避 `READINESS_BACKOFF_SCHEDULE_MS[attempt-1]` 重试，
    /// 至多 `READINESS_MAX_ATTEMPTS`(7) 次。**B1 自适应确认**：健康路径（一路成功）首成即就绪、零确认；疑似
    /// flap（曾失败过）成功后追加 1 次确认探（`READINESS_CONFIRM_MS` + 一探，连续 2 成才判就绪）。epoch 守卫：
    /// 退避 sleep 后 / 每次探测后比对 `epoch0`（invalidate 递增 → 立即放弃本轮返 None）。耗尽 → None。
    /// `deadline`：整轮共享死线。**每次退避/探测前判**，且单次探测按剩余收紧（`op_deadline`）——耗尽即返
    /// `None`（→ 调用方提交 notReady 终态），不空等。这是 上游「deadline 本身即上限」语义的就绪门那一段：
    /// 默认 10s 预算下累进退避在第 5 攻（累计 11.6s）越界收口，故 schedule 末段 +4/+8s 尾在默认预算下不可达，
    /// 仅作 headroom 供预算调大时启用。
    async fn probe_ready<H: UnlockHttp + ?Sized>(
        &self,
        http: &H,
        epoch0: u64,
        deadline: tokio::time::Instant,
    ) -> Option<UnlockEgress> {
        let mut ever_failed = false; // 是否曾有一攻失败（触发 B1 确认，疑似 flap）
        for attempt in 0..READINESS_MAX_ATTEMPTS {
            if attempt > 0 {
                let backoff = Duration::from_millis(
                    READINESS_BACKOFF_SCHEDULE_MS
                        .get(attempt - 1)
                        .copied()
                        .unwrap_or(8_000),
                );
                // 退避跨越 deadline → 不睡了（睡完也没预算探，纯空等）。
                if tokio::time::Instant::now() + backoff >= deadline {
                    return None;
                }
                tokio::time::sleep(backoff).await;
                if self.epoch() != epoch0 {
                    return None; // 退避期间被 invalidate → 放弃本轮
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return None; // 预算耗尽
            }
            let egress = probe_with_deadline(http, deadline).await;
            if self.epoch() != epoch0 {
                return None; // 探测期间被 invalidate → 放弃本轮
            }
            match egress {
                Some(e) => {
                    if !ever_failed {
                        return Some(e); // 健康路径：首攻/一路成 → 直接就绪，零代价
                    }
                    // B1：疑似 flap（曾失败）→ 追加 1 次确认（连续 2 成才判就绪；确认失败则续 schedule）。
                    let confirm_gap = Duration::from_millis(READINESS_CONFIRM_MS);
                    if tokio::time::Instant::now() + confirm_gap >= deadline {
                        // 没预算做确认探 → 直接采信这次成功（有 egress 好过 notReady 空转）。
                        return Some(e);
                    }
                    tokio::time::sleep(confirm_gap).await;
                    if self.epoch() != epoch0 {
                        return None;
                    }
                    let confirm = probe_with_deadline(http, deadline).await;
                    if self.epoch() != epoch0 {
                        return None;
                    }
                    if confirm.is_some() {
                        return Some(e); // 2 连成 → 就绪
                    }
                    ever_failed = true; // 确认失败 → 本轮不判就绪，续下一攻 schedule
                }
                None => ever_failed = true,
            }
        }
        None // 重试耗尽，未就绪
    }

    /// **warm 补测**（#6 partial-timeout 自愈）：重打上轮 timeout 的服务，结果 merge 进缓存并广播。
    ///
    /// epoch 守卫（`epoch0` = 调度时的世代）：补测期间有 invalidate（epoch 变）→ 丢弃，
    /// **别把旧出口的补测结果标给新出口**。无缓存/无 timeout 项 → no-op 返 false。
    /// 生产由 command 层 `tokio::spawn(sleep(WARM_RECHECK_DELAY_MS) + run_recheck)` 调度。
    pub async fn run_recheck<H, S>(
        &self,
        http: &H,
        sink: &S,
        epoch0: u64,
        now: impl Fn() -> u64,
    ) -> bool
    where
        H: UnlockHttp + ?Sized,
        S: UnlockEventSink,
    {
        // item 7 单飞：与 run 共用锁——补测不与并发 run 抢网络（command 层在 run 完成后 5s spawn 本腿，
        // 正常已无竞争；持锁兜并发触发面）。
        let _run_guard = self.run_lock.lock().await;
        // 取当前缓存快照 + 其 timeout 服务集（快照可能已被 invalidate 清空 → no-op）。
        let (mut snapshot, timeout_ids) = {
            let guard = match self.cache.lock() {
                Ok(g) => g,
                Err(_) => return false,
            };
            let Some(cached) = guard.as_ref() else {
                return false;
            };
            let ids: Vec<ServiceId> = ServiceId::ALL
                .iter()
                .copied()
                .filter(|id| {
                    cached
                        .snapshot
                        .results
                        .get(id.as_str())
                        .is_some_and(|r| r.status == UnlockStatus::Timeout)
                })
                .collect();
            (cached.snapshot.clone(), ids)
        };
        if timeout_ids.is_empty() {
            return false;
        }

        // 重打 timeout 项（并发；每 checker CHECKER_BUDGET_MS 封顶 item 3；**先收集不即时 emit**——补测期间
        // 可能 invalidate，须先过 epoch 门再 emit，否则会漏发一两个旧出口的 progress）。
        // 补测轮自成一条 deadline（它是 commit 之后 5s 才起的独立一轮，不共享主轮预算）。
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(TOTAL_DETECTION_BUDGET_MS);
        let fresh = run_checkers_budgeted(http, &timeout_ids, deadline, |_, _| {}).await;

        // epoch 守卫：补测期间有 invalidate → 丢弃（归属 bracket 的补测腿），一个 emit 都不发。
        if self.epoch() != epoch0 {
            return false;
        }

        for (id, result) in &fresh {
            sink.progress(id, result);
            snapshot.results.insert(id.clone(), result.clone());
        }
        let t = now();
        snapshot.checked_at = Some(t);
        let has_timeout = snapshot
            .results
            .values()
            .any(|r| r.status == UnlockStatus::Timeout);
        let restricted =
            is_restricted_egress_region(snapshot.egress.as_ref().and_then(|e| e.region.as_deref()));
        let ttl = if has_timeout && !restricted {
            TIMEOUT_TTL_MS
        } else {
            FRESH_TTL_MS
        };
        // lastSnapshot 同步（补测复过的 timeout 已是可信终态，供 S-gate/force-min）；TTL cache 恒写（含 timeout
        // 由 R3 短 TTL 兜底，2min 后可再自然重检）。
        self.set_last_snapshot(Some(snapshot.clone()));
        self.store(snapshot.clone(), t, ttl);
        sink.updated(&snapshot);
        true
    }
}

/// 单次网络操作的截止点：不晚于整轮 `deadline`、不晚于 `now + budget_ms`，但**至少** [`MIN_OP_BUDGET_MS`]。
///
/// 那条 floor 是有意的（上游 `MIN_OP_BUDGET_MS` 同款）：deadline 逼近时按剩余收紧会算出 0/负值，发出去的
/// 是必然失败的退化请求。宁可整轮超出 deadline 至多 500ms，也要让每个操作有一次真实机会 —— 换来的是
/// **每个 checker 都拿得到终态**，而不是一堆没跑就判超时的假结果。
fn op_deadline(deadline: tokio::time::Instant, budget_ms: u64) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    let capped = deadline.min(now + Duration::from_millis(budget_ms));
    capped.max(now + Duration::from_millis(MIN_OP_BUDGET_MS))
}

/// 受整轮 deadline 约束的 egress 探测：超时按「探不到」处理（与网络失败同路，交退避重试腿）。
async fn probe_with_deadline<H: UnlockHttp + ?Sized>(
    http: &H,
    deadline: tokio::time::Instant,
) -> Option<UnlockEgress> {
    tokio::time::timeout_at(op_deadline(deadline, CHECKER_BUDGET_MS), probe_egress(http))
        .await
        .unwrap_or(None)
}

/// 并发跑指定服务子集的 checker，**每 checker 用 `min(CHECKER_BUDGET_MS, 整轮剩余)` 封顶**（item 3），逐 settle 回调
/// `on_settle(serviceId, &result)`，返回 serviceId → UnlockResult。
///
/// 为何自建而非调 crate `run_checkers_with_progress`：① crate 版内部无预算，Disney 主链+备法可 4 连请求
/// 串联、最坏尾延迟累加远超单请求 8s → 此处 `tokio::time::timeout` 对**整个 checker** 封顶，超预算落
/// `Timeout`（有界即可，非精确；底层请求各自 8s 传输超时惰性释放，不铺 AbortSignal 全栈，对齐 上游 E2）；
/// ② 预算需 timer（`tokio::time`），而 unlock crate 无生产 tokio 依赖（纯逻辑层），故预算只能在此运行时层。
///
/// **并发实现**：手写 `poll_fn` 并发轮询（等价 `FuturesUnordered`，语义=并发齐射 + 逐 settle 回调 + 错误隔离）
/// ——**src-tauri 的 `futures` 仅 dev-dependency**（见 `stats.rs` 注：本仓生产禁 `futures` 依赖），故不用
/// `FuturesUnordered`。集合有界（≤6 服务），每次唤醒 O(N) 重轮询代价可忽略。
async fn run_checkers_budgeted<H, F>(
    http: &H,
    ids: &[ServiceId],
    deadline: tokio::time::Instant,
    mut on_settle: F,
) -> BTreeMap<String, UnlockResult>
where
    H: UnlockHttp + ?Sized,
    F: FnMut(&str, &UnlockResult),
{
    use std::future::Future;
    use std::pin::Pin;
    use std::task::Poll;

    // 单 checker 截止点 = min(CHECKER_BUDGET_MS, 整轮剩余)，floor 于 MIN_OP_BUDGET_MS。
    // **每个 checker 必落终态**：超时 → Timeout，不留 Checking 挂着。
    let cap = op_deadline(deadline, CHECKER_BUDGET_MS);
    type Fut<'a> = Pin<Box<dyn Future<Output = UnlockResult> + Send + 'a>>;
    let mut pending: Vec<(ServiceId, Fut<'_>)> = ids
        .iter()
        .map(|&id| {
            let fut: Fut<'_> = Box::pin(async move {
                match tokio::time::timeout_at(cap, run_checker(id, http)).await {
                    Ok(r) => r,
                    Err(_) => UnlockResult::timeout(), // 超预算 → timeout（Disney 4 连请求尾延迟兜底）
                }
            });
            (id, fut)
        })
        .collect();

    let mut out = BTreeMap::new();
    // 并发轮询：每次外层被唤醒即遍历未决 future，settle 的立即回调 + 移出（共享外层 waker，任一就绪即重轮询）。
    std::future::poll_fn(|cx| {
        let mut i = 0;
        while i < pending.len() {
            match pending[i].1.as_mut().poll(cx) {
                Poll::Ready(result) => {
                    let (id, _) = pending.remove(i); // 移出已 settle（不自增 i，remove 已左移）
                    on_settle(id.as_str(), &result);
                    out.insert(id.as_str().to_string(), result);
                }
                Poll::Pending => i += 1,
            }
        }
        if pending.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    out
}

/// 🔴 `invalidate` 的排程腿不得用 `tokio::spawn` —— 2026-07-21 真机崩溃（`SIGABRT`）的守卫。
///
/// # 为什么必须是源码扫描，而不是行为测试
///
/// 崩溃形态：`tokio::spawn` 要求调用处已在 Tokio runtime 上下文内，否则 panic。`invalidate` 的调用方
/// **全是同步 command**（Tauri 对 `pub fn` command 在主线程直接调用，无 runtime 上下文）⇒ 切一次节点
/// 就 `abort()`，射程覆盖 `server_switch` / `server_delete` / `server_delete_batch` /
/// `subscription_delete` / `config_save` / `config_set_value`。
///
/// **这个 bug 单测抓不到，而且是结构性抓不到**：`#[tokio::test]` 自带 runtime 上下文，`tokio::spawn`
/// 与 `tauri::async_runtime::spawn` 在测试里行为完全一致、都能过。当初 14/14 变异全杀、5 门全绿，
/// 照样把它放进了生产 —— **测试环境比生产环境「更宽容」时，测试的绿是没有信息量的**。
/// 唯一能在本层锁住的判据就是「源码里不许出现那个 API」。
#[cfg(test)]
mod spawn_guard {
    use crate::commands::guard_scan::top_level_fn_body;

    const SRC: &str = include_str!("unlock.rs");

    /// 锚定**生产** impl（`BroadcastSink`）而非 trait 上的默认 no-op 实现。
    ///
    /// 两处签名逐字相同（`fn schedule_self_run(&self, token: u64)`），直接按签名 `find` 会命中靠前的
    /// trait 默认实现 —— 那个 body 是 `let _ = token;`，**既不含 `tokio::spawn` 也不含正确 API**。
    /// 首版守卫就踩了这个：只写否定断言的话会在那段空实现上**恒真通过 = 假绿**；是肯定断言把它顶红的。
    fn production_impl_body() -> String {
        strip_comments(&top_level_fn_body(
            SRC,
            "impl UnlockEventSink for BroadcastSink<'_> {",
        ))
    }

    /// 剥掉注释再扫 —— 否则「解释为什么不能用 `tokio::spawn`」的那段文档注释本身会被扫中，
    /// 守卫在代码完全正确时也红（首版实测踩到）。守卫必须只看**代码**。
    ///
    /// 整行注释现已由 [`top_level_fn_body`] 统一剥掉；本函数**多剥一层行尾注释**（本模块的负面断言
    /// 禁的是 `tokio::spawn` 这种会出现在行尾说明里的词），故保留。
    fn strip_comments(src: &str) -> String {
        src.lines()
            .map(|l| {
                let t = l.trim_start();
                if t.starts_with("//") {
                    "" // 整行注释（含 `///` 文档注释）
                } else {
                    // 行尾注释：`//` 之前的部分保留。本文件无字符串字面量含 `//`，故朴素切分足够；
                    // 若将来有，此处需改成词法级切分（届时本注释即为提示）。
                    l.split("//").next().unwrap_or("")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn schedule_self_run_uses_tauri_async_runtime_not_bare_tokio_spawn() {
        let body = production_impl_body();
        assert!(
            body.contains("tauri::async_runtime::spawn"),
            "schedule_self_run 必须用 tauri::async_runtime::spawn（持全局 runtime handle，任意线程可调）"
        );
        assert!(
            !body.contains("tokio::spawn"),
            "schedule_self_run 出现裸 tokio::spawn —— 同步 command 路径无 runtime 上下文，真机必 panic→abort"
        );
    }

    /// 守卫的守卫：证明扫到的确实是**生产 impl 的函数体**而非空串或 trait 的默认 no-op。
    /// 空串会让 `!contains(...)` 恒真 —— 正是「return 型门 = 没门」的形态。
    #[test]
    fn guard_scan_actually_captured_the_production_impl() {
        let body = production_impl_body();
        assert!(
            body.len() > 200,
            "扫到的 impl 体太短（{} 字节），守卫可能已退化",
            body.len()
        );
        assert!(
            body.contains("SELF_RUN_DEBOUNCE_MS"),
            "扫到的片段里没有 schedule_self_run 的标志性内容 ⇒ 锚点漂了，守卫失去判据"
        );
        // 反向自证：确认扫到的**不是** trait 的默认 no-op（那段的全部内容就是 `let _ = token;`）。
        assert!(
            body.contains("run_unlock_cycle"),
            "扫到的像是 trait 默认 no-op 而非生产 impl ⇒ 锚点撞了（两处签名逐字相同）"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    use polaris_unlock::http::{RedirectHop, UnlockRequest, UnlockResponse};

    // ── 事件记录 sink（组合面门：证「事件真 emit」而无需 Tauri 运行时）──────────────
    #[derive(Default)]
    struct RecordingSink {
        progress: StdMutex<Vec<(String, UnlockResult)>>,
        updated: StdMutex<Vec<UnlockSnapshot>>,
        invalidated: StdMutex<Vec<(bool, bool)>>,
        /// 自跑排程 token 流水（每次 invalidate 一条）——去抖合并的可断言面。
        self_runs: StdMutex<Vec<u64>>,
    }
    impl RecordingSink {
        fn progress_count(&self) -> usize {
            self.progress.lock().unwrap().len()
        }
        fn updated(&self) -> Vec<UnlockSnapshot> {
            self.updated.lock().unwrap().clone()
        }
        fn invalidated(&self) -> Vec<(bool, bool)> {
            self.invalidated.lock().unwrap().clone()
        }
        fn self_runs(&self) -> Vec<u64> {
            self.self_runs.lock().unwrap().clone()
        }
    }
    impl UnlockEventSink for RecordingSink {
        fn progress(&self, service_id: &str, result: &UnlockResult) {
            self.progress
                .lock()
                .unwrap()
                .push((service_id.to_string(), result.clone()));
        }
        fn updated(&self, snapshot: &UnlockSnapshot) {
            self.updated.lock().unwrap().push(snapshot.clone());
        }
        fn invalidated(&self, running: bool, exit_blocked: bool) {
            self.invalidated
                .lock()
                .unwrap()
                .push((running, exit_blocked));
        }
        fn schedule_self_run(&self, token: u64) {
            self.self_runs.lock().unwrap().push(token);
        }
    }

    /// 预算足够大 = 不受 deadline 干扰（对齐 上游「单测以冻结注入时钟绕过 deadline」的手法）。
    const BUDGET_UNBOUNDED_MS: u64 = 10 * 60 * 1_000;

    // ── mock UnlockHttp（按 URL 子串脚本 + egress trace 分序列 + 可选每请求 hook）─────
    struct MockHttp {
        scripts: Vec<(String, UnlockResponse)>,
        /// 出口 egress trace（`cloudflare.com/cdn-cgi/trace`）的**逐次**响应：
        /// probe_egress 轮首/轮尾各一次，可造「出口漂移」（bracket 用）。空 = 用 scripts。
        trace_seq: StdMutex<VecDeque<UnlockResponse>>,
        /// 每请求 hook（如 mid-round invalidate）；返回后再走脚本。
        on_request: Option<Box<dyn Fn() + Send + Sync>>,
    }
    impl MockHttp {
        fn new() -> Self {
            Self {
                scripts: Vec::new(),
                trace_seq: StdMutex::new(VecDeque::new()),
                on_request: None,
            }
        }
        fn on(mut self, pat: &str, resp: UnlockResponse) -> Self {
            self.scripts.push((pat.to_string(), resp));
            self
        }
        fn egress_seq(self, seq: Vec<UnlockResponse>) -> Self {
            *self.trace_seq.lock().unwrap() = seq.into();
            self
        }
        fn hook(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
            self.on_request = Some(Box::new(f));
            self
        }
    }
    #[async_trait::async_trait]
    impl UnlockHttp for MockHttp {
        async fn request(&self, req: &UnlockRequest) -> UnlockResponse {
            if let Some(h) = &self.on_request {
                h();
            }
            // 出口 egress trace 走独立序列（轮首/轮尾可不同 → 造出口漂移）。
            if req.url.contains("cloudflare.com/cdn-cgi/trace") {
                let mut seq = self.trace_seq.lock().unwrap();
                if !seq.is_empty() {
                    // 用完保留末值（后续 probe 复用最后一个响应）。
                    return if seq.len() == 1 {
                        seq[0].clone()
                    } else {
                        seq.pop_front().unwrap()
                    };
                }
            }
            for (pat, resp) in &self.scripts {
                if req.url.contains(pat) {
                    return resp.clone();
                }
            }
            UnlockResponse::err("no-script")
        }
    }

    fn ok(status: u16, body: &str) -> UnlockResponse {
        UnlockResponse::ok(status, body)
    }

    /// 全服务 Ok 的脚本集（1:1 复用 detector.rs `detect_aggregates` 的已证夹具）。egress=US。
    fn all_ok_mock() -> MockHttp {
        MockHttp::new()
            .egress_seq(vec![ok(200, "ip=1.1.1.1\nloc=US\n")])
            .on(
                "chat.openai.com/cdn-cgi/trace",
                ok(200, "ip=1.1.1.1\nloc=US\n"),
            )
            .on("api.openai.com", ok(200, "{}"))
            .on("ios.chat.openai.com", ok(200, "<html>welcome</html>"))
            .on(
                "claude.ai/",
                UnlockResponse {
                    status: 200,
                    body: String::new(),
                    truncated: false,
                    redirect_chain: vec![RedirectHop {
                        status: 302,
                        location: "https://claude.ai/login".to_string(),
                    }],
                    error: None,
                    ..Default::default()
                },
            )
            .on("claude.ai/cdn-cgi/trace", ok(200, "ip=1.1.1.1\nloc=US\n"))
            .on("gemini.google.com", ok(200, "blah 45631641,null,true blah"))
            // grok：**当前不在上线集**（`ServiceId::PENDING_CALIBRATION`，待真机哨兵标定）→ 本轮不会被请求。
            // 仍预置脚本：开关一翻（`types.rs` 把 Grok 移回 `ServiceId::ALL`）这批测试不会因为「mock 漏脚本
            // → Timeout → 走 TIMEOUT_TTL/settle-retry」莫名转红。trace 须排首页脚本**之前**（首个子串匹配即返回）。
            .on("grok.com/cdn-cgi/trace", ok(200, "ip=1.1.1.1\nloc=US\n"))
            .on("grok.com", ok(200, "<html>cdn.grok.com/_next</html>"))
            .on("netflix.com/title/81280792", ok(200, "watchable content"))
            .on("netflix.com/title/70143836", ok(200, "watchable content"))
            .on("bamgrid.com/devices", ok(200, r#"{"assertion":"A"}"#))
            .on("bamgrid.com/token", ok(200, r#"{"refresh_token":"R"}"#))
            .on(
                "bamgrid.com/graph",
                ok(200, r#"{"countryCode":"JP","inSupportedLocation":true}"#),
            )
            .on("disneyplus.com", ok(200, ""))
            // tiktok：store_region 须排在首页脚本**之前**（mock 首个子串匹配即返回，`www.tiktok.com/`
            // 会先吃掉 passport 请求）。首页无跳转 → 停在 feed → Ok。
            .on(
                "tiktok.com/passport/web/store_region/",
                ok(200, r#"{"data":{"store_region":"us"},"message":"success"}"#),
            )
            .on("www.tiktok.com/", ok(200, "<html>feed</html>"))
            .on(
                "spotify.com",
                ok(
                    200,
                    r#"{"status":1,"country":"US","is_country_launched":true}"#,
                ),
            )
    }

    fn runtime() -> UnlockRuntime {
        // http client 仅供出口 pin 构造用；`run` 注入无关（测试注 mock），故建一个真 client 占位。
        UnlockRuntime::new(Arc::new(HttpRuntime::new().expect("建 http client")))
    }

    /// gating SoT 全矩阵（item6）：核未运行/无端口 → ProxyNotRunning；running 但 exit_blocked → ExitInvalid；
    /// running + 端口 + 未 blocked → 放行（None）。优先级 ProxyNotRunning > ExitInvalid。
    ///
    /// 变异有牙：删「exit_blocked → ExitInvalid」分支 → case (true,X,true) 返 None → 转红（ExitInvalid 复归 dead）；
    /// 删「!running → ProxyNotRunning」分支 → case (false,..) 返 None 或 ExitInvalid → 转红。
    #[test]
    fn unlock_gate_reason_matrix() {
        // 核未运行 → ProxyNotRunning（无视 exit_blocked，优先级最高）。
        assert_eq!(
            unlock_gate_reason(false, 0, false),
            Some(UnlockBlockedReason::ProxyNotRunning)
        );
        assert_eq!(
            unlock_gate_reason(false, 1080, true),
            Some(UnlockBlockedReason::ProxyNotRunning)
        );
        // running 但无 mixed 入站 → ProxyNotRunning。
        assert_eq!(
            unlock_gate_reason(true, 0, false),
            Some(UnlockBlockedReason::ProxyNotRunning)
        );
        // running + 端口 + 出口失效 → ExitInvalid（本项接线的核心：不再 dead）。
        assert_eq!(
            unlock_gate_reason(true, 1080, true),
            Some(UnlockBlockedReason::ExitInvalid)
        );
        // running + 端口 + 出口有效 → 放行。
        assert_eq!(unlock_gate_reason(true, 1080, false), None);
    }

    // ── 组合面门（§K7.1）：真调 run → 快照真存 → 事件真 emit ─────────────────────
    #[tokio::test]
    async fn combination_gate_run_stores_snapshot_and_emits_progress_and_updated() {
        let rt = runtime();
        let sink = RecordingSink::default();
        let http = all_ok_mock();
        let snap = rt.run(&http, &sink, false, || 1_000).await;

        // 快照真存：peek 在 TTL 内取得。
        assert!(
            rt.peek(1_000).is_some(),
            "commit 后 peek 必须取得快照（快照真存）"
        );
        assert_eq!(snap.results.len(), ServiceId::ALL.len());
        for (id, r) in &snap.results {
            assert_eq!(r.status, UnlockStatus::Ok, "service {id} 应 Ok");
        }
        // 事件真 emit：逐服务 progress + 一次 updated。
        assert_eq!(
            sink.progress_count(),
            ServiceId::ALL.len(),
            "每服务 settle 各一次 progress"
        );
        assert_eq!(sink.updated().len(), 1, "一轮完成一次 updated");
        assert_eq!(sink.updated()[0].results.len(), ServiceId::ALL.len());
        assert!(sink.invalidated().is_empty(), "正常轮不应 invalidate");
        assert_eq!(snap.egress.as_ref().unwrap().region.as_deref(), Some("US"));
    }

    // ── 淬火不变式 · 出口归属 bracket（#7）：结果标错出口 → 丢弃 ──────────────────
    #[tokio::test]
    async fn egress_bracket_discards_when_exit_moves_midround() {
        let rt = runtime();
        let sink = RecordingSink::default();
        // 轮首 egress = IP-A，轮尾 = IP-B（出口在检测中途翻转）→ 结果不属任一确定出口。
        let http = all_ok_mock().egress_seq(vec![
            ok(200, "ip=1.1.1.1\nloc=US\n"),
            ok(200, "ip=9.9.9.9\nloc=US\n"),
        ]);
        let snap = rt.run(&http, &sink, false, || 1_000).await;

        // **决不把 A 出口的结果标给 B 出口**：丢弃，不 commit，不 emit UPDATED，改 emit INVALIDATED。
        assert!(
            rt.peek(1_000).is_none(),
            "出口漂移 → 结果不得入缓存（否则标错出口）"
        );
        assert!(sink.updated().is_empty(), "丢弃轮不得 emit UPDATED");
        assert_eq!(
            sink.invalidated().len(),
            1,
            "出口漂移应 emit INVALIDATED（自动重跑）"
        );
        assert!(snap.checked_at.is_none(), "丢弃轮返回空快照");
    }

    // ── 淬火不变式 · 出口归属 bracket（#7）：并发 invalidate → 丢弃（epoch 腿）─────
    #[tokio::test]
    async fn epoch_bracket_discards_when_invalidated_midround() {
        let rt = Arc::new(runtime());
        let sink = Arc::new(RecordingSink::default());
        // hook：检测请求飞行期间发生一次 invalidate（切节点）→ epoch 变。
        let rt_hook = rt.clone();
        let sink_hook = sink.clone();
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired2 = fired.clone();
        let http = all_ok_mock().hook(move || {
            // 只触发一次，模拟轮中一次切节点。
            if !fired2.swap(true, Ordering::SeqCst) {
                rt_hook.invalidate(&*sink_hook, true, false);
            }
        });
        let snap = rt.run(&http, &*sink, false, || 1_000).await;

        assert!(
            rt.peek(1_000).is_none(),
            "并发 invalidate → 结果不得 commit（epoch 作废）"
        );
        assert!(sink.updated().is_empty(), "epoch 作废轮不得 emit UPDATED");
        assert!(snap.checked_at.is_none());
    }

    // ── 淬火不变式 · TTL（#65/#6）：过期不再 serve ──────────────────────────────
    #[tokio::test]
    async fn ttl_expired_snapshot_is_not_served() {
        let rt = runtime();
        let sink = RecordingSink::default();
        // 全 Ok → 无 timeout → 30min FRESH TTL。commit 于 T0=1000。
        rt.run(&all_ok_mock(), &sink, false, || 1_000).await;
        assert!(rt.peek(1_000).is_some(), "刚存应可取");
        assert!(rt.peek(1_000 + FRESH_TTL_MS - 1).is_some(), "TTL 内应可取");
        assert!(
            rt.peek(1_000 + FRESH_TTL_MS).is_none(),
            "过 TTL 必须失效（否则陈旧快照永久 serve）"
        );
    }

    // ── 淬火不变式 · invalidate 契约（#7）：切节点/起停 → 清缓存 + 递增 epoch ────
    #[tokio::test]
    async fn invalidate_clears_cache_and_bumps_epoch_and_emits() {
        let rt = runtime();
        let sink = RecordingSink::default();
        rt.run(&all_ok_mock(), &sink, false, || 1_000).await;
        assert!(rt.peek(1_000).is_some(), "前提：已有缓存");
        let e0 = rt.epoch();

        rt.invalidate(&sink, true, false);

        assert!(
            rt.peek(1_000).is_none(),
            "invalidate 必须清缓存（切节点不清缓存 = 陈旧污染）"
        );
        assert_eq!(
            rt.epoch(),
            e0 + 1,
            "invalidate 必须递增 epoch（作废在飞轮）"
        );
        assert_eq!(
            sink.invalidated().last(),
            Some(&(true, false)),
            "带核真态广播"
        );
    }

    // ── 淬火不变式 · 受限地区收敛（#8）：CN 全超按高置信终态收敛（正常 30min TTL）──
    #[tokio::test]
    async fn restricted_cn_all_timeout_converges_not_low_confidence() {
        let rt = runtime();
        let sink = RecordingSink::default();
        // egress CN + 所有 checker 无脚本 → 全 timeout。
        let http = MockHttp::new().egress_seq(vec![ok(200, "ip=1.2.3.4\nloc=CN\n")]);
        let snap = rt.run(&http, &sink, false, || 1_000).await;

        assert!(
            snap.results
                .values()
                .all(|r| r.status == UnlockStatus::Timeout),
            "CN 出口海外服务全超（结构性预期）"
        );
        assert_eq!(
            snap.low_confidence, None,
            "受限地区全超**不**置 low_confidence（高置信终态）"
        );
        // 收敛 = 正常 30min TTL（非 2min churn）：3min 后仍在缓存。
        assert!(rt.peek(1_000).is_some(), "受限终态应入缓存");
        assert!(
            rt.peek(1_000 + 3 * 60 * 1_000).is_some(),
            "受限用 30min TTL（非 2min）→ 3min 后仍 serve，不 churn 重扫"
        );
    }

    // 对照：非受限（US）全超 = 低置信瞬态 → 置 low_confidence + **不入缓存**（避免垃圾快照锁 30min）。
    // `start_paused`：非受限全超会触发 settle-retry 退避（2s+4s），暂停时钟使其瞬时（不真睡）。
    #[tokio::test(start_paused = true)]
    async fn nonrestricted_all_timeout_is_low_confidence_and_not_cached() {
        let rt = runtime();
        let sink = RecordingSink::default();
        let http = MockHttp::new().egress_seq(vec![ok(200, "ip=1.1.1.1\nloc=US\n")]);
        let snap = rt.run(&http, &sink, false, || 1_000).await;

        assert!(snap
            .results
            .values()
            .all(|r| r.status == UnlockStatus::Timeout));
        assert_eq!(snap.low_confidence, Some(true), "非受限全超 = 低置信瞬态");
        assert!(
            rt.peek(1_000).is_none(),
            "低置信全超不写缓存（下一真触发即重检）"
        );
        assert_eq!(
            sink.updated().len(),
            1,
            "仍 emit UPDATED（UI 如实显），只是不入缓存"
        );
    }

    // ── 淬火不变式 · warm 补测（#6）：重打 timeout 项并 merge ──────────────────
    // `start_paused`：首轮 partial-timeout 触发轮内 settle-retry 退避，暂停时钟使其瞬时。
    #[tokio::test(start_paused = true)]
    async fn warm_recheck_reruns_timeout_services_and_merges() {
        let rt = runtime();
        let sink = RecordingSink::default();
        // 首轮：netflix 两片无脚本 → netflix timeout；其余 Ok（partial-timeout）。
        let mut partial = all_ok_mock();
        partial
            .scripts
            .retain(|(p, _)| !p.contains("netflix.com/title"));
        rt.run(&partial, &sink, false, || 1_000).await;
        let first = rt.peek(1_000).expect("partial-timeout 含非超项 → 入缓存");
        assert_eq!(first.results["netflix"].status, UnlockStatus::Timeout);

        // warm 补测：netflix 恢复可看 → run_recheck 应把 netflix merge 成 Ok。
        let epoch0 = rt.epoch();
        let healed = all_ok_mock();
        let committed = rt.run_recheck(&healed, &sink, epoch0, || 2_000).await;
        assert!(committed, "有 timeout 项 + epoch 未变 → 补测应 commit");
        let after = rt.peek(2_000).expect("补测后仍有缓存");
        assert_eq!(
            after.results["netflix"].status,
            UnlockStatus::Ok,
            "netflix 应被补测点亮"
        );
        assert_eq!(after.checked_at, Some(2_000), "补测刷新 checkedAt");
    }

    // warm 补测 epoch 守卫：补测期间 invalidate（epoch 变）→ 丢弃，不改缓存。
    // `start_paused`：首轮 partial-timeout 触发轮内 settle-retry 退避，暂停时钟使其瞬时。
    #[tokio::test(start_paused = true)]
    async fn warm_recheck_epoch_guard_discards_after_invalidate() {
        let rt = runtime();
        let sink = RecordingSink::default();
        let mut partial = all_ok_mock();
        partial
            .scripts
            .retain(|(p, _)| !p.contains("netflix.com/title"));
        rt.run(&partial, &sink, false, || 1_000).await;
        let stale_epoch = rt.epoch();
        // 补测调度后、执行前发生 invalidate（切节点）：epoch 变 + 缓存清。
        rt.invalidate(&sink, true, false);
        let committed = rt
            .run_recheck(&all_ok_mock(), &sink, stale_epoch, || 2_000)
            .await;
        assert!(
            !committed,
            "epoch 变（invalidate 过）→ 补测丢弃（别测旧出口）"
        );
        assert!(
            rt.peek(2_000).is_none(),
            "补测不得复活被 invalidate 清掉的缓存"
        );
    }

    // ── force 绕缓存 ────────────────────────────────────────────────────────────
    #[tokio::test]
    async fn force_bypasses_fresh_cache_and_redetects() {
        let rt = runtime();
        let sink = RecordingSink::default();
        rt.run(&all_ok_mock(), &sink, false, || 1_000).await;
        assert_eq!(sink.updated().len(), 1);
        // 非 force + 新鲜缓存 → 快路（不重跑 checker，但仍 emit updated 让新监听者点亮）。
        rt.run(&all_ok_mock(), &sink, false, || 1_100).await;
        assert_eq!(sink.updated().len(), 2, "快路仍 emit updated");
        assert_eq!(
            sink.progress_count(),
            ServiceId::ALL.len(),
            "快路不重跑 checker（progress 不增）"
        );
        // force → 重跑（progress 再增一轮）。**须越过 force 硬下限（item 5，15s）**——首跑于 T=1_000，故
        // 用 `1_000 + FORCE_MIN_MS` 让 15s 硬下限放行（否则 force<15s 会被 item 5 挡住，见 force_min_* 测）。
        rt.run(&all_ok_mock(), &sink, true, || 1_000 + FORCE_MIN_MS)
            .await;
        assert_eq!(
            sink.progress_count(),
            ServiceId::ALL.len() * 2,
            "force 重跑 checker"
        );
    }

    #[test]
    fn peek_none_when_empty() {
        assert!(runtime().peek(0).is_none());
    }

    // ── A7 · 出口变判准（四写腿共用谓词）：old != new 各组合，含 →null ────────────────
    // 打断（恒 true / 恒 false）→ 对应断言转红：
    //   恒 true → 「重选同一节点 / 始终无选中不失效」转红（白刷探测）；
    //   恒 false → 「换节点 / 首次选中 / →null 失效」转红（陈旧 30min 角标）。
    #[test]
    fn selected_exit_changed_covers_all_option_combos() {
        assert!(selected_exit_changed(Some("a"), Some("b")), "换节点 → 变");
        assert!(
            !selected_exit_changed(Some("a"), Some("a")),
            "重选同一节点 → 不变（防白刷）"
        );
        assert!(
            selected_exit_changed(None, Some("a")),
            "首次选中（旧 None）→ 变"
        );
        assert!(
            selected_exit_changed(Some("a"), None),
            "→null：删当前选中 / 订阅刷没了选中 → 变（必须失效）"
        );
        assert!(!selected_exit_changed(None, None), "始终无选中 → 不变");
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // item 2 · 就绪门退避（probe_ready：核起→路由前探针重试 7 次 + B1 flap）+ S-gate
    // ══════════════════════════════════════════════════════════════════════════════

    /// **item 2 · 就绪门耗尽 → notReady 终态**：egress 始终探不到（inbound 未就绪）→ 7 攻全败 → 提交
    /// notReady（checkedAt=null，一个 checker 都不跑，不污染成假 timeout）。`start_paused` 使 19.6s 退避瞬时。
    ///
    /// **预算放大到 [`BUDGET_UNBOUNDED_MS`]**：整轮 deadline 落地后，默认 10s 预算下第 5 攻即越界收口
    /// （见 `round_deadline_truncates_readiness_gate`），7 攻全跑只在预算充裕时可达。此处验的是**退避
    /// schedule 本身完整**，故绕开 deadline —— 对齐 上游 同一处注释「单测以冻结注入时钟绕过 deadline
    /// 验证全 7 攻仍可达」。
    ///
    /// **变异锁**：删就绪门（改回单探 `probe_egress` 无退避）→ 首探失败即被当结果（全 timeout / 假快照），
    /// `not_ready==Some(true)` 与 `results.is_empty()` 转红。
    #[tokio::test(start_paused = true)]
    async fn readiness_gate_exhausts_to_not_ready_when_egress_never_probes() {
        let rt = runtime();
        let sink = RecordingSink::default();
        // cloudflare trace 恒 503 → probe_egress 恒 None → 就绪门 7 攻全败。计探测次数验「真跑满退避重试」。
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = count.clone();
        let http = MockHttp::new().egress_seq(vec![ok(503, "")]).hook(move || {
            c2.fetch_add(1, Ordering::SeqCst);
        });
        let snap = rt
            .run_with_budget(&http, &sink, false, || 1_000, BUDGET_UNBOUNDED_MS)
            .await;

        assert_eq!(snap.not_ready, Some(true), "就绪门耗尽 → notReady 终态");
        assert!(
            snap.checked_at.is_none(),
            "notReady 不伪造 checkedAt（本轮没跑 checker）"
        );
        assert!(
            snap.results.is_empty(),
            "就绪门未过 → 一个 checker 都不跑（不污染成假 timeout）"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            READINESS_MAX_ATTEMPTS,
            "就绪门跑满 7 攻退避重试（非单探即弃 → 冷启动首轮探测失败不被当结果）"
        );
        assert_eq!(sink.progress_count(), 0, "未就绪 → 零 checker progress");
        assert_eq!(
            sink.updated().len(),
            1,
            "notReady 终态仍 emit UPDATED（前端复位）"
        );
        assert!(
            rt.peek(1_000).is_none(),
            "notReady 不入 TTL 缓存（egress=null）"
        );
    }

    /// **item 2 · S-gate**：已提交 notReady 终态 → 非 force 再触发直接返终态，不再重扫 7 攻就绪门（progress 仍 0）；
    /// force 越过 15s 硬下限才解除重扫。
    ///
    /// **变异锁**：删 S-gate（`last_snapshot().not_ready` 分支）→ 第二次非 force 会重跑就绪门 → `progress_count`
    /// 断言（仍 0）转红（退回「mount/切 tab 反复重扫死出口数十秒」）。
    #[tokio::test(start_paused = true)]
    async fn s_gate_returns_not_ready_terminal_without_rescan() {
        let rt = runtime();
        let sink = RecordingSink::default();
        // 计网络探测：S-gate 命中的第二次非 force 应零网络（否则重扫 7 攻就绪门）。
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = count.clone();
        let http = MockHttp::new().egress_seq(vec![ok(503, "")]).hook(move || {
            c2.fetch_add(1, Ordering::SeqCst);
        });
        rt.run(&http, &sink, false, || 1_000).await; // 就绪门耗尽 → notReady
        let after_first = count.load(Ordering::SeqCst); // ≈7 攻
        let updated_after_first = sink.updated().len();

        // 非 force 再跑：S-gate 命中 → 直接返 notReady，**零网络**（不重扫 7 攻就绪门）。
        let snap2 = rt.run(&http, &sink, false, || 2_000).await;
        assert_eq!(snap2.not_ready, Some(true), "S-gate 返 notReady 终态");
        assert_eq!(
            count.load(Ordering::SeqCst),
            after_first,
            "S-gate：第二次非 force 零网络探测（不重扫死出口就绪门数十秒）"
        );
        assert_eq!(
            sink.progress_count(),
            0,
            "S-gate 不跑 checker（零 progress）"
        );
        assert_eq!(
            sink.updated().len(),
            updated_after_first + 1,
            "S-gate 仍 emit UPDATED（水合）"
        );

        // force 越过 15s 硬下限 → 解除 S-gate，重扫（egress 仍探不到 → 仍 notReady，网络计数增加）。
        let snap3 = rt.run(&http, &sink, true, || 1_000 + FORCE_MIN_MS).await;
        assert_eq!(
            snap3.not_ready,
            Some(true),
            "force 重扫仍 notReady（egress 仍 503）"
        );
        assert!(
            count.load(Ordering::SeqCst) > after_first,
            "force 解除 S-gate → 重扫（网络计数增加，证明 S-gate 只挡非 force）"
        );
    }

    /// **item 2 · B1 自适应确认**：曾失败过（疑似 flap）→ 成功探测后需连续 2 成才判就绪。egress 序列
    /// 失败→成功→确认成功 → 就绪 → 跑 checker（全 ok）。`start_paused` 使退避/确认间隔瞬时。
    ///
    /// **变异锁**：删 B1（成功即 return，不追加确认）→ 第 2 攻单次成功即就绪，与本序列结果同（弱），故辅以
    /// 「就绪需吃到第 3 个 egress 响应」——若无 B1 确认，第 3 个 US 会留给轮尾 bracket，egress 消费序不同；
    /// 主锁仍是就绪成功 → checkedAt 非空。
    #[tokio::test(start_paused = true)]
    async fn readiness_b1_confirm_requires_two_success_after_flap() {
        let rt = runtime();
        let sink = RecordingSink::default();
        // 序列：attempt0 失败(503) → attempt1 成功(US) → B1 确认探成功(US) → 就绪 → 轮尾 egress(US)。
        let http = all_ok_mock().egress_seq(vec![
            ok(503, ""),
            ok(200, "ip=1.1.1.1\nloc=US\n"),
            ok(200, "ip=1.1.1.1\nloc=US\n"),
            ok(200, "ip=1.1.1.1\nloc=US\n"),
        ]);
        let snap = rt.run(&http, &sink, false, || 1_000).await;
        assert!(snap.checked_at.is_some(), "B1 2 连成 → 就绪 → 提交终态");
        assert_eq!(
            snap.results.len(),
            ServiceId::ALL.len(),
            "就绪后跑全部 checker"
        );
        assert_eq!(snap.egress.as_ref().unwrap().region.as_deref(), Some("US"));
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // item 3 · 单 checker 总预算封顶（CHECKER_BUDGET_MS）
    // ══════════════════════════════════════════════════════════════════════════════

    /// **item 3 · 单 checker 超预算 → timeout**：chatgpt 的每个请求卡死 > `CHECKER_BUDGET_MS` → 该 checker 被
    /// `tokio::time::timeout` 封顶落 timeout；其余服务立即返回不受影响。`start_paused` 使预算推进瞬时。
    ///
    /// **变异锁**：删预算（改回裸 `run_checker`）→ chatgpt 请求各睡满后返 `ok(200,"{}")` → checker 判非 timeout
    /// → `chatgpt==Timeout` 断言转红（退回「Disney/多连请求 checker 无兜底、最坏 32s+」）。
    #[tokio::test(start_paused = true)]
    async fn checker_budget_caps_hung_checker_to_timeout() {
        struct SlowChatgpt;
        #[async_trait::async_trait]
        impl UnlockHttp for SlowChatgpt {
            async fn request(&self, req: &UnlockRequest) -> UnlockResponse {
                if req.url.contains("cloudflare.com/cdn-cgi/trace") {
                    return ok(200, "ip=1.1.1.1\nloc=US\n"); // egress 立即就绪
                }
                if req.url.contains("openai.com") {
                    // chatgpt 三请求（cookie/ios/trace）各卡死超预算 → 整 checker 超 CHECKER_BUDGET_MS。
                    tokio::time::sleep(Duration::from_millis(CHECKER_BUDGET_MS + 5_000)).await;
                    return ok(200, "{}");
                }
                ok(200, "{}") // 其余服务立即返回（不 hang）
            }
        }
        let rt = runtime();
        let sink = RecordingSink::default();
        let snap = rt.run(&SlowChatgpt, &sink, false, || 1_000).await;
        assert_eq!(
            snap.results["chatgpt"].status,
            UnlockStatus::Timeout,
            "chatgpt 卡死超预算 → CHECKER_BUDGET_MS 封顶为 timeout"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // item 4 · 轮内 settle-retry（commit 前对 timeout 项退避补测 ≤2 轮）
    // ══════════════════════════════════════════════════════════════════════════════

    /// **item 4 · settle-retry 愈合冷隧道首轮 timeout**：netflix 首轮（前 2 个 title 请求）冷隧道失败 → timeout，
    /// 补测轮恢复 watchable → 最终 ok 合入 commit。`start_paused` 使 2s+4s 退避瞬时。
    ///
    /// **变异锁**：删 settle-retry 循环 → netflix 停在首轮 timeout → `netflix==Ok` 转红（首轮瞬态 timeout 被当结果）。
    #[tokio::test(start_paused = true)]
    async fn settle_retry_heals_cold_tunnel_first_round_timeout() {
        struct NetflixHeals {
            inner: MockHttp,
            calls: StdMutex<usize>,
        }
        #[async_trait::async_trait]
        impl UnlockHttp for NetflixHeals {
            async fn request(&self, req: &UnlockRequest) -> UnlockResponse {
                if req.url.contains("netflix.com/title") {
                    let mut c = self.calls.lock().unwrap();
                    *c += 1;
                    // 首轮 2 个 title 请求 → 冷隧道失败（→ netflix timeout）；补测轮 → watchable（→ ok）。
                    return if *c <= 2 {
                        UnlockResponse::err("cold-tunnel")
                    } else {
                        ok(200, "watchable content")
                    };
                }
                self.inner.request(req).await // 其余服务（含 egress）恒 ok
            }
        }
        let rt = runtime();
        let sink = RecordingSink::default();
        let http = NetflixHeals {
            inner: all_ok_mock(),
            calls: StdMutex::new(0),
        };
        let snap = rt.run(&http, &sink, false, || 1_000).await;
        assert_eq!(
            snap.results["netflix"].status,
            UnlockStatus::Ok,
            "settle-retry 补测轮 netflix 恢复 → 最终 ok（首轮冷隧道 timeout 不落定）"
        );
        // 补测中 netflix 灰点翻回 checking（视觉诚实）。
        let saw_checking = sink
            .progress
            .lock()
            .unwrap()
            .iter()
            .any(|(id, r)| id == "netflix" && r.status == UnlockStatus::Checking);
        assert!(
            saw_checking,
            "settle-retry 补测须对 timeout 项重发 checking"
        );
    }

    /// **item 4 · settle-retry 只重打灰的、不碰高置信项**：netflix 恒 timeout（无脚本），chatgpt 恒 ok。
    /// 断言 chatgpt 从不收到 checking（不被 settle-retry 重扫），netflix 收到 checking（被补测）。
    #[tokio::test(start_paused = true)]
    async fn settle_retry_only_reprobes_timeout_services() {
        let rt = runtime();
        let sink = RecordingSink::default();
        let mut partial = all_ok_mock();
        partial
            .scripts
            .retain(|(p, _)| !p.contains("netflix.com/title")); // netflix 恒 timeout
        rt.run(&partial, &sink, false, || 1_000).await;

        let progress = sink.progress.lock().unwrap().clone();
        let netflix_checking = progress
            .iter()
            .any(|(id, r)| id == "netflix" && r.status == UnlockStatus::Checking);
        let chatgpt_checking = progress
            .iter()
            .any(|(id, r)| id == "chatgpt" && r.status == UnlockStatus::Checking);
        assert!(
            netflix_checking,
            "timeout 项 netflix 被 settle-retry 重打（checking）"
        );
        assert!(!chatgpt_checking, "高置信项 chatgpt 不被 settle-retry 重扫");
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // item 5 · force 硬下限（FORCE_MIN_MS=15s 防连点限频）
    // ══════════════════════════════════════════════════════════════════════════════

    /// **item 5 · force 15s 硬下限**：15s 内连点 force → 返上次快照、不重打 checker；≥15s 才放行重跑。
    ///
    /// **变异锁**：删 force-min 判断 → 5s 后的 force 也重跑 → `progress_count`（仍 6）转红（连点强刷更快触发对端限频）。
    #[tokio::test]
    async fn force_min_blocks_rapid_reforce() {
        let rt = runtime();
        let sink = RecordingSink::default();
        rt.run(&all_ok_mock(), &sink, true, || 10_000).await; // 首次 force → 真跑（lastRunAt=10_000）
        assert_eq!(
            sink.progress_count(),
            ServiceId::ALL.len(),
            "首次 force 真跑"
        );

        // 5s 后再 force（<15s）→ 硬下限挡住：返上次快照，不重打。
        let snap = rt.run(&all_ok_mock(), &sink, true, || 15_000).await;
        assert_eq!(
            sink.progress_count(),
            ServiceId::ALL.len(),
            "force<15s 被挡 → 不重跑 checker（progress 不增）"
        );
        assert!(snap.checked_at.is_some(), "被挡时返上次终态快照（非空）");

        // 15s 后 force → 放行重跑。
        rt.run(&all_ok_mock(), &sink, true, || 10_000 + FORCE_MIN_MS)
            .await;
        assert_eq!(
            sink.progress_count(),
            ServiceId::ALL.len() * 2,
            "force≥15s 放行重跑"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // item 7 · 单飞（并发 run 串行化，第二者命中缓存零重扫）
    // ══════════════════════════════════════════════════════════════════════════════

    /// **item 7 · 单飞**：并发两 run（同冻结时钟）→ run_lock 串行 → 第一者 commit 缓存 → 第二者走 TTL 快路，
    /// 只跑一轮 checker（6 progress，非 12）。
    ///
    /// **变异锁**：删 run_lock（去掉 `_run_guard`）→ 两轮各跑一遍 → `progress_count==6` 转红（并发 run 各跑
    /// 一遍网络往返，资源浪费）。
    #[tokio::test]
    async fn single_flight_serializes_concurrent_runs() {
        // 每请求前 `yield_now` 制造 await 让出点，暴露并发交错——否则同步 mock 会让首轮在单次 poll 内跑完，
        // 第二轮永远走快路，测不出锁的作用（去锁也 6 progress，假绿）。
        struct Yielding(MockHttp);
        #[async_trait::async_trait]
        impl UnlockHttp for Yielding {
            async fn request(&self, req: &UnlockRequest) -> UnlockResponse {
                tokio::task::yield_now().await;
                self.0.request(req).await
            }
        }
        let rt = runtime();
        let sink = RecordingSink::default();
        let h1 = Yielding(all_ok_mock());
        let h2 = Yielding(all_ok_mock());
        let (s1, s2) = tokio::join!(
            rt.run(&h1, &sink, false, || 1_000),
            rt.run(&h2, &sink, false, || 1_000),
        );
        assert_eq!(
            sink.progress_count(),
            ServiceId::ALL.len(),
            "单飞：只一轮 checker（6 progress，非并发双跑的 12）"
        );
        assert!(s1.checked_at.is_some());
        assert!(s2.checked_at.is_some(), "第二者命中第一者缓存（新鲜快照）");
        assert_eq!(s1.results.len(), ServiceId::ALL.len());
        assert_eq!(s2.results.len(), ServiceId::ALL.len());
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // T1 · invalidate → 去抖自跑（驱动层在 Rust 侧，不依赖渲染端 hook）
    // ══════════════════════════════════════════════════════════════════════════════

    /// **每次 invalidate 都排一轮自跑**，且 token 恒等于排程后的当前世代。
    ///
    /// 这条锁的是本批修的缺陷本体：迁移时只搬了 invalidate 的「作废 + 广播」半边，没搬「主进程自跑」半边
    /// ⇒ 六个徽章被置成检测中后无人调 run，永久转圈。
    ///
    /// **变异锁**：删 `invalidate` 末尾的 `sink.schedule_self_run(token)` → `self_runs` 恒空 → 转红
    ///（正是缺陷前的状态：广播了失效、没人重跑）。
    #[test]
    fn invalidate_schedules_self_run_with_current_token() {
        let rt = runtime();
        let sink = RecordingSink::default();

        rt.invalidate(&sink, true, false);
        assert_eq!(sink.self_runs().len(), 1, "invalidate 必须排一轮自跑");
        assert!(
            rt.self_run_token_current(sink.self_runs()[0]),
            "刚排的 token 必须是最新（否则定时器到点就会误判让位 → 一轮都不跑）"
        );

        rt.invalidate(&sink, true, false);
        let tokens = sink.self_runs();
        assert_eq!(tokens.len(), 2, "第二次 invalidate 再排一轮");
        assert!(
            tokens[1] > tokens[0],
            "token 必须单调递增（否则无法区分新旧排程）"
        );
    }

    /// **去抖合并**：窗内多次 invalidate → 只有**最后一次**的 token 仍是最新 → 只跑一轮。
    ///
    /// 「多次 invalidate 只跑一轮」在本实现里等价于「只有最后一个 token 通过 `self_run_token_current`」——
    /// 定时器本身不可取消（spawn 出去的 sleep），靠世代号让先前的排程到点后自行让位。
    ///
    /// **变异锁**：把 `self_run_token_current` 改成恒 `true`（等价于「去抖被删成直接跑」）→ 下方
    /// 「只有最后一个 token 当选」转红；把递增去掉（token 恒 0）→ 同样转红。
    #[test]
    fn self_run_debounce_coalesces_burst_of_invalidates() {
        let rt = runtime();
        let sink = RecordingSink::default();

        // 模拟起代理风暴：起核就绪 + 热切换 + 切节点连发三条 invalidate（真机上落在同一 1500ms 窗内）。
        for _ in 0..3 {
            rt.invalidate(&sink, true, false);
        }
        let tokens = sink.self_runs();
        assert_eq!(
            tokens.len(),
            3,
            "三次 invalidate 各排一轮（排程廉价，合并发生在到点复核）"
        );

        let survivors: Vec<u64> = tokens
            .iter()
            .copied()
            .filter(|t| rt.self_run_token_current(*t))
            .collect();
        assert_eq!(
            survivors,
            vec![*tokens.last().unwrap()],
            "去抖合并：只有最后一次 invalidate 排的那一轮真正开跑，其余到点让位"
        );
    }

    /// **epoch × 去抖的交互**：出口漂移丢弃腿必须**自带**一轮自跑排程，否则「修了触发还是不出终态」。
    ///
    /// 丢弃腿是唯一不 emit UPDATED 的返回路径（`run` 内 `self.invalidate(...)` 后返空快照）。若它不排自跑，
    /// 前端就停在检测中等一个永不到来的终态 —— 与本批修的主缺陷同形态，只是触发源不同。
    ///
    /// **变异锁**：把丢弃腿的 `self.invalidate(sink, ...)` 换成只 `bump_epoch()`（不走 invalidate）→
    /// `self_runs` 为空 → 转红。
    #[tokio::test]
    async fn discarded_round_schedules_a_rerun_and_next_round_emits_terminal() {
        let rt = runtime();
        let sink = RecordingSink::default();
        // 轮首 IP-A / 轮尾 IP-B → 归属校验失败 → 丢弃。
        let drift = all_ok_mock().egress_seq(vec![
            ok(200, "ip=1.1.1.1\nloc=US\n"),
            ok(200, "ip=9.9.9.9\nloc=US\n"),
        ]);
        let discarded = rt.run(&drift, &sink, false, || 1_000).await;
        assert!(discarded.checked_at.is_none(), "前提：本轮被丢弃");
        assert!(sink.updated().is_empty(), "前提：丢弃轮不 emit 终态");
        assert_eq!(
            sink.self_runs().len(),
            1,
            "丢弃腿必须排一轮自跑（否则前端永远等不到终态）"
        );
        assert!(
            rt.self_run_token_current(sink.self_runs()[0]),
            "该 token 应是最新 → 到点会真跑"
        );

        // 模拟自跑落地：出口稳定的一轮 → **必须真的 emit 出去**（「让最后一轮真的 emit」）。
        let snap = rt.run(&all_ok_mock(), &sink, false, || 2_000).await;
        assert!(snap.checked_at.is_some(), "重跑轮落终态");
        assert_eq!(
            sink.updated().len(),
            1,
            "重跑轮 emit UPDATED（终态送达前端）"
        );
        assert_eq!(sink.updated()[0].results.len(), ServiceId::ALL.len());
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // T1b · 出口漂移熔断（MAX_CONSECUTIVE_DRIFT）：掐断「丢弃 → 排自跑 → 再丢弃」的无界自持循环
    // ══════════════════════════════════════════════════════════════════════════════

    /// 出口**每次探测都换 IP** 的 http：轮首/轮尾必然不符 ⇒ **每一轮**都触发漂移丢弃腿。
    ///
    /// 真机对应形态：负载均衡 / urltest / WARP / 多 IP 出口 —— 出口 IP 轮换快过一轮检测。
    /// 复用 [`all_ok_mock`] 的 checker 脚本（checker 全 Ok），只接管 egress trace。
    struct EverDriftingHttp {
        inner: MockHttp,
        traces: std::sync::atomic::AtomicUsize,
    }
    impl EverDriftingHttp {
        fn new() -> Self {
            Self {
                inner: all_ok_mock(),
                traces: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }
    #[async_trait::async_trait]
    impl UnlockHttp for EverDriftingHttp {
        async fn request(&self, req: &UnlockRequest) -> UnlockResponse {
            // 只接管**出口** trace（`cloudflare.com/cdn-cgi/trace`）；checker 自带的
            // `chat.openai.com/cdn-cgi/trace` / `claude.ai/cdn-cgi/trace` 不含 "cloudflare.com"，不误伤。
            if req.url.contains("cloudflare.com/cdn-cgi/trace") {
                let n = self.traces.fetch_add(1, Ordering::SeqCst);
                return ok(200, &format!("ip=10.0.0.{}\nloc=US\n", n + 1));
            }
            self.inner.request(req).await
        }
    }

    /// **本轮最重要的一条**：出口持续漂移 → 连续 N 轮后熔断，**停止再排程且落终态**。
    ///
    /// 缺陷形态（熔断前）：丢弃腿调 `invalidate` → 排 1500ms 后自跑 → 那一轮重新探测、再次漂移、再次
    /// 丢弃 —— 永不收敛。每次迭代是完整 10s 预算的真实网络流量（6 个解锁端点 + 2 次 CF trace），且每次
    /// invalidate 广播 `{running:true}` → 前端 `beginUnlockCheck()` ⇒ **UI 永久钉在「检测中」**。
    ///
    /// **变异锁（逐条覆盖逃逸面，非单点 KILL）**：
    ///  - 删熔断整段（`streak >= MAX_CONSECUTIVE_DRIFT` 分支）→ 第 N 轮照旧丢弃 → ①②③ 三组断言转红；
    ///  - 把 `drift_streak` 的递增删掉（恒 0）→ 永不触发 → 同上转红；
    ///  - 在 `invalidate` 里清零 `drift_streak`（丢弃腿自己调 invalidate ⇒ 计数恒为 1）→ 永不触发 → 转红；
    ///  - 熔断轮漏掉 `sink.updated(&snapshot)` → ① 的「emit UPDATED」转红（UI 仍钉检测中，缺陷未修）；
    ///  - 熔断轮仍调 `invalidate`（继续排自跑）→ ② 转红（自持循环照旧）；
    ///  - 熔断快照标上 `egress`（把抖动中的某个 IP 当归属）→ ① 的 `egress.is_none()` 转红（归属不变式破）；
    ///  - 熔断快照落进 TTL 缓存 → ③ 转红（熔断变永久闩锁，下次真触发也读到垃圾快照）。
    #[tokio::test]
    async fn drift_circuit_breaker_commits_terminal_and_stops_self_run_loop() {
        let rt = runtime();
        let sink = RecordingSink::default();
        let http = EverDriftingHttp::new();
        // force + 每轮推进 20s：绕开 TTL 快路与 15s 硬下限，让每一轮都真跑到 bracket（本测的射程是
        // bracket 之后的熔断，不该被前面的早退路径挡住）。
        let clock = |round: u64| move || 20_000 * round;

        // ── 前 N-1 轮：照旧丢弃 + 排自跑（漂移多半是瞬态，值得重试；熔断不该提前开火）──
        for round in 1..MAX_CONSECUTIVE_DRIFT {
            let snap = rt.run(&http, &sink, true, clock(round)).await;
            assert!(
                snap.checked_at.is_none(),
                "第 {round} 轮（未到阈值 {MAX_CONSECUTIVE_DRIFT}）应照旧丢弃"
            );
            assert!(sink.updated().is_empty(), "第 {round} 轮不得 emit 终态");
            assert_eq!(
                sink.self_runs().len() as u64,
                round,
                "第 {round} 轮应照旧排一轮自跑（重试仍是对的）"
            );
        }

        // ── 第 N 轮：熔断 ──
        let snap = rt
            .run(&http, &sink, true, clock(MAX_CONSECUTIVE_DRIFT))
            .await;

        // ① 落终态 —— UI 脱离「检测中」的唯一出口。
        assert!(
            snap.checked_at.is_some(),
            "熔断轮必须落终态（否则 UI 永远钉在检测中，缺陷根本没修）"
        );
        assert_eq!(
            snap.results.len(),
            ServiceId::ALL.len(),
            "熔断轮如实带上已测到的结果（测了就是测了）"
        );
        assert_eq!(snap.low_confidence, Some(true), "熔断终态必须标低置信");
        assert!(
            snap.egress.is_none(),
            "归属不变式不得因熔断而破：出口在抖 → 结果不标给任何一个出口"
        );
        assert_eq!(
            sink.updated().len(),
            1,
            "熔断轮必须 emit UPDATED（前端据此收口）"
        );
        assert_eq!(
            sink.updated()[0].checked_at,
            snap.checked_at,
            "emit 出去的与返回的是同一份终态"
        );

        // ② 停止再排程 —— 熔断的核心：掐断自持循环。
        assert_eq!(
            sink.self_runs().len() as u64,
            MAX_CONSECUTIVE_DRIFT - 1,
            "熔断轮不得再排自跑（否则循环照旧无界自持，UI 照旧永钉检测中）"
        );

        // ③ 低置信不入 TTL 缓存 → 下一次真触发照常重检（熔断掐的是循环，不是把检测永久闩死）。
        assert!(
            rt.peek(20_000 * MAX_CONSECUTIVE_DRIFT).is_none(),
            "低置信终态不得入缓存（否则熔断变成 30min 永久闩锁）"
        );

        // ④ 计数已随落定清零：出口恢复稳定的下一轮照常 commit，不受熔断残留影响。
        let stable = rt
            .run(&all_ok_mock(), &sink, true, || {
                20_000 * (MAX_CONSECUTIVE_DRIFT + 1)
            })
            .await;
        assert!(stable.checked_at.is_some(), "熔断后出口转稳 → 照常落终态");
        assert!(stable.egress.is_some(), "出口稳定 → 正常归属");
        assert_eq!(stable.low_confidence, None, "稳定轮不是低置信");
    }

    /// **间歇漂移不得触发熔断**：漂移被任一次成功 commit 打断后计数清零，「连续 N 轮」按字面算。
    ///
    /// 没有这条，熔断会退化成「累计 N 次漂移就闭嘴」——偶发漂移的健康出口用久了也会被误熔断。
    ///
    /// **变异锁**：删掉 bracket 通过后的 `drift_streak.store(0, ...)` → 三次分散漂移累加到 3 → 末轮
    /// 变成熔断轮（`low_confidence==Some(true)` 且 `egress` 为 None）→ 转红。
    #[tokio::test]
    async fn intermittent_drift_never_trips_the_breaker() {
        let rt = runtime();
        let sink = RecordingSink::default();
        let mut t = 0u64;
        let mut next = || {
            t += 20_000; // 每轮推进 20s：绕 TTL 与 15s 硬下限
            t
        };

        // 漂移 → 稳定 → 漂移 → 漂移 → 稳定：漂移总数 3（= 阈值），但从未**连续** 3 轮。
        for stable in [false, true, false, false, true] {
            let at = next();
            let snap = if stable {
                rt.run(&all_ok_mock(), &sink, true, || at).await
            } else {
                rt.run(&EverDriftingHttp::new(), &sink, true, || at).await
            };
            if stable {
                assert!(snap.checked_at.is_some(), "稳定轮应正常 commit");
                assert!(snap.egress.is_some(), "稳定轮正常归属");
                assert_eq!(snap.low_confidence, None, "稳定轮不是低置信");
            } else {
                assert!(
                    snap.checked_at.is_none(),
                    "漂移轮应丢弃，而非被误判成熔断终态"
                );
            }
        }
    }

    /// **#2 · 丢弃腿保留 `last_run_at`** ⇒ force 15s 硬下限在漂移出口上仍然武装。
    ///
    /// 缺陷形态：丢弃腿调**裸** `invalidate` → `last_run_at` 归零 ⇒ force 硬下限的 `last_at != 0` 守卫失效；
    /// 且丢弃腿不 emit UPDATED ⇒ 前端 `unlock.lastRunAt` 停在陈旧/null ⇒ `unlockCooldown` 也永不武装。
    /// 于是在漂移出口上刷新按钮**两侧都不受限流** —— 恰好是后端已在自跑、对端限频风险最高的时候。
    ///
    /// **变异锁（两处逃逸面各一条）**：
    ///  - 把 `invalidate_keep_run_at` 换回裸 `self.invalidate(...)` → `last_at==0` → 5s 后的 force 被放行
    ///    重跑 → `progress_count` 增长 → 转红；
    ///  - 把 force 硬下限改回「无 `last_snapshot` 就落空放行」（丢弃腿已清 last_snapshot，正是此形态）
    ///    → 同样放行重跑 → 同一条断言转红。
    #[tokio::test]
    async fn discard_leg_keeps_force_min_armed() {
        let rt = runtime();
        let sink = RecordingSink::default();
        let drift = all_ok_mock().egress_seq(vec![
            ok(200, "ip=1.1.1.1\nloc=US\n"),
            ok(200, "ip=9.9.9.9\nloc=US\n"),
        ]);

        // 首轮 force：真跑一整轮（就绪门 + 6 checker + 2 trace）后因出口漂移丢弃。
        let discarded = rt.run(&drift, &sink, true, || 10_000).await;
        assert!(discarded.checked_at.is_none(), "前提：本轮被丢弃");
        let after_first = sink.progress_count();
        assert_eq!(
            after_first,
            ServiceId::ALL.len(),
            "前提：本轮真跑过 checker（不是零网络早退）"
        );

        // 5s 后连点 force（<15s）→ 必须被硬下限挡住：**丢弃 ≠ 没跑过网络**。
        rt.run(&all_ok_mock(), &sink, true, || 15_000).await;
        assert_eq!(
            sink.progress_count(),
            after_first,
            "丢弃腿必须保留 lastRunAt 且限流不依赖 last_snapshot：否则漂移出口上刷新钮永不限流"
        );

        // ≥15s 后照常放行 —— 保留 lastRunAt 只是不清零，不是把闸门焊死。
        rt.run(&all_ok_mock(), &sink, true, || 10_000 + FORCE_MIN_MS)
            .await;
        assert_eq!(
            sink.progress_count(),
            after_first * 2,
            "≥15s 照常放行重跑（闸门是限流，不是熔断）"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // T2 · 整轮 deadline（10s，就绪门 + 主轮 + settle-retry 共享）
    // ══════════════════════════════════════════════════════════════════════════════

    /// **deadline 截断就绪门**：egress 恒探不到时，默认 10s 预算在第 5 攻越界收口，**不跑满 7 攻 19.6s**。
    ///
    /// 算术（`start_paused` 虚拟时钟，mock 探测零耗时）：attempt0 @0 → 1 @1.2s → 2 @2.4s → 3 @3.6s →
    /// 4 @7.6s；attempt5 需再退避 4s（7.6+4=11.6s ≥ 10s）⇒ 停。共 **5** 次探测。
    ///
    /// **变异锁**：删 deadline（`probe_ready` 里去掉两处 deadline 判）→ 跑满 7 攻 → 计数与耗时双双转红。
    #[tokio::test(start_paused = true)]
    async fn round_deadline_truncates_readiness_gate() {
        let rt = runtime();
        let sink = RecordingSink::default();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = count.clone();
        let http = MockHttp::new().egress_seq(vec![ok(503, "")]).hook(move || {
            c2.fetch_add(1, Ordering::SeqCst);
        });

        let t0 = tokio::time::Instant::now();
        let snap = rt.run(&http, &sink, false, || 1_000).await;
        let elapsed = t0.elapsed();

        assert_eq!(
            count.load(Ordering::SeqCst),
            5,
            "10s 预算下就绪门在第 5 攻越界收口（不空等到 19.6s）"
        );
        assert!(
            elapsed < Duration::from_millis(TOTAL_DETECTION_BUDGET_MS),
            "整轮不得超过 deadline（实测 {elapsed:?}）"
        );
        // **deadline 到点写终态**，不是撒手不管。
        assert_eq!(
            snap.not_ready,
            Some(true),
            "预算耗尽 → notReady 终态（不留检测中挂着）"
        );
        assert_eq!(sink.updated().len(), 1, "终态必须 emit（前端据此复位）");
    }

    /// **deadline 到点写终态（核心）**：所有 checker 卡死远超预算 → 整轮在 deadline 处收口，
    /// **六项全部落 `Timeout` 终态**、快照照常 commit + emit —— 绝不留 `Checking` 挂着。
    ///
    /// 这条正面锁住用户报的症状：「一直在检测中没有最终结果」。
    ///
    /// **变异锁**（假绿形态）：
    /// - 把 `run_checkers_budgeted` 的 `timeout_at(cap, …)` 改回 `timeout(CHECKER_BUDGET_MS, …)`
    ///   → 耗时 15s+ → `elapsed` 断言转红；
    /// - deadline 到点直接 `return` 不 commit → `updated` 为空 + `results` 不足 6 → 转红。
    #[tokio::test(start_paused = true)]
    async fn round_deadline_writes_terminal_results_never_leaves_checking() {
        /// egress trace 秒回（就绪门立刻过），其余 checker 全部卡死远超整轮预算。
        struct AllHang;
        #[async_trait::async_trait]
        impl UnlockHttp for AllHang {
            async fn request(&self, req: &UnlockRequest) -> UnlockResponse {
                if req.url.contains("cloudflare.com/cdn-cgi/trace") {
                    return ok(200, "ip=1.1.1.1\nloc=US\n");
                }
                tokio::time::sleep(Duration::from_secs(600)).await;
                ok(200, "{}")
            }
        }
        let rt = runtime();
        let sink = RecordingSink::default();

        let t0 = tokio::time::Instant::now();
        let snap = rt.run(&AllHang, &sink, false, || 1_000).await;
        let elapsed = t0.elapsed();

        assert_eq!(
            snap.results.len(),
            ServiceId::ALL.len(),
            "六项都要有终态（不缺席）"
        );
        for (id, r) in &snap.results {
            assert_eq!(
                r.status,
                UnlockStatus::Timeout,
                "service {id} 必须落 Timeout 终态，绝不停在 Checking"
            );
        }
        assert_eq!(
            sink.updated().len(),
            1,
            "deadline 到点仍 commit + emit 终态快照"
        );
        // 上限 = deadline + MIN_OP_BUDGET_MS（轮尾确认探的 floor）+ 少量调度余量。
        assert!(
            elapsed < Duration::from_millis(TOTAL_DETECTION_BUDGET_MS + 2 * MIN_OP_BUDGET_MS),
            "整轮须在 deadline(+MIN_OP floor) 内收口，实测 {elapsed:?}（无 deadline 时单 checker 就要 15s）"
        );
    }

    /// **deadline 不误伤健康轮**：预算充裕时行为与加 deadline 前逐项一致（全 Ok、正常 commit、入缓存）。
    /// 防「为了限时把正常路径也砍了」的过度修复。
    #[tokio::test]
    async fn round_deadline_does_not_affect_fast_healthy_round() {
        let rt = runtime();
        let sink = RecordingSink::default();
        let snap = rt.run(&all_ok_mock(), &sink, false, || 1_000).await;
        assert_eq!(snap.results.len(), ServiceId::ALL.len());
        assert!(snap.results.values().all(|r| r.status == UnlockStatus::Ok));
        assert!(rt.peek(1_000).is_some(), "健康轮照常入缓存");
    }
}
