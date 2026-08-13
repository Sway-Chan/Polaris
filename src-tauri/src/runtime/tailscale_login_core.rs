//! Tailscale 瞬态登录核的**宿主层编排**（spawn / STATUS 订阅 / emit / 生命周期注册表）。
//!
//! 纯逻辑（config 生成、双写守卫、登录状态机）在 `polaris_mesh::tailscale_login`，已单测；
//! 本模块只做**运行时接线**：拉起一个独立的瞬态 sing-box、订阅**它自己的**管理 API
//! `SubscribeTailscaleStatus` 流、把帧里的 `authURL` 转成登录 URL 事件、把 `backendState == "Running"`
//! 当作登录成功并就地收核，并管理它的生死（kill-on-relogin / 超时自动杀 / 取消 / 自然退出 reap），
//! 与 `ProxyRuntime` 的常驻代理核**隔离**（独立注册表、独立 child 句柄；瞬态核绝不写进 proxy 的
//! pid 槽，故不会被误当作代理核）。
//!
//! ## 为什么 URL 只认 gRPC，不再扫 stdout（含「gRPC 腿失败要不要回退 stdout」的结论）
//!
//! 曾经的实现从核 stdout 正则抓 `Waiting for authentication: <url>`。改掉它有两条独立理由：
//!
//! 1. **那行是日志文案，不是契约**。上游 `protocol/tailscale/endpoint.go` 里它就是一句
//!    `logger.Info("Waiting for authentication: ", authURL)`；改文案、改前缀、改日志等级都不算破坏性
//!    变更，而 `TailscaleEndpointStatus.authURL` 是 proto 字段，字段号由 `crates/singbox-grpc` 的两道
//!    机械门看守（build.rs 对随包核 descriptor 对账 + `tests/bundled_core_wire.rs`）。
//! 2. **stdout 路径拿不到「登录成功」**。此前的登录成功判据是「无法判定」（`LoginState::NoStatusFallback`），
//!    于是核要么空跑到 5 分钟超时、要么靠用户手动取消 —— 期间它一直占着该节点的 `state_directory`。
//!    `backendState == "Running"` 是控制面给的**终局肯定**，拿到即收核。
//!
//! **gRPC 腿失败时不回退 stdout，硬失败**。取舍写在这里以免下一轮又被「多一条兜底更稳」翻回去：
//! - 「两份 URL 来源」正是本次要消灭的漂移。留一条 stdout 兜底 = 两个解析器、两种格式、两条各自
//!   可能先到的路径，而它们对「登录成功」的能力**不对等**：走上兜底那一刻，功能就悄悄退回改造前的
//!   形态（有 URL、判不了成功、核空跑到超时），且**没有任何人会看见这次降级**。本仓已有过一次同型
//!   教训：`reconnect.rs` 用没接 sink 的日志门面，静默让同一根因扛过两轮修复。
//! - 兜底能覆盖的失败面本来就很窄：api service bind 不上 → 核直接 FATAL 退出，stdout 同样什么都没有；
//!   配置形状不对 → 已被 spawn 前的 `sing-box check` 挡下。真正只属于 gRPC 腿的失败是「核活着但订阅
//!   建不起来」，而 `ReconnectingStream` 本身就带退避重连，这类抖动它自己会吞掉；真的一直连不上，
//!   由既有超时臂杀核并留下明确日志 —— 这是**响的**失败，不是静默降级。
//!
//! stdout/stderr 仍整段转日志（诊断价值不变），但**不再是任何判据的来源**。
//!
//! ## 诚实边界（务必读）
//! 本命令的**端到端价值 = 真 sing-box + 真出站 + 真 Tailscale 控制面**：起一个真核去连 Tailscale 控制服务器、
//! 把它吐的登录 URL 转发给用户。这条真机路径**在本 Linux 开发机上无法验证**（本仓禁跑触碰宿主网络的测试；
//! `sing-box check` 只验配置形状，验不了「核真的吐出登录 URL」这一运行时行为）。因此：
//! - 本模块的**全部可单测面**（注册表生命周期、命令决策流、去重、超时、取消、reap、STATUS→URL relay、
//!   Running→收核）都以注入的 mock [`LoginCoreSpawner`]/[`ConfigChecker`]/[`AuthUrlEmitter`]/
//!   [`LoginStatusSubscriber`] 单测——**无真进程、无网络、无真 sing-box、无真 gRPC**。
//! - **真 spawn + 控制面握手 + 真登录 URL** 一段**在此未验证**，门槛是一次真机会话（见
//!   `~/docs/polaris/design/polaris-tailscale-login-wiring.md` 的验收清单）。不得据本模块宣称「登录端到端可用」。
//!
//! ## 与 `cleanup_stale_cores` 的关系
//! `ProxyRuntime::cleanup_stale_cores` 在**下次起代理核时**清扫「本 app 二进制」的孤儿核，仅排除当前受管的
//! 代理 pid。一个**上次会话遗留**的瞬态登录核届时被扫掉是**可接受**的（它本就该在应用退出时清）。当前批次不把
//! 在飞的瞬态登录 pid 交叉排除进 `cleanup_stale_cores`（那需要 mesh↔proxy 反向耦合，超出本 4 步范围）——
//! 极端情况下「登录在飞时用户又去起代理核」会误杀在飞登录核；这是已知的有界限制，登录核身份始终独立记在本
//! 注册表里，绝不会被**误认成**代理核。

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tauri::AppHandle;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::oneshot;

use polaris_config_engine::user_config::app_config::UserConfig;
use polaris_config_engine::user_config::server_config::ServerConfig;
use polaris_core_supervisor::port_bookkeeping::TokioPortProvider;
use polaris_core_supervisor::{
    PortAllocator, PortExclusions, ProcessKiller, SingBoxSpawner, SpawnError, SpawnRequest,
    TokioSpawner,
};
use polaris_mesh::tailscale_login::{
    advance_login_state, build_tailscale_login_config, login_config_to_json,
    tailscale_endpoint_in_running_core, LoginEvent, LoginState, TailscaleLoginApiService,
};
use polaris_singbox_grpc::{daemon, Endpoint, ReconnectConfig, SingBoxApiClient};

use crate::events::{broadcast, channel::EVENT_TAILSCALE_AUTH_URL};
use crate::runtime::proxy::{pid_alive, resolve_core_binary, send_signal};
use crate::runtime::tailscale_status::decode_tailscale_status;

/// 瞬态登录核的最大挂起时长：登录不完成（用户不去浏览器认证）时到点自动杀核，避免核无限挂着。
/// 交互登录需人去浏览器完成，故给宽松窗口（5 分钟）。
const DEFAULT_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// 杀瞬态核的优雅窗口（SIGTERM → 宽限 → SIGKILL）。对齐 `ProxyRuntime` 的 `STOP_GRACE`（5s）。
const LOGIN_STOP_GRACE: Duration = Duration::from_secs(5);

/// 装箱的异步读端（子进程 stdout/stderr 的抽象——生产是真管道，测试是内存 duplex）。
type BoxRead = Box<dyn AsyncRead + Unpin + Send>;

/// 核二进制解析器（注入点）：生产走 [`resolve_core_binary`]，测试注入固定路径以免依赖真实落盘的 sing-box。
type BinaryResolver = Arc<dyn Fn() -> Result<PathBuf, String> + Send + Sync>;

// ── 抽象 trait（生产真实现 / 测试 mock；这是「无真进程无网络单测」的关键）────────────────────────

/// 瞬态登录核子进程抽象。生产用 `tokio::process::Child` 包装；测试用内存 duplex 假子进程，
/// 使整条编排（spawn/pipe/emit/register/timeout/cancel/reap）可在无真进程无网络下驱动。
#[async_trait]
pub trait LoginCoreChild: Send {
    /// 子进程 pid（仅日志用；假子进程返回占位值）。
    fn pid(&self) -> Option<u32>;
    /// 取走 stdout 读端（仅一次；供逐行扫登录 URL）。
    fn take_stdout(&mut self) -> Option<BoxRead>;
    /// 取走 stderr 读端（仅一次；转日志）。
    fn take_stderr(&mut self) -> Option<BoxRead>;
    /// 等子进程自然退出并收割（cancel-safe：可在 `select!` 中反复创建/丢弃）。
    async fn wait(&mut self);
    /// 主动终止并收割：生产 SIGTERM→宽限→SIGKILL 后 `wait()`；测试置终止标记即返回。
    async fn terminate(&mut self);
}

/// spawn 抽象：返回 [`LoginCoreChild`] 装箱句柄。生产 [`TokioLoginCoreSpawner`] 内部经 [`TokioSpawner`] 起真核。
pub trait LoginCoreSpawner: Send + Sync {
    /// spawn 一个瞬态登录核。失败返 [`SpawnError`]（ENOENT/EACCES）。
    fn spawn(&self, req: &SpawnRequest) -> Result<Box<dyn LoginCoreChild>, SpawnError>;
}

/// `sing-box check` 抽象：spawn 前先验配置形状（fail-fast）。生产真跑 `sing-box check -c <file>`，测试 mock。
#[async_trait]
pub trait ConfigChecker: Send + Sync {
    /// 校验 `config_path` 是否为合法 sing-box 配置。非法 → Err（含核的诊断）。
    async fn check(&self, binary: &Path, config_path: &Path) -> Result<(), String>;
}

/// 登录 URL 事件发射抽象。生产经 [`AppHandle`] 广播 `event:tailscaleAuthUrl`，测试捕获断言。
pub trait AuthUrlEmitter: Send + Sync {
    /// 发射一条登录 URL 事件（URL 首次出现或发生变更时发）。
    fn emit_auth_url(&self, server_id: &str, node_name: &str, url: &str);
}

/// 瞬态核 STATUS 流（每帧 = 全量端点快照）。生产是 `SubscribeTailscaleStatus` 的自动重连流，
/// 测试是喂脚本帧的内存桩 —— 这条抽象是「无真 gRPC 单测整条登录编排」的关键。
#[async_trait]
pub trait LoginStatusStream: Send {
    /// 取下一帧。`None` = 流终止（生产上 [`polaris_singbox_grpc::ReconnectingStream`] 断开即重连，
    /// 正常不返 `None`；返了就是内部终止，由调用方按「没有更多帧」处理）。
    async fn recv(&mut self) -> Option<daemon::TailscaleStatusUpdate>;
}

/// STATUS 流订阅抽象：按瞬态核自己的 api 端口 + secret 建流。
#[async_trait]
pub trait LoginStatusSubscriber: Send + Sync {
    /// 订阅 `127.0.0.1:<port>` 的 `SubscribeTailscaleStatus`。`secret` 空串 → 免认证。
    async fn subscribe(
        &self,
        port: u16,
        secret: &str,
    ) -> Result<Box<dyn LoginStatusStream>, String>;
}

// ── 生产实现 ────────────────────────────────────────────────────────────────────────────────

/// `tokio::process::Child` 包装的生产子进程句柄。
///
/// ## 为什么有 [`Drop`] 守卫（不是洁癖）
///
/// `tokio::process::Child` 默认 `kill_on_drop == false`：句柄被丢弃时 tokio 只把它推进 orphan 队列
/// **等待收割**，子进程照常活着。而瞬态核（登录核 / 测速临时核）的 kill 全靠调用方显式
/// [`terminate`](LoginCoreChild::terminate) —— 只要 future 在 `spawn` 与 `terminate` 之间被丢弃或
/// panic 展开，就留下一个持续持有回环端口（测速临时核是 N 个）+ WG/WARP peer 会话的**孤儿
/// sing-box**，且用户完全看不见。兜底 sweep 只在下次起主核时跑，Windows 更是恒 no-op
/// （`core-supervisor/src/stale_core.rs` 的 `scan_running_cores` 在非 Linux/macOS 返空）。
///
/// 用 Drop 守卫而非 `Command::kill_on_drop(true)`：后者必须设在 **spawn 之前**的 `Command` 上，
/// 而 spawn 收口在 `core-supervisor` 的 `TokioSpawner`（主核与瞬态核共用，主核**不能**跟着 app
/// 的任意 future 生死）。守卫挂在瞬态核专属的这层包装上，射程正好。
///
/// `start_kill` 只发信号不阻塞（Drop 不能 await）；已退出/已收割的 child 返 Err，无害吞掉。
pub struct TokioLoginCoreChild {
    child: tokio::process::Child,
}

impl Drop for TokioLoginCoreChild {
    fn drop(&mut self) {
        // 正常路径（`terminate()` 已收割）到这里是 no-op；异常路径（future 被丢弃 / panic）靠这一发。
        let _ = self.child.start_kill();
    }
}

#[async_trait]
impl LoginCoreChild for TokioLoginCoreChild {
    fn pid(&self) -> Option<u32> {
        self.child.id()
    }
    fn take_stdout(&mut self) -> Option<BoxRead> {
        self.child.stdout.take().map(|s| Box::new(s) as BoxRead)
    }
    fn take_stderr(&mut self) -> Option<BoxRead> {
        self.child.stderr.take().map(|s| Box::new(s) as BoxRead)
    }
    async fn wait(&mut self) {
        let _ = self.child.wait().await;
    }
    async fn terminate(&mut self) {
        // 1:1 镜像 ProxyRuntime::kill_core 的收割纪律：SIGTERM→宽限→SIGKILL，退出后取消挂起升级
        // （防 timer 泄漏 + pid 复用误杀），并 `wait()` 收割防僵尸。
        let pid = self.child.id().unwrap_or(0);
        if pid == 0 {
            // 已退出且被收割 → 仅 reap 残句柄。
            let _ = self.child.wait().await;
            return;
        }
        let escalation = ProcessKiller::escalate_async(
            move |sig| send_signal(pid, sig),
            move || pid_alive(pid),
            LOGIN_STOP_GRACE,
        )
        .await;
        let _ = self.child.wait().await;
        escalation.wait().await;
    }
}

/// 生产 spawner：经 [`TokioSpawner`] 起真 sing-box，再适配为 [`LoginCoreChild`]。
pub struct TokioLoginCoreSpawner;

impl LoginCoreSpawner for TokioLoginCoreSpawner {
    fn spawn(&self, req: &SpawnRequest) -> Result<Box<dyn LoginCoreChild>, SpawnError> {
        let spawned = TokioSpawner::new().spawn(req)?;
        Ok(Box::new(TokioLoginCoreChild {
            child: spawned.child,
        }))
    }
}

/// 生产 checker：真跑 `sing-box check -c <file>` 并按退出码判定。
pub struct SingBoxConfigChecker;

#[async_trait]
impl ConfigChecker for SingBoxConfigChecker {
    async fn check(&self, binary: &Path, config_path: &Path) -> Result<(), String> {
        let mut builder = tokio::process::Command::new(binary);
        builder
            .arg("check")
            .arg("-c")
            .arg(config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = crate::runtime::win_console::no_console_window_async(&mut builder)
            .output()
            .await
            .map_err(|e| format!("sing-box check 启动失败: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        Err(format!("sing-box check 判定登录配置无效: {detail}"))
    }
}

/// 生产 STATUS 订阅器：连瞬态核自己的管理 API（h2c，127.0.0.1）建 `SubscribeTailscaleStatus` 自动重连流。
///
/// 与主核那条 relay（`proxy.rs::spawn_tailscale_status_relay`）**互不相干**：各自端口、各自 secret、
/// 各自 stream 句柄，帧也不进主核的 `MeshRuntime::ts_status` 缓存（那份缓存的 `connected` 语义是
/// 「主核在跑」，写进瞬态核的帧会让它说谎）。
pub struct GrpcLoginStatusSubscriber;

#[async_trait]
impl LoginStatusSubscriber for GrpcLoginStatusSubscriber {
    async fn subscribe(
        &self,
        port: u16,
        secret: &str,
    ) -> Result<Box<dyn LoginStatusStream>, String> {
        // `connect` 建的是 **lazy** channel（`h2c::connect_h2c`）——此处不发生 I/O，故核尚未 bind 完
        // 也不会失败；真正的建流与重试在 `ReconnectingStream` 里。
        let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", port), secret)
            .await
            .map_err(|e| format!("连瞬态登录核管理 API 失败（port={port}）: {e}"))?;
        Ok(Box::new(GrpcLoginStatusStream {
            stream: client.subscribe_tailscale_status(ReconnectConfig::default()),
        }))
    }
}

/// [`GrpcLoginStatusSubscriber`] 建出的流。`ReconnectingStream` 自持 target/secret，与建它的
/// client 无生命周期纠缠，故此处只留流本体。
struct GrpcLoginStatusStream {
    stream: polaris_singbox_grpc::ReconnectingStream<daemon::TailscaleStatusUpdate>,
}

#[async_trait]
impl LoginStatusStream for GrpcLoginStatusStream {
    async fn recv(&mut self) -> Option<daemon::TailscaleStatusUpdate> {
        self.stream.recv().await
    }
}

/// 生产 emitter：经 [`AppHandle`] 广播 `event:tailscaleAuthUrl`。
///
/// payload 与前端 `onTailscaleAuth` 契约同形：`{ serverId, nodeName, url, transient }`
/// （`ui/src/ipc/api-client.ts:155`）。
pub struct AppHandleEmitter {
    /// 广播用的 Tauri 应用句柄。
    pub app: AppHandle,
}

impl AuthUrlEmitter for AppHandleEmitter {
    fn emit_auth_url(&self, server_id: &str, node_name: &str, url: &str) {
        broadcast(
            &self.app,
            EVENT_TAILSCALE_AUTH_URL,
            json!({
                "serverId": server_id,
                "nodeName": node_name,
                "url": url,
                "transient": true,
            }),
        );
    }
}

// ── 注册表 + 编排 ────────────────────────────────────────────────────────────────────────────

/// 注册表条目：一个在飞瞬态登录核的控制句柄。child 本体由 supervisor 任务独占持有，本条目只留信号通道。
struct LoginEntry {
    /// 单调 epoch：区分同一 serverId 的不同代次登录（kill-on-relogin 后旧 supervisor 不得误删新表项）。
    epoch: u64,
    /// 通知 supervisor kill+reap（cancel / kill-on-relogin 用）。
    cancel_tx: oneshot::Sender<()>,
    /// 子进程 pid（日志/诊断）。
    pid: u32,
}

/// 注册表共享状态（supervisor 任务与命令层共享）。
#[derive(Default)]
struct Shared {
    /// serverId → 在飞登录核条目。
    entries: Mutex<HashMap<String, LoginEntry>>,
}

impl Shared {
    fn guard(&self) -> MutexGuard<'_, HashMap<String, LoginEntry>> {
        // 锁只在 insert/remove 的极短临界区持有（绝不跨 await），中毒极不可能；中毒仍恢复内层，不 panic。
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take(&self, id: &str) -> Option<LoginEntry> {
        self.guard().remove(id)
    }

    fn insert(&self, id: String, entry: LoginEntry) {
        self.guard().insert(id, entry);
    }

    /// epoch 守卫下注销：仅当表项仍是本 supervisor 的代次时移除（防 kill-on-relogin 后误删新代次）。
    fn remove_if_epoch(&self, id: &str, epoch: u64) {
        let mut g = self.guard();
        if g.get(id).is_some_and(|e| e.epoch == epoch) {
            g.remove(id);
        }
    }

    #[cfg(test)]
    fn contains(&self, id: &str) -> bool {
        self.guard().contains_key(id)
    }
}

/// [`start_login`](LoginCoreRegistry::start_login) 的结果。命令层据此折成前端 `{ started, reason?, authUrl? }`。
pub enum StartLoginOutcome {
    /// 已起瞬态登录核（登录 URL 稍后经事件到达，非「已登录」）。
    Started,
    /// 双写守卫命中：该 TS endpoint 已在运行主核里，无需瞬态核（前端 `reason: 'inMainCore'`）。
    InMainCore,
    /// 起核前失败（resolve / 写配置 / check / spawn）。返 error，未留表项、未起核。
    Failed(String),
}

/// 瞬态登录核生命周期注册表。持有注入的 spawner/checker/binary-resolver（生产真实现，测试 mock）。
///
/// 支撑：kill-on-relogin、超时自动杀、取消、自然退出 reap。与 `ProxyRuntime` 的常驻代理核隔离。
pub struct LoginCoreRegistry {
    shared: Arc<Shared>,
    spawner: Arc<dyn LoginCoreSpawner>,
    checker: Arc<dyn ConfigChecker>,
    subscriber: Arc<dyn LoginStatusSubscriber>,
    resolve_binary: BinaryResolver,
    timeout: Duration,
    epoch: AtomicU64,
}

impl LoginCoreRegistry {
    /// 生产装配：真 spawner + 真 `sing-box check` + 真 gRPC STATUS 订阅 + 真核解析 + 默认超时。
    #[must_use]
    pub fn production() -> Self {
        Self::with_deps(
            Arc::new(TokioLoginCoreSpawner),
            Arc::new(SingBoxConfigChecker),
            Arc::new(GrpcLoginStatusSubscriber),
            Arc::new(resolve_core_binary),
            DEFAULT_LOGIN_TIMEOUT,
        )
    }

    /// 注入装配（测试用 mock，或自定义超时）。
    #[must_use]
    pub fn with_deps(
        spawner: Arc<dyn LoginCoreSpawner>,
        checker: Arc<dyn ConfigChecker>,
        subscriber: Arc<dyn LoginStatusSubscriber>,
        resolve_binary: BinaryResolver,
        timeout: Duration,
    ) -> Self {
        Self {
            shared: Arc::new(Shared::default()),
            spawner,
            checker,
            subscriber,
            resolve_binary,
            timeout,
            epoch: AtomicU64::new(1),
        }
    }

    /// 取消某 server 在飞的瞬态登录核（kill + 注销）。幂等：无在飞核返 `false`（非错误）。
    pub fn cancel_login(&self, server_id: &str) -> bool {
        match self.shared.take(server_id) {
            Some(entry) => {
                // 通知 supervisor kill+reap；发送失败（supervisor 已退）无妨。
                let _ = entry.cancel_tx.send(());
                true
            }
            None => false,
        }
    }

    /// 起一个瞬态登录核。
    ///
    /// 流程：双写守卫 → 解析核 → 解析管理 API 端口 + 生成 secret → 建 config → 写盘 →
    /// `sing-box check` → kill-on-relogin（先杀该 server 旧核）→ spawn → 订阅 STATUS 流 →
    /// 注册 + 计时臂 + 后台 supervise。
    ///
    /// `primary_api_port` = **运行中主核**的管理 API 端口（未运行传 0）：瞬态核的 api 端口必须避开它，
    /// 否则两个 api service 抢同一个 bind → 瞬态核直接 FATAL。其余排除项（control/http/mixed）取自
    /// `running_config` —— 与双写守卫同一份快照，语义正好：**只排除此刻真的被占着的端口**。
    ///
    /// `started ≠ 已登录`：登录 URL 经 [`AuthUrlEmitter`] 事件异步到达（源 = STATUS 帧的 `authURL`）。
    pub async fn start_login(
        &self,
        server: &ServerConfig,
        user_data: &Path,
        is_running: bool,
        running_config: Option<&UserConfig>,
        primary_api_port: u16,
        emitter: Arc<dyn AuthUrlEmitter>,
    ) -> StartLoginOutcome {
        // (a) 双写守卫：endpoint 已在运行主核 → 拒起瞬态核（两个核同写 tailscale-state 会冲突）。
        if tailscale_endpoint_in_running_core(&server.id, is_running, running_config) {
            return StartLoginOutcome::InMainCore;
        }

        // (b) 解析核二进制（复用 proxy 的解析，禁重复实现）。
        let binary = match (self.resolve_binary)() {
            Ok(b) => b,
            Err(e) => return StartLoginOutcome::Failed(e),
        };

        // (c) 瞬态核管理 API：独立空闲端口 + 每次随机 secret。端口走既有簿记设施
        // （`resolve_tailscale_login_api_port`：bind(0) 取口、撞排除集重滚 5 次、仍撞则回落
        // control_api+2），secret 走与 clashApiSecret 同源的 CSPRNG。**secret 不是洁癖**：
        // 管理 API 虽只监听回环，但同机任意进程都能连上它读 tailnet 拓扑。
        let exclusions = PortExclusions::for_login_api(
            primary_api_port,
            // UserConfig 无 controlPort 字段（`impl PortConfig for UserConfig` 恒 None）→ 走默认 9090。
            None,
            running_config.and_then(|c| c.http_port),
            None,
            running_config.and_then(|c| c.mixed_port),
        );
        let resolved =
            PortAllocator::new(TokioPortProvider).resolve_tailscale_login_api_port(&exclusions);
        if resolved.used_fallback {
            log::warn!(
                "瞬态登录核管理 API 端口 5 次解析均撞排除集 → 回落 {}",
                resolved.port
            );
        }
        let secret = match generate_login_api_secret() {
            Ok(s) => s,
            Err(e) => return StartLoginOutcome::Failed(e),
        };
        let api = TailscaleLoginApiService {
            port: resolved.port,
            secret,
        };

        // (d) 构造登录 config（恒带管理 api service → 恒有 STATUS 流）→ 写盘。
        //
        // 文件名带**代次**（epoch），不是只带 server id：收核后 supervisor 会删掉自己那份 config
        // （里面有 secret），而 kill-on-relogin 下新旧两代同时在场——路径若只按 server id 取，
        // 旧代 supervisor 的删除就会打在**新代**刚写好的那份上（它的 `terminate()` 有最长 5s 的
        // SIGTERM 宽限，删除随时可能晚于新核 spawn）。带代次后每份 config 只有一个主人。
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst);
        let cfg = build_tailscale_login_config(server, user_data, &api);
        let json_cfg = login_config_to_json(&cfg);
        let config_path = user_data.join(format!(
            "tailscale-login-{}-{epoch}.json",
            sanitize_id(&server.id)
        ));
        let bytes = match serde_json::to_vec_pretty(&json_cfg) {
            Ok(b) => b,
            Err(e) => return StartLoginOutcome::Failed(format!("序列化登录配置失败: {e}")),
        };
        if let Err(e) = std::fs::write(&config_path, bytes) {
            return StartLoginOutcome::Failed(format!(
                "写登录配置失败 {}: {e}",
                config_path.display()
            ));
        }

        // (e) sing-box check 先验配置形状（失败快退、不 spawn —— 这一段可单测）。
        if let Err(e) = self.checker.check(&binary, &config_path).await {
            return StartLoginOutcome::Failed(e);
        }

        // (f) kill-on-relogin：先杀该 server 在飞的旧瞬态核（若有），再起新核。
        self.cancel_login(&server.id);

        // (g) spawn 瞬态登录核（`run -c <cfg> --disable-color`，避免 ANSI 污染日志）。
        let mut req = SpawnRequest::new(&binary, &config_path);
        req.extra_args = vec!["--disable-color".to_string()];
        let mut child = match self.spawner.spawn(&req) {
            Ok(c) => c,
            Err(e) => return StartLoginOutcome::Failed(format!("{e}")),
        };
        let pid = child.pid().unwrap_or(0);

        // (h) 订阅瞬态核自己的 STATUS 流。**建不起来就硬失败**（不回退 stdout，理由见模块头）：
        // 此时核已起，必须先收掉它再报错，否则留下一个谁都不认识的孤儿核。
        let status = match self.subscriber.subscribe(api.port, &api.secret).await {
            Ok(s) => s,
            Err(e) => {
                child.terminate().await;
                remove_login_config(&config_path);
                return StartLoginOutcome::Failed(format!(
                    "瞬态登录核 STATUS 流订阅失败（登录 URL 与登录成功判据均取自它，无回退路径）: {e}"
                ));
            }
        };

        // (i) 注册 + 后台 supervise（内含超时臂 / STATUS→URL relay / Running→收核 / cancel / reap）。
        // 复用 (d) 已取的 `epoch`：注册表代次与 config 文件名必须是**同一个**代次，否则「谁该删这份
        // config」与「谁该注销这个表项」会各按各的编号走。
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.shared.insert(
            server.id.clone(),
            LoginEntry {
                epoch,
                cancel_tx,
                pid,
            },
        );
        let ctx = SuperviseCtx {
            shared: self.shared.clone(),
            server_id: server.id.clone(),
            node_name: server.name.clone(),
            // 瞬态核只含本节点一个 endpoint，其 tag = server.name（见 `build_tailscale_login_config`）。
            // 复用主核那套解码器就得给它同一份 tag→id 映射；一并承担了「别的 tag 的帧一律丢弃」。
            tag_to_id: BTreeMap::from([(server.name.clone(), server.id.clone())]),
            config_path,
            epoch,
            timeout: self.timeout,
            emitter,
        };
        tokio::spawn(supervise(ctx, child, status, cancel_rx));
        StartLoginOutcome::Started
    }
}

/// 瞬态核管理 API 的一次性 secret（CSPRNG 16 字节 → 32 位小写 hex）。
/// 与 `clashApiSecret` 同源生成器（[`crate::commands::config::generate_local_api_secret`]）：同一熵源、
/// 同一形状，熵源不可用 → Err（绝不产弱/空密钥而把管理面裸奔当成「降级可用」）。
fn generate_login_api_secret() -> Result<String, String> {
    crate::commands::config::generate_local_api_secret()
        .map_err(|e| format!("生成瞬态登录核管理 API secret 失败: {e}"))
}

/// supervisor 任务的入参束（避免 `too_many_arguments`）。
struct SuperviseCtx {
    shared: Arc<Shared>,
    server_id: String,
    node_name: String,
    /// 单条映射 `server.name → server.id`：喂给 [`decode_tailscale_status`]，顺带把「别的 tag」的
    /// 端点整段丢掉（瞬态核理论上只有一个 endpoint，但判据不该建立在「理论上」之上）。
    tag_to_id: BTreeMap<String, String>,
    /// 本次登录写盘的临时 config 路径，收核后删。
    ///
    /// 此前不删也只是留个垃圾文件；**自本批起它里面有 secret**（管理 API 的一次性 Bearer），
    /// 核一退它就是一份没人再用、却仍躺在盘上的凭据 —— 生命周期该跟核一致。
    config_path: PathBuf,
    epoch: u64,
    timeout: Duration,
    emitter: Arc<dyn AuthUrlEmitter>,
}

/// 瞬态登录核退出原因。
enum ExitReason {
    /// 核自然退出（无需主动 kill，直接 reap）。
    SelfExit,
    /// 用户取消 / kill-on-relogin。
    Cancelled,
    /// 超时未完成登录。
    TimedOut,
    /// STATUS 报 `backendState == "Running"`：登录成功、state 已落盘 → 主动收核。
    LoggedIn,
    /// STATUS 流内部终止（`ReconnectingStream` 正常永不如此）。没有流就没有任何判据来源
    /// （不回退 stdout，见模块头），继续挂着只是让核空跑到超时 → 就地收核。
    StatusStreamEnded,
}

/// 单个瞬态登录核的后台 supervisor：消费 STATUS 流（authURL → 事件、Running → 收核）、
/// 扛超时/取消、退出后按 epoch 守卫注销。
async fn supervise(
    ctx: SuperviseCtx,
    mut child: Box<dyn LoginCoreChild>,
    mut status: Box<dyn LoginStatusStream>,
    mut cancel_rx: oneshot::Receiver<()>,
) {
    // stdout/stderr → 日志（best-effort，fire-and-forget）。**纯诊断**：自本批起它们不再是任何
    // 判据的来源（登录 URL 与登录成功都只认 STATUS 流），故无需进 select、无需回压。
    if let Some(stdout) = child.take_stdout() {
        tokio::spawn(drain_to_log(stdout));
    }
    if let Some(stderr) = child.take_stderr() {
        tokio::spawn(drain_to_log(stderr));
    }
    // 登录状态机（`polaris_mesh`，纯逻辑已单测）：它同时承担「同一 URL 反复到达不重复通知用户」
    // 与「后到的 authURL 不得把已登录态打回去」两条不变式，此处不再另写去重标志。
    let mut state = LoginState::Idle;
    let sleep = tokio::time::sleep(ctx.timeout);
    tokio::pin!(sleep);

    let reason = loop {
        tokio::select! {
            _ = &mut cancel_rx => break ExitReason::Cancelled,
            () = &mut sleep => break ExitReason::TimedOut,
            () = child.wait() => break ExitReason::SelfExit,
            frame = status.recv() => {
                let Some(update) = frame else { break ExitReason::StatusStreamEnded };
                state = apply_status_frame(&ctx, &state, &update);
                if state == LoginState::LoggedIn {
                    break ExitReason::LoggedIn;
                }
            }
        }
    };

    match reason {
        ExitReason::SelfExit => {
            log::info!(
                "瞬态登录核自然退出并收割：server={} pid={:?}",
                ctx.server_id,
                child.pid()
            );
        }
        ExitReason::Cancelled => {
            log::info!("瞬态登录核取消 → 终止：server={}", ctx.server_id);
            child.terminate().await;
        }
        ExitReason::TimedOut => {
            log::warn!(
                "瞬态登录核 {:?} 内未完成登录 → 超时终止：server={}",
                ctx.timeout,
                ctx.server_id
            );
            child.terminate().await;
        }
        ExitReason::LoggedIn => {
            // 控制面的终局肯定：已认证、state 已落盘 → 核没有再活着的理由，且它还占着该节点的
            // state_directory（主核要用同一份）。1:1 对齐 上游 `handleTransientTailscaleStatus`。
            log::info!(
                "Tailscale 登录成功（backendState=Running）→ 收瞬态登录核：server={}",
                ctx.server_id
            );
            child.terminate().await;
        }
        ExitReason::StatusStreamEnded => {
            log::warn!(
                "瞬态登录核 STATUS 流终止（无 URL/登录成功判据来源）→ 终止：server={}",
                ctx.server_id
            );
            child.terminate().await;
        }
    }
    // 核已收割 → 删掉带 secret 的临时 config。
    remove_login_config(&ctx.config_path);
    // reap 后注销（epoch 守卫：不误删 kill-on-relogin 后的新代次表项）。
    ctx.shared.remove_if_epoch(&ctx.server_id, ctx.epoch);
}

/// 删掉本次登录写盘的临时 config（内含一次性管理 API secret）。best-effort：
/// 已被 kill-on-relogin 的新一代覆写、或早被删掉，都不是问题——本函数只保证「核死了就不留凭据」。
fn remove_login_config(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!(
                "删除瞬态登录核临时配置失败（内含一次性 secret）{}：{e}",
                path.display()
            );
        }
    }
}

/// 一帧全量端点快照 → 推进登录状态机（并在 URL 首现/变更时发事件）。
///
/// 解码复用主核那条 relay 的同一个投影器 [`decode_tailscale_status`]：`authURL` / `backendState`
/// 的读法只此一处，proto 再漂移时两条腿一起动，不会出现「主核修好了、登录核还错着」。
fn apply_status_frame(
    ctx: &SuperviseCtx,
    current: &LoginState,
    update: &daemon::TailscaleStatusUpdate,
) -> LoginState {
    let mut state = current.clone();
    for ev in decode_tailscale_status(update, &ctx.tag_to_id) {
        // 登录成功判据 = **backendState 字面为 Running**，不是 `logged_in`（后者含 `Starting`，那还
        // 只是「在连」；上游 `handleTransientTailscaleStatus` 同样只认 Running）。
        if ev.backend_state == "Running" {
            state = advance_login_state(&state, &LoginEvent::StatusRunning);
        }
        if let Some(url) = ev.auth_url {
            let next = advance_login_state(&state, &LoginEvent::AuthUrlSeen(url));
            // 状态真的变了才发：同一 URL 每帧都来（核只在换 URL 时才换值），发一次就够。
            if next != state {
                if let LoginState::AwaitingAuth(u) = &next {
                    ctx.emitter.emit_auth_url(&ctx.server_id, &ctx.node_name, u);
                }
            }
            state = next;
        }
    }
    state
}

/// 把子进程的一条输出流（stdout 或 stderr）逐行转日志（级别按行内容判，对齐 `proxy::pipe_to_log`）。
/// **纯诊断出口**：核那行 `Waiting for authentication: <url>` 也从这里落盘，但它只是日志——
/// 登录 URL 的判据在 STATUS 流（见模块头）。
async fn drain_to_log(stream: BoxRead) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.contains("FATAL") || line.contains("ERROR") {
            log::error!(target: "tailscale-login", "{line}");
        } else if line.contains("WARN") {
            log::warn!(target: "tailscale-login", "{line}");
        } else {
            log::info!(target: "tailscale-login", "{line}");
        }
    }
}

/// server id → 安全文件名片段（防路径穿越；非字母数字/-/_ 归一为 `_`）。
fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! 全部经 mock spawner/checker/emitter/STATUS 订阅器驱动——**无真进程、无网络、无真 sing-box、
    //! 无真 gRPC**（唯一的真系统调用是端口簿记的 `bind(127.0.0.1:0)`，回环、立即释放）。
    //!
    //! 覆盖：relogin 杀旧核 / cancel 杀+注销 / 超时杀 / 自然退出 reap / STATUS→URL 事件（含去重与换 URL）/
    //! **stdout 不再是 URL 来源** / Running→收核 / 幽灵 tag 丢弃 / 流终止→收核 / 订阅失败不留孤儿核 /
    //! api 端口与 secret 的解析 / check-fail 不 spawn / spawn-fail 不留表项 / 双写守卫拦截。
    //! 真 spawn+控制面路径**不在此覆盖**（真机门槛，见模块头）。

    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::io::AsyncWriteExt;
    use tokio::sync::{mpsc, watch};

    use polaris_config_engine::user_config::server_config::Protocol;

    // ── 假子进程 ──
    struct FakeChildState {
        terminated: AtomicBool,
    }

    struct FakeLoginCoreChild {
        stdout: Option<BoxRead>,
        state: Arc<FakeChildState>,
        exited_rx: watch::Receiver<bool>,
        exited_tx: watch::Sender<bool>,
    }

    #[async_trait]
    impl LoginCoreChild for FakeLoginCoreChild {
        fn pid(&self) -> Option<u32> {
            Some(4242)
        }
        fn take_stdout(&mut self) -> Option<BoxRead> {
            self.stdout.take()
        }
        fn take_stderr(&mut self) -> Option<BoxRead> {
            None
        }
        async fn wait(&mut self) {
            // 直到「退出」信号（自然退出或被终止）才返回；否则永挂（模拟核仍在等认证）。
            let mut rx = self.exited_rx.clone();
            let _ = rx.wait_for(|v| *v).await;
        }
        async fn terminate(&mut self) {
            self.state.terminated.store(true, Ordering::SeqCst);
            let _ = self.exited_tx.send(true);
        }
    }

    // ── 假 spawner（记录每次 spawn 的 child 状态供断言；可脚本化 stdout / 自然退出 / spawn 失败）──
    struct FakeSpawner {
        lines: Vec<String>,
        self_exit: bool,
        fail: bool,
        spawned: Arc<Mutex<Vec<Arc<FakeChildState>>>>,
        count: Arc<AtomicUsize>,
    }

    impl LoginCoreSpawner for FakeSpawner {
        fn spawn(&self, _req: &SpawnRequest) -> Result<Box<dyn LoginCoreChild>, SpawnError> {
            self.count.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(SpawnError::Spawn {
                    bin: PathBuf::from("/fake/sing-box"),
                    source: std::io::Error::new(std::io::ErrorKind::NotFound, "fake spawn fail"),
                });
            }
            let (mut writer, reader) = tokio::io::duplex(4096);
            let (exited_tx, exited_rx) = watch::channel(false);
            let state = Arc::new(FakeChildState {
                terminated: AtomicBool::new(false),
            });
            self.spawned.lock().unwrap().push(state.clone());
            let lines = self.lines.clone();
            let self_exit = self.self_exit;
            let exit_signal = exited_tx.clone();
            // 内存写端：把脚本行写进 duplex，然后按需触发自然退出，最后 drop（EOF）。无真进程。
            tokio::spawn(async move {
                for l in &lines {
                    let _ = writer.write_all(l.as_bytes()).await;
                    let _ = writer.write_all(b"\n").await;
                }
                if self_exit {
                    let _ = exit_signal.send(true);
                }
                drop(writer);
            });
            Ok(Box::new(FakeLoginCoreChild {
                stdout: Some(Box::new(reader)),
                state,
                exited_rx,
                exited_tx,
            }))
        }
    }

    // ── 假 checker / emitter ──
    struct FakeChecker {
        ok: bool,
    }
    #[async_trait]
    impl ConfigChecker for FakeChecker {
        async fn check(&self, _binary: &Path, _config_path: &Path) -> Result<(), String> {
            if self.ok {
                Ok(())
            } else {
                Err("fake check 判定配置无效".to_string())
            }
        }
    }

    #[derive(Default)]
    struct FakeEmitter {
        captured: Mutex<Vec<(String, String, String)>>,
    }
    impl AuthUrlEmitter for FakeEmitter {
        fn emit_auth_url(&self, server_id: &str, node_name: &str, url: &str) {
            self.captured.lock().unwrap().push((
                server_id.to_string(),
                node_name.to_string(),
                url.to_string(),
            ));
        }
    }

    // ── 假 STATUS 订阅器（测试可随时推帧 / 关流；并记录被要求订阅的端口与 secret）──
    //
    // 帧由测试**事后**推送而非订阅时一次给定：登录是时序问题（先 URL 后 Running、同 URL 连来两帧、
    // 幽灵 tag 夹在中间），一次性给定的脚本表达不了「先断言没发生、再推一帧证明确实发得出来」这种
    // 带正向对照的观察。
    struct FakeStatusSubscriber {
        /// true → `subscribe` 直接返 Err（驱动「订阅失败」腿）。
        fail: bool,
        /// 每次订阅记一条 `(port, secret)`。
        seen: Arc<Mutex<Vec<(u16, String)>>>,
        /// 各次订阅的推帧句柄（测试持它推帧；drop 掉即关流）。
        senders: Arc<Mutex<Vec<mpsc::UnboundedSender<daemon::TailscaleStatusUpdate>>>>,
    }

    struct FakeStatusStream {
        rx: mpsc::UnboundedReceiver<daemon::TailscaleStatusUpdate>,
    }

    #[async_trait]
    impl LoginStatusStream for FakeStatusStream {
        async fn recv(&mut self) -> Option<daemon::TailscaleStatusUpdate> {
            self.rx.recv().await
        }
    }

    #[async_trait]
    impl LoginStatusSubscriber for FakeStatusSubscriber {
        async fn subscribe(
            &self,
            port: u16,
            secret: &str,
        ) -> Result<Box<dyn LoginStatusStream>, String> {
            self.seen.lock().unwrap().push((port, secret.to_string()));
            if self.fail {
                return Err("fake subscribe fail".to_string());
            }
            let (tx, rx) = mpsc::unbounded_channel();
            self.senders.lock().unwrap().push(tx);
            Ok(Box::new(FakeStatusStream { rx }))
        }
    }

    impl FakeStatusSubscriber {
        /// 往第 `idx` 次订阅的流里推一帧。
        fn push(&self, idx: usize, update: daemon::TailscaleStatusUpdate) {
            let g = self.senders.lock().unwrap();
            g[idx].send(update).expect("流未关闭");
        }
        /// 关掉第 `idx` 次订阅的流（drop sender → `recv` 返 None）。
        fn close(&self, idx: usize) {
            self.senders.lock().unwrap().remove(idx);
        }
    }

    fn fake_subscriber(fail: bool) -> Arc<FakeStatusSubscriber> {
        Arc::new(FakeStatusSubscriber {
            fail,
            seen: Arc::new(Mutex::new(Vec::new())),
            senders: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// 一帧全量端点快照（单端点）。`auth_url` 空串 = 该帧不带 URL（与真核同语义）。
    fn frame(tag: &str, backend_state: &str, auth_url: &str) -> daemon::TailscaleStatusUpdate {
        daemon::TailscaleStatusUpdate {
            endpoints: vec![daemon::TailscaleEndpointStatus {
                endpoint_tag: tag.to_string(),
                backend_state: backend_state.to_string(),
                auth_url: auth_url.to_string(),
                ..Default::default()
            }],
        }
    }

    // ── 测试脚手架 ──
    fn ts_server(id: &str, name: &str) -> ServerConfig {
        ServerConfig {
            id: id.to_string(),
            name: name.to_string(),
            protocol: Protocol::Tailscale,
            ..Default::default()
        }
    }

    fn temp_ud() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("polaris-tslogin-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn fake_spawner(lines: Vec<String>, self_exit: bool, fail: bool) -> Arc<FakeSpawner> {
        Arc::new(FakeSpawner {
            lines,
            self_exit,
            fail,
            spawned: Arc::new(Mutex::new(Vec::new())),
            count: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn reg_with(
        spawner: Arc<FakeSpawner>,
        subscriber: Arc<FakeStatusSubscriber>,
        check_ok: bool,
        timeout: Duration,
    ) -> LoginCoreRegistry {
        LoginCoreRegistry::with_deps(
            spawner,
            Arc::new(FakeChecker { ok: check_ok }),
            subscriber,
            Arc::new(|| Ok(PathBuf::from("/fake/sing-box"))),
            timeout,
        )
    }

    /// 目录里**唯一**那份登录 config（文件名带代次，故不能写死）。顺带是「一次登录只留一份 config」
    /// 的判据：多出来就是有代次没被收掉。
    fn sole_login_config(dir: &Path) -> PathBuf {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("tailscale-login-"))
            })
            .collect();
        assert_eq!(files.len(), 1, "在飞登录应恰好留一份 config：{files:?}");
        files.pop().unwrap()
    }

    /// 有界轮询等待条件成立（无真进程/网络，条件毫秒级达成；总预算 ~2s）。
    async fn wait_until(mut cond: impl FnMut() -> bool) {
        for _ in 0..400 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("等待条件超时未成立");
    }

    /// 曾经的 URL 来源：核 stdout 的这行日志。现在它**只应进日志**。
    const AUTH_LINE: &str =
        "endpoint/tailscale[myts]: Waiting for authentication: https://login.tailscale.com/a/abc123";
    const URL_1: &str = "https://login.tailscale.com/a/abc123";
    const URL_2: &str = "https://login.tailscale.com/a/def456";

    /// 起核 + 拿到 emitter/subscriber 句柄的公共开场（双写守卫不命中、主核未运行）。
    async fn started(
        reg: &LoginCoreRegistry,
        ud: &Path,
        server: &ServerConfig,
    ) -> Arc<FakeEmitter> {
        let emitter = Arc::new(FakeEmitter::default());
        let outcome = reg
            .start_login(server, ud, false, None, 0, emitter.clone())
            .await;
        assert!(matches!(outcome, StartLoginOutcome::Started));
        emitter
    }

    fn captured(em: &FakeEmitter) -> Vec<String> {
        em.captured
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, u)| u.clone())
            .collect()
    }

    /// STATUS 帧的 `authURL` → 登录 URL 事件（serverId/nodeName/url 三元组齐全）。
    ///
    /// 变异（逐条真跑过）：把 `apply_status_frame` 里的 `emit_auth_url` 删 → 转红；
    /// 把 `ev.auth_url` 换成读 `ev.backend_state` → 转红。
    #[tokio::test]
    async fn status_auth_url_emits_event() {
        let spawner = fake_spawner(vec![], false, false);
        let sub = fake_subscriber(false);
        let reg = reg_with(spawner, sub.clone(), true, Duration::from_secs(60));
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        let emitter = started(&reg, &ud, &server).await;
        sub.push(0, frame("myts", "NeedsLogin", URL_1));
        wait_until(|| !emitter.captured.lock().unwrap().is_empty()).await;
        let cap = emitter.captured.lock().unwrap();
        assert_eq!(cap.len(), 1);
        assert_eq!(cap[0].0, "ts1");
        assert_eq!(cap[0].1, "myts");
        assert_eq!(cap[0].2, URL_1);
        drop(cap);
        reg.cancel_login("ts1");
        let _ = std::fs::remove_dir_all(&ud);
    }

    /// **stdout 不再是 URL 来源**（本批的核心行为改动）。
    ///
    /// 负向断言配正向对照：先喂那行历史 stdout 日志并确认**没有**事件，再从 STATUS 推同一个 URL 并
    /// 确认事件到达 —— 后半段证明「没发事件」不是因为夹具根本发不出事件。
    /// 变异：把 stdout→URL 的解析加回 supervise → 前半段的 `assert!(cap.is_empty())` 转红。
    #[tokio::test]
    async fn stdout_auth_line_is_no_longer_a_url_source() {
        let spawner = fake_spawner(vec![AUTH_LINE.to_string()], false, false);
        let sub = fake_subscriber(false);
        let reg = reg_with(spawner, sub.clone(), true, Duration::from_secs(60));
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        let emitter = started(&reg, &ud, &server).await;
        // 核已把那行吐完（duplex 写端随即 drop），给 relay 充分时间；仍不得有任何事件。
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            captured(&emitter).is_empty(),
            "stdout 的 Waiting for authentication 行不得再产出登录 URL 事件"
        );
        // 正向对照：同一个 URL 走 STATUS 就必须发得出来。
        sub.push(0, frame("myts", "NeedsLogin", URL_1));
        wait_until(|| !captured(&emitter).is_empty()).await;
        assert_eq!(captured(&emitter), vec![URL_1.to_string()]);
        reg.cancel_login("ts1");
        let _ = std::fs::remove_dir_all(&ud);
    }

    /// 同一 URL 每帧都来 → 只通知一次；URL 变了（重开授权）→ 再通知一次。
    /// 变异：把 `if next != state` 去掉（无条件 emit）→ 第一条断言从 1 变 2 转红。
    #[tokio::test]
    async fn repeated_auth_url_emits_once_changed_url_emits_again() {
        let spawner = fake_spawner(vec![], false, false);
        let sub = fake_subscriber(false);
        let reg = reg_with(spawner, sub.clone(), true, Duration::from_secs(60));
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        let emitter = started(&reg, &ud, &server).await;
        sub.push(0, frame("myts", "NeedsLogin", URL_1));
        sub.push(0, frame("myts", "NeedsLogin", URL_1));
        wait_until(|| !captured(&emitter).is_empty()).await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            captured(&emitter),
            vec![URL_1.to_string()],
            "同 URL 只发一次"
        );
        sub.push(0, frame("myts", "NeedsLogin", URL_2));
        wait_until(|| captured(&emitter).len() == 2).await;
        assert_eq!(
            captured(&emitter),
            vec![URL_1.to_string(), URL_2.to_string()]
        );
        reg.cancel_login("ts1");
        let _ = std::fs::remove_dir_all(&ud);
    }

    /// `backendState == "Running"` = 登录成功（控制面终局肯定）→ 主动收核 + 注销。
    /// 这正是「登录成功判据从『无法判定』升级」的那一格：此前核要空跑到 5 分钟超时。
    ///
    /// 变异：把 `ExitReason::LoggedIn` 那条 `break` 删 → 核不被终止、表项还在 → 两条断言都转红；
    /// 把判据从 `== "Running"` 换成 `ev.logged_in` → 下面 `Starting` 那格会提前收核 → 转红。
    #[tokio::test]
    async fn running_backend_state_reaps_core_but_starting_does_not() {
        let spawner = fake_spawner(vec![], false, false);
        let sub = fake_subscriber(false);
        let reg = reg_with(spawner.clone(), sub.clone(), true, Duration::from_secs(60));
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        let _emitter = started(&reg, &ud, &server).await;
        // Starting（`logged_in` 谓词会把它算作已登录）**不是**收核判据：那只是「在连」。
        sub.push(0, frame("myts", "Starting", ""));
        tokio::time::sleep(Duration::from_millis(120)).await;
        let st = spawner.spawned.lock().unwrap()[0].clone();
        assert!(
            !st.terminated.load(Ordering::SeqCst),
            "Starting 不得当成登录成功"
        );
        assert!(reg.shared.contains("ts1"));
        // Running → 收核 + 注销。
        sub.push(0, frame("myts", "Running", ""));
        wait_until(|| st.terminated.load(Ordering::SeqCst)).await;
        wait_until(|| !reg.shared.contains("ts1")).await;
        let _ = std::fs::remove_dir_all(&ud);
    }

    /// 端点 tag 不是本节点（幽灵/历史端点）→ 整条丢弃，既不发 URL 也不算登录成功。
    /// 变异：把 `tag_to_id` 换成空映射之外的任何「兜底放行」→ 第一段断言转红。
    #[tokio::test]
    async fn frame_for_other_endpoint_tag_is_ignored() {
        let spawner = fake_spawner(vec![], false, false);
        let sub = fake_subscriber(false);
        let reg = reg_with(spawner.clone(), sub.clone(), true, Duration::from_secs(60));
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        let emitter = started(&reg, &ud, &server).await;
        sub.push(0, frame("别人的节点", "Running", URL_1));
        tokio::time::sleep(Duration::from_millis(120)).await;
        let st = spawner.spawned.lock().unwrap()[0].clone();
        assert!(captured(&emitter).is_empty(), "不在册 tag 不得产出 URL");
        assert!(
            !st.terminated.load(Ordering::SeqCst),
            "不在册 tag 的 Running 不得当成本节点登录成功"
        );
        // 正向对照：换成本节点 tag 就必须两样都发生。
        sub.push(0, frame("myts", "NeedsLogin", URL_1));
        wait_until(|| !captured(&emitter).is_empty()).await;
        let _ = std::fs::remove_dir_all(&ud);
    }

    /// STATUS 流终止 = 判据来源没了（不回退 stdout）→ 就地收核 + 注销，不让核空跑到超时。
    /// 变异：把 `StatusStreamEnded` 腿改成继续循环 → 两条 `wait_until` 超时 panic 转红。
    #[tokio::test]
    async fn status_stream_end_reaps_core() {
        let spawner = fake_spawner(vec![], false, false);
        let sub = fake_subscriber(false);
        let reg = reg_with(spawner.clone(), sub.clone(), true, Duration::from_secs(60));
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        let _emitter = started(&reg, &ud, &server).await;
        wait_until(|| !spawner.spawned.lock().unwrap().is_empty()).await;
        let st = spawner.spawned.lock().unwrap()[0].clone();
        sub.close(0);
        wait_until(|| st.terminated.load(Ordering::SeqCst)).await;
        wait_until(|| !reg.shared.contains("ts1")).await;
        let _ = std::fs::remove_dir_all(&ud);
    }

    /// 订阅失败 → 硬失败（不回退 stdout），且**必须先收掉已 spawn 的核**、不留表项，
    /// 否则留下一个谁都不认识的孤儿 sing-box。
    /// 变异：把订阅失败腿里的 `child.terminate()` 删 → `terminated` 断言转红。
    #[tokio::test]
    async fn subscribe_failure_kills_spawned_core_and_reports_failure() {
        let spawner = fake_spawner(vec![], false, false);
        let sub = fake_subscriber(true); // 订阅必失败
        let reg = reg_with(spawner.clone(), sub, true, Duration::from_secs(60));
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        let outcome = reg
            .start_login(
                &server,
                &ud,
                false,
                None,
                0,
                Arc::new(FakeEmitter::default()),
            )
            .await;
        assert!(matches!(outcome, StartLoginOutcome::Failed(_)));
        assert!(!reg.shared.contains("ts1"), "订阅失败不得留表项");
        let st = spawner.spawned.lock().unwrap()[0].clone();
        assert!(
            st.terminated.load(Ordering::SeqCst),
            "订阅失败必须收掉已起的核，禁留孤儿"
        );
        let _ = std::fs::remove_dir_all(&ud);
    }

    /// 瞬态核管理 API 的端口与 secret：端口非 0、避开主核 api 端口；secret 是 32 位 hex（非空）。
    /// 空 secret = 同机任意进程都能读该核的 tailnet 拓扑，故「有 secret」本身就是判据。
    /// 变异：把 `TailscaleLoginApiService.secret` 传空串 → hex 断言转红；把 `primary_api_port`
    /// 从排除集里删 → 端口断言在撞上时转红（撞概率低，故另有 `port_bookkeeping` 的确定性桩测）。
    #[tokio::test]
    async fn login_api_port_and_secret_are_resolved_and_handed_to_subscriber() {
        let spawner = fake_spawner(vec![], false, false);
        let sub = fake_subscriber(false);
        let reg = reg_with(spawner, sub.clone(), true, Duration::from_secs(60));
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        // 主核 api 端口占着 9099 → 瞬态核不得选它。
        let outcome = reg
            .start_login(
                &server,
                &ud,
                false,
                None,
                9099,
                Arc::new(FakeEmitter::default()),
            )
            .await;
        assert!(matches!(outcome, StartLoginOutcome::Started));
        let seen = sub.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        let (port, secret) = &seen[0];
        assert_ne!(*port, 0, "必须解析出真实端口");
        assert_ne!(*port, 9099, "不得与运行主核的管理 API 端口相撞");
        assert_eq!(secret.len(), 32, "16 字节 CSPRNG → 32 位 hex");
        assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
        // 写盘的配置里也必须带着这一对（否则核根本不会 listen 在这个口上）。
        let cfg: serde_json::Value =
            serde_json::from_slice(&std::fs::read(sole_login_config(&ud)).unwrap()).unwrap();
        assert_eq!(cfg["services"][0]["listen_port"], u64::from(*port));
        assert_eq!(cfg["services"][0]["secret"], *secret);
        reg.cancel_login("ts1");
        let _ = std::fs::remove_dir_all(&ud);
    }

    /// 临时 config 里有一次性 secret → 生命周期必须跟核一致：核在时在盘上，收核后不留。
    /// 且 kill-on-relogin 下**两代不共用同一个文件名**——否则旧代 supervisor 的删除会打在新代刚写好
    /// 的那份上（它的 terminate 有最长 5s 宽限，删除随时可能晚于新核 spawn）。
    ///
    /// 变异：把文件名里的 `-{epoch}` 去掉 → 两代同名，`assert_ne!` 转红；
    /// 把收核后的 `remove_login_config` 删掉 → 末条断言转红。
    #[tokio::test]
    async fn login_config_holds_secret_so_it_dies_with_the_core() {
        let spawner = fake_spawner(vec![], false, false);
        let sub = fake_subscriber(false);
        let reg = reg_with(spawner.clone(), sub.clone(), true, Duration::from_secs(60));
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        let _em = started(&reg, &ud, &server).await;
        let first_cfg = sole_login_config(&ud);
        // relogin：新一代必须写到**另一个**文件名下。
        let _em2 = started(&reg, &ud, &server).await;
        wait_until(|| spawner.count.load(Ordering::SeqCst) == 2).await;
        // 旧代被杀 → 它删自己那份；新代那份留着。剩下的这一份必须不是旧的那份。
        wait_until(|| !first_cfg.exists()).await;
        let second_cfg = sole_login_config(&ud);
        assert_ne!(first_cfg, second_cfg, "两代不得共用同一个 config 文件名");
        // 收掉新代 → 盘上不再留任何带 secret 的 config。
        reg.cancel_login("ts1");
        wait_until(|| !second_cfg.exists()).await;
        let _ = std::fs::remove_dir_all(&ud);
    }

    #[tokio::test]
    async fn check_failure_returns_error_without_spawning() {
        let spawner = fake_spawner(vec![], false, false);
        let reg = reg_with(
            spawner.clone(),
            fake_subscriber(false),
            false,
            Duration::from_secs(60),
        );
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        let outcome = reg
            .start_login(
                &server,
                &ud,
                false,
                None,
                0,
                Arc::new(FakeEmitter::default()),
            )
            .await;
        assert!(matches!(outcome, StartLoginOutcome::Failed(_)));
        assert_eq!(
            spawner.count.load(Ordering::SeqCst),
            0,
            "check 失败不得 spawn"
        );
        assert!(!reg.shared.contains("ts1"));
        let _ = std::fs::remove_dir_all(&ud);
    }

    #[tokio::test]
    async fn spawn_failure_returns_error_and_no_entry() {
        let spawner = fake_spawner(vec![], false, true);
        let reg = reg_with(
            spawner,
            fake_subscriber(false),
            true,
            Duration::from_secs(60),
        );
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        let outcome = reg
            .start_login(
                &server,
                &ud,
                false,
                None,
                0,
                Arc::new(FakeEmitter::default()),
            )
            .await;
        assert!(matches!(outcome, StartLoginOutcome::Failed(_)));
        assert!(!reg.shared.contains("ts1"), "spawn 失败不得留表项");
        let _ = std::fs::remove_dir_all(&ud);
    }

    #[tokio::test]
    async fn guard_blocks_duplicate_endpoint_login() {
        let spawner = fake_spawner(vec![], false, false);
        let reg = reg_with(
            spawner.clone(),
            fake_subscriber(false),
            true,
            Duration::from_secs(60),
        );
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        // 运行主核配置里已含该 TS 节点 → 双写守卫命中。
        let running = UserConfig {
            servers: vec![ts_server("ts1", "myts")],
            ..Default::default()
        };
        let outcome = reg
            .start_login(
                &server,
                &ud,
                true,
                Some(&running),
                0,
                Arc::new(FakeEmitter::default()),
            )
            .await;
        assert!(matches!(outcome, StartLoginOutcome::InMainCore));
        assert_eq!(
            spawner.count.load(Ordering::SeqCst),
            0,
            "守卫命中不得 spawn"
        );
        assert!(!reg.shared.contains("ts1"));
        let _ = std::fs::remove_dir_all(&ud);
    }

    #[tokio::test]
    async fn cancel_kills_and_deregisters() {
        let spawner = fake_spawner(vec![], false, false);
        let reg = reg_with(
            spawner.clone(),
            fake_subscriber(false),
            true,
            Duration::from_secs(60),
        );
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        assert!(matches!(
            reg.start_login(
                &server,
                &ud,
                false,
                None,
                0,
                Arc::new(FakeEmitter::default())
            )
            .await,
            StartLoginOutcome::Started
        ));
        wait_until(|| reg.shared.contains("ts1")).await;
        assert!(reg.cancel_login("ts1"), "取消在飞登录返 true");
        assert!(!reg.shared.contains("ts1"), "cancel 立即注销");
        let st = spawner.spawned.lock().unwrap()[0].clone();
        wait_until(|| st.terminated.load(Ordering::SeqCst)).await;
        // 幂等：再取消不存在的登录 → false（非错误）。
        assert!(!reg.cancel_login("ts1"));
        let _ = std::fs::remove_dir_all(&ud);
    }

    #[tokio::test]
    async fn relogin_kills_prior_child() {
        let spawner = fake_spawner(vec![], false, false);
        let reg = reg_with(
            spawner.clone(),
            fake_subscriber(false),
            true,
            Duration::from_secs(60),
        );
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        let em = Arc::new(FakeEmitter::default());
        assert!(matches!(
            reg.start_login(&server, &ud, false, None, 0, em.clone())
                .await,
            StartLoginOutcome::Started
        ));
        wait_until(|| reg.shared.contains("ts1")).await;
        // 同 server 再登录 → 先杀旧核。
        assert!(matches!(
            reg.start_login(&server, &ud, false, None, 0, em.clone())
                .await,
            StartLoginOutcome::Started
        ));
        wait_until(|| spawner.count.load(Ordering::SeqCst) == 2).await;
        let first = spawner.spawned.lock().unwrap()[0].clone();
        wait_until(|| first.terminated.load(Ordering::SeqCst)).await;
        // 新核仍在册且未被终止。
        assert!(reg.shared.contains("ts1"));
        let second = spawner.spawned.lock().unwrap()[1].clone();
        assert!(!second.terminated.load(Ordering::SeqCst));
        reg.cancel_login("ts1");
        let _ = std::fs::remove_dir_all(&ud);
    }

    #[tokio::test]
    async fn timeout_fires_kill_and_deregisters() {
        let spawner = fake_spawner(vec![], false, false);
        let reg = reg_with(
            spawner.clone(),
            fake_subscriber(false),
            true,
            Duration::from_millis(80),
        );
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        assert!(matches!(
            reg.start_login(
                &server,
                &ud,
                false,
                None,
                0,
                Arc::new(FakeEmitter::default())
            )
            .await,
            StartLoginOutcome::Started
        ));
        wait_until(|| !spawner.spawned.lock().unwrap().is_empty()).await;
        let st = spawner.spawned.lock().unwrap()[0].clone();
        wait_until(|| st.terminated.load(Ordering::SeqCst)).await; // 超时触发 kill
        wait_until(|| !reg.shared.contains("ts1")).await; // 注销
        let _ = std::fs::remove_dir_all(&ud);
    }

    #[tokio::test]
    async fn child_self_exit_reaps_registry() {
        let spawner = fake_spawner(vec![], true, false); // 自然退出
        let reg = reg_with(
            spawner.clone(),
            fake_subscriber(false),
            true,
            Duration::from_secs(60),
        );
        let ud = temp_ud();
        let server = ts_server("ts1", "myts");
        assert!(matches!(
            reg.start_login(
                &server,
                &ud,
                false,
                None,
                0,
                Arc::new(FakeEmitter::default())
            )
            .await,
            StartLoginOutcome::Started
        ));
        wait_until(|| !reg.shared.contains("ts1")).await; // 自然退出后 reap
        let st = spawner.spawned.lock().unwrap()[0].clone();
        assert!(
            !st.terminated.load(Ordering::SeqCst),
            "自然退出不应触发主动 terminate"
        );
        let _ = std::fs::remove_dir_all(&ud);
    }
}
