//! polaris-singbox-grpc — sing-box 1.14 管理 API（daemon.StartedService）的 tonic gRPC 客户端。
//!
//! 移植自 上游 `src/main/services/singbox-api-client.ts`（722 行）。Polaris 用 @grpc/grpc-js + proto-loader，
//! proto 内嵌在 TS 里；本 crate 把 proto vendored 进 `proto/started_service.proto`，经 build.rs（tonic-prost-build）编译。
//!
//! 暴露面（clash 等价管理方法）：
//! - [`SingBoxApiClient::select_outbound`]：selector/urltest group 内热切换出站（`SelectOutbound`）。
//! - [`SingBoxApiClient::first_groups_snapshot`]：读回各 group 的**运行期**选择（`SubscribeGroups` 首帧）。
//! - [`SingBoxApiClient::close_connection`] / [`SingBoxApiClient::close_all_connections`]：关单/全连接。
//! - [`SingBoxApiClient::subscribe_status`]：Status 流（内存/goroutine/上下行速率/累计）→ tokio Stream。
//! - [`SingBoxApiClient::subscribe_connections`]：Connection 事件流（NEW/UPDATE/CLOSED 增量 + reset）→ tokio Stream。
//! - [`SingBoxApiClient::subscribe_tailscale_status`]：Tailscale STATUS 流（全量端点快照：backendState/self/peers）→ tokio Stream。
//! - [`SingBoxApiClient::set_tailscale_exit_node`]：热重设 Tailscale 出口节点（按 stableID，不重启核）。
//! - [`SingBoxApiClient::default_log_level`]：读回核**此刻实际**在用的日志级别（`GetDefaultLogLevel`）。
//! - [`SingBoxApiClient::subscribe_logs`] / [`SingBoxApiClient::clear_logs`]：核日志流（结构化级别 +
//!   首帧 reset 带历史）与核侧日志环清空。
//!
//! 通道：sing-box 管理 API 走 **h2c**（HTTP/2 cleartext，非 TLS）——见私有 `h2c` 模块。
//! 认证：per-call metadata 注入 `authorization: Bearer <secret>`（secret 空则免认证）。
//! 重连：流断开后按周期自动重连（对齐 上游 `subscribeStream` 2s 重建策略）——见私有 `reconnect` 模块。
//!
//! # 关于日志流的一处**已订正**记载
//!
//! 本文件此前写着「daemon.StartedService 没有 Log 流，日志订阅属 clash-api 层」。那是错的：
//! 上游 `daemon/started_service.proto` 逐字含 `rpc SubscribeLog(google.protobuf.Empty) returns(stream Log)`
//! 与 `rpc ClearLogs(...)`（v1.14.0-beta.7 实读）。那句记载的代价是本仓一直在**猜**核日志的级别
//! （按行内 `FATAL`/`WARN` 等字符串 token 判），并为了看 debug 去改核配置 + 重启核（旧
//! `diagnosticCapture` 机制）。现已按上游契约接回结构化流，两者一并撤掉。
//!
//! 见 `~/docs/polaris/design/polaris-system-design.md` §B.2（crate 边界）。

#![forbid(unsafe_code)]

pub mod daemon {
    //! prost 编译产物（OUT_DIR/daemon.rs）。消息类型 + `StartedServiceClient`。
    include!(concat!(env!("OUT_DIR"), "/daemon.rs"));
}

mod h2c;
mod reconnect;

// vendored proto ⇄ 真核 wire 契约对拍器。**同一份文件被三处 `include!`**：
// `build.rs`（release 硬门，随包核）、`tests/bundled_core_wire.rs`（开发机 + 无核的机制自验）、
// 以及这里（**运行期**：换核前对拍用户即将换上的那份非随包核）。
//
// 为什么运行期也要一份：前两处的取材面都是 `resources/*/sing-box` 这四条路径，而**在线换核与
// 用户自带 fork 会让非随包核跑起来** —— 那条路径此前无任何 wire 对拍，正是 2026-08-05 那类
// 「字段号漂一位 ⇒ 整条流静默死掉」在本仓仍然敞着的一格。
//
// 不做成 `mod`：`build.rs` 不能依赖它正在构建的这个 crate，而三处必须共用同一份判据与符号表。
// 模块里的 `repo_root()` / `bundled_cores()` 依赖 `CARGO_MANIFEST_DIR`（构建期常量），
// 运行期无意义 —— 运行期只用 [`proto_wire_check::verdict_for_core_bytes`]（吃字节，不碰路径）。
include!("../proto_wire_check.rs");

pub use proto_wire_check::{verdict_for_core_bytes, WireVerdict};
pub use reconnect::{ReconnectConfig, ReconnectingStream};

/// tonic 再导出。
///
/// [`ClientError::Status`] 内嵌 `tonic::Status`，消费方要区分 Unauthenticated(16) / DeadlineExceeded(4)
/// 等状态码就必须能命名 `tonic::Code` —— 不再导出的话，消费方唯一的出路是自己也依赖 tonic 并
/// 锁死同一版本（版本一漂移，`Status` 就是两个不同的类型）。公开 API 暴露了第三方类型就该连带
/// 再导出该 crate，这是 tonic 生态的通行做法。
pub use tonic;

use daemon::started_service_client::StartedServiceClient;
use std::time::Duration;
use tonic::transport::Channel;
use tonic::Request;

/// unary 调用 deadline：对齐 上游 `UNARY_DEADLINE_MS`（2000ms）。
/// 核启动中（TCP accept 但 StartedService 方法尚未 serve）或 wedged 时 gRPC 永不返回 →
/// UI Close/Close-All 按钮永久 spinner；2s deadline 保证必 settle（DEADLINE_EXCEEDED）。
pub const UNARY_DEADLINE: Duration = Duration::from_millis(2000);

/// 连接首帧快照兜底超时：对齐 上游 `closeOldNodeConnectionsAfterHotSwitch` 的
/// `guard = setTimeout(..., 3000)`。首帧不来 → 放弃断连（宁可漏关，不泄漏订阅、不阻断已成功的热切换）。
pub const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(3);

/// 连接首帧订阅的推送间隔（纳秒）。对齐 上游 `subscribeConnections(1_000_000_000, cb)`——
/// 只取首帧（reset 全量），间隔实际不影响结果。
const SNAPSHOT_INTERVAL_NS: i64 = 1_000_000_000;

/// sing-box 管理 API 端点描述（host + port）。本地端点 → h2c（明文 HTTP/2）。
#[derive(Clone, Debug)]
pub struct Endpoint {
    host: String,
    port: u16,
}

impl Endpoint {
    /// 新建端点。`host` 为 IP/域名（IPv6 字面量如 `::1` 也接受，`target()` 会自动方括号包裹）。
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
    /// gRPC target 字符串。裸 IPv6 字面量（含 `:` 但不以 `[` 开头）方括号包裹，否则 `::1:9090` 非法。
    /// 对齐 上游 `target()` 判定（含 `:` 即包裹，最宽）。
    fn target(&self) -> String {
        let host = if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{host}:{}", self.port)
    }
}

/// sing-box 管理 API 的 tonic gRPC 客户端（clash 等价管理方法 + 流订阅）。
///
/// 构造时建立一条 h2c（或 TLS）长连接的 lazy channel；所有 unary/stream 调用复用该 channel。
/// Bearer secret 经 per-call metadata 注入（h2c 下 grpc-js call credentials 不可用，Rust 侧同理——
/// 走 per-request metadata 而非 channel-level creds，对齐 上游 `authMetadata`）。
pub struct SingBoxApiClient {
    channel: Channel,
    /// 原始 target（`host:port`，IPv6 已方括号包裹）；流订阅重连用（h2c::connect_h2c 会自加 http://）。
    target: String,
    secret: Option<String>,
}

impl SingBoxApiClient {
    /// 连接端点（h2c，明文 HTTP/2）。`secret` 空串 → 免认证；否则 per-call 注入 Bearer。
    pub async fn connect(
        endpoint: Endpoint,
        secret: impl Into<String>,
    ) -> Result<Self, ClientError> {
        let target = endpoint.target();
        let channel = h2c::connect_h2c(&target).await?;
        let secret = {
            let s = secret.into();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        Ok(Self {
            channel,
            target,
            secret,
        })
    }

    /// 注入 Bearer metadata 的请求包装。secret 空则原样返回（免认证）。
    fn with_auth<R>(&self, mut req: Request<R>) -> Request<R> {
        if let Some(secret) = &self.secret {
            let val = format!("Bearer {secret}");
            // secret 经构造校验为非空串；header value 合法 ASCII——unwrap 安全。
            req.metadata_mut()
                .insert("authorization", val.parse().unwrap());
        }
        req
    }

    fn client(&self) -> StartedServiceClient<Channel> {
        // clone()：tonic Channel 是 Arc 内柄，克隆廉价，每次调用建独立 client stub（&mut 自洽）。
        StartedServiceClient::new(self.channel.clone())
    }

    /// clash 等价：在 selector/urltest group 内热切换出站。
    /// `selector_tag` = group tag，`member_tag` = 目标出站 tag。
    /// 带 [`UNARY_DEADLINE`] deadline 保证必 settle。
    pub async fn select_outbound(
        &self,
        selector_tag: impl Into<String>,
        member_tag: impl Into<String>,
    ) -> Result<(), ClientError> {
        let mut c = self.client();
        let mut req = self.with_auth(Request::new(daemon::SelectOutboundRequest {
            group_tag: selector_tag.into(),
            outbound_tag: member_tag.into(),
        }));
        req.set_timeout(UNARY_DEADLINE);
        c.select_outbound(req).await?;
        Ok(())
    }

    /// Tailscale：热重设出口节点（不重启核）。按 `endpoint_tag` 定位具体 tailscale 端点，
    /// `stable_id` = 目标出口节点的 `TailscalePeer.stableID`（对齐 proto `SetTailscaleExitNodeRequest`）。
    /// 服务端按 stableID EditPrefs{ExitNodeID}，幂等。带 [`UNARY_DEADLINE`] deadline 保证必 settle。
    pub async fn set_tailscale_exit_node(
        &self,
        endpoint_tag: impl Into<String>,
        stable_id: impl Into<String>,
    ) -> Result<(), ClientError> {
        let mut c = self.client();
        let mut req = self.with_auth(Request::new(daemon::SetTailscaleExitNodeRequest {
            endpoint_tag: endpoint_tag.into(),
            stable_id: stable_id.into(),
        }));
        req.set_timeout(UNARY_DEADLINE);
        c.set_tailscale_exit_node(req).await?;
        Ok(())
    }

    /// clash 等价：按 id 关闭单条连接。
    pub async fn close_connection(&self, id: impl Into<String>) -> Result<(), ClientError> {
        let mut c = self.client();
        let mut req = self.with_auth(Request::new(daemon::CloseConnectionRequest {
            id: id.into(),
        }));
        req.set_timeout(UNARY_DEADLINE);
        c.close_connection(req).await?;
        Ok(())
    }

    /// clash 等价：关闭全部连接（Empty 请求）。
    pub async fn close_all_connections(&self) -> Result<(), ClientError> {
        let mut c = self.client();
        let mut req = self.with_auth(Request::new(daemon::Empty {}));
        req.set_timeout(UNARY_DEADLINE);
        c.close_all_connections(req).await?;
        Ok(())
    }

    /// 读回核**此刻实际**在用的日志级别（`GetDefaultLogLevel` → `logFactory.Level()`）。
    ///
    /// # 它回答的问题，以及为什么 `config.logLevel` 回答不了
    ///
    /// 盘上那个值是「我写下的意图」，与核在跑的级别有两条已实证的分叉：
    /// ① 隐私锁开启时生成侧走 `LogLevel::effective(privacy)` 抬到 ≥warn（核跑 warn，UI 显示 info）；
    /// ② 配置暂存态下改级别是零 IPC 零磁盘写（控件已高亮新级别，核仍按旧级别记录）。
    /// 两条都不是渲染端能自己补偿的 —— 只有把核的值读回来才算自证。
    ///
    /// # 核未运行时**必然**报错，这是设计内的
    ///
    /// 服务端先 RLock 检查 `serviceStatus.Status ∈ {STARTING, STARTED}`，否则返回 `os.ErrInvalid`
    /// （gRPC 侧现形为一个 `Status`）。调用方必须把它呈现成明确的「未知/未运行」态，
    /// **不得回落成某个具体级别** —— 回落出来的那个值一定是「我写下的值」，这处自证就退化成
    /// 它本要揭穿的那句谎。
    ///
    /// 带 [`UNARY_DEADLINE`] deadline 保证必 settle（核 wedged 时不挂死 UI）。
    pub async fn default_log_level(&self) -> Result<daemon::LogLevel, ClientError> {
        let mut c = self.client();
        let mut req = self.with_auth(Request::new(daemon::Empty {}));
        req.set_timeout(UNARY_DEADLINE);
        let resp = c.get_default_log_level(req).await?.into_inner();
        // prost 对未知枚举值回落 default(=PANIC)，那会把「上游加了新级别」伪装成「核在 panic 级」。
        // 故用 try_from 自己判：识别不出就报错，由调用方呈现成「未知」——不猜。
        daemon::LogLevel::try_from(resp.level).map_err(|_| {
            ClientError::Status(tonic::Status::out_of_range(format!(
                "核返回了本仓不认识的日志级别序号 {}（上游 LogLevel 枚举可能已扩），\
                 拒绝猜测——请对着随包核 descriptor 更新 proto/started_service.proto",
                resp.level
            )))
        })
    }

    /// 取连接列表**首帧全量快照**（精准断连用），带 [`SNAPSHOT_TIMEOUT`] 兜底。
    ///
    /// SubscribeConnections 的首帧是 reset 帧（内核当前全量连接），Polaris
    /// `closeOldNodeConnectionsAfterHotSwitch` 正是订阅后取首帧即退订。此处用**一次性**订阅
    /// （非 [`Self::subscribe_connections`] 的自动重连流）：取到首帧即 drop stream → 连接自然关闭。
    ///
    /// 首帧超时（3s）→ [`ClientError::SnapshotTimeout`]（调用方跳过断连，不阻断已成功的 PUT）。
    /// 返回快照含**已关闭的死连接**（内核历史环）——由调用方按 `closedAt` 过滤。
    pub async fn first_connection_snapshot(&self) -> Result<Vec<daemon::Connection>, ClientError> {
        let mut c = self.client();
        let req = self.with_auth(Request::new(daemon::SubscribeConnectionsRequest {
            interval: SNAPSHOT_INTERVAL_NS,
        }));
        let fut = async move {
            let mut stream = c.subscribe_connections(req).await?.into_inner();
            // 首帧 = reset 全量。流结束（None）→ 空快照（核已停/无连接），非错误。
            let first = stream.message().await?;
            Ok::<_, ClientError>(
                first
                    .map(|ev| ev.events.into_iter().filter_map(|e| e.connection).collect())
                    .unwrap_or_default(),
            )
        };
        // deadline 必须裹住「建流 + 首帧」整体：核 wedged 时 subscribe 本身就可能永不返回。
        match tokio::time::timeout(SNAPSHOT_TIMEOUT, fut).await {
            Ok(r) => r,
            Err(_) => Err(ClientError::SnapshotTimeout),
        }
    }

    /// 取各出站 group 的**运行期选择**快照（`SubscribeGroups` 首帧），带 [`SNAPSHOT_TIMEOUT`] 兜底。
    ///
    /// # 为什么是「订阅取首帧」而不是一次 unary
    ///
    /// 上游 `daemon.StartedService` **没有** unary 的 group 读方法，只有这条 server-stream。但它的
    /// 服务端实现在进入等待前先发一帧当前快照（见 proto 内注释引的 `started_service.go`），所以
    /// 「订阅 → 取首帧 → drop stream」就是一次完整的一次性读，与 [`Self::first_connection_snapshot`]
    /// 同型。drop 即关流，不留后台订阅。
    ///
    /// 返回的每个 `Group` 里，`selected` 是**核此刻实际指着的成员 tag**——与生成 config 的 `default`
    /// 可能分叉（`cache_file.store_selected` 起核时覆盖），这正是本方法存在的理由。
    ///
    /// 流直接结束（无帧）→ 空快照，**非错误**（核正在停 / 尚无 group）。调用方据「查不到 group」
    /// 与「查到但值不对」分别处置，不得把前者当后者。
    pub async fn first_groups_snapshot(&self) -> Result<Vec<daemon::Group>, ClientError> {
        let mut c = self.client();
        let req = self.with_auth(Request::new(daemon::Empty {}));
        let fut = async move {
            let mut stream = c.subscribe_groups(req).await?.into_inner();
            let first = stream.message().await?;
            Ok::<_, ClientError>(first.map(|g| g.group).unwrap_or_default())
        };
        // deadline 必须裹住「建流 + 首帧」整体：服务端的 `waitForStarted` 会在核尚未 STARTED 时
        // 一直挂着，没有这层超时就是永久挂起（同 `first_connection_snapshot` 的理由）。
        match tokio::time::timeout(SNAPSHOT_TIMEOUT, fut).await {
            Ok(r) => r,
            Err(_) => Err(ClientError::SnapshotTimeout),
        }
    }

    /// clash 等价：订阅 Status 流。`interval_ns` = 推送间隔（纳秒，int64；对齐 Polaris subscribeStatus）。
    /// 返回一个会**自动重连**的 tokio Stream（断开后按 [`ReconnectConfig`] 周期重建）。
    pub fn subscribe_status(
        &self,
        interval_ns: i64,
        cfg: ReconnectConfig,
    ) -> ReconnectingStream<daemon::Status> {
        reconnect::subscribe_status(self.target.clone(), self.secret.clone(), interval_ns, cfg)
    }

    /// clash 等价：订阅 Connection 事件流（NEW/UPDATE/CLOSED 增量 + reset 全量重置）。
    /// 返回一个会**自动重连**的 tokio Stream。
    pub fn subscribe_connections(
        &self,
        interval_ns: i64,
        cfg: ReconnectConfig,
    ) -> ReconnectingStream<daemon::ConnectionEvents> {
        reconnect::subscribe_connections(self.target.clone(), self.secret.clone(), interval_ns, cfg)
    }

    /// 订阅核日志流（`SubscribeLog`）。返回一个会**自动重连**的 tokio Stream。
    ///
    /// # 帧语义（消费方必须按这个来，否则不是丢行就是重放）
    ///
    /// - **`reset = true`**：本帧之前的内容作废。订阅首帧恒是它，且携带核侧至多 3000 行历史
    ///   （`daemon/attached_service.go` 的 `defaultAttachedLogMaxLines`）；`ClearLogs` 之后也会来一帧，
    ///   那时 `messages` 为空。
    /// - 其余帧是增量（服务端把短时间内的多条合批进 `messages`）。
    ///
    /// # 这是**全级别**流，级别筛在客户端
    ///
    /// 喂它的 `WriteMessage` 走的是 logFactory 的 platform writer 分发，那一段不受 `log.level`
    /// 过滤（见 proto/started_service.proto 里 `SubscribeLog` 的段落）⇒ 核恒把 trace 在内的每一条
    /// 都推过来。要看 debug **不需要**改核配置、更不需要重启核；反过来，消费方不筛就等于把 trace
    /// 洪流原样灌进 UI 与磁盘。
    ///
    /// # 它盖不住起核期
    ///
    /// 核 `StartStateStarted` 之后才 `AttachPlatformWriter` ⇒ 起核期的日志（含 TUN 装地址失败那类
    /// FATAL）结构性不在本流里，只能从核 stderr 捞。别据本流的沉默判断「核没说过话」。
    pub fn subscribe_logs(&self, cfg: ReconnectConfig) -> ReconnectingStream<daemon::Log> {
        reconnect::subscribe_logs(self.target.clone(), self.secret.clone(), cfg)
    }

    /// 清空核侧那 3000 行日志环（`ClearLogs`）。带 [`UNARY_DEADLINE`] deadline 保证必 settle。
    ///
    /// 「清空日志」必须两侧一起清：只清本地环形缓冲的话，[`Self::subscribe_logs`] 一旦重订阅，
    /// 核那份历史会整份回来 —— 用户看到的是「清了又自己长回来」。
    pub async fn clear_logs(&self) -> Result<(), ClientError> {
        let mut c = self.client();
        let mut req = self.with_auth(Request::new(daemon::Empty {}));
        req.set_timeout(UNARY_DEADLINE);
        c.clear_logs(req).await?;
        Ok(())
    }

    /// 订阅 Tailscale STATUS 流（`SubscribeTailscaleStatus`）。每帧 = **全量端点快照**
    /// （所有 tailscale endpoint 的 `backendState` / `self`（本节点 IP、key 过期）/ `userGroups`（对端）/
    /// `exitNode`），核按自身节奏推、非增量。返回一个会**自动重连**的 tokio Stream。
    ///
    /// 无 `interval_ns` 入参——请求是 `Empty`（对齐 上游 `subscribeTailscaleStatus`，proto
    /// `SubscribeTailscaleStatus(Empty) returns (stream TailscaleStatusUpdate)`）。消费方 `drop`
    /// 该 stream → 重连 future 被 drop、后台自然停（同 `subscribe_status`）。
    pub fn subscribe_tailscale_status(
        &self,
        cfg: ReconnectConfig,
    ) -> ReconnectingStream<daemon::TailscaleStatusUpdate> {
        reconnect::subscribe_tailscale_status(self.target.clone(), self.secret.clone(), cfg)
    }
}

/// 客户端错误：连接失败 / tonic Status（含 Unauthenticated=16 / DeadlineExceeded=4）。
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("transport error connecting to sing-box management API: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("gRPC status: {0}")]
    Status(#[from] tonic::Status),
    /// 连接首帧订阅超时（[`SNAPSHOT_TIMEOUT`] 兜底）。调用方据此跳过精准断连。
    #[error("connection snapshot first-frame timeout")]
    SnapshotTimeout,
}
