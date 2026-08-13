//! 生命周期单飞守卫 —— 上游 `ProxyManager` 生命周期世代 + lifecycle_depth 单飞逻辑的纯逻辑移植
//! （维度7 #1/#2/#3/#4，源 `ProxyManager.ts:1522/1533/1573/364`）。
//!
//! 本模块只做「并发串行化的不变式机」，不持有进程/IPC/系统代理——那部分由上层 actor 编排。状态机可单测：
//! - [`LifecycleGate::begin`] / [`LifecycleGate::end`] = `beginLifecycleOp`/`endLifecycleOp`（:1522/:1533）。
//! - [`LifecycleGate::bump_generation`] = `lifecycleGeneration++`（start/stop 入口，:632/:1347）。
//! - [`LifecycleGate::generation`] = 世代快照供 readiness/recovery 比对让位（#2/#5/#6）。
//! - [`PendingSnapshot`] / [`StopDiscard`] / [`PendingDrain`] = depth>0 时变更暂存 + depth 归零排空 + stop 终态丢弃。
//!
//! 不变式（capability-registry-special-logic.md §1 #1/#3/#4）：
//! 1. depth>0 时新 lifecycle 变更只置 pending；depth 归 0 时**恰好排空一次**。
//! 2. kind=stop（终态停止）**必须丢弃全部 pending**（停止优先，不得停后又被拉起）。
//! 3. 去抖回调**必须先判 depth>0**（置 pending 返回），再判核是否已停（清 force-restart 快照返回）；顺序颠倒即回归。

use std::sync::Mutex;

/// lifecycle 操作种类（endLifecycleOp kind 参数，:1533）。
///
/// - `Start` / `Restart` 收尾时若 depth 归 0 且有 pending → 排空一次尾随重启。
/// - `Stop` 是终态 → depth 归 0 时丢弃全部 pending（:1536-1540）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleKind {
    Start,
    Stop,
    Restart,
}

/// 待决变更快照（issue #176 三轴：去抖重启 / 强制重启配置 / switchMode 对账）。
///
/// Polaris 用三个独立布尔/字段（restartPending / pendingForceRestartConfig / pendingSwitchConfig），
/// 本 crate 不耦合 UserConfig 类型，故泛化成「待决变更是否存在」+ 调用方持有的不透明载荷 id。
/// 上层用 id 关联到真正的 config 快照——本模块只管「是否要排空 / 是否要丢弃」的时序不变式。
#[derive(Debug, Default, Clone)]
pub struct PendingSnapshot {
    /// 去抖重启待决（restartPending，:1573）。
    pub restart_pending: bool,
    /// 强制重启配置快照 id（pendingForceRestartConfig，H-1 栈式 stale 快照防护）。
    /// `Some` 表示有一条「必须用这份 config 重启」的待决项；排空时调用方据此取最新 config。
    pub force_restart_id: Option<u64>,
    /// switchMode 对账待决（pendingSwitchConfig，bug#5）。
    pub switch_id: Option<u64>,
}

impl PendingSnapshot {
    /// 全空（无任何待决）。
    pub fn is_empty(&self) -> bool {
        !self.restart_pending && self.force_restart_id.is_none() && self.switch_id.is_none()
    }

    /// 清空全部三个轴（stop 终态丢弃 / drain 后复位，:1536-1540）。
    /// 注：end() 用 mem::take 复位，本方法保留为单一真值供外部显式清空（如诊断复位）。
    #[allow(dead_code)]
    fn clear(&mut self) {
        self.restart_pending = false;
        self.force_restart_id = None;
        self.switch_id = None;
    }
}

/// 终态 stop 丢弃的待决项（endLifecycleOp kind=stop 分支返回，便于上层观测丢弃了什么）。
#[derive(Debug, Default, Clone)]
pub struct StopDiscard {
    pub discarded_restart: bool,
    pub discarded_force_restart_id: Option<u64>,
    pub discarded_switch_id: Option<u64>,
}

/// depth 归 0 排空时返回的动作指令（endLifecycleOp kind=start/restart 分支，:1542-1557）。
///
/// 调用方据此决定排空动作的顺序：先重放 switch（其内部分流热切/重启），再排空尾随重启（与 switch 的重启
/// 经去抖天然合并为一次）。对应 TS 的 `void this.switchMode(...)` + `scheduleDebouncedRestart()`。
#[derive(Debug, Default, Clone)]
pub struct PendingDrain {
    /// 待重放的 switchMode config id（如有）。
    pub replay_switch_id: Option<u64>,
    /// 是否须排空一次尾随重启。
    pub schedule_restart: bool,
}

/// 生命周期单飞守卫（lifecycle_depth 引用计数 + 世代 token + pending 暂存）。
///
/// 全状态经 [`Mutex`] 保护，begin/end/bump/pending 操作同步、无 await——与 TS 单线程语义对齐
/// （Polaris 主线程同步 begin/end，await 仅发生在 begin/end 之间）。
#[derive(Debug, Default)]
pub struct LifecycleGate {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// 引用计数（重入：restart 内嵌 stop+start 时 depth=2，:1519-1521）。
    depth: u32,
    /// 生命周期世代（start/stop 入口 +1，:364）。u64 单调递增，不回绕（实际场景远不达上限）。
    generation: u64,
    pending: PendingSnapshot,
}

impl LifecycleGate {
    /// 进入一次 lifecycle 操作（beginLifecycleOp，:1522）。depth += 1。
    pub fn begin(&self) {
        let mut g = self.inner.lock().expect("lifecycle lock poisoned");
        g.depth += 1;
    }

    /// 退出一次 lifecycle 操作（endLifecycleOp，:1533）。
    ///
    /// depth -= 1（clamp 0）。仅在回到 idle（depth=0）时处理 pending：
    /// - kind=Stop → 丢弃全部 pending（停止优先），返回 [`StopDiscard`]。
    /// - kind=Start/Restart → 排空一次（重放 switch + 调度重启），返回 [`PendingDrain`]。
    ///
    /// depth>0（仍在更外层操作内）→ 返回 `StillBusy`，留给最外层。
    pub fn end(&self, kind: LifecycleKind) -> LifecycleEndResult {
        let mut g = self.inner.lock().expect("lifecycle lock poisoned");
        g.depth = g.depth.saturating_sub(1);
        if g.depth > 0 {
            return LifecycleEndResult::StillBusy(g.depth);
        }
        // depth 归 0。
        match kind {
            LifecycleKind::Stop => {
                let snap = std::mem::take(&mut g.pending);
                LifecycleEndResult::Stopped(StopDiscard {
                    discarded_restart: snap.restart_pending,
                    discarded_force_restart_id: snap.force_restart_id,
                    discarded_switch_id: snap.switch_id,
                })
            }
            LifecycleKind::Start | LifecycleKind::Restart => {
                let snap = std::mem::take(&mut g.pending);
                if snap.is_empty() {
                    return LifecycleEndResult::Drained(PendingDrain::default());
                }
                LifecycleEndResult::Drained(PendingDrain {
                    replay_switch_id: snap.switch_id,
                    // force_restart 或 restart_pending 任一待决都触发排空一次重启。
                    schedule_restart: snap.restart_pending || snap.force_restart_id.is_some(),
                })
            }
        }
    }

    /// 生命周期世代 +1（start()/stop() 入口，:632/:1347）。返回新世代值。
    pub fn bump_generation(&self) -> u64 {
        let mut g = self.inner.lock().expect("lifecycle lock poisoned");
        g.generation = g.generation.wrapping_add(1);
        g.generation
    }

    /// 当前世代（供 readiness/recovery 比对让位，:4522/5960）。
    pub fn generation(&self) -> u64 {
        self.inner
            .lock()
            .expect("lifecycle lock poisoned")
            .generation
    }

    /// 是否有 lifecycle 操作在飞（isLifecycleBusy，:1561）。
    pub fn is_busy(&self) -> bool {
        self.inner.lock().expect("lifecycle lock poisoned").depth > 0
    }

    /// 当前 depth（测试/诊断用）。
    pub fn depth(&self) -> u32 {
        self.inner.lock().expect("lifecycle lock poisoned").depth
    }

    /// 置去抖重启待决（scheduleDebouncedRestart trailing 命中且 depth>0，:1579-1581）。
    pub fn set_restart_pending(&self) {
        self.inner
            .lock()
            .expect("lifecycle lock poisoned")
            .pending
            .restart_pending = true;
    }

    /// 置强制重启配置快照 id（pendingForceRestartConfig，H-1 #4）。
    /// 调用方须保证 id 指向「最新 config」（非结构腿更新 currentConfig 后同步刷新；结构腿清 None）。
    pub fn set_force_restart(&self, config_id: u64) {
        self.inner
            .lock()
            .expect("lifecycle lock poisoned")
            .pending
            .force_restart_id = Some(config_id);
    }

    /// 清强制重启快照（结构性重启腿：newer 胜，:1894-1895）。
    pub fn clear_force_restart(&self) {
        self.inner
            .lock()
            .expect("lifecycle lock poisoned")
            .pending
            .force_restart_id = None;
    }

    /// 置 switchMode 对账待决（pendingSwitchConfig，bug#5）。
    pub fn set_switch_pending(&self, config_id: u64) {
        self.inner
            .lock()
            .expect("lifecycle lock poisoned")
            .pending
            .switch_id = Some(config_id);
    }

    /// 快照当前 pending（诊断用）。
    pub fn pending(&self) -> PendingSnapshot {
        self.inner
            .lock()
            .expect("lifecycle lock poisoned")
            .pending
            .clone()
    }

    /// 去抖重启守卫顺序门（scheduleDebouncedRestart trailing 回调，:1573-1600）。
    ///
    /// **必须先判 depth>0**（置 pending 返回 [`DebouncedDecision::Defer`]），再判核是否已停
    /// （返回 [`DebouncedDecision::CoreStopped`]，调用方须清 force-restart 快照），最后才返回
    /// [`DebouncedDecision::Proceed`]（执行重启）。顺序颠倒即回归 H-1 丢重启（#3）。
    ///
    /// `core_running`：当前核是否运行（singboxProcess||singboxPid 非空）。
    ///
    /// # `Proceed` **消费** `force_restart_id`（不是只读一眼）
    ///
    /// 返回 `Proceed` 的语义是「这条待决重启现在就被执行」——调用方紧接着真去重启。把 id 留在
    /// pending 里会让它在同一条腿上被数第二遍：执行中 depth>0，重启收尾 [`end`](Self::end)`(Restart)`
    /// 在 depth 归 0 时看见 `force_restart_id.is_some()` → [`PendingDrain::schedule_restart`] 为真
    /// → 再排一次整核重启。实测（陈先生 2026-07-30 真机）：每点一次「立即应用」核重启两遍
    /// （「排空一次尾随重启」紧跟在首次就绪之后），代价是 TUN 出口再夺一次、DNS 再接管一次、
    /// 旧连接再 RST 一次、解锁检测因 epoch 变丢弃整轮结果。
    ///
    /// **只消费 `force_restart_id`，不动 `restart_pending`**：后者只由「depth>0 时 trailing 命中」
    /// 这一条腿置（下方 `Defer`），而那条腿必然经 `end()` 的 `mem::take` 排空后才可能再见 depth=0
    /// —— 到 `Proceed` 时它恒为假，写它是无因果的多余动作。`switch_id` 属另一根轴（switch 重放，
    /// 不是重启），一律不碰。
    pub fn debounced_restart_decision(&self, core_running: bool) -> DebouncedDecision {
        let mut g = self.inner.lock().expect("lifecycle lock poisoned");
        // 1. depth>0 → 置 pending，由 end 排空（#1/#3，顺序不可颠倒）。
        if g.depth > 0 {
            g.pending.restart_pending = true;
            return DebouncedDecision::Defer;
        }
        // 2. 核已停 → 清 force-restart 快照（H-1：陈旧快照勿被后续去抖消费）。
        if !core_running {
            g.pending.force_restart_id = None;
            return DebouncedDecision::CoreStopped;
        }
        // 3. Proceed：调用方读 force_restart_id（优先）或 currentConfig 执行重启。
        //    `take` 而非读 —— 见方法头「Proceed 消费」那节。
        DebouncedDecision::Proceed(g.pending.force_restart_id.take())
    }
}

/// end() 返回值（depth 仍 >0 / 终态停止丢弃 / 排空排空一次）。
#[derive(Debug, Clone)]
pub enum LifecycleEndResult {
    /// depth 仍 >0（在更外层操作内），未处理 pending。
    StillBusy(u32),
    /// kind=Stop 终态：丢弃了这些 pending（停止优先）。
    Stopped(StopDiscard),
    /// kind=Start/Restart 且 depth 归 0：排空这些待决（可能为空集）。
    Drained(PendingDrain),
}

/// 去抖重启 trailing 决策（#3 顺序门）。
#[derive(Debug, Clone)]
pub enum DebouncedDecision {
    /// depth>0 → 置 pending 待排空（已置 restart_pending=true）。
    Defer,
    /// depth=0 但核已停 → 清 force-restart 快照，不重启。
    CoreStopped,
    /// 可执行重启：Option<force_restart_id>（Some=必须用该 config；None=用 currentConfig）。
    Proceed(Option<u64>),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;

    #[test]
    fn begin_end_pairs_track_depth_and_busy() {
        let g = LifecycleGate::default();
        assert!(!g.is_busy());
        assert_eq!(g.depth(), 0);

        g.begin();
        assert!(g.is_busy());
        assert_eq!(g.depth(), 1);

        // restart 内嵌 stop+start：depth 到 2（:1519-1521 重入语义）。
        g.begin();
        assert_eq!(g.depth(), 2);
        assert!(g.is_busy());

        // 内层 end：depth=1，StillBusy，不排空。
        let r = g.end(LifecycleKind::Start);
        assert!(matches!(r, LifecycleEndResult::StillBusy(1)));
        assert!(g.is_busy());

        // 外层 end：depth=0，Drained（无 pending）。
        let r = g.end(LifecycleKind::Restart);
        assert!(matches!(r, LifecycleEndResult::Drained(_)));
        assert!(!g.is_busy());
        assert_eq!(g.depth(), 0);
    }

    #[test]
    fn end_clamps_depth_at_zero_on_unbalanced_call() {
        // end 不配对 begin（防御）：clamp 0 不下溢（:1534 Math.max(0, depth-1)）。
        let g = LifecycleGate::default();
        let r = g.end(LifecycleKind::Stop);
        assert!(matches!(r, LifecycleEndResult::Stopped(_)));
        assert_eq!(g.depth(), 0);
    }

    #[test]
    fn stop_terminal_discards_all_three_pending_axes() {
        // #1：kind=Stop depth 归 0 → 必须丢弃全部三个 pending（停止优先，:1536-1540）。
        let g = LifecycleGate::default();
        g.set_restart_pending();
        g.set_force_restart(42);
        g.set_switch_pending(7);

        g.begin();
        let r = g.end(LifecycleKind::Stop);
        let LifecycleEndResult::Stopped(discard) = r else {
            panic!("expected Stopped, got {r:?}");
        };
        assert!(discard.discarded_restart);
        assert_eq!(discard.discarded_force_restart_id, Some(42));
        assert_eq!(discard.discarded_switch_id, Some(7));

        // pending 已清空。
        let p = g.pending();
        assert!(p.is_empty());
    }

    #[test]
    fn start_drains_pending_exactly_once_at_depth_zero() {
        // #1：kind=Start depth 归 0 → 恰好排空一次（重放 switch + 调度重启）。
        let g = LifecycleGate::default();
        g.set_restart_pending();
        g.set_switch_pending(7);

        g.begin();
        let r = g.end(LifecycleKind::Start);
        let LifecycleEndResult::Drained(drain) = r else {
            panic!("expected Drained, got {r:?}");
        };
        assert_eq!(drain.replay_switch_id, Some(7));
        assert!(drain.schedule_restart);

        // 排空后 pending 清空（恰好一次，不重复）。
        assert!(g.pending().is_empty());

        // 再次 end（无 pending）→ 空 drain。
        g.begin();
        let r = g.end(LifecycleKind::Start);
        let LifecycleEndResult::Drained(drain) = r else {
            panic!("expected Drained, got {r:?}");
        };
        assert_eq!(drain.replay_switch_id, None);
        assert!(!drain.schedule_restart);
    }

    #[test]
    fn nested_end_does_not_drain_until_outermost() {
        // depth=2 时内层 end → StillBusy，不排空；pending 留给最外层。
        let g = LifecycleGate::default();
        g.set_restart_pending();
        g.begin(); // depth=1
        g.begin(); // depth=2

        let r = g.end(LifecycleKind::Start);
        assert!(matches!(r, LifecycleEndResult::StillBusy(1)));
        // pending 仍在（未被排空）。
        assert!(g.pending().restart_pending);

        let r = g.end(LifecycleKind::Restart);
        let LifecycleEndResult::Drained(drain) = r else {
            panic!("expected Drained, got {r:?}")
        };
        assert!(drain.schedule_restart);
    }

    #[test]
    fn force_restart_id_drives_schedule_restart_when_no_restart_pending() {
        // #4：force_restart_id Some 时 schedule_restart=true（即使 restart_pending=false）。
        let g = LifecycleGate::default();
        g.set_force_restart(99);
        g.begin();
        let r = g.end(LifecycleKind::Start);
        let LifecycleEndResult::Drained(drain) = r else {
            panic!("expected Drained, got {r:?}")
        };
        assert!(drain.schedule_restart);
        assert_eq!(drain.replay_switch_id, None);
    }

    #[test]
    fn clear_force_restart_for_structural_restart_leg() {
        // #4：结构性重启腿必须清 force_restart（newer 胜，:1894-1895）。
        let g = LifecycleGate::default();
        g.set_force_restart(99);
        g.clear_force_restart();
        assert!(g.pending().force_restart_id.is_none());
    }

    #[test]
    fn generation_is_monotonic_per_bump() {
        let g = LifecycleGate::default();
        assert_eq!(g.generation(), 0);
        let a = g.bump_generation();
        assert_eq!(a, 1);
        assert_eq!(g.generation(), 1);
        let b = g.bump_generation();
        assert_eq!(b, 2);
        assert!(b > a);
    }

    #[test]
    fn debounced_defer_when_busy_overrides_core_state() {
        // #3：depth>0 必须先判（置 pending），即使核已停也不能进 CoreStopped 分支（顺序不可颠倒）。
        let g = LifecycleGate::default();
        g.begin();
        let d = g.debounced_restart_decision(false); // core_running=false 但 busy
        assert!(matches!(d, DebouncedDecision::Defer));
        assert!(g.pending().restart_pending); // 已置 pending
    }

    #[test]
    fn debounced_core_stopped_clears_force_restart_snapshot() {
        // #3/#4：depth=0 且核已停 → CoreStopped + 清 force-restart 快照（H-1 陈旧防护）。
        let g = LifecycleGate::default();
        g.set_force_restart(42);
        let d = g.debounced_restart_decision(false);
        assert!(matches!(d, DebouncedDecision::CoreStopped));
        assert!(g.pending().force_restart_id.is_none()); // 已清
    }

    #[test]
    fn debounced_proceed_returns_force_restart_id_when_set() {
        let g = LifecycleGate::default();
        g.set_force_restart(77);
        let d = g.debounced_restart_decision(true);
        assert!(matches!(d, DebouncedDecision::Proceed(Some(77))));
    }

    /// **一次「立即应用」只排一次整核重启**（陈先生 2026-07-30 真机：每点一次核重启两遍）。
    ///
    /// 走完整时序：`apply_pending` 置 force id → 去抖 trailing 命中 `Proceed` → 调用方开始重启
    /// （depth>0）→ 重启收尾 `end(Restart)` 在 depth 归 0 时**不得**再排一次。
    ///
    /// 变异对照：把 `debounced_restart_decision` 第 3 腿的 `.take()` 改回读取
    /// （`g.pending.force_restart_id`）→ `drain.schedule_restart` 变 true → 本条转红。
    /// 这正是真机日志里「depth 归零 → 排空一次尾随重启」紧跟首次就绪的那一行。
    #[test]
    fn proceed_consumes_force_restart_so_the_restart_is_not_drained_a_second_time() {
        let g = LifecycleGate::default();
        // ① apply_pending 非在飞腿：记下「必须用这份 config 重启」。
        g.set_force_restart(42);
        // ② 去抖 trailing：depth=0 且核在跑 → 授权执行，且**消费**该待决项。
        let d = g.debounced_restart_decision(true);
        assert!(
            matches!(d, DebouncedDecision::Proceed(Some(42))),
            "调用方仍须拿到 id 才能取对那份 config 快照，got {d:?}"
        );
        assert!(
            g.pending().is_empty(),
            "Proceed = 这条待决重启已被执行，pending 必须清空；留着它 = 同一条腿被数第二遍"
        );
        // ③ 调用方执行重启：depth 抬起再落回 0。
        g.begin();
        let r = g.end(LifecycleKind::Restart);
        let LifecycleEndResult::Drained(drain) = r else {
            panic!("expected Drained, got {r:?}")
        };
        assert!(
            !drain.schedule_restart,
            "已执行的那次重启不得被 end() 再排一遍 —— 否则每次 apply 都是两次整核重启"
        );
    }

    #[test]
    fn debounced_proceed_returns_none_when_only_current_config() {
        let g = LifecycleGate::default();
        let d = g.debounced_restart_decision(true);
        assert!(matches!(d, DebouncedDecision::Proceed(None)));
    }

    #[test]
    fn busy_gate_does_not_let_debounced_through_even_if_core_running() {
        // #3 顺序门核心：busy 时绝不 Proceed（杜绝「就绪等待中又来重启」风暴根因）。
        let g = LifecycleGate::default();
        g.set_force_restart(1);
        g.begin();
        let d = g.debounced_restart_decision(true);
        assert!(matches!(d, DebouncedDecision::Defer));
        // end 排空时仍能消费 force_restart（未被 debounced 清掉）。
        let r = g.end(LifecycleKind::Start);
        let LifecycleEndResult::Drained(drain) = r else {
            panic!("expected Drained, got {r:?}")
        };
        assert!(drain.schedule_restart);
    }

    #[test]
    fn stop_terminal_drops_force_restart_set_during_busy_window() {
        // bug#5：停止终态丢弃暂存 switch（已落盘，下次 start 读盘应用）。
        let g = LifecycleGate::default();
        g.begin();
        g.set_switch_pending(123);
        g.set_force_restart(456);
        let r = g.end(LifecycleKind::Stop);
        let LifecycleEndResult::Stopped(d) = r else {
            panic!("expected Stopped, got {r:?}")
        };
        assert_eq!(d.discarded_switch_id, Some(123));
        assert_eq!(d.discarded_force_restart_id, Some(456));
        assert!(g.pending().is_empty());
    }
}
