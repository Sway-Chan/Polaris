//! Unix socket 服务端（移植自 上游 `helper/helper.go:591-643` 的 `main()`）。
//!
//! ## Go 源结构（`helper.go:604-642`）
//!
//! ```text
//! sockPath := filepath.Join(supportDir, "helper.sock")
//! os.Remove(sockPath)
//! l, _ := net.Listen("unix", sockPath)
//! os.Chmod(sockPath, 0666)   // token 为安全边界
//! // SIGTERM/SIGINT 收割器（proto v3）
//! for { conn, _ := l.Accept(); go handle(conn) }
//! ```
//!
//! ## 移植纪律
//!
//! socket 监听 + accept 循环是 mac-gated 系统操作（`std::os::unix::net::UnixListener`）。
//! 帧解析（token 行 + command + args → [`Request`]）是跨平台纯逻辑 —— 抽为
//! [`decode_request`]，可在 Linux 上完整测「Go wire 形态 → Request」的解析正确性。
//!
//! ## 安全（`helper.go:3-9,611`）
//!
//! socket 权限 0666（任何本地进程可连）—— **token 行是唯一安全边界**（见 [`crate::token`]）。
//! 0666 是为让普通用户 app 能连；远程不可达（unix socket 仅本机）。

use crate::line_io::{read_line_trimmed, write_line};
use crate::platform::macos::handler::{dispatch, MacConfig, MacServices};
use polaris_helper_proto::request::{InstallCoreParams, RouteParams, StartParams};
use polaris_helper_proto::{parse_stop_pid, Request};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Mutex;
use std::time::Duration;

/// socket 文件名（`helper.go:604`：`helper.sock`）。
pub const SOCK_FILENAME: &str = "helper.sock";

/// socket 权限 0666（`helper.go:611`，token 为安全边界）。
#[cfg(unix)]
pub const SOCK_MODE: u32 = 0o666;

/// 连接读超时（`helper.go:401`：5s）。
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// 帧解码错误（wire 形态不符合预期）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// 命令名未识别（Go `default: ERR unknown`，`helper.go:587`）。
    UnknownCommand(String),
    /// 缺少必要参数行（如 start 无 cfg）。
    MissingArg(&'static str),
}

/// 从「行流」解码出 [`Request`]（移植自 Go `handle()` 的 readLine 序列 + switch）。
///
/// 对照 Go `helper.go:403-585`：先读 command 行，再按命令读对应数量的参数行。
/// `lines` 是已读到的参数行迭代器（token 行已由调用方先消费做鉴权）。
///
/// 严格对照各 case 的 readLine 顺序：
/// - ping/version/status/stop/cleanup/flush-dns：无参数行
/// - freeport：1 行 port
/// - start：cfg/log/fwd/ppid(可选)
/// - route-add/route-del：iface/cidrs
/// - install-core：src/wantHash
/// - default-restore：gateway
///
/// ppid 可选行（Go `helper.go:513`：`strconv.Atoi(readLine(r))`，EOF 返回 "" → 0 → 不启看护）。
pub fn decode_request<I: Iterator<Item = String>>(
    command: &str,
    lines: &mut I,
) -> Result<Request, DecodeError> {
    match command {
        "ping" => Ok(Request::Ping),
        "version" => Ok(Request::Version),
        "status" => Ok(Request::Status),
        // stop 的受管 pid 身份行是**可选**的（旧客户端不发 → LineIter 在 EOF 产 None → `None`，
        // 沿用「停当前受管核」旧语义）。见 `polaris_helper_proto::stop_pid_matches`。
        "stop" => Ok(Request::Stop {
            pid: parse_stop_pid(&lines.next().unwrap_or_default()),
        }),
        "cleanup" => Ok(Request::Cleanup),
        "flush-dns" => Ok(Request::FlushDns),
        "freeport" => {
            // helper.go:362: port := TrimSpace(readLine)
            let port_str = lines.next().unwrap_or_default().trim().to_owned();
            let port: u16 = port_str
                .parse()
                .map_err(|_| DecodeError::MissingArg("port"))?;
            Ok(Request::FreePort { port })
        }
        "start" => {
            // helper.go:508-513: cfg/log/fwd/ppid
            let cfg = lines.next().ok_or(DecodeError::MissingArg("cfg"))?;
            let log = lines.next().unwrap_or_default();
            let fwd_str = lines.next().unwrap_or_default();
            let fwd = fwd_str.trim() == "1";
            // helper.go:513: ppid 可选（EOF → "" → 0 → None）
            let ppid_str = lines.next().unwrap_or_default();
            let parent_pid = ppid_str.trim().parse::<u32>().ok().filter(|&p| p > 0);
            Ok(Request::Start(StartParams {
                cfg,
                log,
                fwd,
                parent_pid,
            }))
        }
        "route-add" | "route-del" => {
            // helper.go:455-456: iface/cidrs
            let iface = lines.next().ok_or(DecodeError::MissingArg("iface"))?;
            let cidrs_line = lines.next().unwrap_or_default();
            let cidrs: Vec<String> = cidrs_line
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            let req = Request::RouteAdd(RouteParams { iface, cidrs });
            if command == "route-del" {
                // 重建为 RouteDel（字段同构）
                if let Request::RouteAdd(rp) = req {
                    return Ok(Request::RouteDel(rp));
                }
            }
            Ok(req)
        }
        "install-core" => {
            // helper.go:583-584: src/wantHash
            let src_dir = lines.next().ok_or(DecodeError::MissingArg("src_dir"))?;
            let want_hash = lines
                .next()
                .ok_or(DecodeError::MissingArg("want_hash"))?
                .trim()
                .to_owned();
            Ok(Request::InstallCore(InstallCoreParams {
                src_dir,
                want_hash,
            }))
        }
        "default-restore" => {
            // helper.go:485: gateway
            let gateway_ipv4 = lines
                .next()
                .ok_or(DecodeError::MissingArg("gateway_ipv4"))?
                .trim()
                .to_owned();
            Ok(Request::DefaultRestore { gateway_ipv4 })
        }
        other => Err(DecodeError::UnknownCommand(other.to_owned())),
    }
}

/// 连接处理结果。
#[derive(Debug)]
pub enum ConnOutcome {
    /// 正常处理完毕。
    Done,
    /// 读/写 IO 错误。
    IoError(String),
}

// ===== 纯决策逻辑（跨平台可测；mac-gated 生命周期代码经此委托，杜绝 syscall 内藏判定）=====

/// 命令是否须在 `command_mu` 临界区内处理（移植自 `helper.go:413` vs `:418`）。
///
/// `freeport` 不持锁（其 `lsof`/`ps` 在 stale 挂载下可能长阻塞，持锁会饿死并发 ping/status/start/stop）；
/// 其余命令一律持锁（Go `mu.Lock(); defer mu.Unlock()`）。
#[must_use]
pub fn should_lock_command(command: &str) -> bool {
    command != "freeport"
}

/// chownRuntimeDirs 归还属主的运行时子目录名（移植自 `helper.go:222`）。
///
/// root 跑 sing-box 会把这些目录里的文件写成 root 600 → 登录用户跑读不了 → endpoint post-start FATAL。
pub const CHOWN_SUBDIRS: [&str; 3] = ["tailscale", "singbox-dashboard", "ui"];

/// root 核会直接写在 confDir 根部、退出后必须归还登录用户的运行时文件。
///
/// `cache.db` 不在 [`CHOWN_SUBDIRS`] 的树内；漏掉它会让 TUN(root) → 系统模式(user) 的下一次
/// 起核以 `initialize cache-file: permission denied` 退出。启动前另有 fd 级属主准备，退出归还是
/// 对旧 helper 遗留文件与异常退出的第二道收口。
pub const CHOWN_FILES: [&str; 1] = ["cache.db"];

/// 某条目是否需 `Lchown` 归还（移植自 `helper.go:237`）。
///
/// 仅 root（uid==0）写入的条目才归还 —— 已是登录用户属主的跳过，省无谓 Lchown（TUN 起停周期对可能很大的树）。
#[must_use]
pub fn should_chown_entry(entry_uid: u32) -> bool {
    entry_uid == 0
}

/// confDir 本身属 root 时是否跳过整个 chown（移植自 `helper.go:219-221`）。
///
/// confDir 应属登录用户（app 数据目录）；若属 root（异常）→ 不动，避免把运行时目录误归 root。
#[must_use]
pub fn should_skip_confdir_chown(confdir_uid: u32) -> bool {
    confdir_uid == 0
}

/// terminateChild 是否须升级 SIGKILL（移植自 `helper.go:282-286`）。
///
/// `exited`=进程是否已在宽限窗口内退出（`done` 触发）。未退出 → SIGKILL 强杀。
#[must_use]
pub fn terminate_needs_kill(exited: bool) -> bool {
    !exited
}

/// 处理一个已 accept 的连接（移植自 Go `handle()`，`helper.go:397-589`）。
///
/// 流程：
/// 1. 设读超时 5s（`helper.go:401`）。
/// 2. 读 token 行 + command 行。
/// 3. 解码 command + 参数行为 [`Request`]。
/// 4. `dispatch` 得 [`Response`](polaris_helper_proto::Response)。
/// 5. 写回 wire 行。
///
/// 泛型 `R: Read` 让测试可注入 `Cursor<&[u8]>`，生产接 `UnixStream`。
///
/// `command_mu`（`§3.3 defect 7`，补 Go 单锁）：非 `freeport` 命令在此锁临界区内处理（对齐 Go
/// `helper.go:418` `mu.Lock(); defer mu.Unlock()`，杜绝并发 start 双起核等复合 child 竞态）；`freeport`
/// 不持锁（`helper.go:413`）。生产 serve 传 `Some(&command_mu)`；单线程测试传 `None`（无锁）。
pub fn process_connection<R: Read, W: Write>(
    reader: R,
    mut writer: W,
    services: &dyn MacServices,
    config: &MacConfig,
    command_mu: Option<&Mutex<()>>,
) -> ConnOutcome {
    let mut buf = BufReader::new(reader);
    // helper.go:403-404: token 行 + command 行（锁前读，与 Go 一致）
    let token = match read_line_trimmed(&mut buf) {
        Some(s) => s,
        None => return ConnOutcome::Done, // 连接关闭无数据
    };
    let command = match read_line_trimmed(&mut buf) {
        Some(s) => s,
        None => return ConnOutcome::Done,
    };

    // 🔴 **鉴权早退必须在取锁之前**（`dispatch` 里那道保留作纵深）。
    //
    // socket 是 0666（设计如此，token 行是唯一安全边界，见模块头）。若先取锁再鉴权，一条**未鉴权**
    // 的连接就能：写 token+command 两行后不再写任何数据 → 命中 `should_lock_command` 取到全局
    // `command_mu` → 在锁内的 `decode_request` 阻塞读参数行，直到 5s 连接读超时才放锁。
    // 即「零 token、单条连接 = 独占 root daemon 命令锁 5 秒」；serve 每连接一线程且无并发上限，
    // 循环开 N 条即可把锁占满 —— 期间 GUI 侧 stop/start/status/flush-dns 全部排队超时，
    // 用户点「断开」得到通信失败，而 root sing-box 与 TUN 仍在跑，网络接管解除不了。
    //
    // linux 腿与 Go 源都是**先鉴权后取锁**（`linux/handler.rs` 的步骤 4→5、Go `helper.go` 的
    // `tok != tokenValue() → ERR auth; return` 在 `mu.Lock()` 之前），本处属移植时的次序回退。
    if !matches!(
        crate::token::check_token(&token, &services.token_store().token_value()),
        crate::token::TokenCheck::Authed
    ) {
        let resp = polaris_helper_proto::Response::Err(polaris_helper_proto::Error::new(
            polaris_helper_proto::ErrorCode::Auth,
        ));
        return write_response(&mut writer, &resp);
    }

    // helper.go:413 vs :418 —— freeport 锁外、其余锁内。守卫持有到函数末（覆盖参数解码 + dispatch + 响应
    // 写，与 Go `defer mu.Unlock()` 一致，读超时 5s 兜底防持锁挂死）。command_mu==None（测试）→ 无锁。
    // 锁中毒（某连接线程 panic）时 into_inner 复用（daemon 不因单连接崩溃而永久失锁）。
    let _mu_guard = if should_lock_command(&command) {
        command_mu.map(|m| m.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    } else {
        None
    };

    // 解码命令 + 参数行
    let mut lines_iter = LineIter { buf: &mut buf };
    let req = match decode_request(&command, &mut lines_iter) {
        Ok(r) => r,
        // helper.go:587: 未知命令 / 参数不足 → 统一兜底 ERR unknown（避免歧义）
        Err(DecodeError::UnknownCommand(_) | DecodeError::MissingArg(_)) => {
            let resp = polaris_helper_proto::Response::Err(polaris_helper_proto::Error::new(
                polaris_helper_proto::ErrorCode::Unknown,
            ));
            return write_response(&mut writer, &resp);
        }
    };

    let resp = dispatch(services, config, &token, &req);
    write_response(&mut writer, &resp)
}

/// 行迭代器：包装 BufReader，逐行产出 String。供 [`decode_request`] 消费。
///
/// 读行本体已上提 [`crate::line_io::read_line_trimmed`]（与 linux 共用单一真值）；
/// 本类型只保留 mac 侧的 `Iterator` 适配形状。
struct LineIter<'a, R: BufRead> {
    buf: &'a mut R,
}

impl<R: BufRead> Iterator for LineIter<'_, R> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        read_line_trimmed(self.buf)
    }
}

/// 写一个响应行（加 `\n`，对齐 Go `Fprintln`）—— 委托公共 [`line_io::write_line`]。
fn write_response<W: Write>(writer: &mut W, resp: &polaris_helper_proto::Response) -> ConnOutcome {
    match write_line(writer, &resp.to_wire_line()) {
        Ok(()) => ConnOutcome::Done,
        Err(e) => ConnOutcome::IoError(e.to_string()),
    }
}

// ===== mac-gated：真 unix socket 服务端 + child 生命周期（M1/M3/M4/M11/M12/M13）=====

#[cfg(target_os = "macos")]
mod sys {
    use super::*;
    use crate::platform::macos::exec::SystemRunner;
    use crate::platform::macos::handler::{ChildHandle, SpawnError, TerminateOutcome};
    use crate::platform::macos::proc_start::{
        classify_alive, kill_zero_exists, proc_start_time, AliveProbe, TERMINATE_GRACE,
        WATCH_TICK_INTERVAL,
    };
    use crate::token::{FileTokenStore, TokenStore};
    use nix::sys::signal::Signal;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    /// child 退出信号（等价 Go `childDone chan struct{}`）。
    ///
    /// 收割线程 `wait()` 完成后 [`signal`](DoneFlag::signal)（等价 `close(done)`）；terminateChild 与
    /// watchParent 据此免 KILL / 退出看护（等价 Go `select { <-done: ... }`）。多等待者（terminate +
    /// watchParent）同 `Condvar` 唤醒。
    struct DoneFlag {
        exited: Mutex<bool>,
        cv: Condvar,
    }

    impl DoneFlag {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                exited: Mutex::new(false),
                cv: Condvar::new(),
            })
        }

        /// 置「已退出」并唤醒所有等待者（等价 Go `close(done)`）。
        fn signal(&self) {
            let mut g = self.exited.lock().unwrap();
            *g = true;
            self.cv.notify_all();
        }

        /// 等 `exited` 置位或超时 `d`。返回 `true`=已退出、`false`=超时。
        ///
        /// 等价 Go `select { <-done: (true); <-time.After(d): (false) }`。用绝对截止时间循环 wait，
        /// 规避 `Condvar` 虚假唤醒把等待窗口累加。
        fn wait_exit(&self, d: Duration) -> bool {
            let start = Instant::now();
            let mut g = self.exited.lock().unwrap();
            while !*g {
                let elapsed = start.elapsed();
                if elapsed >= d {
                    break;
                }
                let (ng, res) = self.cv.wait_timeout(g, d - elapsed).unwrap();
                g = ng;
                if res.timed_out() {
                    break;
                }
            }
            *g
        }
    }

    /// child 的 `done` + 身份代记账（proc 内层锁；全局 lock 顺序恒为 `child` → `proc`）。
    struct ProcSlot {
        /// 当前 child 的退出信号（`None`=无 child；等价 Go `childDone`）。
        done: Option<Arc<DoneFlag>>,
        /// 身份代（每次 spawn 自增）—— Rust 无 Go 的 `child *exec.Cmd` 指针身份，用单调代号代替
        /// `child == c` 判定（识别「收割/看护的是不是当前这个 child」，防 PID 复用误判）。
        generation: u64,
    }

    /// 生产 [`MacServices`]：真 child 生命周期（`helper.go` 的 `child`/`childDone`/`mu` 全局态 + spawn/wait
    /// 收割/terminate/watchParent/chownRuntimeDirs）。
    ///
    /// 锁层级（恒 `command_mu` → `child` → `proc`，无环）：
    /// - `command_mu`：Go `mu`。server 在 [`process_connection`] 里跨整条非-freeport 命令持有（串行化命令）。
    /// - `child`（trait [`MacServices::child`]）：pid 视图（Go `child`），dispatch 与后台线程共访。
    /// - `proc`：`done`/`generation`（Go `childDone` + 指针身份）。
    ///
    /// 后台线程（收割 / terminate / watchParent）**不持 `command_mu`**（对齐 Go `go terminateChild`/
    /// `go watchParent` 不持 `mu`）——只经 `child`/`proc` 与 dispatch 协调。
    pub struct DaemonServices {
        token: FileTokenStore,
        runner: SystemRunner,
        config: MacConfig,
        command_mu: Mutex<()>,
        child: Arc<Mutex<Option<ChildHandle>>>,
        proc: Arc<Mutex<ProcSlot>>,
    }

    impl DaemonServices {
        /// 从锁定的 [`MacConfig`] 构造（token 读 `support_dir/helper.token`，与 Go `tokenValue` 一致）。
        #[must_use]
        pub fn new(config: MacConfig) -> Arc<Self> {
            Arc::new(Self {
                token: FileTokenStore::new(&config.support_dir),
                runner: SystemRunner::new(),
                config,
                command_mu: Mutex::new(()),
                child: Arc::new(Mutex::new(None)),
                proc: Arc::new(Mutex::new(ProcSlot {
                    done: None,
                    generation: 0,
                })),
            })
        }

        /// 锁定配置（serve 建 socket / 兜底 pkill 用）。
        #[must_use]
        pub fn config(&self) -> &MacConfig {
            &self.config
        }

        /// 命令串行锁（serve 传给 [`process_connection`]，补 Go `mu` 单锁纪律）。
        #[must_use]
        pub fn command_mu(&self) -> &Mutex<()> {
            &self.command_mu
        }

        /// 真 spawn（`helper.go:538-578`）：起锁定核 + log 重定向 + 收割线程 + ppid>0 起 watchParent。
        ///
        /// 调用前 dispatch 已在 `command_mu` 下过 already-check + cfgAllowed + fwd sysctl（`helper.go:521-537`）。
        fn do_spawn(
            &self,
            cfg: &str,
            log: &str,
            _fwd: bool,
            parent_pid: Option<u32>,
        ) -> Result<u32, SpawnError> {
            let mut child_g = self.child.lock().unwrap();
            // command_mu 下 already-check 已保证此处为 None；防御性再判，杜绝双起核（§3.3 defect 4 的 mac 面）。
            if let Some(existing) = child_g.as_ref() {
                return Ok(existing.pid);
            }
            // root sing-box 会写 `<conf_dir>/cache.db`。文件若由上一轮 TUN 创建成 root:staff 0644，
            // 切到用户态系统代理后会直接 FATAL permission denied；只在退出后 chown 又会与紧接着的
            // 重启竞速。故 root 起核前先按 conf_dir 属主准备 cache，root 仍可写，后续用户核也可写。
            // O_NOFOLLOW + fd 级 fchown：conf_dir 属用户可写，路径级 chown 存在换成 symlink 的 TOCTOU。
            prepare_cache_for_user(&self.config.conf_dir).map_err(SpawnError::Failed)?;
            // helper.go:538: exec.Command(singboxBin, "run", "-c", cfg)
            let mut cmd = std::process::Command::new(&self.config.singbox_bin);
            cmd.arg("run").arg("-c").arg(cfg);
            // CWD = 受管 conf_dir（root 拥有、0755 可写）：helper daemon 由 launchd 拉起 CWD=`/`，其 spawn 的核
            // 继承 `/` → dashboard 下载兜底相对 mkdir `/dashboard` 只读失败每次起核报一条噪音。设为可写 conf_dir 即消。
            // Polaris 生成的核配置其余路径全绝对（cache/log/rules/…），不受 CWD 影响。conf_dir 空则不设（继承旧行为）。
            if !self.config.conf_dir.is_empty() {
                cmd.current_dir(&self.config.conf_dir);
            }
            // B3/W26：不再把 child 直接绑到一个永不重开的 append fd。那种形状外部 rename 后 child
            // 仍持续写旧 inode，Windows 还可能直接拒绝 rename，无法形成运行期硬上限。改用 pipe，
            // 由 shared `polaris-log-budget` writer 掌握 current + `.1` 两代并在本次运行中轮转。
            if !log.is_empty() {
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());
            }
            // helper.go:548-554: c.Start()
            let mut child = cmd.spawn().map_err(|e| SpawnError::Failed(e.to_string()))?;
            let pid = child.id();
            if !log.is_empty() {
                polaris_log_budget::spawn_pipe_loggers(
                    child.stdout.take(),
                    child.stderr.take(),
                    log,
                    polaris_log_budget::DEFAULT_GENERATION_BYTES,
                );
            }
            // helper.go:559-561: child=c; childDone=done（身份代自增）
            let done = DoneFlag::new();
            let generation = {
                let mut proc_g = self.proc.lock().unwrap();
                proc_g.generation += 1;
                proc_g.done = Some(Arc::clone(&done));
                proc_g.generation
            };
            *child_g = Some(ChildHandle { pid });
            drop(child_g);
            // helper.go:517-519: 启动时快照父进程启动时间（唯一身份，watchParent 破 PID 复用假阴性）
            let ppid_start = parent_pid.and_then(proc_start_time);
            // helper.go:562-574: 收割 goroutine（Wait → close(done) → chownRuntimeDirs → 清 child if ==c）
            spawn_reaper(
                Arc::clone(&self.child),
                Arc::clone(&self.proc),
                self.config.conf_dir.clone(),
                child,
                Arc::clone(&done),
                generation,
            );
            // helper.go:576-578: 父死看护（proto v2）
            if let Some(ppid) = parent_pid {
                spawn_watch_parent(
                    Arc::clone(&self.child),
                    Arc::clone(&self.proc),
                    ppid,
                    ppid_start,
                    generation,
                    done,
                );
            }
            Ok(pid)
        }

        /// 真 stop（`helper.go:432-443`）：**受管 pid 身份校验** → 摘 child → 后台 terminateChild
        /// （TERM→≤5s→KILL），立即回复。
        ///
        /// 身份不匹配（手里的核属另一个会话）⇒ 直接返回 [`TerminateOutcome::Mismatch`]，**不摘 child、
        /// 不碰 done、不发任何信号** —— 判据与摘除同在 child 锁内，见
        /// [`polaris_helper_proto::stop_pid_matches`]。
        fn do_terminate(&self, want_pid: Option<u32>) -> TerminateOutcome {
            let taken = {
                let mut child_g = self.child.lock().unwrap();
                let mut proc_g = self.proc.lock().unwrap();
                if let Some(h) = child_g.as_ref() {
                    if !polaris_helper_proto::stop_pid_matches(want_pid, h.pid) {
                        return TerminateOutcome::Mismatch {
                            want: want_pid.unwrap_or(0),
                            current: h.pid,
                        };
                    }
                }
                // helper.go:434-436: 摘 child + done（收割权独占；无 child → None，不碰 done）
                child_g.take().map(|h| (h.pid, proc_g.done.take()))
            };
            match taken {
                Some((pid, done)) => {
                    // helper.go:439: go terminateChild(c, done)（不持锁，后台收割；client stop 超时仅 3-5s）
                    std::thread::spawn(move || match done {
                        Some(done) => terminate_pid(pid, &done),
                        None => {
                            let _ = send_signal(pid, Signal::SIGTERM);
                        }
                    });
                    TerminateOutcome::Stopped { pid }
                }
                None => TerminateOutcome::NotRunning,
            }
        }

        /// 信号收割器主体（`helper.go:618-634`）：摘 child → 有则同步 graceful terminate（≤5s）否则 pkill
        /// 兜底 → `exit(0)`。**同步**收割（非后台）—— Go 的收割 goroutine 在 terminateChild 返回后才
        /// `os.Exit(0)`，否则退出会杀掉收割线程、丢 KILL 升级。
        pub fn shutdown_reap(&self) -> ! {
            // helper.go:620-623: mu.Lock; c,done=child,childDone; child,childDone=nil,nil; mu.Unlock
            let taken = {
                let mut child_g = self.child.lock().unwrap();
                let mut proc_g = self.proc.lock().unwrap();
                let done = proc_g.done.take();
                child_g.take().map(|h| (h.pid, done))
            };
            match taken {
                // helper.go:624-625: 有 child → terminateChild（同步 TERM→≤5s→KILL）
                Some((pid, Some(done))) => terminate_pid(pid, &done),
                Some((pid, None)) => {
                    let _ = send_signal(pid, Signal::SIGTERM);
                }
                // helper.go:627-631: child==nil → pkill -9 -U 0 -f "<singbox> run" 兜底（限 root 进程，避免
                // 误杀 systemProxy 模式下 app 直起的用户态核；覆盖 stop 后台收割窗口内丢失的 KILL 升级）
                None => {
                    let filter = format!("{} run", self.config.singbox_bin);
                    let _ = std::process::Command::new("/usr/bin/pkill")
                        .args(["-9", "-U", "0", "-f", &filter])
                        .status();
                }
            }
            // helper.go:633: os.Exit(0)
            std::process::exit(0);
        }
    }

    impl MacServices for DaemonServices {
        fn token_store(&self) -> &dyn TokenStore {
            &self.token
        }
        fn runner(&self) -> &dyn crate::platform::macos::exec::CommandRunner {
            &self.runner
        }
        fn child(&self) -> &Mutex<Option<ChildHandle>> {
            self.child.as_ref()
        }
        // uid() 走 trait 默认（`nix::unistd::Uid::current()` = `os.Getuid()`）—— 生产 root 下返回 0。
        fn spawn_child(
            &self,
            cfg: &str,
            log: &str,
            fwd: bool,
            parent_pid: Option<u32>,
        ) -> Result<u32, SpawnError> {
            self.do_spawn(cfg, log, fwd, parent_pid)
        }
        fn terminate_child(&self, want_pid: Option<u32>) -> TerminateOutcome {
            self.do_terminate(want_pid)
        }
        fn clear_child(&self) {
            // helper.go:448: child, childDone = nil, nil（cleanup 同步清 done/generation 视图）。
            // 身份代不动 —— 已 pkill 的 child 的收割线程仍会 wait()+触发，届时 generation 若已被新 start
            // 自增则守卫跳过，否则清（幂等）。
            let mut child_g = self.child.lock().unwrap();
            let mut proc_g = self.proc.lock().unwrap();
            *child_g = None;
            proc_g.done = None;
        }
    }

    /// 收割线程（`helper.go:562-574` 的 `go func(){ c.Wait(); close(done); chownRuntimeDirs(); ... }`）。
    fn spawn_reaper(
        child: Arc<Mutex<Option<ChildHandle>>>,
        proc: Arc<Mutex<ProcSlot>>,
        conf_dir: String,
        mut cmd_child: std::process::Child,
        done: Arc<DoneFlag>,
        generation: u64,
    ) {
        std::thread::spawn(move || {
            // helper.go:563: c.Wait()（收割 zombie；期间不持锁）
            let _ = cmd_child.wait();
            // helper.go:564: close(done)（广播：terminateChild 免 KILL、watchParent 退出）
            done.signal();
            // helper.go:565-568: root 跑的 sing-box 退出后把运行时目录属主归还登录用户（放 Wait 之后确保属主稳定）
            chown_runtime_dirs(&conf_dir);
            // helper.go:569-573: if child == c { child, childDone = nil, nil }（身份代守卫）
            let mut child_g = child.lock().unwrap();
            let mut proc_g = proc.lock().unwrap();
            if proc_g.generation == generation {
                *child_g = None;
                proc_g.done = None;
            }
        });
    }

    /// 父死看护线程（`helper.go:316-356` 的 `watchParent`）。
    ///
    /// 每秒探测父 app：`kill(ppid,0)==ESRCH`（父已死）或启动时间变了（PID 复用假阴性）→ 摘 child 收割。
    /// 退出条件（防线程泄漏）：child 退出（`done`）/ 被 stop·cleanup·新 start 摘除（身份代不符）/ 父死收割完成。
    fn spawn_watch_parent(
        child: Arc<Mutex<Option<ChildHandle>>>,
        proc: Arc<Mutex<ProcSlot>>,
        ppid: u32,
        ppid_start: Option<String>,
        generation: u64,
        done: Arc<DoneFlag>,
    ) {
        std::thread::spawn(move || {
            loop {
                // helper.go:319-324: select { <-done: return; <-t.C: tick }
                // wait_exit(1s)：done 触发→true→退出看护；超时→false→本轮探测。
                if done.wait_exit(WATCH_TICK_INTERVAL) {
                    return; // child 已退出，看护使命结束
                }
                // helper.go:325-330: current := child==c; if !current return（收割责任已转移）
                {
                    let _child_g = child.lock().unwrap();
                    let proc_g = proc.lock().unwrap();
                    if proc_g.generation != generation {
                        return;
                    }
                }
                // helper.go:336-343: 父死判定两路（ESRCH / PID 复用）
                let exists = kill_zero_exists(ppid);
                // helper.go:339-341: kill==nil && ppidStart!="" 才取当前启动时间比对
                let current = if exists && ppid_start.is_some() {
                    proc_start_time(ppid)
                } else {
                    None
                };
                let dead = matches!(
                    classify_alive(exists, ppid_start.as_deref(), current.as_deref()),
                    AliveProbe::Dead | AliveProbe::PidReused
                );
                if dead {
                    // helper.go:344-353: 与 stop 竞态 —— 再确认 child==c → 摘除 → 独占收割
                    let pid = {
                        let mut child_g = child.lock().unwrap();
                        let mut proc_g = proc.lock().unwrap();
                        if proc_g.generation != generation {
                            return; // 他人已摘除，收割权已转移
                        }
                        proc_g.done = None;
                        child_g.take().map(|h| h.pid)
                    };
                    if let Some(pid) = pid {
                        terminate_pid(pid, &done);
                    }
                    return;
                }
            }
        });
    }

    /// terminateChild（`helper.go:277-287`）：SIGTERM → 等 `done` 或宽限窗口 → SIGKILL。**不持 `command_mu`**。
    ///
    /// 信号按 pid 直发（`nix::kill`）——`done` 已在收割线程 `wait()` 后置位，正常退出路径 `wait_exit` 返回
    /// `true`、跳过 KILL；仅未在 5s 内退出才 SIGKILL（对齐 Go `select{done / 5s Kill}`）。
    fn terminate_pid(pid: u32, done: &DoneFlag) {
        // helper.go:281: SIGTERM
        let _ = send_signal(pid, Signal::SIGTERM);
        // helper.go:282-286: 等 done 或 5s → 决定是否 KILL
        let exited = done.wait_exit(TERMINATE_GRACE);
        if terminate_needs_kill(exited) {
            let _ = send_signal(pid, Signal::SIGKILL);
        }
    }

    /// 向 pid 发信号（`nix::sys::signal::kill` safe wrapper，等价 Go `c.Process.Signal`/`Kill`）。
    fn send_signal(pid: u32, sig: Signal) -> Result<(), nix::Error> {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), sig)
    }

    /// root 起核前把 cache.db 锁定为 confDir 属主。
    ///
    /// 不截断既有数据库；最后一段若为符号链接则 `O_NOFOLLOW` 拒绝。打开后只对 fd `fchown`，
    /// 不在用户可写目录里做「检查路径 → 再 chown 路径」的 TOCTOU。
    pub(super) fn prepare_cache_for_user(conf_dir: &str) -> Result<(), String> {
        if conf_dir.is_empty() {
            return Ok(());
        }
        let dir_meta = std::fs::metadata(conf_dir)
            .map_err(|e| format!("读取配置目录属主失败（{conf_dir}）：{e}"))?;
        let uid = dir_meta.uid();
        let gid = dir_meta.gid();
        if should_skip_confdir_chown(uid) {
            return Err(format!(
                "配置目录异常归 root 所有，拒绝准备 cache.db：{conf_dir}"
            ));
        }
        let cache = Path::new(conf_dir).join(CHOWN_FILES[0]);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&cache)
            .map_err(|e| format!("安全打开 cache.db 失败（{}）：{e}", cache.display()))?;
        std::os::unix::fs::fchown(&file, Some(uid), Some(gid))
            .map_err(|e| format!("归还 cache.db 属主失败（{}）：{e}", cache.display()))
    }

    /// chownRuntimeDirs（`helper.go:206-242`）：把 confDir/{tailscale,singbox-dashboard,ui} 与根部
    /// cache.db 里仍属 root 的条目 `Lchown` 归还登录用户。尽力而为，单项失败即跳过，绝不阻断。
    fn chown_runtime_dirs(conf_dir: &str) {
        // helper.go:207-209: confDir 空 → 跳过
        if conf_dir.is_empty() {
            return;
        }
        // helper.go:210-218: Stat confDir 取属主 uid/gid（confDir=app 数据目录，属主即登录用户）
        let meta = match std::fs::metadata(conf_dir) {
            Ok(m) => m,
            Err(_) => return,
        };
        let uid = meta.uid();
        let gid = meta.gid();
        // helper.go:219-221: confDir 本身属 root（异常）→ 不动，避免把运行时目录误归 root
        if should_skip_confdir_chown(uid) {
            return;
        }
        // helper.go:222-224: 三个运行时子目录逐树归还
        for name in CHOWN_SUBDIRS {
            chown_tree(&Path::new(conf_dir).join(name), uid, gid);
        }
        for name in CHOWN_FILES {
            chown_tree(&Path::new(conf_dir).join(name), uid, gid);
        }
    }

    /// chownTree（`helper.go:230-242`）：递归把树里仍属 root 的条目 `Lchown` 到 `(uid,gid)`。
    ///
    /// 用 `symlink_metadata`（`Lstat` 语义，不跟随符号链接，对齐 Go `filepath.Walk` 的 Lstat）；条目
    /// 读不了即跳过（绝不中断遍历）；仅 `uid==0` 的条目才 Lchown（省无谓 Lchown，`helper.go:237`）。
    fn chown_tree(root: &Path, uid: u32, gid: u32) {
        let meta = match std::fs::symlink_metadata(root) {
            Ok(m) => m,
            Err(_) => return, // 目录不存在/读不了即跳过（helper.go:232）
        };
        // helper.go:235-239: 仅 root 写入的条目归还（Lchown 不跟随符号链接）
        if should_chown_entry(meta.uid()) {
            let _ = std::os::unix::fs::lchown(root, Some(uid), Some(gid));
        }
        // 递归子项（仅真目录，符号链接经 Lstat 判为非目录 → 不跟随，对齐 Walk）
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    chown_tree(&entry.path(), uid, gid);
                }
            }
        }
    }

    /// 建 socket（0666）+ accept 循环，每连接一线程 dispatch（`helper.go:598-642`）。
    ///
    /// 每连接：5s read deadline（`helper.go:401`）+ 独立线程 `process_connection`（`helper.go:641`
    /// `go handle(conn)`）+ `command_mu` 补 Go 单锁纪律。返回 Err 当 socket 监听失败。
    pub fn serve(services: Arc<DaemonServices>) -> std::io::Result<()> {
        let support = Path::new(&services.config.support_dir);
        // helper.go:598-601: MkdirAll(supportDir, 0755)
        std::fs::create_dir_all(support)?;
        // helper.go:603: 纵深防御 Chmod 0755（确保普通用户可穿越，否则 app 连 socket EACCES）
        let _ = std::fs::set_permissions(support, std::fs::Permissions::from_mode(0o755));

        let sock_path = support.join(SOCK_FILENAME);
        // helper.go:605: Remove 旧 socket（防「地址已在使用」）
        let _ = std::fs::remove_file(&sock_path);

        let listener = UnixListener::bind(&sock_path)?;
        // helper.go:611: Chmod 0666（token 为安全边界）
        let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(SOCK_MODE));

        // helper.go:636-642: accept 循环
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    // helper.go:401: 5s read deadline（set_read_timeout 取 &self）
                    let _ = s.set_read_timeout(Some(READ_TIMEOUT));
                    let svc = Arc::clone(&services);
                    // helper.go:641: go handle(conn)（每连接一线程）
                    std::thread::spawn(move || {
                        // reader=stream；writer=try_clone 副本（读写各一独立 fd）
                        let writer = match s.try_clone() {
                            Ok(w) => w,
                            Err(_) => return,
                        };
                        let _ = process_connection(
                            s,
                            writer,
                            svc.as_ref(),
                            svc.config(),
                            Some(svc.command_mu()),
                        );
                    });
                }
                Err(_) => continue, // helper.go:638-639: continue
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub use sys::{serve, DaemonServices};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::macos::handler::{ChildHandle, MacServices, SpawnError};
    use crate::token::TokenStore;
    use std::io::Cursor;
    use std::sync::Mutex;

    /// 测试用 services（复用 handler 测试的简化版）。
    struct TestServices {
        token: Mutex<String>,
        child: Mutex<Option<ChildHandle>>,
        uid: i64,
    }

    impl TestServices {
        fn new(token: &str) -> Self {
            Self {
                token: Mutex::new(token.into()),
                child: Mutex::new(None),
                uid: 0,
            }
        }
    }

    impl TokenStore for TestServices {
        fn token_value(&self) -> String {
            self.token.lock().unwrap().clone()
        }
    }

    impl crate::platform::macos::exec::CommandRunner for TestServices {
        fn run(
            &self,
            _t: Duration,
            _p: &str,
            _a: &[&str],
        ) -> Result<(), crate::platform::macos::exec::RunError> {
            Ok(())
        }
        fn output(
            &self,
            _t: Duration,
            _p: &str,
            _a: &[&str],
        ) -> Result<Vec<u8>, crate::platform::macos::exec::RunError> {
            Ok(Vec::new())
        }
        fn combined(
            &self,
            _t: Duration,
            _p: &str,
            _a: &[&str],
        ) -> Result<Vec<u8>, crate::platform::macos::exec::RunError> {
            Ok(Vec::new())
        }
    }

    impl MacServices for TestServices {
        fn token_store(&self) -> &dyn TokenStore {
            self
        }
        fn runner(&self) -> &dyn crate::platform::macos::exec::CommandRunner {
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
            let pid = 4242u32;
            *self.child.lock().unwrap() = Some(ChildHandle { pid });
            Ok(pid)
        }
    }

    fn test_config() -> MacConfig {
        MacConfig::new("/sb", "/Users/test/Polaris", "/tmp/support", "/tmp/core")
    }

    // ===== decode_request（对照 Go handle() readLine 序列）=====

    #[test]
    fn decode_ping() {
        let mut lines = std::iter::empty::<String>();
        assert_eq!(decode_request("ping", &mut lines), Ok(Request::Ping));
    }

    /// stop 的受管 pid 身份行：有则解出，无（旧客户端）则 `None`（旧语义）。
    ///
    /// 变异：把 `"stop"` 分支退回 `Ok(Request::Stop { pid: None })`（不读那一行）→ 首条转红，
    /// 身份判据从此永远拿不到 want = 形同虚设。
    #[test]
    fn decode_stop_reads_optional_identity_line() {
        let mut lines = vec!["4242".to_owned()].into_iter();
        assert_eq!(
            decode_request("stop", &mut lines),
            Ok(Request::Stop { pid: Some(4242) })
        );
        let mut none = std::iter::empty::<String>();
        assert_eq!(
            decode_request("stop", &mut none),
            Ok(Request::Stop { pid: None }),
            "旧客户端不发身份行 → None → 沿用「停当前受管核」"
        );
    }

    #[test]
    fn decode_freeport() {
        let mut lines = vec!["9090".to_owned()].into_iter();
        assert_eq!(
            decode_request("freeport", &mut lines),
            Ok(Request::FreePort { port: 9090 })
        );
    }

    #[test]
    fn decode_start_full() {
        let mut lines = vec![
            "/tmp/c.json".to_owned(),
            "/tmp/l.log".to_owned(),
            "1".to_owned(),
            "1234".to_owned(),
        ]
        .into_iter();
        let r = decode_request("start", &mut lines).unwrap();
        match r {
            Request::Start(p) => {
                assert_eq!(p.cfg, "/tmp/c.json");
                assert_eq!(p.log, "/tmp/l.log");
                assert!(p.fwd);
                assert_eq!(p.parent_pid, Some(1234));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decode_start_without_ppid() {
        // helper.go:513: ppid 可选（EOF → 0 → None）
        let mut lines = vec!["/tmp/c.json".to_owned(), "".to_owned(), "0".to_owned()].into_iter();
        let r = decode_request("start", &mut lines).unwrap();
        match r {
            Request::Start(p) => {
                assert_eq!(p.parent_pid, None);
                assert!(!p.fwd);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decode_route_add() {
        let mut lines = vec![
            "polaris-ts".to_owned(),
            "10.0.0.0/8,172.16.0.0/12".to_owned(),
        ]
        .into_iter();
        let r = decode_request("route-add", &mut lines).unwrap();
        match r {
            Request::RouteAdd(rp) => {
                assert_eq!(rp.iface, "polaris-ts");
                assert_eq!(rp.cidrs, vec!["10.0.0.0/8", "172.16.0.0/12"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decode_route_del() {
        let mut lines = vec!["utun3".to_owned(), "::/0".to_owned()].into_iter();
        let r = decode_request("route-del", &mut lines).unwrap();
        assert!(matches!(r, Request::RouteDel(_)));
    }

    #[test]
    fn decode_install_core() {
        let mut lines = vec!["/tmp/staging".to_owned(), "abcd".repeat(16)].into_iter();
        let r = decode_request("install-core", &mut lines).unwrap();
        match r {
            Request::InstallCore(p) => {
                assert_eq!(p.src_dir, "/tmp/staging");
                assert_eq!(p.want_hash.len(), 64);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decode_default_restore() {
        let mut lines = vec!["192.168.1.1".to_owned()].into_iter();
        let r = decode_request("default-restore", &mut lines).unwrap();
        match r {
            Request::DefaultRestore { gateway_ipv4 } => {
                assert_eq!(gateway_ipv4, "192.168.1.1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decode_flush_dns() {
        let mut lines = std::iter::empty::<String>();
        assert_eq!(
            decode_request("flush-dns", &mut lines),
            Ok(Request::FlushDns)
        );
    }

    #[test]
    fn decode_unknown_command() {
        let mut lines = std::iter::empty::<String>();
        assert_eq!(
            decode_request("bogus", &mut lines),
            Err(DecodeError::UnknownCommand("bogus".into()))
        );
    }

    // ===== process_connection 端到端（用 Cursor 模拟 wire 输入）=====

    #[test]
    fn process_conn_ping_success() {
        // 模拟客户端发：token\tping\n（实际是 token\nping\n）
        let svc = TestServices::new("TOK");
        let cfg = test_config();
        let input = Cursor::new(b"TOK\nping\n".to_vec());
        let mut output = Vec::new();
        let outcome = process_connection(input, &mut output, &svc, &cfg, None);
        assert!(matches!(outcome, ConnOutcome::Done));
        let resp = String::from_utf8(output).unwrap();
        assert_eq!(
            resp,
            format!(
                "OK pong uid=0 v{} build={}\n",
                crate::platform::macos::PROTO_VERSION,
                polaris_helper_proto::build_identity::current()
            )
        );
    }

    #[test]
    fn process_conn_auth_fail() {
        // 错 token → ERR auth
        let svc = TestServices::new("real");
        let cfg = test_config();
        let input = Cursor::new(b"wrong\nping\n".to_vec());
        let mut output = Vec::new();
        process_connection(input, &mut output, &svc, &cfg, None);
        assert_eq!(String::from_utf8(output).unwrap(), "ERR auth\n");
    }

    /// **未鉴权连接不得取到 `command_mu`** —— 行为级判据，不是「代码里有没有那句」。
    ///
    /// 做法：后台线程把锁**限时**攥住（800ms），主线程立刻喂一条错 token 的 `start`
    /// （且参数行缺失，正是最坏形态）并测耗时。实现若在鉴权前取锁，主线程要等到后台放锁；
    /// 实现正确则在取锁前就回 `ERR auth`。
    ///
    /// 限时持锁而非「主线程持锁 + 子线程跑」：后者在变异下会让 `thread::scope` 的 join 与
    /// 主线程的锁互等，表现为**挂死**而不是失败（实测撞过，CI 上只会看到超时）。
    #[test]
    fn unauthenticated_connection_never_takes_the_command_lock() {
        let svc = TestServices::new("real");
        let cfg = test_config();
        let mu = Mutex::new(());
        const HOLD: std::time::Duration = std::time::Duration::from_millis(800);

        std::thread::scope(|scope| {
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let mu_ref = &mu;
            scope.spawn(move || {
                let g = mu_ref.lock().unwrap();
                ready_tx.send(()).unwrap();
                std::thread::sleep(HOLD);
                drop(g);
            });
            ready_rx.recv().expect("持锁线程没起来");

            let t0 = std::time::Instant::now();
            let input = Cursor::new(b"wrong-token\nstart\n".to_vec());
            let mut output = Vec::new();
            process_connection(input, &mut output, &svc, &cfg, Some(&mu));
            let elapsed = t0.elapsed();

            assert_eq!(String::from_utf8(output).unwrap(), "ERR auth\n");
            assert!(
                elapsed < HOLD / 2,
                "未鉴权连接等在 command_mu 上（耗时 {elapsed:?}）—— 鉴权跑到取锁之后了"
            );
        });
    }

    /// 正向对照：**已鉴权**的同一类命令确实会去取锁 —— 否则上面那条可能被「谁都不取锁」满足。
    #[test]
    fn authenticated_connection_does_take_the_command_lock() {
        let svc = TestServices::new("real");
        let cfg = test_config();
        let mu = Mutex::new(());
        const HOLD: std::time::Duration = std::time::Duration::from_millis(800);

        std::thread::scope(|scope| {
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let mu_ref = &mu;
            scope.spawn(move || {
                let g = mu_ref.lock().unwrap();
                ready_tx.send(()).unwrap();
                std::thread::sleep(HOLD);
                drop(g);
            });
            ready_rx.recv().expect("持锁线程没起来");

            let t0 = std::time::Instant::now();
            let input = Cursor::new(b"real\nstatus\n".to_vec());
            let mut output = Vec::new();
            process_connection(input, &mut output, &svc, &cfg, Some(&mu));
            let elapsed = t0.elapsed();

            assert!(
                elapsed >= HOLD / 2,
                "已鉴权命令没去取 command_mu（耗时 {elapsed:?}）—— 锁形同虚设，上一条门也就没有信息量"
            );
        });
    }

    #[test]
    fn process_conn_version() {
        let svc = TestServices::new("T");
        let cfg = test_config();
        let input = Cursor::new(b"T\nversion\n".to_vec());
        let mut output = Vec::new();
        process_connection(input, &mut output, &svc, &cfg, None);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("OK {}\n", crate::platform::macos::PROTO_VERSION)
        );
    }

    #[test]
    fn process_conn_unknown_command() {
        let svc = TestServices::new("T");
        let cfg = test_config();
        let input = Cursor::new(b"T\nbogus\n".to_vec());
        let mut output = Vec::new();
        process_connection(input, &mut output, &svc, &cfg, None);
        assert_eq!(String::from_utf8(output).unwrap(), "ERR unknown\n");
    }

    #[test]
    fn process_conn_start_full_flow() {
        let svc = TestServices::new("T");
        let cfg = test_config();
        let input = Cursor::new(b"T\nstart\n/Users/test/Polaris/c.json\n\n0\n".to_vec());
        let mut output = Vec::new();
        let outcome = process_connection(input, &mut output, &svc, &cfg, None);
        assert!(matches!(outcome, ConnOutcome::Done));
        // spawn_child mock 返回 4242
        assert_eq!(String::from_utf8(output).unwrap(), "OK started 4242\n");
    }

    #[test]
    fn process_conn_empty_input_no_panic() {
        // 无数据连接不应 panic
        let svc = TestServices::new("T");
        let cfg = test_config();
        let input = Cursor::new(Vec::new());
        let mut output = Vec::new();
        let outcome = process_connection(input, &mut output, &svc, &cfg, None);
        assert!(matches!(outcome, ConnOutcome::Done));
        assert!(output.is_empty());
    }

    #[test]
    fn process_conn_partial_input_no_panic() {
        // 只有 token 行，无 command 行
        let svc = TestServices::new("T");
        let cfg = test_config();
        let input = Cursor::new(b"T\n".to_vec());
        let mut output = Vec::new();
        let outcome = process_connection(input, &mut output, &svc, &cfg, None);
        assert!(matches!(outcome, ConnOutcome::Done));
    }

    #[test]
    fn sock_filename_matches_go() {
        // helper.go:604
        assert_eq!(SOCK_FILENAME, "helper.sock");
    }

    #[cfg(unix)]
    #[test]
    fn sock_mode_matches_go() {
        // helper.go:611: Chmod 0666
        assert_eq!(SOCK_MODE, 0o666);
    }

    // ===== 纯决策逻辑（mu 分流 / chown 筛选 / terminate 状态机）=====

    #[test]
    fn should_lock_all_but_freeport() {
        // helper.go:413: freeport 不持锁；helper.go:418: 其余持锁
        assert!(!should_lock_command("freeport"));
        for c in [
            "ping",
            "version",
            "status",
            "start",
            "stop",
            "cleanup",
            "route-add",
            "flush-dns",
        ] {
            assert!(should_lock_command(c), "{c} 应持锁");
        }
    }

    #[test]
    fn chown_subdirs_match_go() {
        // helper.go:222
        assert_eq!(CHOWN_SUBDIRS, ["tailscale", "singbox-dashboard", "ui"]);
        assert_eq!(CHOWN_FILES, ["cache.db"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cache_prepare_preserves_bytes_and_uses_confdir_owner() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CHOWN_FILES[0]);
        std::fs::write(&cache, b"keep-cache-bytes").unwrap();

        sys::prepare_cache_for_user(dir.path().to_str().unwrap()).unwrap();

        assert_eq!(std::fs::read(&cache).unwrap(), b"keep-cache-bytes");
        let dir_meta = std::fs::metadata(dir.path()).unwrap();
        let cache_meta = std::fs::metadata(&cache).unwrap();
        assert_eq!(cache_meta.uid(), dir_meta.uid());
        assert_eq!(cache_meta.gid(), dir_meta.gid());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cache_prepare_refuses_symlink_without_touching_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"untouched").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(CHOWN_FILES[0])).unwrap();

        let error = sys::prepare_cache_for_user(dir.path().to_str().unwrap()).unwrap_err();

        assert!(error.contains("安全打开 cache.db 失败"), "{error}");
        assert_eq!(std::fs::read(target).unwrap(), b"untouched");
    }

    #[test]
    fn should_chown_only_root_owned_entries() {
        // helper.go:237: st.Uid==0 → Lchown；非 root 跳过
        assert!(should_chown_entry(0));
        assert!(!should_chown_entry(501));
        assert!(!should_chown_entry(1000));
    }

    #[test]
    fn should_skip_confdir_chown_when_root() {
        // helper.go:219-221: confDir 属 root（异常）→ 跳过整个 chown
        assert!(should_skip_confdir_chown(0));
        assert!(!should_skip_confdir_chown(501));
    }

    #[test]
    fn terminate_kills_only_when_not_exited() {
        // helper.go:282-286: done 触发（已退出）→ 免 KILL；超时（未退出）→ SIGKILL
        assert!(terminate_needs_kill(false));
        assert!(!terminate_needs_kill(true));
    }
}
