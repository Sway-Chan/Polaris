//! 流自动重连。
//!
//! 对齐 上游 `subscribeStream`（singbox-api-client.ts L647）：server-streaming 订阅独立连接，
//! 断开（stream `error`/`end`）后按固定间隔（2s）重建——重新建 client + 重新发 Subscribe 请求。
//!
//! [`ReconnectingStream<T>`] 实现 [`futures_core::Stream`]：内部持有一个 tonic `Streaming<T>`，
//! `poll_next` 时若底层流结束/出错，sleep `backoff` 后**重新调用连接器**重连，继续派发后续帧。
//! 重连失败也只退避重试，不向消费方 yield 错误（对齐 Polaris：`error`/`end` → scheduleReconnect，不 throw）。
//!
//! 连接过程经 [`Reconnect`] trait 对象化——`Status` 与 `ConnectionEvents` 两条流各自具化一个实现，
//! 规避「tonic `Streaming<Status>` 与 `Streaming<ConnectionEvents>` 是不同具体类型」的泛型分发难题。
//!
//! 边界：消费方 `drop` 该 stream → 重连 future 被 drop，后台自然停（对齐 Polaris 返回的 stop 句柄）。

use crate::daemon;
use crate::h2c;
use core::future::Future;
use daemon::started_service_client::StartedServiceClient;
use futures_core::{ready, Stream};
use futures_util::StreamExt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::Sleep;
use tonic::codec::Streaming;
// **门面必须与本仓真实接了 sink 的那个一致**：`log` 由 `src-tauri/src/logging.rs` 用
// `log::set_logger` 接到 stderr + 文件 + 内存环形缓冲。此前这里用的是 `tracing`，而全仓没有
// 任何 `tracing-subscriber` / `set_global_default` ⇒ 下面这几行**恒为 no-op，输出去向为空**。
// 代价不是抽象的：2026-08-05 那次 proto 字段号漂移（见 proto/started_service.proto 的
// `TailscaleEndpointStatus` 段）每帧都在 :331 那行报解码错，而用户与两轮排查者都一行日志没看到，
// 故障只表现为「首帧之后再没有第二帧」。同一根因因此扛过了两次修复。
use log::{debug, warn};

/// 重连策略。`backoff` = 断开后下次重连前的退避间隔（Polaris = 2s）。
#[derive(Clone, Copy, Debug)]
pub struct ReconnectConfig {
    /// 重连退避间隔。默认 2s（对齐 上游 `subscribeStream` 的 `setTimeout(..., 2000)`）。
    pub backoff: Duration,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            backoff: Duration::from_secs(2),
        }
    }
}

impl ReconnectConfig {
    /// 用指定退避间隔构造（测试用短间隔加速）。
    pub fn with_backoff(backoff: Duration) -> Self {
        Self { backoff }
    }
}

/// 连接器：每次（重）连时建 channel + client + 发 Subscribe 请求，返回 tonic Streaming<T>。
/// `Status` / `ConnectionEvents` 各自具化（避开 tonic 不同 Streaming 类型无法泛型分发）。
///
/// `open` 克隆 target/secret 进 future（owned 数据），返回 `'static` future——避免借用 lifetime 纠缠。
trait Reconnect<T>: Send + Sync + 'static {
    /// 建连并返回流。interval_ns 不可变（构造时固定），重连只换底层连接。
    fn open(
        &self,
        target: String,
        secret: Option<String>,
        interval_ns: i64,
    ) -> Pin<Box<dyn Future<Output = Result<Streaming<T>, String>> + Send>>;
}

/// 注入 Bearer metadata（h2c per-call auth，对齐 Polaris authMetadata）。secret 空则免认证。
fn auth_request<R>(secret: &Option<String>, req: R) -> tonic::Request<R> {
    let mut req = tonic::Request::new(req);
    if let Some(s) = secret {
        let val = format!("Bearer {s}");
        req.metadata_mut()
            .insert("authorization", val.parse().unwrap());
    }
    req
}

/// Status 流连接器。
struct StatusReconnect;

impl Reconnect<daemon::Status> for StatusReconnect {
    fn open(
        &self,
        target: String,
        secret: Option<String>,
        interval_ns: i64,
    ) -> Pin<Box<dyn Future<Output = Result<Streaming<daemon::Status>, String>> + Send>> {
        Box::pin(async move {
            let channel = h2c::connect_h2c(&target)
                .await
                .map_err(|e| format!("h2c connect: {e}"))?;
            let mut client = StartedServiceClient::new(channel);
            let req = auth_request(
                &secret,
                daemon::SubscribeStatusRequest {
                    interval: interval_ns,
                },
            );
            client
                .subscribe_status(req)
                .await
                .map_err(|e| format!("SubscribeStatus: {e}"))
                .map(|r| r.into_inner())
        })
    }
}

/// Connections 流连接器。
struct ConnectionsReconnect;

impl Reconnect<daemon::ConnectionEvents> for ConnectionsReconnect {
    fn open(
        &self,
        target: String,
        secret: Option<String>,
        interval_ns: i64,
    ) -> Pin<Box<dyn Future<Output = Result<Streaming<daemon::ConnectionEvents>, String>> + Send>>
    {
        Box::pin(async move {
            let channel = h2c::connect_h2c(&target)
                .await
                .map_err(|e| format!("h2c connect: {e}"))?;
            let mut client = StartedServiceClient::new(channel);
            let req = auth_request(
                &secret,
                daemon::SubscribeConnectionsRequest {
                    interval: interval_ns,
                },
            );
            client
                .subscribe_connections(req)
                .await
                .map_err(|e| format!("SubscribeConnections: {e}"))
                .map(|r| r.into_inner())
        })
    }
}

/// Tailscale STATUS 流连接器（`SubscribeTailscaleStatus`）。
///
/// 与 Status/Connections 两条流的唯一结构差异：请求是 `Empty`（无 interval）——核按自身节奏推
/// **全量端点快照**（每帧含所有 tailscale endpoint 的 backendState/self/peers），非增量。
/// 故 `open` 忽略 `interval_ns`。
struct TailscaleStatusReconnect;

impl Reconnect<daemon::TailscaleStatusUpdate> for TailscaleStatusReconnect {
    fn open(
        &self,
        target: String,
        secret: Option<String>,
        _interval_ns: i64,
    ) -> Pin<
        Box<dyn Future<Output = Result<Streaming<daemon::TailscaleStatusUpdate>, String>> + Send>,
    > {
        Box::pin(async move {
            let channel = h2c::connect_h2c(&target)
                .await
                .map_err(|e| format!("h2c connect: {e}"))?;
            let mut client = StartedServiceClient::new(channel);
            let req = auth_request(&secret, daemon::Empty {});
            client
                .subscribe_tailscale_status(req)
                .await
                .map_err(|e| format!("SubscribeTailscaleStatus: {e}"))
                .map(|r| r.into_inner())
        })
    }
}

/// Taildrop 收件箱流连接器（`SubscribeTaildropInbox`）。
///
/// **唯一带请求参数的连接器**：`endpointTag` 存在连接器自身，重连时原样重发 —— 若改成每次重连
/// 重新解析 tag，核重启后 tag 变了这条流会静默订到别的端点上去（或空端点），而流本身照常「活着」。
struct TaildropInboxReconnect {
    endpoint_tag: String,
}

impl Reconnect<daemon::TaildropInbox> for TaildropInboxReconnect {
    fn open(
        &self,
        target: String,
        secret: Option<String>,
        _interval_ns: i64,
    ) -> Pin<Box<dyn Future<Output = Result<Streaming<daemon::TaildropInbox>, String>> + Send>>
    {
        let endpoint_tag = self.endpoint_tag.clone();
        Box::pin(async move {
            let channel = h2c::connect_h2c(&target)
                .await
                .map_err(|e| format!("h2c connect: {e}"))?;
            let mut client = StartedServiceClient::new(channel);
            let req = auth_request(
                &secret,
                daemon::SubscribeTaildropInboxRequest { endpoint_tag },
            );
            client
                .subscribe_taildrop_inbox(req)
                .await
                .map_err(|e| format!("SubscribeTaildropInbox: {e}"))
                .map(|r| r.into_inner())
        })
    }
}

/// 核日志流连接器（`SubscribeLog`）。请求是 `Empty`（无 interval——核逐条推、短时多条合批），
/// 故 `open` 忽略 `interval_ns`。
///
/// **重连必然重发首帧**（`reset=true` + 至多 3000 行历史）：这是服务端语义，不是本层能压掉的。
/// 消费方须自己决定重连后的历史帧要不要收（收 = 整屏重放，不收 = 断连窗口内的行丢掉）。
struct LogReconnect;

impl Reconnect<daemon::Log> for LogReconnect {
    fn open(
        &self,
        target: String,
        secret: Option<String>,
        _interval_ns: i64,
    ) -> Pin<Box<dyn Future<Output = Result<Streaming<daemon::Log>, String>> + Send>> {
        Box::pin(async move {
            let channel = h2c::connect_h2c(&target)
                .await
                .map_err(|e| format!("h2c connect: {e}"))?;
            let mut client = StartedServiceClient::new(channel);
            let req = auth_request(&secret, daemon::Empty {});
            client
                .subscribe_log(req)
                .await
                .map_err(|e| format!("SubscribeLog: {e}"))
                .map(|r| r.into_inner())
        })
    }
}

/// 构造 Status 流（带自动重连）。target = `host:port`（h2c 内部加 http://）。
pub(crate) fn subscribe_status(
    target: String,
    secret: Option<String>,
    interval_ns: i64,
    cfg: ReconnectConfig,
) -> ReconnectingStream<daemon::Status> {
    ReconnectingStream::new_status(target, secret, interval_ns, cfg)
}

/// 构造 Connections 流（带自动重连）。
pub(crate) fn subscribe_connections(
    target: String,
    secret: Option<String>,
    interval_ns: i64,
    cfg: ReconnectConfig,
) -> ReconnectingStream<daemon::ConnectionEvents> {
    ReconnectingStream::new_connections(target, secret, interval_ns, cfg)
}

/// 构造 Tailscale STATUS 流（带自动重连）。无 interval（`SubscribeTailscaleStatus` 取 `Empty` 请求）。
pub(crate) fn subscribe_tailscale_status(
    target: String,
    secret: Option<String>,
    cfg: ReconnectConfig,
) -> ReconnectingStream<daemon::TailscaleStatusUpdate> {
    ReconnectingStream::new_tailscale_status(target, secret, cfg)
}

/// 构造 Taildrop 收件箱流（带自动重连）。无 interval；请求带 `endpointTag`，故 tag 存在连接器里
/// （`Reconnect::open` 的签名只有 target/secret/interval，重连时要能原样重发同一个 tag）。
pub(crate) fn subscribe_taildrop_inbox(
    target: String,
    secret: Option<String>,
    endpoint_tag: String,
    cfg: ReconnectConfig,
) -> ReconnectingStream<daemon::TaildropInbox> {
    ReconnectingStream::new_taildrop_inbox(target, secret, endpoint_tag, cfg)
}

/// 构造核日志流（带自动重连）。无 interval（`SubscribeLog` 取 `Empty` 请求）。
pub(crate) fn subscribe_logs(
    target: String,
    secret: Option<String>,
    cfg: ReconnectConfig,
) -> ReconnectingStream<daemon::Log> {
    ReconnectingStream::new_logs(target, secret, cfg)
}

/// 重连状态机。
enum State<T> {
    /// 首帧前（未建任何连接）。
    Initial,
    /// 正在退避等待下次重连。
    Backoff(Pin<Box<Sleep>>),
    /// 重连 future 在途。
    Connecting(Pin<Box<dyn Future<Output = Result<Streaming<T>, String>> + Send>>),
    /// 正在派发底层 tonic Streaming 的帧。Box：tonic Streaming 较大（~232B），装箱避免 enum 膨胀。
    Streaming(Box<Streaming<T>>),
}

/// 自动重连的 tokio Stream。消费方如普通 Stream 般 `while let Some(msg) = stream.next().await`。
///
/// 断开（底层流 error/end）→ sleep `backoff` → 调连接器 `open()` 重建 → 继续。永不向消费方
/// yield 错误或 None（除非此 stream 被 drop）。对齐 上游 `subscribeStream` 语义。
pub struct ReconnectingStream<T> {
    target: String,
    secret: Option<String>,
    interval_ns: i64,
    cfg: ReconnectConfig,
    connector: Box<dyn Reconnect<T>>,
    state: State<T>,
}

impl ReconnectingStream<daemon::Status> {
    pub(crate) fn new_status(
        target: String,
        secret: Option<String>,
        interval_ns: i64,
        cfg: ReconnectConfig,
    ) -> Self {
        Self {
            target,
            secret,
            interval_ns,
            cfg,
            connector: Box::new(StatusReconnect),
            state: State::Initial,
        }
    }
}

impl ReconnectingStream<daemon::ConnectionEvents> {
    pub(crate) fn new_connections(
        target: String,
        secret: Option<String>,
        interval_ns: i64,
        cfg: ReconnectConfig,
    ) -> Self {
        Self {
            target,
            secret,
            interval_ns,
            cfg,
            connector: Box::new(ConnectionsReconnect),
            state: State::Initial,
        }
    }
}

impl ReconnectingStream<daemon::TailscaleStatusUpdate> {
    pub(crate) fn new_tailscale_status(
        target: String,
        secret: Option<String>,
        cfg: ReconnectConfig,
    ) -> Self {
        Self {
            target,
            secret,
            // `SubscribeTailscaleStatus` 是 `Empty` 请求，无 interval 字段；连接器 open 忽略此值。
            interval_ns: 0,
            cfg,
            connector: Box::new(TailscaleStatusReconnect),
            state: State::Initial,
        }
    }
}

impl ReconnectingStream<daemon::TaildropInbox> {
    pub(crate) fn new_taildrop_inbox(
        target: String,
        secret: Option<String>,
        endpoint_tag: String,
        cfg: ReconnectConfig,
    ) -> Self {
        Self {
            target,
            secret,
            // `SubscribeTaildropInbox` 的请求只有 `endpointTag`，无 interval 字段；连接器 open 忽略此值。
            interval_ns: 0,
            cfg,
            connector: Box::new(TaildropInboxReconnect { endpoint_tag }),
            state: State::Initial,
        }
    }
}

impl ReconnectingStream<daemon::Log> {
    pub(crate) fn new_logs(target: String, secret: Option<String>, cfg: ReconnectConfig) -> Self {
        Self {
            target,
            secret,
            // `SubscribeLog` 是 `Empty` 请求，无 interval 字段；连接器 open 忽略此值。
            interval_ns: 0,
            cfg,
            connector: Box::new(LogReconnect),
            state: State::Initial,
        }
    }
}

impl<T> ReconnectingStream<T>
where
    T: Send + Sync + 'static,
{
    /// 取下一帧（`Stream::next` 便捷封装）。断开自动重连 → 永不返 `Err`；正常语义下也不返 `None`
    /// （除非内部终止）。
    ///
    /// **为消费方而设**：本 crate 有 `futures_util` 作正式依赖，故此处能 `StreamExt::next`；而下游
    /// src-tauri 的 `futures` 仅 dev-dependency（见 `runtime/stats.rs` 注），无法在生产代码里 `.next()`。
    /// 暴露 `recv().await` 让其无需引 `futures` 即可逐帧消费（TS STATUS relay 用）。cancel-safe：内部状态
    /// 存于本结构体、不在返回的 future 里，`select!`/`timeout` 丢弃它只是停轮询，下次 `recv` 续上。
    pub async fn recv(&mut self) -> Option<T> {
        self.next().await
    }
}

impl<T> Stream for ReconnectingStream<T>
where
    T: Send + Sync + 'static,
{
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                State::Initial => {
                    debug!("gRPC 流首次建连：{}", this.target);
                    let fut = this.connector.open(
                        this.target.clone(),
                        this.secret.clone(),
                        this.interval_ns,
                    );
                    this.state = State::Connecting(fut);
                }
                State::Connecting(fut) => match fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(streaming)) => {
                        debug!("gRPC 流已连上：{}", this.target);
                        this.state = State::Streaming(Box::new(streaming));
                    }
                    Poll::Ready(Err(e)) => {
                        warn!("gRPC 流建连失败，退避后重连（{}）：{e}", this.target);
                        this.state = State::Backoff(Box::pin(tokio::time::sleep(this.cfg.backoff)));
                    }
                    Poll::Pending => return Poll::Pending,
                },
                State::Streaming(s) => match s.as_mut().poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(msg))) => return Poll::Ready(Some(msg)),
                    // **这一格不只是「网络断了」**：prost 解码失败（proto 字段号/类型与真核不符）
                    // 与真实断线在这里长得一模一样——都只是一个 `Err`，而重连语义会把它变成
                    // 无限循环，外部只看得到「没有下一帧」。故消息里必须带上原始错误文本：
                    // wire 不匹配时它是 `UnexpectedWireType` / `unexpected end group tag`，
                    // 一眼可与 `connection refused` 那类真断线区分开。
                    Poll::Ready(Some(Err(e))) => {
                        warn!("gRPC 流出错，退避后重连（{}）：{e}", this.target);
                        this.state = State::Backoff(Box::pin(tokio::time::sleep(this.cfg.backoff)));
                    }
                    Poll::Ready(None) => {
                        debug!("gRPC 流被服务端结束，退避后重连：{}", this.target);
                        this.state = State::Backoff(Box::pin(tokio::time::sleep(this.cfg.backoff)));
                    }
                    Poll::Pending => return Poll::Pending,
                },
                State::Backoff(sleep) => {
                    ready!(sleep.as_mut().poll(cx));
                    debug!("gRPC 退避结束，重连：{}", this.target);
                    let fut = this.connector.open(
                        this.target.clone(),
                        this.secret.clone(),
                        this.interval_ns,
                    );
                    this.state = State::Connecting(fut);
                }
            }
        }
    }
}
