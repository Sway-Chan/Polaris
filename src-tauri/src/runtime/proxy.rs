//! 代理运行时：sing-box 进程编排（上游 `ProxyManager` 等价物）。
//!
//! 装配既有 domain crate（本层只做**接线**，状态机/门控逻辑一律不在此重写）：
//! - [`polaris_core_supervisor`]：`TokioSpawner`（真 spawn）+ `LifecycleGate`（起停竞态单飞）
//!   + `wait_for_core_ready`（就绪门）+ `ProcessKiller`（SIGTERM→宽限→SIGKILL）+ `PortAllocator`（端口簿记）。
//! - [`polaris_switch_engine`]：`DebouncedRestart`（去抖重启 timer + 世代守卫，内部复用 LifecycleGate）。
//! - [`polaris_config_engine`]：`generate_sing_box_config`（config 生成）+ `proxy_ports`（端口单一真值）。
//!
//! 状态：运行标志 + sing-box 子进程句柄 + 管理 API 端点。
//! 启动/停止语义对齐 上游 ProxyManager.start/stop。
//!
//! # 管理 API 是 gRPC，不是 clash REST（实测结论）
//!
//! 上游 `ProxyManager.ts:2360` 明载「clash_api 已移除」；1.14 起管理面走 `services:[{type:'api'}]`
//! 的 **h2c gRPC**（daemon.StartedService），由 [`polaris_singbox_grpc`] 客户端消费。本机对真核
//! 实测（取证于 1.14.0-alpha.44，结论按 1.14 带记，非随包版本号）：该端口对 HTTP/1.1 GET 返回 404，对 HTTP/2 prior-knowledge 返回 h2 帧
//! —— 故就绪判据只能是「TCP 可连」（`core-readiness.ts` 原义），不能是「REST 200」。
//!
//! # 端口三轴（勿混）
//! - `mixed_port`：混合入站（HTTP/SOCKS），`local_proxy_port` 解析。
//! - `control_api_port`：历史 clash 控制端口（9090），仅作端口排除项，**核不再监听它**。
//! - `api_port`：1.14 管理 API 实际监听端口，`PortAllocator` 每次 start 动态解析（对齐 上游
//!   `resolveTailscaleApiPort`：排除 control/http/socks/mixed，fallback = control+1）。

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::sync::{Mutex as AsyncMutex, Notify};

use polaris_config_engine::builder::custom_rule_files::{
    build_custom_rule_files, is_custom_rule_orphan_file,
};
use polaris_config_engine::builder::endpoint_routes::{
    mesh_system_supported_on_platform, mesh_uses_system_interface,
};
use polaris_config_engine::builder::helpers::ServerLike;
use polaris_config_engine::builder::hotswitch::{
    can_skip_restart_for_added_unreferenced, plan_hot_switch, HotSwitchDeps, RuleTargetEntry,
};
use polaris_config_engine::builder::orchestration::{config_generation_norm, stable_stringify};
use polaris_config_engine::builder::outbounds::{build_outbounds, OutboundsDeps};
use polaris_config_engine::builder::route::mesh_selected_exit_falls_back_to_direct;
use polaris_config_engine::builder::{
    build_id_to_tag_map, generate_sing_box_config_with_report, GenerateConfigDeps, GenerateOutcome,
    InvalidNode,
};
use polaris_config_engine::singbox::SingBoxConfig;
use polaris_config_engine::user_config::app_config::UserConfig;
use polaris_config_engine::user_config::dns_constants::{
    is_direct_selection, DIRECT_TAG, PROXY_SELECTOR_TAG,
};
// `enumerate_own_lan_cidrs` 的 unix（getifaddrs）与 windows（GetAdaptersAddresses）两腿共用这套纯逻辑；
// 其余假想平台走空 stub，不消费 → cfg 门避免 unused import 告警。
#[cfg(any(unix, windows))]
use polaris_config_engine::user_config::own_lan::{dedupe_own_lan, own_lan_cidr};
use polaris_config_engine::user_config::proxy_mode::ProxyMode;
use polaris_config_engine::user_config::proxy_ports::{control_api_port, local_proxy_port};
use polaris_config_engine::user_config::rule::RuleAction;
use polaris_config_engine::user_config::server_config::{Protocol, ServerConfig};
use polaris_config_engine::user_config::system_proxy_bypass::{effective_bypass_lan, BypassConfig};
use polaris_config_engine::user_config::tun_config::resolve_win_tun_interface_name;
use polaris_config_engine::user_config::ProxyModeType;
// LifecycleEndResult 未在 crate root 再导出（其兄弟类型 LifecycleGate/LifecycleKind 有）→ 走模块路径。
use polaris_core_supervisor::lifecycle_gate::LifecycleEndResult;
use polaris_core_supervisor::port_bookkeeping::TokioPortProvider;
use polaris_core_supervisor::{
    classify_child_exit, decide_peel, run_config_check, scan_running_cores, stale_pids,
    wait_for_core_ready, AutoRestartOutcome, ChildObservation, CoreReadyDeps, CoreReadyOutcome,
    CrashRecoveryMachine, ExitClassification, FailureOutcome, KernelRejection, LifecycleGate,
    LifecycleKind, PeelStep, PortAllocator, PortExclusions, ProcessKiller, RejectedArray,
    RestartFate, Signal, SingBoxSpawner, SpawnRequest, TokioSpawner, WaitForCoreReadyOptions,
    INVALID_REASON_KERNEL_REJECTED, PEEL_TIME_BUDGET,
};
use polaris_dns_race::{
    plan_upstreams, DecoySet, DefaultUpstreamQuery, DohPost, NodeDnsRaceServer, OnRaceServerDead,
    DEFAULT_RACE_BUDGET,
};
use polaris_singbox_grpc::{Endpoint, ReconnectConfig, SingBoxApiClient};
use polaris_stats_engine::DiagnosticCounters;
use polaris_switch_engine::{
    decide, DebouncedOutcome, DebouncedRestart, DecisionInput, HotSwitchOutcome, ManagementApi,
    SwitchDecision, SwitchExecutor,
};
use polaris_system_integration::error::SystemIntegrationError;
use polaris_system_integration::proxy::MarkerFs;
use polaris_system_integration::proxy_ops::{
    ProxyEnableRequest, SystemProxyController, SystemProxyOps,
};
#[cfg(not(target_os = "windows"))]
use polaris_system_integration::route_ops::SystemRouteOps;
use polaris_system_integration::route_ops::{
    verify_exit_captured, ExitCaptureOutcome, PROBE_IP as ROUTE_PROBE_IP,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;

use crate::runtime::auto_switch::{
    extract_candidates, plan_switch, select_best_candidate, AutoNodeSwitchedPayload,
    AutoSwitchMachine, CandidateLatency, HeartbeatOutcome, SwitchGate, CONNECTIVITY_TIMEOUT_MS,
    CONNECTIVITY_URLS, HEARTBEAT_INTERVAL_MS, PING_TIMEOUT_MS,
};
use crate::runtime::config::ConfigManager;
use crate::runtime::helper::{HelperRuntime, HelperStatusSnapshot, HelperStopOps};
use crate::runtime::management_api::{GroupSelection, GrpcManagementApi};
use crate::runtime::mesh::{
    mesh_login_fallback_should_engage, MeshLoginFallbackInput, MeshRuntime,
};
use crate::runtime::node_fingerprints;
use crate::runtime::tailscale_status::{
    decode_tailscale_status, derive_ts_exit_warning, is_definitive_logged_out,
    TailscaleStatusEvent, TsExitWarning, TsExitWarningInput,
};
use crate::runtime::win_console::no_console_window;
use polaris_helper_proto::Platform;

/// 就绪等待**总超时**（上游 `ProxyManager.CORE_READY_TIMEOUT_MS`，:524）。
///
/// **不得随轮询间隔一起缩短**：这是「慢机器上核到底还起不起得来」的容忍度，调小会把冷启动/杀软扫描
/// 拖慢的正常起核误判成失败。轮询间隔只决定「就绪后多久发现」，与本值正交（见 [`CORE_READY_POLL_MS`]）。
const CORE_READY_TIMEOUT_MS: u64 = 12_000;
/// 就绪轮询间隔。
///
/// # 为什么是 50 而非 上游的 500（**刻意分歧**，非移植疏漏）
///
/// 本值只决定**发现就绪的延迟**，不决定能等多久（那是 [`CORE_READY_TIMEOUT_MS`]）。实测管理 API 口
/// 在 97–221ms 就已 listen，而 500ms 的栅格把「已经就绪」的事实压到下一个刻度才发现 → 平均白等
/// ~250ms、最坏 ~500ms，纯粹是采样精度造成的启动延迟。降到 50ms 后该项 ≤50ms（省 ~0.3s）。
///
/// **CPU 无虞**：每轮只是一次 loopback TCP connect（就绪前是即时 ECONNREFUSED），且一旦可连即
/// 短路返回 —— 典型只多跑几轮，不是忙等。
///
/// **总预算不变**：`max_polls = ceil(timeout/poll)` 随之 24 → 240，覆盖的仍是同一个 12s 窗口。
const CORE_READY_POLL_MS: u64 = 50;
/// 单次 TCP 就绪探测超时（上游 `probeTcpReachable` 默认 1000ms，core-readiness.ts:42）。
const READY_PROBE_TIMEOUT: Duration = Duration::from_millis(1000);
/// SIGTERM→SIGKILL 宽限期（上游 `stopSingBoxProcess` 的 5s 优雅窗口，:5230）。
const STOP_GRACE: Duration = Duration::from_secs(5);
/// 崩溃监测轮询间隔（ms）。tokio `Child::wait()` 单持有者 → 监测只能轮询 `try_wait`（见
/// `spawn_crash_monitor`）；1s 与健康检查同量级，CPU 可忽略，崩溃检出延迟 ≤1s。
const CRASH_MONITOR_POLL_MS: u64 = 1_000;
/// helper 腿的 pid **身份**复核间隔（单位：tick，1 tick = [`CRASH_MONITOR_POLL_MS`]）。
///
/// 不每 tick 复核：mac/win 的身份取材要 spawn 子进程（`ps` / `tasklist`），1Hz 常驻 spawn 不值当。
/// 10s 的检出延迟对一件**今天永远检不出**的事是纯增量，不是折衷（见 [`process_identity`]）。
const PID_IDENTITY_RECHECK_TICKS: u64 = 10;
/// stale-core 清扫 SIGTERM→SIGKILL 宽限期（对齐 上游 `killOrphanedProcessesLinux` 的 1.5s，:1132）。
const STALE_KILL_GRACE: Duration = Duration::from_millis(1_500);
/// **C6-5**：helper 起核时 daemon 侧 sing-box 早期 stdout/stderr 重定向的日志文件名（app 无法捕获 root
/// 受管核的管道，故经 helper 落文件；对齐 上游 `singbox_startup.log`）。落 `<configDir>/`。
const SINGBOX_STARTUP_LOG: &str = "singbox-startup.log";

/// **§15 主核测速探测池槽数 K**（上游 `shared/speed-test.ts PROBE_POOL_SIZE`，单一真值）。
///
/// 起核时分配 K 个空闲回环端口注入 `probe_pool_ports` → config-engine 据此建 K 个 `probe-in-k`（http 入站）
/// `probe-selector-k`（成员=全量 nodeTags）、`probe-in-k→probe-selector-k` 路由、`dns-probe-exit-k`。
/// 测速时按波经 gRPC `select_outbound` 把各槽热切到被测节点、经 `probe-in-k` 端口量 warm-TTFB（同核单会话，
/// 结构性消除 WG/WARP 双会话超时）。K=16 对齐 上游；**分配失败 → 空池（回退当前活跃出口测速）**，`=0` 为回滚锚点。
const PROBE_POOL_SIZE: usize = 16;

/// sing-box 运行态快照（上游 `ProxyStatus` 镜像，序列化字段名与前端一致）。
///
/// 上游 `shared/types/runtime.ts ProxyStatus`：`{ running, pid?, startTime?, uptime?, error?, errorCode? }`。
/// 另携带 mixedPort/clashApiPort（非 上游 ProxyStatus 字段，但 dashboard / 内部端口探测用）。
///
/// # `startTime` 是运行时长的唯一真值，`uptime` 是它的读时投影
///
/// 起核时刻只有后端知道 → `start_time` 由 [`start_inner`](ProxyRuntime::start_inner) 在**就绪后**
/// 落一次（与 `running` 同生共死：`set_error`/`stop` 都经 `..Default::default()` 清回 None）。
/// `uptime` **不存**：存了就会在快照里瞬间过期（快照写于起核那一刻，读可能在几小时后）。
/// 它由 [`status()`](ProxyRuntime::status) **每次读时**从 `start_time` 现算 —— 故存储态恒 `None`，
/// **禁止直接读 `self.status` 里的该字段**，一律经 `status()` 取。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProxyStatus {
    /// 是否运行中。
    pub running: bool,
    /// **是否有起核腿在飞**（`running:false` 期间也可能正在起核——重试预算内一轮可达数十秒）。
    ///
    /// **读时投影**（同 `uptime`）：存储态恒 `false`，真值是 [`ProxyRuntime::start_inflight`] 计数，
    /// 由 [`status()`](ProxyRuntime::status) 在应答那一刻现算。故 `*status.write() = ProxyStatus{..}`
    /// 的各处赋值不必也不应写它。
    ///
    /// **为什么必须暴露给渲染端**：托盘浮层是独立窗口、不共享主窗 store，只能从本快照得知「此刻正在
    /// 启动」。缺了它，托盘在起核期看到的是 `running:false` ⇒ 点击走 start 分支 ⇒ 在已有起核腿之上
    /// **再叠一次启动**（TrayMenu.tsx 原 :219-236 的缺陷）。
    #[serde(default, skip_serializing_if = "is_false")]
    pub starting: bool,
    /// sing-box 进程 pid（未运行=0）。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub pid: u32,
    /// 起核就绪时刻（epoch ms）；未运行 = None。前端「运行时长」的真值源。
    #[serde(rename = "startTime", default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<u64>,
    /// 已运行秒数 —— `start_time` 的**读时投影**（见结构体文档）。存储态恒 None，勿写。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime: Option<u64>,
    /// mixedPort（运行期 HTTP/SOCKS 混合入站；未运行=0）。
    #[serde(rename = "mixedPort", default)]
    pub mixed_port: u16,
    /// 管理 API 端口（sing-box 1.14 `services:[{type:'api'}]` 的 h2c gRPC 监听口；未运行=0）。
    ///
    /// 字段名保留 `clashApiPort` 与前端契约一致（前端仍用此名取 dashboard 端口），但**语义已是
    /// 管理 API 端口**——与 上游 `getTailscaleApiPort()` 同源（:2369），非历史 clash REST 端口。
    #[serde(rename = "clashApiPort", default)]
    pub clash_api_port: u16,
    /// updateInPort（运行期更新链路 update-in socks 入站口；未运行/未分配=0）。
    ///
    /// **C19**：更新链路（App/资源/图标抓取）「经代理」时，流量 pin 到此 loopback socks 口，由 route
    /// 头部按 proxyMode 钉死出站（global/smart→出口 / direct→直连）。消费方经
    /// [`resolve_update_proxy_target`](crate::runtime::http::resolve_update_proxy_target) 决策走此口 vs 直连。
    /// = 上游 `ProxyManager.updateInPort`（allocateProbePorts 产出，UpdateNetwork/icon-protocol 消费）。
    #[serde(rename = "updateInPort", default)]
    pub update_in_port: u16,
    /// 是否经 helper 启动（macOS 提权路径）。
    #[serde(rename = "startedViaHelper", default)]
    pub started_via_helper: bool,
    /// 最近一次错误消息（启动失败 / 运行期崩溃）。仅供展示/日志，**禁止用于分类**（用 `error_code`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 最近一次错误的结构化码（前端 `ProxyErrorCode`）。与 `error` 同点落值（[`set_error`](ProxyRuntime::set_error)），
    /// 也同点经 `event:proxyError` 推送 —— 快照与事件同源，错过事件的 UI 仍能从状态读到码。
    ///
    /// 值域限于[`code`] 模块的常量：**只用控制流位置能诚实断言的码**（如「起核腿失败」⇒ `STARTUP_FAILED`），
    /// 绝不靠猜 message/退出码反推（本仓尚无核错误分类器，猜=伪造分类）。
    #[serde(rename = "errorCode", default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// 代理错误码（前端 `ui/src/contracts/types/runtime.ts` 的 `ProxyErrorCode` string enum 子集镜像）。
///
/// **只收录本层能从控制流位置诚实断言的成员**：本仓无「核 stderr / 退出码 → 错误码」分类器，
/// 补全其余成员就只能靠猜 message 关键字 = 伪造分类。故此处刻意只有 3 个 —— 缺的不是漏了，是**没有依据**。
pub mod code {
    /// 起核腿失败（就绪门判定核已死 / 就绪超时）——「启动失败」轴。
    pub const STARTUP_FAILED: &str = "STARTUP_FAILED";
    /// 核**意外**退出且无法自愈（无可用配置重启）——「运行中崩了」轴。
    pub const PROCESS_EXITED: &str = "PROCESS_EXITED";
    /// 崩溃自愈达上限放弃（反复崩溃 / 自愈重启反复失败）——「运行中崩了」轴的终态。
    pub const AUTO_RESTART_FAILED: &str = "AUTO_RESTART_FAILED";
    /// TUN 经提权 helper 起核，但 helper 未安装（起核前置校验拦截）——「权限/环境」轴。控制流位置可
    /// 诚实断言（判定点直接读到 helper 未装），非猜 message；渲染端据此引导去「设置 › Helper」安装。
    pub const HELPER_NOT_INSTALLED: &str = "HELPER_NOT_INSTALLED";
    /// **T3 终态**：上个会话遗留的 **root 孤儿核清不掉**（用户态 EPERM 杀不动，且 helper 不可用/清扫失败）
    /// ——「权限/环境」轴。对齐 上游 `ROOT_ORPHAN_BLOCKED` 语义。
    ///
    /// **为什么必须是一个诚实终态而不是继续起核**：活着的孤儿核一直独占 `cache.db`，此时起任何新核
    /// 都会 `initialize cache-file: timeout` 而失败，且**连切回 systemProxy 模式也起不来**。若在此静默
    /// 放行，用户看到的是一串莫名其妙的启动失败、无从下手；报出本码才能指向真正的动作
    /// （装/修 helper，或手动 `sudo kill` 掉残留 pid）。控制流位置可诚实断言（清扫腿直接观察到
    /// 「杀过了、仍存活、且提权腿不可用」），非猜 message。
    pub const ROOT_ORPHAN_BLOCKED: &str = "ROOT_ORPHAN_BLOCKED";
    /// **A1**：核已就绪，但把 OS 系统代理指向本地 mixed 入站失败（`networksetup`/`gsettings`/`reg` 报错）
    /// ——「流量不经核」轴。控制流位置可诚实断言（`enable_system_proxy` 直接返 `Err`），非猜 message。
    ///
    /// **非终态**：核确在运行，故经 [`set_nonfatal_error`](super::ProxyRuntime::set_nonfatal_error) 落值
    /// （保留 `running/pid/端口`），**绝不**走 `set_error`（那会把活核标成 not-running = 虚报）。
    /// 与前端 `ProxyErrorCode.SYSTEM_PROXY_FAILED` 逐字对齐（已在 `error-handler.ts` 归入 `System` 类）。
    pub const SYSTEM_PROXY_FAILED: &str = "SYSTEM_PROXY_FAILED";
    /// **出口自证**：核已就绪，但「实际生效出口」≠「用户选中节点」——「静默直连 / 走错节点」轴。
    ///
    /// 判据是**纯静态**的：拿核实际启动的那份 sing-box config（`route.final` + selector `default`）解出
    /// 实际默认出口，与用户落盘的 `selectedServerId` 对账（见 [`attest_effective_exit`](super::attest_effective_exit)）。
    /// 非终态（核在跑），同走 `set_nonfatal_error`。这是「用户以为走代理、实则明文直连」的唯一告警通道。
    pub const EXIT_MISMATCH: &str = "EXIT_MISMATCH";
    /// **内核自证**：核已就绪，但**实际跑起来的那个二进制**不是本次期望的核——「换核没生效」轴。
    ///
    /// 与 [`EXIT_MISMATCH`] 的判据形态**刻意相反**：那一条是纯静态对账（两个输入都源自「意图」），
    /// 本条只吃**事实**——`running` 取自内核对该 pid 的记账（linux `/proc/<pid>/exe`、mac `ps -o comm=`），
    /// 版本取自**对那个文件真跑一次 `sing-box version`**。理由是血证：TUN 提权路径上
    /// 「app 请求 bin=A、helper 实跑 bin=B」持续一天多而全链零告警（p101，A=1.14.0-beta.3、
    /// B=1.14.0-alpha.45），静态对账在此天然瞎——两侧根本不共享同一个「意图」。
    ///
    /// 非终态（核确在跑，只是版本不对），同走 `set_nonfatal_error`。
    pub const CORE_BINARY_MISMATCH: &str = "CORE_BINARY_MISMATCH";
    /// **规则资源缺失**：本次生成有 rule_set tag 因本地 `.srs` 缺失/损坏被 fail-closed 剪枝
    /// ——「分流规则整段没了」轴。控制流位置可诚实断言（剪枝点直接交回悬空 tag 清单，见
    /// `RouteConfigOutcome::pruned_rule_set_tags`），非猜 message；**资源齐全时该清单恒空 ⇒ 不发 = 零噪音**。
    ///
    /// 非终态（核确在跑，只是分流退化），同走 `set_nonfatal_error`。渲染端据此引导去「规则资源」页下载。
    pub const RULE_RESOURCES_MISSING: &str = "RULE_RESOURCES_MISSING";
    /// **TUN 提权引导被用户取消**：起核汇流点的 helper 引导门弹出后用户选了「取消」——「用户明确
    /// 拒绝」轴。控制流位置可诚实断言（门直接收到 [`HelperGateDecision::Abort`](super::HelperGateDecision)），
    /// 非猜 message。
    ///
    /// **与 [`HELPER_NOT_INSTALLED`] 的分工（别合并）**：后者 = 「没装、也没能装上」→ 用户下一步是
    /// **去装**（可操作引导指向「设置 › Helper」）；本码 = 「用户刚刚亲口说了不装」→ 下一步是**什么都
    /// 不做**，再催一遍等于无视用户的选择。文案与告警等级都不同，合并会把两条相反的指引冲成一条。
    ///
    /// 终态（核未起，走 [`set_error`](super::ProxyRuntime::set_error)）。
    /// [`is_unrecoverable_restart_error`](super::is_unrecoverable_restart_error) 按**本码本身**判终态
    /// （用户取消不是瞬时故障，崩溃自愈重试它 = 无视用户刚做出的选择）。注意别退回「按码的字面量在
    /// message 里搜关键字」——实际落进错误的是中文文案 [`HELPER_GATE_ABORTED_MSG`](super::HELPER_GATE_ABORTED_MSG)，
    /// 搜 `"helper_gate_aborted"` 恒不命中。
    pub const HELPER_GATE_ABORTED: &str = "HELPER_GATE_ABORTED";
    /// **TUN 出口未夺到**：TUN 模式起核就绪后，post-flight 出口归属判定发现「本应走代理的公网目的」的
    /// 出口接口 grace 内始终未从 baseline 切走（其他 VPN 占着默认路由 / 我方路由装失败）——「假报已连接」轴。
    ///
    /// 控制流位置可诚实断言（[`verify_tun_route_captured`](super::ProxyRuntime::verify_tun_route_captured)
    /// 直接观测到出口自始至终 == baseline），非猜 message。**终态硬闸**（设计 D1）：核已就绪但流量抢不到
    /// 我方 utun，标 connected 是虚报，故 `kill_core` + [`set_error`](super::ProxyRuntime::set_error) 拒绝
    /// 标 running（设计 §4.2 方向①后验；`polaris-tun-conflict-detect-design-2026-07-22.md`）。
    pub const TUN_ROUTE_NOT_CAPTURED: &str = "TUN_ROUTE_NOT_CAPTURED";
    /// **#327 TUN 网卡从未建出来**：TUN@Windows 起核就绪后，逐腿正向验证在整个重试预算内**一次**都没
    /// 枚举到本次配置的 wintun 适配器 —— 「假报已连接」轴的另一半。
    ///
    /// 控制流位置可诚实断言（[`probe_tun_adapter_present`](super::ProxyRuntime::probe_tun_adapter_present)
    /// 经 `GetAdaptersAddresses` 直接枚举到「这张网卡不在」），非猜 message。
    ///
    /// **与 [`TUN_ROUTE_NOT_CAPTURED`] 的分工（别合并）**：那条是「网卡建出来了、但默认路由被别人占着」
    /// → 用户下一步是**断开另一个 VPN**；本码是「网卡压根没建出来」→ 用户下一步是**查 wintun 驱动是否
    /// 被安全软件拦截 / 重启**。判据来源也相反：那条靠路由归属差分（间接，且他方 VPN 一撤就自愈），
    /// 本码靠适配器存在性正向枚举（直接）。合并等于把两条相反的可操作指引冲成一句谁也用不上的话。
    ///
    /// 终态（本腿判失败即 `kill_core` 并计入重试预算，预算耗尽后走
    /// [`set_error`](super::ProxyRuntime::set_error)）。
    pub const TUN_ADAPTER_MISSING: &str = "TUN_ADAPTER_MISSING";
    /// **#332 TUN 地址无法分配**：核自己的 FATAL 行指明失败发生在「给 TUN 网卡装地址」这一步
    /// （地址被残留网卡/他方 VPN 占用，或系统拒绝分配）——「真因不上屏」轴。
    ///
    /// 控制流位置可诚实断言：判据是**核 stderr 的 FATAL 行内容**，经
    /// [`classify_core_fatal_line`](super::classify_core_fatal_line) 匹配 sing-box/sing-tun 的**源码字面量**
    /// （`configure tun interface` + `set ipv4/ipv6 address` / `add address`），不是猜我方 message 的关键字。
    /// 取证与匹配面（含为什么**不**匹配 errno 文案）见该函数文档。
    ///
    /// **为什么值得一个专属码**：重试预算耗尽后用户此前只看到「sing-box 起核超时/启动期退出」这种
    /// 与现场无关的话，而真正可操作的信息（地址被占了、去断开另一个 VPN 或重启清残留网卡）明明就写在
    /// 核吐出来的那一行里，只是没人读它。
    ///
    /// 终态（核已自行退出，走 [`set_error`](super::ProxyRuntime::set_error)）。
    pub const TUN_ADDRESS_UNAVAILABLE: &str = "TUN_ADDRESS_UNAVAILABLE";
}

/// [`code::HELPER_NOT_INSTALLED`] 的用户可见兜底文案（zh）。command 前置拦截与 runtime preflight
/// 共用同一串 → 「点连接」与「托盘/自动连接」两路给出一致提示。渲染端另有 i18n key
/// (`errors.helperNotInstalled*`) 覆写多语，此常量为无 emitter / 极早期失败时的兜底。
pub const HELPER_NOT_INSTALLED_MSG: &str =
    "TUN 模式需要提权 helper，但 helper 尚未安装。请到「设置 › Helper」安装后重试。";

/// [`code::HELPER_GATE_ABORTED`] 的用户可见兜底文案（zh）。渲染端另有 i18n key
/// (`errors.helperGateAborted`) 覆写多语，此常量为无 emitter / 极早期失败时的兜底。
pub const HELPER_GATE_ABORTED_MSG: &str = "已取消安装提权助手，本次未启动 TUN 模式代理。";

/// [`code::TUN_ROUTE_NOT_CAPTURED`] 的用户可见兜底文案（zh）。渲染端另有 i18n key
/// (`errors.tunRouteNotCaptured`) 覆写多语，此常量为无 emitter / 极早期失败时的兜底。
pub const TUN_ROUTE_NOT_CAPTURED_MSG: &str = "检测到其他 VPN 占用默认路由，请先断开后重试。";

/// [`code::TUN_ADAPTER_MISSING`] 的用户可见兜底文案（zh；渲染端另有 i18n 键 `errors.tunAdapterMissing`，
/// 恒走三段式第 1 段，本串只在 Rust 单独出声的路径上兜底）。
///
/// 措辞刻意**不**提「其他 VPN」—— 那是 [`TUN_ROUTE_NOT_CAPTURED_MSG`] 的场景。本码的现场是网卡根本没
/// 建出来，最常见成因是 wintun 驱动被安全软件拦/驱动没装上/上一张网卡卡在半释放态。
pub const TUN_ADAPTER_MISSING_MSG: &str =
    "TUN 虚拟网卡未能创建，请检查 wintun 驱动是否被安全软件拦截，或重启系统后重试。";

/// [`code::TUN_ADDRESS_UNAVAILABLE`] 的用户可见兜底文案（zh；渲染端另有 i18n 键
/// `errors.tunAddressUnavailable`）。
///
/// **不逐字转述核的原话**：那一行的尾巴是 OS 的 errno 文案（Windows 上经 `FormatMessage` 出来，
/// 是**系统语言**的 —— 中文系统上是「对象已存在。」），把它拼进面向用户的句子只会得到一句半英半中、
/// 且在不同机器上长得不一样的话。码负责分类、文案负责指路，原始行仍完整落在日志里供导出诊断。
pub const TUN_ADDRESS_UNAVAILABLE_MSG: &str =
    "TUN 虚拟网卡地址无法分配，可能被残留网卡或其他 VPN 占用。请断开其他 VPN，或重启系统后重试。";

/// TUN 出口夺取 post-flight 的 grace 探测次数（复用 ~4s 收敛窗，`route -n get` 每次成本极低）。
/// 与 [`TUN_ROUTE_POLL_INTERVAL`] 相乘 ≈ grace 窗口。**真机门**：sing-box 装路由到出口切换的真实耗时
/// 须 macOS 实测校准（设计 §6），此值为首版保守取。
const TUN_ROUTE_GRACE_POLLS: usize = 8;

/// TUN 出口夺取 post-flight 相邻两次探测间隔。8 × 500ms ≈ 3.5s grace（末次不 sleep）。
const TUN_ROUTE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// TUN 出口夺取硬闸是否适用于本模式。
///
/// **仅 TUN 模式适用**：TUN 装内核 tun + `auto_route` 捕获全部流量 → 成功接管必然把「应走代理的公网
/// 目的」的出口切到我方 utun；systemProxy/manual **不接管 tun**，出口恒在物理网卡 → baseline 差分永不
/// 成立，设闸必误判（假阳性拦掉正常起核）。故这两类列 caveat 不闸（设计 §4.7 分流行）。
fn tun_route_gate_applies(mode: ProxyModeType) -> bool {
    mode.is_tun()
}

/// 查询 TUN 接管判据使用的当前出口接口身份。
///
/// macOS/Linux 沿用 [`polaris_system_integration::route_ops::SystemRouteOps`] 的 `route`/`ip` 查询；
/// Windows 复用 helper crate 已有的 `windows-sys` + IP Helper API，直接取 best-interface index。
/// 旧实现每次都冷启 PowerShell `Find-NetRoute`，真机单次约 1.3–1.7s，而 TUN 健康启动必查起核前/后
/// 两次，单这条诊断链就占约 3s。接口索引是内核稳定身份，且本判据只比较前后是否变化，比本地化的
/// `InterfaceAlias` 更窄、更可靠。
fn tun_exit_interface_for_probe() -> Result<Option<String>, SystemIntegrationError> {
    #[cfg(target_os = "windows")]
    {
        let ip = ROUTE_PROBE_IP
            .parse::<std::net::Ipv4Addr>()
            .map_err(|e| SystemIntegrationError::route(e.to_string()))?;
        polaris_helper::platform::windows::wintun::best_route_interface_index(ip)
            .map(|index| Some(format!("ifindex:{index}")))
            .map_err(|e| SystemIntegrationError::route(e.to_string()))
    }
    #[cfg(not(target_os = "windows"))]
    {
        polaris_system_integration::production_route_ops().exit_interface_for(ROUTE_PROBE_IP)
    }
}

/// 起核/重启失败的**类型化错误**：用户可见消息 + 本次失败**自己的**结构化码（[`code`] 常量之一）。
///
/// **为什么必须让错误自带码，而不是让命令层回读 `status().error_code`**（根因）：
/// [`ProxyRuntime::set_error`] 只覆盖一部分失败腿（见其文档「不覆盖的腿及理由」：config 生成 / 写盘 /
/// spawn 前的解析失败一律不经它）。命令层若回读全局状态，拿到的可能是**上一次**失败留下的陈旧码 ——
/// 全局 `error_code` 只有 `stop()` 会清，而「门弹出 → 用户取消」这条路径**根本不经过 stop**。
/// 实际后果：取消后 `HELPER_GATE_ABORTED` 粘在全局，用户装好 helper 再点连接、这次栽在「配置生成失败」
/// 腿上，命令层却把它贴上 `HELPER_GATE_ABORTED` 回给渲染端 → `HomeScreen` 命中「用户取消」分支，弹
/// 中性 info、跳过 `setConnectError`，**真实错误被整条吞掉**。
///
/// 结果与来源在此重新耦合：码随**这一次**的 Err 值一起出栈，不存在「读到别人的码」的物理可能。
/// `code: None` = 本腿没有可诚实断言的分类（不是「忘了填」），命令层照实回落无码错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartError {
    /// 用户可见消息（等价于此前的裸 `String` 错误，`Display`/`Into<String>` 均返回它）。
    pub message: String,
    /// 本次失败的结构化码（[`code`] 模块常量）。`None` = 无可诚实断言的分类。
    pub code: Option<&'static str>,
}

impl StartError {
    /// 带码构造（调用点须与 [`ProxyRuntime::set_error`] 落的码逐字一致：同一次失败对渲染端与对
    /// `event:proxyError` 订阅者必须是同一个分类，两处分叉 = 又一个「结果与来源解耦」）。
    fn coded(message: impl Into<String>, code: &'static str) -> Self {
        Self {
            message: message.into(),
            code: Some(code),
        }
    }
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StartError {}

/// 无码腿的零成本升格：`start_inner` 内既有的 `.map_err(|e| format!(...))?` 经此自动转型，
/// 不必逐条改写（它们本就没有码，`None` 是**诚实**的默认，不是丢信息）。
impl From<String> for StartError {
    fn from(message: String) -> Self {
        Self {
            message,
            code: None,
        }
    }
}

/// 让 `ApiResponse::err(e)`（`impl Into<String>`）与既有 `format!("{e}")` 调用方零改动继续工作。
impl From<StartError> for String {
    fn from(e: StartError) -> Self {
        e.message
    }
}

/// **待应用差集**（前端契约 `PendingNodeChanges`，camelCase 单词字段无需 rename）。
///
/// pull（`proxy:getPendingChanges`）与 push（`event:proxyPendingChanges`）**返回同一个结构**——
/// 没有适配层，两路同构是类型级事实而非靠测试维持（设计 SoT §2.3.2 / T2-7）。
///
/// 三字段的语义（SoT §2.3.1，旧契约 `{added, updated, deleted}` 已废，理由见 Q6）：
///
/// | 字段 | 定义 | 为什么不是旧的那个 |
/// |---|---|---|
/// | `added` | `new_ids − old_ids`：磁盘 config 有、起核快照无 = 未入运行核的新节点 | 语义本就正确，原样保留 |
/// | `modified` | 两侧都有、但 [`modified_fingerprint`]（**全维**）不等 = 核里跑的已不是当前配置 | 旧的 `updated` 是 `old ∩ new` = **全部存活 id**，与「改没改过」无关；id-only diff 在原理上就测不出「改」。修语义 = 换实现，那就该换名字 |
/// | `removed` | `old_ids − new_ids`：起核快照有、磁盘 config 无 = 已删但运行核仍持有 | 原 `deleted` 改名。旧字段语义正确但前端从不消费，通道先接好；U-2（Defer 腿是否扩到「未引用节点的增/改/删均 defer」）未拍板前它多为瞬态 |
///
/// `modified` 与测速 dirty 集的关系是 **`dirty ⊆ modified`**（全维 ⊇ 5 维），不是相等 ——
/// 二者回答的是两个问题，见 [`node_fingerprints`](crate::runtime::node_fingerprints) 模块文档。
/// 这条包含关系正是「测速说 dirty、bar 上却没有那个节点」在结构上不可能再发生的保证。
///
/// [`modified_fingerprint`]: crate::runtime::node_fingerprints::modified_fingerprint
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingChangesSummary {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
    /// 本次运行核起来之后，是否有被「保存不重启」降级、因而**没进核**的结构性变更。
    ///
    /// 三个节点数组回答不了这件事：`mixedPort` / TUN / DNS 这类改动一个节点都不动，
    /// 差集恒空却确实需要重启才生效。少了这一位，「保存」在条上就是完全无痕的
    /// —— 与本仓刚收口的「第四类重启」同一种静默。
    ///
    /// 真值来源是 `switch_mode` 的记账（`ProxyRuntime::restart_deferred`），不是现算的 norm 对比
    /// （后者在 kind=rules 热切后恒真，理由见该字段注释）。
    pub restart_deferred: bool,
}

/// `event:proxyLifecycle` 的载荷：**这一次核起停尝试的真实结局**。
///
/// # 三个 phase 的判据（都是可诚实断言的控制流位置，不猜）
///
/// - `ready` —— [`ProxyRuntime::start_inner`] 就绪腿（核已就绪、`startup_snapshot` 已换新）。
/// - `stopped` —— [`ProxyRuntime::stop_inner`] 拆除腿（`startup_snapshot` 已清）。
/// - `failed` —— [`ProxyRuntime::start`] 包装的 `Err` 腿（**全部**起核入口的唯一汇流点）。
///
/// # 为什么载荷里**没有** pid / 起始时刻
///
/// 那两个的单一真值是 [`ProxyStatus`]（`proxy:getStatus`）。塞进事件载荷等于再造一份镜像，
/// 而这类镜像的失效方式恰恰是**静默**的（同 `ProxyErrorEmitter::privacy_mode` 头注那段因果）。
/// 故本载荷只带「结局」这一位判据，pid / 已运行时长由订阅方照既有范式回拉一次
/// （`App.tsx` 收到即 `refreshProxyStatus()`，与它对 `proxyStarted` 的做法逐字一致）。
/// 代价是每次跃迁多一次**本机** IPC。
///
/// `error_code` / `message` 仅 `failed` 腿非空，且与 [`ProxyRuntime::set_error`] 落的码**同源**
/// （都取自 [`StartError`]）—— 同一次失败对 `event:proxyError` 与本通道必须是同一个分类。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyLifecycleEvent {
    pub phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ProxyLifecycleEvent {
    /// 核已就绪（无失败信息）。
    fn ready() -> Self {
        Self {
            phase: "ready",
            error_code: None,
            message: None,
        }
    }

    /// 核已停（无失败信息）。
    fn stopped() -> Self {
        Self {
            phase: "stopped",
            error_code: None,
            message: None,
        }
    }

    /// 本次起核失败，带上可诚实断言的分类与用户可见文案。
    fn failed(err: &StartError) -> Self {
        Self {
            phase: "failed",
            error_code: err.code.map(str::to_string),
            message: Some(err.message.clone()),
        }
    }
}

/// `config:classifyStaged` 的返回体（spec §2.3.4）：候选配置若落盘会走哪条腿。
///
/// `decision` 用 `&'static str` 而非枚举：它是**前端契约的字面量联合**
/// （`'hotSwitch' | 'noOp' | 'defer' | 'restart'`），派生 `Serialize` 的枚举会引入 tag 重命名这层
/// 无谓的间接。四个取值由 [`ProxyRuntime::classify_staged`] 单点产生，跨语言一致性由
/// `ui/src/contracts/staged-classification.test.ts` 从本文件解析后锁死。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedClassification {
    pub decision: &'static str,
    /// 恒等式：`restart_required == decision ∈ {defer, restart}`。
    /// 它不是第二个判据，是 `decision` 的投影 —— 前端只想问「要不要断流」时不必自己记住哪几腿算。
    pub restart_required: bool,
}

/// [`ProxyRuntime::classify_switch`] 的产物：判定结果，不含任何执行动作。
///
/// 四个变体一一对应 `switch_mode` 里**除 lifecycle 忙态之外**的全部早退腿 + 正式决策。
/// 变体本身携带执行侧需要的载荷（`new_cfg`），使执行侧无需重新解析一遍配置。
enum ClassifiedSwitch {
    /// 核未运行：无核可切，配置留给下次 start 生成。
    NotRunning,
    /// 与运行核当前配置逐字节全等（键序无关）：什么都不用做。
    Unchanged,
    /// 判不出来 → 保守重启。载荷是**给日志用的原因**，不参与判定。
    Fallback(&'static str),
    /// 正式决策（switch-engine `decide` 的产物）+ 已解析的新配置。
    Decided {
        decision: SwitchDecision,
        /// `Box` 是因为 `UserConfig` 远大于其余变体，不装箱会把整个枚举撑到它的大小
        /// （clippy `large_enum_variant`）。
        new_cfg: Box<UserConfig>,
    },
}

/// `event:proxyError` 发射抽象（同 `tailscale_login_core::AuthUrlEmitter` 范式）。
///
/// **为什么是 trait 而非直接持 `AppHandle`**：崩溃自愈跑在后台 task（无 command 上下文、无人 await），
/// 而 `AppHandle` 只在 Tauri `setup` 之后才有 → 运行时必须能「先构造、后接线」。trait 同时让单测能
/// 捕获发射记录断言「这条失败腿真发了事件」——§K7.1 的教训：光测函数、光测失败都不够，要测**组合路径**。
/// **名字为何仍是 `...ErrorEmitter` 而不含后加的两个通道**：接线点在 `main.rs`
/// （`set_error_emitter(Box::new(AppHandleProxyErrorEmitter{..}))`），改名要动 `main.rs`——本批次
/// 不碰它。语义上它已是「ProxyRuntime 的事件出口」，重命名留作纯机械的后续项。
pub trait ProxyErrorEmitter: Send + Sync {
    /// 发射一条代理错误事件（payload 对齐前端 `ProxyErrorEvent`）。
    fn emit_proxy_error(&self, message: &str, error_code: &str);

    /// 发射启动 gate 剔除的非法节点（payload = `InvalidNodeInfo[]`）。
    ///
    /// **空数组不是「没事发生」**：前端据此清陈旧标灰（上次起核剔了、本次没剔 → 必须让灰掉的节点复原），
    /// 故每次起核都发，调用方不得自行短路空集。
    fn emit_invalid_nodes(&self, nodes: &[InvalidNode]);

    /// 发射「TUN 启动后检测到无 marker 的系统代理残留」提示（payload = `{proxy}`）。
    fn emit_system_proxy_residual(&self, proxy: &str);

    /// **A3**：发射一条 Tailscale 端点状态（`event:tailscaleStatus`，逐 endpoint 一条，payload =
    /// 前端 `TailscaleStatusEvent`）。由 STATUS relay 每收一帧对每个在册端点各发一次。
    ///
    /// 未接线（单测 / setup 前）→ relay 侧 `error_emitter.get()` 取不到即静默跳过；本方法只负责「有 emitter
    /// 时怎么发」。之所以复用本 trait（而非新加一个 emitter + main.rs 接线点）：`AppHandleProxyErrorEmitter`
    /// 已持 `AppHandle`、已在 `main.rs` setup 期 `set_error_emitter` 一次接线，扩一个方法**无需动 main.rs**
    /// （本批禁区）；语义上它本就是「ProxyRuntime 的事件出口」（见 trait 头注）。
    fn emit_tailscale_status(&self, event: &TailscaleStatusEvent);

    /// **A4**：发射「组网登录期出口让位」态变（`event:meshLoginFallback`，payload =
    /// `{engaged, serverName?}`）。engage（进入让位）/ disengage（就绪切回 / 关开关 / 停核复位）各发一次。
    ///
    /// 复用本 trait 同 [`emit_tailscale_status`] 的理由：`AppHandleProxyErrorEmitter` 已持 `AppHandle`、
    /// 已在 `main.rs` setup 一次接线，扩方法**无需动 main.rs**（本批禁区）。
    fn emit_mesh_login_fallback(&self, engaged: bool, server_name: Option<&str>);

    /// **C3**：发射「自动换节点成功」通知（`event:autoNodeSwitched`，payload = 前端
    /// `{ reason, newServerName, latency }`）。由自动换节点心跳在热切/重启成功后发一次。
    ///
    /// 复用本 trait 同 [`emit_tailscale_status`] / [`emit_mesh_login_fallback`] 的理由：
    /// `AppHandleProxyErrorEmitter` 已持 `AppHandle`、已在 `main.rs` setup 一次接线，扩方法**无需动
    /// main.rs**（本批禁区）；语义上它本就是「ProxyRuntime 的事件出口」（见 trait 头注）。
    fn emit_auto_node_switched(&self, payload: &AutoNodeSwitchedPayload);

    /// **unlock（核 start/stop 缓存失效）**：核起停即出口隧道换了一次 → 解锁快照必须失效，否则 30min TTL
    /// 内会复用停核前的陈旧解锁快照（对齐 上游 `ProxyManager` start/stop → `unlockService.invalidate()`）。
    ///
    /// 递增 epoch（作废在飞轮）+ 清缓存 + 广播 `EVENT_UNLOCK_INVALIDATED{running,exitBlocked}`。`running`
    /// 带核真态（start=true / stop=false）供渲染端决定「显检测中 vs 复位 idle」。
    ///
    /// 复用本 trait 同 [`emit_tailscale_status`] / [`emit_auto_node_switched`] 的理由：
    /// `AppHandleProxyErrorEmitter` 已持 `AppHandle`、已在 `main.rs` setup 一次接线，扩方法**无需动
    /// main.rs**（本批禁区）。`UnlockRuntime` 经 `AppHandle` 的 `State<AppRuntime>` 取（生产接线点，
    /// 单测 emitter 记录参数即可、不触 Tauri）。
    fn invalidate_unlock(&self, running: bool, exit_blocked: bool);

    /// **出口 IP / 延迟自动重探排程**（移植 上游 `IpInfoService` 的事件驱动触发表；上游 **无周期轮询**，
    /// 本腿同样纯事件驱动）。核起停 / 热切 = 出口换了一次 ⇒ 状态栏那格出口 IP、以及它下游的伴测延迟
    /// 都必须重探，否则要么显示上一个出口的陈旧值、要么（冷启动）恒 `—` 直到用户亲手点「网络检测」。
    ///
    /// `running` = 事件语义（起核 / 热切 = true，停核 = false），实现据此决定是否等选路收敛
    /// （上游 `whenSelectorSettled(4000)`）。**与 [`invalidate_unlock`](Self::invalidate_unlock) 同三点触发
    /// 但不合并进它**：那条是「解锁快照作废」，这条是「出口 IP 重探」，两件事的失效语义、下游、延迟策略
    /// 都不同；合成一个方法会让日后任一侧改触发条件时误伤另一侧。
    ///
    /// 复用本 trait 同 [`emit_tailscale_status`](Self::emit_tailscale_status) 等的理由：
    /// `AppHandleProxyErrorEmitter` 已持 `AppHandle`、已在 `main.rs` setup 一次接线，扩方法**无需动 main.rs**。
    fn schedule_exit_ip_refresh(&self, running: bool);

    /// **R2 出口无效直判终态**（移植 上游 `IpInfoService.markProxyBlocked`）：选中 TS 出口被 API 直判
    /// 无效（未选出口设备 / exit peer 离线 / 在线但未广告出口）时，**不探测**直接把出口 IP 快照落成
    /// 「出口无效」终态并广播 —— 探测在这种形态下必然打空转（重试预算 20s 全耗尽后仍是 null），
    /// 用户看到的是「一直在检测」而不是「出口无效」。
    ///
    /// `reason` = `ui/src/contracts/types/runtime.ts` 的 `ProxyExitBlock` 值域
    /// （`ts-needs-auth` / `ts-no-exit-device` / `ts-exit-device-offline` / `ts-exit-not-advertised`），由
    /// [`ts_exit_block_reason`] 从纯谓词 `TsExitWarning` 投影而来（值域单一真值，不在此处重复拼串）。
    ///
    /// **与 [`schedule_exit_ip_refresh`](Self::schedule_exit_ip_refresh) 是同一物理事实的两条互斥出口**：
    /// 出口换了 ⇒ 要么「重探」（出口有效，值待测），要么「落无效终态」（出口已知无效，不必测）。
    /// `exit_ip_wiring_guard` 因此把两者都算作合法的「出口 IP 腿」。
    ///
    /// 复用本 trait 的理由同 [`invalidate_unlock`](Self::invalidate_unlock)（emitter 已持 `AppHandle`，
    /// 扩方法无需动 `main.rs`）。
    fn mark_exit_blocked(&self, reason: &str);

    /// **R2 待应用差集 PUSH**：发一条差集摘要（`event:proxyPendingChanges`，payload = 前端 `{added, modified}`）。
    /// 由 [`switch_mode`](ProxyRuntime::switch_mode) 落盘后单点推，前端据此渲染 Home 待应用操作条（「N 项待应用」
    /// +「立即应用」）。契约适配依据见 [`PendingChangesSummary`]。
    ///
    /// 复用本 trait 同 [`emit_auto_node_switched`](Self::emit_auto_node_switched) 等的理由：
    /// `AppHandleProxyErrorEmitter` 已持 `AppHandle`、已在 `main.rs` setup 一次接线，扩方法**无需动 main.rs**
    /// （本批禁区）。
    fn emit_pending_changes(&self, summary: &PendingChangesSummary);

    /// **runtime 生命周期结局 PUSH**（`event:proxyLifecycle`，载荷 [`ProxyLifecycleEvent`]）。
    ///
    /// 与 [`emit_pending_changes`](Self::emit_pending_changes) 是**同刻同点的一对**：那条说
    /// 「差集变成什么了」，这条说「核这一次到底起来没起来」。前者判不了后者 —— 起核**失败**时
    /// `startup_snapshot` 同样是 `None`、差集同样为空，拿「差集变空」当成功信号会把失败误报成成功。
    ///
    /// 复用本 trait 的理由同 [`emit_pending_changes`](Self::emit_pending_changes)：
    /// `AppHandleProxyErrorEmitter` 已持 `AppHandle`、已在 `main.rs` setup 一次接线，扩方法无需动 main.rs。
    fn emit_lifecycle(&self, event: &ProxyLifecycleEvent);

    /// **TUN 提权引导门**（移植 上游 `promptHelperGate`，`src/main/index.ts:370-500`）：TUN 起核前
    /// helper 不可用时，**同步**弹一次原生对话框问用户；用户确认 → 在本调用内**就地**执行授权安装
    /// （macOS `SMAppService` / Windows UAC / Linux `pkexec`，各弹一次系统授权框），返回
    /// [`HelperGateDecision::Proceed`] 让起核**原地继续**；用户取消 → [`HelperGateDecision::Abort`]。
    ///
    /// **为什么安装动作在 emitter 内、而不是让 runtime 拿着决策自己去装**：安装要经
    /// `AppRuntime::helper()`，而 runtime 层持有的是 `Arc<HelperRuntime>` —— 两者是同一个实例，
    /// 本可任选。选这里是因为「弹框 → 授权 → 轮询就绪」是**一段不可分割的同步交互**（中途返回
    /// 给异步调用方再回调，会在两次系统弹框之间插入一个可被 lifecycle 抢占的缝）。
    ///
    /// **同步签名**：`blocking_show` 与 `install()`（osascript 可阻塞 30s+）都是阻塞调用，调用方
    /// [`ProxyRuntime::run_helper_gate`] 负责在 `spawn_blocking` 里调它，绝不阻塞 async runtime，
    /// 也绝不在 Tauri 主线程上调（`blocking_show` 在主线程会死锁）。
    ///
    /// 复用本 trait 同 [`emit_tailscale_status`](Self::emit_tailscale_status) 等的理由：
    /// `AppHandleProxyErrorEmitter` 已持 `AppHandle`、已在 `main.rs` setup 一次接线，扩方法**无需动
    /// main.rs**（本批禁区）。
    ///
    /// `status` = 弹框时刻的 helper 快照（供文案分流「安装」vs「修复」）。
    fn prompt_helper_gate(&self, status: &HelperStatusSnapshot) -> HelperGateDecision;

    /// **B1 隐私模式活态**（`generate_deps` 注入 `GenerateConfigDeps::privacy_mode` 用）。
    ///
    /// # 为什么读它要经 emitter，而不是 `ProxyRuntime` 自己存一份
    ///
    /// 隐私模式的**单一真值**是 `commands::config` 的进程状态机（`PRIVACY_MODE: AtomicBool`，由
    /// `config:setPrivacyMode` 翻转 + emit `EVENT_ENTER/EXIT_PRIVACY_MODE`）。若在 runtime 侧再存一份
    /// 镜像（哪怕靠事件同步），就有了两个真相源 —— 而这条轴的失效方式恰恰是**静默**的：镜像漏更新时
    /// 隐私模式看起来开着、核却继续按用户级别把域名写进 helper stderr，没有任何可见症状。故读取一律
    /// 回到那一份 flag。`AppHandleProxyErrorEmitter` 已持 `AppHandle`（`main.rs` setup 一次接线，扩方法
    /// **无需动 main.rs** —— 同 [`invalidate_unlock`](Self::invalidate_unlock) 的既定手法）。
    ///
    /// 未接线（单测 / setup 前极早期）→ 实现方返 `false`：**保守方向正确**——不抬级 = 与本方法接线前
    /// 的行为逐字节一致，绝不会因为「读不到 flag」就误把用户的 debug 日志静默降级掉。
    fn privacy_mode(&self) -> bool;
}

/// [`ProxyErrorEmitter::prompt_helper_gate`] 的用户决策（移植 上游 `'proceed' | 'abort'`）。
///
/// **刻意只有两值**：上游的第三个选项「本次用系统授权启动」对应 osascript/UAC/setcap 回退路径，
/// Polaris 尚未移植该回退（见交付说明）。给一个点了没用的按钮比不给更糟 —— 值域忠实反映**本仓真有的
/// 能力**，而不是照抄上游的按钮数。
/// `Default` = [`Abort`](HelperGateDecision::Abort)：任何「没能真问到用户」的路径都必须落在
/// **不装、不起核**这一侧。默认 `Proceed` 会让缺省值悄悄替用户按下「安装」（弹系统授权框）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HelperGateDecision {
    /// 用户确认 → 已就地尝试授权安装。**不代表安装成功**：成败由调用方复检 helper 状态裁定
    /// （安装失败 → 仍落 [`code::HELPER_NOT_INSTALLED`]，不冒充成功继续 spawn）。
    Proceed,
    /// 用户取消 → 干净终态 [`code::HELPER_GATE_ABORTED`]，本次不起核。
    #[default]
    Abort,
}

/// 出口 IP 重探的延迟策略（[`ProxyErrorEmitter::schedule_exit_ip_refresh`] 的全部决策，抽成纯函数供单测）。
///
/// - 起核 / 热切（`running=true`）→ 等选路收敛（上游 `whenSelectorSettled(4000)`）：此刻 selector 的 PUT
///   才刚落，出口隧道未必已能跑流量，立刻探会打到旧出口或直接失败。
/// - 停核（`running=false`）→ 出口是**确定性消失**而非切换，没有「收敛」这回事，零延迟直接重探直连出口；
///   白等 4s 只会让状态栏多显示 4s 的陈旧代理出口 IP。
#[must_use]
fn exit_ip_refresh_delay_ms(running: bool) -> u64 {
    if running {
        crate::commands::misc::IPINFO_SETTLE_DELAY_MS
    } else {
        0
    }
}

/// 生产实现：经 [`AppHandle`] 广播 `event:proxyError`。
pub struct AppHandleProxyErrorEmitter {
    /// Tauri 应用句柄（`setup` 期注入）。
    pub app: tauri::AppHandle,
}

impl ProxyErrorEmitter for AppHandleProxyErrorEmitter {
    fn emit_proxy_error(&self, message: &str, error_code: &str) {
        // payload 逐字段对齐前端 `ProxyErrorEvent`：message 必给（兼容旧渲染端），errorCode 结构化分类。
        // errorParams/code/signal/error 不发 —— 本层没有可诚实填充它们的依据，宁缺勿造。
        crate::events::broadcast(
            &self.app,
            crate::events::channel::EVENT_PROXY_ERROR,
            serde_json::json!({ "message": message, "errorCode": error_code }),
        );
    }

    fn emit_invalid_nodes(&self, nodes: &[InvalidNode]) {
        // 直接发数组（前端 `onInvalidNodes` 签名即 `InvalidNodeInfo[]`，不再套一层对象）。
        crate::events::broadcast(
            &self.app,
            crate::events::channel::EVENT_PROXY_INVALID_NODES,
            nodes,
        );
    }

    fn emit_system_proxy_residual(&self, proxy: &str) {
        crate::events::broadcast(
            &self.app,
            crate::events::channel::EVENT_SYSTEM_PROXY_RESIDUAL,
            serde_json::json!({ "proxy": proxy }),
        );
    }

    fn emit_tailscale_status(&self, event: &TailscaleStatusEvent) {
        // 直接发单条事件（前端 `onTailscaleStatus` 签名即 `TailscaleStatusEvent`，serde camelCase 对齐契约）。
        crate::events::broadcast(
            &self.app,
            crate::events::channel::EVENT_TAILSCALE_STATUS,
            event,
        );
    }

    fn emit_mesh_login_fallback(&self, engaged: bool, server_name: Option<&str>) {
        // payload 对齐前端 `onMeshLoginFallback` 签名 `{engaged, serverName?}`：serverName 缺省则省略键。
        let mut payload = serde_json::json!({ "engaged": engaged });
        if let Some(name) = server_name {
            payload["serverName"] = serde_json::Value::String(name.to_string());
        }
        crate::events::broadcast(
            &self.app,
            crate::events::channel::EVENT_MESH_LOGIN_FALLBACK,
            payload,
        );
    }

    fn emit_auto_node_switched(&self, payload: &AutoNodeSwitchedPayload) {
        // 直接发 payload（前端 `onAutoNodeSwitched` 签名即 `{reason, newServerName, latency}`，
        // serde camelCase 已对齐契约）。
        crate::events::broadcast(
            &self.app,
            crate::events::channel::EVENT_AUTO_NODE_SWITCHED,
            payload,
        );
    }

    fn invalidate_unlock(&self, running: bool, exit_blocked: bool) {
        use tauri::Manager;
        // `UnlockRuntime` 的失效编排（bump epoch + 清缓存 + 广播）在 `AppRuntime.unlock`；经 `AppHandle` 的
        // managed State 取（manage 之后才有 → `try_state`：setup 前极早期失败取不到即静默跳过，绝不 panic）。
        // 广播出口用 unlock 自己的 `BroadcastSink`（持同一 `AppHandle`），事件键/载荷与 command 层一致。
        if let Some(rt) = self.app.try_state::<crate::runtime::AppRuntime>() {
            let sink = crate::runtime::unlock::BroadcastSink::new(&self.app);
            rt.unlock.invalidate(&sink, running, exit_blocked);
        }
    }

    fn schedule_exit_ip_refresh(&self, running: bool) {
        crate::commands::misc::schedule_ipinfo_refresh(
            &self.app,
            exit_ip_refresh_delay_ms(running),
        );
    }

    fn mark_exit_blocked(&self, reason: &str) {
        // 上游 `IpInfoService.markProxyBlocked`：不探测、直接落终态 —— 代理出口清空 + `proxyBlocked`
        // 置原因 + `loading:false`（blocked 与 error 互斥语义：blocked = 已知无效、根本没探）。
        //
        // **经 `commands::misc` 的权威缓存写入腿，而不是就地 broadcast**：`EVENT_IP_INFO_UPDATED` 只喂
        // 订阅方（状态栏），而 `ipinfo:get(peek)` 型消费方（托盘浮层 / 窗口重建水合）**不订阅**、只读
        // `IPINFO_CACHE` —— 只广播不写缓存 ⇒ 那两处继续吐上一次探到的（此刻已知无效的）代理出口 IP。
        // 载荷折叠（含 direct 保留、error 删键）与广播都由那一侧单点收口，此处零重复实现。
        crate::commands::misc::mark_ipinfo_proxy_blocked(&self.app, reason);
    }

    fn privacy_mode(&self) -> bool {
        use tauri::Manager;
        // 直接读单一真值（`commands::config` 的进程状态机），不镜像。`config_get_privacy_mode` 是普通
        // `pub fn`（`#[tauri::command]` 只生成旁路 wrapper，不改函数本身），故可直调。
        // `try_state`：setup 前极早期取不到 → 保守 false（同上方 `invalidate_unlock` 的取态手法）。
        self.app
            .try_state::<crate::runtime::AppRuntime>()
            .and_then(|s| crate::commands::config::config_get_privacy_mode(s).data)
            .unwrap_or(false)
    }

    fn emit_pending_changes(&self, summary: &PendingChangesSummary) {
        // 直接发 payload（前端 `onPendingChanges` 签名即 `{added, modified}`，serde camelCase 已对齐契约）。
        crate::events::broadcast(
            &self.app,
            crate::events::channel::EVENT_PROXY_PENDING_CHANGES,
            summary,
        );
    }

    fn emit_lifecycle(&self, event: &ProxyLifecycleEvent) {
        crate::events::broadcast(
            &self.app,
            crate::events::channel::EVENT_PROXY_LIFECYCLE,
            event,
        );
    }

    fn prompt_helper_gate(&self, status: &HelperStatusSnapshot) -> HelperGateDecision {
        use tauri::Manager;
        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

        // 上游 `promptHelperGate` 首件事：把主窗拉到前台。门可由**托盘切模式 / 启动自动连接 / 去抖
        // 重启**触发，此时主窗常已收进托盘 —— 不拉前台则原生弹框可能出现在用户看不到的层级，表现为
        // 「点了没反应」（正是本次真机反馈的形态之一）。失败不阻断（无窗口时照样弹应用级模态）。
        if let Some(w) = self.app.get_webview_window("main") {
            let _ = w.show();
            let _ = w.set_focus();
        }

        // 文案分流：已装但不可用 = 修复（多为 proto 升级 / 描述符失效），未装 = 安装。
        // **不提供「本次用系统授权启动」**：Polaris 尚无 osascript/UAC/setcap 回退腿，给这个按钮 = 撒谎。
        //
        // 语言从 `config.language` 来（[`crate::i18n::app_lang`]）而**不是**由前端传下来：本门的
        // 发起方包含 `startup_tasks::spawn_auto_connect`（启动 2s 后 Rust 自己调 `proxy_start`）
        // 与托盘原生菜单的 `tray_toggle` —— 两条都没有前端在场，前端手上那份 i18next 递不进来。
        use crate::i18n::{key, t};
        let lang = crate::i18n::app_lang(&self.app);
        let (message, detail, confirm) = if status.installed {
            (
                key::NATIVE_HELPER_REPAIR_TITLE,
                key::NATIVE_HELPER_REPAIR_BODY,
                key::NATIVE_HELPER_REPAIR_CONFIRM,
            )
        } else {
            (
                key::NATIVE_HELPER_INSTALL_TITLE,
                key::NATIVE_HELPER_INSTALL_BODY,
                key::NATIVE_HELPER_INSTALL_CONFIRM,
            )
        };

        let confirmed = self
            .app
            .dialog()
            .message(t(lang, detail))
            .title(t(lang, message))
            .kind(MessageDialogKind::Info)
            .buttons(MessageDialogButtons::OkCancelCustom(
                t(lang, confirm),
                t(lang, key::NATIVE_CANCEL),
            ))
            .blocking_show();
        if !confirmed {
            log::info!("TUN 提权引导：用户取消 → 本次不起核（HELPER_GATE_ABORTED）");
            return HelperGateDecision::Abort;
        }

        // 就地授权安装（上游 `await helperManager.install().catch(() => {})` —— 失败**不抛**：由调用方
        // 复检 helper 状态统一裁定，装不上就落 HELPER_NOT_INSTALLED，绝不在这里替它决定终态）。
        // `HelperRuntime::install` 内部已含「弹一次系统授权 + 装后轮询 daemon 就绪」（上游 第 6 步）。
        match self.app.try_state::<crate::runtime::AppRuntime>() {
            Some(rt) => {
                let r = rt.helper().install();
                if r.success {
                    log::info!("TUN 提权引导：helper 安装成功 → 原地继续起核");
                } else {
                    log::warn!(
                        "TUN 提权引导：helper 安装未成功（{}）→ 交由起核门复检裁定",
                        r.error.as_deref().unwrap_or("未知原因")
                    );
                }
            }
            // setup 前的极早期（AppRuntime 尚未 manage）：装不了，照样返回 Proceed —— 复检会发现
            // helper 仍未装并落 HELPER_NOT_INSTALLED。此处**不得**返回 Abort：用户明明点了「安装」，
            // 报「用户已取消」是伪造用户意图。
            None => log::warn!("TUN 提权引导：AppRuntime 尚未装配 → 无法安装 helper"),
        }
        HelperGateDecision::Proceed
    }
}

/// serde skip_if 助手：pid=0 时省略（对齐 上游 `pid?`）。
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// `skip_serializing_if` 谓词：false 即省略（`ProxyStatus::starting` 的默认态不占线）。
fn is_false(v: &bool) -> bool {
    !*v
}

/// 纯谓词：config 的 `selectedServerId` 是否指向 `servers` 里**真实存在**的节点。
///
/// 1:1 对齐 上游 `AutoSwitchService.runHeartbeat` 的 `config.servers.find(s => s.id === selectedServerId)`
/// 守卫。返回 `false` 的三种形态（自动换节点心跳据此**跳过**本 tick，防 direct 网络抖动误切走）：
/// - 无选中（`selectedServerId` 缺失）；
/// - direct 哨兵（`__direct__` 从不在 `servers` 数组里，故 find 不到 → false）；
/// - 选中节点已被删（订阅刷新 / 手动删）→ 悬挂 id 找不到。
fn selected_server_present(config: &Value) -> bool {
    let Some(selected) = config.get("selectedServerId").and_then(Value::as_str) else {
        return false;
    };
    config
        .get("servers")
        .and_then(Value::as_array)
        .is_some_and(|arr| {
            arr.iter()
                .any(|s| s.get("id").and_then(Value::as_str) == Some(selected))
        })
}

/// 纯谓词：选中 TS 出口的 backendState 是否**刚跃迁到 `Running`**（= 上游 触发表「TS 隧道就绪」）。
///
/// 判**上升沿**而非当前值：STATUS relay 每秒量级推帧，稳态 Running 帧若也算「就绪」，出口 IP 重探就成了
/// 每秒一次的轮询 —— 而本子系统（`commands::misc` 的 ipinfo）的设计前提正是**纯事件驱动、无轮询**。
///
/// 三类不触发，各有理由：
/// - `Running → Running`：稳态，出口没换（这条挡住的就是上面那个轮询退化）；
/// - `None → None` / 任意 → 非 `Running`：选中的不是 TS 节点 / 首帧未到 / 还在登录中，隧道未就绪；
/// - `None → Running`：**触发**。首帧就是 Running（核起时 TS 已登录且 key 未过期）同样意味着「此刻起
///   公网经 TS 出口走」，与 `NeedsLogin → Running` 对用户是同一件事。起核腿那次重探跑在核就绪那一刻，
///   彼时 TS 隧道未必已通 ⇒ 它探到的可能是让位期的直连出口，正需要本触发点纠正。
///
/// `expired` 帧已由 [`Mesh::selected_exit_backend_state`](super::mesh::Mesh::selected_exit_backend_state)
/// 投影成 `"NeedsLogin"`，故此处无需再判过期。
fn ts_exit_became_ready(before: Option<&str>, after: Option<&str>) -> bool {
    after == Some("Running") && before != Some("Running")
}

/// 纯谓词：relay 自留的末态表里各端点是否**全部**已就绪。
///
/// **空表 → false**，且这条是承重的：一帧都没收到正是最该重订阅的时候，若空表算「全就绪」
/// （`Iterator::all` 对空集恒真），停流自愈在最需要它的那一刻恰好不触发。
fn ts_all_running(states: &BTreeMap<String, String>) -> bool {
    !states.is_empty() && states.values().all(|s| s == "Running")
}

/// STATUS 帧的**跃迁**日志：只有某端点 `backendState` 真的变了才打一行，并把新态写回 `last`。
///
/// 为什么不每帧都打：稳态下核按自身节奏推帧，全打就是刷屏（本仓刚为此治过 dns-race 与
/// switchMode 两处）。而跃迁行恰是排查「TS 到底有没有起来 / 停在哪一态」唯一需要的东西 ——
/// 2026-08-02 那次故障里，整条链一行日志都没有，只能靠核日志侧写。
///
/// 幽灵端点（tag 不在 `tag_to_id`）与 [`decode_tailscale_status`] 同口径丢弃：否则日志里会冒出
/// UI 根本不存在的节点，比不打更误导。
fn log_ts_state_transitions(
    update: &polaris_singbox_grpc::daemon::TailscaleStatusUpdate,
    tag_to_id: &BTreeMap<String, String>,
    last: &mut BTreeMap<String, String>,
) {
    for ep in &update.endpoints {
        let Some(id) = tag_to_id.get(&ep.endpoint_tag) else {
            continue;
        };
        if last.get(id).map(String::as_str) == Some(ep.backend_state.as_str()) {
            continue;
        }
        let prev = last
            .insert(id.clone(), ep.backend_state.clone())
            .unwrap_or_else(|| "<无帧>".to_string());
        let ips = ep.self_.as_ref().map_or(0, |s| s.tailscale_i_ps.len());
        let peers: usize = ep.user_groups.iter().map(|g| g.peers.len()).sum();
        log::info!(
            "TS STATUS 跃迁：{}（{id}）{prev} → {}，tailnetIP {ips} 个，peers {peers}",
            ep.endpoint_tag,
            ep.backend_state
        );
    }
}

/// 起核时刻的热切换基准快照（上游 ProxyManager 的三个 `this.*` 运行态字段的合并镜像）。
///
/// **只在起核路径刷新**（上游 :672 注释「仅此起核路径刷新（switchMode 的 defer/no-op 分支不刷）」）——
/// 热切换/defer 腿绝不动它：它描述的是「运行中的核实际起于什么」，而非「用户最新想要什么」。
/// 停核清空（上游 :1386-1388）。
#[derive(Debug, Clone, Default)]
struct SwitchSnapshot {
    /// id → outbound tag（上游 `currentIdToTagMap`，:3480 = `buildIdToTagMap(config.servers)`）。
    id_to_tag: BTreeMap<String, String>,
    /// ruleKey → rule-sel 元数据（上游 `currentRuleTargetMap`，:3607）。
    rule_target: BTreeMap<String, RuleTargetEntry>,
    /// id → **全维**指纹（[`modified_fingerprint`]，上游 `runningServersFingerprint`，:672）。
    ///
    /// 两个消费面，同一个问题的两种问法：
    /// - `switch-engine` 的重启判据（喂 `HotSwitchDeps::running_servers_fingerprint` 与
    ///   `can_skip_restart_for_added_unreferenced`）——「这改动会不会改变生成产物」。
    /// - `pending_changes().modified`——「运行核里跑的还是不是用户当前配置」。
    ///
    /// **与 [`Self::dirty_fingerprints`] 不可合并**：那一张回答「池里那个出口还能不能代表这个节点」，
    /// 是另一个问题，正确粒度本就更粗（改 `name` 要重启、但出口没变，测速值仍准）。
    ///
    /// [`modified_fingerprint`]: crate::runtime::node_fingerprints::modified_fingerprint
    fingerprints: BTreeMap<String, String>,
    /// id → **5 维**指纹（[`dirty_fingerprint`]），测速 dirty 判据的「旧」侧。
    ///
    /// 与 `fingerprints` **同刻同源**（同一份 `user_config`、同一次 `build_switch_snapshot`），只是投影更粗。
    ///
    /// **为什么必须单独存一张而不是复用 `fingerprints`**：`partition_dirty` 的「新」侧
    /// （`speedtest.rs::current_server_fingerprints`）算的是 5 维串；拿全维表当「旧」侧 ⇒ 两种串永不相等
    /// ⇒ 凡在快照里的节点一律判 dirty ⇒ **整个测速波前每次都被免测**。收口前正是这个形态。
    ///
    /// [`dirty_fingerprint`]: crate::runtime::node_fingerprints::dirty_fingerprint
    dirty_fingerprints: BTreeMap<String, String>,
    /// **§15**：运行核的测速探测池端口（`probe-in-k`，起核分配）。空 = 池未注入（分配失败/回滚）。
    /// 与 `running` 同生共死（起核就绪时随本快照置、停核清）→「有池端口 ⟺ 运行核有池」；`server_speed_test`
    /// 据此裁定走「主核 K 槽分波测速」还是回退「仅活跃出口」。`poolPorts[k] ↔ probe-selector-k`（1:1 槽绑定）。
    probe_pool_ports: Vec<u16>,
}

/// **核构建环境快照**（[`ProxyRuntime::core_build_env`] 产出，`runtime::speedtest` 的临时核消费）。
///
/// 三项与 [`GenerateConfigDeps`] 的同名字段同源：临时核用 config-engine 的**同一批**出站构造函数
/// （`build_proxy_outbound` / `build_wireguard_endpoint`），故必须喂同一套 (platform, arch, cronet)，
/// 否则同一个节点在两个核里被构成不同形状的出站，而测速值却被当作可比。
#[derive(Debug, Clone)]
pub struct CoreBuildEnv {
    /// Node 约定的平台 tag（`darwin` / `win32` / `linux`），见 [`platform_tag`]。
    pub platform: String,
    /// `std::env::consts::ARCH`。
    pub arch: String,
    /// libcronet 是否可用（naive 协议的前置条件；macOS 静态编入 → 恒真）。
    pub has_cronet: bool,
}

/// **§15**：主核测速探测池目标（[`ProxyRuntime::speed_probe_targets`] 产出，`server_speed_test` 消费）。
///
/// = 上游 `MainCoreProbe` 的 Polaris 最小投影。`pool_ports[k]`（`probe-in-k` 的 http 代理口）与
/// `probe-selector-k`（第 k 槽）1:1；`id_to_tag` 是运行核 `probe-selector-k` 的成员命名空间——`id ∈ id_to_tag`
/// 即「已入运行核池」（`hasTag`），据此分流「可测（分波热切）」vs「未入池（notInPool，如实缺席）」。
#[derive(Debug, Clone)]
pub struct SpeedProbeTargets {
    /// K 个 `probe-in-k` 的 http 代理端口（`pool_ports[k] ↔ probe-selector-k`）。
    pub pool_ports: Vec<u16>,
    /// 运行核 id → outbound tag（`probe-selector-k` 成员）。
    pub id_to_tag: BTreeMap<String, String>,
    /// **起核那一刻**运行核各节点的 **5 维** dirty 判据指纹
    /// （= `SwitchSnapshot::dirty_fingerprints` 的只读投影，**不是**全维的 `SwitchSnapshot::fingerprints`）。
    ///
    /// 供测速侧做 dirty 波前预筛：把当前配置的指纹与本表逐 id 比对，**不等 ⇒ 该节点的连接参数已改、
    /// 而池里那个出口还是旧的**（测它量到的是旧参数出口的 RTT），据此标脏免测。
    ///
    /// ⚠️ **两侧必须是同一个公式**（[`dirty_fingerprint`]，= `speedtest.rs::current_server_fingerprints`
    /// 用的那一个）。收口前这里带出的是全维表、而「新」侧算的是 5 维串 —— 两种串永不相等
    /// ⇒ 凡在快照里的节点一律判 dirty ⇒ **整个波前恒被免测**。现在两侧同源，该失败模式在结构上消失。
    ///
    /// ⚠️ **不可用 `pending_changes()` 的 `modified` 代替**：那条是**全维**判据（回答「核里跑的还是不是
    /// 当前配置」），比 dirty 粗一档 —— 只改了 `name` 的节点该进 `modified`，但它的出口没变、测速值仍准，
    /// 判它 dirty 拒测是白白不测一个本可测的节点。两条判据的包含关系（dirty ⊆ modified）见
    /// [`node_fingerprints`](crate::runtime::node_fingerprints) 模块文档。
    ///
    /// [`dirty_fingerprint`]: crate::runtime::node_fingerprints::dirty_fingerprint
    pub fingerprints: BTreeMap<String, String>,
}

/// `switch_mode` 的结果（供 command 层 / 测试断言；上游 switchMode 返 void，此处显式化以便可测）。
///
/// **可测性即门的射程**：上游的 switchMode 吞掉了走哪条腿的信息，测试只能从副作用反推；
/// 显式返回让「切节点走了热切腿而非重启腿」成为可直接断言的事实（§K7：门要能看见它守的东西）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchOutcome {
    /// 热切换成功（selector PUT 全成功，核进程未重启）。
    HotSwitched,
    /// 生成无关变更（norm 等价 + 节点未变）→ 零热切零重启。
    NoOp,
    /// 仅新增未引用节点 → 免整核重启（下次被选中/重启时生效）。
    Deferred,
    /// 走去抖重启（结构性变更 / 热切换失败回退 / 热切换不适用）。
    Restarting,
    /// lifecycle 在飞（depth>0）→ 暂存，由 `end()` 排空时重放。
    Pending,
    /// 核未运行 → 仅更新配置引用（下次 start 按新配置生成）。
    NotRunning,
    /// 配置逐字节全等 → 仅更新引用即返回（上游 bug#5：防外化规则写失败时的无限重启循环）。
    Unchanged,
}

/// 系统代理「清理收口」能力——维度7 #8 失败腿的**最小注入面**。
///
/// start 失败腿只需要「若系统代理确由我们设置且仍指向我们（已死的）端口，则清」这一个动作，
/// 故此 trait 只暴露 `ensure_cleared`（三层门控 + 幂等，实现在
/// [`polaris_system_integration::proxy_ops::SystemProxyController::ensure_cleared`]）。
///
/// **为什么是 trait object 而非直接持 `ProdProxyController`**：`ensure_cleared` 真跑会 exec
/// `networksetup`/`gsettings`/`reg`（触碰宿主系统代理），本机绝不可真跑；trait 让测试注入
/// mock 记录「失败腿是否真调到了它」——§K7.1 的教训是「光测 `ensure_cleared` 函数、光测 start 失败
/// 都不够，要测 `start 真失败 → controller 真被调` 这条组合路径」。
///
/// `Send` supertrait：`clear_system_proxy` 在 async 里经 `spawn_blocking` 跨线程持有它。
pub trait SystemProxyClearer: Send {
    /// 系统代理确由我们设置且仍指向我们（已死的）端口 → 清并返回 `true`；否则幂等 no-op 返回 `false`。
    /// **无 marker（fresh start）→ 门控 1 即返，零系统调用**——这正是它能被无脑挂在每个失败腿的前提。
    fn ensure_cleared(&mut self) -> bool;

    /// 检测「不是我们设的」系统代理，返回其 `host:port`（无则 `None`）。**只读不动手**。
    fn detect_foreign_proxy(&self) -> Option<String>;

    /// **A1 启用侧**：把 OS 系统代理指向本地 mixed 入站（`enable` 内部先写 marker 标属主 → set_proxy，
    /// set_proxy 失败则 fail-closed 自回滚 + 清 marker）。实现在
    /// [`SystemProxyController::enable`](polaris_system_integration::proxy_ops::SystemProxyController::enable)。
    /// 错误折成 `String`（不外泄 crate 错误类型，与本 trait 其余方法一致的最小注入面）。
    fn enable_system_proxy(&mut self, req: &ProxyEnableRequest) -> Result<(), String>;

    /// **C1 启动期崩溃恢复**：若上次会话遗留 marker（enable 后未正常 disable，如崩溃/强杀）→ 清/恢复
    /// 残留代理 + 清 marker，返回 `true`；无 marker（正常 fresh start）→ 门控即返、零系统调用、`false`。
    /// 实现在 [`SystemProxyController::recover_from_marker`](polaris_system_integration::proxy_ops::SystemProxyController::recover_from_marker)。
    fn recover_from_marker(&mut self) -> bool;
}

/// 生产控制器（及任意 `Send` 的 mock 装配）直接满足清理收口面。
///
/// trait 本地、`SystemProxyController` 外来 → 本 impl 合法（孤儿规则：本地 trait 可为外来类型实现）。
impl<Ops, Fs> SystemProxyClearer for SystemProxyController<Ops, Fs>
where
    Ops: SystemProxyOps + Send,
    Fs: MarkerFs + Send,
{
    fn ensure_cleared(&mut self) -> bool {
        SystemProxyController::ensure_cleared(self)
    }

    fn detect_foreign_proxy(&self) -> Option<String> {
        SystemProxyController::detect_foreign_proxy(self)
    }

    fn enable_system_proxy(&mut self, req: &ProxyEnableRequest) -> Result<(), String> {
        SystemProxyController::enable(self, req).map_err(|e| e.to_string())
    }

    fn recover_from_marker(&mut self) -> bool {
        SystemProxyController::recover_from_marker(self).is_some()
    }
}

/// **A1 决策（纯函数）**：仅 `systemProxy` 模式需把 OS 系统代理指向本地 mixed 入站。
///
/// - `SystemProxy` → `true`：核只在 mixedPort 上截流量，OS 不设代理则应用一律直连、根本不经核
///   （即便出口选 direct 也表现「没启动」——这正是 A1 要解的最大缺口）。
/// - `Tun` → `false`：TUN 虚拟网卡接管全量流量，不靠系统代理（残留由 `maybe_warn_system_proxy_residual`
///   只提示不动手）。
/// - `Manual` → `false`：用户自管分流，不代设。
fn should_enable_system_proxy(mode: ProxyModeType) -> bool {
    matches!(mode, ProxyModeType::SystemProxy)
}

/// **C6-5 起核路由决策（纯函数）**：是否经提权 helper 起核（而非 [`TokioSpawner`] 直起）。
///
/// 判据 = TUN 模式 **且** 平台有 helper 实现。根因（对齐 上游 `startViaHelper` 门控）：
/// - **TUN 需提权**：mac/Win 建 utun/wintun 需 root/SYSTEM；linux 建 tun 需 CAP_NET_ADMIN（经 helper 的
///   AmbientCaps + setuid 降权拉核）。三平台 TUN 一律经 helper（上游 `isTunMode` → helper）。
/// - **systemProxy/manual 不接管 TUN**：核只在本地端口截流，app 直接 spawn 即可（无需 root）→ [`TokioSpawner`]。
/// - **平台无 helper**（`Platform::Other`）：无 daemon 可连 → 退回直起（best-effort；TUN 在未知平台本就无解）。
///
/// 变异锚点：删 `is_tun()` → 全模式经 helper（systemProxy 也弹提权，回归）；删平台判 → Other 平台起核必失败。
///
/// DESIGN-REVIEW(c6-5-src-tauri-helper-wiring)：`Platform::Other` 的 TUN 判 false → 退回直起（无 helper
/// 可连）；但直起也建不了 TUN——是否该改「Other+TUN→显式报错」由复审裁（R27.1，目标平台仅 mac/win/linux，低风险）。
fn should_start_via_helper(mode: ProxyModeType, platform: Platform) -> bool {
    mode.is_tun() && matches!(platform, Platform::Mac | Platform::Win | Platform::Linux)
}

/// 代理运行时（`State`-managed，单实例）。
///
/// 持有 config / helper / mesh 引用（跨运行时协作：启动需读 config + 可能经 helper 提权 + mesh exit route）。
pub struct ProxyRuntime {
    config: Arc<ConfigManager>,
    /// 提权 helper（C6-5 接线）：TUN 模式经它起停 root/SYSTEM 受管核（见 [`should_start_via_helper`]）。
    helper: Arc<HelperRuntime>,
    /// C5：mesh 出口路由生命周期接线（起核前 snapshot / 就绪+切换 reconcile / 停核 clear / 崩溃 reset /
    /// 出口恢复 reassert）。OS 路由真操作经 `HelperExitRouteOp`（**已全链接线**：mac/win 经 root helper
    /// `route -ifscope`、Linux 自身 `ip rule/route` 独立表 7732）——生产构造 `MeshRuntime::new_with_helper`
    /// 下是真手术（真机门）；测试构造 `MeshRuntime::new` 下 `enabled=false` 诚实 no-op。见 `runtime/mesh.rs`。
    mesh: Arc<MeshRuntime>,
    status: RwLock<ProxyStatus>,
    /// 运行核启动时的配置快照（待应用差集基准，上游 ProxyManager.startupSnapshot）。
    startup_snapshot: RwLock<Option<Value>>,
    /// 生命周期单飞守卫（core-supervisor 既有状态机；起停竞态/世代/pending 全在其中）。
    gate: Arc<LifecycleGate>,
    /// **世代变更唤醒边沿**（起核腿的取消信号）。
    ///
    /// **不是第二个真值源**：谁当权仍然只看 `gate.generation()`，本 [`Notify`] 只负责把「世代已变」
    /// 这一事实**立刻推醒**正在 sleep 的起核腿。没有它，让位检查点只在**迭代边界**生效：用户点停止时
    /// 若本腿正卡在退避 sleep（2s/4s）里，取消要静默等睡满才被发现 —— 真机上「点连接锁死 UI ≈35s、
    /// 启动卡死阶段无法关闭启动过程」的后半截成因。
    ///
    /// [`notify_waiters`](Notify::notify_waiters) **不留 permit**（无等待者时通知即丢），故所有等待点
    /// 一律「注册 → 复查世代 → select」三步（见 [`sleep_unless_superseded_on`]），靠复查覆盖注册前的
    /// bump、靠注册覆盖复查后的 bump，两侧夹住不漏边沿。唯一发信点是
    /// [`bump_generation`](Self::bump_generation)，与世代同点落值 ⇒ 信号与真值不会分叉。
    gen_changed: Arc<Notify>,
    /// 在飞起核腿计数（[`ProxyStatus::starting`] 的读时投影源）。
    ///
    /// 由 [`start`](Self::start) 全程持有（含 `?` 早退——[`InflightGuard`] 的 `Drop` 兜底），故覆盖
    /// 「stale 清扫 → 提权门 → config 生成 → spawn → 就绪等待 → 重试退避」整条起核腿，而不只是
    /// spawn 之后那一段。计数而非布尔：崩溃自愈/去抖重启也直调 `start`，可与用户发起的腿重叠。
    start_inflight: Arc<AtomicU32>,
    /// 去抖重启调度器（switch-engine 既有 timer + 世代守卫，内部复用同一 `gate`）。
    debounced: DebouncedRestart,
    /// sing-box 子进程句柄。std `Mutex`：就绪门的 `is_alive` 是**同步**闭包（`Fn()->bool`），
    /// 必须能在其中即时 `try_wait`；guard 绝不跨 await 持有（否则 !Send 编译即拒）。
    child: Arc<Mutex<Option<Child>>>,
    /// spawn 出的 pid（child 被 stop 取走后仍可用于日志/诊断；helper 起核时 = daemon 报告的受管核 pid）。
    pid: Arc<Mutex<Option<u32>>>,
    /// **C6-5**：当前运行核是否经 helper 提权起（TUN 路由）。运行期内部真值源（≠ 面向前端的
    /// `ProxyStatus.started_via_helper`，后者仅就绪成功后落）——驱动 [`kill_core`](Self::kill_core) 走
    /// helper stop（child 恒 None）+ 崩溃监测/就绪门改用 pid 探活（helper 核无本地 [`Child`] 句柄）。
    /// 起核提交时置、停核/直起时清。
    core_via_helper: Arc<AtomicBool>,
    /// H-1 强制重启专用配置快照（`(id, config)`）。
    ///
    /// **不可用 currentConfig 替代**：in-flight start 腿会覆盖 currentConfig，drain 必须读本字段
    /// 才能重启到 apply 当时那份 cfg（上游 `pendingForceRestartConfig`，:1729-1730）。
    pending_force_restart: RwLock<Option<(u64, Value)>>,
    /// force-restart 快照 id 发号器（LifecycleGate 只存不透明 id，载荷由本层关联）。
    force_restart_seq: AtomicU64,
    /// 最后**已应用**到运行核的配置（上游 `ProxyManager.currentConfig`）。
    ///
    /// **不可用 `startup_snapshot` 替代**：后者是起核时的快照（待应用差集基准，热切/defer 腿不刷）；
    /// 本字段被热切/no-op/defer 三条非结构腿逐次对账 → 是 `plan_hot_switch` 的 `old` 入参真值。
    /// 也**不可用 `config.current()` 替代**：那是磁盘上的**新**配置（switchMode 的 `new` 入参）。
    current_config: RwLock<Option<Value>>,
    /// 起核时刻的热切换基准（id→tag / rule-sel / 节点指纹）。None = 核未起或快照不可信 → 全部退回重启。
    switch_snapshot: RwLock<Option<SwitchSnapshot>>,
    /// lifecycle 在飞时暂存的 switchMode 配置（上游 `pendingSwitchConfig`，:1753）。
    /// `(id, config, defer_restart)`：id 与 `LifecycleGate::set_switch_pending` 对齐，排空时按 id 认领。
    ///
    /// **`defer_restart` 必须跟着一起暂存**：它是「本次落盘由谁触发」的意图，不是配置内容的一部分。
    /// 若排空重放时丢掉它，用户在核重启窗口内点的那次「保存」会在几秒后自己触发一次重启 ——
    /// 恰是「保存不重启」承诺的反面，且现象是延迟的、极难归因。
    pending_switch: RwLock<Option<(u64, Value, bool)>>,
    /// switch 快照 id 发号器（与 force_restart_seq 同构，各自独立编号）。
    switch_seq: AtomicU64,
    /// 配置入核单飞锁。正常热切换含管理 API I/O；没有这把锁时，快速连续切节点会让多个
    /// `switch_mode` 同时基于同一份 `current_config` 规划，较慢的旧 PUT/commit 可在新请求之后落地，
    /// 表现为最后一次点击被盖回、继而由错误快照触发多余重启。Tokio Mutex 按等待顺序放行，且锁只护
    /// 配置入核流水线，不与同步 [`LifecycleGate`] / 配置写锁混用。
    switch_serial: AsyncMutex<()>,
    /// 「保存不重启」欠下的账：本次运行核起来之后，是否发生过被 `defer_restart` 降级的结构性变更。
    ///
    /// # 为什么是一个记账标记而不是现算的差集
    ///
    /// 待应用差集（[`Self::pending_changes`]）是**节点**差集，看不见 `mixedPort` / TUN / DNS 这类
    /// 非节点结构性变更 —— 「保存」把它们降成 Defer 后，条上会显示 0 项待应用，用户看到的是
    /// 「保存了、什么也没发生、也没人说还差一步」。那正是本仓刚收口的「第四类重启」同一种形态。
    ///
    /// 现算的候选判据（`norm(起核快照) != norm(磁盘)`）**不可用**：kind=rules 的热切换会 PUT 掉
    /// 规则目标而不刷起核快照 ⇒ 两侧 norm 从此长期不等 ⇒ 恒真的假阳性。真正知道「这次落盘没进核」
    /// 的只有 switch_mode 自己，所以由它记账。
    ///
    /// 清账点**只有核真正按磁盘配置起来那一刻**（与 `startup_snapshot` 同刻）+ 停核复位。
    /// 后续的 NoOp / 热切腿都**不清**：它们没有把先前欠下的那份配置送进核。
    restart_deferred: AtomicBool,
    /// 崩溃自愈状态机（core-supervisor 既有决策机：退避 / 上限 / 让位 / 补发全在其中）。
    ///
    /// 后台崩溃监测任务检测到核**意外**退出时喂它决策，本层只执行「退避 sleep + restart」的 I/O。
    /// 与运行核不同生命周期：跨 start/stop 持久（restart_count 靠 60s 冷却复位，不随每次 start 清零——
    /// 否则崩溃→重启→崩溃 的紧密循环永远达不到上限）。std `Mutex`：决策同步、绝不跨 await 持锁。
    crash_recovery: Mutex<CrashRecoveryMachine>,
    /// 诊断分轴计数器（维度7 #11 慢起 vs 核崩，喂给 `diagnostic_export` 报告）。
    ///
    /// **本运行时只在此持有并喂「慢起轴」**（`last_start_ready_retries`）——它是全仓唯一该产生这数的地方
    /// （起核就绪门的重试累计），此前无人喂 → 报告恒零（§O1）。
    ///
    /// **「核崩轴」不在这里并行记**：`restart_count` 的单一真值是上面的 [`CrashRecoveryMachine`]
    /// （它已按 上游 :548 计数且自带「诊断用」getter `restart_count()`）。`diagnostic_counters()`
    /// 在**读时**把它投影进快照，而非在 `run_crash_recovery` 里再 `record_restart` 一遍——同一崩溃事件
    /// 绝不记两遍（否则两计数器的复位时机会分叉，报告数与控制数打架）。故 `DiagnosticCounters` 的核崩轴
    /// API（`record_restart`/`reset_if_past_cooldown`）在本运行时不被生产调用，仅由 stats-engine 自测覆盖。
    /// std `Mutex`：慢起轴更新同步、绝不跨 await 持锁。
    diagnostics: Mutex<DiagnosticCounters>,
    /// stale-core 清扫**禁用**开关（仅单测置位，用于跳过 `/proc` / `ps` 扫描聚焦被测腿）。
    ///
    /// **原先是「一会话只清一次」的门闩，已废——那个前提是错的**：它假设「孤儿只来自上个 app
    /// 会话崩溃」，而本次真机事故的孤儿恰恰产生于**会话中途**（一次失败的 TUN 起核把 root 核留在了
    /// 后台），于是同一会话的后续 start 全都不再清扫 ⇒ 那个孤儿永远落在清扫射程外，一直占着
    /// `cache.db` 把用户彻底卡死。**清扫缺陷自己就能造出它声称不可能存在的孤儿**，故门闩必须去掉。
    ///
    /// 现语义 = 每次 `start` 都清（对齐 上游 `ProxyManager.ts:700`）。成本：无孤儿时仅一次进程扫描
    /// （Linux 读 `/proc`，macOS 一次 `ps` exec），相对一次用户发起的起核可忽略；有孤儿才进入
    /// SIGTERM/宽限腿。**不选「仅在 start 失败时复位门闩」**：起核成功后核也可能中途崩成孤儿
    /// （正是崩溃自愈路径），那条只覆盖失败腿，仍会漏掉同一类事故。
    stale_sweep_disabled: AtomicBool,
    /// stale-core 清扫的**实跑次数**（诊断 + 「每次 start 都清」这条不变式的唯一可观测量）。
    ///
    /// 没有它，「门闩有没有退回一次性」只能靠读代码推理——而这正是本次事故里失守的那类推理。
    stale_sweep_runs: AtomicUsize,
    /// 外化自定义规则文件落盘降级标记（上游 `customRuleFilesDegraded`，:423）。
    ///
    /// [`write_custom_rule_files`](Self::write_custom_rule_files)（起核前）逐文件写失败 → 置位（缺文件
    /// 触发 route/DNS ext 分支 `existsSync` 降级走 inline，用内存态值，功能不损）；成功清位。运行中
    /// `switch_mode` 三条非结构腿（热切/no-op/defer）据此决定「值热更（[`sync_custom_rule_files`]）还是
    /// 改走去抖重启重落盘」——降级态文件无消费者，改走重启才能让新值生效（否则「写了没人消费」的值陈旧）。
    ///
    /// [`sync_custom_rule_files`]: Self::sync_custom_rule_files
    custom_rule_files_degraded: AtomicBool,
    /// 「系统代理残留」提示每会话只发一次的门闩（见 [`maybe_warn_system_proxy_residual`]）。
    ///
    /// [`maybe_warn_system_proxy_residual`]: Self::maybe_warn_system_proxy_residual
    residual_warned: AtomicBool,
    /// 系统代理清理收口器（维度7 #8）。start 失败腿经它清「仍指向我们死端口的系统代理」，防旧会话
    /// 残留 → 死端口 → 全网断。
    ///
    /// - `Arc<Mutex<Box<dyn ..>>>`：`ensure_cleared` 是 `&mut self` **同步** API（会 exec
    ///   `networksetup`/`gsettings`/`reg`），失败腿在 async 里经 `spawn_blocking` **持锁**调用
    ///   （绝不阻塞 async runtime，也绝不跨 await 持锁）。
    /// - 生产装 `production_proxy_controller(<userData>/system-proxy.marker.json)`（见 `runtime.rs`）；
    ///   测试装 mock 记录调用。**必传构造参数**（非默认 no-op）：让「忘接线」变成编译错，杜绝
    ///   §K7「逻辑在、接线不在」的静默缺失。
    proxy_clearer: Arc<Mutex<Box<dyn SystemProxyClearer>>>,
    /// `event:proxyError` 发射器（[`set_error`](Self::set_error) 的出口）。
    ///
    /// **`OnceLock` 而非构造参数**：`AppHandle` 要到 Tauri `setup` 才存在，而本运行时在
    /// `AppRuntime::new(config_dir)` 里就得造出来 → 只能「先构造、后接线」（`main.rs` setup 内
    /// [`set_error_emitter`](Self::set_error_emitter)）。未接线（单测 / setup 前的极早期失败）→
    /// `set_error` 只记日志 + 落状态码，不 panic：**发不出事件绝不能反过来打断错误处理本身**。
    error_emitter: std::sync::OnceLock<Box<dyn ProxyErrorEmitter>>,
    /// A4 登录期出口让位内存态（上游 `bootstrapFallbackEngaged` + `bootstrapFallbackServerId`）。
    ///
    /// engaged=当前 proxy-selector 是否被临时热切到 direct；server_id=让位所服务的选中出口 id（用户中途
    /// 切走出口时据此判 stale 复位）。仅运行期内存态，随停核/崩溃复位（[`reset_login_fallback_state`]）。
    /// 单锁护 `(engaged, server_id)` 对，杜绝命令读到撕裂态。
    ///
    /// [`reset_login_fallback_state`]: Self::reset_login_fallback_state
    login_fallback: Mutex<LoginFallbackState>,
    /// A4 reconcile 单飞守卫（上游 `loginFallbackReconciling`）。多驱动源（STATUS 帧 / switchMode / 起核预置）
    /// 可重入；在飞对账中丢弃后来者（下一帧/tick 幂等收敛）。`swap(true)` 抢占、[`ReconcileGuard`] 保证退场必复位。
    login_fallback_reconciling: AtomicBool,
    /// **R2 TS 出口无效直判的翻转对账缓存**（上游 `lastTsExitBlock`）。`Some(reason)` = 上次对账判定
    /// 出口无效及其原因；`None` = 上次判定有效 / 不适用。
    ///
    /// **存的是「上次值」而不是「当前值」**：对账是**跨态**触发（`cur != prev` 才动作），不是每帧 level
    /// 触发 —— STATUS relay 每秒量级推帧，按当前值动作就成了每秒一次的重探 + 每秒一次解锁失效
    /// （与 [`ts_exit_became_ready`] 挡住的是同一种轮询退化）。停核复位（见
    /// [`reset_ts_exit_block_state`](Self::reset_ts_exit_block_state)）。
    last_ts_exit_block: Mutex<Option<&'static str>>,
    /// **R2 出口恢复腿单飞守卫**（上游 `tsExitRecovering`）。恢复腿含 gRPC EditPrefs + reassert
    /// （macOS resolveIface 轮询最长 ~18s），必须串行。
    ts_exit_recovering: AtomicBool,
    /// **R2 出口恢复腿的补跑标记**（上游 `tsExitRecoverPending`）。
    ///
    /// **为什么恢复腿要 pending 而登录让位对账不要**：让位对账是 level 触发（每帧都跑，被丢的那次下一帧
    /// 自愈）；恢复腿是**边沿**触发（只有 blocked→none 跨态才调），在飞期间发生的
    /// `none→blocked→none` flap 若被单飞直接丢弃，**下一帧同态早退**（`cur == prev`）⇒ 那条边沿永远
    /// 不会重来 ⇒ 卡在「出口已恢复但没人去重探」直到下一次真跨态或用户手点。故在飞期间记 pending，
    /// 收尾若仍是 `none` 则补跑一轮。
    ts_exit_recover_pending: AtomicBool,
    /// C11 DNS race sidecar 运行期端口 + 上游直连 IP（起核时由 race sidecar 填；race off / 未起 = (0, [])）。
    ///
    /// **注入面（本机可验）**：`generate_deps` 据此把 `race_server_port` / `race_upstream_ips` 喂进 config-engine
    /// —— port>0 才生成 `dns-node-race` server 并放行上游直连；否则 withRaceOff 强制单上游、逐字节回现状、
    /// 不悬空引用 dns-node-race（防 FATAL）。
    ///
    /// 写入口只有一处：[`start_race_sidecar`](Self::start_race_sidecar)（起核路径，绑口成功后）；
    /// 清除口只有一处：[`clear_race_server`](Self::clear_race_server)（竞速关 / 绑口失败 / 起核失败 / 停核 /
    /// sidecar watchdog 彻底失败——见 [`race_dead_callback`](Self::race_dead_callback)）。
    ///
    /// **真机门**：真正起本地 UDP race server + 内核消费该口需真核，见 [`set_race_server`](Self::set_race_server)。
    race_server: Mutex<RaceServerState>,
    /// C11 竞速 sidecar 本体（`Some` ⟺ 正在监听）。与 [`race_server`](Self::race_server) 是
    /// **同一件事的两面**：这里是活的 UDP server，那里是喂给 config 生成的端口/上游 IP 投影。
    ///
    /// 两者的置位/清位一律经 [`set_race_server`](Self::set_race_server) /
    /// [`clear_race_server`](Self::clear_race_server) 成对完成 —— 分开改就会出现「config 引用了一个
    /// 已经死掉的端口」或「server 活着但生成侧 race off」这两种静默错配。
    race_sidecar: Mutex<Option<NodeDnsRaceServer>>,
    /// C11 sidecar 的 DoH 上游传输（生产 = `HttpRuntime`，即 workspace 唯一的真实 HTTP/TLS 客户端）。
    ///
    /// **构造必传**（同 `proxy_clearer` 的先例：编译期强制接线）。若做成可选的事后 setter，
    /// 漏接的后果是「Tier1 全部 DoH 上游静默永远 FAIL」—— 竞速看起来在跑、实际每次都退到 Tier2/SERVFAIL，
    /// 而这正是最难从日志里看出来的一类失效。
    doh: Arc<dyn DohPost>,
    /// C7 系统 DNS 接管控制器（生产装 `production_dns_controller(<userData>/system-dns.marker.json)`）。
    ///
    /// **装配（本机可验）**：在 `new` 里从 `config.dir()` 构造（无 marker → 惰性，Linux 本机 `takeover_supported=false`
    /// 兜死写路径，见 [`SystemDnsOps::takeover_supported`](polaris_system_integration::dns_ops::SystemDnsOps::takeover_supported)）。
    /// `set_dns`/`restore_dns` 是 `&mut self` 同步 API（mac 会 exec `networksetup`/`scutil`），故 `Mutex` 护、
    /// async 里经 `spawn_blocking` 持锁调用、绝不跨 await。接管/恢复经 [`Self::set_system_dns_locked`] /
    /// [`Self::restore_system_dns_locked`]，由 TUN 起核/停核生命周期驱动（marker 单一真值）。
    ///
    /// **真机门**：真正的 mac `networksetup -setdnsservers` 接管 / `scutil --dns` 读生效解析器**触碰宿主 DNS**，
    /// 只能真机验（本机 Linux 跑 = 全 no-op；本机**绝不真改系统 DNS**）。装配惰性 / 命令收口 / 决策门控由单测覆盖。
    dns_controller: Mutex<polaris_system_integration::ProdDnsController>,
    /// 起核用的核二进制路径覆盖（**仅单测置位**，同 `stale_sweep_disabled` 的先例；生产恒 `None`）。
    ///
    /// 「起核可取消」的门必须有个**真能 spawn 的**假核（起来就死 / 起来但永不就绪），否则退避中断与
    /// 孤儿收割都测不到。唯一的现成注入点 `POLARIS_SINGBOX_PATH` 是**进程级**的：并发跑的其它单测会
    /// 读到它（`runtime::updater` 那条 `core_binary_path().is_none()` 就被这样打红过），等于把测试间
    /// 耦合做成 flaky 源。故改用 per-runtime 覆盖 —— 作用域随实例，绝不外溢到别的测试。
    #[cfg(test)]
    core_binary_override: Mutex<Option<PathBuf>>,
    /// 管理 API PUT 的落点桩（**仅单测置位**，同 `core_binary_override` 的先例；生产恒 `None`）。
    ///
    /// 生产的 PUT 出口是 [`ProxyRuntime::management_api`] → 真 gRPC；单测里核不起、`clash_api_port` 为 0
    /// ⇒ 恒 `NotReady`，于是「谁被 PUT 成了什么、按什么顺序」这类**序列**不变式全都断言不到 —— H3 校正
    /// 的每一条不变式恰好都是序列不变式。这个桩只替换 [`ProxyRuntime::put_outbound`] 里最末端的那次
    /// 调用，其余（成败→bool 的映射、日志、上层决策）全走生产同一条码路。
    #[cfg(test)]
    management_api_stub: Mutex<Option<Arc<TestPutSink>>>,
    /// row33：DNS 接口热插拔 watcher 任务句柄（macOS `route -n monitor` 长驻，去抖后 reconcile DNS）。
    ///
    /// TUN 起核就绪时 spawn（仅 macOS：`route -n monitor` 是 mac 专属；win/linux 无此腿 → 恒 None），
    /// 停核 / 崩溃复位时 `abort`。`kill_on_drop` 确保任务退场即杀子进程，无 root/宿主残留。
    /// **真机门**：真 `route -n monitor` 子进程 + 链路事件驱动只在 mac 真机可验（本机 Linux 不 spawn）。
    dns_watcher: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

tokio::task_local! {
    /// **本次起核的交互性**（移植 上游 `start(config, {interactive:false})`，`ProxyManager.ts:1475`）。
    ///
    /// 未设置 ⟺ 交互式（默认）：[`ProxyRuntime::run_helper_gate`] 该弹框就弹框。
    /// 设为 `false` ⟺ 非交互：不弹框，直接落 [`code::HELPER_NOT_INSTALLED`] 终态。
    /// **唯一置位者是崩溃自愈重启腿**（[`ProxyRuntime::run_crash_recovery`]）：崩溃循环里凭空弹系统
    /// 授权框（最多连弹 `MAX_RESTART_COUNT` 次）比断流更糟 —— 用户没做任何操作，却被反复索要密码。
    ///
    /// **为什么是 task-local 而不是 runtime 上的 `AtomicBool` 字段**（根因，A2）：交互性是**这一次调用
    /// 的属性**，不是运行时的属性。挂成运行时全局字段有两个必然缺陷：
    /// 1. **跨调用污染**：`LifecycleGate` 只是深度计数器、不是互斥锁，并发 `start` 完全可能同时在飞。
    ///    崩溃自愈的 `restart()`（stop + start + 最多 3 轮重试与就绪等待，可达数十秒）整段置位期间，
    ///    用户**手动点连接**会读到同一个标记 → 门被误抑制、直接落 `HELPER_NOT_INSTALLED`，用户的显式
    ///    交互请求被当成非交互自愈处理（正是本门要消灭的行为）。
    /// 2. **嵌套解除**：字段版用 `Drop` 无条件 `store(false)` 而非计数递减，两个抑制作用域重叠时内层
    ///    退场会提前解除外层。
    ///
    /// task-local 天然随调用链传递、随作用域嵌套、且**不跨任务泄漏** —— 别的任务里的 `start` 读不到，
    /// 上面两条缺陷从物理上不再存在。`tokio::spawn` 出去的任务不继承（正确：那已是另一次调用）。
    static HELPER_GATE_INTERACTIVE: bool;
}

/// 当前调用链是否为交互式起核。**未设置 = 交互式**（默认放行弹框）：绝大多数入口（IPC / 托盘 /
/// 启动自动连接 / switchMode 去抖重启）都不显式声明，它们全是用户驱动的，默认必须能弹框。
fn helper_gate_interactive() -> bool {
    HELPER_GATE_INTERACTIVE.try_with(|v| *v).unwrap_or(true)
}

/// 单测态未注入假核时，[`ProxyRuntime::core_binary_for_start`] 的固定错误文案。
///
/// **必须是固定文案**（而非复用解析器的 "未找到 sing-box 二进制…"）：守这道门的回归测试断言的正是
/// 这句话。若断言只写 `is_err()`，那么在 `resources/` 为空的机器上，门被删掉后测试依然绿
/// （解析器自己也返 Err）—— 门就成了只在装了核的机器上才有牙的门，而那恰恰是最不会被本地跑到的环境。
#[cfg(test)]
const TEST_CORE_NOT_INJECTED: &str =
    "单测态禁止解析真实核二进制：请经 ProxyRuntime::core_binary_override 注入假核（防单测漏出真 sing-box 进程）";

/// 在**非交互**语境下跑一段起核/重启（崩溃自愈专用）：本调用链全程抑制 TUN 提权引导弹框。
///
/// 作用域即 future 本身：`fut` 内（含其 `await` 出去的任意深度）读到 `false`，`fut` 一结束（含中途
/// `return` / panic 展开）作用域随栈销毁 —— 没有「忘了复位 → 标记永久粘住 → 此后所有入口的引导门静默
/// 失效」这类形态可言。嵌套调用天然是栈式的，内层退场绝不会解除外层（旧的 `AtomicBool` + 无条件
/// `Drop::store(false)` 版本会）。
async fn with_helper_gate_suppressed<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    HELPER_GATE_INTERACTIVE.scope(false, fut).await
}

/// row33：DNS 接口热插拔 watcher 去抖窗口（合并 burst 链路变化）。对齐 上游 `DnsInterfaceWatcher`
/// 默认去抖（`crates/system-integration` `dns_watcher` 单测锚定同值）。
const DNS_WATCHER_DEBOUNCE_MS: u64 = 1500;

/// [`ProxyRuntime::flush_connections_once`] 的结果。
///
/// 做成返回值而不是「内部日志了事」，是为了让两条守卫**可被单测直接断言**：跳过与开枪在日志里
/// 长得一样，只看日志的测试分不出「守卫拦下了」和「压根没走到」。
#[derive(Debug, Clone, PartialEq, Eq)]
enum FlushOutcome {
    /// 守卫①拦下：非 TUN 模式。
    SkippedNotTun,
    /// 守卫②拦下：世代已被 stop / 重启接管。
    SkippedSuperseded,
    /// 守卫②拦下：核已停。
    SkippedCoreStopped,
    /// 管理 API 连不上。
    ConnectFailed(String),
    /// `CloseAllConnections` 调用失败。
    CallFailed(String),
    /// 已 RST 全部连接。
    Flushed,
}

/// **#9**：TUN 起核后那一次连接 flush 的延迟（对齐 上游 `CONNECTION_FLUSH_DELAY_MS`）。
///
/// 留这段窗口是给「app 早于 TUN 建立的旧连接」经 TUN 重新进入 sing-box 连接表的时间 ——
/// 就绪那一刻立刻 flush，够不着还没进表的连接，等于白开一枪。
const CONNECTION_FLUSH_DELAY_MS: u64 = 1500;

/// 单测用 DoH 桩：**永远 FAIL**。
///
/// 单测绝不许碰宿主网络（禁向真实 DoH 上游发查询），故这里不是「假成功」而是「明确失败」——
/// 竞速层对 FAIL 的处置本身就有覆盖（Tier2 兜底 / 全 FAIL → SERVFAIL），假成功反而会掩盖问题。
/// 真 DoH 端到端属**真机门**。
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct NoNetworkDoh;

#[cfg(test)]
#[async_trait::async_trait]
impl DohPost for NoNetworkDoh {
    async fn post_dns_message(&self, _url: &str, _body: Vec<u8>) -> Result<Vec<u8>, String> {
        Err("单测桩：不发真实 DoH".into())
    }
}

/// `RecordingErrorEmitter` 的解锁失效记录句柄（与 tests 模块的 `UnlockInvalidations` 同型）。
#[cfg(test)]
type UnlockInvalidationProbe = Arc<Mutex<Vec<(bool, bool)>>>;

/// 单测用管理 API PUT 落点：**按调用序**记录 `(selectorTag, memberTag)` + 回放预置失败 + 可注入 panic。
///
/// 绝不碰宿主网络（不连 gRPC、不开端口）—— 真 PUT 属真机门。装配见
/// [`ProxyRuntime::management_api_stub`]。
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestPutSink {
    /// 全部 PUT 的调用序（含失败那几次 —— 重试腿的行为正是靠它断言的）。
    calls: Mutex<Vec<(String, String)>>,
    /// 前 N 次 PUT 返回失败（模拟「管理 API 刚起还没接上」），其后成功。
    fail_first: AtomicU32,
    /// 置真 → PUT 直接 panic（验续延的 `.finally()` 语义：panic 展开也必跑续延）。
    panic_on_put: AtomicBool,
    /// 续延探针：装上 `RecordingErrorEmitter` 的解锁失效记录句柄后，每次 PUT 都抄一份**当时**的长度。
    ///
    /// 「续延必须晚于校正」是一条**时序**不变式 —— 只看终态（两件事都发生了）验不出顺序。抄这个长度
    /// 等于在 PUT 那一刻给续延拍一张照：全为 0 ⟺ 每一次 PUT 都发生在续延之前。
    invalidation_probe: Mutex<Option<UnlockInvalidationProbe>>,
    /// 每次 PUT 时观测到的续延次数（见 `invalidation_probe`）。
    observed_invalidations: Mutex<Vec<usize>>,
    /// 运行期 selector **读回**的预置快照（`SubscribeGroups` 首帧的桩）。
    ///
    /// `None`（默认）= 读不到 → 自证本轮不判定，与生产「管理 API 读失败」同一条码路 —— 于是既有
    /// H3 用例不必逐个预置也不会凭空多出告警。要驱动「运行期与意图分叉」必须显式摆上快照。
    groups: Mutex<Option<Vec<GroupSelection>>>,
}

#[cfg(test)]
impl TestPutSink {
    fn put(&self, selector_tag: &str, member_tag: &str) -> Result<(), String> {
        if let Some(probe) = self.invalidation_probe.lock().unwrap().as_ref() {
            let n = probe.lock().unwrap().len();
            self.observed_invalidations.lock().unwrap().push(n);
        }
        if self.panic_on_put.load(Ordering::SeqCst) {
            panic!("单测注入：PUT panic");
        }
        // 先记录再判失败：失败轮同样要留在序列里，否则「重试跟最新选中节点」这条断言无从取证。
        self.calls
            .lock()
            .unwrap()
            .push((selector_tag.to_string(), member_tag.to_string()));
        if self.fail_first.load(Ordering::SeqCst) > 0 {
            self.fail_first.fetch_sub(1, Ordering::SeqCst);
            return Err("单测注入：PUT 失败（管理 API 未就绪）".into());
        }
        Ok(())
    }

    /// 已记录的 PUT 序列快照。
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }

    /// 预置的运行期 group 快照（见 `groups` 字段）。
    fn groups(&self) -> Option<Vec<GroupSelection>> {
        self.groups.lock().unwrap().clone()
    }
}

/// C11 DNS race sidecar 运行期状态。`port==0` ⟺ race off（未起 / 起失败 / off / snapshot / preflight / 诊断）。
#[derive(Debug, Clone, Default)]
struct RaceServerState {
    /// 本地 race DNS server 端口（>0 = 就绪 → 注入 config 的 dns-node-race `server_port`）。
    port: u16,
    /// race 就绪时的自定义上游直连 IP（route 直连放行防 TUN 回环；对齐 上游 `raceUpstreamIps`）。
    upstream_ips: Vec<String>,
    /// 上面那些上游**实际在用的端口**（`ResolvedUpstreams::direct_ports`）。与 `upstream_ips` 一同
    /// 翻转、一同注入 —— route 的直连放行是 `ip_cidr × port` 叉乘，只给 IP 不给端口的规则匹配不上，
    /// 非标端口的自定义上游在 TUN 下会经代理出站/回环（issue #147）。
    upstream_ports: Vec<u16>,
}

/// A4 登录期出口让位内存态。engaged ⟺ selector 实指 direct（仅 PUT 成功后置，flag 不与 selector 脱节）。
#[derive(Debug, Clone, Default)]
struct LoginFallbackState {
    engaged: bool,
    server_id: Option<String>,
}

/// **H3 selector 校正的续延守卫** = 上游 `reassertSelectorSelection(...).finally(...)` 的 Rust 等价物。
///
/// 校正腿的任一出口（正常跑完 / 中途 `return` 放弃 / panic 展开）都必须跑续延
/// （[`ProxyRuntime::after_selector_reasserted`]）。写成「`await` 之后跟一行调用」在 panic 展开时会被
/// 跳过 —— 后果是解锁缓存永不失效，boot 窗口那轮经旧出口探到的脏结果永久留在缓存里，且零可见迹象。
struct ReassertSettledGuard(Arc<ProxyRuntime>, u64, ProxyModeType, u16);
impl Drop for ReassertSettledGuard {
    fn drop(&mut self) {
        self.0.after_selector_reasserted(self.1, self.2, self.3);
    }
}

/// **H3 校正腿的终局**——「运行期 selector 与生成产物分叉」这条轴上唯一有信息量的那一刻。
///
/// 校正腿此前是纯 best-effort：成功、放弃、PUT 全失败在调用方眼里**完全一样**（都是返回 `()`）。
/// 而「放弃 / PUT 全失败」恰恰就是 `cache_file` 旧选择原样留任的那个状态 —— 本 bug 的现场。
/// 把终局显式带回来，才谈得上告诉用户。
struct ReassertOutcome {
    stage1: Stage1Outcome,
    /// 阶段 2 **尝试过**的 `(selector_tag, member_tag)`。PUT 成败不记：成败由读回来的运行期值裁决，
    /// 记 PUT 返回值等于又退回「拿意图对账意图」。
    rule_intents: Vec<(String, String)>,
}

/// [`ReassertOutcome`] 的阶段 1（`proxy-selector` 全局出口）终局。
enum Stage1Outcome {
    /// PUT 成功，目标成员 tag（**已折入登录期让位**：未登录 TS 出口时这里是 `direct`，那是设计语义）。
    Applied { member_tag: String },
    /// 选中节点不在运行核 tag 映射里 ⇒ 从未 PUT（上游 bug#5 的那条腿）。
    UnresolvedTag { selected_id: String },
    /// 跑满 [`ProxyRuntime::REASSERT_MAX_ROUNDS`] 轮，每轮 PUT 都失败（管理 API 不可用/恒拒）。
    PutExhausted { member_tag: String },
    /// 核已停 / 世代已变 → **主动退场，不是缺陷**：那个核已经不是用户在看的那个了。
    Abandoned,
}

/// 运行期 selector 自证的判定（纯值；[`Self::user_message`] 是它唯一的用户可见形态）。
///
/// 与 [`ExitAttestation`] 的分工（**别合并**）：那个量的是「生成产物解出的出口」对「盘上选中节点」，
/// 两边都是**意图**，故对 `cache_file` 在起核时覆盖运行期选择这层恒盲 —— 真机血证下它必判 `Match`。
/// 本枚举量的是「核**现在实际**指着谁」对「校正腿的意图」，是唯一能看见那层覆盖的轴。
enum SelectorAttestation {
    /// 运行期选择与校正意图一致，或本轮无从判定（见 [`attest_runtime_selection`] 的「没证据」约定）。
    Match,
    /// 校正腿**从未 PUT**：选中节点不在运行核 tag 映射里 ⇒ selector 原样停在 cache_file 的旧选择上。
    NeverReasserted { selected_id: String },
    /// 校正腿 PUT 跑满重试仍全失败 ⇒ 同上，selector 停在旧选择上。
    ReassertFailed { member_tag: String },
    /// PUT 成功了，但读回来的**全局**出口仍不是意图那个（核未采纳 / 被别的东西改回去了）。
    GlobalDrift {
        want: String,
        got: String,
        /// 同一快照里另有多少条分流规则也不一致（并进同一条文案，别刷屏）。
        rule_drifts: usize,
    },
    /// 全局出口对上了，但有 N 条分流规则的 selector 停在别处。
    RuleDrift {
        count: usize,
        sample_tag: String,
        want: String,
        got: String,
    },
}

impl SelectorAttestation {
    /// 用户可见文案。**统一以「未走/未按设置走」开头**，与 [`ExitAttestation::user_message`] 同语气 ——
    /// 两者共用 [`code::EXIT_MISMATCH`]，渲染端归在同一条「出口误导腿」，文案风格不该分家。
    ///
    /// 三条放弃腿都以「请重新连接」收尾：校正腿是 best-effort，重连是用户手上**真能收敛**这件事的动作
    /// （下一次起核重跑整条校正），而不是一句无处着力的「请检查」。
    fn user_message(&self) -> String {
        match self {
            Self::Match => String::new(),
            Self::NeverReasserted { selected_id } => format!(
                "启动后未能把出口切到选中节点（{selected_id} 不在本次启动的节点表中），流量可能仍走上一次的出口。请重新连接。"
            ),
            Self::ReassertFailed { member_tag } => format!(
                "启动后未能把出口切到选中节点「{member_tag}」（管理接口无响应），流量可能仍走上一次的出口。请重新连接。"
            ),
            Self::GlobalDrift {
                want,
                got,
                rule_drifts,
            } => {
                let tail = if *rule_drifts > 0 {
                    format!("，另有 {rule_drifts} 条分流规则的出口也不一致")
                } else {
                    String::new()
                };
                format!("流量未走选中节点「{want}」，核实际出口为「{got}」{tail}。请重新连接。")
            }
            Self::RuleDrift {
                count,
                sample_tag,
                want,
                got,
            } => format!(
                "有 {count} 条分流规则未走设定的节点（如「{sample_tag}」实际走「{got}」，应为「{want}」）。请重新连接。"
            ),
        }
    }
}

/// 运行期 selector 自证的**纯判定**（零 I/O）：拿校正腿的终局 + 读回来的运行期快照出结论。
///
/// # 「没证据」与「有问题」必须分开
///
/// `groups = None`（读不到：管理 API 连不上 / 首帧超时 / 核正在停）→ 判 [`SelectorAttestation::Match`]，
/// 只留日志。理由不是宽容，是**告警一旦有假就会被整体无视**（同 [`attest_effective_exit`] 门② 的取舍）：
/// 「没读到」根本不是「出口错了」的证据，而「读不到」这一侧本来就已经被
/// [`Stage1Outcome::PutExhausted`] 那条腿覆盖了 —— 管理 API 真的不可用时，PUT 早就先一步跑满重试并
/// 报出来了。两条腿一读一写盯同一件事，不需要在读侧再造一次同因异名的告警。
///
/// 同理，快照里**查不到** `proxy-selector` 这个 group（`sel(...) == None`）也只当没证据：能走到
/// `Applied` 说明这个 group 刚刚还接受过 PUT，读不到它属于核状态自身的异常，不是出口走错。
fn attest_runtime_selection(
    outcome: &ReassertOutcome,
    groups: Option<&[GroupSelection]>,
) -> SelectorAttestation {
    let member_tag = match &outcome.stage1 {
        // 主动退场：那个核已被停/被换，读它、报它都是对着一个不存在的对象说话。
        Stage1Outcome::Abandoned => return SelectorAttestation::Match,
        Stage1Outcome::UnresolvedTag { selected_id } => {
            return SelectorAttestation::NeverReasserted {
                selected_id: selected_id.clone(),
            }
        }
        Stage1Outcome::PutExhausted { member_tag } => {
            return SelectorAttestation::ReassertFailed {
                member_tag: member_tag.clone(),
            }
        }
        Stage1Outcome::Applied { member_tag } => member_tag,
    };
    let Some(groups) = groups else {
        return SelectorAttestation::Match; // 没证据 ≠ 有问题，见上方
    };
    let selected_of = |tag: &str| {
        groups
            .iter()
            .find(|g| g.tag == tag)
            .map(|g| g.selected.as_str())
    };
    // 分流规则侧：只统计**读得到且值不对**的，读不到的一律不计（同上「没证据」约定）。
    let rule_drifts: Vec<(&str, &str, &str)> = outcome
        .rule_intents
        .iter()
        .filter_map(|(tag, want)| match selected_of(tag) {
            Some(got) if got != want => Some((tag.as_str(), want.as_str(), got)),
            _ => None,
        })
        .collect();
    // 全局出口优先报：它决定「所有未命中规则的流量从哪出去」，量级压过单条规则。
    if let Some(got) = selected_of(PROXY_SELECTOR_TAG) {
        if got != member_tag {
            return SelectorAttestation::GlobalDrift {
                want: member_tag.clone(),
                got: got.to_string(),
                rule_drifts: rule_drifts.len(),
            };
        }
    }
    match rule_drifts.first() {
        Some((tag, want, got)) => SelectorAttestation::RuleDrift {
            count: rule_drifts.len(),
            sample_tag: (*tag).to_string(),
            want: (*want).to_string(),
            got: (*got).to_string(),
        },
        None => SelectorAttestation::Match,
    }
}

/// reconcile 单飞守卫：退场（含任一 early-return / panic）必把 `login_fallback_reconciling` 复位，
/// 杜绝「在飞标志卡死 → 让位永不再对账」。
struct ReconcileGuard<'a>(&'a AtomicBool);
impl Drop for ReconcileGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// 在飞起核计数守卫：`start` 的任一出口（Ok / Err / `?` 早退 / panic 展开）都归还计数，
/// 杜绝「计数卡死 → `ProxyStatus::starting` 永久为真 → 连接按钮永远显示成取消」。
struct InflightGuard(Arc<AtomicU32>);
impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// R2 出口恢复腿单飞标志的 Drop 复位（退场含 panic 必复位）。
///
/// 与 [`ReconcileGuard`] 同理由、不同形态：那条持 `&AtomicBool`（作用域内用），恢复腿跑在
/// `spawn` 出去的 `'static` 任务里，只能持 `Arc<ProxyRuntime>`。
/// 漏复位的后果是**静默的**：`ts_exit_recovering` 卡在 `true` ⇒ 本会话此后每一次真恢复都被单飞
/// 直接吞掉（只记 pending 而没人来消费），而日志上什么都看不到。
///
/// # 为什么 Drop 只清 `recovering`、`pending` 要走 `swap` + 补跑（Rust 多线程独有的丢边沿窗口）
///
/// 上游的 `finally` 里两个标志一起清是安全的：TS 单线程下 `while (pending && …)` 判定与 `finally`
/// 之间**没有插入点**。Rust 这里有 —— 循环判 `pending == false` 之后、Drop 执行之前，STATUS relay
/// 线程完全可以跑一次 `begin_ts_exit_recovery` 把 `pending` 置回 `true`；Drop 若无条件清位，这条
/// `blocked→none` 边沿就被**永久**抹掉（恢复腿是边沿触发，同态帧下一轮直接早退，不会自愈）。
/// 故 Drop 用 `swap(false)` 取走边沿并**自己补跑一轮**。
///
/// 补跑的两条前置条件缺一不可：
/// - `status().running`：核已停（或正在重启的停核窗口）时 `selected_ts_exit_block()` 恒 `None`
///   （STATUS 缓存已清），单看它会把「没有核」误读成「出口有效」⇒ 对着已停的核重申路由 + 重探；
/// - `selected_ts_exit_block().is_none()`：在飞期间 flap 回 blocked 就别对着已知无效的出口空跑。
///
/// 补跑走 [`ProxyRuntime::spawn_ts_exit_recovery`]，它**重新快照当前世代** —— 故停核→起核之间被记下的
/// pending 由**新会话**的腿消费，不会拿旧世代空转（这也是 `reset_ts_exit_block_state` 不再碰这两个
/// 原子标志的前提，见该方法文档）。
struct TsExitRecoverGuard(Arc<ProxyRuntime>);
impl Drop for TsExitRecoverGuard {
    fn drop(&mut self) {
        // 单飞位先释放、再取边沿：反过来会让补跑腿撞上自己还没放的位（`begin` 失败 → 边沿又回 pending，
        // 而此刻已经没有在飞腿会去消费它）。
        self.0.ts_exit_recovering.store(false, Ordering::SeqCst);
        if self.0.take_ts_exit_recover_rerun() {
            log::debug!("TS 出口恢复腿收尾时捡回一条被 Drop 窗口丢掉的 blocked→none 边沿 → 补跑");
            ProxyRuntime::spawn_ts_exit_recovery(&self.0);
        }
    }
}

/// **可被「世代变更」中断的等待** —— 起核腿一切阻塞点的唯一等待原语。
///
/// 返回 `true` = 本腿已被接管（用户点了停止 / 更新的 start 抢占），调用方应立即走让位腿；
/// `false` = 睡满 `dur` 且本腿仍当权。
///
/// # 为什么不能只在迭代边界判世代（本函数存在的全部理由）
///
/// 让位检查点（spawn 前持锁判 / 就绪门 `is_superseded` / Dead·Timeout 世代复查 / 就绪后复查）本身是
/// 齐的，但它们**只在两次等待之间执行**。真机事故里起核连续 FATAL、每轮在退避 sleep 上停 2s/4s，
/// 用户此时点停止：`stop` 确实 bump 了世代，可在飞的起核腿还躺在 `tokio::time::sleep` 里 —— 取消
/// 要静默等本轮睡满才生效。「后端理论上可取消」与「点了立刻停」之间差的就是这一层：**等待本身必须
/// 可中断**，而不是等待结束后才发现该走了。
///
/// # 边沿不丢（`notify_waiters` 无 permit 的正确用法）
///
/// [`Notify::notify_waiters`] 只唤醒**此刻已注册**的等待者、不留 permit。故顺序必须是
/// 「`enable()` 注册 → 复查世代 → select」：
/// - 注册**之后**的 bump → 由 `notified` 分支捕获；
/// - 注册**之前**的 bump → 由复查捕获（世代是单调递增的持久事实，不像信号会过期）。
///
/// 两侧夹住，任何时刻的 bump 都至少被一条腿看见。把复查删掉、或挪到 `enable()` 之前，都会开出一个
/// 「信号已发但没人在听、世代却已变」的漏判窗口 —— 那正是回归成「等睡满」的形态。
///
/// 唤醒后仍以 `gate.generation()` 复判（**信号只是提醒，世代才是判据**）：即便将来出现无关唤醒，
/// 也只会退化成「多醒一次继续睡」，不会误判让位。
async fn sleep_unless_superseded_on(
    gate: &LifecycleGate,
    gen_changed: &Notify,
    my_gen: u64,
    dur: Duration,
) -> bool {
    let notified = gen_changed.notified();
    tokio::pin!(notified);
    // 先注册（`enable()` 只登记兴趣、不等待），**再**复查世代。两步顺序不可颠倒，也不可只留一步：
    // 少了 `enable()` 就漏掉复查之后的 bump；少了复查就漏掉注册之前的 bump（信号已丢、世代还在）。
    // 刻意**不**在此之前再加一道「快速路径」复查：那会把这一道遮住，让删掉它的变异测不出来
    // （实测如此）—— 一道说得清、测得到的门，胜过两道互相掩护的门。
    notified.as_mut().enable();
    if gate.generation() != my_gen {
        return true;
    }
    tokio::select! {
        () = tokio::time::sleep(dur) => {}
        () = notified => {}
    }
    gate.generation() != my_gen
}

impl ProxyRuntime {
    /// 新建（注入 config / helper / mesh 运行时 + 系统代理清理收口器）。
    ///
    /// `proxy_clearer` 生产传 `production_proxy_controller(...)`（见 `runtime.rs`），测试传 mock。
    /// `doh` 生产传 `HttpRuntime`（唯一真实 HTTP/TLS 客户端），测试传 stub。
    /// **二者皆必传**：见各自字段文档（编译期强制接线）。
    pub fn new(
        config: Arc<ConfigManager>,
        helper: Arc<HelperRuntime>,
        mesh: Arc<MeshRuntime>,
        proxy_clearer: Box<dyn SystemProxyClearer>,
        doh: Arc<dyn DohPost>,
    ) -> Self {
        let gate = Arc::new(LifecycleGate::default());
        // C7：DNS marker 路径锚 `<userData>/system-dns.marker.json`（对齐 上游 `SystemDnsBase.getMarkerPath`）。
        // 在构造前算好（`config` 随后被 move 进 Self）。无 marker（fresh start）→ 控制器全惰性。
        let dns_marker_path = config
            .dir()
            .join(polaris_system_integration::DNS_MARKER_FILENAME)
            .to_string_lossy()
            .into_owned();
        Self {
            config,
            helper,
            mesh,
            status: RwLock::new(ProxyStatus::default()),
            startup_snapshot: RwLock::new(None),
            debounced: DebouncedRestart::new(gate.clone()),
            gate,
            gen_changed: Arc::new(Notify::new()),
            start_inflight: Arc::new(AtomicU32::new(0)),
            child: Arc::new(Mutex::new(None)),
            pid: Arc::new(Mutex::new(None)),
            core_via_helper: Arc::new(AtomicBool::new(false)),
            pending_force_restart: RwLock::new(None),
            force_restart_seq: AtomicU64::new(1),
            current_config: RwLock::new(None),
            switch_snapshot: RwLock::new(None),
            pending_switch: RwLock::new(None),
            switch_seq: AtomicU64::new(1),
            switch_serial: AsyncMutex::new(()),
            restart_deferred: AtomicBool::new(false),
            crash_recovery: Mutex::new(CrashRecoveryMachine::default()),
            diagnostics: Mutex::new(DiagnosticCounters::new()),
            stale_sweep_disabled: AtomicBool::new(false),
            stale_sweep_runs: AtomicUsize::new(0),
            custom_rule_files_degraded: AtomicBool::new(false),
            residual_warned: AtomicBool::new(false),
            proxy_clearer: Arc::new(Mutex::new(proxy_clearer)),
            error_emitter: std::sync::OnceLock::new(),
            login_fallback: Mutex::new(LoginFallbackState::default()),
            login_fallback_reconciling: AtomicBool::new(false),
            last_ts_exit_block: Mutex::new(None),
            ts_exit_recovering: AtomicBool::new(false),
            ts_exit_recover_pending: AtomicBool::new(false),
            race_server: Mutex::new(RaceServerState::default()),
            race_sidecar: Mutex::new(None),
            doh,
            dns_controller: Mutex::new(polaris_system_integration::production_dns_controller(
                dns_marker_path,
            )),
            dns_watcher: Mutex::new(None),
            #[cfg(test)]
            core_binary_override: Mutex::new(None),
            #[cfg(test)]
            management_api_stub: Mutex::new(None),
        }
    }

    /// 本次起核要 spawn 的核二进制（生产）= [`resolve_core_binary`] 逐字不变。
    #[cfg(not(test))]
    fn core_binary_for_start(&self) -> Result<PathBuf, String> {
        resolve_core_binary()
    }

    /// 本次起核要 spawn 的核二进制（**单测态：注入才给，否则拒**）。
    ///
    /// 单测只认 `core_binary_override` 注入的假核；**未注入即 Err，绝不回落 [`resolve_core_binary`]**。
    /// 根因（本 fn 存在的全部理由）：起核路径是单测里唯一会真 `Command::spawn` 出核进程的地方，而
    /// [`TokioSpawner`] 造出的 `Child` **没有 `kill_on_drop`**（见 `core_supervisor::stale_core` 的边界
    /// 声明：孤儿核靠下次启动的收割器兜，不靠 Drop）。于是「单测解析到真核」必然长成漏进程：
    /// 测试跑完 → tokio runtime 与 `ProxyRuntime` 一起销毁 → 没人调 `stop()` → 真 sing-box 继续跑，
    /// 而它的临时配置目录已被 fixture 删掉（实测形态：`sing-box run -c <已删目录>/singbox-runtime.json`）。
    ///
    /// **为什么必须堵在这里、而不是让 `resolve_core_binary` 测试态返假核**：返假核只是把「漏真核」换成
    /// 「漏假核」，spawn 这一步还在；而这里 deny-by-default 是把「单测起核进程」整类消灭。
    ///
    /// **为什么不是「哪条测试写错了就改哪条」**：那条测试（`helper_gate_never_prompts_for_non_tun_mode`）
    /// 的注释白纸黑字写着「起核会继续往下走并因**本机无核二进制**失败」—— 假设的是开发机 `resources/`
    /// 是空的。装了核的机器（mac 真机 / 跑过 `fetch-core.mjs` 的 CI）上该假设当场失效，而测试**照常全绿**，
    /// 只是多漏一个进程。这类「绿而带副作用」的坑不可能靠逐条 review 兜住，只能靠这道门。
    #[cfg(test)]
    fn core_binary_for_start(&self) -> Result<PathBuf, String> {
        self.core_binary_override
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| TEST_CORE_NOT_INJECTED.to_string())
    }

    // ── C7 系统 DNS 接管/还原/刷缓存（装配 + 命令收口 + 生命周期接线）─────────────────────────
    //
    // 底层三平台操作（mac `networksetup`/`scutil` 真接管；win/linux 写路径 no-op）由 system-integration
    // `dns_ops`/`dns_flush` 承担（已单测三平台）。本层是**接线**：控制器实例装配 + 命令/生命周期收口。
    // 真正的 mac DNS 写入 / 刷缓存**触碰宿主 = 真机门**；本机 Linux 全 no-op。

    /// C7：系统 DNS 接管的同步核（持锁 → `set_dns` → 报告是否留下接管 marker）。
    ///
    /// best-effort：`set_dns` 内部失败仅告警 + 回滚，**绝不抛**（DNS 治理降级不阻断 TUN 启动）。锁中毒 → 跳过。
    /// 命令层直调；async 生命周期经 [`set_system_dns_best_effort`](Self::set_system_dns_best_effort) 的 spawn_blocking 包。
    pub(crate) fn set_system_dns_locked(&self) -> bool {
        match self.dns_controller.lock() {
            Ok(mut c) => {
                c.set_dns();
                c.has_marker()
            }
            Err(e) => {
                log::error!("dns_controller 锁中毒: {e} → 跳过系统 DNS 接管");
                false
            }
        }
    }

    /// C7：系统 DNS 还原的同步核（持锁 → `restore_dns` → 报告 marker 是否已清）。
    pub(crate) fn restore_system_dns_locked(&self) -> bool {
        match self.dns_controller.lock() {
            Ok(mut c) => {
                c.restore_dns();
                !c.has_marker()
            }
            Err(e) => {
                log::error!("dns_controller 锁中毒: {e} → 跳过系统 DNS 还原");
                false
            }
        }
    }

    /// C7：是否存在系统 DNS 接管 marker（命令层/诊断查询）。
    #[must_use]
    pub(crate) fn system_dns_has_marker(&self) -> bool {
        self.dns_controller
            .lock()
            .map(|c| c.has_marker())
            .unwrap_or(false)
    }

    /// C7：TUN 起核尾接管系统 DNS（best-effort、fire-and-forget，绝不阻断/拖垮起核）。
    /// 同步控制器（mac exec）挪进 `spawn_blocking`，锁绝不跨 await。
    async fn set_system_dns_best_effort(self: &Arc<Self>) {
        let this = Arc::clone(self);
        if let Err(e) = tokio::task::spawn_blocking(move || this.set_system_dns_locked()).await {
            log::error!("系统 DNS 接管 spawn_blocking join 失败: {e}");
        }
    }

    /// C7：停核/启动自愈尾还原系统 DNS（best-effort）。无 marker（fresh / 已还原）→ 惰性。
    async fn restore_system_dns_best_effort(self: &Arc<Self>) {
        let this = Arc::clone(self);
        if let Err(e) = tokio::task::spawn_blocking(move || this.restore_system_dns_locked()).await
        {
            log::error!("系统 DNS 还原 spawn_blocking join 失败: {e}");
        }
    }

    // ── row33 DNS 接口热插拔重灌（watcher → 门控 reconcile）───────────────────────────────────
    //
    // 背景：TUN 接管系统 DNS 后，插拔坞站 / 切 WiFi / VPN 上下线会带出**新接口**并把系统解析器改回
    // 物理网卡的 DHCP DNS → DNS 逃逸绕过 TUN（劫持/污染重现）。故长驻 `route -n monitor` 监听链路变化，
    // 去抖后把「新出现 / 仍未受控」的服务重新接管为受控 IP（`reconcile_dns` 幂等，只补未受控项）。

    /// row33：DNS 热插拔重灌的门控判定（纯逻辑，便于单测 + 变异）。`should_reconcile_dns` 的运行时适配：
    /// 仅当前配置仍 TUN 模式（切走 TUN → 虽 marker 在也不再重灌）+ **用户未关 `takeoverSystemDns`** +
    /// 有接管 marker 才放行。三条与起核尾的接管门（[`dns_takeover_enabled`] + `is_tun`）同口径。
    fn dns_reconcile_should_run(is_tun: bool, takeover: Option<bool>, has_marker: bool) -> bool {
        polaris_system_integration::dns_watcher::should_reconcile_dns(
            if is_tun { Some("tun") } else { None },
            takeover,
            has_marker,
        )
    }

    /// row33：DNS 接口热插拔重灌的同步核（持锁 → 门控 → `reconcile_dns`）。best-effort，绝不抛。
    /// 门控（[`Self::dns_reconcile_should_run`]）：当前配置 TUN + 接管 marker 在。锁中毒 / 门未过 → 跳过。
    pub(crate) fn reconcile_system_dns_locked(&self) -> bool {
        let raw = self.config.current().ok();
        let is_tun = raw
            .clone()
            .and_then(|v| serde_json::from_value::<UserConfig>(v).ok())
            .is_some_and(|c| c.proxy_mode_type.is_tun());
        // 用户开关活态（从**原始 JSON** 读：`dnsConfig.takeoverSystemDns` 不在 `DnsConfig` 结构体里，
        // 同 `restartOnNodeChange` / `autoSwitchNode` / `meshLoginFallbackDirect` 的既定手法）。
        let takeover = raw.as_ref().and_then(dns_takeover_enabled);
        match self.dns_controller.lock() {
            Ok(mut c) => {
                if !Self::dns_reconcile_should_run(is_tun, takeover, c.has_marker()) {
                    return false; // 非 TUN / 用户关了接管 / 无 marker → 不擅自重灌（对齐 reconcile_dns 内部 marker 守卫）
                }
                c.reconcile_dns();
                true
            }
            Err(e) => {
                log::error!("dns_controller 锁中毒: {e} → 跳过 DNS 热插拔重灌");
                false
            }
        }
    }

    /// row33：DNS 热插拔重灌（async 包装，spawn_blocking 持锁；锁绝不跨 await）。watcher 去抖后调。
    async fn reconcile_system_dns_best_effort(self: &Arc<Self>) {
        let this = Arc::clone(self);
        if let Err(e) =
            tokio::task::spawn_blocking(move || this.reconcile_system_dns_locked()).await
        {
            log::error!("DNS 热插拔重灌 spawn_blocking join 失败: {e}");
        }
    }

    /// row33：起 DNS 接口热插拔 watcher（TUN 起核就绪调，仅 macOS 真起）。已在跑则先停旧再起新（幂等）。
    ///
    /// 非 macOS 直接不 spawn（`route -n monitor` mac 专属）——**编译全平台覆盖**（loop 非 cfg 门控 → Linux
    /// 也编到、错误可被 `cargo check` 抓），但 Linux **绝不 spawn `route` 进程**（守本机网络约束）。
    fn spawn_dns_watcher(self: &Arc<Self>) {
        if !cfg!(target_os = "macos") {
            return; // 非 mac：不起 watcher（route monitor 不存在；本机 Linux 绝不 spawn route 进程）
        }
        let this = Arc::clone(self);
        let handle = tokio::spawn(async move { this.dns_watcher_loop().await });
        if let Ok(mut g) = self.dns_watcher.lock() {
            if let Some(old) = g.replace(handle) {
                old.abort(); // 幂等：替换前停旧（kill_on_drop 杀旧子进程）
            }
        }
    }

    /// row33：停 DNS 接口热插拔 watcher（停核 / 崩溃复位调）。`abort` + `kill_on_drop` 杀 `route -n monitor`。
    fn stop_dns_watcher(&self) {
        if let Ok(mut g) = self.dns_watcher.lock() {
            if let Some(h) = g.take() {
                h.abort();
            }
        }
    }

    /// row33：DNS watcher 主循环（**macOS 真机门**）：长驻 `route -n monitor`，逐行判触发（
    /// [`is_dns_reconcile_trigger_line`](polaris_system_integration::dns_route_events::is_dns_reconcile_trigger_line)），
    /// 去抖窗口（[`DNS_WATCHER_DEBOUNCE_MS`]）合并 burst，窗口结束触发门控 reconcile。
    ///
    /// 注：`crates/system-integration::dns_watcher::DnsInterfaceWatcher` 封装同款「行缓冲 + 去抖 + 门控」状态机
    /// 并有离线单测；此处 async 子进程驱动用 `tokio` 原生去抖（`BufReader::lines` 已按行切分 → 无需其行缓冲；
    /// 其借用闭包设计 `!Send`、不宜跨 await 持有于长驻任务）。触发判据 / 门控复用该 crate 的纯函数（同一真值）。
    ///
    /// 全平台编译（错误可被 `cargo check` 抓），但仅 [`Self::spawn_dns_watcher`]（mac 守卫）会调它 → Linux 不进。
    async fn dns_watcher_loop(self: Arc<Self>) {
        use polaris_system_integration::dns_route_events::is_dns_reconcile_trigger_line;
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, BufReader};

        let mut child = match tokio::process::Command::new("route")
            .args(["-n", "monitor"])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                log::warn!("DNS watcher：起 `route -n monitor` 失败（DNS 热插拔重灌不可用）：{e}");
                return;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            return;
        };
        let mut lines = BufReader::new(stdout).lines();
        let debounce = std::time::Duration::from_millis(DNS_WATCHER_DEBOUNCE_MS);
        let mut deadline: Option<tokio::time::Instant> = None;
        loop {
            let debounce_elapsed = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(l)) => {
                        // 命中触发行（链路 up/down / 地址增删 / 默认路由切换）→ 排/续去抖窗口。
                        if is_dns_reconcile_trigger_line(&l) {
                            deadline = Some(tokio::time::Instant::now() + debounce);
                        }
                    }
                    // EOF / 读错误 → 子进程退出，收束循环（stop_dns_watcher 亦会 abort）。
                    Ok(None) | Err(_) => break,
                },
                () = debounce_elapsed => {
                    deadline = None;
                    self.reconcile_system_dns_best_effort().await;
                }
            }
        }
    }

    /// C7：核 start/stop 尾刷 OS DNS 缓存（fire-and-forget、best-effort、永不阻塞代理生命周期）。
    ///
    /// 语义对齐 上游 `flushOsDnsCacheBestEffort`：mac 优先 root helper（`flush-dns`：dscacheutil + HUP
    /// mDNSResponder 两层全清）→ 不可用降级用户级 `dscacheutil`；win `ipconfig /flushdns`；linux `resolvectl
    /// flush-caches`。动机：核 start/stop 跨越「系统解析器受控/还原」边界时清缓存里残留另一侧记录（TUN+FakeIP
    /// 会话期假 IP 停核后仍命中 → 直连撞墙，反向同理）。
    ///
    /// **真机门**：真刷宿主 DNS 缓存**触碰宿主**（本机 Linux 会真跑 `resolvectl`）——故仅在**真跑 app** 时发生，
    /// 单测/gate 不触发（本方法只被 start/stop 生命周期调，不被测试直调）。
    fn flush_os_dns_cache_best_effort(self: &Arc<Self>, context: &'static str) {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            // mac helper flush 通道（其它平台不经此腿，见 `flush_os_dns_cache` 平台分派）。
            let helper_flush = || this.helper.flush_dns();
            polaris_system_integration::production_flush_os_dns_cache(
                Some(&helper_flush),
                &mut |m| log::info!("[dns-flush:{context}] {m}"),
            );
        });
    }

    /// C11：读 race sidecar 运行期端口（>0 = race 就绪）。
    #[must_use]
    pub fn race_server_port(&self) -> u16 {
        self.race_server.lock().map(|g| g.port).unwrap_or(0)
    }

    /// C11：设 race sidecar 就绪态（由 [`start_race_sidecar`](Self::start_race_sidecar) 在
    /// sidecar 绑口成功后调用）。`port` 就是随后被烧进 sing-box config 的 `dns-node-race` 端口。
    ///
    /// `upstream_ips` / `upstream_ports` 一律取 `ResolvedUpstreams` 的 `direct_ips` / `direct_ports`
    /// **两轴同源下发**：route 的直连放行按 `ip_cidr × port` 叉乘匹配，缺任一轴规则都不成立。
    pub fn set_race_server(&self, port: u16, upstream_ips: Vec<String>, upstream_ports: Vec<u16>) {
        if let Ok(mut g) = self.race_server.lock() {
            g.port = port;
            g.upstream_ips = upstream_ips;
            g.upstream_ports = upstream_ports;
        }
    }

    /// C11：停 sidecar + 清注入态（race off / 起失败 / 起核失败 / 停核）→ 生成侧回落单上游（withRaceOff）。
    ///
    /// **无条件版**（停核腿 / 测试装配）：调用方即当前权威，不必自证当权。起核腿一律走
    /// [`clear_race_server_owned_by`](Self::clear_race_server_owned_by) 的世代守卫版。
    pub fn clear_race_server(&self) {
        let _ = self.clear_race_server_owned_by(None);
    }

    /// C11：sidecar 收口的**唯一实现**，`owner_gen` 决定要不要自证当权。
    ///
    /// - `None` = 无条件清（停核腿：它自己刚 bump 过世代，就是权威）；
    /// - `Some(g)` = **世代守卫**：翻转前比对 `gate.generation()`，不等即整条跳过并返 `false`
    ///   —— 世代变了说明有更新的 start/stop 接管，当前 sidecar 是**接管方**的，停它比不停更糟。
    ///
    /// 与 [`commit_race_sidecar`](Self::commit_race_sidecar) 用**同一把复合临界区**（先 `race_sidecar`
    /// 后 `race_server`，锁序固定防死锁）：两处状态必须一起翻，否则并发起核腿交叉写会留下
    /// 「注入态指着 A 的端口、活着的 sidecar 却是 B」这种撕裂态 —— 内核会照着 config 去查一个没人听的口。
    ///
    /// **世代比对必须在锁内**：放锁外就是 check-then-act —— 判完「我仍当权」到真去 `take()` 之间，
    /// 接管方完全可以刚好提交它的 sidecar，于是照样被误停（这正是 `maybe_stop_race_sidecar_on_start_failure`
    /// 原先那道锁外守卫留下的微窗口）。`race_sidecar` 是本状态**所有**翻转的必经之路，故把判据放进
    /// 这把锁里就等于把「判权 + 翻转」做成了原子的。
    fn clear_race_server_owned_by(&self, owner_gen: Option<u64>) -> bool {
        let (Ok(mut sidecar), Ok(mut state)) = (self.race_sidecar.lock(), self.race_server.lock())
        else {
            log::error!("race sidecar 锁中毒 → 跳过清理（生成侧可能仍带旧端口）");
            return false;
        };
        if let Some(my_gen) = owner_gen {
            let cur = self.gate.generation();
            if cur != my_gen {
                log::info!(
                    "[dns-race] 起核腿已被接管（世代 {my_gen}→{cur}）→ 不动 sidecar，交接管方收口"
                );
                return false;
            }
        }
        if let Some(srv) = sidecar.take() {
            log::info!("[dns-race] 停止 sidecar（原端口 {}）", srv.port());
            srv.stop(); // drop 亦会 stop；显式调用是为了让意图出现在读代码的路径上
                        // 本会话的两类噪音事件在此一次性汇报（按条已降 `debug`，见 `polaris_dns_race::stats`）。
                        // 一条都没有就不打——「没事发生」不该占一行。
            let s = polaris_dns_race::stats::take_session();
            if !s.is_empty() {
                log::info!(
                    "[dns-race] 本次会话：识别并丢弃投毒应答 {} 条，回包时无 socket {} 条",
                    s.poisoned_dropped,
                    s.reply_no_socket
                );
            }
        }
        *state = RaceServerState::default();
        true
    }

    /// C11：提交刚起好的 sidecar —— 本体与注入态在**同一临界区**内一起换上（见
    /// [`clear_race_server_owned_by`](Self::clear_race_server_owned_by) 的锁序说明）。返回实际生效端口。
    ///
    /// **世代守卫（锁内）**：`my_gen` 已被更新的 start/stop 接管 → 返 `0` 且 `srv` 就地 drop（自动 stop，
    /// 不留孤儿监听）。绑口是个 `await` 点，被接管的旧腿完全可能在那段时间里失去当权资格 —— 不判就会
    /// 把**旧腿的端口**烧进注入态，而新核 config 里烧的是接管方的端口，两边错配 = 内核对死口做节点域名
    /// 解析，静默 SERVFAIL。
    fn commit_race_sidecar(
        &self,
        srv: NodeDnsRaceServer,
        upstream_ips: Vec<String>,
        upstream_ports: Vec<u16>,
        my_gen: u64,
    ) -> u16 {
        let port = srv.port();
        let (Ok(mut sidecar), Ok(mut state)) = (self.race_sidecar.lock(), self.race_server.lock())
        else {
            log::error!("race sidecar 锁中毒 → 放弃本次 sidecar（降级单上游）");
            return 0; // srv 在此 drop → 自动 stop，不留孤儿监听
        };
        let cur = self.gate.generation();
        if cur != my_gen {
            log::info!("[dns-race] 提交时已被接管（世代 {my_gen}→{cur}）→ 丢弃本腿 sidecar");
            return 0; // srv 在此 drop → 自动 stop
        }
        *sidecar = Some(srv); // 旧值（若有）在此 drop → 自动 stop
        state.port = port;
        state.upstream_ips = upstream_ips;
        state.upstream_ports = upstream_ports;
        port
    }

    /// C11：sidecar watchdog **彻底放弃重建**时的生产回调（装进 [`NodeDnsRaceServer::start`]）。
    ///
    /// # 不接的后果（这正是本回调存在的全部理由）
    ///
    /// crate 侧彻底失败只做两件事：`live_port=0` + 一条 error 日志 —— 二者都只在 **sidecar 内部**可见。
    /// 而本运行时的注入态（[`race_server`](Self::race_server)）此后仍是 >0 的旧端口，于是**每一次**
    /// config 重生成都继续把内核指向一个没人监听的口：内核对节点域名的解析全部静默 SERVFAIL，
    /// 既不报错也不降级，表现只是「某些节点连不上」。回调把「死了」传到注入态，清零后生成侧
    /// 下一次即回落单上游（`withRaceOff` / dns-bootstrap）。
    ///
    /// # 持 `Weak` 不持 `Arc`
    ///
    /// 回调最终活在 sidecar 的收发任务里，而 sidecar 由本运行时持有（`race_sidecar`）——
    /// 捕获 `Arc<Self>` 就是 runtime → sidecar → 回调 → runtime 的引用环，[`ProxyRuntime`] 永不释放
    /// （连带 sing-box 客户端 / 监听任务全部泄漏）。`Weak` 升级失败 = 运行时已析构，此时没有任何注入态
    /// 需要清，直接返回。
    ///
    /// # 世代守卫直接复用锁内那道
    ///
    /// 清理走 [`clear_race_server_owned_by`](Self::clear_race_server_owned_by) 的 `Some(my_gen)` 腿，
    /// **不在此另写一套「先判后清」**：死的是 `my_gen` 这条腿起的 sidecar，而回调触发时机完全不可控
    /// （watchdog 最长重试 5×200ms 之后，期间用户重连一次就换了世代）。若那时已被更新的 start/stop 接管，
    /// 活着的注入态是**接管方**的，清它 = 把一个健康的 sidecar 从 config 里抹掉，比不清更糟。
    /// 判权与翻转在同一把锁内完成（见该函数文档「世代比对必须在锁内」）。
    fn race_dead_callback(self: &Arc<Self>, my_gen: u64) -> OnRaceServerDead {
        let weak = Arc::downgrade(self);
        Arc::new(move |dead_port: u16| {
            let Some(rt) = weak.upgrade() else {
                // 运行时已析构（进程收尾）→ 没有注入态需要清。不打 error：此刻没有任何会话会受影响。
                log::info!("[dns-race] sidecar 端口 {dead_port} 失效时运行时已析构 → 无注入态需清");
                return;
            };
            // error 必须打在**真清掉了**之后：让位腿一个字节都没动，若先打 error，真机日志会把一个
            // 健康的接管会话误读成「已降级单上游」——而那正是排查 SERVFAIL 时最先被信的一行。
            if rt.clear_race_server_owned_by(Some(my_gen)) {
                log::error!(
                    "[dns-race] sidecar 端口 {dead_port} 已彻底失效（watchdog 放弃重建）→ 已清注入态，\
                     节点域名解析降级为单上游(dns-bootstrap)；重连可重建"
                );
            } else {
                log::info!(
                    "[dns-race] 死亡回调让位（世代 {my_gen} 已被接管）→ 注入态属接管方，不动"
                );
            }
        })
    }

    /// C11：按配置起竞速 sidecar。**必须在 `generate_deps` 之前调用** —— 端口要先拿到才能烧进 config。
    ///
    /// 三条腿都不阻断起核（fail-open）：
    /// - 竞速关（[`plan_upstreams`] 返回 `None`）→ 不起，端口恒 0 → 生成侧走 `nodeResolverSingle` 单上游；
    /// - 绑口失败 → 不起，同上降级（只记 warn）；
    /// - 起成功 → 注入 `(port, direct_ips)`，生成侧才产 `dns-node-race` server 并放行上游直连。
    ///
    /// 每次起核先清旧 sidecar：重连要按**新配置**重建上游池，沿用旧 sidecar 会让「用户刚改的上游选择」
    /// 直到下次重启才生效。
    ///
    /// # 世代守卫（本方法整条都在 `my_gen` 的名下）
    ///
    /// 本方法位于 `start_inner` 的**两个分钟级 await 之后**（`run_helper_gate` 的 helper 授权弹窗、
    /// `capture_tun_route_baseline`），而轮首让位检查在它**之后** —— 被接管的旧腿醒来时会一路走到这里。
    /// 三道守卫（入口早退 + clear 锁内判权 + commit 锁内判权）缺一都会让旧腿去动接管方的 sidecar：
    /// 停掉接管方已提交的 S_B、再把自己的 P_A 烧进注入态，而 B 的核 config 里烧的是 P_B ⇒ B 的节点域名
    /// 解析打到一个没人听的口，静默 SERVFAIL（且 S_A 成孤儿监听）。
    ///
    /// 入口早退只是**省功**（少绑一个注定要丢弃的 UDP 口），真正的防线是后两道锁内判权。
    ///
    /// `self: &Arc<Self>`（非 `&self`）：要给 sidecar 装死亡回调，回调须持本运行时的
    /// [`Weak`](std::sync::Weak) 句柄，见 [`race_dead_callback`](Self::race_dead_callback)。
    ///
    /// decoy 段覆盖清单文件名。放**规则资源目录** —— 与 geo 资源同一条更新通道，故这张表从此
    /// 能跟着资源更新走，不必再为改一条段发一次版（内置表是编译期定型的）。
    const DECOY_OVERRIDE_FILE: &'static str = "gfw-decoy-cidr.txt";

    /// 载入 POISONED 判定用的 decoy 段集：规则资源目录下的覆盖清单 > 内置表。
    ///
    /// **三种结局都必须在日志里可辨**，否则「清单明明放进去了却没生效」零线索：
    /// - 文件不存在 = 出厂常态 → info 报「用内置」（**absence 也要自曝**，不能静默）
    /// - 解析出段 → info 报条数；坏行另 warn（截断 5 条上报，不刷屏）
    /// - 存在但解析为空 / 全坏 → warn 报「回落内置」。空清单当**故障**而非「想关掉过滤」，
    ///   理由见 `polaris_dns_race::decoy` 模块文档（下载截断远比「刻意清空」更可能）
    ///
    /// 覆盖语义是**替换**不是并集：这张表两个方向都会错，并集只能修「漏」、永远修不了「误杀」
    /// （`31.13.0.0/16` 就是 Facebook 真实段）。同上模块文档。
    fn load_decoy_set(data_dir: &Path) -> Arc<DecoySet> {
        let path = rule_resource_dir(data_dir).join(Self::DECOY_OVERRIDE_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            log::info!("decoy 段表：未提供覆盖清单（{}），用内置表", path.display());
            return Arc::new(DecoySet::builtin());
        };
        let parsed = DecoySet::parse(&text);
        if !parsed.bad_lines.is_empty() {
            let shown: Vec<String> = parsed
                .bad_lines
                .iter()
                .take(5)
                .map(|(n, l)| format!("L{n}:{l}"))
                .collect();
            log::warn!(
                "decoy 覆盖清单有 {} 行无法解析（已跳过）：{}",
                parsed.bad_lines.len(),
                shown.join(" / ")
            );
        }
        let (v4, v6) = parsed.set.len();
        if parsed.fell_back {
            log::warn!("decoy 覆盖清单未解析出任何有效段 → 回落内置表（{v4} v4 / {v6} v6）");
        } else {
            log::info!("decoy 段表：覆盖清单生效，{v4} 条 v4 / {v6} 条 v6");
        }
        Arc::new(parsed.set)
    }

    /// decoy 段集在此载入（[`load_decoy_set`](Self::load_decoy_set)）：起核时读一次、烧进 sidecar，
    /// 与 config 的下发时点同一口径 —— 运行中换清单要下次起核才生效，别当即时开关。
    async fn start_race_sidecar(self: &Arc<Self>, user_config: &UserConfig, my_gen: u64) {
        if !self.clear_race_server_owned_by(Some(my_gen)) {
            return; // 已被接管（或锁中毒）→ 让位，绝不碰接管方的 sidecar
        }
        // `proxy_mode_type` 是 INV-1 的判据（TUN 接管期把 `system` 摘出竞速池，防 hijack-dns 自递归
        // 放大，见 [`plan_upstreams`] 文档）。取 `user_config`（本轮起核烧进 config 的那份）而非
        // `self.current_config`：后者在起核这一刻还是**上一轮**的配置，用它会让「刚切到 TUN」的第一轮
        // 仍按系统代理口径把 system 留在池里 —— 而那正是要防的形态。
        let Some(ups) =
            plan_upstreams(user_config.dns_config.as_ref(), user_config.proxy_mode_type)
        else {
            log::info!("节点域名竞速解析已关闭 → 走单上游路径，不起 sidecar");
            return;
        };
        let (t1, t2) = (ups.tier1.len(), ups.tier2.len());
        // 直连放行的两轴，一并从**真实上游集**取（Tier 分桶 / 去重 / 上限 / INV-1 过滤之后的结果）。
        // 端口刻意不留给 config-engine 照配置复算 —— 那会是第二份真值源，见
        // `RouteConfigDeps::race_upstream_ports` 的文档。
        let direct_ips = ups.direct_ips.clone();
        let direct_ports = ups.direct_ports.clone();
        let query = Arc::new(DefaultUpstreamQuery::new(Arc::clone(&self.doh)));
        // watchdog 彻底失败（端口再也绑不回来）→ 回调清本腿注入态，让降级可见且生成侧自动回落单上游。
        // 传 `None` 就是本功能最静默的一种失效，见 [`race_dead_callback`](Self::race_dead_callback)。
        let on_dead = self.race_dead_callback(my_gen);
        let decoys = Self::load_decoy_set(self.config.dir());
        match NodeDnsRaceServer::start(ups, query, DEFAULT_RACE_BUDGET, Some(on_dead), decoys).await
        {
            Ok(srv) => match self.commit_race_sidecar(srv, direct_ips, direct_ports, my_gen) {
                0 => log::warn!("race sidecar 提交失败 / 已被接管 → 降级单上游(dns-bootstrap)"),
                port => {
                    log::info!(
                        "节点域名 race 解析就绪：127.0.0.1:{port}（Tier1 {t1} / Tier2 {t2}）"
                    )
                }
            },
            Err(e) => {
                // 状态已在开头清过 = race off，生成侧自动走单上游。这里只需要让人看得见降级发生了。
                log::warn!("race server 启动失败，降级单上游(dns-bootstrap): {e}");
            }
        }
    }

    /// 接线 `event:proxyError` 发射器（`main.rs` setup 内调用一次，见 [`error_emitter`](Self::error_emitter) 字段文档）。
    ///
    /// 幂等：已接线则忽略重复接线（`OnceLock::set` 的 Err 腿）——重复接线是编程错误而非运行期状况，
    /// 记 warn 让它可见，但不 panic（不为一个诊断通道搭上 App 启动）。
    pub fn set_error_emitter(&self, emitter: Box<dyn ProxyErrorEmitter>) {
        if self.error_emitter.set(emitter).is_err() {
            log::warn!("proxy error emitter 重复接线 → 忽略（保留首次）");
        }
    }

    /// 置「换核验证窗口」抑制位（上游 `setAutoRestartSuppressed`）。
    ///
    /// 窗口内核**意外退出不自动重启**：让首次失败立刻上报，而不是在坏核上退避空转 3 次 ——
    /// 空转会把「新核有问题」这个信号淹掉，而那正是换核验证唯一要采集的信息。
    ///
    /// 唯一调用方是换核验证守护腿（`commands::updater` 的 `arm_core_validation`），
    /// 置起与撤下成对；撤下后老核照常受崩溃自愈保护。判据本体在
    /// [`CrashRecoveryMachine::should_auto_restart`](polaris_core_supervisor::CrashRecoveryMachine::should_auto_restart)。
    pub fn set_auto_restart_suppressed(&self, suppressed: bool) {
        self.crash_lock().set_auto_restart_suppressed(suppressed);
    }

    /// 当前是否处于换核验证抑制窗口（`run_crash_recovery` 的 GiveUp 文案分流用）。
    #[must_use]
    pub fn auto_restart_suppressed(&self) -> bool {
        self.crash_lock().auto_restart_suppressed()
    }

    /// 当前状态快照（上游 `proxy:getStatus`）。
    ///
    /// `uptime` 在此**现算**（`now - start_time`，秒）而非读存储值：存储的 uptime 写于起核那一刻，
    /// 读可能在几小时后 → 存了必假。见 [`ProxyStatus`] 文档。
    pub fn status(&self) -> ProxyStatus {
        let mut snap = self.status.read().map(|g| g.clone()).unwrap_or_default();
        snap.uptime = snap
            .start_time
            .map(|t0| now_ms().saturating_sub(t0) / 1_000);
        // 读时投影（同 uptime）：起核腿在飞 ⇒ starting=true。存储态恒 false，故读这一处即全部真值。
        snap.starting = self.start_inflight.load(Ordering::SeqCst) > 0;
        snap
    }

    /// 运行中主核所用的用户配置快照（`current_config`）。Tailscale 瞬态登录去重守卫用：
    /// 判该 TS 节点是否已在运行主核里（双写防护 `tailscale_endpoint_in_running_core`）。
    /// 核未跑时 `current_config` 可能仍留上次配置 → 调用方须结合 `status().running` 短路。
    pub(crate) fn current_config_snapshot(&self) -> Option<Value> {
        self.current_config.read().ok().and_then(|g| g.clone())
    }

    /// 诊断两轴计数快照（喂给 `diagnostic_export` 报告，维度7 #11）。
    ///
    /// - **慢起轴** `last_start_ready_retries`：本运行时在就绪门累计（`wait_ready` 的
    ///   begin_start → on_retry→record_retry → finish_start）。
    /// - **核崩轴** `restart_count`：从 [`CrashRecoveryMachine`] **读时投影**（单一真值，不在本地并行记）。
    ///
    /// 两轴各自单一来源、在此合并成一份快照——这也是它俩「不撞车」的收口点：慢起来自 `diagnostics`，
    /// 核崩来自 `crash_recovery`，永不互相写入。
    #[must_use]
    pub fn diagnostic_counters(&self) -> DiagnosticCounters {
        let mut snap = *self.diag_lock();
        snap.restart_count = self.crash_lock().restart_count();
        snap
    }

    /// 核是否运行（`singboxProcess || singboxPid` 等价，上游 :1736）。
    fn core_running(&self) -> bool {
        self.status.read().map(|g| g.running).unwrap_or(false)
    }

    /// 世代 +1 **并唤醒在飞起核腿**（`start`/`stop`/`restart` 入口的唯一 bump 通道）。
    ///
    /// 世代仍是唯一真值（`gate` 持有），此处只是把「世代变了」这条消息同点发出去 —— 两者同一表达式
    /// 内落值，结构上不可能分叉。**绕过本方法直接调 `self.gate.bump_generation()` 即回归**：世代变了
    /// 但没人被叫醒 ⇒ 正在退避 sleep 的起核腿要等睡满才发现自己该让位（有单测锁死）。
    fn bump_generation(&self) -> u64 {
        let g = self.gate.bump_generation();
        self.gen_changed.notify_waiters();
        g
    }

    /// [`sleep_unless_superseded_on`] 的实例侧入口（本运行时的 gate + 取消信号）。
    async fn sleep_unless_superseded(&self, my_gen: u64, dur: Duration) -> bool {
        sleep_unless_superseded_on(&self.gate, &self.gen_changed, my_gen, dur).await
    }

    /// 启动 sing-box（上游 `proxy:start`）。
    ///
    /// 语义对齐 上游 ProxyManager.start：
    /// 1. 世代 +1 + `begin`（单飞守卫；本腿被更新的 start/stop 接管即让位）
    /// 2. 解析端口（config-engine `proxy_ports` + core-supervisor `PortAllocator`）
    /// 3. 生成 sing-box config（config-engine）→ 写盘
    /// 4. spawn sing-box 进程（core-supervisor `TokioSpawner`）+ stdout/stderr 接日志 sink
    /// 5. 就绪门（core-supervisor `wait_for_core_ready`：TCP 可连管理 API）
    /// 6. 置状态 + 记启动快照
    ///
    /// **边界**：系统代理 enable / TUN / helper 提权起核**不在本批次**——见模块级声明。
    pub async fn start(self: &Arc<Self>, config: Value) -> Result<ProxyStatus, StartError> {
        // 起核在飞标记（`ProxyStatus::starting` 的源）：**置于所有早退腿之前**，覆盖整条起核腿——
        // stale 清扫本身就能停数秒（真机事故里正是它撞上杀不动的 root 孤儿），那段时间用户看到的是
        // 「转圈但 running:false」，托盘若据 running 决策就会在此叠第二次 start。Guard 的 Drop 保证
        // 下面 `?` 早退（清扫 → ROOT_ORPHAN_BLOCKED）也归还计数。
        self.start_inflight.fetch_add(1, Ordering::SeqCst);
        let _inflight = InflightGuard(Arc::clone(&self.start_inflight));
        // **每次** start 都清扫孤儿核（对齐 上游 :700），只杀「本 app 二进制起的」核——见
        // `cleanup_stale_cores`。孤儿不只来自上个会话崩溃，也来自本会话中途失败的起核尝试，
        // 故不能只清一次（见 `stale_sweep_disabled` 字段文档：那个门闩正是本次事故的放大器）。
        // 清不掉的 root 孤儿会独占 cache.db 致任何模式都起不来 → 阻断起核并落 ROOT_ORPHAN_BLOCKED，
        // 不放行到 start_inner 去撞一串无从归因的 `initialize cache-file: timeout`（T3）。
        if !self.stale_sweep_disabled.load(Ordering::SeqCst) {
            self.cleanup_stale_cores().await?;
        }
        // 世代 +1（上游 :632 start 入口）：本腿快照世代，被更新的 start/stop 接管即让位（#176）。
        let my_gen = self.bump_generation();
        self.gate.begin();
        let r = self.start_inner(config, my_gen).await;
        // end 恒执行（成功/失败/让位三路），否则 depth 永不归零 → 后续 apply 全被误判 deferred。
        self.finish_lifecycle(LifecycleKind::Start);
        // 维度7 #8：本想启动/重启却失败 → 清「仍指向我们死端口的系统代理」，防旧会话残留全网断。
        // 挂在 public `start` 包装（**而非 command 层**）→ 覆盖全部入口（IPC/托盘/自动连接）+ restart 的
        // start 腿（`restart` 内部直调 `self.start`）——后者正是本不变式的主场景（重启失败→死端口→全网断）。
        // 挂 command 层会漏掉 restart 腿 = §K7「门开在别处却当全域门」。
        self.maybe_clear_system_proxy_on_start_failure(&r, my_gen)
            .await;
        // C11：起核失败 → 把刚起的竞速 sidecar 一并收掉，别留一个没有内核在消费的 UDP 监听
        // （端口占着、下次起核换新口，而生成侧状态还指着旧口）。守卫同上：被接管则交接管方收口。
        self.maybe_stop_race_sidecar_on_start_failure(&r, my_gen);
        // **起核失败的唯一广播点**（`event:proxyLifecycle{phase:'failed'}`）。挂这里而不是各失败腿，
        // 理由同上面两条收口：这是全部起核入口（IPC / 托盘 / 启动自动连接 / `restart` 的 start 腿）
        // 的汇流点，新增失败腿只要照常 `?` 就自动有事件，没有哪条腿能悄悄失败。
        //
        // **它补的是 `event:proxyError` 盖不住的那一类**：`set_error` 头注明列「config 生成 / 写盘 /
        // spawn 失败」不经它，理由是「有 command 在 await，调用方已拿到真错」—— 而**去抖重启这条路
        // 上没有任何人在 await**（`schedule_restart` 的回调只 `log::error!`）。那一类失败此前对 UI
        // 是全静默的：条停在「应用中…」直到 12s 兜底轮询。
        //
        // **不加世代守卫**（与相邻两条收口刻意不同）：那两条做的是**破坏性**动作（清系统代理 / 停
        // sidecar），误做会伤到接管方；发一条事件不破坏任何东西，而接管方随后自己的 ready/failed
        // 会后发覆盖。漏发才是更坏的失效（条永远停在转圈），故取「宁可多发一条可被覆盖的」。
        if let Err(e) = &r {
            self.push_lifecycle(&ProxyLifecycleEvent::failed(e));
        }
        r
    }

    /// 起核失败腿的竞速 sidecar 收口。守卫与
    /// [`maybe_clear_system_proxy_on_start_failure`](Self::maybe_clear_system_proxy_on_start_failure)
    /// 完全同构（success 守卫 + `stopping` 守卫），理由也同构：
    /// - `Ok`（含让位）不收 —— 正在跑（或将被接管方拉起）的核正指着这个端口；
    /// - 世代已变 = 被更新的 stop/start 接管 —— 接管方**已经**用新配置重起过 sidecar，
    ///   此时收口会把**别人的** sidecar 停掉，比不收更糟。
    ///
    /// 世代守卫**下放到 `clear_race_server_owned_by` 的锁内**（而非在此先判后清）：判完到清之间隔着
    /// 一次函数调用，接管方完全可以在这条缝里提交它的 sidecar —— 那是 check-then-act，不是守卫。
    fn maybe_stop_race_sidecar_on_start_failure(
        &self,
        r: &Result<ProxyStatus, StartError>,
        my_gen: u64,
    ) {
        if r.is_ok() {
            return;
        }
        self.clear_race_server_owned_by(Some(my_gen));
    }

    /// start 失败腿的系统代理收口（维度7 #8 + `stopping`/success 双守卫）。
    ///
    /// 两守卫缺一即回归（变异验证锁死）：
    /// - **success 守卫**：仅 `Err` 才收口。成功/让位腿（`Ok`）绝不清——正在跑（或将被接管方拉起）的核
    ///   的系统代理不能被误清。
    /// - **`stopping` 守卫**：`my_gen` 仍等于当前世代 = 无更新的 stop/start 接管。`stop`/`restart` 入口
    ///   **必先** `bump_generation()`（见 `stop`），故「被主动停止/更新覆盖」⟺「世代已变」。世代变了则
    ///   本次失败非「我们要启动却失败」而是被接管——交接管方收口，不清（防 C1：清了又被紧随的 start
    ///   reconcile 设回）。这正是上游 `ensureSystemProxyCleared` 首行 `if (this.stopping) return` 在本
    ///   移植里的等价物——`stopping` 是 lifecycle 状态，`system-integration::ensure_cleared` 明载不持有
    ///   它、须由调用方（本层）判。
    async fn maybe_clear_system_proxy_on_start_failure(
        &self,
        r: &Result<ProxyStatus, StartError>,
        my_gen: u64,
    ) {
        if r.is_ok() {
            return; // success 守卫
        }
        let cur = self.gate.generation();
        if cur != my_gen {
            // stopping 守卫：被主动停止/更新接管 → 不清，交接管方收口。
            log::info!(
                "起核失败但已被更新的 stop/start 接管（世代 {my_gen}→{cur}）→ 不清系统代理，交接管方收口"
            );
            return;
        }
        self.clear_system_proxy().await;
    }

    /// 经 `spawn_blocking` 调用**同步**的 `ensure_cleared`（会 exec `networksetup`/`gsettings`/`reg`，
    /// 绝不在 async runtime 线程上阻塞）。marker 门控幂等：无 marker（fresh start）→ 门控 1 即返、零系统
    /// 调用。返回是否真清了（曾指向我们）。
    ///
    /// 三处共用同一 marker 门控收口点（皆不误清用户自配）：start 失败腿、`systemProxy:disable` 命令
    /// （用户主动关系统代理）、以及 [`stop`](Self::stop)（主动停止终态，维度7 #8 对称面）。
    pub async fn clear_system_proxy(&self) -> bool {
        let clearer = Arc::clone(&self.proxy_clearer);
        // ensure_cleared 是 `&mut self` 同步 API → spawn_blocking 内短暂持锁调用，锁绝不跨 await。
        let outcome = tokio::task::spawn_blocking(move || {
            clearer
                .lock()
                .map(|mut g| g.ensure_cleared())
                .unwrap_or_else(|e| {
                    log::error!("proxy_clearer 锁中毒: {e} → 跳过系统代理清理");
                    false
                })
        })
        .await;
        match outcome {
            Ok(true) => {
                log::info!("系统代理曾指向我们（已死的）端口，已清（维度7 #8 收口）");
                true
            }
            Ok(false) => false,
            Err(e) => {
                log::error!("系统代理收口 spawn_blocking join 失败: {e}");
                false
            }
        }
    }

    /// **C1 启动期崩溃恢复**：上次会话若带系统代理退出却未清（崩溃 / 强杀 / panic-abort → marker 残留），
    /// 早期清掉「仍指向上个已死端口的系统代理」，否则本次启动前用户全网断连、需手动改回。
    ///
    /// 复用控制器既有 [`recover_from_marker`](SystemProxyClearer::recover_from_marker)（有 original 则恢复、
    /// 无则简单关 + 清 marker）。**marker 门控幂等**：正常 fresh start 无 marker → 门控即返、零系统调用 →
    /// 可无脑挂在每次启动。同步控制器（会 exec `networksetup`/`gsettings`/`reg`）经 `spawn_blocking` 调用，
    /// 绝不阻塞 async runtime、锁绝不跨 await。返回是否真恢复过（有残留 marker）。
    ///
    /// 与退出路径（`main.rs` `ExitRequested` → [`stop`](Self::stop) 内清系统代理）互补：那条守正常退出、
    /// 这条守**非正常退出后的下次启动**。
    ///
    /// **启动自愈汇流点**：系统代理（本体）+ 系统 DNS（C7）两条 marker/态的崩溃自愈都在此收口 ——
    /// setup 内单次调用（`main.rs` 已接），piggyback 在同一入口，免动 `main.rs`（其归并行波次）。
    /// （此处曾有第三条：诊断采集（C9）的启动自愈。那套机制已整体删除 —— 它要「自愈」的正是自己
    /// 临时改掉的 `logLevel`，机制没了也就无从崩坏；旧配置里的残留改由 store 的迁移链一次性清掉。）
    /// `self: &Arc<Self>`（非 `&self`）以便 DNS 还原挪进 `spawn_blocking`；`arc.recover_..()` 调用形不变。
    pub async fn recover_system_proxy_on_startup(self: &Arc<Self>) -> bool {
        let clearer = Arc::clone(&self.proxy_clearer);
        let outcome = tokio::task::spawn_blocking(move || {
            clearer
                .lock()
                .map(|mut g| g.recover_from_marker())
                .unwrap_or_else(|e| {
                    log::error!("proxy_clearer 锁中毒: {e} → 跳过启动期系统代理恢复");
                    false
                })
        })
        .await;
        let recovered = match outcome {
            Ok(true) => {
                log::info!(
                    "启动期检测到上次未清的系统代理 marker（上次崩溃/强杀）→ 已清残留（维度7 #8）"
                );
                true
            }
            Ok(false) => false,
            Err(e) => {
                log::error!("启动期系统代理恢复 spawn_blocking join 失败: {e}");
                false
            }
        };
        // C7：系统 DNS 崩溃自愈（对齐 Polaris 启动期 restoreDns）。上次崩溃留下的接管 marker → 还原 + 清；
        // 无 marker（fresh / win/linux）→ 惰性，本机 Linux 全 no-op。
        self.restore_system_dns_best_effort().await;
        recovered
    }

    /// TUN 经 helper 起核的前置校验（R27.3）：当前模式需要提权 helper（TUN@mac/win/linux）**且** helper
    /// 尚未安装 → `true`（阻断起核，回结构化 [`code::HELPER_NOT_INSTALLED`]）。systemProxy/manual 或
    /// helper 已装 → `false`（放行走正常起核）。
    ///
    /// **为什么必须前置**（根因）：未装 helper 时直起 `spawn_core_via_helper` → `helper.start_core` →
    /// `UnixConnector::connect` 拿到 `ENOENT`，用户只看到裸 `connect .../helper.sock: No such file
    /// (os error 2)`——不可操作。前置判定把它换成「helper 未装，去装」的可操作提示。
    ///
    /// **本机安全 / 不连 socket**：未安装态 `helper.status()` 经 `compute_status_with_client` 先判
    /// `is_installed` 短路，绝不触碰 socket（见 `runtime/helper.rs`）。
    ///
    /// **唯一调用点 = [`run_helper_gate`](Self::run_helper_gate) 的短路判定**（「非 TUN / 已装 → 零开销
    /// 放行」以及用户确认安装后的复检）。command 层曾另有一份同谓词的前置拦截，**已删** —— 它只守住
    /// 「点连接按钮」一条腿，托盘切模式 / 启动自动连接 / switchMode 去抖重启全绕过它（§K7「门开在别处
    /// 却当全域门」）。别再据本注释以为命令层还有一道门：门只有 `start_inner` 汇流点那一道。
    fn tun_helper_missing(&self, mode: ProxyModeType) -> bool {
        should_start_via_helper(mode, self.helper.platform()) && !self.helper.status().installed
    }

    /// **TUN 提权引导门**（起核汇流点；移植 上游 `ProxyManager.maybePromptHelperGate`，:1475-1497）。
    ///
    /// 判定 → 弹框 → 就地授权安装 → **复检** → 原地放行/终态，一次调用走完 上游的第 2/5/6/7/9 步：
    ///
    /// | 情形 | 结果 |
    /// |---|---|
    /// | 非 TUN / 已装 helper | `Ok(())` 放行（零弹框、零系统调用） |
    /// | 需要门但被非交互抑制（崩溃自愈） | `Err` + [`code::HELPER_NOT_INSTALLED`] |
    /// | 需要门但 emitter 未接线（单测 / setup 前） | `Err` + [`code::HELPER_NOT_INSTALLED`] |
    /// | 用户取消 | `Err` + [`code::HELPER_GATE_ABORTED`] |
    /// | 用户确认 → 装上了 | `Ok(())`，**原地继续起核**（不要求用户再点一次连接） |
    /// | 用户确认 → 没装上 | `Err` + [`code::HELPER_NOT_INSTALLED`] |
    ///
    /// **确认后必须复检、不得直接放行**（这是本方法最容易被写错的一行）：`prompt_helper_gate` 返回
    /// `Proceed` 只代表「用户点了安装」，不代表装成功（授权框可被系统拒绝、脚本可失败）。不复检就直接
    /// 往下走，会拿着仍不存在的 helper 去 `spawn_core_via_helper`，用户拿到的又是裸 socket ENOENT ——
    /// 正是本门当初要消灭的东西。
    ///
    /// **`spawn_blocking`**：`prompt_helper_gate` 内含原生模态 `blocking_show` + osascript 授权
    /// （可阻塞 30s+）。在 async runtime 线程上直调会阻塞整个 worker；在 Tauri 主线程上调
    /// `blocking_show` 会死锁。故整段挪进阻塞线程池。
    ///
    /// **已知边界：本门无超时上限（A3，刻意不做）**。用户把系统模态晾在后台不点 ⇒ 本门不返回 ⇒
    /// `LifecycleGate` 深度长期 >0 ⇒ 此期间 `switch_mode` / 去抖重启只置 pending 不执行（托盘切档位
    /// 表现为「点了排队但不动」）。**不加超时的理由**：`blocking_show` 与 `install()` 的系统授权框都
    /// 无法从别的线程取消，`spawn_blocking` 的任务也不因丢弃 JoinHandle 而中止。加超时只会得到
    /// 「运行时已判 `HELPER_GATE_ABORTED`，模态却还挂在用户屏幕上，点了『安装』照样装、装完却没核起来」
    /// 外加一条永久占用的阻塞池线程 —— 比排队更坏，且最可能命中的正是「用户正在输管理员密码」那一刻。
    /// 死锁风险已排除：`blocking_show` 跑在 tokio 阻塞池线程而非 Tauri 主线程，`panic=unwind` 下守卫的
    /// `Drop` 可靠。故这是**体验降级而非卡死**，等真机确认为高频痛点再动。
    async fn run_helper_gate(self: &Arc<Self>, mode: ProxyModeType) -> Result<(), StartError> {
        if !self.tun_helper_missing(mode) {
            return Ok(()); // 非 TUN / 已装 → 绝大多数起核走这条，零开销。
        }

        // 非交互（崩溃自愈）→ 退回本门引入前的行为：类型化终态，不打扰用户。
        // 读 task-local：只有**当前调用链**被 `with_helper_gate_suppressed` 包住才为真；并发的用户手动
        // 起核跑在另一个任务里，读不到本标记 ⇒ 照常弹引导（A2 修的正是这条）。
        if !helper_gate_interactive() {
            log::info!(
                "TUN 提权引导：非交互启动（崩溃自愈）→ 不弹引导，直接落 HELPER_NOT_INSTALLED"
            );
            self.set_error(HELPER_NOT_INSTALLED_MSG, code::HELPER_NOT_INSTALLED);
            return Err(StartError::coded(
                HELPER_NOT_INSTALLED_MSG,
                code::HELPER_NOT_INSTALLED,
            ));
        }

        // emitter 未接线（单测 / setup 前极早期）→ 同上。**绝不因为「没法问用户」就放行去 spawn**。
        if self.error_emitter.get().is_none() {
            log::debug!("TUN 提权引导：emitter 未接线 → 直接落 HELPER_NOT_INSTALLED");
            self.set_error(HELPER_NOT_INSTALLED_MSG, code::HELPER_NOT_INSTALLED);
            return Err(StartError::coded(
                HELPER_NOT_INSTALLED_MSG,
                code::HELPER_NOT_INSTALLED,
            ));
        }

        let status = self.helper.status();
        let me = Arc::clone(self);
        let decision = tokio::task::spawn_blocking(move || {
            me.error_emitter
                .get()
                .map(|e| e.prompt_helper_gate(&status))
                // 上面已判非 None；真取不到时按「用户取消」处理（不装、不起核）比按放行安全。
                .unwrap_or(HelperGateDecision::Abort)
        })
        .await
        .map_err(|e| format!("TUN 提权引导任务 join 失败：{e}"))?;

        if decision == HelperGateDecision::Abort {
            self.set_error(HELPER_GATE_ABORTED_MSG, code::HELPER_GATE_ABORTED);
            return Err(StartError::coded(
                HELPER_GATE_ABORTED_MSG,
                code::HELPER_GATE_ABORTED,
            ));
        }

        // 复检（见方法文档）：装上了才原地继续。
        if self.tun_helper_missing(mode) {
            log::warn!("TUN 提权引导：用户已确认但 helper 仍不可用 → 落 HELPER_NOT_INSTALLED");
            self.set_error(HELPER_NOT_INSTALLED_MSG, code::HELPER_NOT_INSTALLED);
            return Err(StartError::coded(
                HELPER_NOT_INSTALLED_MSG,
                code::HELPER_NOT_INSTALLED,
            ));
        }
        log::info!("TUN 提权引导：helper 已就位 → 原地继续起核（无需用户重新点连接）");
        Ok(())
    }

    /// start 主体（错误路径统一由 [`Self::start`] 收口 `end`）。
    async fn start_inner(
        self: &Arc<Self>,
        config: Value,
        my_gen: u64,
    ) -> Result<ProxyStatus, StartError> {
        // 早退让位（#176）：入口即被更新的 start/stop 接管 → 别白做 config 生成/写盘/端口解析。
        // 这只是省功，**不是**孤儿防线——真正的防线是下方 spawn 临界区内的持锁判世代。
        if self.gate.generation() != my_gen {
            log::info!("起核入口即被接管（世代 {my_gen}）→ 让位");
            return Ok(self.status());
        }

        // 分段耗时测量（仅测量，不影响任何判定/控制流）：入口墙钟 + 各段累加器（含跨重试轮的最后一次有效值）。
        let t_total = std::time::Instant::now();
        // 初值恒被循环内首次执行覆盖后才读（读点在 loop 之后，只有 break 出的成功尝试才到得了）；
        // `mut` 是跨重试轮重赋值所需，故 clippy 认为初值「never read」——按实情 allow，不删初值。
        #[allow(unused_assignments)]
        let mut config_gen_ms: u128 = 0;
        #[allow(unused_assignments)]
        let mut spawn_ms: u128 = 0;
        #[allow(unused_assignments)]
        let mut ready_ms: u128 = 0;

        let user_config: UserConfig = serde_json::from_value(config.clone())
            .map_err(|e| format!("配置解析失败（UserConfig）: {e}"))?;
        // C7 门的第二条轴：`dnsConfig.takeoverSystemDns` 用户开关（**必须在此取**——`config` 在就绪段
        // 会被 move 进 `startup_snapshot`）。三态：`Some(false)` = 用户显式关，其余（缺省 / true / 非布尔）
        // 一律视作开（对齐 上游 `takeoverSystemDns !== false` 与 `validateConfig` 的布尔口径）。
        let dns_takeover = dns_takeover_enabled(&config);

        // ── C6-5 TUN 提权引导门（**全入口唯一汇流点**，移植 上游 `maybePromptHelperGate`）──────
        // 置于 config 生成/写盘/端口解析之前（最早 bail，未装时零副作用）。**必须在此、不可在命令层**：
        // 起核入口不止 IPC —— 托盘切模式 / 启动自动连接 / switchMode 去抖重启 / 崩溃自愈 全部直调
        // `self.start`，门开在 `commands::proxy_start` 就只守住了「点连接按钮」一条腿（§K7「门开在别处
        // 却当全域门」）。这也是 systemProxy→TUN 切档位会静默停在停止态的直接成因：重启腿的 stop 跑完，
        // start 腿撞上无人值守的 preflight 直接 bail。
        let t_helper_gate = std::time::Instant::now();
        self.run_helper_gate(user_config.proxy_mode_type).await?;
        let helper_gate_ms = t_helper_gate.elapsed().as_millis();
        log::info!("起核耗时：helper提权门={helper_gate_ms}ms");

        // ── 端口两轴常量（单一真值复用 config-engine::proxy_ports）。mixed/control 由 config 决定、
        //    跨重试不变；管理 API / update-in 是动态空闲口，每次尝试重解析（见 resolve_start_ports）──
        let mixed_port = local_proxy_port(&user_config);
        let control_port = control_api_port(&user_config);

        // 3.1 起核前落盘外化自定义规则文件 + 孤儿对账清扫（**必须在 generate 前**：generate 的 route/DNS
        //     ext 分支按文件真存在性 `ext_rule_file_exists` 决定走 ext 引用还是 inline 降级；文件不在 →
        //     ext 分支 100% 不可达）。移植 上游 start :750 `writeCustomRuleFiles`。一次（重试腿不重清孤儿）。
        self.write_custom_rule_files(&user_config).await;

        // ── 内置 geo 规则集播种（调用点 2/2：**每次起核前**；对齐 上游 `ProxyManager.ts:6375`）──
        // 与上面的 writeCustomRuleFiles 同理，**必须在 generate 之前**：route builder 按
        // `is_valid_srs_fn(<rules>/x.srs)` 的真存在性决定注不注入 rule_set，文件不在 → 规则 100% 被剪。
        // 启动时已种过一轮，这里兜住「运行期被删/被外部清理/首启时目录尚未就绪」。幂等，已有有效副本零开销。
        // 默认选项 = **只补缺失**：出厂态刷新只在启动那次开（运行中可能有并发的规则资源更新，
        // 此处刷新会与之争抢同一个 dest）。见 `geo_seed::SeedOptions::refresh_out_of_box`。
        crate::runtime::geo_seed::seed_builtin_rule_sets_into(
            self.config.dir(),
            "起核前",
            &crate::runtime::geo_seed::SeedOptions::default(),
        );

        let config_path = self.runtime_config_path();
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建配置目录失败 {}: {e}", parent.display()))?;
        }
        // C6-5 起核路由：TUN + 有 helper 的平台 → 经提权 helper 起 root/SYSTEM 受管核；
        // systemProxy/manual（不接管 TUN）→ TokioSpawner 直起（见 `should_start_via_helper`）。
        let via_helper =
            should_start_via_helper(user_config.proxy_mode_type, self.helper.platform());

        // ── #159/#176 起核外层重试预算（移植 上游 `runStartWithRetry`，:859）──
        // 起核期未就绪/退出（wintun 适配器未释放 / 双 utun 抢占 / 管理口慢绑）→ 单次即终态太脆。按预算
        // 重试：**每次尝试重解析空闲端口 + 重生成配置 = 端口重分配自愈**（osascript 授权窗口/竞态被抢占 →
        // 换口重写盘，对齐 上游 onRetry allocateProbePorts）；退避给内核留足异步回收适配器的时间。
        // system_interface（reverseMesh）节点建第二张内核 TUN → 双 TUN 释放慢，预算放宽（见 resolve_start_retry_budget）。
        //
        // DESIGN-REVIEW(fx-proxy-a-runstart-retry-partial)：**不含** 上游 onRetry 的两条增强腿——(a) run 阶段
        //   `dependency[X] not found` 的 pruneTagsClosure 幽灵引用修正（需 config-engine gate-invalid-node 内部机制，
        //   属 config-engine 只读禁区）；(b) libcronet 缺库 strong-heal 重拷闭环（需 resourceManager.ensureCronetHealthy
        //   子系统）。二者靠现有「generate 期 invalid-node 剔除 + has_cronet 生成期报错」部分覆盖；完整移植列 review-queue。
        let budget = resolve_start_retry_budget(
            user_config.proxy_mode_type.is_tun(),
            &user_config.servers,
            platform_tag(),
        );
        let mut attempt: u32 = 0;
        // 内核闸门累计剥掉的节点 id。**必须在重试循环之外**：内核对某个节点的拒收是确定性的
        //（同一节点、同一个核，判定不会变），第 2 腿起沿用即可 ⇒ 重试腿恒只付 1 次 check，
        // 而不是每腿把同一批坏节点重新发现一遍。
        let mut kernel_peeled: BTreeMap<String, InvalidNode> = BTreeMap::new();
        // C-tun-conflict：起核**前**抓「应走代理的公网目的」出口 baseline（仅 TUN 模式；post-flight 差分锚点）。
        // 必须在任何 spawn 之前 —— 我方 utun 尚未上线，此刻查到的是「Polaris 起核前」的出口（物理网卡或
        // 他方 VPN 的 utun）。重试腿间 `kill_core` 会让路由回落 baseline，故只在进循环前抓一次即准。
        let tun_route_baseline = self
            .capture_tun_route_baseline(user_config.proxy_mode_type)
            .await;

        // #327：本次起核**期望**的 TUN 接口名（起核后逐腿正向验证适配器存在性的比对目标）。
        // 经 config-engine 同一个 `resolve_win_tun_interface_name` 解出 —— 生成侧
        //（`builder/inbounds.rs` win32 分支）烧进 config 的就是它，两侧同源 ⇒ 不可能出现「验的名字
        // 与核实际用的名字不是一个」。在循环外算一次即可：接口名不随重试腿变化（变的只有端口）。
        let tun_adapter_name = resolve_win_tun_interface_name(
            user_config
                .tun_config
                .as_ref()
                .and_then(|t| t.interface_name.as_deref()),
        );
        // #327：**跨腿累积**的事实——整个起核过程里是否**曾经**见过该适配器。终态诊断按它分岔：
        // 一次都没见过 = wintun 建不出来（TUN_ADAPTER_MISSING）；见过又没了 = 抖动，不冒充前者。
        let mut tun_adapter_ever_seen = false;

        // stderr 转发腿 ⇄ `SubscribeLog` 流的交接闸（见 [`CoreLogHandoff`] / [`pipe_to_log`]）。
        // **`None` = 经 helper 起核、根本没有管道**（helper 把核 stdout/stderr 重定向进启动日志文件），
        // 此时核日志 relay 要把首帧那份历史收下——那是起核到订阅之间唯一的日志来源。
        // 直起腿在下面置成 `Some`，relay 据此改为「置位交接 + 丢弃首帧历史」。
        //
        // **声明在重试循环之外**：循环是个 `let (…) = loop { … break (…) }` 表达式，就绪处的接线点在它
        // 之外。`via_helper` 在进循环前就定了（上方），故不存在「某腿直起、某腿 helper 起」的混合形态；
        // 直起时每腿都会覆写成本腿的新闸（上一腿的核已被 kill，其管道任务随之结束）。
        let mut log_pipe_handoff: Option<CoreLogHandoff> = None;

        // C11 节点域名解析多源竞速（对齐 上游 start 步骤 3.9 `startNodeDnsRaceServer`）：
        // 节点 outbound.server 恒是域名，由内核运行期解析多 A → DialSerial 逐 IP 重试；这里给内核
        // 提供一个只听回环的竞速解析上游，把「单上游被投毒 = 该节点连不上」变成「多上游竞速 + 剔 decoy」。
        //
        // **位置不可挪**：必须在重试循环**之外、之前**。
        // - 在 `generate_deps` 之前 → 端口先拿到才能烧进 config（生成侧只认 `race_server_port > 0`）；
        // - 在循环之外 → 每次重试重生成 config 时端口保持同一个。放进循环会每轮换口重绑，
        //   而失败重试正是内核可能已经拿着上一轮 config 的时候，端口漂移 = 解析静默打到死口。
        self.start_race_sidecar(&user_config, my_gen).await;

        let (
            pid,
            api_port,
            update_in_port,
            singbox_config,
            deps,
            pruned_rule_set_tags,
            binary,
            effective_user_config,
        ) = loop {
            attempt += 1;
            // 轮首让位：退避已可中断，但被唤醒的腿仍会走到这里 —— 在**重新生成配置 / 写盘 / 重解析端口**
            // 之前就退场，别拿已被接管的世代去动共享的 runtime config 文件。这是「省功 + 早退」，
            // **不是**取消的实现（真正的取消在退避与就绪等待的 select 里）：只留这一条、把等待改回裸
            // sleep，就退回「等本轮走完才生效」的老形态。
            if self.gate.generation() != my_gen {
                log::info!(
                    "起核重试轮首被接管（世代 {my_gen} → {}）→ 让位，不再重生成/重起",
                    self.gate.generation()
                );
                return Ok(self.status());
            }

            // 每次尝试重解析空闲端口（端口重分配自愈）+ 重生成配置（端口嵌入 config，必须同刷写盘）。
            let t_config_gen = std::time::Instant::now();
            let (api_port, update_in_port, pool_ports) =
                self.resolve_start_ports(&user_config, control_port);
            let deps = self.generate_deps(api_port, update_in_port, &pool_ports, &config);
            // 核二进制解析（**移到闸门之前**：闸门要拿它跑 `sing-box check`）。**此处刻意不 `?`** ——
            // 保住既有次序不变式「解析失败是终态 Err，但 gate 剔除结果须已推给渲染端」：先把 Result
            // 拿在手上，闸门按 `Ok` 与否决定跑不跑（解析不到 ⇒ 无核可问 ⇒ failOpen 跳过闸门），
            // emit 之后才在下面 `?`。每尝试解析（字面路径，成本极低）；解析不到 = 终态，不重试（非竞态失败）。
            let binary_res = self.core_binary_for_start();
            // 起核前的内核闸门：生成 → 写盘 → check → 剥掉内核点名拒收的节点 → 重来，直到内核收下。
            // 健康路径恒 1 次 check（随包核实测生产形状 26–29ms），见 `generate_and_gate` 文档。
            let gate = match self
                .generate_and_gate(
                    &user_config,
                    &deps,
                    &config_path,
                    binary_res.as_deref().ok(),
                    &mut kernel_peeled,
                )
                .await
            {
                Ok(g) => g,
                // 🔴 生成失败也要先把「闸门此前剥掉了谁」推给渲染端，再走终态。
                //
                // 这条腿真会被走到：`PeelTarget::Blocked` 只挡「被拒的**就是**选中节点」，挡不住
                // 「剥掉的那个是选中节点代理链上的一跳」—— 后者剥完，下一轮 generate 直接 Err。
                // 裸 `?` 的话 `emit_invalid_nodes` 永远发不出去，用户拿到的是一句「配置生成失败」，
                // 而**完全不知道有节点被摘掉了**，更无从知道是哪个。这正是本闸门反复在防的
                // 「节点消失而不告知」，只是发生在失败路径上。
                Err(e) => {
                    if !kernel_peeled.is_empty() {
                        let peeled_so_far: Vec<InvalidNode> =
                            kernel_peeled.values().cloned().collect();
                        log::error!(
                            "起核内核闸门剥掉 {} 个节点后配置生成失败（很可能剥到了选中节点代理链上的一跳）：{e}",
                            peeled_so_far.len()
                        );
                        self.emit_invalid_nodes(&peeled_so_far);
                    }
                    return Err(e.into());
                }
            };
            // 起核 gate 剔除结果推渲染端（标灰 + 原因 tooltip）。发在 runtime 而非 command（入口不止 IPC：
            // 托盘/自动连接/restart 直调 self.start/崩溃自愈重启）。恒发（含空数组）：空 = 无非法节点 → 清陈旧标灰。
            // **闸门剥掉的、以及被拒的那个选中节点，都在这一份里**（走同一条通道，不另开机制；
            // 后者为什么也要进见 `GateOutcome::assemble`）。
            self.emit_invalid_nodes(&gate.invalid_nodes);
            // 内核拒的正是用户选中的节点 → 终态，不 spawn（理由见 `classify_peel_target`）。
            // **emit 必须在本判定之前**（上一行）：这条腿要 `return Err`，emit 排在后面就永远发不出去，
            // 于是恰恰是「唯一让起核失败的那个节点」拿不到标灰 —— 最需要可视标记的一次反而没有。
            // 用户由此同时拿到：持久标灰的那张卡 + 一句指名道姓的错误，而不是今天那句无从下手的「启动失败」。
            if let Some((blocked, detail)) = gate.blocked {
                let msg = format!(
                    "选中的节点「{}」被 sing-box 内核拒收，已跳过起核（请修正该节点或改选其他节点）：{detail}",
                    blocked.tag
                );
                self.set_error(&msg, code::STARTUP_FAILED);
                return Err(StartError::coded(msg, code::STARTUP_FAILED));
            }
            // 因本地 .srs 缺失被 fail-closed 剪枝的 rule_set tag（空 = 规则集完整）。随本次尝试的
            // config 一起带出循环：出口自证与用户可见信号都必须对账**这一次**生成的产物。
            let pruned_rule_set_tags = gate.pruned_rule_set_tags;
            let singbox_config = gate.config;
            let effective_user_config = gate.effective_user_config;
            config_gen_ms = t_config_gen.elapsed().as_millis();
            log::info!(
                "起核耗时：配置生成+内核闸门={config_gen_ms}ms（第{attempt}次尝试，check {} 次，累计剥除 {} 个节点）",
                gate.checks_run,
                kernel_peeled.len()
            );

            let binary = binary_res?;
            // C5：起核前快照 utun 基线（每尝试；macOS 时序 diff 锚点）——须在核创建 TS 内核接口**前**。
            self.mesh.exit_route_snapshot_baseline().await;
            log::info!(
                "起核（第 {attempt} 次尝试）：bin={} config={} mixedPort={mixed_port} apiPort={api_port} viaHelper={via_helper}",
                binary.display(),
                config_path.display()
            );

            // #332：本腿的核 FATAL 真因收集口。**每腿一个新槽**：重试腿之间不共享，否则第 1 腿的地址
            // 冲突会被扣到第 3 腿头上（真因错配比没有真因更糟）。
            let fatal_slot: CoreFatalSlot = Arc::new(Mutex::new(None));
            // helper 起核走的是**文件**而非 app 管道（helper 把核 stdout/stderr 经受管
            // writer 收进 `SINGBOX_STARTUP_LOG`）。新 helper fresh-rotate，旧 helper append：同时记文件身份
            // 与长度，失败时才能只扫本腿，不把上一次会话的 FATAL 误当本次真因。
            let startup_log_cursor = self.startup_log_cursor(via_helper);

            let t_spawn = std::time::Instant::now();
            let pid = if via_helper {
                // 经 helper 起（阻塞 IPC 挪 spawn_blocking；helper 核无本地 child 句柄）。
                // 让位 → Ok(None) → 静默返回（接管方拥有已提交 pid + core_via_helper 标记，负责收口）。
                match self
                    .spawn_core_via_helper(&binary, &config_path, &user_config, my_gen)
                    .await
                {
                    Ok(Some(pid)) => pid,
                    Ok(None) => return Ok(self.status()),
                    // helper 起核失败 = R27.3 已决策终态（前端 SettingsHelper 引导先装 helper），**不重试**。
                    Err(e) => {
                        self.set_error(&e, code::STARTUP_FAILED);
                        return Err(StartError::coded(e, code::STARTUP_FAILED));
                    }
                }
            } else {
                // ── 直起临界区（与 stop 的「取 child」互斥）──
                // 竞态不变式：stop() 先 bump 世代、再取 child 锁；本处在**持锁期间**判世代。
                //   · 本判定先于 stop 的 bump → 本腿 spawn 并存 child；stop 随后取到 child 并杀 → 无孤儿。
                //   · stop 的 bump 先于本判定 → 本腿直接让位、**根本不 spawn** → 无孤儿。
                self.core_via_helper.store(false, Ordering::SeqCst);
                let mut guard = self
                    .child
                    .lock()
                    .map_err(|e| format!("child lock poisoned: {e}"))?;
                if self.gate.generation() != my_gen {
                    log::info!(
                        "起核在 spawn 前被接管（世代 {my_gen} → {}）→ 让位",
                        self.gate.generation()
                    );
                    return Ok(self.status());
                }
                let mut req = SpawnRequest::new(&binary, &config_path);
                // 核输出恒进日志 sink（非 TTY）；sing-box 不自行关色，不加 flag 会混入 ANSI 转义。
                req.extra_args = vec!["--disable-color".to_string()];
                // CWD = 可写 config 目录：GUI 从 Finder/launchd 拉起时父进程 CWD=`/`，核对 dashboard 下载兜底的
                // 相对目录按 CWD 解析会落 `/dashboard`（只读 mkdir 噪音）。Polaris 生成的其余路径全绝对，不受影响。
                req.working_dir = Some(self.config.dir().to_path_buf());
                let mut spawned = match TokioSpawner::new().spawn(&req) {
                    Ok(s) => s,
                    Err(e) => {
                        // spawn launch 失败：释放 child 锁再判重试。端口/资源竞态可重试；权限/enoent/配置无效
                        // 等确定失败 → 终态（is_retryable_start_error）。已在锁前置 core_via_helper=false，无核可孤。
                        drop(guard);
                        let msg = format!("{e}");
                        if attempt <= budget.max_retries && is_retryable_start_error(&msg) {
                            log::warn!("sing-box spawn 失败（第 {attempt} 次，可重试）→ 预算内自动重试：{msg}");
                            // 退避期被接管 → 让位（本腿 spawn 就没成，无核可孤；不 set_error、不重试）。
                            if self.sleep_start_backoff(&budget, attempt, my_gen).await {
                                return Ok(self.status());
                            }
                            continue;
                        }
                        self.set_error(&msg, code::STARTUP_FAILED);
                        return Err(StartError::coded(msg, code::STARTUP_FAILED));
                    }
                };
                let pid = spawned.pid().unwrap_or(0);
                // stdout/stderr → 日志 sink（logging.rs 已装 log::Log 实现）。
                // stdout 不接真因收集：sing-box 的 `log.Fatal` 走包级 `std` logger，其 writer 恒是
                // **os.Stderr**（`log/export.go` 的 `init()`；`--disable-color` 分支 `cmd/sing-box/cmd.go:55`
                // 换的也仍是 os.Stderr）。给 stdout 也接一份 = 白扫每一行。
                // 两条腿共用同一个交接闸：核就绪后日志改由 `SubscribeLog` 流承担，本腿只剩起核期与
                // FATAL 分类（见 `pipe_to_log` 文档）。
                let handoff: CoreLogHandoff = Arc::new(AtomicBool::new(false));
                pipe_to_log(
                    spawned.child.stdout.take(),
                    None,
                    Some(Arc::clone(&handoff)),
                );
                pipe_to_log(
                    spawned.child.stderr.take(),
                    Some(Arc::clone(&fatal_slot)),
                    Some(Arc::clone(&handoff)),
                );
                log_pipe_handoff = Some(handoff);
                *guard = Some(spawned.child);
                pid
            };
            spawn_ms = t_spawn.elapsed().as_millis();
            log::info!("起核耗时：spawn子进程={spawn_ms}ms（viaHelper={via_helper}）");
            if let Ok(mut g) = self.pid.lock() {
                *g = Some(pid);
            }
            log::info!("sing-box 已 spawn：pid={pid}（viaHelper={via_helper}）");

            // ── 就绪门（core-supervisor 既有轮询逻辑；本层只注入真实 I/O）──
            let t_ready = std::time::Instant::now();
            let ready_outcome = self.wait_ready(api_port, my_gen).await;
            ready_ms = t_ready.elapsed().as_millis();
            match ready_outcome {
                CoreReadyOutcome::Ready => {
                    // #327：就绪 ≠ TUN 网卡建出来了（就绪门只验管理口 + 进程活）。**逐腿**正向验证适配器
                    // 存在性；缺失 = 本腿失败，走重试预算而非直接硬终止 —— 网卡挂载失败多为瞬态，而重试
                    // 腿开头的 kill_core 会把这一次的核连同它半建的网卡一并清掉，下一腿是干净的重来。
                    let observation = self
                        .probe_tun_adapter_present(
                            user_config.proxy_mode_type,
                            &tun_adapter_name,
                            attempt,
                        )
                        .await;
                    if observation == TunAdapterObservation::Present {
                        tun_adapter_ever_seen = true;
                    }
                    let verdict = classify_tun_adapter_leg(
                        observation,
                        tun_adapter_ever_seen,
                        attempt,
                        budget.max_retries,
                    );
                    if verdict == TunAdapterVerdict::Proceed {
                        break (
                            pid,
                            api_port,
                            update_in_port,
                            singbox_config,
                            deps,
                            pruned_rule_set_tags,
                            // 本次真正解析出的核路径 —— 起核后的内核自证要对账的正是**这一次**的期望值
                            //（每次尝试都重解析，故必须随本轮结果带出循环，不能在循环外重算）。
                            binary,
                            // 🔴 内核闸门剥除之后、**真正生成这份 config 的那套 servers**。
                            // 循环外紧接着就用它遮蔽 `user_config`，让出口自证 / 热切快照 / TS 逆表
                            // 三处按 id 反算 tag 时，算的是运行核里真实存在的那套 tag。
                            effective_user_config,
                        );
                    }
                    // 探测最长 3s，期间可能被接管 → 与 Dead/Timeout 两腿同款复查：世代变了就静默让位
                    //（不 kill、不 set_error、不重试；接管方拥有该进程的所有权）。
                    if self.gate.generation() != my_gen {
                        log::info!("TUN 适配器验证期被接管（世代 {my_gen}）→ 让位，不闸");
                        return Ok(self.status());
                    }
                    // 核确实活着（就绪门刚判过），但它没有 TUN ⇒ 标 connected 是虚报，先拆掉再谈重试。
                    self.kill_core().await;
                    if verdict == TunAdapterVerdict::RetryLeg {
                        log::warn!(
                            "TUN 适配器未建出（第 {attempt} 次，iface={tun_adapter_name}）→ 预算内自动重试"
                        );
                        // 同 Dead/Timeout 腿：已 `kill_core()` → 取消腿无孤儿。
                        if self.sleep_start_backoff(&budget, attempt, my_gen).await {
                            return Ok(self.status());
                        }
                        continue;
                    }
                    // 预算耗尽的两条终态：**必须分开**（用户的下一步动作不同，见 code 模块该项文档）。
                    let (msg, error_code) = if verdict == TunAdapterVerdict::TerminalNeverAppeared {
                        (
                            TUN_ADAPTER_MISSING_MSG.to_string(),
                            code::TUN_ADAPTER_MISSING,
                        )
                    } else {
                        // 曾见过又消失：wintun 本身建得出来，故不发 TUN_ADAPTER_MISSING（那会把用户
                        // 导向「重装驱动」这条错误的下一步）。message 载明现场，走 STARTUP_FAILED
                        // 的第 2 段原文送达（该码在前端覆盖门里正是按「message 才是诊断」豁免的）。
                        (
                            format!(
                                "TUN 虚拟网卡 {tun_adapter_name} 反复消失（起核期建出后又不见），已重试 {attempt} 次仍失败"
                            ),
                            code::STARTUP_FAILED,
                        )
                    };
                    self.set_error(&msg, error_code);
                    return Err(StartError::coded(msg, error_code));
                }
                CoreReadyOutcome::Superseded => {
                    // #176：被接管 → 静默让位，**绝不清理/绝不重试**（接管方拥有进程/端口所有权）。
                    log::info!("起核就绪等待期被接管（世代 {my_gen}）→ 静默让位，不清理");
                    return Ok(self.status());
                }
                // Dead/Timeout 腿在报错前**必须复查世代**：`wait_for_core_ready` 每轮只在轮首判一次
                // supersede，故存在「本轮已过 supersede 检查 → 用户点停止（bump 世代 + kill_core 取走
                // child）→ 同轮 is_ready 失败、is_alive 见 child=None 判进程死」的窗口 → 返 Dead 而非
                // Superseded。世代不等即等价让位腿：静默返回，不 kill、不 set_error、不重试。
                CoreReadyOutcome::Dead => {
                    if self.gate.generation() != my_gen {
                        log::info!("起核就绪期被接管（世代 {my_gen}，判定 Dead 系接管方拆核所致）→ 静默让位");
                        return Ok(self.status());
                    }
                    self.kill_core().await;
                    let msg = "sing-box 启动期退出".to_string();
                    // #332：核自己吐的 FATAL 才知道**为什么**退出（就绪门只看得到「没了」）。
                    let fatal =
                        self.observe_core_fatal(via_helper, startup_log_cursor, &fatal_slot);
                    // #159/#176：起核期退出（CoreStartRetryError 等价，恒可重试）→ 预算内静默重起（届时
                    // wintun 适配器/双 utun 已释放，新尝试重解析端口+重生成盘）。上面已 kill_core → 无孤儿核。
                    if attempt <= budget.max_retries {
                        log::warn!("sing-box 起核期退出（第 {attempt} 次）→ 预算内自动重试");
                        // 退避期被接管 → 让位。**此处已 `kill_core()`**（上一行）⇒ 取消腿落的是干净终态：
                        // 无残留进程、无半启动状态（status 仍是上一稳定值，由接管方的 stop 清）。
                        if self.sleep_start_backoff(&budget, attempt, my_gen).await {
                            return Ok(self.status());
                        }
                        continue;
                    }
                    let (msg, error_code) = settle_start_failure(msg, fatal);
                    self.set_error(&msg, error_code);
                    return Err(StartError::coded(msg, error_code));
                }
                CoreReadyOutcome::Timeout => {
                    if self.gate.generation() != my_gen {
                        log::info!("起核就绪期被接管（世代 {my_gen}，判定 Timeout 系接管方拆核所致）→ 静默让位");
                        return Ok(self.status());
                    }
                    self.kill_core().await;
                    let msg =
                        format!("sing-box 起核超时（管理 API {api_port} 在 {CORE_READY_TIMEOUT_MS}ms 内未就绪）");
                    // #332：超时腿同样可能是核已 FATAL 退出、只是就绪门先走完了预算（真因照样在 stderr 里）。
                    let fatal =
                        self.observe_core_fatal(via_helper, startup_log_cursor, &fatal_slot);
                    if attempt <= budget.max_retries {
                        log::warn!("sing-box 起核超时（第 {attempt} 次）→ 预算内自动重试");
                        // 同 Dead 腿：已 `kill_core()` → 取消腿无孤儿。
                        if self.sleep_start_backoff(&budget, attempt, my_gen).await {
                            return Ok(self.status());
                        }
                        continue;
                    }
                    let (msg, error_code) = settle_start_failure(msg, fatal);
                    self.set_error(&msg, error_code);
                    return Err(StartError::coded(msg, error_code));
                }
            }
        };

        // 就绪后再判一次世代：轮询末次判定与本处之间仍有窗口，接管方可能已拆核。
        if self.gate.generation() != my_gen {
            log::info!("起核就绪后被接管（世代 {my_gen}）→ 让位");
            return Ok(self.status());
        }

        // C-tun-conflict：post-flight 出口归属硬闸（仅 TUN 模式；设计 §4.2 方向①后验，D1/D2）。就绪 ≠ 夺到
        // 默认路由 —— 他方 VPN 仍占默认出口时我方 utun 抢不到流量，标 connected 是虚报（真机复现 2026-07-22）。
        // grace 内轮询出口接口，仍未从 baseline 切走 → 不标 running：kill_core + 报 TUN_ROUTE_NOT_CAPTURED。
        // 置于 running:true **之前**（D2 延后标，不做「先标再降级」的闪烁）。
        let t_tun_route = std::time::Instant::now();
        if let Err(msg) = self
            .verify_tun_route_captured(user_config.proxy_mode_type, tun_route_baseline)
            .await
        {
            // grace（数秒）内可能被接管：先复查世代，被接管则静默让位（不 kill、不 set_error，同 Dead/Timeout 腿）。
            if self.gate.generation() != my_gen {
                log::info!("TUN 出口 post-flight 期被接管（世代 {my_gen}）→ 让位，不闸");
                return Ok(self.status());
            }
            self.kill_core().await;
            self.set_error(&msg, code::TUN_ROUTE_NOT_CAPTURED);
            return Err(StartError::coded(msg, code::TUN_ROUTE_NOT_CAPTURED));
        }
        let tun_route_ms = t_tun_route.elapsed().as_millis();
        log::info!("起核耗时：TUN路由校验={tun_route_ms}ms");

        // 🔴 **自此往下 `user_config` 一律指剥除后的那份。**
        //
        // 下面三处都要按 `serverId` 反算运行核里的 outbound tag：
        //   `build_switch_snapshot`（规则热切 PUT 的目标出站）
        //   `ts_tag_to_id`（TS STATUS 帧的端点逆映射）
        //   `attest_selected_exit`（出口自证 —— `code::EXIT_MISMATCH` 是「用户以为走代理、实则
        //     明文直连」的唯一告警通道）
        // 而 `build_id_to_tag_map` 按**名字**去重、撞名追加 `(n)` ⇒ tag 是整个集合的函数。
        // 内核闸门剥掉「HK」之后，原本的「HK (1)」在运行核里就叫「HK」；这三处若拿未剥的全量
        // servers 算，得到的 tag 在运行核里根本不存在 —— 后果不是报错而是**静默错**：
        // 出口完全正确却打 EXIT_MISMATCH 假警报（告警一旦有假就会被整体无视），
        // 热切 PUT 打到不存在的出站上无声失败。
        //
        // 用遮蔽而不是逐处换参：遮蔽之后**任何**新增的下游消费点都自动拿到正确的那份，
        // 逐处换参则要求每个后来者都记得这件事。
        let user_config = effective_user_config;

        let new_status = ProxyStatus {
            running: true,
            // 读时投影字段，存储态恒 false（真值 = `start_inflight` 计数，见字段文档）。
            starting: false,
            pid,
            // 起核就绪时刻 = 运行时长的零点。**取就绪后而非 spawn 时**：就绪前核还没在服务，
            // 把 12s 就绪门算进「已运行」是虚报。与 running 同生共死（stop/set_error 经 Default 清回 None）。
            start_time: Some(now_ms()),
            // 读时投影，存储态恒 None（见 ProxyStatus 文档）。
            uptime: None,
            mixed_port,
            clash_api_port: api_port,
            // C19：暴露给更新链路消费方（resolve_update_proxy_target 据此选走 update-in 口 vs 直连）。
            update_in_port,
            // C6-5：据实际走哪条路由落面向前端的标记（helper 提权 vs 直起）。
            started_via_helper: via_helper,
            error: None,
            error_code: None,
        };
        // 热切换基准：**与 running 状态同生共死**（此处置、stop 清）→「快照在 ⟺ 核在跑」。
        // 上游 在生成期就回填，但那样起核失败时会留下描述「不存在的核」的快照；此处收紧到就绪后。
        if let Ok(mut g) = self.switch_snapshot.write() {
            *g = Some(Self::build_switch_snapshot(
                &user_config,
                &singbox_config,
                &deps,
            ));
        }
        if let Ok(mut g) = self.current_config.write() {
            *g = Some(config.clone());
        }
        if let Ok(mut snap) = self.startup_snapshot.write() {
            *snap = Some(config);
        }
        // 核刚按磁盘配置生成并起来 ⇒ 一切「保存但没进核」的欠账在这一刻结清。
        // 清点必须与 `startup_snapshot` 同刻：这两者一起定义了「运行核吃进去的是什么」。
        self.restart_deferred.store(false, Ordering::SeqCst);
        if let Ok(mut g) = self.status.write() {
            *g = new_status.clone();
        }
        // 差集的**分母侧**刚被改写 ⇒ 必须在此推一次（见 `push_pending_changes` 的「两侧」那节）。
        // 上面三行刚把 `switch_snapshot`/`startup_snapshot`/`restart_deferred` 换成「这个核吃进去的是什么」，
        // 而此前唯一的 PUSH 挂在 `switch_mode` 尾（分子侧）—— 于是**由后端自己驱动的重启**
        // （去抖重启 / 「立即应用」/ drain 排空 / 崩溃自愈）落地后没有任何一侧通知 UI 差集已清。
        // 前端那两条 pull 兜底挂在 `event:proxyStarted`/`Stopped` 上，而那两个事件**只由命令层**
        // （`commands/proxy.rs` 的 proxy_start/stop/restart）发 —— 内部驱动的重启一个都不发。
        // 后果（陈先生 2026-07-30 真机）：点「立即应用」→ 核真重启了 → 条上仍是「立即应用」，
        // 因为 store 里那份差集停在 `switch_mode` 推的最后一帧（`restartDeferred:true`），无人覆盖。
        //
        // 紧跟的 `push_lifecycle(ready)` 是同一次跃迁的另一个投影（**必须相邻**，见 `push_lifecycle`
        // 头注的配对纪律）：差集说「已经没有待应用的了」，它说「核这一次真的起来了」。少了后者，
        // 条只能靠 12s 兜底轮询才敢离开「应用中…」；而差集为空**不能**当成功信号 —— 起核失败时
        // 差集同样为空。
        self.push_pending_changes();
        self.push_lifecycle(&ProxyLifecycleEvent::ready());
        // **H3 修复接线点**：核就绪 → 后台把各 selector 的选择校正回本次 config 的意图（压过 cache_file
        // 持久化的旧选择），校正完成/放弃后才失效解锁缓存。三条时序都是承重的，见
        // [`spawn_reassert_selector_selection`](Self::spawn_reassert_selector_selection) 的方法文档：
        //  ① **spawn 而非 await**：校正最长 10×300ms ≈ 3s，挂在主链上等于给已经偏慢的起核再加 3s；
        //  ② **无条件跑**（不套「配置里有 TS 节点」的门）：cache_file 覆盖 default 与 TS 无关，任何
        //     协议的选中节点都会被上一轮的残留选择顶掉（真机血证：盘上选 Hk01、核实走 Tailscale）；
        //  ③ **失效解锁缓存 + 重探出口 IP + 连接 flush 三条一并挪进续延**（上游 F-C 与「时序修 E」）：
        //     校正可能真翻转 selector，boot 窗口内起跑的解锁轮/出口探测量的都是**旧出口**，其结果会被
        //     当新鲜数据 commit 污染缓存；而 flush 的无差别 RST 会让全部连接**立刻按旧 selector 重连** ——
        //     三条都必须等校正落定（各自的具体理由见 `after_selector_reasserted`）。
        self.spawn_reassert_selector_selection(user_config.clone(), my_gen, api_port);
        // 核就绪 → 挂后台崩溃监测（**唯一**接线点：只在真正 running 后起，让位/失败腿不挂）。
        // 监测「核意外退出」并触发崩溃自愈；主动 stop/restart 由世代区分不误触（见 `spawn_crash_monitor`）。
        self.spawn_crash_monitor(my_gen);
        // 核就绪 → 挂核日志 relay（`SubscribeLog`，同世代范式）。**无条件挂**：这是 TUN/helper 腿上
        // 日志页唯一的核日志来源，也是「改级别立刻生效、不必重启核」的承载（见方法文档）。
        // `log_pipe_handoff` 区分直起（有 stderr 管道，需交接 + 丢首帧历史）与 helper 起（无管道，收历史）。
        self.spawn_core_log_relay(my_gen, api_port, log_pipe_handoff.clone());
        // C3：核就绪 → 挂自动换节点心跳（同世代范式）。**无条件挂**，开关在循环内每 tick 读 `autoSwitchNode`
        // 动态判（对齐 上游 运行期 enable/disable）。与崩溃监测解耦：崩溃原地重启同节点，本腿只对「核活着
        // 但代理链不通」换节点。世代守卫退场同 relay。
        self.spawn_auto_switch_heartbeat(my_gen, mixed_port);
        // A3：核就绪 → 挂 Tailscale STATUS relay（同世代范式）。tag→id 从**核实际启动的这份配置**构建
        // （核发的 endpointTag 恒是它启动时的 tag）。仅当配置含 tailscale 节点时才起（无 TS 节点 = 无端点帧，
        // 白建订阅纯浪费）。停核/接管由世代守卫退场 + `stop_inner`/崩溃腿清缓存。
        if user_config.servers.iter().any(|s| {
            s.protocol == polaris_config_engine::user_config::server_config::Protocol::Tailscale
        }) {
            let tag_to_id = Arc::new(Self::ts_tag_to_id(&user_config));
            self.spawn_tailscale_status_relay(my_gen, api_port, tag_to_id);
        }
        // A4 触发点③（起核预置）**已折入上面的 selector 校正 stage 1**（上游 同款：`wantDirect` 时 PUT
        // `direct` 而非节点 tag）。此处不再单独预置 —— 两个独立写者对同一个 `proxy-selector` 各写一次，
        // 谁最后落地取决于调度，正是「flag 说已让位、selector 却指着未登录的 TS 出口」这类脱节的来源。
        log::info!("sing-box 已就绪：pid={pid} apiPort={api_port}");
        // A1：systemProxy 模式把 OS 系统代理指向本地 mixed 入站（127.0.0.1:mixedPort），否则流量不经核
        // = 表现「选直连也没启动」。放在**核已就绪之后**：核未就绪就设代理会把流量导向尚未服务的端口。
        // 与下方 residual 提示互斥（前者只在 systemProxy 生效、后者只在 tun 生效，见各自门控）。
        let t_system_proxy = std::time::Instant::now();
        self.maybe_enable_system_proxy(&user_config, mixed_port)
            .await;
        let system_proxy_ms = t_system_proxy.elapsed().as_millis();
        log::info!("起核耗时：系统代理设置={system_proxy_ms}ms");
        // **规则资源缺失告知**（T3）：本次生成真有 rule_set 被 fail-closed 剪掉 → 分流规则整段没了，
        // 用户看到的「智能分流」名不副实。放在出口自证**之前**：这是根因，出口自证是后果，后者若也
        // 命中应由它覆写 status（更贴近用户观感的「走错出口」）。两条都各自 emit 事件，互不遮蔽。
        // 空清单（资源齐全）→ 不发，零噪音。
        self.warn_pruned_rule_resources(&pruned_rule_set_tags);
        // **出口自证**：核已就绪 → 校验「实际生效出口 == 选中节点」，不一致即告警，绝不静默显示「已连接」。
        // 放在 A1 之后：二者是正交的两条降级轴（A1 = OS 没把流量导进核；本检查 = 核内部出口指错了），
        // 各自独立 emit，互不遮蔽。纯静态、零 I/O、微秒级 → 不给已经偏慢的起核路径增加任何延迟。
        self.attest_selected_exit(&user_config, &singbox_config);
        // **内核自证**：核已就绪 → 问系统「这个 pid 实际在跑哪个文件、那个文件是什么版本」，
        // 与本次期望的核对账，不一致即告警。与上面的出口自证是两条正交轴，且**判据形态刻意不同**：
        // 出口自证纯静态（意图 vs 意图），本条只吃事实（内核记账 + 真跑一次 version）——
        // 因为「app 请求 bin=A / helper 实跑 bin=B」这类分叉，静态对账天然看不见（见方法文档血证）。
        self.attest_running_core_binary(pid, &binary).await;
        // TUN 起来了 → 后台查一次「别人设的系统代理」并提示（只读不动手，见下方方法文档）。
        // 这只是 advisory、不是起核成立条件；Windows 真机首次 `reg query` 曾因系统冷态/安全软件扫描
        // 阻塞约 12s，把它 await 在主链会让网卡与路由早已就绪却仍显示「连接中」。后台腿带世代 +
        // running 守卫，停核/重连后不会补发陈旧提示。
        self.spawn_system_proxy_residual_warning(user_config.proxy_mode_type, my_gen);
        // C5：核就绪后对齐 mesh 出口路由。契约 #37「绝不抢 sing-box 路由」的让位判定在 crate 内建
        // （仅 TS System + 承载全隧道出口才装单条 ifscope default，其余 None=让位）。**OS 路由操作已全链
        // 接线**（`HelperExitRouteOp`：mac/win 经 helper `route -ifscope`、Linux `ip rule/route` 表 7732）
        // → 生产下是真手术（真机门），测试构造 `enabled=false` 诚实 no-op，见 `runtime/mesh.rs`。
        let t_mesh_route = std::time::Instant::now();
        self.mesh
            .exit_route_reconcile(&user_config, user_config.enable_ipv6.unwrap_or(false))
            .await;
        let mesh_route_ms = t_mesh_route.elapsed().as_millis();
        log::info!("起核耗时：mesh路由接线={mesh_route_ms}ms");
        // C7：TUN 起核尾接管系统 DNS（best-effort；mac 真接管、win/linux 由 `takeover_supported=false` 兜死 no-op）。
        // 门 = **TUN 模式 且 用户未关 `dnsConfig.takeoverSystemDns`**（1:1 上游 `ProxyManager.ts:1103`
        // `proxyModeType === 'tun' && config.dnsConfig?.takeoverSystemDns !== false`）。
        //
        // 两条门是**合取而非冲突**：TUN 是「什么时候技术上需要接管」（on-link 的 LAN/ISP DNS 不进 TUN →
        // hijack-dns 看不到），开关是「用户是否同意我们动系统解析器」（企业内网/自管 DNS 的用户会关）。
        // 此前开关在 Rust 侧无消费者 = 装饰开关：用户关掉后系统 DNS 照样被改写，且**关不掉也还不回来**。
        //
        // else 腿（非 TUN / 用户关了）→ 停 watcher + 还原可能残留的受控 DNS（对齐 上游 同处 else 分支）：
        // 覆盖「TUN→其它模式」与「开→关」两种切换的残留。无 marker → restore 惰性、零系统调用。
        let t_dns = std::time::Instant::now();
        if user_config.proxy_mode_type.is_tun() && dns_takeover != Some(false) {
            self.set_system_dns_best_effort().await;
            // row33：TUN 接管 DNS 后起接口热插拔 watcher（macOS `route -n monitor`；坞站/切 WiFi 出新接口
            // → 系统解析器被改回物理网卡 DHCP DNS → DNS 逃逸绕 TUN → 去抖后 reconcile 重灌）。非 mac no-op。
            self.spawn_dns_watcher();
        } else {
            self.stop_dns_watcher();
            self.restore_system_dns_best_effort().await;
        }
        let dns_ms = t_dns.elapsed().as_millis();
        log::info!("起核耗时：DNS接管={dns_ms}ms");
        // C7：核就绪尾刷 OS DNS 缓存（fire-and-forget，对齐 上游 `flushOsDnsCacheBestEffort('start')`）。
        self.flush_os_dns_cache_best_effort("start");
        // **#9** 的连接 flush 已挪进 [`after_selector_reasserted`]（selector 校正的续延），不在主链上。
        // 它本身的立意没变（app 早于 TUN 建立、已泄漏成真实 IP 的旧连接若不 RST 会继续走物理网卡直出，
        // 而用户看到的是「已连接」），只是**开枪时机**必须晚于 selector 校正：被 RST 的连接会立刻重连，
        // 重连按重连那一刻的 selector 走 —— 早于校正就等于把用户所有连接亲手踢到 cache_file 的旧出口上。
        // 「running:true 落定之后」这条原有约束依然成立且更强：续延只可能更晚。
        // 总计：各段之和（可能与总墙钟有细微差异——未逐段覆盖的边角，如 JSON 解析/规则文件预置/事件广播）
        // + 总墙钟（本次 start_inner 从入口到此处的真实耗时）。
        let segments_ms = helper_gate_ms
            + config_gen_ms
            + spawn_ms
            + ready_ms
            + tun_route_ms
            + system_proxy_ms
            + mesh_route_ms
            + dns_ms;
        log::info!(
            "起核耗时：总计={}ms（各段之和={segments_ms}ms：helper提权门={helper_gate_ms}ms \
             配置生成={config_gen_ms}ms spawn子进程={spawn_ms}ms 就绪等待={ready_ms}ms \
             TUN路由校验={tun_route_ms}ms 系统代理设置={system_proxy_ms}ms \
             mesh路由接线={mesh_route_ms}ms DNS接管={dns_ms}ms）",
            t_total.elapsed().as_millis()
        );
        Ok(new_status)
    }

    /// **C6-5**：经提权 helper 起 root/SYSTEM 受管核（TUN 路由）。移植自 上游 `startViaHelper`。
    ///
    /// 返回 `Ok(Some(pid))` = 已起（daemon 报告受管核 pid）；`Ok(None)` = 起核前被接管 → 让位；
    /// `Err` = 通信/起核失败。
    ///
    /// DESIGN-REVIEW(c6-5-src-tauri-helper-wiring)：(R27.3) 不实现 上游 #159「helper 起核失败→回退
    /// UAC/osascript 直起重试」增强腿——失败直接报错（前端 SettingsHelper 引导先装 helper）。
    /// (R27.2) 孤儿残余微竞态（stop 的 Stop 与本 Start 在 daemon 单 mu 到达序）= 真机门，与 上游 同形。
    ///
    /// **孤儿不变式**：`core_via_helper` 标记先于 IPC 置、pid 于 `start_core` 返回即提交（对齐 上游 在
    /// `startCore` 返回即置 `singboxPid`，:4430）——这样任何随后 bump 世代的 stop 的 [`kill_core`] 都能据
    /// 标记走 helper stop（daemon 摘其受管 child，无需 app 传 pid），封死「app 让位但 root 核残留」。
    /// **残余微竞态**（stop 的 Stop 与本 Start 在 daemon 单 mu 上的到达序）= 真机门（与 上游 同形）。
    async fn spawn_core_via_helper(
        self: &Arc<Self>,
        binary: &Path,
        config_path: &Path,
        user_config: &UserConfig,
        my_gen: u64,
    ) -> Result<Option<u32>, String> {
        // 让位早退（与直起临界区的「持锁判世代」同义；helper 核无本地 child 锁可持，靠世代 + 标记守）。
        if self.gate.generation() != my_gen {
            log::info!("helper 起核前被接管（世代 {my_gen}）→ 让位");
            return Ok(None);
        }
        // **受保护核对账**（换核在本条腿上真正生效的唯一途径）：helper 只会 exec 它安装期锁定的那个
        // 路径，故必须先把现役核的**内容**推进去。幂等——hash 相同即零动作、零 IPC。
        // 放在置 `core_via_helper` 标记与 IPC 之前：此刻还没有受管核，失败也不产生孤儿。
        self.reconcile_protected_core(binary).await;
        // 先于 IPC 置标记：racing stop 的 kill_core 据此走 helper stop（child 恒 None）。
        self.core_via_helper.store(true, Ordering::SeqCst);
        let log_path = self.config.join(SINGBOX_STARTUP_LOG);
        // fwd = allowLan（helper 侧开 IP 转发；上游 `forward = !!currentConfig.allowLan`）。
        let fwd = user_config.allow_lan.unwrap_or(false);
        // 父死看护：把 app pid 交 helper，app 崩溃时 helper 收割受管核（防孤儿）。
        let ppid = Some(std::process::id());
        let helper = Arc::clone(&self.helper);
        let config_path = config_path.to_path_buf();
        // HelperClient::send 是同步阻塞 IPC → 挪出 async worker 线程。
        // **不传 bin**：helper 单方面决定跑哪个二进制（见 `HelperRuntime::start_core` 文档），
        // 传了也只会被丢掉——正是本缺陷的成因。
        let started = tokio::task::spawn_blocking(move || {
            helper.start_core(&config_path, &log_path, fwd, ppid)
        })
        .await
        .map_err(|e| format!("helper 起核任务 join 失败：{e}"))?;
        let pid = match started {
            Ok(pid) => pid,
            Err(e) => {
                self.core_via_helper.store(false, Ordering::SeqCst);
                return Err(e);
            }
        };
        // 提交 pid（先于就绪等待——接管方/崩溃监测/就绪门据此探活；上游 singboxPid 于 startCore 返回即置）。
        if let Ok(mut g) = self.pid.lock() {
            *g = Some(pid);
        }
        // 上游：helper 报告已启动但进程不存在 → 判失败。
        if !pid_alive(pid) {
            self.core_via_helper.store(false, Ordering::SeqCst);
            if let Ok(mut g) = self.pid.lock() {
                *g = None;
            }
            return Err(Self::reject_helper_start(
                Arc::clone(&self.helper) as Arc<dyn HelperStopOps>,
                pid,
            )
            .await);
        }
        log::info!("helper 已起 sing-box：pid={pid}（TUN 提权路径）");
        Ok(Some(pid))
    }

    /// **受保护核对账**：把现役核推进 helper 锁定的受保护核目录（幂等；内容相同则零动作）。
    ///
    /// # 为什么在**每次**经 helper 起核前做，而不是「换核成功后推一次」
    ///
    /// 受保护核与现役核至少有四条独立的漂移路径，挂在换核事件上只能堵住第一条：
    ///  1. 在线换核 / 手动上传 / 回滚 / reset-factory；
    ///  2. **app 升级触发的重播种**（`core_paths` 的 reseed 写新随包基线进 `core_update/`）——
    ///     p101 实测正是这条：2026-07-30 12:46 重播种到 1.14.0-beta.3，而受保护核停在 7-29 装 helper
    ///     时播下的 1.14.0-alpha.45；
    ///  3. helper 装得比核晚（安装脚本的播种被 `if [ ! -x "$COREDIR/sing-box" ]` 守着，**已存在就不覆盖**，
    ///     故重装 helper 也修不好已漂移的受保护核）；
    ///  4. 用户换机器/迁移配置目录。
    ///
    /// 起核前对账把这四条一次性收口，且天然覆盖「helper 早就在跑」这个常态（不需要重启 helper：
    /// 路径不变、内容变新，helper 每次 `start` 现 spawn）。
    ///
    /// # 失败处置：只告警不阻断 —— 判定权交给下游的**事实**自证
    ///
    /// 本方法失败（IPC 挂了 / hash 不符 / 磁盘满）**不**中止起核：此刻核还没起，中止只会把
    /// 「版本可能旧」升级成「彻底连不上」。真正该不该向用户报警，由起核后的
    /// [`attest_running_core_binary`](Self::attest_running_core_binary) 按**实跑二进制**判 ——
    /// 提升失败但受保护核本来就已是新版（例如上一轮已推成功）时，报警才是噪音。
    /// 这是刻意的分工：**本方法是机制，自证是判据**。
    async fn reconcile_protected_core(&self, active_core: &Path) {
        use crate::runtime::core_promote as promote;

        if !promote::platform_has_protected_core(self.helper.platform()) {
            return; // Windows：核走 app 侧，helper 的 --singbox 即 app 侧核路径，无受保护目录。
        }
        let Some(src_dir) = active_core.parent().map(Path::to_path_buf) else {
            log::warn!(
                "现役核路径无父目录，跳过受保护核对账：{}",
                active_core.display()
            );
            return;
        };
        let core_dir = self.helper.protected_core_dir_path();
        let dest = promote::protected_core_path_in(&core_dir, std::env::consts::OS);
        let protected_payload_dir = core_dir.clone();
        let staged_dir = self.config.join(promote::CORE_PROMOTE_DIR_NAME);
        let helper = Arc::clone(&self.helper);
        let active_core = active_core.to_path_buf();

        // 全程同步 FS + 阻塞 IPC（sha256 两个 80MB 量级文件 + 可能的 30s install-core）→ spawn_blocking。
        let outcome = tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
            let src_hash = promote::sha256_file(&active_core)?;
            // 受保护核读不到（不存在 / 无权限）→ None ⇒ 判「要推」。**不吞成"已最新"**。
            let dest_hash = promote::sha256_file(&dest).ok();
            // 核同版但 Cronet 缺失/漂移也必须推：Linux helper 真正执行的是 root 受保护目录，
            // 只比 sing-box hash 会让旧安装永久缺 libcronet.so，Naive/H3 继续报依赖缺失。
            let sidecars_match = promote::sidecar_payload_matches(&src_dir, &protected_payload_dir);
            if promote::decide_promote(&src_hash, dest_hash.as_deref(), sidecars_match)
                == promote::PromoteDecision::UpToDate
            {
                return Ok(None);
            }
            let core_filename = crate::runtime::core_paths::core_filename();
            let names = promote::promote_names(&promote::list_file_names(&src_dir), core_filename);
            if names.is_empty() {
                return Err(format!("现役核目录没有可提升的文件：{}", src_dir.display()));
            }
            promote::stage_promote_dir(&src_dir, &staged_dir, &names)?;
            let r = helper.install_core(&staged_dir, &src_hash);
            // 暂存目录用完即清（硬链不占额外空间，但留着会让下一轮的"先清后建"多做一次 I/O，
            // 且用户目录里躺一个 80MB 影子核容易被误读为"又一份核"）。
            let _ = std::fs::remove_dir_all(&staged_dir);
            r.map(|()| Some(src_hash))
        })
        .await;

        match outcome {
            Ok(Ok(None)) => log::info!("受保护核已与现役核一致 → 跳过提升"),
            Ok(Ok(Some(h))) => log::info!(
                "受保护核已提升到现役核（sha256={}…）：{}",
                &h[..h.len().min(12)],
                core_dir.display()
            ),
            // 只警告不中止：判据在下游的实跑自证（见方法文档）。
            Ok(Err(e)) => log::warn!("受保护核提升失败（起核继续，由起核后自证判定是否告警）：{e}"),
            Err(e) => log::warn!("受保护核提升任务 join 失败（起核继续）：{e}"),
        }
    }

    /// **内核自证**：核就绪后校验「**实际跑起来的那个二进制**的版本 == 本次期望的核版本」。
    ///
    /// # 这一条为什么必须观测事实（血证）
    ///
    /// 同仓既有的[出口自证](Self::attest_selected_exit)是**纯静态对账**（自述「纯函数、零 I/O」
    /// 「不用探针 / 不查 selector」）：它拿本次生成的 config 与落盘的用户意图互校 —— 两个输入同源于
    /// 「意图」，故意图自洽而事实偏离时它一律判通过。今天这个缺陷正是在它眼皮底下溜过去的：
    /// app 请求 bin=`core_update/sing-box`(1.14.0-beta.3)，helper 实跑
    /// `/Library/Application Support/Polaris/core/sing-box`(1.14.0-alpha.45)，持续一天多、零告警。
    ///
    /// 故本方法**不**对账「我请求了什么 / 我配置了什么」，而是问系统两个事实问题：
    ///  1. **内核记账里，这个 pid 正在执行哪个文件？**（`running_exe_path`：linux `/proc/<pid>/exe`
    ///     符号链接、mac `ps -p <pid> -o comm=`）—— 与我们的请求完全独立的来源；
    ///  2. **那个文件自报什么版本？**（对它真跑一次 `sing-box version`）。
    ///
    /// # 判据与代价
    ///
    /// 路径相同 ⇒ 同一文件，直接通过，**零 spawn**（app 直起腿的稳态走这里）。
    /// 路径不同才各跑一次 `version`（TUN 提权腿的稳态：实跑受保护核副本，版本应相同）。
    /// 「读不出版本」判**告警**而非通过 —— 见 [`CoreBinaryAttestation::VersionUnreadable`]。
    /// 「读不到实跑 exe」判 [`Unobservable`](CoreBinaryAttestation::Unobservable)：只落 warn，
    /// **绝不写成「自证通过」**（没观测到 ≠ 观测到没问题）。
    ///
    /// [`CoreBinaryAttestation::VersionUnreadable`]: crate::runtime::core_promote::CoreBinaryAttestation::VersionUnreadable
    async fn attest_running_core_binary(&self, pid: u32, expected: &Path) {
        use crate::runtime::core_promote::{attest_core_binary, CoreBinaryAttestation};

        let expected = expected.to_path_buf();
        // 观测腿全是阻塞 syscall / 子进程 → spawn_blocking。
        let attestation = tokio::task::spawn_blocking(move || {
            let running = running_exe_path(pid);
            // 路径相同就不必花两次 spawn 去问版本（同一文件，版本必同）。
            let (ev, rv) = match running.as_deref() {
                Some(r) if r != expected.as_path() => (
                    core_version_first_line(&expected),
                    core_version_first_line(r),
                ),
                _ => (String::new(), String::new()),
            };
            attest_core_binary(&expected, running.as_deref(), &ev, &rv)
        })
        .await;

        let attestation = match attestation {
            Ok(a) => a,
            Err(e) => {
                log::warn!("内核自证任务 join 失败（未判定通过）：{e}");
                return;
            }
        };
        if attestation.is_alarm() {
            // 非终态：核确在跑，只是版本不对 → 保留 running/pid/端口，只落错误两轴 + 广播事件。
            self.set_nonfatal_error(&attestation.user_message(), code::CORE_BINARY_MISMATCH);
            return;
        }
        match attestation {
            // 「没观测到」既不是通过也不是错误：只留痕，绝不说「通过」。
            CoreBinaryAttestation::Unobservable => log::warn!("{}", attestation.user_message()),
            _ => log::info!("{}", attestation.user_message()),
        }
    }

    /// **A1 启用侧**：`systemProxy` 模式下把 OS 系统代理指向本地 mixed 入站（`127.0.0.1:mixedPort`）。
    ///
    /// **为什么必须有（根因）**：`systemProxy` 模式下核只在 `mixedPort` 上截流量。若 OS 系统代理从不设置，
    /// 应用一律直连、根本不经本地 sing-box——即便出口选 direct，用户也会看到「选直连也没启动」。这是
    /// 上游 `ProxyManager` 在 `systemProxy` 模式 start 成功后 `enableSystemProxy` 的等价接线（此前 Rust 侧
    /// **只移植了清除侧、缺启用侧** = 本批要补的最大缺口）。
    ///
    /// **参数**：`address` 恒 `127.0.0.1`——本机应用经 loopback 连本地核；`allow_lan` 只改入站 bind 地址
    /// （`::` vs `127.0.0.1`），不改本机代理指向。`http_port`/`socks_port` 均取 `mixedPort`（mixed 入站同口
    /// 服务 HTTP/SOCKS）。`bypass_list` 复用 config-engine 纯逻辑 [`effective_bypass_lan`]（缺省补 27 条
    /// `DEFAULT_BYPASS_LAN`，总开关关 → `[]`），与内核侧 route/bypass 同一真值，不另写一份。
    ///
    /// **marker**：`enable` 内部前置写 `system-proxy.marker.json`（`our_host_port` = `address:http_port`）标
    /// 属主——供 [`stop`](Self::stop) / start 失败腿 / [`recover_system_proxy_on_startup`] 识别「这代理是我们
    /// 设的」而安全清除（不 stomp 用户自配）。
    ///
    /// **失败处置**：`enable` 是 fail-closed（`set_proxy` 失败自回滚 + 清 marker）。此处失败**不**走
    /// `set_error`（核确在运行，把它标成 not-running 是虚报），而走
    /// [`set_nonfatal_error`](Self::set_nonfatal_error) + `code::SYSTEM_PROXY_FAILED` —— 落状态并广播
    /// `event:proxyError`，让用户知道「代理已起但系统代理没设上，流量未经核」。
    /// 原 review-queue 条目 `a1-enable-failure-surface` 问的正是「是否额外 emit 前端提示」，
    /// **已答：emit**（见下方两条失败腿；此前只 `log::error!` 时用户看到的是绿灯 + 全量直连 + 零提示）。
    ///
    /// 装配同 [`maybe_warn_system_proxy_residual`](Self::maybe_warn_system_proxy_residual)：同步控制器经
    /// `spawn_blocking` 调用，锁绝不跨 await。
    async fn maybe_enable_system_proxy(&self, user_config: &UserConfig, mixed_port: u16) {
        if !should_enable_system_proxy(user_config.proxy_mode_type) {
            return;
        }
        // bypass 生效清单：复用 config-engine 纯逻辑（缺省补默认 27 条 / 总开关关 → []），不重写一份。
        struct BypassCfg<'a>(&'a UserConfig);
        impl BypassConfig for BypassCfg<'_> {
            fn bypass_lan(&self) -> Option<bool> {
                self.0.bypass_lan
            }
            fn bypass_lan_list(&self) -> Option<&[String]> {
                self.0.bypass_lan_list.as_deref()
            }
        }
        let req = ProxyEnableRequest {
            address: "127.0.0.1".to_string(),
            http_port: mixed_port,
            socks_port: mixed_port,
            bypass_list: effective_bypass_lan(&BypassCfg(user_config)),
        };
        let clearer = Arc::clone(&self.proxy_clearer);
        // enable 是 `&mut self` 同步 API（会 exec `networksetup`/`gsettings`/`reg`）→ spawn_blocking 内
        // 短暂持锁调用，锁绝不跨 await。
        let outcome = tokio::task::spawn_blocking(move || {
            clearer
                .lock()
                .map(|mut g| g.enable_system_proxy(&req))
                .unwrap_or_else(|e| {
                    log::error!("proxy_clearer 锁中毒: {e} → 跳过系统代理启用");
                    Err("proxy_clearer 锁中毒".to_string())
                })
        })
        .await;
        match outcome {
            Ok(Ok(())) => {
                log::info!(
                    "系统代理已指向本地 mixed 入站（127.0.0.1:{mixed_port}）→ 流量经本地核（A1）"
                );
            }
            // A1 失败 = 核在跑但**流量根本不经核**（明文直连）。此前只 log::error! → 用户看到的是
            // 「已连接」绿灯 + 全量直连 + 零提示。改为落状态 + 广播（非终态，核确在运行）。
            Ok(Err(e)) => {
                self.set_nonfatal_error(
                    &format!("系统代理启用失败，流量未经代理（当前为直连）：{e}"),
                    code::SYSTEM_PROXY_FAILED,
                );
            }
            // join 失败 = 我们**不知道**系统代理设没设上 → 同样按「可能未生效」冒给用户。
            // 「不确定」与「确定失败」对用户的行动含义一致（都得去查系统代理），不为区分二者留一条静默腿。
            Err(e) => {
                self.set_nonfatal_error(
                    &format!("系统代理启用结果未知，流量可能未经代理：{e}"),
                    code::SYSTEM_PROXY_FAILED,
                );
            }
        }
    }

    /// **规则资源缺失** → 用户可见信号（`RULE_RESOURCES_MISSING`）。
    ///
    /// 入参是剪枝点交回的悬空 tag 清单（[`polaris_config_engine::builder::route::RouteConfigOutcome`]）——
    /// **不是**猜出来的：资源齐全时它恒空，故「只在真的发生剪枝时发」由数据本身保证，无需另加门控。
    ///
    /// 非终态（核确在跑，只是分流退化）→ `set_nonfatal_error`，保留 `running/pid/端口`。
    ///
    /// **文案里的「到「规则资源」页下载」对内置 tag 也成立**，靠的是 `builder/route.rs` 内置注入腿在
    /// `<userData>/rules/` 缺失时回落 `<userData>/rule-resource/`（catalog id 与 builtin tag 同形）。
    /// 那条回落腿若被删，本文案对 `geosite-cn`/`geoip-cn` 就重新变成死路——两者必须一起改。
    fn warn_pruned_rule_resources(&self, pruned: &[String]) {
        if pruned.is_empty() {
            return;
        }
        self.set_nonfatal_error(
            &format!(
                "规则资源 {} 缺少本地副本，引用它们的分流规则本次已被跳过（分流将不完整）。请到「规则资源」页下载后重连恢复。",
                pruned.join("、")
            ),
            code::RULE_RESOURCES_MISSING,
        );
    }

    /// **出口自证**：核就绪后校验「实际生效出口 == 用户选中节点」，不一致即经同一 error/warn 通道告警。
    ///
    /// 判据与「为什么不用探针 / 不查 selector」见 [`attest_effective_exit`] 上方的模块级说明。
    ///
    /// **不增启动延迟**：本方法是纯函数 + 一次 `ConfigManager::current()`（命中内存缓存的 RwLock 读，
    /// 不碰磁盘、不碰网络、不 spawn、不 await），耗时微秒量级 → 直接内联在就绪后调用即可，既无需
    /// 放后台也无需超时兜底。这是选静态对账而非探针的**直接收益**：探针要一整个网络 RTT，本检查不要。
    ///
    /// **绝不静默**：`Match` 以外的每个变体都落 `set_nonfatal_error`（非终态——核确在跑），从而
    /// 同时落 `status.error/errorCode` 与广播 `event:proxyError`。
    fn attest_selected_exit(&self, user_config: &UserConfig, singbox_config: &SingBoxConfig) {
        // 落盘的用户意图（单一真值）。读不到（首启/损坏）→ None → 退化为「只做配置内部自洽对账」，
        // 而**不是**跳过整个自证：拿不到意图不等于出口没问题。
        let persisted = self.config.current().ok().and_then(|c| {
            c.get("selectedServerId")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        match attest_effective_exit(user_config, singbox_config, persisted.as_deref()) {
            ExitAttestation::Match => {
                log::info!("出口自证通过：实际生效出口 == 选中节点");
            }
            other => self.set_nonfatal_error(&other.user_message(), code::EXIT_MISMATCH),
        }
    }

    /// TUN 起核成功后，检测「无 marker 的系统代理残留」并发一次性提示（`event:systemProxyResidual`）。
    ///
    /// **为什么只提示不清**：无 marker ⟺ 不是我们设的 ⟺ 用户自配或别的工具设的。marker 门控的全部
    /// 立意就是「绝不 stomp 用户自配」（契约 L138），所以这里**读到了也不动手**，只把事实交给用户。
    /// 前端文案 `settings.systemProxyResidualDesc`（「…如非必要建议关闭」）也正是这个语气。
    ///
    /// **为什么限 TUN**：系统代理模式下系统代理本就该开着（且那是我们设的、有 marker）；手动模式下
    /// 用户自负分流。只有 TUN 模式「本该全量接管」却被系统代理旁路，才构成用户看不懂的异常。
    ///
    /// **为什么每会话只发一次**：这是 advisory 不是告警，且状态在一次会话内基本不变；每次起核
    /// （含崩溃自愈重启）都弹 = 噪音。`residual_warned` 门闩同 `stale_sweep_disabled` 范式。
    ///
    /// `my_gen=Some(..)` 是起核后台腿：探测完成时须复核仍是同一代、核仍在跑，防止慢 `reg query` /
    /// `networksetup` 在 stop 或下一轮 start 之后补发陈旧提示。`None` 只供不拉真核的决策单测使用。
    async fn maybe_warn_system_proxy_residual(&self, mode: ProxyModeType, my_gen: Option<u64>) {
        if !mode.is_tun() {
            return;
        }
        if self.residual_warned.load(Ordering::SeqCst) {
            return; // 本会话已有一条有效探测消费过门闩
        }
        let clearer = Arc::clone(&self.proxy_clearer);
        // detect_foreign_proxy 是**同步**只读 API，但会 exec `networksetup`/`gsettings`/`reg`
        // → 同 clear_system_proxy，经 spawn_blocking 调用，锁绝不跨 await。
        let found = tokio::task::spawn_blocking(move || {
            clearer
                .lock()
                .map(|g| g.detect_foreign_proxy())
                .unwrap_or_else(|e| {
                    log::error!("proxy_clearer 锁中毒: {e} → 跳过系统代理残留检测");
                    None
                })
        })
        .await;
        if my_gen.is_some_and(|gen| self.gate.generation() != gen || !self.status().running) {
            log::debug!("系统代理残留检测完成时起核世代已失效或核已停止 → 丢弃陈旧结果");
            return;
        }
        // 到这里才消费「本会话已检查」门闩：若慢探测跨过 stop/restart 变成陈旧结果，不能让旧世代
        // 抢先置位，导致新一代永远失去残留代理提示。正常每代只 spawn 一次；swap 仍兜住极端并发，
        // 只有第一条有效结果可以 emit。
        if self.residual_warned.swap(true, Ordering::SeqCst) {
            return;
        }
        match found {
            Ok(Some(proxy)) => {
                log::info!("TUN 模式下检测到非 Polaris 设置的系统代理（{proxy}）→ 提示用户");
                match self.error_emitter.get() {
                    Some(e) => e.emit_system_proxy_residual(&proxy),
                    None => log::debug!("emitter 未接线 → 跳过 event:systemProxyResidual"),
                }
            }
            Ok(None) => {}
            Err(e) => log::error!("系统代理残留检测 spawn_blocking join 失败: {e}"),
        }
    }

    /// 把 advisory 探测移出起核关键路径。Windows 真机冷态的两个 `reg query` 曾耗约 12s；
    /// 连接正确性不依赖这份只读结果，故不能让它延迟 `proxy_start` 的成功回包。
    fn spawn_system_proxy_residual_warning(self: &Arc<Self>, mode: ProxyModeType, my_gen: u64) {
        if !mode.is_tun() {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            me.maybe_warn_system_proxy_residual(mode, Some(my_gen))
                .await;
            log::info!(
                "后台系统代理残留检测耗时={}ms（不阻塞起核）",
                started.elapsed().as_millis()
            );
        });
    }

    /// 起核时刻建热切换基准快照（上游 在 generateSingBoxConfig / startInternal 内回填三个 `this.*`）。
    ///
    /// 三份基准各自的真值来源（**逐条对齐 上游，不自创**）：
    /// - `id_to_tag`：`build_id_to_tag_map(servers)` —— 与 上游 :3480 同一函数、同一入参。
    ///   注：`build_outbounds` 内部另持一份**可变**副本（detour 死引用剔除会删 entry），但 上游的
    ///   `currentIdToTagMap` 存的正是**未剔除**的那份，且 config-engine 的 `generate.rs:204` 也用它
    ///   喂 route/dns → 此处保持一致。
    /// - `rule_target`：`build_outbounds` 产的 `pending_rule_selectors`，**再按「该 selector 是否真的
    ///   存在于生成出来的 outbounds」过滤** —— 1:1 复刻 上游 :3601-3610 的 `liveSelectorTags` 过滤
    ///   （detour 死引用剔除可能删空 rule-sel → 该 entry 不进 map）。
    /// - `fingerprints`：`server_fingerprint` 逐节点（上游 :672 `computeServersFingerprint`）。
    ///
    /// **为什么要重跑一次 `build_outbounds`**：`generate_sing_box_config` 只返回 `SingBoxConfig`，
    /// 不外露 `pending_rule_selectors`（上游 靠 `this.pendingRuleSelectors` 实例态取，Rust 侧是纯函数
    /// 无实例态），而 config-engine 本批**只读复用不可改签名** → 只能用其公开的 `build_outbounds` 重算。
    /// 重算与生成的唯一入参差异是 `with_race_off`（私有，无法调用），它只改
    /// `dnsConfig.resolveNodeDomainsAhead` → 仅影响节点 outbound 的 `domain_resolver`，**不改任何 tag
    /// 集合**，故 `pending_rule_selectors` 不受影响。这个「不受影响」不靠推断背书：live-selector 过滤
    /// 拿**真实生成产物**当裁判，重算若与产物不一致，对应 entry 直接出局。
    fn build_switch_snapshot(
        user_config: &UserConfig,
        singbox_config: &SingBoxConfig,
        deps: &GenerateConfigDeps,
    ) -> SwitchSnapshot {
        // ── id→tag ──
        struct SrvLike<'a>(&'a polaris_config_engine::user_config::server_config::ServerConfig);
        impl ServerLike for SrvLike<'_> {
            fn id(&self) -> &str {
                &self.0.id
            }
            fn name(&self) -> &str {
                &self.0.name
            }
        }
        let wrappers: Vec<SrvLike> = user_config.servers.iter().map(SrvLike).collect();
        let id_to_tag = build_id_to_tag_map(&wrappers);

        // ── 节点指纹（两张表，两个问题；公式单点在 runtime::node_fingerprints，见其模块文档）──
        // ① 全维：喂 switch-engine 重启判据 + pending_changes().modified。
        let fingerprints = node_fingerprints::modified_table(&user_config.servers);
        // ② 5 维：喂测速 partition_dirty 的「旧」侧。必须与 speedtest 的「新」侧同公式，否则恒不等。
        let dirty_fingerprints = node_fingerprints::dirty_table(&user_config.servers);

        // ── rule-sel 映射（重算 + live 过滤）──
        // OutboundsDeps 逐字段镜像 config-engine `generate.rs:208-219`；漏一个字段就可能算出与运行核
        // 不同的 selector 集合（→ 被 live 过滤兜住，退化为「该规则不热切」而非 PUT 到错的 selector）。
        let system_interface_available = matches!(
            user_config.proxy_mode_type,
            polaris_config_engine::user_config::ProxyModeType::Tun
        )
            && polaris_config_engine::builder::endpoint_routes::mesh_system_supported_on_platform(
                &deps.platform,
            );
        let mut outbounds_deps = OutboundsDeps {
            platform: deps.platform.clone(),
            arch: deps.arch.clone(),
            // 类型随 `OutboundsDeps` 由 `BTreeSet<String>` 改为 `BTreeMap<String, &'static str>`
            // （值 = 剔除原因 token，供 UI 说清「这个节点为什么不可用」）。此处是**预置为空**的
            // 入参，语义未变：`build_outbounds` 只往里写，不读预置内容。
            gate_invalid_nodes: std::collections::BTreeMap::new(),
            system_interface_available,
            probe_pool_ports: deps.probe_pool_ports.clone(),
            tailscale_state_dir_prefix: deps.tailscale_state_dir_prefix.clone(),
            has_cronet_lib: deps.has_cronet,
            log: deps.log,
        };
        let rule_target = match build_outbounds(user_config, &mut outbounds_deps) {
            Ok(res) => {
                // live 裁判：真实生成产物里仍在的 selector tag（上游 liveSelectorTags）。
                let live: BTreeSet<&str> = singbox_config
                    .outbounds
                    .iter()
                    .filter(|o| o.type_field == "selector")
                    .map(|o| o.tag.as_str())
                    .collect();
                res.pending_rule_selectors
                    .into_iter()
                    .filter(|r| live.contains(r.selector_tag.as_str()))
                    .map(|r| {
                        (
                            r.rule_key,
                            RuleTargetEntry {
                                selector_tag: r.selector_tag,
                                member_tag: r.member_tag,
                            },
                        )
                    })
                    .collect()
            }
            // 重算失败（生成已成功却重算报错 = 二者已分叉）→ 空 map。空 map ≠ None：
            // 空 map 下规则热切换查不到 entry → 跳过该规则（上游 同款语义）；而 id_to_tag 仍在 →
            // 全局切节点仍可热切。分叉本身响亮记日志。
            Err(e) => {
                log::warn!("rule-sel 快照重算失败（规则热切换将退化为跳过）: {e}");
                BTreeMap::new()
            }
        };

        SwitchSnapshot {
            id_to_tag,
            rule_target,
            fingerprints,
            dirty_fingerprints,
            // §15：与运行核 config 同源（deps.probe_pool_ports 正是本次 generate 注入的池端口）→ 快照即池真值。
            probe_pool_ports: deps.probe_pool_ports.clone(),
        }
    }

    /// 就绪门接线：把真实 I/O（TCP 探测 / 子进程存活 / sleep / 世代比对）注入 core-supervisor 的
    /// [`wait_for_core_ready`]。**轮询/判定顺序/boundary check 全在 crate 内，本处不重写**。
    ///
    /// **诊断慢起轴喂数点**（维度7 #11）：本次 start 的就绪重试经 `on_retry` 逐次
    /// [`StartAttempt::record_retry`](polaris_stats_engine::StartAttempt)，成功（`Ready`）后
    /// [`finish_start`](DiagnosticCounters::finish_start) 落库到 `last_start_ready_retries`
    /// （上游 :906/:1012）。失败/让位腿不落库 → 保留上次成功值（该字段义为「最近一次成功起核的就绪重试数」）。
    async fn wait_ready(&self, api_port: u16, my_gen: u64) -> CoreReadyOutcome {
        // 分段耗时测量：本函数总耗时 + TCP 探测轮询轮数（`is_ready` 每被调一次计一轮）。
        // 纯观测计数器，不改判定/不改返回值——与下方既有 `on_retry` 诊断喂数点同一手法。
        let t_wait_ready = std::time::Instant::now();
        let ready_poll_count = Arc::new(AtomicU32::new(0));
        let child = self.child.clone();
        let gate = self.gate.clone();
        // 轮询 sleep 的取消腿：与 `is_superseded` 同一个 gate（同一真值），外加唤醒边沿。
        let gate_for_sleep = self.gate.clone();
        let signal_for_sleep = self.gen_changed.clone();
        // C6-5：helper 起核无本地 child 句柄 → 就绪门探活改用 pid（对齐 上游 `isAlive:()=>isProcessAlive(pid)`）。
        // 直起路径 child.try_wait 不变。pid 已在 spawn（两路径）提交 → 此处读定值。
        let via_helper = self.core_via_helper.load(Ordering::SeqCst);
        let helper_pid = if via_helper {
            self.pid.lock().ok().and_then(|g| *g)
        } else {
            None
        };
        // 慢起轴：本次 start 的就绪重试累计句柄（begin_start → on_retry 累计 → 成功 finish_start 落库）。
        let attempt = Arc::new(Mutex::new(self.diag_lock().begin_start()));
        let attempt_cb = Arc::clone(&attempt);
        let deps = CoreReadyDeps {
            // 子进程存活：直起走 try_wait 非阻塞收割（Ok(None)=仍在跑；child 被 stop 取走→不活）；
            // helper 核走 pid 探活（kill(pid,0)，root 核跨用户 EPERM 亦判活）。
            is_alive: Box::new(move || {
                if via_helper {
                    return helper_pid.is_some_and(pid_alive);
                }
                let Ok(mut g) = child.lock() else {
                    return false;
                };
                match g.as_mut() {
                    Some(c) => matches!(c.try_wait(), Ok(None)),
                    None => false,
                }
            }),
            // 就绪信号：管理 API 端口 TCP 可连（core-readiness.ts 原义；该口是 h2c gRPC，无 REST 可判）。
            is_ready: Box::new({
                let ready_poll_count = Arc::clone(&ready_poll_count);
                move || {
                    ready_poll_count.fetch_add(1, Ordering::Relaxed);
                    Box::pin(async move {
                        matches!(
                            tokio::time::timeout(
                                READY_PROBE_TIMEOUT,
                                tokio::net::TcpStream::connect(("127.0.0.1", api_port)),
                            )
                            .await,
                            Ok(Ok(_))
                        )
                    })
                }
            }),
            // 轮询间隔 sleep **可被取消中断**：睡到一半世代变了就立刻醒，下一轮轮首的 `is_superseded`
            // 当场判 Superseded。「等待本身可中断」这条不变式在此处也要成立，不能只守退避那一处。
            //
            // **诚实标注射程**：`CORE_READY_POLL_MS` 现为 50ms，所以这一处今天只省下 ≤50ms —— 变异实测
            // （换回裸 `tokio::time::sleep`）**杀不动任何测试**，本改动是不变式对齐 + 防将来把 poll 调大，
            // 不是当前那 35s 的成因。真正的成因是退避那一处（2s/4s，有测锁死）。
            sleep: Box::new(move |d| {
                let gate = Arc::clone(&gate_for_sleep);
                let signal = Arc::clone(&signal_for_sleep);
                Box::pin(async move {
                    sleep_unless_superseded_on(&gate, &signal, my_gen, d).await;
                })
            }),
            // #176 让位判据：世代变了即被更新的 start/stop 接管。
            is_superseded: Some(Box::new(move || gate.generation() != my_gen)),
            // 慢起轴喂数：每次就绪重试累计一次（上游 onRetry，:906）。纯观测，不改就绪判定。
            on_retry: Some(Box::new(move || {
                if let Ok(mut a) = attempt_cb.lock() {
                    a.record_retry();
                }
            })),
        };
        let outcome = wait_for_core_ready(
            WaitForCoreReadyOptions {
                timeout_ms: CORE_READY_TIMEOUT_MS,
                poll_ms: CORE_READY_POLL_MS,
            },
            &deps,
        )
        .await;
        // 轮询类段：只记总等待时长 + 轮询轮数，不逐轮打点刷屏。
        log::info!(
            "起核耗时：就绪等待={}ms 轮询轮数={} 结果={outcome:?}",
            t_wait_ready.elapsed().as_millis(),
            ready_poll_count.load(Ordering::Relaxed)
        );
        // 成功起核 → 把本次累计的就绪重试落库（上游 :1012）。失败/让位不落库（保留上次成功值）。
        if outcome == CoreReadyOutcome::Ready {
            if let Ok(a) = attempt.lock() {
                self.diag_lock().finish_start(&a);
            }
        }
        outcome
    }

    /// C-tun-conflict：起核**前**快照「应走代理的公网目的」出口接口（post-flight 差分锚点）。
    ///
    /// 非 TUN 模式 → `None`（不设闸，见 [`tun_route_gate_applies`]）。TUN 模式经 `spawn_blocking` 跑
    /// [`tun_exit_interface_for_probe`]（同步系统查询不阻塞 async runtime）；读失败 → `None`
    /// （判定层按「不可断言」不闸，避免假阳性）。**必须在任何 spawn 之前**：此刻我方 utun 尚未上线，
    /// 查到的是「Polaris 起核前」的出口（物理网卡或他方 VPN 的 utun）——差分的基准。
    async fn capture_tun_route_baseline(&self, mode: ProxyModeType) -> Option<String> {
        if !tun_route_gate_applies(mode) {
            return None;
        }
        let iface = tokio::task::spawn_blocking(|| tun_exit_interface_for_probe().ok().flatten())
            .await
            .unwrap_or(None);
        log::info!("TUN 出口 baseline（起核前 {ROUTE_PROBE_IP} 出口）= {iface:?}");
        iface
    }

    /// C-tun-conflict：起核就绪**后**的出口归属硬闸（方向①后验；设计 §4.2）。
    ///
    /// 就绪门只验「进程活 + 管理 API 环回口可连」，**不验默认路由归属** → 其他 VPN 占着默认路由时，
    /// 我方 utun 抢不到流量却照样判就绪 = 假报「已连接」。此处在 grace 窗口内轮询出口接口，按 baseline
    /// 差分判定是否真夺到路由：
    /// - 非 TUN 模式 / baseline 不可读 / grace 内探到出口切走 → `Ok(())`（放行）。
    /// - grace 耗尽出口仍 == baseline（他方 VPN 占路由 / 我方路由装失败，一网打尽）→ `Err(msg)`
    ///   （调用方 `kill_core` + `set_error(TUN_ROUTE_NOT_CAPTURED)` 拒绝标 running；设计 D1 硬闸 / D2 延后标）。
    ///
    /// 探测 + grace sleep 全在 `spawn_blocking`（同步 CommandRunner + `thread::sleep`），不占 async runtime。
    async fn verify_tun_route_captured(
        &self,
        mode: ProxyModeType,
        baseline: Option<String>,
    ) -> Result<(), String> {
        if !tun_route_gate_applies(mode) {
            return Ok(());
        }
        let outcome = tokio::task::spawn_blocking(move || {
            verify_exit_captured(
                baseline,
                TUN_ROUTE_GRACE_POLLS,
                tun_exit_interface_for_probe,
                || std::thread::sleep(TUN_ROUTE_POLL_INTERVAL),
            )
        })
        .await
        .unwrap_or(ExitCaptureOutcome::Indeterminate);

        match outcome {
            ExitCaptureOutcome::Captured { interface } => {
                log::info!("TUN 出口夺取成功：{ROUTE_PROBE_IP} 出口已切到 {interface:?}");
                Ok(())
            }
            // 不可断言（baseline/探测不可读）→ 不闸：宁可漏检也不误拦正常起核（设计 §4.7）。
            ExitCaptureOutcome::Indeterminate => {
                log::warn!(
                    "TUN 出口 post-flight 不可断言（baseline/探测不可读）→ 不闸，按 caveat 放行"
                );
                Ok(())
            }
            ExitCaptureOutcome::NotCaptured { baseline, last } => {
                log::error!(
                    "TUN 出口未夺到：grace 内 {ROUTE_PROBE_IP} 出口始终未从 baseline 切走\
                     （baseline={baseline} last={last}）→ 硬闸拒绝标 connected"
                );
                Err(TUN_ROUTE_NOT_CAPTURED_MSG.to_string())
            }
        }
    }

    /// 停止 sing-box（上游 `proxy:stop`）——**主动停止终态**：停核 ＋ 清系统代理（维度7 #8 对称面）。
    ///
    /// = [`stop_inner`](Self::stop_inner)（停核 + 清状态/快照）＋ 系统代理收口。
    ///
    /// **为什么主动停止必须清系统代理**：系统代理若由我们设置且仍指向刚被杀的本地端口，停核后它就指向
    /// 一个死端口 → 用户全网断连、需手动改回。这是 start 失败腿
    /// （[`maybe_clear_system_proxy_on_start_failure`](Self::maybe_clear_system_proxy_on_start_failure)）的
    /// **对称面**：那条守「起核失败别留死端口」，这条守「主动停核别留死端口」。
    ///
    /// **guard 复用同一 marker 门控**：`clear_system_proxy` → `ensure_cleared` 门控 1「无 marker 即 no-op」
    /// —— 系统代理非我方设置（或已清）绝不动手，不误清用户自配的第三方代理。清理失败只记日志、不 panic、
    /// 不阻断停止（`stop` 恒返回 `Ok`）。
    ///
    /// **只挂主动停止腿，不挂 restart 的停核腿**：`restart` = stop→start 是**瞬态**停核，紧接 start 重建，
    /// 清了会在重建前留下「无系统代理」窗口（对齐上游 `ensureSystemProxyCleared` 首行
    /// `if (this.stopping) return`）。restart 若在 start 腿失败留下死端口，由上面的 start 失败腿收口，不由
    /// 此处。故 [`restart`](Self::restart) 调 [`stop_inner`](Self::stop_inner) 而非本方法。
    /// **换代即让位**：`stop_inner` 返 `false` 表示本腿在停核期间已被更新的 start/stop 接管
    /// （见该方法的换代守卫）。此时系统代理**属接管方**——清它就是把新会话刚设好的代理抹掉、
    /// 用户全网走直连。故这条收口也一并让位，由接管方自己的终态负责。
    pub async fn stop(self: &Arc<Self>) -> Result<(), String> {
        if self.stop_inner().await {
            // 维度7 #8 对称收口（见方法文档）：marker 门控幂等，失败只记日志不阻断停止。
            self.clear_system_proxy().await;
        }
        Ok(())
    }

    /// 停核主体（**不含系统代理收口**）：世代 +1 → kill → 清状态/快照 → `end(Stop)` 丢弃 pending。
    ///
    /// 1. 世代 +1（接管在飞的 start：其就绪门即刻让位）
    /// 2. kill 进程（core-supervisor `ProcessKiller`：SIGTERM → 宽限 → SIGKILL）
    /// 3. 清状态 + 快照；`end(Stop)` 丢弃全部 pending（停止优先）
    ///
    /// **[`restart`](Self::restart) 复用本腿**（瞬态停核，紧接 start 重建，故**不**清系统代理——见
    /// [`stop`](Self::stop)）。
    ///
    /// # 返回值 = 「本腿跑完了拆除、且仍当权」
    ///
    /// `false` = 拆除中途发现已被更新的 start/stop 接管，余下步骤整段让位（见下面的换代守卫）。
    /// [`stop`](Self::stop) 据此决定要不要做系统代理收口。
    ///
    /// # 换代守卫：超预算残 stop 的**晚落地换代毒性**
    ///
    /// 本腿的每一个 await 都可能挂到分钟级：`kill_core` 的 SIGTERM→5s 宽限→SIGKILL / 经 helper 停核的
    /// 阻塞 IPC（`spawn_blocking` 可被饥饿）、`restore_system_dns_best_effort` 的两次系统 exec
    /// （macOS 上 `networksetup` 卡死有实证）。而 `commands::helper::helper_uninstall` 的看门狗收停是
    /// **有预算**的（`WATCHDOG_JOIN_BUDGET`）：超预算后命令直接返回，那次 `proxy.stop()` 变成**残任务**
    /// 继续挂着。用户此时完全可能重装 helper 并起一个新核 —— 残 stop 随后醒来，后半段每一步都在改
    /// **当前会话**的共享态：`clear_race_server()`（`None` 腿无条件清）会抹掉新核的 sidecar 注入态
    /// （节点域名解析静默 SERVFAIL）、`status = default` 抹掉新核的 running 态、`restore_system_dns` 把
    /// 新核接管的 DNS 还原掉、`mesh.exit_route_clear()` 连带取消新会话在飞的出口路由作业。
    ///
    /// 判据用**本腿自己 bump 出来的世代**（`bump_generation` 返回值）：全仓只有 `start` / `stop` 两个
    /// 入口 bump 世代 ⇒ 「世代变了」⟺「有更新的 start 或 stop 接管了」，两种情况都该让位（接管方是
    /// start ⇒ 不许碰它的态；接管方是 stop ⇒ 该做的它自己会做）。
    ///
    /// 检查点摆在**每个 await 之后**（不多不少）：同步语句之间不存在别的任务插入的可能，唯一能发生
    /// 换代的位置就是 await 让出执行权的那些点。`ts_exit_recover_once_order_is_reapply_reassert_refresh`
    /// 同款范式；本腿的配对扫描见 `stop_teardown_guard`。
    ///
    /// 让位路径**照样 `finish_lifecycle(Stop)`**：`gate.begin()` 与 `end()` 必须配对，漏掉即
    /// `LifecycleGate` depth 永久 >0 ⇒ 此后每一次 switch_mode / 去抖重启都只置 pending 不执行
    /// （`commands::helper::join_watchdog_cooperatively` 文档里记的那条最重后果）。
    async fn stop_inner(self: &Arc<Self>) -> bool {
        // 必须先 bump（早于取 child 锁）：与 start 的「持锁判世代」共同封死孤儿窗口。
        // 走 [`bump_generation`](Self::bump_generation) 而非 `gate.bump_generation()`：同一次调用里
        // 唤醒在飞起核腿，**取消当场生效**而不是等它退避睡满（这就是「点了立刻停」的那一下）。
        let my_gen = self.bump_generation();
        self.gate.begin();
        self.kill_core().await;
        if self.stop_superseded(my_gen, "kill_core") {
            self.finish_lifecycle(LifecycleKind::Stop);
            return false;
        }
        // C5：停核 → TS 内核接口随之拆除 → 清理出口路由（真装过才发 route del；未装成 / 测试构造
        // `enabled=false` 下 installed 恒 None → clear_inner 早退 = 纯 no-op）。
        self.mesh.exit_route_clear().await;
        if self.stop_superseded(my_gen, "exit_route_clear") {
            self.finish_lifecycle(LifecycleKind::Stop);
            return false;
        }
        // R2：停核 → 复位 TS 出口无效直判的翻转对账缓存（新会话首帧须能重新触发 none→blocked，
        // 对齐 上游 会话起点 `lastTsExitBlock = null`）。
        self.reset_ts_exit_block_state();
        // A3：停核 → STATUS 流不再 live → 清 TS 状态末帧缓存（陈旧 live 数据不再供 tailscale_get_status）。
        // relay 任务本身由世代守卫（`stop_inner` 已 bump 世代）自行退场；此处只清缓存。
        self.mesh.clear_ts_status();
        // A4：停核 → 复位登录期出口让位内存态 + 撤 UI（若在让位中）。不切 selector（核已停）。
        self.reset_login_fallback_state();
        // C11：停核 → 停 race sidecar + 清注入态（sidecar 绑主核生命周期；下次起核按新配置重建）。
        self.clear_race_server();
        // row33：停核 → 停 DNS 接口热插拔 watcher（abort route -n monitor）。先于还原 DNS：避免 watcher 在
        // 还原窗口里看到链路事件又重灌（幂等无害，但停在前更干净）。
        self.stop_dns_watcher();
        // C7：停核 → 还原系统 DNS（best-effort；无接管 marker → 惰性，win/linux 只清残留 marker 不写系统）。
        // 对齐 上游 `stopSystemDns`（restoreDns）。放在刷缓存之前：先把系统解析器还原，再清缓存里的旧记录。
        self.restore_system_dns_best_effort().await;
        if self.stop_superseded(my_gen, "restore_system_dns") {
            self.finish_lifecycle(LifecycleKind::Stop);
            return false;
        }
        // C7：停核尾刷 OS DNS 缓存（fire-and-forget，对齐 上游 `flushOsDnsCacheBestEffort('stop')`）。
        self.flush_os_dns_cache_best_effort("stop");
        if let Ok(mut g) = self.status.write() {
            *g = ProxyStatus::default();
        }
        // 核停（出口隧道下线）→ 失效解锁缓存：清缓存 + 广播 `{running:false}`，让渲染端复位 idle（不再 serve
        // 停核前的陈旧解锁快照）。`unlock_get` 的停核短路是自证腿，此处显式失效并广播使 UI 即时复位、不等下次挂载。
        self.invalidate_unlock_cache(false, false);
        // 核停（出口隧道下线）→ 重探出口 IP：代理出口已消失，直连出口是新的真值。无收敛可等（出口是
        // 确定性消失，不是切换）⇒ 零延迟直接探。
        self.schedule_exit_ip_refresh(false);
        if let Ok(mut snap) = self.startup_snapshot.write() {
            *snap = None;
        }
        // 核停 ⇒ 没有「运行核」这个分母，待应用差集恒空（见 `pending_changes`）→ 欠账标记一并复位，
        // 否则停核期间条上会挂着一条谈不上「待应用」的提示，且下次起核前无人清。
        self.restart_deferred.store(false, Ordering::SeqCst);
        // 分母侧刚被清空 ⇒ 同上刻推一次（与起核就绪腿严格对偶）。停核由命令层发 `proxyStopped`，
        // 前端确有 pull 兜底；但**重启内嵌的这次停核不经命令层**，只靠那条 pull 就是漏的一半。
        // `push_lifecycle(stopped)` 同上必须相邻：核停了就谈不上「正在应用」，条该离开转圈态。
        self.push_pending_changes();
        self.push_lifecycle(&ProxyLifecycleEvent::stopped());
        if let Ok(mut g) = self.pending_force_restart.write() {
            *g = None;
        }
        // 核停 → 热切换基准失效（上游 :1386-1388）。留着会让下次 switch_mode 拿「上一个核」的
        // id→tag 去 PUT 新核里不存在的成员。current_config 保留（上游 :1758 未运行腿仍读写它）。
        if let Ok(mut g) = self.switch_snapshot.write() {
            *g = None;
        }
        self.finish_lifecycle(LifecycleKind::Stop);
        true
    }

    /// 停核拆除腿的换代让位判据（见 [`stop_inner`](Self::stop_inner) 的换代守卫段）。
    ///
    /// `at` = 刚跨过的那个 await 名，只进日志 —— 真机上「残 stop 在哪一步被换代拦下」是这条腿唯一
    /// 可观测的痕迹（没有它，表现只是「什么都没发生」）。
    fn stop_superseded(&self, my_gen: u64, at: &str) -> bool {
        let cur = self.gate.generation();
        if cur == my_gen {
            return false;
        }
        log::warn!(
            "停核腿在 {at} 之后发现已被接管（世代 {my_gen}→{cur}）→ 余下拆除整段让位：\
             此刻的 sidecar 注入态 / running 态 / 系统 DNS 都属**新会话**，动它们等于让新核静默失效"
        );
        true
    }

    /// 杀核（接线 core-supervisor [`ProcessKiller`]）：SIGTERM → 宽限 → SIGKILL，并 reap 子进程。
    ///
    /// 无在跑核 = no-op。退出/崩溃/重启后不留孤儿：child 句柄被 take 后必 `wait()` 收割。
    async fn kill_core(&self) {
        // C6-5：经 helper 起的核 → 经 helper stop（对称）。daemon 摘其受管 child → SIGTERM→宽限→SIGKILL
        // 收割（app 无本地 child 句柄）。阻塞 IPC 挪出 async worker。
        if self.core_via_helper.load(Ordering::SeqCst) {
            self.kill_core_via_helper(Arc::clone(&self.helper) as Arc<dyn HelperStopOps>)
                .await;
            return;
        }
        let child_opt = match self.child.lock() {
            Ok(mut g) => g.take(),
            Err(e) => {
                log::error!("child lock poisoned: {e}");
                return;
            }
        };
        let Some(mut child) = child_opt else {
            return;
        };
        let pid = child.id().unwrap_or(0);
        if pid == 0 {
            // 已退出且被收割 → 仅 reap 残句柄。
            //
            // **同样要清 `self.pid`**：此前这条腿直接 return，把上一次 spawn 的 pid 留在字段里。这不是
            // 罕见角落 —— 核「起来就死」时就绪门的 `try_wait` 会先一步收割它，`child.id()` 随即变 None ⇒
            // 每一次起核失败都从这里走。留下的陈旧 pid 会被 `status()`、诊断、以及 stale 清扫的「受管
            // pid 排除表」当成活的受管核继续引用（排除表里挂个死 pid，等于给同号新进程发免死金牌）。
            let _ = child.wait().await;
            if let Ok(mut g) = self.pid.lock() {
                *g = None;
            }
            return;
        }
        log::info!("停核：pid={pid}（SIGTERM → {STOP_GRACE:?} 宽限 → SIGKILL）");
        let escalation = ProcessKiller::escalate_async(
            move |sig| send_signal(pid, sig),
            move || pid_alive(pid),
            STOP_GRACE,
        )
        .await;
        // 等进程退出（reap，防僵尸）。进程若拒 SIGTERM，升级 task 到点补 SIGKILL 解开此处。
        let _ = child.wait().await;
        // 进程已退出 → 取消挂起的 SIGKILL 升级（防 timer 泄漏 + 防 pid 复用误杀）。
        escalation.wait().await;
        if let Ok(mut g) = self.pid.lock() {
            *g = None;
        }
        log::info!("停核完成：pid={pid} 已退出并收割");
    }

    /// [`kill_core`](Self::kill_core) 的 helper 分支：**带身份**请 daemon 停它自己的受管 child。
    ///
    /// `ops` 参数化（生产传 [`HelperRuntime`]）是为了让本腿可注入替身 —— 否则「请求带没带身份 pid」
    /// 「IPC 期间被接管时记账动没动」两条都只能靠读代码推理，没法变成有牙的门。
    ///
    /// **身份先于 await 取定**（根因）：`stop_inner` 的换代守卫只能在 `kill_core` **返回之后**让位，
    /// 够不着这条 IPC 内部 —— 而经 helper 停核是同步阻塞往返（socket 已删 / daemon 无响应时可以挂
    /// 很久），期间用户完全可能重装 helper 并起了新核。不带身份下发，daemon 就按「停我当前受管的
    /// 那个」执行 = 杀掉用户刚连上的新核（现象：刚连上就被静默断开，且酷似核自己崩了）。
    async fn kill_core_via_helper(&self, ops: Arc<dyn HelperStopOps>) {
        let intended = self.pid.lock().ok().and_then(|g| *g);
        // 阻塞 IPC 挪出 async worker。
        match tokio::task::spawn_blocking(move || ops.stop_managed_core(intended)).await {
            Ok(Ok(())) => log::info!("经 helper 停核完成（pid={intended:?}）"),
            // daemon 可能已因父死看护/崩溃自行收割 → stop 返 notrunning/错误，非致命；
            // 也可能是身份不匹配的诚实 no-op（消息自述），那正是本守卫生效的痕迹。
            Ok(Err(e)) => log::warn!("经 helper 停核未完成：{e}"),
            Err(e) => log::error!("helper 停核任务 join 失败：{e}"),
        }
        self.clear_helper_core_bookkeeping(intended);
    }

    /// helper 停核腿的记账收口：**只清自己那笔**（[`kill_core`](Self::kill_core) 的 helper 分支专用）。
    ///
    /// `intended` = 本腿进 IPC 前拿到的受管 pid。IPC 往返期间 `self.pid` 可能已被**新会话**写成另一个
    /// pid（这正是身份判据要防的那条时序）。此时把它清成 `None` 的后果不是「多清一次」而是让新核**失联**：
    /// `status()` 的 helper 腿据 `self.pid` 探活、诊断据它报 pid、`cleanup_stale_cores` 的「受管 pid 排除表」
    /// 也据它——排除表里少了新核，下一次起核的孤儿清扫就会把它当孤儿杀掉（换个地方杀错进程）。
    ///
    /// 只在「记账已换成另一个 pid」时留手；其余情形（等值 / 现为 `None`）一律照清，保持原语义。
    fn clear_helper_core_bookkeeping(&self, intended: Option<u32>) {
        let Ok(mut g) = self.pid.lock() else {
            log::error!("pid lock poisoned：跳过 helper 停核记账收口");
            return;
        };
        let current = *g;
        if current.is_some() && current != intended {
            log::warn!(
                "helper 停核腿收口时发现受管 pid 记账已换人（{intended:?}→{current:?}）→ \
                 整段记账属新会话，不动它（清它等于让新核在 status/诊断/孤儿清扫排除表里集体失联）"
            );
            return;
        }
        *g = None;
        self.core_via_helper.store(false, Ordering::SeqCst);
    }

    /// 重启（上游 `proxy:restart`，:1499-1508）。**外层 begin/finish 包住内嵌 stop+start，全程 depth≥1**。
    ///
    /// 上游 `restart` = `beginLifecycleOp()` / try{ stop; start } / finally `endLifecycleOp('restart')`。
    /// 内嵌 [`stop_inner`](Self::stop_inner)/[`start`](Self::start) 各自 begin/end 把 depth 抬到 2 再落回 1
    /// （:1519-1521 重入语义）——**封死「stop→start 空窗内 depth 归 0」**，否则去抖 timer / 并发 `switch_mode`
    /// 会钻进空窗并发起第二条重启，且内层 `stop_inner` 的 [`finish_lifecycle`](Self::finish_lifecycle)`(Stop)`
    /// 在 depth 0 命中 `Stopped` 终态分支 → **静默丢弃**窗口内暂存的 switch/force-restart（本不变式的 drifted 缺陷）。
    ///
    /// 用 `stop_inner`（**不清系统代理**）而非 [`stop`](Self::stop)：重启是瞬态停核，紧接 start 重建；主动清会在
    /// 重建前留下「无系统代理」窗口。restart 若在 start 腿失败留死端口，由 start 失败腿
    /// （`maybe_clear_system_proxy_on_start_failure`）收口——见 [`stop`](Self::stop) 文档。
    pub async fn restart(self: &Arc<Self>, config: Value) -> Result<ProxyStatus, StartError> {
        self.gate.begin(); // restart 外层 begin（上游 beginLifecycleOp，:1500）→ depth≥1 不变式起点。
        let r = self.restart_inner(config).await;
        // finish 恒执行（成功/失败/让位三路，try/finally 语义）：depth 归 0 时按 Restart 排空一次
        // 暂存 switch（其内部再分流热切/重启）+ 尾随去抖重启（上游 endLifecycleOp('restart')，:1506）。
        self.finish_lifecycle(LifecycleKind::Restart);
        r
    }

    /// [`restart`](Self::restart) 内层：瞬态停核 + 重建。外层 begin/finish 由 `restart` 持有（depth≥1 不变式）。
    async fn restart_inner(self: &Arc<Self>, config: Value) -> Result<ProxyStatus, StartError> {
        // 停核腿的返回值（是否仍当权）在这里**刻意不消费**：重启的语义就是「无论如何都要按这份 config
        // 重建」，而随后的 `start` 自己会 bump 世代并带着新世代跑完整条起核（含它自己的让位判定）。
        // `stop` 那边要判，是因为「清系统代理」是**终态**动作，让位与否会改变最终留给用户的系统状态。
        let _ = self.stop_inner().await;
        self.start(config).await
    }

    /// 挂后台崩溃监测任务（上游 `singboxProcess.on('exit')` → `handleProcessExit` 的等价物）。
    ///
    /// **为何是轮询而非 `child.wait()`**：tokio `Child::wait()` 需 `&mut self` 单持有者，而主动停止
    /// 路径（`kill_core`）已经持有并 `wait()` 那个句柄 → 崩溃监测不能也去 `wait()`，只能短暂持锁
    /// `try_wait` 观察。轮询绝不跨 await 持 `child` 锁（否则 !Send 编译即拒 + 与 `kill_core` 抢锁）。
    ///
    /// **主动 vs 意外的区分**（本任务最易出 bug 处）：完全靠 `LifecycleGate` 世代。
    /// `stop`/`restart` 入口必先 `bump_generation()` 再杀核 → 世代一变本监测即 `Retire`，
    /// 主动杀核的 SIGTERM/SIGKILL 绝不会被误判成崩溃。判据见 [`classify_child_exit`]。
    fn spawn_crash_monitor(self: &Arc<Self>, my_gen: u64) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            // helper 腿的 pid 身份基线：`(基线取自哪个 pid, 令牌)`。见 [`process_identity`]。
            let mut identity: Option<(u32, String)> = None;
            let mut identity_unobservable_logged = false;
            let mut ticks: u64 = 0;
            loop {
                tokio::time::sleep(Duration::from_millis(CRASH_MONITOR_POLL_MS)).await;
                ticks += 1;
                let gen_now = me.gate.generation();
                // 观察核存活。C6-5：helper 核无本地 child 句柄 → 若按 child 观察必得 `Absent`→`Retire`
                //（永不自愈）。改用 pid 探活（对齐 上游 健康检查 `isProcessAlive(activePid)`）：pid 死=崩溃。
                // 直起路径仍走 child.try_wait（仅短暂持锁，绝不跨 await）。
                //
                // **pid 探活只回答「这个号码上有进程吗」**，不回答「是不是我那个」⇒ 核死后号码被复用
                // 时它恒真、崩溃自愈永不触发。故每 `PID_IDENTITY_RECHECK_TICKS` 个 tick 复核一次
                // 进程身份令牌（[`process_identity`]），换人即判退出。
                let observation = if me.core_via_helper.load(Ordering::SeqCst) {
                    match me.pid.lock().ok().and_then(|g| *g) {
                        Some(p) => {
                            if !pid_alive(p) {
                                ChildObservation::Exited
                            } else {
                                // 基线：首次观测到存活时取一次；记账换了 pid（新会话写了 `self.pid`）则重取，
                                // **不**拿旧 pid 的令牌去比新 pid（那会是一次必然的假不匹配）。
                                if identity.as_ref().is_none_or(|(bp, _)| *bp != p) {
                                    identity = process_identity(p).map(|tok| (p, tok));
                                    if identity.is_none() && !identity_unobservable_logged {
                                        identity_unobservable_logged = true;
                                        log::warn!(
                                            "崩溃监测：取不到 pid={p} 的进程身份令牌 → \
                                             本代只按 pid 探活（pid 复用不可发现）"
                                        );
                                    }
                                }
                                let due = ticks.is_multiple_of(PID_IDENTITY_RECHECK_TICKS);
                                let verdict = if due {
                                    pid_identity_verdict(
                                        identity.as_ref().map(|(_, t)| t.as_str()),
                                        process_identity(p).as_deref(),
                                    )
                                } else {
                                    PidIdentity::Match
                                };
                                if verdict == PidIdentity::Mismatch {
                                    log::warn!(
                                        "崩溃监测：pid={p} 的进程身份令牌已变 ⇒ 受管核实际已退出、\
                                         该号码被系统复用（探活恒真是假象）"
                                    );
                                    ChildObservation::Exited
                                } else {
                                    ChildObservation::Alive
                                }
                            }
                        }
                        // pid 已被清（停核/让位收口）→ 视作退场，非崩溃。
                        None => ChildObservation::Absent,
                    }
                } else {
                    let mut guard = match me.child.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            log::error!("崩溃监测：child lock poisoned: {e} → 退场");
                            return;
                        }
                    };
                    match guard.as_mut() {
                        None => ChildObservation::Absent,
                        Some(c) => match c.try_wait() {
                            Ok(None) => ChildObservation::Alive,
                            // 已退出（收割）或探活出错 → 保守当已退出。
                            Ok(Some(_)) | Err(_) => ChildObservation::Exited,
                        },
                    }
                };
                match classify_child_exit(my_gen, gen_now, observation) {
                    ExitClassification::KeepWatching => {}
                    // 主动 stop/restart 接管（世代变 / 句柄被取）→ 退场，不触发自愈。
                    ExitClassification::Retire => return,
                    ExitClassification::Crash => {
                        log::warn!(
                            "检测到 sing-box 意外退出（世代 {my_gen} 未变、非主动停止）→ 触发崩溃自愈"
                        );
                        // C5：核意外退出 → TS 内核接口已随进程消失、其 ifscope 路由自动失效 → 同步复位内存态
                        // （不发删命令，防对已消失接口误删主表）。自愈重启后由 start_inner 就绪后 reconcile 重建。
                        me.mesh.exit_route_reset_state().await;
                        // row33：核已死 → 停 DNS 接口热插拔 watcher（route -n monitor 无核可重灌；自愈重启后 start_inner 重起）。
                        me.stop_dns_watcher();
                        // A3：核已死 → STATUS 流失效 → 清 TS 状态末帧缓存（本 relay 亦随后由世代守卫退场）。
                        me.mesh.clear_ts_status();
                        // A4：核已死 → 复位登录期出口让位内存态 + 撤 UI。自愈重启后由 start_inner 预置重建。
                        me.reset_login_fallback_state();
                        // R2：核已死 → 复位 TS 出口无效直判的翻转对账缓存（新会话首帧须能重新触发
                        // none→blocked）。**恢复腿的单飞令牌不在此清**——它归在飞任务的 Drop 归还，
                        // 见 `reset_ts_exit_block_state` 文档。
                        me.reset_ts_exit_block_state();
                        me.run_crash_recovery().await;
                        return; // 自愈成功会起新核 + 新监测；失败/放弃则本核生命周期终结。
                    }
                }
            }
        });
    }

    /// 崩溃自愈执行体：决策全在 [`CrashRecoveryMachine`]（退避 / 上限 / 让位 / 补发），本方法只执行
    /// 「退避 sleep + restart」的 I/O，并把结果反馈回状态机（上游 `attemptAutoRestart` 的 I/O 侧）。
    ///
    /// **绝不无限重启**：`should_auto_restart` 达 `MAX_RESTART_COUNT`(3) → `GiveUp` → 报错并退场；
    /// 60s 冷却窗口内计数不复位（紧密崩溃循环必收敛到 GiveUp）。
    async fn run_crash_recovery(self: &Arc<Self>) {
        // 崩溃时用的配置：优先 last-applied（current_config），回落磁盘最新配置。
        let cfg = self
            .current_config
            .read()
            .ok()
            .and_then(|g| g.clone())
            .or_else(|| self.config.current().ok());
        let Some(cfg) = cfg else {
            let msg = "sing-box 意外退出，且无可用配置重启 → 放弃自愈".to_string();
            log::error!("{msg}");
            self.set_error(&msg, code::PROCESS_EXITED);
            return;
        };

        loop {
            let outcome = {
                let mut m = self.crash_lock();
                // M-2′-G1：喂 handle_crash **真实的在途腿世代**（此前硬编码 `None`）。缺此，接管会话
                // （新代核）崩溃永不置 `crash_while_superseded` → 让位腿 replay=false → 新代核崩溃无人接管。
                // 单锁内读 getter + 决策（seam `drive_crash_decision`），绝不 TOCTOU（两次取锁间被改）。
                drive_crash_decision(&mut m, now_ms(), self.gate.generation())
            };
            match outcome {
                AutoRestartOutcome::GiveUp => {
                    // GiveUp 有两种成因，文案必须分开：换核验证窗口下这是**第一次**崩溃，
                    // 报「已达自愈上限（3 次/60s）」是字面为假 —— 而这条 message 会原样进
                    // `event:proxyError` 给用户看，也会进日志成为下次排查的起点。
                    // 码沿用 `AUTO_RESTART_FAILED`（我们确实放弃了自动重启），不新增码：
                    // 新码要同步前端 `ProxyErrorCode` 与 5 份 locale，而这里的信息差在文案不在分类。
                    let msg = if self.crash_lock().auto_restart_suppressed() {
                        "新内核首次运行即异常退出（换核验证窗口内不自动重启）→ 将尝试回滚到原内核"
                            .to_string()
                    } else {
                        "sing-box 反复崩溃，已达自愈上限（3 次/60s）→ 放弃自动重启".to_string()
                    };
                    log::error!("{msg}");
                    self.set_error(&msg, code::AUTO_RESTART_FAILED);
                    return;
                }
                // 已有重启腿在途 / 用户已停 → 静默退场。
                AutoRestartOutcome::Dedup | AutoRestartOutcome::AbortedByUser => return,
                AutoRestartOutcome::Attempt {
                    attempt,
                    backoff,
                    generation,
                } => {
                    log::warn!("崩溃自愈：第 {attempt} 次尝试，退避 {backoff:?} 后重启");
                    tokio::time::sleep(backoff).await;
                    let fate = self
                        .crash_lock()
                        .post_backoff(generation, self.gate.generation());
                    match fate {
                        RestartFate::AbortedByUser => {
                            log::info!("崩溃自愈：退避期间用户已主动停止 → 放弃重启");
                            return;
                        }
                        RestartFate::Superseded { replay } => {
                            if replay {
                                log::info!("崩溃自愈：让位，但接管腿也崩溃 → 补发一次");
                                continue;
                            }
                            log::info!("崩溃自愈：退避期间被更新的 start/stop 接管 → 让位");
                            return;
                        }
                        // 非交互（上游 `start(cfg, {interactive:false})`）：崩溃自愈是**用户没做任何
                        // 操作**时自动发生的，此处弹系统授权框 = 凭空索要管理员密码，且崩溃循环里最多
                        // 连弹 MAX_RESTART_COUNT 次。抑制后退回类型化终态，待用户手动启停时经门引导。
                        RestartFate::Start => {
                            match with_helper_gate_suppressed(self.restart(cfg.clone())).await {
                                Ok(st) if st.running => {
                                    let _ = self.crash_lock().post_start(false);
                                    log::info!("崩溃自愈：重启成功（新 pid={}）", st.pid);
                                    return; // 新核已挂新监测。
                                }
                                // 就绪等待期被接管 → 让位，不报成功（lastStartSuperseded）。
                                Ok(_) => {
                                    let _ = self.crash_lock().post_start(true);
                                    log::info!("崩溃自愈：重启就绪期被接管 → 让位");
                                    return;
                                }
                                Err(e) => {
                                    log::error!("崩溃自愈：重启失败: {e}");
                                    // 不可恢复错误（helper 缺失/用户取消提权门 → 按码；权限/root 残留/
                                    // clash_api 端口占用 → 按 message 关键字）→ 立即终态放弃，不再空耗退避
                                    // （上游 isUnrecoverableRestartError，:6039/:6043）。整个 `e` 而非只
                                    // `e.message`：码腿要读 `e.code`，见 is_unrecoverable_restart_error 文档。
                                    let unrecoverable = is_unrecoverable_restart_error(&e);
                                    match self.crash_lock().post_start_failure(unrecoverable) {
                                        FailureOutcome::GiveUp => {
                                            self.report_auto_restart_giveup(&e);
                                            return;
                                        }
                                        // 未达上限 → 自循环再试一次（下一轮 attempt 内按计数退避）。
                                        FailureOutcome::Retry => continue,
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// 崩溃自愈 **GiveUp 腿的终态播报**：本次失败没有更具体的码时才补发 [`code::AUTO_RESTART_FAILED`]。
    ///
    /// **修的是什么**：`run_helper_gate` 非交互腿（:1513-1516）自己就 `set_error(HELPER_NOT_INSTALLED)`
    /// 发过一条，回 `Err` 后 [`is_unrecoverable_restart_error`] 判终态 → `post_start_failure(true)`
    /// 返 `GiveUp` → 此处再叠一条 `AUTO_RESTART_FAILED`。**两条码各自在前端触发 `toast.error` +
    /// `notifyDesktop`，且这两腿无人 `await` ⇒ 认领闸门不抑制** ⇒ 用户背靠背吃 2 toast + 2 桌面通知。
    ///
    /// **判据 = [`StartError::code`]，不是回读全局 `status().error_code`**：本文件 8 处
    /// `StartError::coded` 构造点（:1516/:1523/:1540/:1547/:1668/:1704/:1755/:1771）无一例外**紧邻**
    /// 一条同码同文案的 `self.set_error(..)`，而无码腿（`From<String>` 零成本升格的
    /// `.map_err(|e| format!(..))?`）**一条都不 set_error** ⇒ `code.is_some()` ⟺「本次失败已播报过更
    /// 具体的分类」。回读全局则会踩 A1 同款陈旧读（全局 `error_code` 只有 `stop()` 会清、多条腿根本
    /// 不写），理由见 `commands/proxy.rs::start_err_response` 文档。
    ///
    /// **刻意不修过头**：无码腿（config 解析/生成/建目录/写盘失败等）**必须**照常发
    /// `AUTO_RESTART_FAILED` —— 否则崩溃自愈放弃时前端一条提示都收不到，「静默」比「双报」更坏。
    fn report_auto_restart_giveup(&self, e: &StartError) {
        if let Some(code) = e.code {
            // 已有更具体的终态码在前 → 只留日志，不叠发第二条事件。
            log::error!(
                "sing-box 崩溃自愈重启失败且达上限 → 放弃：{e}（已按 {code} 播报，不叠发）"
            );
            return;
        }
        let msg = format!("sing-box 崩溃自愈重启失败且达上限 → 放弃：{e}");
        self.set_error(&msg, code::AUTO_RESTART_FAILED);
    }

    /// 短暂借出崩溃自愈状态机（决策同步、单语句用完即释；**绝不跨 await 持锁**）。
    fn crash_lock(&self) -> std::sync::MutexGuard<'_, CrashRecoveryMachine> {
        self.crash_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 短暂借出诊断计数器（慢起轴更新同步、绝不跨 await 持锁）。
    fn diag_lock(&self) -> std::sync::MutexGuard<'_, DiagnosticCounters> {
        self.diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 启动期 stale-core 清扫：杀掉上个会话崩溃遗留的**本 app** 孤儿核。
    ///
    /// **安全第一性**（本任务核心）：只杀 cmdline 精确匹配 `resolve_core_binary()` 路径 + `run` 的进程
    /// （core-supervisor [`stale_pids`]），并排除当前受管 pid。**绝不 `pkill sing-box`**——用户机器上
    /// 可能装有无关的 sing-box。解析不到核二进制 / 非 Linux（扫描返空）→ 静默跳过（fail-closed，不误杀）。
    async fn cleanup_stale_cores(&self) -> Result<(), StartError> {
        // 实跑计数：置于所有早退腿之前 —— 计的是「清扫这条腿被走到几次」，而非「杀掉几个孤儿」。
        self.stale_sweep_runs.fetch_add(1, Ordering::SeqCst);
        let binary = match resolve_core_binary() {
            Ok(b) => b,
            Err(e) => {
                log::debug!("stale 清扫：未解析到核二进制（{e}）→ 跳过");
                return Ok(());
            }
        };
        // **不 canonicalize**：spawner 用 `resolve_core_binary()` 的**字面**路径起核（`Command::new`），
        // /proc 里的 argv[0] 即那个字面路径；两次会话同一 resolve 逻辑 → 字面一致即可匹配。规范化反而会
        // 与含 symlink/`..` 的字面 argv[0] 失配、漏杀自己的孤儿（与 上游 pgrep 用字面 singboxPath 同源）。
        let candidates = scan_running_cores();
        // 排除当前受管 pid（避免误杀正在跑/正要接管的核）。
        let managed: Vec<u32> = self.pid.lock().ok().and_then(|g| *g).into_iter().collect();
        let victims = stale_pids(&candidates, &binary, &managed);
        if victims.is_empty() {
            return Ok(());
        }
        log::warn!(
            "发现 {} 个上次遗留的孤儿核（本 app 二进制 {}），清理：{victims:?}",
            victims.len(),
            binary.display()
        );
        // SIGTERM → 宽限 → SIGKILL 存活者（对齐 上游 killOrphanedProcessesLinux）。
        for pid in &victims {
            send_signal(*pid, Signal::Sigterm);
        }
        tokio::time::sleep(STALE_KILL_GRACE).await;
        for pid in &victims {
            if pid_alive(*pid) {
                log::warn!("孤儿核 pid={pid} 宽限期未退 → SIGKILL");
                send_signal(*pid, Signal::Sigkill);
            }
        }
        // **T3 二次确认**：SIGKILL 后仍存活 = 用户态根本杀不动（`send_signal` 对 root 进程收 EPERM 且
        // 被 `let _ =` 吞掉，**杀失败与杀成功在调用处无从区分**）。故只能靠再探一次活来判定。
        tokio::time::sleep(STALE_KILL_GRACE).await;
        let survivors: Vec<u32> = victims.iter().copied().filter(|p| pid_alive(*p)).collect();
        if survivors.is_empty() {
            log::info!("孤儿核清理完成：{victims:?}");
            return Ok(());
        }
        self.escalate_root_orphans(&survivors).await
    }

    /// **起核收口腿**：helper 报了 pid 但本侧探活判死 → **先请 daemon 停掉它自己的受管 child**，
    /// 再返回失败消息。
    ///
    /// **为什么必须 stop（结构保证，不是修当前 bug）**：原实现在此只清 `core_via_helper` 标记和
    /// `pid` 就返回，理由是「进程已死，无需再 stop」——而那个前提**正是被 EPERM 误判打破的那条**。
    /// 一旦探活判错，daemon 手里那个活着的 root 核就此失联：标记已清 ⇒ 之后 `kill_core` 不走 helper 腿，
    /// child 又恒 `None` ⇒ 停核彻底变 no-op，孤儿就此诞生（本次真机事故的成因）。
    ///
    /// 让 daemon 收口它自己的 child，把「不会漏下孤儿」从**对探活正确性的推理**降格成**结构保证**：
    /// 探活对不对，这条腿都不留残留。与 T1 的探活修复是两道独立防线，将来任何探活缺陷都不会
    /// 再复制这次事故。stop 失败不改判（核确实可能真死了）——照实记日志，错误消息原样返回。
    async fn reject_helper_start(ops: Arc<dyn HelperStopOps>, pid: u32) -> String {
        // stop 是同步阻塞 IPC → 挪出 async worker 线程（同 start_core/stop_core/cleanup_cores）。
        // **带上 pid**：本腿要收口的是 daemon 刚报给我们的这一个（helper 报活但探活判死的那个），
        // 不是「daemon 此刻手里的随便哪个」——本方法整段可能与新会话并发。
        match tokio::task::spawn_blocking(move || ops.stop_managed_core(Some(pid))).await {
            Ok(Ok(())) => log::info!("起核收口：已请 daemon 停掉其受管 child（pid={pid}）"),
            Ok(Err(e)) => log::warn!("起核收口：请 daemon 停核失败（pid={pid}）：{e}"),
            Err(e) => log::error!("起核收口：停核任务 join 失败（pid={pid}）：{e}"),
        }
        format!("helper 报告已启动但进程不存在（pid={pid}）")
    }

    /// **T3**：用户态杀不动的 root 孤儿核 → 经 helper 提权清扫；清不掉则落诚实终态。
    ///
    /// 对齐 上游 `escalateKillRootOrphans` + `ROOT_ORPHAN_BLOCKED`。**为什么必须阻断起核而不是继续**：
    /// 活着的 root 孤儿一直独占 `<userData>/cache.db`，此时起任何新核都会
    /// `initialize cache-file: timeout`，**连切回 systemProxy 模式也起不来**——继续放行只会让用户撞上
    /// 一串无从归因的启动失败。报 [`code::ROOT_ORPHAN_BLOCKED`] 才指得出真正的动作。
    async fn escalate_root_orphans(&self, survivors: &[u32]) -> Result<(), StartError> {
        log::warn!(
            "{} 个孤儿核用户态杀不动（root 所有，EPERM）：{survivors:?} → 尝试经 helper 提权清扫",
            survivors.len()
        );
        // helper 未装 → 无提权腿，直接落终态（不假装尝试过）。
        if self.helper.status().installed {
            let helper = Arc::clone(&self.helper);
            // `cleanup_cores` 是同步阻塞 IPC → 挪出 async worker 线程（同 start_core/stop_core）。
            match tokio::task::spawn_blocking(move || helper.cleanup_cores()).await {
                Ok(Ok(())) => {
                    tokio::time::sleep(STALE_KILL_GRACE).await;
                    let still: Vec<u32> = survivors
                        .iter()
                        .copied()
                        .filter(|p| pid_alive(*p))
                        .collect();
                    if still.is_empty() {
                        log::info!("经 helper 提权清扫已清掉 root 孤儿核：{survivors:?}");
                        return Ok(());
                    }
                    // daemon 返成功但进程仍在 → 照实报，不采信回执（结果以探活为准）。
                    log::error!("helper 清扫返回成功，但 {still:?} 仍存活");
                }
                Ok(Err(e)) => log::error!("helper 提权清扫失败：{e}"),
                Err(e) => log::error!("helper 清扫任务 join 失败：{e}"),
            }
        } else {
            log::error!("helper 未安装 → 无提权腿可用，root 孤儿核 {survivors:?} 清不掉");
        }
        let msg = format!(
            "上次遗留的 sing-box 核（pid {survivors:?}）以管理员权限运行且无法清理，\
             它占用着内核缓存文件，任何模式都无法启动。请安装/修复 Helper 后重试，\
             或手动执行：sudo kill -9 {}",
            survivors
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        );
        self.set_error(&msg, code::ROOT_ORPHAN_BLOCKED);
        Err(StartError::coded(msg, code::ROOT_ORPHAN_BLOCKED))
    }

    /// lifecycle 收尾：`end` + 按返回的排空/丢弃指令动作（**语义全在 core-supervisor，本处只执行**）。
    fn finish_lifecycle(self: &Arc<Self>, kind: LifecycleKind) {
        match self.gate.end(kind) {
            LifecycleEndResult::StillBusy(depth) => {
                log::debug!("lifecycle end（{kind:?}）：depth={depth} 仍在飞，pending 留给最外层");
            }
            LifecycleEndResult::Stopped(discard) => {
                // 停止终态：丢弃全部 pending（停止优先，不得停后又被拉起）。
                if discard.discarded_restart
                    || discard.discarded_force_restart_id.is_some()
                    || discard.discarded_switch_id.is_some()
                {
                    log::info!(
                        "停止终态丢弃 pending：restart={} force={:?} switch={:?}",
                        discard.discarded_restart,
                        discard.discarded_force_restart_id,
                        discard.discarded_switch_id
                    );
                }
                if let Ok(mut g) = self.pending_force_restart.write() {
                    *g = None;
                }
                // 停止终态同样丢弃暂存的 switch（停止优先：不得停后又被 switch 拉起）。
                if let Ok(mut g) = self.pending_switch.write() {
                    *g = None;
                }
            }
            LifecycleEndResult::Drained(drain) => {
                if drain.schedule_restart {
                    log::info!("depth 归零 → 排空一次尾随重启");
                    self.schedule_restart();
                }
                // 排空暂存的 switchMode（上游 :1540 `void this.switchMode(pendingSwitch)`）。
                // depth 已归零 → 重放时不会再落回 Pending 腿，可正常判热切/重启。
                if let Some(id) = drain.replay_switch_id {
                    if let Some((cfg, defer_restart)) = self.take_pending_switch(Some(id)) {
                        log::info!(
                            "depth 归零 → 重放暂存的 switchMode（defer_restart={defer_restart}）"
                        );
                        let me = Arc::clone(self);
                        tokio::spawn(async move {
                            me.switch_mode_with(cfg, defer_restart).await;
                        });
                    }
                }
            }
        }
    }

    /// 调度一次去抖重启（接线 switch-engine [`DebouncedRestart`]：timer + 世代守卫 + gate 顺序门）。
    fn schedule_restart(self: &Arc<Self>) {
        let me = Arc::clone(self);
        // handle 不持有：drop 不取消 task（task 自查 gate 决策，过期自行 Superseded）。
        let _handle = self
            .debounced
            .schedule(self.core_running(), move |outcome| {
                match outcome {
                    DebouncedOutcome::Proceed(force_id) => {
                        tokio::spawn(async move {
                            // H-1：优先读 force-restart 专用快照（in-flight start 会覆盖 currentConfig）。
                            let cfg = me.take_force_restart_config(force_id);
                            let cfg = match cfg.or_else(|| me.config.current().ok()) {
                                Some(c) => c,
                                None => {
                                    log::warn!("去抖重启：无可用配置 → 放弃");
                                    return;
                                }
                            };
                            if let Err(e) = me.restart(cfg).await {
                                log::error!("去抖重启失败: {e}");
                            }
                        });
                    }
                    other => log::info!("去抖重启未执行：{other:?}"),
                }
            });
    }

    /// 取出并清除 force-restart 专用配置快照（id 对得上才取；对不上回落 None）。
    fn take_force_restart_config(&self, id: Option<u64>) -> Option<Value> {
        let mut g = self.pending_force_restart.write().ok()?;
        match (&*g, id) {
            (Some((sid, _)), Some(want)) if *sid == want => g.take().map(|(_, c)| c),
            // id 为 None（用 currentConfig）或对不上（更新的 apply 已换快照）→ 不消费。
            _ => None,
        }
    }

    /// 置错误态（起核失败）。
    /// 进入错误终态：落状态（`running=false` + error + errorCode）→ 广播 `event:proxyError`。
    ///
    /// **为什么发射点收口在这里而不是各失败腿**：`set_error` 是「运行时进入错误态」的唯一状态跃迁点，
    /// 挂在这里 ⇒ 新增失败腿只要照常 `set_error` 就自动有事件，**没有哪条腿能悄悄错掉**（挂在各腿上
    /// 则漏一个就退回本 bug：`EVENT_PROXY_ERROR` 定义了却全仓零 emit）。
    ///
    /// **不覆盖的腿及理由**：
    /// - **用户主动 `stop`**：不是错误，是达成了用户意图的终态 → 走 `event:proxyStopped`，此处不发。
    /// - **被更新的 start/stop 接管（让位腿）**：本腿没失败，只是不再是当权者；接管方会自己收口
    ///   （发错误会让 UI 为一次正常的接管报警）。让位腿本就返 `Ok(status)`、不经 `set_error`。
    /// - **config 生成 / 写盘 / spawn 失败**：有 command 在 await（`ApiResponse::err` → 前端 throw），
    ///   调用方已拿到真错。这些腿此前也不经 `set_error`，本次不扩面（要扩得连状态一起落，属另一议题）。
    ///
    /// 事件发不出（emitter 未接线 / 无窗口）绝不打断状态落值 —— 诊断通道不该反噬它诊断的东西。
    fn set_error(&self, msg: &str, error_code: &str) {
        log::error!("{msg}");
        if let Ok(mut g) = self.status.write() {
            *g = ProxyStatus {
                error: Some(msg.to_string()),
                error_code: Some(error_code.to_string()),
                ..ProxyStatus::default()
            };
        }
        match self.error_emitter.get() {
            Some(e) => e.emit_proxy_error(msg, error_code),
            // 未接线：单测 / setup 前的极早期失败。状态已落，只是没有渲染端可推。
            None => log::debug!("proxy error emitter 未接线 → 跳过 event:proxyError（状态已落）"),
        }
    }

    /// 置**非终态**告警（核仍在运行，但有用户必须知道的降级）→ 落 error/errorCode + 广播 `event:proxyError`。
    ///
    /// **与 [`set_error`](Self::set_error) 的分工（别混用）**：
    /// - `set_error` = 「运行时进入错误终态」→ 整个 `ProxyStatus` 重置为 `default()`（`running=false`、
    ///   `pid=0`、端口清零）。用于起核失败 / 核崩了。
    /// - 本方法 = 「核在跑，但流量的安全属性被降级了」→ **只写 error 两字段，保留 `running/pid/端口/startTime`**。
    ///
    /// **为什么必须分开**：A1 启用失败与出口不一致时核**确实在运行**。若复用 `set_error`，UI 会显示
    /// 「未运行」而进程还活着 = 虚报，且抹掉 `pid`/`clashApiPort` 会让停核、管理 API、统计全部失联 ——
    /// 用一个诊断通道换掉运行态真值，比它诊断的问题更糟。这正是 `DESIGN-REVIEW(a1-enable-failure-surface)`
    /// 留的口子：要「冒给用户」，但不许把活核标成死核。
    ///
    /// 状态未落值（锁中毒）或事件发不出（emitter 未接线）都**不打断调用方** —— 诊断通道不该反噬被诊断者。
    ///
    /// **消费端**：`App.tsx` 的 `api.proxy.onError` 订阅按错误码白名单放行，当前已含
    /// `SYSTEM_PROXY_FAILED` / `EXIT_MISMATCH` / `RULE_RESOURCES_MISSING`（本方法发的三个码）。
    /// 新增码时**必须同步前端白名单**，否则后端这半条链（落状态 + 发事件 + 单测锁死）齐备、
    /// 用户端仍是静默丢弃——那正是本方法早先的状态。
    fn set_nonfatal_error(&self, msg: &str, error_code: &str) {
        log::error!("{msg}");
        if let Ok(mut g) = self.status.write() {
            // 只覆盖错误两轴，其余字段（running/pid/mixed_port/clash_api_port/start_time…）原样保留。
            g.error = Some(msg.to_string());
            g.error_code = Some(error_code.to_string());
        }
        match self.error_emitter.get() {
            Some(e) => e.emit_proxy_error(msg, error_code),
            None => log::debug!("proxy error emitter 未接线 → 跳过 event:proxyError（状态已落）"),
        }
    }

    /// 推送本次起核 gate 的非法节点（`event:proxy:invalid-nodes`）。
    ///
    /// 未接线（单测 / setup 前）→ 只记日志：**发不出事件绝不能反过来打断起核本身**（同 [`set_error`]
    /// 的取舍）。gate 已经把这些节点剔出 config 了，事件只是让用户看见，缺它不影响正确性。
    ///
    /// [`set_error`]: Self::set_error
    fn emit_invalid_nodes(&self, nodes: &[InvalidNode]) {
        if !nodes.is_empty() {
            log::info!(
                "启动 gate 剔除 {} 个非法节点: {}",
                nodes.len(),
                nodes
                    .iter()
                    .map(|n| n.tag.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        match self.error_emitter.get() {
            Some(e) => e.emit_invalid_nodes(nodes),
            None => log::debug!("emitter 未接线 → 跳过 proxy:invalid-nodes"),
        }
    }

    /// 应用一次配置变更（上游 `ProxyManager.switchMode`，:1746-1890）。
    ///
    /// **本仓此前无此路径** —— `server:switch` 等命令只落盘 + 广播 UI 事件，从不触达运行核；
    /// 唯一的入核手段是 `apply_pending`（恒全量重启）。本方法把既有的三腿决策接上生产路径：
    ///
    /// 1. lifecycle 在飞（depth>0）→ 暂存 + `set_switch_pending`，由 `end()` 排空重放（上游 :1752）。
    ///    **必须先于「核未运行」判**：restart 的 stop→start 空窗内核看起来没在跑，先判会把本次变更
    ///    永久丢弃（与 `apply_pending` 的 H-1 同型陷阱）。
    /// 2. 核未运行 → 仅更新 `current_config`（下次 start 按新配置生成）（上游 :1757）。
    /// 3. 与 `current_config` 逐字节全等 → 仅更新引用（上游 bug#5，:1767）。
    /// 4. `plan_hot_switch` + `decide` 三腿分发（switch-engine 既有纯逻辑，本处只喂参数 + 执行）。
    ///
    /// 返回 [`SwitchOutcome`] 供 command 层与测试断言走了哪条腿。
    pub async fn switch_mode(self: &Arc<Self>, new_config: Value) -> SwitchOutcome {
        self.switch_mode_with(new_config, false).await
    }

    /// [`Self::switch_mode`] 带「保存不重启」标志的形态（暂存层「保存」腿，spec §2.5 Q4）。
    ///
    /// `defer_restart=true` **只**把 switch-engine 的第 4 腿（结构性变更 → 去抖重启）降级为 Defer：
    /// 落盘 + 提交 `current_config` + 留在待应用差集里，但不排程重启。射程边界与「为什么不降级
    /// `must_restart`」在 [`DecisionInput::defer_restart`] 上有完整因果，此处不复述。
    ///
    /// **默认入口仍是 [`Self::switch_mode`]**（等价于本方法传 `false`）：配置写的十余个生产路径
    /// 全部经 `broadcast_config_changed` 汇流，只有那一处会按前端是否传 `deferRestart` 决定传什么。
    /// 新增写路径若直接调本方法并硬编码 `true`，等于绕过用户意图 —— 不要这么做。
    pub async fn switch_mode_with(
        self: &Arc<Self>,
        new_config: Value,
        defer_restart: bool,
    ) -> SwitchOutcome {
        // 管理 API PUT、current_config commit 与重启判定必须是一个串行事务。尤其要排在 lifecycle
        // busy 判定之前：等待期间可能恰好进入/退出重启，拿锁后必须重新看当下 gate，而非沿用旧快照。
        let _switch_guard = self.switch_serial.lock().await;
        // ── 腿 0：lifecycle 在飞 → 暂存重放（顺序门，见方法文档）──
        if self.gate.is_busy() {
            let id = self.switch_seq.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut g) = self.pending_switch.write() {
                *g = Some((id, new_config, defer_restart));
            }
            self.gate.set_switch_pending(id);
            log::info!("switchMode：lifecycle 在飞（depth>0）→ 暂存，settle 后重放");
            return SwitchOutcome::Pending;
        }

        // ── 腿 0.5 起的**判定**全部下沉 [`Self::classify_switch`]（纯读，无副作用）──
        // 本方法自此只负责「执行」：判据与 `config:classifyStaged` 逐字共用同一份。
        let (decision, new_cfg) = match self.classify_switch(&new_config, defer_restart) {
            ClassifiedSwitch::NotRunning => {
                if let Ok(mut g) = self.current_config.write() {
                    *g = Some(new_config);
                }
                log::info!("switchMode：核未运行 → 仅更新配置（下次 start 生效）");
                return SwitchOutcome::NotRunning;
            }
            ClassifiedSwitch::Unchanged => {
                if let Ok(mut g) = self.current_config.write() {
                    *g = Some(new_config);
                }
                return SwitchOutcome::Unchanged;
            }
            ClassifiedSwitch::Fallback(why) => {
                log::warn!("switchMode：{why} → 保守走重启");
                self.apply_restart(new_config);
                return SwitchOutcome::Restarting;
            }
            ClassifiedSwitch::Decided { decision, new_cfg } => (decision, *new_cfg),
        };

        // ── 腿 3：三腿分发（决策全在 switch-engine，本处只执行）──
        let outcome = match decision {
            SwitchDecision::HotSwitch(plan) => {
                let api = self.management_api().await;
                let interrupt = new_cfg.interrupt_connections_on_switch == Some(true);
                log::info!(
                    "switchMode：热切换腿（kind={:?}，{} 个 selector PUT，断连开关={interrupt}）",
                    plan.kind,
                    plan.puts.len()
                );
                match SwitchExecutor.execute(&api, &plan, interrupt).await {
                    HotSwitchOutcome::Applied { disconnect } => {
                        self.commit_applied(&new_config);
                        // C5：热切换可能切换了全局出口节点（到/离 TS System 全隧道出口）→ 对齐出口路由。
                        // 重启腿的出口路由由重启后 start_inner 的就绪后 reconcile 覆盖，故仅热切腿需在此显式对齐。
                        self.mesh
                            .exit_route_reconcile(&new_cfg, new_cfg.enable_ipv6.unwrap_or(false))
                            .await;
                        log::info!(
                            "switchMode：热切换成功（核未重启），精准断连 {} 条",
                            disconnect.map_or(0, |d| d.closed_ids.len())
                        );
                        // M1（上游 `proxyManager.on('unlock-invalidate')`，`index.ts:2006-2008`）：**任何**热切换
                        // ——切全局节点 / 改规则目标节点 / 两者——都可能换掉解锁检测走的出口或分流，故一律失效重测。
                        // 与 `commands/config.rs` 的 `selected_exit_changed` 腿的分工：那条只覆盖「选中出口变」，
                        // **kind=rules 的纯规则热切换它看不见**（selectedServerId 没动）→ 漏失效，正是 上游 M1 要堵的洞。
                        // 两条重叠触发无害：1500ms 去抖窗把它们合并成一轮（这正是去抖存在的理由之一）。
                        self.invalidate_unlock_cache(true, false);
                        // 同理（上游「节点热切换」触发点）：热切换换掉的正是出口本身 ⇒ 状态栏 IP + 旗面
                        // + 伴测延迟全部作废，须重探。留着旧值 = 用上一个出口冒充当前出口。
                        self.schedule_exit_ip_refresh(true);
                        SwitchOutcome::HotSwitched
                    }
                    // 失败/未就绪 → 退回去抖重启兜底（executor 契约：「任一失败 → 整体退回去抖重启，
                    // 保证一定能应用」）。**刻意偏离 上游**：上游 热切失败后 fall-through 到 no-op 腿，
                    // kind=rules 的失败会因 norm 等价 + 节点未变而被 no-op **静默吞掉**（变更永不生效）。
                    // 见交付说明「边界声明」。
                    other => {
                        log::warn!("switchMode：热切换失败（{other:?}）→ 退回重启式切换");
                        self.apply_restart(new_config);
                        SwitchOutcome::Restarting
                    }
                }
            }
            SwitchDecision::NoOp => {
                log::info!("switchMode：生成无关变更（norm 等价 + 节点未变）→ 零重启");
                self.commit_applied(&new_config);
                SwitchOutcome::NoOp
            }
            SwitchDecision::Defer => {
                if defer_restart {
                    // 记账：这次落盘没进核。节点差集看不见非节点结构性变更，条上要靠这个标记才不撒谎
                    //（字段注释里有为什么不能现算）。
                    self.restart_deferred.store(true, Ordering::SeqCst);
                    log::info!(
                        "switchMode：「保存不重启」→ 已落盘并进待应用差集，等用户点「立即应用」"
                    );
                } else {
                    log::info!("switchMode：仅新增未引用节点 → 免重启（下次启动/被选中时生效）");
                }
                self.commit_applied(&new_config);
                SwitchOutcome::Deferred
            }
            SwitchDecision::Restart => {
                log::info!("switchMode：结构性变更 → 调度去抖重启");
                self.apply_restart(new_config);
                SwitchOutcome::Restarting
            }
        };
        // A4 触发点②：非重启腿提交后对账登录期出口让位。覆盖两类驱动——
        //  · 切出口（HotSwitched）：切走原让位出口 → stale 复位（清 flag，不 PUT，selector 已被 planHotSwitch 移走）；
        //  · 切「meshLoginFallbackDirect 开关」（NoOp：该字段排除出 norm → 走 no-op 腿）：关开关须即刻 disengage 切回出口。
        // 重启腿不在此对账——重启后 start_inner 的预置 + 首帧 reconcile 覆盖。
        if !matches!(outcome, SwitchOutcome::Restarting) {
            // L3 外化规则「值」热更：norm 排除了外化规则的值 → 结构相等但值可能变（如「切节点 + 改外化规则
            // 值」同一次 save）。非重启腿（热切/no-op/defer）补一次文件对账（通常零 diff、幂等）。降级态文件
            // 无消费者 → 改走去抖重启重落盘（对齐 上游 三腿 :1806-1807/:1850-1851/:1877-1878）。
            if self.custom_rule_files_degraded() {
                self.schedule_restart();
            } else {
                self.sync_custom_rule_files(&new_cfg).await;
            }
            self.reconcile_login_fallback().await;
        }
        // R2 待应用差集 PUSH（单点，最小 runtime 面）：任何经 switch_mode 的落盘（增/删/改节点、排序、
        // `server:switch`）= 上游 `configChanged` 触发点 → 推当下差集给 UI。Defer 腿 added 非空 → 操作条现；
        // 重启腿此刻起核快照未刷、added 仍非空 = **真·待应用**（重启落地后由前端 onStarted pull 清），与 上游
        // 「configChanged 显示、started 清空」同型，非 bug。emitter 未接线（单测）静默跳过，不打断本腿。
        self.push_pending_changes();
        outcome
    }

    /// [`Self::switch_mode_with`] 的**纯判定半边**：候选配置会落哪条腿，不产生任何副作用。
    ///
    /// # 为什么抽出来
    ///
    /// `config:classifyStaged`（spec §2.3.4）要在**保存之前**告诉用户「这批改动保存后需不需要重启」。
    /// 若它自己再实现一遍判定，那么「核未起 / 无基准 / 解析失败 / 逐字节全等」这四条兜底腿就有了
    /// 第二份实现 —— 它们的分歧只会在真机上以「预告说不重启、实际断了流」的形态暴露，
    /// 而这恰恰是最难归因的一类。共用同一函数后，预告与实际**在构造上**不可能分歧。
    ///
    /// # 不含 lifecycle 在飞那一腿（腿 0）
    ///
    /// 「lifecycle 在飞 → 暂存重放」是**时机**而非判据：暂存的那份配置排空后仍会走完整判定。
    /// 把瞬时的忙态算进预告，会让同一批改动在核重启窗口内被预告成另一种结果。
    fn classify_switch(&self, new_config: &Value, defer_restart: bool) -> ClassifiedSwitch {
        // 腿 0.5：核未运行 → 无核可切（下次 start 按新配置生成）。
        if !self.core_running() {
            return ClassifiedSwitch::NotRunning;
        }

        // 核在跑却无 current_config = 不可能态（start 就绪时必置）。保守走重启，绝不猜。
        let Some(old_value) = self.current_config.read().ok().and_then(|g| g.clone()) else {
            return ClassifiedSwitch::Fallback("核在跑但无 current_config 基准");
        };

        // 腿 1：逐字节全等 → 仅更新引用（上游 bug#5）。
        // 键序无关比较：ConfigManager 落盘/回读可能改键序，裸 == 会把「没变」误判成「变了」→ 无谓重启。
        if stable_stringify(new_config) == stable_stringify(&old_value) {
            return ClassifiedSwitch::Unchanged;
        }

        // 腿 2：解析 + 规划。
        // 任一侧解析失败 → 保守重启（fail-closed）：热切换靠精确 diff，解析不出就无从判断，
        // 宁可多断一次流，也不能把「没看懂的变更」当成「无需动作」静默吞掉。
        let (Ok(old_cfg), Ok(new_cfg)) = (
            serde_json::from_value::<UserConfig>(old_value),
            serde_json::from_value::<UserConfig>(new_config.clone()),
        ) else {
            return ClassifiedSwitch::Fallback("配置解析失败");
        };

        // 核在跑却无基准 → 无法判热切换（PUT 目标 tag 无从解析）→ 重启（今日行为）。
        let Some(snapshot) = self.switch_snapshot.read().ok().and_then(|g| g.clone()) else {
            return ClassifiedSwitch::Fallback("无热切换基准快照");
        };

        let deps = HotSwitchDeps {
            current_id_to_tag_map: Some(snapshot.id_to_tag.clone()),
            running_servers_fingerprint: Some(snapshot.fingerprints.clone()),
            current_rule_target_map: Some(snapshot.rule_target.clone()),
            // 登录期出口让位（TS 未就绪时 proxy-selector 实指 direct）属 mesh 批次，未接线 → false。
            // 保守方向正确：false 时旧成员 tag 按 config 选中节点解析，最坏是精准断连漏关几条旧连接
            // （它们会自然结束），绝不影响 PUT 正确性。
            bootstrap_fallback_engaged: false,
        };
        let plan = plan_hot_switch(&old_cfg, &new_cfg, &deps);

        // 决策输入：三个布尔全部由 config-engine 的纯函数现算（不缓存、不自己判等价）。
        let input = DecisionInput {
            norm_equal: config_generation_norm(&old_cfg, None)
                == config_generation_norm(&new_cfg, None),
            selected_server_id_equal: old_cfg.selected_server_id == new_cfg.selected_server_id,
            only_added_unreferenced: can_skip_restart_for_added_unreferenced(
                &old_cfg,
                &new_cfg,
                &snapshot.fingerprints,
            ),
            // `restartOnNodeChange` **不在 Polaris 的 UserConfig 结构体里**（config-engine 的 norm 排除
            // 清单 `orchestration.rs:119` 列了它，但结构体从未建模该字段 → 那条排除对它恒是空转）。
            // 故只能从原始 JSON 读。语义对齐 上游 `validateConfig`（ConfigManager.ts:916）：
            // **非 true 一律 false**（缺键/null/非布尔都按 false=进待应用差集）。
            restart_on_node_change: new_config
                .get("restartOnNodeChange")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            defer_restart,
        };

        ClassifiedSwitch::Decided {
            decision: decide(&plan, &input),
            new_cfg: Box::new(new_cfg),
        }
    }

    /// `config:classifyStaged`（spec §2.3.4）的运行时半边：候选配置**若现在落盘**会走哪条腿。
    ///
    /// 判定复用 [`Self::classify_switch`]，`defer_restart` 恒传 `false` —— 本接口回答的是
    /// 「这批改动**本性上**需不需要重启才生效」，而 `deferRestart` 是「用户要不要现在断流」的
    /// 另一个正交选择。传 `true` 会把结构性变更预告成 Defer，等于用「我打算延后」回答
    /// 「它需不需要重启」，那条预告对用户毫无信息量。
    ///
    /// 三条兜底腿的映射（与执行侧同源，不另立判据）：
    /// - 核未运行 → `noOp` / 不需重启：落盘不触发任何核动作，改动在下次起核时自然生效；
    /// - 逐字节全等 → `noOp`；
    /// - 无基准 / 解析失败 → `restart`：执行侧此时正是保守重启，预告不得比实际乐观。
    pub fn classify_staged(&self, candidate: &Value) -> StagedClassification {
        let decision = match self.classify_switch(candidate, false) {
            ClassifiedSwitch::NotRunning | ClassifiedSwitch::Unchanged => "noOp",
            ClassifiedSwitch::Fallback(_) => "restart",
            ClassifiedSwitch::Decided { decision, .. } => match decision {
                SwitchDecision::HotSwitch(_) => "hotSwitch",
                SwitchDecision::NoOp => "noOp",
                SwitchDecision::Defer => "defer",
                SwitchDecision::Restart => "restart",
            },
        };
        StagedClassification {
            decision,
            // 契约恒等式（spec §2.3.4）：restartRequired = decision ∈ {defer, restart}。
            // `defer` 也算 —— 它的语义正是「已落盘、运行核还没吃进去，要重启才进核」。
            restart_required: matches!(decision, "defer" | "restart"),
        }
    }

    /// 非重启腿（热切/no-op/defer）的收尾：对账 `current_config` + 刷新待决 force-restart 快照。
    ///
    /// H-1（上游 :1792-1801/1846-1847/1875-1876）：这三条腿都不重启，但若有 `apply_pending` 已排程的
    /// 待决 force-restart，其快照仍是**旧** cfg → timer 到点会把核重启回旧节点，把刚热切的结果吃掉。
    /// 故必须把快照**值**刷新到 newConfig，同时**保留 force-restart 意图与 id**（不清空、不换号）。
    fn commit_applied(&self, new_config: &Value) {
        if let Ok(mut g) = self.current_config.write() {
            *g = Some(new_config.clone());
        }
        if let Ok(mut g) = self.pending_force_restart.write() {
            if let Some((id, _)) = g.take() {
                *g = Some((id, new_config.clone()));
            }
        }
    }

    /// 重启腿收尾：对账 `current_config` + **丢弃**待决 force-restart 快照 + 调度去抖重启。
    ///
    /// 上游 :1886-1889：结构性重启用的是最新完整 config → 超代任何待决 force-restart 快照
    /// （newer 胜，避免旧 force cfg 反 shadow 本次变更）。快照清空后，去抖回调按 id 取不到载荷 →
    /// 自然回落 `config.current()`（磁盘上的最新配置）。
    fn apply_restart(self: &Arc<Self>, new_config: Value) {
        if let Ok(mut g) = self.current_config.write() {
            *g = Some(new_config);
        }
        if let Ok(mut g) = self.pending_force_restart.write() {
            *g = None;
        }
        self.schedule_restart();
    }

    /// 建管理 API 客户端（h2c gRPC）。核未起 / 端口未解析 / 连不上 → `not_ready()`（→ 退回重启）。
    ///
    /// 每次热切换现连：tonic channel 是 lazy 的，建连成本低；持久化客户端需处理换核/换端口后的
    /// 失效重建，属 stats-worker 批次的连接管理范畴，本批不引入该状态。
    async fn management_api(&self) -> GrpcManagementApi {
        let status = self.status();
        if !status.running || status.clash_api_port == 0 {
            return GrpcManagementApi::not_ready();
        }
        let secret = self.clash_api_secret();
        match SingBoxApiClient::connect(Endpoint::new("127.0.0.1", status.clash_api_port), secret)
            .await
        {
            Ok(c) => GrpcManagementApi::new(c),
            Err(e) => {
                log::warn!("管理 API 连接失败（热切换将退回重启）: {e}");
                GrpcManagementApi::not_ready()
            }
        }
    }

    /// 管理 API 的 Bearer secret（`clashApiSecret`，缺失/空 → 空串免认证）。热切换与 TS STATUS relay 共用。
    ///
    /// **必须走 `with_current` 投影，不得用 `current()`**：后者恒 clone **整份**用户配置（含全部
    /// `servers` 与规则，`runtime/config.rs:181-189` 明写），而本方法只要一个字符串字段。调用链是
    /// `probe_select_slot → hot_switch_selector → management_api → 本方法` —— **测速一轮 = N 次整份配置
    /// 深拷贝**（200 节点级配置下不是小数目），此外所有热切节点的路径都付这笔账。
    ///
    /// 闭包内禁忌（持读锁，禁再调 `ConfigManager` 任何方法）在此满足：只读一个字符串字段、无 I/O、
    /// 无回调。debug 构型下该禁忌由 `ReentrancyProbe` 有牙。
    fn clash_api_secret(&self) -> String {
        self.config
            .with_current(|c| {
                c.get("clashApiSecret")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// 某个节点在**当前运行核**里的管理 API 落点：`(端口, secret, 该节点的 endpoint tag)`。
    ///
    /// `None` 有两种成因，调用方对用户的表述必须一致（都是「现在做不了」而不是「出错了」）：
    /// 核没在跑；或该 serverId 不在运行快照的 `id_to_tag` 里 —— 后者意味着**核吃进去的那份配置里
    /// 没有这个节点**（刚加还没重启、或已被删）。
    ///
    /// 🔴 **tag 解不到时不得回落成 `server.name` 猜一个**。热重设 exit_node 那条腿有这个回落，
    /// 是因为它猜错只是「热切失败、退回重启」；而 Taildrop 侧猜错的后果是**静默空结果**：核对
    /// 未知 endpointTag 返回的是一帧空收件箱而非错误（`daemon/started_service_taildrop.go:90-97`，
    /// 判据见 `SingBoxApiClient::first_taildrop_inbox_snapshot` 文档）⇒ 用户看到「收件箱是空的」，
    /// 而真实的收件箱在另一个端点上。宁可明说「取不到」。
    pub(crate) fn management_target_for(&self, server_id: &str) -> Option<(u16, String, String)> {
        let status = self.status();
        if !status.running {
            return None;
        }
        let tag = self
            .switch_snapshot
            .read()
            .ok()
            .and_then(|g| g.as_ref().and_then(|s| s.id_to_tag.get(server_id).cloned()))?;
        Some((status.clash_api_port, self.clash_api_secret(), tag))
    }

    /// 用户是否关掉了日志写盘（`disableLogFile`）。**它不只是「不写文件」**：该开关落到生成配置就是
    /// `log.disabled=true`，而 sing-box 见到它直接返回 `NewNOPFactory()`（`log/log.go`）—— 整个日志
    /// 工厂变空实现，`SubscribeLog` 也就永久没有任何一帧。核日志 relay 据此决定压根不订阅。
    ///
    /// 同 [`Self::clash_api_secret`] 走 `with_current` 投影而非 `current()`：只要一个布尔，
    /// 不为它 clone 整份配置。
    fn log_file_disabled(&self) -> bool {
        self.config
            .with_current(|c| c.get("disableLogFile").and_then(Value::as_bool))
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    /// 起核配置的 `endpointTag → serverId` 逆映射（`build_id_to_tag_map` 的逆）。
    ///
    /// TS STATUS 帧里端点以 `endpointTag`（= 节点显示名去重后的 outbound tag）标识；解码时据此逆映回
    /// `serverId`。**在起核时刻从核实际启动的那份配置构建**（而非读 `current_config`）：核发的 tag 恒是它
    /// 启动时的 tag，rename-without-restart 也不改运行核 tag，故 start 快照才与核 wire 一致。tag 唯一 →
    /// 逆映射 1:1；撞名（`build_id_to_tag_map` 追加 `(n)`）后仍唯一。
    fn ts_tag_to_id(user_config: &UserConfig) -> BTreeMap<String, String> {
        struct SrvLike<'a>(&'a polaris_config_engine::user_config::server_config::ServerConfig);
        impl ServerLike for SrvLike<'_> {
            fn id(&self) -> &str {
                &self.0.id
            }
            fn name(&self) -> &str {
                &self.0.name
            }
        }
        let wrappers: Vec<SrvLike> = user_config.servers.iter().map(SrvLike).collect();
        build_id_to_tag_map(&wrappers)
            .into_iter()
            .map(|(id, tag)| (tag, id))
            .collect()
    }

    /// **A3 relay 每帧处理**（可测的纯接线段：解码 → 更缓存 → 逐端点 emit）。
    ///
    /// 拆成独立方法而非埋在 spawn 循环里：让「一帧全量端点快照 → 缓存更新 + `event:tailscaleStatus` 逐条发」
    /// 这条组合路径能被单测直接喂 mock 帧断言（§K7.1：测组合路径，别只测纯函数或只测 spawn）。
    ///
    /// **同时是 上游 触发表第四点「TS 隧道就绪」的接线处**（§10.1）：mesh 出口从 `NeedsLogin`/`Starting`
    /// 跃迁到 `Running` 的那一刻，公网流量才真正开始经 TS 出口走 —— 出口 IP 就此换掉，与起核/热切同性质。
    /// 判据取「**选中出口**的 backendState 由非 Running 变为 Running」的边沿（见 [`ts_exit_became_ready`]）。
    ///
    /// **同时是 R2「TS 出口无效直判翻转对账」的接线处**：缓存换完（`peers`/`loggedIn` 已是本帧最新）后
    /// 立即跑 [`reconcile_ts_exit_block`](Self::reconcile_ts_exit_block)。
    ///
    /// `self: &Arc<Self>`（原 `&self`）：R2 恢复腿要 spawn 一个持 runtime 的后台任务（reassert 在
    /// macOS 上可轮询到 ~18s，绝不能在 relay 的取帧循环里同步等）。
    ///
    /// `my_gen` = **本帧所属的核会话世代**（relay 起时的快照）。往下透给
    /// [`reconcile_ts_exit_block`](Self::reconcile_ts_exit_block) 做锁内判权 —— relay 的收帧世代复查
    /// 之后本方法还要跑一整段，那段里停核完全可能跑完 bump + 复位，见该方法文档。
    fn apply_ts_status_frame(
        self: &Arc<Self>,
        update: &polaris_singbox_grpc::daemon::TailscaleStatusUpdate,
        tag_to_id: &BTreeMap<String, String>,
        my_gen: u64,
    ) {
        let events = decode_tailscale_status(update, tag_to_id);
        // 选中出口在**本帧之前**的 backendState —— 边沿判定的左值，必须在换缓存之前取。
        let selected_id = self.selected_server_id();
        let before = selected_id
            .as_deref()
            .and_then(|id| self.mesh.selected_exit_backend_state(id));
        // 缓存整体替换（每帧即全量）——供 `tailscale_get_status` 拉末帧。
        self.mesh.update_ts_status(events.clone());
        // 逐端点 emit（前端 `onTailscaleStatus` 逐条消费）。未接线 emitter（单测/setup 前）→ 静默跳过。
        if let Some(emitter) = self.error_emitter.get() {
            for ev in &events {
                emitter.emit_tailscale_status(ev);
            }
        }
        let after = selected_id
            .as_deref()
            .and_then(|id| self.mesh.selected_exit_backend_state(id));
        // 上游 触发点④「TS 隧道就绪」：**只在上升沿**触发，不是每帧（relay 每秒量级推帧，
        // 稳态 Running 帧若也触发就成了轮询——而本子系统的设计前提是纯事件驱动、无轮询）。
        if ts_exit_became_ready(before.as_deref(), after.as_deref()) {
            log::debug!("TS 隧道就绪（{before:?} → Running）→ 失效解锁缓存 + 重探出口 IP");
            // 新出口上线 ⇒ 解锁快照作废（与起核/热切/停核三点同语义）。
            self.invalidate_unlock_cache(true, false);
            // 新出口上线 ⇒ 状态栏出口 IP、两处旗面、伴测延迟全部作废，须重探（等选路收敛 4s）。
            self.schedule_exit_ip_refresh(true);
        }
        // R2：出口无效直判翻转对账（缓存已是本帧最新 → 据最新 peers/loggedIn 判 blocked 跨态）。
        // 与上面的「隧道就绪」上升沿正交：那条判 backendState（隧道通没通），这条判 exit_node 有没有
        // （通了也可能出口设备离线/未广告 ⇒ 公网出不去）。上游 同处也是两条并列（:7345-7346）。
        self.reconcile_ts_exit_block(my_gen);
    }

    // ══════════════ R2：TS 出口无效直判【翻转对账】 + 出口恢复腿 ══════════════
    //
    // 1:1 移植 上游 `reconcileTsExitBlock`（ProxyManager.ts:2596-2617）+ `recoverTsExit`（:2620-2646）
    // + `reapplyTsExitNode`（:2653-2690）。
    //
    // **拉侧已在（`commands/unlock.rs::compute_selected_exit_blocked` → `unlock_gate_reason` 的
    // `exit_blocked`），本段补的是推侧**：拉侧只在用户点检测那一刻求值，出口从无效恢复成有效后
    // 没有任何东西会来重检 —— 拉侧越准，推侧缺失就越显形（用户看到的是「出口无效」一直挂着）。

    /// `TsExitWarning` → 前端契约 `ProxyExitBlock` 值域（`ui/src/contracts/types/runtime.ts`）。
    ///
    /// 纯投影，与 上游 `selectedTsExitBlock` 的三条 map 逐条对齐。**值域单一真值**：三个字符串只在这里
    /// 出现一次，别处（emitter / 日志）一律传本函数的产物，杜绝「后端发 `ts-exit-not-advertised`、
    /// 前端判 `ts-not-advertised`」这类拼串漂移。
    #[must_use]
    fn ts_exit_block_reason(w: TsExitWarning) -> Option<&'static str> {
        match w {
            TsExitWarning::None => None,
            TsExitWarning::NeedsAuth => Some("ts-needs-auth"),
            TsExitWarning::NoExitDevice => Some("ts-no-exit-device"),
            TsExitWarning::ExitDeviceOffline => Some("ts-exit-device-offline"),
            TsExitWarning::ExitDeviceNotAdvertised => Some("ts-exit-not-advertised"),
        }
    }

    /// 选中 TS 出口当前是否被直判无效（`None` = 有效 / 不适用）。上游 `selectedTsExitBlock`。
    ///
    /// 输入三源：当前配置（选中节点 + `proxyMode`）、STATUS 末帧（`loggedIn` / `peers`）、核 running
    /// （= STATUS 流是否 live；新鲜度守卫已内建在 [`derive_ts_exit_warning`]，流断时不据陈旧 peers 报离线）。
    ///
    /// 与 `commands/unlock.rs::compute_selected_exit_blocked`（拉侧）**同谓词不同调用时机**：谓词本体
    /// [`derive_ts_exit_warning`] 是单一真值（两侧都调它），此处多出的只是「从 runtime 自身取三源」这段
    /// 装配 —— 拉侧从 `State<AppRuntime>` 取、推侧从 `self` 取，无法共用同一个签名。
    ///
    /// **配置源刻意取 `ConfigManager` 的落盘态（此处经 `with_current` 投影）而非 `current_config`
    /// （运行核那份）**，这一点偏离
    /// 上游（它读 `this.currentConfig`）：Polaris 的拉侧读的就是落盘态，两侧若各读一份，会出现
    /// 「推侧广播了出口无效终态、拉侧的 gate 却判有效（或反过来）」的自相矛盾 —— 用户看到的是角标与
    /// 检测结果打架。宁可与**同一子系统的另一侧**对齐，也不为形式上贴近上游而制造两个真相源。
    fn selected_ts_exit_block(&self) -> Option<&'static str> {
        // 廉价前置（**只跳过工作、不改结论**）：STATUS 缓存里一个在册端点都没有（无 TS 节点 / 核未跑 /
        // 首帧未到）⇒ `logged_in` 恒 false ⇒ [`derive_ts_exit_warning`] 必在第一道守卫返 None。
        // 挡在这里是因为下面那次配置读**本可能**深拷贝整份配置（含 200 节点级 servers 数组），
        // 而本方法由 STATUS relay 每帧（~1/s）驱动 —— 正是 `selected_server_id` 文档点名要避免的那类
        // 常驻开销。等价性由 `exit_block_is_none_when_status_cache_empty` 钉住。
        if !self.mesh.has_ts_status() {
            return None;
        }
        // **零深拷贝 + 只投影三个字段**：走 [`ConfigManager::with_current`]（持读锁投影，不产 owned
        // `Value`）而非 `current()`（恒 clone 整份）；闭包内也不 `from_value::<UserConfig>(整份)` ——
        // 那会把 200 节点级的 `servers` 全量建成 typed 结构（每个 `ServerConfig` 又带若干
        // `Option<...Settings>` / `Vec<String>`），而谓词只要 `selectedServerId` + **被选中的那一个**
        // server + `proxyMode` 三样。两半浪费（整份 clone、整份反序列化）在此一并消掉。
        //
        // 逐字段投影与整份反序列化的**结论等价**（`selected_ts_exit_block_projection_matches_typed_parse`
        // 用同一份配置双路对拍钉住）：三个键的 serde 表示都是平凡的（`Option<String>` / 数组 /
        // `rename_all = "lowercase"` 的枚举），且谓词对其余字段一概不看。
        // 唯一的行为差异在退化输入上——某个**无关**字段坏掉时，投影不再连带把整个判定短路成 None。
        // 方向是 fail-safe 的（坏字段不再静默吞掉出口告警），且配置在 `ConfigStore::load` 已过校验。
        //
        // ⚠️ 闭包内**只做纯投影**：`ConfigManager` 的读锁正持着，回调进 `self.mesh` / `self.status()`
        // 之类的子系统是禁忌（见 `with_current` 文档）。故 `ts_status_event` / `status()` 一律留到
        // 闭包**外**再取。
        let (sel_id, selected, proxy_mode_direct) = self
            .config
            .with_current(|raw| {
                let sel_id = raw.get("selectedServerId")?.as_str()?.to_string();
                let selected: Option<ServerConfig> = raw
                    .get("servers")?
                    .as_array()?
                    .iter()
                    .find(|s| s.get("id").and_then(Value::as_str) == Some(sel_id.as_str()))
                    .and_then(|s| serde_json::from_value(s.clone()).ok());
                let proxy_mode_direct = raw.get("proxyMode").and_then(Value::as_str)
                    == Some(ProxyMode::Direct.as_str());
                Some((sel_id, selected, proxy_mode_direct))
            })
            .ok()
            .flatten()?;
        let event = self.mesh.ts_status_event(&sel_id);
        let (logged_in, peers, definitive_logged_out) =
            event.as_ref().map_or((false, &[][..], false), |e| {
                (e.logged_in, e.peers.as_slice(), is_definitive_logged_out(e))
            });
        Self::ts_exit_block_reason(derive_ts_exit_warning(&TsExitWarningInput {
            selected: selected.as_ref(),
            logged_in,
            proxy_mode_direct,
            proxy_running: self.status().running,
            peers,
            definitive_logged_out,
        }))
    }

    /// **R2 翻转对账**（每帧 STATUS 末尾跑）：仅在 `cur != prev` 的**跨态**动作，同态帧一律早退。
    ///
    /// 三分支（上游 `reconcileTsExitBlock` 1:1）：
    /// - `none → blocked`（含 `blocked → blocked'` 原因变更）：出口 IP **不探测直落终态**
    ///   （[`mark_exit_blocked`](Self::mark_exit_blocked)）—— 探测在这种形态下必然打空转，20s 重试预算
    ///   耗尽后仍是 null，用户看到「一直在检测」而不是「出口无效」；同时令解锁快照失效并**带
    ///   `exit_blocked=true`**（渲染端据此复位 idle 而非留着陈旧绿点，R-gate 拦重跑）。
    /// - `blocked → none`：起**出口恢复腿**（R1 热重设 exit_node → reassert System 路由 → 重探），
    ///   并令解锁快照失效（有效出口恢复 ⇒ 自动重检，与重探同节奏）。
    /// - 同态：零动作（relay 每秒量级推帧，level 触发就是每秒一次重探 + 每秒一次解锁失效）。
    ///
    /// **缓存先于动作更新**：先写 `last_ts_exit_block` 再动作，恢复腿里的 `selected_ts_exit_block()`
    /// 复查读到的才是本次已提交的态。
    ///
    /// # `my_gen`：帧所属的核会话世代（**锁内**比对）
    ///
    /// relay 在收帧后已复查过一次世代（`spawn_tailscale_status_relay` 的取帧腿），但那之后还要跑完整个
    /// `apply_ts_status_frame`。停核腿是「`bump_generation()` → … → `reset_ts_exit_block_state()`」，
    /// 若本函数尾部的缓存写入晚于那次复位，`last_ts_exit_block` 就带着 `Some(reason)` 漏进**新会话**
    /// ⇒ 重连后同因 blocked 的首帧被同态早退吞掉，终态**永远落不下去**（而 `reconcile` 是边沿触发，
    /// 没有轮询会来纠正）。
    ///
    /// 判据放在 `last_ts_exit_block` 的锁内、而不是函数入口：`reset_ts_exit_block_state` 持的是**同一把**
    /// 锁，故「判世代 + 写缓存」与「bump + 复位」不会交叉；放锁外就还是 check-then-act。
    fn reconcile_ts_exit_block(self: &Arc<Self>, my_gen: u64) {
        let cur = self.selected_ts_exit_block();
        let prev = match self.last_ts_exit_block.lock() {
            Ok(mut g) => {
                if self.gate.generation() != my_gen {
                    return; // 本帧所属的核会话已被停核/换核/新 start 接管 → 不得写进新会话的缓存
                }
                if *g == cur {
                    return; // 同态 → 零动作（挡住 level 触发退化成每秒轮询）
                }
                std::mem::replace(&mut *g, cur)
            }
            // 锁中毒 → 放弃本次对账（best-effort：出口对账绝不该反过来打断 STATUS 帧处理）。
            Err(_) => return,
        };
        let running = self.status().running;
        if let Some(reason) = cur {
            log::info!("TS 出口直判无效（{prev:?} → {reason}）→ 出口 IP 落终态 + 解锁快照失效");
            // 跨态即令解锁检测失效（G-flip）。`exit_blocked=true` 是本参数**唯一**的生产真值来源：
            // 其余三个触发点（起核/停核/热切）传的都是 false。
            self.invalidate_unlock_cache(running, true);
            // 出口 IP 腿：无探测直落「出口无效」终态（与 schedule_exit_ip_refresh 互斥的另一条出口）。
            self.mark_exit_blocked(reason);
        } else {
            log::info!("TS 出口恢复有效（{prev:?} → none）→ 热重设 exit_node + 重申路由 + 重探");
            self.invalidate_unlock_cache(running, false);
            // 出口 IP 腿：恢复腿内部按「reapply → reassert → refresh」顺序收尾重探（顺序不可换，
            // 见 ts_exit_recover_once）。
            self.spawn_ts_exit_recovery();
        }
    }

    /// **R2 恢复腿的单飞抢占**（同步、可直测）：抢到 → `true`；已在飞 → 记 pending 并 `false`。
    ///
    /// 抽成独立同步方法而不是内联进 [`spawn_ts_exit_recovery`]：单飞 + 补跑是这条腿唯一的状态机，
    /// 而 spawn 出去的异步体在单测里无法确定性观测 —— 门要能被看见（§K7）。
    fn begin_ts_exit_recovery(&self) -> bool {
        if self.ts_exit_recovering.swap(true, Ordering::SeqCst) {
            self.ts_exit_recover_pending.store(true, Ordering::SeqCst);
            return false;
        }
        true
    }

    /// **R2 恢复腿收尾时的「丢边沿补救」判定**（同步、可直测；[`TsExitRecoverGuard`] 的 Drop 唯一消费方）。
    ///
    /// 取走 pending 边沿并回答「该不该再起一轮」。抽成独立同步方法而非内联进 Drop：Drop 里那一步会
    /// `spawn` 出后台任务，单测无法确定性观测 —— 门要能被看见（§K7）。
    ///
    /// 三条判据缺一不可：
    /// - `swap(false)` 取到边沿：`load` 会让边沿留在位上被下一次 Drop 重复消费；
    /// - `status().running`：核已停（或 restart 的停核窗口）时 `selected_ts_exit_block()` 恒 `None`
    ///   （STATUS 缓存已清），只看它会把「**没有核**」误读成「出口有效」⇒ 对着已停的核重申路由 + 重探；
    /// - `selected_ts_exit_block().is_none()`：在飞期间 flap 回 blocked 就别对着已知无效的出口空跑。
    fn take_ts_exit_recover_rerun(&self) -> bool {
        self.ts_exit_recover_pending.swap(false, Ordering::SeqCst)
            && self.status().running
            && self.selected_ts_exit_block().is_none()
    }

    /// **R2 恢复腿**（`blocked → none` 触发，串行单飞 + 补跑门）。fire-and-forget，绝不抛。
    ///
    /// `tauri::async_runtime::spawn` 而非 `tokio::spawn`：本方法的调用链可自**同步** Tauri command
    /// 路径进入（`apply_ts_status_frame` 的测试腿与将来的同步驱动源），裸 `tokio::spawn` 在无 runtime
    /// 上下文时当场 panic，而 panic 在 Tauri IPC 回调里无处可 catch ⇒ `abort()`（2026-07-21 真机
    /// SIGABRT 血证，见 `runtime::unlock::schedule_self_run`）。
    /// **世代守卫**：spawn 那一刻快照 `gate.generation()`，整条腿（含补跑轮）都在这个世代名下跑。
    /// 这条 `'static` 任务能活过停核 / 换核 / 新 start，而它的三步全是**对着当前核**的动作 ——
    /// 没有守卫时旧腿的三条坏后果（见 [`ts_exit_recover_once`](Self::ts_exit_recover_once) 文档）
    /// 每条都能独立发生。
    fn spawn_ts_exit_recovery(self: &Arc<Self>) {
        if !self.begin_ts_exit_recovery() {
            return; // 在飞 → 已记 pending，由在飞那轮收尾补跑
        }
        let me = Arc::clone(self);
        let my_gen = self.gate.generation();
        tauri::async_runtime::spawn(async move {
            // 单飞标志的复位走 Drop 守卫（同 `ReconcileGuard` 的理由）：任一步 panic 也必复位。
            // 漏复位 = 本会话此后**所有**真恢复都被单飞永久吞掉，且没有任何可见症状。
            let _guard = TsExitRecoverGuard(Arc::clone(&me));
            loop {
                me.ts_exit_recover_pending.store(false, Ordering::SeqCst);
                me.ts_exit_recover_once(my_gen).await;
                // 世代守卫：本轮跑完发现已被停核/换核/新 start 接管 → 整腿退场，别拿旧世代再跑补跑轮
                // （补跑轮的三步同样是对着「当时那个核」的动作）。留下的 pending 由 Drop 按当前世代裁定。
                if me.gate.generation() != my_gen {
                    log::debug!(
                        "TS 出口恢复腿世代变（{my_gen}→{}）→ 退场",
                        me.gate.generation()
                    );
                    break;
                }
                // 补跑门：在飞期间又发生过 flip **且**当下仍是有效出口 → 再跑一轮。
                // 少了「仍为 none」这条，flap 到 blocked 时会对着一个已知无效的出口空跑恢复。
                if !(me.ts_exit_recover_pending.load(Ordering::SeqCst)
                    && me.selected_ts_exit_block().is_none())
                {
                    break;
                }
            }
        });
    }

    /// **R2 恢复腿单轮**（上游 `recoverTsExit` 的循环体）。三步**顺序不可换**：
    ///
    /// 1. [`reapply_ts_exit_node`](Self::reapply_ts_exit_node)：re-advertise 后运行中的 sing-box **不随
    ///    netmap 重解析 exit_node**（上游 watchState 缺陷）⇒ 不热重设的话，出口在 tailnet 侧已恢复、
    ///    核内部却还指着「已失效」的解析结果，后面两步全白做；
    /// 2. `exit_route_reassert`：补 macOS `resolveIface` 18s 轮询超时那次没装成的 System 出口路由
    ///    （crate 侧只在「从未装成 / iface 已消失」两种真缺口下动手，不 churn 已存路由）；
    /// 3. `schedule_exit_ip_refresh`：**最后**才重探——前两步没做完就探，探到的还是恢复前的出口。
    ///
    /// 全程 best-effort、绝不抛：恢复属增益路径，任一步失败都不该污染 STATUS 帧处理或阻断后续轮。
    ///
    /// # 世代守卫：**每步之前**都要比对，不是只在入口判一次
    ///
    /// 本腿跑在 `spawn` 出去的 `'static` 任务里，可以活过停核 / 换核 / 新 start，而三步全是「对着**当前**
    /// 核」的动作。三条坏后果各自独立（缺任一道守卫就漏一条）：
    ///
    /// 1. `exit_route_reassert` 持 `mesh.exit_route` 的 tokio Mutex，macOS 下 `find_tailnet_iface` 最长
    ///    轮询 ~18s（`mesh.rs` `MACOS_RESOLVE_ATTEMPTS × MACOS_RESOLVE_DELAY`）—— 停核腿的
    ///    `exit_route_clear` 与新 start 每轮的 `exit_route_snapshot_baseline` 都排在它后面 ⇒
    ///    **点停止最长卡 18s**。守卫挡住的是「已被接管的旧腿还去**开启**一轮新的 18s 轮询」；
    /// 2. 快速 stop→start 后的 stale 腿会看到 `installed=None`（停核已清）+ 新核 utun 已现，于是按
    ///    **旧会话**的 `current_config`（停核**不**清它）重装出口路由，与新会话的 reconcile 争路；
    /// 3. 收尾的 `schedule_exit_ip_refresh(true)` 是「代理在跑」语义：停核后落地会去重探一个已死的核，
    ///    并可能**后发覆盖** `stop_inner` 那次 `schedule_exit_ip_refresh(false)`。
    ///
    /// 三步之间隔着 gRPC 往返与最长 18s 的路由手术，世代随时可能变 —— 只在入口判一次等于没判。
    async fn ts_exit_recover_once(&self, my_gen: u64) {
        if self.gate.generation() != my_gen {
            return;
        }
        let reapplied = self.reapply_ts_exit_node().await;
        log::debug!(
            "TS 出口恢复腿：热重设 exit_node {}",
            if reapplied { "已下发" } else { "跳过" }
        );
        // ② 之前：别拿旧会话的 current_config 去给新会话（或已停的核）装出口路由，也别再开一轮 18s 轮询。
        if self.gate.generation() != my_gen {
            return;
        }
        if let Some(cfg) = self
            .current_config
            .read()
            .ok()
            .and_then(|g| g.clone())
            .and_then(|v| serde_json::from_value::<UserConfig>(v).ok())
        {
            let ipv6 = cfg.enable_ipv6.unwrap_or(false);
            self.mesh.exit_route_reassert(&cfg, ipv6).await;
        }
        // ③ 之前：`running=true` 语义的重探不得在核已停/已换之后落地（会覆盖停核腿的 refresh(false)）。
        if self.gate.generation() != my_gen {
            return;
        }
        self.schedule_exit_ip_refresh(true);
    }

    /// **R1 热重设选中 TS 出口的 `exit_node`**（gRPC `SetTailscaleExitNode` → `EditPrefs{ExitNodeID}`，
    /// 幂等，免整核重启）。上游 `reapplyTsExitNode`。返回是否真的下发了一次。
    ///
    /// 守卫链（任一不满足 → 跳过返 `false`，**绝不猜**）：核 running → 选中节点存在且协议为 tailscale →
    /// 配了非空 `exitNode` → STATUS 末帧 `peers` 里能按 `ip` / `hostName` 双口径匹配到该 peer →
    /// 该 peer 带 `stableID`（旧核不发 → None）。切走出口 / 未配出口的场景被守卫天然跳过（此时恢复腿
    /// 仍会走 reassert + 重探，不受影响）。
    ///
    /// `endpoint_tag` 取**运行核快照**的 `id_to_tag`（`build_id_to_tag_map` 产物，含撞名去重后缀）而非
    /// 裸 `server.name`：核发的 endpointTag 恒是它启动时的 tag，撞名节点用裸 name 会打到错的端点上。
    /// 快照缺失（核未起）→ 退回 `server.name`（与 上游 一致），此时守卫链的 running 条件通常已拦下。
    ///
    /// 同值 `EditPrefs` 在核侧是 no-op ⇒ 对「本就已生效」零副作用，故不做「值没变就不发」的短路
    /// （那需要缓存上次下发值，多一个会与核真态脱节的状态）。
    async fn reapply_ts_exit_node(&self) -> bool {
        let status = self.status();
        if !status.running || status.clash_api_port == 0 {
            return false;
        }
        let Some(cfg) = self
            .current_config
            .read()
            .ok()
            .and_then(|g| g.clone())
            .and_then(|v| serde_json::from_value::<UserConfig>(v).ok())
        else {
            return false;
        };
        let Some(sel_id) = cfg.selected_server_id.as_deref() else {
            return false;
        };
        let Some(server) = cfg.servers.iter().find(|s| s.id == sel_id) else {
            return false;
        };
        if server.protocol != Protocol::Tailscale {
            return false;
        }
        let exit_node = server
            .tailscale_settings
            .as_ref()
            .and_then(|t| t.exit_node.as_deref())
            .map(str::trim)
            .filter(|e| !e.is_empty());
        let Some(exit_node) = exit_node else {
            return false; // 未配出口（切走 / 仅内网）→ 无可重设
        };
        let Some(event) = self.mesh.ts_status_event(sel_id) else {
            return false;
        };
        let Some(stable_id) = event
            .peers
            .iter()
            .find(|p| p.ip == exit_node || p.host_name == exit_node)
            .and_then(|p| p.stable_id.clone())
        else {
            log::debug!("热重设 exit_node 跳过：peers 未解到 stableID（exitNode={exit_node}）");
            return false;
        };
        let endpoint_tag = self
            .switch_snapshot
            .read()
            .ok()
            .and_then(|g| g.as_ref().and_then(|s| s.id_to_tag.get(sel_id).cloned()))
            .unwrap_or_else(|| server.name.clone());
        let secret = self.clash_api_secret();
        let client = match SingBoxApiClient::connect(
            Endpoint::new("127.0.0.1", status.clash_api_port),
            secret,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                log::warn!("热重设 exit_node：连管理 API 失败 {e}");
                return false;
            }
        };
        match client
            .set_tailscale_exit_node(endpoint_tag, stable_id)
            .await
        {
            Ok(()) => {
                log::info!("已热重设 TS exit_node → {exit_node}（免重启核）");
                true
            }
            Err(e) => {
                log::warn!("热重设 exit_node 失败：{e}");
                false
            }
        }
    }

    /// **R2 会话起点复位**（上游 `lastTsExitBlock = null`，`ProxyManager.ts:695`）：停核 / 崩溃拆除时调。
    ///
    /// 不复位的后果：停核时缓存停在 `Some(reason)`，重连同一节点后**首帧**若判有效（`None`）会被当成
    /// 一次 `blocked→none` 跨态而白跑一轮恢复腿；更糟的是停在 `None` 时，重连后仍无效的出口不会再触发
    /// `none→blocked` ⇒ 终态永远落不下去。
    ///
    /// # 🔴 为什么**不**顺手清 `ts_exit_recovering` / `ts_exit_recover_pending`（曾经清过，是移植新增偏离）
    ///
    /// 那两个原子是**在飞任务的所有权令牌**，而本方法看不见在飞任务。清了会出两种坏账：
    /// - 新会话可以在旧腿还在飞时再抢一次令牌 ⇒ 两条恢复腿并发跑同一套 route 手术；
    /// - 更糟：旧腿退出时 [`TsExitRecoverGuard`] 的 Drop 会把**新会话**刚置的 recovering/pending 清掉
    ///   ⇒ 单飞被打穿（第三条腿又能进），且新会话记下的边沿被静默抹掉。
    ///
    /// 令牌只由持有者的 Drop 归还，而 Drop 会按**当下**的核状态决定要不要补跑（见该守卫文档）——
    /// 「跨会话残留 `recovering=true`」在那之后不可达：Drop 在 panic 展开时同样执行，没有绕过它的退出路径。
    /// 上游侧本就只清 `lastTsExitBlock`，此处回归对齐。
    fn reset_ts_exit_block_state(&self) {
        if let Ok(mut g) = self.last_ts_exit_block.lock() {
            *g = None;
        }
    }

    /// 当前落盘配置的 `selectedServerId`（空串 / 缺失 → `None`）。
    ///
    /// **持读锁直接取 `&str`，不 clone 整份 `Value`**：唯一调用方 [`Self::apply_ts_status_frame`] 由
    /// TS STATUS relay 每秒量级调用，且每帧调**两次**（换缓存前后各取一次做边沿判定）。`g.clone()`
    /// 会深拷贝整份 `UserConfig` JSON（含 200 节点级别的 `servers` 数组）—— 语义无误，但那是常驻开销。
    fn selected_server_id(&self) -> Option<String> {
        let guard = self.current_config.read().ok()?;
        guard
            .as_ref()?
            .get("selectedServerId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// **#9**：TUN 起核就绪后延迟一次连接 flush（`spawn_crash_monitor` 的世代范式）。
    ///
    /// 为什么需要：app 在 TUN 建立**之前**发起的连接已经泄漏成真实 IP，起核后它们的后续包仍走物理
    /// 网卡直出 —— 用户看到「已连接」，实际那几条连接从未进过代理。延迟一小段后 `CloseAllConnections`
    /// 把它们 RST 掉，逼 app 重连、DNS 经 FakeIP 重新反查，从而落到代理链上。**不重启内核**
    /// （与切节点的 `interruptConnectionsOnSwitch` 开关正交，那条管的是热切换）。
    ///
    /// 代价是无差别 RST 也会重置 flush 之前用户新建的正确连接（app 自行重连、短暂抖动）——
    /// 属「启用代理即断开现有连接」的固有代价，用单次短窗口把误伤面压到最小。
    ///
    /// **两条守卫都在 [`flush_connections_once`](Self::flush_connections_once) 里**，本方法只负责
    /// 「等一段时间再问一次」。刻意不在这里预先判 TUN 早退：判据留在**单一决策点**上，才能被单测直接
    /// 覆盖到；非 TUN 时多出的那个 sleep 任务在 1.5s 后自行退场，代价可忽略。
    ///
    /// 世代守卫替代了 上游的 `clearTimeout` 取消腿：本仓 stop/restart 一律先 bump 世代，
    /// 到点的回调自查即让位，无需再维护一个可取消的 timer 句柄。
    fn schedule_connection_flush(
        self: &Arc<Self>,
        mode: ProxyModeType,
        my_gen: u64,
        api_port: u16,
    ) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(CONNECTION_FLUSH_DELAY_MS)).await;
            match me.flush_connections_once(mode, my_gen, api_port).await {
                FlushOutcome::Flushed => {
                    log::info!("TUN 起核连接 flush：CloseAllConnections → ok（旧连接已 RST）");
                }
                // best-effort：flush 与起核成功正交，失败只记日志，绝不反向影响已就绪的核。
                FlushOutcome::ConnectFailed(e) => {
                    log::warn!("TUN 起核连接 flush：管理 API 连接失败（apiPort={api_port}）: {e}");
                }
                FlushOutcome::CallFailed(e) => {
                    log::warn!("TUN 起核连接 flush：CloseAllConnections 失败: {e}");
                }
                // 三条跳过腿都是正常形态（非 TUN / 已被接管 / 核已停）→ debug 级，不进用户可见日志。
                skipped => log::debug!("TUN 起核连接 flush 跳过：{skipped:?}"),
            }
        });
    }

    /// [`schedule_connection_flush`](Self::schedule_connection_flush) 的**单一决策点**：两条守卫 + 开枪。
    ///
    /// 守卫漏任何一条都是新 bug，不是「少一层防御」：
    /// 1. **仅 TUN**：`systemProxy` / `manual` 的旧连接多在 sing-box 连接表之外，无差别 RST 够不着
    ///    它们，却会误伤已经过代理的连接 —— 净负收益；
    /// 2. **世代 + 核在跑**：延迟窗口内可能已被 stop / 重启接管，这一枪会打到**已经换掉的核**上，
    ///    把新核刚建立的连接全 RST 掉。`connect` 本身是 await 点，故其后再复查一次。
    async fn flush_connections_once(
        &self,
        mode: ProxyModeType,
        my_gen: u64,
        api_port: u16,
    ) -> FlushOutcome {
        if !mode.is_tun() {
            return FlushOutcome::SkippedNotTun;
        }
        if self.gate.generation() != my_gen {
            return FlushOutcome::SkippedSuperseded;
        }
        if !self.status().running {
            return FlushOutcome::SkippedCoreStopped;
        }
        let secret = self.clash_api_secret();
        let client =
            match SingBoxApiClient::connect(Endpoint::new("127.0.0.1", api_port), secret).await {
                Ok(c) => c,
                Err(e) => return FlushOutcome::ConnectFailed(e.to_string()),
            };
        // 建连是 await 点：期间可能被接管 —— 复查过世代才真开枪。
        if self.gate.generation() != my_gen {
            return FlushOutcome::SkippedSuperseded;
        }
        match client.close_all_connections().await {
            Ok(()) => FlushOutcome::Flushed,
            Err(e) => FlushOutcome::CallFailed(e.to_string()),
        }
    }

    /// **A3**：核就绪后挂 Tailscale STATUS relay（`spawn_crash_monitor` 的世代范式）。
    ///
    /// 订阅运行核管理 API 的 `SubscribeTailscaleStatus` 流（自动重连），每帧解码 → 更新末帧缓存 +
    /// `event:tailscaleStatus`。**世代守卫**：`my_gen ≠ 当前世代`（被更新的 start/stop 接管）→ 退场并
    /// drop 流（停订阅、停重连），绝不让旧核的 relay 污染新核。因 [`ReconnectingStream`] 永不自行结束
    /// （断开即退避重连），必须有独立的周期 tick 兜底世代检查——否则「核停了但一直没帧」时 relay 会
    /// 泄漏、对死端口无限重连。tick 复用 `spawn_crash_monitor` 的 1s 量级。
    ///
    /// # 停流自愈（2026-08-02 真机实证）
    ///
    /// 真机上出现过**首帧之后再没有第二帧**：核在 22:38:43 起、tsnet 在 22:38:46 拿到 tailnet IPv4
    /// 并正常带流量（核日志 186 条成功 outbound），而 Polaris 的末帧缓存到 22:39:09 仍是首帧那个
    /// `NoState` —— 表现为「TS 管理后台显示 Connected，节点卡片却说尚未登录就绪、测速被挡、出口卡
    /// 显示 `—`」。上下游都读过：核侧 `SubscribeTailscaleStatus` 是订阅即先发一帧快照、其后靠
    /// `WatchNotifications` 推；本侧 `ReconnectingStream` 的状态存在结构体里、`timeout` 丢弃 future
    /// 是 cancel-safe 的。**为什么通知没再来，静态读不出来。**
    ///
    /// 故不赌成因，按「重订阅必得当前真值」这个上游结构事实兜底：**长时间无帧且末帧不是全 Running**
    /// → 丢掉旧流重订一条。核侧 `sendStatus()` 在挂 watcher 之前先跑，一次重订阅必然拿到此刻的真状态。
    /// 稳态（全 Running）**不重订**——那时无帧是正常的（没有变化就没有通知），churn 无意义。
    /// 阈值指数退避（15s → 30s → … → 5min 封顶）：真的卡在 `NeedsLogin` 时不至于每 15 秒空转一次。
    fn spawn_tailscale_status_relay(
        self: &Arc<Self>,
        my_gen: u64,
        api_port: u16,
        tag_to_id: Arc<BTreeMap<String, String>>,
    ) {
        /// 无帧多久后开始怀疑流停了（首个阈值，其后每次重订阅翻倍）。
        const RESUBSCRIBE_IDLE_MS: u64 = 15_000;
        /// 退避封顶：卡在 `NeedsLogin` 这类稳定非就绪态时，最慢 5 分钟才重订一次。
        const RESUBSCRIBE_IDLE_MAX_MS: u64 = 300_000;

        let me = Arc::clone(self);
        tokio::spawn(async move {
            let secret = me.clash_api_secret();
            let client =
                match SingBoxApiClient::connect(Endpoint::new("127.0.0.1", api_port), secret).await
                {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("TS STATUS relay 连接管理 API 失败（apiPort={api_port}）: {e}");
                        return;
                    }
                };
            let mut stream = client.subscribe_tailscale_status(ReconnectConfig::default());
            // 世代兜底轮询间隔：`ReconnectingStream` 永不自结束（断开即重连），故必须用 `timeout` 包住取帧
            // 给世代守卫一个「无帧也能醒」的兜底——否则核停了但一直没帧时 relay 会泄漏、对死端口无限重连。
            let tick = Duration::from_millis(CRASH_MONITOR_POLL_MS);
            log::info!("TS STATUS relay 起（世代 {my_gen}，apiPort={api_port}）");
            // 本 relay 自留的「上一帧各端点 backendState」——只为**跃迁才打日志**（稳态每秒一帧全打
            // 就是刷屏），以及判「是不是全就绪」以决定要不要重订阅。不作真值源（真值在 mesh 缓存）。
            let mut last_states: BTreeMap<String, String> = BTreeMap::new();
            let mut frames: u64 = 0;
            let mut resubscribes: u32 = 0;
            let mut idle_ms: u64 = 0;
            let mut idle_threshold_ms: u64 = RESUBSCRIBE_IDLE_MS;
            loop {
                // 世代守卫：核被停/接管（stop/restart 先 bump 世代）→ 退场，drop stream 停订阅+重连
                // （防对死端口无限重连、防旧核 relay 污染新核）。取帧前后各查一次。
                if me.gate.generation() != my_gen {
                    log::info!(
                        "TS STATUS relay 退场（世代 {my_gen}→{}）：本代共收 {frames} 帧、重订阅 {resubscribes} 次，末态 {last_states:?}",
                        me.gate.generation()
                    );
                    return;
                }
                match tokio::time::timeout(tick, stream.recv()).await {
                    Ok(Some(update)) => {
                        // 收帧后复查世代：接管方可能刚拆核，别把旧核末帧写进新核缓存。
                        if me.gate.generation() != my_gen {
                            return;
                        }
                        idle_ms = 0;
                        frames += 1;
                        log_ts_state_transitions(&update, &tag_to_id, &mut last_states);
                        me.apply_ts_status_frame(&update, &tag_to_id, my_gen);
                        // A4 触发点①：每帧后对账登录期出口让位（读该帧刚写入缓存的选中出口 backendState）。
                        // 收帧世代已复查（上方），reconcile 内部 hotSwitch 走 management_api（核未起即 not_ready→false，
                        // 不改 flag），世代进一步接管由下一轮循环顶守卫退场。
                        me.reconcile_login_fallback().await;
                    }
                    // ReconnectingStream 正常永不返 None（断开即重连）；真返 None = 内部终止 → 退场。
                    Ok(None) => return,
                    // tick 内无帧：稳态下正常（核按自身节奏推）。但**未就绪 + 长时间无帧**是本方法
                    // 文档记的那个真机故障形态 → 重订阅取当前真值（见方法文档）。
                    Err(_) => {
                        idle_ms = idle_ms.saturating_add(CRASH_MONITOR_POLL_MS);
                        if idle_ms >= idle_threshold_ms && !ts_all_running(&last_states) {
                            resubscribes += 1;
                            log::info!(
                                "TS STATUS 流已 {}s 无帧且末态非全就绪（{last_states:?}）→ 重订阅取当前真值（第 {resubscribes} 次）",
                                idle_ms / 1000
                            );
                            stream = client.subscribe_tailscale_status(ReconnectConfig::default());
                            idle_ms = 0;
                            idle_threshold_ms =
                                (idle_threshold_ms * 2).min(RESUBSCRIBE_IDLE_MAX_MS);
                        }
                    }
                }
            }
        });
    }

    /// 核就绪后挂**核日志 relay**：订阅管理 API 的 `SubscribeLog`，逐行喂进本仓日志 sink
    /// （`logging.rs` 的环形缓冲 + 落盘 + UI 直播流）。世代范式与 `spawn_tailscale_status_relay` 同款。
    ///
    /// # 它修掉的两件事
    ///
    /// ① **TUN/helper 腿 app 侧没有 child 管道**：helper 在自己的进程里排空核 stdout/stderr，app 无法
    ///    从那根 pipe 做实时分发。本 relay 不经 child stderr，三平台 helper 腿统一拿到结构化实时日志。
    /// ② **看 debug 不必再改核配置**：本流恒是全级别（喂它的 platform writer 分发不受 `log.level`
    ///    过滤，见 crate `polaris-singbox-grpc` 的 `subscribe_logs` 文档），级别筛在客户端 ——
    ///    判据是 `log::max_level()`，由 `logging::set_level` 跟着 `config.logLevel` 即时改。
    ///    把级别拨到 debug **立刻**就能看到核的 debug 行，无需落盘、无需重启核。
    ///    该判据在本方法里由 [`core_log_admits`] **提前**取一次（下游 `log::log!` 仍会按同一个值再筛
    ///    一遍，故去留不变）—— 提前只为省下注定被丢的行的剥除代价，理由见该函数文档。
    ///    旧的 `diagnosticCapture`（快照原级别 → 改配置到 debug → 落盘 → 广播 → 重启核 → 事后还原，
    ///    外加崩溃自愈）整条链的存在理由就是这一条，故随本批一并删除。
    ///
    /// # 与 stderr 转发腿的交接（`pipe_handoff`）
    ///
    /// `Some(flag)` = 本腿是直起（有 stderr 管道）：
    ///
    /// - 收到首帧即置位 flag，`pipe_to_log` 随即停止转发 —— 否则同一行进两遍环形缓冲；
    /// - 首帧那份历史**丢弃**：它覆盖的正是管道已经转发过的那一段，收下就是整屏重放。
    ///
    /// `None` = 本腿经 helper 起（无管道）：首帧历史**收下**，那是起核到订阅之间唯一的日志来源。
    ///
    /// 残留窗口如实记账：从服务端 Subscribe 到本侧收到首帧之间（loopback 上一个往返）产生的行，
    /// 既在增量帧里、也仍被管道转发一次 —— 会重一两行，不做进一步收敛（消除它要引入序号对账，
    /// 代价远大于收益）。
    ///
    /// # 重连后的历史同样丢弃
    ///
    /// `ReconnectingStream` 断线重连必然再收一帧 `reset=true` + 全量历史（服务端语义）。整份收下 =
    /// 最多 3000 行重放上屏；故一律跳过，代价是断连窗口内的行看不到。**这是有意的取舍**，并在
    /// debug 日志里点名说出跳过了多少行，不静默。
    ///
    /// # `disableLogFile` 时压根不订阅
    ///
    /// 该开关落到核就是 `log.disabled=true` → `log.New` 直接返回 `NewNOPFactory()`
    /// （`log/log.go`），其 `AttachPlatformWriter` 是空实现 ⇒ 本流永久空。此时订阅只是白建连接，
    /// 更糟的是「订阅着却一行没有」与「核真的一句话没说」在外部无从区分。故直接不订阅，并把原因
    /// **写进日志**（那行本身会进日志页，用户一眼看见为什么这里是空的）——这就是原先挂在
    /// 「开始诊断采集」按钮上那道护栏的去处：它守的事实没变，只是搬到了机制真正所在的地方。
    fn spawn_core_log_relay(
        self: &Arc<Self>,
        my_gen: u64,
        api_port: u16,
        pipe_handoff: Option<CoreLogHandoff>,
    ) {
        if self.log_file_disabled() {
            log::warn!(
                "「关闭日志写盘」已开启 ⇒ sing-box 侧日志被整体禁用（log.disabled），\
                 本次运行不会有任何内核日志（实时日志与基于日志的诊断均不可用）。\
                 要排查内核问题请先在「设置 · 高级」里关掉该开关"
            );
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let secret = me.clash_api_secret();
            let client =
                match SingBoxApiClient::connect(Endpoint::new("127.0.0.1", api_port), secret).await
                {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("核日志 relay 连接管理 API 失败（apiPort={api_port}）: {e}");
                        return;
                    }
                };
            let mut stream = client.subscribe_logs(ReconnectConfig::default());
            // 世代兜底轮询间隔：`ReconnectingStream` 永不自结束（断开即重连），核安静时也得有机会
            // 醒来查世代 —— 否则核停了但一直没帧时 relay 会泄漏、对死端口无限重连（同 TS STATUS relay）。
            let tick = Duration::from_millis(CRASH_MONITOR_POLL_MS);
            // 首帧那份历史收不收：无管道（helper 腿）才收，见方法文档。
            let mut history_pending = pipe_handoff.is_none();
            let mut forwarded: u64 = 0;
            // 被本侧筛掉的行数。计它不是为了好看：**「全级别流的常态开销」在此之前无从观测** ——
            // 核恒推 trace 在内的每一帧，用户却常年停在 info，两者之比只能靠猜。退场时把它说出来，
            // 于是真机上「这条流到底白搬了多少」是一个可读的数，而不是一个待办事项。
            let mut filtered: u64 = 0;
            log::info!("核日志 relay 起（世代 {my_gen}，apiPort={api_port}）");
            loop {
                if me.gate.generation() != my_gen {
                    log::info!(
                        "核日志 relay 退场（世代 {my_gen}→{}）：本代共转发 {forwarded} 行、筛掉 {filtered} 行",
                        me.gate.generation()
                    );
                    // 交接闸复位：本腿的管道任务可能还活着（子进程尚未收尸），别让它一直哑着。
                    if let Some(h) = &pipe_handoff {
                        h.store(false, Ordering::SeqCst);
                    }
                    return;
                }
                match tokio::time::timeout(tick, stream.recv()).await {
                    Ok(Some(frame)) => {
                        // 收帧后复查世代：接管方可能刚拆核，别把旧核的行写进新核的会话。
                        if me.gate.generation() != my_gen {
                            continue; // 交给循环顶的守卫统一退场（含闸门复位）
                        }
                        // 流已活 → stderr 转发腿让位（它继续跑 FATAL 分类，只是不再转发）。
                        if let Some(h) = &pipe_handoff {
                            h.store(true, Ordering::SeqCst);
                        }
                        if frame.reset {
                            if !history_pending {
                                if !frame.messages.is_empty() {
                                    log::debug!(
                                        "核日志流（重）订阅：跳过 {} 行历史（已在缓冲里，重收 = 整屏重放）",
                                        frame.messages.len()
                                    );
                                }
                                continue;
                            }
                            history_pending = false;
                        }
                        // 两道闸都**逐帧现读**（隐私模式与日志级别都能在运行期变；起流时定死分别就是
                        // 「开了锁还在漏」和「拨到 debug 却还是看不到」）。
                        let floor = core_log_privacy_floor(me.privacy_mode_active());
                        let max = log::max_level();
                        for m in &frame.messages {
                            let level = core_log_level(m.level);
                            if !core_log_admits(level, floor, max) {
                                filtered += 1;
                                continue;
                            }
                            let text = strip_core_log_decoration(&m.message);
                            if text.is_empty() {
                                continue;
                            }
                            log::log!(target: crate::logging::SING_BOX_TARGET, level, "{text}");
                            forwarded += 1;
                        }
                    }
                    // ReconnectingStream 正常永不返 None（断开即重连）；真返 None = 内部终止 → 退场。
                    Ok(None) => {
                        if let Some(h) = &pipe_handoff {
                            h.store(false, Ordering::SeqCst);
                        }
                        return;
                    }
                    // tick 内无帧：核安静时的常态。只为让世代守卫有机会跑。
                    Err(_) => {}
                }
            }
        });
    }

    // ════════════════ C3：自动换节点（节点不可达 → 热切/重启到最优候选）════════════════
    //
    // 决策全在 [`AutoSwitchMachine`] + 纯选择函数（`runtime/auto_switch.rs`，真值表 + 变异锁死）；
    // 本层只做「心跳探测 → 喂决策机 → 复用 switch_mode 切换 → emit」的 I/O。**与崩溃恢复解耦**：
    // 进程崩溃由 `spawn_crash_monitor` 原地重启同节点兜底，本腿只对「核活着但代理链不通」换节点
    // （1:1 移植 上游 AutoSwitchService 的职责边界）。
    //
    // **DESIGN-REVIEW(auto-node-switch-probe-source)**：上游的失败探测是「经代理 HTTP generate_204」
    // 应用层连通性检测，本仓无现成等价信号（崩溃监测=进程死、就绪门=起核期 TCP、测速=按需）。本实现忠实
    // 移植 上游的 through-proxy 探测（[`probe_through_proxy`]）+ 候选 TCP 延迟（[`measure_latency`]）——
    // 二者**真碰宿主网络 = 真机门，本机不单测**（决策层已纯逻辑覆盖）。是否改「复用现有测速引擎」而非
    // 本 through-proxy 探测，留复审裁（reuse vs 忠实移植的权衡）。见 review-queue。

    /// **C3**：核就绪后挂自动换节点心跳（`spawn_tailscale_status_relay` 的世代范式）。
    ///
    /// **无条件挂**（与崩溃监测同接线点），开关在循环内每 tick 读原始配置 `autoSwitchNode` 动态判
    /// （对齐 上游 config-change-handler 的运行期 enable/disable，轮询版——避免动命令层加事件驱动，
    /// 本批禁区 commands/config.rs）。**世代守卫**：核被停/接管（stop/restart 先 bump 世代）→ 退场，
    /// 绝不让旧核的心跳污染新核（探测/切换均先复查世代）。
    fn spawn_auto_switch_heartbeat(self: &Arc<Self>, my_gen: u64, mixed_port: u16) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let mut machine = AutoSwitchMachine::new();
            let tick = Duration::from_millis(HEARTBEAT_INTERVAL_MS);
            log::debug!("自动换节点心跳起（世代 {my_gen}，mixedPort={mixed_port}）");
            loop {
                tokio::time::sleep(tick).await;
                // 世代守卫：核被停/接管 → 退场。
                if me.gate.generation() != my_gen {
                    return;
                }
                // 动态开关（上游 config-change-handler，轮询版）：autoSwitchNode 真才启用。
                let want_enabled = me.auto_switch_enabled();
                if want_enabled && !machine.is_enabled() {
                    machine.enable();
                    log::info!("自动换节点已启用（应用层连通性检测）");
                } else if !want_enabled && machine.is_enabled() {
                    machine.disable();
                    log::info!("自动换节点已禁用");
                }
                if !machine.is_enabled() {
                    continue;
                }
                // 换节点在飞中 → 跳过本次心跳（上游 runHeartbeat isSwitching 守卫）。
                if machine.is_switching() {
                    continue;
                }
                // 核未运行 → 只复位失败计数（不动熔断），继续（等退场或恢复）。
                if !me.core_running() {
                    machine.reset_failures_only();
                    continue;
                }
                // 守卫（上游 AutoSwitchService.runHeartbeat:113-116）：选中节点须真实存在于 servers。
                // direct 模式（`__direct__` 不在 servers）/ 选中被删 / 无选中 → 跳过本 tick（不探测/不计失败/
                // 不切走）。否则 direct 下网络抖动会被当成「当前节点不通」→ 自动切到某代理节点（用户明明选的是
                // 直连），把「换节点」误用到一个根本不是节点的选择上。
                if !me.selected_server_is_real() {
                    continue;
                }
                // 应用层连通性探测（真机门：真起核 + 碰网络）。
                let alive = probe_proxy_connectivity(mixed_port).await;
                // 探测耗时窗口内可能已被接管 → 复查世代。
                if me.gate.generation() != my_gen {
                    return;
                }
                match machine.on_heartbeat(alive) {
                    HeartbeatOutcome::Trigger => {
                        log::warn!(
                            "连通性连续 {} 次失败 → 触发自动换节点",
                            crate::runtime::auto_switch::MAX_CONSECUTIVE_FAILURES
                        );
                        me.run_auto_switch(&mut machine, "连通性检测").await;
                    }
                    HeartbeatOutcome::Recovered { prior } => {
                        log::info!("连通性恢复正常（此前连续失败 {prior} 次）");
                    }
                    HeartbeatOutcome::Failing { failures } => {
                        log::warn!(
                            "连通性检测失败 [{failures}/{}]",
                            crate::runtime::auto_switch::MAX_CONSECUTIVE_FAILURES
                        );
                    }
                    HeartbeatOutcome::Stable => {}
                }
            }
        });
    }

    /// 原始配置 `autoSwitchNode === true`（上游 index.ts:1846 门控）。**从原始 JSON 读**——该字段不在
    /// `UserConfig` 结构体（同 `restartOnNodeChange` / `meshLoginFallbackDirect`，见 [`switch_mode`] 注）。
    ///
    /// 走 [`ConfigManager::with_current`] 而非 `current()`：本方法由自动换节点心跳**每 tick 无条件**
    /// 调用（`HEARTBEAT_INTERVAL_MS`，核在跑就一直跑），而它只要一个 bool —— 为此深拷贝整份配置
    /// （含 200 节点级 `servers`）纯属常驻浪费。闭包内只取字段，不回调任何子系统。
    ///
    /// [`switch_mode`]: Self::switch_mode
    fn auto_switch_enabled(&self) -> bool {
        self.config
            .with_current(|c| c.get("autoSwitchNode").and_then(Value::as_bool))
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    /// **unlock 缓存失效（核 start/stop）**：核起停 = 出口隧道换一次 → 解锁快照必须作废（否则 30min TTL
    /// 内复用停核前的陈旧解锁角标）。经 [`ProxyErrorEmitter::invalidate_unlock`] 收口（bump epoch、清缓存、
    /// 广播三合一）。emitter 未接线（单测 / setup 前极早期失败）→ 静默跳过，对齐既有 emit 腿——发不出失效
    /// 事件绝不反过来打断起停本身。对齐 上游 `ProxyManager` start/stop → `unlockService.invalidate()`。
    fn invalidate_unlock_cache(&self, running: bool, exit_blocked: bool) {
        if let Some(emitter) = self.error_emitter.get() {
            emitter.invalidate_unlock(running, exit_blocked);
        }
    }

    /// **出口 IP / 延迟自动重探（核 start/stop/热切）**：出口换了一次 ⇒ 状态栏出口 IP 与其下游的伴测
    /// 延迟都须重探。经 [`ProxyErrorEmitter::schedule_exit_ip_refresh`] 收口（排程 + 检测中占位 + 探测
    /// 广播三合一）。emitter 未接线（单测 / setup 前极早期）→ 静默跳过，绝不打断起停本身（同
    /// [`invalidate_unlock_cache`] 范式）。
    ///
    /// 对齐 上游 `IpInfoService` 的事件驱动触发表；**不引入周期轮询**（上游 也没有）。
    ///
    /// [`invalidate_unlock_cache`]: Self::invalidate_unlock_cache
    fn schedule_exit_ip_refresh(&self, running: bool) {
        if let Some(emitter) = self.error_emitter.get() {
            emitter.schedule_exit_ip_refresh(running);
        }
    }

    /// **R2 出口无效终态**：经 [`ProxyErrorEmitter::mark_exit_blocked`] 把出口 IP 快照落成「出口无效」
    /// （无探测、即时）。emitter 未接线 → 静默跳过（同 [`invalidate_unlock_cache`] 范式）。
    ///
    /// [`invalidate_unlock_cache`]: Self::invalidate_unlock_cache
    fn mark_exit_blocked(&self, reason: &str) {
        if let Some(emitter) = self.error_emitter.get() {
            emitter.mark_exit_blocked(reason);
        }
    }

    /// **B1 隐私模式活态**（`generate_deps` 用）：经 [`ProxyErrorEmitter::privacy_mode`] 读单一真值。
    /// emitter 未接线（单测 / setup 前极早期）→ `false` = 与接线前逐字节同的保守值（见 trait 方法文档）。
    fn privacy_mode_active(&self) -> bool {
        self.error_emitter.get().is_some_and(|e| e.privacy_mode())
    }

    /// **R2 待应用差集 PUSH**：把 [`pending_changes`](Self::pending_changes) 原样推给 UI
    /// （`event:proxyPendingChanges`）。
    ///
    /// **无适配层**（SoT §2.3.2 / T2-7）：pull 与 push 返回同一个 [`PendingChangesSummary`]，
    /// 「两路同构」是类型级事实而非靠测试维持。收口前这里曾丢弃 `updated`/`deleted` 并把 `modified`
    /// 硬编码成空 —— 那正是「测速说这个节点已编辑未生效，而 pending-bar 上根本没有它」的成因。
    ///
    /// # 接线不变式：差集有**两侧**，两侧都得推
    ///
    /// `pending_changes()` = f(分子: `config.current()`，分母: `startup_snapshot` + `switch_snapshot`
    /// + `restart_deferred`)。**任一侧被改写都改变差集**，故 PUSH 必须挂在两侧各自的写入点上：
    ///
    /// - **分子**（配置变了）→ [`switch_mode_with`](Self::switch_mode_with) 尾。
    /// - **分母**（运行核换了）→ [`start_inner`](Self::start_inner) 就绪腿与
    ///   [`stop_inner`](Self::stop_inner) 拆除腿，即写/清 `startup_snapshot` 的那两处。
    ///
    /// 只挂分子那一侧曾是本缺陷的根因：后端自驱的重启（去抖 / 「立即应用」/ drain / 崩溃自愈）
    /// 落地后差集其实已清，但没人说 —— 而前端的 pull 兜底挂在 `event:proxyStarted`/`Stopped`，
    /// 那两个事件**只由命令层**发（`commands/proxy.rs`），内部驱动的重启一个都不发。
    /// 由 `pending_changes_push_is_wired_on_both_sides_of_the_diff` 钉住。
    ///
    /// emitter 未接线（单测 / setup 前极早期）→ 静默跳过，绝不打断调用腿（同 [`invalidate_unlock_cache`] 范式）。
    ///
    /// [`invalidate_unlock_cache`]: Self::invalidate_unlock_cache
    fn push_pending_changes(&self) {
        if let Some(emitter) = self.error_emitter.get() {
            emitter.emit_pending_changes(&self.pending_changes());
        }
    }

    /// **runtime 生命周期结局 PUSH**（`event:proxyLifecycle`）。
    ///
    /// # 与 [`push_pending_changes`](Self::push_pending_changes) 的配对纪律
    ///
    /// `ready` / `stopped` 两个 phase **必须与 `push_pending_changes()` 严格同处、同条件**
    /// （紧邻的两行）：它们描述的是同一次跃迁的两个投影 —— 分开放就会出现「差集清了但态没翻」
    /// 或反过来。由 `lifecycle_push_is_paired_with_the_diff_push` 钉住相邻性。
    ///
    /// `failed` **刻意不在这一对里**，因为它**不改变差集的分母**：重启的停核腿早已把
    /// `startup_snapshot` 清空并推过一次空差集，起核失败只是「它没回来」这一条追加信息。
    /// 故它挂在 [`start`](Self::start) 包装的 `Err` 腿 —— 那是全部起核入口（IPC / 托盘 /
    /// 启动自动连接 / `restart` 的 start 腿）的**唯一**汇流点，同
    /// `maybe_clear_system_proxy_on_start_failure` 挂在那里的理由（挂命令层会漏掉 restart 腿）。
    ///
    /// emitter 未接线（单测 / setup 前极早期）→ 静默跳过，绝不打断调用腿（同 `push_pending_changes`）。
    fn push_lifecycle(&self, event: &ProxyLifecycleEvent) {
        if let Some(emitter) = self.error_emitter.get() {
            emitter.emit_lifecycle(event);
        }
    }

    /// **自动换节点心跳守卫**（上游 `AutoSwitchService.runHeartbeat`:113-116）：当前选中节点是否真实
    /// 存在于 `servers`。委托纯谓词 [`selected_server_present`]（无选中 / direct 哨兵 `__direct__` 不在
    /// servers / 选中被删 → false）。读配置失败 → false（保守跳过心跳，绝不误切）。
    ///
    /// 与 [`auto_switch_enabled`](Self::auto_switch_enabled) 同属心跳**每 tick 的无条件调用**，故同样走
    /// [`ConfigManager::with_current`]：谓词本体只需 `&Value`，不需要 owned 快照。
    fn selected_server_is_real(&self) -> bool {
        self.config
            .with_current(selected_server_present)
            .unwrap_or(false)
    }

    /// **C3 换节点执行体**（上游 `triggerSwitch` 的 I/O 侧，:150-260）。闸门（熔断/冷却/在飞）决策全在
    /// [`AutoSwitchMachine::evaluate_switch`]；放行后测候选延迟 → [`select_best_candidate`] → [`plan_switch`]
    /// → 复用 [`switch_mode`](Self::switch_mode) 切换 → emit。**真机门**（真起核 + 碰网络）。
    async fn run_auto_switch(self: &Arc<Self>, machine: &mut AutoSwitchMachine, reason: &str) {
        match machine.evaluate_switch(now_ms()) {
            SwitchGate::Proceed => {}
            SwitchGate::InFlight => return,
            SwitchGate::Breaker { remaining_ms } => {
                log::warn!(
                    "自动切换已熔断（连续切换未恢复连通），{}s 内暂停切换，请检查网络/订阅",
                    remaining_ms.div_ceil(1000)
                );
                return;
            }
            SwitchGate::Cooldown { remaining_ms } => {
                log::info!(
                    "自动换节点冷却中，{}s 后可再次触发",
                    remaining_ms.div_ceil(1000)
                );
                return;
            }
        }
        // 放行 → 进入在飞态（提前置 lastSwitchTime → 失败/无候选也进冷却，防空转，上游 :180-181）。
        machine.begin_switch(now_ms());
        let switched = self.do_switch_io(reason).await;
        // 真发生了切换 → 记账熔断窗口（上游 :233-236）；候选空/全不可达的早退不记（对齐 上游 两个 return）。
        if switched {
            machine.record_switch_success(now_ms());
        }
        // finally：退出在飞态（上游 :257-259）。
        machine.end_switch();
    }

    /// 换节点的纯 I/O 段：载配置 → 测候选延迟 → 选最优 → 落盘 + `switch_mode` + emit。
    /// 返回 `true` = 真切了一次（用于熔断记账）；候选空 / 全不可达 / 载配置失败 → `false`（上游 :189-216）。
    async fn do_switch_io(self: &Arc<Self>, reason: &str) -> bool {
        let config = match self.config.current() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("自动换节点：读配置失败 → 跳过：{e}");
                return false;
            }
        };
        let current_id = config
            .get("selectedServerId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let candidates = extract_candidates(&config, current_id.as_deref());
        if candidates.is_empty() {
            log::warn!("没有其他可用节点，无法自动切换");
            return false;
        }
        log::info!("[{reason}] 开始对 {} 个候选节点测速...", candidates.len());

        // 并行测候选 TCP 延迟（上游 Promise.all，:201-206）。真机门：碰宿主网络。
        let mut set = tokio::task::JoinSet::new();
        for c in candidates {
            set.spawn(async move {
                let latency = measure_latency(&c.address, c.port).await;
                CandidateLatency {
                    id: c.id,
                    name: c.name,
                    latency_ms: latency,
                }
            });
        }
        let mut measured: Vec<CandidateLatency> = Vec::new();
        while let Some(res) = set.join_next().await {
            if let Ok(cl) = res {
                measured.push(cl);
            }
        }

        let Some(best) = select_best_candidate(&measured) else {
            log::warn!("所有候选节点均不可达，无法自动切换");
            return false;
        };
        let best_latency = best.latency_ms.unwrap_or(0);
        log::info!("选中最优节点: {} ({best_latency}ms)", best.name);

        let Some(plan) = plan_switch(&config, best, reason) else {
            log::warn!("自动换节点：生成新配置失败 → 跳过");
            return false;
        };

        // 先落盘新选中节点（上游 :227 saveConfig：保证 UI/重启都用新节点）。失败 → 不切、不记账。
        if let Err(e) = self.config.save_full(&plan.new_config) {
            log::warn!("自动换节点：保存新配置失败 → 跳过：{e}");
            return false;
        }
        // 复用现有 switch 逻辑（热切优先，失败/不适用自动退回去抖重启，上游 :230 switchMode）。
        let outcome = self.switch_mode(plan.new_config).await;
        log::info!(
            "✅ 自动换节点成功: {}（{outcome:?}）",
            plan.payload.new_server_name
        );

        // emit（未接线 emitter：单测 / setup 前 → 静默跳过，对齐既有 emit 腿）。
        if let Some(emitter) = self.error_emitter.get() {
            emitter.emit_auto_node_switched(&plan.payload);
        }
        true
    }

    // ════════════════ A4：组网登录期出口让位（零重启热切 selector 编排）════════════════
    //
    // 机制（1:1 移植 上游 `reconcileLoginFallback`/`loginFallbackEligible`/`markLoginFallbackEngaged`）：
    // 选中出口=正登录的账号制 TS 全隧道节点时，其隧道 Running 前把默认路由（proxy-selector）临时**热切** direct
    // （`select_outbound`，**零重启**、`direct` 恒是 proxy-selector 成员），Running 后切回。**不**重生成 config、
    // **不**重启核（重启=断流，正是此腿规避的）。见复审队列 R26。

    /// A4 让位态读侧：当前是否处于登录期出口让位态（推送经 `EVENT_MESH_LOGIN_FALLBACK`）。
    #[must_use]
    pub fn login_fallback_engaged(&self) -> bool {
        self.login_fallback
            .lock()
            .map(|g| g.engaged)
            .unwrap_or(false)
    }

    /// A4 早退闸的**廉价一半**：选中出口在**原始配置 JSON** 上是否为 Tailscale 协议。
    ///
    /// # 为什么读 raw `Value` 而不是 `UserConfig`
    ///
    /// 这个判据存在的唯一理由，是省掉 [`reconcile_login_fallback`](Self::reconcile_login_fallback)
    /// 每帧那两份整配置分配（`current_config` 的深拷贝 + 反序列化出的 `UserConfig`，两者都含全部
    /// 节点）。为判它再反序列化一次就等于白做。本实现只在读锁内对**借来的** `Value` 做两次 `&str`
    /// 比较：**零堆分配**，代价 = 一次 `RwLock` 读 + 对 `servers` 数组的一次线性扫描。
    ///
    /// # 与 [`login_fallback_eligible`](Self::login_fallback_eligible) 的等价性
    ///
    /// 那条判据经 `UserConfig` 走 `selected_server_id` → `servers.iter().find(id)` →
    /// `protocol == Protocol::Tailscale`。三处键名与取法在此**逐字对齐**：`selectedServerId`
    /// （`UserConfig` 的 `#[serde(rename)]`）、`servers[].id`、`servers[].protocol`。
    /// 同样用 `find` 而非 `any`：id 重复时两条路必须取到同一个元素。
    ///
    /// 🔴 **等价性依赖一条本函数管不到的外部性质**：`Protocol` 的反序列化严格小写、无别名
    /// （`#[serde(rename_all = "lowercase")]` 且未手写宽容 `Deserialize`）⇒ 线上字面量恒为
    /// `"tailscale"`。它并非天然如此 —— 同一个文件里的 `SecurityMode` 就是大小写不敏感解析的活先例。
    /// 若有人照抄着给 `Protocol` 加宽容解析，`"Tailscale"` 会让完整判据说「符合」、本判据说「不符合」
    /// ⇒ **engage 帧被早退闸吃掉**，未登录的 TS 出口永不让位，而本模块的等价性测试（只喂现存形态）
    /// 不会转红。绊线落在定义侧：`config-engine` 的 `protocol_deserialization_is_case_strict`。
    /// **要给 `Protocol` 加宽容解析，先来改这里**（改成走 `Protocol::deserialize` 或对齐新口径）。
    ///
    /// 任何让 `UserConfig::deserialize` 失败的形态（缺 `protocol`、大小写不符、类型不对）在本判据
    /// 下同样落 `false`，而 `reconcile_login_fallback` 遇反序列化失败本就 `return` ⇒ 两条路的可观测
    /// 结果一致（都无效果）。空 `selectedServerId` 亦然（对账里 `sel_id` 同样按非空过滤）。
    fn selected_exit_is_tailscale(&self) -> bool {
        let Ok(guard) = self.current_config.read() else {
            return false;
        };
        let Some(raw) = guard.as_ref() else {
            return false;
        };
        let Some(selected) = raw
            .get("selectedServerId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return false;
        };
        raw.get("servers")
            .and_then(Value::as_array)
            .and_then(|servers| {
                servers
                    .iter()
                    .find(|s| s.get("id").and_then(Value::as_str) == Some(selected))
            })
            .and_then(|s| s.get("protocol").and_then(Value::as_str))
            == Some("tailscale")
    }

    /// 内存态写：置/清 `(engaged, server_id)`。
    fn set_login_fallback(&self, engaged: bool, server_id: Option<String>) {
        if let Ok(mut g) = self.login_fallback.lock() {
            g.engaged = engaged;
            g.server_id = server_id;
        }
    }

    /// A4：选中出口在【配置层】是否符合让位形态（账号制 TS 全隧道出口 + 开关开 + 非 direct 模式 + 无 authKey）。
    ///
    /// 就绪与否的【动态】判断不在此（`tunnel_ready` 恒传 false，只为「配置符合」时返 true）；由 reconcile 按
    /// backendState 决策。`raw` 供读 `meshLoginFallbackDirect`（**非 UserConfig 结构体字段**，同 `restartOnNodeChange`
    /// 只在原始 JSON 里，见 switch_mode 注）。上游 `loginFallbackEligible`。
    fn login_fallback_eligible(&self, config: &UserConfig, raw: &Value) -> bool {
        let selected = config
            .selected_server_id
            .as_deref()
            .and_then(|id| config.servers.iter().find(|s| s.id == id));
        let input = MeshLoginFallbackInput {
            // `meshLoginFallbackDirect !== false`（缺键/true → 开；显式 false → 关）。
            fallback_enabled: raw.get("meshLoginFallbackDirect").and_then(Value::as_bool)
                != Some(false),
            proxy_mode_direct: config.proxy_mode == ProxyMode::Direct,
            selected_exit_falls_back_direct: mesh_selected_exit_falls_back_to_direct(config),
            selected_is_tailscale: selected
                .map(|s| s.protocol == Protocol::Tailscale)
                .unwrap_or(false),
            selected_has_auth_key: selected
                .and_then(|s| s.tailscale_settings.as_ref())
                .and_then(|t| t.auth_key.as_deref())
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false),
            selected_tunnel_ready: false,
        };
        mesh_login_fallback_should_engage(&input)
    }

    /// A4：热切 selector（`select_outbound` PUT，零重启）。核未起/未就绪 → `management_api` 返 not_ready →
    /// `select_outbound` Err → false（不改 flag，下次 tick 重试）。上游 `hotSwitchSelector`。
    async fn hot_switch_selector(&self, selector_tag: &str, member_tag: &str) -> bool {
        if member_tag.is_empty() {
            return false;
        }
        match self.put_outbound(selector_tag, member_tag).await {
            Ok(()) => {
                log::info!("已热切换 {selector_tag} → {member_tag}（管理 API，无重启）");
                true
            }
            Err(e) => {
                log::warn!("管理 API 热切换 {selector_tag} 失败：{e}");
                false
            }
        }
    }

    /// PUT 落点：生产 = 真管理 API gRPC `SelectOutbound`；单测经 `management_api_stub` 注入
    /// （同 [`core_binary_for_start`](Self::core_binary_for_start) 的先例，见该字段文档）。
    ///
    /// 只替换**最末端的那一次调用**，成败映射 / 日志 / 上层决策全部走生产同一条码路。
    async fn put_outbound(&self, selector_tag: &str, member_tag: &str) -> Result<(), String> {
        #[cfg(test)]
        if let Some(sink) = self
            .management_api_stub
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(Arc::clone))
        {
            return sink.put(selector_tag, member_tag);
        }
        self.management_api()
            .await
            .select_outbound(selector_tag, member_tag)
            .await
            .map_err(|e| e.to_string())
    }

    /// **§15**：主核测速探测池目标快照（`server_speed_test` 消费）——运行核的池端口 + id→tag 映射。
    ///
    /// 返回 `Some` ⟺ 核运行且起核时成功注入了探测池（`probe_pool_ports` 非空）；否则 `None`（测速回退活跃出口）。
    /// 与 `switch_snapshot` 同源（起核就绪置、停核清）→ 池端口与运行核 config 逐槽一致，`id_to_tag` 即
    /// `probe-selector-k` 的成员命名空间（`hasTag(id)` = `id ∈ id_to_tag` = 已入运行核池；新增未重启的节点不在其中）。
    pub fn speed_probe_targets(&self) -> Option<SpeedProbeTargets> {
        if !self.status().running {
            return None;
        }
        let snap = self.switch_snapshot.read().ok()?.clone()?;
        if snap.probe_pool_ports.is_empty() {
            return None;
        }
        Some(SpeedProbeTargets {
            pool_ports: snap.probe_pool_ports,
            id_to_tag: snap.id_to_tag,
            // dirty 波前预筛的唯一诚实判据（见字段文档）：起核那刻的 **5 维**指纹表，与 id_to_tag 同源同刻。
            // **必须是 dirty_fingerprints 而非 fingerprints** —— 后者是全维表（喂重启判据 + pending
            // modified），与测速「新」一侧的 5 维公式不同 ⇒ 恒不等 ⇒ 全员恒 dirty、整个波前恒被免测。
            fingerprints: snap.dirty_fingerprints,
        })
    }

    /// **临时测速核**的构建环境快照（platform / arch / cronet 可用性）。
    ///
    /// 三项与 [`generate_deps`](Self::generate_deps) 喂给主核 config 生成的**同名字段逐字同源**（同一个
    /// `platform_tag()` / `std::env::consts::ARCH` / `cronet_available(...)`）。抽这个只读面而不是让
    /// `runtime::speedtest` 自己再算一遍：三项里任一算法漂了，表现都是**静默**的 —— 临时核按另一套
    /// 平台/架构判定构出的出站与主核不同（如 macOS 的 cronet 静态编入判定漂了 ⇒ naive 节点被临时核
    /// 无谓剔掉、或反过来进核 FATAL 拖垮整批），而两边都「能跑」。
    #[must_use]
    pub fn core_build_env(&self) -> CoreBuildEnv {
        CoreBuildEnv {
            platform: platform_tag().to_string(),
            arch: std::env::consts::ARCH.to_string(),
            has_cronet: cronet_available(
                self.cronet_lib_exists_for_start(),
                platform_tag(),
                std::env::consts::ARCH,
            ),
        }
    }

    /// **§15.11**：当前生命周期世代（测速分波编排的**让位判据**之一）。
    ///
    /// = 上游 `SpeedTestService.getCoreGeneration()`。`start`/`stop`/`restart`/`regen` 均先
    /// [`bump_generation`](Self::bump_generation) 再动核 ⇒ 「核被换掉/停掉」⟺「世代已变」。
    ///
    /// 测速侧据此判「本轮测的还是不是当初那个核」：世代变了则在飞结果量的是**别的核**，必须丢弃而非记账。
    /// 单独用它**不够** —— 自发崩溃不 bump 世代（见 [`status`](Self::status) 的 `running`），故让位判据是
    /// 「世代跃迁 **或** 核已不在运行」两条腿的**析取**，缺一条就会把崩溃窗口的在飞失败误记成真实超时。
    pub fn core_generation(&self) -> u64 {
        self.gate.generation()
    }

    /// **§15**：把第 `k` 槽 `probe-selector-k` 热切到被测节点出站 `member_tag`（gRPC `select_outbound`，live 生效）。
    ///
    /// = 上游 `MainCoreProbe.selectSlot`。复用 [`hot_switch_selector`](Self::hot_switch_selector)（同 PUT 原语，
    /// 核未就绪 → false）。`interrupt_exist_connections:true` 由 config-engine 挂在 selector 上 → 同槽跨波重指前断残留、防串味。
    pub async fn probe_select_slot(&self, k: usize, member_tag: &str) -> bool {
        self.hot_switch_selector(&format!("probe-selector-{k}"), member_tag)
            .await
    }

    /// A4：发射让位态变事件（emitter 未接线 = 单测/setup 前 → 静默跳过）。
    fn emit_mesh_login_fallback(&self, engaged: bool, server_name: Option<&str>) {
        if let Some(emitter) = self.error_emitter.get() {
            emitter.emit_mesh_login_fallback(engaged, server_name);
        }
    }

    /// A4：置让位 flag（PUT 成功后调，flag 与 selector 一致）。幂等，仅首次 emit engaged:true。
    /// 上游 `markLoginFallbackEngaged`。
    fn mark_login_fallback_engaged(&self, server_id: &str, config: &UserConfig) {
        let first = {
            let mut g = match self.login_fallback.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if g.engaged && g.server_id.as_deref() == Some(server_id) {
                return; // 幂等：同出口已让位，不重复 emit
            }
            let first = !g.engaged;
            g.engaged = true;
            g.server_id = Some(server_id.to_string());
            first
        };
        if first {
            let name = config
                .servers
                .iter()
                .find(|s| s.id == server_id)
                .map(|s| s.name.clone());
            log::info!(
                "组网出口「{}」尚未登录，登录期默认路由让位直连",
                name.as_deref().unwrap_or(server_id)
            );
            self.emit_mesh_login_fallback(true, name.as_deref());
        }
    }

    /// A4：复位让位内存态 + 撤销 UI 提示（若在让位中）。停核/崩溃调用；**不切 selector**（核已停/将停）。
    /// 上游 `resetLoginFallbackState`。
    fn reset_login_fallback_state(&self) {
        let was_engaged = {
            let mut g = match self.login_fallback.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if !g.engaged && g.server_id.is_none() {
                return;
            }
            g.engaged = false;
            g.server_id = None;
            true
        };
        if was_engaged {
            self.emit_mesh_login_fallback(false, None);
        }
    }

    // ══════════════ H3：起核后把 selector 选择校正回 config 意图 ══════════════
    //
    // 1:1 移植 上游 `reassertSelectorSelection` + `reassertRuleSelectors`（`ProxyManager.ts:1176-1237`）
    // 与其调用点的 `.finally()` 串接（:1144-1165）。
    //
    // **根因**：sing-box 1.14 的 `experimental.cache_file` 默认 `store_selected` —— 它把 selector 的
    // **运行期**选择持久化进 `cache.db` 的 `selected` bucket，起核时用它**覆盖**新生成 config 里的
    // `default`。于是「盘上选 Hk01、生成的 `proxy-selector.default = "Hk01"`」与「核实际跑上一轮残留的
    // `Tailscale`」可以同时成立，且**全链路零告警**（`attest_selected_exit` 是纯静态自证，量的是生成
    // 产物不是运行态，看不见这层覆盖）。
    //
    // **为什么不是关掉 `store_selected`**：那要往生成产物里下发 `cache_file.store_selected:false`，
    // 而 上游 不下发该键 ⇒ `golden_config_snapshot` 的 37 例逐字对拍立刻红。金样门是对的——修法必须是
    // 「起核后用管理 API 把 selector 拨回 config 意图」，让 config 成为单一真值、压过缓存。

    /// H3 selector 校正阶段 1 的最大轮数（上游 `for (let i = 0; i < 10; i++)`）。
    ///
    /// 重试的对象是「管理 API 刚起可能未就绪」：核进程已就绪 ≠ 它的 api service 已能接 gRPC。
    const REASSERT_MAX_ROUNDS: usize = 10;

    /// H3 selector 校正阶段 1 的轮间退避（上游 `await new Promise((r) => setTimeout(r, 300))`）。
    const REASSERT_RETRY_DELAY_MS: u64 = 300;

    /// **H3 修复的后台腿 + 续延**（= 上游 `void this.reassertSelectorSelection(config).finally(...)`）。
    ///
    /// # 为什么是 spawn 而不是 `.await` 在起核主链上
    ///
    /// 阶段 1 最坏 10 轮 × 300ms ≈ 3s（管理 API 迟迟不就绪时）。挂在 `start_inner` 主链上 = 每次起核
    /// 都可能凭空多等 3s，而校正**不是起核成功的前提**（校正失败时 cache/default 仍是一个有效节点，
    /// 只是可能不是用户选的那个）。best-effort 的东西不该卡住关键路径。
    ///
    /// # 为什么续延用 Drop 守卫而不是「跑完再调一行」
    ///
    /// 上游 那里是 `.finally()`：**reassert 抛异常也要跑续延**。Rust 里等价物就是 Drop 守卫——把续延
    /// 写在 `await` 之后，一旦 reassert 内部 panic，展开会跳过它，解锁缓存永远不失效（症状：boot 窗口
    /// 内那轮解锁检测的脏结果永久留在缓存里，且没有任何可见迹象）。
    ///
    /// # 世代守卫
    ///
    /// `my_gen` 是起核那一刻的世代快照。这条 `'static` 任务能活过停核/换核/新 start，而它的每个动作
    /// （PUT 到当前核、广播 `{running:true}`）都是**对着那个核**的 —— 世代变了必须整腿退场。
    fn spawn_reassert_selector_selection(
        self: &Arc<Self>,
        user_config: UserConfig,
        my_gen: u64,
        api_port: u16,
    ) {
        let me = Arc::clone(self);
        // 模式取**核实际启动的那份配置**（与 flush 自身守卫同源），在 move 前抄下。
        let mode = user_config.proxy_mode_type;
        // `tauri::async_runtime::spawn` 而非裸 `tokio::spawn`：同 `spawn_ts_exit_recovery` 的理由
        // （两者在 tauri 运行时下等价，但前者在无 tokio 上下文时不当场 panic）。
        tauri::async_runtime::spawn(async move {
            // 内层作用域：守卫在**这一行**（而不是整个 task 末尾）drop ⇒ 三条续延仍严格晚于每一次
            // PUT、且**不被自证的那次 gRPC 读回拖慢**。读回最坏要等满 `SNAPSHOT_TIMEOUT`（3s），而
            // 连接 flush / 解锁失效 / 出口 IP 重探一条都不该为一次只读观测多等 3s。
            let outcome = {
                let _settled = ReassertSettledGuard(Arc::clone(&me), my_gen, mode, api_port);
                me.reassert_selector_selection(&user_config, my_gen).await
            };
            // reassert 内部 panic 时展开会跳过这一行 —— 那是对的：守卫已把续延跑掉（`.finally()` 语义），
            // 而自证是**观测**，观测不到就该沉默，不该在展开路径上再造一条半截结论。
            me.attest_runtime_selector(&outcome, my_gen).await;
        });
    }

    /// H3 校正的**续延**：校正完成 / 放弃 / panic 后都必跑（见 [`ReassertSettledGuard`]）。
    ///
    /// **F-C 解锁污染根治**（上游 同名修复）：校正可能**真的翻转** selector（cache_file 复活的旧选择
    /// 被拨回 config 选中节点，含 rule-sel）。这次翻转不经 `switch_mode`，原本**不在解锁失效契约内**
    /// ⇒ boot 窗口内起跑的解锁检测轮经的是**旧出口**，其结果会被当成新鲜数据 commit 进缓存（epoch
    /// 守卫对它失明）。此处把校正补进契约：作废 boot 窗口那批在飞轮，让它们在校正后的出口上重跑。
    ///
    /// 校正是同值 no-op 时也会多失效一次 —— 与前端自身的去抖合并，无害（宁可多重跑一轮，不可留脏值）。
    ///
    /// **出口 IP 重探同理，也必须排在校正之后**（`exit_ip_wiring_guard` 的配对契约在此处成立）：它量的
    /// 就是「我现在从哪出去」。留在起核主链上则校正一旦真翻转 selector，那次探测拿到的是**旧出口**的
    /// 公网 IP，并被当成当前出口写进 ipinfo 缓存。上游 那边这条是靠 S1 `whenSelectorSettled` 让探测
    /// 自己等校正落定（Polaris 未港该门）；把排程本身挂到续延上是同一条保证的等价形态。
    ///
    /// **世代守卫是本移植的有意加强，不是 上游的逐字形态**：上游的 `finally` 里 `emit('unlock-invalidate')`
    /// 无守卫。但 Polaris 这两条都带 `running:true` 参数，而「校正在飞时核已被停/换」这个窗口是**把它们
    /// 从主链挪进异步续延才产生的**（原来就在主链上、紧跟 status 提交，不存在这个窗口）。不守卫等于亲手
    /// 造一个「核已停却广播 running:true / 对着死核排一次 4s 后的出口探测」的假信号。
    /// **连接 flush 也必须排在校正之后**（上游的「时序修 E」，逐字同源）：flush 干的是无差别
    /// `CloseAllConnections`，被 RST 的连接会**立刻重连**——重连走的是重连那一刻的 selector。
    /// flush 若早于校正落定，这批重连全部按 cache_file 的旧选择建链，本 bug 的症状在这个窄窗里
    /// 原样复现，而且是**我们亲手把用户所有连接踢过去的**，比自然漂移更糟。
    ///
    /// flush 自身的两条守卫（仅 TUN / 世代+核在跑）原样保留、不放宽：这里只改「什么时候开枪」，
    /// 不改「该不该开枪」。
    fn after_selector_reasserted(
        self: &Arc<Self>,
        my_gen: u64,
        mode: ProxyModeType,
        api_port: u16,
    ) {
        if self.gate.generation() != my_gen {
            log::debug!(
                "selector 校正续延：世代已变（{my_gen}→{}）→ 退场",
                self.gate.generation()
            );
            return;
        }
        self.invalidate_unlock_cache(true, false);
        self.schedule_exit_ip_refresh(true);
        self.schedule_connection_flush(mode, my_gen, api_port);
    }

    /// **H3 阶段 1**：把 `proxy-selector` 校正回用户意图（带短重试，等管理 API 就绪）。成功/放弃后跑阶段 2。
    ///
    /// 逐条时序都有来历，别自己发明顺序：
    /// - **每轮重读最新 `selectedServerId`**（而不是复用起核那刻的值）：起核窗口内用户完全可能已经热切到
    ///   别的节点，此时校正必须跟最新意图，绝不能把它 revert 回起核时那个（上游 同处注释明写）。
    /// - **tag 从起核那刻的 `switch_snapshot.id_to_tag` 解析**：PUT 的成员必须是**运行核里真实存在**的
    ///   tag，而运行核的 tag 集合定格在它启动的那份 config 上（`current_config` 可能已被并发推进）。
    /// - **解析不出 tag 不静默 break**（上游 bug#5）：选中节点不在运行核的 tag 映射里（config 被并发
    ///   推进）时，静默放弃会让 selector 无声地停在 cache_file 的旧选择上 —— 那正是本 bug 的症状放大器。
    ///   留 warn 日志，收敛交给后续的对账重启。
    /// - **停核 / 被接管中直接放弃**：别在杀核窗口里重连一个将死的管理 API（上游 `this.stopping` 守卫，
    ///   Polaris 侧等价物是「`running` 已假」或「世代已变」两条腿的析取）。
    ///
    /// # 返回值：为什么不再是 `()`
    ///
    /// 「PUT 成功」「解析不出 tag 就放弃」「跑满重试仍全失败」这三种终局，对**用户**的意义完全不同：
    /// 后两种就是 selector 原样停在 `cache_file` 旧选择上的那个状态 —— 即本 bug 的现场 —— 而它们此前
    /// 只落一行 `log::warn`，用户什么都看不到。把终局带回给调用方（[`Self::attest_runtime_selector`]），
    /// 才谈得上经 `set_nonfatal_error` 告知。
    async fn reassert_selector_selection(
        &self,
        config: &UserConfig,
        my_gen: u64,
    ) -> ReassertOutcome {
        // 循环跑满 ⟺ 每轮 PUT 都失败；`member_tag` 逐轮覆盖为**当轮**意图，故落到循环外时它是最后一轮
        // 的意图（重试腿每轮重读最新选中节点，最后一轮才是最新的那个）。初值只在
        // `REASSERT_MAX_ROUNDS == 0` 这个构型下逃逸，而该常量恒 10。
        let mut stage1 = Stage1Outcome::PutExhausted {
            member_tag: String::new(),
        };
        for _ in 0..Self::REASSERT_MAX_ROUNDS {
            if !self.status().running {
                // 核已停 → 放弃（cache/default 兜底）。**不报**：那个核已经不在了。
                return ReassertOutcome {
                    stage1: Stage1Outcome::Abandoned,
                    rule_intents: Vec::new(),
                };
            }
            if self.gate.generation() != my_gen {
                // 主动 stop/restart 接管中：勿在杀核窗口里 PUT，也别对着将死的核出自证结论。
                return ReassertOutcome {
                    stage1: Stage1Outcome::Abandoned,
                    rule_intents: Vec::new(),
                };
            }
            // 每轮现读 `current_config`（**最新意图**）；读不到/解析不出则退回起核那份。
            let raw = self.current_config.read().ok().and_then(|g| g.clone());
            let latest: Option<UserConfig> = raw
                .as_ref()
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            let cur = latest.as_ref().unwrap_or(config);
            let target_id = cur.selected_server_id.clone().filter(|s| !s.is_empty());
            let tag = target_id.as_deref().and_then(|id| {
                self.switch_snapshot
                    .read()
                    .ok()
                    .and_then(|g| g.as_ref().and_then(|s| s.id_to_tag.get(id).cloned()))
            });
            let Some(tag) = tag else {
                log::warn!(
                    "selector 校正放弃：选中节点 {} 不在运行核 tag 映射，待启动后对账收敛",
                    target_id.as_deref().unwrap_or("<未选中>")
                );
                stage1 = Stage1Outcome::UnresolvedTag {
                    selected_id: target_id.unwrap_or_else(|| "<未选中>".to_string()),
                };
                break;
            };
            // 登录期出口让位【预置】折入本阶段（上游 同款）：选中的是账号制 TS 全隧道出口且**未登录过**
            // （state 目录不存在）→ 本轮 PUT `direct` 而不是那个连不上的 TS tag，消除「核起→首帧」黑洞。
            // 判据用 fresh 值（eligible + !stateExists）而非读 flag；且**只在 PUT 成功后**才 markEngaged
            // —— flag 与 selector 必须同进退，否则会出现「flag 说已让位、selector 指着未登录的 TS 出口」。
            // `raw` 缺失时按 `Value::Null` 求值：`meshLoginFallbackDirect` 取不到键 ⇒ 缺省开，与 上游
            // 回退到 `config` 对象的语义一致。
            let null = Value::Null;
            let raw_ref = raw.as_ref().unwrap_or(&null);
            let want_direct = self.login_fallback_eligible(cur, raw_ref)
                && target_id.as_ref().is_some_and(|id| {
                    !self
                        .mesh
                        .tailscale_state_exists(std::slice::from_ref(id))
                        .get(id)
                        .copied()
                        .unwrap_or(false)
                });
            let member_tag = if want_direct {
                DIRECT_TAG
            } else {
                tag.as_str()
            };
            // 先记「本轮意图」，PUT 成功再升级成 `Applied`：跑满退出时它就是最后一轮的意图。
            stage1 = Stage1Outcome::PutExhausted {
                member_tag: member_tag.to_string(),
            };
            if self
                .hot_switch_selector(PROXY_SELECTOR_TAG, member_tag)
                .await
            {
                if want_direct {
                    if let Some(id) = target_id.as_deref() {
                        self.mark_login_fallback_engaged(id, cur);
                    }
                }
                stage1 = Stage1Outcome::Applied {
                    member_tag: member_tag.to_string(),
                };
                break;
            }
            // 管理 API 未就绪 / 瞬时失败 → 短退避后重试。
            tokio::time::sleep(std::time::Duration::from_millis(
                Self::REASSERT_RETRY_DELAY_MS,
            ))
            .await;
        }
        let rule_intents = self.reassert_rule_selectors(config).await;
        ReassertOutcome {
            stage1,
            rule_intents,
        }
    }

    /// **H3 阶段 2**：把各 `rule-sel-<id>` 校正回对应规则的 `targetServerId`（防 cache_file 把规则选择
    /// 回弹到旧节点）。
    ///
    /// - **无 `targetServerId` 的规则跳过**：它们生成时 `default = proxy-selector`（嵌套跟随全局），
    ///   而 sing-box 重载不擦 selector 的 default ⇒ 跟随关系本身不需要校正（上游 同处注释明写此语义）。
    /// - **不重试**：阶段 1 成功已经证明管理 API 可用；失败由 cache/default 兜底。
    /// - **selector tag 取自 `switch_snapshot.rule_target`**，绝不自己 `format!("rule-sel-{id}")`：生成侧
    ///   撞名时会追加 ` (n)` 后缀（`builder/outbounds.rs` 的 `emit`），手拼模板会 PUT 到一个不存在的 tag。
    ///   那份快照本身还经「该 selector 是否真在生成产物里」过滤过，是运行核 rule-sel 的唯一真值。
    /// - **逐条串行 await**（上游 是 fire-and-forget 并发）：best-effort 语义等价（`hot_switch_selector`
    ///   已把失败吞成 `false`），但顺序确定 ⇒ 可断言、可复现。
    ///
    /// 返回**尝试过**的 `(selector_tag, member_tag)` 序列，交给 [`Self::attest_runtime_selector`] 读回对账
    /// （PUT 返回值不带出去：那是意图侧的东西，成败以核里读回来的运行期值为准）。
    async fn reassert_rule_selectors(&self, config: &UserConfig) -> Vec<(String, String)> {
        let mut intents: Vec<(String, String)> = Vec::new();
        let Some(snapshot) = self.switch_snapshot.read().ok().and_then(|g| g.clone()) else {
            return intents; // 无快照（核未起/已停）→ 无从解析 rule-sel tag
        };
        let latest: Option<UserConfig> = self
            .current_config
            .read()
            .ok()
            .and_then(|g| g.clone())
            .and_then(|v| serde_json::from_value(v).ok());
        let cur = latest.as_ref().unwrap_or(config);

        for rule in &cur.custom_rules {
            if !rule.enabled || rule.action != RuleAction::Proxy {
                continue;
            }
            let Some(target) = rule.target_server_id.as_deref() else {
                continue; // 无目标 → default=proxy-selector 嵌套跟随全局，无须校正
            };
            intents.extend(
                self.reassert_one_rule_selector(&snapshot, &format!("custom:{}", rule.id), target)
                    .await,
            );
        }
        for app_rule in &cur.app_rules {
            if !app_rule.enabled || app_rule.action != RuleAction::Proxy {
                continue;
            }
            let Some(target) = app_rule.target_server_id.as_deref() else {
                continue;
            };
            intents.extend(
                self.reassert_one_rule_selector(
                    &snapshot,
                    &format!("app:{}", app_rule.app_id),
                    target,
                )
                .await,
            );
        }
        intents
    }

    /// 单条 rule-sel 的校正 PUT。快照里查不到该规则的 selector（生成时被剔除）或查不到目标节点的 tag
    /// （目标被 gate 剔除 / 已删除）→ 跳过，**不是 FATAL**：该 selector 的 default 仍是有效成员。
    ///
    /// 返回 `Some((selector_tag, member_tag))` ⟺ **确实 PUT 过**（不论成败）；跳过的两条腿返 `None`，
    /// 它们没有可对账的意图。
    async fn reassert_one_rule_selector(
        &self,
        snapshot: &SwitchSnapshot,
        rule_key: &str,
        target_server_id: &str,
    ) -> Option<(String, String)> {
        let entry = snapshot.rule_target.get(rule_key)?;
        let member_tag = snapshot.id_to_tag.get(target_server_id)?;
        let _ = self
            .hot_switch_selector(&entry.selector_tag, member_tag)
            .await;
        Some((entry.selector_tag.clone(), member_tag.clone()))
    }

    /// **H3 阶段 3：运行期出口自证** —— 把「实际生效出口 ≠ 选中节点」这条轴变成可观测的。
    ///
    /// # 为什么非有这一步不可
    ///
    /// [`attest_selected_exit`](Self::attest_selected_exit) 自述「纯函数、零 I/O、不用探针 / 不查
    /// selector」，它比的是**生成 config 解出的出口**对**盘上 `selectedServerId`** —— 本 bug 下这两个
    /// 都写着选中节点，故必判 `Match`。真机血证（盘上 Hk01、生成的 `proxy-selector.default = "Hk01"`、
    /// 核实走 `Tailscale`）就是从它眼皮底下走过去并打了「通过」的那一次。**两份同源的意图对账，
    /// 永远量不出运行期的分叉。**
    ///
    /// 本方法是它的读侧对偶：一半靠校正腿的终局（写侧：PUT 到底做成了没有），一半靠
    /// [`SingBoxApiClient::first_groups_snapshot`](polaris_singbox_grpc::SingBoxApiClient::first_groups_snapshot)
    /// 读回核**此刻实际**指着谁（读侧）。
    ///
    /// # 为什么不是「起核后探一次出口 IP」
    ///
    /// 那条腿仓里已经有了（[`schedule_exit_ip_refresh`](Self::schedule_exit_ip_refresh)，且已挂在校正
    /// 续延上），再探一次既重复又慢一整个网络 RTT。本方法零网络出站：只对 loopback 上的管理 API 读一帧。
    ///
    /// # 世代/存活守卫
    ///
    /// 读回来的是**当前核**的状态，而这条 `'static` 任务能活过停核/换核。世代已变或核已停 → 整段退场：
    /// 此时无论读到什么，它都不是「用户正在看的那个核」的事实，报出来就是假信号。
    ///
    /// # 射程外：`probe-selector-*`（**有意不接线**）
    ///
    /// 真机 `cache.db` 的 `selected` bucket 里除了 `proxy-selector → Tailscale`，还躺着
    /// `probe-selector-0..15 →` 上一轮测速残留的节点 —— 它们同样在分叉。但那 16 个槽是**测速探测池**，
    /// 起核时校正腿一次都不 PUT（槽位由 `probe_select_slot` 在每次测速临选临用），此刻**没有可对账的
    /// 意图**：拿「上一轮残留」去比「本轮还没发生的选择」只会得出一堆无意义的告警。真要覆盖，正确的
    /// 位置是测速自己选槽之后，不是起核自证这里。快照本身是全量的（`SubscribeGroups` 返回所有 group），
    /// 将来要接线不必再动读侧。
    async fn attest_runtime_selector(self: &Arc<Self>, outcome: &ReassertOutcome, my_gen: u64) {
        if self.gate.generation() != my_gen || !self.status().running {
            log::debug!("运行期出口自证：世代已变 / 核已停 → 退场");
            return;
        }
        // 只有「PUT 成功」这一支需要读回来对账：另外三支的结论不依赖运行期值（放弃腿本身就是结论，
        // 主动退场则不出结论），此时再去连一次管理 API 纯属多余。
        let groups = match outcome.stage1 {
            Stage1Outcome::Applied { .. } => self.read_selector_groups().await,
            _ => None,
        };
        match attest_runtime_selection(outcome, groups.as_deref()) {
            SelectorAttestation::Match => {
                log::info!("运行期出口自证通过：selector 实际选择 == 校正意图");
            }
            other => self.set_nonfatal_error(&other.user_message(), code::EXIT_MISMATCH),
        }
    }

    /// 读回各 group 的运行期选择。读不到（管理 API 连不上 / 首帧超时 / 核正在停）→ `None`
    /// （**不是**空 `Vec`：空 `Vec` 是「核确实没有 group」，两者在
    /// [`attest_runtime_selection`] 里处置相同但语义不同，别在这一层就抹平）。
    ///
    /// 单测经 `management_api_stub` 注入（同 [`put_outbound`](Self::put_outbound) 的先例）：
    /// 桩未预置 → `None` → 自证本轮不判定，与生产读失败**同一条码路**，测试环境不比生产宽容。
    async fn read_selector_groups(&self) -> Option<Vec<GroupSelection>> {
        #[cfg(test)]
        if let Some(sink) = self
            .management_api_stub
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(Arc::clone))
        {
            return sink.groups();
        }
        match self.management_api().await.groups_snapshot().await {
            Ok(groups) => Some(groups),
            Err(e) => {
                log::warn!("运行期 selector 读回失败（本轮不判定）：{e:?}");
                None
            }
        }
    }

    /// A4 登录期出口让位【对账】（单一入口，幂等可重入；PUT 成功才翻 flag → 杜绝「flag 与 selector 脱节永卡 direct」）。
    ///
    /// 三态决策（按选中出口 backendState）。engage：符合条件 且 `NeedsLogin`（含 key 过期）→ 热切
    /// proxy-selector→direct，成功才置 flag、失败下次 tick 重试。disengage：已让位 且（不再符合条件[关开关/
    /// 切非 TS/direct/authKey] 或 已就绪 `Running`）——同一选中出口则 PUT 切回其 tag（关开关切回=用户明确「宁可
    /// 授权失败也不直连」），切走出口则仅清 flag（不 PUT）。其余过渡态（NoState/Starting/Stopped/无帧）维持现状
    /// 不翻转（避免过渡期抖动 / 已登录节点起核闪直连）。由 STATUS 帧 / switchMode 非重启腿 / 起核预置后共同驱动；
    /// 核未起时 hotSwitch 返 false → 不改 flag。上游 `reconcileLoginFallback`。
    ///
    /// 开头有一道**早退闸**（谓词 `!engaged && !选中出口是 TS`），只跳过「结构性无任何可观测效果」的那
    /// 一格，决策矩阵与谓词推导见函数体内注释。非 TS 用户的每帧成本由此归零。
    async fn reconcile_login_fallback(&self) {
        // 单飞：抢占失败（已在飞）→ 丢弃后来者（下一帧/tick 幂等收敛）。
        if self.login_fallback_reconciling.swap(true, Ordering::SeqCst) {
            return;
        }
        let _guard = ReconcileGuard(&self.login_fallback_reconciling);

        // ── 早退闸：本帧结构性不可能有任何可观测效果时，跳过下面两份整配置分配 ──
        //
        // 三态决策矩阵（`eligible` = 配置层符合让位形态，**蕴含**「选中出口是 TS 协议」；
        // `state` = 选中出口 STATUS 末帧 backendState；`engaged` = 当前让位 flag）：
        //
        // | # | eligible | state       | engaged | 本帧动作                                              |
        // |---|----------|-------------|---------|-------------------------------------------------------|
        // | 1 | true     | NeedsLogin  | 任意    | **engage**：PUT selector→direct，成功才置 flag/emit    |
        // | 2 | true     | Running     | true    | **disengage**：同出口 → PUT 回其 tag；已切走 → 仅清 flag |
        // | 3 | true     | Running     | false   | 维持（本就没让位）                                     |
        // | 4 | true     | 其它 / 无帧 | 任意    | 维持（过渡态不翻转，避免抖动 / 已登录节点起核闪直连）   |
        // | 5 | false    | 任意        | true    | **disengage**：关开关 / 切非 TS / authKey / direct 模式 |
        // | 6 | false    | 任意        | false   | **无任何效果** ← 本闸的射程，且**仅此一行**             |
        //
        // ⚠️ 上表是**过了下面两条前置早退之后**的决策图，不是本函数的全图：`current_config` 为空、
        // 或 `UserConfig` 反序列化失败时（见下方两条 `else { return; }`），函数在读到 `eligible` 之
        // 前就退场 —— `engaged=true` 时这**同样吞掉 disengage**（既存行为，本批未改：那两条是「连
        // 真值都读不出来」，此时按旧状态维持比按残缺配置翻转更保守）。本闸只在 `!engaged` 时开火，
        // 与这两条早退不相交，故上面的论证不受影响；但别把这张表当成函数全图。
        //
        // 谓词必须是两条的合取：`eligible ⇒ 选中是 TS`（`mesh_login_fallback_should_engage` 的必要
        // 条件之一），故 `!选中是 TS` 单独就排除第 1 行；第 2/5 行则一律以 `engaged` 为前提，故
        // `!engaged` 排除它们。**只判「选中是不是 TS」会杀掉第 5 行**——用户从 TS 出口切走后
        // `eligible` 立刻为假，而那一帧恰恰必须跑完才能清 flag + 撤让位横幅
        // （`emit_mesh_login_fallback(false)`）；早退会让 engaged 态永不收敛、横幅永不撤。
        //
        // 与 `mesh.rs::has_ts_status` 的范式差别（**别照抄那条**）：那条安全，是因为「无 TS 帧 ⇒ 结论
        // 恒为无告警」；本函数在 engaged 态下结论会变，不满足该前提，故必须把 `engaged` 并进谓词。
        //
        // 竞态：`engaged` 由假翻真只发生在本函数与 `reassert_selector_selection`，而后者置 flag 的
        // 前提同样是「选中出口为 TS」⇒ 那条腿下本闸第二个合取项亦为假、不会早退。配置在本闸与下面
        // 那次读之间被改写只影响本帧取舍，STATUS 每帧驱动 ⇒ 下一帧即收敛。
        if !self.login_fallback_engaged() && !self.selected_exit_is_tailscale() {
            return;
        }

        let Some(raw) = self.current_config.read().ok().and_then(|g| g.clone()) else {
            return;
        };
        // 借用反序列化而非 `from_value(raw.clone())`：`raw` 在下一行的 `login_fallback_eligible`
        // 里还要用，所以上面那份 clone 省不掉；但 `from_value` 要的是 owned `Value` ⇒ 只能再深拷
        // 一整棵配置树（含全部节点），拷完立刻丢。`UserConfig` 无 borrow 字段，两条路等价：
        // 反序列化失败仍落同一条 `else { return; }`。
        let Ok(config) = UserConfig::deserialize(&raw) else {
            return;
        };
        let sel_id = config.selected_server_id.clone().filter(|s| !s.is_empty());
        let eligible = sel_id.is_some() && self.login_fallback_eligible(&config, &raw);
        let backend_state = sel_id
            .as_deref()
            .and_then(|id| self.mesh.selected_exit_backend_state(id));

        // engage：符合条件 且 明确需要交互登录（NeedsLogin / 过期）。**不**因「已 engaged」提前 return——每次
        // NeedsLogin 帧都重 PUT direct（gRPC 选同成员=核侧 no-op，无害）→ 与起核预置这个独立写者脱节时能自愈；
        // markEngaged 的 first 守卫保证 UI 只 emit 一次。
        if eligible && backend_state.as_deref() == Some("NeedsLogin") {
            if !self
                .hot_switch_selector(PROXY_SELECTOR_TAG, DIRECT_TAG)
                .await
            {
                return; // PUT 失败：不改 flag，下次 tick 重试
            }
            // sel_id 必 Some（eligible 蕴含）。
            if let Some(id) = sel_id.as_deref() {
                self.mark_login_fallback_engaged(id, &config);
            }
            return;
        }

        // disengage 条件：已让位 且（不再符合条件 或 已就绪 Running）。过渡态一律维持现状。
        let (engaged_now, engaged_id) = self
            .login_fallback
            .lock()
            .map(|g| (g.engaged, g.server_id.clone()))
            .unwrap_or((false, None));
        let should_disengage =
            engaged_now && (!eligible || backend_state.as_deref() == Some("Running"));
        if !should_disengage {
            return;
        }

        match engaged_id {
            // 同一选中出口撤销让位（就绪 或 用户关开关）→ PUT 切回其 tag（成功才清 flag）。
            Some(eid) if Some(eid.as_str()) == sel_id.as_deref() => {
                let tag = self
                    .switch_snapshot
                    .read()
                    .ok()
                    .and_then(|g| g.as_ref().and_then(|s| s.id_to_tag.get(&eid).cloned()));
                if let Some(tag) = tag {
                    if !self.hot_switch_selector(PROXY_SELECTOR_TAG, &tag).await {
                        return; // PUT 失败：不改 flag，下次 tick 重试
                    }
                } else {
                    // tag 缺失（罕见：核停/gate 剔除）→ 无法 PUT 回；清 flag 避免永卡，selector 由起核预置兜底。
                    log::warn!("组网出口让位撤销：找不到出口 tag（{eid}），跳过 selector 切回");
                }
                let name = config
                    .servers
                    .iter()
                    .find(|s| s.id == eid)
                    .map(|s| s.name.clone());
                self.set_login_fallback(false, None);
                log::info!(
                    "组网出口「{}」让位撤销，默认路由切回该出口",
                    name.as_deref().unwrap_or(&eid)
                );
                self.emit_mesh_login_fallback(false, name.as_deref());
            }
            // 切走出口：selector 已由 planHotSwitch/config default PUT 到新目标，仅清 flag + 撤 UI（不 PUT，避免打架）。
            _ => {
                self.set_login_fallback(false, None);
                self.emit_mesh_login_fallback(false, None);
            }
        }
    }

    /// 取出并清除 pending switch 配置（id 对得上才取；对不上回落 None）。与 force-restart 同构。
    ///
    /// 返回 `(config, defer_restart)` —— 两者必须一起取，理由见 [`Self::pending_switch`] 字段注释。
    fn take_pending_switch(&self, id: Option<u64>) -> Option<(Value, bool)> {
        let mut g = self.pending_switch.write().ok()?;
        match (&*g, id) {
            (Some((sid, _, _)), Some(want)) if *sid == want => {
                g.take().map(|(_, c, defer)| (c, defer))
            }
            _ => None,
        }
    }

    /// 取待应用节点差集（`proxy:getPendingChanges`）：当前 config 相对**起核快照**的增 / 改 / 删。
    ///
    /// 契约 = [`PendingChangesSummary`]（`{added, modified, removed}`），pull 与 push 同一个结构。
    ///
    /// # 基准与投影
    ///
    /// - **基准**：`startup_snapshot`（id 集）与 `switch_snapshot`（指纹表）—— 二者在起核就绪腿相隔 8 行
    ///   同置、停核腿相隔 8 行同清，是同一刻同一份配置的**孪生投影**，不是两个基准。
    ///   `modified` 的「旧」侧取 `switch_snapshot.fingerprints`（**不重算**）：重算等于把「运行核起于什么」
    ///   换成「磁盘上现在是什么」，那就恒等于空集了。
    /// - **投影**：`added`/`removed` 是 id 集差；`modified` 是**全维**指纹比对
    ///   （[`modified_fingerprint`](crate::runtime::node_fingerprints::modified_fingerprint)）。
    ///
    /// # 各腿的降级方向（全部保守：少显示，不虚报）
    ///
    /// - 核未运行 / 无 `startup_snapshot` → 全空差集（没有「运行核」这个分母，谈不上待应用）。
    /// - 有 `startup_snapshot` 但无 `switch_snapshot`（孪生对理论上不可能只剩一半）→ `added`/`removed`
    ///   照给，`modified` 空：拿不到起核那刻的指纹表就没有比对基准，宁可漏报也不猜。
    /// - 读当前 config 失败 → 回落到快照自身 ⇒ 三个集合全空（自己跟自己比）。
    ///
    /// 三个集合都**排序**后返回：`HashSet` 的迭代序每次进程都不同，不排序会让 UI 明细列表无故重排、
    /// 也让单测只能写成集合比较。排序成本 O(n log n)、n = 节点数，可忽略。
    pub fn pending_changes(&self) -> PendingChangesSummary {
        // 无起核快照 = 核没在跑（或快照不可信）⇒ 没有「运行核」这个分母 ⇒ 谈不上待应用。
        // `restart_deferred` 在此同样为 false：停核腿已把它复位，这里只是把该不变式写死在返回值上。
        let empty = || PendingChangesSummary {
            added: Vec::new(),
            modified: Vec::new(),
            removed: Vec::new(),
            restart_deferred: false,
        };
        let Some(snap) = self.startup_snapshot.read().ok().and_then(|g| g.clone()) else {
            return empty();
        };
        let current = self.config.current().unwrap_or_else(|_| snap.clone());
        let old_ids: std::collections::HashSet<String> = server_ids(&snap);
        let new_ids: std::collections::HashSet<String> = server_ids(&current);

        let mut added: Vec<_> = new_ids.difference(&old_ids).cloned().collect();
        let mut removed: Vec<_> = old_ids.difference(&new_ids).cloned().collect();

        // `modified` ⊂ old ∩ new：只在一侧存在的 id 属 added/removed，不属 modified。
        let snap_fps = self
            .switch_snapshot
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.fingerprints.clone()))
            .unwrap_or_default();
        let current_fps = node_fingerprints::modified_table_json(&current);
        let mut modified: Vec<_> = old_ids
            .intersection(&new_ids)
            .filter(|id| match (snap_fps.get(*id), current_fps.get(*id)) {
                (Some(old), Some(new)) => old != new,
                // 任一侧取不到指纹（快照缺失 / 节点解析不出）⇒ 没有比对基准 ⇒ 不判 modified。
                _ => false,
            })
            .cloned()
            .collect();

        added.sort();
        modified.sort();
        removed.sort();
        PendingChangesSummary {
            added,
            modified,
            removed,
            restart_deferred: self.restart_deferred.load(Ordering::SeqCst),
        }
    }

    /// 强制应用待应用变更（上游 `proxy:applyPendingChanges`）：force-restart 入核。
    ///
    /// 1:1 对齐 上游 `applyConfigForcingRestart`（:1723-1740）的**判定顺序**：
    /// 1. lifecycle 在飞（depth>0）→ 置 pending 专用配置 + restart_pending → `deferred`
    ///    （**必须先于句柄判空**：restart 的 stop→start 空窗内句柄暂空，以句柄早退会静默丢弃本次强制重启，
    ///    复现 H-1 死循环）
    /// 2. 真未运行 → `skipped`（下次 start 从磁盘纳入）
    /// 3. depth=0 且运行中 → 去抖重启排程 → `applied`
    ///
    /// **边界**：上游的 `coreSwapInProgress` 轴（换核窗口 → deferred）在 Polaris 无对应 actor，
    /// 该轴不存在（非省略）。
    pub async fn apply_pending(self: &Arc<Self>) -> &'static str {
        let new_config = match self.config.current() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("applyPendingChanges 读配置失败: {e} → skipped");
                return "skipped";
            }
        };
        let id = self.force_restart_seq.fetch_add(1, Ordering::SeqCst);

        // 1. lifecycle 在飞 → 排入 drain（由 end() depth 归零时排空一次）。
        if self.gate.is_busy() {
            if let Ok(mut g) = self.pending_force_restart.write() {
                *g = Some((id, new_config));
            }
            self.gate.set_force_restart(id);
            self.gate.set_restart_pending();
            log::info!("applyPendingChanges：lifecycle 在飞（depth>0）→ deferred（排入 drain）");
            return "deferred";
        }
        // 2. 真未运行 → 下次 start 从磁盘纳入新节点。
        if !self.core_running() {
            log::info!("applyPendingChanges：核未运行 → skipped");
            return "skipped";
        }
        // 3. depth=0 且运行中 → 去抖重启（drain 亦读专用字段，绕开潜在覆盖）。
        if let Ok(mut g) = self.pending_force_restart.write() {
            *g = Some((id, new_config));
        }
        self.gate.set_force_restart(id);
        self.schedule_restart();
        log::info!("applyPendingChanges：运行中 + 非在飞 → applied（已排程去抖重启）");
        "applied"
    }

    /// 取 dashboard 连接信息（上游 `app:getSingboxDashboardConnection`，:2377-2389）。
    ///
    /// 端口取运行期管理 API 端口（动态解析，渲染端构造不出）；secret 取 currentConfig.clashApiSecret。
    pub fn dashboard_connection(&self) -> Value {
        let s = self.status();
        if !s.running || s.clash_api_port == 0 {
            return serde_json::json!({ "ok": false, "url": "", "apiUrl": "", "secret": "" });
        }
        let secret = self
            .config
            .current()
            .ok()
            .and_then(|c| {
                c.get("clashApiSecret")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        serde_json::json!({
            "ok": true,
            "url": format!("http://127.0.0.1:{}/dashboard/", s.clash_api_port),
            "apiUrl": format!("http://127.0.0.1:{}", s.clash_api_port),
            "secret": secret,
        })
    }

    /// 配置运行时引用（供 command 层 / 其他运行时取 config 路径等）。
    #[must_use]
    pub fn config(&self) -> &ConfigManager {
        &self.config
    }

    /// sing-box 临时配置文件路径（写 generate_sing_box_config 输出，供 spawner 读）。
    #[must_use]
    pub fn runtime_config_path(&self) -> PathBuf {
        self.config.join("singbox-runtime.json")
    }

    /// 外化规则目录（`<configDir>/custom-rules`）——与 `generate_deps` 的 `custom_rules_dir` 及
    /// config-engine route/DNS ext 分支 `ext_rule_file_exists` 探测路径同源（单一真值）。
    fn custom_rules_dir(&self) -> PathBuf {
        self.config.dir().join("custom-rules")
    }

    /// 外化自定义规则落盘是否处于降级态（`customRuleFilesDegraded`）。
    fn custom_rule_files_degraded(&self) -> bool {
        self.custom_rule_files_degraded.load(Ordering::SeqCst)
    }

    /// 起核期两轴动态空闲端口解析（管理 API + update-in），**每次起核尝试重解析**（端口重分配自愈）。
    ///
    /// 抽出以便 retry 每次拿新口：osascript 授权窗口 / 竞态被抢占 → 换口重生成，对齐 上游 onRetry
    /// `allocateProbePorts`（:913）。`control_port` / `mixed_port` 由 config 决定、跨重试不变，故不在此解析。
    ///
    /// **§15**：额外分配 K 个测速探测池端口（`probe-in-k`）——排除 api/update-in/control/http/mixed 及池内互异；
    /// 整批原子失败 → 空 vec（探测池不注入，测速回退活跃出口）。返回 `(api, update_in, pool_ports)`。
    fn resolve_start_ports(
        &self,
        user_config: &UserConfig,
        control_port: u16,
    ) -> (u16, u16, Vec<u16>) {
        // 管理 API 端口（上游 resolveTailscaleApiPort，:3006）。
        let exclusions = PortExclusions::for_primary_api(
            Some(control_port),
            user_config.http_port,
            None, // UserConfig 增量子集无 socksPort 字段 → 不排除（与 config-engine 现状一致）
            user_config.mixed_port,
        );
        let resolved =
            PortAllocator::new(TokioPortProvider).resolve_tailscale_api_port(&exclusions);
        let api_port = resolved.port;
        if resolved.used_fallback {
            log::warn!("管理 API 端口 5 次解析均撞排除集 → 回落 {api_port}");
        }
        // C19 update-in 端口：额外排除已占的 api_port，fallback = control_api+3（避与 api/login 的 +1/+2 撞）。
        let update_in_excl = PortExclusions::for_login_api(
            api_port,
            Some(control_port),
            user_config.http_port,
            None,
            user_config.mixed_port,
        );
        let update_in_resolved = PortAllocator::new(TokioPortProvider)
            .resolve_free_local_port(&update_in_excl, control_port.wrapping_add(3));
        let update_in_port = update_in_resolved.port;
        if update_in_resolved.used_fallback {
            log::warn!("update-in 端口 5 次解析均撞排除集 → 回落 {update_in_port}");
        }
        // §15 测速探测池 K 端口（probe-in-k）：额外排除已占的 api/update-in（避与管理面/更新链路撞）。
        // 整批原子失败（任一槽拿不到互异空闲口）→ 空 vec：探测池不注入，测速回退活跃出口（不阻断代理启动）。
        let pool_excl = PortExclusions::for_login_api(
            api_port,
            Some(control_port),
            user_config.http_port,
            None,
            user_config.mixed_port,
        );
        // update_in_port 也须排除（for_login_api 未涵盖）——借 socks 槽注入（该槽当前恒 None）。
        let pool_excl = PortExclusions {
            socks: update_in_port,
            ..pool_excl
        };
        let pool_ports = PortAllocator::new(TokioPortProvider)
            .resolve_distinct_free_ports(&pool_excl, PROBE_POOL_SIZE);
        if pool_ports.is_empty() && PROBE_POOL_SIZE > 0 {
            log::warn!(
                "测速探测池 {PROBE_POOL_SIZE} 端口分配失败 → 探测池不注入（测速回退活跃出口）"
            );
        }
        (api_port, update_in_port, pool_ports)
    }

    /// 起核重试退避 sleep（第 `attempt` 次失败后、下一次尝试前）。**可被取消中断**。
    /// 指数：`delay * 2^(attempt-1)`；恒定：`delay`（对齐 上游 retry util `delay * 2^attempt`，其 attempt 0-based）。
    ///
    /// 返回 `true` = 退避期内被接管（用户点停止 / 更新的 start），调用方**必须立即走让位腿、不得
    /// `continue`**：再起一次核就是在接管方的核之上叠第二个进程。返回 `false` = 睡满，照常重试。
    ///
    /// 走 [`sleep_unless_superseded`](Self::sleep_unless_superseded) 而非裸 `tokio::time::sleep`：
    /// 退避是这条腿上**最长的单次阻塞**（TUN 预算下 2s→4s），裸 sleep 会把取消延迟抬到一个退避周期。
    async fn sleep_start_backoff(
        &self,
        budget: &StartRetryBudget,
        attempt: u32,
        my_gen: u64,
    ) -> bool {
        let delay = if budget.exponential_backoff {
            // attempt 1-based → 移位 attempt-1（clamp 上限防溢出，实际预算远不达）。
            budget
                .delay_ms
                .saturating_mul(1u64 << (attempt.saturating_sub(1)).min(16))
        } else {
            budget.delay_ms
        };
        log::info!("起核失败，将在 {delay}ms 后进行第 {} 次尝试", attempt + 1);
        if self
            .sleep_unless_superseded(my_gen, Duration::from_millis(delay))
            .await
        {
            log::info!(
                "起核退避期被接管（世代 {my_gen} → {}）→ 就地中断退避、让位，不等睡满 {delay}ms",
                self.gate.generation()
            );
            return true;
        }
        false
    }

    /// **#327**：起核**就绪后**正向验证本次 TUN 适配器真被建出来（每条重试腿各验一次）。
    ///
    /// # 缺陷原形
    ///
    /// 就绪门（[`wait_ready`](Self::wait_ready) → `core-supervisor::wait_for_core_ready`）的三条判据
    /// —— 管理 API 环回口可连、进程活、未被接管 —— 没有一条与 TUN 网卡有关。于是「sing-box 活着、
    /// mixed 入站正常、wintun 适配器从未创建」会被判成起核成功：用户看到「已连接」，TUN 却完全没生效
    /// （上游侧的同一形态表现为无限「正在自动重试」）。
    ///
    /// # 与 [`verify_tun_route_captured`](Self::verify_tun_route_captured) 的分工（两者互不重叠，别合并）
    ///
    /// | 层 | 时机 | 判据 | 失败处置 |
    /// |---|---|---|---|
    /// | **本方法** | 就绪**后**、逐腿 | 这一张建出来没（正向枚举） | 计入重试预算，耗尽报 [`code::TUN_ADAPTER_MISSING`] |
    /// | [`verify_tun_route_captured`](Self::verify_tun_route_captured) | 全部重试**之后**一次 | 默认路由归属差分 | 硬终止，报 [`code::TUN_ROUTE_NOT_CAPTURED`] |
    ///
    /// （曾经还有第三层「spawn 前等上一张 wintun 释放」，#159。已删：sing-tun 的 `New()` 撞
    /// `os.ErrExist` 会 `OpenAdapter` 复用同名网卡，残留适配器本就不阻断起核，那条腿只是白等。）
    ///
    /// 顺序也不能对调：网卡都没有时去问「默认路由切走了没」，答案必然是「没切」，于是用户拿到
    /// 「其他 VPN 占用默认路由，请先断开」——一条与现场毫无关系的指引。先验存在性，才轮得到问归属。
    ///
    /// # `iface` 不在可枚举前缀面内 ⇒ 不可断言（这条漏了会杀正常核）
    ///
    /// [`AdapterProbe::list_matching_adapters`] 只返回
    /// [`PROBE_PREFIXES`](polaris_helper::platform::windows::wintun::PROBE_PREFIXES) 命中的适配器。用户把
    /// TUN 接口名改成 `my-tun`（`resolve_win_tun_interface_name` 允许）时，我方**永远**枚举不到那张网卡
    /// → 若据此判「没建出来」，就会把一次完全正常的起核杀掉。故先过
    /// [`adapter_name_is_probeable`](polaris_helper::platform::windows::wintun::adapter_name_is_probeable)
    /// （与枚举实现共用同一谓词），看不见就整条跳过。
    ///
    /// 复用起核前那对超时/间隔常量（3s / 200ms）：健康路径上网卡在就绪前就挂好了 ⇒ 首次枚举即命中、
    /// 零 sleep；异常路径给内核留 3s 挂载余量。为同一件事再引入第二组可调参数不会换来任何东西。
    async fn probe_tun_adapter_present(
        &self,
        mode: ProxyModeType,
        iface: &str,
        attempt: u32,
    ) -> TunAdapterObservation {
        if !should_probe_wintun_adapter(mode, platform_tag()) {
            return TunAdapterObservation::Indeterminate; // 非 TUN / 非 Windows → 零系统调用
        }
        log::debug!("起核后验证 wintun 适配器存在性：iface={iface}（第 {attempt} 次尝试）");
        #[cfg(windows)]
        {
            use polaris_helper::platform::windows::wintun::{
                adapter_name_is_probeable, probe_adapter_present, PresenceOutcome, StdSleep,
                WinAdapterProbe, DEFAULT_POLL_INTERVAL, DEFAULT_PROBE_TIMEOUT,
            };
            if !adapter_name_is_probeable(iface) {
                log::info!(
                    "TUN 适配器存在性：接口名 {iface} 不在可枚举前缀面内（自定义名）→ 不可断言，不闸"
                );
                return TunAdapterObservation::Indeterminate;
            }
            // 有界轮询内含 `std::thread::sleep`（最长 DEFAULT_PROBE_TIMEOUT=3s）→ 必须挪出 async worker。
            let expected = iface.to_owned();
            let outcome = tokio::task::spawn_blocking(move || {
                probe_adapter_present(
                    &WinAdapterProbe,
                    &expected,
                    DEFAULT_PROBE_TIMEOUT,
                    DEFAULT_POLL_INTERVAL,
                    &StdSleep,
                )
            })
            .await;
            match outcome {
                Ok(PresenceOutcome::Present) => {
                    log::info!("TUN 适配器已建出：{iface}");
                    TunAdapterObservation::Present
                }
                Ok(PresenceOutcome::Absent { seen }) => {
                    log::error!(
                        "TUN 适配器未建出：{iface} 在 {DEFAULT_PROBE_TIMEOUT:?} 内始终未出现\
                         （同前缀可见适配器：{}）",
                        if seen.is_empty() {
                            "无".to_owned()
                        } else {
                            seen.join(", ")
                        }
                    );
                    TunAdapterObservation::Absent
                }
                // 枚举 API 坏了 / 任务 join 失败 → 判据本身不可用，绝不据此杀核。
                Ok(PresenceOutcome::Error(e)) => {
                    log::warn!("TUN 适配器枚举失败（{e}）→ 不可断言，不闸");
                    TunAdapterObservation::Indeterminate
                }
                Err(e) => {
                    log::warn!("TUN 适配器探测任务 join 失败：{e} → 不可断言，不闸");
                    TunAdapterObservation::Indeterminate
                }
            }
        }
        // 非 Windows 编译单元：上面的 `should_probe_wintun_adapter` 已恒假早退，此处仅作类型收口。
        #[cfg(not(windows))]
        TunAdapterObservation::Indeterminate
    }

    /// **#332**：helper 起核腿开始前的启动日志游标（非 helper 腿为空游标，零系统调用）。
    ///
    /// 新 helper 会在每次 spawn 前 fresh-rotate 启动日志，旧 helper 仍会 append。整文件扫 FATAL 会把
    /// 上一次会话（甚至上一条重试腿）的失败当成这一次的真因 —— 那比不给真因更糟，因为它看起来是
    /// 确诊。故同时记文件身份与长度：同一文件才从旧长度读，身份变化或文件缩短都从 0 读。
    ///
    /// 取不到长度（文件还不存在 = 首次起核）→ 0，语义正好是「整文件都是本腿写的」。
    fn startup_log_cursor(&self, via_helper: bool) -> StartupLogCursor {
        if !via_helper {
            return StartupLogCursor::default();
        }
        std::fs::metadata(self.config.join(SINGBOX_STARTUP_LOG)).map_or_else(
            |_| StartupLogCursor::default(),
            |metadata| StartupLogCursor {
                offset: metadata.len(),
                identity: log_file_identity(&metadata),
            },
        )
    }

    /// **#332**：读出本腿核 stderr 里的结构化真因（两条起核路径各取各的来源）。
    ///
    /// - **直起**：核 stderr 是我方管道，[`pipe_to_log`] 已在流上逐行判过 → 直接取槽。
    /// - **helper 起**（Windows/macOS 的 TUN 路径）：app 侧**没有**那根管道，核 stderr 被 helper
    ///   经受管 writer 收进 `SINGBOX_STARTUP_LOG` → 按本腿游标取会话片段再扫。**这一条不能省**：#332 的现场就是
    ///   Windows TUN，而 TUN 恒经 helper 起核 —— 只接管道那条腿，等于修在一条永远跑不到的路上。
    ///
    /// # 已知边界（诚实标注，不是漏了）
    ///
    /// - **直起腿有竞态**：判 Dead 与转发任务读完最后一行之间没有同步点，核 FATAL 后立即退出时可能
    ///   还没写进槽 → 退回泛化 `STARTUP_FAILED`。**只降级不误报**（拿不到真因就不声称有），故不为它
    ///   引入一次「等管道 drain」的额外等待 —— 那要在每条失败腿上给所有用户加延迟，换一个偶发的
    ///   诊断精度。
    /// - 尾巴读取有上限（[`CORE_FATAL_SCAN_BYTES`]）：核在 FATAL 前刷了海量 debug 行时只看最后这一段。
    ///   FATAL 恒是**最后**几行（`log.Fatal` 之后进程即退出），故上限截的是前面的噪音。
    /// - 同步 `std::fs` 读：有界（≤ 上限）本地文件、且只在**失败腿**上发生；与 `start_inner` 里既有的
    ///   `std::fs::write(&config_path, …)` 同款处置，不为它起 `spawn_blocking`。
    fn observe_core_fatal(
        &self,
        via_helper: bool,
        cursor: StartupLogCursor,
        slot: &CoreFatalSlot,
    ) -> Option<CoreFatalKind> {
        let kind = if via_helper {
            let path = self.config.join(SINGBOX_STARTUP_LOG);
            // 新 helper 每次 spawn 前 fresh-rotate（current 文件身份变化）→ 从 0 读；旧 helper 仍在
            // 同一个文件 append（身份不变）→ 从旧长度读。不能只比较长度：新会话完全可能比旧文件更长。
            let metadata = std::fs::metadata(&path).ok()?;
            let start =
                startup_log_read_start(cursor, metadata.len(), log_file_identity(&metadata));
            let tail = read_file_range(&path, start, CORE_FATAL_SCAN_BYTES)?;
            scan_core_fatal(&tail)
        } else {
            slot.lock().ok().and_then(|g| *g)
        };
        if let Some(k) = kind {
            log::error!("核启动失败真因（本腿 stderr 判定）：{k:?}");
        }
        kind
    }

    /// 起核前落盘外化自定义规则文件 + 孤儿对账清扫（`start_inner` 在 generate 前调用）。移植 上游
    /// `writeCustomRuleFiles`（:1636）：① 清降级标记；② mkdir；③ 删孤儿（`is_custom_rule_orphan_file` 命中
    /// 且不在期望集——删规则/禁用/转 inline/改 id/direct 切换的遗留 + 原子写残留 `.tmp`）；④ 期望集内容变才
    /// 原子写。逐文件写失败 → 删旧副本回退 inline + 置降级标记（缺文件触发 route/DNS ext 分支 `existsSync`
    /// 降级走 inline，用内存态值，功能不损），仅 warn 不抛。
    ///
    /// **必须在 generate 前**：generate 的 route/DNS ext 分支按文件真存在性（`ext_rule_file_exists`）决定走
    /// ext 引用还是 inline 降级；文件不在则 ext 分支 100% 不可达。非 smart 模式 `build_custom_rule_files` 返
    /// 空集 → 已存在的外化文件被当孤儿全清（route 侧无消费者）。
    async fn write_custom_rule_files(&self, config: &UserConfig) {
        let dir = self.custom_rules_dir();
        let expected = build_custom_rule_files(config); // fileName → JSON
        self.custom_rule_files_degraded
            .store(false, Ordering::SeqCst);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.custom_rule_files_degraded
                .store(true, Ordering::SeqCst);
            log::warn!(
                "落盘外化规则文件失败（回退 inline）：创建目录 {} 失败：{e}",
                dir.display()
            );
            return;
        }
        // 孤儿清扫：is_custom_rule_orphan_file 命中且不在期望集 → unlink（含裸 .json + .tmp 变体）。
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if is_custom_rule_orphan_file(&name) && !expected.contains_key(&name) {
                    let _ = std::fs::remove_file(dir.join(&name));
                }
            }
        }
        // 期望集落盘（内容未变跳过）。写失败 → 删旧副本回退 inline + 置降级标记。
        for (name, content) in &expected {
            let file_path = dir.join(name);
            let cur = std::fs::read_to_string(&file_path).ok();
            if cur.as_deref() == Some(content.as_str()) {
                continue;
            }
            if let Err(e) = atomic_write_custom_rule(&file_path, content) {
                let _ = std::fs::remove_file(&file_path);
                self.custom_rule_files_degraded
                    .store(true, Ordering::SeqCst);
                log::warn!("外化规则文件写失败，已删旧副本回退 inline：{name}（{e}）");
            }
        }
    }

    /// 运行中外化规则「值」热更：仅原子替换内容变化的文件（rename-over 触发 sing-box fswatch 热重载），
    /// **绝不删文件**（运行中删被挂载文件会致 sing-box reload 报错；删除只在起核 `write_custom_rule_files`
    /// 清扫）。移植 上游 `syncCustomRuleFiles`（:1688）。任一写失败 → 退回去抖重启兜底。
    async fn sync_custom_rule_files(self: &Arc<Self>, config: &UserConfig) {
        let dir = self.custom_rules_dir();
        let expected = build_custom_rule_files(config);
        for (name, content) in &expected {
            let file_path = dir.join(name);
            let cur = std::fs::read_to_string(&file_path).ok();
            if cur.as_deref() == Some(content.as_str()) {
                continue;
            }
            if let Err(e) = atomic_write_custom_rule(&file_path, content) {
                log::warn!("热更外化规则文件失败，退回去抖重启：{name}（{e}）");
                self.schedule_restart();
                return;
            }
        }
    }

    /// 装配 [`GenerateConfigDeps`]（上游侧所有 `this.*` 实例态的真值注入）。
    ///
    /// **边界（本批未接线的轴，一律取保守值，非静默省略）**：
    /// - `race_server_port` / `race_upstream_ips` 由 [`race_server`](Self::race_server) 运行期状态注入，
    ///   该状态由 [`start_race_sidecar`](Self::start_race_sidecar) 在本函数**之前**填好（竞速关 /
    ///   起 sidecar 失败 → 恒 (0, []) = race off）。
    /// - `probe_direct_port` / `probe_proxy_port` 空 → 出口 IP 探针（direct/proxy 双探）未接线（属 ip-info 域，另批）。
    /// - **§15**：`probe_pool_ports` 由 `resolve_start_ports` 分配的 K 个空闲口注入（非空 → config-engine 建
    ///   probe-in-k 入站 + probe-selector-k + 路由 + dns-probe-exit-k；测速经此按波热切量延迟）。空 = 分配失败/回滚。
    /// - `has_cronet` 经 [`cronet_available`]：linux/win 按 libcronet 落盘探测；macOS（arm64+x64）cronet
    ///   已静态编入内核（无 dylib）→ 恒可用。缺库 + 选中 naive 节点 → 生成期报错（符合 上游 语义）。
    ///
    /// **C12**：`own_lan_cidrs` 由 [`enumerate_own_lan_cidrs`] 真枚举本机非回环接口（unix getifaddrs，
    /// 只读非破坏性）。**C19**：`update_in_port` 由 `start` 分配的空闲口注入（>0 时生成 update-in 入站+路由）。
    fn generate_deps(
        &self,
        api_port: u16,
        update_in_port: u16,
        pool_ports: &[u16],
        config: &Value,
    ) -> GenerateConfigDeps {
        let dir = self.config.dir();
        // A2/C13：日志两轴跟随 config（此前硬编码 Info + 不落 disableLogFile）。
        let (log_level, disable_log_file) = log_axes_from_config(config);
        // C11：race sidecar 运行期状态注入。port>0 才生成 dns-node-race + 放行上游直连。
        // IP 与端口**两轴同源**（都来自 sidecar 起好时提交的 `ResolvedUpstreams`）：route 的直连放行按
        // `ip_cidr × port` 叉乘匹配，只下发 IP 会让非标端口的自定义上游在 TUN 下经代理出站（issue #147）。
        let (race_server_port, race_upstream_ips, race_upstream_ports) = self
            .race_server
            .lock()
            .map(|g| (g.port, g.upstream_ips.clone(), g.upstream_ports.clone()))
            .unwrap_or((0, Vec::new(), Vec::new()));
        GenerateConfigDeps {
            platform: platform_tag().to_string(),
            arch: std::env::consts::ARCH.to_string(),
            race_server_port,
            probe_direct_port: None,
            probe_proxy_port: None,
            // C19：>0 才注入（0 = 分配失败/未接线，退化为不生成 update-in，对齐 上游 `deps.updateInPort` 真值判定）。
            update_in_port: (update_in_port > 0).then_some(update_in_port),
            // §15：起核分配的 K 个测速探测池端口（空 = 分配失败/回滚 → 池不注入，测速回退活跃出口）。
            probe_pool_ports: pool_ports.to_vec(),
            lan_resolver_for_dns: None,
            race_upstream_ips,
            race_upstream_ports,
            // macOS(arm64+x64) cronet 静态编入内核（无 dylib 文件）→ 不能只看落盘，否则误拦所有 naive 节点。
            has_cronet: cronet_available(
                self.cronet_lib_exists_for_start(),
                platform_tag(),
                std::env::consts::ARCH,
            ),
            cronet_copy_failed: false,
            // 随包核恒 pin 在 1.14 带（具体版本见 src-tauri/core-manifest.json 的 bundledCoreVersion，
            // **勿在此抄具体 alpha/beta 号**：抄一次就漂一次）→ 恒有 services schema。
            // 换核后若允许 <1.14 需按 coreVersionAtLeast 门控（上游 hasManagementApi）——见边界声明。
            has_management_api: true,
            // B1：隐私模式**活态**（读单一真值 = `commands::config` 的 `PRIVACY_MODE` 进程状态机，
            // 经 emitter 的 `privacy_mode()` 取，见该方法文档解释为何走 emitter 而非另存一份）。
            // 下游 `build_log_config` 据此 `effective()` 把核日志级别抬到 ≥warn —— 隐私期 relay 才
            // 不再把连接明细（含用户访问的域名）写进受管核日志。此前硬编码 false ⇒ 隐私模式只在
            // 前端遮蔽，盘上仍是明文域名 —— 而前端遮蔽只管显示，管不到磁盘，那不是防线。
            //
            // ⚠️ **延迟生效口径（与 上游 一致，别当即时开关读）**：活态只在**本函数（config 生成）**
            // 被读一次，而 config 只在起核时写盘 ⇒ 运行中切隐私模式**不改变已在跑的那个核**的日志级别，
            // 要**下次起核**才生效（上游 `main/index.ts:222` 同款注释：「sing-box 连接日志级别在下次
            // 核心重启时按新隐私」；app.log / UI 侧才是即时收敛）。要即时，得走管理 API 改核日志级别，
            // 那是另一件事、不在本注入面。
            privacy_mode: self.privacy_mode_active(),
            log_level,
            disable_log_file,
            // dashboard #55 回归修复：此前硬编码 None → 面板开关 on 时核无 path → 联网下载兜底 → CWD 相对 mkdir 噪音。
            // 改为解析「运行时下载覆盖 > 随包内置 resources/dashboard」（对齐 上游）→ 命中则核 serve 本地、零下载。
            dashboard_serve_dir: resolve_dashboard_serve_dir(dir),
            tailscale_api_port: api_port,
            cache_path: dir.join("cache.db").to_string_lossy().into_owned(),
            // B3/W26：不再让 sing-box 自己持有固定 output fd。子进程不会响应外部轮转：Unix rename
            // 后继续写旧 inode，Windows 还可能拒绝 rename，均无法形成运行期硬上限。核日志由既有
            // SubscribeLog / 起核 stderr 管道进入 `logging.rs` 的 shared bounded writer；helper 腿的
            // pre-ready/FATAL stderr 则由 helper 同一 writer 收进 `singbox-startup.log`。
            log_file_path: None,
            runtime_rules_dir: dir.join("rules").to_string_lossy().into_owned(),
            rule_resources_path: rule_resource_dir(dir).to_string_lossy().into_owned(),
            custom_rules_dir: dir.join("custom-rules").to_string_lossy().into_owned(),
            tailscale_state_dir_prefix: dir.join("tailscale").to_string_lossy().into_owned(),
            is_valid_srs_fn: is_valid_srs_file,
            // C12：真枚举本机所有非回环接口 CIDR（连入来源排除 guard / bypassLAN carve guard / mesh 重叠告警）。
            own_lan_cidrs: enumerate_own_lan_cidrs(),
            log: config_log,
            on_degraded: config_on_degraded,
        }
    }

    /// 生成配置 → 写盘 → **内核闸门**（`sing-box check`），把内核点名拒收的节点剥掉后重来一轮，
    /// 直到内核收下这份配置（或按 fail-open 停下来）。返回**已落盘的那一份**。
    ///
    /// # 为什么闸门放在这一层而不是生成侧
    ///
    /// 判据是「**内核**认不认」，取证方式是拿**即将下发的那个文件**问**本次解析出的那个核**——两个
    /// 输入都只在运行时层才存在（config-engine 是纯逻辑 crate，既没有核也没有落盘路径）。放生成侧就
    /// 只能退回静态白名单，而那正是已定口径明确排除的做法（逃生舱不得变回白名单，且必与内核版本漂移）。
    ///
    /// # 剥离靠「重新生成」而不是「从数组里删掉那一项」
    ///
    /// 直接从 `outbounds[]` 里 `remove(index)` 会留下一地悬空引用（selector 成员、`route.rules`、
    /// `dns.rules` 还指着那个 tag），而 `check` **抓不到**悬空引用（实测 selector 指向不存在的 tag
    /// 时 `check` rc=0，真起核才 `dependency[X] not found`）⇒ 剥完照样炸，还炸得更难查。
    /// 改成「把该节点从 `servers` 里去掉后重跑 `generate_sing_box_config_with_report`」，
    /// 选择器成员清理 / detour 级联剪枝 / 死引用修正**全部复用生成侧既有机制**，不新造第二份。
    ///
    /// # 剥掉的集合跨重试腿累积（`peeled` 由调用方持有）
    ///
    /// 外层起核重试（端口重分配自愈）会重跑本函数。内核对某个节点的拒收是**确定性**的（同一个节点、
    /// 同一个核，判定不会变），故第 2 腿起无需重新发现，直接沿用 ⇒ 重试腿恒只付 1 次 check。
    async fn generate_and_gate(
        &self,
        user_config: &UserConfig,
        deps: &GenerateConfigDeps,
        config_path: &Path,
        binary: Option<&Path>,
        peeled: &mut BTreeMap<String, InvalidNode>,
    ) -> Result<GateOutcome, String> {
        let started = std::time::Instant::now();
        let mut checks_run: u32 = 0;
        loop {
            // 已剥的节点从 `servers` 摘掉后重新生成（空集合时等价于原配置）。
            let mut effective = user_config.clone();
            effective.servers.retain(|s| !peeled.contains_key(&s.id));
            let gen_out = generate_sing_box_config_with_report(&effective, &BTreeMap::new(), deps)
                .map_err(|e| format!("sing-box 配置生成失败: {e}"))?;
            let json = serde_json::to_string_pretty(&gen_out.config)
                .map_err(|e| format!("sing-box 配置序列化失败: {e}"))?;
            std::fs::write(config_path, &json)
                .map_err(|e| format!("写 sing-box 配置失败 {}: {e}", config_path.display()))?;

            // 核解析不到（首启未落核 / 单测未注入）→ 闸门无从判定，照原样下发（failOpen）。
            let Some(bin) = binary else {
                return Ok(GateOutcome::assemble(
                    gen_out, effective, peeled, checks_run, None,
                ));
            };
            checks_run += 1;
            let verdict = run_config_check(bin, config_path).await;
            let rejection = match decide_peel(&verdict, started.elapsed(), PEEL_TIME_BUDGET) {
                PeelStep::Proceed => {
                    return Ok(GateOutcome::assemble(
                        gen_out, effective, peeled, checks_run, None,
                    ))
                }
                PeelStep::Stop(why) => {
                    log::warn!("起核内核闸门停止剥离（放行到 spawn，由内核自己报错）：{why}");
                    return Ok(GateOutcome::assemble(
                        gen_out, effective, peeled, checks_run, None,
                    ));
                }
                PeelStep::Peel(r) => r,
            };

            // 下标 → tag → 节点 id。tag→id 反表必须由**本轮**那份 `effective.servers` 现算：
            // `build_id_to_tag_map` 的撞名去重会让 tag 随集合变化（剥掉「HK」后，原本的「HK (1)」
            // 就变成「HK」），拿上一轮的表查这一轮的 tag 会张冠李戴。
            let wrappers: Vec<ServerLikeRef> =
                effective.servers.iter().map(ServerLikeRef).collect();
            let tag_to_id: BTreeMap<String, String> = build_id_to_tag_map(&wrappers)
                .into_iter()
                .map(|(id, tag)| (tag, id))
                .collect();
            // `classify_peel_target` 是纯函数，刻意只认「哪些 id 已剥」这个最小输入，不认
            // `InvalidNode`——上报形态是编排层的事。这里给它一个由 `peeled` 现导的视图，
            // 而不是在别处另存一份 id 集合（另存的那份迟早与 `peeled` 漂）。
            let already_peeled: BTreeSet<String> = peeled.keys().cloned().collect();
            match classify_peel_target(
                &rejection,
                &gen_out.config,
                &tag_to_id,
                user_config.selected_server_id.as_deref(),
                &already_peeled,
            ) {
                PeelTarget::Unattributable => {
                    log::warn!(
                        "起核内核闸门：内核拒收但该下标不对应任何节点，放行到 spawn —— {}",
                        rejection.detail
                    );
                    return Ok(GateOutcome::assemble(
                        gen_out, effective, peeled, checks_run, None,
                    ));
                }
                PeelTarget::Stalled { tag } => {
                    log::warn!(
                        "起核内核闸门：节点「{tag}」已剥除却仍被内核点名，停止剥离并放行 —— {}",
                        rejection.detail
                    );
                    return Ok(GateOutcome::assemble(
                        gen_out, effective, peeled, checks_run, None,
                    ));
                }
                PeelTarget::Blocked { id, tag } => {
                    log::error!(
                        "起核内核闸门：内核拒收的正是选中节点「{tag}」（id={id}）→ 终态，不 spawn —— {}",
                        rejection.detail
                    );
                    let blocked = InvalidNode {
                        id,
                        tag,
                        reason: INVALID_REASON_KERNEL_REJECTED.to_string(),
                    };
                    return Ok(GateOutcome::assemble(
                        gen_out,
                        effective,
                        peeled,
                        checks_run,
                        Some((blocked, rejection.detail)),
                    ));
                }
                PeelTarget::Peel { id, tag } => {
                    log::warn!(
                        "起核内核闸门：内核拒收节点「{tag}」（id={id}），已剔除并上报，其余节点照常起核 —— {}",
                        rejection.detail
                    );
                    // 剥除集合与上报清单是**同一次插入**：分成两个容器写就会漂，而漂的方向恰好是
                    // 「节点从配置里消失、用户侧却没有任何标记」——本仓明文判定它比报错更坏。
                    peeled.insert(
                        id.clone(),
                        InvalidNode {
                            id,
                            tag,
                            reason: INVALID_REASON_KERNEL_REJECTED.to_string(),
                        },
                    );
                }
            }
        }
    }

    /// 本次实际要启动的核心旁是否有 cronet 动态库。
    ///
    /// 必须与 [`Self::core_binary_for_start`] 同源：环境覆盖、可写核、随包核三条优先级任一变化时，
    /// 依赖探测都跟着实际 spawn 路径走，不能再固定查配置目录根部。
    fn cronet_lib_exists_for_start(&self) -> bool {
        self.core_binary_for_start()
            .ok()
            .is_some_and(|core| cronet_lib_exists_beside_core(&core, std::env::consts::OS))
    }
}

/// [`ProxyRuntime::generate_and_gate`] 的产物：**已落盘**的那份配置 + 本次全部剔除报告。
struct GateOutcome {
    config: SingBoxConfig,
    pruned_rule_set_tags: Vec<String>,
    /// 生成侧 gate 剔除的 ∪ 内核闸门剥掉的（走同一条 `EVENT_PROXY_INVALID_NODES` 通道）。
    invalid_nodes: Vec<InvalidNode>,
    /// 本次真跑了几次 `sing-box check`（健康路径恒 1；日志用，不参与判定）。
    checks_run: u32,
    /// `Some` = 被内核拒收的正是用户选中的节点（附内核原话）→ 调用方落终态错误，不 spawn。
    blocked: Option<(InvalidNode, String)>,
    /// 🔴 **`config` 真正由哪一份 servers 生成** —— 剥除之后的那份，不是调用方手里的 `user_config`。
    ///
    /// 为什么必须带出来：`build_id_to_tag_map` 按**名字**去重、撞名追加 `(n)` 后缀 ⇒ tag 是
    /// **整个集合**的函数，不是单个节点的函数。剥掉「HK」之后，原本的「HK (1)」在重新生成的配置里
    /// 就叫「HK」。而起核后有三处要按 id 反算 tag：
    ///   `attest_selected_exit`（出口自证，`code::EXIT_MISMATCH` 是「以为走代理、实则明文直连」的
    ///   唯一告警通道）、`build_switch_snapshot`（规则热切的 PUT 目标）、`ts_tag_to_id`（TS 帧逆映射）。
    /// 这三处若拿未剥的全量 servers 算，得到的 tag 在运行核里**根本不存在** ⇒ 出口完全正确却误报
    /// EXIT_MISMATCH、热切 PUT 打空、TS 端点认不出来。
    ///
    /// 所以「剥后集合」只在这里构造一次、由调用方原样接手，**不给第二处重算的机会** ——
    /// 重算出来的第二份判据迟早与这里漂移，而漂移的表现是静默的假告警。
    effective_user_config: UserConfig,
}

impl GateOutcome {
    /// 把「生成侧 gate 的剔除」+「内核闸门剥掉的」+「被拒的选中节点」并成一份上报清单。
    ///
    /// **`blocked` 那一个也必须进 `invalid_nodes`**：它是本次唯一让起核失败的节点，用户最需要看见的
    /// 就是它。只放进 `blocked`（终态错误文案）而不进上报清单，卡片就不会标灰 —— 而 toast 会消失、
    /// 卡片不会，持久的可视标记正是用户回头去修那个节点时唯一还在的线索。
    ///
    /// tooltip 文案（`servers.nodeInvalid`「节点配置无效，已在启动时跳过」）对这一条略有偏差 ——
    /// 本次是**整个没启动**、而非「启动时跳过了它」。取「标灰 + 略偏的后半句」而非「不标灰」：
    /// 前者的错处只在措辞，后者丢的是「哪个节点坏了」这个唯一可行动信息。
    ///
    /// 独立成关联函数而非循环里的闭包：闭包会在整个循环体上按引用捕获 `checks_run` / `peeled`，
    /// 与随后的 `checks_run += 1` / `peeled.insert` 直接借用冲突（编译器实测拦下）。
    ///
    /// `peeled` 直接当上报清单用（而不是另攒一份 `Vec<InvalidNode>`）：二者本就是同一件事的两种
    /// 表示，分开存就会漂 —— 起核重试腿第一次踩的正是这个（剥除集合跨腿累积、上报清单每腿新建
    /// ⇒ 第 2 腿 emit 一份空数组，节点仍被剥出配置而卡片上的标灰被前端整表替换掉了）。
    fn assemble(
        outcome: GenerateOutcome,
        effective_user_config: UserConfig,
        peeled: &BTreeMap<String, InvalidNode>,
        checks_run: u32,
        blocked: Option<(InvalidNode, String)>,
    ) -> Self {
        Self {
            config: outcome.config,
            pruned_rule_set_tags: outcome.pruned_rule_set_tags,
            invalid_nodes: outcome
                .invalid_nodes
                .into_iter()
                .chain(peeled.values().cloned())
                .chain(blocked.iter().map(|(n, _)| n.clone()))
                .collect(),
            checks_run,
            blocked,
            effective_user_config,
        }
    }
}

/// `build_id_to_tag_map` 要的最小投影（同 `generate.rs` 内部那份 `SrvLike`：`ServerConfig` 本身没实现
/// `ServerLike`，两处都得薄包一层）。
struct ServerLikeRef<'a>(&'a ServerConfig);

impl ServerLike for ServerLikeRef<'_> {
    fn id(&self) -> &str {
        &self.0.id
    }
    fn name(&self) -> &str {
        &self.0.name
    }
}

/// 内核点名一项之后，闸门对它的处置。[`classify_peel_target`] 的产物。
#[derive(Debug, Clone, PartialEq, Eq)]
enum PeelTarget {
    /// 剥掉这个节点、上报、再来一轮。
    Peel { id: String, tag: String },
    /// 它是用户**选中**的节点 → 不剥，落终态错误（理由见 [`classify_peel_target`]）。
    Blocked { id: String, tag: String },
    /// 已剥过却又被点名 = 剥了没生效 → 停（推进不变式）。
    Stalled { tag: String },
    /// 下标不对应任何节点 → 停（fail-open）。
    Unattributable,
}

/// 内核点名的下标 → 闸门该拿它怎么办。**纯函数**（不改 `already_peeled`，不碰进程/FS/时钟；
/// 单测直接喂结构体，不需要核也不需要落盘）。
///
/// 三条判据的**顺序是语义的一部分**，不可换：
///
/// 1. **先归因**：连是哪个节点都说不出，后两条无从谈起。
/// 2. **再判选中**：选中节点必须在「剥」之前被拦下 —— 一旦先剥了，`servers` 里就没有它，下一轮
///    `generate_sing_box_config_with_report` 直接返回 `Selected server not found`，用户拿到的又是
///    一句和现场无关的话。
/// 3. **最后判推进**：只有确定「该剥、且能剥」了，才问「上一轮是不是已经剥过它」。
///
/// # 为什么选中节点不静默剥掉换一个
///
/// 剥了就等于替用户改出口。而「实际生效出口 ≠ 选中节点」在本仓是**要专门告警**的一类事故
/// （[`code::EXIT_MISMATCH`]，见其文档：「用户以为走代理、实则明文直连」的唯一告警通道）——
/// 闸门自己去制造那个状态是自相矛盾。故落终态：用户看到的是「哪个节点、内核说了什么」，
/// 比今天那句无从下手的「启动失败」严格更好，且他的出口选择没有被人背着改掉。
fn classify_peel_target(
    rejection: &KernelRejection,
    config: &SingBoxConfig,
    tag_to_id: &BTreeMap<String, String>,
    selected_server_id: Option<&str>,
    already_peeled: &BTreeSet<String>,
) -> PeelTarget {
    let Some((id, tag)) = attribute_rejected_node(rejection, config, tag_to_id) else {
        return PeelTarget::Unattributable;
    };
    if selected_server_id == Some(id.as_str()) {
        return PeelTarget::Blocked { id, tag };
    }
    if already_peeled.contains(&id) {
        return PeelTarget::Stalled { tag };
    }
    PeelTarget::Peel { id, tag }
}

/// 把内核点名的「第几项」翻回「哪个节点」。**纯函数**。
///
/// 返回 `None` 的三种情形，调用方一律 fail-open：
/// 1. 下标越界（内核与我方对同一份 JSON 的编号不该错位，真错位了说明前提已失效 —— 此时猜比不猜坏）；
/// 2. 该项是内置出站（`direct` / `block` / `proxy-selector`）—— 它们不在 `id_to_tag` 里，也没有节点可剥；
/// 3. 该项是**由节点派生但不等于节点**的出站，典型是 shadowTLS 后处理造出的外层 `stls-out-<id>`。
///    刻意**不**按 `stls-out-` 前缀反解：那等于把 `outbounds.rs` 的命名约定抄第二份，命名一改这里就
///    悄悄失效（而且是「静默剥错节点」这种最难查的失效），不如老实归到「归因不到」。
fn attribute_rejected_node(
    rejection: &KernelRejection,
    config: &SingBoxConfig,
    tag_to_id: &BTreeMap<String, String>,
) -> Option<(String, String)> {
    let tag = match rejection.array {
        RejectedArray::Outbounds => &config.outbounds.get(rejection.index)?.tag,
        RejectedArray::Endpoints => &config.endpoints.as_ref()?.get(rejection.index)?.tag,
    };
    Some((tag_to_id.get(tag)?.clone(), tag.clone()))
}

/// 规则资源目录（`<data>/rule-resource/`）。**目录名的唯一定义点** —— config 生成侧的
/// `rule_resources_path` 与 decoy 覆盖清单都取这里，各写一遍字面量就是第二份真值源。
fn rule_resource_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("rule-resource")
}

/// 核 stderr 转发腿 ⇄ `SubscribeLog` 流的交接闸。
///
/// `false` = 流还没活，stderr 那条腿负责把核日志喂进 sink；`true` = 流已收到首帧并接管，
/// stderr 腿只保留 FATAL 分类、**不再转发**（否则直起腿每行会进两遍环形缓冲）。
type CoreLogHandoff = Arc<AtomicBool>;

/// helper 启动日志的会话边界。`identity=None` 表示起核前没有可识别的 current 文件。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StartupLogCursor {
    offset: u64,
    identity: Option<u128>,
}

/// 判定本次 helper 起核日志从哪里开始读。
///
/// 兼容两代 helper：旧版在同一文件 append，新版 fresh-rotate 后 current 身份变化。单看长度不能区分
/// 「旧文件继续增长」和「新会话写得比旧文件更长」，故只有身份相同且未缩短时才沿用旧偏移。
fn startup_log_read_start(
    cursor: StartupLogCursor,
    current_len: u64,
    current_identity: Option<u128>,
) -> u64 {
    if cursor.identity.is_some()
        && cursor.identity == current_identity
        && current_len >= cursor.offset
    {
        cursor.offset
    } else {
        0
    }
}

/// 文件身份只用于区分 helper 日志轮转前后的 current，不参与持久化或安全判定。
#[cfg(unix)]
fn log_file_identity(metadata: &std::fs::Metadata) -> Option<u128> {
    use std::os::unix::fs::MetadataExt;
    Some((u128::from(metadata.dev()) << 64) | u128::from(metadata.ino()))
}

#[cfg(windows)]
fn log_file_identity(metadata: &std::fs::Metadata) -> Option<u128> {
    use std::os::windows::fs::MetadataExt;
    Some(u128::from(metadata.creation_time()))
}

#[cfg(not(any(unix, windows)))]
fn log_file_identity(_metadata: &std::fs::Metadata) -> Option<u128> {
    None
}

/// 子进程 stdout/stderr → 日志 sink（`logging.rs` 的 `log::Log` 实现）+ 起核期 FATAL 真因分类。
///
/// # 本腿现在只覆盖「起核期」，但**不可删**
///
/// 核就绪后的日志已改由管理 API 的 `SubscribeLog` 流承担（结构化级别、全级别、不受 `log.level`
/// 过滤）。但那条流盖不住起核期：核在 `StartStateStarted` 才 `AttachPlatformWriter`
/// （`service/api/server.go`），此前的每一行——**包括 #332 那类 TUN 装地址失败的 FATAL**——
/// 结构性地不在流里。那一段只有 stderr 这一条路。
///
/// 交接由 `handoff` 表达（见 [`CoreLogHandoff`]）：流一收到首帧就置位，本腿随即停止转发但
/// **继续跑 [`classify_core_fatal_line`]** —— 核可以在就绪之后仍以 `log.Fatal` 死掉，那条行同样
/// 只走 stderr（包级 `std` logger 的 writer 恒是 `os.Stderr`，见调用处注释）。
/// `handoff` 为 `None` = 本腿经 helper 起核、压根没有管道（helper 把核输出重定向进启动日志文件）。
///
/// 逐行转发，不做正则脱敏（上游的 PRIVATE_IP_PATTERNS 过滤属 LogManager 批次）。
///
/// **级别按行内容判，不按流判**：sing-box 把 INFO/WARN/FATAL **全写 stderr**（实测），
/// 故「stderr ⇒ warn」会把满屏正常 INFO 谎报成 warn；反过来「stderr ⇒ info」又会让
/// `POLARIS_LOG=warn` 的用户丢掉核的 FATAL。取行内自带的级别 token 做映射（见 [`singbox_line_level`]）。
/// 这套「按字符串猜级别」只服务起核期这一小段——就绪后的级别由核经 gRPC 结构化给出，不再猜。
///
/// **#332**：`fatal` 非空时，同一条已判过级别的行再过一次
/// [`classify_core_fatal_line`]，命中就把结构化真因落进槽里 —— 转发与分类**共用一次级别判定**，
/// 不在旁边并排再起一套行解析。槽由起核腿在失败时读走（见
/// [`observe_core_fatal`](ProxyRuntime::observe_core_fatal)）。
fn pipe_to_log<R>(stream: Option<R>, fatal: Option<CoreFatalSlot>, handoff: Option<CoreLogHandoff>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(stream) = stream else { return };
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let level = singbox_line_level(&line);
            // 已交接给 SubscribeLog 流 → 不转发（分类照跑：就绪后的 log.Fatal 仍只走 stderr）。
            if !handoff.as_ref().is_some_and(|h| h.load(Ordering::SeqCst)) {
                log::log!(target: crate::logging::SING_BOX_TARGET, level, "{line}");
            }
            let Some(slot) = fatal.as_ref() else { continue };
            let Some(kind) = classify_core_fatal_line(&line, level) else {
                continue;
            };
            // **首个命中为准**：核在 FATAL 之后可能还吐一串收尾错误，后来的更泛化，覆盖会稀释真因。
            if let Ok(mut g) = slot.lock() {
                g.get_or_insert(kind);
            }
        }
    });
}

/// 核侧 `LogLevel`（七档、0=PANIC 最严重）→ 本仓 sink 的 `log::Level`（五档）。
///
/// panic/fatal 无对应档，归 `Error`（本层最高档，与 [`crate::logging`] 的 `parse_level` 对 `fatal`
/// 的处置同口径）。**未知级号归 `Info` 而不是丢弃**：上游扩枚举时宁可级别偏保守，也不能把一行核日志
/// 静默吃掉——日志页是排障的最后一根线。
fn core_log_level(raw: i32) -> log::Level {
    use polaris_singbox_grpc::daemon::LogLevel;
    match LogLevel::try_from(raw) {
        Ok(LogLevel::Panic | LogLevel::Fatal | LogLevel::Error) => log::Level::Error,
        Ok(LogLevel::Warn) => log::Level::Warn,
        Ok(LogLevel::Info) => log::Level::Info,
        Ok(LogLevel::Debug) => log::Level::Debug,
        Ok(LogLevel::Trace) => log::Level::Trace,
        Err(_) => log::Level::Info,
    }
}

/// 隐私锁下的核日志级别下限（纯函数）。比它更啰嗦的行一律丢弃。
///
/// # 这不是「再加一道保险」，是 `SubscribeLog` 亲手打开的一个新口子
///
/// 隐私锁此前把连接明细挡在盘外，靠的是**生成侧**把核的 `log.level` 抬到 ≥warn
/// （`config-engine::user_config::LogLevel::effective`）—— 核自己就不写 info/debug，自然也没什么可漏。
/// 但 `SubscribeLog` **不受 `log.level` 约束**（喂它的 platform writer 分发无级别过滤），核照样把
/// 每一条 debug/trace 推过来。若照单转发，隐私锁开着而 `config.logLevel=debug` 时，用户访问的域名
/// 会经本仓自己的 sink 落进 `polaris.log`（那份**不脱敏**；UI 上的脱敏只管显示，管不到磁盘）——
/// 隐私锁在生成侧堵住的那条路，就从这条新流上原样漏了回来。
///
/// 故此处复用**同一条判据**（`LogLevel::effective(privacy)` 抬到 warn），把它落在转发口上。
/// 判据只有一份，两侧不会各自漂。
fn core_log_privacy_floor(privacy: bool) -> log::Level {
    use polaris_config_engine::user_config::LogLevel;
    // `log::Level` 的 Ord 是「越啰嗦越大」（Error < Warn < Info < Debug < Trace），故下限取 Warn
    // 即表示「比 Warn 啰嗦的都丢」；非隐私态取 Trace = 不设限。
    match LogLevel::Debug.effective(privacy) {
        LogLevel::Debug => log::Level::Trace, // 未抬级 ⇒ 非隐私态 ⇒ 不设限
        _ => log::Level::Warn,
    }
}

/// 一条核日志帧转不转发（纯函数）：隐私锁下限 ∧ 用户级别上限，两道闸**都**得过。
///
/// # 为什么级别上限要在这里再判一次
///
/// 下游 `log::log!` 本来就会按 `log::max_level()` 筛，所以这道闸**不改变任何一行的去留** ——
/// 它改变的是**筛之前干了多少活**。`SubscribeLog` 恒推全级别（含 trace），而用户常年停在 info：
/// 每一条注定被丢掉的 debug/trace 行，此前都要先付一遍 [`strip_core_log_decoration`] 的代价 ——
/// 而喂这条流的 formatter **没关色**，于是 `strip_ansi` 必然走到分配分支，加上末尾的 `to_string()`，
/// **每条被丢掉的行两次堆分配 + 两趟字符扫描**。核一忙（debug 档的路由/DNS 决策是每连接若干行）
/// 这就是一条常态空转的流水线。
///
/// 判据合成一处而不是散在调用点，是为了让它能被单独变异验证：两道闸各自的短路都有对应用例
/// （见 `core_log_admits_*`）。
///
/// 上限取 `log::max_level()` 的**当次读数**：它由 `logging::set_level` 跟着 `config.logLevel` 走，
/// 与本函数之外的那次 `log::log!` 之间存在窗口 —— 无所谓，级别变更本就没有「精确到某一行」的语义。
fn core_log_admits(level: log::Level, floor: log::Level, max: log::LevelFilter) -> bool {
    // `log::Level` 的 Ord 是「越啰嗦越大」；`Level <= LevelFilter` 是 log crate 提供的跨类型比较。
    level <= floor && level <= max
}

/// `Log.Message.message` 的装饰剥除（纯函数）：ANSI 色码 + 冗余的 `LEVEL[nnnn] ` 前缀。
///
/// # 为什么必须剥
///
/// 喂 `SubscribeLog` 的是 logFactory 的 **platformFormatter**，而它构造时**没关色**
/// （`log/observable.go`：`Formatter{BaseTime: …, DisableLineBreak: true}`，`DisableColors` 取默认
/// `false`，紧邻那段关色的代码是被注释掉的），且走的是 `Format` 的默认时间戳分支
/// （`levelString + "[" + xd(启动至今秒数, 4) + "] " + message`）。于是每条消息实际长这样：
///
/// ```text
/// "\x1b[36mINFO\x1b[0m[0012] router: loaded 5 rules"
/// ```
///
/// 不剥的话，日志页每行会显示成 `INFO: <ESC>[36mINFO<ESC>[0m[0012] router: …` —— 转义序列以乱码
/// 呈现，级别还重复一遍（结构化 `level` 字段已经承担了级别，UI 自己渲染）。
///
/// # 剥不掉就原样返回
///
/// 前缀形状对不上（上游改了 formatter / 消息本身以别的东西开头）→ **整段原样保留**。
/// 剥除是显示层的清理，绝不能演变成「看起来不像我预期的行就被吃掉一半」。
fn strip_core_log_decoration(msg: &str) -> String {
    let plain = strip_ansi(msg);
    strip_level_prefix(&plain).to_string()
}

/// 去掉 ANSI CSI 序列（`ESC [ … <字母>`）。无 `ESC` → 原样借用，不分配。
fn strip_ansi(s: &str) -> Cow<'_, str> {
    if !s.contains('\u{1b}') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // ESC 后若不是 '['，不是 CSI（不认识）→ 连同 ESC 一起丢，后续原样保留。
        if chars.as_str().starts_with('[') {
            chars.next();
            // CSI 以 0x40..=0x7E 的字节收尾（色码恒是 'm'）。
            for t in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&t) {
                    break;
                }
            }
        }
    }
    Cow::Owned(out)
}

/// 去掉行首那截 `LEVEL[nnnn] `（`nnnn` = 核启动至今的秒数，`log/format.go` 的 `xd(…, 4)`）。
/// 形状对不上 → 原样返回。
fn strip_level_prefix(s: &str) -> &str {
    const LEVELS: [&str; 7] = ["PANIC", "FATAL", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"];
    let Some(lv) = LEVELS.iter().find(|l| s.starts_with(**l)) else {
        return s;
    };
    let rest = &s[lv.len()..];
    let Some(rest) = rest.strip_prefix('[') else {
        return s;
    };
    let Some(close) = rest.find(']') else {
        return s;
    };
    // 方括号内必须全是数字（`xd` 产出的是零填充秒数）——不是就说明形状变了，别乱剥。
    if rest[..close].is_empty() || !rest[..close].bytes().all(|b| b.is_ascii_digit()) {
        return s;
    }
    rest[close + 1..]
        .strip_prefix(' ')
        .unwrap_or(&rest[close + 1..])
}

/// 核 stderr 里可结构化的**启动真因**（#332）。
///
/// 只收录「核自己说清楚了、且我方能给出不同用户动作」的那几类。**判据是内核源码里的字面量**，
/// 不是我方 message 的关键字 —— 后者就是 [`code`] 模块头注说的「猜 message = 伪造分类」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoreFatalKind {
    /// 给 TUN 网卡装地址这一步失败（地址被占 / 系统拒绝分配）→ [`code::TUN_ADDRESS_UNAVAILABLE`]。
    TunAddressUnavailable,
}

/// [`CoreFatalKind`] 的跨任务投递槽：`pipe_to_log` 的转发任务写、起核腿失败时读。
type CoreFatalSlot = Arc<Mutex<Option<CoreFatalKind>>>;

/// 核 stderr 单行 → 结构化真因（纯函数；`level` 由既有的 [`singbox_line_level`] 给，本函数不另判级别）。
///
/// # 取证（2026-08-05 实取，随包核 `resources/linux/sing-box` = 1.14.0-beta.7）
///
/// 命中链路（自外向内，`E.Cause` 以 `": "` 拼接）：
///
/// ```text
/// FATAL start service: initialize inbound/tun[...]: configure tun interface: set ipv4 address: <errno 文案>
///       ^cmd_run.go:168                            ^protocol/tun/inbound.go:438  ^sing-tun/tun_windows.go:81
/// ```
///
/// - `configure tun interface` —— sing-box `protocol/tun/inbound.go:438`（`E.Cause(err, "configure tun interface")`，
///   `tun.New` 的唯一包装点）。**已在随包二进制里逐字验到**（`strings resources/linux/sing-box` 命中）。
/// - `set ipv4 address` / `set ipv6 address` —— sing-tun `tun_windows.go:81` / `:102`
///   （`luid.SetIPAddressesForFamily` 失败的包装串；`SetIPAddressesForFamily` → `AddIPAddress` →
///   `CreateUnicastIpAddressEntry`，地址已被别的网卡占用即在此失败）。**Windows-only 文件，随包的
///   linux 核里查不到**（build tag 排除，`strings` 实测 0 命中）—— 证据取自 sing-tun 源码，不是猜的，
///   但也**没有**在二进制里对到字面量，这是本条匹配面唯一的取证缺口。
/// - `add address ` —— sing-tun `tun_linux.go:145` / `:154`（Linux 侧同一件事的包装串；
///   同理不在 Windows 核里）。收进来是因为本函数跨平台共用，Linux TUN 撞地址冲突时该给同一个码。
/// - macOS：`tun_darwin.go` 里**没有**对应的地址设置包装串（地址随 `SIOCAIFADDR` 一并设，失败走裸
///   errno），故 mac 侧本判定天然不命中 —— 不硬凑一个猜出来的 token 冒充覆盖。
///
/// # 为什么**不**匹配 errno 文案（"already exists" / "file exists"）
///
/// Windows 侧那截尾巴是 `syscall.Errno.Error()` 经 `FormatMessage` 生成的，**跟随系统语言**
/// （中文系统上是「对象已存在。」）。拿它做判据 = 判定在中文/俄文 Windows 上静默失效，而那正是
/// 用户最多的那批机器。上面三个 token 全是 Go 源码里的 ASCII 字面量，与系统语言无关。
///
/// 代价：判据比「地址冲突」宽 —— 装地址这一步的**任何**失败都会归到本码。这是有意的取舍：
/// 该步失败的现实成因几乎全是「地址被占/装不上」，且给出的指引（断开其他 VPN、重启清残留网卡）
/// 对这一整类都成立；而收窄到 errno 文案的代价是对非英文系统全盲。
fn classify_core_fatal_line(line: &str, level: log::Level) -> Option<CoreFatalKind> {
    // 只看错误档（FATAL/ERROR）。正常 INFO 行里出现这些词只可能是别人的日志噪音。
    if level != log::Level::Error {
        return None;
    }
    // 外层包装必须在：单看 `add address` 会把任何提到该词的行都算上。
    if !line.contains("configure tun interface") {
        return None;
    }
    const ADDRESS_STEP_TOKENS: &[&str] = &["set ipv4 address", "set ipv6 address", "add address "];
    if ADDRESS_STEP_TOKENS.iter().any(|t| line.contains(t)) {
        return Some(CoreFatalKind::TunAddressUnavailable);
    }
    None
}

/// 文本块（helper 起核时核 stderr 被重定向进的启动日志片段）→ 首个命中的真因。
///
/// 逐行复用 [`classify_core_fatal_line`]（级别同样经 [`singbox_line_level`]），与管道那条腿**同一判据**。
fn scan_core_fatal(text: &str) -> Option<CoreFatalKind> {
    text.lines()
        .find_map(|line| classify_core_fatal_line(line, singbox_line_level(line)))
}

/// 启动日志一次最多回扫的字节数（#332）。核 FATAL 恒在**末尾**（`log.Fatal` 之后进程即退出），
/// 故上限截掉的是它前面的 debug 噪音，不是真因本身。
const CORE_FATAL_SCAN_BYTES: u64 = 64 * 1024;

/// 从 `offset` 起读至多 `max_bytes`（读不到/读不动一律 `None`，best-effort 诊断绝不阻断主流程）。
///
/// **为什么不复用 `commands/misc.rs::read_tail`**：那个是「取文件**末尾** N 字节」，语义上没有起点，
/// 拿它扫启动日志会把上一次会话遗留的 FATAL 一并扫进来（文件是 append 的）——本函数存在的全部理由
/// 就是那个起点。此外它私有、且失败时返回「(读取失败: …)」这类给人看的占位串，判定链路要的是 `None`。
fn read_file_range(path: &Path, offset: u64, max_bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    f.take(max_bytes).read_to_end(&mut buf).ok()?;
    // 核日志恒是 UTF-8；lossy 只为杜绝「日志里一个坏字节 = 整条诊断链路失效」。
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// 起核终态的码/文案收口（纯函数）：核给出了可诚实断言的真因就用真因的专属码，否则维持泛化
/// [`code::STARTUP_FAILED`]。
///
/// **`base_msg` 在有真因时被整句替换**而不是拼接：`base_msg` 是我方从控制流位置写下的话
/// （「起核超时」「启动期退出」），它描述的是**症状**；真因的文案描述的是**病因 + 下一步**。
/// 拼成一句只会得到「起核超时（管理 API 9090 …）：TUN 虚拟网卡地址无法分配…」这种把用户注意力
/// 引向前半句（无用）的句子。症状原样留在日志里，不进用户可见串。
fn settle_start_failure(base_msg: String, fatal: Option<CoreFatalKind>) -> (String, &'static str) {
    match fatal {
        Some(CoreFatalKind::TunAddressUnavailable) => (
            TUN_ADDRESS_UNAVAILABLE_MSG.to_string(),
            code::TUN_ADDRESS_UNAVAILABLE,
        ),
        None => (base_msg, code::STARTUP_FAILED),
    }
}

/// sing-box 日志行 → `log::Level`（行内自带的级别 token）。
///
/// **DEBUG/TRACE 必须单独认**：此前它们落进 else 分支被打成 `info`，于是日志页把级别调到 DEBUG 时，
/// 核的 DEBUG 行早已伪装成 info 混在里面——既没法按 DEBUG 筛出来，也让「调到 INFO 就该看不见 DEBUG」
/// 失效（DEBUG 噪音在 INFO 档全量泄漏）。级别过滤要有意义，标级别就必须如实。
///
/// 按严重度**从高到低**匹配：一行只取最先命中的 token（sing-box 的行格式是 `+0800 INFO xxx`，
/// 级别 token 在正文前，正文里再出现别的 token 属噪音，取高档是安全侧——宁可留下也不误丢）。
fn singbox_line_level(line: &str) -> log::Level {
    if line.contains("FATAL") || line.contains("ERROR") {
        log::Level::Error
    } else if line.contains("WARN") {
        log::Level::Warn
    } else if line.contains("DEBUG") {
        log::Level::Debug
    } else if line.contains("TRACE") {
        log::Level::Trace
    } else {
        log::Level::Info
    }
}

/// config-engine 子 builder 日志回调（`fn(LogLevel, &str)` 裸函数指针，不可捕获）。
///
/// **级别必须由调用方给**：此前签名无 level、恒 `log::info!` → 「规则资源缺少本地副本」这类降级告知
/// 被日志级别过滤直接吞掉（真机 2026-07-20：全量明文直连，日志里唯一线索只剩 `rule_set=0`）。
/// 对齐 上游 `deps.log('warn', …)`。
fn config_log(level: polaris_config_engine::user_config::LogLevel, msg: &str) {
    use polaris_config_engine::user_config::LogLevel;
    let lv = match level {
        LogLevel::Debug => log::Level::Debug,
        LogLevel::Info => log::Level::Info,
        LogLevel::Warn => log::Level::Warn,
        // config-engine 侧无 panic 档；fatal 映射到 error（本层最高档）。
        LogLevel::Error | LogLevel::Fatal => log::Level::Error,
    };
    log::log!(target: "config-engine", lv, "{msg}");
}

/// config-engine customRuleFiles 降级回调（外化规则文件缺失 → 回落 inline 生成）。
fn config_on_degraded() {
    log::warn!(target: "config-engine", "自定义规则外化文件不可用 → 回落 inline 生成");
}

/// **C12**：枚举本机**所有非回环接口**的连接网段（CIDR，含主机位）——注入 `buildInbounds own_lan_cidrs`。
///
/// = 上游 `getOwnLanCidrs`（`singbox-inbounds-builder.ts:57-69`）的 Rust 等价：Node 用
/// `os.networkInterfaces()` 取 `!internal && cidr` dedupe，Rust 用 `getifaddrs` 拿 addr+netmask 分离态，
/// netmask→prefix / 格式化 / dedupe / 滤回环的**纯逻辑**下沉 config-engine `own_lan`（确定性单测），
/// 本函数只做 I/O 枚举。
///
/// **只读 `getifaddrs` 系统调用，非破坏性**（不改路由 / iptables / 网络接管，非宿主网络禁区）。best-effort：
/// 取不到接口 / 掩码非法 → 跳过（对齐 上游的 catch→空，「宁漏排也不误破」）。
///
/// 消费面：macOS「连入来源排除」guard（排除物理 LAN 会触发 NE 反向路由丢包）、Windows bypassLAN carve
/// guard（保护物理子网不被 mesh carve）、Linux mesh/own-lan 重叠告警。
#[cfg(unix)]
fn enumerate_own_lan_cidrs() -> Vec<String> {
    use nix::ifaddrs::getifaddrs;
    use nix::net::if_::InterfaceFlags;

    let Ok(addrs) = getifaddrs() else {
        // 枚举失败（罕见）→ 空（macOS guard 退化为不额外剔除，交真机验证兜底）。
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for ifa in addrs {
        let is_loopback = ifa.flags.contains(InterfaceFlags::IFF_LOOPBACK);
        let (Some(address), Some(netmask)) = (ifa.address, ifa.netmask) else {
            continue; // 无 addr 或无 netmask 的接口帧（如 AF_PACKET）跳过。
        };
        // IPv4：addr + netmask（u32，大端主机序）→ prefix → "addr/prefix"（含主机位）。
        if let (Some(a4), Some(m4)) = (address.as_sockaddr_in(), netmask.as_sockaddr_in()) {
            let prefix = own_lan_v4_addr_prefix(u32::from(a4.ip()), u32::from(m4.ip()));
            if let Some((ip, pfx)) = prefix {
                if let Some(cidr) = own_lan_cidr(&ip, pfx, is_loopback) {
                    out.push(cidr);
                }
            }
        } else if let (Some(a6), Some(m6)) = (address.as_sockaddr_in6(), netmask.as_sockaddr_in6())
        {
            // IPv6：SockaddrIn6::ip() → Ipv6Addr；netmask 同。
            if let Some(pfx) = polaris_config_engine::user_config::own_lan::prefix_from_netmask_v6(
                u128::from(m6.ip()),
            ) {
                if let Some(cidr) = own_lan_cidr(&a6.ip().to_string(), pfx, is_loopback) {
                    out.push(cidr);
                }
            }
        }
    }
    dedupe_own_lan(out)
}

/// v4 helper：addr(u32)+netmask(u32) → (点分地址串, prefix)。掩码非法 → None（best-effort 丢弃）。
#[cfg(unix)]
fn own_lan_v4_addr_prefix(addr: u32, mask: u32) -> Option<(String, u8)> {
    let pfx = polaris_config_engine::user_config::own_lan::prefix_from_netmask_v4(mask)?;
    Some((std::net::Ipv4Addr::from(addr).to_string(), pfx))
}

/// **C12**（Windows）：`GetAdaptersAddresses` 枚举单播地址 + `OnLinkPrefixLength`（`polaris_helper` 的
/// [`netinfo`] 模块，只读免提权），再喂**同一套** config-engine 纯逻辑（`own_lan_cidr` 滤回环 + 组串、
/// `dedupe_own_lan` 去重）——与 unix 腿结构逐条对称，判定逻辑单一真值、不复制。
///
/// **为何 FFI 在 `polaris-helper` 而不在此**：本文件 `#![forbid(unsafe_code)]`（`forbid` 不可被内层
/// `allow` 覆盖），unix 腿能写在这里是因为 `nix` 提供 `getifaddrs` 的 safe wrapper，Windows 侧依赖树
/// 里没有等价物。而 `polaris-helper` 已有 `windows-sys` 的 IpHelper feature **且已在调同一个
/// `GetAdaptersAddresses`**（`wintun::WinAdapterProbe`），`src-tauri` 也已依赖它 ⇒ 复用既有能力，
/// 不给 `src-tauri` 加 `windows-sys`（简约阶梯：workspace 里有等价能力就不再引一份）。
///
/// best-effort：枚举失败 / 前缀哨兵值 → 该条跳过（对齐 unix 腿与 上游 `getOwnLanCidrs` 的 catch→空）。
/// 消费面：Windows bypassLAN carve guard（保护物理子网不被 mesh carve）。
///
/// [`netinfo`]: polaris_helper::platform::windows::netinfo
#[cfg(windows)]
fn enumerate_own_lan_cidrs() -> Vec<String> {
    use polaris_helper::platform::windows::netinfo::enumerate_local_unicast_addrs;
    let out: Vec<String> = enumerate_local_unicast_addrs()
        .into_iter()
        .filter_map(|a| own_lan_cidr(&a.ip, a.prefix, a.is_loopback))
        .collect();
    dedupe_own_lan(out)
}

/// **C12**（既非 unix 也非 windows 的假想平台）：无枚举实现 → 空。与 上游 `getOwnLanCidrs` catch→空
/// 的 best-effort 语义一致（少一层物理子网保护，非破坏、不断网）。
#[cfg(not(any(unix, windows)))]
fn enumerate_own_lan_cidrs() -> Vec<String> {
    Vec::new()
}

/// **C7 用户开关**：原始 config JSON 的 `dnsConfig.takeoverSystemDns` 三态读取（纯函数）。
///
/// **为何从裸 JSON 读**：该字段不在 config-engine 的 `DnsConfig` 结构体里（前端契约
/// `ui/src/contracts/types.ts:324` 有、Rust 侧无建模），与 `restartOnNodeChange` / `autoSwitchNode` /
/// `meshLoginFallbackDirect` 同法 —— 不为一个纯运行期开关去改共享的配置结构体（那会波及 norm/生成/快照
/// 四条链，而它一条都不该影响：接管与否不改 sing-box config 一个字节）。
///
/// 返回**三态**而非 bool：调用方一律按 上游的 `!== false` 口径判（`Some(false)` 才算关），
/// 缺省与非布尔都等价于「未显式关」。若在此折成 bool，`None`（缺省=开）与 `Some(true)` 的区别就没了，
/// 下游想改默认方向时会误把「用户没表态」当成「用户选了开」。
fn dns_takeover_enabled(config: &Value) -> Option<bool> {
    config
        .get("dnsConfig")
        .and_then(|d| d.get("takeoverSystemDns"))
        .and_then(Value::as_bool)
}

/// 从原始 config JSON 读日志两轴（`logLevel` / `disableLogFile`），喂 `GenerateConfigDeps`。
///
/// **为何从裸 JSON 读**：`UserConfig` 增量子集未建模这两字段（见 `GenerateConfigDeps` 字段注释），
/// 与 `restartOnNodeChange`（switch_mode）同法从原始 `Value` 读，不经 `UserConfig` 结构体。
/// - `logLevel` 缺省 / 非法字符串 → `Info`（`LogLevel` 的 `#[default]`）。
/// - `disableLogFile` 非 `true` 一律 `false`（对齐 上游 `validateConfig` 布尔口径）。
///
/// 隐私抬级（`effective`）不在此：它由 `build_log_config` 按 `deps.privacy_mode` 处理，privacy 轴接线属 B1。
fn log_axes_from_config(config: &Value) -> (polaris_config_engine::user_config::LogLevel, bool) {
    let log_level = config
        .get("logLevel")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let disable_log_file = config
        .get("disableLogFile")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (log_level, disable_log_file)
}

// ── 曾经的「C9 诊断采集会话」（`diagnosticCapture`）已整体删除 ───────────────────────
//
// 它做的事是：临时把 `config.logLevel` 拉到 `debug`（快照原级别）→ 落盘 → 广播 → 重启运行核，
// 事后还原，外加一条启动期崩溃自愈。**存在的唯一理由**是「想看核的 debug 行就必须让核以 debug 跑」。
//
// 这个前提是错的：核的 `SubscribeLog` 流恒是全级别（喂它的 platform writer 分发不受 `log.level`
// 过滤，见 crate `polaris-singbox-grpc` 的 `subscribe_logs` 文档），级别筛在客户端。接上该流之后
// （[`ProxyRuntime::spawn_core_log_relay`]），把日志页级别拨到 debug 就**立刻**能看到核的 debug 行 ——
// 零磁盘写、零核重启、也就无所谓「还原」与「崩溃自愈」。故整条链（两个 command、三个纯函数、
// `BACKEND_AUTHORITATIVE_KEYS` 特例、备份排除位、前端采集条与按钮）一并撤掉；旧配置里残留的键由
// `polaris_store::migrate::migrate_diagnostic_capture` 还原级别后清除，不留孤儿键。

/// `.srs` 规则集有效性（上游 `isValidSrsFile`，builtin-geo-rulesets.ts:142）：
/// 读头 3 字节判魔数 `SRS`。任何 IO 失败 → false（fail-closed，缺文件时 builder 自行降级）。
fn is_valid_srs_file(path: &str) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 3];
    f.read_exact(&mut buf).is_ok() && &buf == b"SRS"
}

/// 平台标签：config-engine 沿用 上游/Node 约定（`linux` / `darwin` / `win32`），
/// 与 Rust 的 `std::env::consts::OS`（`linux` / `macos` / `windows`）**不同名** → 必须映射。
/// 漏映射会让 inbounds/route 的平台分支（如 `platform == "win32"`）全部落空。
fn platform_tag() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// **#327**：本次起核是否该做 wintun 适配器探测（纯谓词，供单测 + 变异）。
///
/// 唯一消费者是起核就绪后的存在性探测 [`ProxyRuntime::probe_tun_adapter_present`]。抽成独立谓词而不是
/// 内联进那个 `async fn`：判定本身与 Windows API、tokio 都无关，抽出来才跑得进本机单测（见下方平台入参）。
///
/// 两条都必须成立：
/// - **TUN 模式**：只有 TUN 会建 wintun 适配器；systemProxy/manual 根本不碰它，探了必然恒 `Absent`
///   —— 那不是白等，是把一次完全正常的起核判成失败。
/// - **Windows**：wintun 是 Windows 专属，`WinAdapterProbe` 枚举的也只是 Windows 适配器
///   （mac 用 utun、Linux 用 tun 设备，创建语义与命名谱系都不同，由各自的腿处理——mac 的双 utun
///   竞态走的是 `resolve_start_retry_budget` 放宽预算那条）。
///
/// 平台从 [`platform_tag`] 取（`win32`，Node 约定）而非 `cfg!(windows)`：让判定在**任何 host 上都可测**，
/// 而不是变成本机永远跑不到的 cfg 死代码（同 `resolve_start_retry_budget` 收平台入参的手法）。
#[must_use]
fn should_probe_wintun_adapter(mode: ProxyModeType, platform: &str) -> bool {
    mode.is_tun() && platform == "win32"
}

/// **#327**：一条起核腿对「TUN 适配器是否已建出」的观测结果（判定的**唯一输入**，不含运行期状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TunAdapterObservation {
    /// 枚举到了本次配置的适配器名。
    Present,
    /// 有界轮询内始终没枚举到。
    Absent,
    /// **不可断言**：非 TUN@Windows / 接口名不在可枚举前缀面内 / 枚举 API 报错 / 探测任务 join 失败。
    /// 一律按放行处理 —— 判据坏掉时误拦一次正常起核，比漏检一次假连接更糟（同
    /// [`ProxyRuntime::verify_tun_route_captured`] 的 `Indeterminate` 纪律）。
    Indeterminate,
}

/// **#327**：单条起核腿的 TUN 适配器判定（纯函数，形态对齐既有的 `classify_child_exit`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TunAdapterVerdict {
    /// 放行（见到适配器 / 不可断言）。
    Proceed,
    /// 本腿失败，但预算还有 → 计入重试预算，杀核后重来一腿。
    RetryLeg,
    /// 预算耗尽，且**整个起核过程中一次都没见过**适配器 → 终态 [`code::TUN_ADAPTER_MISSING`]。
    TerminalNeverAppeared,
    /// 预算耗尽，但**中途见过**适配器（建出来又消失/反复）→ 终态，但**不冒充**「网卡建不出来」。
    TerminalAfterFlap,
}

/// **#327**：起核就绪后的适配器存在性判定（纯函数：吃观测值 + 累积事实 + 预算，不现查任何运行期状态）。
///
/// # 为什么是「逐腿判 + 计入重试预算」，而不是就绪门里多一条判据，也不是循环后单次硬闸
///
/// - **不塞进就绪门**（`core-supervisor::wait_for_core_ready`）：那是纯轮询骨架、跨平台共用，塞一条
///   Windows 专属的网卡判据进去，等于让所有平台的就绪语义为一个平台的怪癖买单；且它拿不到本次
///   config 解出的接口名。
/// - **不学 `verify_tun_route_captured` 放到循环之后单次执行**：那条的失败是「他方 VPN 占路由」，
///   重试同一件事没有意义，故硬终止是对的。本条的失败恰恰**是重试能治的**——网卡挂载失败多为瞬态
///   （驱动/句柄尚未就位），而重试腿开头的 `kill_core` 会把这一次的核连同它半建的网卡一并清掉，
///   下一腿是干净的重来。放循环外 = 把一个可自愈的瞬态判成终态。
/// - **`ever_seen` 必须跨腿累积**：只看本腿会把「第 1 腿建出来了、第 3 腿抖没了」误报成
///   「wintun 根本建不出来」，把用户导向「重装驱动」这条错误的下一步。见过一次就永远不是那条结论。
///
/// 重试条件 `attempt <= max_retries` 与 Dead/Timeout 两腿逐字一致（预算的定义在
/// [`StartRetryBudget::max_retries`]：总尝试 = max_retries + 1）。
#[must_use]
fn classify_tun_adapter_leg(
    observation: TunAdapterObservation,
    ever_seen: bool,
    attempt: u32,
    max_retries: u32,
) -> TunAdapterVerdict {
    match observation {
        TunAdapterObservation::Present | TunAdapterObservation::Indeterminate => {
            TunAdapterVerdict::Proceed
        }
        TunAdapterObservation::Absent if attempt <= max_retries => TunAdapterVerdict::RetryLeg,
        TunAdapterObservation::Absent if ever_seen => TunAdapterVerdict::TerminalAfterFlap,
        TunAdapterObservation::Absent => TunAdapterVerdict::TerminalNeverAppeared,
    }
}

/// NaiveProxy 可用性判定（抽纯函数便于单测 + 变异验证）。`generate_deps` 的 `has_cronet` 经此。
///
/// **为什么不能只看 libcronet 落盘**（真机 bug 根因）：macOS 的 sing-box 二进制已把 cronet **静态编入**
/// （CGO + `with_naive_outbound`），naive 内核原生支持、**不需要动态库文件**。strings 二进制坐实
/// **mac-arm64 与 mac-x64 两架构都编入**：tags 逐字同含 `with_naive_outbound`，cronet 符号计数均 1588，
/// 二进制体积 73/78MB（远大于走动态库的 linux 70/win 71MB）。故 macOS 无 `libcronet.dylib` 时
/// `lib_exists=false`，但 naive 仍可用 —— 若只看文件会误判 `has_cronet=false` → `generate.rs` 的
/// `is_node_usable` 丢弃所有 naive 节点 + 报「macOS 核心未内置 cronet」。这是 上游 时代「naive 靠外部
/// libcronet」前提，换核后前提变了，判定必须跟上。
///
/// - macOS（`darwin`，arm64 与 x64 皆然）：静态编入 → true（不看文件；arch 不参与判定）。
/// - linux/win：看 libcronet 动态库落盘 `lib_exists`。
///
/// `arch` 目前不参与判定（macOS 两架构一致），保留入参把「(platform, arch)」两轴显式带进单测四象限，
/// 并为将来若某架构的核回退动态库时收窄留 seam。
fn cronet_available(lib_exists: bool, platform: &str, arch: &str) -> bool {
    let _ = arch;
    lib_exists || platform == "darwin"
}

/// 指定核心旁是否存在本平台的 cronet 动态库（路径纯函数在 `core_paths`，这里仅做 FS 探测）。
fn cronet_lib_exists_beside_core(core: &Path, os: &str) -> bool {
    crate::runtime::core_paths::core_sidecar_path_for(core, os).is_some_and(|p| p.is_file())
}

/// 崩溃自愈决策 seam（`run_crash_recovery` 与其单测共用）：读机内在途腿世代 → 喂 `handle_crash`。
///
/// **为什么抽 seam**：`run_crash_recovery` 是重 I/O（退避 sleep + 真起核 = 真机门），其「把在途世代喂给
/// `handle_crash` 而非 `None`」这条 wiring 无法零进程单测。抽成纯 seam 后，`drive_crash_decision_feeds_
/// real_inflight_gen`（proxy 测）可确定性验：把 `m.restarting_gen()` 换回 `None` → replay 恒 false → 转红。
fn drive_crash_decision(
    m: &mut CrashRecoveryMachine,
    now_ms: u64,
    current_generation: u64,
) -> AutoRestartOutcome {
    // M-2′-G1：真实在途腿世代（无在途腿 → None）。此前上层硬编码 None → 接管会话崩溃永不置补发标记。
    let inflight_gen = m.restarting_gen();
    m.handle_crash(now_ms, current_generation, inflight_gen)
}

/// 崩溃自愈重启失败是否「不可恢复」→ 立即终态放弃（不再空耗退避）。移植 上游
/// `isUnrecoverableRestartError`（:6039）。
///
/// **码优先，keyword 兜底 —— 两条腿都留，不是二选一**：
///
/// 1. **码腿（新）**：[`StartError::code`] 是判定点在**控制流位置**诚实断言出来的（见 [`code`] 模块
///    文档），比事后猜 message 关键字可靠。[`code::HELPER_GATE_ABORTED`]（用户刚亲口说了「不装」）与
///    [`code::HELPER_NOT_INSTALLED`]（前置条件缺失；非交互自愈下 `run_helper_gate` 连引导都不弹，
///    :1511-1514 直接落此码）两者**重试多少轮都不会自己变好**，故立即终态。
///
///    此前只有 keyword 腿时，这两条腿实际落在错误里的是中文文案 [`HELPER_GATE_ABORTED_MSG`] /
///    [`HELPER_NOT_INSTALLED_MSG`]，**不命中下方任何一个关键词**（"提权助手，"≠"提权助手不可用"，
///    "提权 helper"里也没有"权限"）⇒ helper 缺失/用户取消时崩溃自愈会白烧满 `MAX_RESTART_COUNT`(3)
///    轮退避才放弃。
///
/// 2. **keyword 腿（原）**：覆盖**没有码**、以及**有码但码本身不表达终态性**的错误形态。
///    **为什么有码也仍要走这条腿**（而不是 `if let Some(c) = code { return matches!(c, ...) }`）：
///    spawn launch 失败腿把**原始 OS 错误**格式化进 message 后贴 [`code::STARTUP_FAILED`]
///    （:1699-1702），EACCES 的 "Permission denied" 正是从那儿来的。若「有码即跳过 keyword」，权限
///    拒绝会退回「烧满 3 轮退避 ~22s」—— 正是 keyword 腿当初要修的那个缺陷。
///
/// 故本函数是既有行为的**严格超集**：只新增 `true`，绝不把原本 `true` 的判成 `false`。瞬态失败
/// （起核超时 / 启动期退出 / 端口资源竞态）两条腿都不命中 ⇒ 照常重试。
fn is_unrecoverable_restart_error(err: &StartError) -> bool {
    // 码腿：控制流位置诚实断言出的确定性终态。
    let coded_terminal = err
        .code
        .is_some_and(|c| c == code::HELPER_GATE_ABORTED || c == code::HELPER_NOT_INSTALLED);
    coded_terminal || is_unrecoverable_restart_message(&err.message)
}

/// [`is_unrecoverable_restart_error`] 的 message 关键字腿（权限/提权助手不可用/root 残留/clash_api
/// 端口占用等确定性失败，重试无意义）。CJK 字符 `to_lowercase` 为恒等（无大小写），ASCII 关键词经
/// 小写归一后匹配（如 "Permission denied"）。
fn is_unrecoverable_restart_message(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("权限")
        || m.contains("permission")
        || m.contains("helper_gate_aborted")
        || m.contains("提权助手不可用")
        || m.contains("提权助手引导")
        || m.contains("root_orphan_blocked")
        || m.contains("root 残留")
        || m.contains("clash_api_port_busy")
        || m.contains("clash_api 端口")
}

/// 起核重试预算（上游 start-retry-policy.ts `resolveStartRetryBudget`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StartRetryBudget {
    /// retry 次数（总尝试 = max_retries + 1）。
    max_retries: u32,
    /// 基础退避（ms）。
    delay_ms: u64,
    /// 指数退避 vs 恒定间隔。
    exponential_backoff: bool,
}

/// 起核重试预算（移植 上游 `resolveStartRetryBudget`，start-retry-policy.ts:26）。
///
/// system_interface（reverseMesh）节点在 TUN 下建第二张内核 TUN，双 TUN 同时 stop→start 时旧接口内核侧
/// 释放慢 → 起核撞「TUN 初始化未完成」退出（macOS 双 utun 抢占）。默认 2 次+指数退避（~6s）打不过释放 →
/// 放宽为 10 次+恒定 3s（给内核留足异步回收双 utun/适配器的时间）。Windows 禁 System（`mesh_system_
/// supported_on_platform` false）→ reverseMesh 强制 gVisor 不建第二张 TUN、无竞态 → 沿用默认。
fn resolve_start_retry_budget(
    is_tun: bool,
    servers: &[ServerConfig],
    platform: &str,
) -> StartRetryBudget {
    let has_system_interface_node = is_tun
        && mesh_system_supported_on_platform(platform)
        && servers.iter().any(mesh_uses_system_interface);
    if has_system_interface_node {
        StartRetryBudget {
            max_retries: 10,
            delay_ms: 3000,
            exponential_backoff: false,
        }
    } else {
        StartRetryBudget {
            max_retries: 2,
            delay_ms: 2000,
            exponential_backoff: true,
        }
    }
}

/// 起核 spawn **launch** 失败是否可重试（移植 上游 retry `shouldRetry` 的 `nonRetryableErrors` 反面，:882）。
///
/// 权限/找不到/enoent/eacces/eperm/配置无效 → 确定性失败，不重试；其余（端口/资源竞态）→ 可重试。
/// 起核期**就绪**失败（Dead/Timeout = CoreStartRetryError 等价）恒可重试、不经本谓词（其文案本不含关键词）。
fn is_retryable_start_error(message: &str) -> bool {
    let m = message.to_lowercase();
    const NON_RETRYABLE: &[&str] = &[
        "找不到",
        "权限",
        "permission",
        "enoent",
        "eacces",
        "eperm",
        "配置文件格式错误",
        "invalid config",
    ];
    !NON_RETRYABLE.iter().any(|&p| m.contains(p))
}

/// 外化规则文件原子写（tmp→rename，rename-over 触发 sing-box fswatch 热重载）。
///
/// 复用 store 的 `<base>.<12hex>.tmp` 唯一后缀命名——其形态被 `is_custom_rule_orphan_file` 的 `.tmp` 分支
/// 识别，故起核清扫能回收断电/强杀留下的半写 tmp（对齐 上游 atomicWrite 用 `writeFileAtomic`，:1711）。
fn atomic_write_custom_rule(path: &Path, content: &str) -> Result<(), String> {
    polaris_store::fs::atomic_write_plan(path, &polaris_store::fs::random_tmp_suffix(), content)
        .execute(&polaris_store::fs::StdFs)
        .map_err(|e| format!("{e:?}"))
}

/// 解析 sing-box 二进制路径。
///
/// 顺序：`POLARIS_SINGBOX_PATH` 环境变量（开发/测试逃生门）→ 可执行文件同级 `resources/<平台>/`
/// （打包态，fetch-core.mjs 的落地处）→ 仓内 `resources/<平台>/`（开发态）。
///
/// 内核平台子目录候选（**必须与 `fetch-core.mjs` 的落地目录逐字一致**：linux / win / mac-arm64 / mac-x64）。
///
/// 抽成纯函数是为了钉住一个真机 bug：此前 macOS 硬编码 "mac"，而 fetch-core 落 "mac-arm64"/"mac-x64"
/// 且 tauri.conf.json 也按这俩打包 → mac 上即便内核在包里也永远找不到。macOS 按运行架构优先
/// （aarch64→arm64），另一架构作回退（Rosetta / 异架构包兜底）。
fn core_platform_dirs(os: &str, arch: &str) -> Vec<&'static str> {
    match os {
        "macos" => {
            if arch == "aarch64" {
                vec!["mac-arm64", "mac-x64"]
            } else {
                vec!["mac-x64", "mac-arm64"]
            }
        }
        "windows" => vec!["win"],
        _ => vec!["linux"],
    }
}

/// bundled 资源二进制候选路径（sing-box 核 / polaris-helper 共用）。抽纯函数便于**钉 `_up_` 布局回归**。
///
/// 布局兜底顺序：① exe 同级 `resources/`（legacy 探针：三平台 conf 资源项全带 `../`，tauri 产物
/// 布局中不再出现，保守保留）② **`exe/_up_/resources/`（Windows NSIS 装机布局）**：tauri-utils 的
/// `resource_relpath` 把 `../` 段改名 `_up_`，NSIS 装机后资源在 `<exe目录>\_up_\resources\`——W10 根因，
/// 漏掉则装机态核/helper 解析双双落空（2026-08-19 真机 toast 首曝）③ `exe/../Resources/resources/`
/// （旧 mac 猜测）④ **`exe/../Resources/_up_/resources/`（macOS .app 真实布局）**：同一 `_up_` 改名
/// 机制在 .app 里的形态，实际落 `Contents/Resources/_up_/resources/`。漏 ④ → 打包态 mac 上核/helper
/// 恒找不到、proxy 起不来。⑤ `CARGO_MANIFEST_DIR/../resources/`（开发态）。
///
/// `exe_dir` = `current_exe().parent()`（None=取不到）；`manifest_dir` = `CARGO_MANIFEST_DIR`。
pub(crate) fn bundle_resource_candidates(
    exe_dir: Option<&std::path::Path>,
    manifest_dir: &std::path::Path,
    platform_dirs: &[&str],
    filename: &str,
) -> Vec<PathBuf> {
    let mut prefixes: Vec<PathBuf> = Vec::new();
    if let Some(dir) = exe_dir {
        prefixes.push(dir.join("resources"));
        // Windows NSIS 装机布局（W10 根因，2026-08-19 真机 toast 首曝）：tauri-utils 的
        // `resource_relpath` 把 `../` 段改名 `_up_`（与 bundler 无关，NSIS 同样生效），装机后资源
        // 在 `<exe目录>\_up_\resources\`——此前候选表只有 mac 的一种 `_up_` 形态（`../Resources/
        // _up_/resources`），Windows 装机态的核 / helper 解析双双落空（helper 安装 toast「未找到
        // polaris-helper 二进制」，核解析同函数同病）。放裸 `resources/` 之后：后者是 legacy 探针
        // （本 app 三平台 conf 的资源项全带 `../`，tauri 产物布局里裸形态不再出现），排序保守无害。
        prefixes.push(dir.join("_up_").join("resources"));
        prefixes.push(dir.join("..").join("Resources").join("resources"));
        prefixes.push(
            dir.join("..")
                .join("Resources")
                .join("_up_")
                .join("resources"),
        );
    }
    prefixes.push(manifest_dir.join("..").join("resources"));

    let mut candidates: Vec<PathBuf> = Vec::new();
    for prefix in &prefixes {
        for pdir in platform_dirs {
            candidates.push(prefix.join(pdir).join(filename));
        }
    }
    candidates
}

/// **观测腿**：内核记账里该 pid 正在执行的可执行文件路径（读不到 → `None`）。
///
/// 这是[内核自证](ProxyRuntime::attest_running_core_binary)的**事实来源**，其价值全在于它与
/// 「app 请求了什么」完全独立 —— 问的是操作系统「这个进程实际是从哪个文件起来的」。
///
/// - **linux**：读 `/proc/<pid>/exe` 符号链接（内核直给，最硬的一手证据）。二进制在进程起来后被
///   替换/删除时内核会给出 `<路径> (deleted)`，此处剥掉该后缀还原原路径（否则恒判不等 = 假告警）。
/// - **macOS**：`ps -p <pid> -o comm=`（无 `/proc`；`comm` 给的是完整路径而非 16 字节的 `p_comm`
///   短名——2026-07-31 在 p101 以普通用户查 root helper 实测得到完整 46 字符路径）。
///   受保护核路径含空格，故 `comm=` 必须是**唯一**输出字段，整行即路径。
/// - **windows**：返 `None`（无低成本 std 途径；且 win 的核走 app 侧、无受保护核目录，
///   本自证在该平台的价值本就最小）。`None` ⇒ 判 `Unobservable` ⇒ 只 warn 不误报。
fn running_exe_path(pid: u32) -> Option<PathBuf> {
    // pid=0 = 调用方还没拿到真 pid（helper 未回传 / spawn 失败）→ 没有可观测对象。
    if pid == 0 {
        return None;
    }
    running_exe_path_impl(pid)
}

/// [`running_exe_path`] 的 linux 实现：`/proc/<pid>/exe` 符号链接（内核直给）。
#[cfg(target_os = "linux")]
fn running_exe_path_impl(pid: u32) -> Option<PathBuf> {
    let p = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    // 内核对「映像已被替换/删除」的进程追加 " (deleted)"，剥掉还原真实路径（否则恒判不等 = 假告警）。
    let s = p.to_string_lossy();
    Some(
        s.strip_suffix(" (deleted)")
            .map_or_else(|| p.clone(), PathBuf::from),
    )
}

/// [`running_exe_path`] 的 macOS 实现：`ps -p <pid> -o comm=`（无 `/proc`）。
#[cfg(target_os = "macos")]
fn running_exe_path_impl(pid: u32) -> Option<PathBuf> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    // 进程已退出时 ps 可能成功但无输出 → 别把空串当成一个路径。
    (!line.is_empty()).then(|| PathBuf::from(line))
}

/// [`running_exe_path`] 的其余平台实现：无低成本 std 途径 → 恒 `None`（判 `Unobservable`，不误报）。
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn running_exe_path_impl(_pid: u32) -> Option<PathBuf> {
    None
}

/// **观测腿**：对**磁盘上那个文件**跑一次 `sing-box version`，取原始第一行；失败恒空串。
///
/// 与 `UpdaterRuntime::read_core_version_line` 同一纪律：**探测失败绝不回落随包基线** ——
/// 那会把「读不到」伪装成「就是基线」，正是自证最不能犯的错。此处更严：空串在
/// [`attest_core_binary`](crate::runtime::core_promote::attest_core_binary) 里被判**告警**而非通过。
fn core_version_first_line(bin: &Path) -> String {
    match no_console_window(std::process::Command::new(bin).arg("version")).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned(),
        Ok(out) => {
            log::warn!(
                "{} version 非零退出 {:?}：版本行置空",
                bin.display(),
                out.status
            );
            String::new()
        }
        Err(e) => {
            log::warn!("{} version spawn 失败 {e}：版本行置空", bin.display());
            String::new()
        }
    }
}

/// 现役核解析（三级优先级）：
///  1. `POLARIS_SINGBOX_PATH` 环境逃生门（**开发态**；指向不存在的文件即 Err，不静默回落）；
///  2. **可写现役核** `<config_dir>/core_update/sing-box[.exe]`（换核/回滚的落位目标，见
///     [`crate::runtime::core_paths`]）——存在即用；
///  3. 随包出厂核（bundle 种子，[`resolve_bundled_core_binary`]）。
///
/// 第 2 级是「可写现役核 + 随包种子」模型的读侧（移植 上游 `ResourceManager.getSingBoxPath`）：
/// 缺失即回落种子 ⇒ **首启/迁移永不 brick**。核基目录未注入时（单测/子进程）第 2 级恒 miss，
/// 行为与接线前逐字一致。
///
/// 找不到 → Err（**不静默回落 PATH**：误起系统里别的 sing-box 比起不来更糟）。
pub(crate) fn resolve_core_binary() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("POLARIS_SINGBOX_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "POLARIS_SINGBOX_PATH 指向的文件不存在: {}",
            p.display()
        ));
    }

    // 可写现役核优先（换核/回滚/reset-factory 全部落位于此）。
    if let Some(p) = crate::runtime::core_paths::writable_core_path() {
        if p.is_file() {
            return Ok(p);
        }
    }

    resolve_bundled_core_binary()
}

/// **随包出厂核**（bundle 种子）：绕过环境逃生门与可写核层，只解析打进安装包的资源。
///
/// 这是 reset-factory / reseed 的**源**——它们要的恰是「出厂那一份」，而非现役核。
pub(crate) fn resolve_bundled_core_binary() -> Result<PathBuf, String> {
    let filename = if cfg!(windows) {
        "sing-box.exe"
    } else {
        "sing-box"
    };
    let platform_dirs = core_platform_dirs(std::env::consts::OS, std::env::consts::ARCH);

    let exe = std::env::current_exe().ok();
    let candidates = bundle_resource_candidates(
        exe.as_deref().and_then(std::path::Path::parent),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        &platform_dirs,
        filename,
    );
    for c in &candidates {
        if c.is_file() {
            return Ok(c.clone());
        }
    }
    Err(format!(
        "未找到 sing-box 二进制（尝试过：{}）。开发态可设 POLARIS_SINGBOX_PATH，或跑 `node scripts/fetch-core.mjs`。",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" | ")
    ))
}

/// sing-box 官方面板运行时下载覆盖目录名（`<config_dir>/singbox-dashboard`）。
/// 与 `commands/misc.rs` 的 `SINGBOX_DASHBOARD_DIR` 同名同义：「刷新面板资源」清此目录 → 核下次启动回落随包内置。
const SINGBOX_DASHBOARD_DIR_NAME: &str = "singbox-dashboard";

/// 解析 sing-box 官方面板 `services[].dashboard.path`（对齐 上游 `resolveDashboardServeDir`）。
///
/// 优先级：**运行时下载覆盖**（`<config_dir>/singbox-dashboard` 含 `index.html`）→ **随包内置**
/// （`resources/dashboard/index.html`，`scripts/fetch-dashboard.mjs` 落地、tauri.conf `resources` 打包）→
/// 两者皆无返 `None`。
///
/// `None` 时 config-engine 省略 `path` → 核回落**联网下载**兜底（保「异常打包不 brick」）；该下载会在进程 CWD 下
/// 相对 mkdir `dashboard`，故必须配合起核 `.current_dir(<可写目录>)`（见 spawner `working_dir` / helper spawn）
/// 避免 CWD=`/` 下的只读 mkdir 噪音。命中（有 `path`）时核直接 serve 本地文件、**零联网下载、打开即时离线可用**
/// ——根治噪音的首选路径。
pub(crate) fn resolve_dashboard_serve_dir(config_dir: &std::path::Path) -> Option<String> {
    // 1) 运行时下载覆盖优先。
    let override_dir = config_dir.join(SINGBOX_DASHBOARD_DIR_NAME);
    if override_dir.join("index.html").is_file() {
        return Some(override_dir.to_string_lossy().into_owned());
    }
    // 2) 随包内置 resources/dashboard（非平台特定 → 借 bundle_resource_candidates 以 "dashboard" 作子目录、
    //    "index.html" 作探针；命中即取其父目录 = serve 根）。
    let exe = std::env::current_exe().ok();
    bundle_resource_candidates(
        exe.as_deref().and_then(std::path::Path::parent),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        &["dashboard"],
        "index.html",
    )
    .iter()
    .find(|c| c.is_file())
    .and_then(|c| c.parent())
    .map(|p| p.to_string_lossy().into_owned())
}

/// 发信号给 pid（core-supervisor [`ProcessKiller`] 的注入点）。
///
/// unix：`nix::sys::signal::kill`（safe wrapper，本文件 `forbid(unsafe_code)` 下不可直接 libc FFI）。
#[cfg(unix)]
pub(crate) fn send_signal(pid: u32, sig: Signal) {
    use nix::sys::signal::{kill, Signal as NixSignal};
    let nix_sig = match sig {
        Signal::Sigterm => NixSignal::SIGTERM,
        Signal::Sigkill => NixSignal::SIGKILL,
    };
    // 对已退出进程为安全 no-op（ESRCH）——吞掉。非法 pid 直接不发（见 checked_pid）。
    if let Some(p) = checked_pid(pid) {
        let _ = kill(p, nix_sig);
    }
}

/// windows 无 POSIX 信号：两级均退化为 `taskkill /F /T`（对齐 上游 Windows 停核路径）。
/// **未在本机验证**（本批真机验证限 Linux）。
#[cfg(windows)]
pub(crate) fn send_signal(pid: u32, _sig: Signal) {
    let _ = no_console_window(std::process::Command::new("taskkill").args([
        "/PID",
        &pid.to_string(),
        "/F",
        "/T",
    ]))
    .output();
}

/// `u32` pid → `nix::Pid`，**只放行真实单进程 pid**（`1..=i32::MAX`），否则 `None`。
///
/// **为什么必须有（安全，非洁癖）**：`pid as i32` 对 `pid > i32::MAX` 会**回绕成负数**，而 POSIX
/// `kill` 的负数/零 pid 是**广播语义**：`-1` = 给「本用户有权发信号的所有进程」发，`0` = 给整个
/// 当前进程组发。落到 [`send_signal`] 就是 `SIGKILL` 全场——把 app 自己和用户所有进程一起杀掉。
/// 落到 [`pid_alive`] 则是 `kill(-1,0)` 恒 `Ok` → 任何越界 pid 都被判「存活」，孤儿清扫永远收不了尾。
#[cfg(unix)]
fn checked_pid(pid: u32) -> Option<nix::unistd::Pid> {
    (pid >= 1 && pid <= i32::MAX as u32).then(|| nix::unistd::Pid::from_raw(pid as i32))
}

/// `kill(pid, 0)` 的 errno → 存活判定（纯逻辑，穷举各 errno 语义；探活的真值在此）。
///
/// **判定方向恒为「无死亡证据即判存活」**——五个消费点（起核门 / 就绪门 / 崩溃监测 /
/// 停核升级 / 孤儿清扫）里，误判「死」全是破坏性的（虚报起核失败、无谓重启、漏发 SIGKILL），
/// 误判「活」最多多发一次信号（对已死进程是 no-op）。故只有确证不存在才判不活。
// nix 是 unix-only 依赖，故必须 cfg(unix)——`test` cfg 在 windows `cargo test` 也为真，
// 若含 test 会在 windows 编入却找不到 nix crate（E0433）。测试端一并 cfg(unix)。
#[cfg(unix)]
fn alive_from_probe(r: Result<(), nix::errno::Errno>) -> bool {
    use nix::errno::Errno;
    match r {
        // 有权发信号且进程在 → 存活。
        Ok(()) => true,
        // **EPERM = 进程存在，只是不属本用户**（helper 以 root 起的核，app 以普通用户探活）。
        // 把它当「不存在」正是 TUN 提权路径下「helper 报告已启动但进程不存在」的根因。
        Err(Errno::EPERM) => true,
        // ESRCH = 内核确认无此进程 → 唯一的「不活」判据。
        Err(Errno::ESRCH) => false,
        // 其余 errno（EINVAL 等）非死亡证据 → 保守判活，绝不据此宣告核已崩。
        Err(_) => true,
    }
}

/// pid 是否存活（宽限期到点的二次确认，防 race 误杀）。
#[cfg(unix)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    // 非法 pid（0 / 越 i32 回绕）不是「不确定」而是「压根不是个进程」→ 判不活，且**绝不**让它
    // 走到 kill 的广播语义上去（见 [`checked_pid`]）。
    let Some(p) = checked_pid(pid) else {
        return false;
    };
    // signal 0 = 仅探活不发信号。
    alive_from_probe(kill(p, None))
}

/// **进程身份令牌**：回答「这个 pid 上挂的还是不是原来那个进程」。
///
/// # 为什么需要它
///
/// helper 腿（三平台的 TUN 一律经 helper，见 `should_start_via_helper`）没有本地 child 句柄，
/// 崩溃监测只能靠 [`pid_alive`] —— 而 `kill(pid, 0)` / `tasklist` 只回答「这个号码上有进程吗」，
/// **不回答「是不是我那个」**。核死后 pid 被系统复用，探活恒真 ⇒ 崩溃自愈永不触发，
/// 用户看到 `running: true` 而代理全断。直起腿不受影响（`child.try_wait()` 认的是句柄不是号码）。
///
/// # 为什么不复用 [`running_exe_path`]
///
/// 它在本场景最需要的两个平台上取不到材料：linux 的 `/proc/<pid>/exe` 对 root / setuid 降权后的
/// 进程，普通用户读会 `EACCES`（helper 腿的核正是这两类）；windows 侧它恒 `None`。
///
/// # 各平台取什么（性质相同：**活着期间恒定不变，换了进程必不同**）
///
/// - **linux**：`/proc/<pid>/stat` 的 starttime（第 22 字段）。该文件**世界可读**，不受属主与
///   dumpable 影响 —— 正是 exe 那条路取不到时仍取得到的那一格。
/// - **macos**：`ps -p <pid> -o lstart=`（跨用户可读，同 [`running_exe_path`] 的 mac 腿）。
/// - **windows**：`tasklist` 的映像名。比前两者弱（复用成**同名**进程认不出），但「复用成了另一个
///   程序」必被抓到；windows 无低成本的创建时间途径，不为此引入 WMI。
/// - **其余平台**：`None` ⇒ 判 [`PidIdentity::Unobservable`]，只跳过、**绝不**据此报崩溃。
fn process_identity(pid: u32) -> Option<String> {
    // pid=0 = 还没拿到真 pid → 没有可观测对象（同 `running_exe_path` 的口径）。
    if pid == 0 {
        return None;
    }
    process_identity_impl(pid)
}

#[cfg(target_os = "linux")]
fn process_identity_impl(pid: u32) -> Option<String> {
    parse_proc_stat_starttime(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

#[cfg(target_os = "macos")]
fn process_identity_impl(pid: u32) -> Option<String> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    // 进程已退出时 ps 可能成功但无输出 → 别把空串当成一个令牌。
    (!line.is_empty()).then_some(line)
}

#[cfg(windows)]
fn process_identity_impl(pid: u32) -> Option<String> {
    let out = no_console_window(std::process::Command::new("tasklist").args([
        "/FI",
        &format!("PID eq {pid}"),
        "/NH",
    ]))
    .output()
    .ok()?;
    parse_tasklist_image_name(&String::from_utf8_lossy(&out.stdout), pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn process_identity_impl(_pid: u32) -> Option<String> {
    None
}

/// `/proc/<pid>/stat` → starttime（第 22 字段，纯逻辑）。
///
/// **必须从最后一个 `)` 之后切**：第 2 字段 comm 被括号包着，且**可含空格与右括号**
/// （进程名由用户控制）⇒ 直接按空白切分会在这类进程上整体错位，取到一个恒变或恒不变的错字段。
/// 切完后首 token 是第 3 字段 state ⇒ starttime 是其中第 20 个（下标 19）。
#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat_starttime(stat: &str) -> Option<String> {
    let tail = &stat[stat.rfind(')')? + 1..];
    tail.split_whitespace().nth(19).map(str::to_owned)
}

/// `tasklist /NH` 输出 → 该 pid 那一行的映像名（首列，纯逻辑）。
///
/// 定位判据与 [`tasklist_reports_pid`] 同源（按 token 全等找行，不做子串匹配，理由见那里）。
/// 映像名含空格时只取首段——windows 的可执行名不带空格，且本令牌只用于**比对是否变化**，
/// 截断对该用途无害（截断是稳定的）。
#[cfg(any(windows, test))]
fn parse_tasklist_image_name(stdout: &str, pid: u32) -> Option<String> {
    let want = pid.to_string();
    stdout
        .lines()
        .find(|line| {
            line.split_whitespace()
                .any(|tok| tok.trim_end_matches(',') == want)
        })?
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

/// pid 身份复核的三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PidIdentity {
    Match,
    Mismatch,
    Unobservable,
}

/// 基线令牌 × 当前令牌 → 三态（纯逻辑）。
///
/// **「没观测到」绝不折成「不匹配」**：取不到材料（平台不支持 / 读失败 / 进程刚好在这一刻消失）
/// 一律 [`PidIdentity::Unobservable`]。折成 `Mismatch` 会把一次读失败变成一次**假崩溃**，
/// 而假崩溃的下游是自动重启 —— 本仓在 `running_exe_path` 那条自证腿上写过同一句：
/// 没观测到 ≠ 观测到没问题。
fn pid_identity_verdict(baseline: Option<&str>, current: Option<&str>) -> PidIdentity {
    match (baseline, current) {
        (Some(a), Some(b)) if a == b => PidIdentity::Match,
        (Some(_), Some(_)) => PidIdentity::Mismatch,
        _ => PidIdentity::Unobservable,
    }
}

/// `tasklist /NH` 输出 → 该 pid 是否在列（纯逻辑）。行格式：
/// `imagename  pid  session  session#  memusage`，**按空白切分逐 token 全等比对**。
///
/// 不用 `contains(pid)` 子串匹配：内存列（`12,500 K`）/ 会话号会撞小 pid，
/// 且过滤无命中时 tasklist 打印 `INFO: No tasks are running...` 也可能夹带数字 → 假「存活」。
#[cfg(any(windows, test))]
fn tasklist_reports_pid(stdout: &str, pid: u32) -> bool {
    let want = pid.to_string();
    stdout.lines().any(|line| {
        line.split_whitespace()
            .any(|tok| tok.trim_end_matches(',') == want)
    })
}

#[cfg(windows)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    // tasklist 过滤该 pid。Windows 无 EPERM 等价盲区（普通用户也能枚举 SYSTEM 进程的
    // 名字/pid），但 tasklist 本身起不来时**没有死亡证据** → 与 unix EPERM 同一保守方向判存活，
    // 绝不因为探活工具缺失就宣告核已崩（那会触发无谓重启）。
    let Ok(out) = no_console_window(std::process::Command::new("tasklist").args([
        "/FI",
        &format!("PID eq {pid}"),
        "/NH",
    ]))
    .output() else {
        return true;
    };
    tasklist_reports_pid(&String::from_utf8_lossy(&out.stdout), pid)
}

/// 当前 epoch 毫秒（喂崩溃自愈状态机的冷却/退避时间轴）。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// **C3**：测量到节点 `host:port` 的 TCP 建连延迟（ms）。可达 → `Some(ms)`，超时/失败 → `None`
/// （上游 `measureLatency`，:319-338）。空 host / 0 端口 → `None`（无从连）。
///
/// **真机门**：真发起对节点地址的 TCP 连接（碰宿主网络），禁本机单测——决策层用 mock 延迟覆盖。
async fn measure_latency(host: &str, port: u16) -> Option<u32> {
    if host.is_empty() || port == 0 {
        return None;
    }
    let start = std::time::Instant::now();
    let addr = format!("{host}:{port}");
    match tokio::time::timeout(
        Duration::from_millis(PING_TIMEOUT_MS),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_stream)) => Some(start.elapsed().as_millis().min(u128::from(u32::MAX)) as u32),
        // 超时 / 连接错误 → 不可达。
        _ => None,
    }
}

/// **C3**：应用层连通性检测（上游 `checkProxyConnectivity`，:268-273）：经本地 mixed 代理端口以绝对
/// URI 形式 GET generate_204，任一端点返回 2xx/3xx → 判通。比裸 TCP ping 节点地址更可靠——端口可达不代表
/// 代理握手/转发正常（鉴权失效、节点限流、TUN 回流等）。**真机门**：需真起核 + 碰网络，禁本机单测。
async fn probe_proxy_connectivity(mixed_port: u16) -> bool {
    for url in CONNECTIVITY_URLS {
        if probe_through_proxy(mixed_port, url).await {
            return true;
        }
    }
    false
}

/// **C3**：经本地 HTTP 代理（`127.0.0.1:mixed_port`）以绝对 URI 形式 GET 目标，判是否拿到 2xx/3xx
/// （上游 `probeThroughProxy`，:278-314）。mixed 入站是 HTTP/SOCKS 混合口，发绝对 URI 的 GET = HTTP
/// 代理请求格式，故直连该口即经代理链出海。**真机门**：需真起核 + 碰网络，禁本机单测。
async fn probe_through_proxy(mixed_port: u16, target_url: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // 取 Host 头（`http://<host>/path` → `<host>`）。
    let host = target_url
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("");
    if host.is_empty() {
        return false;
    }
    let request = format!(
        "GET {target_url} HTTP/1.1\r\nHost: {host}\r\nProxy-Connection: close\r\nConnection: close\r\n\r\n"
    );
    let addr = format!("127.0.0.1:{mixed_port}");
    let probe = async {
        let mut stream = tokio::net::TcpStream::connect(&addr).await.ok()?;
        stream.write_all(request.as_bytes()).await.ok()?;
        // 只需状态行（`HTTP/1.1 204 No Content`）；读一小段即可解析首行。
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.ok()?;
        let text = std::str::from_utf8(&buf[..n]).ok()?;
        let code: u32 = text.split_whitespace().nth(1)?.parse().ok()?;
        Some((200..400).contains(&code))
    };
    matches!(
        tokio::time::timeout(Duration::from_millis(CONNECTIVITY_TIMEOUT_MS), probe).await,
        Ok(Some(true))
    )
}

// ════════════════ 出口自证：「实际生效出口 == 选中节点」的静态对账 ════════════════
//
// **根因**：从「用户选中节点」到「实际出口」之间，此前没有任何一处校验二者相等。起核的成功判据只有
// 「进程起来了 + 管理 API 可连」，不含「流量真的从选中节点出去」。于是 selector 降级、`route.final=direct`、
// mesh 出口回落、渲染端传错 `selectedServerId` —— 多条互不相关的路径，用户看到的都是同一个「已连接」绿灯，
// 实则明文直连。**安全定级**：用户以为流量加密走代理、实则未加密，且无任何信号。
//
// **为什么走静态对账，而不是探针 / 管理 API 查 selector**（两条备选都实测过，均不可行）：
//  1. **复用 `probe_proxy_connectivity`/`probe_through_proxy`**：二者自陈「真机门：需真起核 + 碰网络」，
//     每次探测是一趟**真实外网往返**（`CONNECTIVITY_TIMEOUT_MS` 量级）。挂进起核路径 = 直接给已经偏慢的
//     启动再加一个网络 RTT；且它只能答「通不通」，答不出「从**哪个**节点出去」——对本不变式根本不是判据。
//  2. **查管理 API 的 selector 实际 `selected`**：本仓的核是 sing-box 1.14，**`clash_api` 已移除**
//     （见 `singbox/config.rs` `services` 字段注释），管理面只剩 `daemon.StartedService` gRPC，而该 proto
//     **只有 `SelectOutbound`（写），没有任何读 selector 状态的 RPC**（见 `started_service.proto`）。
//     即「查 selector 实际值」这条路在当前核上**不存在可调的接口**，要走得先给核加 RPC + grpcurl 反射核对。
//
// **本实现取的判据**：核实际启动用的那份 sing-box config 就是出口的**权威真值**——`route.final` 与 selector
// `default` 决定了第一个包从哪出去。把它与用户**落盘**的 `selectedServerId` 对账，即可静态拆穿全部降级路径。
//
// **零启动延迟**：整条链是纯函数 + 一次内存缓存读（`ConfigManager::current()` 命中 RwLock 缓存，不碰磁盘），
// 无 I/O、无网络、无 spawn、无 await —— 耗时在微秒量级，故直接内联在就绪后调用，既不阻塞也不需要超时。
// 这也**强于**任何探针：探针只在探测那一刻采样，静态对账覆盖的是「核启动时装的是什么」这一确定事实。

/// 出口自证判定结果。`Match` 以外的每个变体都对应一条**用户必须知道**的降级路径。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExitAttestation {
    /// 实际生效出口 == 用户意图（含「用户自己选了直连」「模式语义本就直连」两种放行）。
    Match,
    /// 用户选中真实节点，实际默认出口却是 `direct` —— **明文直连**，最高危。
    SilentDirect { expected_tag: String },
    /// 实际默认出口是**另一个** tag（走错节点）。仍加密，但不是用户选的出口。
    WrongExit {
        expected_tag: String,
        actual_tag: String,
    },
    /// 核实际启动用的选中节点 ≠ 用户落盘意图（渲染端传了陈旧 config 快照）。
    StaleSelection {
        persisted: String,
        started_with: String,
    },
    /// 选中 id 在本次 tag 映射里查无对应（generate 侧本应已拦，留作兜底可见性）。
    UnknownSelection { selected_id: String },
    /// 配置里解不出默认出口（无 `route.final`）→ 无法自证，按「不确定即告警」处理。
    UnresolvedExit { expected_tag: String },
}

impl ExitAttestation {
    /// 用户可见文案。**统一以「流量未按预期走选中节点」开头**——用户要的第一信息是「我不安全」，
    /// 而不是内部 tag 名；tag/id 作为定位细节跟在后面。
    fn user_message(&self) -> String {
        match self {
            Self::Match => String::new(),
            Self::SilentDirect { expected_tag } => format!(
                "流量未走选中节点「{expected_tag}」，实际出口为直连（未加密）。请检查该节点配置或重新选择节点。"
            ),
            Self::WrongExit {
                expected_tag,
                actual_tag,
            } => format!(
                "流量未走选中节点「{expected_tag}」，实际出口为「{actual_tag}」。"
            ),
            Self::StaleSelection {
                persisted,
                started_with,
            } => format!(
                "启动用的节点（{started_with}）与当前选中节点（{persisted}）不一致，流量可能未走选中节点。请重新连接。"
            ),
            Self::UnknownSelection { selected_id } => format!(
                "选中节点（{selected_id}）不在本次启动的节点表中，流量可能未走该节点。"
            ),
            Self::UnresolvedExit { expected_tag } => format!(
                "无法确认流量是否走选中节点「{expected_tag}」（配置未指定默认出口）。"
            ),
        }
    }
}

/// 从**核实际启动的那份** sing-box config 解出「实际默认出口 tag」。
///
/// `route.final` 是第一跳：它要么直接是某个出站 tag，要么指向 selector —— 后者的实际出口是其 `default`
/// 成员（热切换发生前，`default` 就是核启动时选中的那个）。两级都解开才是真正的出口。
fn effective_exit_tag(singbox_config: &SingBoxConfig) -> Option<String> {
    let final_tag = singbox_config.route.as_ref()?.final_outbound.as_deref()?;
    if final_tag == DIRECT_TAG {
        return Some(DIRECT_TAG.to_string());
    }
    // final 指向 selector → 实际出口 = 它的 default；非 selector（无 default）→ final 自身即出口。
    Some(
        singbox_config
            .outbounds
            .iter()
            .find(|o| o.tag == final_tag)
            .and_then(|o| o.default.clone())
            .unwrap_or_else(|| final_tag.to_string()),
    )
}

/// 出口自证（**纯函数、零 I/O**）：对账「核实际启动的配置解出的出口」与「用户落盘的选中节点」。
///
/// `persisted_selected_id` = 用户**已提交**的意图（`config.json` 的 `selectedServerId`）。之所以以它为准
/// 而非只看 `user_config`：`user_config` 来自渲染端传来的 config 快照，**它本身就可能是错的**（陈旧快照 →
/// 起核按旧值落直连，而 UI 已显示新节点）。落盘值才是用户点过的那一下——`server:switch` 与自动换节点
/// 都是**先 `save_full` 再入核**，故「落盘值 ≠ 起核值」⟺ 渲染端传了陈旧快照，不存在合法的第三种解释。
/// 门② 的前置条件：**地区反向（回国）模式的「→代理」腿真的还在**。
///
/// 纯函数、零 I/O：判据全部取自「核实际启动的这份 config」——本地地区 geo tag（`region_local_geo`，
/// cn = `geosite-cn`/`geoip-cn`）是否仍有 `route.rule_set` 定义。定义在 ⟺ 引用它的规则没被
/// [`apply_rule_set_prune`](polaris_config_engine::builder::helpers::apply_rule_set_prune) 剪掉
/// ⟺ 国内流量确实还会被送去代理。
///
/// **为什么查 rule_set 定义而不是查规则条目**：剪枝是「定义缺失 → 连规则一起剪」，定义是因、规则是果，
/// 查因不会被规则形态的后续改动带偏。越界 region（手改 JSON）→ `region_local_geo` 返 None → 判定不完整
/// → 不放行（fail-safe：判不准就告警，不静默放行）。
fn region_reverse_rule_sets_intact(
    user_config: &UserConfig,
    singbox_config: &SingBoxConfig,
) -> bool {
    use polaris_config_engine::user_config::region_local_geo;
    let Some(rr) = user_config.region_routing.as_ref() else {
        return false;
    };
    let Some(local) = region_local_geo(&rr.region) else {
        return false;
    };
    let defined: BTreeSet<&str> = singbox_config
        .route
        .as_ref()
        .and_then(|r| r.rule_set.as_deref())
        .unwrap_or(&[])
        .iter()
        .map(|rs| rs.tag.as_str())
        .collect();
    local
        .geosite
        .iter()
        .chain(local.geoip.iter())
        .all(|t| defined.contains(t.as_str()))
}

fn attest_effective_exit(
    user_config: &UserConfig,
    singbox_config: &SingBoxConfig,
    persisted_selected_id: Option<&str>,
) -> ExitAttestation {
    // 门①：用户显式选「全直连」模式 → `final=direct` 是设计语义，不是降级。
    if user_config.proxy_mode == ProxyMode::Direct {
        return ExitAttestation::Match;
    }
    // 门②：smart + 地区反向（回国：本地走代理·海外直连）→ `final=direct` 同为设计语义。
    // 这两门不放行就会对**用户自己选的模式**天天误报，告警一旦有假就会被整体无视。
    //
    // **但白名单的是「reverse 且规则集完整」，不是「reverse」**：reverse 下唯一把流量送去代理的就是
    // 本地地区 geo 那两条 rule_set 规则（`geosite-cn`/`geoip-cn`），它们的 rule_set 定义若因本地 `.srs`
    // 缺失被 fail-closed 剪掉，「回国」就已经退化成全量明文直连——那是**真故障**，不是设计语义。
    // 旧粒度把这个故障一并放行，于是真机全量直连时零告警、日志还打「出口自证通过」。
    //
    // ⚠️ **不可达性登记（别照着它设计真机验收）**：这条收紧在**当前生产链路上已构造不出来**。
    // 同一场景下 `builder/route.rs` 的 T2 fail-safe 先一步把 `final` 从 direct 翻成 `proxy-selector`，
    // `effective_exit_tag` 解到 selector 的 `default` = 选中节点 ⇒ 本函数拿到的从来不是 `direct`，
    // 结论恒 `Match`。唯一还能让生产 config 带着 `final=direct` 走到这里的，是 D4/D7 组网出口回退
    // （`user_exit_tag == "direct"`，T2 明确不改写）——而那里 direct 是**设计语义**，告警反成误报。
    // 故：「手删 `<userData>/rules/*.srs` 后起核应出现 `EXIT_MISMATCH`」这类真机门**不可能达成**，
    // 谁再提出来请先读这段。本收紧与下面三条测试保留的理由是 defense-in-depth（T2 若被改坏 / 未来
    // 新增绕过 T2 的 config 来源时，这里仍是最后一道），**不是**因为它当前会触发。
    // 同类登记见 golden_config_snapshot.rs 对偶用例里的 mesh-exit-fallback 边界。
    if user_config.proxy_mode == ProxyMode::Smart
        && user_config
            .region_routing
            .as_ref()
            .is_some_and(|r| r.enabled && r.reverse)
        && region_reverse_rule_sets_intact(user_config, singbox_config)
    {
        return ExitAttestation::Match;
    }

    let started_with = user_config.selected_server_id.as_deref();

    // 轴①（渲染端竞态）：起核用的选中节点 ≠ 落盘意图。**必须先判**——此腿下 `user_config` 整体不可信，
    // 再拿它去推「期望 tag」只会得出「配置自洽」的假绿（配置确实自洽，只是自洽于一个错的意图）。
    if let Some(persisted) = persisted_selected_id {
        if started_with != Some(persisted) {
            return ExitAttestation::StaleSelection {
                persisted: persisted.to_string(),
                started_with: started_with.unwrap_or("<none>").to_string(),
            };
        }
    }

    // 轴②：用户选了直连哨兵 → 出口本就该是 direct。
    if is_direct_selection(started_with) {
        return ExitAttestation::Match;
    }
    // 未选中任何节点 → 无可对账的意图（generate 侧另有校验）。
    let Some(selected_id) = started_with else {
        return ExitAttestation::Match;
    };

    // 期望 tag：复用 outbounds/selector 构建用的**同一个** `build_id_to_tag_map`（撞名去重规则一致），
    // 不另写一份——自己算一遍 tag 就等于用一份可能不同的规则去校验，撞名场景必假。
    struct SrvLike<'a>(&'a ServerConfig);
    impl ServerLike for SrvLike<'_> {
        fn id(&self) -> &str {
            &self.0.id
        }
        fn name(&self) -> &str {
            &self.0.name
        }
    }
    let wrappers: Vec<SrvLike> = user_config.servers.iter().map(SrvLike).collect();
    let id_to_tag = build_id_to_tag_map(&wrappers);
    let Some(expected_tag) = id_to_tag.get(selected_id) else {
        return ExitAttestation::UnknownSelection {
            selected_id: selected_id.to_string(),
        };
    };

    match effective_exit_tag(singbox_config) {
        Some(actual) if actual == *expected_tag => ExitAttestation::Match,
        Some(actual) if actual == DIRECT_TAG => ExitAttestation::SilentDirect {
            expected_tag: expected_tag.clone(),
        },
        Some(actual) => ExitAttestation::WrongExit {
            expected_tag: expected_tag.clone(),
            actual_tag: actual,
        },
        None => ExitAttestation::UnresolvedExit {
            expected_tag: expected_tag.clone(),
        },
    }
}

/// 从 config Value 抽 server id 集（差集用）。
fn server_ids(config: &Value) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    if let Some(servers) = config.get("servers").and_then(Value::as_array) {
        for s in servers {
            if let Some(id) = s.get("id").and_then(Value::as_str) {
                set.insert(id.to_string());
            }
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    // 出口自证测试用的直连哨兵（与 `is_direct_selection` 同源，勿在测试里另写字面量）。
    use polaris_config_engine::user_config::dns_constants::DIRECT_SERVER_ID;

    /// **R4 就绪门参数钉死**：轮询间隔调细的同时，**总超时预算一格都不许缩**。
    ///
    /// 两个常量管的是正交的事，混为一谈会直接造出「慢机器起核被误判失败」的 bug：
    /// - `CORE_READY_POLL_MS` = 采样精度（就绪后多久**发现**）→ 调小只影响启动快慢。
    /// - `CORE_READY_TIMEOUT_MS` = 容忍度（到底能等多久）→ 调小会砍掉冷启动/杀软扫描的余量。
    ///
    /// 故本测同时钉两头：间隔已降到 50ms，且 12s 的等待窗口原封不动。
    #[test]
    fn core_ready_gate_shortens_poll_without_shrinking_timeout_budget() {
        // 轮询间隔已细化（实测 API 口 97–221ms 就 listen，500ms 栅格纯属白等）。
        assert_eq!(CORE_READY_POLL_MS, 50);
        // 总超时**未被一起缩短** —— 这是慢机器的容忍度，动它就是误判起核失败。
        assert_eq!(
            CORE_READY_TIMEOUT_MS, 12_000,
            "总超时是慢机器容忍度，不得随轮询间隔一起缩短"
        );

        // 等待窗口以「实际覆盖的时间」为准，而非轮数：max_polls = ceil(timeout/poll)。
        // 缩 timeout 或（在 timeout 不变时）把两者一起改小，都会让本断言转红。
        let max_polls = CORE_READY_TIMEOUT_MS.div_ceil(CORE_READY_POLL_MS).max(1);
        assert_eq!(max_polls, 240);
        assert_eq!(
            max_polls * CORE_READY_POLL_MS,
            12_000,
            "轮数 × 间隔必须仍覆盖满 12s 窗口"
        );
        // 单次就绪探测超时不得超过一整个等待窗口（否则一次探测就能吃满预算）。
        assert!(READY_PROBE_TIMEOUT.as_millis() as u64 <= CORE_READY_TIMEOUT_MS);
    }

    /// 内核平台目录必须与 fetch-core.mjs / tauri.conf.json 逐字一致。
    /// 打断 macOS 分支（若回退成 "mac"）→ 本测转红，即「mac 找不到内核、代理起不来」那个 bug。
    #[test]
    fn core_platform_dirs_match_fetch_layout() {
        // macOS：按 arch 选 mac-arm64/mac-x64，**绝不是 "mac"**（那是 bug 值）。
        assert_eq!(
            core_platform_dirs("macos", "aarch64"),
            vec!["mac-arm64", "mac-x64"]
        );
        assert_eq!(
            core_platform_dirs("macos", "x86_64"),
            vec!["mac-x64", "mac-arm64"]
        );
        assert!(
            !core_platform_dirs("macos", "aarch64").contains(&"mac"),
            "macOS 目录必须带 arch 后缀，裸 'mac' 是 fetch-core 里不存在的目录"
        );
        // 其余平台与 fetch-core 落地目录一致。
        assert_eq!(core_platform_dirs("linux", "x86_64"), vec!["linux"]);
        assert_eq!(core_platform_dirs("windows", "x86_64"), vec!["win"]);
    }

    /// **钉 macOS `_up_` 布局回归**：tauri 把 `../resources/` 打进 `Contents/Resources/_up_/resources/`。
    /// 候选必须含 `_up_` 段，否则打包态 mac 上 sing-box 核 / polaris-helper 恒找不到、proxy 起不来
    /// （真机踩坑：recipe 里「core 路径已修」只加了无 `_up_` 的 `Resources/resources/`，仍找不到）。
    #[test]
    fn bundle_candidates_include_macos_up_layout() {
        use std::path::Path;
        let exe_dir = Path::new("/Applications/Polaris.app/Contents/MacOS");
        let manifest = Path::new("/dev/polaris/src-tauri");
        let c = bundle_resource_candidates(
            Some(exe_dir),
            manifest,
            &["mac-arm64", "mac-x64"],
            "sing-box",
        );
        // 关键断言：mac `_up_` 布局候选必须在，且必须**从 mac 前缀**来——单 contains 会被
        // `<exe>/_up_/resources/`（Windows NSIS 形态，2026-08-19 加）喂成恒绿假钉（评审实证：
        // 删 mac `_up_` push 后单条件版本 2/2 仍绿）。双条件合取恢复杀伤力。
        // Windows 上 Path::join 产生 `\` 分隔 → 归一成 `/` 再比子串（断言的是布局结构非分隔符）。
        assert!(
            c.iter().any(|p| {
                let n = p.to_string_lossy().replace('\\', "/");
                n.contains("_up_/resources/mac-arm64/sing-box")
                    && n.starts_with("/Applications/Polaris.app/Contents/MacOS/../Resources/_up_")
            }),
            "缺 macOS `_up_` 布局候选 → 打包态核找不到；候选={c:?}"
        );
        // 开发态 CARGO_MANIFEST_DIR/../resources 兜底也在（`..` 不规范化，串里保留 `src-tauri/../`）。
        assert!(c.iter().any(|p| p
            .to_string_lossy()
            .replace('\\', "/")
            .contains("src-tauri/../resources/mac-arm64/sing-box")));
        // exe_dir=None（取不到 exe）时只剩开发态候选，不 panic。
        let none = bundle_resource_candidates(None, manifest, &["linux"], "sing-box");
        assert_eq!(none.len(), 1);
    }

    /// **钉 Windows NSIS 装机布局回归（W10 根因）**：资源在 `<exe目录>\_up_\resources\`。
    /// 候选缺 `<exe>/_up_/resources/<平台>/` 形态 → 装机态 helper 安装报「未找到二进制」不触发提权、
    /// 核解析同函数同病（2026-08-19 真机 toast 首曝；候选表当时只有 mac 的两种 `_up_` 形态）。
    #[test]
    fn bundle_candidates_include_windows_nsis_up_layout() {
        use std::path::Path;
        let exe_dir = Path::new(r"C:\Users\doveh\AppData\Local\Polaris");
        let manifest = Path::new("/dev/polaris/src-tauri");
        let c = bundle_resource_candidates(Some(exe_dir), manifest, &["win"], "polaris-helper.exe");
        // 关键断言：`<exe>/_up_/resources/win/` 必须在（删那行 push → 本测转红）。
        assert!(
            c.iter().any(|p| p
                .to_string_lossy()
                .replace('\\', "/")
                .contains("/_up_/resources/win/polaris-helper.exe")
                && p.to_string_lossy()
                    .replace('\\', "/")
                    .starts_with("C:/Users/doveh/AppData/Local/Polaris/_up_")),
            "缺 Windows NSIS `_up_` 装机布局候选 → 装机态核/helper恒找不到；候选={c:?}"
        );
    }

    /// sing-box 行级别映射：DEBUG/TRACE 须如实标级，不得混进 info。
    /// 打断 DEBUG 分支（落回 else → Info）→ 本测转红，即 DEBUG 过滤形同虚设的那个 bug。
    #[test]
    fn singbox_line_level_maps_all_tokens() {
        assert_eq!(
            singbox_line_level("+0800 2026-07-17 10:00:00 FATAL start service: xxx"),
            log::Level::Error
        );
        assert_eq!(
            singbox_line_level("+0800 ERROR bad config"),
            log::Level::Error
        );
        assert_eq!(
            singbox_line_level("+0800 WARN deprecated"),
            log::Level::Warn
        );
        assert_eq!(
            singbox_line_level("+0800 DEBUG dns: exchange example.com"),
            log::Level::Debug,
            "DEBUG 行必须标 Debug，否则日志页 DEBUG 档筛不出核的详情"
        );
        assert_eq!(
            singbox_line_level("+0800 TRACE inbound/mixed packet"),
            log::Level::Trace
        );
        assert_eq!(
            singbox_line_level("+0800 INFO router: loaded"),
            log::Level::Info
        );
        // 无级别 token（核的裸输出行）→ Info 兜底，不丢行。
        assert_eq!(
            singbox_line_level("bare line without level"),
            log::Level::Info
        );
    }

    /// 核侧七档 `LogLevel` → sink 五档：panic/fatal/error 并档 Error，其余逐一对应。
    ///
    /// **未知级号必须归 Info 而不是被丢掉**：上游扩枚举时宁可级别偏保守，也不能静默吃掉一行核日志。
    /// 打断 Debug 分支（落 Info）→ 「日志页选 DEBUG 却筛不出核的详情」那个 bug 原地复现 → 本测转红。
    #[test]
    fn core_log_level_maps_all_seven_upstream_levels() {
        use polaris_singbox_grpc::daemon::LogLevel as L;
        assert_eq!(core_log_level(L::Panic as i32), log::Level::Error);
        assert_eq!(core_log_level(L::Fatal as i32), log::Level::Error);
        assert_eq!(core_log_level(L::Error as i32), log::Level::Error);
        assert_eq!(core_log_level(L::Warn as i32), log::Level::Warn);
        assert_eq!(core_log_level(L::Info as i32), log::Level::Info);
        assert_eq!(
            core_log_level(L::Debug as i32),
            log::Level::Debug,
            "DEBUG 必须如实标级，否则日志页 DEBUG 档筛不出核的详情"
        );
        assert_eq!(core_log_level(L::Trace as i32), log::Level::Trace);
        assert_eq!(
            core_log_level(99),
            log::Level::Info,
            "上游扩了枚举 → 保守归 Info，绝不丢行"
        );
    }

    /// 隐私锁下限这道闸不受用户级别影响：级别拨到 debug（`max=Trace`）也不许 info 行落盘。
    /// 这是 N1 那条真隐私回归的判据，不得被「反正 log! 也会筛」的化简吃掉。
    #[test]
    fn core_log_admits_enforces_privacy_floor_independent_of_max() {
        let max = log::LevelFilter::Trace; // 用户把级别拨到最啰嗦
        let floor = core_log_privacy_floor(true); // 隐私锁开 ⇒ Warn
        assert!(
            !core_log_admits(log::Level::Info, floor, max),
            "隐私锁开着 info 不得转发"
        );
        assert!(!core_log_admits(log::Level::Debug, floor, max));
        assert!(
            core_log_admits(log::Level::Warn, floor, max),
            "warn 及更严的必须过"
        );
        assert!(core_log_admits(log::Level::Error, floor, max));
    }

    /// 用户级别这道闸不受隐私锁影响：非隐私态（下限 = Trace，不设限）下仍按 `max_level` 筛。
    /// 它**不改变去留**（下游 `log::log!` 一样会筛），改变的是筛之前做不做剥除 —— 故判据是
    /// 「与 log! 的结果逐格一致」，一致就说明提前筛是等价的。
    #[test]
    fn core_log_admits_enforces_user_level_independent_of_floor() {
        let floor = core_log_privacy_floor(false); // 非隐私态 ⇒ Trace，不设限
        for max in [
            log::LevelFilter::Off,
            log::LevelFilter::Error,
            log::LevelFilter::Warn,
            log::LevelFilter::Info,
            log::LevelFilter::Debug,
            log::LevelFilter::Trace,
        ] {
            for level in [
                log::Level::Error,
                log::Level::Warn,
                log::Level::Info,
                log::Level::Debug,
                log::Level::Trace,
            ] {
                assert_eq!(
                    core_log_admits(level, floor, max),
                    level <= max,
                    "提前筛必须与 log! 的判定逐格一致（level={level}, max={max}）"
                );
            }
        }
    }

    /// 两道闸是**合取**：任一不过即不转发。取两者各自放行、另一者拦截的交叉组合。
    #[test]
    fn core_log_admits_is_conjunction_of_both_gates() {
        let floor = core_log_privacy_floor(true); // Warn
                                                  // 级别闸放行（max=Trace）但隐私闸拦：
        assert!(!core_log_admits(
            log::Level::Debug,
            floor,
            log::LevelFilter::Trace
        ));
        // 隐私闸放行（Error ≤ Warn）但级别闸拦（max=Off）：
        assert!(!core_log_admits(
            log::Level::Error,
            floor,
            log::LevelFilter::Off
        ));
        // 两闸都放行：
        assert!(core_log_admits(
            log::Level::Error,
            floor,
            log::LevelFilter::Error
        ));
    }

    /// `SubscribeLog` 消息体的装饰剥除。
    ///
    /// 夹具是**真核实际会发出的形状**：喂这条流的 `platformFormatter` 没关色
    /// （`log/observable.go` 里关色那两行是注释掉的），且走 `Format` 的默认时间戳分支
    /// ⇒ `"\x1b[36mINFO\x1b[0m[0012] router: …"`。不剥的话日志页每行都是转义乱码 + 重复级别。
    ///
    /// 打断 ANSI 剥除 → 第一断言转红；打断级别前缀剥除 → 第二断言转红；
    /// 把「形状对不上就原样返回」改成强行截断 → 后三条转红。
    #[test]
    fn strip_core_log_decoration_removes_ansi_and_redundant_level_prefix() {
        assert_eq!(
            strip_core_log_decoration("\u{1b}[36mINFO\u{1b}[0m[0012] router: loaded 5 rules"),
            "router: loaded 5 rules"
        );
        assert_eq!(
            strip_core_log_decoration("DEBUG[0001] dns: exchange example.com"),
            "dns: exchange example.com",
            "无色时同样要剥掉级别前缀（级别由结构化字段承担，UI 自己渲染）"
        );

        // ── 形状对不上 → 整段原样保留（剥除绝不能演变成「吃掉半行」）──
        assert_eq!(
            strip_core_log_decoration("router: loaded"),
            "router: loaded",
            "没有前缀就别乱剥"
        );
        assert_eq!(
            strip_core_log_decoration("INFOrmation about the tunnel"),
            "INFOrmation about the tunnel",
            "级别名只是正文开头的一截字母 → 不是前缀"
        );
        assert_eq!(
            strip_core_log_decoration("WARN[abcd] weird"),
            "WARN[abcd] weird",
            "方括号里不是数字 ⇒ 形状变了，别猜"
        );
        // 正文里的方括号内容不得被吃掉。
        assert_eq!(
            strip_core_log_decoration("ERROR[0003] dial tcp [::1]:443: refused"),
            "dial tcp [::1]:443: refused"
        );
    }

    /// 🔴 隐私锁下核日志转发下限：`SubscribeLog` 是全级别流，不设限就等于把隐私锁在生成侧堵住的
    /// 那条路从新流上放回来 —— 用户访问的域名会经本仓 sink 落进**不脱敏**的 `polaris.log`。
    ///
    /// 判据必须与生成侧同源（`LogLevel::effective`），否则两侧各自漂：这里断言的正是「同一条判据
    /// 在转发口上的投影」。
    ///
    /// **变异锁**：把隐私腿改成 `log::Level::Trace`（等于不设限）→ 第二、三条转红；
    /// 把非隐私腿改成 `Warn`（过度设限，常态下丢掉用户要看的 info/debug）→ 第一条转红。
    #[test]
    fn core_log_privacy_floor_matches_generation_side_effective_level() {
        use polaris_config_engine::user_config::LogLevel;
        // 非隐私：不设限（`log::Level` 的最啰嗦档）。
        assert_eq!(core_log_privacy_floor(false), log::Level::Trace);
        // 隐私：抬到 warn ⇒ info/debug/trace 的核行一律不转发。
        assert_eq!(core_log_privacy_floor(true), log::Level::Warn);
        assert!(
            log::Level::Info > core_log_privacy_floor(true)
                && log::Level::Debug > core_log_privacy_floor(true)
                && log::Level::Trace > core_log_privacy_floor(true),
            "隐私锁开启时，连接明细所在的 info/debug/trace 三档必须全部被下限挡掉"
        );
        assert!(
            log::Level::Warn <= core_log_privacy_floor(true)
                && log::Level::Error <= core_log_privacy_floor(true),
            "warn/error 仍要转发（隐私锁不是把排障能力也一起关掉）"
        );
        // 与生成侧同源：判据是 `LogLevel::effective(privacy)`，不是这里另写的一条阈值。
        assert_eq!(LogLevel::Debug.effective(true), LogLevel::Warn);
        assert_eq!(LogLevel::Debug.effective(false), LogLevel::Debug);
    }

    /// ANSI 剥除自身：无 ESC → 零分配借用；CSI 序列整段吞掉；孤立 ESC 不得把后文一起吃了。
    #[test]
    fn strip_ansi_handles_csi_and_degenerate_input() {
        assert!(matches!(strip_ansi("plain"), Cow::Borrowed("plain")));
        assert_eq!(strip_ansi("\u{1b}[1;31mred\u{1b}[0m tail"), "red tail");
        assert_eq!(
            strip_ansi("a\u{1b}b"),
            "ab",
            "孤立 ESC（非 CSI）只丢它自己，后文原样保留"
        );
    }

    /// A2/C13：日志两轴从裸 config JSON 读。
    /// 打断 `logLevel` 读取（回退恒 Info）→ 第一断言转红；打断 `disableLogFile` 读取 → 第二断言转红。
    #[test]
    fn log_axes_follow_config() {
        use polaris_config_engine::user_config::LogLevel;
        // logLevel 跟随（此前硬编码 Info 会让 warn/debug 全丢）。
        let (lvl, dis) = log_axes_from_config(&serde_json::json!({ "logLevel": "warn" }));
        assert_eq!(lvl, LogLevel::Warn, "logLevel 必须跟随 config，不得恒 Info");
        assert!(!dis, "未给 disableLogFile → false");
        // disableLogFile 跟随。
        let (lvl2, dis2) = log_axes_from_config(
            &serde_json::json!({ "logLevel": "debug", "disableLogFile": true }),
        );
        assert_eq!(lvl2, LogLevel::Debug);
        assert!(dis2, "disableLogFile=true 必须落地");
        // 缺省 / 非法字符串 → Info；disableLogFile 非 true 一律 false。
        let (lvl3, dis3) = log_axes_from_config(
            &serde_json::json!({ "logLevel": "bogus", "disableLogFile": "yes" }),
        );
        assert_eq!(lvl3, LogLevel::Info, "非法 logLevel → 默认 Info");
        assert!(!dis3, "disableLogFile 非布尔 true → false");
        // 空 config → (Info, false)。
        let (lvl4, dis4) = log_axes_from_config(&serde_json::json!({}));
        assert_eq!(lvl4, LogLevel::Info);
        assert!(!dis4);
    }

    /// 平台标签必须是 Node 约定（config-engine 的平台分支按此比较），不是 Rust 的 consts::OS。
    #[test]
    fn platform_tag_uses_node_convention() {
        let t = platform_tag();
        assert!(
            matches!(t, "linux" | "darwin" | "win32"),
            "platform_tag 必须映射为 Node 约定，得到 {t}"
        );
        // 绝不能把 Rust 名直接漏出去（漏了 config-engine 的 win32/darwin 分支会全落空）。
        assert_ne!(t, "macos");
        assert_ne!(t, "windows");
    }

    /// `cronet_available` 四象限：naive 可用性判定。累积式断言（不短路）便于变异验证——删掉
    /// 「|| platform=="darwin"」半式后，mac-arm64 与 mac-x64 两条**同时**列入失败（两架构都靠这半式；
    /// linux 两条不依赖，恒绿）。
    #[test]
    fn cronet_available_four_quadrants() {
        // (lib_exists, platform, arch, expected, label)
        let cases = [
            // macOS 两架构 cronet 静态编入内核 → 无 dylib 也可用（真机 bug 修复的核心断言）。
            (
                false,
                "darwin",
                "aarch64",
                true,
                "mac-arm64 静态编入 → 无 dylib 也须 true",
            ),
            (
                false,
                "darwin",
                "x86_64",
                true,
                "mac-x64 也静态编入 → 无 dylib 也须 true",
            ),
            // linux/win 按 libcronet 动态库落盘。
            (true, "linux", "x86_64", true, "linux 有 libcronet → true"),
            (
                false,
                "linux",
                "x86_64",
                false,
                "linux 无 libcronet → false",
            ),
            (true, "win32", "x86_64", true, "Windows 有 libcronet → true"),
            (
                false,
                "win32",
                "x86_64",
                false,
                "Windows 无 libcronet → false",
            ),
        ];
        let mut fails = Vec::new();
        for (lib, plat, arch, want, label) in cases {
            let got = cronet_available(lib, plat, arch);
            if got != want {
                fails.push(format!("{label}（期望 {want}，得到 {got}）"));
            }
        }
        assert!(
            fails.is_empty(),
            "cronet_available 四象限失败:\n  {}",
            fails.join("\n  ")
        );
    }

    #[test]
    fn cronet_probe_follows_the_actual_core_directory() {
        let dir = std::env::temp_dir().join(format!(
            "polaris-cronet-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let core = dir.join("sing-box.exe");
        std::fs::write(&core, b"CORE").unwrap();

        assert!(!cronet_lib_exists_beside_core(&core, "windows"));
        std::fs::write(dir.join("libcronet.dll"), b"CRONET").unwrap();
        assert!(
            cronet_lib_exists_beside_core(&core, "windows"),
            "Windows 必须按打包名 libcronet.dll 且只在实际核心同目录探测"
        );
        assert!(
            !cronet_lib_exists_beside_core(&dir.join("other/sing-box.exe"), "windows"),
            "配置根目录里有 DLL 不能替另一个核心目录冒充依赖可用"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_valid_srs_file_checks_magic() {
        let dir = std::env::temp_dir().join(format!(
            "polaris-srs-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.srs");
        std::fs::write(&good, b"SRS\x01\x02").unwrap();
        assert!(is_valid_srs_file(good.to_str().unwrap()));

        let bad = dir.join("bad.srs");
        std::fs::write(&bad, b"XXX\x01").unwrap();
        assert!(!is_valid_srs_file(bad.to_str().unwrap()));

        // 短于 3 字节 → false（read_exact 失败）。
        let short = dir.join("short.srs");
        std::fs::write(&short, b"SR").unwrap();
        assert!(!is_valid_srs_file(short.to_str().unwrap()));

        // 不存在 → false，不 panic。
        assert!(!is_valid_srs_file(dir.join("nope.srs").to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_core_binary_env_override_rejects_missing_file() {
        // 逃生门指向不存在的路径 → 明确报错，绝不静默回落 PATH（误起别的 sing-box 更糟）。
        temp_env_var(
            "POLARIS_SINGBOX_PATH",
            "/nonexistent/polaris/sing-box-xyz",
            || {
                let r = resolve_core_binary();
                assert!(r.is_err(), "指向不存在文件应 Err");
                assert!(r.unwrap_err().contains("POLARIS_SINGBOX_PATH"));
            },
        );
    }

    #[test]
    fn resolve_core_binary_env_override_accepts_real_file() {
        let f = std::env::temp_dir().join(format!(
            "polaris-fake-core-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::write(&f, b"#!/bin/sh\n").unwrap();
        let path = f.to_string_lossy().into_owned();
        temp_env_var("POLARIS_SINGBOX_PATH", &path, || {
            assert_eq!(resolve_core_binary().unwrap(), f);
        });
        let _ = std::fs::remove_file(&f);
    }

    /// **门：单测态起核只认注入的假核**（本门存在的理由见 [`ProxyRuntime::core_binary_for_start`]
    /// 的 cfg(test) 版文档——单测漏出真 sing-box 进程的那个坑）。
    ///
    /// 变异有牙（**两种环境都红**，这正是断固定文案而非 `is_err()` 的原因）：
    /// - cfg(test) 版 `core_binary_for_start` 删回 `resolve_core_binary()` → 装了核的机器
    ///   （mac 真机 / 跑过 `fetch-core.mjs` 的 CI）上返 `Ok(真核路径)`，第一条断言红；`resources/`
    ///   为空的机器上虽仍是 Err，但文案变「未找到 sing-box 二进制…」，同样红。
    /// - 顺手把注入腿也删了（恒 Err）→ 第二条断言红（门太紧会锁死所有需要假核的起核测试）。
    #[test]
    fn test_mode_start_refuses_real_core_unless_injected() {
        let (rt, dir) = test_runtime();
        assert_eq!(
            rt.core_binary_for_start()
                .expect_err("未注入假核 → 必须拒绝起核，绝不回落真核"),
            TEST_CORE_NOT_INJECTED
        );
        let fake = dir.join("fake-core");
        *rt.core_binary_override.lock().unwrap() = Some(fake.clone());
        assert_eq!(
            rt.core_binary_for_start().unwrap(),
            fake,
            "注入后必须照常放行（否则起核类测试全被锁死）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 串行化 env 改动（cargo test 同进程多线程；env 是进程全局态）。
    /// `POLARIS_SINGBOX_PATH` 等进程级 env 的测试串行化锁（模块级共享）：`temp_env_var` 与
    /// 「驱动真 start 但用 env 逼 resolve_core_binary 失败」的异步测试**必须共用同一把锁**，否则
    /// cargo 默认并行跑测时二者对同一 env var 打架 → 偶发假红/假绿。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_env_var(key: &str, val: &str, f: impl FnOnce()) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var(key).ok();
        // SAFETY 说明：本文件 forbid(unsafe_code)；set_var 在 2021 edition 为 safe fn。
        std::env::set_var(key, val);
        f();
        match old {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    /// [`fresh_test_dir`] 的目录名前缀（清扫器按它认领自己的垃圾，勿改成别的模块也在用的前缀）。
    const TEST_DIR_PREFIX: &str = "polaris-proxy-test-";

    /// 陈旧临时目录清扫（每个测试进程只跑一次）。
    ///
    /// 为什么需要：本 fixture 的目录靠各测试末尾的 `remove_dir_all` 自清，而 `assert!` 失败会 panic
    /// 在那行之前 —— 于是每次红都留一份，跨月累积到四位数（实测某台机 `/tmp` 里 1998 个）。
    /// 与其给上百处调用点改成 Drop 守卫（返回类型全变），不如在开跑时把**上一轮的**残留扫掉：
    /// 稳态从「无限累积」变成「至多一轮的量」。
    ///
    /// 两道安全闸，缺一不可：① 只删 [`TEST_DIR_PREFIX`] 前缀的**目录**（本 fixture 自己造的名字）；
    /// ② 只删 mtime 早于 1 小时的 —— 同机并发跑另一个测试进程时，它的目录还是新的，绝不误删。
    /// 全程 best-effort：清扫失败不影响任何测试。
    fn sweep_stale_test_dirs() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
                return;
            };
            let cutoff = std::time::SystemTime::now() - Duration::from_secs(3600);
            for e in entries.flatten() {
                if !e.file_name().to_string_lossy().starts_with(TEST_DIR_PREFIX) {
                    continue;
                }
                let stale = e
                    .metadata()
                    .is_ok_and(|m| m.is_dir() && m.modified().is_ok_and(|t| t < cutoff));
                if stale {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        });
    }

    /// 唯一临时目录（纳秒时戳去重）。首次调用顺带清掉上一轮的残留（见 [`sweep_stale_test_dirs`]）。
    /// decoy 覆盖清单：缺文件 → 内置；有清单 → **替换**内置（不是并集）。
    ///
    /// 第二段同时守住**路径拼装**：目录名或文件名写错的话，`load_decoy_set` 会静默恒用内置表
    /// （用户把清单放进去却毫无反应，且日志只说「未提供」），这条断言让那种漂移转红。
    #[test]
    fn decoy_override_replaces_builtin_and_falls_back_when_absent() {
        let dir = fresh_test_dir();

        let builtin = ProxyRuntime::load_decoy_set(&dir);
        assert!(
            builtin.contains(&[31, 13, 95, 169]),
            "无覆盖清单 → 必须是内置表"
        );

        let rr = dir.join("rule-resource");
        std::fs::create_dir_all(&rr).unwrap();
        std::fs::write(rr.join("gfw-decoy-cidr.txt"), "1.2.0.0/16\n").unwrap();
        let over = ProxyRuntime::load_decoy_set(&dir);
        assert!(
            over.contains(&[1, 2, 3, 4]),
            "覆盖清单必须生效（含路径拼对）"
        );
        assert!(
            !over.contains(&[31, 13, 95, 169]),
            "替换语义：内置段必须失效，并集会把误杀写死"
        );

        // 空清单当故障 → 回落内置（不当「关掉过滤」）。
        std::fs::write(rr.join("gfw-decoy-cidr.txt"), "# 只有注释\n").unwrap();
        assert!(
            ProxyRuntime::load_decoy_set(&dir).contains(&[31, 13, 95, 169]),
            "空清单必须回落内置"
        );
    }

    fn fresh_test_dir() -> PathBuf {
        sweep_stale_test_dirs();
        let dir = std::env::temp_dir().join(format!(
            "{TEST_DIR_PREFIX}{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 系统代理清理收口 mock：只**记录调用次数**，不触碰宿主系统代理（本机硬约束：绝不真跑
    /// `networksetup`/`gsettings`/`reg`）。用于验「start 真失败 → controller 真被调」这条组合路径。
    struct RecordingClearer {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl SystemProxyClearer for RecordingClearer {
        fn ensure_cleared(&mut self) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // mock 无真实 marker → 模拟「无需动作」返回 false（幂等 no-op 的对外可见形态）。
            false
        }
        fn detect_foreign_proxy(&self) -> Option<String> {
            None // 默认无残留；要验提示腿的测试用 `ResidualClearer`。
        }
        fn enable_system_proxy(&mut self, _req: &ProxyEnableRequest) -> Result<(), String> {
            Ok(()) // 本 mock 只验清理腿；启用/恢复腿的记录用 `EnableRecordingClearer`。
        }
        fn recover_from_marker(&mut self) -> bool {
            false
        }
    }

    /// 「检测到别人的系统代理」mock：detect 恒返固定 host:port（不触碰宿主系统）。
    struct ResidualClearer {
        found: Option<String>,
    }
    impl SystemProxyClearer for ResidualClearer {
        fn ensure_cleared(&mut self) -> bool {
            false
        }
        fn detect_foreign_proxy(&self) -> Option<String> {
            self.found.clone()
        }
        fn enable_system_proxy(&mut self, _req: &ProxyEnableRequest) -> Result<(), String> {
            Ok(())
        }
        fn recover_from_marker(&mut self) -> bool {
            false
        }
    }

    /// 启用/恢复侧记录 mock：记录 `enable` 收到的 `req` + `recover_from_marker` 调用次数（不触碰宿主
    /// 系统代理，本机硬约束）。用于验「systemProxy start 成功腿 → `enable` 真被调、参数正确」+「启动期
    /// → `recover_from_marker` 真被调」这两条**组合路径**（§K7.1：光有函数、光有调用点都不够）。
    #[derive(Default)]
    struct EnableRecordingClearer {
        enable_reqs: Arc<Mutex<Vec<ProxyEnableRequest>>>,
        recover_calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl SystemProxyClearer for EnableRecordingClearer {
        fn ensure_cleared(&mut self) -> bool {
            false
        }
        fn detect_foreign_proxy(&self) -> Option<String> {
            None
        }
        fn enable_system_proxy(&mut self, req: &ProxyEnableRequest) -> Result<(), String> {
            self.enable_reqs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(req.clone());
            Ok(())
        }
        fn recover_from_marker(&mut self) -> bool {
            self.recover_calls.fetch_add(1, Ordering::SeqCst);
            true // 模拟「发现残留 marker 并恢复」→ 方法应回传 true。
        }
    }

    /// 造一个用临时配置目录的 ProxyRuntime（不起核）。
    ///
    /// 系统代理清理收口器用**真实生产控制器** + 临时目录 marker 路径（无 marker → 门控 1 即返、零系统
    /// 调用 → 本机安全）。不预置 config.json —— 首次 `current()` 自会建默认配置。
    fn test_runtime() -> (Arc<ProxyRuntime>, PathBuf) {
        let dir = fresh_test_dir();
        let config = Arc::new(ConfigManager::new(dir.clone()));
        // 替身 helper（恒未装）：见 `HelperRuntime::never_installed_for_tests` —— 用 `new` 会让
        // 下面所有 helper 门的绿取决于跑测机器装没装过 Polaris，且装了会真连特权 daemon。
        let helper = Arc::new(HelperRuntime::never_installed_for_tests(dir.clone()));
        let mesh = Arc::new(MeshRuntime::new(dir.clone()));
        let clearer: Box<dyn SystemProxyClearer> =
            Box::new(polaris_system_integration::production_proxy_controller(
                dir.join(polaris_system_integration::PROXY_MARKER_FILENAME)
                    .to_string_lossy()
                    .into_owned(),
            ));
        (
            Arc::new(ProxyRuntime::new(
                config,
                helper,
                mesh,
                clearer,
                Arc::new(NoNetworkDoh),
            )),
            dir,
        )
    }

    /// row33：DNS 热插拔重灌门控——仅「当前 TUN + 用户未关接管开关 + 有接管 marker」三条同时成立才放行。
    /// 变异有牙：删 `is_tun` 分支 → (false,·,true) 转真 → 转红（非 TUN 也重灌，误改已切走的系统 DNS）；
    /// 删 `has_marker` 分支 → (true,·,false) 转真 → 转红（无接管却擅自灌 DNS）；
    /// 把 `takeover` 参数重新写死成 `None`（本轮修的正是这个）→ `Some(false)` 那条转真 → 转红
    /// （用户关掉接管后，watcher 仍会在每次链路变化时把系统 DNS 重新抢回来）。
    #[test]
    fn dns_reconcile_gate_only_tun_with_marker() {
        assert!(
            ProxyRuntime::dns_reconcile_should_run(true, None, true),
            "TUN + 开关缺省（未显式关） + marker → 放行"
        );
        assert!(
            ProxyRuntime::dns_reconcile_should_run(true, Some(true), true),
            "开关显式开 → 放行"
        );
        assert!(
            !ProxyRuntime::dns_reconcile_should_run(true, Some(false), true),
            "用户显式关掉 takeoverSystemDns → 即便 TUN + marker 也不得重灌（此前该开关是装饰）"
        );
        assert!(
            !ProxyRuntime::dns_reconcile_should_run(false, None, true),
            "切走 TUN（虽 marker 在）→ 不重灌"
        );
        assert!(
            !ProxyRuntime::dns_reconcile_should_run(true, None, false),
            "无接管 marker → 不擅自灌 DNS"
        );
        assert!(!ProxyRuntime::dns_reconcile_should_run(false, None, false));
    }

    /// C7 用户开关的**原始 JSON 三态读取**（`dnsConfig.takeoverSystemDns` 不在 `DnsConfig` 结构体里）。
    ///
    /// 变异有牙：把路径写成顶层 `takeoverSystemDns`（漏 `dnsConfig` 一层）→ 第一条转红；
    /// 把返回折成 bool（`unwrap_or(true)`）→ 「缺省」与「显式 true」不可区分 → 第三条的 `None` 断言转红；
    /// 用 `as_bool` 之外的宽松解析（如把字符串 `"false"` 也当 false）→ 第四条转红。
    #[test]
    fn dns_takeover_switch_reads_three_states_from_raw_json() {
        assert_eq!(
            dns_takeover_enabled(&serde_json::json!({
                "dnsConfig": { "takeoverSystemDns": false }
            })),
            Some(false),
            "显式关 → Some(false)（唯一会拦下接管的取值）"
        );
        assert_eq!(
            dns_takeover_enabled(&serde_json::json!({
                "dnsConfig": { "takeoverSystemDns": true }
            })),
            Some(true)
        );
        assert_eq!(
            dns_takeover_enabled(&serde_json::json!({ "dnsConfig": {} })),
            None,
            "缺省 = 未表态（≠ 显式 true），下游按 `!= Some(false)` 判开"
        );
        assert_eq!(
            dns_takeover_enabled(&serde_json::json!({
                "dnsConfig": { "takeoverSystemDns": "false" }
            })),
            None,
            "非布尔一律 None（对齐 上游 validateConfig 布尔口径），绝不把字符串 \"false\" 当关"
        );
        assert_eq!(dns_takeover_enabled(&serde_json::json!({})), None);
    }

    /// **接线守卫**：起核尾的 DNS 接管门必须同时看 `is_tun` 与 `dns_takeover`，且 else 腿必须还原。
    ///
    /// 为何用源码扫描而非行为测试：`start_inner` 要真起核（真机门），而 DNS 接管在本机 Linux 是
    /// `takeover_supported=false` 的全 no-op —— 把 `&& dns_takeover != Some(false)` 删掉，本机
    /// **零症状**，只有 mac 真机才炸。断言用**连续片段**（含缩进与 else 腿全文）而不是逐条 `contains`：
    /// 后者会被同 impl 块里别处的同名调用（`stop_inner` 也调 `restore_system_dns_best_effort`）假绿放行。
    #[test]
    fn start_leg_dns_takeover_gate_reads_the_switch() {
        const SRC: &str = include_str!("proxy.rs");
        assert!(
            SRC.contains("let dns_takeover = dns_takeover_enabled(&config);"),
            "start_inner 必须在 config 被 move 进 startup_snapshot 之前取一次 takeoverSystemDns 活态"
        );
        // 连续片段：门的合取形态 + 接管两步 + else 腿的两步还原，一个字都不能少。
        const GATE: &str = "\
        if user_config.proxy_mode_type.is_tun() && dns_takeover != Some(false) {
            self.set_system_dns_best_effort().await;";
        const ELSE_LEG: &str = "\
        } else {
            self.stop_dns_watcher();
            self.restore_system_dns_best_effort().await;
        }";
        assert!(
            SRC.contains(GATE),
            "起核尾 DNS 接管门必须是「TUN 且用户未显式关」的合取（1:1 上游 ProxyManager.ts:1103）"
        );
        assert!(
            SRC.contains(ELSE_LEG),
            "else 腿必须停 watcher + 还原残留受控 DNS（覆盖 TUN→其它模式 / 开→关 两种切换）——\
             少了它，用户关掉接管开关后系统解析器还不回来"
        );
    }

    /// 同 [`test_runtime`]，但收口器换成 [`RecordingClearer`] mock，额外返回其调用计数句柄——
    /// 用于断言「失败腿是否真调到了 ensure_cleared」。
    fn test_runtime_recording() -> (
        Arc<ProxyRuntime>,
        PathBuf,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let dir = fresh_test_dir();
        let config = Arc::new(ConfigManager::new(dir.clone()));
        let helper = Arc::new(HelperRuntime::never_installed_for_tests(dir.clone()));
        let mesh = Arc::new(MeshRuntime::new(dir.clone()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let clearer: Box<dyn SystemProxyClearer> = Box::new(RecordingClearer {
            calls: Arc::clone(&calls),
        });
        (
            Arc::new(ProxyRuntime::new(
                config,
                helper,
                mesh,
                clearer,
                Arc::new(NoNetworkDoh),
            )),
            dir,
            calls,
        )
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // C11 DNS race sidecar 注入面（`generate_deps` 据运行期 race_server 状态喂 config-engine）。
    //
    // 生成侧（port>0 → dns-node-race server；race off → withRaceOff 单上游）已由 config-engine
    // `builder::dns` / `builder::generate` 单测覆盖；此处专测**注入接线**：`generate_deps` 是否真把
    // ProxyRuntime.race_server 状态透传下去。变异验证：把 generate_deps 里 race 两轴改回硬编码 0/`[]`
    // 会让 `injects_positive_port` 转红。
    // ══════════════════════════════════════════════════════════════════════════════

    #[test]
    fn race_server_default_is_off_zero_port() {
        let (rt, _dir) = test_runtime();
        assert_eq!(rt.race_server_port(), 0, "未起 sidecar → race off");
        let deps = rt.generate_deps(9090, 0, &[], &serde_json::json!({}));
        assert_eq!(deps.race_server_port, 0, "注入面回落 0（race off）");
        assert!(
            deps.race_upstream_ips.is_empty(),
            "race off → 无上游直连放行"
        );
        assert!(
            deps.race_upstream_ports.is_empty(),
            "race off → 端口轴同样空（route 端口集回 [53,443] 基线，金样不动）"
        );
    }

    #[test]
    fn race_server_injects_positive_port_and_upstreams() {
        let (rt, _dir) = test_runtime();
        // 模拟 sidecar 起成功回调（真起 sidecar 属真机门；此处只验注入接线）。
        rt.set_race_server(
            5353,
            vec!["1.1.1.1".into(), "8.8.8.8".into()],
            vec![443, 8443],
        );
        assert_eq!(rt.race_server_port(), 5353);
        let deps = rt.generate_deps(9090, 0, &[], &serde_json::json!({}));
        assert_eq!(
            deps.race_server_port, 5353,
            "端口须透传进 GenerateConfigDeps"
        );
        assert_eq!(
            deps.race_upstream_ips,
            vec!["1.1.1.1".to_string(), "8.8.8.8".to_string()],
            "上游直连 IP 须透传（route 直连放行防 TUN 回环）"
        );
        assert_eq!(
            deps.race_upstream_ports,
            vec![443u16, 8443],
            "上游端口须与 IP 同轴透传（缺端口 → ip_cidr×port 规则匹配不上，issue #147）"
        );
        // clear → 回落 race off。
        rt.clear_race_server();
        let deps2 = rt.generate_deps(9090, 0, &[], &serde_json::json!({}));
        assert_eq!(deps2.race_server_port, 0);
        assert!(deps2.race_upstream_ips.is_empty());
        assert!(deps2.race_upstream_ports.is_empty(), "清理须两轴一起翻");
    }

    /// 把 `dnsConfig` 片段塞进最小 UserConfig（起 sidecar 只读 dnsConfig；`servers` 无 serde default，必带）。
    fn user_config_with_dns(dns: serde_json::Value) -> UserConfig {
        serde_json::from_value(serde_json::json!({ "servers": [], "dnsConfig": dns }))
            .expect("最小 UserConfig")
    }

    /// 【不变式：竞速 off 不走池】总开关关 → **不起 sidecar** → 注入面恒 (0, []) →
    /// config-engine `with_race_off` 走 `nodeResolverSingle` 单上游。
    ///
    /// 变异验证：删掉 `start_race_sidecar` 里的 `plan_upstreams(..) else { return }` 早退
    /// （或把 `plan_upstreams` 的 `resolve_node_domains_ahead == Some(false)` 判断去掉）→
    /// sidecar 会照起、端口 >0 → 本测试转红。
    #[tokio::test]
    async fn race_off_starts_no_sidecar_and_keeps_generate_deps_at_zero() {
        let (rt, _dir) = test_runtime();
        rt.start_race_sidecar(
            &user_config_with_dns(serde_json::json!({
                "resolveNodeDomainsAhead": false,
                // 池里塞满上游也不该生效 —— 总开关优先级高于上游选择。
                "nodeResolverPool": ["ali", "dnspod", "system"],
            })),
            rt.gate.generation(),
        )
        .await;
        assert_eq!(rt.race_server_port(), 0, "竞速关 → 端口恒 0");
        let deps = rt.generate_deps(9090, 0, &[], &serde_json::json!({}));
        assert_eq!(deps.race_server_port, 0);
        assert!(
            deps.race_upstream_ips.is_empty(),
            "竞速关 → 不放行任何上游直连"
        );
        assert!(deps.race_upstream_ports.is_empty(), "竞速关 → 端口轴同样空");
    }

    /// 竞速开（含缺省）→ sidecar 真绑回环口，端口与自定义上游的 **IP + 端口两轴**一并进
    /// `GenerateConfigDeps`。
    ///
    /// **只绑 127.0.0.1、不发任何真实上游查询**（DoH 走 [`NoNetworkDoh`] 桩，池里不含 system）。
    ///
    /// 自定义上游刻意用**非标端口** `:8443` —— 这正是 issue #147 的形态：端口若不随 IP 下发，
    /// route 只放行 IP、端口集仍是 `[53,443]`，规则匹配不上 ⇒ TUN 下该上游经代理出站/回环。
    ///
    /// **变异锁**：把 `generate_deps` 的 `race_upstream_ports` 改回硬编码 `vec![]`（或删掉
    /// `commit_race_sidecar` 里 `state.upstream_ports = …` 那行）→ `8443` 断言转红。
    #[tokio::test]
    async fn race_on_starts_sidecar_and_feeds_port_and_custom_upstream_ips() {
        let (rt, _dir) = test_runtime();
        rt.start_race_sidecar(
            &user_config_with_dns(serde_json::json!({
                "nodeResolverPool": ["ali", "my-doh"],
                "nodeResolverCustom": [{ "id": "my-doh", "spec": "https://9.9.9.9:8443/dns-query" }],
            })),
            rt.gate.generation(),
        )
        .await;
        let port = rt.race_server_port();
        assert!(port > 0, "竞速开 → sidecar 应绑到回环口");
        let deps = rt.generate_deps(9090, 0, &[], &serde_json::json!({}));
        assert_eq!(deps.race_server_port, port, "端口须与 sidecar 实际监听一致");
        assert!(
            deps.race_upstream_ips.contains(&"9.9.9.9".to_string()),
            "自定义上游 IP 须进 route 直连放行（否则 TUN 下 sidecar 的 DoH 会回环）：{:?}",
            deps.race_upstream_ips
        );
        assert!(
            deps.race_upstream_ports.contains(&8443),
            "自定义上游的**非标端口**须与 IP 同轴下发（issue #147）：{:?}",
            deps.race_upstream_ports
        );
        assert!(
            deps.race_upstream_ports.contains(&443),
            "内置 ali 的 :443 也在真实上游集里：{:?}",
            deps.race_upstream_ports
        );
        // 停 → 端口与放行清零（生成侧回落单上游）。
        rt.clear_race_server();
        assert_eq!(rt.race_server_port(), 0);
        let deps = rt.generate_deps(9090, 0, &[], &serde_json::json!({}));
        assert!(
            deps.race_upstream_ports.is_empty(),
            "停 sidecar 后端口轴须一起清（否则 config 会放行一个已无人使用的端口）"
        );
    }

    /// **INV-1 接线门**：起 sidecar 时 `proxy_mode_type` 必须从**本轮起核的那份配置**透传进
    /// `plan_upstreams`，否则 TUN 下 `system` 照样入池（= 那条 hijack-dns 自递归放大链原样复活）。
    ///
    /// # 为什么只能是源码型判据
    ///
    /// `system` 上游**没有 IP** ⇒ 不进 `direct_ips` ⇒ 不进 `race_upstream_ips`；而 `NodeDnsRaceServer`
    /// 不暴露它拿到的上游集。故「池里到底有没有 system」在 src-tauri 这一层**没有任何可观测出口** ——
    /// 摘不摘 system，`race_server_port` / `generate_deps` 逐字节相同。摘除逻辑本身的行为覆盖在
    /// `polaris_dns_race::upstream` 的四条单测里（正向 + 反向 + 回退闸 + 自定义 UDP 保留），
    /// 本门只负责钉住「那个决策点真的收到了模式」这一条接线事实。
    ///
    /// **变异锁**：把参数写死成 `ProxyModeType::SystemProxy`（或改从 `self.current_config` 取 ——
    /// 起核这一刻它还是**上一轮**的配置，「刚切到 TUN」的第一轮就会漏筛）→ `contains` 落空 → 转红。
    #[test]
    fn start_race_sidecar_threads_proxy_mode_type_into_plan_upstreams() {
        let body = method_body(
            include_str!("proxy.rs"),
            "    async fn start_race_sidecar(self: &Arc<Self>, user_config: &UserConfig, my_gen: u64) {",
        );
        assert!(
            body.contains(
                "plan_upstreams(user_config.dns_config.as_ref(), user_config.proxy_mode_type)"
            ),
            "起 sidecar 的唯一决策点必须收到**本轮**配置的 proxy_mode_type（INV-1 的判据）；\
             实际方法体：\n{body}"
        );
    }

    /// 重复起核：端口按新配置重建，且旧 sidecar 不留孤儿（新旧端口不同即证明旧的已被换掉）。
    #[tokio::test]
    async fn restarting_sidecar_replaces_the_previous_one() {
        let (rt, _dir) = test_runtime();
        let on = serde_json::json!({ "nodeResolverPool": ["ali", "dnspod"] });
        rt.start_race_sidecar(&user_config_with_dns(on.clone()), rt.gate.generation())
            .await;
        let p1 = rt.race_server_port();
        assert!(p1 > 0);
        // 第二次起核把竞速关掉 → 必须把上一个 sidecar 收掉、端口归 0（不是留着旧的继续跑）。
        rt.start_race_sidecar(
            &user_config_with_dns(serde_json::json!({ "resolveNodeDomainsAhead": false })),
            rt.gate.generation(),
        )
        .await;
        assert_eq!(rt.race_server_port(), 0, "改配置后旧 sidecar 必须被收掉");
    }

    /// 🔴 **C11 sidecar 的世代所有权守卫**：被更新的 start 接管的旧起核腿，一个字节都不许碰接管方的 sidecar。
    ///
    /// # 复现的真机时序
    ///
    /// `start_race_sidecar` 位于 `start_inner` 的两个**分钟级** await 之后（`run_helper_gate` 的 helper
    /// 授权弹窗、`capture_tun_route_baseline`），而轮首让位检查在它**之后** —— 被接管的旧腿醒来一定会
    /// 走到它。A 停在弹窗 → 用户从托盘再触发 start B（B 起核完成，config 烧的是 P_B）→ 用户点掉 A 的
    /// 弹窗 → 无守卫时 A 先停掉 S_B、再把 P_A 烧进注入态，而 B 的核照着 config 去查 P_B（已无人监听）
    /// ⇒ 节点域名解析静默 SERVFAIL，同时 S_A 成孤儿监听。
    ///
    /// # 变异锁（三道守卫逐条，删任一条都有一条断言转红）
    ///
    /// - `start_race_sidecar` 入口改回无条件 `clear_race_server()` → ② 转红（S_B 被停）；
    /// - `clear_race_server_owned_by` 删掉锁内世代判据 → ③ 转红；
    /// - `commit_race_sidecar` 删掉锁内世代判据 → ④ 转红（旧腿端口盖掉接管方的）；
    /// - `maybe_stop_race_sidecar_on_start_failure` 把守卫改回锁外先判后清、或干脆删掉 → ⑤ 转红。
    ///
    /// ⑥ 是**正向对照**：守卫不得退化成「谁都拒」（那样等于 sidecar 永远起不来，同样是静默失效）。
    #[tokio::test]
    async fn superseded_start_leg_must_not_touch_the_takeover_sidecar() {
        let (rt, _dir) = test_runtime();
        let on = serde_json::json!({ "nodeResolverPool": ["ali", "dnspod"] });
        // ① A 先快照世代（此后停在 helper 弹窗）；B 接管（bump）并起好自己的 sidecar。
        let gen_a = rt.gate.generation();
        let gen_b = rt.bump_generation();
        rt.start_race_sidecar(&user_config_with_dns(on.clone()), gen_b)
            .await;
        let port_b = rt.race_server_port();
        assert!(port_b > 0, "接管方 B 的 sidecar 应已就绪");

        // ② A 醒来跑完整条 start_race_sidecar。
        rt.start_race_sidecar(&user_config_with_dns(on.clone()), gen_a)
            .await;
        assert_eq!(
            rt.race_server_port(),
            port_b,
            "被接管的旧腿不得停掉/替换接管方的 sidecar（否则 B 的核对死口做节点域名解析）"
        );

        // ③ 旧腿直调收口口（起核失败腿的形态）→ 锁内判权必须拒。
        assert!(
            !rt.clear_race_server_owned_by(Some(gen_a)),
            "旧世代 clear 必须返 false 且不翻转状态"
        );
        assert_eq!(rt.race_server_port(), port_b);

        // ④ 旧腿直调提交口（绑口 await 之后才失去当权的形态）→ 拒绝，`srv` 就地 drop 不留孤儿监听。
        let cfg = user_config_with_dns(on);
        let ups = plan_upstreams(cfg.dns_config.as_ref(), cfg.proxy_mode_type)
            .expect("竞速开 → 应有上游计划");
        let query = Arc::new(DefaultUpstreamQuery::new(Arc::clone(&rt.doh)));
        let srv = NodeDnsRaceServer::start(
            ups,
            query,
            DEFAULT_RACE_BUDGET,
            None,
            Arc::new(DecoySet::builtin()),
        )
        .await
        .expect("绑回环口");
        let stale_port = srv.port();
        assert_ne!(stale_port, port_b, "两个 sidecar 应绑到不同的临时口");
        assert_eq!(
            rt.commit_race_sidecar(srv, vec!["1.1.1.1".into()], vec![443], gen_a),
            0,
            "旧世代提交必须被拒（返 0 = 降级单上游）"
        );
        assert_eq!(
            rt.race_server_port(),
            port_b,
            "注入态必须仍指向接管方的端口，绝不能被旧腿的端口盖掉"
        );

        // ⑤ 旧腿的起核失败收口（`maybe_stop_race_sidecar_on_start_failure`）同样不得动手。
        rt.maybe_stop_race_sidecar_on_start_failure(
            &Err(StartError::from("起核失败".to_string())),
            gen_a,
        );
        assert_eq!(
            rt.race_server_port(),
            port_b,
            "被接管的失败腿不得收掉接管方的 sidecar（比不收更糟）"
        );

        // ⑥ 正向对照：当权者（B）自己收口仍必须真生效，否则守卫退化成「谁都拒」。
        assert!(rt.clear_race_server_owned_by(Some(gen_b)));
        assert_eq!(rt.race_server_port(), 0);
    }

    /// 取本运行时当前 sidecar 上**真正注册**的死亡回调。
    ///
    /// 为什么不直接调 `rt.race_dead_callback(g)` 自己造一只：那样测的只是回调体，测不到
    /// **生产调用点是否真把它传给了 sidecar**（本轮修的正是这条被并行编辑覆盖掉的接线）。
    /// 从 `race_sidecar` 里取，链路才是「`start_race_sidecar` → `NodeDnsRaceServer::start` → 回调」。
    ///
    /// 真让 watchdog 死需要在 sidecar 之外占死它的端口，而端口是 OS 现分配、拿到时 socket 已在监听 ——
    /// 无可控复现路径，故直接触发注册的回调（crate 侧「耗尽 → 必触发回调」另有单测
    /// `watchdog_gives_up_zeroes_live_port_and_fires_on_dead` 锁死）。
    fn registered_dead_callback(rt: &Arc<ProxyRuntime>) -> Option<OnRaceServerDead> {
        rt.race_sidecar
            .lock()
            .expect("race_sidecar 锁")
            .as_ref()
            .and_then(NodeDnsRaceServer::dead_callback)
    }

    /// 🔴 **C11 sidecar 死亡回调的生产接线**：watchdog 彻底放弃重建 → 注入态必须被清（回落单上游）。
    ///
    /// 不清的后果：`live_port=0` 只有 sidecar 自己知道，注入态仍是那个死端口 ⇒ 此后每次 config
    /// 重生成都继续把内核指向没人听的口，节点域名解析全部**静默** SERVFAIL。
    ///
    /// # 变异锁
    /// - `start_race_sidecar` 把 `Some(on_dead)` 改回 `None`（本轮修复前的状态）→ 取不到回调 → 转红；
    /// - 回调体里删掉 `clear_race_server_owned_by` 调用 → 端口不归零 → 转红。
    #[tokio::test]
    async fn race_sidecar_dead_callback_clears_injected_state() {
        let (rt, _dir) = test_runtime();
        let my_gen = rt.gate.generation();
        rt.start_race_sidecar(
            &user_config_with_dns(serde_json::json!({ "nodeResolverPool": ["ali", "dnspod"] })),
            my_gen,
        )
        .await;
        let port = rt.race_server_port();
        assert!(port > 0, "竞速开 → sidecar 应绑到回环口");

        let on_dead = registered_dead_callback(&rt)
            .expect("生产调用点必须给 sidecar 装死亡回调（传 None = sidecar 死了注入态仍指死口）");
        on_dead(port);

        assert_eq!(
            rt.race_server_port(),
            0,
            "sidecar 死亡 → 注入态必须归 0，否则内核继续对死口做节点域名解析（静默 SERVFAIL）"
        );
        let deps = rt.generate_deps(9090, 0, &[], &serde_json::json!({}));
        assert_eq!(deps.race_server_port, 0, "生成侧须随之回落单上游");
        assert!(deps.race_upstream_ips.is_empty());
    }

    /// 🔴 **死亡回调也受世代守卫**：A 腿的 sidecar 死在 B 接管之后 → 回调只许让位，不许清 B 的注入态。
    ///
    /// 真机时序：A 起好 sidecar → 用户重连（B 接管、起自己的 sidecar，config 烧的是 P_B）→ A 那只
    /// 早已被停掉的 sidecar 的 watchdog 才耗尽重试并触发回调。无守卫时它会把 B 的注入态清成 0，
    /// 而 B 的核 config 里烧的是 P_B（sidecar 还活着）⇒ 白白降级，且下次重生成 config 时两边错配。
    ///
    /// # 变异锁
    /// - 回调里把 `clear_race_server_owned_by(Some(my_gen))` 换成无守卫的 `clear_race_server()`
    ///   （或传 `None`）→ B 的端口被清成 0 → 转红。
    #[tokio::test]
    async fn race_sidecar_dead_callback_yields_to_takeover() {
        let (rt, _dir) = test_runtime();
        let on = serde_json::json!({ "nodeResolverPool": ["ali", "dnspod"] });
        // A 起核 + 拿到 A 腿注册的回调（必须在 B 接管前取：B 会把 A 的 sidecar 收走）。
        let gen_a = rt.gate.generation();
        rt.start_race_sidecar(&user_config_with_dns(on.clone()), gen_a)
            .await;
        let port_a = rt.race_server_port();
        assert!(port_a > 0);
        let on_dead_a = registered_dead_callback(&rt).expect("A 腿 sidecar 必须装了死亡回调");

        // B 接管并起自己的 sidecar（注入态被合法替换成 P_B）。
        let gen_b = rt.bump_generation();
        rt.start_race_sidecar(&user_config_with_dns(on), gen_b)
            .await;
        let port_b = rt.race_server_port();
        assert!(port_b > 0, "接管方 B 的 sidecar 应已就绪");
        assert_ne!(port_b, port_a, "两腿应绑到不同的临时口");

        // A 的 watchdog 此刻才耗尽 → 回调触发。
        on_dead_a(port_a);
        assert_eq!(
            rt.race_server_port(),
            port_b,
            "被接管的旧腿死亡不得清掉接管方的注入态（B 的 sidecar 还活着，清了就是白降级）"
        );
    }

    /// 🔵 **降级 error 只许打在真清掉之后**（真机日志可信度）。
    ///
    /// 让位腿（世代已被接管）一个字节都没动 —— 若 error 打在 `clear_race_server_owned_by` 之前，
    /// 那么每一次「旧腿 sidecar 姗姗来迟地死掉」都会在日志里留下「已清注入态、降级单上游」，
    /// 而实际上接管方的 sidecar 好端端活着。排查节点域名 SERVFAIL 时最先被信的就是这一行，
    /// 它把一个健康会话误报成已降级，等于把排查引到反方向。
    ///
    /// 用源码扫描：回调体的日志分支没有可注入的观测点（`log` crate 无本仓 sink），而「谁先谁后」
    /// 是纯结构事实。**牙**：把 `log::error!` 挪回 `weak.upgrade()` / 判权之前 → 转红。
    #[test]
    fn race_dead_callback_logs_the_downgrade_only_after_it_really_cleared() {
        const SRC: &str = include_str!("proxy.rs");
        let body = method_body(
            SRC,
            "fn race_dead_callback(self: &Arc<Self>, my_gen: u64) -> OnRaceServerDead {",
        );
        let cleared = body
            .find("if rt.clear_race_server_owned_by(Some(my_gen))")
            .expect("死亡回调必须经锁内判权腿清理（变异锁：见 dead_callback_yields_to_takeover）");
        let downgrade = body
            .find("log::error!")
            .expect("彻底失效仍必须留一条 error —— 降级是用户可感知故障，不能只有 info");
        assert!(
            cleared < downgrade,
            "降级 error 必须在判权+清理**之后**：让位腿什么都没动，先打 error 会把健康的接管会话\
             在真机日志里误报成已降级单上游"
        );
        assert!(
            body.find("weak.upgrade()").expect("必须持 Weak 升级") < downgrade,
            "运行时已析构时同样不该打降级 error（此刻没有任何会话受影响）"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // switch_mode 三腿决策（**生产路径**：全部经 `ProxyRuntime::switch_mode` 入口，
    // 不直接调 `decide` / `SwitchExecutor` ——§K7.1 的教训是「两扇门之间的缝才是生产路径」，
    // 故这里一律从生产入口打，断言它真的落到了预期的腿上。）
    // ══════════════════════════════════════════════════════════════════════════════

    /// 造一个 shadowsocks 节点（地址指向 127.0.0.1 的死端口：核只需能**生成 outbound**，不需真连）。
    fn ss_node(id: &str, name: &str, port: u16) -> Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "protocol": "shadowsocks",
            "address": "127.0.0.1",
            "port": port,
            "shadowsocksSettings": {
                "method": "aes-128-gcm",
                "password": "polaris-test",
            },
        })
    }

    /// 同 [`two_node_config`]，但两个节点的地址端口可指定（真机验证要把节点指到**我们自己的**
    /// 本地监听器上，从而直接观测核到底拨了谁——比读日志可靠得多，也不依赖全局 logger）。
    ///
    /// **两份 config 之间 `pa`/`pb` 必须保持一致**：节点地址进 norm，改了它 norm 就变 →
    /// plan_hot_switch 的前提失败 → 退回重启，热切换测试自我拆台。
    fn two_node_config_ports(mixed: u16, selected: &str, pa: u16, pb: u16) -> Value {
        let mut cfg = polaris_store::default_config();
        let obj = cfg.as_object_mut().unwrap();
        obj.insert(
            "servers".into(),
            serde_json::json!([
                ss_node("node-a", "Node A", pa),
                ss_node("node-b", "Node B", pb)
            ]),
        );
        obj.insert("selectedServerId".into(), serde_json::json!(selected));
        obj.insert("proxyMode".into(), serde_json::json!("global"));
        // 安全硬约束：绝不可改成 tun/systemProxy（会破坏工作机网络）。
        obj.insert("proxyModeType".into(), serde_json::json!("manual"));
        obj.insert("mixedPort".into(), serde_json::json!(mixed));
        cfg
    }

    /// 起一个只统计「被连了几次」的本地 TCP 监听器（不说 SS 协议——核**拨过来**这一事实本身
    /// 就是路由证据；握手成不成功无关紧要）。
    async fn counting_listener() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::AtomicUsize;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                h.fetch_add(1, Ordering::SeqCst);
                drop(s); // 立刻断开：核会报连接失败，但「它拨了我」已被记下。
            }
        });
        (port, hits)
    }

    /// 经混合入站发一个 HTTP 代理请求（目标 192.0.2.1 = RFC 5737 TEST-NET-1：非私网 →
    /// 不命中私网直连规则；IP 字面量 → 不触发 DNS 查询 → 零外部流量）。
    async fn drive_traffic_through_proxy(mixed: u16) {
        use tokio::io::AsyncWriteExt;
        if let Ok(mut c) = tokio::net::TcpStream::connect(("127.0.0.1", mixed)).await {
            let _ = c
                .write_all(b"GET http://192.0.2.1/ HTTP/1.1\r\nHost: 192.0.2.1\r\n\r\n")
                .await;
            let _ = c.flush().await;
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
    }

    /// 两节点 + 指定选中节点的本地安全配置（节点指向本地死端口，纯决策类测试用）。
    fn two_node_config(mixed: u16, selected: &str) -> Value {
        two_node_config_ports(mixed, selected, 18001, 18002)
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // P1 契约收口：pending_changes() = {added, modified, removed}
    //
    // 这批测试的分母（R5）：`pending_changes()` 此前 **零覆盖**。先钉住四态基线，再谈「改对了」——
    // 否则改完无法区分「修好了」与「换了个错法」。
    // ══════════════════════════════════════════════════════════════════════════════

    /// 装一副「起核快照」：`startup_snapshot`（id 集基准）+ `switch_snapshot`（指纹基准）。
    /// 二者在生产的起核就绪腿相隔 8 行同置，是同刻同源的孪生对 —— 测试里也必须同置，
    /// 只装一半会造出生产中不可达的形态。
    fn install_startup_snapshot(rt: &ProxyRuntime, cfg: &Value) {
        let uc: UserConfig = serde_json::from_value(cfg.clone()).expect("测试配置应可解析");
        *rt.startup_snapshot.write().unwrap() = Some(cfg.clone());
        *rt.switch_snapshot.write().unwrap() = Some(SwitchSnapshot {
            fingerprints: node_fingerprints::modified_table(&uc.servers),
            dirty_fingerprints: node_fingerprints::dirty_table(&uc.servers),
            ..Default::default()
        });
    }

    /// **契约形状**（T1-8）：键集恰为 `{added, modified, removed, restartDeferred}` —— 不多不少。
    ///
    /// 旧契约 `{added, updated, deleted}` 里 `updated` = `old ∩ new`（全部存活 id，与改没改过无关），
    /// 前端从不消费；留着它只会让后来者按旧名字读出旧含义。
    ///
    /// `restartDeferred`（P4）是**第四个键而非第四个数组**：它回答的是「有没有非节点结构性变更
    /// 被『保存不重启』降级」，这类改动一个节点都不动，塞进任何一个 id 数组都是撒谎。
    /// 键名走 camelCase（`#[serde(rename_all)]`）与前端契约同形。
    ///
    /// **变异对照**：给 `PendingChangesSummary` 加回 `updated` 字段 → 键集断言转红；
    /// 去掉 `#[serde(rename_all = "camelCase")]` → 键名变 `restart_deferred` → 同样转红。
    #[test]
    fn pending_changes_contract_has_exactly_three_keys() {
        let (rt, dir) = test_runtime();
        let v = serde_json::to_value(rt.pending_changes()).expect("契约体应可序列化");
        let mut keys: Vec<&str> = v
            .as_object()
            .expect("对象形")
            .keys()
            .map(|k| &**k)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["added", "modified", "removed", "restartDeferred"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **基线四态**（T1-19）：无快照 / 有快照无变化 / 空 servers / 读不到当前配置。
    /// 全部走「空差集」，且**绝不 panic**。
    ///
    /// **变异对照**：把「无 `startup_snapshot` → 空差集」腿删掉（改成拿当前配置当基准）→
    /// 核未运行时 `added` 会变成全部节点 → 转红。
    #[test]
    fn pending_changes_baseline_states_are_all_empty() {
        let (rt, dir) = test_runtime();
        let empty = PendingChangesSummary {
            added: vec![],
            modified: vec![],
            removed: vec![],
            restart_deferred: false,
        };

        // ① 核未运行 / 无起核快照。
        assert_eq!(rt.pending_changes(), empty, "无快照 = 没有分母，不谈待应用");

        // ② 有快照、配置一字未改。
        let cfg = two_node_config(7891, "node-a");
        rt.config.save_full(&cfg).expect("落盘");
        install_startup_snapshot(&rt, &cfg);
        assert_eq!(rt.pending_changes(), empty, "配置未变 → 三个集合全空");

        // ③ 空 servers 两侧。
        let mut bare = cfg.clone();
        bare["servers"] = serde_json::json!([]);
        rt.config.save_full(&bare).expect("落盘");
        install_startup_snapshot(&rt, &bare);
        assert_eq!(rt.pending_changes(), empty, "两侧都没节点 → 全空");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **`removed` 语义**（T1-9）：`old_ids − new_ids`，不是交集、不是并集、不是反过来。
    ///
    /// **变异对照**：把 `removed` 写成 `new_ids − old_ids` → 它会与 `added` 相等 → 转红。
    #[test]
    fn pending_changes_removed_is_old_minus_new() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        rt.config.save_full(&cfg).expect("落盘");
        install_startup_snapshot(&rt, &cfg);

        // 删 node-b、加 node-c（selected 仍是 node-a，避免牵动别的腿）。
        let mut next = cfg.clone();
        let servers = next["servers"].as_array_mut().unwrap();
        servers.retain(|s| s["id"] != "node-b");
        servers.push(ss_node("node-c", "Node C", 18003));
        rt.config.save_full(&next).expect("落盘");

        let p = rt.pending_changes();
        assert_eq!(p.added, vec!["node-c".to_string()], "added = new − old");
        assert_eq!(p.removed, vec!["node-b".to_string()], "removed = old − new");
        assert!(
            p.modified.is_empty(),
            "只增删、没改存活节点 → modified 空（modified ⊂ old ∩ new）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **`modified` 判据 = 全维**（U-3 已拍板）：改一个 5 维覆盖不到的字段（`name`），
    /// 该节点**必须**出现在 `modified` 里。
    ///
    /// 因果：`modified` 回答「运行核里跑的还是不是用户当前配置」。改 `name` 会改生成产物
    /// （outbound tag 随之变）⇒ 核里跑的确实已不是当前配置 ⇒ 必须报。用 5 维判据会漏报，
    /// 表现为「核因为它重启了，而 pending-bar 从没提过这件事」。
    ///
    /// **变异对照**：把 `node_fingerprints::modified_fingerprint` 换成 5 维公式 → 本条转红。
    #[test]
    fn pending_changes_modified_uses_full_projection_not_five_dims() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        rt.config.save_full(&cfg).expect("落盘");
        install_startup_snapshot(&rt, &cfg);

        // 只改显示名：5 维指纹一动不动，全维投影变。
        let mut next = cfg.clone();
        next["servers"].as_array_mut().unwrap()[1]["name"] = serde_json::json!("改过名字");
        rt.config.save_full(&next).expect("落盘");

        let p = rt.pending_changes();
        assert_eq!(
            p.modified,
            vec!["node-b".to_string()],
            "改 name 必须进 modified —— 判据是全维，不是 5 维"
        );
        assert!(p.added.is_empty() && p.removed.is_empty(), "没增没删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **核心不变式：测速 dirty ⊆ pending modified**（接线级，非仅公式级）。
    ///
    /// 「测速说这个节点『已编辑未生效，去应用』，而 pending-bar 上根本没有它」—— 用户实报症状。
    /// 本条钉死它在**结构上**不可能再发生：凡测速判 dirty 的节点，必在 `modified` 里。
    ///
    /// 与 `node_fingerprints` 里那条纯公式测的分工：那条证「全维 ⊇ 5 维」，本条证**两条数据通路
    /// 真的各自接到了正确的那张表** —— 把 `speed_probe_targets` 接成全维表、或把 `modified` 接成
    /// 5 维表，公式测都还是绿的，只有本条会说话。
    ///
    /// **变异对照**：
    /// - `speed_probe_targets` 里 `snap.dirty_fingerprints` 改回 `snap.fingerprints`
    ///   → dirty 侧恒不等 → `node-a`（一字未改）也进 dirty 而不在 modified → 转红。
    /// - `pending_changes` 的 `modified` 换成 5 维表 → 改 `port` 的仍在，但下面「改 name 只进 modified」
    ///   那条断言转红。
    #[test]
    fn speedtest_dirty_is_always_a_subset_of_pending_modified() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        rt.config.save_full(&cfg).expect("落盘");
        install_startup_snapshot(&rt, &cfg);
        *rt.status.write().unwrap() = ProxyStatus {
            running: true,
            ..Default::default()
        };
        // 池端口非空才吐 SpeedProbeTargets（生产同款前提）。
        if let Ok(mut g) = rt.switch_snapshot.write() {
            if let Some(s) = g.as_mut() {
                s.probe_pool_ports = vec![41001];
            }
        }

        // node-a：一字未改。node-b：只改 name（非 5 维）。node-c 不存在，改 node-b 的 port 另测。
        let mut next = cfg.clone();
        next["servers"].as_array_mut().unwrap()[1]["name"] = serde_json::json!("改过名字");
        rt.config.save_full(&next).expect("落盘");

        let modified: std::collections::BTreeSet<String> =
            rt.pending_changes().modified.into_iter().collect();
        // 复刻测速侧的 dirty 判据（`partition_dirty` 的公式：快照有该 id 且与当前指纹不等）。
        let targets = rt.speed_probe_targets().expect("核在跑 + 池非空");
        let current_dirty = node_fingerprints::dirty_table(
            &serde_json::from_value::<UserConfig>(next.clone())
                .expect("可解析")
                .servers,
        );
        let dirty: std::collections::BTreeSet<String> = current_dirty
            .iter()
            .filter(|(id, fp)| {
                targets
                    .fingerprints
                    .get(*id)
                    .is_some_and(|snap| snap != *fp)
            })
            .map(|(id, _)| id.clone())
            .collect();

        assert!(
            dirty.is_subset(&modified),
            "违反 dirty ⊆ modified：dirty={dirty:?} modified={modified:?} —— \
             测速会把用户指引到一个 pending-bar 上不存在的节点"
        );
        assert!(
            dirty.is_empty(),
            "只改 name → 连接参数没变 → 池里那个出口仍能代表它 → 不该判 dirty（判了就是白白拒测）"
        );
        assert!(
            modified.contains("node-b"),
            "只改 name → 核里跑的已不是当前配置 → 必须进 modified"
        );

        // 再改 5 维字段（port）：两个集合都应含它，包含关系仍成立。
        let mut moved = next.clone();
        moved["servers"].as_array_mut().unwrap()[1]["port"] = serde_json::json!(18999);
        rt.config.save_full(&moved).expect("落盘");
        let modified2: std::collections::BTreeSet<String> =
            rt.pending_changes().modified.into_iter().collect();
        let current_dirty2 = node_fingerprints::dirty_table(
            &serde_json::from_value::<UserConfig>(moved)
                .expect("可解析")
                .servers,
        );
        let dirty2: std::collections::BTreeSet<String> = current_dirty2
            .iter()
            .filter(|(id, fp)| {
                targets
                    .fingerprints
                    .get(*id)
                    .is_some_and(|snap| snap != *fp)
            })
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(
            dirty2,
            std::collections::BTreeSet::from(["node-b".to_string()])
        );
        assert!(
            dirty2.is_subset(&modified2),
            "改 5 维字段后包含关系仍须成立"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 有 `startup_snapshot` 但无 `switch_snapshot`（孪生对理论上不会只剩一半）→
    /// `added`/`removed` 照给，`modified` 降级为空：拿不到起核那刻的指纹表就没有比对基准，
    /// **宁可漏报也不猜**。
    ///
    /// **变异对照**：把缺表时的降级改成「当作全部改过」→ modified 含 node-a/node-b → 转红。
    #[test]
    fn pending_changes_without_fingerprint_baseline_degrades_to_empty_modified() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        rt.config.save_full(&cfg).expect("落盘");
        *rt.startup_snapshot.write().unwrap() = Some(cfg.clone());
        *rt.switch_snapshot.write().unwrap() = None;

        let mut next = cfg.clone();
        next["servers"].as_array_mut().unwrap()[1]["name"] = serde_json::json!("改过名字");
        next["servers"]
            .as_array_mut()
            .unwrap()
            .push(ss_node("node-c", "Node C", 18003));
        rt.config.save_full(&next).expect("落盘");

        let p = rt.pending_changes();
        assert_eq!(p.added, vec!["node-c".to_string()], "id 集差不依赖指纹表");
        assert!(p.modified.is_empty(), "没有指纹基准 → 不猜，报空");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 三个集合恒排序：`HashSet` 迭代序每进程不同（`RandomState`），不排序会让明细列表无故重排、
    /// 也让单测只能退化成集合比较。
    ///
    /// **变异对照**：删掉 `added.sort()` → 转红。用 6 个乱序 id：未排序时恰好撞上升序的概率 1/720，
    /// 即「几乎必红」；3 个 id 是 1/6，会让这条门形同虚设。
    #[test]
    fn pending_changes_sets_are_sorted() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        rt.config.save_full(&cfg).expect("落盘");
        install_startup_snapshot(&rt, &cfg);

        let mut next = cfg.clone();
        let servers = next["servers"].as_array_mut().unwrap();
        for (i, id) in ["z-n", "a-n", "m-n", "c-n", "t-n", "f-n"]
            .iter()
            .enumerate()
        {
            servers.push(ss_node(id, id, 18100 + i as u16));
        }
        rt.config.save_full(&next).expect("落盘");

        assert_eq!(
            rt.pending_changes().added,
            vec![
                "a-n".to_string(),
                "c-n".to_string(),
                "f-n".to_string(),
                "m-n".to_string(),
                "t-n".to_string(),
                "z-n".to_string(),
            ],
            "added 必须升序"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===== C6-5 起核路由决策 + helper 起核失败路径（变异验证） =====

    /// 起核路由真值表（纯决策）。变异锚点：删 `is_tun()` → systemProxy/manual 断言炸；删平台判 → Other 断言炸。
    #[test]
    fn should_start_via_helper_truth_table() {
        use ProxyModeType::{Manual, SystemProxy, Tun};
        // TUN + 有 helper 的平台 → 经 helper。
        for p in [Platform::Mac, Platform::Win, Platform::Linux] {
            assert!(
                should_start_via_helper(Tun, p),
                "TUN@{p:?} 应经 helper 起核"
            );
        }
        // TUN@Other（无 helper 实现）→ 退回直起（不经 helper）。
        assert!(
            !should_start_via_helper(Tun, Platform::Other),
            "无 helper 平台的 TUN 不应经 helper（退回直起 best-effort）"
        );
        // 非 TUN（systemProxy/manual 不接管 TUN）→ 恒直起，绝不弹提权。
        for p in [
            Platform::Mac,
            Platform::Win,
            Platform::Linux,
            Platform::Other,
        ] {
            assert!(
                !should_start_via_helper(SystemProxy, p),
                "systemProxy@{p:?} 不应经 helper"
            );
            assert!(
                !should_start_via_helper(Manual, p),
                "manual@{p:?} 不应经 helper"
            );
        }
    }

    /// helper 起核前置校验（R27.3 preflight）：TUN 需 helper 且未装 → 拦截；非 TUN → 放行。
    ///
    /// 本机/CI 从不安装 `polaris-helper`（系统路径），故 `status().installed` 恒 false（与既有
    /// `status_supported_reflects_platform` 同赖此不变式）→ TUN 恒判 missing。变异锚点：删
    /// `!installed` 条件 → TUN 断言仍过但**已装态误拦**逃逸面靠真机门；删 `should_start_via_helper`
    /// 门 → systemProxy/manual 断言炸（被误拦）。**不连 socket**：未装态 status() 短路，本机安全。
    #[test]
    fn tun_helper_missing_gates_on_mode_and_install() {
        use ProxyModeType::{Manual, SystemProxy, Tun};
        let (rt, dir) = test_runtime();
        // 本机 helper 未装 → TUN 需 helper 且未装 → 拦截（换裸 socket ENOENT 为可操作码）。
        assert!(
            rt.tun_helper_missing(Tun),
            "TUN + helper 未装 → 应前置拦截（HELPER_NOT_INSTALLED）"
        );
        // systemProxy/manual 不经 helper（直起）→ 恒放行，即便 helper 未装也绝不误拦正常直起路径。
        assert!(
            !rt.tun_helper_missing(SystemProxy),
            "systemProxy 不需 helper → 放行（不误拦直起）"
        );
        assert!(
            !rt.tun_helper_missing(Manual),
            "manual 不需 helper → 放行（不误拦直起）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── TUN 提权引导门（汇流点 / 原地续起核 / 取消终态 / 非交互抑制）────────────────────────
    //
    // **本机安全**：以下全部在门内就终止（helper 本机恒未装 → 门必命中；mock 绝不真装、绝不弹框），
    // 一律走不到 `generate`/`spawn`/`spawn_core_via_helper` —— 不建 TUN、不碰宿主网络、不起真核。

    /// 装 mock 门的运行时 + 门调用计数（`test_runtime` 的门控变体）。
    fn test_runtime_gated(
        decision: HelperGateDecision,
    ) -> (
        Arc<ProxyRuntime>,
        PathBuf,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let (rt, dir) = test_runtime();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            helper_gate_calls: Arc::clone(&calls),
            helper_gate_decision: decision,
            ..Default::default()
        }));
        // 起核首次会扫孤儿核（遍历 /proc）——本测与之无关，闸门直接置位跳过。
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst);
        (rt, dir, calls)
    }

    /// TUN 配置（门必命中：本机 helper 恒未装）。
    fn tun_config() -> Value {
        let mut c = two_node_config(7891, "node-a");
        c["proxyModeType"] = Value::String("tun".into());
        c
    }

    /// **门是全入口唯一汇流点**——本批最重要的一条。
    ///
    /// `start`（连接按钮 / 启动自动连接）与 `restart`（切档位去抖重启 / 托盘切模式 / apply-pending）
    /// **两条入口都必须经门**。此前门开在 `commands::proxy_start` 命令层，`restart` 腿完全绕过它 →
    /// 「系统代理切 TUN」的 stop 跑完、start 撞上无人值守的 preflight → 静默停在停止态（真机反馈 #1）。
    ///
    /// **变异有牙（穷举逃逸面，逐条实测见交付说明）**：
    /// - 把门移回命令层 / 从 `start_inner` 删掉调用 → 两个 `calls` 断言双双转 0，红；
    /// - 只在 `start` 加门、`restart` 不加（模拟「补一条腿而非补汇流点」）→ 第二段 `restart` 断言红；
    /// - 门放到 `spawn` 之后 → 本机会真去连 helper socket，错误码变 STARTUP_FAILED，红。
    #[tokio::test]
    async fn helper_gate_covers_start_and_restart_entries() {
        let (rt, dir, calls) = test_runtime_gated(HelperGateDecision::Abort);

        // 入口 1：start（连接按钮 / 启动自动连接）。
        let r = rt.start(tun_config()).await;
        assert!(r.is_err(), "TUN + helper 未装 + 用户取消 → 起核必失败");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "start 入口必须经门（=0 即该入口绕过了汇流点）"
        );

        // 入口 2：restart（**切档位/托盘/去抖重启走这条**）。stop→start，start 腿必须再次经门。
        let r = rt.restart(tun_config()).await;
        assert!(r.is_err(), "restart 的 start 腿同样被门拦住");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "restart 入口必须经门（=1 即 restart 绕过了汇流点，正是真机「切档位静默停止」的成因）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 用户取消 → **干净终态** `HELPER_GATE_ABORTED`，而不是静默停止或伪装成启动失败。
    ///
    /// 变异有牙：把 Abort 腿的码换成 `HELPER_NOT_INSTALLED` → 断言红（两码的用户下一步动作相反，
    /// 见 `code::HELPER_GATE_ABORTED` 文档）；删 `set_error` 只 `return Err` → `error_code` 为
    /// None，红（后端知道、前端不知道 = 真机反馈里「点了没反应」的同型病灶）。
    #[tokio::test]
    async fn helper_gate_abort_lands_clean_terminal_code() {
        let (rt, dir, _calls) = test_runtime_gated(HelperGateDecision::Abort);
        let err = rt.start(tun_config()).await.expect_err("取消 → Err");
        assert_eq!(err.message, HELPER_GATE_ABORTED_MSG);
        // A1：码随**这一次**的 Err 出栈，不靠命令层回读全局。变异：把 Abort 腿改回裸
        // `Err(msg.into())`（走 `From<String>` → code=None）→ 本断言红（渲染端又只剩 message 可猜）。
        assert_eq!(
            err.code,
            Some(code::HELPER_GATE_ABORTED),
            "错误自身必须带码（命令层据此分流，不再回读全局 status）"
        );
        let st = rt.status();
        assert_eq!(
            st.error_code.as_deref(),
            Some(code::HELPER_GATE_ABORTED),
            "取消必须落可分类的干净终态码"
        );
        assert!(!st.running, "取消 → 核未起");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 用户确认但**没装上**（mock 不真装 → 复检仍缺）→ 落 `HELPER_NOT_INSTALLED`，**不冒充成功继续 spawn**。
    ///
    /// 这条守的是 `run_helper_gate` 里最易写错的一行：确认后直接放行、不复检。
    /// 变异有牙：删复检腿（`Proceed` 直接 `Ok(())`）→ 起核继续走到 helper socket，错误码变
    /// `STARTUP_FAILED`（裸 ENOENT 又回来了）→ 本断言红。
    #[tokio::test]
    async fn helper_gate_proceed_without_successful_install_still_blocks() {
        let (rt, dir, calls) = test_runtime_gated(HelperGateDecision::Proceed);
        let err = rt.start(tun_config()).await.expect_err("没装上 → 仍 Err");
        assert_eq!(err.message, HELPER_NOT_INSTALLED_MSG);
        assert_eq!(err.code, Some(code::HELPER_NOT_INSTALLED), "码随 Err 出栈");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "门确实跑了");
        assert_eq!(
            rt.status().error_code.as_deref(),
            Some(code::HELPER_NOT_INSTALLED),
            "确认后装不上 → 仍是「去装」轴，绝不放行 spawn"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **非 TUN 不弹门**：systemProxy 起核绝不因 helper 未装而弹框（弹了就是每次连接都骚扰）。
    /// 变异有牙：删 `run_helper_gate` 首行的 `tun_helper_missing` 短路 → calls 变 1，红。
    #[tokio::test]
    async fn helper_gate_never_prompts_for_non_tun_mode() {
        let (rt, dir, calls) = test_runtime_gated(HelperGateDecision::Abort);
        // systemProxy（默认）：门不该命中。起核会继续往下走并在核二进制解析处失败——
        // 只断言「没弹门」，不断言起核结果。
        //
        // 【史】这里原本写的是「因**本机无核二进制**失败」，即假定开发机 `resources/` 是空的。
        // 装了核的机器（mac 真机 / 跑过 `fetch-core.mjs` 的 CI）上该假设当场失效：本行会真 spawn 出
        // 一个 sing-box，就绪后 `start` 返 Ok，而测试结束时没人 `stop()`、`Child` 又无 `kill_on_drop`
        // ⇒ **每跑一次单测漏一个真核进程**（配置目录随即被下面的 remove_dir_all 删掉，进程还在跑）。
        // 现由 `core_binary_for_start` 的 cfg(test) 版 deny-by-default 兜死，失败原因与平台无关。
        let _ = rt.start(two_node_config(7893, "node-a")).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "systemProxy 不经 helper → 绝不弹提权引导"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **非交互抑制**（崩溃自愈）：不弹框，退回类型化终态。用户没做任何操作时凭空索要管理员密码，
    /// 比断流更糟（上游 `options.interactive === false`）。
    ///
    /// 变异有牙：删 `helper_gate_interactive()` 判 → calls 变 1，红（崩溃循环里开始弹框）。
    #[tokio::test]
    async fn helper_gate_suppressed_in_non_interactive_restart() {
        let (rt, dir, calls) = test_runtime_gated(HelperGateDecision::Proceed);
        let r = with_helper_gate_suppressed(rt.start(tun_config())).await;
        assert!(r.is_err(), "抑制态仍拦住起核（只是不弹框）");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "非交互腿绝不弹门");
        assert_eq!(
            rt.status().error_code.as_deref(),
            Some(code::HELPER_NOT_INSTALLED),
            "抑制态退回类型化终态，而非 GATE_ABORTED（用户压根没被问）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 抑制**必须随作用域退场**（含内层 `Err` —— 崩溃自愈重启失败是常态）。
    ///
    /// 粘住的后果是「功能整体消失」型坑：此后**所有**入口的引导门静默失效，且只在崩溃后才显形。
    /// 变异有牙：把 task-local 换回 runtime 字段 + 只在 future 正常返回后 `store(false)`（不用 Drop
    /// 守卫）→ 第二段的 `calls==1` 在内层 Err 路径上转红。
    #[tokio::test]
    async fn helper_gate_suppression_resets_even_on_error() {
        let (rt, dir, calls) = test_runtime_gated(HelperGateDecision::Abort);
        let r = with_helper_gate_suppressed(rt.start(tun_config())).await;
        assert!(r.is_err(), "内层确实走的是 Err 路径（本测的前提）");
        // 复位真的生效：下一次交互式起核照常弹门。
        let _ = rt.start(tun_config()).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "作用域退场后门恢复工作（=0 说明抑制粘住了）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A2：抑制只作用于崩溃自愈那条调用链，绝不外溢到并发的用户交互起核。**
    ///
    /// 失败场景（本测锁死的那个）：TUN 运行中 helper 被卸载 + 核崩 → 自愈走
    /// `with_helper_gate_suppressed(restart(...))`，该段含 stop + start + 最多 3 轮重试与就绪等待，
    /// **可达数十秒**。此窗口内用户**手动点连接** → 若抑制是 runtime 级共享标记，用户的显式交互请求
    /// 会被当成非交互自愈处理：不弹引导框、直接落 `HELPER_NOT_INSTALLED` = 退回本门修复前的行为。
    ///
    /// **变异有牙（穷举逃逸面）**：
    /// - 抑制改回 runtime 级 `AtomicBool` 字段 → 后台腿置位期间用户腿读到 true ⇒ `calls==0` 且码变
    ///   `HELPER_NOT_INSTALLED`，**两个断言双红**；
    /// - 把 task-local 换成进程级 `static AtomicBool` → 同上双红；
    /// - `helper_gate_interactive()` 的 `unwrap_or(true)` 写成 `unwrap_or(false)`（未声明即抑制）→
    ///   用户腿也读到抑制 ⇒ 双红。
    #[tokio::test]
    async fn helper_gate_suppression_does_not_leak_into_concurrent_interactive_start() {
        let (rt, dir, calls) = test_runtime_gated(HelperGateDecision::Abort);

        // 后台任务模拟「崩溃自愈重启在飞」：进入抑制作用域后**挂住不退**，直到本测放行。
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let bg = tokio::spawn(with_helper_gate_suppressed(async move {
            let _ = entered_tx.send(());
            let _ = release_rx.await;
        }));
        entered_rx.await.expect("后台抑制作用域应已进入");

        // 此刻自愈窗口在飞。用户手动点连接（另一个任务 → 读不到那条链的 task-local）。
        let err = rt.start(tun_config()).await.expect_err("取消 → Err");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "自愈窗口内的用户交互起核**必须**照常弹引导（=0 即抑制外溢，退回修复前行为）"
        );
        assert_eq!(
            err.code,
            Some(code::HELPER_GATE_ABORTED),
            "用户被问了且选了取消 → GATE_ABORTED；若是 NOT_INSTALLED 说明门被误抑制、用户压根没被问"
        );

        let _ = release_tx.send(());
        bg.await.expect("后台腿应正常退场");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A2：抑制作用域可嵌套，内层退场绝不解除外层。**
    ///
    /// 变异有牙：换回 `AtomicBool` + `Drop` 里无条件 `store(false)`（而非计数递减）→ 内层退场即把
    /// 外层的抑制一并解除 ⇒ 外层内的起核开始弹框，`calls==0` 转红。
    #[tokio::test]
    async fn helper_gate_suppression_scopes_nest() {
        let (rt, dir, calls) = test_runtime_gated(HelperGateDecision::Abort);
        let rt2 = Arc::clone(&rt);
        with_helper_gate_suppressed(async move {
            // 内层作用域开合一次（模拟自愈腿内部再嵌一段非交互调用）。
            with_helper_gate_suppressed(async {}).await;
            // 外层仍在，抑制必须继续有效。
            let r = rt2.start(tun_config()).await;
            assert!(r.is_err(), "抑制态仍拦住起核");
        })
        .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "内层退场不得解除外层抑制（>0 说明外层被内层的 Drop 提前解除）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A1：陈旧全局错误码不得污染下一次失败的分类。**
    ///
    /// 真机复现路径：TUN + helper 未装 → 点连接 → 门弹出 → 取消 ⇒ 全局 `error_code` 留下
    /// `HELPER_GATE_ABORTED`（**本路径无 `stop()`，而全局码只有 `stop()` 清**）。用户去设置页装好
    /// helper 回来再点连接，这次栽在「配置解析失败」腿上 —— 该腿根本不经 `set_error`（见其文档）。
    /// 若命令层回读全局，就会把这次失败贴上 `HELPER_GATE_ABORTED` → `HomeScreen` 命中「用户取消」
    /// 分支，弹中性 info 并 `return`，`setConnectError(true)` 被跳过、真实错误消息被丢弃。
    ///
    /// **变异有牙（穷举逃逸面）**：
    /// - 把 `start` 的 Err 改回回读 `self.status().error_code` 填 `code` → 第二段 `err.code` 变
    ///   `Some(HELPER_GATE_ABORTED)`，红；
    /// - 给 `From<String> for StartError` 的 `code` 填任意常量而非 `None` → 同一断言红；
    /// - 删掉第一段（不制造陈旧码）→ 本测退化为恒真，故第一段的 `st.error_code` 断言把「陈旧码确实
    ///   还在全局」本身也钉死，防止哪天 `stop()` 之外多了个清理点让本测变成假绿。
    #[tokio::test]
    async fn start_error_code_is_not_polluted_by_stale_global_error_code() {
        let (rt, dir, _calls) = test_runtime_gated(HelperGateDecision::Abort);

        // 第一段：门弹出 → 用户取消 → 全局落 HELPER_GATE_ABORTED（且本路径无 stop 可清）。
        let first = rt.start(tun_config()).await.expect_err("取消 → Err");
        assert_eq!(first.code, Some(code::HELPER_GATE_ABORTED));
        assert_eq!(
            rt.status().error_code.as_deref(),
            Some(code::HELPER_GATE_ABORTED),
            "陈旧码确实滞留在全局（本测的前提；没了就说明有别的清理点，断言需重估）"
        );

        // 第二段：另一条**不经 set_error** 的失败腿（配置解析失败，`start_inner` 首个 `?`）。
        let second = rt
            .start(serde_json::json!({ "proxyModeType": 12345 }))
            .await
            .expect_err("坏配置 → Err");
        assert!(
            second.message.contains("配置解析失败"),
            "确实走的是无码腿（实际：{}）",
            second.message
        );
        assert_eq!(
            second.code, None,
            "无码腿必须回落 None，绝不继承上一次失败留在全局的 HELPER_GATE_ABORTED"
        );
        assert_eq!(
            rt.status().error_code.as_deref(),
            Some(code::HELPER_GATE_ABORTED),
            "全局仍是陈旧码（本腿不经 set_error）——正因如此才不能回读它"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// emitter 未接线（单测 / setup 前极早期）→ 退回类型化终态，**绝不因为「没法问用户」就放行 spawn**。
    /// 变异有牙：把该腿改成 `Ok(())` 放行 → 错误码变 STARTUP_FAILED，红。
    #[tokio::test]
    async fn helper_gate_without_emitter_falls_back_to_typed_terminal() {
        let (rt, dir) = test_runtime(); // 刻意不接 emitter
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst);
        let err = rt.start(tun_config()).await.expect_err("无 emitter → 仍拦");
        assert_eq!(err.message, HELPER_NOT_INSTALLED_MSG);
        assert_eq!(err.code, Some(code::HELPER_NOT_INSTALLED), "码随 Err 出栈");
        assert_eq!(
            rt.status().error_code.as_deref(),
            Some(code::HELPER_NOT_INSTALLED)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// helper 起核路径：本机无 daemon → 起核失败（**不静默回退直起**）且**复位 `core_via_helper`**。
    ///
    /// 复位是硬不变式：若失败后仍留标记 true，后续 [`kill_core`] 会误走 helper stop（child 恒 None）→
    /// 直起的核永不被杀。变异锚点：删 `store(false)` 复位腿 → 本断言炸。
    /// **本机安全**：`start_core` 在 build_client→UnixConnector 连不存在的 socket 时即 ENOENT 失败，
    /// **绝不 spawn 真核 / 建 TUN / 碰宿主网络**。
    #[tokio::test]
    async fn helper_start_without_daemon_errs_and_resets_flag() {
        let (rt, dir) = test_runtime();
        let cfg_path = dir.join("singbox-runtime.json");
        std::fs::write(&cfg_path, "{}").ok();
        let binary = PathBuf::from("/nonexistent/sing-box");
        let user_config: UserConfig =
            serde_json::from_value(polaris_store::default_config()).unwrap();
        let my_gen = rt.gate.generation();
        let r = rt
            .spawn_core_via_helper(&binary, &cfg_path, &user_config, my_gen)
            .await;
        assert!(
            r.is_err(),
            "本机无 helper daemon → 起核必失败（不静默直起）"
        );
        assert!(
            !rt.core_via_helper.load(Ordering::SeqCst),
            "起核失败必复位 core_via_helper（否则 kill_core 误走 helper stop）"
        );
        // pid 亦不得残留。
        assert!(rt.pid.lock().unwrap().is_none(), "失败不得残留 pid");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// helper 起核路径：起核前已被更新的 start/stop 接管（世代变）→ 让位（`Ok(None)`）、不 IPC、不置标记。
    /// 变异锚点：删入口世代判 → 返 `Some`/`Err`（真去 IPC）而非 `None`。
    #[tokio::test]
    async fn helper_start_superseded_before_ipc_yields_none() {
        let (rt, dir) = test_runtime();
        let cfg_path = dir.join("singbox-runtime.json");
        std::fs::write(&cfg_path, "{}").ok();
        let binary = PathBuf::from("/nonexistent/sing-box");
        let user_config: UserConfig =
            serde_json::from_value(polaris_store::default_config()).unwrap();
        let stale_gen = rt.gate.generation();
        rt.gate.bump_generation(); // 模拟被接管
        let r = rt
            .spawn_core_via_helper(&binary, &cfg_path, &user_config, stale_gen)
            .await
            .expect("让位是正常返回，非 Err");
        assert!(r.is_none(), "起核前世代已变 → 让位 Ok(None)");
        assert!(
            !rt.core_via_helper.load(Ordering::SeqCst),
            "让位早退不得置标记（未起核）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 给运行时装一个假的热切换基准（不起真核，测决策分流用）。
    fn mark_running_with_snapshot(rt: &ProxyRuntime, cfg: &Value) {
        mark_running(rt);
        let uc: UserConfig = serde_json::from_value(cfg.clone()).unwrap();
        let mut id_to_tag = BTreeMap::new();
        id_to_tag.insert("node-a".to_string(), "Node A".to_string());
        id_to_tag.insert("node-b".to_string(), "Node B".to_string());
        // 两张表都装：生产的 build_switch_snapshot 同刻同源置两张，假快照漏一张会让被测腿看到
        // 「有全维表但 dirty 表空」这个生产里不可达的形态。
        *rt.switch_snapshot.write().unwrap() = Some(SwitchSnapshot {
            id_to_tag,
            rule_target: BTreeMap::new(),
            fingerprints: node_fingerprints::modified_table(&uc.servers),
            dirty_fingerprints: node_fingerprints::dirty_table(&uc.servers),
            probe_pool_ports: vec![],
        });
        *rt.current_config.write().unwrap() = Some(cfg.clone());
    }

    /// 快速连续切换必须在唯一生产入口串行，且拿锁后再看 lifecycle 真值。底层 Mutex 的 FIFO 语义由
    /// tokio 提供；本门守的是它确实接在 `switch_mode_with`、并覆盖判定/PUT/commit 整条流水线。
    #[test]
    fn switch_mode_serializes_before_reading_lifecycle_state() {
        let body = method_body(
            include_str!("proxy.rs"),
            "    pub async fn switch_mode_with(",
        );
        let lock = body
            .find("self.switch_serial.lock().await")
            .expect("switch_mode_with 必须取得配置入核单飞锁");
        let gate = body
            .find("if self.gate.is_busy()")
            .expect("lifecycle 判定锚点");
        let execute = body.find("SwitchExecutor.execute").expect("热切换执行锚点");
        let commit = body
            .find("self.commit_applied(&new_config)")
            .expect("热切换提交锚点");
        assert!(
            lock < gate,
            "等待期间 lifecycle 会变化，必须拿锁后再判 busy"
        );
        assert!(
            gate < execute && execute < commit,
            "锁须覆盖判定、PUT 与 commit 全链路"
        );
    }

    /// 腿 0（顺序门）：lifecycle 在飞 → Pending 暂存，**即使核看起来没在跑**。
    /// 与 apply_pending 的 H-1 同型：先判「核未运行」会让 restart 空窗内的切节点被永久丢弃。
    #[tokio::test]
    async fn switch_mode_pending_when_lifecycle_busy_even_though_core_appears_stopped() {
        let (rt, dir) = test_runtime();
        rt.gate.begin();
        assert!(!rt.core_running(), "前提：此刻看起来「未运行」");

        let out = rt.switch_mode(two_node_config(7891, "node-b")).await;
        assert_eq!(
            out,
            SwitchOutcome::Pending,
            "depth>0 必须先判 → Pending；先判「未运行」会静默丢弃本次切换"
        );
        assert!(
            rt.gate.pending().switch_id.is_some(),
            "Pending 必须在 gate 上登记 switch_id，否则 end() 排空时无从重放"
        );
        assert!(
            rt.pending_switch.read().unwrap().is_some(),
            "必须暂存配置载荷"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 停止终态必须丢弃暂存的 switch（停止优先：不得停后又被 switch 拉起）。
    #[tokio::test]
    async fn stop_terminal_discards_pending_switch() {
        let (rt, dir) = test_runtime();
        rt.gate.begin();
        let _ = rt.switch_mode(two_node_config(7891, "node-b")).await;
        assert!(rt.pending_switch.read().unwrap().is_some());

        rt.finish_lifecycle(LifecycleKind::Stop);
        assert!(
            rt.gate.pending().is_empty(),
            "stop 终态必须丢弃 gate 内全部 pending"
        );
        assert!(
            rt.pending_switch.read().unwrap().is_none(),
            "stop 终态必须同步清掉暂存的 switch 载荷"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 腿 0.5：核未运行 → 仅更新 current_config（下次 start 生效），不重启不热切。
    #[tokio::test]
    async fn switch_mode_not_running_only_updates_config() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-b");
        assert_eq!(rt.switch_mode(cfg.clone()).await, SwitchOutcome::NotRunning);
        assert_eq!(rt.current_config.read().unwrap().clone(), Some(cfg));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 腿 1（上游 bug#5）：逐字节全等 → Unchanged，绝不重启。
    /// 键序不敏感：ConfigManager 落盘/回读会改键序，裸 == 会把「没变」误判成结构变更 → 无谓断流。
    #[tokio::test]
    async fn switch_mode_unchanged_is_key_order_insensitive() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        mark_running_with_snapshot(&rt, &cfg);

        // 同一份配置，逐键**反序**重建（模拟落盘→回读改了键序）。
        // 注：serde_json 未开 preserve_order 时 Map 是 BTreeMap（键恒排序），此时本断言退化为
        // 「同内容 → Unchanged」——仍是要守的不变式，只是失去了「乱序」这一维。
        let mut reordered = serde_json::Map::new();
        let mut keys: Vec<String> = cfg.as_object().unwrap().keys().cloned().collect();
        keys.reverse();
        for k in keys {
            reordered.insert(k.clone(), cfg[&k].clone());
        }
        let reordered = Value::Object(reordered);
        assert_eq!(
            rt.switch_mode(reordered).await,
            SwitchOutcome::Unchanged,
            "键序不同但内容相同 → Unchanged（stable_stringify 归一），不得触发重启"
        );
        assert!(
            !rt.gate.pending().restart_pending,
            "Unchanged 腿绝不能排程重启"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 腿 3-热切：切节点 → 走热切腿。此处**无真核** → gRPC 连不上 → executor 返 ClientNotReady
    /// → 按契约退回去抖重启（而非静默吞掉变更）。
    ///
    /// 这条同时锁死两件事：① 切节点确实被判为热切腿（否则不会去连 gRPC）；② 热切不可用时**必**回退重启。
    #[tokio::test]
    async fn switch_mode_node_switch_falls_back_to_restart_when_grpc_unavailable() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        mark_running_with_snapshot(&rt, &cfg);

        // mark_running 给的是假 apiPort（19090，无核监听）→ 连不上 → ClientNotReady。
        let out = rt.switch_mode(two_node_config(7891, "node-b")).await;
        assert_eq!(
            out,
            SwitchOutcome::Restarting,
            "热切换不可用（gRPC 连不上）必须退回重启兜底，绝不能静默吞掉切节点"
        );
        assert!(
            rt.current_config
                .read()
                .unwrap()
                .as_ref()
                .and_then(|c| c.get("selectedServerId").and_then(Value::as_str))
                == Some("node-b"),
            "回退重启腿也必须把 current_config 对账到新配置"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 腿 3-重启：改 norm 内字段（mixedPort）→ 结构性变更 → 去抖重启，**不**热切。
    #[tokio::test]
    async fn switch_mode_norm_field_change_takes_restart_leg() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        mark_running_with_snapshot(&rt, &cfg);

        // 只改端口（norm 内字段）→ plan_hot_switch 的 norm 前提失败 → kind=None → Restart。
        let out = rt.switch_mode(two_node_config(7899, "node-a")).await;
        assert_eq!(out, SwitchOutcome::Restarting, "norm 内字段变更必须走重启");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── P4「保存不重启」（spec §2.5 Q4）：defer_restart 的射程与记账 ──────────────────────
    //
    // 这一组与 switch-engine 的 `defer_restart_*` 五条互补：那边钉纯决策，这边钉**接线与副作用**
    //（真没排重启 / 记了账 / 账在核起来时结清 / 预告与实际同源）。

    /// 结构性变更 + `defer_restart=true` → 落 Defer 腿：**不排重启**，但记下欠账。
    ///
    /// 变异对照：把 `switch_mode_with` 里传给 `DecisionInput` 的 `defer_restart` 硬编码成 `false`
    /// → 本条第一个断言转红（回到 Restarting）。
    #[tokio::test]
    async fn defer_restart_flag_defers_structural_change_and_records_debt() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        mark_running_with_snapshot(&rt, &cfg);
        // 起核快照 = 待应用差集的分母，生产里由 start_inner 与 switch_snapshot 同刻装上。
        *rt.startup_snapshot.write().unwrap() = Some(cfg.clone());

        // 同一份输入在 defer_restart=false 时是 Restarting（见 switch_mode_norm_field_change_takes_restart_leg）。
        let out = rt
            .switch_mode_with(two_node_config(7899, "node-a"), true)
            .await;
        assert_eq!(
            out,
            SwitchOutcome::Deferred,
            "「保存」腿的结构性变更必须落 Defer"
        );
        assert!(
            !rt.gate.pending().restart_pending,
            "「保存不重启」若还排了重启，这个按钮就没有存在的意义"
        );
        assert!(
            rt.pending_changes().restart_deferred,
            "非节点结构性变更在三个数组里看不见 → 必须靠这一位让条不撒谎"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 欠账只在**核真按磁盘配置起来**那一刻结清；其后的 NoOp / 热切腿都不清。
    ///
    /// 变异对照：把清账点从 `startup_snapshot` 同刻挪进 NoOp 腿 → 第二个断言转红
    ///（用户切个语言就把「还差一次重启」的提示抹掉了，欠账仍在、提示没了）。
    #[tokio::test]
    async fn deferred_debt_survives_noop_and_clears_only_on_core_start() {
        let (rt, dir) = test_runtime();
        let mut cfg = two_node_config(7891, "node-a");
        cfg.as_object_mut()
            .unwrap()
            .insert("language".into(), serde_json::json!("zh-CN"));
        mark_running_with_snapshot(&rt, &cfg);
        *rt.startup_snapshot.write().unwrap() = Some(cfg.clone());

        let mut saved = cfg.clone();
        saved
            .as_object_mut()
            .unwrap()
            .insert("mixedPort".into(), serde_json::json!(7899));
        assert_eq!(
            rt.switch_mode_with(saved.clone(), true).await,
            SwitchOutcome::Deferred
        );
        assert!(rt.restart_deferred.load(Ordering::SeqCst));

        // 之后切语言（NoOp 腿）：它没有把欠下的那份配置送进核 → 不得清账。
        let mut next = saved.clone();
        next.as_object_mut()
            .unwrap()
            .insert("language".into(), serde_json::json!("en-US"));
        assert_eq!(rt.switch_mode(next).await, SwitchOutcome::NoOp);
        assert!(
            rt.restart_deferred.load(Ordering::SeqCst),
            "NoOp 腿不把配置送进核 → 欠账必须留着"
        );

        // 正向对照：有分母时读出口确实报 true —— 否则下面那条「不报」是恒真断言，没有信息量。
        assert!(rt.pending_changes().restart_deferred);

        // 无起核快照（= 核没在跑）时读出口恒报 false：即便记账位还没被复位，
        // 「待应用」也谈不上 —— 这条不变式写死在 `pending_changes` 的 empty() 腿上。
        *rt.startup_snapshot.write().unwrap() = None;
        assert!(
            !rt.pending_changes().restart_deferred,
            "没有运行核这个分母时不得报欠账"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 暂存重放必须**带着 `defer_restart` 一起**存取。
    ///
    /// 它是「本次落盘由谁触发」的意图，不是配置内容的一部分：丢了它，用户在核重启窗口内点的那次
    /// 「保存」会在几秒后自己触发一次重启 —— 恰是「保存不重启」承诺的反面，且现象是延迟的、极难归因。
    ///
    /// 变异对照：把 `pending_switch` 的第三元写死 `false`（或存取时丢弃它）→ 本条转红。
    #[tokio::test]
    async fn pending_switch_carries_the_defer_restart_intent_across_replay() {
        let (rt, dir) = test_runtime();
        rt.gate.begin(); // lifecycle 在飞 → 走腿 0 暂存
        let cfg = two_node_config(7891, "node-b");
        assert_eq!(
            rt.switch_mode_with(cfg.clone(), true).await,
            SwitchOutcome::Pending
        );
        let id = rt
            .pending_switch
            .read()
            .unwrap()
            .as_ref()
            .map(|(id, _, _)| *id)
            .expect("在飞时必须暂存");
        let (replayed, defer) = rt
            .take_pending_switch(Some(id))
            .expect("按 id 认领应取得载荷");
        assert_eq!(replayed, cfg, "重放的配置必须逐字节是暂存那份");
        assert!(defer, "「保存不重启」的意图必须跟着载荷一起被取回");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 排空腿的**接线**守卫：重放走的必须是 `switch_mode_with(cfg, defer_restart)`。
    ///
    /// 上一条证明「存进去、取回来」；这条证明取回来的那一位真被喂回决策 ——
    /// 调用 `switch_mode(cfg)`（丢掉第二个参数）在类型上完全合法，只有这道门能抓。
    #[test]
    fn replay_leg_feeds_the_defer_restart_intent_back_into_the_decision() {
        let body = method_body(
            include_str!("proxy.rs"),
            "    fn finish_lifecycle(self: &Arc<Self>, kind: LifecycleKind) {",
        );
        assert!(
            body.contains("me.switch_mode_with(cfg, defer_restart).await"),
            "排空重放必须把取回的意图喂回去；调 switch_mode(cfg) 会静默降级成「保存后仍重启」"
        );
    }

    /// 清账点的**接线**守卫：`restart_deferred` 必须在 `startup_snapshot` 被写/被清的**同一个方法体**里复位。
    ///
    /// 单测够不着这两处（`start_inner` 要真起核、`stop_inner` 会碰系统 DNS 与路由，本机禁跑触网测试），
    /// 故用源码型守卫兜底。它证不了行为，只证「接线没掉」——behavior 那一半由上面两条测试覆盖。
    /// 锚点失配即 panic（`method_body` 自带），不会退化成恒真。
    #[test]
    fn deferred_debt_is_cleared_where_the_startup_snapshot_is_written_and_cleared() {
        const SRC: &str = include_str!("proxy.rs");
        let started = method_body(SRC, "    async fn start_inner(");
        assert!(
            started.contains("*snap = Some(config);")
                && started.contains("restart_deferred.store(false"),
            "起核就绪腿必须与写 startup_snapshot 同刻清账 —— 否则核已按新配置起来了，条上还挂着「待应用」"
        );
        let stopped = method_body(SRC, "async fn stop_inner(self: &Arc<Self>) -> bool {");
        assert!(
            stopped.contains("restart_deferred.store(false"),
            "停核腿必须复位欠账 —— 否则停核期间挂着一条谈不上「待应用」的提示，且下次起核前无人清"
        );
    }

    /// **差集 PUSH 的两侧接线守卫**：差集 = f(分子 `config.current()`，分母 `startup_snapshot` 等)，
    /// **两侧都得推**（因果在 [`ProxyRuntime::push_pending_changes`] 头注）。
    ///
    /// 只推分子那一侧是本缺陷的根因（陈先生 2026-07-30 真机「点击未真实生效，依然还是显示立即应用」）：
    /// 点「立即应用」→ 后端自驱去抖重启 → 核真按新配置起来了、差集其实已清，但
    /// ① 分母侧没人 PUSH；② 前端那条 pull 兜底挂在 `event:proxyStarted`，而该事件**只由命令层**
    /// （`commands/proxy.rs` 的 proxy_start/stop/restart）发，内部驱动的重启一个都不发
    /// ⇒ store 里的差集停在 `switch_mode` 推的最后一帧（`restartDeferred:true`），条永远停在「立即应用」。
    ///
    /// 单测够不着这三个方法体（要真起核 / 会碰系统 DNS 与路由，本机禁跑触网测试），故用源码型守卫。
    /// 变异对照：删掉任一处 `push_pending_changes()` 调用 → 对应断言转红；锚点失配即 panic
    /// （`method_body` 自带），不会退化成恒真。
    #[test]
    fn pending_changes_push_is_wired_on_both_sides_of_the_diff() {
        const SRC: &str = include_str!("proxy.rs");
        // 分子侧（配置变了）。
        let switched = method_body(SRC, "    pub async fn switch_mode_with(");
        assert!(
            switched.contains("self.push_pending_changes();"),
            "分子侧（落盘/切节点）必须推 —— 否则改完配置条根本不出现"
        );
        // 分母侧（运行核换了）：写 / 清 `startup_snapshot` 的那两处。
        let started = method_body(SRC, "    async fn start_inner(");
        assert!(
            started.contains("self.push_pending_changes();"),
            "起核就绪腿必须推 —— 否则「立即应用」引发的重启落地后没人告诉 UI 差集已清，条停在「立即应用」"
        );
        let stopped = method_body(SRC, "async fn stop_inner(self: &Arc<Self>) -> bool {");
        assert!(
            stopped.contains("self.push_pending_changes();"),
            "停核腿必须推 —— 重启内嵌的这次停核不经命令层，只靠前端 proxyStopped 的 pull 是漏的一半"
        );
    }

    /// **生命周期 PUSH 与差集 PUSH 的配对守卫**（接线级，锚点失配自带 panic）。
    ///
    /// `ready`/`stopped` 两个 phase 必须与 `push_pending_changes()` **严格同处、紧邻**：它们是同一次
    /// 跃迁的两个投影。分开放（哪怕只是挪到同方法的另一段）就会出现「差集清了但态没翻」或反过来 ——
    /// 而这两种不一致在真机上都表现为「点了没反应」，正是本轮要根除的形态。
    ///
    /// `failed` **刻意不在这一对里**（因果在 `push_lifecycle` 头注：起核失败不改变差集的分母），
    /// 但它必须落在 `start` 包装的 `Err` 腿 —— 那是全部起核入口的唯一汇流点。挪进任一条具体失败腿
    /// 就会漏掉别的入口，而漏掉的那些正是「没人在 await」的托盘 / 自动连接 / 去抖重启。
    ///
    /// 变异对照：把 `start_inner` 里那两行的顺序颠倒、或在中间插一条语句 → 第一条转红；
    /// 删掉 `start` 包装里的 `failed` 腿 → 第三条转红。
    #[test]
    fn lifecycle_push_is_paired_with_the_diff_push() {
        const SRC: &str = include_str!("proxy.rs");
        const DIFF: &str = "self.push_pending_changes();";

        let started = method_body(SRC, "    async fn start_inner(");
        assert!(
            line_immediately_followed_by(
                &started,
                DIFF,
                "self.push_lifecycle(&ProxyLifecycleEvent::ready());"
            ),
            "起核就绪腿：`ready` 必须紧跟差集 PUSH —— 两者描述同一次跃迁，拆开即引入可分叉的第二个时点"
        );

        let stopped = method_body(SRC, "async fn stop_inner(self: &Arc<Self>) -> bool {");
        assert!(
            line_immediately_followed_by(
                &stopped,
                DIFF,
                "self.push_lifecycle(&ProxyLifecycleEvent::stopped());"
            ),
            "停核拆除腿：`stopped` 必须紧跟差集 PUSH（与起核腿严格对偶）"
        );

        let start_wrap = method_body(
            SRC,
            "    pub async fn start(self: &Arc<Self>, config: Value) -> Result<ProxyStatus, StartError> {",
        );
        assert!(
            start_wrap.contains("if let Err(e) = &r {")
                && start_wrap.contains("self.push_lifecycle(&ProxyLifecycleEvent::failed(e));"),
            "`failed` 必须挂在 `start` 包装的 Err 腿（全部起核入口的唯一汇流点）——\
             挪进具体失败腿会漏掉托盘 / 自动连接 / 去抖重启这些「没人在 await」的入口"
        );
    }

    /// 预告（`classify_staged`）与实际（`switch_mode`）**同源**：同一份候选配置，两者结论必须一致。
    ///
    /// 变异对照：让 `classify_staged` 自己再判一次（哪怕只重写「逐字节全等」那一条）→ 本条转红。
    /// 这道门守的是「预告说不重启、实际断了流」——真机上最难归因的一类。
    #[tokio::test]
    async fn classify_staged_agrees_with_the_leg_switch_mode_actually_takes() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        mark_running_with_snapshot(&rt, &cfg);

        // ① 逐字节全等 → noOp、不需重启。
        let same = rt.classify_staged(&cfg);
        assert_eq!(same.decision, "noOp");
        assert!(!same.restart_required);

        // ② norm 内字段（端口）变 → restart、需重启。随后真跑一次 switch_mode 验证结论一致。
        let structural = two_node_config(7899, "node-a");
        let predicted = rt.classify_staged(&structural);
        assert_eq!(predicted.decision, "restart");
        assert!(predicted.restart_required);
        assert_eq!(
            rt.switch_mode(structural).await,
            SwitchOutcome::Restarting,
            "预告说 restart，实际必须真走重启腿"
        );

        // ③ 核未运行 → 落盘不触发任何核动作 ⇒ 不存在「需重启才生效」。
        *rt.status.write().unwrap() = ProxyStatus::default();
        let stopped = rt.classify_staged(&two_node_config(7999, "node-a"));
        assert_eq!(stopped.decision, "noOp");
        assert!(!stopped.restart_required);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `classify_staged` 恒以 `defer_restart=false` 判：它回答「这批改动**本性上**要不要重启」，
    /// 而不是「我打算不打算现在重启」。
    ///
    /// 变异对照：把 `classify_switch(candidate, false)` 改成 `true` → 本条转红（预告变成 `defer`，
    /// 用户看到的是「不用重启」，而它其实还没进核）。
    #[tokio::test]
    async fn classify_staged_never_predicts_its_own_deferral() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        mark_running_with_snapshot(&rt, &cfg);
        let c = rt.classify_staged(&two_node_config(7899, "node-a"));
        assert_eq!(
            c.decision, "restart",
            "本性上需重启的改动不得被预告成 defer"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 腿 3-NoOp：只改 norm **排除**的纯偏好字段（language）+ 节点未变 → 零热切零重启。
    /// 这是 norm 排除清单真正的价值：切个语言不该断流。
    #[tokio::test]
    async fn switch_mode_norm_excluded_field_change_is_noop() {
        let (rt, dir) = test_runtime();
        let mut cfg = two_node_config(7891, "node-a");
        cfg.as_object_mut()
            .unwrap()
            .insert("language".into(), serde_json::json!("zh-CN"));
        mark_running_with_snapshot(&rt, &cfg);

        let mut next = cfg.clone();
        next.as_object_mut()
            .unwrap()
            .insert("language".into(), serde_json::json!("en-US"));

        assert_eq!(
            rt.switch_mode(next).await,
            SwitchOutcome::NoOp,
            "language 在 norm 排除清单内 + 节点未变 → NoOp，不得重启"
        );
        assert!(!rt.gate.pending().restart_pending, "NoOp 腿绝不能排程重启");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 腿 3-Defer：仅新增未被引用的节点（订阅刷新常见）→ 免整核重启。
    #[tokio::test]
    async fn switch_mode_added_unreferenced_node_defers() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        mark_running_with_snapshot(&rt, &cfg);

        // 纯新增一个没人引用的节点，选中节点不变。
        let mut next = cfg.clone();
        next["servers"]
            .as_array_mut()
            .unwrap()
            .push(ss_node("node-c", "Node C", 18003));

        assert_eq!(
            rt.switch_mode(next).await,
            SwitchOutcome::Deferred,
            "仅新增未引用节点 → Defer（免重启），否则订阅刷新每次都断流"
        );
        assert!(!rt.gate.pending().restart_pending, "Defer 腿绝不能排程重启");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **R2 PUSH**：`switch_mode` 末尾 `push_pending_changes` 把 `pending_changes()` **原样**推一次
    /// （无适配层，pull/push 同一个 `PendingChangesSummary`）。added=相对起核快照的新增未引用节点。
    /// 变异有牙：删 switch_mode 末尾 emit 点 → len==0 转红；把 `added` 换成 `old ∩ new`（旧 `updated` 的
    /// 语义）→ added 含 node-a/node-b 转红；漏起核快照基准（startup_snapshot）→ pending_changes 退化转红。
    #[tokio::test]
    async fn switch_mode_push_pending_changes_added_only() {
        let (rt, dir) = test_runtime();
        let pending: PendingChangesEvents = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            pending_changes: Arc::clone(&pending),
            ..Default::default()
        }));

        // 起核差集基准（分母）：仅 node-a/node-b。落盘 + 装热切换快照 + startup_snapshot（pending_changes 的分母）。
        let cfg = two_node_config(7891, "node-a");
        rt.config.save_full(&cfg).expect("落盘基准配置");
        *rt.startup_snapshot.write().unwrap() = Some(cfg.clone());
        mark_running_with_snapshot(&rt, &cfg);

        // 纯新增未引用节点 node-c（差集分子）；落盘使 pending_changes 读的 config.current() 反映之。
        let mut next = cfg.clone();
        next["servers"]
            .as_array_mut()
            .unwrap()
            .push(ss_node("node-c", "Node C", 18003));
        rt.config.save_full(&next).expect("落盘新增节点");

        assert_eq!(
            rt.switch_mode(next).await,
            SwitchOutcome::Deferred,
            "仅新增未引用节点 → Defer（前置：push 挂在 switch_mode 末尾，此腿也走）"
        );

        let evs = pending.lock().unwrap();
        assert_eq!(evs.len(), 1, "switch_mode 末尾恰 push 一次");
        assert_eq!(
            evs[0].added,
            vec!["node-c".to_string()],
            "added = 相对起核快照的新增未引用节点"
        );
        assert!(
            evs[0].modified.is_empty(),
            "node-a/node-b 一字未改 → 不该进 modified（进了说明判据把「存活」当成了「改过」）"
        );
        assert!(evs[0].removed.is_empty(), "本次没删节点");
        drop(evs);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Defer 开关：`restartOnNodeChange=true` → 节点变更即刻重启（auto-apply 语义），不落 Defer。
    ///
    /// 该字段**不在 UserConfig 结构体里**，只能从原始 JSON 读 → 这条锁死那条读取路径没写错，
    /// 否则开关恒失效（用户开了「立即应用」却仍走 defer）。
    #[tokio::test]
    async fn switch_mode_restart_on_node_change_defeats_defer() {
        let (rt, dir) = test_runtime();
        let mut cfg = two_node_config(7891, "node-a");
        cfg.as_object_mut()
            .unwrap()
            .insert("restartOnNodeChange".into(), serde_json::json!(true));
        mark_running_with_snapshot(&rt, &cfg);

        let mut next = cfg.clone();
        next["servers"]
            .as_array_mut()
            .unwrap()
            .push(ss_node("node-c", "Node C", 18003));

        assert_eq!(
            rt.switch_mode(next).await,
            SwitchOutcome::Restarting,
            "restartOnNodeChange=true → 节点变更即刻重启，不得落 Defer"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无热切换基准快照（核在跑但快照缺失）→ 保守走重启，绝不静默吞。
    #[tokio::test]
    async fn switch_mode_without_snapshot_falls_back_to_restart() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        mark_running(&rt);
        *rt.current_config.write().unwrap() = Some(cfg);
        // 不装 switch_snapshot。
        assert_eq!(
            rt.switch_mode(two_node_config(7891, "node-b")).await,
            SwitchOutcome::Restarting,
            "无基准 → 无从判热切 → 必须重启（fail-closed）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H-1：非重启腿（NoOp/Defer/热切）必须把待决 force-restart 快照的**值**刷新到 newConfig，
    /// 且**保留 id**。不刷新 → 去抖 timer 到点把核重启回旧配置，刚应用的变更被吃掉。
    #[tokio::test]
    async fn non_restart_leg_refreshes_pending_force_restart_snapshot_keeping_id() {
        let (rt, dir) = test_runtime();
        let mut cfg = two_node_config(7891, "node-a");
        cfg.as_object_mut()
            .unwrap()
            .insert("language".into(), serde_json::json!("zh-CN"));
        mark_running_with_snapshot(&rt, &cfg);
        // 模拟已有待决 force-restart（apply_pending 排程过），载荷是旧 cfg。
        *rt.pending_force_restart.write().unwrap() = Some((42, cfg.clone()));

        let mut next = cfg.clone();
        next.as_object_mut()
            .unwrap()
            .insert("language".into(), serde_json::json!("en-US"));
        assert_eq!(rt.switch_mode(next.clone()).await, SwitchOutcome::NoOp);

        let pending = rt.pending_force_restart.read().unwrap().clone().unwrap();
        assert_eq!(
            pending.0, 42,
            "必须保留 force-restart id（换号 = 排空时认领不到）"
        );
        assert_eq!(
            pending.1, next,
            "必须把载荷刷新到 newConfig，否则重启回退旧配置"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 重启腿相反：**丢弃**待决 force-restart 快照（上游 :1888 `pendingForceRestartConfig = null`）。
    /// 结构性重启用最新完整 config → 旧 force 快照必须让位，否则它反 shadow 本次变更。
    #[tokio::test]
    async fn restart_leg_discards_pending_force_restart_snapshot() {
        let (rt, dir) = test_runtime();
        let cfg = two_node_config(7891, "node-a");
        mark_running_with_snapshot(&rt, &cfg);
        *rt.pending_force_restart.write().unwrap() = Some((42, cfg.clone()));

        assert_eq!(
            rt.switch_mode(two_node_config(7899, "node-a")).await,
            SwitchOutcome::Restarting
        );
        assert!(
            rt.pending_force_restart.read().unwrap().is_none(),
            "重启腿必须丢弃旧 force 快照（否则去抖回调消费它 → 重启回旧配置）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 把状态直接置为「运行中」（不起真核）——测 apply_pending 的判定分支用。
    fn mark_running(rt: &ProxyRuntime) {
        *rt.status.write().unwrap() = ProxyStatus {
            running: true,
            pid: 424242,
            start_time: Some(now_ms()),
            mixed_port: 7890,
            clash_api_port: 19090,
            ..ProxyStatus::default()
        };
    }

    // ── apply_pending 真实状态（此前硬编码 "applied" → UI 误报成功）──

    #[tokio::test]
    async fn apply_pending_skipped_when_core_not_running() {
        // 核未运行 → skipped（下次 start 从磁盘纳入），绝不谎报 applied。
        let (rt, dir) = test_runtime();
        assert_eq!(rt.apply_pending().await, "skipped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn apply_pending_applied_when_running_and_idle() {
        let (rt, dir) = test_runtime();
        mark_running(&rt);
        assert_eq!(rt.apply_pending().await, "applied");
        // applied 必须真的留下 force-restart 专用快照（drain/去抖据此重启到这份 cfg）。
        assert!(
            rt.pending_force_restart.read().unwrap().is_some(),
            "applied 必须写 force-restart 快照，否则重启会回落旧 config（H-1 死循环）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **「apply 之后差集必须为空」**（陈先生 2026-07-30 真机：点了「立即应用」核**真**重启了，
    /// 条上却仍是「立即应用」，连点三次形态相同）。
    ///
    /// 走全链路的**纯状态半边**（不起真核）：起核快照 = 旧配置、磁盘 = 新配置 ⇒ 差集非空
    /// （**正向对照**，否则下面那句「为空」毫无信息量）→ `apply_pending` 排程并留下 force-restart
    /// 快照 → 模拟那次去抖重启落地（去抖回调按 id 取回该快照，`start_inner` 就绪腿把它装成新的
    /// 起核快照并清欠账）⇒ 差集必须为空。
    ///
    /// **本条证不到的那一半**（如实标注）：`start_inner` 真的装了快照、真的清了欠账 —— 那要真起核，
    /// 由源码型守卫 `deferred_debt_is_cleared_where_the_startup_snapshot_is_written_and_cleared`
    /// 与 `pending_changes_push_is_wired_on_both_sides_of_the_diff` 兜。
    ///
    /// 变异对照（真能转红的）：
    /// - `apply_pending` 改成拿 `current_config`（在飞 start 会覆盖它）而非 `self.config.current()`
    ///   做快照 → 落地装回旧配置 → `added` 里 node-c 还在 → 红；
    /// - `take_force_restart_config` 不按 id 认领（恒取 / 恒不取）→ `expect` 炸或取到 None → 红；
    /// - `pending_changes` 的 `modified` 旧侧改成现算磁盘 → 差集恒空，本条的**正向对照**那一半先红。
    #[tokio::test]
    async fn diff_is_empty_after_the_restart_that_apply_pending_scheduled_lands() {
        let (rt, dir) = test_runtime();
        let base = two_node_config(7891, "node-a");
        rt.config.save_full(&base).expect("落盘");
        install_startup_snapshot(&rt, &base);
        mark_running(&rt);

        // 条上「配置变更待应用」的两条来源各摆一个：①「保存不重启」的欠账 ② 一个尚未进核的新节点。
        rt.restart_deferred.store(true, Ordering::SeqCst);
        let mut next = base.clone();
        next["servers"]
            .as_array_mut()
            .unwrap()
            .push(ss_node("node-c", "Node C", 18003));
        rt.config.save_full(&next).expect("落盘");

        let before = rt.pending_changes();
        assert_eq!(
            before.added,
            vec!["node-c".to_string()],
            "正向对照：apply 之前差集必须真的非空"
        );
        assert!(before.restart_deferred, "正向对照：欠账必须真的在");

        // 用户点「立即应用」。
        assert_eq!(rt.apply_pending().await, "applied");
        let id = rt
            .gate
            .pending()
            .force_restart_id
            .expect("applied 必须在 gate 里记下 force id");

        // 模拟去抖重启落地 = `schedule_restart` 回调取配置 + `start_inner` 就绪腿装快照/清欠账。
        let landed = rt
            .take_force_restart_config(Some(id))
            .expect("去抖回调必须能按 id 取回 apply 排程那一刻的 config");
        install_startup_snapshot(&rt, &landed);
        rt.restart_deferred.store(false, Ordering::SeqCst);

        assert_eq!(
            rt.pending_changes(),
            PendingChangesSummary {
                added: vec![],
                modified: vec![],
                removed: vec![],
                restart_deferred: false,
            },
            "重启落地后差集必须为空 —— 非空 = 条上继续挂「立即应用」，用户再点也清不掉"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// H-1 顺序门（上游 ProxyManager.ts:1731 注释）：**depth>0 必须先于句柄判空**。
    /// 顺序颠倒 → restart 的 stop→start 空窗内（句柄暂空、depth>0）本次强制重启被静默丢弃，
    /// 用户重试遇 304 → 不再触发 force-restart → 死循环。
    #[tokio::test]
    async fn apply_pending_deferred_when_busy_even_though_core_appears_stopped() {
        let (rt, dir) = test_runtime();
        // 模拟 restart 的 stop→start 空窗：lifecycle 在飞，但状态尚未回到 running。
        rt.gate.begin();
        assert!(!rt.core_running(), "前提：此刻句柄/状态看起来是「未运行」");

        let r = rt.apply_pending().await;
        assert_eq!(
            r, "deferred",
            "depth>0 必须先判 → deferred；若先判「未运行」会返回 skipped 并永久丢弃本次变更（H-1）"
        );
        // 必须排入 drain：restart_pending + force-restart 快照都要在。
        let pending = rt.gate.pending();
        assert!(
            pending.restart_pending,
            "deferred 必须置 restart_pending 供 end() 排空"
        );
        assert!(
            pending.force_restart_id.is_some(),
            "deferred 必须记 force-restart id"
        );
        assert!(rt.pending_force_restart.read().unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 停止终态必须丢弃 pending（停止优先，不得停后又被拉起）——接线 end(Stop) 的语义。
    #[tokio::test]
    async fn stop_terminal_discards_pending_force_restart() {
        let (rt, dir) = test_runtime();
        rt.gate.begin();
        let _ = rt.apply_pending().await; // deferred，置下 pending
        assert!(rt.pending_force_restart.read().unwrap().is_some());

        // 收尾为 Stop → 丢弃全部 pending，且本层的专用快照同步清空。
        rt.finish_lifecycle(LifecycleKind::Stop);
        assert!(
            rt.gate.pending().is_empty(),
            "stop 终态必须丢弃 gate 内全部 pending"
        );
        assert!(
            rt.pending_force_restart.read().unwrap().is_none(),
            "stop 终态必须同步清掉本层 force-restart 快照，否则下次排空会重启到陈旧 cfg"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// force-restart 快照按 id 消费：id 对不上（更新的 apply 已换快照）→ 不消费旧的。
    #[tokio::test]
    async fn take_force_restart_config_matches_by_id() {
        let (rt, dir) = test_runtime();
        *rt.pending_force_restart.write().unwrap() = Some((7, serde_json::json!({"k": 1})));
        // id 对不上 → 不取。
        assert!(rt.take_force_restart_config(Some(8)).is_none());
        // None（用 currentConfig）→ 不消费专用快照。
        assert!(rt.take_force_restart_config(None).is_none());
        // id 对得上 → 取出并清空。
        assert_eq!(
            rt.take_force_restart_config(Some(7)),
            Some(serde_json::json!({"k": 1}))
        );
        assert!(rt.pending_force_restart.read().unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 起核腿被接管（世代已变）→ 静默让位，且**根本不 spawn**（无孤儿进程）。
    /// 世代判定在持 child 锁期间进行，故此处模拟「stop 已 bump 世代」后起核必不落地。
    #[tokio::test]
    async fn start_yields_without_spawning_when_superseded_before_spawn() {
        let (rt, dir) = test_runtime();
        let stale_gen = rt.gate.generation();
        rt.gate.bump_generation(); // 模拟并发 stop/start 接管

        // 直接调 start_inner 并传入已过期的世代 → 应让位返回、不 spawn。
        let cfg = serde_json::json!({ "servers": [], "selectedServerId": "__direct__" });
        let r = rt.start_inner(cfg, stale_gen).await;
        assert!(r.is_ok(), "让位是正常返回，不是错误");
        assert!(!rt.status().running, "让位腿不得置 running");
        assert!(
            rt.child.lock().unwrap().is_none(),
            "让位腿绝不能 spawn 子进程（否则成孤儿：接管方不知道它的存在）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // 维度7 #8：start 失败腿清系统代理（**组合面门**，§K7.1）
    //
    // §K7.1 教训：「光测 ensure_cleared 函数、光测 start 失败」都不够——两扇门之间的缝才是生产路径。
    // 故这里打的是 `start 真失败 → controller 真被调 → mock 记录到 ensure_cleared 被触发` 这条组合路径，
    // 并单独覆盖 restart 失败腿（本不变式的主场景）。本机绝不真跑 networksetup/gsettings/reg。
    //
    // 坏配置（非对象 JSON）→ UserConfig 反序列化在 start_inner **第一步**即失败 → 不 spawn、不写盘、
    // 不解析端口 → 返回 Err，零宿主副作用。`stale_sweep_disabled=true` 预置跳过 /proc 孤儿清扫。
    // ══════════════════════════════════════════════════════════════════════════════

    /// 坏配置：反序列化为 UserConfig 必失败（非对象顶层）→ start_inner 首步即 Err，无任何副作用。
    fn bad_config() -> Value {
        serde_json::json!("not-a-user-config-object")
    }

    /// 组合面：`start` 真失败（世代未被接管）→ 系统代理收口器**真被调**（维度7 #8 主门）。
    #[tokio::test]
    async fn start_failure_invokes_system_proxy_clearer() {
        let (rt, dir, calls) = test_runtime_recording();
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst); // 跳过孤儿清扫（/proc 扫描），聚焦失败腿。
        let r = rt.start(bad_config()).await;
        assert!(r.is_err(), "坏配置必失败");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "起核失败（世代未变）必触发系统代理收口——组合路径必须走通"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 组合面·**主场景**：`restart` 的 start 腿失败 → 系统代理收口器真被调（重启失败→死端口→全网断）。
    /// 挂 command 层会漏掉这条腿（restart 内部直调 self.start，不经 command）——这正是必须挂 public
    /// `start` 而非 command 的证据。
    #[tokio::test]
    async fn restart_start_leg_failure_invokes_system_proxy_clearer() {
        let (rt, dir, calls) = test_runtime_recording();
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst);
        // restart = stop_inner()（**不清系统代理**：瞬态停核，无核 → no-op）+ start(bad)（失败 → 清）。
        let r = rt.restart(bad_config()).await;
        assert!(r.is_err(), "restart 的 start 腿坏配置必失败");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "restart 的 start 腿失败必收口（主场景）；stop_inner 腿不清 → 恰好一次"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // FX-proxy-A 修复批变异防线（proxy-lifecycle 域 + config-gen I/O 落盘）
    // ══════════════════════════════════════════════════════════════════════════════

    // ── Fix 1：restart() 全程 depth≥1 不变式 ──

    /// 机制门（纯 gate 序列）：restart 外层 begin/finish 包裹下，内层 stop 的 end(Stop) 在 depth 1 命中
    /// `StillBusy`（**不丢弃**暂存 switch），外层 end(Restart) 归 0 时排空重放它。
    #[test]
    fn restart_wrapper_keeps_depth_positive_so_inner_stop_does_not_discard() {
        let g = LifecycleGate::default();
        g.begin(); // restart 外层 begin → depth 1
        g.set_switch_pending(7); // 窗口内暂存 switch
        g.begin(); // 内层 stop begin → depth 2
        let r_stop = g.end(LifecycleKind::Stop); // → depth 1
        assert!(
            matches!(r_stop, LifecycleEndResult::StillBusy(1)),
            "内层 stop 须 StillBusy，不落 Stopped 终态丢弃"
        );
        assert_eq!(g.pending().switch_id, Some(7), "包裹下暂存 switch 存活");
        g.begin(); // 内层 start begin → depth 2
        let r_start = g.end(LifecycleKind::Start); // → depth 1
        assert!(matches!(r_start, LifecycleEndResult::StillBusy(1)));
        let r_restart = g.end(LifecycleKind::Restart); // → depth 0
        match r_restart {
            LifecycleEndResult::Drained(d) => {
                assert_eq!(
                    d.replay_switch_id,
                    Some(7),
                    "外层归 0 时排空重放暂存 switch"
                );
            }
            other => panic!("expected Drained, got {other:?}"),
        }
    }

    /// 反证门：**无**外层包裹 → 内层 stop 在 depth 0 命中 `Stopped` 终态 → 丢弃暂存 switch（drifted 缺陷）。
    #[test]
    fn without_restart_wrapper_inner_stop_discards_pending_switch() {
        let g = LifecycleGate::default();
        g.set_switch_pending(7);
        g.begin(); // 仅 stop（无外层）→ depth 1
        let r = g.end(LifecycleKind::Stop); // → depth 0
        let LifecycleEndResult::Stopped(d) = r else {
            panic!("无包裹时 stop 在 depth 0 应落 Stopped")
        };
        assert_eq!(
            d.discarded_switch_id,
            Some(7),
            "无包裹时 stop 终态吞掉暂存 switch"
        );
    }

    /// wiring 门：`restart()` 外层包裹使暂存 switch 在收尾（depth 0 Restart）被**重放**而非丢弃。
    /// 变异（删掉 restart 的 `gate.begin()`/`finish_lifecycle(Restart)`）→ 内层 stop 在 depth 0 丢弃暂存
    /// switch → 无重放 → current_config 永不更新 → 下方轮询超时 → 转红。start 腿用坏配置快速失败（不 spawn 真核）。
    #[tokio::test]
    async fn restart_replays_pending_switch_via_outer_lifecycle_wrapper() {
        let (rt, dir) = test_runtime();
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst);
        // 暂存一条 switch（核未运行 → 重放的 switch_mode 走 NotRunning 分支落 current_config，无真核、可观测）。
        let switch_cfg = serde_json::json!({ "servers": [], "selectedServerId": "__direct__", "marker": "replayed" });
        let id = rt.switch_seq.fetch_add(1, Ordering::SeqCst);
        *rt.pending_switch.write().unwrap() = Some((id, switch_cfg.clone(), false));
        rt.gate.set_switch_pending(id);

        let _ = rt.restart(bad_config()).await; // start 腿坏配置快速失败，不 spawn。

        // 轮询 current_config 直到被重放的 switch 落定（spawn 的重放任务近即执行；有界等待防超长）。
        let mut replayed = false;
        for _ in 0..50 {
            if rt.current_config.read().unwrap().as_ref() == Some(&switch_cfg) {
                replayed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            replayed,
            "restart 外层收尾须重放暂存 switch（wrapper 缺失则内层 stop 丢弃 → current_config 永不更新）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Fix 2：崩溃自愈 supersede-crash 补发（M-2′-G1）传真实在途世代 ──

    /// `drive_crash_decision` seam 读回**真实在途世代**喂 handle_crash（非硬编码 None）。
    /// 变异（把 seam 里 `m.restarting_gen()` 换回 `None`）→ crash_while_superseded 不置 → replay=false → 转红。
    #[test]
    fn drive_crash_decision_feeds_real_inflight_gen_for_supersede_replay() {
        const NOW: u64 = 1_000_000;
        let mut m = CrashRecoveryMachine::default();
        // 第一条腿 attempt（gen=5）→ is_restarting + restarting_gen=5。
        let r = drive_crash_decision(&mut m, NOW, 5);
        assert!(matches!(r, AutoRestartOutcome::Attempt { .. }));
        // 接管会话（gen=6）崩溃：seam 读回在途世代 5（≠6）→ 置 crash_while_superseded；本崩溃被 dedup 吞掉。
        let r2 = drive_crash_decision(&mut m, NOW + 1, 6);
        assert_eq!(r2, AutoRestartOutcome::Dedup);
        // 第一条腿退避完 → Superseded{replay:true}（若 seam 传 None 则 replay:false → 断言红）。
        let fate = m.post_backoff(5, 6);
        assert_eq!(fate, RestartFate::Superseded { replay: true });
    }

    // ── Fix 4：崩溃遇不可恢复错误立即终态判定 ──

    #[test]
    fn is_unrecoverable_restart_error_classifies_terminal_failures() {
        // 确定性失败 → 终态（不再空耗退避）。
        assert!(is_unrecoverable_restart_message("Permission denied")); // ASCII 大写经小写归一
        assert!(is_unrecoverable_restart_message("提权助手不可用"));
        assert!(is_unrecoverable_restart_message("clash_api 端口被占用"));
        assert!(is_unrecoverable_restart_message("HELPER_GATE_ABORTED"));
        assert!(is_unrecoverable_restart_message("检测到 root 残留孤儿核"));
        // 慢起/瞬态 → 非终态（否则慢起被误判放弃）。
        assert!(!is_unrecoverable_restart_message("sing-box 起核超时"));
        assert!(!is_unrecoverable_restart_message("sing-box 启动期退出"));
    }

    /// **本缺陷的复现锚**：helper 门的两条终态腿实际落进 `StartError` 的是**中文文案**，而 message
    /// 关键字表里一个都不命中 —— 先把这条「keyword 腿看不见它俩」钉死，再断言码腿把它们捞回来。
    ///
    /// 变异有牙（逃逸面穷举）：
    /// - 删 `is_unrecoverable_restart_error` 的码腿（`coded_terminal` 恒 `false`）= 退回纯 message
    ///   匹配 → 下方两条 `assert!(is_unrecoverable_restart_error(..))` **双红**（本缺陷复现）。
    /// - 码腿只留 `HELPER_GATE_ABORTED`（漏 `HELPER_NOT_INSTALLED`）→ 第二条红；反之第一条红。
    /// - 把两条 `assert!(!is_unrecoverable_restart_message(..))` 的前置删掉 → 无法区分「码腿生效」
    ///   与「keyword 恰好命中」，测试失去指向性（故保留为前置断言）。
    #[test]
    fn helper_gate_terminal_codes_are_unrecoverable_though_messages_match_no_keyword() {
        // 前置：两串中文文案对 keyword 腿是**完全不可见**的（缺陷根因）。
        assert!(
            !is_unrecoverable_restart_message(HELPER_GATE_ABORTED_MSG),
            "取消文案不含任何关键词 → 纯 message 匹配判不出终态"
        );
        assert!(
            !is_unrecoverable_restart_message(HELPER_NOT_INSTALLED_MSG),
            "未装文案不含任何关键词（“提权 helper”里没有“权限”）→ 纯 message 匹配判不出终态"
        );

        // 码腿把它们捞回终态：用户亲口取消 / 前置条件缺失，重试多少轮都不会自己变好。
        assert!(
            is_unrecoverable_restart_error(&StartError::coded(
                HELPER_GATE_ABORTED_MSG,
                code::HELPER_GATE_ABORTED
            )),
            "用户取消提权门 → 立即终态，不得再烧退避重试"
        );
        assert!(
            is_unrecoverable_restart_error(&StartError::coded(
                HELPER_NOT_INSTALLED_MSG,
                code::HELPER_NOT_INSTALLED
            )),
            "helper 未装（非交互自愈弹不了引导，每轮必然同样失败）→ 立即终态"
        );
    }

    /// **反向失效面**：码腿不得「改过头」把瞬态失败也判成终态 —— 那会让慢起/接管期退出的核**一次都
    /// 不重试**，比原缺陷更糟（原缺陷只是多烧几轮）。
    ///
    /// 变异有牙：把码腿放宽成 `err.code.is_some()`（任何带码错误即终态）→ 下方 `STARTUP_FAILED`
    /// 两条**双红**；把码腿写成 `!matches!(..)` 之类的取反 → 同样红。
    #[test]
    fn transient_start_failures_remain_retryable_regardless_of_code() {
        for msg in ["sing-box 起核超时", "sing-box 启动期退出"] {
            // 带 STARTUP_FAILED 码的瞬态失败：码腿不认，keyword 腿也不认 → 继续重试。
            assert!(
                !is_unrecoverable_restart_error(&StartError::coded(msg, code::STARTUP_FAILED)),
                "{msg}（STARTUP_FAILED）是瞬态失败，必须仍然重试"
            );
            // 无码腿（`From<String>` 升格，start_inner 里绝大多数失败腿）→ 同样继续重试。
            assert!(
                !is_unrecoverable_restart_error(&StartError::from(msg.to_string())),
                "{msg}（无码）必须仍然重试"
            );
        }
    }

    /// **keyword 腿不得被码腿挤掉**：spawn launch 失败把**原始 OS 错误**塞进 message 后贴
    /// `STARTUP_FAILED`（:1699-1702），EACCES 的 "Permission denied" 正从那儿来。若实现写成
    /// 「有码就 `return matches!(code, ..)`、不再看 message」，权限拒绝会退回烧满 3 轮退避。
    ///
    /// 变异有牙：把 `coded_terminal || is_unrecoverable_restart_message(..)` 改成
    /// `if let Some(c) = err.code { return c == ...HELPER_GATE_ABORTED || c == ...HELPER_NOT_INSTALLED }`
    /// （严格码优先）→ 本测**红**，而上面两测仍绿 ⇒ 只有这条守得住这个逃逸面。
    #[test]
    fn keyword_leg_still_applies_to_coded_errors() {
        assert!(
            is_unrecoverable_restart_error(&StartError::coded(
                "spawn sing-box 失败：Permission denied (os error 13)",
                code::STARTUP_FAILED
            )),
            "带 STARTUP_FAILED 码的权限拒绝仍须由 keyword 腿判终态（码腿不表达终态性 ≠ 可重试）"
        );
        // 无码的权限拒绝（既有行为）不得被回归破坏。
        assert!(is_unrecoverable_restart_error(&StartError::from(
            "Permission denied".to_string()
        )));
    }

    // ── Fix 3：起核外层重试预算 ──

    fn server_json(v: serde_json::Value) -> ServerConfig {
        serde_json::from_value(v).expect("server fixture")
    }

    #[test]
    fn resolve_start_retry_budget_widens_only_for_system_interface_node_on_supported_platform() {
        let plain = server_json(serde_json::json!({
            "id":"p","name":"p","protocol":"shadowsocks","address":"1.1.1.1","port":443
        }));
        let ts_system = server_json(serde_json::json!({
            "id":"t","name":"t","protocol":"tailscale","address":"","port":0,
            "tailscaleSettings": { "reverseMesh": true }
        }));
        let widened = StartRetryBudget {
            max_retries: 10,
            delay_ms: 3000,
            exponential_backoff: false,
        };
        let default = StartRetryBudget {
            max_retries: 2,
            delay_ms: 2000,
            exponential_backoff: true,
        };
        // TUN + darwin + 含 system_interface 节点 → 放宽。
        assert_eq!(
            resolve_start_retry_budget(true, &[plain.clone(), ts_system.clone()], "darwin"),
            widened
        );
        // Windows 禁 System（无双 TUN 竞态）→ 默认。
        assert_eq!(
            resolve_start_retry_budget(true, std::slice::from_ref(&ts_system), "win32"),
            default
        );
        // 非 TUN → 默认。
        assert_eq!(
            resolve_start_retry_budget(false, &[ts_system], "darwin"),
            default
        );
        // 无 system 节点 → 默认。
        assert_eq!(
            resolve_start_retry_budget(true, &[plain], "darwin"),
            default
        );
    }

    #[test]
    fn is_retryable_start_error_separates_transient_from_terminal() {
        // 端口/资源竞态 / 起核期退出 → 可重试。
        assert!(is_retryable_start_error("address already in use"));
        assert!(is_retryable_start_error("sing-box 启动期退出"));
        // 权限/找不到/配置无效 → 不重试（确定性失败）。
        assert!(!is_retryable_start_error("Permission denied (EACCES)"));
        assert!(!is_retryable_start_error("ENOENT: no such file"));
        assert!(!is_retryable_start_error("权限不足"));
        assert!(!is_retryable_start_error("invalid config: bad field"));
    }

    // ── Fix 5：config-gen I/O 落盘交接（写盘 + 孤儿清扫 + sync 只改不删）──

    fn smart_config_with_ext_rule() -> UserConfig {
        serde_json::from_value(serde_json::json!({
            "servers": [], "selectedServerId": "__direct__", "proxyMode": "smart",
            "customRules": [
                { "id":"r1", "type":"domain", "values":["a.com"], "action":"proxy", "enabled":true }
            ]
        }))
        .expect("smart config fixture")
    }

    #[tokio::test]
    async fn write_custom_rule_files_writes_expected_and_sweeps_orphans() {
        let (rt, dir) = test_runtime();
        let crdir = rt.custom_rules_dir();
        std::fs::create_dir_all(&crdir).unwrap();
        // 预置孤儿：裸 .json + 原子写残留 .tmp（均被 is_custom_rule_orphan_file 识别）。
        std::fs::write(crdir.join("custom-rule-stale.json"), "{}").unwrap();
        std::fs::write(crdir.join("custom-rule-x.json.abcdef012345.tmp"), "x").unwrap();
        // 非规则文件不得被误清（谓词不匹配）。
        std::fs::write(crdir.join("keep.txt"), "keep").unwrap();

        let cfg = smart_config_with_ext_rule();
        let expected = build_custom_rule_files(&cfg);
        assert!(
            expected.contains_key("custom-rule-r1.json"),
            "fixture 应产 ext 文件（否则本测无 teeth）"
        );

        rt.write_custom_rule_files(&cfg).await;

        // 期望文件落盘，内容逐字节 == 纯函数期望集。
        for (name, content) in &expected {
            let on_disk = std::fs::read_to_string(crdir.join(name)).unwrap();
            assert_eq!(&on_disk, content, "落盘内容须 == build_custom_rule_files");
        }
        // 孤儿清扫（不在期望集）。
        assert!(
            !crdir.join("custom-rule-stale.json").exists(),
            "裸 .json 孤儿须清"
        );
        assert!(
            !crdir.join("custom-rule-x.json.abcdef012345.tmp").exists(),
            ".tmp 孤儿须清"
        );
        // 非规则文件保留。
        assert!(
            crdir.join("keep.txt").exists(),
            "非 custom-rule 文件不得误清"
        );
        assert!(!rt.custom_rule_files_degraded(), "全成功不应降级");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// wiring 门：`start` 路径真调 `write_custom_rule_files`（generate 前落盘）。用 POLARIS_SINGBOX_PATH→目录
    /// 逼 `resolve_core_binary` 在 emit 后失败（不起真核），此刻外化规则文件已落盘。变异（删 `start_inner` 里
    /// `write_custom_rule_files` 调用）→ 文件不落 → 断言红。
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn start_lands_custom_rule_files_before_generate() {
        let (rt, dir) = test_runtime();
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst);
        let cfg = serde_json::json!({
            "servers": [], "selectedServerId": "__direct__", "proxyMode": "smart",
            "customRules": [{ "id":"r1", "type":"domain", "values":["a.com"], "action":"proxy", "enabled":true }]
        });
        // env 串行化（与其它 start 测共用 ENV_LOCK）：POLARIS_SINGBOX_PATH→目录 → resolve_core_binary 必 Err。
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("POLARIS_SINGBOX_PATH", &dir);
        let r = rt.start(cfg).await;
        std::env::remove_var("POLARIS_SINGBOX_PATH");
        drop(_g);
        assert!(
            r.is_err(),
            "核二进制解析失败 → 起核失败（但外化规则已落盘）"
        );
        assert!(
            rt.custom_rules_dir().join("custom-rule-r1.json").exists(),
            "start 路径须在 generate 前落盘外化规则文件（write_custom_rule_files 未接线则文件不存在）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // 起核可取消（真机事故「点连接锁死 UI ≈35s、启动卡死阶段无法关闭启动过程」的后端半）
    //
    // 事故形状：TUN 模式起核，孤儿 root 核锁死 cache 文件 → 核起来跑 ~9s 后 FATAL → 预算内重试
    // （3 次尝试 × ~9s + 2s/4s 退避）。让位检查点本来就齐（spawn 前持锁判 / 就绪门 / Dead·Timeout
    // 世代复查 / 就绪后复查），但它们**只在迭代边界执行** —— 卡在等待里时取消要静默等本轮走完。
    //
    // 下面四条门分别锁死：① 退避真被中断（非等睡满）② 取消腿落干净终态·无孤儿
    // ③ 唤醒边沿不丢（bump 早于注册也算）④ 没取消时绝不误中断（正常重试预算跑满）。
    // ══════════════════════════════════════════════════════════════════════════════

    /// ④ 无人接管 → 睡满并返 `false`（**改过头门**）。
    ///
    /// 变异：让 `sleep_unless_superseded_on` 无条件返 true / 让 select 的取消腿凭空提前完成 →
    /// 本测两条断言（返回值 + 实际耗时）同时转红。没有这条，「可取消」很容易做成「起核腿被自己
    /// 的取消信号打断」= 正常启动路径再也跑不完。
    #[tokio::test]
    async fn sleep_unless_superseded_sleeps_full_span_when_nobody_takes_over() {
        let gate = LifecycleGate::default();
        let signal = Notify::new();
        let my_gen = gate.generation();
        let t0 = std::time::Instant::now();
        let taken =
            sleep_unless_superseded_on(&gate, &signal, my_gen, Duration::from_millis(120)).await;
        let elapsed = t0.elapsed();
        assert!(
            !taken,
            "无人 bump 世代 → 必须报「未被接管」，否则正常起核会被自己的取消腿打断"
        );
        assert!(
            elapsed >= Duration::from_millis(110),
            "无人接管时必须睡满（实得 {elapsed:?}）—— 提前返回 = 退避被架空，重试节奏失真"
        );
    }

    /// ① 等待期被接管 → **立刻**醒（不是等睡满）。
    ///
    /// 变异：把 select 换回裸 `tokio::time::sleep(dur).await` → 取消要 3s 后才被发现 → 耗时断言转红。
    /// 这条就是「等 35s」那个形态的最小复现：等待本身不可中断时，取消只能在下一个迭代边界生效。
    #[tokio::test]
    async fn sleep_unless_superseded_wakes_immediately_on_takeover() {
        let gate = Arc::new(LifecycleGate::default());
        let signal = Arc::new(Notify::new());
        let my_gen = gate.generation();
        let (g2, s2) = (Arc::clone(&gate), Arc::clone(&signal));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            g2.bump_generation(); // ＝ stop() 入口做的事
            s2.notify_waiters();
        });
        let t0 = std::time::Instant::now();
        let taken =
            sleep_unless_superseded_on(&gate, &signal, my_gen, Duration::from_secs(3)).await;
        let elapsed = t0.elapsed();
        assert!(taken, "世代已变 → 必须报「被接管」");
        assert!(
            elapsed < Duration::from_millis(600),
            "取消必须就地生效（实得 {elapsed:?}，退避全长 3s）—— 等睡满即回归事故形态"
        );
    }

    /// ③ 唤醒边沿不丢：bump 发生在**注册之前**（信号已丢）也必须立刻判出被接管。
    ///
    /// 变异：删掉 `enable()` 之后那次世代复查、只靠 `notified` 分支 → `notify_waiters` 不留 permit ⇒
    /// 本测挂到睡满才返回 → 耗时断言转红。这是「信号 vs 真值」分工的门：信号会过期，世代不会。
    #[tokio::test]
    async fn sleep_unless_superseded_catches_takeover_that_happened_before_registration() {
        let gate = LifecycleGate::default();
        let signal = Notify::new();
        let my_gen = gate.generation();
        gate.bump_generation();
        signal.notify_waiters(); // 无等待者 → 通知即丢，只剩世代这条持久事实
        let t0 = std::time::Instant::now();
        let taken =
            sleep_unless_superseded_on(&gate, &signal, my_gen, Duration::from_secs(3)).await;
        assert!(
            taken,
            "注册前就发生的 bump 必须被复查捕获（信号已丢，世代还在）"
        );
        assert!(
            t0.elapsed() < Duration::from_millis(300),
            "应即刻返回，而非睡满"
        );
    }

    /// ① `bump_generation` 必须与唤醒同点落值 —— 绕过它直接 `gate.bump_generation()` 即回归。
    ///
    /// 两腿对照：走 wrapper 的腿 ~即刻醒；绕过 wrapper 的腿只能等睡满（正是「静默等 35s」）。
    /// 变异：把 wrapper 里的 `notify_waiters()` 删掉 → 第一条腿退化成第二条 → 转红。
    #[tokio::test]
    async fn bump_generation_wakes_waiters_but_raw_gate_bump_does_not() {
        let (rt, dir) = test_runtime();
        let my_gen = rt.gate.generation();

        // 腿 A：走 `ProxyRuntime::bump_generation`（生产路径）→ 立刻醒。
        let rt_a = Arc::clone(&rt);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            rt_a.bump_generation();
        });
        let t0 = std::time::Instant::now();
        assert!(
            rt.sleep_unless_superseded(my_gen, Duration::from_secs(3))
                .await
        );
        let via_wrapper = t0.elapsed();
        assert!(
            via_wrapper < Duration::from_millis(600),
            "经 bump_generation 的接管必须就地唤醒在飞起核腿（实得 {via_wrapper:?}）"
        );

        // 腿 B：绕过 wrapper 直接动 gate（＝把 `self.bump_generation()` 写回 `self.gate.bump_generation()`）。
        // 世代确实变了，但没人被叫醒 → 只能等睡满才发现。对照证明「唤醒」这一半是真在起作用的。
        let my_gen_b = rt.gate.generation();
        let rt_b = Arc::clone(&rt);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            rt_b.gate.bump_generation(); // 刻意绕过 wrapper
        });
        let t1 = std::time::Instant::now();
        assert!(
            rt.sleep_unless_superseded(my_gen_b, Duration::from_millis(400))
                .await
        );
        assert!(
            t1.elapsed() >= Duration::from_millis(380),
            "绕过 wrapper 时只能等睡满 —— 这条对照一旦变快，说明有第二个发信点（真值源分叉）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 假核落盘（0o755）。`run_body` 是收到 `run` 子命令时的 shell 体。
    ///
    /// **`check` 必须单独短路**：起核腿在 spawn 之前先跑一次 `sing-box check`（内核闸门，见
    /// `generate_and_gate`），而真核的 `check` 是**快速返回**的静态校验、只有 `run` 才常驻。
    /// 假核若对所有 argv 一视同仁，`check` 会跟着 `run` 的语义走 —— 常驻型假核会把闸门吊死到超时
    /// （实测：`cancelling_start_during_readiness_wait_reaps_the_real_process` 因此拿不到 pid 而红），
    /// 立退型假核则让闸门收到一个假的「配置无效」。二者都不是被测行为，是假核没跟上真核的契约。
    #[cfg(unix)]
    fn write_fake_core(dir: &std::path::Path, name: &str, run_body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        // 按**整条 argv** 找 `check`，不按位置：闸门发的是 `--disable-color check -c <path>`
        // （`check` 在 `$2`），而 spawn 发的是 `run -c <path>`。写死 `$1` 会漏掉前者（实测：漏了就等于
        // 假核对 check 走 run 的语义，常驻型假核把闸门吊到超时）。
        std::fs::write(
            &p,
            format!("#!/bin/sh\ncase \" $* \" in *\" check \"*) exit 0;; esac\n{run_body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// 假核（**立刻死**）：spawn 得起来、起来就退 → 就绪门判 Dead → `kill_core` → 退避重试。
    /// ＝真机 FATAL 循环的形状（把「跑 9s 后 FATAL」压成「立刻 FATAL」），不碰宿主网络。
    #[cfg(unix)]
    fn write_fake_dying_core(dir: &std::path::Path) -> PathBuf {
        write_fake_core(dir, "fake-dying-sing-box", "exit 1")
    }

    /// 按**完整命令行**数在跑的假核实例数（`pgrep -f <唯一临时路径>`）。
    ///
    /// 比「记一个 pid 再验它死没死」强的地方：**新** spawn 出来的孤儿也算得到。让位腿若不 return 而是
    /// 继续重试、且 spawn 临界区的世代判定被打断，多出来的那个核正是这样一个新 pid —— 只盯旧 pid 的
    /// 断言会漏判（这条是变异实测补上的：只验旧 pid 时「continue 而非 return」能活下来）。
    #[cfg(unix)]
    fn fake_core_proc_count(path: &std::path::Path) -> usize {
        std::process::Command::new("pgrep")
            .args(["-f", &path.to_string_lossy()])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
            .unwrap_or(0)
    }

    /// 假核（**活着但永不就绪**）：占住进程不退、绝不 bind 管理口 → 就绪门一直轮询。
    /// 用来验「取消发生在就绪等待期」时接管方是否真把它收割了（孤儿门要有真进程才有牙）。
    #[cfg(unix)]
    fn write_fake_hanging_core(dir: &std::path::Path) -> PathBuf {
        // exec：让 sleep 顶替 shell 成为受管 pid，SIGTERM 直达（否则杀的是 shell、sleep 变孤儿）。
        write_fake_core(dir, "fake-hanging-sing-box", "exec sleep 60")
    }

    /// ① **退避期取消 → 就地退场**（本任务的主门；直接对应「点了立刻停 vs 静默等 35s」）。
    ///
    /// 非 TUN 预算 = 3 次尝试、退避 2s→4s。本测在第 1 次退避（2s）中途点停止，断言起核腿在
    /// **远小于一个退避周期**内退场，并落干净终态。
    ///
    /// 变异（逐条转红）：
    /// - `sleep_start_backoff` 退回裸 `tokio::time::sleep` → 取消要等退避睡满才在轮首被发现 → 耗时断言红；
    /// - 取消腿写成 `continue` 而不是 `return` → 在接管方之上又起一次核 → pid/child 残留断言红；
    /// - `InflightGuard` 去掉 → `starting` 投影卡在 true → 终态断言红。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_start_interrupts_backoff_and_settles_clean() {
        let (rt, dir) = test_runtime();
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst);
        *rt.core_binary_override.lock().unwrap() = Some(write_fake_dying_core(&dir));

        let cfg = local_only_config(free_port());
        let rt2 = Arc::clone(&rt);
        let start = tokio::spawn(async move { rt2.start(cfg).await });

        // 等第 1 次尝试走完（spawn → 核即死 → 就绪门 Dead → kill_core）并进入 2s 退避。
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            rt.status().starting,
            "起核腿应仍在飞 —— `starting` 投影是托盘/UI 判「此刻正在启动」的唯一依据"
        );

        // 用户点停止。
        let t0 = std::time::Instant::now();
        rt.stop().await.expect("停止应成功");
        let out = tokio::time::timeout(Duration::from_secs(3), start)
            .await
            .expect("取消后起核腿必须迅速退场 —— 超时即回归「静默等睡满」")
            .expect("起核任务不应 panic");
        let elapsed = t0.elapsed();

        assert!(
            out.is_ok(),
            "用户主动取消是达成意图、不是失败：让位腿须返 Ok，绝不落 STARTUP_FAILED 弹红框；实得 {out:?}"
        );
        assert!(
            elapsed < Duration::from_millis(1000),
            "取消延迟必须 ≪ 一个退避周期（2s）；实得 {elapsed:?} —— 超出即说明又在等睡满"
        );
        // 干净终态：无半启动状态、无残留句柄。
        let st = rt.status();
        assert!(!st.running, "取消后不得自称 running");
        assert!(
            !st.starting,
            "取消后在飞计数必须归零（InflightGuard 兜底所有出口）"
        );
        assert!(st.error.is_none(), "主动取消不得留错误态");
        assert!(rt.pid.lock().unwrap().is_none(), "取消后不得残留 pid");
        assert!(
            rt.child.lock().unwrap().is_none(),
            "取消后不得残留 child 句柄"
        );
        assert!(
            !rt.core_via_helper.load(Ordering::SeqCst),
            "取消后 helper 受管标记必须清（否则下次 kill_core 走错腿）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ② **就绪等待期取消 → 真进程被收割，不留孤儿**（孤儿门；用真活着的假核才有牙）。
    ///
    /// 假核活着但永不就绪 → 起核腿卡在就绪轮询。此时 stop：世代 bump 唤醒轮询 sleep → 让位腿
    /// （Superseded）**不 kill**（接管方拥有进程所有权），由 stop 的 `kill_core` 收割。
    ///
    /// 变异：让位腿改成自己 `kill_core` 再 return（看似"更干净"）→ 与接管方争抢句柄；
    /// 或让位腿改成 `continue` 重起一次核 → 老核失联 = 孤儿 → `ps` 实证断言转红。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_start_during_readiness_wait_reaps_the_real_process() {
        let (rt, dir) = test_runtime();
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst);
        let fake = write_fake_hanging_core(&dir);
        *rt.core_binary_override.lock().unwrap() = Some(fake.clone());

        let cfg = local_only_config(free_port());
        let rt2 = Arc::clone(&rt);
        let start = tokio::spawn(async move { rt2.start(cfg).await });

        // 等核 spawn 出来并进入就绪轮询（永不就绪）。
        tokio::time::sleep(Duration::from_millis(400)).await;
        let pid = rt.pid.lock().unwrap().expect("此刻应已 spawn 出受管核 pid");
        assert!(
            ps_alive(pid),
            "前提：假核应在跑（ps 实证），否则本测测不到孤儿面"
        );

        let t0 = std::time::Instant::now();
        rt.stop().await.expect("停止应成功");
        let out = tokio::time::timeout(Duration::from_secs(5), start)
            .await
            .expect("取消后起核腿必须迅速退场")
            .expect("起核任务不应 panic");
        let elapsed = t0.elapsed();

        assert!(out.is_ok(), "就绪等待期被接管 = 让位，返 Ok；实得 {out:?}");
        assert!(
            elapsed < Duration::from_secs(3),
            "取消应就地生效；实得 {elapsed:?}"
        );
        // **孤儿门**：ps ground truth，不信 status 自述。
        assert!(
            !ps_alive(pid),
            "取消后受管核 pid={pid} 必须已被收割 —— 活着 = 孤儿（正是本次事故里锁死 cache 文件的那种）"
        );
        // 更宽的一张网：**任何**本假核实例都不许留着。只验旧 pid 会漏掉「取消腿没 return、又 spawn 了
        // 一个」这条逃逸路径 —— 那个新核的 pid 根本不在旧断言的射程里（变异实测补）。
        assert_eq!(
            fake_core_proc_count(&fake),
            0,
            "取消后不得有任何假核实例存活（含让位腿又新起的那种 = 谁也不认领的孤儿）"
        );
        assert!(!rt.status().running);
        assert!(!rt.status().starting, "在飞计数必须归零");
        assert!(
            rt.child.lock().unwrap().is_none(),
            "child 句柄必须已被接管方取走并收割"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ④ **没人取消时，重试预算必须原样跑满**（改过头门的端到端形态）。
    ///
    /// 假核每次都立刻死 → 3 次尝试 + 2s + 4s 退避 → 终态 Err(STARTUP_FAILED)。
    /// 变异：取消信号误触发（如 select 的取消腿写成恒就绪、或 `notify_waiters` 被无关路径调用）→
    /// 起核腿会提前返 Ok(让位) → 「必须是 Err」与「必须耗满退避」两条同时转红。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn uncancelled_start_still_burns_the_whole_retry_budget() {
        let (rt, dir) = test_runtime();
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst);
        *rt.core_binary_override.lock().unwrap() = Some(write_fake_dying_core(&dir));

        let t0 = std::time::Instant::now();
        let r = rt.start(local_only_config(free_port())).await;
        let elapsed = t0.elapsed();

        let err = r.expect_err("三次尝试全失败 → 必须落终态 Err（不得被取消腿吞成 Ok）");
        assert_eq!(
            err.code,
            Some(code::STARTUP_FAILED),
            "起核期退出耗尽预算 → STARTUP_FAILED"
        );
        assert!(
            elapsed >= Duration::from_millis(5_500),
            "无人接管时两次退避（2s+4s）必须真睡满；实得 {elapsed:?} —— 变短即说明退避被取消信号误中断"
        );
        assert!(!rt.status().starting, "终态后在飞计数必须归零");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sync_custom_rule_files_updates_content_but_never_deletes() {
        let (rt, dir) = test_runtime();
        let crdir = rt.custom_rules_dir();
        std::fs::create_dir_all(&crdir).unwrap();
        // 预置一个「本轮期望集之外」的既存文件：sync **绝不删**（运行中删被挂载文件会致 sing-box reload 报错）。
        std::fs::write(crdir.join("custom-rule-stale.json"), "stale").unwrap();

        let cfg = smart_config_with_ext_rule();
        let expected = build_custom_rule_files(&cfg);

        rt.sync_custom_rule_files(&cfg).await;

        // 期望文件被写（内容变 → 原子替换）。
        for (name, content) in &expected {
            assert_eq!(&std::fs::read_to_string(crdir.join(name)).unwrap(), content);
        }
        // 绝不删：本轮期望集外的既存文件仍在（删除只在起核 write_custom_rule_files 清扫）。
        assert!(
            crdir.join("custom-rule-stale.json").exists(),
            "sync 绝不删文件（仅起核清扫删孤儿）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 变异②守卫：成功/让位腿（`Ok`）绝不清系统代理（去掉 success 守卫 → 本测转红）。
    #[tokio::test]
    async fn success_leg_never_clears_system_proxy() {
        let (rt, _dir, calls) = test_runtime_recording();
        let g = rt.gate.generation();
        rt.maybe_clear_system_proxy_on_start_failure(&Ok(ProxyStatus::default()), g)
            .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "成功腿绝不清——正在跑的核的系统代理不能被误清"
        );
    }

    /// 变异①守卫（stopping）：世代已被更新的 stop/start 接管的失败**不清**（去掉世代守卫 → 转红）。
    /// stop 入口必先 bump_generation，故「被主动停止/更新覆盖」⟺「世代已变」。
    #[tokio::test]
    async fn superseded_failure_does_not_clear_system_proxy() {
        let (rt, _dir, calls) = test_runtime_recording();
        let my_gen = rt.gate.generation();
        rt.gate.bump_generation(); // 模拟并发 stop/start 接管（stop 入口先 bump）。
        rt.maybe_clear_system_proxy_on_start_failure(
            &Err(StartError::from("boom".to_string())),
            my_gen,
        )
        .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "被接管的失败不清（stopping 守卫）——交接管方收口，防 C1 清了又被设回"
        );
    }

    /// 变异①守卫（正例）：世代**未变**的真失败必清（与上一条构成守卫的双向锁）。
    #[tokio::test]
    async fn same_generation_failure_clears_system_proxy() {
        let (rt, _dir, calls) = test_runtime_recording();
        let g = rt.gate.generation();
        rt.maybe_clear_system_proxy_on_start_failure(&Err(StartError::from("boom".to_string())), g)
            .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "世代未变的真失败（本想启动却失败）必清"
        );
    }

    /// 变异③相邻：**真实生产控制器**（无 marker）挂在 start 失败腿上必须完全惰性——
    /// 返回 Err 且**不凭空造出 marker 文件**（门控 1 在任何系统调用前短路）。这证明「fresh start
    /// 无 marker → no-op」这条「挂每个失败腿都安全」的前提，在**真装配**（非 mock）上成立。
    /// （`ensure_cleared` 本身的门控 1 幂等由 system-integration 的
    /// `ensure_cleared_noop_without_marker` / `production_proxy_controller_is_inert_without_marker` 锁死。）
    #[tokio::test]
    async fn production_controller_inert_on_start_failure() {
        let (rt, dir) = test_runtime(); // 真实 production_proxy_controller + 临时 marker 路径。
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst);
        let marker = dir.join(polaris_system_integration::PROXY_MARKER_FILENAME);
        assert!(!marker.exists(), "前置：无 marker");
        let r = rt.start(bad_config()).await;
        assert!(r.is_err(), "坏配置必失败");
        assert!(
            !marker.exists(),
            "无 marker 的失败收口必须零副作用——绝不凭空造 marker / 触碰系统代理"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 组合面·**对称门**：主动停止（`stop`）真调系统代理收口器（维度7 #8 对称面）。停核后系统代理若仍
    /// 指向刚被杀的本地死端口 → 全网断，故 `stop` 必须像 start 失败腿一样过 `ensure_cleared`。
    /// 打生产入口 `stop`（非直调 `clear_system_proxy`），断言收口器**真被接线到停止路径**——§K7.1
    /// 「两扇门之间的缝才是生产路径」。marker 门控幂等由 system-integration 单测锁死，本处只验接线。
    #[tokio::test]
    async fn deliberate_stop_invokes_system_proxy_clearer() {
        let (rt, dir, calls) = test_runtime_recording();
        rt.stop().await.expect("停核应成功（清理失败也不阻断停止）");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "主动停止必调一次 ensure_cleared（清指向死端口的系统代理）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **超预算残 stop 的晚落地换代毒性**：拆除途中被新会话接管 → 余下步骤整段让位。
    ///
    /// # 复现的真机形态
    ///
    /// `helper_uninstall` 的看门狗收停是**有预算**的（`WATCHDOG_JOIN_BUDGET`）：在飞的 `proxy.stop()`
    /// 挂过 20s（macOS `networksetup` exec 卡死 / `spawn_blocking` 饥饿）后命令直接返回，那次 stop 成为
    /// **残任务**继续挂着。用户此时重装 helper 并起了新核 —— 残 stop 随后醒来，后半段每一步都落在
    /// **新会话**上：抹 running 态、清新核的 race sidecar 注入态（节点域名解析静默 SERVFAIL）、
    /// 还原新核接管的系统 DNS、清掉新会话刚设好的系统代理。
    ///
    /// # 窗口是**确定性**的，不靠 sleep 赌时序
    ///
    /// 测试先占住 `mesh.exit_route` 锁：那是 `stop_inner` 拆除段第一个必然挂起的 await
    /// （`lock().await` 拿不到就一定 Pending）⇒ 停核腿**不可能**越过它。于是「等它 bump 出自己的世代
    /// → 再 bump 一次冒充新 start → 放锁」这个序列，必然把换代插在它的某个检查点之前。
    /// （current_thread 运行时：观测到世代变化与随后的 `bump_generation()` 之间没有 await，
    /// 停核腿不可能在这两句之间推进。）
    ///
    /// **变异实跑**：删掉 `stop_inner` 里任一 `stop_superseded` 早退 → 对应断言转红；
    /// 把 `stop` 的 `if self.stop_inner().await` 改回无条件 `clear_system_proxy()` → 第三条转红。
    #[tokio::test]
    async fn superseded_stop_teardown_stands_down_instead_of_clobbering_the_new_session() {
        let (rt, dir, clears) = test_runtime_recording();
        let refreshes: ExitIpRefreshes = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            exit_ip_refreshes: Arc::clone(&refreshes),
            ..Default::default()
        }));
        // 冒充「新会话已经起来了」：running 态 + 热切基准快照。残 stop 不让位就会把两者一起抹掉。
        if let Ok(mut g) = rt.status.write() {
            g.running = true;
        }
        if let Ok(mut g) = rt.switch_snapshot.write() {
            *g = Some(SwitchSnapshot::default());
        }
        // 新会话已提交的 sidecar 注入态：残 stop 的 `clear_race_server()`（`None` 腿无条件清）会把它
        // 抹成 0 ⇒ 新核 config 里烧的端口没人听 ⇒ 节点域名解析静默 SERVFAIL。
        if let Ok(mut g) = rt.race_server.lock() {
            g.port = 5353;
        }

        let gen0 = rt.gate.generation();
        // 占位任务先拿住 exit_route 锁（拿到才发 `acquired`），停核腿于是必然堵在那个 await 上。
        let (acquired, release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let holder = {
            let (mesh, a, r) = (
                Arc::clone(&rt.mesh),
                Arc::clone(&acquired),
                Arc::clone(&release),
            );
            tokio::spawn(async move { mesh.occupy_exit_route_lock_for_test(a, r).await })
        };
        acquired.notified().await;
        let stopper = {
            let rt = Arc::clone(&rt);
            tokio::spawn(async move { rt.stop().await })
        };
        // 等停核腿领到它自己的世代（= 已进 stop_inner），且它此刻必然堵在 exit_route 锁上。
        let mut spins = 0;
        while rt.gate.generation() == gen0 {
            tokio::task::yield_now().await;
            spins += 1;
            assert!(spins < 10_000, "停核腿始终没 bump 世代 —— 前置假设已失效");
        }
        rt.bump_generation(); // ← 新一轮 start 接管（用户重装 helper 后点了连接）
        release.notify_one(); // 放锁：停核腿醒来，撞上换代守卫
        holder.await.expect("占位任务不得 panic");
        stopper
            .await
            .expect("停核任务不得 panic")
            .expect("stop 恒 Ok");

        assert!(
            rt.status().running,
            "残 stop 让位后不得把新会话的 running 态抹成 default（前端会显示「已断开」而核还跑着）"
        );
        assert!(
            rt.switch_snapshot.read().unwrap().is_some(),
            "让位后不得清掉新会话的热切基准（清了 ⇒ 下次 switch_mode 拿不到 id→tag 基准）"
        );
        assert_eq!(
            rt.race_server_port(),
            5353,
            "让位后不得清新会话的 race sidecar 注入态（清了 ⇒ 内核对死口做节点域名解析，静默 SERVFAIL）"
        );
        assert_eq!(
            clears.load(Ordering::SeqCst),
            0,
            "让位后不得清系统代理：此刻的系统代理属**新会话**，清了 = 用户全网走直连"
        );
        assert!(
            refreshes.lock().unwrap().is_empty(),
            "让位后不得按「停核」语义重探出口 IP（新核跑着，直连出口不是真值）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 正例（与上一条构成双向锁）：**没有**换代时，停核腿必须照常跑完全部拆除并清系统代理。
    ///
    /// 缺这条，「让位判据写成恒真」就是一条无声的回归：停核从此什么都不做，而上面那条照样绿。
    #[tokio::test]
    async fn unsuperseded_stop_completes_the_whole_teardown() {
        let (rt, dir, clears) = test_runtime_recording();
        let refreshes: ExitIpRefreshes = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            exit_ip_refreshes: Arc::clone(&refreshes),
            ..Default::default()
        }));
        if let Ok(mut g) = rt.status.write() {
            g.running = true;
        }
        if let Ok(mut g) = rt.switch_snapshot.write() {
            *g = Some(SwitchSnapshot::default());
        }
        if let Ok(mut g) = rt.race_server.lock() {
            g.port = 5353;
        }

        rt.stop().await.expect("stop 恒 Ok");

        assert!(
            !rt.status().running,
            "未被接管 → running 态必须被抹成 default"
        );
        assert!(
            rt.switch_snapshot.read().unwrap().is_none(),
            "未被接管 → 热切基准必须失效"
        );
        assert_eq!(
            rt.race_server_port(),
            0,
            "未被接管 → sidecar 注入态必清（下次起核按新配置重建）"
        );
        assert_eq!(clears.load(Ordering::SeqCst), 1, "未被接管 → 系统代理必清");
        assert_eq!(
            *refreshes.lock().unwrap(),
            vec![false],
            "未被接管 → 按停核语义零延迟重探直连出口"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🟠 **配对扫描**：`stop_inner` 的拆除段里，**每一个 `.await` 之后都必须紧跟一次换代检查**，
    /// 且二者严格交替。
    ///
    /// # 为什么行为测试盖不住这一条
    ///
    /// 上面那条行为测试只能证明「某一个检查点确实拦住了残 stop」—— 它在 `exit_route_clear` 那个
    /// 挂起点造窗口，于是删掉别的检查点它照样绿。而每个 await 都是一个独立的挂机窗口（`kill_core`
    /// 的 SIGTERM→宽限→SIGKILL / helper 阻塞 IPC、`restore_system_dns` 的两次系统 exec），漏掉哪个
    /// 都等于那一段的换代毒性原样保留。判据是**结构**的：换代只可能发生在让出执行权的地方，所以
    /// 「await 数 == 检查数且交替」就是完备的配对条件，而且将来有人往拆除段加第四个 await 时会自动转红。
    ///
    /// 牙：删掉任一 `stop_superseded` → 交替断言转红；往拆除段加一个不带检查的 `.await` → 同样转红。
    #[test]
    fn stop_teardown_yields_after_every_await() {
        const SRC: &str = include_str!("proxy.rs");
        let body = method_body(SRC, "async fn stop_inner(self: &Arc<Self>) -> bool {");
        let mut marks: Vec<(usize, &str)> = body
            .match_indices(".await")
            .map(|(i, _)| (i, "await"))
            .chain(
                body.match_indices("self.stop_superseded(my_gen,")
                    .map(|(i, _)| (i, "check")),
            )
            .collect();
        marks.sort_unstable();
        assert!(
            marks.len() >= 6,
            "锚点漂了或拆除段被改瘦：只扫到 {} 个标记（期望 ≥3 个 await + 3 次检查）",
            marks.len()
        );
        let seq: Vec<&str> = marks.into_iter().map(|(_, k)| k).collect();
        assert!(
            seq.len().is_multiple_of(2) && seq.chunks(2).all(|c| c == ["await", "check"]),
            "拆除段的 await 与换代检查必须严格交替（实得 {seq:?}）—— \
             缺检查的那个 await 就是残 stop 晚落地时的换代毒性窗口"
        );
    }

    /// 变异守卫（与上一条 + `restart_start_leg_failure` 构成双向锁）：`restart` 的**停核腿不清**系统代理
    /// （瞬态停核，紧接 start 重建）。此处 restart 的 start 腿用坏配置**必失败** → 全程唯一的一次清来自
    /// **start 失败腿**，而非停核腿。若把清挂进 `stop_inner`（restart 复用腿），本测将读到 2 次而转红。
    #[tokio::test]
    async fn restart_stop_leg_does_not_clear_system_proxy() {
        let (rt, dir, calls) = test_runtime_recording();
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst); // 跳过 /proc 孤儿清扫，聚焦清理计数。
        let r = rt.restart(bad_config()).await;
        assert!(r.is_err(), "restart 的 start 腿坏配置必失败");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "restart 全程恰一次清（来自 start 失败腿）；停核腿绝不清，否则重建前留无代理窗口"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // 诊断两轴计数喂数（§O1 缺口修复）——**组合面门**（§K7.1）
    //
    // 不测「DiagnosticCounters 函数」也不测「proxy 起核」，而是打生产路径的缝：
    //   慢起轴：真起就绪门（带真实重试）→ ProxyRuntime 累计 → diagnostic_counters() → build_diagnostic_report 渲染非零行。
    //   核崩轴：崩溃自愈机计数 → diagnostic_counters() **投影** → 报告渲染非零行。
    // 两轴各自单一来源、绝不互写（维度7 #11：慢起 ≠ 核崩，混为一谈会误报核崩）。
    // ══════════════════════════════════════════════════════════════════════════════

    /// 用给定两轴计数造一份最小诊断报告（组合面：真调 `build_diagnostic_report`，验证行是否渲染）。
    fn report_with_counters(counters: DiagnosticCounters) -> String {
        use polaris_stats_engine::{
            build_diagnostic_report, DiagnosticReportInput, RuntimeSection,
        };
        let input = DiagnosticReportInput {
            runtime: RuntimeSection {
                counters,
                ..RuntimeSection::default()
            },
            ..DiagnosticReportInput::default()
        };
        build_diagnostic_report(&input)
    }

    /// 组合面·慢起轴：真起就绪门（带重试）→ 慢起轴真被喂 → 报告读到非零「就绪重试」行。
    ///
    /// 不经真核（无需 sing-box 二进制）：放一个真·存活子进程（`sleep`）满足 `is_alive`，
    /// 管理 API 端口**延迟**监听 → 就绪探测头几轮失败（真实重试）→ 监听起来后 `Ready`。
    /// 全程仅 127.0.0.1，不触碰宿主网络。**变异门**：去掉 `on_retry`→record_retry 接线 → 慢起轴恒 0 → 本测转红。
    #[tokio::test(flavor = "multi_thread")]
    async fn diagnostic_slow_start_axis_fed_and_rendered() {
        let (rt, dir) = test_runtime();
        let my_gen = rt.gate.generation();

        // 真·存活子进程当「核」（is_alive 靠它）；用完只杀我们自己起的这个。
        // Windows 无 sleep.exe（sleep 只是 PS cmdlet）→ 按平台选常驻占位进程。
        let mut cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("powershell");
            c.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"]);
            c
        } else {
            let mut c = tokio::process::Command::new("sleep");
            c.arg("30");
            c
        };
        let child = cmd.spawn().expect("spawn 占位核");
        *rt.child.lock().unwrap() = Some(child);

        // 管理 API 端口：先占一个空闲口，延迟 ~700ms 再真正监听 → 头几轮就绪探测真失败（真实重试）。
        let port = free_port();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(700)).await;
            // 监听但不 accept：TcpStream::connect 成功即「就绪」。持有到测试结束。
            let _l = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let outcome = rt.wait_ready(port, my_gen).await;
        assert_eq!(outcome, CoreReadyOutcome::Ready, "延迟监听后必最终就绪");

        let counters = rt.diagnostic_counters();
        assert!(
            counters.last_start_ready_retries >= 1,
            "慢起轴必须真被喂（延迟就绪 → ≥1 次重试）；实得 {}",
            counters.last_start_ready_retries
        );

        // 组合面收口：喂进真实报告构建器，断言「就绪重试」行真渲染（非零才渲染）。
        let md = report_with_counters(counters);
        assert!(
            md.contains("次就绪重试才成功"),
            "诊断报告必须渲染慢起轴行（生产路径读到非零）"
        );
        assert!(
            !md.contains("核崩溃自动重启"),
            "无崩溃 → 核崩轴行不应出现（两轴独立）"
        );

        rt.kill_core().await; // 只杀我们起的 sleep 占位核
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 组合面·核崩轴：`restart_count` 从 [`CrashRecoveryMachine`] **读时投影** → 报告渲染「核崩溃自动重启」行。
    ///
    /// 无需真崩溃：直接驱动崩溃自愈机计数（与 `run_crash_recovery` 同一 `attempt_crash` 入口）。
    /// 锁死投影接线（去掉 `diagnostic_counters` 里的投影 → 本测转红），也证明**没有**在本地并行 record_restart
    /// （核崩轴的唯一真值就是崩溃机）。
    #[test]
    fn diagnostic_crash_axis_projected_from_recovery_machine_and_rendered() {
        let (rt, dir) = test_runtime();

        // 初始两轴皆 0 → 报告无任一行。
        let c0 = rt.diagnostic_counters();
        assert_eq!(c0.restart_count, 0);
        assert_eq!(c0.last_start_ready_retries, 0);
        assert!(!report_with_counters(c0).contains("核崩溃自动重启"));

        // 驱动崩溃自愈机计数两次（真实自愈路径同一 attempt 入口；期间不动慢起轴）。
        let now = now_ms();
        let gen = rt.gate.generation();
        rt.crash_lock().attempt_crash(now, gen); // restart_count=1，in-flight
        rt.crash_lock().post_start_failure(false); // 复位 in-flight，计数保留
        rt.crash_lock().attempt_crash(now, gen); // restart_count=2

        let c = rt.diagnostic_counters();
        assert_eq!(
            c.restart_count, 2,
            "核崩轴必须从 CrashRecoveryMachine 投影进快照（单一真值）"
        );
        assert_eq!(c.last_start_ready_retries, 0, "驱动崩溃机不得污染慢起轴");
        let md = report_with_counters(c);
        assert!(
            md.contains("核崩溃自动重启：2 次"),
            "报告必须渲染核崩轴行（读到非零投影值）"
        );
        assert!(!md.contains("次就绪重试才成功"), "慢起轴 0 → 该行不渲染");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 组合面·两轴独立同现：慢起轴（`diagnostics`）+ 核崩轴（`crash_recovery`）各自来源，
    /// `diagnostic_counters()` 合并后两行都渲染，且互不写入对方（维度7 #11 两轴不混）。
    #[test]
    fn diagnostic_two_axes_combine_independently_in_snapshot() {
        let (rt, dir) = test_runtime();

        // 慢起轴：喂 2 次就绪重试（与 wait_ready 同一 begin/record/finish API）。
        {
            let mut a = rt.diag_lock().begin_start();
            a.record_retry();
            a.record_retry();
            rt.diag_lock().finish_start(&a);
        }
        // 核崩轴：崩溃自愈机计数 1 次。
        rt.crash_lock()
            .attempt_crash(now_ms(), rt.gate.generation());

        let c = rt.diagnostic_counters();
        assert_eq!(c.last_start_ready_retries, 2, "慢起轴来自 diagnostics");
        assert_eq!(c.restart_count, 1, "核崩轴来自 crash_recovery（投影）");

        let md = report_with_counters(c);
        assert!(md.contains("2 次就绪重试才成功"), "慢起轴行");
        assert!(md.contains("核崩溃自动重启：1 次"), "核崩轴行");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // 真机验证（**非 CI 门**）
    //
    // §K7 教训：「夹具缺失就 return 的门 = 没有门」。故此处不写「env 没设就静默 return」的假门——
    // 而是 `#[ignore]`：CI 里它显式显示为 ignored（不冒充通过），由人显式跑：
    //   POLARIS_SINGBOX_PATH=<某个可用的 sing-box 二进制路径> \
    //     cargo test -p polaris --bin polaris -- --ignored --nocapture
    // 前置缺失时**panic 报错**，不跳过。
    //
    // 安全硬约束：config 恒 `proxyModeType: manual` + 全局直连 + 仅 127.0.0.1 监听
    //   → 不接管系统网络、无 TUN、无系统代理。**绝不可改成 tun/systemProxy**（会破坏宿主网络）。
    // ══════════════════════════════════════════════════════════════════════════════

    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// 真机验证用最小 config：manual 模式 + 全局直连 + 仅本地混合入站。
    fn local_only_config(mixed: u16) -> Value {
        serde_json::json!({
            "servers": [],
            "selectedServerId": "__direct__", // DIRECT_SERVER_ID：全局直连，无真实节点
            "proxyMode": "direct",
            "proxyModeType": "manual",        // 安全：不接管系统代理、不建 TUN
            "mixedPort": mixed,
        })
    }

    fn require_core() -> String {
        std::env::var("POLARIS_SINGBOX_PATH")
            .expect("真机验证需 POLARIS_SINGBOX_PATH 指向真实 sing-box 二进制（前置缺失即失败，不静默跳过）")
    }

    /// `ps -p <pid>` 实证进程存在（不信 status 自述，走系统 ground truth）。
    fn ps_alive(pid: u32) -> bool {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .output()
            .map(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())
            })
            .unwrap_or(false)
    }

    /// 系统内 sing-box 进程数（孤儿检测）。
    ///
    /// 用 `pgrep -x`（**精确进程名**）而非 `ps | grep 'sing-box run'`：后者会把「命令行里含该字面量」
    /// 的 shell/测试进程本身算进去 —— 我在人工核对时就被这个假计数骗过一次（报 3 实为 0）。
    fn singbox_proc_count() -> usize {
        std::process::Command::new("pgrep")
            .args(["-x", "sing-box"])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .count()
            })
            .unwrap_or(0)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "真机验证：需 POLARIS_SINGBOX_PATH 指向真实 sing-box；非 CI 门"]
    async fn real_core_full_lifecycle() {
        use futures::StreamExt;
        use polaris_singbox_grpc::{Endpoint, ReconnectConfig, SingBoxApiClient};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 前置断言：env 未设即失败（不静默跳过）。resolve_core_binary 读同一个 env，
        // 故此处**不 set/remove**——两个真机测试同进程跑，改动进程级 env 会互相踩（实测：
        // 先跑完的那个 remove 掉，后跑的 require_core 直接 panic）。
        require_core();
        let (rt, dir) = test_runtime();
        let mixed = free_port();
        // 装日志 sink：否则 log:: 全是 no-op，核的 stdout/stderr 无处可看（也顺带验证 logging.rs 接线）。
        crate::logging::init(&dir);

        // ── ① spawn + 就绪 ──────────────────────────────────────────────────────
        let st = rt
            .start(local_only_config(mixed))
            .await
            .expect("起核应成功");
        println!(
            "[①] start → running={} pid={} mixedPort={} apiPort={}",
            st.running, st.pid, st.mixed_port, st.clash_api_port
        );
        assert!(st.running, "start 后必须 running");
        assert_ne!(st.pid, 0, "必须拿到真实 pid");
        assert_eq!(
            st.mixed_port, mixed,
            "mixedPort 必须来自 config，不是硬编码 7890"
        );
        assert_ne!(st.clash_api_port, 0, "管理 API 端口必须已解析");
        assert!(
            ps_alive(st.pid),
            "[①] ps 必须能看到 pid={} —— 进程真在跑",
            st.pid
        );
        println!("[①] ps -p {} → 进程存在 ✓", st.pid);

        // ── ② 管理 API 真的通（h2c gRPC unary RPC，非 clash REST）─────────────────
        let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", st.clash_api_port), "")
            .await
            .expect("[②] 管理 API gRPC 连接应成功");
        client
            .close_all_connections()
            .await
            .expect("[②] CloseAllConnections unary RPC 应成功返回");
        println!("[②] gRPC CloseAllConnections → OK（h2c 管理 API 真的通）✓");

        // ── ③ stats 数据面真的有数据 ────────────────────────────────────────────
        // `ReconnectingStream` 是**首次 poll 才真正连**的懒流：若只是建好流对象就去造流量，
        // 订阅其实尚未建立 → 错过该连接的 NEW 事件，能否看到它就取决于核会不会为一条空闲连接
        // 再补发 UPDATE ⇒ 测试随机红（实测 2 轮 1 红）。故这里**先起后台 drain 把订阅真正拉起**，
        // 再造流量，NEW 事件必到。
        let conn_stream =
            client.subscribe_connections(200_000_000 /* 200ms */, ReconnectConfig::default());
        let collected: Arc<Mutex<Vec<polaris_stats_engine::ConnectionEntry>>> =
            Arc::new(Mutex::new(Vec::new()));
        let sink = collected.clone();
        tokio::spawn(async move {
            let mut s = Box::pin(conn_stream);
            while let Some(ev) = s.next().await {
                for e in ev.events {
                    if let Some(conn) = e.connection {
                        sink.lock()
                            .unwrap()
                            .push(polaris_stats_engine::ConnectionEntry {
                                id: conn.id.clone(),
                                chains: conn.chain_list.clone(),
                                rule: conn.rule.clone(),
                                metadata: None,
                                upload: Some(conn.uplink_total as u64),
                                download: Some(conn.downlink_total as u64),
                                start: None,
                            });
                    }
                }
            }
        });
        let mut status_stream = client.subscribe_status(200_000_000, ReconnectConfig::default());
        // 等订阅真正建立（懒流首帧）后再造流量。
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 本地回显 HTTP 服务器（仅 127.0.0.1；不出网）。
        let srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let srv_port = srv.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = srv.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = s.read(&mut buf).await;
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello-polar")
                        .await;
                    // 保持连接开着，让 stats 有活连接可报。
                    tokio::time::sleep(Duration::from_secs(6)).await;
                });
            }
        });

        // 经混合入站发一个 HTTP 代理请求（目标是本地回显服务器）。
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", mixed))
            .await
            .expect("[③] 混合入站应可连（端口来自 config）");
        let req = format!(
            "GET http://127.0.0.1:{srv_port}/ HTTP/1.1\r\nHost: 127.0.0.1:{srv_port}\r\n\r\n"
        );
        c.write_all(req.as_bytes()).await.unwrap();
        let mut resp = vec![0u8; 128];
        let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut resp))
            .await
            .expect("[③] 经代理读响应超时")
            .expect("[③] 经代理读响应失败");
        let body = String::from_utf8_lossy(&resp[..n]);
        assert!(
            body.contains("200 OK"),
            "[③] 经混合入站的请求应拿到 200，实得：{body}"
        );
        println!(
            "[③] 经 mixed:{mixed} 代理请求 → {} ✓",
            body.lines().next().unwrap_or("")
        );

        // 等后台 drain 收到真实连接事件（拓扑 aggregate_connections 的供数源）。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while tokio::time::Instant::now() < deadline && collected.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let entries = collected.lock().unwrap().clone();
        assert!(
            !entries.is_empty(),
            "[③] Connections 流必须报出真实连接（拓扑 aggregate_connections 的供数源）"
        );
        println!("[③] Connections 流 → {} 条真实连接：", entries.len());
        for e in entries.iter().take(3) {
            println!(
                "      id={} rule={:?} chains={:?} up={:?} down={:?}",
                e.id, e.rule, e.chains, e.upload, e.download
            );
        }
        let agg = polaris_stats_engine::aggregate_connections(&entries, 0);
        println!("[③] aggregate_connections → {agg:?}");

        // Status 流也应给出真实数字。
        let s0 = tokio::time::timeout(Duration::from_secs(5), status_stream.next())
            .await
            .expect("[③] Status 流应在 5s 内出帧")
            .expect("[③] Status 流不应立即结束");
        println!(
            "[③] Status 流 → memory={} goroutines={} connectionsOut={} upTotal={} downTotal={}",
            s0.memory, s0.goroutines, s0.connections_out, s0.uplink_total, s0.downlink_total
        );
        assert!(
            s0.memory > 0 && s0.goroutines > 0,
            "[③] Status 必须是真实运行数据"
        );

        // ── ⑤ apply_pending 真实状态（运行中 + 非在飞 → applied）──────────────────
        let ap = rt.apply_pending().await;
        println!("[⑤] apply_pending（运行中）→ {ap}");
        assert_eq!(ap, "applied");

        // ── ④ 停核干净、无孤儿 ──────────────────────────────────────────────────
        let pid = st.pid;
        rt.stop().await.expect("停核应成功");
        assert!(!rt.status().running, "[④] stop 后 running 必须为 false");
        // 给 OS 一点收尾时间后用 ps 实证。
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !ps_alive(pid),
            "[④] stop 后 pid={pid} 必须不存在（无孤儿进程）"
        );
        println!("[④] stop → ps -p {pid} 已消失（无孤儿）✓");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⑥ lifecycle race：起核在飞时快速 stop → 起核腿必须让位，且**不留孤儿**。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "真机验证：需 POLARIS_SINGBOX_PATH 指向真实 sing-box；非 CI 门"]
    async fn real_core_lifecycle_race_start_then_immediate_stop() {
        require_core();
        let (rt, dir) = test_runtime();
        // 孤儿基线：快速起停 3 轮后系统内 sing-box 进程数不得增长。
        let baseline = singbox_proc_count();

        for round in 1..=3 {
            let mixed = free_port();
            let rt2 = rt.clone();
            let starter = tokio::spawn(async move { rt2.start(local_only_config(mixed)).await });
            // 起核在飞（就绪门轮询中）时立刻 stop → bump 世代 → 起核腿 Superseded 让位。
            tokio::time::sleep(Duration::from_millis(60)).await;
            rt.stop().await.expect("stop 应成功");
            let started = starter.await.expect("start task 不应 panic");
            println!(
                "[⑥] round{round}: start 返回 {:?}，stop 已接管",
                started.map(|s| s.running)
            );

            assert!(
                !rt.status().running,
                "[⑥] round{round}: stop 后不得 running"
            );
            assert!(
                rt.child.lock().unwrap().is_none(),
                "[⑥] round{round}: stop 后不得残留 child 句柄"
            );
        }
        // 全轮结束后系统里不应有本测试起的 sing-box 残留（起停竞态最易漏杀之处）。
        tokio::time::sleep(Duration::from_millis(500)).await;
        let after = singbox_proc_count();
        assert_eq!(
            after, baseline,
            "[⑥] 3 轮快速起停后 sing-box 进程数应回到基线 {baseline}，实得 {after} —— 起停竞态漏了孤儿"
        );
        println!(
            "[⑥] 3 轮快速起停完成：child 句柄已清 + sing-box 进程数 {baseline}→{after}（无孤儿）✓"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 等 pid 变化（去抖重启 ~1.5s + 起核就绪）。返回新 pid（超时返 None）。
    async fn wait_pid_change(rt: &Arc<ProxyRuntime>, old_pid: u32, secs: u64) -> Option<u32> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        while tokio::time::Instant::now() < deadline {
            let s = rt.status();
            if s.running && s.pid != 0 && s.pid != old_pid {
                return Some(s.pid);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        None
    }

    /// **本任务的核心 gate**：切节点 → 核进程 PID 不变（= 热切换真的生效，不是重启）。
    ///
    /// 全程走**生产入口** `ProxyRuntime::switch_mode`（不是直接调 `decide`/`SwitchExecutor`）——
    /// §K7.1：门必须开在唯一的生产路径上，两扇门之间的缝正是 bug 的藏身处。
    ///
    /// 安全硬约束：`proxyModeType: manual` + 仅 127.0.0.1 混合入站 + 节点地址指向本地死端口
    ///   → 不接管系统网络、无 TUN、无系统代理、零外部流量。**绝不可改成 tun/systemProxy**。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "真机验证：需 POLARIS_SINGBOX_PATH 指向真实 sing-box；非 CI 门"]
    async fn real_core_hot_switch_keeps_pid() {
        require_core();
        let (rt, dir) = test_runtime();
        crate::logging::init(&dir);
        let mixed = free_port();
        // 两个节点各指到我们自己的本地监听器（127.0.0.1，随机空闲口）→ ③b 直接观测核拨了谁。
        let (pa, hits_a) = counting_listener().await;
        let (pb, hits_b) = counting_listener().await;
        println!("[①] 节点监听器：Node A=127.0.0.1:{pa}, Node B=127.0.0.1:{pb}");

        // ── ① 起核（两节点，选中 Node A）────────────────────────────────────────
        // 生产时序：save_full 落盘 → broadcast_config_changed → switch_mode。测试逐条照做，
        // 否则去抖重启回落读磁盘时会拿到默认配置（restart 腿的载荷来自 config.current()）。
        let cfg_a = two_node_config_ports(mixed, "node-a", pa, pb);
        rt.config.save_full(&cfg_a).expect("落盘应成功");
        let st = rt.start(cfg_a.clone()).await.expect("起核应成功");
        let pid1 = st.pid;
        println!(
            "[①] start → pid={pid1} mixedPort={} apiPort={}",
            st.mixed_port, st.clash_api_port
        );
        assert!(ps_alive(pid1), "[①] ps 必须能看到 pid={pid1}");
        assert!(
            rt.switch_snapshot.read().unwrap().is_some(),
            "[①] 起核就绪后必须留下热切换基准快照（否则一切变更退化为重启）"
        );
        let snap_tags = rt
            .switch_snapshot
            .read()
            .unwrap()
            .clone()
            .unwrap()
            .id_to_tag;
        println!("[①] 热切换基准 id→tag = {snap_tags:?}");

        // ── ② 切节点 → 热切换，PID 不变（**核心判据**）───────────────────────────
        let cfg_b = two_node_config_ports(mixed, "node-b", pa, pb);
        rt.config.save_full(&cfg_b).expect("落盘应成功");
        let out = rt.switch_mode(cfg_b.clone()).await;
        println!("[②] switch_mode（node-a → node-b）→ {out:?}");
        assert_eq!(
            out,
            SwitchOutcome::HotSwitched,
            "[②] 切节点必须走热切腿（实得 {out:?}）—— 这正是本任务要接的线"
        );
        let pid_after = rt.status().pid;
        assert_eq!(
            pid_after, pid1,
            "[②] 热切换后 PID 必须不变（{pid1} → {pid_after}）—— 变了就说明还是在重启整个核"
        );
        assert!(
            ps_alive(pid1),
            "[②] 原进程 pid={pid1} 必须仍在跑（ps 实证）"
        );
        println!("[②] ps -p {pid1} 仍存活 + PID 未变 → 热切换真的生效 ✓");

        // ── ③ SelectOutbound 真的经 gRPC 下发且被核接受 ──────────────────────────
        // 负向对照：核必须拒绝不存在的成员 tag。它会拒 ⇒ ② 里那次成功的 PUT 确实选中了真实成员，
        // 而不是「核照单全收、根本没校验」。没有这条，「PUT 返回 Ok」只能证明 RPC 通了。
        let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", st.clash_api_port), "")
            .await
            .expect("[③] 管理 API gRPC 连接应成功");
        let bogus = client
            .select_outbound("proxy-selector", "no-such-member-xyz")
            .await;
        println!("[③] 负向对照 SelectOutbound(proxy-selector, no-such-member-xyz) → {bogus:?}");
        assert!(
            bogus.is_err(),
            "[③] 核必须拒绝不存在的成员 tag —— 它若照单全收，② 的 PUT 成功就不能证明真的切了"
        );
        let good = client.select_outbound("proxy-selector", "Node B").await;
        println!("[③] 正向 SelectOutbound(proxy-selector, Node B) → {good:?}");
        assert!(good.is_ok(), "[③] 真实成员 tag 的 PUT 必须成功");

        // ── ③b 决定性实证：热切换后**真实流量**改走 Node B（不是只让 RPC 返了个 Ok）──────
        // 前面几条只证明「PUT 被核接受」。要证明**路由真的变了**，就得看核实际拨了谁：
        // 两个节点各指向我们自己的本地监听器，谁被连上一目了然。
        // 不读日志——日志经全局 OnceLock logger 中转，多测试同进程时会绑到别人的目录（实测踩到）。
        client
            .select_outbound("proxy-selector", "Node B")
            .await
            .expect("[③b] 复位到 Node B（③ 的负向对照后需还原）");
        hits_a.store(0, Ordering::SeqCst);
        hits_b.store(0, Ordering::SeqCst);
        drive_traffic_through_proxy(mixed).await;
        let (a, b) = (hits_a.load(Ordering::SeqCst), hits_b.load(Ordering::SeqCst));
        println!("[③b] 热切换后打流量：Node A 监听器被连 {a} 次，Node B 监听器被连 {b} 次");
        assert!(
            b > 0,
            "[③b] 热切换后流量必须由 Node B 承载（B 被连 {b} 次）—— \
             PUT 返回了 Ok 但路由没切 = 假阳性，正是「兜底把失败伪装成成功」的形态"
        );
        assert_eq!(
            a, 0,
            "[③b] 切换后绝不该再有新连接落到 Node A（实得 {a} 次）"
        );
        println!("[③b] 核未重启（pid={pid1}）且流量已改走 Node B → 热切换真的改变了实际路由 ✓");

        // ── ④ norm 内字段（端口）变更 → 走重启，PID 必须变 ──────────────────────
        let mixed2 = free_port();
        let cfg_port = two_node_config_ports(mixed2, "node-b", pa, pb);
        rt.config.save_full(&cfg_port).expect("落盘应成功");
        let out = rt.switch_mode(cfg_port).await;
        println!("[④] switch_mode（改 mixedPort {mixed} → {mixed2}）→ {out:?}");
        assert_eq!(
            out,
            SwitchOutcome::Restarting,
            "[④] norm 内字段变更必须走重启腿（热切换切不了端口）"
        );
        let pid2 = wait_pid_change(&rt, pid1, 20)
            .await
            .expect("[④] 去抖重启应在 20s 内换出新 pid");
        println!("[④] 重启完成：pid {pid1} → {pid2}");
        assert!(ps_alive(pid2), "[④] 新核必须在跑");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!ps_alive(pid1), "[④] 旧核 pid={pid1} 必须已退出（无孤儿）");
        assert_eq!(
            rt.status().mixed_port,
            mixed2,
            "[④] 重启后必须真的起在新端口上（否则重启是空转）"
        );
        println!("[④] 旧核已退、新核监听 {mixed2} → 重启路径完好 ✓");

        rt.stop().await.expect("停核应成功");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⑤ 热切换失败 → 回退重启，且**不卡死**。
    ///
    /// 真实失败注入：直接把核杀掉，此时 switch_mode 会判热切腿 → gRPC 连不上 → ClientNotReady →
    /// 按 executor 契约退回去抖重启 → 新核起来。这同时实证了「热切换失败不会把变更吞掉、也不会挂起」。
    ///
    /// **与崩溃自愈的关系（本批接线后）**：SIGKILL 后崩溃监测也会检出并自愈，但 (a) 监测轮询间隔 1s，
    /// 本测试在 500ms 处断言 `running` 时尚未检出；(b) 热切换失败触发的回退重启会 bump 世代，崩溃监测
    /// 据此 `post_backoff` 判 Superseded 让位 → **二者经世代协同，只发生一次重启**，不打架。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "真机验证：需 POLARIS_SINGBOX_PATH 指向真实 sing-box；非 CI 门"]
    async fn real_core_hot_switch_failure_falls_back_to_restart() {
        require_core();
        let (rt, dir) = test_runtime();
        crate::logging::init(&dir);
        let mixed = free_port();

        let cfg_a = two_node_config(mixed, "node-a");
        rt.config.save_full(&cfg_a).expect("落盘应成功");
        let st = rt.start(cfg_a).await.expect("起核应成功");
        let pid1 = st.pid;
        println!("[⑤] 起核 pid={pid1}");

        // 失败注入：SIGKILL 核（绕过 rt.stop，世代不变 → 快照仍在；崩溃监测 1s 后才检出，此刻 status 仍 running）。
        send_signal(pid1, Signal::Sigkill);
        tokio::time::sleep(Duration::from_millis(500)).await;
        // 判据用「管理 API 已不可连」而**不用 `ps_alive`**：SIGKILL 后 child 句柄仍被 ProxyRuntime
        // 持有、无人 `wait()` 收割 → 进程处于 **zombie** 态，`ps -p` 照样看得见它（`ps_alive` 分辨
        // 不了 zombie 与存活）。而「管理 API 连不上」正是本测试要注入的失败条件本身。
        let api_dead = tokio::net::TcpStream::connect(("127.0.0.1", st.clash_api_port))
            .await
            .is_err();
        assert!(api_dead, "[⑤] 核已被 SIGKILL → 管理 API 端口应不可连");
        assert!(
            rt.status().running,
            "[⑤] 前提：崩溃监测 1s 后才检出，此刻（500ms）status 仍自称 running（这正是热切换会失败的场景）"
        );
        println!("[⑤] 已 SIGKILL 核 pid={pid1}：管理 API 不可连，status 仍自称 running");

        // 切节点 → 热切腿 → gRPC 连不上 → 回退重启。带超时断言「不卡死」。
        let cfg_b = two_node_config(mixed, "node-b");
        rt.config.save_full(&cfg_b).expect("落盘应成功");
        let out = tokio::time::timeout(Duration::from_secs(15), rt.switch_mode(cfg_b))
            .await
            .expect("[⑤] switch_mode 必须在 15s 内返回 —— 卡死即失败");
        println!("[⑤] 核已死时 switch_mode → {out:?}");
        assert_eq!(
            out,
            SwitchOutcome::Restarting,
            "[⑤] 热切换失败必须退回重启，绝不能静默吞掉切节点"
        );

        // 回退的重启真的把核拉起来了（不是只喊了一声）。
        let pid2 = wait_pid_change(&rt, pid1, 20)
            .await
            .expect("[⑤] 回退重启应在 20s 内起出新核");
        println!(
            "[⑤] 回退重启完成：pid {pid1} → {pid2}（新核存活={}）",
            ps_alive(pid2)
        );
        assert!(ps_alive(pid2), "[⑤] 回退重启必须真的起出新核");

        rt.stop().await.expect("停核应成功");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // 崩溃自愈 + stale-core 清扫 真机验证（**非 CI 门**，本任务核心 gate）
    //
    // 安全硬约束：全程 manual + 全局直连 + 仅 127.0.0.1 监听 → 不接管系统网络、无 TUN、无系统代理。
    // 只杀「自己起的核」：崩溃自愈重启自管句柄；stale 清扫按本 app 二进制路径精确判定。
    // ══════════════════════════════════════════════════════════════════════════════

    /// 最小合法 sing-box config（裸核直起用）：仅 127.0.0.1 混合入站 + direct 出站，绝不触碰宿主网络。
    fn write_bare_singbox_config(path: &std::path::Path, mixed: u16) {
        let cfg = serde_json::json!({
            "log": { "disabled": true },
            "inbounds": [{ "type": "mixed", "listen": "127.0.0.1", "listen_port": mixed }],
            "outbounds": [{ "type": "direct" }]
        });
        std::fs::write(path, serde_json::to_string_pretty(&cfg).unwrap()).expect("写裸核 config");
    }

    /// ⑦ 崩溃自愈：`kill -9` 掉核（模拟崩溃）→ 世代未变 → 崩溃监测检出 → 退避后自愈重启（ps 实证新 pid）。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "真机验证：需 POLARIS_SINGBOX_PATH 指向真实 sing-box；非 CI 门"]
    async fn real_core_crash_triggers_auto_restart() {
        require_core();
        let (rt, dir) = test_runtime();
        crate::logging::init(&dir);
        let mixed = free_port();
        // 直接传 config 给 start（不经 save_full：其 validate 要求 tunConfig，而 manual+direct 用不到；
        // start 的 from_value::<UserConfig> 里 tun_config 是 Option → 缺省即可。崩溃自愈重启读 current_config
        // （start 就绪时已置），无需磁盘配置）。
        let st = rt
            .start(local_only_config(mixed))
            .await
            .expect("起核应成功");
        let pid1 = st.pid;
        assert!(ps_alive(pid1), "[⑦] 起核后 pid={pid1} 应在跑");
        println!("[⑦] 起核 pid={pid1}");

        // 模拟崩溃：SIGKILL（绕过 rt.stop → 世代不变 → 崩溃监测判为**意外**退出）。
        send_signal(pid1, Signal::Sigkill);
        println!("[⑦] 已 SIGKILL pid={pid1}（模拟崩溃），等待自愈重启...");

        // 检出（≤1s）+ 退避（第 1 次 2s）+ 起核就绪 → 20s 内必换出新 pid。
        let pid2 = wait_pid_change(&rt, pid1, 20)
            .await
            .expect("[⑦] 崩溃后必须自愈重启并换出新 pid（自愈未生效）");
        assert_ne!(pid2, pid1, "[⑦] 自愈后必须是新进程");
        assert!(ps_alive(pid2), "[⑦] 自愈重启的新核必须在跑（ps 实证）");
        assert!(rt.status().running, "[⑦] 自愈后 status 必须 running");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !ps_alive(pid1),
            "[⑦] 崩溃的旧核 pid={pid1} 必须已被收割（无僵尸/孤儿）"
        );
        println!("[⑦] 崩溃自愈生效：pid {pid1} → {pid2}，旧核已收割 ✓");

        rt.stop().await.expect("停核应成功");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⑦b 真机·核崩轴：真核崩溃 → 自愈重启 → `diagnostic_counters().restart_count` 非零 →
    /// 诊断报告渲染「核崩溃自动重启」行（§O1 组合面在真核上再实证一次）。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "真机验证：需 POLARIS_SINGBOX_PATH 指向真实 sing-box；非 CI 门"]
    async fn real_core_crash_feeds_diagnostic_restart_axis() {
        require_core();
        let (rt, dir) = test_runtime();
        crate::logging::init(&dir);
        let mixed = free_port();

        let st = rt
            .start(local_only_config(mixed))
            .await
            .expect("起核应成功");
        let pid1 = st.pid;
        // 崩溃前核崩轴应为 0（尚无崩溃）。
        assert_eq!(
            rt.diagnostic_counters().restart_count,
            0,
            "[⑦b] 起核后未崩溃 → 核崩轴应为 0"
        );

        send_signal(pid1, Signal::Sigkill); // 模拟崩溃（世代不变 → 判为意外退出）
        let pid2 = wait_pid_change(&rt, pid1, 20)
            .await
            .expect("[⑦b] 崩溃后必须自愈重启");
        assert_ne!(pid2, pid1);

        // 自愈重启后：核崩轴（从 CrashRecoveryMachine 投影）必非零。
        let counters = rt.diagnostic_counters();
        assert!(
            counters.restart_count >= 1,
            "[⑦b] 真核崩溃自愈后核崩轴必非零；实得 {}",
            counters.restart_count
        );

        // 组合面：喂进真实报告构建器，断言「核崩溃自动重启」行真渲染。
        let md = report_with_counters(counters);
        assert!(
            md.contains("核崩溃自动重启"),
            "[⑦b] 诊断报告必须渲染核崩轴行（真核崩溃 → 非零）"
        );
        println!(
            "[⑦b] 真核崩溃自愈 → restart_count={} → 报告核崩轴行已渲染 ✓",
            counters.restart_count
        );

        rt.stop().await.expect("停核应成功");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⑧ 主动 stop → **不**触发自愈（ps 实证无重启）。这是崩溃自愈最易出的 bug：把主动杀核当崩溃。
    ///
    /// 变异对照：若把「主动 stop」也当崩溃（去掉世代判据），则 stop 后 status 会被自愈拉回 running。
    /// 单测 `classify_child_exit` 的 `intentional_stop_bumped_generation_is_retire` 已在 CI 层锁死此判据，
    /// 本条在真机层再实证一次。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "真机验证：需 POLARIS_SINGBOX_PATH 指向真实 sing-box；非 CI 门"]
    async fn real_core_intentional_stop_does_not_restart() {
        require_core();
        let (rt, dir) = test_runtime();
        crate::logging::init(&dir);
        let mixed = free_port();
        // 直接传 config 给 start（不经 save_full：其 validate 要求 tunConfig，而 manual+direct 用不到；
        // start 的 from_value::<UserConfig> 里 tun_config 是 Option → 缺省即可。崩溃自愈重启读 current_config
        // （start 就绪时已置），无需磁盘配置）。
        let st = rt
            .start(local_only_config(mixed))
            .await
            .expect("起核应成功");
        let pid1 = st.pid;
        assert!(ps_alive(pid1), "[⑧] 起核后 pid={pid1} 应在跑");
        println!("[⑧] 起核 pid={pid1}");

        // 主动 stop：入口先 bump 世代再杀核 → 崩溃监测应 Retire（不触发自愈）。
        rt.stop().await.expect("停核应成功");
        assert!(!rt.status().running, "[⑧] stop 后 running 必须为 false");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!ps_alive(pid1), "[⑧] stop 后旧核 pid={pid1} 必须退出");

        // 关键：等足够久（超过 poll 1s + 退避 2s + 余量）确认**绝无**自愈重启。
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            !rt.status().running,
            "[⑧] 主动 stop 绝不能触发崩溃自愈（status 必须仍未运行）—— 世代判据失效即此处转红"
        );
        assert_eq!(rt.status().pid, 0, "[⑧] 主动 stop 后不得有任何新核 pid");
        println!("[⑧] 主动 stop 后 5s 无任何重启（status 未运行）→ 世代判据正确 ✓");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⑨ 超阈值崩溃 → 放弃自愈并报错，**绝不无限重启**。
    ///
    /// `MAX_RESTART_COUNT=3` / 60s 冷却：连续崩溃 3 次自愈成功，第 4 次崩溃 → `GiveUp` → 置 error、不再重启。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "真机验证：需 POLARIS_SINGBOX_PATH 指向真实 sing-box；非 CI 门（含 2+5+15s 退避，耗时较长）"]
    async fn real_core_crash_loop_gives_up_without_infinite_restart() {
        require_core();
        let (rt, dir) = test_runtime();
        crate::logging::init(&dir);
        let mixed = free_port();
        // 直接传 config 给 start（不经 save_full：其 validate 要求 tunConfig，而 manual+direct 用不到；
        // start 的 from_value::<UserConfig> 里 tun_config 是 Option → 缺省即可。崩溃自愈重启读 current_config
        // （start 就绪时已置），无需磁盘配置）。
        let st = rt
            .start(local_only_config(mixed))
            .await
            .expect("起核应成功");
        let mut pid = st.pid;
        println!("[⑨] 起核 pid={pid}");

        // 崩溃 3 次，每次都应自愈（退避 2s/5s/15s）。
        for i in 1..=3 {
            send_signal(pid, Signal::Sigkill);
            let next = wait_pid_change(&rt, pid, 30)
                .await
                .unwrap_or_else(|| panic!("[⑨] 第 {i} 次崩溃应自愈换出新 pid"));
            println!("[⑨] 第 {i} 次崩溃自愈：pid {pid} → {next}");
            pid = next;
            // 让新核稳定一小会儿（但远小于 60s 冷却，确保计数不复位）。
            tokio::time::sleep(Duration::from_millis(300)).await;
        }

        // 第 4 次崩溃 → 达上限 → 放弃自愈（不再换 pid）。
        send_signal(pid, Signal::Sigkill);
        println!("[⑨] 第 4 次 SIGKILL pid={pid}，应放弃自愈（不无限重启）...");
        let extra = wait_pid_change(&rt, pid, 8).await;
        assert!(
            extra.is_none(),
            "[⑨] 第 4 次崩溃必须放弃自愈，绝不无限重启（实得新 pid {extra:?}）"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!ps_alive(pid), "[⑨] 第 4 次崩溃的核已死、无自愈");
        assert!(
            rt.status().error.is_some(),
            "[⑨] 放弃自愈必须置 error 供 UI 上报（实得 {:?}）",
            rt.status()
        );
        println!(
            "[⑨] 第 4 次崩溃已放弃自愈，error={:?}，未无限重启 ✓",
            rt.status().error
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⑩ stale-core 清扫：**本 app** 孤儿被清 + **非本 app** 的 sing-box **不被误杀**（最关键的安全点）。
    ///
    /// - 「本 app 孤儿」= 用 `POLARIS_SINGBOX_PATH` 指向的核二进制直接 spawn（不经 ProxyRuntime → 无句柄管理）。
    /// - 「非本 app」= 把同一核**复制到另一路径**再起 → argv[0] 路径不同 → `is_our_core` 判 false → 存活。
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "真机验证：需 POLARIS_SINGBOX_PATH 指向真实 sing-box；非 CI 门"]
    async fn real_core_stale_cleanup_kills_own_orphan_spares_foreign() {
        use std::process::Stdio;
        let core = require_core();
        let (rt, dir) = test_runtime();
        crate::logging::init(&dir);

        // ── 孤儿①（本 app）：用本 app 核路径直接 spawn，不经 ProxyRuntime → 成孤儿 ──
        let ours_cfg = dir.join("orphan-ours.json");
        write_bare_singbox_config(&ours_cfg, free_port());
        let mut ours_orphan = tokio::process::Command::new(&core)
            .args(["run", "-c", ours_cfg.to_str().unwrap(), "--disable-color"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn 本 app 孤儿核");
        let ours_pid = ours_orphan.id().expect("本 app 孤儿 pid");

        // ── 「非本 app」sing-box：复制核到异路径再起 → 路径不同 → 绝不该被误杀 ──
        let foreign_bin = dir.join("foreign-sing-box");
        std::fs::copy(&core, &foreign_bin).expect("复制核到异路径（std::fs::copy 保留可执行位）");
        let foreign_cfg = dir.join("foreign.json");
        write_bare_singbox_config(&foreign_cfg, free_port());
        let mut foreign = tokio::process::Command::new(&foreign_bin)
            .args([
                "run",
                "-c",
                foreign_cfg.to_str().unwrap(),
                "--disable-color",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn 非本 app sing-box（异路径）");
        let foreign_pid = foreign.id().expect("非本 app sing-box pid");

        // 等两个核都真正起来。
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(ps_alive(ours_pid), "[⑩] 前提：本 app 孤儿在跑");
        assert!(ps_alive(foreign_pid), "[⑩] 前提：非本 app sing-box 在跑");
        println!("[⑩] 本 app 孤儿 pid={ours_pid}（{core}）");
        println!(
            "[⑩] 非本 app sing-box pid={foreign_pid}（{}）",
            foreign_bin.display()
        );

        // ── stale 清扫：按本 app 二进制路径精确判定 ──
        // 同用户起的孤儿用户态就杀得动 → 不该走到 T3 提权腿，必须干净返回 Ok。
        assert!(
            rt.cleanup_stale_cores().await.is_ok(),
            "[⑩] 同用户孤儿用户态可杀 → 不得落 ROOT_ORPHAN_BLOCKED"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 本 app 孤儿已被 SIGKILL，但本测试持有其 Child 句柄 → 它是**未收割的 zombie**，`ps -p` 仍看得见
        // （zombie ≠ alive，与 ⑤ 同一陷阱）。故用 `wait()` 收割并确认它**确已退出**：收割立即返回 = 已被杀死。
        let reaped = tokio::time::timeout(Duration::from_secs(3), ours_orphan.wait()).await;
        assert!(
            reaped.is_ok(),
            "[⑩] 本 app 孤儿核 pid={ours_pid} 必须被清扫（wait 应立即返回退出状态；超时=仍在跑=没杀掉）"
        );
        // 非本 app sing-box（异路径）genuinely 存活（未被杀、非 zombie）→ ps_alive 判据可靠。
        assert!(
            ps_alive(foreign_pid),
            "[⑩] **核心安全点**：非本 app 的 sing-box pid={foreign_pid}（异路径）绝不能被误杀"
        );
        println!("[⑩] 本 app 孤儿已清（wait 收割确认退出）+ 非本 app sing-box 存活 → 只杀自己、不误杀他人 ✓");

        // 收尾：清掉 foreign（本 app 孤儿已收割）。
        send_signal(foreign_pid, Signal::Sigkill);
        let _ = foreign.wait().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn server_ids_extracts_ids_and_tolerates_garbage() {
        let cfg = serde_json::json!({
            "servers": [{"id": "a"}, {"id": "b"}, {"noid": 1}, "junk"]
        });
        let ids = server_ids(&cfg);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("a") && ids.contains("b"));
        // 无 servers 键 → 空集，不 panic。
        assert!(server_ids(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn proxy_status_serializes_camel_case_contract() {
        // 前端契约：running / pid / startTime / uptime / error / errorCode / mixedPort / clashApiPort / startedViaHelper。
        let s = ProxyStatus {
            running: true,
            pid: 42,
            start_time: Some(1_700_000_000_000),
            uptime: Some(90),
            mixed_port: 7890,
            clash_api_port: 19090,
            error: Some("boom".to_string()),
            error_code: Some(code::STARTUP_FAILED.to_string()),
            started_via_helper: false,
            update_in_port: 45678,
            starting: false,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["running"], true);
        assert_eq!(v["pid"], 42);
        assert_eq!(v["mixedPort"], 7890);
        assert_eq!(v["clashApiPort"], 19090);
        assert_eq!(v["updateInPort"], 45678);
        // 打断 startTime/errorCode 的 serde rename（写成 snake_case）→ 本测转红：
        // 前端 ProxyStatus 按 camelCase 读，名字错 = 字段又变成恒 undefined（正是本次修的 bug 形态）。
        assert_eq!(v["startTime"], 1_700_000_000_000_u64);
        assert_eq!(v["uptime"], 90);
        assert_eq!(v["error"], "boom");
        assert_eq!(v["errorCode"], "STARTUP_FAILED");

        // pid=0 / 未运行时省略（对齐 上游 `pid?` / `startTime?` / `uptime?` / `errorCode?`）。
        let z = ProxyStatus::default();
        let zv = serde_json::to_value(&z).unwrap();
        assert!(zv.get("pid").is_none());
        assert!(zv.get("startTime").is_none());
        assert!(zv.get("uptime").is_none());
        assert!(zv.get("errorCode").is_none());
        // starting 同样是「false 即省略」的可选字段（渲染端 `starting?: boolean`）。
        assert!(zv.get("starting").is_none());
        let sv = serde_json::to_value(ProxyStatus {
            starting: true,
            ..ProxyStatus::default()
        })
        .unwrap();
        assert_eq!(
            sv["starting"], true,
            "起核在飞必须出现在快照里 —— 托盘据它把「连接」换成「取消」，缺了就会叠第二次 start"
        );
    }

    /// **C12 只读 smoke**：`enumerate_own_lan_cidrs` 真枚举本机接口（unix `getifaddrs` / Windows
    /// `GetAdaptersAddresses`，二者皆只读、非破坏性）—— 断言**格式**不变式（每项合法 CIDR、含 `/`、
    /// 非回环、去重），**不**断言具体网段（随宿主网络变，会 flaky）。允许空集（容器/无接口环境）。
    /// 打断枚举（如漏滤回环 / 不 dedupe / Windows 侧漏了 `prefix_is_valid` 让哨兵 255 混进来）→ 本测转红。
    ///
    /// **cfg 从 `unix` 放宽到 `any(unix, windows)`**：Windows 腿此前是恒空 stub、无可测；现在它走真枚举，
    /// 同一组格式不变式必须同样守住（本机跑不到，但 Windows CI/真机跑到的就是这条）。
    #[cfg(any(unix, windows))]
    #[test]
    fn enumerate_own_lan_cidrs_yields_valid_non_loopback_cidrs() {
        use std::collections::HashSet;
        let cidrs = enumerate_own_lan_cidrs();
        let mut seen = HashSet::new();
        for c in &cidrs {
            // 形态：`addr/prefix`（含主机位）。
            let (addr, prefix) = c
                .rsplit_once('/')
                .unwrap_or_else(|| panic!("每项须为 CIDR 形态（含 /），实得: {c}"));
            assert!(!addr.is_empty(), "地址段非空: {c}");
            // prefix 是合法数字（v4 ≤32 / v6 ≤128）。
            let p: u32 = prefix
                .parse()
                .unwrap_or_else(|_| panic!("前缀须为数字: {c}"));
            let max = if addr.contains(':') { 128 } else { 32 };
            assert!(p <= max, "前缀越界: {c}");
            // 非回环（滤回环生效）。
            assert_ne!(addr, "127.0.0.1", "回环须被剔除: {c}");
            assert_ne!(addr, "::1", "回环须被剔除: {c}");
            assert!(!addr.starts_with("127."), "127/8 回环段须被剔除: {c}");
            // 去重生效（dedupe_own_lan）。
            assert!(seen.insert(c.clone()), "枚举结果须去重，实得重复项: {c}");
        }
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // startTime / uptime：运行时长真值 + 读时投影
    // ══════════════════════════════════════════════════════════════════════════════

    /// `status()` 必须**现算** uptime，而非回存储值（存储恒 None）。
    /// 打断 `status()` 里的投影（改成直接 clone）→ 本测转红：那就是 Home「运行时长」恒空的老 bug。
    #[test]
    fn status_projects_uptime_from_start_time_on_read() {
        let (rt, dir) = test_runtime();
        // 起点设在 90s 前，模拟已跑一阵的核。
        *rt.status.write().unwrap() = ProxyStatus {
            running: true,
            start_time: Some(now_ms() - 90_000),
            ..ProxyStatus::default()
        };
        // 存储态的 uptime 恒 None —— 投影只发生在读侧。
        assert!(rt.status.read().unwrap().uptime.is_none());
        let uptime = rt
            .status()
            .uptime
            .expect("running 时 status() 必须投影出 uptime");
        assert!(
            (89..=92).contains(&uptime),
            "uptime 应约等于 90s（现算），实得 {uptime}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未运行（无 start_time）→ uptime 为 None，**不是 0**：
    /// 0 会被前端 `fmtUptime` 渲染成「已运行 0 秒」= 谎称在跑。打断（改成 unwrap_or(0)）→ 本测转红。
    #[test]
    fn status_has_no_uptime_when_not_running() {
        let (rt, dir) = test_runtime();
        assert!(rt.status().start_time.is_none());
        assert!(rt.status().uptime.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `set_error` 清空 start_time（错误终态 = 没在跑）→ uptime 随之消失。
    /// 打断（set_error 保留 start_time）→ 本测转红：Home 会在核已崩时继续走字。
    #[test]
    fn set_error_clears_start_time_and_uptime() {
        let (rt, dir) = test_runtime();
        *rt.status.write().unwrap() = ProxyStatus {
            running: true,
            start_time: Some(now_ms() - 5_000),
            ..ProxyStatus::default()
        };
        rt.set_error("核崩了", code::PROCESS_EXITED);
        let s = rt.status();
        assert!(!s.running);
        assert!(s.start_time.is_none());
        assert!(s.uptime.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // event:proxyError 发射（此前通道两端全死：定义了、全仓零 emit）
    // ══════════════════════════════════════════════════════════════════════════════

    /// 发射记录：`(message, errorCode)` 逐条。
    type ErrorEvents = Arc<Mutex<Vec<(String, String)>>>;

    /// 非法节点发射记录：每次 emit 一帧（`Vec<InvalidNode>`）。**逐帧存而非扁平化**——
    /// 「发了空数组」与「压根没发」是两个不同事实（前者清标灰，后者是 bug），扁平化会把二者抹平成同一个空。
    type InvalidNodeFrames = Arc<Mutex<Vec<Vec<InvalidNode>>>>;

    /// 系统代理残留发射记录：每次 emit 一条 proxy 串。
    type ResidualEvents = Arc<Mutex<Vec<String>>>;

    /// TS 状态发射记录：每次 emit 一条 `TailscaleStatusEvent`（逐 endpoint）。
    type TsStatusEvents = Arc<Mutex<Vec<TailscaleStatusEvent>>>;

    /// A4 让位态变发射记录：每次 emit 一条 `(engaged, serverName?)`。
    type MeshLoginFallbackEvents = Arc<Mutex<Vec<(bool, Option<String>)>>>;

    /// C3 自动换节点发射记录：每次 emit 一条 payload。
    type AutoNodeSwitchedEvents = Arc<Mutex<Vec<AutoNodeSwitchedPayload>>>;

    /// unlock 缓存失效发射记录：每次 invalidate 一条 `(running, exitBlocked)`。
    type UnlockInvalidations = Arc<Mutex<Vec<(bool, bool)>>>;

    /// R2 待应用差集 PUSH 发射记录：每次 emit 一条 `PendingChangesSummary`。
    type PendingChangesEvents = Arc<Mutex<Vec<PendingChangesSummary>>>;

    /// runtime 生命周期结局发射记录：每次 emit 一条 `ProxyLifecycleEvent`。
    /// **逐帧存**（同 `InvalidNodeFrames` 的理由）：「发了 failed」与「压根没发」是两个不同事实。
    type LifecycleEvents = Arc<Mutex<Vec<ProxyLifecycleEvent>>>;

    /// 出口 IP 重探排程记录：每次排程一条 `running`（起核/热切=true，停核=false）。
    type ExitIpRefreshes = Arc<Mutex<Vec<bool>>>;

    /// R2 出口无效终态记录：每次 `mark_exit_blocked` 一条 `ProxyExitBlock` 原因串。
    type ExitBlockedMarks = Arc<Mutex<Vec<String>>>;

    /// 发射记录 mock（不碰 Tauri：`AppHandle` 本机无从构造，且发事件不该是测不了的死角）。
    #[derive(Default)]
    struct RecordingErrorEmitter {
        events: ErrorEvents,
        invalid_frames: InvalidNodeFrames,
        residual: ResidualEvents,
        ts_status: TsStatusEvents,
        mesh_login_fallback: MeshLoginFallbackEvents,
        auto_node_switched: AutoNodeSwitchedEvents,
        unlock_invalidations: UnlockInvalidations,
        exit_ip_refreshes: ExitIpRefreshes,
        exit_blocked_marks: ExitBlockedMarks,
        pending_changes: PendingChangesEvents,
        lifecycle: LifecycleEvents,
        /// 预置的隐私模式活态（生产侧读 `commands::config` 的进程状态机；mock 直接回放）。
        privacy_mode: bool,
        /// 门被调用的次数（`0` = 这条入口**根本没经过门** → 变异「某入口绕过门」立刻转红）。
        helper_gate_calls: Arc<std::sync::atomic::AtomicUsize>,
        /// 预置的用户决策。`Default` 为 `Abort`（见 `prompt_helper_gate` 注释）。
        helper_gate_decision: HelperGateDecision,
        /// 每次 `emit_proxy_error` 那一刻观测到的解锁失效次数（= 续延已跑过几轮）。
        ///
        /// 「运行期自证必须排在续延之后」是一条**时序**不变式：只看终态（两件事都发生了）验不出顺序，
        /// 而后台腿里两者相隔可能只有微秒，轮询采样必然 flaky。在告警那一刻给续延拍照才是确定性判据
        /// （同 `TestPutSink::invalidation_probe` 的范式，方向相反）。
        error_seen_invalidations: Arc<Mutex<Vec<usize>>>,
    }
    impl ProxyErrorEmitter for RecordingErrorEmitter {
        fn emit_proxy_error(&self, message: &str, error_code: &str) {
            let n = self.unlock_invalidations.lock().unwrap().len();
            self.error_seen_invalidations.lock().unwrap().push(n);
            self.events
                .lock()
                .unwrap()
                .push((message.to_string(), error_code.to_string()));
        }
        fn emit_invalid_nodes(&self, nodes: &[InvalidNode]) {
            self.invalid_frames.lock().unwrap().push(nodes.to_vec());
        }
        fn emit_system_proxy_residual(&self, proxy: &str) {
            self.residual.lock().unwrap().push(proxy.to_string());
        }
        fn emit_tailscale_status(&self, event: &TailscaleStatusEvent) {
            self.ts_status.lock().unwrap().push(event.clone());
        }
        fn emit_mesh_login_fallback(&self, engaged: bool, server_name: Option<&str>) {
            self.mesh_login_fallback
                .lock()
                .unwrap()
                .push((engaged, server_name.map(str::to_string)));
        }
        fn emit_auto_node_switched(&self, payload: &AutoNodeSwitchedPayload) {
            self.auto_node_switched
                .lock()
                .unwrap()
                .push(payload.clone());
        }
        fn invalidate_unlock(&self, running: bool, exit_blocked: bool) {
            self.unlock_invalidations
                .lock()
                .unwrap()
                .push((running, exit_blocked));
        }
        fn schedule_exit_ip_refresh(&self, running: bool) {
            self.exit_ip_refreshes.lock().unwrap().push(running);
        }
        fn mark_exit_blocked(&self, reason: &str) {
            self.exit_blocked_marks
                .lock()
                .unwrap()
                .push(reason.to_string());
        }
        fn privacy_mode(&self) -> bool {
            self.privacy_mode
        }
        fn emit_pending_changes(&self, summary: &PendingChangesSummary) {
            self.pending_changes.lock().unwrap().push(summary.clone());
        }
        fn emit_lifecycle(&self, event: &ProxyLifecycleEvent) {
            self.lifecycle.lock().unwrap().push(event.clone());
        }

        /// 记录一次门调用 + 回放预置决策（默认 `Abort`：mock 绝不代替用户点「安装」）。
        /// **不真装 helper**（本机绝不碰系统路径 / 绝不弹提权框）—— 复检腿因此恒判「仍缺」，
        /// 这正好让「确认后没装上」那条腿可测。
        fn prompt_helper_gate(&self, _status: &HelperStatusSnapshot) -> HelperGateDecision {
            self.helper_gate_calls.fetch_add(1, Ordering::SeqCst);
            self.helper_gate_decision
        }
    }

    /// 装 mock emitter 的运行时 + 其发射记录句柄。
    fn test_runtime_recording_errors() -> (Arc<ProxyRuntime>, PathBuf, ErrorEvents) {
        let (rt, dir) = test_runtime();
        let events = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            events: Arc::clone(&events),
            ..Default::default()
        }));
        (rt, dir, events)
    }

    /// 装 mock emitter 并同时返回 `event:proxyError` 与 `event:proxyLifecycle` 两路记录句柄。
    /// **两路一起取**：本批要证的正是「有一类失败只走后者」，只拿一路证不了那句话。
    fn test_runtime_recording_lifecycle(
    ) -> (Arc<ProxyRuntime>, PathBuf, ErrorEvents, LifecycleEvents) {
        let (rt, dir) = test_runtime();
        let events: ErrorEvents = Arc::new(Mutex::new(Vec::new()));
        let lifecycle: LifecycleEvents = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            events: Arc::clone(&events),
            lifecycle: Arc::clone(&lifecycle),
            ..Default::default()
        }));
        (rt, dir, events, lifecycle)
    }

    /// **C 的全部价值所在**：起核失败在 UI 上必须**可辨**，而不是「差集也空了、看着像成功」。
    ///
    /// 取的是 `set_error` 头注明列**不覆盖**的那一类（「config 生成 / 写盘 / spawn 失败」，理由是
    /// 「有 command 在 await」）—— 而去抖重启这条路上**没有任何人在 await**
    /// （`schedule_restart` 的回调只 `log::error!`）。故此前这一类失败对渲染端是**全静默**的：
    /// 既无 `proxyStarted`（本就不发）也无 `proxyError`，条只能停在「应用中…」等 12s 兜底轮询。
    ///
    /// 断言两件事：① 这一类确实**不发** `event:proxyError`（钉住前提，否则本条毫无意义）；
    /// ② 但**必发**一条 `lifecycle{phase:"failed"}` 且带用户可见 message。
    ///
    /// 变异对照：
    /// - 删掉 `start` 包装里那个 `if let Err(e) = &r { push_lifecycle(failed) }` → ② 转红；
    /// - 把它挪进某条具体失败腿（如只在 `set_error` 里发）→ ② 转红（本腿压根不经 `set_error`）；
    /// - 给它加世代守卫并在此让位 → ② 转红。
    #[tokio::test]
    async fn start_failure_is_observable_even_when_it_never_reaches_set_error() {
        let (rt, dir, errors, lifecycle) = test_runtime_recording_lifecycle();
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst); // 跳过孤儿清扫（/proc 扫描），聚焦失败腿。

        let r = rt.start(bad_config()).await;
        assert!(r.is_err(), "前提：坏配置必失败");

        assert!(
            errors.lock().unwrap().is_empty(),
            "前提（钉住 set_error 的「不覆盖腿」清单）：这一类失败不经 set_error ⇒ 不发 proxyError。\
             若这条转红，说明失败分类的边界变了，本测的因果叙述需重写而非放宽"
        );

        let seen = lifecycle.lock().unwrap().clone();
        assert_eq!(
            seen.len(),
            1,
            "起核失败必发且只发一条 lifecycle，实际：{seen:?}"
        );
        assert_eq!(seen[0].phase, "failed");
        assert!(
            seen[0]
                .message
                .as_deref()
                .is_some_and(|m| !m.trim().is_empty()),
            "failed 腿必须带用户可见文案 —— 没有它，条只能显示一个说不清的红：{seen:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `restart` 的 start 腿失败同样可辨（**主场景**：「立即应用」→ 去抖重启 → 起核失败，无人 await）。
    ///
    /// 与 `restart_start_leg_failure_invokes_system_proxy_clearer` 同一条路径、同一个理由：
    /// 挂命令层会漏掉这条腿。这里额外钉住**顺序** —— 停核腿的 `stopped` 必须先到、起核失败的
    /// `failed` 后到；顺序颠倒会让条先转红再被一条 `stopped` 抹回转圈。
    ///
    /// 变异对照：把 `push_lifecycle(failed)` 挪到 `restart` 外层（`finish_lifecycle` 之后）→ 顺序仍对，
    /// 但托盘/自动连接那两条入口不经 `restart` ⇒ 上一条测试转红。两条合起来才锁住「唯一汇流点」。
    #[tokio::test]
    async fn restart_start_leg_failure_emits_stopped_then_failed_in_order() {
        let (rt, dir, _errors, lifecycle) = test_runtime_recording_lifecycle();
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst);

        let r = rt.restart(bad_config()).await;
        assert!(r.is_err(), "前提：坏配置必失败");

        let phases: Vec<&str> = lifecycle.lock().unwrap().iter().map(|e| e.phase).collect();
        assert_eq!(
            phases,
            vec!["stopped", "failed"],
            "重启失败的可见序列必须是「停了 → 没回来」，实际：{phases:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 载荷契约：键集恰为 `{phase}`（ready/stopped）或 `{phase, errorCode?, message}`（failed），
    /// camelCase，且 `ready`/`stopped` **不得**带 error 字段（带了前端就分不清成功与失败）。
    ///
    /// 变异对照：去掉 `#[serde(rename_all = "camelCase")]` → `errorCode` 变 `error_code` → 转红；
    /// 去掉两个 `skip_serializing_if` → ready 帧多出两个 `null` 键 → 转红。
    #[test]
    fn lifecycle_payload_contract_keys() {
        let ready = serde_json::to_value(ProxyLifecycleEvent::ready()).expect("可序列化");
        assert_eq!(
            ready,
            serde_json::json!({ "phase": "ready" }),
            "ready 帧只该有 phase —— 多一个 null 的 error 键就够前端写出错误的判据"
        );
        assert_eq!(
            serde_json::to_value(ProxyLifecycleEvent::stopped()).expect("可序列化"),
            serde_json::json!({ "phase": "stopped" })
        );
        let failed = serde_json::to_value(ProxyLifecycleEvent::failed(&StartError::coded(
            "核起不来",
            code::ROOT_ORPHAN_BLOCKED,
        )))
        .expect("可序列化");
        assert_eq!(
            failed,
            serde_json::json!({
                "phase": "failed",
                "errorCode": code::ROOT_ORPHAN_BLOCKED,
                "message": "核起不来",
            })
        );
        // 无码腿（`start_inner` 里绝大多数 `?`）：只省 errorCode，message 仍在。
        let uncoded = serde_json::to_value(ProxyLifecycleEvent::failed(&StartError {
            message: "写盘失败".into(),
            code: None,
        }))
        .expect("可序列化");
        assert_eq!(
            uncoded,
            serde_json::json!({ "phase": "failed", "message": "写盘失败" }),
            "无码不等于无消息 —— 省掉 message 会让条只能显示一个说不清的红"
        );
    }

    // ══════════════ A4 登录期出口让位：编排面门（emit / 单飞 / eligible raw 读）══════════════

    /// 装 mock emitter 并返回让位事件记录句柄。
    fn test_runtime_recording_fallback() -> (Arc<ProxyRuntime>, PathBuf, MeshLoginFallbackEvents) {
        let (rt, dir) = test_runtime();
        let handle: MeshLoginFallbackEvents = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            mesh_login_fallback: Arc::clone(&handle),
            ..Default::default()
        }));
        (rt, dir, handle)
    }

    /// 让位形态 config：选中账号制 TS 全隧道出口（exitNode 非空 → carries_full_tunnel）+ 非 direct + 无 authKey。
    fn ts_fallback_config() -> Value {
        serde_json::json!({
            "servers": [{
                "id": "ts1", "name": "组网出口", "protocol": "tailscale",
                "address": "100.64.0.5", "port": 0,
                "tailscaleSettings": { "exitNode": "peer-x" }
            }],
            "selectedServerId": "ts1",
            "proxyMode": "smart"
        })
    }

    /// A4：`login_fallback_eligible` 从**原始 JSON** 读 `meshLoginFallbackDirect`（非 UserConfig 结构体字段）。
    /// 变异有牙：打断 raw 读（恒 true）→ 「显式关开关」case 转红；改 `!= Some(false)` 为别的比较 → 缺省 case 转红。
    #[test]
    fn login_fallback_eligible_reads_flag_from_raw_json() {
        let (rt, dir) = test_runtime();
        let raw = ts_fallback_config();
        let cfg: UserConfig = serde_json::from_value(raw.clone()).expect("parse");
        // 缺省（无 meshLoginFallbackDirect 键）→ 视作开 → eligible。
        assert!(
            rt.login_fallback_eligible(&cfg, &raw),
            "缺省应默认开 → 符合让位形态"
        );
        // 显式关（false）→ 不 eligible（用户明确「宁可授权失败也不直连」）。
        let mut raw_off = raw.clone();
        raw_off["meshLoginFallbackDirect"] = Value::Bool(false);
        assert!(
            !rt.login_fallback_eligible(&cfg, &raw_off),
            "meshLoginFallbackDirect=false 必须不让位"
        );
        // 显式开（true）→ eligible。
        let mut raw_on = raw.clone();
        raw_on["meshLoginFallbackDirect"] = Value::Bool(true);
        assert!(rt.login_fallback_eligible(&cfg, &raw_on));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A4：非 TS 出口 / 有 authKey → 不 eligible（谓词其余项，防「只读了开关」的假绿）。
    #[test]
    fn login_fallback_eligible_rejects_non_ts_and_authkey() {
        let (rt, dir) = test_runtime();
        // 非 TS（vless）→ 不 eligible。
        let raw_vless = serde_json::json!({
            "servers": [{ "id": "s1", "name": "x", "protocol": "vless", "address": "a", "port": 1 }],
            "selectedServerId": "s1", "proxyMode": "smart"
        });
        let cfg_vless: UserConfig = serde_json::from_value(raw_vless.clone()).expect("parse");
        assert!(
            !rt.login_fallback_eligible(&cfg_vless, &raw_vless),
            "非 TS 出口不让位"
        );
        // TS 带 authKey（静态凭据，无交互登录死锁）→ 不 eligible。
        let raw_ak = serde_json::json!({
            "servers": [{
                "id": "ts1", "name": "x", "protocol": "tailscale", "address": "100.64.0.5", "port": 0,
                "tailscaleSettings": { "exitNode": "peer-x", "authKey": "tskey-abc" }
            }],
            "selectedServerId": "ts1", "proxyMode": "smart"
        });
        let cfg_ak: UserConfig = serde_json::from_value(raw_ak.clone()).expect("parse");
        assert!(
            !rt.login_fallback_eligible(&cfg_ak, &raw_ak),
            "authKey TS 不让位"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A4：`mark_login_fallback_engaged` 首次 emit `(true, name)`；同出口重复调**幂等不再 emit**；engaged()=true。
    /// 变异有牙：删 `first` 守卫 → 第二次也 emit → len==2 转红；删 emit → len==0 转红。
    #[test]
    fn mark_engaged_emits_once_and_is_idempotent() {
        let (rt, dir, handle) = test_runtime_recording_fallback();
        let cfg: UserConfig = serde_json::from_value(ts_fallback_config()).expect("parse");
        rt.mark_login_fallback_engaged("ts1", &cfg);
        rt.mark_login_fallback_engaged("ts1", &cfg); // 幂等：同出口不再 emit
        assert!(rt.login_fallback_engaged(), "mark 后 engaged 必真");
        let evs = handle.lock().unwrap();
        assert_eq!(evs.len(), 1, "同出口只 emit 一次（first 守卫）");
        assert_eq!(
            evs[0],
            (true, Some("组网出口".to_string())),
            "engage 带出口名"
        );
        drop(evs);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A4：`reset_login_fallback_state` 让位中 → emit `(false, None)` 一次；未让位 → 零 emit（不刷屏）。
    /// 变异有牙：删「未让位 return」→ 未让位也 emit → 转红；删 emit → 让位 reset 后 len==0 转红。
    #[test]
    fn reset_emits_disengage_only_when_engaged() {
        let (rt, dir, handle) = test_runtime_recording_fallback();
        // 未让位 reset → 无 emit。
        rt.reset_login_fallback_state();
        assert!(handle.lock().unwrap().is_empty(), "未让位 reset 不 emit");
        // 让位中 reset → emit(false,None) 一次；再 reset → 无新 emit。
        rt.set_login_fallback(true, Some("ts1".to_string()));
        rt.reset_login_fallback_state();
        rt.reset_login_fallback_state();
        assert!(!rt.login_fallback_engaged(), "reset 后 engaged 必假");
        let evs = handle.lock().unwrap();
        assert_eq!(evs.len(), 1, "让位 reset 只 emit 一次 disengage");
        assert_eq!(evs[0], (false, None));
        drop(evs);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A4：reconcile 单飞——在飞标志占用时重入调用被丢弃（不改状态、零 emit）；正常退场 Guard 必复位标志。
    /// 变异有牙：删 `swap(true)` 早退 → 重入会跑对账动状态 → engaged 断言转红；删 Guard → 标志不复位断言转红。
    #[tokio::test]
    async fn reconcile_single_flight_drops_reentrant_call() {
        let (rt, dir, handle) = test_runtime_recording_fallback();
        // 手动占用单飞标志 + 置让位态 → reconcile 必被挡下（swap 返 true → 早退，不动状态、不 emit）。
        rt.login_fallback_reconciling.store(true, Ordering::SeqCst);
        rt.set_login_fallback(true, Some("ts1".to_string()));
        rt.reconcile_login_fallback().await;
        assert!(
            rt.login_fallback_engaged(),
            "在飞 → reconcile 早退，不改状态"
        );
        assert!(handle.lock().unwrap().is_empty(), "早退 → 零 emit");
        assert!(
            rt.login_fallback_reconciling.load(Ordering::SeqCst),
            "早退路径不复位标志（占用者持有）"
        );
        // 释放后正常一次 reconcile（无 current_config → 早退，但 Guard 必复位标志）。
        rt.login_fallback_reconciling.store(false, Ordering::SeqCst);
        rt.reconcile_login_fallback().await;
        assert!(
            !rt.login_fallback_reconciling.load(Ordering::SeqCst),
            "正常退场 ReconcileGuard 必复位单飞标志"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════ A4 早退闸（P0-2②）：三态决策矩阵不变 + 「切走仍能 disengage」回归 ══════════
    //
    // 被守的是 `reconcile_login_fallback` 开头那道 `!engaged && !选中是 TS` 的合取闸。它只允许跳过
    // 矩阵里「无任何可观测效果」的那一格；下面四条把**有效果**的三格逐格钉住，任何把闸写宽的变异
    // （尤其是漏掉 `!engaged` 那一半）都会在其中一条上转红。

    /// 一 TS + 一 vless 的两节点配置（`selected` 指定选中谁）。TS 侧为账号制全隧道（`exitNode` 非空、
    /// 无 authKey）⇒ 选中它时符合让位形态。
    fn ts_and_vless_config(selected: &str) -> Value {
        serde_json::json!({
            "servers": [
                { "id": "ts1", "name": "组网出口", "protocol": "tailscale",
                  "address": "100.64.0.5", "port": 0,
                  "tailscaleSettings": { "exitNode": "peer-x" } },
                { "id": "node-a", "name": "A", "protocol": "vless",
                  "address": "a.example.com", "port": 443, "uuid": "u-a" }
            ],
            "selectedServerId": selected,
            "proxyMode": "smart"
        })
    }

    /// 往 mesh 末帧缓存塞一条指定 `backendState` 的 STATUS 帧（`selected_exit_backend_state` 的唯一来源）。
    /// Taildrop 四位取中性值，不用 `..Default::default()`：日后加字段时这里必须被人再看一眼。
    fn seed_backend_state(rt: &Arc<ProxyRuntime>, server_id: &str, backend_state: &str) {
        rt.mesh.update_ts_status(vec![TailscaleStatusEvent {
            server_id: server_id.into(),
            backend_state: backend_state.into(),
            logged_in: backend_state == "Running",
            auth_url: None,
            tailscale_ips: vec!["100.64.0.9".into()],
            expired: false,
            peers: Vec::new(),
            can_share_files: false,
            waiting_file_count: 0,
            receiving_file_count: 0,
            unread_file_count: 0,
        }]);
    }

    /// 早退闸的**廉价一半**必须与 `login_fallback_eligible` 里那条 TS 判定同口径。
    ///
    /// 这条判据本身不可由行为观测（矩阵第 6 行两条路都无效果），故在这里直测：既证它没漏判
    /// （选中 TS → 真，闸不会误吞 engage 腿），也证它没误判（切走 / 无选中 / 选中不存在 → 假）。
    ///
    /// **变异锁**：键名写错（`selectedServerId` → `selected_server_id`、`protocol` → `type`）⇒ 首段转红；
    /// 把协议字面量写成 `"Tailscale"` ⇒ 首段转红；去掉「选中项才算」这一跳（改成「任一节点是 TS」）
    /// ⇒ 「切走」那段转红。
    #[test]
    fn selected_exit_is_tailscale_agrees_with_eligible_predicate() {
        let (rt, dir) = test_runtime();
        // 选中 TS：判据为真，且与配置层 eligible 同向。
        let raw = ts_and_vless_config("ts1");
        *rt.current_config.write().unwrap() = Some(raw.clone());
        let cfg: UserConfig = serde_json::from_value(raw.clone()).expect("parse");
        assert!(
            rt.selected_exit_is_tailscale(),
            "选中 TS 出口 → 廉价判据必须为真"
        );
        assert!(
            rt.login_fallback_eligible(&cfg, &raw),
            "正向对照：同一份配置在完整判据下也符合让位形态"
        );
        // 切走到 vless：判据为假（配置里仍有 TS 节点，但它不是选中项）。
        let raw_away = ts_and_vless_config("node-a");
        *rt.current_config.write().unwrap() = Some(raw_away.clone());
        let cfg_away: UserConfig = serde_json::from_value(raw_away.clone()).expect("parse");
        assert!(
            !rt.selected_exit_is_tailscale(),
            "选中的是 vless → 判据必须为假（不能被配置里另一个 TS 节点喂饱）"
        );
        assert!(!rt.login_fallback_eligible(&cfg_away, &raw_away));
        // 空 selectedServerId / 选中 id 不在册 → 假（与 reconcile 里 `sel_id` 的非空过滤同口径）。
        *rt.current_config.write().unwrap() = Some(ts_and_vless_config(""));
        assert!(!rt.selected_exit_is_tailscale(), "空选中 → 假");
        *rt.current_config.write().unwrap() = Some(ts_and_vless_config("ghost"));
        assert!(!rt.selected_exit_is_tailscale(), "选中 id 不在册 → 假");
        // 无配置 → 假（`reconcile` 本就在下一步 return）。
        *rt.current_config.write().unwrap() = None;
        assert!(!rt.selected_exit_is_tailscale(), "无 current_config → 假");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 矩阵第 1 行：eligible + `NeedsLogin` → PUT direct、置 flag、emit(true, 出口名)。
    ///
    /// **变异锁**：闸改成无条件 `return` ⇒ 无 PUT、无 emit ⇒ 转红。
    #[tokio::test]
    async fn reconcile_engages_when_ts_exit_needs_login() {
        let tags = BTreeMap::from([("ts1".to_string(), "组网出口".to_string())]);
        let (rt, dir, sink, _i, _r, fb) =
            reassert_runtime(&ts_and_vless_config("ts1"), tags, BTreeMap::new());
        seed_backend_state(&rt, "ts1", "NeedsLogin");
        rt.reconcile_login_fallback().await;
        assert_eq!(
            sink.calls(),
            vec![("proxy-selector".to_string(), "direct".to_string())],
            "未登录的 TS 出口：默认路由必须让位 direct"
        );
        assert!(rt.login_fallback_engaged(), "PUT 成功 → 让位 flag 必置");
        assert_eq!(
            fb.lock().unwrap().as_slice(),
            &[(true, Some("组网出口".to_string()))]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 矩阵第 2 行（同一选中出口）：已让位 + `Running` → PUT 回该出口 tag、清 flag、emit(false, 出口名)。
    ///
    /// **变异锁**：把 `should_disengage` 里的 `Running` 腿删掉 ⇒ 无 PUT、flag 仍真 ⇒ 转红。
    #[tokio::test]
    async fn reconcile_disengages_when_ts_exit_becomes_running() {
        let tags = BTreeMap::from([("ts1".to_string(), "组网出口".to_string())]);
        let (rt, dir, sink, _i, _r, fb) =
            reassert_runtime(&ts_and_vless_config("ts1"), tags, BTreeMap::new());
        rt.set_login_fallback(true, Some("ts1".to_string()));
        seed_backend_state(&rt, "ts1", "Running");
        rt.reconcile_login_fallback().await;
        assert_eq!(
            sink.calls(),
            vec![("proxy-selector".to_string(), "组网出口".to_string())],
            "隧道就绪 → 默认路由必须切回该出口"
        );
        assert!(!rt.login_fallback_engaged(), "撤销让位必须清 flag");
        assert_eq!(
            fb.lock().unwrap().as_slice(),
            &[(false, Some("组网出口".to_string()))]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **本项存在的唯一风险面**（矩阵第 5 行）：让位中途用户从 TS 出口**切走** → 本帧仍须 disengage。
    ///
    /// 切走后 `eligible` 立刻为假，若早退闸只判「选中是不是 TS」，这一帧会被整个跳过 ⇒ flag 永不清、
    /// 「已让位直连」横幅永不撤、selector 与 UI 长期脱节（陈旧态永不收敛）。这一格必须由谓词里的
    /// `!engaged` 那一半接住。
    ///
    /// **变异锁**：闸改成 `if !self.selected_exit_is_tailscale() { return; }`（丢掉 `!engaged`）⇒
    /// flag 仍真、零 emit ⇒ 本条转红。
    #[tokio::test]
    async fn reconcile_disengages_after_switching_away_from_ts_exit() {
        let tags = BTreeMap::from([
            ("ts1".to_string(), "组网出口".to_string()),
            ("node-a".to_string(), "A".to_string()),
        ]);
        let (rt, dir, sink, _i, _r, fb) =
            reassert_runtime(&ts_and_vless_config("node-a"), tags, BTreeMap::new());
        rt.set_login_fallback(true, Some("ts1".to_string()));
        // 选中已是 vless ⇒ 廉价判据为假；本帧全靠 `!engaged` 那一半才不会被早退闸吞掉。
        assert!(!rt.selected_exit_is_tailscale());
        rt.reconcile_login_fallback().await;
        assert!(
            !rt.login_fallback_engaged(),
            "切走出口后必须清让位 flag —— 否则 engaged 态永不收敛"
        );
        assert_eq!(
            fb.lock().unwrap().as_slice(),
            &[(false, None)],
            "必须撤 UI 让位横幅"
        );
        assert!(
            sink.calls().is_empty(),
            "切走腿只清 flag，不 PUT（selector 已由换节点那条路径落定，再 PUT 会打架）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 矩阵第 4 行：让位中 + 过渡态（`Starting` / 无帧）→ 维持现状，零 PUT 零 emit。
    ///
    /// **变异锁**：把「其余过渡态维持」改成「非 NeedsLogin 即 disengage」⇒ 两段都转红。
    #[tokio::test]
    async fn reconcile_holds_through_transitional_backend_states() {
        let tags = BTreeMap::from([("ts1".to_string(), "组网出口".to_string())]);
        for state in ["Starting", "NoState"] {
            let (rt, dir, sink, _i, _r, fb) =
                reassert_runtime(&ts_and_vless_config("ts1"), tags.clone(), BTreeMap::new());
            rt.set_login_fallback(true, Some("ts1".to_string()));
            seed_backend_state(&rt, "ts1", state);
            rt.reconcile_login_fallback().await;
            assert!(rt.login_fallback_engaged(), "{state}：过渡态不得翻转 flag");
            assert!(sink.calls().is_empty(), "{state}：过渡态不得 PUT");
            assert!(fb.lock().unwrap().is_empty(), "{state}：过渡态不得 emit");
            let _ = std::fs::remove_dir_all(&dir);
        }
        // 无 STATUS 帧（核刚起 / 未选中 TS）同属过渡态。
        let (rt, dir, sink, _i, _r, fb) =
            reassert_runtime(&ts_and_vless_config("ts1"), tags, BTreeMap::new());
        rt.set_login_fallback(true, Some("ts1".to_string()));
        rt.reconcile_login_fallback().await;
        assert!(rt.login_fallback_engaged(), "无帧：不得翻转 flag");
        assert!(sink.calls().is_empty());
        assert!(fb.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🟡 **源码型守卫**：早退闸必须是**两条腿的合取**，且必须排在整配置深拷贝**之前**。
    ///
    /// 行为断言管不到这两件事：闸写弱（只剩 `!engaged`）行为完全等价、只是白付；闸挪到 clone 之后
    /// 则一分钱都省不下来，而全部行为断言照绿。故这一条只能落在源码上。
    ///
    /// **变异锁**：删任一合取项 / 把 `&&` 改 `||` ⇒ 首段 `find` 落空转红；把闸挪到 `let Some(raw) = …`
    /// 之后 ⇒ 顺序断言转红。
    #[test]
    fn login_fallback_early_gate_is_a_conjunction_before_the_clone() {
        let body = method_body(
            include_str!("proxy.rs"),
            "    async fn reconcile_login_fallback(&self) {",
        );
        let gate = body
            .find("if !self.login_fallback_engaged() && !self.selected_exit_is_tailscale() {")
            .expect(
                "早退闸不见了或谓词被改写 —— 只判『选中是不是 TS』会杀掉 disengage 腿，\
                 只判 `!engaged` 则省不掉非 TS 用户的每帧成本",
            );
        let clone_site = body
            .find("self.current_config.read().ok().and_then(|g| g.clone())")
            .expect("整配置深拷贝的锚点消失，本守卫已失去判据");
        assert!(
            gate < clone_site,
            "早退闸必须排在整配置深拷贝之前，否则跳过的是白工之后的那一段，一分钱不省"
        );
    }

    // ══════════════ H3：起核后 selector 校正（reassert_selector_selection）══════════════
    //
    // 被测的是**序列**不变式（谁被 PUT 成什么、按什么顺序、续延排在哪），故一律经
    // `management_api_stub` 断言 PUT 序列。全程零网络、零进程：核不起，PUT 落在内存桩上。
    // 真 gRPC PUT / 真核 cache_file 覆盖行为属真机门（`real_core_hot_switch_keeps_pid`）。

    /// 装 PUT 桩的运行时：核状态置「运行中」（不起真核）+ 装 `switch_snapshot` + `current_config`。
    #[allow(clippy::type_complexity)]
    fn reassert_runtime(
        cfg: &Value,
        id_to_tag: BTreeMap<String, String>,
        rule_target: BTreeMap<String, RuleTargetEntry>,
    ) -> (
        Arc<ProxyRuntime>,
        PathBuf,
        Arc<TestPutSink>,
        UnlockInvalidations,
        ExitIpRefreshes,
        MeshLoginFallbackEvents,
    ) {
        let (rt, dir) = test_runtime();
        let inval: UnlockInvalidations = Arc::new(Mutex::new(Vec::new()));
        let refreshes: ExitIpRefreshes = Arc::new(Mutex::new(Vec::new()));
        let fallback: MeshLoginFallbackEvents = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            unlock_invalidations: Arc::clone(&inval),
            exit_ip_refreshes: Arc::clone(&refreshes),
            mesh_login_fallback: Arc::clone(&fallback),
            ..Default::default()
        }));
        let sink = Arc::new(TestPutSink::default());
        *rt.management_api_stub.lock().unwrap() = Some(Arc::clone(&sink));
        mark_running(&rt);
        let uc: UserConfig = serde_json::from_value(cfg.clone()).expect("parse UserConfig");
        *rt.switch_snapshot.write().unwrap() = Some(SwitchSnapshot {
            id_to_tag,
            rule_target,
            fingerprints: node_fingerprints::modified_table(&uc.servers),
            dirty_fingerprints: node_fingerprints::dirty_table(&uc.servers),
            ..Default::default()
        });
        *rt.current_config.write().unwrap() = Some(cfg.clone());
        (rt, dir, sink, inval, refreshes, fallback)
    }

    /// 两个 vless 节点的配置（`node-a` 选中）。
    fn reassert_config(selected: &str) -> Value {
        serde_json::json!({
            "servers": [
                { "id": "node-a", "name": "A", "protocol": "vless",
                  "address": "a.example.com", "port": 443, "uuid": "u-a" },
                { "id": "node-b", "name": "B", "protocol": "vless",
                  "address": "b.example.com", "port": 443, "uuid": "u-b" }
            ],
            "selectedServerId": selected,
            "proxyMode": "smart"
        })
    }

    fn ab_tags() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("node-a".to_string(), "A".to_string()),
            ("node-b".to_string(), "B".to_string()),
        ])
    }

    /// **不变式①**：起核后 `proxy-selector` 必被 PUT 成**选中节点的 tag** —— 这是 H3 的整个存在理由。
    ///
    /// 不 PUT 就等于把出口交给 `cache_file` 里上一轮的残留选择（真机血证：盘上选 Hk01、核实走
    /// Tailscale → 家用路由 OpenClash → Jp01，全链路零告警）。
    ///
    /// **变异锁**：把 stage 1 里那次 `hot_switch_selector(PROXY_SELECTOR_TAG, member_tag)` 删掉
    /// （只留循环与 break）→ PUT 序列空 → 转红。
    #[tokio::test]
    async fn reassert_puts_proxy_selector_to_selected_tag() {
        let (rt, dir, sink, _inval, _refresh, _fb) =
            reassert_runtime(&reassert_config("node-a"), ab_tags(), BTreeMap::new());
        let cfg: UserConfig = serde_json::from_value(reassert_config("node-a")).unwrap();
        let my_gen = rt.gate.generation();
        rt.reassert_selector_selection(&cfg, my_gen).await;
        assert_eq!(
            sink.calls(),
            vec![("proxy-selector".to_string(), "A".to_string())],
            "起核后必须把 proxy-selector 拨回选中节点的 tag（压过 cache_file 旧选择）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **不变式②**：选中的是**未登录**的账号制 TS 全隧道出口 → 本轮 PUT 的必须是 `direct`（不是那个
    /// 连不上的 TS tag），且 **PUT 成功之后**才置让位 flag。
    ///
    /// 后半条是「flag 与 selector 不得脱节」的唯一保证：先置 flag 再 PUT，PUT 失败时 UI 会显示「已让位
    /// 直连」而 selector 实际仍指着未就绪的 TS 出口 —— 用户看到的和跑着的是两回事。
    ///
    /// **变异锁**：① 把 `member_tag` 恒取 `tag`（删掉 `want_direct` 分支）→ 第一段断言 PUT 到 TS tag 转红；
    /// ② 把 `mark_login_fallback_engaged` 挪到 `hot_switch_selector` 之前/之外（无条件置）→ 第二段
    /// 「全失败仍不置 flag」转红。
    #[tokio::test]
    async fn reassert_yields_to_direct_when_ts_exit_never_logged_in() {
        // ── 成功腿：PUT direct + 置 flag + emit ──
        let tags = BTreeMap::from([("ts1".to_string(), "组网出口".to_string())]);
        let (rt, dir, sink, _inval, _refresh, fb) =
            reassert_runtime(&ts_fallback_config(), tags.clone(), BTreeMap::new());
        let cfg: UserConfig = serde_json::from_value(ts_fallback_config()).unwrap();
        let my_gen = rt.gate.generation();
        rt.reassert_selector_selection(&cfg, my_gen).await;
        assert_eq!(
            sink.calls(),
            vec![("proxy-selector".to_string(), "direct".to_string())],
            "未登录的 TS 出口：PUT 的必须是 direct，不是连不上的 TS tag"
        );
        assert!(rt.login_fallback_engaged(), "PUT 成功 → 让位 flag 必置");
        assert_eq!(
            fb.lock().unwrap().as_slice(),
            &[(true, Some("组网出口".to_string()))],
            "首次让位 emit 一次"
        );
        let _ = std::fs::remove_dir_all(&dir);

        // ── 失败腿：PUT 全失败 → 绝不置 flag（flag 与 selector 同进退）──
        let (rt2, dir2, sink2, _i2, _r2, fb2) =
            reassert_runtime(&ts_fallback_config(), tags, BTreeMap::new());
        sink2
            .fail_first
            .store(ProxyRuntime::REASSERT_MAX_ROUNDS as u32, Ordering::SeqCst);
        let my_gen2 = rt2.gate.generation();
        rt2.reassert_selector_selection(&cfg, my_gen2).await;
        assert_eq!(
            sink2.calls().len(),
            ProxyRuntime::REASSERT_MAX_ROUNDS,
            "管理 API 一直不就绪 → 跑满重试轮数"
        );
        assert!(
            !rt2.login_fallback_engaged(),
            "PUT 从未成功 → 绝不置让位 flag（否则 UI 说直连、selector 指着 TS 出口）"
        );
        assert!(fb2.lock().unwrap().is_empty(), "未让位成功 → 零 emit");
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// **不变式③**：起核窗口内用户已热切到别的节点 → 重试轮必须跟**最新**的 `selectedServerId`，
    /// 绝不能把它 revert 回起核那一刻的旧节点。
    ///
    /// 首轮 PUT 注入失败 → 退避 300ms；退避期间外部把 `current_config` 改成 `node-b`；第二轮必须 PUT `B`。
    ///
    /// **变异锁**：把每轮的 `current_config` 现读提到循环**外**（只读一次）→ 第二次 PUT 仍是 `A` → 转红。
    #[tokio::test]
    async fn reassert_follows_latest_selection_across_retries() {
        let (rt, dir, sink, _inval, _refresh, _fb) =
            reassert_runtime(&reassert_config("node-a"), ab_tags(), BTreeMap::new());
        sink.fail_first.store(1, Ordering::SeqCst); // 首轮失败 → 进退避重试腿
        let cfg: UserConfig = serde_json::from_value(reassert_config("node-a")).unwrap();
        let my_gen = rt.gate.generation();
        let rt2 = Arc::clone(&rt);
        // 退避窗口（300ms）内热切到 node-b。
        let switcher = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            *rt2.current_config.write().unwrap() = Some(reassert_config("node-b"));
        });
        rt.reassert_selector_selection(&cfg, my_gen).await;
        switcher.await.unwrap();
        assert_eq!(
            sink.calls(),
            vec![
                ("proxy-selector".to_string(), "A".to_string()),
                ("proxy-selector".to_string(), "B".to_string()),
            ],
            "重试轮必须跟最新选中节点（B），不得把用户刚切的选择 revert 回起核时的 A"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **不变式④**：有 `targetServerId` 的规则 → PUT 其 rule-sel；无 `targetServerId` 的规则 → **跳过**
    /// （它们生成时 `default = proxy-selector`，嵌套跟随全局，不需要也不该被单独钉死）。
    ///
    /// selector tag 取自 `switch_snapshot.rule_target`（生成侧真值），本例故意让 `r1` 的 tag 带撞名去重
    /// 后缀 `rule-sel-r1 (1)` —— 手拼 `format!("rule-sel-{id}")` 会 PUT 到一个不存在的 tag。
    ///
    /// **变异锁**：① 把「无 target 就 continue」改成回落一个默认目标 → 序列里多出 `rule-sel-r2` → 转红；
    /// ② 把 `entry.selector_tag` 换成手拼模板 → 第一条断言的 `rule-sel-r1 (1)` 转红。
    #[tokio::test]
    async fn reassert_rule_selectors_skips_rules_without_target() {
        let mut cfg = reassert_config("node-a");
        cfg["customRules"] = serde_json::json!([
            { "id": "r1", "type": "domain", "values": ["x.com"], "action": "proxy",
              "enabled": true, "targetServerId": "node-b" },
            { "id": "r2", "type": "domain", "values": ["y.com"], "action": "proxy",
              "enabled": true },
            { "id": "r3", "type": "domain", "values": ["z.com"], "action": "proxy",
              "enabled": false, "targetServerId": "node-b" }
        ]);
        cfg["appRules"] = serde_json::json!([
            { "appId": "app1", "action": "proxy", "enabled": true, "targetServerId": "node-a" }
        ]);
        let rule_target = BTreeMap::from([
            (
                "custom:r1".to_string(),
                RuleTargetEntry {
                    selector_tag: "rule-sel-r1 (1)".into(), // 撞名去重后的真实 tag
                    member_tag: "B".into(),
                },
            ),
            (
                "custom:r2".to_string(),
                RuleTargetEntry {
                    selector_tag: "rule-sel-r2".into(),
                    member_tag: "proxy-selector".into(),
                },
            ),
            (
                "custom:r3".to_string(),
                RuleTargetEntry {
                    selector_tag: "rule-sel-r3".into(),
                    member_tag: "B".into(),
                },
            ),
            (
                "app:app1".to_string(),
                RuleTargetEntry {
                    selector_tag: "rule-sel-app-app1".into(),
                    member_tag: "A".into(),
                },
            ),
        ]);
        let (rt, dir, sink, _inval, _refresh, _fb) = reassert_runtime(&cfg, ab_tags(), rule_target);
        let uc: UserConfig = serde_json::from_value(cfg).unwrap();
        let my_gen = rt.gate.generation();
        rt.reassert_selector_selection(&uc, my_gen).await;
        assert_eq!(
            sink.calls(),
            vec![
                ("proxy-selector".to_string(), "A".to_string()),
                ("rule-sel-r1 (1)".to_string(), "B".to_string()),
                ("rule-sel-app-app1".to_string(), "A".to_string()),
            ],
            "只 reassert 有 targetServerId 的启用 proxy 规则；无 target（r2）/ 禁用（r3）一律跳过，\
             且 selector tag 必须取自生成侧真值（含撞名去重后缀）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **不变式⑤（行为门，非结构门）**：续延（失效解锁缓存 + 重探出口 IP）必须**晚于** reassert 的
    /// 每一次 PUT。
    ///
    /// 早于则 boot 窗口内起跑的解锁检测轮 / 出口 IP 探测量的还是**旧出口**，其脏结果会被当新鲜数据
    /// commit 进缓存（epoch 守卫对这次翻转失明）—— 这正是 上游 F-C 修的东西。判据是 PUT 那一刻抄下来
    /// 的续延计数：全为 0 ⟺ 每次 PUT 都发生在续延之前（只看终态验不出顺序）。
    ///
    /// **变异锁**：把 `spawn_reassert_selector_selection` 里的续延从 `ReassertSettledGuard` 改成
    /// 「先 `me.after_selector_reasserted(my_gen)` 再 await reassert」→ 观测值变成 `[1]` → 转红。
    #[tokio::test]
    async fn continuation_runs_strictly_after_reassert_puts() {
        let (rt, dir, sink, inval, refresh, _fb) =
            reassert_runtime(&reassert_config("node-a"), ab_tags(), BTreeMap::new());
        *sink.invalidation_probe.lock().unwrap() = Some(Arc::clone(&inval));
        let uc: UserConfig = serde_json::from_value(reassert_config("node-a")).unwrap();
        let my_gen = rt.gate.generation();
        rt.spawn_reassert_selector_selection(uc, my_gen, 0);
        // 后台腿：轮询等它跑完（无 PUT 失败 ⇒ 单轮即结束，这里给足余量）。
        for _ in 0..100 {
            if !inval.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            sink.calls(),
            vec![("proxy-selector".to_string(), "A".to_string())],
            "前提：校正确实 PUT 过"
        );
        assert_eq!(
            sink.observed_invalidations.lock().unwrap().as_slice(),
            &[0],
            "每次 PUT 的那一刻续延都还没跑过（续延必须严格晚于校正）"
        );
        assert_eq!(
            inval.lock().unwrap().as_slice(),
            &[(true, false)],
            "续延跑且只跑一次，参数为 running=true / exit_blocked=false"
        );
        assert_eq!(
            refresh.lock().unwrap().as_slice(),
            &[true],
            "出口 IP 重探同样只在续延里排一次（留在主链上则探到的是校正前的旧出口）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **不变式⑥**：reassert 中途 panic → 续延**仍必须跑**（= 上游 `.finally()` 的语义）。
    ///
    /// 丢了续延的后果是静默的：解锁缓存永不失效，boot 窗口那轮经旧出口探到的脏结果永久留在缓存里，
    /// 没有任何日志或 UI 迹象。
    ///
    /// **变异锁**：把 `let _settled = ReassertSettledGuard(...)` 换成「await 之后直接调
    /// `me.after_selector_reasserted(my_gen)`」→ panic 展开跳过该行 → 续延零次 → 转红。
    #[tokio::test]
    async fn continuation_still_runs_when_reassert_panics() {
        let (rt, dir, sink, inval, refresh, _fb) =
            reassert_runtime(&reassert_config("node-a"), ab_tags(), BTreeMap::new());
        sink.panic_on_put.store(true, Ordering::SeqCst);
        let uc: UserConfig = serde_json::from_value(reassert_config("node-a")).unwrap();
        let my_gen = rt.gate.generation();
        // panic 的 backtrace 噪音对本门无意义，压掉（退场时还原，不影响并发跑的其它测试的默认 hook）。
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        rt.spawn_reassert_selector_selection(uc, my_gen, 0);
        for _ in 0..100 {
            if !inval.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        std::panic::set_hook(prev);
        assert!(sink.calls().is_empty(), "前提：PUT 在记录前就 panic 了");
        assert_eq!(
            inval.lock().unwrap().as_slice(),
            &[(true, false)],
            "reassert panic 也必须跑续延（Drop 守卫 = 上游的 .finally()）"
        );
        assert_eq!(
            refresh.lock().unwrap().as_slice(),
            &[true],
            "续延的两件事同进退：panic 腿也必须排出口 IP 重探"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **接线守卫（源码型，非行为门）**：`start_inner` 只能 **spawn** 校正腿，且解锁失效 / 出口 IP
    /// 重探**只能**在续延里发生。
    ///
    /// 行为门够不着这三条：第一条是「不阻塞起核」（要真起核才量得到那 ≤3s）；后两条是「主链上没有
    /// 第二个写者」—— 多失效一次 / 多排一次探测不改变任何可断言的终态，却会把 boot 窗口内经旧出口拿到的
    /// 脏结果重新放回来。
    ///
    /// **变异锁**：① 把 spawn 改成 `self.reassert_selector_selection(...).await` → 第一、二条转红；
    /// ② 把 `self.invalidate_unlock_cache(true, false)` 加回起核主链 → 第三条转红；
    /// ③ 把 `self.schedule_exit_ip_refresh(true)` 加回起核主链 → 第四条转红。
    #[test]
    fn start_inner_spawns_reassert_and_defers_unlock_invalidation() {
        let body = method_body(include_str!("proxy.rs"), "    async fn start_inner(");
        assert_eq!(
            body.matches("self.spawn_reassert_selector_selection(")
                .count(),
            1,
            "起核就绪段必须**spawn**一次 selector 校正腿"
        );
        assert!(
            !body.contains("self.reassert_selector_selection("),
            "校正腿绝不能 await 在起核主链上：最坏 10×300ms≈3s，会挂在已经偏慢的起核路径上"
        );
        assert!(
            !body.contains("self.invalidate_unlock_cache("),
            "解锁失效必须只在校正的续延里发生（上游 F-C）：留在主链上则 boot 窗口内经**旧出口**\
             起跑的解锁轮，其结果会被当新鲜数据 commit 进缓存"
        );
        assert!(
            !body.contains("self.schedule_exit_ip_refresh("),
            "出口 IP 重探同理必须只在续延里排：留在主链上则校正一旦真翻转 selector，探到并写进 ipinfo \
             缓存的是**校正前那个出口**的公网 IP"
        );
        assert!(
            !body.contains("self.schedule_connection_flush("),
            "连接 flush 同理必须只在续延里排（上游「时序修 E」）：被 RST 的连接会立刻重连、且按重连\
             那一刻的 selector 建链 —— 早于校正就等于把用户全部连接亲手踢到 cache_file 的旧出口上"
        );
    }

    /// **接线守卫（源码型，非行为门）**：三条续延动作必须都在 `after_selector_reasserted` 里，
    /// 且都排在世代守卫**之后**。
    ///
    /// 与上一条互补：上一条证「主链上没有」，这条证「续延里真有且只有一份」——两条都在，删掉任一
    /// 落点才必然有门转红（只有上一条时，把三行整个删掉是全绿的：主链确实也没有）。
    ///
    /// 世代守卫的位置是承重的：这三条动作全部对着**起核那一刻的那个核**（广播 `running:true`、
    /// 对 `api_port` 开 flush 枪）。把它们排在早退之前 = 核已被停/换时仍照发，等于亲手造假信号 +
    /// 把新核刚建的连接 RST 掉。
    ///
    /// **变异锁**：① 删 `after_selector_reasserted` 里任一行 → 对应计数转红；
    /// ② 把世代早退挪到三行之后 → 位置断言转红。
    #[test]
    fn selector_reassert_continuation_holds_all_three_deferred_actions() {
        let body = method_body(
            include_str!("proxy.rs"),
            "    fn after_selector_reasserted(",
        );
        for (needle, why) in [
            (
                "self.invalidate_unlock_cache(",
                "解锁失效（上游 F-C）：boot 窗口那轮量的是旧出口，必须作废重跑",
            ),
            (
                "self.schedule_exit_ip_refresh(",
                "出口 IP 重探：它量的就是「我现在从哪出去」，必须在校正落定后才排",
            ),
            (
                "self.schedule_connection_flush(",
                "连接 flush（上游 时序修 E）：RST 后的重连必须走校正后的 selector",
            ),
        ] {
            assert_eq!(
                body.matches(needle).count(),
                1,
                "`after_selector_reasserted` 必须恰含一次 `{needle}` —— {why}"
            );
        }
        let guard = body
            .find("if self.gate.generation() != my_gen")
            .expect("世代守卫消失，本门已失去判据");
        for needle in [
            "self.invalidate_unlock_cache(",
            "self.schedule_exit_ip_refresh(",
            "self.schedule_connection_flush(",
        ] {
            let at = body.find(needle).expect("上一条断言已保证存在");
            assert!(
                guard < at,
                "`{needle}` 必须排在世代早退之后：核已被停/换时照发 = 假的 running:true + 把新核\
                 刚建的连接 RST 掉"
            );
        }
    }

    // ══════════ H3 阶段 3：运行期出口自证（attest_runtime_selector）══════════
    //
    // 被测的轴是「核**实际**指着谁」对「校正腿的意图」——`attest_selected_exit` 对这条轴恒盲
    // （它拿生成产物对盘上意图，本 bug 下两边都写着选中节点 ⇒ 必判 Match）。真 gRPC 读回属
    // `crates/singbox-grpc/tests/mock_server.rs` 的 wire 门与真机门，此处零网络零进程。

    /// 同 `reassert_runtime`，但**额外把 `event:proxyError` 的记录句柄带出来**。
    ///
    /// 不改 `reassert_runtime` 的返回元组是有意的：那会逼既有 7 个 H3 用例逐个加一个 `_` 绑定，
    /// 而这个文件此刻有多路改动在飞，无谓的行位移只会制造合并冲突。
    fn reassert_runtime_watching_errors(
        cfg: &Value,
        id_to_tag: BTreeMap<String, String>,
        rule_target: BTreeMap<String, RuleTargetEntry>,
    ) -> (Arc<ProxyRuntime>, PathBuf, Arc<TestPutSink>, ErrorEvents) {
        let (rt, dir) = test_runtime();
        let events: ErrorEvents = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            events: Arc::clone(&events),
            ..Default::default()
        }));
        let sink = Arc::new(TestPutSink::default());
        *rt.management_api_stub.lock().unwrap() = Some(Arc::clone(&sink));
        mark_running(&rt);
        let uc: UserConfig = serde_json::from_value(cfg.clone()).expect("parse UserConfig");
        *rt.switch_snapshot.write().unwrap() = Some(SwitchSnapshot {
            id_to_tag,
            rule_target,
            fingerprints: node_fingerprints::modified_table(&uc.servers),
            dirty_fingerprints: node_fingerprints::dirty_table(&uc.servers),
            ..Default::default()
        });
        *rt.current_config.write().unwrap() = Some(cfg.clone());
        (rt, dir, sink, events)
    }

    fn group(tag: &str, selected: &str) -> GroupSelection {
        GroupSelection {
            tag: tag.to_string(),
            selected: selected.to_string(),
        }
    }

    fn applied(member_tag: &str, rule_intents: &[(&str, &str)]) -> ReassertOutcome {
        ReassertOutcome {
            stage1: Stage1Outcome::Applied {
                member_tag: member_tag.to_string(),
            },
            rule_intents: rule_intents
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        }
    }

    /// **本缺陷的正身**：PUT 成功、生成产物也自洽，但核**运行期**仍停在 `cache_file` 的旧选择上
    /// （真机血证：盘上 Hk01、`proxy-selector.default = "Hk01"`、核实走 `Tailscale`）。
    ///
    /// `attest_selected_exit` 对这一幕恒判 `Match`（它比的是两份同源的意图）；本轴必须判出 drift。
    ///
    /// **变异锁**：把 `attest_runtime_selection` 里 `got != member_tag` 改成 `false`（或整支直接返
    /// `Match`）→ 转红。
    #[test]
    fn runtime_selection_drift_is_caught_where_static_attest_is_blind() {
        let got = attest_runtime_selection(
            &applied("Hk01", &[]),
            Some(&[group(PROXY_SELECTOR_TAG, "Tailscale")]),
        );
        match got {
            SelectorAttestation::GlobalDrift {
                want,
                got,
                rule_drifts,
            } => {
                assert_eq!(
                    (want.as_str(), got.as_str(), rule_drifts),
                    ("Hk01", "Tailscale", 0)
                );
            }
            other => panic!(
                "运行期分叉必须判 GlobalDrift，实得 {}",
                other.user_message()
            ),
        }
        // 对照：同一份意图下运行期确实是 Hk01 → 零告警（假阳性会让整条通道失信）。
        assert!(matches!(
            attest_runtime_selection(
                &applied("Hk01", &[]),
                Some(&[group(PROXY_SELECTOR_TAG, "Hk01")])
            ),
            SelectorAttestation::Match
        ));
    }

    /// **「没证据」不得报成「有问题」**：读不到快照（`None`）/ 快照里查无 `proxy-selector` → 判 `Match`。
    ///
    /// 反过来做（读不到就报）会让每次管理 API 抖动都弹一条「流量没走选中节点」，而那一侧本来就已由
    /// `PutExhausted` 腿覆盖 —— 同因异名的重复告警是把整条通道推向被无视的最快路径。
    ///
    /// **变异锁**：把 `let Some(groups) = groups else { return Match }` 改成 `unwrap_or_default()`
    /// （空切片继续往下走）→ 仍是 Match，本条不红；改成「读不到就 GlobalDrift」→ 两条断言都转红。
    #[test]
    fn unobservable_runtime_selection_stays_silent() {
        assert!(
            matches!(
                attest_runtime_selection(&applied("Hk01", &[]), None),
                SelectorAttestation::Match
            ),
            "读不到运行期快照 ≠ 出口走错"
        );
        assert!(
            matches!(
                attest_runtime_selection(
                    &applied("Hk01", &[]),
                    Some(&[group("some-other-group", "whatever")])
                ),
                SelectorAttestation::Match
            ),
            "快照里查无 proxy-selector ≠ 出口走错"
        );
    }

    /// **两条放弃腿必须变成用户可见信号**（此前只有 `log::warn`）：它们就是「selector 原样停在
    /// cache_file 旧选择上」的那个状态。且它们**不依赖读回**（`groups=None` 照报）——管理 API 正是
    /// 在这两腿下最可能读不到。
    ///
    /// **变异锁**：把 `UnresolvedTag` / `PutExhausted` 任一支改成返 `Match`（= 退回只写日志）→ 转红。
    #[test]
    fn reassert_giveup_legs_are_reported_even_without_readback() {
        match attest_runtime_selection(
            &ReassertOutcome {
                stage1: Stage1Outcome::UnresolvedTag {
                    selected_id: "node-x".into(),
                },
                rule_intents: Vec::new(),
            },
            None,
        ) {
            SelectorAttestation::NeverReasserted { selected_id } => {
                assert_eq!(selected_id, "node-x")
            }
            other => panic!("解析不出 tag 必须报，实得 {}", other.user_message()),
        }
        match attest_runtime_selection(
            &ReassertOutcome {
                stage1: Stage1Outcome::PutExhausted {
                    member_tag: "Hk01".into(),
                },
                rule_intents: Vec::new(),
            },
            None,
        ) {
            SelectorAttestation::ReassertFailed { member_tag } => assert_eq!(member_tag, "Hk01"),
            other => panic!("PUT 跑满仍失败必须报，实得 {}", other.user_message()),
        }
        // 主动退场（核已停 / 世代已变）**不是**缺陷：那个核已经不是用户在看的那个了。
        assert!(matches!(
            attest_runtime_selection(
                &ReassertOutcome {
                    stage1: Stage1Outcome::Abandoned,
                    rule_intents: Vec::new(),
                },
                None
            ),
            SelectorAttestation::Match
        ));
    }

    /// 分流规则侧同轴：全局对上了、但 rule-sel 停在别处 → 仍要报（`RuleDrift`）；全局也错时并进
    /// `GlobalDrift` 的计数，**不刷两条**（`error_code` 是单槽，后来的会把前一条挤掉）。
    ///
    /// **变异锁**：删 `reassert_rule_selectors` 的返回值收集（intents 恒空）→ 第一段的 count 变 0 →
    /// 转红；把 `rule_drifts.len()` 写死 0 → 第二段转红。
    #[test]
    fn rule_selector_drift_is_on_the_same_axis() {
        match attest_runtime_selection(
            &applied("Hk01", &[("rule-sel-r1", "Jp02"), ("rule-sel-r2", "Hk01")]),
            Some(&[
                group(PROXY_SELECTOR_TAG, "Hk01"),
                group("rule-sel-r1", "Tailscale"),
                group("rule-sel-r2", "Hk01"),
            ]),
        ) {
            SelectorAttestation::RuleDrift {
                count,
                sample_tag,
                want,
                got,
            } => {
                assert_eq!(count, 1, "只有 r1 分叉");
                assert_eq!(
                    (sample_tag.as_str(), want.as_str(), got.as_str()),
                    ("rule-sel-r1", "Jp02", "Tailscale")
                );
            }
            other => panic!("规则出口分叉必须报，实得 {}", other.user_message()),
        }
        match attest_runtime_selection(
            &applied("Hk01", &[("rule-sel-r1", "Jp02")]),
            Some(&[
                group(PROXY_SELECTOR_TAG, "Tailscale"),
                group("rule-sel-r1", "Tailscale"),
            ]),
        ) {
            SelectorAttestation::GlobalDrift { rule_drifts, .. } => assert_eq!(
                rule_drifts, 1,
                "全局与规则同时分叉 → 并成一条，规则数并进计数"
            ),
            other => panic!("全局分叉优先报，实得 {}", other.user_message()),
        }
    }

    /// **组合路径**（§K7.1：光测纯函数、光测 emit 都不够）：`Applied` + 桩里摆一份分叉的运行期快照
    /// → 真 emit `event:proxyError`（`EXIT_MISMATCH`）+ 落 `status.error_code`，且**不把核标成未运行**。
    ///
    /// **变异锁**：把 `attest_runtime_selector` 的告警腿改成 `log::warn!` → 零事件 → 转红（退回静默）。
    #[tokio::test]
    async fn attest_runtime_selector_emits_and_keeps_running() {
        let (rt, dir, sink, events) = reassert_runtime_watching_errors(
            &reassert_config("node-a"),
            ab_tags(),
            BTreeMap::new(),
        );
        *sink.groups.lock().unwrap() = Some(vec![group(PROXY_SELECTOR_TAG, "Tailscale")]);
        let my_gen = rt.gate.generation();

        rt.attest_runtime_selector(&applied("A", &[]), my_gen).await;

        let got = events.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            1,
            "运行期分叉必须发一条 proxyError，实得 {got:?}"
        );
        assert_eq!(got[0].1, code::EXIT_MISMATCH);
        assert!(
            got[0].0.contains("Tailscale"),
            "文案须点名实际出口：{}",
            got[0].0
        );
        assert!(rt.status().running, "核确在跑 → 不得标成未运行");
        assert_eq!(rt.status().error_code.as_deref(), Some(code::EXIT_MISMATCH));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一致 → **零告警**；且世代已变时**整段退场**（读到什么都不是「用户在看的那个核」的事实）。
    ///
    /// **变异锁**：删 `attest_runtime_selector` 的世代/存活守卫 → 第二段转红（对着换代后的核报了一条）。
    #[tokio::test]
    async fn attest_runtime_selector_silent_when_consistent_or_superseded() {
        let (rt, dir, sink, events) = reassert_runtime_watching_errors(
            &reassert_config("node-a"),
            ab_tags(),
            BTreeMap::new(),
        );
        *sink.groups.lock().unwrap() = Some(vec![group(PROXY_SELECTOR_TAG, "A")]);
        let my_gen = rt.gate.generation();
        rt.attest_runtime_selector(&applied("A", &[]), my_gen).await;
        assert!(
            events.lock().unwrap().is_empty(),
            "运行期一致不得告警，实得 {:?}",
            events.lock().unwrap()
        );

        // 世代已变：即便快照分叉、即便终局是放弃腿，也一律不报。
        *sink.groups.lock().unwrap() = Some(vec![group(PROXY_SELECTOR_TAG, "Tailscale")]);
        rt.attest_runtime_selector(&applied("A", &[]), my_gen.wrapping_add(1))
            .await;
        rt.attest_runtime_selector(
            &ReassertOutcome {
                stage1: Stage1Outcome::PutExhausted {
                    member_tag: "A".into(),
                },
                rule_intents: Vec::new(),
            },
            my_gen.wrapping_add(1),
        )
        .await;
        assert!(
            events.lock().unwrap().is_empty(),
            "世代已变 → 整段退场，实得 {:?}",
            events.lock().unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **终局必须真从校正腿带出来**（而不是自证自己另算一份）：管理 API 一直不就绪 → 跑满重试 →
    /// `PutExhausted{member_tag}`，且 tag 是最后一轮的**最新**意图。
    ///
    /// 这是「放弃腿此前只写 log」那个洞的正身：终局若还是 `()`，调用方无从分辨成功与放弃。
    ///
    /// **变异锁**：把 stage 1 里 `stage1 = Stage1Outcome::PutExhausted{...}` 那行删掉（回到只在函数
    /// 开头设一次初值）→ `member_tag` 变空串 → 转红。
    #[tokio::test]
    async fn reassert_outcome_reports_put_exhaustion_with_latest_intent() {
        let (rt, dir, sink, _events) = reassert_runtime_watching_errors(
            &reassert_config("node-a"),
            ab_tags(),
            BTreeMap::new(),
        );
        sink.fail_first
            .store(ProxyRuntime::REASSERT_MAX_ROUNDS as u32, Ordering::SeqCst);
        let cfg: UserConfig = serde_json::from_value(reassert_config("node-a")).unwrap();
        let my_gen = rt.gate.generation();

        let outcome = rt.reassert_selector_selection(&cfg, my_gen).await;

        match outcome.stage1 {
            Stage1Outcome::PutExhausted { member_tag } => assert_eq!(
                member_tag, "A",
                "跑满退出时终局须带最后一轮的意图 tag（供文案点名）"
            ),
            _ => panic!("PUT 全失败必须留下 PutExhausted 终局"),
        }
        assert_eq!(
            sink.calls().len(),
            ProxyRuntime::REASSERT_MAX_ROUNDS,
            "前提：确实跑满了重试轮"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 解析不出 tag（选中节点不在运行核 tag 映射里）→ `UnresolvedTag`，**且一次 PUT 都不发**。
    ///
    /// **变异锁**：把该腿的 `stage1 = Stage1Outcome::UnresolvedTag{...}` 删掉 → 终局退化成
    /// `PutExhausted{member_tag: ""}` → 转红。
    #[tokio::test]
    async fn reassert_outcome_reports_unresolved_tag_without_putting() {
        // tag 映射里只有 node-b，选中的却是 node-a。
        let (rt, dir, sink, _events) = reassert_runtime_watching_errors(
            &reassert_config("node-a"),
            BTreeMap::from([("node-b".to_string(), "B".to_string())]),
            BTreeMap::new(),
        );
        let cfg: UserConfig = serde_json::from_value(reassert_config("node-a")).unwrap();
        let my_gen = rt.gate.generation();

        let outcome = rt.reassert_selector_selection(&cfg, my_gen).await;

        match outcome.stage1 {
            Stage1Outcome::UnresolvedTag { selected_id } => assert_eq!(selected_id, "node-a"),
            _ => panic!("tag 解析不出必须留下 UnresolvedTag 终局（此前只有一行 log::warn）"),
        }
        assert!(
            sink.calls().is_empty(),
            "从未解析出 tag → 一次 PUT 都不该发"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **接线门**：阶段 2 尝试过的 rule-sel 意图必须原样带回终局 —— 否则读回来也没东西可对账。
    ///
    /// **变异锁**：把 `reassert_rule_selectors` 的 `intents.extend(...)` 改回丢弃返回值 → 转红。
    #[tokio::test]
    async fn reassert_outcome_carries_rule_intents_for_readback() {
        let mut cfg = reassert_config("node-a");
        cfg["customRules"] = serde_json::json!([
            { "id": "r1", "type": "domain", "values": ["x.com"], "action": "proxy",
              "enabled": true, "targetServerId": "node-b" }
        ]);
        let rule_target = BTreeMap::from([(
            "custom:r1".to_string(),
            RuleTargetEntry {
                selector_tag: "rule-sel-r1 (1)".into(),
                member_tag: "B".into(),
            },
        )]);
        let (rt, dir, _sink, _events) =
            reassert_runtime_watching_errors(&cfg, ab_tags(), rule_target);
        let uc: UserConfig = serde_json::from_value(cfg).unwrap();
        let my_gen = rt.gate.generation();

        let outcome = rt.reassert_selector_selection(&uc, my_gen).await;

        assert_eq!(
            outcome.rule_intents,
            vec![("rule-sel-r1 (1)".to_string(), "B".to_string())],
            "rule-sel 的意图必须带回（含撞名去重后缀），否则读回对账无从下手"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **接线门（行为型）**：`spawn_reassert_selector_selection` 必须在校正之后真跑一次自证，
    /// 且自证**排在续延之后**（续延不为一次只读观测多等一个 gRPC 往返，最坏 3s 快照超时）。
    ///
    /// 判据：**告警发出的那一刻**续延（解锁失效）已经跑过 —— 只看「两件事都发生了」验不出顺序，
    /// 而后台腿里两者可能只隔微秒，轮询采样必然 flaky。故在 `emit_proxy_error` 里给续延拍照
    /// （`error_seen_invalidations`），断言拍到的是 1。
    ///
    /// **变异锁**：① 删 `spawn_reassert_selector_selection` 里的 `me.attest_runtime_selector(...)` →
    /// 零告警 → 转红；② 把内层作用域去掉（守卫活到 task 末尾，续延晚于自证）→ 拍到 0 → 转红。
    #[tokio::test]
    async fn spawn_runs_attestation_after_continuation() {
        let (rt, dir) = test_runtime();
        let events: ErrorEvents = Arc::new(Mutex::new(Vec::new()));
        let inval: UnlockInvalidations = Arc::new(Mutex::new(Vec::new()));
        let seen: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            events: Arc::clone(&events),
            unlock_invalidations: Arc::clone(&inval),
            error_seen_invalidations: Arc::clone(&seen),
            ..Default::default()
        }));
        let sink = Arc::new(TestPutSink::default());
        // PUT 成功（默认），但运行期快照分叉 → 自证必报。
        *sink.groups.lock().unwrap() = Some(vec![group(PROXY_SELECTOR_TAG, "Tailscale")]);
        *rt.management_api_stub.lock().unwrap() = Some(Arc::clone(&sink));
        mark_running(&rt);
        let cfg = reassert_config("node-a");
        let uc: UserConfig = serde_json::from_value(cfg.clone()).unwrap();
        *rt.switch_snapshot.write().unwrap() = Some(SwitchSnapshot {
            id_to_tag: ab_tags(),
            fingerprints: node_fingerprints::modified_table(&uc.servers),
            dirty_fingerprints: node_fingerprints::dirty_table(&uc.servers),
            ..Default::default()
        });
        *rt.current_config.write().unwrap() = Some(cfg);
        let my_gen = rt.gate.generation();

        rt.spawn_reassert_selector_selection(uc, my_gen, 0);
        for _ in 0..100 {
            if !events.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let got = events.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "spawn 腿必须真跑一次运行期自证，实得 {got:?}");
        assert_eq!(got[0].1, code::EXIT_MISMATCH);
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[1],
            "告警那一刻续延必须已经恰好跑过一次（自证不得挡在三条续延前面）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A3 relay 组合面门：一帧全量端点快照 → 缓存整体更新（幽灵过滤）+ 逐在册端点 `emit_tailscale_status`。
    /// 打断 emit 循环 → 记录空转红；打断解码幽灵过滤 → len 转红；打断 `update_ts_status` → 缓存空转红。
    #[tokio::test]
    async fn ts_status_frame_updates_cache_and_emits_per_registered_endpoint() {
        use polaris_singbox_grpc::daemon;
        let (rt, dir) = test_runtime();
        let ts_events: TsStatusEvents = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            ts_status: Arc::clone(&ts_events),
            ..Default::default()
        }));
        let tag_to_id = BTreeMap::from([("东京 03".to_string(), "srv-tokyo".to_string())]);
        let update = daemon::TailscaleStatusUpdate {
            endpoints: vec![
                daemon::TailscaleEndpointStatus {
                    endpoint_tag: "东京 03".into(),
                    backend_state: "Running".into(),
                    self_: Some(daemon::TailscalePeer {
                        host_name: "self".into(),
                        tailscale_i_ps: vec!["100.64.0.9".into()],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                // 幽灵端点（tag 不在册）→ 既不进缓存也不 emit。
                daemon::TailscaleEndpointStatus {
                    endpoint_tag: "幽灵".into(),
                    backend_state: "Running".into(),
                    ..Default::default()
                },
            ],
        };
        rt.apply_ts_status_frame(&update, &tag_to_id, rt.gate.generation());

        // 缓存：只留在册端点，可经 tailscale_status_snapshot 读回（非恒空）。
        let snap = rt.mesh.tailscale_status_snapshot(true);
        assert_eq!(snap.statuses.len(), 1, "幽灵端点不进缓存");
        assert_eq!(snap.statuses[0].server_id, "srv-tokyo");
        assert!(snap.statuses[0].logged_in);

        // emit：逐在册端点各一条（幽灵不发）。
        let emitted = ts_events.lock().unwrap();
        assert_eq!(emitted.len(), 1, "逐在册端点发一条（幽灵端点不发）");
        assert_eq!(emitted[0].server_id, "srv-tokyo");
        drop(emitted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔵 **上游 触发点④「TS 隧道就绪」纯谓词**：只认**上升沿**（非 Running → Running）。
    ///
    /// **变异锁**（逐条覆盖逃逸面，不是碰巧杀一条）：
    /// - 去掉 `before != Some("Running")`（改成只看 after）→ 稳态 Running 帧也触发 → 第 2 条转红，
    ///   而那正是「纯事件驱动」退化成每秒一次轮询的形态；
    /// - 去掉 `after == Some("Running")`（改成只看 before 变了）→ 第 3/4 条转红；
    /// - 把 `Running` 写成别的状态串 → 第 1 条转红。
    #[test]
    fn ts_exit_ready_fires_only_on_the_rising_edge() {
        // ① 登录完成 / 首帧即就绪 → 触发（出口此刻才真正换成 TS 出口）。
        assert!(ts_exit_became_ready(Some("NeedsLogin"), Some("Running")));
        assert!(ts_exit_became_ready(Some("Starting"), Some("Running")));
        assert!(
            ts_exit_became_ready(None, Some("Running")),
            "首帧即 Running 同样是「此刻起经 TS 出口走」——起核腿那次重探跑在隧道未通时，正需本点纠正"
        );
        // ② 稳态 Running：relay 每秒量级推帧，若也触发 = 每秒重探一次出口 IP（轮询退化）。
        assert!(
            !ts_exit_became_ready(Some("Running"), Some("Running")),
            "稳态帧绝不能触发，否则纯事件驱动退化成轮询"
        );
        // ③ 隧道未就绪 / 掉线：不触发（掉线由停核腿与解锁 gating 各自负责，非本触发点射程）。
        assert!(!ts_exit_became_ready(Some("Running"), Some("NeedsLogin")));
        assert!(!ts_exit_became_ready(
            Some("NeedsLogin"),
            Some("NeedsLogin")
        ));
        assert!(
            !ts_exit_became_ready(None, None),
            "选中的不是 TS 节点 / 首帧未到"
        );
        assert!(!ts_exit_became_ready(None, Some("Starting")));
    }

    /// 🔴 **停流自愈的两个判据**（2026-08-02 真机：首帧 `NoState` 之后再无第二帧，TS 早已就绪却
    /// 一直被当成「尚未登录」，测速被挡、出口卡显示 `—`）。
    ///
    /// **变异锁**：
    /// - 去掉 `!states.is_empty()`（只留 `all`）→ 第 1 条转红。空集上 `all` 恒真 ⇒ 一帧都没收到时
    ///   被判成「全就绪」⇒ 自愈在最该触发的那一刻恰好不触发，这正是本条存在的全部理由；
    /// - 把 `all` 写成 `any` → 第 4 条转红（一个端点就绪就不再自愈另一个卡住的）。
    #[test]
    fn ts_resubscribe_only_when_not_all_endpoints_ready() {
        let m = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect()
        };
        assert!(
            !ts_all_running(&m(&[])),
            "一帧都没收到 = 最该重订阅，绝不能因空集被判成全就绪"
        );
        assert!(!ts_all_running(&m(&[("a", "NoState")])));
        assert!(ts_all_running(&m(&[("a", "Running")])), "稳态不该 churn");
        assert!(
            !ts_all_running(&m(&[("a", "Running"), ("b", "NeedsLogin")])),
            "有端点没就绪就还要自愈"
        );
        assert!(ts_all_running(&m(&[("a", "Running"), ("b", "Running")])));
    }

    /// 🔴 **跃迁日志只在真变了时落**（稳态每秒一帧全打 = 刷屏，与本批治理的 switchMode/dns-race 同病），
    /// 且**幽灵端点不入日志**（tag 不在册 ⇒ UI 上根本没这个节点，打出来比不打更误导）。
    ///
    /// 断言的是**末态表**而非日志文本（`log` 宏在单测里无 sink 可断言）：末态表既是跃迁判据的载体，
    /// 也是停流自愈 `ts_all_running` 的输入 —— 它错了两个功能一起错。
    ///
    /// **变异锁**：删掉「相同即 continue」那一句 → 末态表仍对，但第 3 段的 `<无帧>` 语义丢失
    /// （`insert` 返回值会变成上一次的同值）；删掉幽灵过滤 → 第 2 条断言转红。
    #[test]
    fn ts_transition_log_records_only_registered_endpoints_and_real_changes() {
        let tag_to_id = BTreeMap::from([("mesh-01".to_string(), "srv-ts".to_string())]);
        use polaris_singbox_grpc::daemon as dm;
        let frame = |state: &str, ips: Vec<String>| dm::TailscaleStatusUpdate {
            endpoints: vec![
                dm::TailscaleEndpointStatus {
                    endpoint_tag: "mesh-01".into(),
                    backend_state: state.into(),
                    self_: Some(dm::TailscalePeer {
                        tailscale_i_ps: ips,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                dm::TailscaleEndpointStatus {
                    endpoint_tag: "幽灵".into(),
                    backend_state: "Running".into(),
                    ..Default::default()
                },
            ],
        };
        let mut last = BTreeMap::new();

        log_ts_state_transitions(&frame("NoState", vec![]), &tag_to_id, &mut last);
        assert_eq!(last.get("srv-ts").map(String::as_str), Some("NoState"));
        assert!(
            !last.contains_key("幽灵") && last.len() == 1,
            "幽灵端点（tag 不在册）不得进末态表——否则 ts_all_running 会被一个 UI 上不存在的节点左右"
        );

        // 稳态重复帧：末态不变（也不该打日志）。
        log_ts_state_transitions(&frame("NoState", vec![]), &tag_to_id, &mut last);
        assert_eq!(last.get("srv-ts").map(String::as_str), Some("NoState"));

        // 真跃迁：末态跟上，且此刻 tailnet IP 已有 ⇒ 自愈判据翻成「全就绪」。
        log_ts_state_transitions(
            &frame("Running", vec!["100.64.0.9".into()]),
            &tag_to_id,
            &mut last,
        );
        assert_eq!(last.get("srv-ts").map(String::as_str), Some("Running"));
        assert!(ts_all_running(&last));
    }

    /// 🔵 **触发点④的组合面门**：一帧把选中 TS 出口带到 `Running` ⇒ `apply_ts_status_frame` 必须同时
    /// 失效解锁缓存并排程出口 IP 重探；紧接着的稳态 Running 帧**一次都不许**再触发。
    ///
    /// # 这条补的是什么洞
    ///
    /// §10.1 的 上游 触发表含「TS 隧道就绪」，而 Polaris 侧原先只接了广播半边
    /// （`emit_tailscale_status`）—— mesh 出口就绪同样换掉出口 IP，漏掉它就是那句「只移植了广播半边」
    /// 的同款形态。且 `exit_ip_wiring_guard` 的配对扫描对它**天然失明**（它压根不在命中的三个点里）。
    ///
    /// 帧④⑤（选中端点从帧里消失 → 再带 Running 回来）是第三轮复审登记的**覆盖缺口**补测：
    /// 它钉住「`after=None` 不算就绪」与「消失后回来算新的上升沿」这一对语义。
    ///
    /// **变异锁**：删掉 `apply_ts_status_frame` 里那对调用 → 两处记录皆空 → 转红；
    /// 只删其中一条 → 对应那条转红；把上升沿判据改成「看当前值」→ 第二帧后计数变 2 → 转红；
    /// 把 `after == Some("Running")` 放宽成「after 非空」→ 帧④（endpoints 为空）语义不变，
    /// 但帧①（NeedsLogin）即触发 → 转红。
    #[tokio::test]
    async fn ts_tunnel_ready_invalidates_unlock_and_refreshes_exit_ip_once() {
        use polaris_singbox_grpc::daemon;
        let (rt, dir) = test_runtime();
        let inval: UnlockInvalidations = Arc::new(Mutex::new(Vec::new()));
        let refreshes: ExitIpRefreshes = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            unlock_invalidations: Arc::clone(&inval),
            exit_ip_refreshes: Arc::clone(&refreshes),
            ..Default::default()
        }));
        // 选中出口 = 那个 TS 节点（触发点只关心**选中**出口：别的端点就绪不换我的出口 IP）。
        *rt.current_config.write().unwrap() =
            Some(serde_json::json!({ "selectedServerId": "srv-ts" }));
        let tag_to_id = BTreeMap::from([("mesh-01".to_string(), "srv-ts".to_string())]);
        let frame = |state: &str| daemon::TailscaleStatusUpdate {
            endpoints: vec![daemon::TailscaleEndpointStatus {
                endpoint_tag: "mesh-01".into(),
                backend_state: state.into(),
                ..Default::default()
            }],
        };

        // 帧①登录中 → 未就绪，不触发。
        rt.apply_ts_status_frame(&frame("NeedsLogin"), &tag_to_id, rt.gate.generation());
        assert!(
            refreshes.lock().unwrap().is_empty(),
            "隧道未就绪就重探 = 探到让位期的直连出口，把它当成 TS 出口显示"
        );

        // 帧②跃迁 Running → 隧道就绪，出口 IP 换掉 ⇒ 两条腿都必须动。
        rt.apply_ts_status_frame(&frame("Running"), &tag_to_id, rt.gate.generation());
        assert_eq!(
            *refreshes.lock().unwrap(),
            vec![true],
            "TS 隧道就绪须排程出口 IP 重探（running=true ⇒ 等 4s 选路收敛）"
        );
        assert_eq!(
            *inval.lock().unwrap(),
            vec![(true, false)],
            "新出口上线 ⇒ 解锁快照作废，与起核/热切/停核三点同语义"
        );

        // 帧③稳态 Running → 一次都不许再触发（relay 每秒量级推帧）。
        rt.apply_ts_status_frame(&frame("Running"), &tag_to_id, rt.gate.generation());
        rt.apply_ts_status_frame(&frame("Running"), &tag_to_id, rt.gate.generation());
        assert_eq!(
            refreshes.lock().unwrap().len(),
            1,
            "稳态帧重复触发 ⇒ 出口 IP 重探退化成每秒一次的轮询（本子系统的设计前提是无轮询）"
        );
        assert_eq!(
            inval.lock().unwrap().len(),
            1,
            "同上：解锁检测也会被每秒作废一次"
        );

        // 帧④选中端点**从帧里消失**（relay 重连后的首帧可能不含它 / 该端点被摘）：
        // `after = None` ⇒ 不触发，但边沿状态也就此复位。这一形态原先组合测未覆盖（第三轮复审登记的
        // 覆盖缺口），补在这里是因为它决定了帧⑤的语义 —— 而帧⑤才是真正需要钉死的那一条。
        rt.apply_ts_status_frame(
            &daemon::TailscaleStatusUpdate { endpoints: vec![] },
            &tag_to_id,
            rt.gate.generation(),
        );
        assert_eq!(
            refreshes.lock().unwrap().len(),
            1,
            "端点消失（after=None）不是「就绪」，不得触发重探"
        );

        // 帧⑤端点带着 Running 回来 ⇒ **重新触发**（`ts_exit_became_ready(None, Some(\"Running\"))`）。
        // 这是**有意**的：中间那一帧意味着 relay 眼里这条隧道确实不在了，回来即「此刻起经 TS 出口走」，
        // 与首帧即 Running 同性质。它不构成轮询——复位需要一次真正的「端点消失」帧，稳态 Running 帧
        // （帧③）一次都不会复位。
        rt.apply_ts_status_frame(&frame("Running"), &tag_to_id, rt.gate.generation());
        assert_eq!(
            *refreshes.lock().unwrap(),
            vec![true, true],
            "端点消失后再回到 Running = 新的上升沿，须重探（出口在这期间确实换过）"
        );
        assert_eq!(inval.lock().unwrap().len(), 2, "同上：解锁快照同样须作废");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `ts_tag_to_id` = `build_id_to_tag_map` 的逆（tag→id）。打断（tuple 反了 → id→tag）→ 查 tag 取不到 → 转红。
    #[test]
    fn ts_tag_to_id_inverts_id_to_tag_map() {
        use polaris_config_engine::user_config::server_config::{Protocol, ServerConfig};
        let mut cfg = UserConfig::default();
        cfg.servers.push(ServerConfig {
            id: "id-a".into(),
            name: "东京 03".into(),
            protocol: Protocol::Tailscale,
            ..Default::default()
        });
        let map = ProxyRuntime::ts_tag_to_id(&cfg);
        assert_eq!(
            map.get("东京 03").map(String::as_str),
            Some("id-a"),
            "endpointTag → serverId 逆映射"
        );
    }

    // ══════════════ R2：TS 出口无效直判翻转对账 + 出口恢复腿 ══════════════

    /// 装 mock emitter，同时暴露「解锁失效 / 出口 IP 重探 / 出口无效终态」三类记录句柄
    /// —— R2 的每条腿都要同时看这三者（只看一条会漏掉「失效了但没落终态」这类半接线）。
    fn test_runtime_r2() -> (
        Arc<ProxyRuntime>,
        PathBuf,
        UnlockInvalidations,
        ExitIpRefreshes,
        ExitBlockedMarks,
    ) {
        let (rt, dir) = test_runtime();
        let inval: UnlockInvalidations = Arc::new(Mutex::new(Vec::new()));
        let refreshes: ExitIpRefreshes = Arc::new(Mutex::new(Vec::new()));
        let marks: ExitBlockedMarks = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            unlock_invalidations: Arc::clone(&inval),
            exit_ip_refreshes: Arc::clone(&refreshes),
            exit_blocked_marks: Arc::clone(&marks),
            ..Default::default()
        }));
        (rt, dir, inval, refreshes, marks)
    }

    /// 选中 TS 出口的**可落盘**配置（`exit_node` 为 None ⇒ NoExitDevice；给值则按 peers 判 offline/未广告）。
    ///
    /// 基于 `polaris_store::default_config()` 增量覆盖（同 `two_node_config_ports` 的既定手法）——
    /// `save_full` 会跑校验（`tunConfig` 等必填），裸 json 字面量过不了。
    /// 安全硬约束：`proxyModeType` 恒 `manual`（本组测试全程不起核，但绝不在配置里留 tun/systemProxy）。
    fn ts_exit_config(exit_node: Option<&str>) -> Value {
        let mut ts = serde_json::json!({});
        if let Some(e) = exit_node {
            ts["exitNode"] = Value::String(e.to_string());
        }
        let mut cfg = polaris_store::default_config();
        let obj = cfg.as_object_mut().unwrap();
        obj.insert(
            "servers".into(),
            serde_json::json!([{
                "id": "ts1", "name": "组网出口", "protocol": "tailscale",
                "address": "100.64.0.5", "port": 0,
                "tailscaleSettings": ts
            }]),
        );
        obj.insert("selectedServerId".into(), serde_json::json!("ts1"));
        obj.insert("proxyMode".into(), serde_json::json!("smart"));
        obj.insert("proxyModeType".into(), serde_json::json!("manual"));
        cfg
    }

    /// 让 `mesh.ts_status_event("ts1")` 有一帧（`logged_in` 是 `derive_ts_exit_warning` 的必要前置）。
    fn seed_ts_frame(
        rt: &Arc<ProxyRuntime>,
        peers: Vec<crate::runtime::tailscale_status::TailscaleStatusPeer>,
    ) {
        rt.mesh.update_ts_status(vec![TailscaleStatusEvent {
            server_id: "ts1".into(),
            backend_state: "Running".into(),
            logged_in: true,
            auth_url: None,
            tailscale_ips: vec!["100.64.0.9".into()],
            expired: false,
            peers,
            // Taildrop 四位在本用例无关，取「无能力、无文件」的中性值；不给 Default 是刻意的：
            // 日后再加字段时，这些构造点必须重新被人看一眼，而不是被 `..Default::default()` 静默补齐。
            can_share_files: false,
            waiting_file_count: 0,
            receiving_file_count: 0,
            unread_file_count: 0,
        }]);
    }

    fn ts_peer(
        host: &str,
        ip: &str,
        online: bool,
        advertises: bool,
    ) -> crate::runtime::tailscale_status::TailscaleStatusPeer {
        crate::runtime::tailscale_status::TailscaleStatusPeer {
            host_name: host.into(),
            ip: ip.into(),
            online,
            exit_node: false,
            exit_node_option: advertises,
            active: false,
            stable_id: Some("sid-x".into()),
        }
    }

    /// `TsExitWarning` → 前端 `ProxyExitBlock` 值域的**逐条**投影（四个字符串是跨层契约，拼错 = 前端读不到）。
    ///
    /// **变异锁**：任一分支改串 / 合并两个分支 / 把 `None` 也映成某个原因 → 对应断言转红。
    /// 这四个值必须与 `ui/src/contracts/types/runtime.ts` 的 `ProxyExitBlock` 联合类型逐字一致。
    #[test]
    fn ts_exit_block_reason_projects_the_frontend_contract_values() {
        assert_eq!(
            ProxyRuntime::ts_exit_block_reason(TsExitWarning::None),
            None
        );
        assert_eq!(
            ProxyRuntime::ts_exit_block_reason(TsExitWarning::NeedsAuth),
            Some("ts-needs-auth")
        );
        assert_eq!(
            ProxyRuntime::ts_exit_block_reason(TsExitWarning::NoExitDevice),
            Some("ts-no-exit-device")
        );
        assert_eq!(
            ProxyRuntime::ts_exit_block_reason(TsExitWarning::ExitDeviceOffline),
            Some("ts-exit-device-offline")
        );
        assert_eq!(
            ProxyRuntime::ts_exit_block_reason(TsExitWarning::ExitDeviceNotAdvertised),
            Some("ts-exit-not-advertised")
        );
    }

    /// **廉价前置的等价性**：STATUS 缓存空 ⇒ 判定恒 `None`，**与配置内容无关**。
    ///
    /// 前置存在的理由是省掉每帧一次整份配置深拷贝；它的正确性靠的是
    /// 「无帧 ⇒ `logged_in=false` ⇒ [`derive_ts_exit_warning`] 第一道守卫返 None」这条链。本测用一份
    /// **必然会判无效**的配置（选中 TS 出口 + 无 `exitNode` ⇒ NoExitDevice）压住它：只要前置被写成
    /// 「跳过时返回别的东西」或链条断了（如把 `logged_in` 默认成 true），本测立刻转红。
    ///
    /// **变异锁**：把 `has_ts_status` 的空判反向（空 → true 继续走）→ 判定变成 `Some(...)` → 转红；
    /// 把前置整个删掉 → 本测仍绿（前置只是省功），但 `ts_exit_none_to_blocked_*` 那条仍守着行为——
    /// 这正是设计意图：前置是优化，不是语义。
    #[tokio::test]
    async fn exit_block_is_none_when_status_cache_empty() {
        let (rt, dir) = test_runtime();
        rt.config
            .save_full(&ts_exit_config(None))
            .expect("save cfg");
        assert!(
            rt.selected_ts_exit_block().is_none(),
            "无任何 TS STATUS 帧 ⇒ 判定恒 None（廉价前置与全量判定必须同结论）"
        );
        // 补一帧后，同一份配置立刻判无效 —— 证明上面的 None 来自「无帧」而非「判定坏了」。
        seed_ts_frame(&rt, vec![]);
        assert_eq!(
            rt.selected_ts_exit_block(),
            Some("ts-no-exit-device"),
            "有帧后同一配置必须判无效，否则上面那条 None 是假绿"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **逐字段投影 ≡ 整份 `UserConfig` 反序列化**（NIT：每帧一次 200 节点级 typed 反序列化）。
    ///
    /// `selected_ts_exit_block` 不再 `from_value::<UserConfig>(整份)`，改为只取
    /// `selectedServerId` / 被选中的那**一个** server / `proxyMode` 三项。等价性不能靠肉眼读 ——
    /// 本测把同一份配置**双路**跑：投影路（真方法）vs typed 路（原样重建 `TsExitWarningInput`），
    /// 逐格对拍谓词结论。
    ///
    /// 覆盖矩阵（每格都能单独打死一种投影写法）：
    /// - `proxyMode=direct` ⇒ 恒 None（投影若把 `proxyMode` 取错键/大小写敏感反了 → 两路分叉）；
    /// - 选中项 = 撞在**后面**的那个 server（投影若按下标 0 取 / 忘了按 id 匹配 → 拿到错的节点）；
    /// - 选中 TS 无 `exitNode` ⇒ `ts-no-exit-device`（投影若把整个 server 丢了 → 变 None）。
    ///
    /// **变异锁**：把投影的 `find(id == sel_id)` 换成 `first()` → 第二格转红；把 `proxyMode` 比对写成
    /// `== Some("Direct")` → 第一格转红；把 `selected` 恒置 None → 第三格转红。
    #[tokio::test]
    async fn selected_ts_exit_block_projection_matches_typed_parse() {
        let (rt, dir) = test_runtime();
        seed_ts_frame(&rt, vec![]); // logged_in=true、peers 空 → 走到 exitNode 那道判据

        // 选中项刻意排在**第二位**，且前面放一个同为 TS、但配了 exitNode 的干扰项。
        let mut cfg = ts_exit_config(None);
        let obj = cfg.as_object_mut().unwrap();
        obj.insert(
            "servers".into(),
            serde_json::json!([
                { "id": "decoy", "name": "干扰", "protocol": "tailscale", "address": "100.64.0.9",
                  "port": 0, "tailscaleSettings": { "exitNode": "100.64.0.9" } },
                { "id": "ts1", "name": "组网出口", "protocol": "tailscale", "address": "100.64.0.5",
                  "port": 0, "tailscaleSettings": {} },
            ]),
        );

        for mode in ["smart", "direct"] {
            cfg["proxyMode"] = serde_json::json!(mode);
            rt.config.save_full(&cfg).expect("save cfg");
            // typed 路：原样重建（这正是被替换掉的那段实现）。
            let typed: UserConfig = serde_json::from_value(cfg.clone()).expect("typed 解析");
            let sel_id = typed.selected_server_id.as_deref().expect("有选中项");
            let event = rt.mesh.ts_status_event(sel_id);
            let (logged_in, peers, definitive_logged_out) =
                event.as_ref().map_or((false, &[][..], false), |e| {
                    (e.logged_in, e.peers.as_slice(), is_definitive_logged_out(e))
                });
            let expected =
                ProxyRuntime::ts_exit_block_reason(derive_ts_exit_warning(&TsExitWarningInput {
                    selected: typed.servers.iter().find(|s| s.id == sel_id),
                    logged_in,
                    proxy_mode_direct: typed.proxy_mode == ProxyMode::Direct,
                    proxy_running: rt.status().running,
                    peers,
                    definitive_logged_out,
                }));
            assert_eq!(
                rt.selected_ts_exit_block(),
                expected,
                "proxyMode={mode}：逐字段投影与整份反序列化必须同结论"
            );
            // 反证：矩阵里至少有一格是**非 None**，否则整条对拍可能只是「两边都恒 None」。
            if mode == "smart" {
                assert_eq!(
                    expected,
                    Some("ts-no-exit-device"),
                    "前置：选中的 ts1 未配 exitNode ⇒ 必判无效（若这里是 None，本测退化成空对拍）"
                );
            } else {
                assert_eq!(expected, None, "direct ⇒ 方向反转不适用");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **R2 `none → blocked`**：出口 IP **不探测**直落终态 + 解锁快照失效并带 `exit_blocked=true`。
    ///
    /// 这条钉住的是三件事，缺一不可：
    /// 1. 走的是 `mark_exit_blocked` 而**不是** `schedule_exit_ip_refresh` —— 排探测在已知无效的出口上
    ///    必然打空转（20s 重试预算耗尽后仍是 null），用户看到「一直在检测」；
    /// 2. `exit_blocked=true` 真的传下去了 —— 这是该参数**唯一**的生产真值来源（其余三个触发点恒 false），
    ///    渲染端据此复位 idle 而非留着陈旧绿点；
    /// 3. 原因串是契约值域里的那一个（NoExitDevice → `ts-no-exit-device`）。
    ///
    /// **变异锁**：删 `mark_exit_blocked` 调用 → marks 空转红；把它换成 `schedule_exit_ip_refresh` →
    /// refreshes 非空 + marks 空、两处同时转红；`exit_blocked` 写死 false → 第 2 条转红；
    /// 把跨态判据改成 level（每帧都动作）→ 下面的「同态零动作」测转红。
    #[tokio::test]
    async fn ts_exit_none_to_blocked_marks_terminal_state_and_invalidates_with_flag() {
        let (rt, dir, inval, refreshes, marks) = test_runtime_r2();
        rt.config
            .save_full(&ts_exit_config(None))
            .expect("save cfg");
        seed_ts_frame(&rt, vec![]);

        rt.reconcile_ts_exit_block(rt.gate.generation());

        assert_eq!(
            *marks.lock().unwrap(),
            vec!["ts-no-exit-device".to_string()],
            "出口已知无效 ⇒ 无探测直落终态（探了必然空转 20s 预算再吐 null）"
        );
        assert_eq!(
            *inval.lock().unwrap(),
            vec![(false, true)],
            "跨态即令解锁快照失效，且 exit_blocked=true 必须真传下去（该参数唯一的生产真值来源）"
        );
        assert!(
            refreshes.lock().unwrap().is_empty(),
            "blocked 态绝不能排真探测：那正是「一直在检测」的成因"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **R2 同态零动作**：`blocked → blocked`（同原因）一次都不许再动作。
    ///
    /// STATUS relay 每秒量级推帧，level 触发 = 每秒一次解锁失效 + 每秒一次终态广播
    /// （与 [`ts_exit_became_ready`] 挡住的是同一种轮询退化）。
    ///
    /// **变异锁**：删掉 `if *g == cur { return; }` 早退 → 第二次调用后计数变 2 → 转红。
    #[tokio::test]
    async fn ts_exit_same_state_frames_never_re_fire() {
        let (rt, dir, inval, _refreshes, marks) = test_runtime_r2();
        rt.config
            .save_full(&ts_exit_config(None))
            .expect("save cfg");
        seed_ts_frame(&rt, vec![]);

        rt.reconcile_ts_exit_block(rt.gate.generation());
        rt.reconcile_ts_exit_block(rt.gate.generation());
        rt.reconcile_ts_exit_block(rt.gate.generation());

        assert_eq!(marks.lock().unwrap().len(), 1, "同态帧不得重复落终态");
        assert_eq!(inval.lock().unwrap().len(), 1, "同态帧不得重复失效解锁");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **R2 原因变更 `blocked → blocked'`** 仍算跨态：终态原因要更新（离线 → 未广告是两种不同的用户指引）。
    ///
    /// **变异锁**：把跨态判据从「值不等」改成「有无 block 的布尔不等」→ 第二次不触发 → 转红。
    #[tokio::test]
    async fn ts_exit_reason_change_is_a_transition_too() {
        let (rt, dir, _inval, _refreshes, marks) = test_runtime_r2();
        rt.config
            .save_full(&ts_exit_config(Some("exit-host")))
            .expect("save cfg");
        *rt.status.write().unwrap() = ProxyStatus {
            running: true,
            ..Default::default()
        };
        // ① exit peer 离线
        seed_ts_frame(&rt, vec![ts_peer("exit-host", "100.64.0.5", false, true)]);
        rt.reconcile_ts_exit_block(rt.gate.generation());
        // ② 同一 peer 上线但未广告出口 → 原因变了
        seed_ts_frame(&rt, vec![ts_peer("exit-host", "100.64.0.5", true, false)]);
        rt.reconcile_ts_exit_block(rt.gate.generation());

        assert_eq!(
            *marks.lock().unwrap(),
            vec![
                "ts-exit-device-offline".to_string(),
                "ts-exit-not-advertised".to_string()
            ],
            "原因变更也是跨态：终态原因必须更新，否则用户拿到的是上一个原因的排障指引"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **R2 `blocked → none`**：走**恢复腿**（而非直接重探）+ 解锁快照失效且 `exit_blocked=false`。
    ///
    /// 用「预置单飞在飞」把 spawn 挡在门外，使断言完全确定（否则后台任务与断言竞速）——
    /// 同时这本身就证明了对账腿**真的调到了** `begin_ts_exit_recovery`：pending 只可能由它置位。
    ///
    /// **变异锁**：删 `spawn_ts_exit_recovery` 调用 → pending 保持 false → 转红；
    /// 把恢复腿换成裸 `schedule_exit_ip_refresh` → pending false + refreshes 非空 → 两处转红
    /// （而那正是「re-advertise 后核不重解析 exit_node、只重探等于探了个寂寞」的形态）；
    /// `exit_blocked` 在恢复腿传 true → 断言转红。
    #[tokio::test]
    async fn ts_exit_blocked_to_none_runs_recovery_leg_not_a_bare_reprobe() {
        let (rt, dir, inval, refreshes, marks) = test_runtime_r2();
        rt.config
            .save_full(&ts_exit_config(Some("exit-host")))
            .expect("save cfg");
        *rt.status.write().unwrap() = ProxyStatus {
            running: true,
            ..Default::default()
        };
        // ① 无效（peer 离线）
        seed_ts_frame(&rt, vec![ts_peer("exit-host", "100.64.0.5", false, true)]);
        rt.reconcile_ts_exit_block(rt.gate.generation());
        assert_eq!(marks.lock().unwrap().len(), 1);
        inval.lock().unwrap().clear();

        // ② 恢复有效（peer 上线且广告出口）。预置「恢复腿在飞」→ 本次只记 pending，不 spawn。
        rt.ts_exit_recovering.store(true, Ordering::SeqCst);
        seed_ts_frame(&rt, vec![ts_peer("exit-host", "100.64.0.5", true, true)]);
        rt.reconcile_ts_exit_block(rt.gate.generation());

        assert!(
            rt.ts_exit_recover_pending.load(Ordering::SeqCst),
            "blocked→none 必须触达恢复腿（在飞时记 pending）——只重探不热重设 exit_node = 探了个寂寞"
        );
        assert_eq!(
            *inval.lock().unwrap(),
            vec![(true, false)],
            "出口恢复有效 ⇒ 解锁自动重检，且 exit_blocked 必须翻回 false"
        );
        assert_eq!(marks.lock().unwrap().len(), 1, "恢复态不得再落无效终态");
        assert!(
            refreshes.lock().unwrap().is_empty(),
            "重探由恢复腿在 reapply+reassert 之后收尾，不在对账腿里抢跑"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **R2 恢复腿单飞 + 补跑门**（纯状态机，同步可直测）。
    ///
    /// - 首次抢占成功；
    /// - 在飞期间的后来者一律 `false` 且**记 pending**（边沿触发的腿丢了边沿不会自愈，见字段文档）；
    /// - 令牌归还后可再次抢占。
    ///
    /// 归还口是 [`TsExitRecoverGuard`] 的 Drop（**唯一**归还点，见
    /// [`ProxyRuntime::reset_ts_exit_block_state`] 文档解释为何停核腿不再代为归还）。
    ///
    /// **变异锁**：`swap(true)` 写成 `load()` → 第二次也返 true → 转红；
    /// 删掉 pending 置位 → 第二条断言转红（那正是 flap 期边沿被静默吞掉的形态）；
    /// Drop 里删掉 `recovering` 复位 → 末条转红（此后本会话所有真恢复被单飞永久吞掉）。
    #[tokio::test]
    async fn ts_exit_recovery_single_flight_records_pending_for_late_comers() {
        let (rt, dir) = test_runtime();
        assert!(rt.begin_ts_exit_recovery(), "首次必须抢到");
        assert!(
            !rt.ts_exit_recover_pending.load(Ordering::SeqCst),
            "首次抢到不该置 pending（否则每轮都白补跑一次）"
        );
        assert!(!rt.begin_ts_exit_recovery(), "在飞期间后来者必须被单飞挡下");
        assert!(
            rt.ts_exit_recover_pending.load(Ordering::SeqCst),
            "被挡下的边沿必须记 pending：恢复腿是边沿触发，丢了不会靠下一帧自愈"
        );
        // 持有者退场（核未运行 ⇒ Drop 的补跑门第一条就不成立，不会 spawn 出新腿来干扰断言）。
        drop(TsExitRecoverGuard(Arc::clone(&rt)));
        assert!(rt.begin_ts_exit_recovery(), "令牌归还后可再次抢占");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **丢边沿补救门（#8）**：`pending` 由 Drop 用 `swap` 取走并按当下核状态裁定要不要补跑。
    ///
    /// # 这条窗口 上游 没有、Rust 有
    ///
    /// 上游 `recoverTsExit` 的 `while (this.tsExitRecoverPending && …)` 判定与 `finally` 之间没有插入点
    /// （单线程）。Rust 这里有：循环判 `pending == false` 之后、Drop 执行之前，STATUS relay **另一条线程**
    /// 完全可以跑一次 `begin_ts_exit_recovery` 把 pending 置回 `true`。Drop 若无条件 `store(false)`，
    /// 这条 `blocked→none` 边沿就被**永久**抹掉 —— 恢复腿是边沿触发，同态帧下一轮直接早退，不会自愈。
    ///
    /// **变异锁**：
    /// - `swap(false)` 改回 `load()` → ② 转红（边沿留在位上，下一次 Drop 会重复消费）；
    /// - 删掉 `status().running` 判据 → ③ 转红（核已停时 `selected_ts_exit_block()` 恒 None，会被误读成
    ///   「出口有效」⇒ 对着已停的核重申路由 + 以 `running=true` 语义重探）；
    /// - 删掉 `selected_ts_exit_block().is_none()` 判据 → ④ 转红（flap 回 blocked 还去空跑恢复）。
    #[tokio::test]
    async fn drop_reclaims_the_edge_lost_between_the_loop_check_and_the_guard() {
        let (rt, dir, _inval, _refreshes, _marks) = test_runtime_r2();
        rt.config
            .save_full(&ts_exit_config(Some("exit-host")))
            .expect("save cfg");
        *rt.status.write().unwrap() = ProxyStatus {
            running: true,
            ..Default::default()
        };
        // 出口有效（peer 上线且广告出口）⇒ selected_ts_exit_block() == None。
        seed_ts_frame(&rt, vec![ts_peer("exit-host", "100.64.0.5", true, true)]);
        assert!(rt.selected_ts_exit_block().is_none(), "前置：出口判有效");

        // ① 无边沿 → 不补跑（否则每轮恢复腿都白跑第二遍）。
        assert!(!rt.take_ts_exit_recover_rerun());

        // ② 有边沿 + 核在跑 + 出口仍有效 → 补跑，且边沿被**取走**（不留给下一次 Drop 重复消费）。
        rt.ts_exit_recover_pending.store(true, Ordering::SeqCst);
        assert!(
            rt.take_ts_exit_recover_rerun(),
            "循环判定与 Drop 之间被 relay 记下的边沿必须由 Drop 捡回来补跑"
        );
        assert!(
            !rt.ts_exit_recover_pending.load(Ordering::SeqCst),
            "边沿必须被 swap 取走"
        );

        // ③ 核已停：`selected_ts_exit_block()` 因 STATUS 缓存被清而恒 None —— 只看它就会把「没有核」
        //    误读成「出口有效」，于是拿旧会话的 current_config 去重申出口路由并以 running=true 重探。
        rt.ts_exit_recover_pending.store(true, Ordering::SeqCst);
        *rt.status.write().unwrap() = ProxyStatus::default();
        rt.mesh.clear_ts_status();
        assert!(
            !rt.take_ts_exit_recover_rerun(),
            "核已停 ⇒ 绝不补跑（那会对着已停的核重申路由 + 重探）"
        );

        // ④ 核在跑但出口 flap 回 blocked（peer 离线）→ 不对已知无效的出口空跑。
        *rt.status.write().unwrap() = ProxyStatus {
            running: true,
            ..Default::default()
        };
        seed_ts_frame(&rt, vec![ts_peer("exit-host", "100.64.0.5", false, true)]);
        assert!(rt.selected_ts_exit_block().is_some(), "前置：出口判无效");
        rt.ts_exit_recover_pending.store(true, Ordering::SeqCst);
        assert!(
            !rt.take_ts_exit_recover_rerun(),
            "flap 回 blocked ⇒ 不得对着已知无效的出口空跑恢复"
        );

        // ⑤ Drop 真的接了这道门（行为侧的 spawn 不可确定性观测 ⇒ 判据落在 Drop 体的源码上）。
        let drop_body = method_body(
            include_str!("proxy.rs"),
            "impl Drop for TsExitRecoverGuard {",
        );
        assert!(
            drop_body.contains("take_ts_exit_recover_rerun()")
                && drop_body.contains("spawn_ts_exit_recovery(&self.0)"),
            "Drop 必须「先放单飞位 → 取边沿判定 → 命中则补跑」；少了补跑那一步，被 Drop 窗口丢掉的\
             边沿就永远没人消费"
        );
        assert!(
            drop_body.find("ts_exit_recovering").expect("放单飞位")
                < drop_body
                    .find("take_ts_exit_recover_rerun()")
                    .expect("取边沿"),
            "单飞位必须先放：反过来补跑腿的 begin 会撞上还没放的位 → 边沿又被记回 pending，\
             而此刻已经没有在飞腿会去消费它"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **恢复腿的世代守卫（#2）**：被停核 / 换核 / 新 start 接管的旧腿，三步一步都不许做。
    ///
    /// 这条 `'static` 任务能活过停核（`spawn` 出去、无人 abort），而三步全是「对着**当前**核」的动作。
    /// 可观测末端取 `schedule_exit_ip_refresh`（`running=true` 语义的重探）：旧腿放它出去，会去重探一个
    /// 已死的核，并**后发覆盖** `stop_inner` 那次 `schedule_exit_ip_refresh(false)`。
    ///
    /// 另两条后果本机观测不到（macOS `find_tailnet_iface` 的 18s 轮询 + 真 route 手术是真机门），
    /// 由源码守卫 `ts_exit_recover_once_order_is_reapply_reassert_refresh` 的「三处世代比对」断言锁住。
    ///
    /// **变异锁**：删掉 `ts_exit_recover_once` 的任一处世代比对 → 那条源码断言转红；删掉**收尾**那处
    /// → 本测试也转红。
    #[tokio::test]
    async fn superseded_recovery_leg_must_not_reprobe_a_dead_core() {
        let (rt, dir, _inval, refreshes, _marks) = test_runtime_r2();
        rt.config
            .save_full(&ts_exit_config(Some("exit-host")))
            .expect("save cfg");
        *rt.current_config.write().unwrap() = Some(ts_exit_config(Some("exit-host")));
        let stale = rt.gate.generation();
        rt.bump_generation(); // 停核 / 新 start 接管

        rt.ts_exit_recover_once(stale).await;

        assert!(
            refreshes.lock().unwrap().is_empty(),
            "被接管的旧腿不得以「代理在跑」语义重探：它会对着已死的核探，并后发覆盖停核腿的 refresh(false)"
        );
        // 正向对照：当权者仍必须跑完三步（守卫不得退化成「谁都不跑」）。
        rt.ts_exit_recover_once(rt.gate.generation()).await;
        assert_eq!(
            *refreshes.lock().unwrap(),
            vec![true],
            "当权的恢复腿必须照常以重探收尾"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **对账腿的锁内世代守卫（#7）**：旧会话的在飞帧不得把 `Some(reason)` 写进新会话的缓存。
    ///
    /// relay 在收帧后复查过一次世代，但那之后还要跑完整个 `apply_ts_status_frame`。停核腿是
    /// 「`bump_generation()` → … → `reset_ts_exit_block_state()`」，若对账尾部的缓存写入晚于那次复位，
    /// `last_ts_exit_block` 就带着旧原因漏进新会话 ⇒ 重连后**同因** blocked 的首帧被同态早退吞掉，
    /// 终态永远落不下去（对账是边沿触发，没有轮询会来纠正）。
    ///
    /// **变异锁**：删掉 `reconcile_ts_exit_block` 里那句 `if self.gate.generation() != my_gen`
    /// → ①② 同时转红；把它挪到 `last_ts_exit_block.lock()` 之外（函数入口）→ 语义仍是 check-then-act，
    /// 由 `reconcile_generation_guard_is_inside_the_cache_lock` 的源码判据转红。
    #[tokio::test]
    async fn a_superseded_frame_must_not_poison_the_next_session_reconcile_cache() {
        let (rt, dir, _inval, _refreshes, marks) = test_runtime_r2();
        rt.config
            .save_full(&ts_exit_config(None))
            .expect("save cfg");
        seed_ts_frame(&rt, vec![]);
        let stale = rt.gate.generation();
        // 停核：bump 世代 + 复位会话起点缓存。
        rt.bump_generation();
        rt.reset_ts_exit_block_state();

        // 旧世代的在飞帧此刻才跑到对账尾部。
        rt.reconcile_ts_exit_block(stale);

        assert!(
            marks.lock().unwrap().is_empty(),
            "① 旧会话的帧不得再落终态（核都停了）"
        );
        assert!(
            rt.last_ts_exit_block.lock().unwrap().is_none(),
            "② 残留的 Some(reason) 会让新会话同因 blocked 的首帧被同态早退吞掉 ⇒ 终态永不落"
        );
        // 正向对照：新会话的帧照常落终态。
        rt.reconcile_ts_exit_block(rt.gate.generation());
        assert_eq!(marks.lock().unwrap().len(), 1, "新会话必须能正常落终态");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **#7 的位置判据**：世代比对必须在 `last_ts_exit_block` 的**锁内**。
    ///
    /// 放函数入口是 check-then-act：判完到写缓存之间隔着 `selected_ts_exit_block()`（深拷贝整份配置 +
    /// 反序列化，微秒级但非零），停核腿完全可以在这条缝里跑完 bump + 复位。`reset_ts_exit_block_state`
    /// 持的是**同一把**锁，故把判据放进锁里就等于把「判权 + 写缓存」做成原子的。
    #[test]
    fn reconcile_generation_guard_is_inside_the_cache_lock() {
        let seg = method_body(
            include_str!("proxy.rs"),
            "    fn reconcile_ts_exit_block(self: &Arc<Self>, my_gen: u64) {",
        );
        let lock_at = seg
            .find("self.last_ts_exit_block.lock()")
            .expect("对账缓存锚点消失，守卫已失去判据");
        let guard_at = seg
            .find("if self.gate.generation() != my_gen {")
            .expect("对账腿缺世代守卫：旧会话的在飞帧会把 Some(reason) 写进新会话的缓存");
        let swap_at = seg
            .find("std::mem::replace(&mut *g, cur)")
            .expect("缓存写入锚点消失，守卫已失去判据");
        assert!(
            lock_at < guard_at && guard_at < swap_at,
            "世代比对必须夹在「取锁」与「写缓存」之间（= 锁内）；放函数入口就还是 check-then-act"
        );
    }

    /// **R2 恢复腿单轮**跑完三步后必须以「重探」收尾（顺序的可观测末端）。
    ///
    /// 核未运行 ⇒ `reapply_ts_exit_node` 守卫链在第一道就返 false（零 gRPC、零网络）、
    /// `exit_route_reassert` 在测试构造的 `enabled=false` op 下诚实 no-op（零 `ip`/`route` 进程）——
    /// 本测因此**绝不碰宿主网络**，却仍能证明整轮跑到了尾。
    ///
    /// **变异锁**：删掉末尾的 `schedule_exit_ip_refresh` → 空转红；把它挪到 reapply 之前 → 顺序守卫
    /// （`ts_exit_recover_once_order_is_reapply_reassert_refresh`）转红。
    #[tokio::test]
    async fn ts_exit_recover_once_ends_with_a_reprobe() {
        let (rt, dir, _inval, refreshes, _marks) = test_runtime_r2();
        rt.config
            .save_full(&ts_exit_config(Some("exit-host")))
            .expect("save cfg");
        *rt.current_config.write().unwrap() = Some(ts_exit_config(Some("exit-host")));

        rt.ts_exit_recover_once(rt.gate.generation()).await;

        assert_eq!(
            *refreshes.lock().unwrap(),
            vec![true],
            "恢复腿必须以重探收尾（running=true ⇒ 等 4s 选路收敛）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **`clash_api_secret` 必须投影读，不得整份深拷贝配置。**
    ///
    /// # 为什么必须是源码型守卫
    ///
    /// `current()` 与 `with_current()` 的返回值**逐字节相同** —— 差别只在「谁付整份 clone 的账」
    /// （`runtime/config.rs:181-196` 自陈）。任何行为断言都区分不出这两者，退回深拷贝**没有任何测试
    /// 会红**，而代价是：调用链 `probe_select_slot → hot_switch_selector → management_api → 本方法`
    /// 意味着**测速一轮 = N 次整份用户配置深拷贝**（含全部 `servers` 与规则），且所有热切节点的路径
    /// 都付这笔账。这正是「静默回退无人察觉」的形态，故补本条结构守卫。
    ///
    /// **变异锁**：把 `with_current` 换回 `.current()` → 两条断言全红。
    #[test]
    fn clash_api_secret_projects_instead_of_deep_copying_the_config() {
        let body = method_body(
            include_str!("proxy.rs"),
            "    fn clash_api_secret(&self) -> String {",
        );
        assert!(
            body.contains("with_current"),
            "必须走持锁投影：只取一个字符串字段，不 clone 整份配置"
        );
        assert!(
            !body.contains(".current()"),
            "`current()` 恒 clone 整份用户配置（含全部 servers）—— 热切路径上按 N 次计费"
        );
    }

    /// 截出**单个方法体**的源码文本（源码型守卫的唯一取材口）。
    ///
    /// # 为什么不能直接 `&src[start..]`（本仓踩过的两次假绿）
    ///
    /// 切到 EOF 的 `seg` 会让 `find` 命中**后文其它方法**里的同名调用：从目标方法里删掉那一行，
    /// 顺序 / 接线断言照样绿。判据必须限定在这一个函数体内。
    ///
    /// 边界判据是「行首恰好 4 空格 + `}`」—— `impl` 成员的收尾花括号就在这一列，而方法体内的一切
    /// 嵌套块都 ≥8 空格。比「下一个 `fn `」稳：后者会把中间的 doc 注释一并算进 seg（注释里的示例代码
    /// 就能让守卫误绿）。
    ///
    /// # 为什么还要**剥掉整行注释**（与 `commands/misc.rs::ipinfo_epoch_guard::fn_body` 对称）
    ///
    /// 截出的方法体里仍含**体内注释**，而本模块有 `count() == 3` 这类**计数**断言
    /// （见 [`ts_exit_recover_once_order_is_reapply_reassert_refresh`]）。计数断言对注释是敏感的：
    /// 在方法体里写一行 `// if self.gate.generation() != my_gen {` 就能给计数充数 —— 真删掉一处
    /// 世代守卫，守卫仍绿。位置断言（`find` 比大小）也同理会被注释里的锚点文本带偏。
    /// 当前源码里没有这类命中，但「靠现状没撞上」不是判据 —— 剥掉才是。
    ///
    /// 只剥**整行**注释（`trim_start().starts_with("//")`），与 misc.rs 逐字同款：行尾注释要剥就得
    /// 分辨字符串字面量里的 `//`，那是把守卫的取材器写成半个词法分析器，代价与收益不成比例。
    /// 在 `body` 里断言「含 `first` 的那一行，其**下一非空行**含 `second`」。
    ///
    /// [`method_body`] 已把整行注释替换成空行 ⇒ 「紧邻」允许中间夹注释（说明因果本该写在那里），
    /// 但不允许夹任何别的语句。找不到 `first` 即 **panic**（锚点失配自曝，绝不退化成恒真）。
    fn line_immediately_followed_by(body: &str, first: &str, second: &str) -> bool {
        let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
        let i = lines
            .iter()
            .position(|l| l.contains(first))
            .unwrap_or_else(|| panic!("锚点 `{first}` 在该方法体内消失，配对守卫已失去判据"));
        lines.get(i + 1).is_some_and(|l| l.contains(second))
    }

    fn method_body(src: &str, head: &str) -> String {
        let start = src
            .find(head)
            .unwrap_or_else(|| panic!("锚点 `{head}` 消失，源码型守卫已失去判据"));
        let rest = &src[start + head.len()..];
        let end = rest.find("\n    }\n").map_or(rest.len(), |i| i + 1);
        rest[..end]
            .lines()
            .map(|l| {
                if l.trim_start().starts_with("//") {
                    ""
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// [`method_body`] 自身的门：**截得准** + **剥得掉整行注释**。
    ///
    /// 取材器是本模块所有源码型守卫的共同判据，它坏了则上面每一条都静默失效（且各自的断言仍是绿的）。
    /// 两条属性各对应一种已知假绿：
    /// - 不封顶（切到 EOF）⇒ `find` 命中后文别的方法里的同名调用（本仓踩过两次）；
    /// - 不剥注释 ⇒ 方法体内注释里的锚点文本给 `count()` / `find()` 充数（NIT：与
    ///   `commands/misc.rs::ipinfo_epoch_guard::fn_body` 不对称的那一处，现已对齐）。
    ///
    /// **变异锁**：去掉 `method_body` 的整行注释剥除 → 第二条断言（注释里的锚点不得被数到）转红；
    /// 把封顶判据 `"\n    }\n"` 删掉（切到 EOF）→ 第三条（不得越界到下一个方法）转红。
    #[test]
    fn method_body_is_bounded_and_strips_line_comments() {
        const SRC: &str = "impl X {\n    fn a(&self) {\n        real_call();\n\
                           // real_call() 出现在整行注释里\n        let s = \"x\";\n    }\n\
                           \n    fn b(&self) {\n        real_call();\n    }\n}\n";
        let body = method_body(SRC, "    fn a(&self) {");
        assert_eq!(
            body.matches("real_call()").count(),
            1,
            "整行注释里的锚点文本必须被剥掉（否则 count()==N 类断言可被注释充数）：\n{body}"
        );
        assert!(body.contains("let s = \"x\";"), "非注释行必须原样保留");
        assert!(
            !body.contains("fn b"),
            "射程必须封顶在本方法体（切到 EOF 会命中后文同名调用）"
        );
    }

    /// **R2 恢复腿三步顺序守卫**：`reapply → reassert → refresh`，一步都不许换位。
    ///
    /// 为什么必须守：三步的**顺序本身**就是修复内容 —— re-advertise 后运行中的 sing-box 不随 netmap
    /// 重解析 exit_node（上游 watchState 缺陷），不先热重设就 reassert/重探，探到的还是恢复前的出口。
    /// 而顺序错了行为测试**看不出来**（本机三步都是 no-op / 记录，末端记录一样有）。
    ///
    /// **取材限定在 `ts_exit_recover_once` 的函数体内**（[`method_body`]）：早先的版本把 `seg` 从方法头
    /// 一路切到 EOF，`self.schedule_exit_ip_refresh(true);` 会命中后文其它方法里的同名调用 ⇒ 从恢复腿
    /// 里删掉收尾重探，本断言**仍绿**。
    /// **常驻轮询腿禁整份深拷贝**：这三个方法都由无条件周期循环驱动，必须走
    /// [`ConfigManager::with_current`](crate::runtime::ConfigManager::with_current) 持锁投影，
    /// 不得回退到 `config.current()`（后者恒 clone 整份配置，含 200 节点级 `servers`）。
    ///
    /// # 为什么只能是源码型判据
    ///
    /// 这是**纯性能**改动：`current()` 与 `with_current()` 读的是同一份缓存、结论逐字节相同，故把
    /// 任何一处改回 `current()`，全部行为断言（`selected_ts_exit_block_projection_matches_typed_parse`
    /// / `exit_block_is_none_when_status_cache_empty` / 心跳那几条）**照样全绿** —— 省下的那次深拷贝
    /// 在单测里根本不可观测。没有这条守卫，「热路径不深拷贝」就只是注释里的一句话。
    ///
    /// 三条腿的节奏：`selected_ts_exit_block` = TS STATUS relay **每帧（~1Hz）**；
    /// 另两条 = 自动换节点心跳**每 tick**（`HEARTBEAT_INTERVAL_MS`，核在跑就一直跑）。
    ///
    /// **双向断言**（缺一都能被绕过）：禁 `.current()` 挡住回退；要求 `.with_current(` 挡住
    /// 「把配置读整个删掉」这种让负面断言恒真的改法。
    ///
    /// **变异锁**：任一方法体里把 `.with_current(` 换回 `.current()` ⇒ 逐条转红。
    #[test]
    fn periodic_legs_read_config_by_projection_not_full_clone() {
        let src = include_str!("proxy.rs");
        for head in [
            "    fn selected_ts_exit_block(&self) -> Option<&'static str> {",
            "    fn auto_switch_enabled(&self) -> bool {",
            "    fn selected_server_is_real(&self) -> bool {",
        ] {
            let body = method_body(src, head);
            assert!(
                !body.contains(".current()"),
                "`{head}` 是常驻周期腿，出现了 `config.current()` —— 那是每帧/每 tick 一次整份配置\
                 深拷贝（含 200 节点级 servers）。改用 `with_current(|v| …)` 只投影要用的字段。"
            );
            assert!(
                body.contains(".with_current("),
                "`{head}` 里连 `with_current` 都没有了 —— 负面断言会因此恒真（门被抽空）。\
                 若确实不再读配置，请连同本守卫的这一项一起删掉，而不是留个空壳。"
            );
        }
    }

    #[test]
    fn ts_exit_recover_once_order_is_reapply_reassert_refresh() {
        let seg = method_body(
            include_str!("proxy.rs"),
            "    async fn ts_exit_recover_once(&self, my_gen: u64) {",
        );
        let reapply = seg
            .find("self.reapply_ts_exit_node().await")
            .expect("① 热重设");
        let reassert = seg
            .find("self.mesh.exit_route_reassert(&cfg, ipv6).await")
            .expect("② 重申出口路由");
        let refresh = seg
            .find("self.schedule_exit_ip_refresh(true);")
            .expect("③ 重探");
        assert!(
            reapply < reassert && reassert < refresh,
            "恢复腿必须按 reapply → reassert → refresh 排列：先修核内 exit_node 与 System 路由，最后才探——\
             顺序换了就是「对着恢复前的出口重探」，与不修一样"
        );
        // 世代守卫（#2）也归本段守：三步之间隔着 gRPC 往返与最长 18s 的路由手术，只在入口判一次等于没判。
        assert_eq!(
            seg.matches("if self.gate.generation() != my_gen {").count(),
            3,
            "恢复腿必须在**每步之前**比对世代（3 处）：少一处就漏掉「停核卡 18s / 旧配置重装路由 / \
             对死核重探」三条后果里的一条"
        );
    }

    /// **R1 热重设的守卫链**：核未运行 → 一律跳过（返 false），**零 gRPC 连接**（本机零网络的前提）。
    ///
    /// **变异锁**：删掉 `!status.running` 守卫 → 本测会尝试连 127.0.0.1:0 → 仍返 false 但耗时/日志变化；
    /// 故同时断言 `clash_api_port == 0` 这条：把端口守卫删掉 → `Endpoint::new("127.0.0.1", 0)` 建连
    /// 路径被真的走到（连接必失败，返回值不变但语义已错）。两条守卫都由本测覆盖其**存在性**。
    #[tokio::test]
    async fn reapply_ts_exit_node_short_circuits_when_core_not_running() {
        let (rt, dir) = test_runtime();
        *rt.current_config.write().unwrap() = Some(ts_exit_config(Some("exit-host")));
        seed_ts_frame(&rt, vec![ts_peer("exit-host", "100.64.0.5", true, true)]);
        assert!(
            !rt.reapply_ts_exit_node().await,
            "核未运行 → 无管理 API 可打，必须直接跳过（绝不盲连）"
        );
        // 有 running 但无端口 → 同样跳过（端口是 gRPC 目标的必要条件）。
        *rt.status.write().unwrap() = ProxyStatus {
            running: true,
            clash_api_port: 0,
            ..Default::default()
        };
        assert!(
            !rt.reapply_ts_exit_node().await,
            "clash_api_port=0 → 无从建连，必须跳过"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **R1 守卫链的其余分支**：未配 `exitNode` / peers 解不到 `stableID` → 跳过（不猜、不盲发）。
    ///
    /// **变异锁**：删掉「exitNode 非空」守卫 → 第一条会走到 peers 匹配（找不到 → 仍 false，但下一条
    /// 断言的语义已丢）；删掉 `stable_id` 守卫 → 第二条会走到真 gRPC 建连 → 本机零网络前提被打破。
    #[tokio::test]
    async fn reapply_ts_exit_node_requires_exit_node_and_stable_id() {
        let (rt, dir) = test_runtime();
        *rt.status.write().unwrap() = ProxyStatus {
            running: true,
            clash_api_port: 65_535,
            ..Default::default()
        };
        // ① 未配 exitNode（切走出口 / 仅内网）→ 无可重设
        *rt.current_config.write().unwrap() = Some(ts_exit_config(None));
        seed_ts_frame(&rt, vec![ts_peer("exit-host", "100.64.0.5", true, true)]);
        assert!(!rt.reapply_ts_exit_node().await, "未配 exitNode → 跳过");
        // ② 配了但 peers 里那条没有 stableID（旧核不发）→ 跳过
        *rt.current_config.write().unwrap() = Some(ts_exit_config(Some("exit-host")));
        let mut p = ts_peer("exit-host", "100.64.0.5", true, true);
        p.stable_id = None;
        seed_ts_frame(&rt, vec![p]);
        assert!(
            !rt.reapply_ts_exit_node().await,
            "无 stableID → 跳过，绝不盲发 EditPrefs"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **R2 会话起点复位**：停核/崩溃后翻转对账缓存归零，而在飞恢复腿的**单飞令牌一根手指都不许碰**。
    ///
    /// 后半条是本轮修的偏离（上游 `ProxyManager.ts:695` 只清 `lastTsExitBlock`）：清了令牌，
    /// 新会话就能在旧腿还在飞时再抢一次 ⇒ 两条恢复腿并发；更糟的是旧腿退出时
    /// [`TsExitRecoverGuard`] 的 Drop 会把**新会话**刚置的 recovering/pending 清掉 ⇒ 单飞被打穿。
    ///
    /// **变异锁**：删掉 `last_ts_exit_block` 复位 → ① 转红（复位后第一次 blocked 被当成同态吞掉）；
    /// 把 `ts_exit_recovering` / `ts_exit_recover_pending` 的 `store(false)` **加回** `reset_ts_exit_block_state`
    /// → ②③ 转红。
    #[tokio::test]
    async fn reset_clears_the_reconcile_cache_but_never_the_single_flight_token() {
        let (rt, dir, _inval, _refreshes, marks) = test_runtime_r2();
        rt.config
            .save_full(&ts_exit_config(None))
            .expect("save cfg");
        seed_ts_frame(&rt, vec![]);
        rt.reconcile_ts_exit_block(rt.gate.generation());
        assert_eq!(marks.lock().unwrap().len(), 1);
        // 造出「旧会话的恢复腿仍在飞、且期间又记下一条边沿」的现场。
        assert!(rt.begin_ts_exit_recovery(), "首次抢占");
        assert!(!rt.begin_ts_exit_recovery(), "在飞 → 记 pending");

        rt.reset_ts_exit_block_state();

        // ① 复位后同一无效态必须能**重新**触发（会话起点语义）。
        rt.reconcile_ts_exit_block(rt.gate.generation());
        assert_eq!(
            marks.lock().unwrap().len(),
            2,
            "复位后首帧须能重新落终态；不复位则重连后终态永远落不下去"
        );
        // ②③ 令牌与边沿都归在飞任务所有，停核腿无权归还 —— 归还权错位正是「旧腿 Drop 清掉新会话
        // 的单飞位」那条打穿路径的入口。
        assert!(
            rt.ts_exit_recovering.load(Ordering::SeqCst),
            "停核不得替在飞任务归还单飞令牌（否则新会话可再抢一次 → 两条恢复腿并发跑同一套 route 手术）"
        );
        assert!(
            rt.ts_exit_recover_pending.load(Ordering::SeqCst),
            "停核不得抹掉在飞期间记下的边沿（消费权归在飞任务的 Drop，它会按当下核状态裁定要不要补跑）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **组合面门**：R2 对账真的挂在 `apply_ts_status_frame` 尾部（而不是只写了个没人调的方法）。
    ///
    /// 喂一帧真 proto 更新（选中 TS 出口无 exit_node ⇒ NoExitDevice）→ 终态必须被落下。
    ///
    /// **变异锁**：删掉 `apply_ts_status_frame` 尾部的 `self.reconcile_ts_exit_block(my_gen);` → marks 空 → 转红。
    /// 这正是「逻辑在、接线不在」那类缺陷的守卫（本仓已栽过一次：`exit_route_reassert` 挂着
    /// `#[allow(dead_code)]` 全仓零调用点）。
    #[tokio::test]
    async fn ts_status_frame_drives_the_exit_block_reconcile() {
        use polaris_singbox_grpc::daemon;
        let (rt, dir, _inval, _refreshes, marks) = test_runtime_r2();
        rt.config
            .save_full(&ts_exit_config(None))
            .expect("save cfg");
        let tag_to_id = BTreeMap::from([("组网出口".to_string(), "ts1".to_string())]);
        let update = daemon::TailscaleStatusUpdate {
            endpoints: vec![daemon::TailscaleEndpointStatus {
                endpoint_tag: "组网出口".into(),
                backend_state: "Running".into(),
                self_: Some(daemon::TailscalePeer {
                    host_name: "self".into(),
                    tailscale_i_ps: vec!["100.64.0.9".into()],
                    ..Default::default()
                }),
                ..Default::default()
            }],
        };

        rt.apply_ts_status_frame(&update, &tag_to_id, rt.gate.generation());

        assert_eq!(
            *marks.lock().unwrap(),
            vec!["ts-no-exit-device".to_string()],
            "STATUS 帧尾必须跑翻转对账，否则推侧整条腿是死代码（拉侧只在用户点检测那刻求值）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════ §15：测速 dirty 波前预筛的指纹开口 ══════════════

    /// `speed_probe_targets` 必须带出**起核时刻的节点指纹表**（dirty 波前预筛的唯一诚实判据）。
    ///
    /// 消费侧要判「运行核里的这个节点还是不是用户现在配置的那个」，只能拿指纹比 —— 而
    /// `pending_changes()` 的 `updated` 是 **id 交集**（不是指纹比对），拿它当 dirty 会把每个既有节点
    /// 全判脏。故必须把指纹本身开口子带出来。
    ///
    /// **变异锁**：删掉 `fingerprints` 的透传（填 `BTreeMap::new()`）→ 第二条断言转红；
    /// 把它接成 `id_to_tag` → 值不符转红。
    #[test]
    fn speed_probe_targets_carry_running_core_fingerprints() {
        let (rt, dir) = test_runtime();
        *rt.status.write().unwrap() = ProxyStatus {
            running: true,
            ..Default::default()
        };
        *rt.switch_snapshot.write().unwrap() = Some(SwitchSnapshot {
            id_to_tag: BTreeMap::from([("id-a".to_string(), "东京 03".to_string())]),
            // 全维表（喂重启判据 + pending modified）与 5 维表（喂测速 dirty）刻意填成不同值：
            // 带错哪一张，下面的断言立刻说话。
            fingerprints: BTreeMap::from([("id-a".to_string(), "全维-a".to_string())]),
            dirty_fingerprints: BTreeMap::from([("id-a".to_string(), "fp-a".to_string())]),
            probe_pool_ports: vec![41001, 41002],
            ..Default::default()
        });

        let t = rt.speed_probe_targets().expect("核在跑 + 池非空 → Some");
        assert_eq!(t.pool_ports, vec![41001, 41002]);
        assert_eq!(
            t.fingerprints.get("id-a").map(String::as_str),
            Some("fp-a"),
            "必须带出 **5 维** dirty 表：带成全维表 → 与测速「新」侧公式不同 → 恒不等 → 全员恒 dirty"
        );
        assert_eq!(t.id_to_tag.get("id-a").map(String::as_str), Some("东京 03"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════ B1：隐私模式不抬核日志级别 ══════════════

    /// **隐私模式活态必须真的流进 `GenerateConfigDeps.privacy_mode`**（此前硬编码 false）。
    ///
    /// 后果不是 UI 问题而是**落盘泄露**：`build_log_config` 的 `effective(privacy)` 把 info/debug 抬到
    /// warn，正是为了让隐私期 helper stderr 不记连接明细；硬编码 false 时那条抬级永远不触发。
    ///
    /// **变异锁**：把 `privacy_mode:` 改回 `false` → 第二条转红；把 `privacy_mode_active` 的
    /// emitter 未接线默认改成 `true` → 第一条转红（未接线时不得擅自抬级 = 静默改变用户设定的日志级别）。
    #[test]
    fn privacy_mode_flows_into_generate_deps() {
        let (rt, dir) = test_runtime(); // 未接线 emitter
        assert!(
            !rt.generate_deps(1, 0, &[], &serde_json::json!({}))
                .privacy_mode,
            "emitter 未接线（单测 / setup 前）→ 保守 false，与接线前逐字节同"
        );
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            privacy_mode: true,
            ..Default::default()
        }));
        assert!(
            rt.generate_deps(1, 0, &[], &serde_json::json!({}))
                .privacy_mode,
            "隐私模式开启时 deps 必须为 true，否则核日志级别不抬 ⇒ 隐私期域名照写 helper stderr"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// W26：生产 runtime 永远不给 sing-box `log.output` 文件句柄；否则 child 自己持有的 fd/handle
    /// 无法被 Polaris writer 运行期轮转，1.46GB 同型故障会直接复发。
    #[test]
    fn runtime_log_output_is_owned_by_bounded_sink_not_core() {
        let (rt, dir) = test_runtime();
        assert!(
            rt.generate_deps(1, 0, &[], &serde_json::json!({}))
                .log_file_path
                .is_none(),
            "runtime config 不得把固定 output 文件重新交给 sing-box 持有"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════ #327：起核后 TUN 适配器存在性逐腿验证 ══════════════

    /// 探测适用面的真值表：**仅 TUN@Windows**。
    ///
    /// **变异锁**：删 `is_tun` → 第 3 条转红（systemProxy 也去探，而它根本不建适配器 ⇒ 恒 `Absent`，
    /// 等于把完全正常的起核判成失败）；删平台判据 → 第 4/5 条转红（mac/Linux 上 `WinAdapterProbe`
    /// 不存在，本机还会白跑）；把 `"win32"` 写成 `"windows"` → 第 1/2 条转红（`platform_tag` 用的是
    /// Node 约定）。
    #[test]
    fn wintun_probe_gate_is_tun_on_windows_only() {
        assert!(should_probe_wintun_adapter(ProxyModeType::Tun, "win32"));
        assert!(!should_probe_wintun_adapter(
            ProxyModeType::SystemProxy,
            "win32"
        ));
        assert!(!should_probe_wintun_adapter(ProxyModeType::Manual, "win32"));
        assert!(!should_probe_wintun_adapter(ProxyModeType::Tun, "darwin"));
        assert!(!should_probe_wintun_adapter(ProxyModeType::Tun, "linux"));
    }

    /// 判定真值表：见到 / 不可断言 → 放行；缺失 → 预算内重试，耗尽按「曾见过」分岔两个终态。
    ///
    /// **变异锁**：
    /// - 把 `Indeterminate` 归到失败侧 → 第 2 条转红（枚举 API 一坏就杀正常核，比原缺陷更糟）；
    /// - 把重试条件写成 `attempt < max_retries` → 第 4 条转红（少用一整条腿的预算）；
    /// - 丢掉 `ever_seen` 分岔（两个终态压成一个）→ 第 6 条转红（抖动被误报成「wintun 建不出来」，
    ///   把用户导向「重装驱动」这条错误的下一步）。
    #[test]
    fn tun_adapter_leg_verdicts() {
        use TunAdapterObservation as O;
        use TunAdapterVerdict as V;
        // 1) 见到 → 放行。
        assert_eq!(classify_tun_adapter_leg(O::Present, true, 3, 2), V::Proceed);
        // 2) 不可断言（非 TUN@win / 自定义接口名 / 枚举报错）→ 放行，绝不据此杀核。
        assert_eq!(
            classify_tun_adapter_leg(O::Indeterminate, false, 3, 2),
            V::Proceed
        );
        // 3) 缺失 + 预算充足 → 计入重试预算。
        assert_eq!(
            classify_tun_adapter_leg(O::Absent, false, 1, 2),
            V::RetryLeg
        );
        // 4) 缺失 + 恰好用到最后一次重试（attempt == max_retries）→ 仍重试（与 Dead/Timeout 腿同判据）。
        assert_eq!(
            classify_tun_adapter_leg(O::Absent, false, 2, 2),
            V::RetryLeg
        );
        // 5) 缺失 + 预算耗尽 + 全程没见过 → 终态：wintun 建不出来。
        assert_eq!(
            classify_tun_adapter_leg(O::Absent, false, 3, 2),
            V::TerminalNeverAppeared
        );
        // 6) 缺失 + 预算耗尽 + 中途见过 → 终态，但不是「建不出来」（抖动，指引完全不同）。
        assert_eq!(
            classify_tun_adapter_leg(O::Absent, true, 3, 2),
            V::TerminalAfterFlap
        );
        // 7) 零重试预算（max_retries=0）→ 首腿缺失即终态。
        assert_eq!(
            classify_tun_adapter_leg(O::Absent, false, 1, 0),
            V::TerminalNeverAppeared
        );
    }

    /// **接线守卫**：存在性验证必须在**重试循环内**、且在就绪判定之后、`verify_tun_route_captured` 之前。
    ///
    /// 三条位置关系各锁一个真实的退化方向：
    /// - 挪出循环 → 退回「只验最后一腿」，前 N-1 腿的假就绪照样能标 connected（本 issue 的原形）；
    /// - 挪到就绪之前 → 核还没起完就问「网卡呢」，恒缺失 ⇒ TUN 模式全线起不来；
    /// - 排到出口归属校验之后 → 网卡都没有时先问「默认路由切走了没」，用户拿到的是
    ///   「其他 VPN 占用默认路由，请先断开」这条与现场无关的指引。
    ///
    /// 行为测试够不着：整条是 `cfg(windows)` + 真起核 + 真建网卡（三重真机门），本机跑不到。
    #[test]
    fn tun_adapter_presence_probe_is_wired_per_retry_leg() {
        // 不带 `self.` 前缀：调用点被 rustfmt 折成 `self\n.probe_tun_adapter_present(`，
        // 连写 `self.` 的判据会被换行静默打空（那就是「扫到 0 条于是全绿」的假门）。
        const PROBE: &str = ".probe_tun_adapter_present(";
        let body = method_body(include_str!("proxy.rs"), "    async fn start_inner(");
        let loop_head = body
            .find("= loop {")
            .expect("起核重试 loop 锚点消失，接线守卫已失去判据");
        let ready_arm = body
            .find("CoreReadyOutcome::Ready => {")
            .expect("就绪腿锚点消失，接线守卫已失去判据");
        let route_gate = body
            .find(".verify_tun_route_captured(")
            .expect("出口归属校验锚点消失，接线守卫已失去判据");
        let probe = body.find(PROBE).expect("TUN 适配器存在性验证未接线");
        assert_eq!(
            body.matches(PROBE).count(),
            1,
            "start_inner 里只该有一处存在性验证；出现第二处说明判据被复制，两处会分头漂移"
        );
        assert!(
            loop_head < probe,
            "存在性验证必须在起核重试循环**内**（逐腿验）"
        );
        assert!(
            ready_arm < probe,
            "存在性验证必须在就绪判定**之后** —— 核没起完就问网卡，恒缺失"
        );
        assert!(
            probe < route_gate,
            "存在性验证必须排在出口归属校验**之前**：网卡都没有时问「路由切走了没」，\
             只会给出「断开其他 VPN」这条与现场无关的指引"
        );
    }

    // ══════════════ #332：核 stderr FATAL 真因 → 专属错误码 ══════════════

    /// 判定用的样本行按**取证到的字面量**拼（链路见 [`classify_core_fatal_line`] 文档）：
    /// 外层 `configure tun interface`（sing-box `protocol/tun/inbound.go:438`，已在随包 1.14.0-beta.7
    /// 二进制里 `strings` 验到）+ 内层 `set ipv4 address`（sing-tun `tun_windows.go:81`，Windows-only
    /// 文件，取自源码而非二进制）。
    fn fatal_line(inner: &str) -> String {
        format!("+0800 FATAL start service: initialize inbound/tun[tun-in]: {inner}")
    }

    #[test]
    fn core_fatal_classifies_tun_address_step() {
        let win =
            fatal_line("configure tun interface: set ipv4 address: The object already exists.");
        assert_eq!(
            classify_core_fatal_line(&win, singbox_line_level(&win)),
            Some(CoreFatalKind::TunAddressUnavailable)
        );
        let win6 =
            fatal_line("configure tun interface: set ipv6 address: The object already exists.");
        assert_eq!(
            classify_core_fatal_line(&win6, singbox_line_level(&win6)),
            Some(CoreFatalKind::TunAddressUnavailable)
        );
        // Linux 侧同一件事的包装串（sing-tun `tun_linux.go:145`）。
        let linux = fatal_line("configure tun interface: add address 172.19.0.1/30: file exists");
        assert_eq!(
            classify_core_fatal_line(&linux, singbox_line_level(&linux)),
            Some(CoreFatalKind::TunAddressUnavailable)
        );
    }

    /// **本条锁的是「不拿 errno 文案当判据」这个决定**：Windows 的那截尾巴经 `FormatMessage` 生成、
    /// 跟随系统语言。若判据里塞了 `"already exists"`，中文/俄文 Windows 上判定静默失效 —— 而那恰是
    /// 用户最多的那批机器。改判据前先看这条测试。
    #[test]
    fn core_fatal_is_independent_of_os_errno_language() {
        for tail in [
            "对象已存在。",
            "Объект уже существует.",
            "L'objet existe déjà.",
        ] {
            let line = fatal_line(&format!(
                "configure tun interface: set ipv4 address: {tail}"
            ));
            assert_eq!(
                classify_core_fatal_line(&line, singbox_line_level(&line)),
                Some(CoreFatalKind::TunAddressUnavailable),
                "errno 文案换个语言就判不出来 = 判据依赖了系统语言"
            );
        }
    }

    #[test]
    fn core_fatal_rejects_non_tun_address_failures() {
        // 1) 端口占用（mixed 入站）——同样带「address already in use」，但**不是** TUN 地址冲突。
        //    误归本码会把用户导向「断开其他 VPN」，而真正该做的是换端口。
        let port = fatal_line("listen tcp 127.0.0.1:7890: bind: address already in use");
        assert_eq!(
            classify_core_fatal_line(&port, singbox_line_level(&port)),
            None
        );
        // 2) TUN 配置里**别的**步骤失败（MTU）→ 不是地址轴。
        let mtu = fatal_line("configure tun interface: set mtu: invalid argument");
        assert_eq!(
            classify_core_fatal_line(&mtu, singbox_line_level(&mtu)),
            None
        );
        // 3) 级别门：正常 INFO 行里出现同样的词（回放/引用）不算真因。
        let info = "+0800 INFO configure tun interface: set ipv4 address ok";
        assert_eq!(
            classify_core_fatal_line(info, singbox_line_level(info)),
            None
        );
    }

    #[test]
    fn scan_core_fatal_takes_first_hit_in_block() {
        let block = format!(
            "+0800 INFO router: loaded rule-set\n{}\n+0800 FATAL sing-box did not close!\n",
            fatal_line("configure tun interface: set ipv4 address: The object already exists.")
        );
        assert_eq!(
            scan_core_fatal(&block),
            Some(CoreFatalKind::TunAddressUnavailable)
        );
        // 无命中 → None（绝不因为「有 FATAL」就瞎归类）。
        assert_eq!(
            scan_core_fatal("+0800 FATAL start service: create service: bad json"),
            None
        );
    }

    #[test]
    fn startup_log_cursor_distinguishes_append_from_fresh_rotation() {
        let cursor = StartupLogCursor {
            offset: 128,
            identity: Some(7),
        };
        assert_eq!(
            startup_log_read_start(cursor, 256, Some(7)),
            128,
            "旧 helper 在同一文件 append：只读本腿新增部分"
        );
        assert_eq!(
            startup_log_read_start(cursor, 512, Some(8)),
            0,
            "新 helper fresh-rotate：即使新文件更长也必须从头读"
        );
        assert_eq!(
            startup_log_read_start(cursor, 64, Some(7)),
            0,
            "同一身份但长度缩短时不能 seek 越过本腿日志"
        );
        assert_eq!(
            startup_log_read_start(StartupLogCursor::default(), 64, Some(8)),
            0,
            "起核前没有 current 文件时整份都属于本腿"
        );
    }

    #[test]
    fn settle_start_failure_swaps_generic_code_only_when_cause_is_known() {
        // 有真因 → 专属码 + 专属文案（症状串被整句替换，见该函数文档）。
        let (msg, code_out) = settle_start_failure(
            "sing-box 起核超时（管理 API 9090 在 12000ms 内未就绪）".to_string(),
            Some(CoreFatalKind::TunAddressUnavailable),
        );
        assert_eq!(code_out, code::TUN_ADDRESS_UNAVAILABLE);
        assert_eq!(msg, TUN_ADDRESS_UNAVAILABLE_MSG);
        // 无真因 → 逐字维持原有行为（本条是回归锁：拿不到真因时不许改动既有的失败面）。
        let (msg, code_out) = settle_start_failure("sing-box 启动期退出".to_string(), None);
        assert_eq!(code_out, code::STARTUP_FAILED);
        assert_eq!(msg, "sing-box 启动期退出");
    }

    /// **接线守卫**：起核失败的两条终态腿（Dead / Timeout）都必须经 [`settle_start_failure`] 收口。
    ///
    /// 漏一条 = 那条腿上的真因永远上不了屏，而它与另一条腿的差别只是「就绪门先超时还是进程先没」
    /// —— 用户视角完全同一件事。行为测试够不着（真起核 + 真地址冲突 = 真机门）。
    #[test]
    fn core_fatal_is_wired_into_both_terminal_start_legs() {
        let body = method_body(include_str!("proxy.rs"), "    async fn start_inner(");
        assert_eq!(
            body.matches("settle_start_failure(").count(),
            2,
            "起核终态收口必须恰好两处（Dead / Timeout 各一）；少了 = 有腿绕过真因判定，\
             多了 = 收口点被复制"
        );
        assert_eq!(
            body.matches("self.observe_core_fatal(").count(),
            2,
            "两条腿各自读一次本腿的 stderr 真因；共用一次读会跨腿错配"
        );
        // stderr 才接真因槽（stdout 传 None）——写反了等于永远收不到 FATAL。
        assert!(
            body.contains("pipe_to_log(\n                    spawned.child.stderr.take(),\n                    Some(Arc::clone(&fatal_slot)),"),
            "真因槽必须接在 stderr 上（sing-box 的 log.Fatal 恒写 os.Stderr）"
        );
        assert!(
            body.contains(
                "pipe_to_log(\n                    spawned.child.stdout.take(),\n                    None,"
            ),
            "stdout 不接真因槽（白扫每一行）"
        );
    }

    /// **接线守卫**：核日志 relay 与 stderr 转发腿的交接必须成对存在。
    ///
    /// 两半各自缺席的后果不同、且都静默：
    ///  - 直起腿没把 `handoff` 交给 `pipe_to_log` ⇒ 核就绪后每行进两遍环形缓冲（日志页整屏重影）；
    ///  - relay 没接上 `log_pipe_handoff` ⇒ 管道永不让位，同样重影；
    ///  - relay 压根没挂 ⇒ TUN/helper 腿日志页零核行（这正是本批要修的那条），而直起腿看不出区别。
    ///
    /// 三条都够不着行为测试（要真起核 + 真管理 API），故落成源码接线断言。
    #[test]
    fn core_log_relay_and_stderr_pipe_hand_off_to_each_other() {
        let body = method_body(include_str!("proxy.rs"), "    async fn start_inner(");
        assert!(
            body.contains("let handoff: CoreLogHandoff = Arc::new(AtomicBool::new(false));")
                && body.contains("log_pipe_handoff = Some(handoff);"),
            "直起腿必须建交接闸并交给 relay（否则核就绪后每行进两遍缓冲）"
        );
        assert_eq!(
            body.matches("Some(Arc::clone(&handoff))").count(),
            2,
            "stdout / stderr 两条管道都要拿到交接闸（漏一条 = 那条腿永不让位）"
        );
        assert!(
            body.contains("self.spawn_core_log_relay(my_gen, api_port, log_pipe_handoff.clone());"),
            "relay 必须在核就绪处按世代挂上，且接的就是本腿的交接闸"
        );
        // helper 腿不建闸：那条腿根本没有管道，`None` 同时也是 relay「收下首帧历史」的判据。
        assert!(
            body.contains("let mut log_pipe_handoff: Option<CoreLogHandoff> = None;"),
            "交接闸默认 None（helper 腿无管道 ⇒ relay 必须收下首帧历史）"
        );
    }

    /// **接线守卫（relay 体内）**：核日志 relay 的三条承重接线。缺任一条都不会编译报错、也不会让
    /// 任何行为测试转红（relay 要真核 + 真管理 API 才跑得起来），但后果各自明确：
    ///
    ///  - 不读隐私下限 / 读了不用 ⇒ 隐私锁开着时用户访问的域名照样落进**不脱敏**的 `polaris.log`；
    ///  - 下限（或级别上限）在循环**外**只读一次 ⇒ 运行期打开隐私锁 / 改级别不生效
    ///    （「开了锁还在漏」「拨到 debug 却还是看不到」）；
    ///  - 级别上限的预筛落在 [`strip_core_log_decoration`] **之后** ⇒ 判定结果一模一样、
    ///    `core_log_admits` 的单测也全绿，但每条注定被丢的 trace/debug 行照旧付两次堆分配 ——
    ///    这道预筛的**全部价值**就在那个先后次序上，只有源码断言看得见；
    ///  - `frame.reset` 那格丢了 ⇒ 每次断线重连把至多 3000 行历史当增量整屏重放。
    #[test]
    fn core_log_relay_applies_privacy_floor_per_frame_and_guards_reset_history() {
        let body = method_body(include_str!("proxy.rs"), "    fn spawn_core_log_relay(");
        assert!(
            body.contains("let floor = core_log_privacy_floor(me.privacy_mode_active());")
                && body.contains("if !core_log_admits(level, floor, max) {"),
            "转发口必须过 core_log_admits（否则隐私锁在生成侧堵住的路从这条流上原样漏回来）"
        );
        // 两道闸都必须在**收帧之后**读：隐私模式与日志级别都可运行期切换，起流时定死即失效。
        let frame_at = body.find("Ok(Some(frame)) =>").expect("收帧分支锚点消失");
        assert!(
            frame_at
                < body
                    .find("let floor = core_log_privacy_floor(")
                    .expect("隐私下限锚点消失"),
            "隐私下限必须逐帧现读，不得在起流时定死"
        );
        assert!(
            frame_at
                < body
                    .find("let max = log::max_level();")
                    .expect("级别上限锚点消失"),
            "级别上限必须逐帧现读，不得在起流时定死"
        );
        // 预筛必须早于剥除 —— 否则这道闸只剩「与 log! 判定一致」，白搬的活一点没省。
        assert!(
            body.find("if !core_log_admits(level, floor, max) {")
                .expect("预筛锚点消失")
                < body
                    .find("let text = strip_core_log_decoration(")
                    .expect("剥除锚点消失"),
            "级别预筛必须在剥除之前（放到之后 = 每条被丢的行照付两次堆分配，行为测试全绿）"
        );
        assert!(
            body.contains("if frame.reset {") && body.contains("if !history_pending {"),
            "reset 帧必须单独判：重连必然重发全量历史，照单收下 = 整屏重放"
        );
        assert!(
            body.contains("if me.gate.generation() != my_gen {"),
            "世代守卫必须在（ReconnectingStream 永不自结束，没有它 relay 会泄漏并对死端口无限重连）"
        );
    }

    /// **接线守卫（消费侧）**：`pipe_to_log` 真的按交接闸让位，且**让位只挡转发、不挡 FATAL 分类**。
    ///
    /// 上一条守的是「闸有没有被建出来、有没有交到两边手里」，管不到闸**在管道循环里被怎么用**：
    /// 把那个 `if` 删掉 ⇒ 核就绪后每行进两遍环形缓冲；反过来把分类也塞进 `if` 里 ⇒ 就绪之后核以
    /// `log.Fatal` 死掉时真因收不到（那条行只走 stderr，`SubscribeLog` 结构性看不见它）。两种改法
    /// 上一条都恒绿。
    ///
    /// # 为什么是源码断言而不是行为测试
    ///
    /// 转发的落点是 `log::log!` → `logging.rs` 的 sink，而**单测进程里根本没有装 sink**
    /// （`log::set_logger` 只在 `logging::init` 里调，生产启动路径才走）⇒ 无论闸是开是关，环形缓冲
    /// 都收不到任何东西，行为测试对这两种改法**结构上零信息量**。装一个进程级 logger 又会污染同一
    /// 测试二进制里 `logging.rs` 那几条已经串行化的全局级别用例。故按本模块既有惯例落成源码断言。
    ///
    /// **判据区域排除自身**：只在 `pipe_to_log` 函数体这一段里找（起于其函数头、止于其闭合大括号），
    /// 且断言该函数头出现在 `mod tests` 之前 —— 否则本测试自己写下的那几行字面量就会给判据充数。
    #[test]
    fn pipe_to_log_yields_forwarding_on_handoff_but_never_yields_fatal_classification() {
        const SRC: &str = include_str!("proxy.rs");
        let start = SRC
            .find("fn pipe_to_log<R>(stream:")
            .expect("锚点 `fn pipe_to_log<R>(stream:` 消失，源码型守卫已失去判据");
        assert!(
            start
                < SRC
                    .find("\nmod tests {")
                    .expect("测试模块锚点消失，无法确认判据区域排除了自身"),
            "取到的必须是生产代码里那个 pipe_to_log，不是本测试自己写下的字面量"
        );
        let body = &SRC[start..];
        let body = &body[..body.find("\n}\n").expect("pipe_to_log 函数体没闭合")];

        assert!(
            body.contains(
                "if !handoff.as_ref().is_some_and(|h| h.load(Ordering::SeqCst)) {\n                log::log!(target: crate::logging::SING_BOX_TARGET, level, \"{line}\");"
            ),
            "转发必须被交接闸挡住（否则核就绪后每行进两遍环形缓冲，日志页整屏重影）"
        );
        // 分类在闸**外**：缩进 12 空格 = 与 `if` 同层；被塞进 `if` 里就会变成 16 空格。
        assert!(
            body.contains(
                "\n            let Some(kind) = classify_core_fatal_line(&line, level) else {"
            ),
            "FATAL 分类不得被交接闸挡住：就绪之后核以 log.Fatal 死掉时，那条行只走 stderr"
        );
    }

    /// 装 mock emitter + **可注入的 clearer** 的运行时，暴露全部三类发射记录句柄。
    /// 供「走真 start 路径验发射接线」的组合测试用（core 路径靠 POLARIS_SINGBOX_PATH 指向不存在文件
    /// 在 spawn 前失败 —— emit 发生在起核之前，本机零进程零网络）。
    #[allow(clippy::type_complexity)]
    fn test_runtime_recording_full(
        clearer: Box<dyn SystemProxyClearer>,
    ) -> (
        Arc<ProxyRuntime>,
        PathBuf,
        InvalidNodeFrames,
        ResidualEvents,
    ) {
        let dir = fresh_test_dir();
        let config = Arc::new(ConfigManager::new(dir.clone()));
        let helper = Arc::new(HelperRuntime::never_installed_for_tests(dir.clone()));
        let mesh = Arc::new(MeshRuntime::new(dir.clone()));
        let rt = Arc::new(ProxyRuntime::new(
            config,
            helper,
            mesh,
            clearer,
            Arc::new(NoNetworkDoh),
        ));
        let invalid_frames: InvalidNodeFrames = Arc::new(Mutex::new(Vec::new()));
        let residual: ResidualEvents = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            events: Arc::new(Mutex::new(Vec::new())),
            invalid_frames: Arc::clone(&invalid_frames),
            residual: Arc::clone(&residual),
            ..Default::default()
        }));
        (rt, dir, invalid_frames, residual)
    }

    /// `set_error` → 真发 `event:proxyError`，且 message/errorCode 与落进状态的**同源**。
    /// 打断 set_error 里的 emit（删掉那段 match）→ 本测转红 = 退回「通道定义了却零 emit」的原 bug。
    #[test]
    fn set_error_emits_proxy_error_event() {
        let (rt, dir, events) = test_runtime_recording_errors();
        rt.set_error("sing-box 启动期退出", code::STARTUP_FAILED);
        let got = events.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![(
                "sing-box 启动期退出".to_string(),
                "STARTUP_FAILED".to_string()
            )]
        );
        // 事件与状态快照同源（错过事件的 UI 仍能从 getStatus 读到同一个码）。
        let s = rt.status();
        assert_eq!(s.error.as_deref(), Some("sing-box 启动期退出"));
        assert_eq!(s.error_code.as_deref(), Some(code::STARTUP_FAILED));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「启动失败」与「运行中崩了」必须靠 errorCode 分得开（brief 的硬要求）。
    /// 打断（两条腿传同一个码）→ 本测转红。
    #[test]
    fn startup_failure_and_runtime_crash_carry_distinct_codes() {
        let (rt, dir, events) = test_runtime_recording_errors();
        rt.set_error("起核超时", code::STARTUP_FAILED);
        rt.set_error("反复崩溃", code::AUTO_RESTART_FAILED);
        let codes: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        assert_eq!(codes, vec!["STARTUP_FAILED", "AUTO_RESTART_FAILED"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── C2：崩溃自愈 GiveUp 腿不得在已有更具体终态码时叠发 AUTO_RESTART_FAILED ──
    //
    // 断言的是**发射条数**（缺陷的可观测症状本身：前端两码各自 toast.error + notifyDesktop，且
    // 崩溃自愈两腿无人 await ⇒ 认领闸门不抑制 ⇒ 用户背靠背吃 2 toast + 2 桌面通知），不是布尔判定。

    /// **本缺陷的复现锚**：`run_helper_gate` 非交互腿已 `set_error(HELPER_NOT_INSTALLED)` 发过一条，
    /// GiveUp 腿不得再叠一条 `AUTO_RESTART_FAILED`。
    ///
    /// **变异有牙（逃逸面穷举）**：
    /// - 删 `report_auto_restart_giveup` 的 `if let Some(code) { return }` 早退（退回无条件 set_error）
    ///   = **缺陷复现**（双发）→ 三个码族各自 `len()==2` → 转红。
    /// - 早退只判 `HELPER_NOT_INSTALLED`（漏 GATE_ABORTED / STARTUP_FAILED）→ 后两轮转红
    ///   （故此处对**全部三个码族**各跑一轮，而非只钉 helper 一条腿）。
    /// - 判据换成回读全局 `status().error_code` → 本测仍绿（状态确实刚落），但那是 A1 陈旧读，
    ///   由下一条 `..._still_reports_when_no_specific_code` 的无码腿把它钉住：无码腿不写全局，
    ///   回读拿到的是**上一次**失败的残留码 ⇒ 误判「已播报」⇒ 变静默 ⇒ 那一条转红。
    #[test]
    fn auto_restart_giveup_does_not_stack_code_when_specific_terminal_already_emitted() {
        for (msg, specific) in [
            (HELPER_NOT_INSTALLED_MSG, code::HELPER_NOT_INSTALLED),
            (HELPER_GATE_ABORTED_MSG, code::HELPER_GATE_ABORTED),
            ("sing-box 启动期退出", code::STARTUP_FAILED),
        ] {
            let (rt, dir, events) = test_runtime_recording_errors();
            // 失败腿自己那条（`StartError::coded` 构造点恒紧邻的同码 set_error）。
            rt.set_error(msg, specific);
            // 同一个 e 出栈到 GiveUp 腿 → 不得再发第二条。
            rt.report_auto_restart_giveup(&StartError::coded(msg, specific));

            let got = events.lock().unwrap().clone();
            assert_eq!(
                got.len(),
                1,
                "{specific}：GiveUp 腿叠发 AUTO_RESTART_FAILED ⇒ 前端 2 toast + 2 桌面通知（本缺陷）"
            );
            assert_eq!(got[0].1, specific, "留下的必须是**更具体**的那条码");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// **反向失效锁（防修过头）**：无码腿（config 解析/生成/建目录/写盘 —— `From<String>` 升格，
    /// 自身**从不** set_error）放弃时**必须**发 `AUTO_RESTART_FAILED`，否则前端一条提示都收不到。
    ///
    /// **变异有牙**：
    /// - `report_auto_restart_giveup` 改成无条件早退（“修过头”）→ 零发射 → 转红（变静默）。
    /// - 早退条件写反（`e.code.is_none()` 时早退）→ 本条 + 上一条**双红**。
    /// - 发错码（如沿用 STARTUP_FAILED）→ 码断言转红。
    /// - 丢掉原始错因（只发一句固定文案）→ message 包含断言转红：放弃时用户至少得看到**为什么**。
    #[test]
    fn auto_restart_giveup_still_reports_when_no_specific_code() {
        let (rt, dir, events) = test_runtime_recording_errors();
        // 无码腿：`From<String> for StartError` → code == None，且这条腿此前没有任何 set_error。
        rt.report_auto_restart_giveup(&StartError::from(
            "生成 sing-box 配置失败：invalid json".to_string(),
        ));

        let got = events.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            1,
            "无更具体的码时仍不发 ⇒ 崩溃自愈放弃全静默（比双报更坏）"
        );
        assert_eq!(got[0].1, code::AUTO_RESTART_FAILED);
        assert!(
            got[0].0.contains("invalid json"),
            "终态播报须带上原始错因，否则用户只知道“放弃了”不知道为什么：{}",
            got[0].0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // unlock 缓存失效接线（item 1：核 start/stop → unlock.invalidate）
    // ══════════════════════════════════════════════════════════════════════════════

    /// **item 1 · 核停 → unlock 缓存失效**：`stop()` 经 `stop_inner` 调 `invalidate_unlock_cache(false,false)`
    /// → emitter 记一条 `(running=false, exitBlocked=false)`。
    ///
    /// **变异锁**：删 `stop_inner` 里 `self.invalidate_unlock_cache(false, false)` → 零记录 → 转红
    /// （退回「跨起停 30min TTL 内复用停核前陈旧解锁快照」）。
    #[tokio::test]
    async fn stop_invalidates_unlock_cache() {
        let (rt, dir) = test_runtime();
        let inval: UnlockInvalidations = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            unlock_invalidations: Arc::clone(&inval),
            ..Default::default()
        }));
        rt.stop().await.expect("停无核应 Ok");
        assert_eq!(
            *inval.lock().unwrap(),
            vec![(false, false)],
            "停核须失效解锁缓存（running=false, exitBlocked=false）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **item 1 · 起核腿 running=true 参数透传**：起核就绪提交点用的正是 `invalidate_unlock_cache(true,false)`。
    /// 完整起核路径含真起核（真机门），本测锁「helper → emitter + running 语义」这段——起核调用点是就绪提交
    /// 后 code-review 可见的一行。
    ///
    /// **变异锁**：`invalidate_unlock_cache` 里不调 emitter（或吞掉 running）→ 记录不符 → 转红。
    #[test]
    fn invalidate_unlock_cache_passes_running_flag() {
        let (rt, dir) = test_runtime();
        let inval: UnlockInvalidations = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            unlock_invalidations: Arc::clone(&inval),
            ..Default::default()
        }));
        rt.invalidate_unlock_cache(true, false);
        rt.invalidate_unlock_cache(false, false);
        assert_eq!(
            *inval.lock().unwrap(),
            vec![(true, false), (false, false)],
            "running 真态须原样透传给 emitter（起核=true / 停核=false）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // 出口 IP / 延迟自动重探接线（待修 #1：核 start/stop/热切 → ipinfo 重探 → 伴测点亮延迟）
    // ══════════════════════════════════════════════════════════════════════════════

    /// **停核 → 出口 IP 重探**：`stop()` 经 `stop_inner` 调 `schedule_exit_ip_refresh(false)`。
    /// 三个触发点里**只有这个**能在单测里真跑（无核 stop 仍走 `stop_inner`），起核 / 热切走真机门 +
    /// `mod exit_ip_wiring_guard` 的配对扫描。
    ///
    /// **变异锁**：删 `stop_inner` 里那行 → 零记录 → 转红（退回「停核后状态栏仍显示代理出口 IP」）；
    /// running 传成 true → 值不符 → 转红（会让停核腿白等 4s 收敛，而出口已确定性消失）。
    #[tokio::test]
    async fn stop_schedules_exit_ip_refresh() {
        let (rt, dir) = test_runtime();
        let refreshes: ExitIpRefreshes = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            exit_ip_refreshes: Arc::clone(&refreshes),
            ..Default::default()
        }));
        rt.stop().await.expect("停无核应 Ok");
        assert_eq!(
            *refreshes.lock().unwrap(),
            vec![false],
            "停核须排程出口 IP 重探（running=false ⇒ 无收敛可等，零延迟直接探直连出口）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **running 语义透传**：起核/热切=true（要等选路收敛）与停核=false（不等）在 emitter 侧是**不同**
    /// 的延迟策略，吞掉这个参数会让停核腿白等 4s、或让起核腿在隧道未就绪时就探（探到旧出口/直接失败）。
    ///
    /// **变异锁**：`schedule_exit_ip_refresh` 里不调 emitter（或写死某个 running）→ 记录不符 → 转红。
    #[test]
    fn schedule_exit_ip_refresh_passes_running_flag() {
        let (rt, dir) = test_runtime();
        let refreshes: ExitIpRefreshes = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            exit_ip_refreshes: Arc::clone(&refreshes),
            ..Default::default()
        }));
        rt.schedule_exit_ip_refresh(true);
        rt.schedule_exit_ip_refresh(false);
        assert_eq!(
            *refreshes.lock().unwrap(),
            vec![true, false],
            "running 真态须原样透传给 emitter（起核/热切=true / 停核=false）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **延迟策略真值表**：起核/热切必须等选路收敛（否则探到旧出口或直接失败）、停核必须零延迟
    /// （出口是确定性消失，白等 4s = 状态栏多显示 4s 陈旧代理 IP）。
    ///
    /// **变异锁**：两腿写成同一个值（无论都 0 还是都 4000）→ 转红；两腿写反 → 双断言转红。
    #[test]
    fn exit_ip_refresh_delay_splits_by_running() {
        assert_eq!(
            exit_ip_refresh_delay_ms(true),
            crate::commands::misc::IPINFO_SETTLE_DELAY_MS,
            "起核/热切须等选路收敛后再探"
        );
        assert_eq!(
            exit_ip_refresh_delay_ms(false),
            0,
            "停核无收敛可等，须零延迟直接重探直连出口"
        );
        assert!(
            exit_ip_refresh_delay_ms(true) > exit_ip_refresh_delay_ms(false),
            "两腿必须是不同策略；相等即等于吞掉了 running 语义"
        );
    }

    /// **emitter 未接线不得打断起停**：与 `invalidate_unlock_cache` 同范式——发不出重探排程，绝不
    /// 反过来把停核腿弄失败。
    ///
    /// **变异锁**：`schedule_exit_ip_refresh` 改成 `self.error_emitter.get().unwrap()` → panic → 转红。
    #[tokio::test]
    async fn exit_ip_refresh_without_emitter_is_silent_noop() {
        let (rt, dir) = test_runtime();
        rt.stop()
            .await
            .expect("未接 emitter 时停核仍须 Ok（重探是增益腿，不是前置条件）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // 自动换节点心跳「选中节点须真实存在」守卫（item 10）
    // ══════════════════════════════════════════════════════════════════════════════

    /// **item 10 · 心跳守卫谓词**（上游 `AutoSwitchService.runHeartbeat`:113-116）真值表。
    ///
    /// **变异锁**：谓词恒 true → 「direct/悬挂 → false」断言转红（direct 网络抖动误切回归）；
    /// 恒 false → 「真实节点 → true」断言转红（正常节点心跳被永久跳过）。
    #[test]
    fn selected_server_present_truth_table() {
        let real = serde_json::json!({
            "selectedServerId": "a",
            "servers": [{ "id": "a" }, { "id": "b" }]
        });
        assert!(
            selected_server_present(&real),
            "选中真实节点 → true（放行心跳）"
        );

        let direct = serde_json::json!({
            "selectedServerId": "__direct__",
            "servers": [{ "id": "a" }]
        });
        assert!(
            !selected_server_present(&direct),
            "direct 哨兵不在 servers → false（跳过心跳，不切走）"
        );

        let dangling = serde_json::json!({
            "selectedServerId": "gone",
            "servers": [{ "id": "a" }]
        });
        assert!(
            !selected_server_present(&dangling),
            "选中被删（id 悬挂）→ false"
        );

        let no_sel = serde_json::json!({ "servers": [{ "id": "a" }] });
        assert!(!selected_server_present(&no_sel), "无选中 → false");

        let no_servers = serde_json::json!({ "selectedServerId": "a" });
        assert!(
            !selected_server_present(&no_servers),
            "servers 缺失 → false"
        );

        let empty_servers = serde_json::json!({ "selectedServerId": "a", "servers": [] });
        assert!(
            !selected_server_present(&empty_servers),
            "servers 空数组 → false"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // event:proxy:invalid-nodes 发射（#1：起核 gate 剔除的非法节点推给渲染端）
    // ══════════════════════════════════════════════════════════════════════════════

    /// 组合面（**两半接线**）：真 `start` 路径 → 生成 gate 报告 → `emit_invalid_nodes` 真被调。
    ///
    /// **不起真核**：`POLARIS_SINGBOX_PATH` 指向 temp **目录**（非文件）→ `resolve_core_binary`
    /// `is_file()` 判否即 Err。而 emit 发生在 **resolve/spawn 之前**（generate 之后立刻发）→ 起核尚未
    /// 发生就已发过事件，本机零进程零网络。
    ///
    /// 用 detour 级联无效配置（naive 缺 cronet 被丢 → 链到它的 ss 死引用被剔）：test dir 无
    /// libcronet.so → `has_cronet=false` 自然成立 → 报告非空。
    ///
    /// **变异锁**：删掉 `start_inner` 里 `self.emit_invalid_nodes(&outcome.invalid_nodes)` → 零帧 → 转红。
    // 跨 await 持 `ENV_LOCK`：**有意为之**。current-thread test runtime（futures 不要求 Send），锁只为
    // 把「set POLARIS_SINGBOX_PATH → 跑 start → unset」这段对并行测试串行化，无死锁面（唯一持有者）。
    // 本测试用「naive 缺 libcronet → 生成期剔除 → 级联剔 detour 引用方 ch」造无效节点。macOS 的
    // sing-box 把 cronet **静态编入**二进制（见 `cronet_available` 注释），naive 恒可用 → nv 不被剔、
    // 无级联、frame 为空，该场景在 mac 根本不成立（是 mac 正确行为，非 bug）。emit 接线本身平台无关，
    // 由 ubuntu/windows 两 leg 覆盖；无其它平台无关的「造无效节点」原语（endpoint 不能作 detour 目标、
    // detour 指向不存在 id 不剔节点），故本测试 gate 掉 macOS。
    #[cfg(not(target_os = "macos"))]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn start_emits_invalid_nodes_on_real_start_path() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let clearer: Box<dyn SystemProxyClearer> = Box::new(RecordingClearer {
            calls: Arc::clone(&calls),
        });
        let (rt, dir, frames, _residual) = test_runtime_recording_full(clearer);
        rt.stale_sweep_disabled.store(true, Ordering::SeqCst); // 跳过 /proc 孤儿清扫

        // 选中节点合法（vless+tls）→ 生成成功；naive(缺 cronet 被丢) + ss detour→naive（死引用被剔）。
        let config = serde_json::json!({
            "servers": [
                { "id": "sel", "name": "SEL", "protocol": "vless",
                  "address": "sel.example.com", "port": 443, "uuid": "u", "security": "tls" },
                { "id": "nv", "name": "NAIVE", "protocol": "naive",
                  "address": "nv.example.com", "port": 443, "naiveSettings": {} },
                { "id": "ch", "name": "CHAINED", "protocol": "shadowsocks",
                  "address": "ch.example.com", "port": 8388, "detour": "nv",
                  "shadowsocksSettings": { "method": "aes-256-gcm", "password": "p" } }
            ],
            "selectedServerId": "sel",
            "proxyMode": "smart",
            "proxyModeType": "systemProxy"
        });

        // env 串行化（与 temp_env_var 共用 ENV_LOCK）：POLARIS_SINGBOX_PATH → 目录 → resolve 必 Err。
        // current-thread test runtime，std MutexGuard 跨 await 不要求 Send。
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("POLARIS_SINGBOX_PATH", &dir);
        let r = rt.start(config).await;
        std::env::remove_var("POLARIS_SINGBOX_PATH");
        drop(_g);

        assert!(
            r.is_err(),
            "核二进制解析失败 → 起核失败（但 emit 早已发生）"
        );
        let got = frames.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "起核路径必发且仅发一帧 invalid-nodes");
        // 该帧含被级联剔除的 ch，带 detour-cascade 原因（真值端到端穿过 runtime）。
        let frame = &got[0];
        assert!(
            frame.iter().any(|n| n.id == "ch"
                && n.reason
                    == polaris_config_engine::builder::outbounds::INVALID_REASON_DETOUR_CASCADE),
            "帧内应含级联剔除的 ch（真值贯穿 config-engine→runtime→emitter），实得 {frame:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 方法级：`emit_invalid_nodes` 把非空列表原样路由到 emitter（帧内容 = 传入内容）。
    /// 变异锁：把 `emit_invalid_nodes` 里的 `e.emit_invalid_nodes(nodes)` 改成传 `&[]` → 转红。
    #[test]
    fn emit_invalid_nodes_routes_payload_to_emitter() {
        let clearer: Box<dyn SystemProxyClearer> = Box::new(RecordingClearer {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let (rt, dir, frames, _r) = test_runtime_recording_full(clearer);
        let nodes = vec![InvalidNode {
            id: "x".into(),
            tag: "节点X".into(),
            reason: "detour-cascade".into(),
        }];
        rt.emit_invalid_nodes(&nodes);
        let got = frames.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], nodes, "payload 必须原样送达，不截断不改形");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // event:systemProxyResidual 发射（#3：TUN 起核后无 marker 系统代理残留一次性提示）
    //
    // 注：**start_inner 内的调用点**（wait_ready 成功后）无法在本机验证——它须真核就绪，而本机
    // 硬禁起核。故此处覆盖 `maybe_warn_system_proxy_residual` 的**全部决策逻辑**（TUN 门控 / 每会话
    // 门闩 / detect→emit 路由），detect 侧的判定逻辑另在 `system-integration::detect_foreign_proxy`
    // 单测 + 双变异验证；另用源码不变式锁住 start_inner 只能 spawn、不得 await advisory。
    // ══════════════════════════════════════════════════════════════════════════════

    /// TUN + 检测到别人的系统代理 → 发一条 residual（payload=proxy 串）。
    /// 变异锁：把 `emit_system_proxy_residual` 调用删掉 → 零事件 → 转红。
    #[tokio::test]
    async fn residual_emitted_for_tun_with_foreign_proxy() {
        let clearer: Box<dyn SystemProxyClearer> = Box::new(ResidualClearer {
            found: Some("192.168.1.2:7890".into()),
        });
        let (rt, dir, _f, residual) = test_runtime_recording_full(clearer);
        let cfg = tun_user_config();
        rt.maybe_warn_system_proxy_residual(cfg.proxy_mode_type, None)
            .await;
        assert_eq!(
            residual.lock().unwrap().clone(),
            vec!["192.168.1.2:7890".to_string()],
            "TUN + 检出残留 → 必发一条"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 每会话只发一次（门闩）：连调两次仅一条事件。
    /// 变异锁：删有效探测后的 `residual_warned.swap(..)` 门闩 → 两条 → 转红。
    #[tokio::test]
    async fn residual_warned_only_once_per_session() {
        let clearer: Box<dyn SystemProxyClearer> = Box::new(ResidualClearer {
            found: Some("10.0.0.1:1080".into()),
        });
        let (rt, dir, _f, residual) = test_runtime_recording_full(clearer);
        let cfg = tun_user_config();
        rt.maybe_warn_system_proxy_residual(cfg.proxy_mode_type, None)
            .await;
        rt.maybe_warn_system_proxy_residual(cfg.proxy_mode_type, None)
            .await;
        assert_eq!(residual.lock().unwrap().len(), 1, "门闩：每会话仅一次");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旧起核世代的慢探测只丢结果，不得消费本会话门闩；否则随后的有效世代不会再提示。
    #[tokio::test]
    async fn stale_residual_probe_does_not_consume_session_latch() {
        let clearer: Box<dyn SystemProxyClearer> = Box::new(ResidualClearer {
            found: Some("10.0.0.1:1080".into()),
        });
        let (rt, dir, _f, residual) = test_runtime_recording_full(clearer);
        let mode = tun_user_config().proxy_mode_type;
        let stale_generation = rt.gate.generation().wrapping_add(1);

        rt.maybe_warn_system_proxy_residual(mode, Some(stale_generation))
            .await;
        assert!(residual.lock().unwrap().is_empty(), "陈旧世代不得发提示");

        rt.maybe_warn_system_proxy_residual(mode, None).await;
        assert_eq!(
            residual.lock().unwrap().as_slice(),
            ["10.0.0.1:1080"],
            "陈旧世代不得抢占本会话门闩"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非 TUN（系统代理模式）→ 绝不提示（系统代理模式下系统代理本就该开且是我们设的）。
    /// 变异锁：删 TUN 门控 → 系统代理模式也发 → 转红。
    #[tokio::test]
    async fn residual_not_emitted_when_not_tun() {
        let clearer: Box<dyn SystemProxyClearer> = Box::new(ResidualClearer {
            found: Some("10.0.0.1:1080".into()), // 即便检出也不该发
        });
        let (rt, dir, _f, residual) = test_runtime_recording_full(clearer);
        let mut cfg = tun_user_config();
        cfg.proxy_mode_type = polaris_config_engine::user_config::ProxyModeType::SystemProxy;
        rt.maybe_warn_system_proxy_residual(cfg.proxy_mode_type, None)
            .await;
        assert!(residual.lock().unwrap().is_empty(), "非 TUN 不提示");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TUN 但无残留（detect 返 None）→ 不发，但门闩已消耗（advisory 已「查过」）。
    #[tokio::test]
    async fn residual_none_when_no_foreign_proxy() {
        let clearer: Box<dyn SystemProxyClearer> = Box::new(ResidualClearer { found: None });
        let (rt, dir, _f, residual) = test_runtime_recording_full(clearer);
        rt.maybe_warn_system_proxy_residual(tun_user_config().proxy_mode_type, None)
            .await;
        assert!(residual.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 最小 TUN UserConfig（供 residual 决策测试）。
    fn tun_user_config() -> UserConfig {
        serde_json::from_value(serde_json::json!({
            "servers": [],
            "selectedServerId": "__direct__",
            "proxyMode": "smart",
            "proxyModeType": "tun"
        }))
        .expect("最小 TUN 配置应可解析")
    }

    /// advisory 必须只 spawn，不能再次 await 回起核主链。行为测试无法在无真核环境量墙钟，
    /// 因此用源码不变式锁住这条性能边界。
    #[test]
    fn system_proxy_residual_probe_never_blocks_start_inner() {
        let body = method_body(include_str!("proxy.rs"), "    async fn start_inner(");
        assert_eq!(
            body.matches("self.spawn_system_proxy_residual_warning(")
                .count(),
            1,
            "起核成功段必须恰好 spawn 一次残留探测"
        );
        assert!(
            !body.contains("self.maybe_warn_system_proxy_residual("),
            "残留提示只是 advisory，不得 await 回起核关键路径"
        );
    }

    /// 最小 systemProxy UserConfig（供 A1 启用侧决策测试）。
    fn systemproxy_user_config() -> UserConfig {
        serde_json::from_value(serde_json::json!({
            "servers": [],
            "selectedServerId": "__direct__",
            "proxyMode": "smart",
            "proxyModeType": "systemProxy"
        }))
        .expect("最小 systemProxy 配置应可解析")
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // A1 系统代理启用侧（最大缺口：systemProxy 模式 start 成功却从不设 OS 代理 → 流量不经核）
    //
    // 注：**start_inner 内的调用点**（wait_ready 成功后）无法在本机验证——它须真核就绪、而本机硬禁
    // 起核（同 residual 发射的约束，见其上方注释）。故此处覆盖 `maybe_enable_system_proxy` 的**全部
    // 决策 + 装配逻辑**（模式门控 / enable 真被调 / req 参数），start_inner 的单行调用点靠代码审查背书
    // （诚实披露，见报告）。enable 内部状态机（marker/防自指/fail-closed 回滚）另在
    // `system-integration::proxy_ops` 单测覆盖。
    // ══════════════════════════════════════════════════════════════════════════════

    /// 纯决策：仅 systemProxy 模式设 OS 系统代理（tun 走 TUN、manual 用户自管）。
    /// 变异锁：改成恒 true → `enable_not_called_for_tun_or_manual` 转红；恒 false →
    /// `enable_called_for_systemproxy_with_local_mixed_port` 转红。
    #[test]
    fn should_enable_system_proxy_only_for_systemproxy() {
        assert!(should_enable_system_proxy(ProxyModeType::SystemProxy));
        assert!(!should_enable_system_proxy(ProxyModeType::Tun));
        assert!(!should_enable_system_proxy(ProxyModeType::Manual));
    }

    /// C-tun-conflict 模式守卫：TUN 出口夺取硬闸**仅**适用 TUN 模式。
    /// systemProxy/manual 不接管 tun、出口恒在物理网卡 → baseline 差分永不成立，设闸必误判 → 不闸（caveat）。
    /// 变异锁：改成恒 true → systemProxy/manual 起核会被本不该有的闸拦（且 baseline/verify 空跑）。
    #[test]
    fn tun_route_gate_only_applies_to_tun_mode() {
        assert!(tun_route_gate_applies(ProxyModeType::Tun));
        assert!(!tun_route_gate_applies(ProxyModeType::SystemProxy));
        assert!(!tun_route_gate_applies(ProxyModeType::Manual));
    }

    /// systemProxy 成功腿 → `enable` 真被调，且 req = `127.0.0.1:mixedPort`（http+socks 同口）+ 生效 bypass。
    /// 变异锁：删掉 `maybe_enable_system_proxy` 里的 `g.enable_system_proxy(&req)` → 零 req → 转红。
    #[tokio::test]
    async fn enable_called_for_systemproxy_with_local_mixed_port() {
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let clearer: Box<dyn SystemProxyClearer> = Box::new(EnableRecordingClearer {
            enable_reqs: Arc::clone(&reqs),
            ..Default::default()
        });
        let (rt, dir, _f, _r) = test_runtime_recording_full(clearer);
        rt.maybe_enable_system_proxy(&systemproxy_user_config(), 7890)
            .await;
        let got = reqs.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "systemProxy 成功腿必调 enable 一次");
        assert_eq!(got[0].address, "127.0.0.1", "本机应用经 loopback 连本地核");
        assert_eq!(got[0].http_port, 7890, "http 指向 mixedPort");
        assert_eq!(
            got[0].socks_port, 7890,
            "socks 同口 mixedPort（mixed 入站同口服务）"
        );
        // bypass 复用 config-engine 生效清单（缺省补 DEFAULT_BYPASS_LAN 的 27 条，含 loopback 段）。
        assert!(
            got[0].bypass_list.contains(&"127.0.0.0/8".to_string()),
            "bypass 应含默认私网/保留段，实得 {:?}",
            got[0].bypass_list
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// tun / manual 模式绝不设 OS 系统代理（tun 走 TUN 接管、manual 用户自管）。
    /// 变异锁：删 `should_enable_system_proxy` 门控 → 这些模式也调 enable → 转红。
    #[tokio::test]
    async fn enable_not_called_for_tun_or_manual() {
        for mode in [ProxyModeType::Tun, ProxyModeType::Manual] {
            let reqs = Arc::new(Mutex::new(Vec::new()));
            let clearer: Box<dyn SystemProxyClearer> = Box::new(EnableRecordingClearer {
                enable_reqs: Arc::clone(&reqs),
                ..Default::default()
            });
            let (rt, dir, _f, _r) = test_runtime_recording_full(clearer);
            let mut cfg = systemproxy_user_config();
            cfg.proxy_mode_type = mode;
            rt.maybe_enable_system_proxy(&cfg, 7890).await;
            assert!(
                reqs.lock().unwrap().is_empty(),
                "{mode:?} 模式绝不设系统代理"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // A1 **失败腿**必须冒给用户（此前只 log::error! → 用户见「已连接」绿灯 + 全量直连 + 零提示）
    // ══════════════════════════════════════════════════════════════════════════════

    /// enable 恒失败的 clearer（不触碰宿主系统代理，本机硬约束）。
    struct FailingEnableClearer;
    impl SystemProxyClearer for FailingEnableClearer {
        fn ensure_cleared(&mut self) -> bool {
            false
        }
        fn detect_foreign_proxy(&self) -> Option<String> {
            None
        }
        fn enable_system_proxy(&mut self, _req: &ProxyEnableRequest) -> Result<(), String> {
            Err("networksetup 退出码 1".to_string())
        }
        fn recover_from_marker(&mut self) -> bool {
            false
        }
    }

    /// 装 mock emitter + 可注入 clearer，暴露**错误事件**记录句柄（A1 失败腿 / 出口自证共用）。
    fn test_runtime_errors_with_clearer(
        clearer: Box<dyn SystemProxyClearer>,
    ) -> (Arc<ProxyRuntime>, PathBuf, ErrorEvents) {
        let dir = fresh_test_dir();
        let config = Arc::new(ConfigManager::new(dir.clone()));
        let helper = Arc::new(HelperRuntime::never_installed_for_tests(dir.clone()));
        let mesh = Arc::new(MeshRuntime::new(dir.clone()));
        let rt = Arc::new(ProxyRuntime::new(
            config,
            helper,
            mesh,
            clearer,
            Arc::new(NoNetworkDoh),
        ));
        let events: ErrorEvents = Arc::new(Mutex::new(Vec::new()));
        rt.set_error_emitter(Box::new(RecordingErrorEmitter {
            events: Arc::clone(&events),
            ..Default::default()
        }));
        (rt, dir, events)
    }

    /// **A1 失败 → 真 emit `event:proxyError`（SYSTEM_PROXY_FAILED）**，不再静默。
    /// 变异锁：把失败腿改回只 `log::error!` → 零事件 → 转红（退回本 bug）。
    #[tokio::test]
    async fn a1_enable_failure_emits_proxy_error() {
        let (rt, dir, events) = test_runtime_errors_with_clearer(Box::new(FailingEnableClearer));
        mark_running(&rt);
        rt.maybe_enable_system_proxy(&systemproxy_user_config(), 7890)
            .await;
        let got = events.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "A1 失败必须发一条 proxyError，实得 {got:?}");
        assert_eq!(got[0].1, code::SYSTEM_PROXY_FAILED, "错误码须可分类");
        assert!(
            got[0].0.contains("系统代理启用失败") && got[0].0.contains("直连"),
            "文案须让用户看懂「流量没走代理」，实得 {}",
            got[0].0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A1 失败是非终态**：核确在跑 → 绝不把状态抹成 not-running（虚报同样有害），
    /// 但 error/errorCode 必须落进状态（前端拉 status 也看得到）。
    /// 变异锁：把 `set_nonfatal_error` 换成 `set_error` → running/pid/端口全被 `default()` 抹掉 → 转红。
    #[tokio::test]
    async fn a1_enable_failure_keeps_core_running_state() {
        let (rt, dir, _events) = test_runtime_errors_with_clearer(Box::new(FailingEnableClearer));
        mark_running(&rt);
        rt.maybe_enable_system_proxy(&systemproxy_user_config(), 7890)
            .await;
        let s = rt.status();
        assert!(s.running, "核确在跑 → 绝不因系统代理失败标成未运行（虚报）");
        assert_eq!(
            s.pid, 424242,
            "pid 不得被抹（抹了则停核/管理 API/统计全失联）"
        );
        assert_eq!(s.clash_api_port, 19090, "管理 API 端口不得被抹");
        assert_eq!(s.error_code.as_deref(), Some(code::SYSTEM_PROXY_FAILED));
        assert!(s.error.is_some(), "错误文案须落进状态");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A1 **成功**腿绝不告警（告警一旦有假就会被整体无视）。
    /// 变异锁：把 emit 挪到 match 之外（无条件发）→ 转红。
    #[tokio::test]
    async fn a1_enable_success_emits_nothing() {
        let clearer: Box<dyn SystemProxyClearer> = Box::new(EnableRecordingClearer::default());
        let (rt, dir, events) = test_runtime_errors_with_clearer(clearer);
        mark_running(&rt);
        rt.maybe_enable_system_proxy(&systemproxy_user_config(), 7890)
            .await;
        assert!(
            events.lock().unwrap().is_empty(),
            "成功腿不得告警，实得 {:?}",
            events.lock().unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // 出口自证：「实际生效出口 == 选中节点」
    //
    // 判据是**纯静态**的（核实际启动的那份 sing-box config vs 用户落盘意图），故**全部可本机断言**，
    // 无需起核、不碰网络——这正是选静态对账而非探针的第二个收益（探针路径根本没法在 gate 里验）。
    // ══════════════════════════════════════════════════════════════════════════════

    /// 造 sing-box config：`route.final` = `final_tag`；有 `selector_default` 则装 proxy-selector。
    fn singbox_fixture(final_tag: &str, selector_default: Option<&str>) -> SingBoxConfig {
        let mut outbounds = vec![
            serde_json::json!({ "type": "direct", "tag": "direct" }),
            serde_json::json!({ "type": "shadowsocks", "tag": "HK01" }),
            serde_json::json!({ "type": "shadowsocks", "tag": "JP01" }),
        ];
        if let Some(d) = selector_default {
            outbounds.push(serde_json::json!({
                "type": "selector", "tag": PROXY_SELECTOR_TAG,
                "outbounds": ["HK01", "JP01", "direct"], "default": d
            }));
        }
        serde_json::from_value(serde_json::json!({
            "log": { "level": "info", "timestamp": true },
            "inbounds": [],
            "outbounds": outbounds,
            "route": { "rules": [], "final": final_tag }
        }))
        .expect("fixture sing-box config 应可解析")
    }

    /// 造 UserConfig：两个节点（HK01/JP01），选中 `selected`。
    fn exit_user_config(selected: &str) -> UserConfig {
        serde_json::from_value(serde_json::json!({
            "servers": [
                { "id": "n-hk", "name": "HK01", "protocol": "shadowsocks",
                  "address": "1.2.3.4", "port": 8388 },
                { "id": "n-jp", "name": "JP01", "protocol": "shadowsocks",
                  "address": "5.6.7.8", "port": 8388 }
            ],
            "selectedServerId": selected,
            "proxyMode": "smart",
            "proxyModeType": "systemProxy"
        }))
        .expect("fixture UserConfig 应可解析")
    }

    /// 健康形态：选中 HK01 + selector default=HK01 → 自证通过。
    /// 变异锁：把 `Match` 腿改成恒告警 → 转红（假阳性会让告警整体失信）。
    #[test]
    fn attest_match_when_selector_default_is_selected_node() {
        let got = attest_effective_exit(
            &exit_user_config("n-hk"),
            &singbox_fixture(PROXY_SELECTOR_TAG, Some("HK01")),
            Some("n-hk"),
        );
        assert_eq!(got, ExitAttestation::Match);
    }

    /// **本 bug 的核心形态**：选中真实节点，selector 却降级到 direct → 明文直连，必须判 SilentDirect。
    /// 变异锁：把 `actual == DIRECT_TAG` 腿删掉（落进 WrongExit）→ 转红（丢掉「未加密」这一最高危语义）。
    #[test]
    fn attest_silent_direct_when_selector_defaults_to_direct() {
        let got = attest_effective_exit(
            &exit_user_config("n-hk"),
            &singbox_fixture(PROXY_SELECTOR_TAG, Some(DIRECT_TAG)),
            Some("n-hk"),
        );
        assert_eq!(
            got,
            ExitAttestation::SilentDirect {
                expected_tag: "HK01".into()
            }
        );
        assert!(
            got.user_message().contains("直连") && got.user_message().contains("未加密"),
            "文案必须点明「未加密」，这是用户唯一在意的事实：{}",
            got.user_message()
        );
    }

    /// `route.final=direct`（mesh 出口回落 / outbounds 兜底等路径）→ 同样是明文直连。
    /// 变异锁：只查 selector default、不解 `route.final` → 本测转红（漏掉整条 final 轴）。
    #[test]
    fn attest_silent_direct_when_route_final_is_direct() {
        let got = attest_effective_exit(
            &exit_user_config("n-hk"),
            &singbox_fixture(DIRECT_TAG, Some("HK01")),
            Some("n-hk"),
        );
        assert_eq!(
            got,
            ExitAttestation::SilentDirect {
                expected_tag: "HK01".into()
            },
            "final=direct 时 selector 里装的是谁都无关——流量根本不经 selector"
        );
    }

    /// 走错节点（selector default 指向另一个节点）→ WrongExit（仍加密，但不是用户选的出口）。
    #[test]
    fn attest_wrong_exit_when_selector_points_to_other_node() {
        let got = attest_effective_exit(
            &exit_user_config("n-hk"),
            &singbox_fixture(PROXY_SELECTOR_TAG, Some("JP01")),
            Some("n-hk"),
        );
        assert_eq!(
            got,
            ExitAttestation::WrongExit {
                expected_tag: "HK01".into(),
                actual_tag: "JP01".into()
            }
        );
    }

    /// **前端竞态（S4）真机现象的静态复现**：落盘意图 = HK01，起核却用了 `__direct__` 旧值。
    /// 此腿下 config 内部完全自洽（selector default 确是 direct），只有与落盘意图对账才能拆穿 →
    /// 变异锁：删掉 persisted 对账腿 → 落进 `Match`（因为 `is_direct_selection` 放行）→ 转红。
    /// 这正是「配置自洽于一个错的意图」的假绿，是本 bug 最难抓的一条。
    #[test]
    fn attest_stale_selection_when_renderer_passed_old_direct_sentinel() {
        let mut cfg = exit_user_config("n-hk");
        cfg.selected_server_id = Some(DIRECT_SERVER_ID.to_string()); // 渲染端传来的陈旧快照
        let got = attest_effective_exit(&cfg, &singbox_fixture(DIRECT_TAG, None), Some("n-hk"));
        assert_eq!(
            got,
            ExitAttestation::StaleSelection {
                persisted: "n-hk".into(),
                started_with: DIRECT_SERVER_ID.into()
            }
        );
    }

    /// 用户**自己**选了直连 → 出口是 direct 本就正确，不得告警。
    /// 变异锁：删 `is_direct_selection` 放行腿 → 转红（对用户自选直连天天误报）。
    #[test]
    fn attest_match_when_user_selected_direct() {
        let got = attest_effective_exit(
            &exit_user_config(DIRECT_SERVER_ID),
            &singbox_fixture(DIRECT_TAG, None),
            Some(DIRECT_SERVER_ID),
        );
        assert_eq!(got, ExitAttestation::Match);
    }

    /// 设计语义放行①：`proxyMode=direct`（全直连模式）→ final=direct 是用户选的，不告警。
    /// 变异锁：删门① → 转红。
    #[test]
    fn attest_match_for_direct_proxy_mode() {
        let mut cfg = exit_user_config("n-hk");
        cfg.proxy_mode = ProxyMode::Direct;
        let got = attest_effective_exit(&cfg, &singbox_fixture(DIRECT_TAG, None), Some("n-hk"));
        assert_eq!(got, ExitAttestation::Match);
    }

    /// 造带 `route.rule_set` 定义的 fixture（tags = 已注入的 rule_set tag）。
    fn singbox_fixture_with_rule_sets(final_tag: &str, tags: &[&str]) -> SingBoxConfig {
        let rule_sets: Vec<serde_json::Value> = tags
            .iter()
            .map(|t| {
                serde_json::json!({
                    "tag": t, "type": "local", "format": "binary",
                    "path": format!("/fake/rules/{t}.srs")
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "log": { "level": "info", "timestamp": true },
            "inbounds": [],
            "outbounds": [
                { "type": "direct", "tag": "direct" },
                { "type": "shadowsocks", "tag": "HK01" },
                { "type": "shadowsocks", "tag": "JP01" }
            ],
            "route": { "rules": [], "final": final_tag, "rule_set": rule_sets }
        }))
        .expect("fixture sing-box config 应可解析")
    }

    /// 造 smart + 回国（reverse）的 UserConfig。
    fn reverse_cn_user_config(selected: &str) -> UserConfig {
        let mut cfg = exit_user_config(selected);
        cfg.region_routing = Some(
            serde_json::from_value(serde_json::json!({
                "enabled": true, "region": "cn", "reverse": true
            }))
            .expect("region fixture 应可解析"),
        );
        cfg
    }

    /// 设计语义放行②：smart + 地区反向（回国：海外直连）**且规则集完整** → final=direct 是设计语义，不告警。
    /// 变异锁：删门② → 转红（回国模式每次起核都误报）。
    #[test]
    fn attest_match_for_smart_region_reverse() {
        // 回国模式的「→代理」腿（geosite-cn / geoip-cn）rule_set 定义俱在 = 规则集完整。
        let cfg = reverse_cn_user_config("n-hk");
        let sb = singbox_fixture_with_rule_sets(DIRECT_TAG, &["geosite-cn", "geoip-cn"]);
        assert_eq!(
            attest_effective_exit(&cfg, &sb, Some("n-hk")),
            ExitAttestation::Match
        );
        // 反向关掉 → 同一份 config 必须重新告警（证明放行是 `reverse` 驱动、不是恒放行）。
        let mut off = cfg.clone();
        off.region_routing.as_mut().unwrap().reverse = false;
        assert!(
            matches!(
                attest_effective_exit(&off, &sb, Some("n-hk")),
                ExitAttestation::SilentDirect { .. }
            ),
            "reverse=false 时 final=direct 就是真降级，必须告警"
        );
    }

    /// **门② 收紧（T4）**：reverse **但规则集缺失** → 回国模式已退化成全量明文直连，是真故障，
    /// **不得**被白名单放行。这正是真机 2026-07-20「零告警 + 日志还打『出口自证通过』」的成因。
    ///
    /// ⚠️ **按构造不可达（与 M2 同类）**：本用例喂的是**手工构造**的 config。生产链路上同场景会先被
    /// `builder/route.rs` 的 T2 fail-safe 把 `final` 翻成 `proxy-selector`，走不到这条腿——详见
    /// `attest_effective_exit` 门② 上方的「不可达性登记」。保留理由是 defense-in-depth，
    /// **不是**「真机能复现」。别据此写真机验收门。
    ///
    /// 变异锁：删 `region_reverse_rule_sets_intact` 前置条件（退回旧的「只看 reverse」粒度）→ 转红。
    #[test]
    fn attest_mismatch_for_reverse_with_missing_rule_sets() {
        let cfg = reverse_cn_user_config("n-hk");
        // rule_set 全缺（真机现场：磁盘零 .srs → 一个都没注入）。
        let sb_none = singbox_fixture_with_rule_sets(DIRECT_TAG, &[]);
        assert!(
            matches!(
                attest_effective_exit(&cfg, &sb_none, Some("n-hk")),
                ExitAttestation::SilentDirect { .. }
            ),
            "规则集全缺 + reverse + final=direct = 全量明文直连，必须告警而非放行"
        );

        // **部分缺失同样不放行**：只剩 geosite-cn，geoip-cn 没了 → 国内 IP 段不再回国。
        let sb_partial = singbox_fixture_with_rule_sets(DIRECT_TAG, &["geosite-cn"]);
        assert!(
            matches!(
                attest_effective_exit(&cfg, &sb_partial, Some("n-hk")),
                ExitAttestation::SilentDirect { .. }
            ),
            "回国的两条 →代理 腿缺任意一条都算不完整（变异：把 all() 写成 any() → 此断言转红）"
        );
    }

    /// 门② 前置谓词自身的边界：越界 region（手改 JSON）→ 判不准 → **不放行**（fail-safe）。
    /// 变异锁：把 `region_local_geo` 返 None 的腿改成 `true` → 转红。
    #[test]
    fn reverse_rule_sets_intact_is_false_for_unknown_region() {
        let mut cfg = exit_user_config("n-hk");
        cfg.region_routing = Some(
            serde_json::from_value(serde_json::json!({
                "enabled": true, "region": "atlantis", "reverse": true
            }))
            .expect("region fixture 应可解析"),
        );
        // 即便 rule_set 里塞满 CN 三件套，未知 region 也解不出「该有哪些腿」→ 判定不完整。
        let sb = singbox_fixture_with_rule_sets(DIRECT_TAG, &["geosite-cn", "geoip-cn"]);
        assert!(!region_reverse_rule_sets_intact(&cfg, &sb));
        assert!(
            matches!(
                attest_effective_exit(&cfg, &sb, Some("n-hk")),
                ExitAttestation::SilentDirect { .. }
            ),
            "判不准就告警，不静默放行"
        );
    }

    /// 无 `route.final` → 解不出出口 = 无法自证 → 按「不确定即告警」处理，不静默放行。
    /// 变异锁：把 `None` 腿改成 `Match` → 转红（「解不出」被当成「没问题」是最典型的假绿）。
    #[test]
    fn attest_unresolved_when_no_route_final() {
        let sb: SingBoxConfig = serde_json::from_value(serde_json::json!({
            "log": { "level": "info", "timestamp": true },
            "inbounds": [],
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": { "rules": [] }
        }))
        .expect("无 final 的 fixture 应可解析");
        assert_eq!(
            attest_effective_exit(&exit_user_config("n-hk"), &sb, Some("n-hk")),
            ExitAttestation::UnresolvedExit {
                expected_tag: "HK01".into()
            }
        );
    }

    /// 选中 id 不在节点表 → UnknownSelection（兜底可见性，不静默）。
    #[test]
    fn attest_unknown_selection_for_missing_id() {
        let mut cfg = exit_user_config("n-hk");
        cfg.selected_server_id = Some("ghost".into());
        assert_eq!(
            attest_effective_exit(
                &cfg,
                &singbox_fixture(PROXY_SELECTOR_TAG, Some("HK01")),
                None
            ),
            ExitAttestation::UnknownSelection {
                selected_id: "ghost".into()
            }
        );
    }

    /// 落盘「用户已提交的选中意图」。**基于 `current()` 的真实默认配置改**（而非手搓最小 JSON）——
    /// `save_full` 会跑完整 sanitize+validate，手搓必缺字段；基于默认配置改也更贴近真实落盘形态。
    fn persist_selection(rt: &ProxyRuntime, selected_id: &str) {
        let mut cfg = rt.config.current().expect("默认配置应可读");
        cfg["servers"] = serde_json::json!([
            { "id": "n-hk", "name": "HK01", "protocol": "shadowsocks",
              "address": "1.2.3.4", "port": 8388 }
        ]);
        cfg["selectedServerId"] = serde_json::json!(selected_id);
        rt.config.save_full(&cfg).expect("落盘测试配置应成功");
    }

    /// **组合路径**：不一致 → `attest_selected_exit` 真 emit `event:proxyError`（EXIT_MISMATCH），
    /// 且**不把核标成未运行**（核确在跑）。§K7.1：光测纯函数、光测 emit 都不够，要测组合。
    /// 变异锁：把 `attest_selected_exit` 的告警腿改成 `log::warn!` → 零事件 → 转红（退回静默）。
    #[tokio::test]
    async fn attest_selected_exit_emits_and_keeps_running() {
        let (rt, dir, events) =
            test_runtime_errors_with_clearer(Box::new(EnableRecordingClearer::default()));
        mark_running(&rt);
        // 落盘意图 = n-hk（用户点过的那一下）。
        persist_selection(&rt, "n-hk");
        // 核实际起来的配置：selector 降级到 direct → 明文直连。
        rt.attest_selected_exit(
            &exit_user_config("n-hk"),
            &singbox_fixture(PROXY_SELECTOR_TAG, Some(DIRECT_TAG)),
        );
        let got = events.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            1,
            "出口不一致必须发一条 proxyError，实得 {got:?}"
        );
        assert_eq!(got[0].1, code::EXIT_MISMATCH);
        assert!(rt.status().running, "核确在跑 → 不得标成未运行");
        assert_eq!(rt.status().error_code.as_deref(), Some(code::EXIT_MISMATCH));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一致 → **零告警**（假阳性会让整条告警通道失信）。
    /// 变异锁：把告警改成无条件发 → 转红。
    #[tokio::test]
    async fn attest_selected_exit_silent_when_consistent() {
        let (rt, dir, events) =
            test_runtime_errors_with_clearer(Box::new(EnableRecordingClearer::default()));
        mark_running(&rt);
        persist_selection(&rt, "n-hk");
        rt.attest_selected_exit(
            &exit_user_config("n-hk"),
            &singbox_fixture(PROXY_SELECTOR_TAG, Some("HK01")),
        );
        assert!(
            events.lock().unwrap().is_empty(),
            "出口一致不得告警，实得 {:?}",
            events.lock().unwrap()
        );
        assert!(rt.status().error_code.is_none(), "一致时不得落错误码");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **T3 组合路径**：规则集被剪枝 → 真 emit `event:proxyError`（`RULE_RESOURCES_MISSING`），
    /// 且**不把核标成未运行**（核确在跑，只是分流退化）。
    /// 变异锁：把 `warn_pruned_rule_resources` 的告警腿改成 `log::warn!` → 零事件 → 转红（退回静默）。
    #[tokio::test]
    async fn pruned_rule_resources_emit_and_keep_running() {
        let (rt, dir, events) =
            test_runtime_errors_with_clearer(Box::new(EnableRecordingClearer::default()));
        mark_running(&rt);

        rt.warn_pruned_rule_resources(&["geosite-cn".to_string(), "geoip-cn".to_string()]);

        let got = events.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            1,
            "规则被剪枝必须发一条 proxyError，实得 {got:?}"
        );
        assert_eq!(got[0].1, code::RULE_RESOURCES_MISSING);
        assert!(
            got[0].0.contains("geosite-cn"),
            "文案应点名缺失的资源：{}",
            got[0].0
        );
        assert!(rt.status().running, "核确在跑 → 不得标成未运行");
        assert_eq!(
            rt.status().error_code.as_deref(),
            Some(code::RULE_RESOURCES_MISSING)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 资源齐全（剪枝清单为空）→ **零告警**。任务硬约束：「别在资源齐全时噪音」。
    /// 变异锁：删 `if pruned.is_empty() { return; }` 早退 → 每次起核都弹一条空名单告警 → 转红。
    #[tokio::test]
    async fn intact_rule_resources_emit_nothing() {
        let (rt, dir, events) =
            test_runtime_errors_with_clearer(Box::new(EnableRecordingClearer::default()));
        mark_running(&rt);

        rt.warn_pruned_rule_resources(&[]);

        assert!(
            events.lock().unwrap().is_empty(),
            "资源齐全不得告警，实得 {:?}",
            events.lock().unwrap()
        );
        assert!(rt.status().error_code.is_none(), "齐全时不得落错误码");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C1 启动期恢复：`recover_system_proxy_on_startup` 真调到 controller 的 `recover_from_marker`。
    /// 变异锁：删 `maybe_enable_system_proxy`... 不——删 `recover_system_proxy_on_startup` 里的
    /// `g.recover_from_marker()` → recover_calls=0 且回传 false → 转红。
    #[tokio::test]
    async fn startup_recovery_invokes_recover_from_marker() {
        let recover_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let clearer: Box<dyn SystemProxyClearer> = Box::new(EnableRecordingClearer {
            recover_calls: Arc::clone(&recover_calls),
            ..Default::default()
        });
        let (rt, dir, _f, _r) = test_runtime_recording_full(clearer);
        let recovered = rt.recover_system_proxy_on_startup().await;
        assert_eq!(
            recover_calls.load(Ordering::SeqCst),
            1,
            "启动期必调 recover_from_marker 一次"
        );
        assert!(
            recovered,
            "mock recover 返 true → 方法回传 true（真恢复过）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 用户主动 `stop` 绝不发 `event:proxyError`（正常终态，不是错误）——防「停一次代理报一次错」。
    /// 打断（把 emit 挪到 stop_inner / status 清空处）→ 本测转红。
    #[tokio::test]
    async fn active_stop_emits_no_proxy_error() {
        let (rt, dir, events) = test_runtime_recording_errors();
        rt.stop().await.unwrap();
        assert!(
            events.lock().unwrap().is_empty(),
            "主动停止是达成用户意图的终态，绝不该报 proxyError"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── P1-b：起核收口腿必须让 daemon 停掉它自己的受管 child ──────────────────────────

    use std::sync::atomic::AtomicUsize;

    /// 可观测的 [`HelperStopOps`] 替身：记调用次数 + 每次带的身份 pid，并可被指定成失败腿。
    ///
    /// `during_call` 在「IPC 往返中」执行 —— 用来**确定性**地复现「停核请求在飞、期间新会话起了新核」
    /// 那条时序（真机上它是 helper 无响应 + 用户重装 helper 的窗口，靠 sleep 撞不出来）。
    struct RecordingStop {
        calls: Arc<AtomicUsize>,
        wants: Arc<Mutex<Vec<Option<u32>>>>,
        result: Result<(), String>,
        during_call: Option<Box<dyn Fn() + Send + Sync>>,
    }
    type StopProbe = (
        Arc<RecordingStop>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<Option<u32>>>>,
    );
    impl RecordingStop {
        fn new(result: Result<(), String>) -> StopProbe {
            Self::with_hook(result, None)
        }
        fn with_hook(
            result: Result<(), String>,
            during_call: Option<Box<dyn Fn() + Send + Sync>>,
        ) -> StopProbe {
            let calls = Arc::new(AtomicUsize::new(0));
            let wants = Arc::new(Mutex::new(Vec::new()));
            let ops = Arc::new(Self {
                calls: Arc::clone(&calls),
                wants: Arc::clone(&wants),
                result,
                during_call,
            });
            (ops, calls, wants)
        }
    }
    impl HelperStopOps for RecordingStop {
        fn stop_managed_core(&self, want_pid: Option<u32>) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.wants.lock().unwrap().push(want_pid);
            if let Some(f) = self.during_call.as_ref() {
                f();
            }
            self.result.clone()
        }
    }

    // ─── 停核的受管 pid 身份：app 侧下发 + 记账收口 ────────────────────────────────

    /// **变异门（下发侧）**：helper 停核腿必须把「本腿意图停的那个 pid」**随请求带下去**。
    ///
    /// 判据只能在 helper 进程里执行（真正杀进程的是它），app 不下发 = 判据永远拿不到 want =
    /// daemon 退回「反正要停就杀当前的」。
    ///
    /// 变异（逃逸面穷举）：
    /// - `stop_managed_core(intended)` 改回 `stop_managed_core(None)` → 首条断言转红。
    /// - 把 `let intended = ...` 挪到 await **之后**再读 → 读到的是新会话的 pid → 首条转红
    ///   （那等于把「我要停谁」交给接管方决定）。
    #[tokio::test]
    async fn helper_stop_leg_sends_the_pid_it_intends_to_stop() {
        let (rt, dir) = test_runtime();
        *rt.pid.lock().unwrap() = Some(4242);
        rt.core_via_helper.store(true, Ordering::SeqCst);
        let (ops, calls, wants) = RecordingStop::new(Ok(()));

        rt.kill_core_via_helper(ops as Arc<dyn HelperStopOps>).await;

        assert_eq!(
            *wants.lock().unwrap(),
            vec![Some(4242)],
            "停核请求必须携带受管 pid 身份 —— 这是 helper 侧唯一能据以拒杀的依据"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "恰调一次");
        // 无人接管 → 记账照常清（反向失效：留着会让下次 kill_core 走错腿）。
        assert!(rt.pid.lock().unwrap().is_none());
        assert!(!rt.core_via_helper.load(Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **变异门（记账侧）**：IPC 往返期间受管 pid 记账被新会话换人 → 收口腿**不得**清它。
    ///
    /// 清了不是「多清一次」而是让新核**失联**：`status()` 的 helper 腿据 `pid` 探活、诊断据它报 pid、
    /// `cleanup_stale_cores` 的「受管 pid 排除表」也据它 —— 排除表里少了新核，下一次起核的孤儿清扫
    /// 就把它当孤儿杀掉（换个地方杀错进程）；`core_via_helper` 被清则让此后的停核走本地 child 腿
    /// （child 恒 None）= 停核变 no-op = root 孤儿。
    ///
    /// 变异：`clear_helper_core_bookkeeping` 退回无条件 `*g = None; store(false)` → 两条断言全红。
    #[tokio::test]
    async fn helper_stop_leg_does_not_wipe_bookkeeping_taken_over_mid_flight() {
        let (rt, dir) = test_runtime();
        *rt.pid.lock().unwrap() = Some(4242);
        rt.core_via_helper.store(true, Ordering::SeqCst);
        // 「IPC 在飞时新会话起了新核并提交 pid」——真机上这正是老 stop 腿醒来后会杀错人的那一刻。
        let pid_slot = Arc::clone(&rt.pid);
        let (ops, _calls, wants) = RecordingStop::with_hook(
            Ok(()),
            Some(Box::new(move || {
                *pid_slot.lock().unwrap() = Some(9001);
            })),
        );

        rt.kill_core_via_helper(ops as Arc<dyn HelperStopOps>).await;

        assert_eq!(
            *wants.lock().unwrap(),
            vec![Some(4242)],
            "下发的身份仍是老腿意图停的那个（不是接管方的）"
        );
        assert_eq!(
            *rt.pid.lock().unwrap(),
            Some(9001),
            "新会话的受管 pid 记账必须原样保留 —— 清它 = 新核在 status/诊断/孤儿清扫排除表里集体失联"
        );
        assert!(
            rt.core_via_helper.load(Ordering::SeqCst),
            "helper 受管标记同样属新会话：清它会让此后的停核走本地 child 腿（child 恒 None）= 停核变 no-op"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **变异门（逃逸面穷举）**：探活判死的收口腿**必须**调 daemon stop，且**恰调一次**。
    ///
    /// - 删掉 `spawn_blocking(stop_managed_core)` → calls==0 → 转红（这就是孤儿的成因）。
    /// - 改成循环/重复调用 → calls!=1 → 转红（重复停核会误伤后续世代的核）。
    /// - 把返回消息改掉丢了 pid → 末条断言转红（用户拿不到可 `sudo kill` 的 pid）。
    /// - 把 `stop_managed_core(Some(pid))` 退回不带身份的 `None` → 身份断言转红（那等于让 daemon
    ///   「停它此刻手里的随便哪个」，本方法整段可与新会话并发 ⇒ 杀错进程）。
    #[tokio::test]
    async fn rejected_helper_start_asks_daemon_to_stop_its_child() {
        let (ops, calls, wants) = RecordingStop::new(Ok(()));
        let msg = ProxyRuntime::reject_helper_start(ops, 6439).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "探活判死时必须请 daemon 收口它自己的受管 child，恰一次——否则活着的 root 核就此失联成孤儿"
        );
        assert_eq!(
            *wants.lock().unwrap(),
            vec![Some(6439)],
            "收口请求必须**指名道姓**停那个 pid：不带身份 = 授权 daemon 杀它当前受管的任何核，\
             而这条腿完全可能与新会话的起核并发"
        );
        assert!(msg.contains("6439"), "失败消息须带 pid，用户才可能手动收拾");
    }

    /// **反向失效门**：stop 失败**不得**改判成功、也不得吞掉错误消息。
    ///
    /// 核确实可能真死了（那时 daemon stop 返 notrunning/错误是正常的），故这条腿是 best-effort：
    /// 打断（stop 返 Err 时改成 `return Ok`/返回空串/panic）→ 本测转红。
    #[tokio::test]
    async fn reject_leg_still_reports_failure_when_daemon_stop_errors() {
        let (ops, calls, _wants) = RecordingStop::new(Err("daemon 说 notrunning".to_owned()));
        let msg = ProxyRuntime::reject_helper_start(ops, 777).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "失败腿也必须真尝试过 stop");
        assert!(
            msg.contains("777") && msg.contains("进程不存在"),
            "stop 失败不改判：起核失败的结论与消息原样返回，不得被 stop 的结果污染"
        );
    }

    /// **P1-a 不变式门（有牙版）**：**每一次** `start` 都必须走 stale 清扫腿，不是只走首次。
    ///
    /// 直接驱动**两次真 start** 并数清扫实跑次数——不是读那个开关（读开关的写法对
    /// `swap(true)` 一次性门闩免疫 = 没门）。
    ///
    /// **变异门（逃逸面穷举）**：
    /// - 调用点退回 `swap(true, ...)` 一次性门闩 → 第二次 start 不清扫 → runs==1 → 转红。
    /// - 删掉整个清扫调用 → runs==0 → 转红。
    /// - 把计数挪到 `resolve_core_binary` 成功之后 → 本测（核不可解析）恒 0 → 转红。
    ///
    /// **本机零副作用**：`POLARIS_SINGBOX_PATH` 指向目录 → `resolve_core_binary` 必 Err → 清扫在
    /// 计数后立刻早退，**不扫 /proc、不发任何信号**。
    // 跨 await 持 `ENV_LOCK`：同 `start_emits_invalid_nodes_on_real_start_path`，见该测说明。
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn stale_sweep_runs_on_every_start_not_only_the_first() {
        let (rt, dir) = test_runtime();
        assert!(
            !rt.stale_sweep_disabled.load(Ordering::SeqCst),
            "生产默认必须开启清扫（该开关仅单测置位）"
        );
        // 端口解析都到不了就失败的最小配置：本测只关心清扫腿被走到几次。
        let config = serde_json::json!({ "servers": [], "proxyModeType": "systemProxy" });

        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("POLARIS_SINGBOX_PATH", &dir);
        let first = rt.start(config.clone()).await;
        assert_eq!(
            rt.stale_sweep_runs.load(Ordering::SeqCst),
            1,
            "首次 start 必清扫一次"
        );
        let second = rt.start(config).await;
        std::env::remove_var("POLARIS_SINGBOX_PATH");
        drop(_g);

        assert!(
            first.is_err() && second.is_err(),
            "核二进制解析失败 → 两次均失败"
        );
        assert_eq!(
            rt.stale_sweep_runs.load(Ordering::SeqCst),
            2,
            "第二次 start 也必须清扫——一次性门闩会让本会话中途产生的孤儿永远落在射程外，\
             那正是真机把用户卡死的放大器"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── T1：pid 探活的 errno 语义（真机 TUN 卡死链的判定侧根因）────────────────────────

    /// **变异门①（复现缺陷）**：`EPERM` 必须判**存活**。
    ///
    /// 把 [`alive_from_probe`] 退回成 `r.is_ok()` → EPERM 落进 false → 本测转红。那正是真机
    /// 「helper 报告已启动但进程不存在」的判定侧根因：helper 以 root 起核，app 以普通用户
    /// `kill(pid,0)` 探活收 EPERM（进程活得好好的，只是没权限发信号）。
    ///
    /// **变异门②（反向失效）**：`ESRCH` 必须判**不存活**。
    /// 把 `Err(_) => true` 写成无条件 true（改过头，连 ESRCH 也算活）→ 本测转红。
    /// 没有这一半，崩溃监测就永远发现不了核真的死了，孤儿也永远清不掉。
    #[cfg(unix)] // 用 nix::errno / alive_from_probe（均 unix-only），windows 排除
    #[test]
    fn alive_probe_treats_eperm_as_alive_and_only_esrch_as_dead() {
        use nix::errno::Errno;
        assert!(alive_from_probe(Ok(())), "有权发信号且进程在 → 存活");
        assert!(
            alive_from_probe(Err(Errno::EPERM)),
            "[变异门①] EPERM = 进程存在但不属本用户（root 核）→ 必须判存活"
        );
        assert!(
            !alive_from_probe(Err(Errno::ESRCH)),
            "[变异门②] ESRCH = 内核确认无此进程 → 唯一的不存活判据"
        );
        // 其余 errno 不是死亡证据 → 保守判活（绝不据此宣告核已崩）。
        assert!(alive_from_probe(Err(Errno::EINVAL)), "非死亡证据 → 判存活");
    }

    /// 端到端接线：真跑 `kill(pid,0)` 三种现实情形，锁死 [`pid_alive`] 确实用了新判据。
    ///
    /// **pid 1**（launchd/systemd）是现成的 **root 且非本用户**进程 —— 正是 helper 起的 root 核那一类。
    /// 打断（`pid_alive` 绕开 `alive_from_probe` 直接 `.is_ok()`）→ 非 root 运行时本测转红。
    #[cfg(unix)] // 用 nix::sys::signal::kill / nix::unistd::Pid（unix-only），windows 排除
    #[test]
    fn pid_alive_reports_root_owned_process_as_alive() {
        use nix::errno::Errno;
        // 自身必存活（任何实现都该过——防呆基线）。
        assert!(pid_alive(std::process::id()), "自身进程必判存活");
        // 不存在的 pid 必判死（取一个合法但不可能被占用的值）。
        assert!(!pid_alive(i32::MAX as u32), "不存在的 pid 必判不存活");

        // **广播语义门**：0 与越 i32 回绕的 pid 必须判不活，且不得走到 kill 的广播语义上。
        // 打断（去掉 `checked_pid` 直接 `pid as i32`）→ `kill(-1,0)`/`kill(0,0)` 恒 Ok → 本测转红。
        // 同一个 cast 也喂 `send_signal`，在那边等价于 `SIGKILL` 全场，故这是安全门不是洁癖。
        assert!(
            !pid_alive(0),
            "pid 0 = 当前进程组广播，绝不可判为某个进程存活"
        );
        assert!(
            !pid_alive(u32::MAX),
            "u32::MAX 回绕成 -1 = 全体广播，绝不可判存活"
        );

        // pid 1 的属主判定：非 root 用户探它必得 EPERM。若本次恰以 root 运行（CI 容器），
        // 这一腿没有 EPERM 可验 —— 照实跳过，不伪装成验过。
        let probe = nix::sys::signal::kill(nix::unistd::Pid::from_raw(1), None);
        if probe == Err(Errno::EPERM) {
            assert!(
                pid_alive(1),
                "[变异门①端到端] root 所有的 pid 1 探活收 EPERM，必须判存活"
            );
        } else {
            // 以 root 运行 → kill(1,0) 返 Ok，EPERM 腿在本环境无从构造。
            assert_eq!(probe, Ok(()), "非 EPERM 时只可能是 root 运行下的 Ok");
        }
    }

    /// Windows 探活的解析腿（跨权限盲区体检的产物）。本机 Linux 编不到 `pid_alive` 的 win 分支，
    /// 但纯解析函数经 `cfg(any(windows, test))` 在此可验。
    ///
    /// 打断（退回 `stdout.contains(&pid.to_string())` 子串匹配）→ 第二个断言转红：
    /// 内存列 `12,500 K` 含子串 "500" 会把**任意** pid 500 误判成存活。
    #[test]
    fn tasklist_parser_matches_whole_pid_token_not_substring() {
        let row = "sing-box.exe                  6439 Services                   0     45,120 K";
        assert!(tasklist_reports_pid(row, 6439), "该行确实报告了 6439");
        assert!(
            !tasklist_reports_pid(row, 500),
            "内存列 45,120 / 会话号等数字不得把别的 pid 撞成存活（子串匹配会）"
        );
        // 过滤无命中时 tasklist 打印 INFO 行 → 必须判不存活。
        assert!(
            !tasklist_reports_pid(
                "INFO: No tasks are running which match the specified criteria.",
                6439
            ),
            "无命中提示行不得被当成存活"
        );
    }

    /// `/proc/<pid>/stat` 的 starttime 取材腿（helper 腿 pid 身份令牌的 linux 侧）。
    ///
    /// 打断（改成对整行 `split_whitespace().nth(21)`，即不从最后一个 `)` 之后切）→ 第二个断言转红：
    /// comm 含空格/右括号的进程会整体错位。这不是理论角落 —— 进程名由启动方控制，
    /// 而本令牌一旦取到**错字段**，要么恒变（假崩溃 + 无谓重启）要么恒不变（门形同虚设），
    /// 两种都比没有这道复核更坏。
    #[test]
    fn proc_stat_starttime_survives_comm_with_spaces_and_parens() {
        // 真实形状：pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt majflt
        // cmajflt utime stime cutime cstime priority nice num_threads itrealvalue starttime …
        let fields: Vec<String> = (3..=22).map(|i| i.to_string()).collect();
        let tail = fields.join(" ");
        let plain = format!("6439 (sing-box) {tail} 上略");
        assert_eq!(
            parse_proc_stat_starttime(&plain).as_deref(),
            Some("22"),
            "starttime 是第 22 字段"
        );

        let nasty = format!("6439 (we ird) (name) {tail} 上略");
        assert_eq!(
            parse_proc_stat_starttime(&nasty).as_deref(),
            Some("22"),
            "comm 含空格与右括号时仍须取到第 22 字段（必须从最后一个 `)` 之后切）"
        );

        // 字段不够（读到半截 / 不是 stat）→ None，不返回一个错位的值。
        assert_eq!(
            parse_proc_stat_starttime("6439 (sing-box) S 1").as_deref(),
            None
        );
        // 连 `)` 都没有 → None。
        assert_eq!(parse_proc_stat_starttime("garbage").as_deref(), None);
    }

    /// Windows 侧身份令牌的解析腿：取该 pid 那一行的映像名。
    ///
    /// 打断（改成取 `lines().next()` 而不是「含该 pid token 的那一行」）→ 第二个断言转红：
    /// 多行输出时会恒取第一行，令牌与 pid 脱钩。
    #[test]
    fn tasklist_image_name_comes_from_the_row_of_that_pid() {
        let out = "sing-box.exe                  6439 Services                   0     45,120 K\n\
                   other.exe                      777 Console                    1      1,024 K";
        assert_eq!(
            parse_tasklist_image_name(out, 6439).as_deref(),
            Some("sing-box.exe")
        );
        assert_eq!(
            parse_tasklist_image_name(out, 777).as_deref(),
            Some("other.exe"),
            "必须按 pid 定位到行，而不是恒取第一行"
        );
        assert_eq!(parse_tasklist_image_name(out, 12345), None, "不在列 → None");
        assert_eq!(
            parse_tasklist_image_name(
                "INFO: No tasks are running which match the specified criteria.",
                6439
            ),
            None,
            "无命中提示行不得被当成一个映像名"
        );
    }

    /// **本次修复要防住的那件事的回放**：pid 还在（`pid_alive` 恒真）、但号码上换了进程。
    ///
    /// 三条断言各锁一个方向：
    /// - 令牌变 ⇒ `Mismatch`（崩溃监测据此判退出 → 自愈；此前这一格恒 `Alive`，自愈永不触发）；
    /// - 取不到材料 ⇒ `Unobservable` 而**非** `Mismatch` —— 折成不匹配等于把一次读失败变成一次
    ///   假崩溃，下游是自动重启；
    /// - 令牌未变 ⇒ `Match`。
    ///
    /// 打断（把 `pid_identity_verdict` 的 `_ => Unobservable` 改成 `_ => Mismatch`）→ 第二组转红。
    #[test]
    fn pid_identity_flags_reuse_but_never_invents_a_crash() {
        assert_eq!(
            pid_identity_verdict(Some("998877"), Some("112233")),
            PidIdentity::Mismatch,
            "同一 pid 上令牌变了 = 换了进程"
        );
        assert_eq!(
            pid_identity_verdict(Some("998877"), Some("998877")),
            PidIdentity::Match
        );
        for (base, cur) in [(None, Some("x")), (Some("x"), None), (None, None)] {
            assert_eq!(
                pid_identity_verdict(base, cur),
                PidIdentity::Unobservable,
                "缺任一侧材料一律 Unobservable（没观测到 ≠ 观测到没问题）"
            );
        }
    }

    /// **接线门**：纯逻辑对了不代表崩溃监测真的去问了它。
    ///
    /// 本仓两天内被同一形状骗过两次（判据落在「这个词出现过吗」，而词的来源包含判据自身）⇒
    /// 判据取的是 [`method_body`] 截出的 `spawn_crash_monitor` **方法体**（剥掉整行注释、
    /// 到方法末尾封顶），既排除本测试模块自身，也排除方法内注释里的同名文本。
    ///
    /// 打断（把复核那段删掉、只留 `pid_alive`）→ 三条断言全红。
    #[test]
    fn crash_monitor_actually_consults_the_pid_identity() {
        const HEAD: &str = "fn spawn_crash_monitor(self: &Arc<Self>, my_gen: u64) {";
        let src = include_str!("proxy.rs");
        // 切在「锚点之后的第一个顶层 `#[cfg(test)]`」：本文件里生产码与测试模块**交替**出现
        // （实测顶层 cfg(test) 有 5 处，最后一处还在本测试之后）⇒ 切第一处会把待验方法切掉、
        // 切最后一处会把本测试留在判据区域里。两种都实测过。
        let at = src
            .find(HEAD)
            .unwrap_or_else(|| panic!("锚点 `{HEAD}` 消失，源码型守卫已失去判据"));
        let cut = src[at..]
            .find("\n#[cfg(test)]\n")
            .map_or(src.len(), |i| at + i);
        let prod = &src[..cut];
        // 切点自检：判据区域里若还留着本测试自身，下面三条就会被自己写的字面量喂饱（生产调用点
        // 删光也照样绿）。本仓两天内被这个形状骗过两次，故显式锁住。
        assert!(
            !prod.contains("fn crash_monitor_actually_consults_the_pid_identity"),
            "判据区域包含本测试自身 —— 切点选错，断言会被自己的字面量污染"
        );
        let body = method_body(prod, HEAD);
        assert!(
            body.contains("pid_identity_verdict("),
            "崩溃监测没有调用 pid_identity_verdict —— 身份复核没接线，pid 复用仍不可发现"
        );
        assert!(
            body.contains("process_identity(p)"),
            "崩溃监测没有取当前令牌 —— 复核会拿基线跟自己比，恒 Match"
        );
        assert!(
            body.contains("PidIdentity::Mismatch"),
            "崩溃监测没有据不匹配改判退出 —— 复核结果被丢弃"
        );
    }

    /// emitter 未接线（setup 前的极早期失败 / 单测）→ 状态照落，不 panic。
    /// 打断（emitter 用 `.get().unwrap()`）→ 本测转红：诊断通道不该反噬它诊断的东西。
    #[test]
    fn set_error_without_emitter_still_records_state() {
        let (rt, dir) = test_runtime(); // 刻意不接线 emitter
        rt.set_error("无 emitter 也要落状态", code::PROCESS_EXITED);
        assert_eq!(
            rt.status().error_code.as_deref(),
            Some(code::PROCESS_EXITED)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── #9 TUN 起核连接 flush：两条守卫 ───────────────────────────────────────────────
    //
    // 四条测试各钉一条腿，合起来把 `flush_connections_once` 的五个出口盖到四个；
    // 第五个（`Flushed` = 真 RST）要活核才有意义，属真机门（见 P4-b 记录）。

    /// TUN + 同世代 + 核在跑的 runtime（下面三条测试的共同前置）。
    fn flush_ready_runtime() -> (Arc<ProxyRuntime>, PathBuf, u64) {
        let (rt, dir) = test_runtime();
        *rt.status.write().unwrap() = ProxyStatus {
            running: true,
            ..Default::default()
        };
        let my_gen = rt.gate.generation();
        (rt, dir, my_gen)
    }

    /// 🔴 **守卫①**：非 TUN 模式一律不 flush。
    ///
    /// systemProxy / manual 的旧连接多在 sing-box 连接表之外，无差别 RST 够不着它们、只会误伤
    /// 已经过代理的连接。**其余前置全部满足**（核在跑、世代未变）—— 唯一变量就是模式，
    /// 否则测的是「别的守卫恰好也拦了」。
    ///
    /// **变异锁**：删掉 `if !mode.is_tun()` 早退 → 两个模式都走到建连腿 → 本测转红。
    #[tokio::test]
    async fn flush_skips_every_non_tun_mode() {
        let (rt, dir, my_gen) = flush_ready_runtime();
        for mode in [ProxyModeType::SystemProxy, ProxyModeType::Manual] {
            assert_eq!(
                rt.flush_connections_once(mode, my_gen, 1).await,
                FlushOutcome::SkippedNotTun,
                "{mode:?} 模式绝不允许 flush：够不着表外的旧连接，只会误伤已代理的连接"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **守卫②·世代**：延迟窗口内被 stop / 重启接管 → 放弃，不得打到已换的核。
    ///
    /// **变异锁**：删掉世代比对 → 落到建连腿（非 Skipped*）→ 本测转红。
    #[tokio::test]
    async fn flush_skips_when_generation_superseded() {
        let (rt, dir, my_gen) = flush_ready_runtime();
        rt.bump_generation(); // 等价于窗口内来了一次 stop / restart
        assert_eq!(
            rt.flush_connections_once(ProxyModeType::Tun, my_gen, 1)
                .await,
            FlushOutcome::SkippedSuperseded,
            "世代已被接管仍开枪 = 把新核刚建立的连接全 RST 掉"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **守卫②·核在跑**：核已停 → 无连接表可 flush。
    ///
    /// **变异锁**：删掉 `status().running` 判定 → 落到建连腿（非 Skipped*）→ 本测转红。
    #[tokio::test]
    async fn flush_skips_when_core_stopped() {
        let (rt, dir, my_gen) = flush_ready_runtime();
        *rt.status.write().unwrap() = ProxyStatus::default(); // running:false
        assert_eq!(
            rt.flush_connections_once(ProxyModeType::Tun, my_gen, 1)
                .await,
            FlushOutcome::SkippedCoreStopped,
            "核已停不该再去连管理 API"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **守卫全过 ⇒ 真的走到管理 API**（不是「三条跳过腿都绿」的假闭环）。
    ///
    /// 没有活核，所以断言只到「**不是**任何一条跳过腿」：说明两条守卫都放行、代码真的去开枪了。
    /// 端口取一个刚释放的空闲口（纯回环、无监听，不碰宿主网络），故必然落 `ConnectFailed`
    /// 或 `CallFailed` —— 具体哪个取决于 tonic 建连是否惰性，不该由本测钉死。
    /// 真的把连接 RST 掉需要活核 + 抓包，属真机门。
    #[tokio::test]
    async fn flush_reaches_management_api_when_both_guards_pass() {
        let (rt, dir, my_gen) = flush_ready_runtime();
        let dead_port = free_port(); // 监听已 drop ⇒ 无人接
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            rt.flush_connections_once(ProxyModeType::Tun, my_gen, dead_port),
        )
        .await
        .expect("flush 腿必须自行了结，不得挂死在建连上");
        assert!(
            matches!(
                outcome,
                FlushOutcome::ConnectFailed(_) | FlushOutcome::CallFailed(_)
            ),
            "两条守卫都放行时必须真的走到管理 API，实得 {outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **接线守卫**：flush 必须在 `running:true` 落定之后才可能开枪。
    ///
    /// 顺序不是洁癖：守卫②查的就是 `status().running`，排在状态提交之前会让每次起核都落
    /// `SkippedCoreStopped` —— 腿在、恒不开枪，而上面四条单测照样全绿（它们直调决策点，不经起核腿）。
    ///
    /// **判据现在是间接的**：flush 已挪进 selector 校正的续延（上游「时序修 E」，理由见
    /// [`after_selector_reasserted`](ProxyRuntime::after_selector_reasserted)），起核腿里只剩
    /// **spawn** 那一行。于是这条不变式改由「spawn 点晚于状态提交」承担 —— 续延只会更晚，
    /// 传递性给出同样的保证，且比原来更强（原来 flush 与提交之间还隔着一整段可被重排的主链）。
    ///
    /// 「恰调一次 flush」那条计数不在这里：它已经被
    /// [`selector_reassert_continuation_holds_all_three_deferred_actions`] 与
    /// [`start_inner_spawns_reassert_and_defers_unlock_invalidation`] 两侧夹住
    /// （续延里恰一次 + 主链上零次）。此处再抄一遍只会在下次搬家时留下第三处要改的地方。
    ///
    /// **变异锁**：把 `spawn_reassert_selector_selection(...)` 挪到 `*g = new_status.clone();` 之前 → 转红。
    #[test]
    fn connection_flush_is_reachable_only_after_status_commit() {
        let body = method_body(include_str!("proxy.rs"), "    async fn start_inner(");
        let commit = body
            .find("*g = new_status.clone();")
            .expect("锚点 `*g = new_status.clone();` 消失，顺序守卫已失去判据");
        let spawn = body
            .find("self.spawn_reassert_selector_selection(")
            .expect("校正腿的 spawn 点消失 —— flush 已随它挪进续延，没有 spawn 就没有 flush");
        assert!(
            commit < spawn,
            "校正腿（flush 挂在它的续延上）必须 spawn 在 running:true 提交之后，\
             否则守卫②恒判『核已停』→ 腿在但永不开枪"
        );
    }

    /// 🔴 **建连之后必须再查一次世代**（上面四条行为测试够不着的那半条守卫）。
    ///
    /// 为什么只能用源码守卫：这条腿只在「建连**成功**、随后被接管」时才走到，而单测里没有活的
    /// 管理 API —— 建连必失败、必在此之前返回。造一个假 gRPC 服务端来喂它，代价远超这条断言的价值；
    /// 真实覆盖在真机门（TUN 起核 + 窗口内点停止）。
    ///
    /// **变异锁**：删掉建连后的那次世代比对 → 计数从 2 掉到 1 → 本测转红。
    #[test]
    fn flush_rechecks_generation_after_connect() {
        let body = method_body(
            include_str!("proxy.rs"),
            "    async fn flush_connections_once(",
        );
        assert_eq!(
            body.matches("self.gate.generation() != my_gen").count(),
            2,
            "世代必须查两次：建连前一次、建连（await 点）后一次 —— 少一次就可能把新核的连接 RST 掉"
        );
        let connect = body
            .find("SingBoxApiClient::connect(")
            .expect("锚点 `SingBoxApiClient::connect(` 消失，顺序守卫已失去判据");
        let last_check = body
            .rfind("self.gate.generation() != my_gen")
            .expect("上一条断言已保证存在");
        assert!(
            connect < last_check,
            "第二次世代比对必须排在建连之后，排在前面等于两次查同一个时刻"
        );
    }

    // ── #14 反向不变式：喂进 sing-box 生成的 config 键必须对 norm 可见 ──────────────────
    //
    // 淬火不变式 #14 原文预言过一次**风险方向反转**，本仓已实证发生：
    // - 上游侧 norm 是「全量哈希 + 排除表」⇒ 漏加**排除**项 = 多重启一次（吵，但看得见）；
    // - 本仓 norm 是「白名单入投影」（`UserConfig::FIELD_NAMES`）⇒ 漏加**白名单**项 =
    //   `config_generation_norm` 恒相等 → 落 NoOp 腿 → **永不进 pending 差集**：改了要重启内核，
    //   而 pending-bar 不出现、U-7 弹窗也不出现，全程零提示（`ui/src/domain/app-restart-keys.ts`
    //   称之为「第四类重启」）。少提示是静默的，比多提示危险得多。
    //
    // 方向反了，守卫也必须反过来写：不是「排除表别漏」（那条由 config-engine 的
    // `exclusion_table_live_entries_are_pinned` 钉着），而是**「生成侧消费的键别漏进 FIELD_NAMES」**。

    /// `GenerateConfigDeps` 的装配体 —— 原始 config JSON 进入生成侧的**唯一**通道。
    ///
    /// 用返回类型行当锚点（而非 `fn generate_deps(`）是刻意的：[`method_body`] 从锚点末尾起切，
    /// 用函数头会把参数列表 `config: &Value` 一起切进来，下面的「参数用了几次」就恒多数一次。
    const DEPS_ASSEMBLY: &str = "    ) -> GenerateConfigDeps {";

    /// 装配体里**唯一**允许的裸 JSON 取值点（形参 `config` 的全部去向）。
    ///
    /// 新增一个 `xxx_from_config(config)` 或直接写 `config.get("k")` → 下面的「用了恰好一次」转红，
    /// 逼改动者把新读法登记进本表，随后 [`raw_keys`] 会把它读的键一并纳入可见性判定。
    const RAW_CONFIG_READERS: &[&str] = &["fn log_axes_from_config("];

    /// 形参 `param` 在 `body` 里被当**标识符**用了几次（`self.config` 这类字段访问不计）。
    ///
    /// 按标识符边界判而非裸 `contains`：否则 `config_log` / `log_axes_from_config` 这些**含**
    /// `config` 的名字会把计数喂饱，守卫失去分辨力。
    fn param_use_count(body: &str, param: &str) -> usize {
        let bytes = body.as_bytes();
        let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        body.match_indices(param)
            .filter(|(i, _)| {
                let before = if *i == 0 { None } else { Some(bytes[i - 1]) };
                let after = bytes.get(i + param.len()).copied();
                // 左边是标识符字符 ⇒ 是更长名字的一截；左边是 `.` ⇒ 字段访问（`self.config`）。
                !matches!(before, Some(c) if ident(c) || c == b'.')
                    && !matches!(after, Some(c) if ident(c))
            })
            .count()
    }

    /// 函数体里所有 `.get("键")` 的键名。
    fn raw_keys(body: &str) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        let mut rest = body;
        while let Some(i) = rest.find(".get(\"") {
            let after = &rest[i + ".get(\"".len()..];
            match after.find('"') {
                Some(j) => {
                    out.insert(after[..j].to_string());
                    rest = &after[j..];
                }
                None => break,
            }
        }
        out
    }

    /// 🔴 **生成侧读到的每个 config 键都必须在 `UserConfig::FIELD_NAMES` 里**（#14 反向不变式）。
    ///
    /// 不在 ⇒ 该键改了 `config_generation_norm` 也不变 ⇒ 永不进 pending 差集 ⇒ 静默跑陈旧核。
    ///
    /// **变异锁**：把 `log_axes_from_config(config)` 从装配体里删掉 → 第一条断言（用了恰好一次）转红；
    /// 往 `log_axes_from_config` 里加一个不在 `FIELD_NAMES` 的键 → 末条断言转红。
    #[test]
    fn every_generation_input_key_is_visible_to_norm() {
        const SRC: &str = include_str!("proxy.rs");
        let deps = method_body(SRC, DEPS_ASSEMBLY);
        assert!(
            !deps.contains("fn "),
            "装配体切过头了（切到了下一个函数）—— 下面的断言正在扫一段不属于 generate_deps 的源码"
        );

        // ① 通道封闭：形参 `config` 只许流向登记在册的读法，且**恰好一次**。
        assert_eq!(
            param_use_count(&deps, "config"),
            RAW_CONFIG_READERS.len(),
            "`generate_deps` 里对原始 config 的取值点数目变了。新增取值点必须登记进 RAW_CONFIG_READERS，\
             否则它读的键逃过本守卫 —— 那正是「第四类重启」的生成方式。当前体：\n{deps}"
        );
        for reader in RAW_CONFIG_READERS {
            let name = reader.trim_start_matches("fn ").trim_end_matches('(');
            assert!(
                deps.contains(&format!("{name}(config)")),
                "登记在册的读法 `{name}(config)` 在装配体里找不到 —— 表与代码已分叉"
            );
        }

        // ② 登记在册的读法读了哪些键。
        let mut keys = std::collections::BTreeSet::new();
        for reader in RAW_CONFIG_READERS {
            let body = crate::commands::guard_scan::top_level_fn_body(SRC, reader);
            let found = raw_keys(&body);
            assert!(
                !found.is_empty(),
                "`{reader}` 里一个 `.get(\"…\")` 都没扫到 —— 取材器失配，本守卫已退化成恒真断言"
            );
            keys.extend(found);
        }

        // ③ 可见性判定。
        let visible: std::collections::BTreeSet<&str> =
            UserConfig::FIELD_NAMES.iter().copied().collect();
        let invisible: Vec<&str> = keys
            .iter()
            .map(String::as_str)
            .filter(|k| !visible.contains(k))
            .collect();
        assert!(
            invisible.is_empty(),
            "这些键喂进了 sing-box 生成，却不在 `UserConfig::FIELD_NAMES` 里：{invisible:?}\n\
             ⇒ 改它们 `config_generation_norm` 恒相等 → 落 NoOp 腿 → **永不进 pending 差集**：\
             核在跑时改了要重启才生效，而 pending-bar 与 U-7 弹窗都不出现，全程零提示。\n\
             修法：把它加进 `UserConfig`（值可以是 `serde_json::Value` —— 本结构只需要「看得见变化」，\
             不需要解释它），或说明它为何根本不该影响生成、从而不必被生成侧读。"
        );
    }

    /// 🔴 **第四类重启已消灭**（行为门，钉住上一条断言背后的那个事实）。
    ///
    /// 上一条是源码扫描：它保证「表与代码一致」，但**证明不了 norm 真的动了**——
    /// 键在 `FIELD_NAMES` 里而投影却把它排掉（比如有人往 `config_generation_norm` 的排除表里
    /// 补一行），扫描面照样全绿。本条从行为侧钉死：两键一变，norm 必须判不等。
    ///
    /// **变异锁**：把这两个字段从 `UserConfig` 摘掉（或在 norm 的排除表里加上它们）→ 第一条断言转红。
    #[test]
    fn log_axes_changes_are_visible_to_norm() {
        // `servers` 无 serde default（缺了整份配置解析不出来）→ 两份都带上空数组，
        // 让唯一的变量真的只有这两个键。
        let base =
            serde_json::json!({ "servers": [], "logLevel": "info", "disableLogFile": false });
        let flipped =
            serde_json::json!({ "servers": [], "logLevel": "debug", "disableLogFile": true });
        let norm = |v: &Value| {
            config_generation_norm(
                &serde_json::from_value::<UserConfig>(v.clone()).expect("测试配置必须可解析"),
                None,
            )
        };
        assert_ne!(
            norm(&base),
            norm(&flipped),
            "改日志两轴必须让 norm 判不等 —— 否则它们又回到「改了要重启核而差集看不见」的第四类"
        );
        assert_ne!(
            log_axes_from_config(&base),
            log_axes_from_config(&flipped),
            "两键必须真的改变生成输入，否则上面那条 norm 断言守的是一个不存在的因果"
        );
    }

    /// 🔴 **取值域不由本仓独占 ⇒ 解析必须宽容**（`Value` 而非强类型的理由，钉成门）。
    ///
    /// `UserConfig` 解析是全有全无的：一旦 `Err`，起核腿整个放弃。若把 `logLevel` 收紧成 `LogLevel`，
    /// 一份写着 sing-box 的 `trace`（或任何手改值）的配置会从「日志级别退化成 info」变成「**起不了核**」。
    ///
    /// **变异锁**：把 `log_level` 改成 `Option<LogLevel>` → 第一条转红。
    #[test]
    fn unknown_log_level_still_parses_and_degrades_to_info() {
        let cfg = serde_json::json!({ "servers": [], "logLevel": "trace", "disableLogFile": 1 });
        let parsed = serde_json::from_value::<UserConfig>(cfg.clone());
        assert!(
            parsed.is_ok(),
            "本仓不认识的 logLevel 取值不得让整份 UserConfig 解析失败（那等于起不了核）：{:?}",
            parsed.err()
        );
        assert_eq!(
            log_axes_from_config(&cfg),
            (polaris_config_engine::user_config::LogLevel::Info, false),
            "值的解释权仍在 log_axes_from_config：非法级别退化 Info、非 true 的 disableLogFile 记 false"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // 起核前的内核闸门：内核点名的下标 → 该拿这个节点怎么办
    //
    // 判据全部纯静态（内核诊断行 + 我方生成的那份 config + id→tag 表），故**无需核、无需落盘、
    // 不碰网络**即可全覆盖。诊断行的解析与三态映射另有门：`core-supervisor` 的
    // `config_gate::tests`（纯解析）与 `tests/config_gate_process.rs`（真子进程接线）。
    // ══════════════════════════════════════════════════════════════════════════════

    /// 造闸门用的 config：`outbounds` = [direct, HK01, JP01, proxy-selector]，
    /// `endpoints` = [WG01]。下标即数组下标 —— 内核给的就是这个坐标系。
    fn gate_fixture() -> SingBoxConfig {
        serde_json::from_value(serde_json::json!({
            "log": { "level": "info", "timestamp": true },
            "inbounds": [],
            "outbounds": [
                { "type": "direct", "tag": "direct" },
                { "type": "shadowsocks", "tag": "HK01" },
                { "type": "shadowsocks", "tag": "JP01" },
                { "type": "selector", "tag": PROXY_SELECTOR_TAG,
                  "outbounds": ["HK01", "JP01", "direct"], "default": "HK01" }
            ],
            "endpoints": [ { "type": "wireguard", "tag": "WG01" } ],
            "route": { "rules": [], "final": PROXY_SELECTOR_TAG }
        }))
        .expect("fixture sing-box config 应可解析")
    }

    /// tag → id 反表（`generate_and_gate` 里由 `build_id_to_tag_map` 现算的那一份的等价物）。
    /// 注意内置出站（direct / proxy-selector）**不在表里** —— 它们不是节点。
    fn gate_tag_to_id() -> BTreeMap<String, String> {
        [("HK01", "n-hk"), ("JP01", "n-jp"), ("WG01", "n-wg")]
            .into_iter()
            .map(|(t, i)| (t.to_string(), i.to_string()))
            .collect()
    }

    fn rejection(array: RejectedArray, index: usize) -> KernelRejection {
        KernelRejection {
            array,
            index,
            detail: "unknown outbound type: zzz".to_string(),
        }
    }

    /// 🔴 **变异锁：下标必须翻成对应节点，且 `outbounds[]` / `endpoints[]` 是两个独立坐标系**。
    ///
    /// `outbounds[2]` = JP01、`endpoints[0]` = WG01 —— 两者下标都不是 0/2 的巧合：若把
    /// `RejectedArray::Endpoints` 那一支错接到 `config.outbounds`，`endpoints[0]` 会翻成 `direct`
    /// ⇒ 落 `Unattributable`，本条转红。
    #[test]
    fn kernel_index_maps_back_to_the_right_node_in_the_right_array() {
        let cfg = gate_fixture();
        let map = gate_tag_to_id();
        let peeled = BTreeSet::new();
        assert_eq!(
            classify_peel_target(
                &rejection(RejectedArray::Outbounds, 2),
                &cfg,
                &map,
                None,
                &peeled
            ),
            PeelTarget::Peel {
                id: "n-jp".into(),
                tag: "JP01".into()
            }
        );
        assert_eq!(
            classify_peel_target(
                &rejection(RejectedArray::Endpoints, 0),
                &cfg,
                &map,
                None,
                &peeled
            ),
            PeelTarget::Peel {
                id: "n-wg".into(),
                tag: "WG01".into()
            }
        );
    }

    /// 🔴 **变异锁：归因不到就绝不剥**（内置出站 / 下标越界 / 无 endpoints 数组）。
    ///
    /// 错误归因会剥掉一个**本来能用**的节点，且用户完全无从察觉 —— 比不归因坏得多。
    /// 变异：给 `attribute_rejected_node` 加一条「查不到就按 tag 当 id 用」的兜底 ⇒ 前两条断。
    #[test]
    fn non_node_or_out_of_range_index_is_never_attributed() {
        let cfg = gate_fixture();
        let map = gate_tag_to_id();
        let peeled = BTreeSet::new();
        for (array, index, why) in [
            (RejectedArray::Outbounds, 0, "direct 是内置出站，不是节点"),
            (
                RejectedArray::Outbounds,
                3,
                "proxy-selector 是内置出站，不是节点",
            ),
            (RejectedArray::Outbounds, 99, "下标越界"),
            (RejectedArray::Endpoints, 7, "endpoints 下标越界"),
        ] {
            assert_eq!(
                classify_peel_target(&rejection(array, index), &cfg, &map, None, &peeled),
                PeelTarget::Unattributable,
                "{why}"
            );
        }
        // 整个 endpoints 键缺席（绝大多数配置的常态）→ 同样归因不到，不得 panic。
        let mut no_ep = gate_fixture();
        no_ep.endpoints = None;
        assert_eq!(
            classify_peel_target(
                &rejection(RejectedArray::Endpoints, 0),
                &no_ep,
                &map,
                None,
                &peeled
            ),
            PeelTarget::Unattributable,
            "无 endpoints 数组时不得越界 panic，也不得错归因"
        );
    }

    /// 🔴 **变异锁：内核拒的若是用户选中的节点，必须落 `Blocked`，绝不静默剥掉**。
    ///
    /// 剥了就等于替用户改出口，而「实际生效出口 ≠ 选中节点」在本仓是要专门告警的事故
    /// （`code::EXIT_MISMATCH`）—— 闸门自己去制造它是自相矛盾。且真剥了下一轮 generate 会直接
    /// 返回 `Selected server not found`，用户又拿到一句和现场无关的话。
    ///
    /// 变异：删掉 `selected_server_id ==` 那一支（回到无差别剥）⇒ 本条断在 `Peel`。
    #[test]
    fn rejecting_the_selected_node_blocks_instead_of_silently_switching_exit() {
        let cfg = gate_fixture();
        let map = gate_tag_to_id();
        let peeled = BTreeSet::new();
        assert_eq!(
            classify_peel_target(
                &rejection(RejectedArray::Outbounds, 1),
                &cfg,
                &map,
                Some("n-hk"),
                &peeled
            ),
            PeelTarget::Blocked {
                id: "n-hk".into(),
                tag: "HK01".into()
            },
            "选中节点被拒 → 终态，不得改出口"
        );
        // 同一份现场，只是选中的是**别的**节点 → 照常剥（证明上面那条断的是「选中」这个条件本身，
        // 不是「HK01 这个节点」）。
        assert_eq!(
            classify_peel_target(
                &rejection(RejectedArray::Outbounds, 1),
                &cfg,
                &map,
                Some("n-jp"),
                &peeled
            ),
            PeelTarget::Peel {
                id: "n-hk".into(),
                tag: "HK01".into()
            }
        );
    }

    /// 🔴 **变异锁：判「是否选中」必须先于判「是否已剥过」**。
    ///
    /// 顺序颠倒时，一个「既是选中节点、又已在集合里」的现场会落 `Stalled`（= 静默放行去 spawn，
    /// 拿一份缺了选中节点的配置起核 ⇒ 出口跑到别的节点上，正是 `EXIT_MISMATCH` 要抓的那种事故），
    /// 而不是落 `Blocked`。这条现场在真机上可达：选中节点在第 N 轮被剥后，用户改选中它。
    #[test]
    fn selected_check_precedes_stall_check() {
        let cfg = gate_fixture();
        let map = gate_tag_to_id();
        let peeled: BTreeSet<String> = ["n-hk".to_string()].into_iter().collect();
        assert_eq!(
            classify_peel_target(
                &rejection(RejectedArray::Outbounds, 1),
                &cfg,
                &map,
                Some("n-hk"),
                &peeled
            ),
            PeelTarget::Blocked {
                id: "n-hk".into(),
                tag: "HK01".into()
            },
            "既选中又已剥 → 必须是 Blocked（判序颠倒会落 Stalled，等于静默改出口）"
        );
    }

    /// 🔴 **变异锁：推进不变式 —— 已剥过却又被点名就停，不许原地打转**。
    ///
    /// 这条比时间预算更根本：预算只封顶延迟，**终止**靠它。变异：删掉 `already_peeled.contains`
    /// 那一支 ⇒ 本条断在 `Peel`，而生产上那意味着「剥了没生效 → 无限重生成 → 起核永远回不来」。
    #[test]
    fn already_peeled_node_named_again_stalls_the_loop() {
        let cfg = gate_fixture();
        let map = gate_tag_to_id();
        let peeled: BTreeSet<String> = ["n-jp".to_string()].into_iter().collect();
        assert_eq!(
            classify_peel_target(
                &rejection(RejectedArray::Outbounds, 2),
                &cfg,
                &map,
                None,
                &peeled
            ),
            PeelTarget::Stalled { tag: "JP01".into() }
        );
    }

    /// 两节点配置（选中 keep-me），供 `generate_and_gate` 的整环门用。
    fn gate_two_node_config() -> Value {
        serde_json::json!({
            "servers": [
                { "id": "n-bad", "name": "BAD", "protocol": "shadowsocks",
                  "address": "1.2.3.4", "port": 8388, "method": "aes-256-gcm", "password": "p" },
                { "id": "n-keep", "name": "KEEP", "protocol": "shadowsocks",
                  "address": "5.6.7.8", "port": 8388, "method": "aes-256-gcm", "password": "p" }
            ],
            "selectedServerId": "n-keep",
            "proxyMode": "global",
            "proxyModeType": "manual",  // 安全：不接管系统代理、不建 TUN
            "mixedPort": 17890,
        })
    }

    /// 假核（**闸门腿**）：第一次 `check` 吐一条点名 `outbounds[<idx>]` 的 FATAL 并 rc=1，
    /// 之后的 `check` 一律 rc=0（模拟「坏节点被剥掉后配置就合法了」）。`run` 直接退出（本门不 spawn）。
    ///
    /// 「第一次 vs 之后」靠**落盘 marker** 记状态：闸门每轮都是一个**全新子进程**，进程内变量存不住。
    #[cfg(unix)]
    fn write_fake_checking_core(dir: &std::path::Path, reject_index: usize) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("fake-checking-sing-box");
        let marker = dir.join("gate-check-seen").to_string_lossy().into_owned();
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\ncase \" $* \" in *\" check \"*)\n\
                 if [ -f {marker} ]; then exit 0; fi\n\
                 touch {marker}\n\
                 echo 'FATAL[0000] decode config at cfg.json: outbounds[{reject_index}]: \
                 unknown outbound type: zzz' >&2\n\
                 exit 1;;\nesac\nexit 1\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// 🔴 **整环门：内核点名 → 真的重新生成 → 坏节点从落盘配置里消失 → 走既有通道上报**。
    ///
    /// 这条补的是纯决策面单测够不着的那一半：`generate_and_gate` 里「剥完**重跑生成**」这条接线。
    /// 变异（逐条转红）：
    /// - 把 `effective.servers.retain(...)` 删掉（剥了却不重新生成）⇒ 坏节点仍在落盘配置里，断言 1 红；
    /// - 把 `kernel_invalid` 不并进 `invalid_nodes`（剥了不上报）⇒ 断言 2 红 —— 节点凭空消失而不告知，
    ///   正是 `outbounds.rs` 那条「节点消失而不告知比报错更坏」反复强调的失效形态；
    /// - 把 `peeled` 换成每轮新建的局部集合 ⇒ 剥了不记账 → 第二轮又生成出坏节点，断言 1 红。
    ///
    /// **下标不写死**：先用 `binary=None`（failOpen 腿，不跑 check）拿到本次真实生成的 outbounds
    /// 顺序，再据此算出 BAD 的下标喂给假核 —— 生成顺序哪天变了，本测自动跟上，不会变成假绿。
    #[cfg(unix)]
    #[tokio::test]
    async fn kernel_rejected_node_is_regenerated_out_and_reported_through_the_existing_channel() {
        let (rt, dir) = test_runtime();
        let cfg = gate_two_node_config();
        let user_config: UserConfig = serde_json::from_value(cfg.clone()).unwrap();
        let deps = rt.generate_deps(0, 0, &[], &cfg);
        let path = dir.join("gate-probe.json");

        // ① failOpen 腿（无核）：闸门整个跳过 —— 两个节点都在，且**没有**任何剔除上报。
        let mut peeled = BTreeMap::new();
        let base = rt
            .generate_and_gate(&user_config, &deps, &path, None, &mut peeled)
            .await
            .expect("无核时闸门必须放行，不得把「核不可用」判成「配置无效」");
        assert_eq!(base.checks_run, 0, "无核 ⇒ 一次 check 都不该跑");
        assert!(base.invalid_nodes.is_empty(), "无核 ⇒ 不得凭空上报剔除");
        let bad_index = base
            .config
            .outbounds
            .iter()
            .position(|o| o.tag == "BAD")
            .expect("BAD 节点应在生成的 outbounds 里");

        // ② 真闸门腿：假核第一次 check 点名 BAD 的下标。
        let fake = write_fake_checking_core(&dir, bad_index);
        let mut peeled = BTreeMap::new();
        let gated = rt
            .generate_and_gate(&user_config, &deps, &path, Some(&fake), &mut peeled)
            .await
            .expect("剥掉非选中的坏节点后应正常返回");

        // 断言 1：坏节点从**落盘的那一份**里真的没了，选中的节点还在。
        let on_disk: SingBoxConfig =
            serde_json::from_slice(&std::fs::read(&path).expect("闸门必须把最终配置写盘")).unwrap();
        assert!(
            !on_disk.outbounds.iter().any(|o| o.tag == "BAD"),
            "被内核拒收的节点必须从落盘配置里消失（剥了不重新生成 = 白剥）"
        );
        assert!(
            on_disk.outbounds.iter().any(|o| o.tag == "KEEP"),
            "其余节点必须照常保留 —— 一个坏节点不该连累全局"
        );
        assert_eq!(peeled.keys().collect::<Vec<_>>(), vec!["n-bad"]);

        // 断言 2：走**既有**上报通道（`InvalidNode` → `EVENT_PROXY_INVALID_NODES`），不是新造机制。
        assert_eq!(
            gated.invalid_nodes,
            vec![InvalidNode {
                id: "n-bad".into(),
                tag: "BAD".into(),
                reason: INVALID_REASON_KERNEL_REJECTED.to_string(),
            }],
            "剥掉的节点必须带成因上报（节点消失而不告知比报错更坏）"
        );
        assert!(gated.blocked.is_none(), "被拒的不是选中节点 ⇒ 不该落终态");
        assert_eq!(gated.checks_run, 2, "一次发现 + 一次确认，恰好两次 check");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **整环门：内核拒的若是选中节点 → `blocked` 落值，且绝不把它剥掉**。
    ///
    /// 变异：把 `PeelTarget::Blocked` 那一支改成照常 `Peel` ⇒ `blocked.is_none()` 断言红；
    /// 更坏的是生产行为——剥掉选中节点后下一轮 generate 直接 `Selected server not found`，
    /// 用户拿到的又是一句和现场无关的话。
    #[cfg(unix)]
    #[tokio::test]
    async fn kernel_rejecting_the_selected_node_yields_blocked_not_a_silent_exit_switch() {
        let (rt, dir) = test_runtime();
        let cfg = gate_two_node_config();
        let user_config: UserConfig = serde_json::from_value(cfg.clone()).unwrap();
        let deps = rt.generate_deps(0, 0, &[], &cfg);
        let path = dir.join("gate-probe.json");

        let mut peeled = BTreeMap::new();
        let base = rt
            .generate_and_gate(&user_config, &deps, &path, None, &mut peeled)
            .await
            .unwrap();
        let keep_index = base
            .config
            .outbounds
            .iter()
            .position(|o| o.tag == "KEEP")
            .expect("KEEP 节点应在生成的 outbounds 里");

        let fake = write_fake_checking_core(&dir, keep_index);
        let mut peeled = BTreeMap::new();
        let gated = rt
            .generate_and_gate(&user_config, &deps, &path, Some(&fake), &mut peeled)
            .await
            .unwrap();

        let (blocked, detail) = gated.blocked.expect("内核拒选中节点 ⇒ 必须落 blocked");
        assert_eq!(blocked.tag, "KEEP");
        assert_eq!(blocked.id, "n-keep");
        assert_eq!(blocked.reason, INVALID_REASON_KERNEL_REJECTED);
        assert!(
            detail.contains("unknown outbound type"),
            "必须把内核原话交出去（用户要靠它知道到底哪儿错了）；实得 {detail:?}"
        );
        assert!(
            peeled.is_empty(),
            "选中节点绝不许被剥 —— 剥了就是背着用户改出口（EXIT_MISMATCH 要抓的正是这个）"
        );
        assert_eq!(gated.checks_run, 1, "一次 check 判完即终态，不再重生成");
        // 🔴 变异锁：`blocked` 那个节点**也必须**进上报清单 —— 否则卡片不标灰，用户只剩一条会消失的
        // toast。变异：把 `assemble` 里 `.chain(blocked.iter()…)` 删掉 ⇒ 本条断在空 Vec。
        assert_eq!(
            gated.invalid_nodes,
            vec![InvalidNode {
                id: "n-keep".into(),
                tag: "KEEP".into(),
                reason: INVALID_REASON_KERNEL_REJECTED.to_string(),
            }],
            "被拒的选中节点必须走同一条通道上报（持久标灰是用户回头修它时唯一还在的线索）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **剥除会改写幸存同名节点的 tag —— 故闸门必须把「剥后的那份 servers」交出来。**
    ///
    /// 机制：`build_id_to_tag_map` 按**名字**去重、撞名追加 `(n)` ⇒ tag 是**整个集合**的函数，
    /// 不是单个节点的函数。剥掉第一个「HK」之后，第二个在重新生成的配置里就叫「HK」而不是「HK (1)」。
    ///
    /// 为什么这是 blocker 而不是洁癖：起核后有三处要按 `serverId` 反算运行核里的 tag ——
    /// `attest_selected_exit`（出口自证，`code::EXIT_MISMATCH` 是「用户以为走代理、实则明文直连」的
    /// **唯一**告警通道）、`build_switch_snapshot`（规则热切 PUT 的目标出站）、`ts_tag_to_id`。
    /// 它们若拿未剥的全量 servers 算，得到的 tag 在运行核里根本不存在 ⇒ 出口完全正确却打
    /// EXIT_MISMATCH 假警报（告警一旦有假就会被整体无视）、热切 PUT 静默打空。
    ///
    /// 本测同时钉住**两侧**：① 闸门交出的 `effective_user_config` 确实是剥后的；
    /// ② 用它算出的 tag 与用全量算出的**确实不同** —— 没有 ② 的话，哪天去重规则变了、
    /// 两者恒等，本测就退化成一条恒真断言而没人发现。
    #[cfg(unix)]
    #[tokio::test]
    async fn peeling_reshuffles_duplicate_name_tags_so_the_gate_hands_back_the_peeled_servers() {
        let (rt, dir) = test_runtime();
        // 两个**同名**节点：撞名去重会让第二个拿到 `HK (1)`。选中第三个，免得撞上 Blocked 腿。
        let cfg = serde_json::json!({
            "servers": [
                { "id": "n-a", "name": "HK", "protocol": "shadowsocks",
                  "address": "1.2.3.4", "port": 8388, "method": "aes-256-gcm", "password": "p" },
                { "id": "n-b", "name": "HK", "protocol": "shadowsocks",
                  "address": "5.6.7.8", "port": 8388, "method": "aes-256-gcm", "password": "p" },
                { "id": "n-sel", "name": "SEL", "protocol": "shadowsocks",
                  "address": "9.9.9.9", "port": 8388, "method": "aes-256-gcm", "password": "p" }
            ],
            "selectedServerId": "n-sel",
            "proxyMode": "global",
            "proxyModeType": "manual",
            "mixedPort": 17891,
        });
        let user_config: UserConfig = serde_json::from_value(cfg.clone()).unwrap();
        let deps = rt.generate_deps(0, 0, &[], &cfg);
        let path = dir.join("gate-dup.json");

        // 前提对照：未剥之前，两个同名节点确实拿到不同 tag（去重规则还在）。
        let tag_of = |uc: &UserConfig, id: &str| -> String {
            let wrappers: Vec<ServerLikeRef> = uc.servers.iter().map(ServerLikeRef).collect();
            build_id_to_tag_map(&wrappers)
                .into_iter()
                .find(|(k, _)| k == id)
                .expect("id 必须在表里")
                .1
        };
        assert_eq!(tag_of(&user_config, "n-a"), "HK");
        assert_eq!(
            tag_of(&user_config, "n-b"),
            "HK (1)",
            "撞名去重规则变了 —— 下面整条推理的前提没了，先确认新规则再改本测"
        );

        // 剥掉 n-a（第一个 HK）。下标由 failOpen 腿现算，不写死。
        let mut peeled = BTreeMap::new();
        let base = rt
            .generate_and_gate(&user_config, &deps, &path, None, &mut peeled)
            .await
            .unwrap();
        let a_index = base
            .config
            .outbounds
            .iter()
            .position(|o| o.tag == "HK")
            .expect("HK 应在生成的 outbounds 里");
        let fake = write_fake_checking_core(&dir, a_index);
        let mut peeled = BTreeMap::new();
        let gated = rt
            .generate_and_gate(&user_config, &deps, &path, Some(&fake), &mut peeled)
            .await
            .unwrap();
        assert_eq!(peeled.keys().collect::<Vec<_>>(), vec!["n-a"]);

        // ① 闸门交出的就是剥后的那份。
        let eff = &gated.effective_user_config;
        assert_eq!(
            eff.servers
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["n-b", "n-sel"],
            "effective_user_config 必须是剥除之后的 servers —— 下游三处都按它算 tag"
        );

        // ② 用剥后的算，n-b 的 tag 变成了「HK」；用全量算还是「HK (1)」。两者**必须**不同，
        //    否则本测没有区分力（而生产上那三处正是靠这个差别才会打假警报）。
        assert_eq!(
            tag_of(eff, "n-b"),
            "HK",
            "剥掉第一个 HK 之后，幸存的同名节点在运行核里就叫 HK"
        );
        assert_ne!(
            tag_of(eff, "n-b"),
            tag_of(&user_config, "n-b"),
            "剥前剥后算出的 tag 竟然一样 —— 本测失去区分力，先确认去重规则是不是变了"
        );
        // 落盘的那份印证同一件事：运行核里 `HK (1)` 这个 tag 根本不存在。
        let on_disk: SingBoxConfig =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            !on_disk.outbounds.iter().any(|o| o.tag == "HK (1)"),
            "运行核里不该再有 `HK (1)` —— 按全量算 tag 的下游会去找一个不存在的出站"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🔴 **起核重试腿不得把闸门的剔除上报清空。**
    ///
    /// `kernel_peeled` 声明在重试循环**之外**（同一节点、同一个核，判定不会变 ⇒ 第 2 腿沿用即可，
    /// 恒只付 1 次 check）。而上报清单若是每次调用新建的局部 `Vec`，第 2 腿 emit 的就是一份**空数组**
    /// —— 节点仍被剥出落盘配置，前端 store 整表替换后已标灰的卡片被清掉。
    /// 「节点消失而不告知比报错更坏」，这正是那个形态。
    ///
    /// 修法是让上报清单**由 `peeled` 现导**（`assemble` 里 `peeled.values()`），二者不可能再漂。
    /// 本测模拟第 2 腿：`peeled` 预置一条，配置本身健康（假核 marker 已存在 ⇒ 直接 rc=0）。
    #[cfg(unix)]
    #[tokio::test]
    async fn retry_leg_keeps_reporting_nodes_peeled_by_an_earlier_leg() {
        let (rt, dir) = test_runtime();
        let cfg = gate_two_node_config();
        let user_config: UserConfig = serde_json::from_value(cfg.clone()).unwrap();
        let deps = rt.generate_deps(0, 0, &[], &cfg);
        let path = dir.join("gate-retry.json");

        // 假核：marker 已存在 ⇒ 本次 check 一律 rc=0（= 上一腿已把坏节点剥干净的现场）。
        let fake = write_fake_checking_core(&dir, 0);
        std::fs::write(dir.join("gate-check-seen"), b"1").unwrap();

        // 第 1 腿的产物：一条已剥记录。
        let mut peeled = BTreeMap::new();
        peeled.insert(
            "n-bad".to_string(),
            InvalidNode {
                id: "n-bad".into(),
                tag: "BAD".into(),
                reason: INVALID_REASON_KERNEL_REJECTED.to_string(),
            },
        );

        let gated = rt
            .generate_and_gate(&user_config, &deps, &path, Some(&fake), &mut peeled)
            .await
            .unwrap();

        assert_eq!(
            gated.checks_run, 1,
            "沿用上一腿的剥除结果 ⇒ 本腿只付 1 次 check"
        );
        assert!(
            !gated.config.outbounds.iter().any(|o| o.tag == "BAD"),
            "已剥节点在本腿仍不得出现在配置里"
        );
        // 🔴 核心断言：节点从配置里消失了，上报清单就**必须**同时还带着它。
        assert_eq!(
            gated.invalid_nodes,
            vec![InvalidNode {
                id: "n-bad".into(),
                tag: "BAD".into(),
                reason: INVALID_REASON_KERNEL_REJECTED.to_string(),
            }],
            "重试腿把剔除上报清空了 ⇒ 前端整表替换后标灰被抹掉，节点消失而用户毫不知情"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 🔴 **出口 IP 重探腿与 unlock 失效腿必须成对**——起核 / 停核 / 热切三点，一个都不许漏。
///
/// # 为什么必须是源码扫描，而不是行为测试
///
/// 三个触发点里只有**停核**能在单测里真跑（`stop()` 无核也走 `stop_inner`）；起核与热切都要真起核 +
/// 真管理 API，属真机门。而这条不变式恰恰是本轮真机反馈「IP/延迟需手点」的根因所在：上游的触发表
/// 在 Polaris 侧**整列为空**，宿主（`invalidate_unlock_cache` 三点）明明已就位、只是没人接上去。
///
/// 故守的不是「某次调用发生了」，而是**结构性配对**：凡是判定「出口换了一次」而失效解锁快照的地方，
/// 出口 IP 也必然作废（同一个物理事实的两个下游）。将来有人加第四个触发点，本守卫会逼他对出口 IP 这条
/// 腿做一次显式决定，而不是默默漏掉——那正是这次漏掉的方式。
///
/// # ⚠️ 本守卫的逃逸面（已知取舍，别高估它的射程）
///
/// 判据是**文本邻近**（`WINDOW = 6` 行内出现配对调用），**不是同一执行分支**。因此下面这类写法守卫
/// 照样放行，而真机行为已经错了：
///
/// ```ignore
/// self.invalidate_unlock_cache();
/// if some_condition {
///     self.schedule_exit_ip_refresh(delay);   // 只在部分分支跑 —— 守卫看不出来
/// }
/// ```
///
/// 同理，两者被塞进不同的 `match` 臂、早退 `return` 之后、或相隔 7 行以上（哪怕逻辑正确）都会让守卫
/// 给出错误答案（前两种假绿，后一种假红）。要真正锁住「同一分支必然成对」得做控制流分析，静态文本扫描
/// 够不着，不在本批范围。本守卫只承诺：**触发点的数目**变了必转红（`KNOWN_TRIGGER_SITES` 写死），逼
/// 改动者对新触发点做一次显式决定。
#[cfg(test)]
mod exit_ip_wiring_guard {
    const SRC: &str = include_str!("proxy.rs");

    /// 被守的调用点标记（带 `self.` 前缀 ⇒ 不会撞上各自的 `fn` 定义行）。
    const INVALIDATE: &str = "self.invalidate_unlock_cache(";

    /// 出口 IP 腿的**全部合法形态**（同一物理事实的下游，任一出现即算配对）。
    ///
    /// 为什么是三个而不是一个：「出口换了一次」的下游动作**本来就分岔**——
    /// - [`REFRESH`]：出口有效、值待测 ⇒ 排一次真探测（起核 / 停核 / 热切 / TS 隧道就绪 / 出口恢复）；
    /// - [`MARK_BLOCKED`]：出口**已知无效** ⇒ 不探测、直落终态（R2 `none→blocked`）。此时排探测是错的：
    ///   必然打空转 20s 重试预算再吐 null，用户看到「一直在检测」而不是「出口无效」；
    /// - [`RECOVERY`]：出口恢复 ⇒ 先热重设 exit_node + 重申路由**再**探（顺序不可换，见
    ///   `ts_exit_recover_once`）。探测调用在异步腿内部（`me.` 前缀，不带 `self.`），故必须把
    ///   恢复腿本身登记成一条合法形态，否则守卫会把它误判成「有失效没重探」。
    ///
    /// **放宽了吗？没有**：守卫的射程仍是「每个失效点旁必须有一个被点名的出口 IP 腿」+「总数写死」。
    /// 新增第四种形态同样要改这张表 —— 那正是要逼出的那次显式裁定。
    ///
    /// [`REFRESH`]: self::REFRESH
    /// [`MARK_BLOCKED`]: self::MARK_BLOCKED
    /// [`RECOVERY`]: self::RECOVERY
    const REFRESH: &str = "self.schedule_exit_ip_refresh(";
    const MARK_BLOCKED: &str = "self.mark_exit_blocked(";
    const RECOVERY: &str = "self.spawn_ts_exit_recovery(";
    const EXIT_IP_LEGS: &[&str] = &[REFRESH, MARK_BLOCKED, RECOVERY];

    /// 已知触发点数（起核就绪 / 停核 / 热切成功 / TS 隧道就绪 / **R2 出口无效 none→blocked** /
    /// **R2 出口恢复 blocked→none**）。**写死是刻意的**：数目变了就说明有人动了触发表，该让他停下来
    /// 显式裁定新触发点要不要重探出口 IP，而不是让守卫自适应地放行。
    ///
    /// 第四点（TS 隧道就绪，`apply_ts_status_frame`）是 2026-07-21 补的：§10.1 的 上游 触发表本就含它，
    /// 而 Polaris 侧只接了「广播半边」（emit_tailscale_status），既不失效解锁缓存也不重探出口 IP ——
    /// 守卫当时**对它天然失明**（它压根不在扫描命中的三个点里）。补线后数目从 3 变 4，守卫方能看见。
    ///
    /// 第五、六点（`reconcile_ts_exit_block` 的两条跨态腿）是 R2 补的：出口从有效变无效 / 从无效恢复，
    /// 与前四点是**同一个物理事实**（当前出口换了），只是下游动作分岔成「落终态」与「先修再探」。
    const KNOWN_TRIGGER_SITES: usize = 6;

    /// 出口 IP 腿的**调用点**总数 = 触发点数 + 1。
    ///
    /// 多出的那一条是 `ts_exit_recover_once` 里的 `schedule_exit_ip_refresh` —— 它不是独立触发点，
    /// 而是**恢复腿自己的收尾**（`reapply → reassert → refresh` 三步的第三步，见该方法文档）。
    /// 触发侧记的是 `spawn_ts_exit_recovery`，真探测被推迟到那条异步腿里执行。
    ///
    /// **为什么不把它并进触发点数**：那会让「有失效没配腿」的判据被稀释 —— 两个数各自写死、各自解释，
    /// 谁变了都要停下来说清楚，正是本守卫要的效果。
    const KNOWN_EXIT_IP_LEG_SITES: usize = KNOWN_TRIGGER_SITES + 1;

    /// 生产区源码（剥掉 `mod tests`，并剥掉整行注释）。
    ///
    /// **两处都必须剥**，否则守卫会假绿/假红：`mod tests` 里本来就调这两个方法（不剥 ⇒ 测试代码自己
    /// 就能满足配对断言）；文档注释里 `[`invalidate_unlock_cache`]` 这类链接遍布（不剥 ⇒ 注释被当调用点）。
    fn production_lines() -> Vec<String> {
        let end = SRC
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("锚点 `mod tests` 消失，守卫已失去生产区边界");
        SRC[..end]
            .lines()
            .map(|l| {
                if l.trim_start().starts_with("//") {
                    String::new()
                } else {
                    l.to_string()
                }
            })
            .collect()
    }

    /// 每个 `invalidate_unlock_cache` 调用点后 `WINDOW` 行内必须出现 `schedule_exit_ip_refresh`。
    /// 窗口留 6 行是为容纳两者之间那段解释性注释（注释已被剥成空行，仍占行位）。
    const WINDOW: usize = 6;

    #[test]
    fn every_unlock_invalidation_site_also_refreshes_exit_ip() {
        let lines = production_lines();
        let sites: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains(INVALIDATE))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            sites.len(),
            KNOWN_TRIGGER_SITES,
            "触发点数量变了（{} 个）：新增/删除「出口换了一次」的判定点时，必须同时裁定出口 IP 重探腿",
            sites.len()
        );
        for i in sites {
            let paired = lines[i + 1..(i + 1 + WINDOW).min(lines.len())]
                .iter()
                .any(|l| EXIT_IP_LEGS.iter().any(|leg| l.contains(leg)));
            assert!(
                paired,
                "第 {} 行的 invalidate_unlock_cache 后 {WINDOW} 行内没有任何出口 IP 腿（重探 / 落无效终态 / \
                 恢复腿）⇒ 该触发点的出口 IP/延迟不会自动刷新，退回「必须手点网络检测」的真机缺陷",
                i + 1
            );
        }
    }

    /// 🔵 **接线守卫**：`mark_exit_blocked` 必须**委托**给 `commands::misc` 的权威缓存写入腿，
    /// 而不是就地 broadcast 一帧了事。
    ///
    /// # 这条补的是什么洞
    ///
    /// 旧实现只 `broadcast(EVENT_IP_INFO_UPDATED, …)`。事件只喂**订阅方**（状态栏）；`ipinfo:get(peek)`
    /// 型消费方（托盘浮层每次弹出即 peek、主窗窗口重建水合）**不订阅**、只读 `IPINFO_CACHE` ⇒ 出口被
    /// 直判无效之后，那两处继续吐上一次探到的代理出口 IP。同屏两处对「我现在从哪出去」互相矛盾，且错的
    /// 那个是「用一个已知无效的旧出口冒充当前出口」。
    ///
    /// 行为测试够不着：本方法在 `AppHandleProxyErrorEmitter` 上，要真 `AppHandle`（本仓未引 `tauri::test`）。
    ///
    /// 牙：① 把委托改回就地 `json!` + broadcast ② 删掉委托调用 —— 两条均转红。
    #[test]
    fn mark_exit_blocked_delegates_to_the_authoritative_cache_writer() {
        let lines = production_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("crate::commands::misc::mark_ipinfo_proxy_blocked(")),
            "出口无效终态必须经 commands::misc 的权威缓存写入腿落地（只广播不写缓存 ⇒ peek 型消费方读陈旧出口）"
        );
        assert!(
            !lines.iter().any(|l| l.contains("\"proxyBlocked\":")),
            "emitter 侧不得就地拼 ipInfo 载荷：那会绕开 direct 保留 / error 删键 / 缓存写回三条语义，\
             并让载荷形状出现第二个真相源"
        );
    }

    /// 守卫的守卫：证明扫到的是**真的生产区**，而不是空串 / 被剥光的一片空行。
    /// 空输入会让上面的 `sites.len()` 恒为 0 —— 那是「return 型门 = 没门」的形态，只不过这里表现为
    /// 数量断言恒红；仍显式钉住正向内容，避免将来有人「修」成 `>= 0` 之类的宽松判据。
    #[test]
    fn guard_scan_actually_captured_the_production_region() {
        let lines = production_lines();
        assert!(
            lines.len() > 3_000,
            "扫到的生产区只有 {} 行 ⇒ 边界锚点漂了，守卫失去判据",
            lines.len()
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| EXIT_IP_LEGS.iter().any(|leg| l.contains(leg)))
                .count(),
            KNOWN_EXIT_IP_LEG_SITES,
            "生产区里出口 IP 腿的调用点总数变了：要么有腿没配对失效侧（多），要么某条触发点的腿被删（少）——\
             两种都必须停下来显式裁定，不许让守卫自适应放行"
        );
        // 三种形态各自至少有一个真实调用点 —— 防「把某个 leg 常量留在表里、生产侧其实已删」的假绿：
        // 那种状态下总数断言可以靠另外两种形态凑够，而被删的那条腿永远没人再守。
        for leg in EXIT_IP_LEGS {
            assert!(
                lines.iter().any(|l| l.contains(leg)),
                "出口 IP 腿 `{leg}` 在生产区零调用点 ⇒ 它要么已被删（该同步删表项），要么从未接线"
            );
        }
        // 反向自证：确认剥注释没有把代码一并剥掉（`fn` 定义行本身不带 `self.` 前缀，不计入调用点）。
        assert!(
            lines
                .iter()
                .any(|l| l.contains("fn schedule_exit_ip_refresh")),
            "连方法定义都没扫到 ⇒ 剥注释逻辑把代码也剥了"
        );
    }
}
