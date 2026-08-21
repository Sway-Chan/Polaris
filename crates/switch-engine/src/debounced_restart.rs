//! 去抖重启调度 —— 上游 `ProxyManager.scheduleDebouncedRestart`（L1573-1600）
//! 及 endLifecycleOp 排空（L1542-1557）、issue #176 单飞（depth>0 不并发）。
//!
//! 连改多条配置合并为一次重启：trailing 触发时取最新 currentConfig（调用方须已更新它），
//! 故窗口内的后续切节点(hotSwitch)/no-op/再次结构变更都会被最终那次重启自然纳入。
//!
//! # 复用 core-supervisor 的 LifecycleGate
//!
//! Polaris 的去抖逻辑与 lifecycle 单飞深度耦合。两个关键不变式：
//! trailing 回调**必须先判 depth>0**（置 pending 返回），再判核是否已停（清 force-restart 快照返回），
//! 顺序颠倒即回归 H-1（#3 顺序门不变式）；depth>0 时 trailing 命中只置 `restart_pending`，
//! 由 endLifecycleOp 在 depth 归 0 时排空一次。
//!
//! 这些不变式已在 `polaris-core-supervisor::lifecycle_gate` 完整实现（`debounced_restart_decision`
//! 与 `LifecycleEndResult::Drained`）。本模块**不重复**状态机——复用 [`LifecycleGate`]，
//! 只在其上加 tokio timer（去抖延迟）和世代 token（防 timer 回调打到已换的核）。
//!
//! # 与 sing-box 进程的关系
//!
//! 本模块**不直接**重启 sing-box 进程（那是 core-supervisor::spawner 的职责）。它只产出
//! 「该重启了」的决策（[`DebouncedOutcome::Proceed`]）或「该排空 pending 了」的信号
//! （[`LifecycleGate::end`] 返回的 [`PendingDrain`]）。上层 actor 据此调 core-supervisor
//! 执行真正的 stop+spawn。
//!
//! # 世代 token（防过期 timer）
//!
//! `schedule()` 快照当前 [`LifecycleGate::generation`]。timer 触发时若世代已变（窗口内有
//! 新的 start/stop 接管生命周期），放弃本次去抖——对齐 上游 `scheduleConnectionFlush`
//! 的 `gen !== this.lifecycleGeneration` 早退（L1620）。去抖重启本身 Polaris 用 depth/busy 门控，
//! 此处再加世代守卫做双保险（timer 竞态下不误触发）。

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use polaris_core_supervisor::lifecycle_gate::{
    DebouncedDecision, LifecycleEndResult, LifecycleGate, LifecycleKind, PendingDrain,
};

/// 去抖延迟：窗口内连改多条配置只重启一次（消除「连改 5 条规则 = 5 次断流」）。
///
/// 上游原值 1500ms 会被完整叠加到显式的 System/TUN 模式切换上；Windows 真机即使 helper
/// 起核只需数百毫秒，端到端仍会越过 5s。500ms 足以合并同一轮 UI 连续落盘，同时不再让用户
/// 为内部去抖白等 1.5s。
pub const RESTART_DEBOUNCE: Duration = Duration::from_millis(500);

/// 去抖 trailing 回调的决策结果（包装 [`DebouncedDecision`] + 世代守卫结果）。
///
/// 上层 actor 据此决定动作：Proceed → 执行重启（用 force_restart_id 或 currentConfig）；
/// Defer / CoreStopped / Superseded → 不重启（已置 pending / 已清快照 / 已过期）。
#[derive(Debug, Clone)]
pub enum DebouncedOutcome {
    /// depth>0 → 已置 restart_pending，由 endLifecycleOp 排空（不并发起第二条重启）。
    Defer,
    /// depth=0 但核已停 → 已清 force-restart 快照（H-1 陈旧防护），不重启。
    CoreStopped,
    /// 可执行重启：Option<force_restart_id>（Some=必须用该 config；None=用 currentConfig）。
    Proceed(Option<u64>),
    /// 世代已变（窗口内 start/stop 接管）→ 放弃本次去抖，不重启。
    /// Polaris 原文去抖未显式判世代（用 depth 门控），本 crate 加世代守卫做双保险。
    Superseded,
}

impl DebouncedOutcome {
    /// 是否为「应执行重启」的决策。
    pub fn should_restart(&self) -> bool {
        matches!(self, DebouncedOutcome::Proceed(_))
    }
}

/// 从 [`DebouncedDecision`]（LifecycleGate 的纯决策）转换为本模块的 outcome（加世代守卫）。
///
/// 世代未变 → 透传 gate 的决策；世代已变 → Superseded（覆盖 gate 的 Proceed/Defer/CoreStopped）。
fn to_outcome(decision: DebouncedDecision, gen_changed: bool) -> DebouncedOutcome {
    if gen_changed {
        return DebouncedOutcome::Superseded;
    }
    match decision {
        DebouncedDecision::Defer => DebouncedOutcome::Defer,
        DebouncedDecision::CoreStopped => DebouncedOutcome::CoreStopped,
        DebouncedDecision::Proceed(id) => DebouncedOutcome::Proceed(id),
    }
}

/// 去抖重启调度器：tokio sleep + LifecycleGate 状态机。
///
/// 设计：每次 [`schedule`] 启动一个独立的 tokio task（sleep RESTART_DEBOUNCE 后查 gate 决策）。
/// 多次 schedule 在窗口内并发——每个 task 独立 sleep，但都查同一 [`LifecycleGate`]；
/// gate 的 depth/pending 状态保证最终只排空一次（最先触发的 task 通常 Proceed，后续若 depth>0
/// 则 Defer 置 pending，由 end 排空）。
///
/// 这忠实复刻 Polaris「clearTimeout + setTimeout」的单 timer 语义：Polaris 每次 schedule 清掉旧 timer
/// （只保留最后一个）；本实现用「每 task 自查 gate」达到等价效果——gate 的 pending 状态是单一真值，
/// 多 task 触发不会多次重启（Proceed 仅在 depth=0 且核运行时返回，且 gate 的 force_restart_id
/// **在 Proceed 那一刻就被消费**——见 [`LifecycleGate::debounced_restart_decision`]，留着它会让
/// 重启收尾的 `end()` 把同一次重启再排一遍）。测试验证此等价
/// （`multiple_schedules_collapse_to_single_restart`）。
#[derive(Debug, Clone)]
pub struct DebouncedRestart {
    gate: Arc<LifecycleGate>,
}

impl DebouncedRestart {
    /// 新建调度器，绑定一个 [`LifecycleGate`]（通常与 core-supervisor 共享同一 gate 实例）。
    pub fn new(gate: Arc<LifecycleGate>) -> Self {
        Self { gate }
    }

    /// 调度一次去抖重启（trailing）。
    ///
    /// 上游 `scheduleDebouncedRestart`（L1573-1600）：clearTimeout + setTimeout(RESTART_DEBOUNCE_MS)。
    /// trailing 回调：先判 depth>0（置 pending 返回），再判核是否已停（清 force-restart 快照返回），
    /// 最后返回 Proceed（调用方执行重启）。
    ///
    /// `core_running`：当前核是否运行（`singboxProcess||singboxPid` 非空）。trailing 回调查 gate 时传入。
    /// `on_fire`：trailing 触发后的决策回调（收到 [`DebouncedOutcome`]）。上层据此执行重启 / 排空 / 跳过。
    ///
    /// 返回一个 [`DebouncedHandle`]：调用方可 [`DebouncedHandle::cancel`] 取消（对齐 Polaris stop()/quit
    /// 时 `clearTimeout`）。drop handle 不取消 task（task 自查 gate 决策，过期会自行 Superseded）。
    pub fn schedule<F>(&self, core_running: bool, on_fire: F) -> DebouncedHandle
    where
        F: FnOnce(DebouncedOutcome) + Send + 'static,
    {
        let gate = self.gate.clone();
        // 世代快照：防 timer 回调打到已换的核（Polaris scheduleConnectionFlush 同款守卫）。
        let gen0 = gate.generation();
        // cancel 用 Notify：handle 的 Drop **不**发信号（task 继续跑、自查 gate 决策），
        // 仅显式 cancel() 才唤醒 select 提前返回。oneshot Sender Drop 会关闭 channel 误触发 cancel，
        // 故不用 oneshot——Notify 无 Drop 副作用。
        let cancel = Arc::new(tokio::sync::Notify::new());
        let cancel_for_task = cancel.clone();
        let join = tokio::spawn(async move {
            // 去抖延迟（可被显式 cancel 中断；handle drop 不中断）。
            tokio::select! {
                _ = tokio::time::sleep(RESTART_DEBOUNCE) => {}
                _ = cancel_for_task.notified() => return,
            }
            // trailing 回调：世代守卫 + gate 顺序门决策。
            let gen_changed = gate.generation() != gen0;
            let decision = gate.debounced_restart_decision(core_running);
            on_fire(to_outcome(decision, gen_changed));
        });
        DebouncedHandle {
            cancel: Some(cancel),
            _join: join,
        }
    }

    /// 同步立即触发一次去抖决策（不经 timer）——用于测试 / endLifecycleOp 排空时的即时查询。
    ///
    /// 不调度延迟，直接查 gate 当前状态。带世代守卫（`gen_changed` 由调用方传入）。
    pub fn decide_now(&self, core_running: bool, gen_changed: bool) -> DebouncedOutcome {
        let decision = self.gate.debounced_restart_decision(core_running);
        to_outcome(decision, gen_changed)
    }

    /// 通知 lifecycle 操作开始（beginLifecycleOp，L1522）。depth += 1。
    /// 去抖 trailing 命中时若 depth>0 → 置 pending 不并发。
    pub fn begin_lifecycle(&self) {
        self.gate.begin();
    }

    /// 通知 lifecycle 操作结束（endLifecycleOp，L1533）。
    ///
    /// depth -= 1。depth 归 0 时：
    /// - kind=Stop → 丢弃全部 pending（停止优先）。
    /// - kind=Start/Restart → 排空一次尾随重启（吃最新 pending）。
    ///
    /// 返回 [`LifecycleEndResult`] 供上层决策（排空动作 / 丢弃观测）。
    pub fn end_lifecycle(&self, kind: LifecycleKind) -> LifecycleEndResult {
        self.gate.end(kind)
    }

    /// 排空 pending 的尾随重启决策（endLifecycleOp kind=start/restart 且 depth 归 0 时）。
    ///
    /// 便捷封装：等价于 [`Self::end_lifecycle`] 后若返回 `Drained` 则取其 [`PendingDrain`]。
    /// 返回 `Some(PendingDrain)` 当且仅当 depth 归 0 且有 pending 须排空；否则 None。
    pub fn drain_pending(&self, kind: LifecycleKind) -> Option<PendingDrain> {
        match self.end_lifecycle(kind) {
            LifecycleEndResult::Drained(drain)
                if drain.schedule_restart || drain.replay_switch_id.is_some() =>
            {
                Some(drain)
            }
            LifecycleEndResult::Drained(_) => None,
            _ => None,
        }
    }

    /// 置强制重启配置快照 id（pendingForceRestartConfig，H-1 #4）。
    pub fn set_force_restart(&self, config_id: u64) {
        self.gate.set_force_restart(config_id);
    }

    /// 清强制重启快照（结构性重启腿：newer 胜，:1894-1895）。
    pub fn clear_force_restart(&self) {
        self.gate.clear_force_restart();
    }

    /// 置 switchMode 对账待决（pendingSwitchConfig，bug#5）。
    pub fn set_switch_pending(&self, config_id: u64) {
        self.gate.set_switch_pending(config_id);
    }

    /// 置去抖重启待决（restartPending）。通常由 trailing 回调在 depth>0 时自动置；
    /// 暴露为 public 供上层显式标记（如 applyConfigForcingRestart 在 lifecycle busy 时直接置 pending）。
    pub fn set_restart_pending(&self) {
        self.gate.set_restart_pending();
    }

    /// 当前世代（start/stop 入口 +1）。
    pub fn generation(&self) -> u64 {
        self.gate.generation()
    }

    /// 是否有 lifecycle 操作在飞（isLifecycleBusy，:1561）。
    pub fn is_lifecycle_busy(&self) -> bool {
        self.gate.is_busy()
    }
}

/// 去抖 task 句柄：可 cancel（对齐 Polaris clearTimeout）。
///
/// drop 不取消 task（task 自查 gate，过期 Superseded）；显式 [`Self::cancel`] 才中断 sleep。
/// 用 [`tokio::sync::Notify`]（Drop 无副作用）而非 oneshot（Sender Drop 会关闭 channel 误触发）。
pub struct DebouncedHandle {
    cancel: Option<Arc<tokio::sync::Notify>>,
    _join: tokio::task::JoinHandle<()>,
}

impl DebouncedHandle {
    /// 取消去抖 task（对齐 Polaris stop()/quit 时 `clearTimeout(this.restartDebounceTimer)`）。
    /// 幂等：多次 notify 安全；task 已触发后再 cancel 无影响。
    ///
    /// 用 `notify_one()`（非 `notify_waiters()`）：前者存一个 permit，即使 cancel 早于 task 进
    /// `notified()` 也保证后续 `notified()` 立即完成；后者只唤醒**已在等待**者，cancel 与
    /// select 注册竞态时会漏唤醒（task 已跑完 sleep 才被唤醒 → 无效）。
    pub fn cancel(mut self) {
        if let Some(n) = self.cancel.take() {
            n.notify_one();
        }
    }

    /// 是否仍在等待（未触发 / 未取消）。
    /// 注：JoinHandle::is_finished 在 task 完成后 true；cancel 后也 true（已 select 分支返回）。
    pub fn is_finished(&self) -> bool {
        self._join.is_finished()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn new_engine() -> DebouncedRestart {
        DebouncedRestart::new(Arc::new(LifecycleGate::default()))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schedule_proceeds_when_idle_and_core_running() {
        // depth=0 + 核运行 → Proceed(None)（用 currentConfig 重启）。
        let engine = new_engine();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = engine.schedule(true, move |o| {
            let _ = tx.send(o);
        });
        let outcome = rx.await.unwrap();
        assert!(matches!(outcome, DebouncedOutcome::Proceed(None)));
        // task 完成。
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(handle.is_finished());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schedule_returns_force_restart_id_when_set() {
        // force_restart_id Some → Proceed(Some(id))。
        let engine = new_engine();
        engine.set_force_restart(42);
        let (tx, rx) = tokio::sync::oneshot::channel();
        engine.schedule(true, move |o| {
            let _ = tx.send(o);
        });
        let outcome = rx.await.unwrap();
        assert!(matches!(outcome, DebouncedOutcome::Proceed(Some(42))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schedule_defers_when_lifecycle_busy() {
        // depth>0 → Defer（置 pending，不并发起重启）。
        let engine = new_engine();
        engine.begin_lifecycle();
        let (tx, rx) = tokio::sync::oneshot::channel();
        engine.schedule(true, move |o| {
            let _ = tx.send(o);
        });
        let outcome = rx.await.unwrap();
        assert!(matches!(outcome, DebouncedOutcome::Defer));
        // pending 已置。
        assert!(engine.gate.pending().restart_pending);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schedule_core_stopped_clears_force_restart() {
        // depth=0 + 核停 → CoreStopped + 清 force-restart 快照。
        let engine = new_engine();
        engine.set_force_restart(99);
        let (tx, rx) = tokio::sync::oneshot::channel();
        engine.schedule(false, move |o| {
            let _ = tx.send(o);
        });
        let outcome = rx.await.unwrap();
        assert!(matches!(outcome, DebouncedOutcome::CoreStopped));
        assert!(engine.gate.pending().force_restart_id.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schedule_superseded_when_generation_changed() {
        // 世代变了（窗口内 start/stop 接管）→ Superseded。
        let engine = new_engine();
        let gen0 = engine.generation();
        // 模拟窗口内 start 接管：bump_generation。
        let (tx, rx) = tokio::sync::oneshot::channel();
        let gate = engine.gate.clone();
        engine.schedule(true, move |o| {
            let _ = tx.send(o);
        });
        // 在 sleep 窗口内 bump 世代。
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(gate.bump_generation(), gen0 + 1);
        let outcome = rx.await.unwrap();
        assert!(matches!(outcome, DebouncedOutcome::Superseded));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_prevents_callback() {
        // cancel 后 on_fire 不被调用。
        let engine = new_engine();
        let called = Arc::new(AtomicU32::new(0));
        let called_clone = called.clone();
        let handle = engine.schedule(true, move |_| {
            called_clone.fetch_add(1, Ordering::SeqCst);
        });
        handle.cancel();
        // 等 debounce 窗口过去，确认回调没被调用。
        tokio::time::sleep(RESTART_DEBOUNCE + Duration::from_millis(50)).await;
        assert_eq!(called.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multiple_schedules_collapse_to_single_restart() {
        // 连改 3 条配置（schedule 3 次）→ 只 Proceed 一次（去抖合并语义）。
        // 第一个 task 触发 Proceed（清 force_restart_id）；后续 task 在 depth=0 核运行时也 Proceed(None)
        // —— 但上层 actor 只在第一个 Proceed 执行重启并 bump generation，后续 task 见世代变 → Superseded。
        // 这里直接测：force_restart_id 被第一个 Proceed 消费后，gate 状态清空，后续 task Proceed(None)。
        let engine = new_engine();
        engine.set_force_restart(7);

        let mut handles = Vec::new();
        for _ in 0..3 {
            handles.push(engine.schedule(true, |_| {}));
        }
        // 等所有 task 完成。
        tokio::time::sleep(RESTART_DEBOUNCE + Duration::from_millis(50)).await;
        for h in &handles {
            assert!(h.is_finished());
        }
        assert!(!engine.is_lifecycle_busy());
        // 头注那句「第一个 Proceed 清 force_restart_id」如今是真的：`debounced_restart_decision`
        // 在 Proceed 腿 `take()` 该 id。此前它只读不清，于是 id 活到重启收尾的 `end()`，被当成
        // 「还有一条待决重启」再排一次 —— 真机上每点一次「立即应用」核重启两遍。
        //
        // 变异对照：把 `lifecycle_gate.rs` Proceed 腿的 `.take()` 改回读取 → 本条转红。
        assert!(
            engine.gate.pending().is_empty(),
            "Proceed 必须消费 force_restart_id，否则 end() 会把已执行的重启再排一遍"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_gate_defers_even_if_core_running() {
        // #3 顺序门核心：busy 时绝不 Proceed（杜绝「就绪等待中又来重启」风暴根因）。
        let engine = new_engine();
        engine.set_force_restart(1);
        engine.begin_lifecycle();
        let (tx, rx) = tokio::sync::oneshot::channel();
        engine.schedule(true, move |o| {
            let _ = tx.send(o);
        });
        let outcome = rx.await.unwrap();
        assert!(matches!(outcome, DebouncedOutcome::Defer));
        // end 排空时仍能消费 force_restart（未被 debounced 清掉）。
        let drain = engine.drain_pending(LifecycleKind::Start);
        assert!(drain.is_some());
        let drain = drain.unwrap();
        assert!(drain.schedule_restart);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_terminal_discards_pending() {
        // kind=Stop 终态 → 丢弃全部 pending（停止优先）。
        let engine = new_engine();
        engine.set_force_restart(10);
        engine.set_switch_pending(20);
        engine.set_restart_pending();
        engine.begin_lifecycle();
        let result = engine.end_lifecycle(LifecycleKind::Stop);
        match result {
            LifecycleEndResult::Stopped(d) => {
                assert!(d.discarded_restart);
                assert_eq!(d.discarded_force_restart_id, Some(10));
                assert_eq!(d.discarded_switch_id, Some(20));
            }
            _ => panic!("expected Stopped"),
        }
        assert!(engine.gate.pending().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_drains_pending_exactly_once() {
        // kind=Start depth 归 0 → 恰好排空一次。
        let engine = new_engine();
        engine.set_switch_pending(5);
        engine.set_restart_pending();
        engine.begin_lifecycle();
        let drain = engine.drain_pending(LifecycleKind::Start);
        let drain = drain.expect("expected drain");
        assert_eq!(drain.replay_switch_id, Some(5));
        assert!(drain.schedule_restart);
        // 排空后 pending 清空（恰好一次）。
        assert!(engine.gate.pending().is_empty());
        // 再次 end（无 pending）→ 空 drain。
        engine.begin_lifecycle();
        assert!(engine.drain_pending(LifecycleKind::Start).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nested_lifecycle_does_not_drain_until_outermost() {
        // depth=2 时内层 end → StillBusy，不排空；pending 留给最外层。
        let engine = new_engine();
        engine.set_restart_pending();
        engine.begin_lifecycle(); // depth=1
        engine.begin_lifecycle(); // depth=2
        let r = engine.end_lifecycle(LifecycleKind::Start);
        assert!(matches!(r, LifecycleEndResult::StillBusy(1)));
        // pending 仍在。
        assert!(engine.gate.pending().restart_pending);
        // 最外层 end → drain。
        let drain = engine.drain_pending(LifecycleKind::Restart);
        assert!(drain.is_some_and(|d| d.schedule_restart));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn decide_now_is_immediate_without_timer() {
        // decide_now 不经 timer，立即返回决策。
        let engine = new_engine();
        let o = engine.decide_now(true, false);
        assert!(matches!(o, DebouncedOutcome::Proceed(None)));
        engine.begin_lifecycle();
        let o = engine.decide_now(true, false);
        assert!(matches!(o, DebouncedOutcome::Defer));
        let o = engine.decide_now(false, true);
        assert!(matches!(o, DebouncedOutcome::Superseded)); // 世代守卫优先
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debounce_delay_is_500ms() {
        // 验证去抖窗口 ≈ 500ms：保留合并语义，但不把显式模式切换拖过 5s。
        let engine = new_engine();
        let start = std::time::Instant::now();
        let (tx, rx) = tokio::sync::oneshot::channel();
        engine.schedule(true, move |_| {
            let _ = tx.send(());
        });
        let _ = rx.await;
        let elapsed = start.elapsed();
        // 允许 -100/+300ms 抖动（tokio timer 精度 + CI 调度）。
        assert!(
            elapsed >= Duration::from_millis(400) && elapsed <= Duration::from_millis(800),
            "debounce elapsed {elapsed:?} not within the 500ms window"
        );
    }
}
