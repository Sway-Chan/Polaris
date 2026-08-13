//! emit 节流闸门 —— 长驻连接流下「轮询节拍」的替代物。
//!
//! # 为什么长驻流反而**更**需要一道闸门
//!
//! 轮询时代，「多久推一帧给渲染端」这件事是免费搭在拉取节拍上的：`first_connection_snapshot`
//! 每 250ms（aggregate）/ 1s（detail）拉一次，emit 自然也就是那个频率 —— 节拍同时充当了
//! **拉取周期**与**推送上限**两个角色。
//!
//! 换成 `SubscribeConnections` 长驻流后，拉取周期这一半消失了（内核对 NEW/CLOSED 是
//! `case event := <-subscription` 事件驱动即时推送，`daemon/started_service.go:752`），
//! 但**推送上限那一半不能跟着消失**：
//!
//! - 内核在同一次 select 里带 `drain:` 标签把队列里已到的事件一次排空再 Send，故单帧可以很大，
//!   但**帧与帧之间没有任何最小间隔**。一个 BT 客户端瞬间开 500 条连接 = 一串背靠背的帧。
//! - 我们这侧每帧的代价不是「解一次 protobuf」而已：aggregate 要 O(n log n) 排序 + Top-N，
//!   detail 要把整张表 trim 成 `ConnectionEntry` 再整体 JSON 序列化过 IPC，渲染端还要重排一次
//!   拓扑图 / 重渲一张表。**把这条链路的频率交给内核的事件速率去定，等于把前端的帧预算
//!   外包给了对端的负载。**
//!
//! 故：**闸门从「拉取节拍」降级成「emit 下限间隔」**——上游帧照单全收（连接表必须实时准确，
//! 否则 CLOSED 漏一条就是永久幽灵），但**下游 emit 有地板**。两者解耦正是长驻流的收益所在：
//! 状态是实时的，渲染是有节制的。
//!
//! # 合并语义（coalescing，不是采样）
//!
//! 冷却期内到达的 N 帧**不产生 N 次 emit，也不被丢弃** —— 它们把 `pending` 置起，冷却一到
//! 就用**当时最新的连接表**推一帧。这是「尾沿保证」：
//!
//! - 不做尾沿 → 一次孤立的连接变化若恰好落在冷却期内，就**永远**不会被推（下一帧要等下一次
//!   变化，而变化可能几分钟后才有）。拓扑图会停在旧状态，看着像「流断了」。
//!   这是节流实现最经典的一个坑，[`EmitGate::wait_for`] 存在的唯一理由就是它。
//! - 做成「每帧都 emit 但丢弃冷却期内的」= 采样，会丢状态；本闸门丢的是**中间帧**，不丢**状态**
//!   （状态在连接表里，emit 时现取最新的）。
//!
//! # 纯逻辑
//!
//! 不持定时器、不 sleep、不碰时钟：时刻经 `now_ms` 参数注入（对齐 [`crate::resubscribe`] 的同一约定），
//! 上层 actor 据 [`EmitGate::wait_for`] 的返回值调度真实 `tokio::time::sleep`。
//! 于是「冷却期内 N 帧只 emit 一次」「尾沿不丢」这些规则可以用构造的事件序列逐条单测，
//! 不需要真流、不需要真内核、不需要碰网络。

use std::time::Duration;

/// 单条投影（aggregate / detail）的 emit 节流闸门。
///
/// 用法（上层 actor 的流循环）：
/// 1. 收到上游帧、更新完连接表 → [`note_change`](Self::note_change)。
/// 2. 循环顶部 → [`wait_for`](Self::wait_for) 拿「距下次可 emit 还要多久」：
///    `None` = 无待推变更（不设定时器，纯等下一帧）；`Some(ZERO)` = 立刻可推；
///    `Some(d)` = 冷却中，`select!` 里挂一个 `sleep(d)`。
/// 3. 真 emit 后 → [`mark_emitted`](Self::mark_emitted)。
/// 4. 流被 drop / 重订阅 → [`reset`](Self::reset)（下一帧不受上一条流的冷却牵连）。
#[derive(Debug, Clone)]
pub struct EmitGate {
    /// 两次 emit 之间的下限间隔。
    min_interval: Duration,
    /// 上次 emit 时刻（ms）。`None` = 本条流尚未 emit 过 → 首帧不等冷却。
    ///
    /// **首帧必须免冷却**：订阅（或重订阅）后的第一帧是 `reset=true` 全量表，它是渲染端
    /// 从「无数据 / 旧数据」切到「当前真相」的唯一一帧。让它等满 250ms 就是把长驻流最大的
    /// 收益（首帧即真相）主动还回去；轮询时代的「首拍不睡」语义也正是这一条。
    last_emit_ms: Option<u64>,
    /// 是否有「已收到、尚未 emit」的变更（尾沿标志）。
    pending: bool,
}

impl EmitGate {
    /// 用指定下限间隔构造。间隔取值由调用方定（策略属运行时，不属纯逻辑层）。
    #[must_use]
    pub const fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_emit_ms: None,
            pending: false,
        }
    }

    /// 当前下限间隔。
    #[must_use]
    pub const fn min_interval(&self) -> Duration {
        self.min_interval
    }

    /// 记一次上游变更（收到流帧、连接表已更新）。幂等：冷却期内来 100 帧与来 1 帧等效。
    pub const fn note_change(&mut self) {
        self.pending = true;
    }

    /// 是否有待推变更（尚未 emit）。
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending
    }

    /// 距下次可 emit 还要等多久。
    ///
    /// - `None`：无待推变更 —— 调用方**不该**设定时器，安静等下一帧即可（长驻流空闲时是常态，
    ///   这一条就是「没变化就零开销」的来源）。
    /// - `Some(Duration::ZERO)`：现在就能推。
    /// - `Some(d)`：冷却中，`d` 之后推。
    ///
    /// 时钟回拨（`now_ms` < `last_emit_ms`）走 `saturating_sub` → 视作「刚推过」→ 等满一个间隔，
    /// 不 panic、也不会因为负数溢出成天文数字把 emit 永久饿死。
    #[must_use]
    pub fn wait_for(&self, now_ms: u64) -> Option<Duration> {
        if !self.pending {
            return None;
        }
        let Some(last) = self.last_emit_ms else {
            return Some(Duration::ZERO); // 本条流首帧：免冷却
        };
        let elapsed = Duration::from_millis(now_ms.saturating_sub(last));
        Some(self.min_interval.saturating_sub(elapsed))
    }

    /// 此刻是否应该 emit（= 有待推变更且冷却已过）。[`wait_for`](Self::wait_for) 的布尔投影。
    #[must_use]
    pub fn should_emit(&self, now_ms: u64) -> bool {
        self.wait_for(now_ms) == Some(Duration::ZERO)
    }

    /// 记一次真实 emit（清尾沿标志 + 重置冷却锚）。
    pub const fn mark_emitted(&mut self, now_ms: u64) {
        self.pending = false;
        self.last_emit_ms = Some(now_ms);
    }

    /// 复位（流被 drop / 重订阅 / 核重启）。
    ///
    /// **冷却锚一并清掉**是刻意的：重订阅后的首帧是 `reset=true` 全量表，若还背着上一条流的冷却
    /// 锚，用户切回窗口的那一刻要多等一个间隔才看到真相 —— 而「恢复不等整拍」正是降流门那一侧
    /// 花了力气保证的事，不该在这里被抵消掉。
    pub const fn reset(&mut self) {
        self.pending = false;
        self.last_emit_ms = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_millis(250);

    fn gate() -> EmitGate {
        EmitGate::new(TICK)
    }

    /// 无变更 → 不设定时器、不 emit（长驻流空闲时的常态：零开销）。
    ///
    /// **变异探针**：`wait_for` 去掉 `if !self.pending` 短路（恒返回 `Some`）⇒ 转红 ——
    /// 那会让上层每个 min_interval 白醒一次，把「事件驱动」退化回轮询。
    #[test]
    fn 无变更时不推也不设定时器() {
        let g = gate();
        assert_eq!(g.wait_for(0), None);
        assert_eq!(g.wait_for(10_000), None);
        assert!(!g.should_emit(10_000));
        assert!(!g.is_pending());
    }

    /// 🟡 **首帧免冷却**：订阅后第一帧（reset 全量表）立刻放行。
    ///
    /// **变异探针**：`wait_for` 的 `None => Some(ZERO)` 分支改成走冷却计算（如把 `last_emit_ms`
    /// 初值设成 0 而非 `None`）⇒ 转红。那等于首帧要等满一个间隔才到渲染端。
    #[test]
    fn 首帧免冷却立刻放行() {
        let mut g = gate();
        g.note_change();
        assert_eq!(g.wait_for(0), Some(Duration::ZERO));
        assert!(g.should_emit(0));
    }

    /// 🟡 **冷却期内的 N 帧只产出一次 emit**（合并，不是 N 次）。
    ///
    /// **变异探针**：`mark_emitted` 不写 `last_emit_ms` ⇒ 每帧都放行 ⇒ 转红。
    /// 这条锁的是「把前端帧预算外包给对端负载」那个坑：内核一次连接风暴推来多少帧，
    /// 我们就只推一帧。
    #[test]
    fn 冷却期内多帧合并为一次emit() {
        let mut g = gate();
        g.note_change();
        assert!(g.should_emit(1_000));
        g.mark_emitted(1_000);

        // 冷却期内连来 100 帧
        for i in 0..100u64 {
            g.note_change();
            let now = 1_000 + i; // 全部落在 1_000..1_100，远早于 1_250
            assert!(
                !g.should_emit(now),
                "冷却期内第 {i} 帧不得 emit（应合并到冷却结束时一次推出）"
            );
        }
        // 冷却结束 → 恰好一次
        assert!(g.should_emit(1_250), "冷却一到必须推出合并后的那一帧");
        g.mark_emitted(1_250);
        assert!(!g.is_pending(), "推完即清尾沿");
    }

    /// 🔴 **尾沿保证：冷却期内到达的孤立变更绝不能被吞掉。**
    ///
    /// 这是节流实现最经典的一个坑，也是本闸门与「采样」的分界：变更落在冷却期内且此后再无变更时，
    /// 若不做尾沿，这一帧就**永远**不会推（下一次 emit 要等下一次变化，可能几分钟后）——
    /// 拓扑图停在旧状态，用户看到的现象与「流断了」完全一样，且没有任何日志。
    ///
    /// **变异探针**：`mark_emitted` 里删掉 `pending = false` 之外的任何写法都不会红；
    /// 真正的变异是把 `note_change` 改成「冷却期内直接丢弃」（`if self.should_emit(now) { .. }`）
    /// ⇒ 本测转红。
    #[test]
    fn 冷却期内的孤立变更在冷却结束后仍会推出() {
        let mut g = gate();
        g.note_change();
        g.mark_emitted(1_000);

        // 冷却期内来一帧，此后再无任何变更
        g.note_change();
        assert!(!g.should_emit(1_100), "冷却期内先不推");
        assert!(g.is_pending(), "但必须记着这笔账");

        // 冷却一到，即便再没有新帧进来，也必须推出
        assert_eq!(g.wait_for(1_100), Some(Duration::from_millis(150)));
        assert!(
            g.should_emit(1_250),
            "尾沿：冷却结束必须把期内那笔变更推出，否则它永远不会到渲染端"
        );
    }

    /// `wait_for` 返回的是**剩余**时长，不是固定间隔（上层据此挂 `sleep`，多睡即多等一拍）。
    #[test]
    fn wait_for返回剩余冷却而非整段间隔() {
        let mut g = gate();
        g.note_change();
        g.mark_emitted(1_000);
        g.note_change();
        assert_eq!(g.wait_for(1_000), Some(TICK));
        assert_eq!(g.wait_for(1_100), Some(Duration::from_millis(150)));
        assert_eq!(g.wait_for(1_249), Some(Duration::from_millis(1)));
        assert_eq!(g.wait_for(1_250), Some(Duration::ZERO));
        assert_eq!(g.wait_for(9_999), Some(Duration::ZERO), "早已到期 → 立刻");
    }

    /// 🟡 **重订阅复位后首帧免冷却**（与降流门的「恢复不等整拍」对齐）。
    ///
    /// **变异探针**：`reset` 只清 `pending` 不清 `last_emit_ms` ⇒ 转红 —— 用户切回窗口时
    /// 要多等一个间隔才看到 reset 全量帧，而降流门那侧刚花力气保证了立刻唤醒。
    #[test]
    fn reset后首帧不背上一条流的冷却() {
        let mut g = gate();
        g.note_change();
        g.mark_emitted(1_000);
        assert!(!g.should_emit(1_050), "同一条流内仍受冷却");

        g.reset(); // 流被 drop（窗口隐藏）→ 重订阅
        assert!(!g.is_pending(), "复位必须清掉旧的待推标志（旧表已作废）");
        g.note_change(); // 重订阅后的 reset=true 全量首帧
        assert!(
            g.should_emit(1_050),
            "重订阅首帧必须免冷却，否则恢复要多等一个间隔"
        );
    }

    /// 时钟回拨不 panic、也不把 emit 永久饿死。
    #[test]
    fn 时钟回拨退化为等满一个间隔() {
        let mut g = gate();
        g.note_change();
        g.mark_emitted(1_000_000);
        g.note_change();
        // now < last：saturating_sub → elapsed 0 → 等满一个间隔（而非溢出成天文数字）
        assert_eq!(g.wait_for(0), Some(TICK));
        assert!(!g.should_emit(0));
    }

    /// 间隔为 0 时退化成「每帧都推」（不做闸门），且仍遵守尾沿语义。
    #[test]
    fn 零间隔退化为逐帧推送() {
        let mut g = EmitGate::new(Duration::ZERO);
        g.note_change();
        assert!(g.should_emit(0));
        g.mark_emitted(0);
        assert!(!g.should_emit(0), "无变更仍不推");
        g.note_change();
        assert!(g.should_emit(0));
    }
}
