//! polaris-stats-engine — 上游 `StatsService` + `stats-worker` + `connections-aggregate` 的纯逻辑层移植
//! （§B.2 / §H B5 纯逻辑部分）。
//!
//! 职责（不含 gRPC 流接收——那依赖 B2-b singbox-grpc，在 B5 actor 集成层接）：
//! 1. **stats 聚合** [`aggregator`]：连接聚合（按 tag/outbound 去重计数）、流量累计（uplink/downlink）、
//!    速率计算、connMap 维护（reset/NEW/UPDATE/CLOSED + LRU + OOM 驱逐）。
//! 2. **订阅注册表** [`subscription`]：订阅者注册/注销 + 无订阅者时降流判定（维度7：降流省资源）。
//! 3. **长流周期重建** [`resubscribe`]：gRPC 流断连重连策略（周期重建 + generation 守卫，维度7 #6）。
//! 4. **诊断分轴计数** [`diagnostic`]：lastStartReadyRetries（慢起）/ restartCount（核崩）分轴计数（维度7 #11）。
//! 5. **流注入抽象** [`stream`]：`trait StatsStream`（StatusStream/ConnectionEventStream），测试 mock。
//! 6. **诊断脱敏** [`redact`]：键名黑名单打码 + url 只留 origin + 节点身份占位（**红线：报告贴公开 issue，
//!    绝不含明文密钥**）。
//! 7. **诊断报告构建** [`diagnostic_report`]：脱敏后输入 → 单 Markdown（纯拼装，无 IO）。
//!
//! 纯逻辑：所有 gRPC 帧经 [`stream::StatusStream`] / [`stream::ConnectionEventStream`] trait 注入；
//! 订阅/重订阅/降流决策经 `&self`/`&mut self` 同步方法返回，上层 actor 执行真实 I/O。
//!
//! 锚点（Polaris 源真值）：
//! - `StatsService.ts`（401 行）：订阅注册表 + 聚合算法 + 降流 + resubscribe。
//! - `stats-worker.ts`（113 行）：worker 进程的 stats 解析 + 需求驱动门控。
//! - `connections-aggregate.ts`（111 行）：拓扑聚合纯函数 + change-driven 签名。
//! - `ProxyManager.ts`（lastStartReadyRetries:536 / restartCount:548）：诊断分轴。
//!
//! 见 ~/docs/polaris/design/polaris-system-design.md §B.2（crate 边界）；行为不变式见
//! vault 的能力入口点清单（special-logic 分册）（维度7 淬火）。

#![forbid(unsafe_code)]

pub mod aggregator;
pub mod diagnostic;
pub mod diagnostic_report;
pub mod emit_gate;
pub mod redact;
pub mod resubscribe;
pub mod stream;
pub mod subscription;
pub mod types;

// 顶层便利重导出（常用类型/函数无需写全路径）。
pub use aggregator::{
    aggregate_connections, aggregate_connections_with_topn, aggregate_signature,
    created_at_to_rfc3339, host_name_of, outbound_of, split_host_port, trim_connection,
    StatsAggregator, MAX_CONN_MAP_SIZE,
};
pub use diagnostic::{
    DiagnosticCounters, StartAttempt, MAX_RESTART_COUNT as DIAG_MAX_RESTART_COUNT,
    RESTART_COOLDOWN_MS as DIAG_RESTART_COOLDOWN_MS,
};
pub use diagnostic_report::{
    assemble_diagnostic_report, build_diagnostic_report, AppSection, DiagnosticReportInput,
    DiagnosticReportSource, RuntimeSection,
};
pub use emit_gate::EmitGate;
pub use redact::{
    collect_custom_secret_keys, collect_node_identifiers, normalize_key, redact_deep,
    redact_identifiers, redact_url_value, NodeIdentifier, REDACTED, SECRET_KEYS, URL_KEYS,
};
pub use resubscribe::{
    ResubscribeDecision, ResubscribeKind, ResubscribeStrategy, STREAM_RESUBSCRIBE_INTERVAL,
};
pub use stream::{ConnectionEventStream, StatusStream, VecConnectionEventStream, VecStatusStream};
pub use subscription::{SubscriptionRegistry, SubscriptionToken, Topic};
pub use types::{
    ClosedConnectionEntry, ConnectionAggFlow, ConnectionAggHost, ConnectionAggOutbound,
    ConnectionEntry, ConnectionEventType, ConnectionMetadata, ConnectionsAggregate,
    ConnectionsClosedSnapshot, ConnectionsClosedUpdate, ConnectionsSnapshot, SingBoxConnection,
    SingBoxConnectionEvent, SingBoxConnectionEvents, SingBoxProcessInfo, SingBoxStatus,
    TrafficStats, TOPOLOGY_OTHERS_KEY, TOPOLOGY_TOP_N,
};
