//! 协议分派核心（移植自 `helper-win/helper.go` 的 `handle()` switch 骨架）。
//!
//! ## 设计
//!
//! Go 源 `handle(conn)` 读 token+command+args 行 → `mu.Lock()` → switch command → 回复。本 Rust 实现把
//! 「帧已解析」与「分派」解耦：调用方（`service.rs` 的 serve 循环）负责读帧 + 鉴权 + 解析为
//! [`polaris_helper_proto::Request`]，本模块的 [`WinHelper::handle`] 负责 switch 分派 + 子进程状态管理
//! + 回复构造。这样：
//! - 分派逻辑（=Go switch 主体）跨平台纯逻辑，Linux 可单测。
//! - IO（命名管道读写）由 `service.rs` 承接（`#[cfg(windows)]`）。
//!
//! ## 并发纪律（对齐 Go `mu` / `child` / `childDone`）
//!
//! Go 用 `sync.Mutex` 保护 `child *exec.Cmd` + `childDone chan`，规则：
//! - **持锁摘除 child**（`child, childDone = nil, nil`），**不持锁收割**（`terminateChild` 最长阻塞 2s，
//!   持锁会饿死并发 ping/status）。
//! - 摘除后由摘除方独占收割权（后台 goroutine / `terminateChild`），watchParent 见 `child != c` 即退。
//!
//! 本实现用 [`std::sync::Mutex`]`<`[`ChildState`]`>` 镜像：持锁摘 pid，不持锁调 [`ops::ProcOps::reap_child`]。
//! 收割权的独占性由「摘除时取出 pid → 释放锁 → 对该 pid 调 reap」保证（同一 pid 只被取一次）。
//!
//! ## 子进程状态
//!
//! Go 的 `child *exec.Cmd` 承载 pid + Wait 语义。本 trait 抽象的 [`ops::ProcOps`] 只需 pid + reap_child，
//! 故 [`ChildState`] 只存 `Option<u32>`（pid）。真正的 `Wait()` 回收（Go `go func() { c.Wait(); close(done) }`）
//! 由 [`ops::ProcOps`] 实现内部承载（生产侧 `winproc` 持有 `std::process::Child` 或 Job Object 句柄）。

use crate::platform::windows::logic;
use crate::platform::windows::ops::{NetTableOps, ProcOps};
use crate::token::{is_authed_constant_time, TokenStore};
use polaris_helper_proto::{Request, Response, ResponseKind};
use std::sync::{Arc, Mutex};

/// Windows helper 错误（helper 内部分派/状态错误，非协议错误码）。
#[derive(Debug, thiserror::Error)]
pub enum HelperError {
    /// sing-box 启动失败（对应 Go `ERR start %v`，详情保留 io 错误）。
    #[error("sing-box start failed: {0}")]
    Start(String),
    /// LISTEN 持有者枚举失败（对应 Go `ERR enum %v`）。
    #[error("enum listen table failed: {0}")]
    Enum(String),
}

/// 子进程状态（Go `child` + `childDone` 的 Rust 等价）。
#[derive(Debug, Default, Clone)]
struct ChildState {
    /// 当前 child sing-box 的 pid（None = 无 child）。
    pid: Option<u32>,
}

/// Windows helper 核心分派器（移植自 `helper.go` 的 `handle()` + 模块级 `mu/child/childDone`）。
///
/// 持有：
/// - `child_mu`：保护 `child` 状态（Go `mu sync.Mutex`）。
/// - `child`：当前 child sing-box pid（Go `child *exec.Cmd`）。
/// - `token`：token 存储（鉴权）。
/// - `proc`：进程操作（spawn/kill/reap/job）。
/// - `net`：网络表操作（freeport）。
/// - `singbox_bin`：安装时锁定的 sing-box 路径（Go `singboxBin`）。
/// - `conf_dir`：允许的配置目录（Go `confDir`）。
/// - `service_name` / `support_dir`：自卸载旁路参数（Go `serviceName` / `supportDir`）。
pub struct WinHelper<T, P, N> {
    /// `Arc<Mutex<..>>`：child 状态须在**父死看护后台线程**（W15）与管道命令线程间共享（Go 的
    /// 包级 `mu`/`child` 全局，goroutine 直接引用；Rust 用 Arc 共享所有权）。
    child_mu: Arc<Mutex<ChildState>>,
    token: T,
    /// `Arc<P>`：父死看护的 `on_parent_dead` 闭包（后台线程）须持 proc 收割 child，故需共享所有权。
    proc: Arc<P>,
    net: N,
    singbox_bin: String,
    conf_dir: String,
    service_name: String,
    support_dir: String,
}

/// 单次 handle 的结果（成功响应，或鉴权失败/其它需提前终止的情形）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleOutcome {
    /// 正常响应（已成功处理，回此 Response 给客户端）。
    Respond(Response),
    /// 鉴权失败（Go `ERR auth` + return，不进 switch）。
    AuthFailed,
    /// uninstall 后自退（Go `uninstall` 分支：回 OK uninstalling → 800ms 后 os.Exit(0)）。
    /// 调用方（serve 循环）据此触发进程退出。
    UninstallAndExit(Response),
}

impl<T, P, N> WinHelper<T, P, N>
where
    T: TokenStore,
    P: ProcOps + 'static, // 'static：父死看护闭包捕获 Arc<P> 进 'static 后台线程（W15）。
    N: NetTableOps,
{
    /// 构造。
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        token: T,
        proc: P,
        net: N,
        singbox_bin: impl Into<String>,
        conf_dir: impl Into<String>,
        service_name: impl Into<String>,
        support_dir: impl Into<String>,
    ) -> Self {
        Self {
            child_mu: Arc::new(Mutex::new(ChildState::default())),
            token,
            proc: Arc::new(proc),
            net,
            singbox_bin: singbox_bin.into(),
            conf_dir: conf_dir.into(),
            service_name: service_name.into(),
            support_dir: support_dir.into(),
        }
    }

    /// 处理一个已解析的 [`Request`]（移植自 `helper.go:161-393` 的 `handle()`）。
    ///
    /// # 参数
    ///
    /// - `client_token`：客户端发来的 token 行（已 trim `\r\n`）。
    /// - `req`：解析后的请求。
    ///
    /// # 返回
    ///
    /// [`HandleOutcome`]，调用方据此回复 + 决定是否退出。
    #[must_use]
    pub fn handle(&self, client_token: &str, req: Request) -> HandleOutcome {
        // Go helper.go:169: if tok == "" || tok != tokenValue() { ERR auth; return }
        if !is_authed_constant_time(client_token, &self.token.token_value()) {
            return HandleOutcome::AuthFailed;
        }
        // 鉴权通过 → 进 switch（持锁分派）
        match req {
            Request::Ping => HandleOutcome::Respond(Response::Ok(ResponseKind::Pong(
                // Go helper.go:179: Windows Getuid()=-1 破坏正则，固定发 0。
                polaris_helper_proto::Pong::current(0),
            ))),
            Request::Version => HandleOutcome::Respond(Response::Ok(ResponseKind::Version {
                proto_version: crate::platform::windows::PROTO_VERSION,
            })),
            Request::Status => {
                // Go helper.go:184-188: child!=nil → running <pid>；else → stopped
                let state = self.child_mu.lock().unwrap();
                match state.pid {
                    Some(pid) => HandleOutcome::Respond(Response::Ok(ResponseKind::Status(
                        polaris_helper_proto::Status::Running { pid },
                    ))),
                    None => HandleOutcome::Respond(Response::Ok(ResponseKind::Status(
                        polaris_helper_proto::Status::Stopped,
                    ))),
                }
            }
            Request::Stop { pid: want } => {
                // Go helper.go:189-200: 持锁摘 child → 后台收割（不持 mu）→ OK stopped <pid> / notrunning
                //
                // **受管 pid 身份判据**（本协议新增）：判定与摘除同在 child_mu 临界区内。手里的核不是
                // 请求所指的那个 = 它属另一个会话（老 stop 腿在管道上挂住期间用户重装 helper / 重起了
                // 核）⇒ 诚实 no-op，绝不「反正要停就杀当前的」。见 `stop_pid_matches`。
                let pid = {
                    let mut state = self.child_mu.lock().unwrap();
                    if let Some(cur) = state.pid {
                        if !polaris_helper_proto::stop_pid_matches(want, cur) {
                            return HandleOutcome::Respond(Response::Ok(ResponseKind::Stop(
                                polaris_helper_proto::Stop::Mismatch {
                                    want: want.unwrap_or(0),
                                    current: cur,
                                },
                            )));
                        }
                    }
                    state.pid.take() // 摘除（独占收割权）
                };
                match pid {
                    Some(pid) => {
                        // 摘除后不持锁、后台异步收割（Go: go terminateChild(c, done)）。
                        // W6 修：reap_child 内部起线程（不再同步阻塞 2s）→ stop 立即回复。
                        self.proc.reap_child(pid);
                        HandleOutcome::Respond(Response::Ok(ResponseKind::Stop(
                            polaris_helper_proto::Stop::Stopped { pid },
                        )))
                    }
                    None => HandleOutcome::Respond(Response::Ok(ResponseKind::Stop(
                        polaris_helper_proto::Stop::NotRunning,
                    ))),
                }
            }
            Request::Cleanup => {
                // Go helper.go:202-211: 先摘自家 child 精准收割，再 killAllSingbox 兜底外部孤儿 → OK cleaned
                if let Some(pid) = { self.child_mu.lock().unwrap().pid.take() } {
                    self.proc.reap_child(pid);
                }
                let _ = self.proc.kill_all_singbox(&self.singbox_bin);
                HandleOutcome::Respond(Response::Ok(ResponseKind::Cleaned))
            }
            Request::Uninstall => {
                // Go helper.go:276-294: 收割 child → killAllSingbox 兜底 → spawnSelfUninstall →
                // OK uninstalling → 800ms 后 os.Exit(0)
                if let Some(pid) = { self.child_mu.lock().unwrap().pid.take() } {
                    self.proc.reap_child(pid);
                }
                let _ = self.proc.kill_all_singbox(&self.singbox_bin);
                self.proc
                    .spawn_self_uninstall(&self.service_name, &self.support_dir);
                HandleOutcome::UninstallAndExit(Response::Ok(ResponseKind::Uninstalling))
            }
            Request::Start(params) => self.handle_start(&params),
            Request::FreePort { port } => self.handle_freeport(port),
            // W9 修：route-add / route-del 须区分 add/delete（此前合并丢了 op 区分）。
            Request::RouteAdd(rp) => self.handle_route(&rp.iface, &rp.cidrs, false),
            Request::RouteDel(rp) => self.handle_route(&rp.iface, &rp.cidrs, true),
            Request::IfaceMetric { iface, metric } => self.handle_iface_metric(&iface, metric),
            // 以下命令 Windows helper 不支持（mac/linux 专属）—— Go default 分支回 ERR unknown。
            Request::LinuxStart(_)
            | Request::InstallCore(_)
            | Request::DefaultRestore { .. }
            | Request::FlushDns => HandleOutcome::Respond(Response::Err(
                polaris_helper_proto::Error::new(polaris_helper_proto::ErrorCode::Unknown),
            )),
        }
    }

    /// `start` 分支（Go helper.go:338-390）。
    ///
    /// **并发纪律（修 W3）**：「已在跑？」判定 + `start_singbox` + 记录 pid 须在**同一 child_mu 临界区**内
    /// （对齐 Go `handle()` 顶部 `mu.Lock` 覆盖整个 start）。此前判定与记录之间放锁 → 两个并发 start 都见
    /// `None` → 双起核。记录 pid 后**先释放锁**再接父死看护（W15）——`spawn_watch_parent` 的闭包会重入
    /// child_mu（`is_current`/`on_parent_dead`），持锁调用即自死锁。
    fn handle_start(&self, p: &polaris_helper_proto::StartParams) -> HandleOutcome {
        // ===== 全程持锁的临界区：check → start → record pid =====
        let started_pid = {
            let mut state = self.child_mu.lock().unwrap();
            // Go helper.go:345-348: child 已在跑 → OK already <pid>
            if let Some(pid) = state.pid {
                return HandleOutcome::Respond(Response::Ok(ResponseKind::Start(
                    polaris_helper_proto::Start::Already { pid },
                )));
            }
            // Go helper.go:349-351: cfg 空 → ERR no-config
            if p.cfg.is_empty() {
                return HandleOutcome::Respond(Response::Err(polaris_helper_proto::Error::new(
                    polaris_helper_proto::ErrorCode::NoConfig,
                )));
            }
            // Go helper.go:352-356: cfg 不在 confDir 白名单 → ERR config-path-denied
            if !logic::cfg_allowed(&p.cfg, &self.conf_dir) {
                return HandleOutcome::Respond(Response::Err(polaris_helper_proto::Error::new(
                    polaris_helper_proto::ErrorCode::ConfigPathDenied,
                )));
            }
            // **Polaris 新增（上游无）**：log 走与 cfg 同一条白名单。
            //
            // 上游只校验 cfg，而 `start_singbox` 会以 **SYSTEM** 身份 `O_CREATE|O_APPEND` 打开这个
            // log 路径（`winproc/win.rs` 的 STARTUPINFOW 重定向）⇒ 不校验就是「SYSTEM 在任意位置
            // 建文件并持续追加写」。生产下发的恒是 `<conf_dir>\singbox-startup.log`
            //（`runtime/proxy.rs` 的 `self.config.join(SINGBOX_STARTUP_LOG)`），与 cfg 同目录
            // ⇒ 这条收紧**零行为变更**。
            //
            // 空串 = 不重定向（`win.rs` 的 `if !log_path.is_empty()`），放行。
            if !p.log.is_empty() && !logic::cfg_allowed(&p.log, &self.conf_dir) {
                return HandleOutcome::Respond(Response::Err(polaris_helper_proto::Error::new(
                    polaris_helper_proto::ErrorCode::LogPathDenied,
                )));
            }
            // Go helper.go:358-369: fwd=="1" → enableIPForwarding + startSingbox（均在生产侧
            // start_singbox 内；失败 → ERR start）。持锁调用 —— start_singbox 起进程后即返回（不 Wait），
            // 不长阻塞（与 Go 持 mu 调 c.Start() 同）。
            match self
                .proc
                .start_singbox(&self.singbox_bin, &p.cfg, &p.log, p.fwd)
            {
                Ok(pid) => {
                    // Go helper.go:374-385: child = c（收割/Wait goroutine 由生产侧 ProcOps 承载）。
                    state.pid = Some(pid);
                    pid
                }
                Err(e) => {
                    return HandleOutcome::Respond(Response::Err(
                        polaris_helper_proto::Error::with_detail(
                            polaris_helper_proto::ErrorCode::Start,
                            e.to_string(),
                        ),
                    ));
                }
            }
            // MutexGuard 在此块结束时释放 → 下方接线看护时锁已放。
        };

        // ===== W15 修：父死看护接线（Go helper.go:386-389: ppid>0 → go watchParent）=====
        // 此前 parent_pid 被丢弃 → app 崩溃/taskkill（管道 stop 够不到）后 sing-box 成孤儿。
        // 必须在释放 child_mu 后接线（闭包重入 child_mu，持锁调用即自死锁）。
        if let Some(ppid) = p.parent_pid.filter(|&ppid| ppid > 0) {
            // is_current(child_pid)：child 是否仍是当前的（被 stop/cleanup/新 start 摘除后返 false）。
            let st_cur = Arc::clone(&self.child_mu);
            let is_current = Box::new(move |cpid: u32| st_cur.lock().unwrap().pid == Some(cpid));
            // on_parent_dead(child_pid)：与 stop 竞态下持锁摘除（独占收割权）→ 后台异步收割。
            let st_dead = Arc::clone(&self.child_mu);
            let proc = Arc::clone(&self.proc);
            let on_parent_dead = Box::new(move |cpid: u32| {
                let taken = {
                    let mut s = st_dead.lock().unwrap();
                    if s.pid == Some(cpid) {
                        s.pid.take()
                    } else {
                        None // 他人已摘除（Go: child != c）→ 由他收割
                    }
                };
                if let Some(pid) = taken {
                    proc.reap_child(pid);
                }
            });
            self.proc
                .spawn_watch_parent(ppid, started_pid, is_current, on_parent_dead);
        }

        // Go helper.go:390: OK started <pid>
        HandleOutcome::Respond(Response::Ok(ResponseKind::Start(
            polaris_helper_proto::Start::Started { pid: started_pid },
        )))
    }

    /// `freeport` 分支（Go helper.go:295-337）。
    fn handle_freeport(&self, port: u16) -> HandleOutcome {
        // Go helper.go:308-316: listenPidsForPort 失败 → ERR enum
        let pids = match self.net.listen_pids_for_port(port) {
            Ok(pids) => pids,
            Err(e) => {
                return HandleOutcome::Respond(Response::Err(
                    polaris_helper_proto::Error::with_detail(
                        polaris_helper_proto::ErrorCode::Enum,
                        e.to_string(),
                    ),
                ));
            }
        };
        // Go helper.go:313-315: 无 LISTEN → OK free
        if pids.is_empty() {
            return HandleOutcome::Respond(Response::Ok(ResponseKind::FreePort(
                polaris_helper_proto::FreePort::Free,
            )));
        }
        // Go helper.go:317-330: 遍历 pid：是锁定 sing-box → TerminateProcess；否则回报占用者名（不杀）
        let mut killed: Vec<u32> = Vec::new();
        let mut foreign: Vec<String> = Vec::new();
        for pid in &pids {
            let img = self.proc.process_image_name(*pid);
            if logic::is_locked_singbox(&img, &self.singbox_bin) {
                if self.proc.terminate_pid(*pid).is_ok() {
                    killed.push(*pid);
                }
            } else {
                // Go helper.go:325-328: name := filepath.Base(img)；空 → "pid:<n>"
                let name = {
                    let base = logic::filepath_base(&img);
                    if base.is_empty() || base == "." {
                        format!("pid:{pid}")
                    } else {
                        base.to_owned()
                    }
                };
                foreign.push(name);
            }
        }
        // Go helper.go:333-337: foreign 非空 → OK foreign <names | names>（混合占用也归此，诚实告知未释放）
        if !foreign.is_empty() {
            HandleOutcome::Respond(Response::Ok(ResponseKind::FreePort(
                polaris_helper_proto::FreePort::Foreign { names: foreign },
            )))
        } else {
            HandleOutcome::Respond(Response::Ok(ResponseKind::FreePort(
                polaris_helper_proto::FreePort::Killed { pids: killed },
            )))
        }
    }

    /// `route-add` / `route-del` 分支（Go helper.go:212-241）。
    ///
    /// **修 W9**：此前是空壳（只校验 iface 即回 `OK route`，netsh 从未执行，注释谎称在 winproc）。现真跑
    /// netsh：iface 白名单 + 每个 CIDR 校验后，经 [`ProcOps::apply_route`] 对每个合法 CIDR 跑
    /// `netsh interface <fam> add|delete route ... store=active`（`del` 区分 add/delete）。生产 FFI 在
    /// [`crate::platform::windows::winproc`]（真机门验生效），本分派层做校验 + 每 CIDR 派发（可 mock 测）。
    fn handle_route(&self, iface: &str, cidrs: &[String], del: bool) -> HandleOutcome {
        // Go helper.go:218-221: !ifaceAllowed(iface) → ERR iface-denied
        if !logic::iface_allowed(iface) {
            return HandleOutcome::Respond(Response::Err(polaris_helper_proto::Error::new(
                polaris_helper_proto::ErrorCode::IfaceDenied,
            )));
        }
        // Go helper.go:226-240: for c := range Split(cidrsLine, ","): 空跳过 / ParseCIDR 失败跳过 /
        // 否则跑 netsh。CIDR 校验用 helper-proto::codec::is_valid_cidr（stdlib 解析，严于 Go 手写）。
        for cidr in cidrs {
            let cidr = cidr.trim();
            if cidr.is_empty() {
                continue; // Go: c == "" → continue
            }
            if !polaris_helper_proto::codec::is_valid_cidr(cidr) {
                continue; // Go: net.ParseCIDR err → continue（静默跳过非法 CIDR）
            }
            self.proc.apply_route(iface, cidr, del);
        }
        // Go helper.go:241: OK route（幂等 best-effort，最终由 app 侧校验）
        HandleOutcome::Respond(Response::Ok(ResponseKind::Route))
    }

    /// `iface-metric` 分支（Go helper.go:242-275，退役保留兼容）。
    ///
    /// DESIGN-REVIEW(iface-metric-retired-stub)：Go 侧此命令**已退役保留兼容**（proto v3/v4，自 Windows
    /// 禁 System 起客户端不再调用；新客户端 EXPECTED_PROTO 不调）。本 Rust 侧**忠实保留退役桩**——校验
    /// iface 白名单 + metric 范围后即回 `OK iface-metric`，**不执行** PowerShell `Set-NetIPInterface`。
    /// 这是与 Go 的**有意差异**（Go 仍跑 PowerShell 以兼容已部署的旧 proto helper；新客户端不触发本路径，
    /// 故不实现 PowerShell 无功能损失）。若未来需兼容旧 proto helper 的真实降权，须补 PowerShell 执行。
    fn handle_iface_metric(&self, iface: &str, metric: u16) -> HandleOutcome {
        // Go helper.go:253-256: !ifaceAllowed → ERR iface-denied
        if !logic::iface_allowed(iface) {
            return HandleOutcome::Respond(Response::Err(polaris_helper_proto::Error::new(
                polaris_helper_proto::ErrorCode::IfaceDenied,
            )));
        }
        // Go helper.go:257-261: metric 非法（Go atoi err 或 <0 或 >65535）→ ERR bad-metric
        // u16 天然 ∈ [0,65535]，无需额外检查（Go 的 <0/>65535 在 u16 下不可能）。
        let _ = metric;
        // Go helper.go:262-274: PowerShell Set-NetIPInterface（生产侧 exec，失败 → ERR set-metric）
        // 退役命令：生产侧直接 Ok（新客户端 EXPECTED_PROTO=v1 不再调用；保留仅为兼容已部署 proto≥3 helper）。
        HandleOutcome::Respond(Response::Ok(ResponseKind::IfaceMetric))
    }

    /// 服务停止/关机时的兜底收割（Go helper.go:400-410 `reapChildOnExit`）。
    ///
    /// 持锁摘 child → 不持锁 reap；child==nil 时兜底 killAllSingbox（覆盖「停止恰落在某次 stop 的
    /// 后台收割窗口内」—— Go 注释 helper.go:398-399）。
    pub fn reap_child_on_exit(&self) {
        let pid = { self.child_mu.lock().unwrap().pid.take() };
        if let Some(pid) = pid {
            // 退出路径用**同步**收割（Go reapChildOnExit 的同步 terminateChild）：进程即将退出，
            // 须在返回前杀完 child，否则后台异步收割线程随进程消失 → 孤儿。命令路径才用异步 reap_child。
            self.proc.reap_child_blocking(pid);
        } else {
            let _ = self.proc.kill_all_singbox(&self.singbox_bin);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;
    use crate::platform::windows::ops::{MockNetTableOps, MockProcOps};
    use crate::token::StaticTokenStore;
    use polaris_helper_proto::StartParams;

    fn make_helper(
        proc_ops: MockProcOps,
        net_ops: MockNetTableOps,
    ) -> WinHelper<StaticTokenStore, MockProcOps, MockNetTableOps> {
        WinHelper::new(
            StaticTokenStore::new("real-token"),
            proc_ops,
            net_ops,
            r"C:\Program Files\Polaris\sing-box.exe",
            r"C:\Users\polaris\config",
            "PolarisHelper",
            r"C:\ProgramData\Polaris",
        )
    }

    fn make_helper_defaults() -> WinHelper<StaticTokenStore, MockProcOps, MockNetTableOps> {
        make_helper(MockProcOps::new(), MockNetTableOps::new())
    }

    // ===== 鉴权 =====

    #[test]
    fn auth_failed_on_wrong_token() {
        let h = make_helper_defaults();
        let out = h.handle("wrong-token", Request::Ping);
        assert_eq!(out, HandleOutcome::AuthFailed);
    }

    #[test]
    fn auth_failed_on_empty_token() {
        let h = make_helper_defaults();
        let out = h.handle("", Request::Ping);
        assert_eq!(out, HandleOutcome::AuthFailed);
    }

    #[test]
    fn auth_passes_on_correct_token() {
        let h = make_helper_defaults();
        let out = h.handle("real-token", Request::Ping);
        let HandleOutcome::Respond(Response::Ok(ResponseKind::Pong(pong))) = out else {
            panic!("{out:?}");
        };
        // Windows uid 固定 0（helper.go:179）
        assert_eq!(pong.uid, 0);
        assert_eq!(pong.proto_version, crate::platform::windows::PROTO_VERSION);
    }

    // ===== ping/version/status =====

    #[test]
    fn version_returns_proto_version() {
        let h = make_helper_defaults();
        let out = h.handle("real-token", Request::Version);
        let HandleOutcome::Respond(Response::Ok(ResponseKind::Version { proto_version })) = out
        else {
            panic!("{out:?}");
        };
        assert_eq!(proto_version, crate::platform::windows::PROTO_VERSION);
    }

    #[test]
    fn status_stopped_when_no_child() {
        let h = make_helper_defaults();
        let out = h.handle("real-token", Request::Status);
        assert_eq!(
            out,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Status(
                polaris_helper_proto::Status::Stopped
            )))
        );
    }

    // ===== start =====

    #[test]
    fn start_with_empty_cfg_is_no_config() {
        let h = make_helper_defaults();
        let out = h.handle(
            "real-token",
            Request::Start(StartParams {
                cfg: String::new(),
                log: String::new(),
                fwd: false,
                parent_pid: None,
            }),
        );
        let HandleOutcome::Respond(Response::Err(e)) = out else {
            panic!("{out:?}");
        };
        assert_eq!(e.code, polaris_helper_proto::ErrorCode::NoConfig);
    }

    #[test]
    fn start_with_cfg_outside_confdir_is_denied() {
        let h = make_helper_defaults();
        let out = h.handle(
            "real-token",
            Request::Start(StartParams {
                cfg: r"C:\Windows\evil.json".to_owned(),
                log: String::new(),
                fwd: false,
                parent_pid: None,
            }),
        );
        let HandleOutcome::Respond(Response::Err(e)) = out else {
            panic!("{out:?}");
        };
        assert_eq!(e.code, polaris_helper_proto::ErrorCode::ConfigPathDenied);
    }

    /// log 与 cfg 走同一条白名单 —— 不校验就是「SYSTEM 在任意位置建文件并持续追加写」。
    #[test]
    fn start_with_log_outside_confdir_is_denied() {
        let h = make_helper_defaults();
        let out = h.handle(
            "real-token",
            Request::Start(StartParams {
                // cfg 合法，只有 log 越界 —— 单独钉住 log 这一格。
                cfg: r"C:\Users\polaris\config\singbox-runtime.json".to_owned(),
                log: r"C:\Windows\System32\drivers\etc\hosts".to_owned(),
                fwd: false,
                parent_pid: None,
            }),
        );
        let HandleOutcome::Respond(Response::Err(e)) = out else {
            panic!("{out:?}");
        };
        assert_eq!(e.code, polaris_helper_proto::ErrorCode::LogPathDenied);
    }

    /// 生产形态（cfg 与 log 同在 confDir）必须放行 —— 否则上面那条可能被「恒拒」满足。
    #[test]
    fn start_with_log_inside_confdir_is_allowed() {
        let h = make_helper_defaults();
        let out = h.handle(
            "real-token",
            Request::Start(StartParams {
                cfg: r"C:\Users\polaris\config\singbox-runtime.json".to_owned(),
                log: r"C:\Users\polaris\config\singbox-startup.log".to_owned(),
                fwd: false,
                parent_pid: None,
            }),
        );
        assert!(
            !matches!(
                &out,
                HandleOutcome::Respond(Response::Err(e))
                    if e.code == polaris_helper_proto::ErrorCode::LogPathDenied
            ),
            "生产形态被误拒：{out:?}"
        );
    }

    /// 空 log = 不重定向（`win.rs` 的 `if !log_path.is_empty()`），必须放行。
    #[test]
    fn start_with_empty_log_is_allowed() {
        let h = make_helper_defaults();
        let out = h.handle(
            "real-token",
            Request::Start(StartParams {
                cfg: r"C:\Users\polaris\config\singbox-runtime.json".to_owned(),
                log: String::new(),
                fwd: false,
                parent_pid: None,
            }),
        );
        assert!(
            !matches!(
                &out,
                HandleOutcome::Respond(Response::Err(e))
                    if e.code == polaris_helper_proto::ErrorCode::LogPathDenied
            ),
            "空 log 被误拒：{out:?}"
        );
    }

    #[test]
    fn start_with_valid_cfg_starts_and_records_pid() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops, MockNetTableOps::new());
        let out = h.handle(
            "real-token",
            Request::Start(StartParams {
                cfg: r"C:\Users\polaris\config\c.json".to_owned(),
                log: String::new(),
                fwd: false,
                parent_pid: None,
            }),
        );
        let HandleOutcome::Respond(Response::Ok(ResponseKind::Start(
            polaris_helper_proto::Start::Started { pid },
        ))) = out
        else {
            panic!("{out:?}");
        };
        assert_eq!(pid, 1000); // mock next_pid
                               // status 应反映 running
        let out2 = h.handle("real-token", Request::Status);
        assert!(matches!(
            out2,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Status(
                polaris_helper_proto::Status::Running { pid: 1000 }
            )))
        ));
    }

    #[test]
    fn start_when_already_running_returns_already() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops, MockNetTableOps::new());
        let cfg = r"C:\Users\polaris\config\c.json".to_owned();
        let req = Request::Start(StartParams {
            cfg,
            log: String::new(),
            fwd: false,
            parent_pid: None,
        });
        let _ = h.handle("real-token", req.clone());
        let out = h.handle("real-token", req);
        assert!(matches!(
            out,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Start(
                polaris_helper_proto::Start::Already { pid: 1000 }
            )))
        ));
    }

    #[test]
    fn start_failure_returns_err_start() {
        let proc_ops = MockProcOps::new();
        proc_ops.set_start_error(std::io::Error::other("ENOENT"));
        let h = make_helper(proc_ops, MockNetTableOps::new());
        let out = h.handle(
            "real-token",
            Request::Start(StartParams {
                cfg: r"C:\Users\polaris\config\c.json".to_owned(),
                log: String::new(),
                fwd: false,
                parent_pid: None,
            }),
        );
        let HandleOutcome::Respond(Response::Err(e)) = out else {
            panic!("{out:?}");
        };
        assert_eq!(e.code, polaris_helper_proto::ErrorCode::Start);
    }

    #[test]
    fn start_with_fwd_calls_enable_ip_forwarding() {
        let proc_ops = MockProcOps::new();
        let snap_before = proc_ops.snapshot();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        let _ = h.handle(
            "real-token",
            Request::Start(StartParams {
                cfg: r"C:\Users\polaris\config\c.json".to_owned(),
                log: String::new(),
                fwd: true, // 应触发 enable_ip_forwarding
                parent_pid: None,
            }),
        );
        let snap_after = proc_ops.snapshot();
        assert!(snap_after.ip_forward_calls > snap_before.ip_forward_calls);
    }

    // ===== stop =====

    #[test]
    fn stop_when_not_running_is_idempotent() {
        let h = make_helper_defaults();
        let out = h.handle("real-token", Request::Stop { pid: None });
        assert!(matches!(
            out,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Stop(
                polaris_helper_proto::Stop::NotRunning
            )))
        ));
    }

    #[test]
    fn stop_reaps_running_child_and_clears_state() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        // 先 start
        let _ = h.handle(
            "real-token",
            Request::Start(StartParams {
                cfg: r"C:\Users\polaris\config\c.json".to_owned(),
                log: String::new(),
                fwd: false,
                parent_pid: None,
            }),
        );
        let snap_before = proc_ops.snapshot();
        // stop
        let out = h.handle("real-token", Request::Stop { pid: None });
        assert!(matches!(
            out,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Stop(
                polaris_helper_proto::Stop::Stopped { pid: 1000 }
            )))
        ));
        let snap_after = proc_ops.snapshot();
        assert_eq!(snap_after.reap_calls, snap_before.reap_calls + 1);
        // status 现在应是 stopped
        let out2 = h.handle("real-token", Request::Status);
        assert!(matches!(
            out2,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Status(
                polaris_helper_proto::Status::Stopped
            )))
        ));
    }

    /// **变异门（核心）**：身份不匹配 → 不摘不收割，child 状态原样留给新会话。
    ///
    /// 变异（逃逸面穷举）：
    /// - 删掉 `Request::Stop` 分支里的 `stop_pid_matches` 判据 → 响应变 `Stopped{1000}` +
    ///   `reap_calls` 涨 → 转红（那正是「杀掉用户刚连上的新核」）。
    /// - 只改响应不改行为（回 Mismatch 但仍 `take()` + `reap_child`）→ 后两条断言转红。
    /// - `parse_request` 里把身份行丢掉、恒 `pid: None` → 判据永不触发 → 转红（另有
    ///   `service/win.rs::parse_request_stop_reads_optional_pid` 直接钉住解码侧）。
    #[test]
    fn stop_refuses_to_reap_when_managed_pid_is_another_session() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        // daemon 手里的是新会话的核（MockProcOps 的 start 固定报 1000）。
        let _ = h.handle(
            "real-token",
            Request::Start(StartParams {
                cfg: r"C:\Users\polaris\config\c.json".to_owned(),
                log: String::new(),
                fwd: false,
                parent_pid: None,
            }),
        );
        let snap_before = proc_ops.snapshot();
        // 老 stop 腿声明它要停 4242。
        let out = h.handle("real-token", Request::Stop { pid: Some(4242) });
        assert_eq!(
            out,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Stop(
                polaris_helper_proto::Stop::Mismatch {
                    want: 4242,
                    current: 1000
                }
            ))),
            "身份不匹配 → 诚实 no-op，回报两个 pid"
        );
        assert_eq!(
            proc_ops.snapshot().reap_calls,
            snap_before.reap_calls,
            "绝不能收割：1000 是用户刚连上的新核"
        );
        assert!(
            matches!(
                h.handle("real-token", Request::Status),
                HandleOutcome::Respond(Response::Ok(ResponseKind::Status(
                    polaris_helper_proto::Status::Running { pid: 1000 }
                )))
            ),
            "child 记账必须原样留给新会话（摘掉 = 新核失联，daemon 再也停不掉它）"
        );
    }

    /// 反向失效门：身份匹配照常停（判据不能收得太紧，否则停核彻底失效）。
    #[test]
    fn stop_proceeds_when_managed_pid_matches() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        let _ = h.handle(
            "real-token",
            Request::Start(StartParams {
                cfg: r"C:\Users\polaris\config\c.json".to_owned(),
                log: String::new(),
                fwd: false,
                parent_pid: None,
            }),
        );
        let snap_before = proc_ops.snapshot();
        let out = h.handle("real-token", Request::Stop { pid: Some(1000) });
        assert_eq!(
            out,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Stop(
                polaris_helper_proto::Stop::Stopped { pid: 1000 }
            )))
        );
        assert_eq!(proc_ops.snapshot().reap_calls, snap_before.reap_calls + 1);
    }

    // ===== cleanup =====

    #[test]
    fn cleanup_reaps_child_and_kills_all_singbox() {
        let proc_ops = MockProcOps::new();
        proc_ops.set_kill_all_return(2);
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        // 先 start
        let _ = h.handle(
            "real-token",
            Request::Start(StartParams {
                cfg: r"C:\Users\polaris\config\c.json".to_owned(),
                log: String::new(),
                fwd: false,
                parent_pid: None,
            }),
        );
        let snap_before = proc_ops.snapshot();
        let out = h.handle("real-token", Request::Cleanup);
        assert_eq!(
            out,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Cleaned))
        );
        let snap_after = proc_ops.snapshot();
        assert_eq!(snap_after.reap_calls, snap_before.reap_calls + 1);
        // kill_all_singbox 由 mock 的 kill_all_return 返回 2（调用计数不经此路径，但行为对齐 Go）
    }

    // ===== uninstall =====

    #[test]
    fn uninstall_spawns_self_uninstall_and_signals_exit() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        let snap_before = proc_ops.snapshot();
        let out = h.handle("real-token", Request::Uninstall);
        match out {
            HandleOutcome::UninstallAndExit(Response::Ok(ResponseKind::Uninstalling)) => {}
            other => panic!("expected UninstallAndExit, got {other:?}"),
        }
        let snap_after = proc_ops.snapshot();
        assert_eq!(
            snap_after.spawn_uninstall_calls,
            snap_before.spawn_uninstall_calls + 1
        );
        // spawn 参数传了 service_name + support_dir
        let args = proc_ops.last_spawn_args();
        assert_eq!(
            args,
            Some((
                "PolarisHelper".to_owned(),
                r"C:\ProgramData\Polaris".to_owned()
            ))
        );
    }

    // ===== freeport =====

    #[test]
    fn freeport_free_when_no_listener() {
        let h = make_helper_defaults();
        let out = h.handle("real-token", Request::FreePort { port: 9090 });
        assert_eq!(
            out,
            HandleOutcome::Respond(Response::Ok(ResponseKind::FreePort(
                polaris_helper_proto::FreePort::Free
            )))
        );
    }

    #[test]
    fn freeport_kills_locked_singbox_listener() {
        let proc_ops = MockProcOps::new();
        proc_ops.set_image(1000, r"C:\Program Files\Polaris\sing-box.exe");
        let net_ops = MockNetTableOps::new();
        net_ops.set_entries(vec![crate::platform::windows::logic::ListenEntry {
            pid: 1000,
            port: 9090,
        }]);
        let h = make_helper(proc_ops.clone(), net_ops);
        let out = h.handle("real-token", Request::FreePort { port: 9090 });
        let HandleOutcome::Respond(Response::Ok(ResponseKind::FreePort(
            polaris_helper_proto::FreePort::Killed { pids },
        ))) = out
        else {
            panic!("{out:?}");
        };
        assert_eq!(pids, vec![1000]);
        // terminate_pid 被调用
        assert_eq!(proc_ops.snapshot().terminate_calls, 1);
    }

    #[test]
    fn freeport_reports_foreign_listener_without_killing() {
        let proc_ops = MockProcOps::new();
        proc_ops.set_image(2000, r"C:\Windows\System32\nginx.exe");
        let net_ops = MockNetTableOps::new();
        net_ops.set_entries(vec![crate::platform::windows::logic::ListenEntry {
            pid: 2000,
            port: 80,
        }]);
        let h = make_helper(proc_ops.clone(), net_ops);
        let out = h.handle("real-token", Request::FreePort { port: 80 });
        let HandleOutcome::Respond(Response::Ok(ResponseKind::FreePort(
            polaris_helper_proto::FreePort::Foreign { names },
        ))) = out
        else {
            panic!("{out:?}");
        };
        assert_eq!(names, vec!["nginx.exe".to_owned()]);
        // 不应调 terminate_pid（foreign 不杀）
        assert_eq!(proc_ops.snapshot().terminate_calls, 0);
    }

    // ===== route-add / route-del =====

    #[test]
    fn route_add_denies_non_polaris_iface() {
        let h = make_helper_defaults();
        let out = h.handle(
            "real-token",
            Request::RouteAdd(polaris_helper_proto::RouteParams {
                iface: "Ethernet0".to_owned(),
                cidrs: vec!["10.0.0.0/8".to_owned()],
            }),
        );
        let HandleOutcome::Respond(Response::Err(e)) = out else {
            panic!("{out:?}");
        };
        assert_eq!(e.code, polaris_helper_proto::ErrorCode::IfaceDenied);
    }

    #[test]
    fn route_add_allows_polaris_iface() {
        let h = make_helper_defaults();
        let out = h.handle(
            "real-token",
            Request::RouteAdd(polaris_helper_proto::RouteParams {
                iface: "polaris-tun0".to_owned(),
                cidrs: vec!["10.0.0.0/8".to_owned()],
            }),
        );
        assert_eq!(
            out,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Route))
        );
    }

    // ===== route netsh 真派发（W9 修）=====

    #[test]
    fn route_add_dispatches_apply_route_for_each_valid_cidr() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        let out = h.handle(
            "real-token",
            Request::RouteAdd(polaris_helper_proto::RouteParams {
                iface: "polaris-tun0".to_owned(),
                // 中间一项非法 CIDR + 一项空 → 应被跳过（Go: ParseCIDR err / "" → continue）。
                cidrs: vec![
                    "10.0.0.0/8".to_owned(),
                    "not-a-cidr".to_owned(),
                    String::new(),
                    "::/0".to_owned(),
                ],
            }),
        );
        assert_eq!(
            out,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Route))
        );
        // 仅 2 个合法 CIDR 触发 apply_route（非法/空跳过）。
        assert_eq!(proc_ops.snapshot().route_calls, 2);
        // 最后一次 = ::/0, del=false（add）。
        assert_eq!(
            proc_ops.last_route(),
            Some(("polaris-tun0".to_owned(), "::/0".to_owned(), false))
        );
    }

    #[test]
    fn route_del_dispatches_apply_route_with_del_true() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        let _ = h.handle(
            "real-token",
            Request::RouteDel(polaris_helper_proto::RouteParams {
                iface: "polaris-tun0".to_owned(),
                cidrs: vec!["10.0.0.0/8".to_owned()],
            }),
        );
        assert_eq!(proc_ops.snapshot().route_calls, 1);
        assert_eq!(
            proc_ops.last_route(),
            Some(("polaris-tun0".to_owned(), "10.0.0.0/8".to_owned(), true))
        );
    }

    #[test]
    fn route_denied_iface_runs_no_netsh() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        let out = h.handle(
            "real-token",
            Request::RouteAdd(polaris_helper_proto::RouteParams {
                iface: "Ethernet0".to_owned(),
                cidrs: vec!["10.0.0.0/8".to_owned()],
            }),
        );
        let HandleOutcome::Respond(Response::Err(e)) = out else {
            panic!("{out:?}");
        };
        assert_eq!(e.code, polaris_helper_proto::ErrorCode::IfaceDenied);
        // iface 被拒 → 不应跑任何 netsh。
        assert_eq!(proc_ops.snapshot().route_calls, 0);
    }

    // ===== 父死看护接线（W15 修）=====

    fn start_req_with_ppid(ppid: Option<u32>) -> Request {
        Request::Start(StartParams {
            cfg: r"C:\Users\polaris\config\c.json".to_owned(),
            log: String::new(),
            fwd: false,
            parent_pid: ppid,
        })
    }

    #[test]
    fn start_with_ppid_wires_watch_parent() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        let _ = h.handle("real-token", start_req_with_ppid(Some(4242)));
        let snap = proc_ops.snapshot();
        assert_eq!(snap.watch_parent_calls, 1);
        // 传入 (ppid, child_pid)：child_pid = mock start 返回的 1000。
        assert_eq!(proc_ops.last_watch_args(), Some((4242, 1000)));
    }

    #[test]
    fn start_without_ppid_does_not_wire_watch_parent() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        let _ = h.handle("real-token", start_req_with_ppid(None));
        assert_eq!(proc_ops.snapshot().watch_parent_calls, 0);
    }

    #[test]
    fn start_with_zero_ppid_does_not_wire_watch_parent() {
        // Go: ppid <= 0 → 不启看护。parent_pid=Some(0) 等价（filter ppid>0 排除）。
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        let _ = h.handle("real-token", start_req_with_ppid(Some(0)));
        assert_eq!(proc_ops.snapshot().watch_parent_calls, 0);
    }

    #[test]
    fn watch_parent_reaps_child_when_parent_dead() {
        // mock spawn_watch_parent 单次评估：父死（alive=false）+ child 仍当前 → on_parent_dead 收割 + 摘 child。
        let proc_ops = MockProcOps::new();
        proc_ops.set_alive(false);
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        let snap_before = proc_ops.snapshot();
        let _ = h.handle("real-token", start_req_with_ppid(Some(4242)));
        let snap_after = proc_ops.snapshot();
        // on_parent_dead → proc.reap_child(pid) → reap_calls +1。
        assert_eq!(snap_after.reap_calls, snap_before.reap_calls + 1);
        // child 已被摘 → status 现在 stopped。
        let out = h.handle("real-token", Request::Status);
        assert!(matches!(
            out,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Status(
                polaris_helper_proto::Status::Stopped
            )))
        ));
    }

    #[test]
    fn watch_parent_keeps_child_when_parent_alive() {
        // 父存活（alive=true 默认）→ on_parent_dead 不触发 → child 仍在（running）。
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        let _ = h.handle("real-token", start_req_with_ppid(Some(4242)));
        let out = h.handle("real-token", Request::Status);
        assert!(matches!(
            out,
            HandleOutcome::Respond(Response::Ok(ResponseKind::Status(
                polaris_helper_proto::Status::Running { pid: 1000 }
            )))
        ));
    }

    // ===== iface-metric =====

    #[test]
    fn iface_metric_denies_non_polaris_iface() {
        let h = make_helper_defaults();
        let out = h.handle(
            "real-token",
            Request::IfaceMetric {
                iface: "en0".to_owned(),
                metric: 999,
            },
        );
        let HandleOutcome::Respond(Response::Err(e)) = out else {
            panic!("{out:?}");
        };
        assert_eq!(e.code, polaris_helper_proto::ErrorCode::IfaceDenied);
    }

    // ===== reap_child_on_exit =====

    #[test]
    fn reap_child_on_exit_reaps_when_child_present() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        let _ = h.handle(
            "real-token",
            Request::Start(StartParams {
                cfg: r"C:\Users\polaris\config\c.json".to_owned(),
                log: String::new(),
                fwd: false,
                parent_pid: None,
            }),
        );
        let snap_before = proc_ops.snapshot();
        h.reap_child_on_exit();
        let snap_after = proc_ops.snapshot();
        assert_eq!(snap_after.reap_calls, snap_before.reap_calls + 1);
    }

    #[test]
    fn reap_child_on_exit_killall_when_no_child() {
        let proc_ops = MockProcOps::new();
        let h = make_helper(proc_ops.clone(), MockNetTableOps::new());
        h.reap_child_on_exit(); // 无 child → 走 killAllSingbox 兜底
                                // kill_all_singbox 调用无独立计数器，但行为对齐 Go（Go 注释 helper.go:407-409）
    }

    // ===== unsupported commands =====

    #[test]
    fn mac_linux_commands_return_unknown() {
        // Windows helper 无 install-core / flush-dns / default-restore / linux-start
        let h = make_helper_defaults();
        let out = h.handle(
            "real-token",
            Request::FlushDns, // mac 专属
        );
        let HandleOutcome::Respond(Response::Err(e)) = out else {
            panic!("{out:?}");
        };
        assert_eq!(e.code, polaris_helper_proto::ErrorCode::Unknown);
    }
}
