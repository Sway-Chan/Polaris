//! 协议分派（移植自 上游 `helper/helper.go:397-589` 的 `handle()`）。
//!
//! ## Go 源结构（`helper.go:397-419`）
//!
//! ```text
//! func handle(conn net.Conn) {
//!     defer conn.Close()
//!     conn.SetReadDeadline(5s)
//!     r := bufio.NewReader(conn)
//!     tok := readLine(r)          // 行1: token
//!     cmd := readLine(r)          // 行2: command
//!     if tok == "" || tok != tokenValue() {
//!         fmt.Fprintln(conn, "ERR auth"); return
//!     }
//!     if cmd == "freeport" { handleFreeport(...); return }   // 不持锁
//!     mu.Lock(); defer mu.Unlock()
//!     switch cmd { case "ping": ... case "start": ... }
//! }
//! ```
//!
//! ## 移植纪律
//!
//! 把「鉴权 + 命令分派」从 socket IO 中剥离为纯函数 [`dispatch`] —— 接收已读的 token/command/args 行
//! + 一个 [`MacServices`] 服务 bundle，返回 [`Response`]。这样：
//! - 协议分派逻辑（参数校验、白名单判定、错误码映射）跨平台可测（Linux CI 完整覆盖）。
//! - socket IO（accept/read/write/超时）留给 [`crate::platform::macos::server`] 的 mac-gated 部分。
//!
//! freeport 不持锁（Go `helper.go:413`）—— 在 dispatch 内直接处理，由调用方决定是否在锁外调用。
//! 本 dispatch 是无状态的（状态在 [`MacServices`] 内），不模拟 Go 的 mu —— 并发控制由 server 层负责。

use crate::core_install::{install_core_files, InstallResult, SINGBOX_BIN_NAME};
use crate::platform::macos::exec::{CommandRunner, CODESIGN_TIMEOUT, EXEC_TIMEOUT};
use crate::platform::macos::flush_dns;
use crate::platform::macos::freeport;
use crate::platform::macos::install_core::to_response;
use crate::platform::macos::route::{self, RouteOp};
use crate::platform::macos::whitelist;
use crate::token::{check_token, TokenCheck, TokenStore};
use polaris_helper_proto::request::{InstallCoreParams, RouteParams};
use polaris_helper_proto::response::{ResponseKind, Start as StartResp, Status, Stop};
use polaris_helper_proto::{stop_pid_matches, Error as ProtoError, ErrorCode, Request, Response};
use std::path::Path;
use std::sync::Mutex;

/// macOS helper 配置（对应 Go 的 `--singbox`/`--confdir`/`--support`/`--coredir` flag）。
///
/// 这些值在安装期锁定（改需 root），是安全边界的一部分：
/// - `singbox_bin`：sing-box 二进制路径锁定（杜绝「持 token 跑任意二进制」，`helper.go:6`）。
/// - `conf_dir`：允许的配置文件目录（`cfgAllowed` 白名单，`helper.go:245-252`）。
/// - `support_dir`：socket + token 所在目录。
/// - `core_dir`：install-core 只写此目录（防写任意路径，`helper.go:128`）。
#[derive(Debug, Clone)]
pub struct MacConfig {
    /// 锁定的 sing-box 二进制路径（`helper.go:592` flag `--singbox`）。
    pub singbox_bin: String,
    /// 允许的配置文件目录（`helper.go:593` flag `--confdir`）。
    pub conf_dir: String,
    /// socket + token 所在目录（`helper.go:594` flag `--support`，默认 `/Library/Application Support/Polaris`）。
    pub support_dir: String,
    /// install-core 的受保护内核目录（`helper.go:595` flag `--coredir`）。
    pub core_dir: String,
}

impl MacConfig {
    /// 构造默认配置（对应 Go main 的 flag 默认值）。
    #[must_use]
    pub fn new(
        singbox_bin: impl Into<String>,
        conf_dir: impl Into<String>,
        support_dir: impl Into<String>,
        core_dir: impl Into<String>,
    ) -> Self {
        Self {
            singbox_bin: singbox_bin.into(),
            conf_dir: conf_dir.into(),
            support_dir: support_dir.into(),
            core_dir: core_dir.into(),
        }
    }
}

/// child sing-box 进程的运行时状态（对应 Go 的 `child`/`childDone` 全局变量 + mu）。
///
/// 用 `Arc<Mutex<Option<ChildHandle>>>` 持有 —— 对齐 Go 的 `mu.Lock()` 互斥语义。
/// 实际 child 进程的 spawn/wait/terminate 由 [`crate::platform::macos::server`] 的 mac-gated 部分实现
/// （需 tokio/std::process + 信号处理）；本结构只持有状态快照（pid）。
#[derive(Debug, Clone, Default)]
pub struct ChildHandle {
    /// child sing-box 的 pid。
    pub pid: u32,
}

/// macOS helper 的服务 bundle —— 把所有外部依赖（token 存储、命令执行、child 状态）打包成一个 trait，
/// 便于测试注入 mock、生产注入真实实现。
pub trait MacServices: Send + Sync {
    /// token 存储（读 `helper.token`）。
    fn token_store(&self) -> &dyn TokenStore;

    /// 命令执行器（route/lsof/ps/dscacheutil/codesign 等）。
    fn runner(&self) -> &dyn CommandRunner;

    /// child 进程状态（对应 Go 的 `child` 全局变量）。
    fn child(&self) -> &Mutex<Option<ChildHandle>>;

    /// 当前 helper 进程的 uid（mac `os.Getuid()`；测试可固定 0）。
    fn uid(&self) -> i64 {
        #[cfg(target_os = "macos")]
        {
            // os.Getuid() 等价 —— nix 0.31 的 getuid() 在 `user` feature 后（Cargo.toml 已开）。
            // Uid::current() 是 getuid() 的官方 Rusty 别名（nix unistd.rs:64-68，doc alias "getuid"）。
            nix::unistd::Uid::current().as_raw() as i64
        }
        #[cfg(not(target_os = "macos"))]
        {
            0
        }
    }

    /// spawn sing-box child（mac-gated，由 server 层实现，`helper.go:538-578`）。
    ///
    /// 返回新 child 的 pid。`parent_pid`=父 app PID（`Some`→起 watchParent 父死看护，`helper.go:576-578`；
    /// `None`→不看护，兼容旧客户端）。默认实现返回未实现错误（测试可 mock）。
    fn spawn_child(
        &self,
        _cfg: &str,
        _log: &str,
        _fwd: bool,
        _parent_pid: Option<u32>,
    ) -> Result<u32, SpawnError> {
        Err(SpawnError::NotImplemented)
    }

    /// 终止 child（TERM→等→KILL，mac-gated，由 server 层实现）。
    ///
    /// `want_pid` = 客户端声明它意图停的那个受管 pid（`None` = 旧语义「停当前受管核」）。判据走
    /// [`stop_pid_matches`]：不匹配 ⇒ 手里这个核属另一个会话 ⇒ **不摘不杀**，回
    /// [`TerminateOutcome::Mismatch`]（诚实 no-op）。判定与摘除在**同一把 child 锁**下完成，
    /// 不给「判完才被换掉」留缝。
    fn terminate_child(&self, want_pid: Option<u32>) -> TerminateOutcome {
        terminate_managed_child(self.child(), want_pid)
    }

    /// 摘除 child 状态（cleanup 用，不发信号；对应 Go `helper.go:448` `child, childDone = nil, nil`）。
    ///
    /// 默认只清 [`child`](Self::child) 视图；生产实现（server 层）覆盖以同步清内部 `done`/`generation`
    /// 记账（否则收割身份代守卫会残留陈旧 done）。
    fn clear_child(&self) {
        *self.child().lock().unwrap() = None;
    }
}

/// [`MacServices::terminate_child`] 的判定 + 摘除本体（**单一判定点**）。
///
/// 抽成自由函数而非留在 trait 默认体里，是为了让测试替身能**复用真判据**而不是各抄一份
/// （替身抄一份 = 测试测的是替身，身份判据被删掉也照样绿）。
///
/// 判定与摘除在同一把 child 锁下完成：不匹配 ⇒ 不摘、不杀，回 [`TerminateOutcome::Mismatch`]。
pub fn terminate_managed_child(
    child: &Mutex<Option<ChildHandle>>,
    want_pid: Option<u32>,
) -> TerminateOutcome {
    let mut guard = child.lock().unwrap();
    match guard.as_ref() {
        Some(c) if !stop_pid_matches(want_pid, c.pid) => TerminateOutcome::Mismatch {
            want: want_pid.unwrap_or(0),
            current: c.pid,
        },
        Some(_) => guard.take().map_or(TerminateOutcome::NotRunning, |c| {
            TerminateOutcome::Stopped { pid: c.pid }
        }),
        None => TerminateOutcome::NotRunning,
    }
}

/// spawn child 的错误。
#[derive(Debug, Clone)]
pub enum SpawnError {
    /// 非 mac 环境 / 未注入实现。
    NotImplemented,
    /// 启动失败（对齐 Go `helper.go:552` 的 `ERR start <err>`）。
    Failed(String),
}

/// terminate child 的结果（对应 Go `helper.go:440-442`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminateOutcome {
    /// `OK stopped <pid>`。
    Stopped { pid: u32 },
    /// `OK notrunning`。
    NotRunning,
    /// `OK stop-mismatch <want> <current>` —— 受管 pid 非请求所指 ⇒ 不动手（见
    /// [`stop_pid_matches`]）。
    Mismatch { want: u32, current: u32 },
}

/// 协议分派结果（一个请求一个响应）。
///
/// 这是纯逻辑：输入 token + [`Request`] + 服务 bundle，输出 [`Response`]。
/// socket IO 层负责读 token 行 → 构造 Request → 调本函数 → 写回 Response。
pub fn dispatch(
    services: &dyn MacServices,
    config: &MacConfig,
    token: &str,
    req: &Request,
) -> Response {
    // helper.go:405-408: token 鉴权（首道边界）
    let stored = services.token_store().token_value();
    if !matches!(check_token(token, &stored), TokenCheck::Authed) {
        return Response::Err(ProtoError::new(ErrorCode::Auth));
    }

    // freeport 不持锁、不碰 child（helper.go:413）—— 直接处理
    if let Request::FreePort { port } = req {
        let port_str = port.to_string();
        return freeport::run_freeport(services.runner(), &port_str);
    }

    // 其余命令在「临界区」内处理 —— 这里用 child() mutex 体现互斥语义
    // （实际生产由 server 层在调 dispatch 前后包 mu；本函数内部对 child 状态的访问仍走 mutex）
    match req {
        Request::Ping => {
            // helper.go:422-423: OK pong uid=<n> v<ver>
            Response::Ok(ResponseKind::Pong(polaris_helper_proto::Pong::current(
                services.uid(),
            )))
        }
        Request::Version => {
            // helper.go:424-425: OK <ver>
            Response::Ok(ResponseKind::Version {
                proto_version: crate::platform::macos::PROTO_VERSION,
            })
        }
        Request::Status => {
            // helper.go:426-430
            let guard = services.child().lock().unwrap();
            match &*guard {
                Some(child) => {
                    Response::Ok(ResponseKind::Status(Status::Running { pid: child.pid }))
                }
                None => Response::Ok(ResponseKind::Status(Status::Stopped)),
            }
        }
        Request::Stop { pid } => {
            // helper.go:432-442（+ 受管 pid 身份判据，见 `terminate_child`）
            match services.terminate_child(*pid) {
                TerminateOutcome::Stopped { pid } => {
                    Response::Ok(ResponseKind::Stop(Stop::Stopped { pid }))
                }
                TerminateOutcome::NotRunning => Response::Ok(ResponseKind::Stop(Stop::NotRunning)),
                TerminateOutcome::Mismatch { want, current } => {
                    Response::Ok(ResponseKind::Stop(Stop::Mismatch { want, current }))
                }
            }
        }
        Request::Cleanup => {
            // helper.go:444-449: pkill -9 -f "<singboxBin> run" + 摘 child
            let pattern = format!("{} run", config.singbox_bin);
            let _ = services
                .runner()
                .run(EXEC_TIMEOUT, "/usr/bin/pkill", &["-9", "-f", &pattern]);
            // helper.go:448: child, childDone = nil, nil（生产实现同步清 done/generation）
            services.clear_child();
            Response::Ok(ResponseKind::Cleaned)
        }
        Request::RouteAdd(rp) | Request::RouteDel(rp) => {
            // helper.go:450-480
            handle_route(services.runner(), rp, matches!(req, Request::RouteAdd(_)))
        }
        Request::DefaultRestore { gateway_ipv4 } => {
            // helper.go:481-491
            handle_default_restore(services.runner(), gateway_ipv4)
        }
        Request::FlushDns => {
            // helper.go:492-506
            flush_dns::flush_dns(services.runner()).into()
        }
        Request::Start(params) => {
            // helper.go:507-579
            handle_start(
                services,
                config,
                params.cfg.as_str(),
                params.log.as_str(),
                params.fwd,
                params.parent_pid,
            )
        }
        Request::InstallCore(params) => {
            // helper.go:580-585
            handle_install_core(services.runner(), config, params)
        }
        // 以下命令不属于 mac helper 谱系（LinuxStart/IfaceMetric/Uninstall）
        Request::LinuxStart(_) | Request::IfaceMetric { .. } | Request::Uninstall => {
            Response::Err(ProtoError::new(ErrorCode::Unknown))
        }
        // FreePort 已在上方早返回；此处不可达（穷尽性兜底）
        Request::FreePort { .. } => unreachable!("freeport handled above"),
    }
}

/// route-add / route-del 处理（移植自 `helper.go:450-480`）。
fn handle_route(runner: &dyn CommandRunner, rp: &RouteParams, is_add: bool) -> Response {
    // helper.go:457-460: iface 白名单
    if !whitelist::iface_allowed(&rp.iface) {
        return Response::Err(ProtoError::new(ErrorCode::IfaceDenied));
    }
    let op = if is_add {
        RouteOp::Add
    } else {
        RouteOp::Delete
    };
    // helper.go:465-479: 逐 CIDR 构造 argv + 执行
    for cidr in &rp.cidrs {
        let cidr = cidr.trim();
        if cidr.is_empty() {
            continue;
        }
        if let Some(argv) = route::build_route_argv(op, &rp.iface, cidr) {
            let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
            // helper.go:478: 幂等 best-effort，忽略错误
            let _ = runner.run(EXEC_TIMEOUT, route::ROUTE_BIN, &refs);
        }
        // 非法 CIDR 跳过（helper.go:470-471 continue）
    }
    Response::Ok(ResponseKind::Route)
}

/// default-restore 处理（移植自 `helper.go:481-491`）。
fn handle_default_restore(runner: &dyn CommandRunner, gateway_ipv4: &str) -> Response {
    let gw = gateway_ipv4.trim();
    match route::build_default_restore_argv(gw) {
        Some(argv) => {
            let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
            let _ = runner.run(EXEC_TIMEOUT, route::ROUTE_BIN, &refs);
            Response::Ok(ResponseKind::DefaultRestored)
        }
        None => Response::Err(ProtoError::new(ErrorCode::BadGateway)),
    }
}

/// start 处理（移植自 `helper.go:507-579`）。
fn handle_start(
    services: &dyn MacServices,
    config: &MacConfig,
    cfg: &str,
    log: &str,
    fwd: bool,
    parent_pid: Option<u32>,
) -> Response {
    // helper.go:521-524: 已有 child → OK already <pid>
    {
        let guard = services.child().lock().unwrap();
        if let Some(child) = &*guard {
            return Response::Ok(ResponseKind::Start(StartResp::Already { pid: child.pid }));
        }
    }
    // helper.go:525-527: cfg 空 → ERR no-config
    if cfg.is_empty() {
        return Response::Err(ProtoError::new(ErrorCode::NoConfig));
    }
    // helper.go:528-531: cfg 不在白名单 → ERR config-path-denied
    if !whitelist::cfg_allowed(cfg, &config.conf_dir) {
        return Response::Err(ProtoError::new(ErrorCode::ConfigPathDenied));
    }
    // **Polaris 新增（上游无）**：log 走与 cfg 同一条白名单。
    //
    // 上游只校验 cfg，而 `do_spawn` 会以 **root** 身份 `O_CREATE|O_APPEND` 打开这个 log 路径
    //（`server.rs` 的 `OpenOptions::new().create(true).append(true).open(log)`）⇒ 不校验就是
    // 「root 在任意位置建文件并持续追加写」。生产下发的恒是 `<conf_dir>/singbox-startup.log`
    //（`runtime/proxy.rs` 的 `self.config.join(SINGBOX_STARTUP_LOG)`），与 cfg 同目录
    // ⇒ 这条收紧**零行为变更**。空串 = 不重定向，放行。
    if !log.is_empty() && !whitelist::cfg_allowed(log, &config.conf_dir) {
        return Response::Err(ProtoError::new(ErrorCode::LogPathDenied));
    }
    // helper.go:533-537: allowLan 开启 IP 转发
    if fwd {
        let _ = services.runner().run(
            EXEC_TIMEOUT,
            "/usr/sbin/sysctl",
            &["-w", "net.inet.ip.forwarding=1"],
        );
        let _ = services.runner().run(
            EXEC_TIMEOUT,
            "/usr/sbin/sysctl",
            &["-w", "net.inet6.ip6.forwarding=1"],
        );
    }
    // helper.go:538-579: spawn child sing-box（ppid>0 → 起 watchParent 父死看护）
    match services.spawn_child(cfg, log, fwd, parent_pid) {
        Ok(pid) => Response::Ok(ResponseKind::Start(StartResp::Started { pid })),
        Err(SpawnError::NotImplemented) => Response::Err(ProtoError::with_detail(
            ErrorCode::Start,
            "spawn not implemented on this platform",
        )),
        Err(SpawnError::Failed(msg)) => {
            Response::Err(ProtoError::with_detail(ErrorCode::Start, msg))
        }
    }
}

/// install-core 处理（移植自 `helper.go:580-585,127-198`）。
///
/// 文件操作（校验+原子写入+清理）走 [`install_core_files`](crate::core_install::install_core_files)，
/// mac 专属的 xattr/codesign 在文件就位后执行（`helper.go:195-196`）。
fn handle_install_core(
    runner: &dyn CommandRunner,
    config: &MacConfig,
    params: &InstallCoreParams,
) -> Response {
    let core_dir = Path::new(&config.core_dir);
    let src_dir = Path::new(&params.src_dir);
    // helper.go:133-198: 文件操作
    match install_core_files(core_dir, src_dir, &params.want_hash) {
        Ok(_) => {
            // helper.go:194-196: mac 专属 —— 清 quarantine + adhoc 签名 sing-box
            let core_dir_str = config.core_dir.as_str();
            let (xattr_prog, xattr_args) =
                crate::platform::macos::btm::clear_quarantine_cmd(core_dir_str);
            let xattr_refs: Vec<&str> = xattr_args.iter().map(String::as_str).collect();
            let _ = runner.run(EXEC_TIMEOUT, xattr_prog, &xattr_refs);

            let sb_path = std::path::PathBuf::from(core_dir).join(SINGBOX_BIN_NAME);
            let sb_str = sb_path.to_string_lossy();
            let (cs_prog, cs_args) = crate::platform::macos::btm::adhoc_sign_cmd(&sb_str);
            let cs_refs: Vec<&str> = cs_args.iter().map(String::as_str).collect();
            let _ = runner.run(CODESIGN_TIMEOUT, cs_prog, &cs_refs);

            Response::Ok(ResponseKind::Installed)
        }
        Err(InstallResult::Installed) => Response::Ok(ResponseKind::Installed),
        Err(e) => to_response(e),
    }
}

// wire 写方向已上提 helper-proto：见 [`polaris_helper_proto::Response::to_wire_line`]
//（与 `Response::parse` 成对，三平台共用）。原 mac 私有副本（`response_to_wire_line`/`ok_kind_to_wire`/
// `freeport_wire`/`flush_dns_wire`，61 行）已删 —— 调用方直接 `resp.to_wire_line()`。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::macos::exec::RunError;
    use crate::token::FileTokenStore;
    use std::sync::Mutex;

    // 测试用 services bundle：注入 FileTokenStore（tempfile）+ 记录型 runner + 可控 child。
    struct TestServices {
        token_value: Mutex<String>,
        runner_calls: Mutex<Vec<(String, Vec<String>)>>,
        child: Mutex<Option<ChildHandle>>,
        uid: i64,
        spawn_result: Mutex<Option<Result<u32, SpawnError>>>,
        terminate_calls: Mutex<u32>,
    }

    impl TestServices {
        fn new(token: &str) -> Self {
            Self {
                token_value: Mutex::new(token.into()),
                runner_calls: Mutex::new(Vec::new()),
                child: Mutex::new(None),
                uid: 0,
                spawn_result: Mutex::new(Some(Ok(5555))),
                terminate_calls: Mutex::new(0),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.runner_calls.lock().unwrap().clone()
        }
    }

    impl TokenStore for TestServices {
        fn token_value(&self) -> String {
            self.token_value.lock().unwrap().clone()
        }
    }

    // 复用 exec::MockRunner 的记录逻辑，但需接入 TestServices。
    impl CommandRunner for TestServices {
        fn run(&self, _t: std::time::Duration, p: &str, a: &[&str]) -> Result<(), RunError> {
            self.runner_calls
                .lock()
                .unwrap()
                .push((p.into(), a.iter().map(|s| (*s).into()).collect()));
            Ok(())
        }
        fn output(
            &self,
            _t: std::time::Duration,
            p: &str,
            a: &[&str],
        ) -> Result<Vec<u8>, RunError> {
            self.runner_calls
                .lock()
                .unwrap()
                .push((p.into(), a.iter().map(|s| (*s).into()).collect()));
            Ok(Vec::new())
        }
        fn combined(
            &self,
            _t: std::time::Duration,
            p: &str,
            a: &[&str],
        ) -> Result<Vec<u8>, RunError> {
            self.runner_calls
                .lock()
                .unwrap()
                .push((p.into(), a.iter().map(|s| (*s).into()).collect()));
            Ok(Vec::new())
        }
    }

    impl MacServices for TestServices {
        fn token_store(&self) -> &dyn TokenStore {
            self
        }
        fn runner(&self) -> &dyn CommandRunner {
            self
        }
        fn child(&self) -> &Mutex<Option<ChildHandle>> {
            &self.child
        }
        fn uid(&self) -> i64 {
            self.uid
        }
        fn spawn_child(
            &self,
            _cfg: &str,
            _log: &str,
            _fwd: bool,
            _parent_pid: Option<u32>,
        ) -> Result<u32, SpawnError> {
            let r = self.spawn_result.lock().unwrap().clone();
            match r {
                Some(Ok(pid)) => {
                    *self.child.lock().unwrap() = Some(ChildHandle { pid });
                    Ok(pid)
                }
                Some(Err(e)) => Err(e),
                None => Err(SpawnError::NotImplemented),
            }
        }
        fn terminate_child(&self, want_pid: Option<u32>) -> TerminateOutcome {
            *self.terminate_calls.lock().unwrap() += 1;
            // 复用生产判定点（替身只记调用，不自抄一份判据 —— 否则删掉判据本测照样绿）。
            terminate_managed_child(&self.child, want_pid)
        }
    }

    fn test_config() -> MacConfig {
        MacConfig::new(
            "/usr/local/lib/polaris/sing-box",
            "/Users/test/Polaris",
            "/Library/Application Support/Polaris",
            "/Library/Application Support/Polaris/core",
        )
    }

    // ===== 鉴权 =====

    #[test]
    fn auth_denied_wrong_token() {
        // helper.go:405-407: tok != tokenValue → ERR auth
        let svc = TestServices::new("real-token");
        let cfg = test_config();
        let resp = dispatch(&svc, &cfg, "wrong-token", &Request::Ping);
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::Auth,
                ..
            })
        ));
    }

    #[test]
    fn auth_denied_empty_token() {
        let svc = TestServices::new("real-token");
        let cfg = test_config();
        let resp = dispatch(&svc, &cfg, "", &Request::Ping);
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::Auth,
                ..
            })
        ));
    }

    #[test]
    fn auth_ok_correct_token_reaches_command() {
        let svc = TestServices::new("real-token");
        let cfg = test_config();
        let resp = dispatch(&svc, &cfg, "real-token", &Request::Ping);
        match resp {
            Response::Ok(ResponseKind::Pong(p)) => {
                assert_eq!(p.proto_version, crate::platform::macos::PROTO_VERSION);
                assert_eq!(p.uid, 0);
            }
            other => panic!("{other:?}"),
        }
    }

    // ===== ping / version =====

    #[test]
    fn ping_response_format() {
        // wire 形态追加 shared build identity；旧 app 忽略，新 app 用它识别同 proto 旧 helper。
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(&svc, &cfg, "t", &Request::Ping);
        let line = resp.to_wire_line();
        assert_eq!(
            line,
            format!(
                "OK pong uid=0 v{} build={}",
                crate::platform::macos::PROTO_VERSION,
                polaris_helper_proto::build_identity::current()
            )
        );
    }

    #[test]
    fn version_response_format() {
        // wire 形态 `OK <ver>`；版本走统一的 PROTO_VERSION（不再是 上游 mac=9）。
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(&svc, &cfg, "t", &Request::Version);
        let line = resp.to_wire_line();
        assert_eq!(
            line,
            format!("OK {}", crate::platform::macos::PROTO_VERSION)
        );
    }

    // ===== status =====

    #[test]
    fn status_stopped_when_no_child() {
        // helper.go:429-430
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(&svc, &cfg, "t", &Request::Status);
        assert!(matches!(
            resp,
            Response::Ok(ResponseKind::Status(Status::Stopped))
        ));
        assert_eq!(resp.to_wire_line(), "OK stopped");
    }

    #[test]
    fn status_running_after_start() {
        // helper.go:427-428
        let svc = TestServices::new("t");
        let cfg = test_config();
        let _ = dispatch(&svc, &cfg, "t", &Request::Start(proto_start_params()));
        let resp = dispatch(&svc, &cfg, "t", &Request::Status);
        match resp {
            Response::Ok(ResponseKind::Status(Status::Running { pid })) => assert_eq!(pid, 5555),
            other => panic!("{other:?}"),
        }
    }

    // ===== stop =====

    #[test]
    fn stop_notrunning_when_no_child() {
        // helper.go:442
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(&svc, &cfg, "t", &Request::Stop { pid: None });
        assert!(matches!(
            resp,
            Response::Ok(ResponseKind::Stop(Stop::NotRunning))
        ));
    }

    #[test]
    fn stop_stopped_when_child_present() {
        // helper.go:440-441
        let svc = TestServices::new("t");
        let cfg = test_config();
        let _ = dispatch(&svc, &cfg, "t", &Request::Start(proto_start_params()));
        let resp = dispatch(&svc, &cfg, "t", &Request::Stop { pid: None });
        match resp {
            Response::Ok(ResponseKind::Stop(Stop::Stopped { pid })) => assert_eq!(pid, 5555),
            other => panic!("{other:?}"),
        }
    }

    /// **变异门（核心）**：身份不匹配 → 不摘不杀，child 原样留给新会话。
    ///
    /// 时序同 linux 版：老 stop 腿拿着旧 pid 落到已换了核的 daemon 上。
    ///
    /// 变异（逃逸面穷举）：
    /// - 删掉 [`terminate_managed_child`] 里的 `stop_pid_matches` 判据 → 响应变 `Stopped{5555}`
    ///   且 child 被摘 → 转红。
    /// - 只改响应不改行为（回 Mismatch 但仍 `take()`）→ 末条 child 断言转红。
    /// - dispatch 里把 `*pid` 丢掉、恒传 `None` → 判据永不触发 → 转红。
    #[test]
    fn stop_refuses_to_kill_when_managed_pid_is_another_session() {
        let svc = TestServices::new("t");
        let cfg = test_config();
        // daemon 手里的是新会话的核（TestServices 的 spawn 固定报 5555）。
        let _ = dispatch(&svc, &cfg, "t", &Request::Start(proto_start_params()));
        let resp = dispatch(&svc, &cfg, "t", &Request::Stop { pid: Some(4242) });
        assert_eq!(
            resp,
            Response::Ok(ResponseKind::Stop(Stop::Mismatch {
                want: 4242,
                current: 5555
            })),
            "身份不匹配 → 诚实 no-op，回报两个 pid"
        );
        assert_eq!(
            svc.child.lock().unwrap().as_ref().map(|c| c.pid),
            Some(5555),
            "child 必须原样留给新会话 —— 摘掉它 = 新核失联，daemon 此后再也停不掉"
        );
    }

    /// 反向失效门：身份匹配照常停（判据不能收得太紧）。
    #[test]
    fn stop_proceeds_when_managed_pid_matches() {
        let svc = TestServices::new("t");
        let cfg = test_config();
        let _ = dispatch(&svc, &cfg, "t", &Request::Start(proto_start_params()));
        let resp = dispatch(&svc, &cfg, "t", &Request::Stop { pid: Some(5555) });
        assert_eq!(
            resp,
            Response::Ok(ResponseKind::Stop(Stop::Stopped { pid: 5555 }))
        );
        assert!(svc.child.lock().unwrap().is_none(), "匹配时须真摘 child");
    }

    // ===== start =====

    #[test]
    fn start_already_when_child_exists() {
        // helper.go:521-523: OK already <pid>
        let svc = TestServices::new("t");
        let cfg = test_config();
        let _ = dispatch(&svc, &cfg, "t", &Request::Start(proto_start_params()));
        let resp = dispatch(&svc, &cfg, "t", &Request::Start(proto_start_params()));
        match resp {
            Response::Ok(ResponseKind::Start(StartResp::Already { pid })) => {
                assert_eq!(pid, 5555)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn start_no_config_when_empty_cfg() {
        // helper.go:525-527: ERR no-config
        let svc = TestServices::new("t");
        let cfg = test_config();
        let mut p = proto_start_params();
        p.cfg = String::new();
        let resp = dispatch(&svc, &cfg, "t", &Request::Start(p));
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::NoConfig,
                ..
            })
        ));
    }

    #[test]
    fn start_config_path_denied_when_outside_confdir() {
        // helper.go:528-531: ERR config-path-denied
        let svc = TestServices::new("t");
        let cfg = test_config(); // conf_dir = /Users/test/Polaris
        let mut p = proto_start_params();
        p.cfg = "/etc/passwd".into(); // 越权
        let resp = dispatch(&svc, &cfg, "t", &Request::Start(p));
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::ConfigPathDenied,
                ..
            })
        ));
    }

    #[test]
    fn start_ok_when_cfg_inside_confdir() {
        let svc = TestServices::new("t");
        let cfg = test_config();
        let mut p = proto_start_params();
        p.cfg = "/Users/test/Polaris/config.json".into();
        let resp = dispatch(&svc, &cfg, "t", &Request::Start(p));
        match resp {
            Response::Ok(ResponseKind::Start(StartResp::Started { pid })) => assert_eq!(pid, 5555),
            other => panic!("{other:?}"),
        }
    }

    /// log 与 cfg 走同一条白名单 —— 不校验就是「root 在任意位置建文件并持续追加写」。
    #[test]
    fn start_log_path_denied_when_outside_confdir() {
        let svc = TestServices::new("t");
        let cfg = test_config(); // conf_dir = /Users/test/Polaris
        let mut p = proto_start_params();
        // cfg 合法，只有 log 越界 —— 单独钉住 log 这一格。
        p.cfg = "/Users/test/Polaris/singbox-runtime.json".into();
        p.log = "/etc/newsyslog.d/pwn.conf".into();
        let resp = dispatch(&svc, &cfg, "t", &Request::Start(p));
        assert!(
            matches!(
                resp,
                Response::Err(ProtoError {
                    code: ErrorCode::LogPathDenied,
                    ..
                })
            ),
            "{resp:?}"
        );
    }

    /// 生产形态（cfg 与 log 同在 confDir）必须放行 —— 否则上面那条可能被「恒拒」满足。
    #[test]
    fn start_ok_when_log_inside_confdir() {
        let svc = TestServices::new("t");
        let cfg = test_config();
        let mut p = proto_start_params();
        p.cfg = "/Users/test/Polaris/singbox-runtime.json".into();
        p.log = "/Users/test/Polaris/singbox-startup.log".into();
        let resp = dispatch(&svc, &cfg, "t", &Request::Start(p));
        match resp {
            Response::Ok(ResponseKind::Start(StartResp::Started { pid })) => assert_eq!(pid, 5555),
            other => panic!("生产形态被误拒：{other:?}"),
        }
    }

    /// 空 log = 不重定向（`server.rs` 的 `if !log.is_empty()`），必须放行。
    #[test]
    fn start_ok_when_log_empty() {
        let svc = TestServices::new("t");
        let cfg = test_config();
        let mut p = proto_start_params();
        p.cfg = "/Users/test/Polaris/singbox-runtime.json".into();
        p.log = String::new();
        let resp = dispatch(&svc, &cfg, "t", &Request::Start(p));
        match resp {
            Response::Ok(ResponseKind::Start(StartResp::Started { pid })) => assert_eq!(pid, 5555),
            other => panic!("空 log 被误拒：{other:?}"),
        }
    }

    #[test]
    fn start_fwd_enables_ip_forwarding() {
        // helper.go:534-537: allowLan → sysctl net.inet.ip.forwarding=1 + ip6.forwarding=1
        let svc = TestServices::new("t");
        let cfg = test_config();
        let mut p = proto_start_params();
        p.cfg = "/Users/test/Polaris/c.json".into();
        p.fwd = true;
        let _ = dispatch(&svc, &cfg, "t", &Request::Start(p));
        let calls = svc.calls();
        let sysctls: Vec<_> = calls
            .iter()
            .filter(|(p, _)| p == "/usr/sbin/sysctl")
            .collect();
        assert_eq!(sysctls.len(), 2, "应有 ipv4 + ipv6 两次 sysctl");
        assert!(sysctls
            .iter()
            .any(|(_, a)| a.contains(&"net.inet.ip.forwarding=1".to_string())));
        assert!(sysctls
            .iter()
            .any(|(_, a)| a.contains(&"net.inet6.ip6.forwarding=1".to_string())));
    }

    fn proto_start_params() -> polaris_helper_proto::request::StartParams {
        polaris_helper_proto::request::StartParams {
            cfg: "/Users/test/Polaris/c.json".into(),
            log: String::new(),
            fwd: false,
            parent_pid: None,
        }
    }

    // ===== route-add / route-del =====

    #[test]
    fn route_add_denied_bad_iface() {
        // helper.go:457-459: ERR iface-denied
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(
            &svc,
            &cfg,
            "t",
            &Request::RouteAdd(RouteParams {
                iface: "en0".into(), // 非白名单
                cidrs: vec!["10.0.0.0/8".into()],
            }),
        );
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::IfaceDenied,
                ..
            })
        ));
    }

    #[test]
    fn route_add_ok_runs_route_cmd() {
        // helper.go:465-480
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(
            &svc,
            &cfg,
            "t",
            &Request::RouteAdd(RouteParams {
                iface: "polaris-ts".into(),
                cidrs: vec!["10.0.0.0/8".into(), "172.16.0.0/12".into()],
            }),
        );
        assert!(matches!(resp, Response::Ok(ResponseKind::Route)));
        let calls = svc.calls();
        let route_calls: Vec<_> = calls
            .iter()
            .filter(|(p, _)| p == route::ROUTE_BIN)
            .collect();
        assert_eq!(route_calls.len(), 2, "每个 CIDR 一次 route 命令");
    }

    #[test]
    fn route_add_skips_invalid_cidr() {
        // helper.go:470-471: 非法 CIDR continue
        let svc = TestServices::new("t");
        let cfg = test_config();
        let _ = dispatch(
            &svc,
            &cfg,
            "t",
            &Request::RouteAdd(RouteParams {
                iface: "polaris-wg".into(),
                cidrs: vec![
                    "10.0.0.0/8".into(),
                    "not-a-cidr".into(),
                    "192.168.0.0/16".into(),
                ],
            }),
        );
        let calls = svc.calls();
        let route_calls: Vec<_> = calls
            .iter()
            .filter(|(p, _)| p == route::ROUTE_BIN)
            .collect();
        assert_eq!(route_calls.len(), 2, "非法项跳过，仅 2 条 route");
    }

    #[test]
    fn route_del_uses_delete_subcmd() {
        let svc = TestServices::new("t");
        let cfg = test_config();
        let _ = dispatch(
            &svc,
            &cfg,
            "t",
            &Request::RouteDel(RouteParams {
                iface: "utun3".into(),
                cidrs: vec!["10.0.0.0/8".into()],
            }),
        );
        let calls = svc.calls();
        let route_call = calls.iter().find(|(p, _)| p == route::ROUTE_BIN).unwrap();
        assert!(route_call.1.contains(&"delete".to_string()));
    }

    // ===== default-restore =====

    #[test]
    fn default_restore_ok_valid_ipv4() {
        // helper.go:485-491
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(
            &svc,
            &cfg,
            "t",
            &Request::DefaultRestore {
                gateway_ipv4: "192.168.1.1".into(),
            },
        );
        assert!(matches!(resp, Response::Ok(ResponseKind::DefaultRestored)));
    }

    #[test]
    fn default_restore_bad_gateway() {
        // helper.go:486-489: ERR bad-gateway
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(
            &svc,
            &cfg,
            "t",
            &Request::DefaultRestore {
                gateway_ipv4: "not-an-ip".into(),
            },
        );
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::BadGateway,
                ..
            })
        ));
    }

    // ===== flush-dns =====

    #[test]
    fn flush_dns_response_wire() {
        // helper.go:506: OK flushed
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(&svc, &cfg, "t", &Request::FlushDns);
        let line = resp.to_wire_line();
        assert_eq!(line, "OK flushed");
    }

    // ===== cleanup =====

    #[test]
    fn cleanup_pkill_and_clears_child() {
        // helper.go:444-449: pkill -9 -f "<singboxBin> run" + 摘 child
        let svc = TestServices::new("t");
        let cfg = test_config();
        let _ = dispatch(&svc, &cfg, "t", &Request::Start(proto_start_params()));
        let resp = dispatch(&svc, &cfg, "t", &Request::Cleanup);
        assert!(matches!(resp, Response::Ok(ResponseKind::Cleaned)));
        let calls = svc.calls();
        let pkill = calls
            .iter()
            .find(|(p, _)| p == "/usr/bin/pkill")
            .expect("应有 pkill 调用");
        assert!(pkill.1.contains(&"-9".to_string()));
        assert!(pkill.1.contains(&"-f".to_string()));
        assert!(pkill
            .1
            .contains(&"/usr/local/lib/polaris/sing-box run".to_string()));
        // child 被摘除
        assert!(svc.child.lock().unwrap().is_none());
    }

    // ===== install-core（文件操作 + mac 签名）=====

    #[test]
    fn install_core_bad_args() {
        // helper.go:583-584 → installCore → :137 bad-args
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(
            &svc,
            &cfg,
            "t",
            &Request::InstallCore(InstallCoreParams {
                src_dir: "".into(),
                want_hash: "a".repeat(64),
            }),
        );
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::BadArgs,
                ..
            })
        ));
    }

    #[test]
    fn install_core_coredir_unset() {
        let svc = TestServices::new("t");
        let mut cfg = test_config();
        cfg.core_dir = String::new();
        let resp = dispatch(
            &svc,
            &cfg,
            "t",
            &Request::InstallCore(InstallCoreParams {
                src_dir: "/tmp/x".into(),
                want_hash: "a".repeat(64),
            }),
        );
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::CoredirUnset,
                ..
            })
        ));
    }

    #[test]
    fn install_core_success_runs_xattr_and_codesign() {
        // helper.go:195-196: 文件就位后 xattr -cr + codesign --force --deep -s -
        use sha2::{Digest, Sha256};
        let src = tempfile::tempdir().unwrap();
        let sb = b"fake sing-box";
        std::fs::write(src.path().join("sing-box"), sb).unwrap();
        let mut h = Sha256::new();
        h.update(sb);
        let hash = hex::encode(h.finalize());

        let svc = TestServices::new("t");
        let cfg = test_config();
        // 用临时 core_dir
        let core_tmp = tempfile::tempdir().unwrap();
        let mut cfg2 = cfg.clone();
        cfg2.core_dir = core_tmp.path().to_string_lossy().into_owned();

        let resp = dispatch(
            &svc,
            &cfg2,
            "t",
            &Request::InstallCore(InstallCoreParams {
                src_dir: src.path().to_string_lossy().into_owned(),
                want_hash: hash,
            }),
        );
        assert!(matches!(resp, Response::Ok(ResponseKind::Installed)));
        // 验证 xattr + codesign 被调
        let calls = svc.calls();
        assert!(calls.iter().any(|(p, _)| p == "/usr/bin/xattr"));
        assert!(calls.iter().any(|(p, _)| p == "/usr/bin/codesign"));
    }

    // ===== 未知/不属于 mac 谱系的命令 =====

    #[test]
    fn linux_start_unknown_on_mac() {
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(
            &svc,
            &cfg,
            "t",
            &Request::LinuxStart(polaris_helper_proto::request::LinuxStartParams {
                singbox_path: "/x".into(),
                common: proto_start_params(),
            }),
        );
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::Unknown,
                ..
            })
        ));
    }

    #[test]
    fn iface_metric_unknown_on_mac() {
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(
            &svc,
            &cfg,
            "t",
            &Request::IfaceMetric {
                iface: "polaris-ts".into(),
                metric: 100,
            },
        );
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::Unknown,
                ..
            })
        ));
    }

    // ===== wire 序列化（对照 Go 各 fmt.Fprintln/Fprintf 调用点）=====

    #[test]
    fn wire_started_format() {
        // helper.go:579: OK started <pid>
        let r = Response::Ok(ResponseKind::Start(StartResp::Started { pid: 1234 }));
        assert_eq!(r.to_wire_line(), "OK started 1234");
    }

    #[test]
    fn wire_already_format() {
        // helper.go:522: OK already <pid>
        let r = Response::Ok(ResponseKind::Start(StartResp::Already { pid: 5678 }));
        assert_eq!(r.to_wire_line(), "OK already 5678");
    }

    #[test]
    fn wire_freeport_killed_format() {
        // helper.go:393: OK killed <pid>,<pid>
        let r = Response::Ok(ResponseKind::FreePort(
            polaris_helper_proto::response::FreePort::Killed {
                pids: vec![111, 222],
            },
        ));
        assert_eq!(r.to_wire_line(), "OK killed 111,222");
    }

    #[test]
    fn wire_freeport_foreign_format() {
        // helper.go:391: OK foreign <name> | <name>
        let r = Response::Ok(ResponseKind::FreePort(
            polaris_helper_proto::response::FreePort::Foreign {
                names: vec!["nginx".into(), "pid:789".into()],
            },
        ));
        assert_eq!(r.to_wire_line(), "OK foreign nginx | pid:789");
    }

    #[test]
    fn wire_flushed_partial_format() {
        // helper.go:503: OK flushed-partial killall-hup <err> <out>
        let r = Response::Ok(ResponseKind::FlushDns(
            polaris_helper_proto::response::FlushDns::FlushedPartial {
                tail: "killall-hup exit status 1 ".into(),
            },
        ));
        let line = r.to_wire_line();
        assert!(line.starts_with("OK flushed-partial"));
        assert!(line.contains("killall-hup"));
    }

    #[test]
    fn wire_err_format() {
        let r = Response::Err(ProtoError::with_detail(ErrorCode::Start, "exit status 1"));
        assert_eq!(r.to_wire_line(), "ERR start exit status 1");
    }

    // ===== freeport 经 dispatch 端到端（mock 已在 freeport.rs 测，这里测 dispatch 路由）=====

    #[test]
    fn dispatch_routes_freeport() {
        // freeport 在鉴权后、锁前处理（helper.go:413）
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(&svc, &cfg, "t", &Request::FreePort { port: 9090 });
        // mock lsof 返回空 → OK free
        assert!(matches!(
            resp,
            Response::Ok(ResponseKind::FreePort(
                polaris_helper_proto::response::FreePort::Free
            ))
        ));
    }

    #[test]
    fn dispatch_freeport_still_requires_auth() {
        // 即使是 freeport，token 错也先 ERR auth
        let svc = TestServices::new("t");
        let cfg = test_config();
        let resp = dispatch(&svc, &cfg, "wrong", &Request::FreePort { port: 9090 });
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::Auth,
                ..
            })
        ));
    }

    /// 确认 FileTokenStore 可作 token_store 注入（编译期验证 trait 兼容）。
    #[test]
    fn file_token_store_trait_compat() {
        let _store = FileTokenStore::new("/tmp/nonexistent");
    }
}
