//! 订阅注册表 —— 上游 `StatsService` 订阅门控 + `StatsWorkerHost` 需求开关的纯逻辑移植。
//!
//! Polaris 的订阅模型比 Polaris 更显式：Polaris 把「渲染端 watcher 计数」隐式耦合进「代理运行即订阅」，
//! 且 connections 流的开关经 `setConnectionsStreamEnabled`（窗口隐藏/无消费者 → cancel 上游 SubscribeConnections）。
//! 本 crate 把它拆成可单测的注册表：
//!
//! - [`Topic`]：stats / connections / detail / closed 四条投影的订阅分轨。
//! - [`SubscriptionRegistry`]：记录每个 [`Topic`] 的活跃订阅者集合 + 窗口可见性；判定「是否应保持该 topic 的上游流」。
//!
//! 降流语义（维度7 #实测：无订阅者 / 无可见窗口时断流省资源）：
//! - [`SubscriptionRegistry::should_stream`]：某 topic 的活跃订阅者数 > 0 **且**（窗口可见 **或** 该 topic 不受可见性门控）
//!   才返回 true。无订阅者 → false → 上游 cancel 流。
//! - **全部 topic 口径一致**（[`Topic::gated_by_visibility`] 恒 true）：无可见窗口 = 无 UI 消费者 → 全部降流。
//!   Stats 曾经是例外（"恒需、不门控"），该例外已随其前提一并作废——理由见 [`Topic::gated_by_visibility`]
//!   的「为什么与 上游 表面形态不同」，**不是**漂移。
//!
//! 注册/注销返回 [`SubscriptionToken`]（u64 单调递增），调用方持有 token 注销（避免按字符串 id 误删他人订阅）。

use std::collections::HashMap;

/// 订阅分轨。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Topic {
    /// Status 流（流量速率/累计，首页流量条 + StatusBar）。
    Stats,
    /// Connections 流（首页拓扑聚合）。
    Connections,
    /// 连接明细（连接信息页 detail topic）。
    Detail,
    /// 已结束连接历史（连接信息页 closed topic）。
    Closed,
}

impl Topic {
    /// 该 topic 是否受窗口可见性门控（true = 无可见窗口时应降流）。
    ///
    /// **全部 topic 同一口径，恒 true**：无可见窗口 = 无 UI 消费者，任何一条 topic 的帧都无人消费。
    /// 刻意不按 topic 分叉——分叉过一次（Stats 曾恒需），理由已随下述前提一并失效。
    ///
    /// # 为什么与 上游的表面形态不同（审计请读完再判漂移）
    ///
    /// 上游的 worker 明写「status 不门控：始终流动，驱动 host 惰性下发 setDemand」
    /// （`src/main/workers/stats-worker.ts:51`）。那条「不门控」是 **worker 协议的实现细节 + 成本事实**，
    /// 不是「流量条恒需」的产品语义：
    /// 1. **载体**：上游的 host 靠常流的 status 帧当载体惰性重发 `setDemand`
    ///    （`StatsWorkerHost.ts:187-199`，reconnect/respawn 后的自愈路径）——status 停了，demand 就发不出去。
    /// 2. **成本**：上游的 status 是 sing-box **server-push 的 `SubscribeStatus` 廉价帧**，保持流动近乎零成本。
    ///
    /// 第一条前提在 Polaris 不成立，这一条就够定论：Polaris 没有 worker 进程、没有 `setDemand`
    /// 握手——**没有任何东西需要 status 流当载体**，停掉它不会让别的东西自愈失败。
    ///
    /// ⚠️ 本条曾另有第二条理由「Polaris 的 stats 与 aggregate 共用 `first_connection_snapshot`
    /// 全量快照，保持流动 = 每秒一次全量连接 dump」——**该理由已随实现作废，勿再据以判断**：
    /// stats 现在也吃 `SubscribeStatus`（`polaris::runtime::stats::run_stats_stream`），
    /// 与 上游 同为廉价 server-push 帧。**但结论不变**：省的不再是 gRPC 全量 dump，而是
    /// 「没人看的时候不做无人消费的 IPC + 重渲染」，而这本来就是 上游 广播侧的语义（见下）。
    ///
    /// 而「窗口不可见就不给 UI 发流量帧」**本来就是 上游的语义**，只是它落在广播侧：
    /// `StatsService.ts:312` 的 `if (this.isWindowVisible && !this.isWindowVisible()) return;`
    /// 与 `StatsWorkerHost.ts:217` 的 `if (this.opts.isUiActive()) this.opts.onStats(...)`。
    /// Polaris 的轮询与广播在同一条 poller 上（没有 worker 那层分离），故把这条门落在**判据本体**：
    /// 对 UI 而言语义等价（不可见 → 不发帧），并顺带省掉那次没人消费的 gRPC。
    pub fn gated_by_visibility(self) -> bool {
        true
    }
}

/// 订阅 token（注册时返回，注销时凭 token 删除——避免按字符串 id 误删）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionToken(pub u64);

/// 订阅注册表：记录每个 topic 的活跃订阅者 + 窗口可见性，判定「该 topic 的上游流是否应保持」。
///
/// 纯逻辑、无 I/O——调用方（B5 stats actor）据 [`should_stream`] 的返回值决定 tonic 流的 subscribe/drop。
/// 多线程访问：调用方自行包 Mutex（对齐 core-supervisor LifecycleGate 的 `&self + 内部 Mutex` 模式）；
/// 本类型方法取 `&self`/`&mut self`，同步语义即可单测。
#[derive(Debug, Default)]
pub struct SubscriptionRegistry {
    /// topic -> (token -> subscriber_id)。用有序结构便于调试 + 确定性测试。
    topics: HashMap<Topic, HashMap<SubscriptionToken, String>>,
    /// 窗口是否可见（无可见窗口 = 无 UI 消费者）。
    ///
    /// 默认 **true = fail-open**：全部 topic 都受可见性门控，缺省若取 false 就等于「还没人告诉我
    /// 窗口状态」时先把 UI 饿死一拍。调用方（poller）在第一拍即按窗口实况回写真值，故乐观缺省不会
    /// 让隐藏态漏降流，只是把「不确定」的那一瞬倒向不伤 UI 的一侧。
    window_visible: bool,
    /// 下一 token（u64 单调递增，不回绕——实际场景远不达上限）。
    next_token: u64,
}

impl SubscriptionRegistry {
    /// 新建空注册表（窗口默认可见）。
    pub fn new() -> Self {
        Self {
            topics: HashMap::new(),
            window_visible: true,
            next_token: 0,
        }
    }

    /// 注册一个订阅者到指定 topic。返回 token（注销时用）。`subscriber_id` 仅诊断/日志用，不参与去重
    /// （同一 subscriber 可订阅多次，每次独立 token——对齐渲染端多窗口/多组件各自订阅）。
    pub fn subscribe(
        &mut self,
        topic: Topic,
        subscriber_id: impl Into<String>,
    ) -> SubscriptionToken {
        let token = SubscriptionToken(self.next_token);
        self.next_token += 1;
        self.topics
            .entry(topic)
            .or_default()
            .insert(token, subscriber_id.into());
        token
    }

    /// 注销一个订阅（凭 token）。返回是否确实删除了（false = token 不存在/已注销）。
    pub fn unsubscribe(&mut self, topic: Topic, token: SubscriptionToken) -> bool {
        self.topics
            .get_mut(&topic)
            .map(|m| m.remove(&token).is_some())
            .unwrap_or(false)
    }

    /// 某 topic 的活跃订阅者数。
    pub fn subscriber_count(&self, topic: Topic) -> usize {
        self.topics.get(&topic).map(|m| m.len()).unwrap_or(0)
    }

    /// 是否有任何 topic 有活跃订阅者。
    pub fn has_any_subscriber(&self) -> bool {
        self.topics.values().any(|m| !m.is_empty())
    }

    /// 设置窗口可见性（无可见窗口 = 无 UI 消费者 → 受门控的 topic 降流）。
    pub fn set_window_visible(&mut self, visible: bool) {
        self.window_visible = visible;
    }

    /// 窗口是否可见。
    pub fn window_visible(&self) -> bool {
        self.window_visible
    }

    /// 判定指定 topic 的上游流是否应保持（true = 订阅上游，false = cancel 降流）。
    ///
    /// 降流语义（维度7）：活跃订阅者数 > 0 **且**（窗口可见 **或** 该 topic 不受可见性门控）。
    /// 全部 topic 口径一致（[`Topic::gated_by_visibility`] 恒 true）→ 实际等价于「有订阅者 且 窗口可见」。
    ///
    /// 门控项刻意保留 `topic.gated_by_visibility()` 这一跳而非内联成常量：它是契约的显式落点，
    /// 「哪些 topic 受可见性门控」的答案（连同它为何是全部）写在那个方法的文档里，改口径改那一处。
    pub fn should_stream(&self, topic: Topic) -> bool {
        if self.subscriber_count(topic) == 0 {
            return false;
        }
        if topic.gated_by_visibility() && !self.window_visible {
            return false;
        }
        true
    }

    /// 判定**连接流**（`SubscribeConnections` 长驻流）是否应保持。
    ///
    /// [`Topic::Connections`]（首页拓扑）、[`Topic::Detail`]（活动明细）与 [`Topic::Closed`]
    /// （已结束历史）来自同一条连接事件流，共用**一条**上游流 ⇒ 任一有需求即保持，全部没需求才降流。
    ///
    /// 为什么不让三个 topic 各开一条流：那是轮询时代的形状，在长驻流下会变成
    /// 两份完全相同的事件流 + 两张各自维护的连接表 —— 上游成本翻倍，且两张表还可能因为
    /// 建流时刻不同而给出**互相矛盾**的两帧（拓扑说 12 条、明细列 13 条）。
    ///
    /// ⚠️ 本判定只回答「流开不开」。**开着不等于全部 topic 都该 emit** —— 每条 topic 的 emit
    /// 仍各自按 `should_stream(topic)` 门控（只订了拓扑就别把全量明细 JSON 推过去）。
    pub fn should_stream_connections(&self) -> bool {
        self.should_stream(Topic::Connections)
            || self.should_stream(Topic::Detail)
            || self.should_stream(Topic::Closed)
    }

    /// 清空某 topic 的全部订阅（stream 断开 / 上层销毁时）。
    pub fn clear_topic(&mut self, topic: Topic) {
        if let Some(m) = self.topics.get_mut(&topic) {
            m.clear();
        }
    }

    /// 清空全部订阅。
    pub fn clear_all(&mut self) {
        for m in self.topics.values_mut() {
            m.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 🔴 **连接流的 demand = 三个投影的并集**（aggregate / detail / closed 任一有需求即保持）。
    ///
    /// **变异探针**：`should_stream_connections` 改成只看 `Topic::Connections` ⇒ 「只开着连接页、
    /// 首页拓扑没订阅」时流不开，连接明细页永远空白 ⇒ 转红；改成 `&&` ⇒ 两条都得订阅才开流 ⇒ 转红。
    #[test]
    fn 连接流demand是三个投影的并集() {
        let mut r = SubscriptionRegistry::new();
        assert!(!r.should_stream_connections(), "都没订阅 → 不开流");

        let t_agg = r.subscribe(Topic::Connections, "main");
        assert!(r.should_stream_connections(), "只订拓扑 → 也得开流");
        r.unsubscribe(Topic::Connections, t_agg);

        let t_detail = r.subscribe(Topic::Detail, "main");
        assert!(r.should_stream_connections(), "只订明细 → 也得开流");
        r.unsubscribe(Topic::Detail, t_detail);

        let t_closed = r.subscribe(Topic::Closed, "main");
        assert!(r.should_stream_connections(), "只订已结束历史 → 也得开流");
        r.unsubscribe(Topic::Closed, t_closed);

        // Stats 是另一条腿（走 Status/轮询），不该把连接流拉起来
        r.subscribe(Topic::Stats, "main");
        assert!(
            !r.should_stream_connections(),
            "stats 订阅不得拉起连接流 —— 它不消费连接表"
        );
    }

    /// 🔴 **窗口不可见 → 连接流必须 drop**（长驻流下降流的实质：断流，不是 park 一拍）。
    ///
    /// **变异探针**：`should_stream_connections` 绕过 `should_stream` 直接看 `subscriber_count`
    /// （即「不可见时不 drop 流」）⇒ 转红。
    #[test]
    fn 窗口不可见时连接流必须drop() {
        let mut r = SubscriptionRegistry::new();
        r.subscribe(Topic::Connections, "main");
        r.subscribe(Topic::Detail, "main");
        assert!(r.should_stream_connections());

        r.set_window_visible(false);
        assert!(
            !r.should_stream_connections(),
            "窗口隐藏 = 无 UI 消费者 → 长驻流必须断掉，而不是留着白收事件"
        );

        r.set_window_visible(true);
        assert!(r.should_stream_connections(), "窗口回来 → 必须重订阅");
    }

    #[test]
    fn subscribe_returns_unique_tokens() {
        let mut r = SubscriptionRegistry::new();
        let t1 = r.subscribe(Topic::Stats, "win1");
        let t2 = r.subscribe(Topic::Stats, "win2");
        assert_ne!(t1, t2);
        assert_eq!(r.subscriber_count(Topic::Stats), 2);
    }

    #[test]
    fn unsubscribe_by_token_removes_one() {
        let mut r = SubscriptionRegistry::new();
        let t1 = r.subscribe(Topic::Connections, "win1");
        let _t2 = r.subscribe(Topic::Connections, "win2");
        assert!(r.unsubscribe(Topic::Connections, t1));
        assert_eq!(r.subscriber_count(Topic::Connections), 1);
        // 再次注销同一 token → false
        assert!(!r.unsubscribe(Topic::Connections, t1));
    }

    #[test]
    fn unsubscribe_wrong_topic_returns_false() {
        let mut r = SubscriptionRegistry::new();
        let t = r.subscribe(Topic::Stats, "win1");
        assert!(!r.unsubscribe(Topic::Connections, t));
    }

    /// Stats 也受可见性门控（原 `should_stream_stats_only_needs_subscribers` 的新语义）。
    ///
    /// 旧断言是「窗口隐藏也保持流」，其前提是 上游 那条「status 流是 worker demand 握手的载体」
    /// ——Polaris 没有 worker、没有该握手，见 [`Topic::gated_by_visibility`]。隐藏后继续流 =
    /// 每秒一次无人消费的 IPC + 重渲染。
    #[test]
    fn should_stream_stats_needs_subscribers_and_visibility() {
        let mut r = SubscriptionRegistry::new();
        assert!(!r.should_stream(Topic::Stats)); // 无订阅者
        let _t = r.subscribe(Topic::Stats, "win1");
        assert!(r.should_stream(Topic::Stats)); // 有订阅者 + 可见
        r.set_window_visible(false);
        assert!(
            !r.should_stream(Topic::Stats),
            "窗口隐藏 → Stats 也降流（无 UI 消费者，且本仓 stats 拉的是全量快照）"
        );
        r.set_window_visible(true);
        assert!(r.should_stream(Topic::Stats), "窗口回来 → 恢复");
    }

    #[test]
    fn should_stream_connections_gated_by_visibility() {
        let mut r = SubscriptionRegistry::new();
        let _t = r.subscribe(Topic::Connections, "win1");
        assert!(r.should_stream(Topic::Connections)); // 可见 + 有订阅者
        r.set_window_visible(false);
        // 无可见窗口 → 降流（省核 CPU），即使有订阅者
        assert!(!r.should_stream(Topic::Connections));
    }

    #[test]
    fn should_stream_detail_gated_by_visibility() {
        let mut r = SubscriptionRegistry::new();
        let _t = r.subscribe(Topic::Detail, "conn-page");
        assert!(r.should_stream(Topic::Detail));
        r.set_window_visible(false);
        assert!(!r.should_stream(Topic::Detail));
    }

    #[test]
    fn 降流_注册后注销至无订阅者应停止流() {
        // 维度7 降流测试：注册 → 注销 → should_stream 翻 false
        let mut r = SubscriptionRegistry::new();
        let t = r.subscribe(Topic::Connections, "win1");
        assert!(r.should_stream(Topic::Connections));
        r.unsubscribe(Topic::Connections, t);
        assert!(!r.should_stream(Topic::Connections), "注销后无订阅者应降流");
    }

    #[test]
    fn 降流_多订阅者最后一个注销才降流() {
        let mut r = SubscriptionRegistry::new();
        let t1 = r.subscribe(Topic::Connections, "win1");
        let t2 = r.subscribe(Topic::Connections, "win2");
        assert!(r.should_stream(Topic::Connections));
        r.unsubscribe(Topic::Connections, t1);
        assert!(
            r.should_stream(Topic::Connections),
            "仍有一个订阅者，保持流"
        );
        r.unsubscribe(Topic::Connections, t2);
        assert!(!r.should_stream(Topic::Connections), "全部注销才降流");
    }

    #[test]
    fn 降流_窗口隐藏时全部topic门控口径一致() {
        // ★ 契约测试（口径一致）：本条是 `Topic::gated_by_visibility` 恒 true 的锁。
        //
        // 前身是 `降流_窗口隐藏取消connections保持stats`，断言「隐藏时 connections 降流但 stats 不降」的
        // **差异化**门控。该差异随其前提作废（见 `Topic::gated_by_visibility` 的「为什么与 上游 表面形态
        // 不同」：上游的 status 不门控是 worker demand 握手载体 + 廉价 server-push 帧，两条前提 Polaris
        // 都没有；而 上游 广播侧 `StatsService.ts:312` / `StatsWorkerHost.ts:217` 本来就按可见性门控 stats）。
        //
        // 任何一条 topic 再被单独开成「隐藏也流」→ 本测转红。
        let mut r = SubscriptionRegistry::new();
        for t in [
            Topic::Stats,
            Topic::Connections,
            Topic::Detail,
            Topic::Closed,
        ] {
            r.subscribe(t, "win1");
            assert!(r.should_stream(t), "{t:?}：可见 + 有订阅 → 应保持流");
            assert!(
                t.gated_by_visibility(),
                "{t:?}：必须受可见性门控（口径一致）"
            );
        }
        r.set_window_visible(false);
        for t in [
            Topic::Stats,
            Topic::Connections,
            Topic::Detail,
            Topic::Closed,
        ] {
            assert!(
                !r.should_stream(t),
                "{t:?}：窗口隐藏 → 必须降流（无可见窗口 = 无 UI 消费者，全部 topic 同一口径）"
            );
        }
        r.set_window_visible(true);
        for t in [
            Topic::Stats,
            Topic::Connections,
            Topic::Detail,
            Topic::Closed,
        ] {
            assert!(r.should_stream(t), "{t:?}：窗口回来 → 全部一起恢复");
        }
    }

    #[test]
    fn window_visible_defaults_true() {
        let r = SubscriptionRegistry::new();
        // fail-open 缺省：真值由调用方首拍按窗口实况回写，缺省只保证「不确定」时不先饿死 UI。
        assert!(r.window_visible());
    }

    #[test]
    fn clear_topic_empties_only_that_topic() {
        let mut r = SubscriptionRegistry::new();
        r.subscribe(Topic::Stats, "w1");
        r.subscribe(Topic::Connections, "w1");
        r.clear_topic(Topic::Connections);
        assert_eq!(r.subscriber_count(Topic::Connections), 0);
        assert_eq!(r.subscriber_count(Topic::Stats), 1);
    }

    #[test]
    fn clear_all_empties_everything() {
        let mut r = SubscriptionRegistry::new();
        r.subscribe(Topic::Stats, "w1");
        r.subscribe(Topic::Connections, "w1");
        r.subscribe(Topic::Detail, "w1");
        r.clear_all();
        assert!(!r.has_any_subscriber());
    }

    #[test]
    fn has_any_subscriber_tracks_across_topics() {
        let mut r = SubscriptionRegistry::new();
        assert!(!r.has_any_subscriber());
        r.subscribe(Topic::Detail, "w1");
        assert!(r.has_any_subscriber());
    }

    #[test]
    fn same_subscriber_id_can_subscribe_multiple_times() {
        let mut r = SubscriptionRegistry::new();
        let _t1 = r.subscribe(Topic::Stats, "win1");
        let _t2 = r.subscribe(Topic::Stats, "win1"); // 同 id 再订一次
        assert_eq!(r.subscriber_count(Topic::Stats), 2);
    }
}
