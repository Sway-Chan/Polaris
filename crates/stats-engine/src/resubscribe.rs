//! 长流周期重建策略 —— 上游 `StatsService` resubscribe / resubscribeStreamsOnly / startResubscribeTimer
//! 的纯逻辑移植（issue #210 根因 #3 + 维度7 #6 generation 守卫）。
//!
//! 背景：gRPC 长流不间断长跑（数小时）下，tonic 内部 HTTP/2 会话/通道对象可能缓慢保留（对齐 Polaris 注释引用的
//! grpc-node #2068）。周期性 cancel + 重订阅使流对象不长期驻留，重连瞬断 < 1s（首帧到达即恢复），换取长会话内存稳定。
//!
//! 两类重建（Polaris 区分）：
//! - **周期重建**（[`ResubscribeKind::Cyclic`]）：同一核的流刷新，**不归零 snapshot**（速率/总量是连续值，归零会闪烁）。
//!   每 [`ResubscribeStrategy::interval`] 触发一次。
//! - **崩溃/切端口重建**（[`ResubscribeKind::Forced`]）：核重启后新 client，**归零 snapshot**（新核首帧到达前 ~1s 窗口
//!   里旧值会与新连接列表不一致）。由调用方在 api-client-ready 时主动触发。
//!
//! generation 守卫（维度7 #6，对齐 ProxyManager lifecycleGeneration supersede 语义）：
//! 每次 [`ResubscribeStrategy::begin_generation`] 把 generation +1（一次 lifecycle 接管 = 一个新世代）。
//! 周期重建到期时若 generation 已变（被 start/stop/端口切换接管）→ 让位不重建，避免重建陈旧流。
//!
//! 纯逻辑：时间经 `now_ms` 参数注入（测试用虚拟时钟），不持定时器——上层 actor 据决策调度真实 sleep。

use std::time::Duration;

/// 上游 `STREAM_RESUBSCRIBE_INTERVAL_MS = 30 * 60 * 1000`（StatsService.ts:28）。
pub const STREAM_RESUBSCRIBE_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// 重建种类（Polaris resubscribe vs resubscribeStreamsOnly 的区别）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResubscribeKind {
    /// 周期重建（resubscribeStreamsOnly）：不归零 snapshot，规避长流对象驻留。每 interval 触发一次。
    Cyclic,
    /// 强制重建（resubscribe）：归零 snapshot，核重启/切端口时用。调用方主动触发，不耗周期配额。
    Forced,
}

/// 一次重建决策（decide_* 的返回值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResubscribeDecision {
    pub kind: ResubscribeKind,
    /// 触发本次决策时观察到的世代（调用方据此比对 begin_generation 的基线，确认未被 supersede）。
    pub generation: u64,
    /// 是否应归零 snapshot（Cyclic=false，Forced=true）。
    pub reset_snapshot: bool,
}

/// 长流周期重建策略状态机。
///
/// 纯逻辑——不持定时器、不 sleep。上层 actor：
/// 1. 启动时 [`ResubscribeStrategy::begin_generation`] 拿一个基线世代。
/// 2. 周期调度（每 ~poll 间隔）调 [`ResubscribeStrategy::decide_cyclic`]，传入 now_ms；
///    返回 Some(decision) 时执行真实重订阅（先比对 decision.generation == 自己的基线）。
/// 3. 核重启/端口切换时 [`ResubscribeStrategy::begin_generation`] + [`ResubscribeStrategy::force`]。
pub struct ResubscribeStrategy {
    interval: Duration,
    /// 当前生命周期世代（每次 begin_generation +1）。
    generation: u64,
    /// 上次周期重建触发后的时间锚（now_ms）。下次周期重建应在 anchor + interval_ms。
    last_cyclic_ms: u64,
}

impl Default for ResubscribeStrategy {
    fn default() -> Self {
        Self::new(STREAM_RESUBSCRIBE_INTERVAL)
    }
}

impl ResubscribeStrategy {
    /// 用指定周期构造（默认 [`STREAM_RESUBSCRIBE_INTERVAL`]）。测试可注入短周期加速。
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            generation: 0,
            last_cyclic_ms: 0,
        }
    }

    /// 当前周期（Duration）。
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// 当前世代。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 开始一个新世代（start/stop/端口切换接管生命周期）。返回新世代值（对齐 LifecycleGate::bump_generation）。
    /// 同时把周期重建锚重置为传入 now_ms（新世代从现在起算下一周期）。
    pub fn begin_generation(&mut self, now_ms: u64) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.last_cyclic_ms = now_ms;
        self.generation
    }

    /// 判定是否应触发一次**周期重建**（resubscribeStreamsOnly）。
    ///
    /// 距上次周期重建/世代起点 ≥ interval 即触发。返回 Some(Cyclic decision)；未到期返回 None。
    /// 调用方执行重订阅后须调 [`mark_cyclic_done`] 更新锚（避免一周期内重复触发）。
    ///
    /// 注意：本方法只判定「时间到」，generation 守卫由调用方在执行时再比对
    /// （decision.generation == 调用方持有的基线）——因 decide 与执行之间可能被 begin_generation 抢占。
    pub fn decide_cyclic(&self, now_ms: u64) -> Option<ResubscribeDecision> {
        let interval_ms = self.interval.as_millis() as u64;
        if now_ms.saturating_sub(self.last_cyclic_ms) >= interval_ms {
            Some(ResubscribeDecision {
                kind: ResubscribeKind::Cyclic,
                generation: self.generation,
                reset_snapshot: false,
            })
        } else {
            None
        }
    }

    /// 标记一次周期重建已完成（更新锚 = now_ms）。下次周期重建在 now_ms + interval。
    /// Polaris startResubscribeTimer 的 setInterval 每 interval 触发一次（非重入）；本方法对齐该语义。
    pub fn mark_cyclic_done(&mut self, now_ms: u64) {
        self.last_cyclic_ms = now_ms;
    }

    /// 生成一次**强制重建**决策（resubscribe：归零 snapshot）。核重启/切端口时用。
    /// 不消耗周期配额（不调 mark_cyclic_done）——Polaris resubscribe() 内部也 startResubscribeTimer（幂等），
    /// 但周期锚由 startResubscribeTimer 的 setInterval 从 0 起算；此处等效：调用方执行后应 begin_generation 重置锚。
    pub fn force(&self) -> ResubscribeDecision {
        ResubscribeDecision {
            kind: ResubscribeKind::Forced,
            generation: self.generation,
            reset_snapshot: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cyclic_not_triggered_before_interval() {
        let mut s = ResubscribeStrategy::new(Duration::from_secs(30));
        s.begin_generation(0); // 锚=0
        assert!(s.decide_cyclic(10_000).is_none());
        assert!(s.decide_cyclic(29_999).is_none());
    }

    #[test]
    fn cyclic_triggered_at_or_after_interval() {
        let mut s = ResubscribeStrategy::new(Duration::from_secs(30));
        s.begin_generation(0);
        // 30s 到期
        let d = s.decide_cyclic(30_000).expect("到期应触发");
        assert_eq!(d.kind, ResubscribeKind::Cyclic);
        assert!(!d.reset_snapshot, "周期重建不归零 snapshot");
    }

    #[test]
    fn cyclic_decision_carries_current_generation() {
        let mut s = ResubscribeStrategy::new(Duration::from_secs(10));
        let g = s.begin_generation(0);
        let d = s.decide_cyclic(10_000).unwrap();
        assert_eq!(d.generation, g);
    }

    #[test]
    fn mark_cyclic_done_resets_anchor_no_repeat_in_same_period() {
        let mut s = ResubscribeStrategy::new(Duration::from_secs(30));
        s.begin_generation(0);
        assert!(s.decide_cyclic(30_000).is_some());
        s.mark_cyclic_done(30_000);
        // 同一时刻再判 → 不应再触发（锚已更新到 30_000）
        assert!(s.decide_cyclic(30_000).is_none());
        assert!(s.decide_cyclic(59_999).is_none());
        // 下一周期到期
        assert!(s.decide_cyclic(60_000).is_some());
    }

    #[test]
    fn begin_generation_increments_and_resets_anchor() {
        let mut s = ResubscribeStrategy::new(Duration::from_secs(30));
        let g0 = s.begin_generation(0);
        let g1 = s.begin_generation(100);
        assert_eq!(g1, g0 + 1);
        // 新世代从 100 起算 → 100 + 30s = 30100ms 才到期
        assert!(s.decide_cyclic(30_099).is_none());
        assert!(s.decide_cyclic(30_100).is_some());
    }

    #[test]
    fn begin_generation_generation_is_monotonic() {
        let mut s = ResubscribeStrategy::new(Duration::from_secs(1));
        let a = s.begin_generation(0);
        let b = s.begin_generation(1);
        let c = s.begin_generation(2);
        assert!(c > b && b > a, "世代应单调递增");
    }

    #[test]
    fn forced_decision_resets_snapshot() {
        let s = ResubscribeStrategy::new(Duration::from_secs(30));
        let d = s.force();
        assert_eq!(d.kind, ResubscribeKind::Forced);
        assert!(d.reset_snapshot, "强制重建归零 snapshot");
    }

    #[test]
    fn forced_does_not_consume_cyclic_quota() {
        // force 不调 mark_cyclic_done → 周期锚不变。调用方应在 force 后 begin_generation 重置锚。
        let mut s = ResubscribeStrategy::new(Duration::from_secs(30));
        s.begin_generation(0);
        let _ = s.force();
        // 仍按原锚判定周期
        assert!(s.decide_cyclic(30_000).is_some(), "force 不影响周期锚");
    }

    #[test]
    fn cyclic_let_go_if_generation_advanced_between_decide_and_execute() {
        // 维度7 #6 generation 守卫：decide 拿到 g=1，但执行前 begin_generation 把世代推到 g=2
        // → 调用方比对 decision.generation != 当前基线 → 让位不重建。本测试锁 decide 返回的 generation
        // 是「快照世代」，供调用方比对（decide 与执行之间的抢占由调用方处理）。
        let mut s = ResubscribeStrategy::new(Duration::from_secs(10));
        let g1 = s.begin_generation(0);
        let decision = s.decide_cyclic(10_000).unwrap();
        assert_eq!(decision.generation, g1);
        // 执行前被 supersede
        let g2 = s.begin_generation(10_000);
        assert_ne!(decision.generation, g2, "决策世代已陈旧，调用方应让位");
    }

    #[test]
    fn default_interval_is_30min() {
        let s = ResubscribeStrategy::default();
        assert_eq!(s.interval(), STREAM_RESUBSCRIBE_INTERVAL);
        assert_eq!(s.interval(), Duration::from_secs(30 * 60));
    }

    #[test]
    fn decide_cyclic_with_saturating_sub_on_underflow() {
        // now_ms < last_cyclic_ms（时钟回拨）不应 panic；saturating_sub → 0 → 不触发。
        let mut s = ResubscribeStrategy::new(Duration::from_secs(30));
        s.begin_generation(1_000_000);
        assert!(s.decide_cyclic(0).is_none(), "时钟回拨不应触发周期重建");
    }
}
