//! 集成测试：mock gRPC server（h2c tonic transport::Server）验证客户端连接/认证/流/重连。
//!
//! 不触碰宿主网络：mock server 绑定 127.0.0.1 随机端口。客户端经 h2c 连接（明文 HTTP/2）。
//!
//! 覆盖：
//! - SelectOutbound unary（Bearer 认证 + 参数透传）。
//! - SubscribeGroups 首帧一次性读（`first_groups_snapshot`：拿到运行期 `selected` 即退订）。
//! - CloseConnection / CloseAllConnections unary。
//! - SetTailscaleExitNode unary（Bearer 认证 + 参数透传）。
//! - 认证缺失 → Unauthenticated。
//! - SubscribeStatus 流（多帧 + 帧内容）。
//! - SubscribeConnections 流（事件 + reset）。
//! - 流自动重连：server 关连接后客户端 backoff 重连，从新 server 继续收帧。

#![forbid(unsafe_code)]

use polaris_singbox_grpc::{Endpoint, ReconnectConfig, SingBoxApiClient, UNARY_DEADLINE};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use polaris_singbox_grpc::daemon::started_service_server::{StartedService, StartedServiceServer};
use polaris_singbox_grpc::daemon::{
    self, CloseConnectionRequest, Connection, ConnectionEvent, ConnectionEventType,
    ConnectionEvents, Empty, Group, Groups, SelectOutboundRequest, SetTailscaleExitNodeRequest,
    SubscribeConnectionsRequest, SubscribeStatusRequest, TailscaleEndpointStatus,
    TailscaleLogoutRequest, TailscalePeer, TailscaleStatusUpdate, TailscaleUserGroup,
};

const SECRET: &str = "test-secret-123";

/// mock server 状态：记录收到的调用 + 可配置的流行为。
#[derive(Default)]
struct MockState {
    select_calls: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    close_calls: Arc<std::sync::Mutex<Vec<String>>>,
    close_all_count: Arc<AtomicU64>,
    set_exit_node_calls: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// `SubscribeGroups` 首帧回放的 group 快照（测试预置）。
    groups: Arc<std::sync::Mutex<Vec<Group>>>,
    /// `SubscribeGroups` 被订阅的次数（验「一次性读」不留后台订阅）。
    groups_calls: Arc<AtomicU64>,
    /// `GetDefaultLogLevel` 回放的级号（`None` = 复现核未运行时上游报错那一支）。
    default_log_level: Arc<std::sync::Mutex<Option<i32>>>,
    /// `SubscribeLog` 首帧回放的「核侧历史」（对应真核那 3000 行环）。
    log_history: Arc<std::sync::Mutex<Vec<daemon::log::Message>>>,
    /// `ClearLogs` 被调用的次数（验「清空必须两侧一起清」真的发到了核）。
    clear_logs_count: Arc<AtomicU64>,
}

struct MockService {
    secret: String,
    state: Arc<MockState>,
    /// 控制流持续推送多少帧后自行结束（模拟 server 端流中断，触发客户端重连）。
    status_frames: Arc<AtomicU64>,
    /// 同 `status_frames`，但作用于 Tailscale STATUS 流（每次连接推这么多帧后 drop tx）。
    ts_status_frames: Arc<AtomicU64>,
}

#[tonic::async_trait]
impl StartedService for MockService {
    type SubscribeTailscaleStatusStream = ReceiverStream<Result<TailscaleStatusUpdate, Status>>;
    async fn subscribe_tailscale_status(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<Self::SubscribeTailscaleStatusStream>, Status> {
        check_auth(&req, &self.secret)?;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let frames = self.ts_status_frames.load(Ordering::Relaxed);
        tokio::spawn(async move {
            for i in 0..frames {
                let _ = tx.send(Ok(canned_ts_update(i))).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // 不显式 end——drop tx 让流自然结束（触发客户端重连）。
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
    async fn tailscale_logout(
        &self,
        _req: Request<TailscaleLogoutRequest>,
    ) -> Result<Response<Empty>, Status> {
        Err(Status::unimplemented("not used in tests"))
    }
    async fn set_tailscale_exit_node(
        &self,
        req: Request<SetTailscaleExitNodeRequest>,
    ) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.secret)?;
        let r = req.into_inner();
        self.state
            .set_exit_node_calls
            .lock()
            .unwrap()
            .push((r.endpoint_tag, r.stable_id));
        Ok(Response::new(Empty {}))
    }

    type SubscribeStatusStream = ReceiverStream<Result<daemon::Status, Status>>;
    async fn subscribe_status(
        &self,
        req: Request<SubscribeStatusRequest>,
    ) -> Result<Response<Self::SubscribeStatusStream>, Status> {
        check_auth(&req, &self.secret)?;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let frames = self.status_frames.load(Ordering::Relaxed);
        let memory_base = req.get_ref().interval; // 复用 interval 字段当 memory 种子，区分调用
        tokio::spawn(async move {
            for i in 0..frames {
                let _ = tx
                    .send(Ok(daemon::Status {
                        // `memory` 是 uint64（对齐真核 descriptor），而 interval 种子是 int64 → 显式转。
                        memory: memory_base as u64 + i,
                        uplink: (i * 100) as i64,
                        downlink: (i * 200) as i64,
                        goroutines: 42,
                        ..Default::default()
                    }))
                    .await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // 不显式 end——drop tx 让流自然结束（触发客户端重连）。
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type SubscribeConnectionsStream = ReceiverStream<Result<ConnectionEvents, Status>>;
    async fn subscribe_connections(
        &self,
        req: Request<SubscribeConnectionsRequest>,
    ) -> Result<Response<Self::SubscribeConnectionsStream>, Status> {
        check_auth(&req, &self.secret)?;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let reset = req.get_ref().interval != 0; // interval!=0 → 首帧带 reset
        tokio::spawn(async move {
            if reset {
                let _ = tx
                    .send(Ok(ConnectionEvents {
                        events: vec![ConnectionEvent {
                            r#type: ConnectionEventType::New as i32,
                            id: "conn-1".into(),
                            connection: Some(Connection {
                                id: "conn-1".into(),
                                destination: "example.com:443".into(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }],
                        reset: true,
                    }))
                    .await;
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type SubscribeGroupsStream = ReceiverStream<Result<Groups, Status>>;
    /// 模拟服务端「进入等待前先发一帧当前快照」的语义：首帧即 group 全量，随后**不再发**（drop tx）。
    /// 客户端 `first_groups_snapshot` 必须只取这一帧就退订，而不是等第二帧（等 = 3s 超时）。
    async fn subscribe_groups(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<Self::SubscribeGroupsStream>, Status> {
        check_auth(&req, &self.secret)?;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let groups = self.state.groups.lock().unwrap().clone();
        self.state.groups_calls.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let _ = tx.send(Ok(Groups { group: groups })).await;
            // 后续帧要等 urlTest/status 观察者被触发；此处保持沉默即可模拟「首帧之后长时间无更新」。
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn select_outbound(
        &self,
        req: Request<SelectOutboundRequest>,
    ) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.secret)?;
        let r = req.into_inner();
        self.state
            .select_calls
            .lock()
            .unwrap()
            .push((r.group_tag, r.outbound_tag));
        Ok(Response::new(Empty {}))
    }

    async fn close_connection(
        &self,
        req: Request<CloseConnectionRequest>,
    ) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.secret)?;
        self.state
            .close_calls
            .lock()
            .unwrap()
            .push(req.into_inner().id);
        Ok(Response::new(Empty {}))
    }

    async fn close_all_connections(&self, req: Request<Empty>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.secret)?;
        self.state.close_all_count.fetch_add(1, Ordering::Relaxed);
        Ok(Response::new(Empty {}))
    }

    type SubscribeLogStream = ReceiverStream<Result<daemon::Log, Status>>;
    /// 复现真核 `SubscribeLog` 的两段语义：**首帧 `reset=true` + 历史全量**，随后逐条增量。
    /// 增量那两条刻意跨级别（TRACE / ERROR），用来证明本流与 `log.level` 无关（客户端自己筛）。
    async fn subscribe_log(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<Self::SubscribeLogStream>, Status> {
        check_auth(&req, &self.secret)?;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let history = self.state.log_history.lock().unwrap().clone();
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(daemon::Log {
                    messages: history,
                    reset: true,
                }))
                .await;
            for (level, msg) in [
                (daemon::LogLevel::Trace, "trace: dns exchange"),
                (daemon::LogLevel::Error, "error: dial failed"),
            ] {
                let _ = tx
                    .send(Ok(daemon::Log {
                        messages: vec![daemon::log::Message {
                            level: level as i32,
                            message: msg.into(),
                        }],
                        reset: false,
                    }))
                    .await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn clear_logs(&self, req: Request<Empty>) -> Result<Response<Empty>, Status> {
        check_auth(&req, &self.secret)?;
        self.state.clear_logs_count.fetch_add(1, Ordering::Relaxed);
        self.state.log_history.lock().unwrap().clear();
        Ok(Response::new(Empty {}))
    }

    /// 回放 `default_log_level`：`None` 复现**核未运行**那一支 —— 上游此时不返空值而是
    /// `os.ErrInvalid`（gRPC 侧一个错误 Status），客户端必须原样往上报错，不得回落成某个级别。
    async fn get_default_log_level(
        &self,
        req: Request<Empty>,
    ) -> Result<Response<daemon::DefaultLogLevel>, Status> {
        check_auth(&req, &self.secret)?;
        match *self.state.default_log_level.lock().unwrap() {
            Some(level) => Ok(Response::new(daemon::DefaultLogLevel { level })),
            None => Err(Status::failed_precondition("invalid argument")),
        }
    }
}

/// 校验 Bearer。secret 空则放行（免认证）；否则要求 `authorization: Bearer <secret>`。
fn check_auth<R>(req: &Request<R>, secret: &str) -> Result<(), Status> {
    if secret.is_empty() {
        return Ok(());
    }
    let v = req
        .metadata()
        .get("authorization")
        .ok_or_else(|| Status::unauthenticated("missing authorization"))?;
    let expected = format!("Bearer {secret}");
    if v != expected.as_str() {
        return Err(Status::unauthenticated("bad secret"));
    }
    Ok(())
}

/// 起一条 h2c mock server，返回 (addr, 共享 state)。server 随返回的 handle drop 而停。
async fn spawn_server(
    secret: &str,
    status_frames: u64,
) -> (SocketAddr, Arc<MockState>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(MockState::default());
    let svc = MockService {
        secret: secret.to_string(),
        state: state.clone(),
        status_frames: Arc::new(AtomicU64::new(status_frames)),
        ts_status_frames: Arc::new(AtomicU64::new(0)),
    };
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let handle = tokio::spawn(async move {
        let _ = Server::builder()
            .serve_with_incoming(StartedServiceServer::new(svc), incoming)
            .await;
    });
    (addr, state, handle)
}

/// 单端点 mock TS 状态帧：tag="ts-node"，backendState=Running，self IP + 1 个「可当出口」的 peer。
/// `seed` 掺进 self host 以区分不同帧/重连轮次。
fn canned_ts_update(seed: u64) -> TailscaleStatusUpdate {
    TailscaleStatusUpdate {
        endpoints: vec![TailscaleEndpointStatus {
            endpoint_tag: "ts-node".into(),
            backend_state: "Running".into(),
            auth_url: String::new(),
            self_: Some(TailscalePeer {
                host_name: format!("self-{seed}"),
                tailscale_i_ps: vec!["100.64.0.1".into()],
                online: true,
                expired: false,
                ..Default::default()
            }),
            user_groups: vec![TailscaleUserGroup {
                peers: vec![TailscalePeer {
                    host_name: "exit-peer".into(),
                    tailscale_i_ps: vec!["100.64.0.2".into()],
                    online: true,
                    exit_node_option: true,
                    stable_id: "peer-stable-1".into(),
                    ..Default::default()
                }],
            }],
            exit_node: None,
            // 其余字段（stateText / networkName / magicDNSSuffix / keyAuth）本 mock 不摆弄：
            // 它们是 1.14 beta 期真核新增的，本文件只覆盖 STATUS relay 关心的那几格。
            ..Default::default()
        }],
    }
}

/// 起一条只配置 TS 状态帧数的 mock server（其它 unary/流按默认，本处只覆盖 TS STATUS 路径）。
/// 返回 (addr, handle)。server 随 handle drop 而停。
async fn spawn_ts_server(
    secret: &str,
    ts_status_frames: u64,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let svc = MockService {
        secret: secret.to_string(),
        state: Arc::new(MockState::default()),
        status_frames: Arc::new(AtomicU64::new(0)),
        ts_status_frames: Arc::new(AtomicU64::new(ts_status_frames)),
    };
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let handle = tokio::spawn(async move {
        let _ = Server::builder()
            .serve_with_incoming(StartedServiceServer::new(svc), incoming)
            .await;
    });
    (addr, handle)
}

#[tokio::test]
async fn select_outbound_succeeds_with_bearer_auth() {
    let (addr, state, _h) = spawn_server(SECRET, 0).await;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .expect("connect");

    client
        .select_outbound("🇯🇵-selector", "jp-tokyo-01")
        .await
        .expect("select_outbound ok");

    let calls = state.select_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "🇯🇵-selector");
    assert_eq!(calls[0].1, "jp-tokyo-01");
}

/// **首帧一次性读**：`SubscribeGroups` 是 server-stream，但服务端先发一帧当前快照，
/// 故 `first_groups_snapshot` 必须拿到首帧就返回，**不得等到第二帧**（mock 首帧后沉默 30s，
/// 等第二帧就会撞 3s `SnapshotTimeout`）。
///
/// 读回的 `selected` 是**运行期**选择，这是「selector 与生成产物分叉」唯一能被观测到的地方。
///
/// **变异锁**：把 `first_groups_snapshot` 里的 `stream.message().await?` 改成连取两帧
/// （`let _ = stream.message().await?;` 再取一次）→ 撞 SnapshotTimeout → 转红。
#[tokio::test]
async fn first_groups_snapshot_reads_runtime_selection_from_first_frame() {
    let (addr, state, _h) = spawn_server(SECRET, 0).await;
    *state.groups.lock().unwrap() = vec![
        Group {
            tag: "proxy-selector".into(),
            r#type: "selector".into(),
            selectable: true,
            // 真机血证的形态：config 生成的 default 是 Hk01，运行期却停在上一轮的 Tailscale。
            selected: "Tailscale".into(),
            ..Default::default()
        },
        Group {
            tag: "rule-sel-r1".into(),
            r#type: "selector".into(),
            selectable: true,
            selected: "Hk01".into(),
            ..Default::default()
        },
    ];
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .expect("connect");

    let got = client
        .first_groups_snapshot()
        .await
        .expect("first_groups_snapshot ok");

    assert_eq!(got.len(), 2, "首帧应带回全部 group，实得 {got:?}");
    assert_eq!(got[0].tag, "proxy-selector");
    assert_eq!(
        got[0].selected, "Tailscale",
        "必须读回运行期选择（而不是生成产物的 default）"
    );
    assert_eq!(got[1].selected, "Hk01");
    assert_eq!(
        state.groups_calls.load(Ordering::Relaxed),
        1,
        "一次性读只订阅一次"
    );
}

/// 认证同样对读侧生效：secret 不对 → Unauthenticated，**不得**退化成「读到空快照」。
/// 空快照与「读不到」在上层是两种处置（前者不告警、后者也不告警但日志不同），
/// 若认证失败被压成空快照，上层就会把「没权限读」误当成「核确实没有 group」。
///
/// **变异锁**：把建流那次 `c.subscribe_groups(req).await?` 的错误吞掉（`let Ok(resp) = ... else
/// { return Ok(Vec::new()) }`）→ 被拒退化成空快照 → 转红。
///
/// **本条抓不到两件事，别误当它把守了**：① 「客户端漏发 Bearer」——客户端这里本就不带 secret，
/// 删 `with_auth` 对它零影响，那条由上一个用例（带 secret 的正向读）把守；② 吞掉
/// `stream.message()` 的错误——认证是在**建流**那一步被拒的，首帧读根本没跑到。
#[tokio::test]
async fn first_groups_snapshot_unauthenticated_without_secret() {
    let (addr, _state, _h) = spawn_server(SECRET, 0).await;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), "")
        .await
        .expect("connect");

    let err = client
        .first_groups_snapshot()
        .await
        .expect_err("should be unauthenticated");
    match err {
        polaris_singbox_grpc::ClientError::Status(s) => {
            assert_eq!(s.code(), tonic::Code::Unauthenticated, "err={s}");
        }
        other => panic!("expected Status error, got {other:?}"),
    }
}

#[tokio::test]
async fn select_outbound_unauthenticated_without_secret() {
    // server 要求 secret，客户端给空 → Unauthenticated。
    let (addr, _state, _h) = spawn_server(SECRET, 0).await;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), "")
        .await
        .expect("connect");

    let err = client
        .select_outbound("g", "m")
        .await
        .expect_err("should be unauthenticated");
    // Unauthenticated (code 16) 的 Display 形如 "Unauthenticated: ..." 或含认证描述；用 code 判定最稳。
    match err {
        polaris_singbox_grpc::ClientError::Status(s) => {
            assert_eq!(s.code(), tonic::Code::Unauthenticated, "err={s}");
        }
        other => panic!("expected Status error, got {other:?}"),
    }
}

#[tokio::test]
async fn set_tailscale_exit_node_succeeds_with_bearer_auth() {
    let (addr, state, _h) = spawn_server(SECRET, 0).await;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .expect("connect");

    client
        .set_tailscale_exit_node("ts-node", "peer-stable-1")
        .await
        .expect("set_tailscale_exit_node ok");

    let calls = state.set_exit_node_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "ts-node");
    assert_eq!(calls[0].1, "peer-stable-1");
}

#[tokio::test]
async fn set_tailscale_exit_node_unauthenticated_without_secret() {
    let (addr, _state, _h) = spawn_server(SECRET, 0).await;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), "")
        .await
        .expect("connect");

    let err = client
        .set_tailscale_exit_node("ts-node", "peer-stable-1")
        .await
        .expect_err("should be unauthenticated");
    match err {
        polaris_singbox_grpc::ClientError::Status(s) => {
            assert_eq!(s.code(), tonic::Code::Unauthenticated, "err={s}");
        }
        other => panic!("expected Status error, got {other:?}"),
    }
}

#[tokio::test]
async fn close_connection_and_close_all() {
    let (addr, state, _h) = spawn_server(SECRET, 0).await;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .unwrap();

    client.close_connection("conn-xyz").await.unwrap();
    client.close_all_connections().await.unwrap();

    let closed = state.close_calls.lock().unwrap();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0], "conn-xyz");
    assert_eq!(state.close_all_count.load(Ordering::Relaxed), 1);
}

/// **读回核在跑的真实级别**：`WARN`（4→不，序号 3）必须原样回来，不得被压成别的级别。
///
/// 用 `WARN` 而不是默认的 `INFO` 做样本是有意的：隐私锁下生成侧正是把 info 抬成 warn，
/// 「核跑 warn / 盘上写 info」就是这条 rpc 要揭穿的那个分叉。若客户端把响应丢了直接回默认值，
/// 拿到的会是 `PANIC`（prost 枚举 default = 0），断言即红。
#[tokio::test]
async fn default_log_level_reads_the_level_the_core_is_running() {
    let (addr, state, _h) = spawn_server(SECRET, 0).await;
    *state.default_log_level.lock().unwrap() = Some(daemon::LogLevel::Warn as i32);
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .expect("connect");

    assert_eq!(
        client.default_log_level().await.expect("读回级别"),
        daemon::LogLevel::Warn
    );
}

/// **核未运行 → 必须报错，不得回落成某个具体级别**。
///
/// 上游在核未 STARTING/STARTED 时返 `os.ErrInvalid`。这条腿一旦被谁「兜底」成
/// `unwrap_or(LogLevel::Info)`，日志页就会在核没跑的时候信誓旦旦地显示 INFO —— 那正是这处自证
/// 本要揭穿的那句谎，只是换了个地方说。
///
/// **变异锁**：把 `default_log_level` 的 `?` 换成 `unwrap_or(daemon::LogLevel::Info)` → 转红。
#[tokio::test]
async fn default_log_level_errors_when_core_not_running() {
    let (addr, state, _h) = spawn_server(SECRET, 0).await;
    *state.default_log_level.lock().unwrap() = None; // 复现 os.ErrInvalid 那一支
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .expect("connect");

    assert!(
        client.default_log_level().await.is_err(),
        "核未运行时必须报错，绝不回落成某个级别"
    );
}

/// 核回了本仓枚举里没有的序号（上游扩了 `LogLevel`）→ 必须报错，**不得静默变成 PANIC**。
///
/// prost 的 `i32 → enum` 在未知值上回落 default(=0=PANIC)，那会把「上游加了新级别」伪装成
/// 「核正跑在 panic 级」——一个看起来完全正常、实际全错的显示。
#[tokio::test]
async fn default_log_level_rejects_unknown_level_number() {
    let (addr, state, _h) = spawn_server(SECRET, 0).await;
    *state.default_log_level.lock().unwrap() = Some(99);
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .expect("connect");

    assert!(
        client.default_log_level().await.is_err(),
        "不认识的级号必须报错而不是猜成 PANIC"
    );
}

#[tokio::test]
async fn unary_deadline_is_two_seconds() {
    // UNARY_DEADLINE 常量对齐 Polaris UNARY_DEADLINE_MS=2000。
    assert_eq!(UNARY_DEADLINE, Duration::from_millis(2000));
}

#[tokio::test]
async fn subscribe_status_delivers_frames() {
    // server 推 3 帧后结束流。interval 字段当 memory 种子 = 1000。
    let (addr, _state, _h) = spawn_server(SECRET, 3).await;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .unwrap();

    // 用极短 backoff 让重连快；但本测试只验证首批 3 帧。
    let cfg = ReconnectConfig::with_backoff(Duration::from_millis(20));
    let mut stream = client.subscribe_status(1000, cfg);
    use tokio_stream::StreamExt;

    let mut mems = Vec::new();
    for _ in 0..3 {
        let s = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("frame in time")
            .expect("frame present");
        mems.push(s.memory);
    }
    assert_eq!(mems, vec![1000, 1001, 1002]);
}

#[tokio::test]
async fn subscribe_connections_delivers_reset_event() {
    let (addr, _state, _h) = spawn_server(SECRET, 0).await;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .unwrap();
    // interval != 0 → server 发 reset 帧。
    let cfg = ReconnectConfig::with_backoff(Duration::from_millis(20));
    let mut stream = client.subscribe_connections(1, cfg);
    use tokio_stream::StreamExt;

    let ev = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert!(ev.reset, "first frame should be reset");
    assert_eq!(ev.events.len(), 1);
    assert_eq!(ev.events[0].id, "conn-1");
    assert_eq!(ev.events[0].r#type, ConnectionEventType::New as i32);
}

#[tokio::test]
async fn stream_auto_reconnects_after_server_drop() {
    // server A 推 2 帧后流结束 → 客户端重连 → 因 A 已停，连新 server B → 继续收帧。
    let (addr_a, _state_a, _h_a) = spawn_server(SECRET, 2).await;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr_a.port()), SECRET)
        .await
        .unwrap();

    // 极短 backoff 加速重连。
    let cfg = ReconnectConfig::with_backoff(Duration::from_millis(30));
    let mut stream = client.subscribe_status(5000, cfg);
    use tokio_stream::StreamExt;

    // 收首批 2 帧。
    for _ in 0..2 {
        let _ = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
    }

    // 原流已结束。客户端会试图重连同一 target（A），但 A 的 h2 连接断后会重连成功
    // （A 仍存活，再推 2 帧——status_frames=2 每次连接固定推 2 帧后断）。
    // 验证重连后继续收到帧（memory 种子仍是 5000 → 5000/5001）。
    let frame = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("reconnect frame within timeout")
        .expect("frame present after reconnect");
    assert!(
        (5000..=5001).contains(&frame.memory),
        "reconnected frame memory should be 5000/5001, got {}",
        frame.memory
    );
}

#[tokio::test]
async fn endpoint_target_ipv6_bracketing() {
    // Endpoint 不暴露 target()，但经客户端连接间接验证：IPv6 host 不会 panic/hang（连接到 ::1 未监听 → transport err）。
    // 此测试仅校验 target 构造不 panic（连接失败为预期——::1:1 无 server）。
    let res = SingBoxApiClient::connect(Endpoint::new("::1", 1), "").await;
    // lazy channel：connect 不立即失败，但首 RPC 才报错。这里仅断言不 panic。
    // 若 connect 实现为 lazy（h2c 是 lazy），返回 Ok。
    let _ = res;
}

#[tokio::test]
async fn subscribe_tailscale_status_delivers_frames() {
    // server 每次连接推 2 帧全量端点快照。断言首帧真到达且字段解码正确（endpointTag/backendState/self IP）。
    let (addr, _h) = spawn_ts_server(SECRET, 2).await;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .unwrap();
    let cfg = ReconnectConfig::with_backoff(Duration::from_millis(20));
    let mut stream = client.subscribe_tailscale_status(cfg);
    use tokio_stream::StreamExt;

    let frame = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("frame in time")
        .expect("frame present");
    assert_eq!(frame.endpoints.len(), 1);
    assert_eq!(frame.endpoints[0].endpoint_tag, "ts-node");
    assert_eq!(frame.endpoints[0].backend_state, "Running");
    let self_ = frame.endpoints[0].self_.as_ref().expect("self present");
    assert_eq!(self_.tailscale_i_ps, vec!["100.64.0.1".to_string()]);
    assert_eq!(
        frame.endpoints[0].user_groups[0].peers[0].host_name,
        "exit-peer"
    );
}

#[tokio::test]
async fn tailscale_status_stream_auto_reconnects_after_server_drop() {
    // 每次连接推 1 帧后 drop tx（流结束）→ 客户端 backoff 重连 → server 仍存活 → 再收一帧。
    // 打断 `TailscaleStatusReconnect::open` / 复用的重连状态机任一 → 重连帧收不到 → 转红。
    let (addr, _h) = spawn_ts_server(SECRET, 1).await;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .unwrap();
    let cfg = ReconnectConfig::with_backoff(Duration::from_millis(30));
    let mut stream = client.subscribe_tailscale_status(cfg);
    use tokio_stream::StreamExt;

    // 首帧（首次连接）。
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap();
    // 原流结束 → 重连后继续收帧（每次连接固定推 1 帧）。
    let frame = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("reconnect frame within timeout")
        .expect("frame present after reconnect");
    assert_eq!(frame.endpoints[0].endpoint_tag, "ts-node");
}

#[tokio::test]
async fn no_auth_when_secret_empty() {
    // server secret 空 = 免认证；客户端 secret 空 → 调用应成功。
    let (addr, state, _h) = spawn_server("", 0).await;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), "")
        .await
        .unwrap();
    client.select_outbound("g", "m").await.unwrap();
    assert_eq!(state.select_calls.lock().unwrap().len(), 1);
}

/// `SubscribeLog` 的两段语义都必须到位：**首帧 `reset=true` 且带核侧历史**，其后是增量帧。
///
/// 这两件事分别对应两类真实故障：首帧的 `reset` 标志若丢了，重连时那 3000 行历史会被当增量整屏重放；
/// 首帧的 `messages` 若没解出来，起核到订阅之间那一段日志就永久看不到（TUN/helper 腿没有 stderr 管道，
/// 那段**只有**这一条路）。
///
/// 增量那两帧刻意跨级别（TRACE / ERROR）：本流不受核 `log.level` 约束，级别筛在客户端 ——
/// 这正是 `diagnosticCapture`（改核配置 + 重启核换 debug）被撤掉的依据。
#[tokio::test]
async fn subscribe_log_delivers_reset_history_then_increments() {
    let (addr, state, _h) = spawn_server(SECRET, 0).await;
    *state.log_history.lock().unwrap() = vec![
        daemon::log::Message {
            level: daemon::LogLevel::Info as i32,
            message: "router: loaded".into(),
        },
        daemon::log::Message {
            level: daemon::LogLevel::Debug as i32,
            message: "dns: cache hit".into(),
        },
    ];
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .unwrap();
    let mut stream =
        client.subscribe_logs(ReconnectConfig::with_backoff(Duration::from_millis(20)));
    use tokio_stream::StreamExt;

    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("首帧应及时到达")
        .expect("首帧存在");
    assert!(first.reset, "订阅首帧必须带 reset=true（历史帧标志）");
    assert_eq!(
        first
            .messages
            .iter()
            .map(|m| m.message.as_str())
            .collect::<Vec<_>>(),
        vec!["router: loaded", "dns: cache hit"],
        "首帧必须带回核侧历史"
    );
    assert_eq!(
        first.messages[1].level,
        daemon::LogLevel::Debug as i32,
        "级别是结构化枚举，不是待猜的行内字符串"
    );

    let mut seen: Vec<(i32, String)> = Vec::new();
    for _ in 0..2 {
        let f = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("增量帧应及时到达")
            .expect("增量帧存在");
        assert!(!f.reset, "增量帧不得带 reset");
        for m in f.messages {
            seen.push((m.level, m.message));
        }
    }
    assert_eq!(
        seen,
        vec![
            (daemon::LogLevel::Trace as i32, "trace: dns exchange".into()),
            (daemon::LogLevel::Error as i32, "error: dial failed".into()),
        ],
        "本流恒是全级别（含 trace），核的 log.level 管不着它"
    );
}

/// `ClearLogs` 必须真的发到核：只清本地环的话，下一次重订阅会把核那 3000 行历史整份带回来
/// （用户看到「清了又自己长回来」）。
///
/// **变异锁**：把 `clear_logs` 里的 `c.clear_logs(req).await?` 换成直接 `Ok(())` → 计数为 0 → 转红。
#[tokio::test]
async fn clear_logs_reaches_the_core() {
    let (addr, state, _h) = spawn_server(SECRET, 0).await;
    *state.log_history.lock().unwrap() = vec![daemon::log::Message {
        level: daemon::LogLevel::Info as i32,
        message: "stale".into(),
    }];
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", addr.port()), SECRET)
        .await
        .unwrap();

    client.clear_logs().await.expect("clear_logs ok");
    assert_eq!(state.clear_logs_count.load(Ordering::Relaxed), 1);
    assert!(
        state.log_history.lock().unwrap().is_empty(),
        "核侧历史应已清空（否则重订阅还会把它带回来）"
    );
}
