/**
 * 节点测速 —— 渲染端持有的最小面：`api.speedTest()` 的 invoke 返回契约（①类跨语言类型镜像）。
 *
 * **测速逻辑的单一真值全在 Rust**：
 *  - 目标 URL 解析 + 分波编排 + 让位中断：`src-tauri/src/commands/speedtest.rs`
 *    （`DEFAULT_SPEED_TEST_URL` / `resolve_speed_test_url` / `plan_waves` / `drive_pool_waves`）；
 *  - 探测池槽数 K + 槽热切：`src-tauri/src/runtime/proxy.rs` 的 `PROBE_POOL_SIZE` / `probe_select_slot`；
 *  - 池 inbound 注入（probe-in-k）：`crates/config-engine/src/builder/inbounds.rs` 的 `probe_pool_ports`。
 *
 * 曾另有 `crates/speedtest`（照 Electron 三路径形态 1:1 建的纯逻辑层：`MainCoreProbe` trait /
 * `run_with_limit` / `aggregator` / 带宽 / RTT 统计）。应用层接线时就地重写了一遍，该 crate 除
 * `DEFAULT_SPEED_TEST_URL` 外**无任何消费者**，其抽象也与 Tauri 侧实际形态不符（无 ICMP 路径、
 * 无带宽测量、排序在渲染端、槽↔端口 1:1 绑定不能过 worker 池）⇒ 已整体删除，常量就近落在消费者处。
 *
 * 原 TS 副本是 上游 主进程 SpeedTestService / ProxyManager 的照搬（要开 socket、要分配端口、要 gRPC 热切
 * selector）——在 Tauri 的 webview 里结构性跑不起来，留着=假装能力还在，故第二轮 shared 清理已删（审计 §D，
 * 同 §A3 DNS race 的处置先例）。设置页测速 URL 的输入校验走提交时 invoke，不在前端复算。
 */

/** 测速本次运行结局（§16.2）：completed=本次入参全部有结果（含真实 -1 超时）；interrupted=有入参节点缺席
 *  （核 stop/restart/regen 跃迁或崩溃打断，保留旧值不写假 -1）。「起测即知不可测」（skipped）不产生 interrupted。 */
export type SpeedTestOutcome = 'completed' | 'interrupted';

/** SERVER_SPEED_TEST invoke 返回（renderer 消费 §16.2/§16.3.3）：Record 化结果（null→-1）+ outcome + 波前缺席两列表。
 *
 *  `notInPool` = 请求了但不在**运行核**测速池里的节点（订阅新增/改址后未重启核 ⇒ 出站 tag 尚不是
 *  `probe-selector-k` 成员）→ 本轮如实缺席，绝不伪造 -1。NodesScreen/HomeScreen 据此 toast「N 个节点
 *  未纳入本次测速（重启内核后纳入）」。
 *
 *  探针池已接线（`run_pool_speed_test` 分波批量是常规路径）；仅当起核时池端口分配失败回退，才降级到
 *  「只测当前活跃出口」并返 `SPEEDTEST_PROBE_POOL_UNWIRED`（码名是历史遗留，语义是**本次不可用**）。 */
export interface SpeedTestInvokeResult {
  results: Record<string, number>;
  outcome: SpeedTestOutcome;
  notInPool: string[];
  tsNotReady: string[];
}

/**
 * `event:speedTestDone` 载荷（后端 `events.rs::EVENT_SPEED_TEST_DONE`，三条腿各在唯一出口发一次）。
 *
 * # 为什么终态要走事件，而不是继续读上面那个 invoke 返回值
 *
 * `SpeedTestInvokeResult.outcome` 只有**发起那次 invoke 的 JS 堆**拿得到。托盘浮层是独立 webview /
 * 独立 JS 堆 ⇒ 托盘发起的那轮测速，主窗的进度 toast 结构上收不到终态，只能靠静默超时去猜「是不是
 * 被打断了」。而后端在让位判据命中那一刻就已经知道并返回了 `interrupted`。改成广播后主窗**当场**收敛。
 *
 * `serverIds` = 本轮已裁定要测的原始节点范围（= 中断后「重新测速」的输入）。
 * `pending` = 本轮已裁定要测、但**没拿到值**的节点 id（= 中断后「继续剩余」的输入）。差集由后端算：
 * 前端只在自己发起时才知道请求集。波前预筛掉的 notInPool/dirty/tsNotReady **不在**其中。
 */
export interface SpeedTestDonePayload {
  outcome: SpeedTestOutcome;
  /** 已出值的节点数（含真实 -1）。 */
  tested: number;
  /** 本轮已裁定要测的节点数（与进度事件的 `total` 同一口径）。 */
  total: number;
  /** 本轮原始可测范围；重测必须复用它，不能扩大成当前全部节点。 */
  serverIds: string[];
  /** 没拿到值的节点 id —— 续测的输入。 */
  pending: string[];
}
