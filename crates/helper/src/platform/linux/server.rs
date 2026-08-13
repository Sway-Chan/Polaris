//! Server 主循环 —— 移植自 上游 `helper-linux/main.go`。
//!
//! ## 流程（逐行对照 Go 源 main()）
//!
//! 1. 解析 flags：socket / authfile / coredir / console（:17-22）。
//! 2. 建 socket 目录（0755，任意登录用户可穿越）+ 删旧 socket（:25-31）。
//! 3. Listen(unix) + chmod socket 0666（:33-38）—— socket 本身 0666 + SO_PEERCRED + 授权列表把关。
//! 4. SIGTERM/SIGINT 收割器：先收割 child sing-box，等在途后台收割，复位转发态，退出（:42-58）。
//! 5. Accept 循环：每连接 go handle(conn)（:63-69）。
//!
//! ## Rust 移植
//!
//! Go 源的 socket 循环 + handle 分发是同步多 goroutine。本实现提供：
//! - [`ServerConfig`]：flags 的类型化等价（socket/authfile/coredir 三路径 + console 标记）。
//! - [`prepare_socket`]：建目录 + 删旧 socket + bind + chmod（纯逻辑，可单测）。
//! - [`ss_lookup`]：freeport 的 ss 子进程封装。
//!
//! ## C6-2 提权心脏（本批落地）
//!
//! [`AmbientCapsSpawner`] 是真实的 fork+setuid+AmbientCaps 拉核（替换 C6-0 的 `NotImplementedSpawner` 桩）：
//! `Command` + `pre_exec`（`set_keepcaps` → `setgroups`/`setgid`/`setuid` 降权 → raise Inheritable/Ambient
//! CAP_NET_ADMIN/RAW/BIND_SERVICE），log 重定向 + chown 到对端 uid，收割线程收尸 + 清 state，父死看护
//! （watchParent），terminate（TERM→≤5s→KILL）+ reapWG 退出兜底。**pre_exec 后 fork+execve 链、真降权
//! 拉核为真机门**（本机绝不跑，见 [`super`] 模块文档「关键地雷」段）；纯逻辑（caps 集/terminate 决策/
//! watchParent 决策/ChildSlot 协调）本文件单测覆盖。
//!
//! [`ConnServer`] 把 accept 到的 tokio `UnixStream` 转同步 [`LineConn`](crate::platform::linux::handler::LineConn)
//! （5s 读超时 + 捕获 SO_PEERCRED），交给同步 [`handle`](crate::platform::linux::handle)，对应 Go 的
//! `for { conn := l.Accept(); go handle(conn) }`。

#![allow(unsafe_code)] // 唯一 unsafe 点 = CommandExt::pre_exec（fork 后子进程降权拉核，见 attach_privilege_drop 的 SAFETY）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use crate::platform::linux::ops::set_forward_prod;
use crate::platform::linux::state::{
    CoreHandle, CoreSpawner, HandlerState, SpawnCoreRequest, SpawnError,
};

/// 默认 socket 路径（移植自 Go flag default `/run/polaris/helper.sock`，:18）。
pub const DEFAULT_SOCK_PATH: &str = "/run/polaris/helper.sock";
/// 默认授权 uid 列表文件（Go default `/var/lib/polaris/authorized-uids`，:19）。
pub const DEFAULT_AUTH_FILE: &str = "/var/lib/polaris/authorized-uids";
/// 默认锁定的 root-owned 受管核目录（Go default `/usr/local/lib/polaris/core`，:20）。
pub const DEFAULT_CORE_DIR: &str = "/usr/local/lib/polaris/core";

/// server 配置（flags 的类型化等价，对照 Go main 的 flag.String/flag.Bool）。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// unix socket 路径。
    pub sock_path: PathBuf,
    /// 授权 uid 列表文件。
    pub auth_file: PathBuf,
    /// 锁定的 root-owned 受管核目录（None = install-core 报 coredir-unset）。
    pub core_dir: Option<PathBuf>,
    /// 前台运行（开发/测试，systemd 不要求）。
    pub console: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            sock_path: PathBuf::from(DEFAULT_SOCK_PATH),
            auth_file: PathBuf::from(DEFAULT_AUTH_FILE),
            core_dir: Some(PathBuf::from(DEFAULT_CORE_DIR)),
            console: false,
        }
    }
}

/// socket bind 失败的错误（对照 Go main 的 `fmt.Fprintln(os.Stderr, err); os.Exit(1)`，:35-37）。
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// socket 目录创建失败。
    #[error("mkdir socket dir {dir:?}: {source}")]
    Mkdir {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// unix socket bind 失败。
    #[error("listen {path:?}: {source}")]
    Listen {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// chmod socket 失败。
    #[error("chmod {path:?}: {source}")]
    Chmod {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// 准备 socket：建目录 0755 + 删旧 socket + bind + chmod 0666（移植自 Go :25-38）。
///
/// 返回绑定好的 std UnixListener（同步，对齐 Go `net.Listen`）。生产 accept 循环在 async 上下文中
/// 经 `tokio::net::UnixListener::from_std` + `set_nonblocking(true)` 转换为 tokio listener。
/// 这样 bind/chmod 不依赖 tokio reactor，单元测试可在同步上下文直接验证。
pub fn prepare_socket(cfg: &ServerConfig) -> Result<std::os::unix::net::UnixListener, ServerError> {
    // :25-30: socket 目录必须 0755（任意登录用户可穿越 → app 才能连）。
    let sock_dir = cfg.sock_path.parent().unwrap_or_else(|| Path::new("/"));
    std::fs::create_dir_all(sock_dir).map_err(|source| ServerError::Mkdir {
        dir: sock_dir.to_path_buf(),
        source,
    })?;
    set_mode(sock_dir, 0o755).map_err(|source| ServerError::Chmod {
        path: sock_dir.to_path_buf(),
        source,
    })?;
    // :31: 删旧 socket（bind 失败 otherwise）。
    let _ = std::fs::remove_file(&cfg.sock_path);

    // :33-37: Listen(unix) —— std 同步 bind（对齐 Go net.Listen）。
    let listener = std::os::unix::net::UnixListener::bind(&cfg.sock_path).map_err(|source| {
        ServerError::Listen {
            path: cfg.sock_path.clone(),
            source,
        }
    })?;

    // :38: chmod socket 0666（SO_PEERCRED + 授权列表把关，socket 本身可连）。
    set_mode(&cfg.sock_path, 0o666).map_err(|source| ServerError::Chmod {
        path: cfg.sock_path.clone(),
        source,
    })?;

    Ok(listener)
}

/// 设文件/dir 权限（unix only）。
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// ss 命令的输出提供者（freeport 用，对照 Go `exec.Command("ss", "-H", "-ltnp", ...)`，:292）。
///
/// 生产实现：调 `ss` 子进程；失败返回 None（freeport 视作端口空闲）。
pub fn ss_lookup(port: &str) -> Option<String> {
    let sport = format!("sport = :{port}");
    let out = std::process::Command::new("ss")
        .args(["-H", "-ltnp", &sport])
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

/// IP 转发开关闭包（生产 = set_forward_prod，对齐 Go setForward）。
pub fn forward_fn() -> fn(bool) {
    set_forward_prod
}

// ===== L16 AmbientCaps 常量 + 集（对照 Go `helper-linux/helper.go:51-55,441`）=====

/// `capNetBindService`（Go :52）—— `caps::Capability::CAP_NET_BIND_SERVICE.index()` 恒等于此。
pub const CAP_NET_BIND_SERVICE_NUM: u8 = 10;
/// `capNetAdmin`（Go :53）。
pub const CAP_NET_ADMIN_NUM: u8 = 12;
/// `capNetRaw`（Go :54）。
pub const CAP_NET_RAW_NUM: u8 = 13;

/// start 拉核授予的 ambient capability 集（顺序对照 Go `AmbientCaps: []uintptr{capNetAdmin, capNetRaw,
/// capNetBindService}`，:441 —— 与现役 setcap 授权一致，不推测削减）。
#[must_use]
pub fn ambient_caps() -> [caps::Capability; 3] {
    [
        caps::Capability::CAP_NET_ADMIN,
        caps::Capability::CAP_NET_RAW,
        caps::Capability::CAP_NET_BIND_SERVICE,
    ]
}

// ===== terminate / watchParent 纯决策（可测；对照 Go terminateChild / watchParent）=====

/// terminate 宽限期（Go terminateChild：TERM → 等 ≤5s → KILL，:253）。
pub const TERMINATE_GRACE_SECS: u64 = 5;
/// watchParent 轮询周期（Go: `time.NewTicker(time.Second)`，:259）。
pub const WATCH_PARENT_INTERVAL_SECS: u64 = 1;

/// terminate 决策（对照 Go `terminateChild`，:246-256）：先 TERM，等退出；期限内退出则**不** KILL，
/// 超时才 KILL。抽象三原语便于单测（不发真信号）。`wait_exited` 返回是否在期限内退出。
fn terminate_child<S, W, K>(send_term: S, wait_exited: W, send_kill: K)
where
    S: FnOnce(),
    W: FnOnce() -> bool,
    K: FnOnce(),
{
    send_term(); // Go: c.Process.Signal(SIGTERM)
    if !wait_exited() {
        // Go: case <-time.After(5s): c.Process.Kill()
        send_kill();
    }
}

/// watchParent 单 tick 决策（对照 Go `watchParent` 循环体，:267-283）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchStep {
    /// 父仍活 + 仍是当前 child → 继续下一 tick。
    Continue,
    /// 已非当前 child（被 stop/cleanup 摘除或自然退出）→ 停看护（Go: `if !current { return }`）。
    Stop,
    /// 父已死（`kill(ppid,0)==ESRCH`）→ 摘 child + terminate（Go: :273-282）。
    ParentDead,
}

/// 由「仍是当前 child」+「父仍活」推出 watchParent 决策（纯逻辑，短路顺序对照 Go：先判 current 再判父存活）。
#[must_use]
pub fn watch_parent_step(still_current_child: bool, parent_alive: bool) -> WatchStep {
    if !still_current_child {
        WatchStep::Stop
    } else if !parent_alive {
        WatchStep::ParentDead
    } else {
        WatchStep::Continue
    }
}

// ===== 信号原语（nix safe wrapper；forbid(unsafe) 下替代 libc::kill）=====

/// u32 pid → nix Pid（真实 pid ≤ PID_MAX≈4M，恒 ≤ i32::MAX；越界退化 i32::MAX 仅防御性）。
fn to_pid(pid: u32) -> nix::unistd::Pid {
    nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap_or(i32::MAX))
}

fn send_signal(pid: u32, sig: nix::sys::signal::Signal) -> nix::Result<()> {
    nix::sys::signal::kill(to_pid(pid), sig)
}

/// 父进程是否存活（对照 Go `syscall.Kill(ppid, 0) == ESRCH`，:273）。signal 0 仅探活不投递。
fn parent_alive(ppid: u32) -> bool {
    nix::sys::signal::kill(to_pid(ppid), None).is_ok()
}

// ===== child 退出协调槽（对应 Go `childDone chan struct{}`）=====

/// 单个 child 的退出协调槽：收割线程 `child.wait()` 收尸后 [`mark_exited`](ChildSlot::mark_exited) 唤醒
/// 等待中的 terminate（TERM 后据此决定是否 KILL），并让 watchParent 的 tick 等待可提前结束。
///
/// **顺序不变式**：收割线程必须先 `mark_exited` 再去拿 `HandlerState` 锁清 child —— 否则与「持 state 锁
/// 调 terminate 并等 `wait_exited`」的 handler 线程互等死锁。
struct ChildSlot {
    pid: u32,
    exited: Mutex<bool>,
    cv: Condvar,
}

impl ChildSlot {
    fn new(pid: u32) -> Arc<Self> {
        Arc::new(Self {
            pid,
            exited: Mutex::new(false),
            cv: Condvar::new(),
        })
    }

    fn mark_exited(&self) {
        let mut g = self.exited.lock().unwrap_or_else(PoisonError::into_inner);
        *g = true;
        self.cv.notify_all();
    }

    /// 等退出，最多 `timeout`；返回是否在期限内退出（对应 Go `select{<-done; <-time.After(...)}`）。
    fn wait_exited(&self, timeout: Duration) -> bool {
        let g = self.exited.lock().unwrap_or_else(PoisonError::into_inner);
        let (g, _) = self
            .cv
            .wait_timeout_while(g, timeout, |exited| !*exited)
            .unwrap_or_else(PoisonError::into_inner);
        *g
    }
}

/// 对某 child 执行 terminate（TERM→≤5s→KILL，经 slot 协调，pid-复用安全：slot 存活期间才发信号）。
fn terminate_slot(slot: &ChildSlot) {
    let grace = Duration::from_secs(TERMINATE_GRACE_SECS);
    terminate_child(
        || {
            let _ = send_signal(slot.pid, nix::sys::signal::Signal::SIGTERM);
        },
        || slot.wait_exited(grace),
        || {
            let _ = send_signal(slot.pid, nix::sys::signal::Signal::SIGKILL);
        },
    );
}

// ===== reapWG：在途后台 terminate 计数（对应 Go `reapWG sync.WaitGroup`，L15）=====

/// SIGTERM 退出前须等在途后台 terminate 跑完 KILL 升级，杜绝留下带 CAP_NET_ADMIN 的孤儿核。
struct ReapGroup {
    count: Mutex<usize>,
    cv: Condvar,
}

impl ReapGroup {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            count: Mutex::new(0),
            cv: Condvar::new(),
        })
    }
    fn add(&self) {
        *self.count.lock().unwrap_or_else(PoisonError::into_inner) += 1;
    }
    fn done(&self) {
        let mut g = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        *g = g.saturating_sub(1);
        if *g == 0 {
            self.cv.notify_all();
        }
    }
    /// 等在途 terminate 归零，最多 `timeout`（对照 Go `waitReaps`，`main.go:73-80`）。
    fn wait(&self, timeout: Duration) {
        let g = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = self
            .cv
            .wait_timeout_while(g, timeout, |c| *c > 0)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

// ===== L10 真 CoreSpawner：fork + setuid + AmbientCaps 拉核 =====

/// 生产 spawner（对照 Go start 分支 `c.SysProcAttr = ...; c.Start()`，:431-478）。
///
/// 持共享 [`HandlerState`]（收割/看护线程清 child）+ 在途 child 槽（terminate 按 pid 查）+ reapWG。
pub struct AmbientCapsSpawner {
    state: Arc<Mutex<HandlerState>>,
    /// 在途 child 槽（至多 1 个：start 见 running 回 already）。收割后移除。
    slots: Arc<Mutex<Vec<Arc<ChildSlot>>>>,
    /// 在途后台 terminate 计数（SIGTERM 退出前 waitReaps 等它归零）。
    reaps: Arc<ReapGroup>,
}

impl AmbientCapsSpawner {
    #[must_use]
    pub fn new(state: Arc<Mutex<HandlerState>>) -> Self {
        Self {
            state,
            slots: Arc::new(Mutex::new(Vec::new())),
            reaps: ReapGroup::new(),
        }
    }

    fn find_slot(&self, pid: u32) -> Option<Arc<ChildSlot>> {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|s| s.pid == pid)
            .cloned()
    }

    /// 等在途后台 terminate 归零（对照 Go `waitReaps`）。SIGTERM 退出兜底用。
    pub fn wait_reaps(&self, timeout: Duration) {
        self.reaps.wait(timeout);
    }

    /// 同步 terminate 指定 child（SIGTERM 退出兜底对当前 child 用，Go main reaper 同步 `terminateChild`）。
    fn terminate_now(&self, pid: u32) {
        if let Some(slot) = self.find_slot(pid) {
            terminate_slot(&slot);
        }
    }
}

impl CoreSpawner for AmbientCapsSpawner {
    fn spawn(&self, req: &SpawnCoreRequest) -> Result<CoreHandle, SpawnError> {
        // Go: exec.Command(coreBin(), "run", "-c", cfg)（:431）。
        let mut cmd = std::process::Command::new(&req.binary);
        cmd.arg("run").arg("-c").arg(&req.config);

        // CWD = 配置文件所在目录（= 用户可写 config 目录）：helper daemon（systemd）CWD=`/`，spawn 的核继承 `/`
        // → dashboard 下载兜底相对 mkdir `/dashboard` 只读失败噪音。设为可写目录即消。std 在 fork 后、pre_exec
        // 降权闭包**之前** chdir（此刻仍 root，可 chdir 任意目录），降权后核 CWD = 用户目录，两不冲突。
        // Polaris 生成的核配置其余路径全绝对，不受 CWD 影响。取不到父目录（极端形态）则不设，继承旧行为。
        if let Some(cwd) = req.config.parent() {
            cmd.current_dir(cwd);
        }

        // Go :443-451：log 重定向 + chown 到对端 uid。开失败 → 不重定向（继承 helper，Go 同）。
        if let Some(log) = &req.log {
            use std::os::unix::fs::OpenOptionsExt;
            // Go: os.OpenFile(logPath, O_CREATE|O_WRONLY|O_APPEND, 0o644)。
            if let Ok(f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o644)
                .open(log)
            {
                // Go: lf.Chown(int(cred.Uid), int(cred.Gid))（best-effort）。fchown = 对 fd（非 path）。
                let _ = std::os::unix::fs::fchown(&f, Some(req.uid), Some(req.gid));
                if let Ok(dup) = f.try_clone() {
                    cmd.stdout(std::process::Stdio::from(dup));
                }
                cmd.stderr(std::process::Stdio::from(f));
            }
        }

        // Go :434-442：SysProcAttr{Credential{Uid,Gid,Groups}, AmbientCaps}。降权+ambient 经 pre_exec 装。
        attach_privilege_drop(&mut cmd, req.uid, req.gid, req.groups.clone());

        // Go :452：c.Start()。失败 → ERR start（转发态复位由 handler 负责，对照 :456）。
        let child = cmd.spawn().map_err(|e| SpawnError::Spawn {
            detail: e.to_string(),
        })?;
        let pid = child.id();

        let slot = ChildSlot::new(pid);
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::clone(&slot));

        // Go :466-474：收割 goroutine —— c.Wait() 收尸 → close(done) → 清 child（若仍是本 child）。
        spawn_reaper(
            child,
            pid,
            Arc::clone(&self.state),
            Arc::clone(&slot),
            Arc::clone(&self.slots),
        );

        // Go :475-477：if ppid>0 { go watchParent(ppid, c, done) }。
        if let Some(ppid) = req.parent_pid {
            spawn_watch_parent(ppid, pid, Arc::clone(&self.state), Arc::clone(&slot));
        }

        // Go :478：OK started <pid>（wire 由 handler 拼）。
        Ok(CoreHandle { pid })
    }

    fn terminate(&self, h: &CoreHandle) {
        // Go stop：`reapWG.Add(1); go terminateChild(c, done)`（:375-376）—— 后台 TERM→≤5s→KILL，
        // stop 立即回复、**不持 state 锁 5s**。无槽（已收割）= no-op（防 pid 复用误杀，对齐 slot 协调）。
        let Some(slot) = self.find_slot(h.pid) else {
            return;
        };
        let reaps = Arc::clone(&self.reaps);
        reaps.add();
        std::thread::spawn(move || {
            terminate_slot(&slot);
            reaps.done();
        });
    }

    fn kill(&self, h: &CoreHandle) {
        // Go cleanup：child.Process.Kill()（SIGKILL 即时；收割线程随后收尸，:383）。
        let _ = send_signal(h.pid, nix::sys::signal::Signal::SIGKILL);
    }
}

/// 装 pre_exec 降权+ambient 拉核闭包（**唯一 unsafe 点** = `CommandExt::pre_exec`）。
///
/// 所有可分配的输入（gids/caps 列表）在 fork **前**于父进程算好，闭包本体只做 syscall。
fn attach_privilege_drop(cmd: &mut std::process::Command, uid: u32, gid: u32, groups: Vec<u32>) {
    use std::os::unix::process::CommandExt;
    let gids: Vec<nix::unistd::Gid> = groups.into_iter().map(nix::unistd::Gid::from_raw).collect();
    let caps = ambient_caps();
    // SAFETY: pre_exec 闭包在 fork 后、execve 前于**子进程**运行，仅调 async-signal-safe 的 syscall
    //   （set_keepcaps/setgroups/setgid/setuid + capset/prctl via caps crate），且 gids/caps 列表已在
    //   fork 前于父进程分配 → 闭包本体不分配。每步失败即返 Err 中止 execve（**fail-closed**：setuid 失败
    //   绝不以 root 拉核）。残留风险：caps crate 内部 capget 可能分配 —— 与「真降权拉核链」同属真机门
    //   （DESIGN-REVIEW(preexec-async-signal-safety)），本机绝不跑。
    unsafe {
        cmd.pre_exec(move || apply_privilege_drop(uid, gid, &gids, &caps));
    }
}

/// pre_exec 闭包本体：降权到对端登录用户 + raise ambient caps（全 safe wrapper，无 unsafe 块）。
///
/// 顺序对照 Go runtime 对 `SysProcAttr{Credential, AmbientCaps}` 的编排（keepcaps→降权→raise ambient）：
/// 1. `set_keepcaps(true)`：permitted caps 跨 setuid 存活（Go AmbientCaps 非空时 PR_SET_KEEPCAPS）。
/// 2. `setgroups`：补充组（须在 setuid 前、仍 root 时；空 = 清空，对齐 Go `Groups: nil`）。
/// 3. `setgid` → 4. `setuid`：降权（drop 放最后）。
/// 5. raise Inheritable + 6. raise Ambient（逐 cap；降权后 permitted 已留，加 inheritable 再抬 ambient）。
///
/// 任一步失败即 `Err` → std 中止 execve → `c.spawn()` 返错 → handler 回 `ERR start`（fail-closed）。
fn apply_privilege_drop(
    uid: u32,
    gid: u32,
    groups: &[nix::unistd::Gid],
    caps: &[caps::Capability],
) -> std::io::Result<()> {
    use nix::unistd::{setgid, setgroups, setuid, Gid, Uid};
    caps::securebits::set_keepcaps(true).map_err(std::io::Error::other)?;
    setgroups(groups).map_err(std::io::Error::other)?;
    setgid(Gid::from_raw(gid)).map_err(std::io::Error::other)?;
    setuid(Uid::from_raw(uid)).map_err(std::io::Error::other)?;
    for &cap in caps {
        caps::raise(None, caps::CapSet::Inheritable, cap).map_err(std::io::Error::other)?;
        caps::raise(None, caps::CapSet::Ambient, cap).map_err(std::io::Error::other)?;
    }
    Ok(())
}

/// 收割线程（Go reaper goroutine，:466-474）。owns Child → `wait()` 收尸（防僵尸）。
fn spawn_reaper(
    mut child: std::process::Child,
    pid: u32,
    state: Arc<Mutex<HandlerState>>,
    slot: Arc<ChildSlot>,
    slots: Arc<Mutex<Vec<Arc<ChildSlot>>>>,
) {
    std::thread::spawn(move || {
        let _ = child.wait(); // Go: _ = c.Wait()
                              // 顺序不变式：先唤醒等待中的 terminate（不需 state 锁），再拿 state 锁清 child——杜绝与
                              // 「持 state 锁调 terminate 等 wait_exited」的 handler 线程死锁。
        slot.mark_exited(); // Go: close(done)
        {
            let mut g = state.lock().unwrap_or_else(PoisonError::into_inner);
            if g.child.as_ref().map(|h| h.pid) == Some(pid) {
                g.child = None; // Go: if child == c { child, childDone = nil, nil }
            }
        }
        slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|s| s.pid != pid);
    });
}

/// 父死看护线程（Go `watchParent`，:258-285）。每 1s tick：child 退出→停；非当前 child→停；父死→摘+terminate。
fn spawn_watch_parent(ppid: u32, pid: u32, state: Arc<Mutex<HandlerState>>, slot: Arc<ChildSlot>) {
    std::thread::spawn(move || {
        let interval = Duration::from_secs(WATCH_PARENT_INTERVAL_SECS);
        loop {
            // Go: select{<-done; <-t.C}。以 slot.wait_exited 折叠「等 1s 或 child 已退出」：
            // 已退出（true）→ 停看护（Go: <-done → return）；超时（false）→ 走 tick 检查。
            if slot.wait_exited(interval) {
                return;
            }
            // Go: current := (child == c)。仍是当前 child 才继续（否则被 stop/cleanup 摘除）。
            let still_current = {
                let g = state.lock().unwrap_or_else(PoisonError::into_inner);
                g.child.as_ref().map(|h| h.pid) == Some(pid)
            };
            // 短路顺序对齐 Go：仅当仍是当前 child 才探父存活（parent_alive 是一次 kill(0) syscall）。
            let alive = still_current && parent_alive(ppid);
            match watch_parent_step(still_current, alive) {
                WatchStep::Continue => {}
                WatchStep::Stop => return,
                WatchStep::ParentDead => {
                    // Go :274-280：摘 child（若仍是本 child）。
                    {
                        let mut g = state.lock().unwrap_or_else(PoisonError::into_inner);
                        if g.child.as_ref().map(|h| h.pid) == Some(pid) {
                            g.child = None;
                        }
                    }
                    // Go :281：terminateChild(c, done)（不持 state 锁）。
                    terminate_slot(&slot);
                    return;
                }
            }
        }
    });
}

// ===== 生产连接服务（accept 循环 → handle）=====

/// 跨连接共享的 daemon 服务：持 [`HandlerState`] + [`AmbientCapsSpawner`] + 生产 deps，把 accept 到的
/// tokio `UnixStream` 交给同步 [`handle`](crate::platform::linux::handle)（Go `for { l.Accept(); go handle }`）。
pub struct ConnServer {
    state: Arc<Mutex<HandlerState>>,
    spawner: Arc<AmbientCapsSpawner>,
    freeport: crate::platform::linux::freeport::ProdFreePortDeps,
    systemd: crate::platform::linux::ops::TokioSystemd,
    core_dir: Option<PathBuf>,
    auth_file: PathBuf,
}

impl ConnServer {
    /// 由 [`ServerConfig`] 建服务（单一共享 state + spawner）。
    #[must_use]
    pub fn new(cfg: &ServerConfig) -> Arc<Self> {
        let state = Arc::new(Mutex::new(HandlerState::new()));
        let spawner = Arc::new(AmbientCapsSpawner::new(Arc::clone(&state)));
        Arc::new(Self {
            state,
            spawner,
            freeport: crate::platform::linux::freeport::ProdFreePortDeps,
            systemd: crate::platform::linux::ops::TokioSystemd,
            core_dir: cfg.core_dir.clone(),
            auth_file: cfg.auth_file.clone(),
        })
    }

    /// 处理一个连接（Go: `go handle(conn)`）。捕获 SO_PEERCRED → 转 std 阻塞流（5s 读超时）→
    /// spawn_blocking 跑同步 handle（含 fork 拉核，不占 async worker）。
    pub fn dispatch(self: &Arc<Self>, stream: tokio::net::UnixStream) {
        use crate::platform::linux::auth::{CapturedPeerCred, PeerCredProvider, TokioPeerCred};
        use crate::platform::linux::handler::{handle, HandlerDeps, LineConn, READ_TIMEOUT_SECS};

        // 1. 先取 SO_PEERCRED（转 std 后原流被消费，无法再取）。失败 → 捕获 None → handle 回 ERR peercred。
        let cred = TokioPeerCred::new(&stream).peer_cred();
        // 2. 转 std 阻塞流 + 5s 读超时。Go SetReadDeadline 是**连接级绝对**期限，std set_read_timeout 是
        //    **每次读** SO_RCVTIMEO —— 行数上界固定（start ≤6 行），DESIGN-REVIEW(read-deadline-per-read)，
        //    且 socket 已受授权 uid 门限，差异可控。
        let std_stream = match stream.into_std() {
            Ok(s) => s,
            Err(_) => return,
        };
        if std_stream.set_nonblocking(false).is_err() {
            return;
        }
        let _ = std_stream.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)));

        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let peer = CapturedPeerCred(cred);
            let ss_fn = ss_lookup;
            let fwd_fn = set_forward_prod;
            let ss: &(dyn Fn(&str) -> Option<String> + Send + Sync) = &ss_fn;
            let fwd: &(dyn Fn(bool) + Send + Sync) = &fwd_fn;
            let deps = HandlerDeps {
                core_dir: this.core_dir.as_deref(),
                auth_file: &this.auth_file,
                peer_cred: &peer,
                spawner: this.spawner.as_ref(),
                freeport_deps: &this.freeport,
                systemd: &this.systemd,
                ss_provider: ss,
                set_forward: fwd,
            };
            let state: &Mutex<HandlerState> = &this.state;
            let mut conn = LineConn::new(std_stream);
            handle(state, &deps, &mut conn);
        });
    }

    /// SIGTERM 退出兜底（Go main reaper，`main.go:46-54`）：同步 terminate 当前 child（TERM→≤5s→KILL）+
    /// waitReaps 等在途后台 terminate 归零。调用方随后 `set_forward_prod(false)`（对齐 `main.go:55`）。
    pub fn reap_on_shutdown(&self) {
        let child = {
            let mut g = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            g.child.take()
        };
        if let Some(h) = child {
            self.spawner.terminate_now(h.pid); // 同步（Go main 里 terminateChild 是同步调用）。
        }
        self.spawner.wait_reaps(Duration::from_secs(6));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;

    #[test]
    fn default_config_paths_match_design() {
        // 锁住默认路径（运维契约：systemd unit 文件、pkexec 安装器依赖这些路径）。
        let cfg = ServerConfig::default();
        assert_eq!(cfg.sock_path, PathBuf::from("/run/polaris/helper.sock"));
        assert_eq!(
            cfg.auth_file,
            PathBuf::from("/var/lib/polaris/authorized-uids")
        );
        assert_eq!(
            cfg.core_dir,
            Some(PathBuf::from("/usr/local/lib/polaris/core"))
        );
        assert!(!cfg.console);
    }

    #[test]
    fn default_constants_match_strings() {
        // 常量是 wire/运维契约（改名 = 断 systemd unit 引用）。
        assert_eq!(DEFAULT_SOCK_PATH, "/run/polaris/helper.sock");
        assert_eq!(DEFAULT_AUTH_FILE, "/var/lib/polaris/authorized-uids");
        assert_eq!(DEFAULT_CORE_DIR, "/usr/local/lib/polaris/core");
    }

    #[test]
    fn server_config_can_override_paths() {
        let cfg = ServerConfig {
            sock_path: PathBuf::from("/tmp/test.sock"),
            auth_file: PathBuf::from("/tmp/auth"),
            core_dir: None,
            console: true,
        };
        assert_eq!(cfg.sock_path, PathBuf::from("/tmp/test.sock"));
        assert!(cfg.core_dir.is_none());
        assert!(cfg.console);
    }

    #[test]
    fn server_config_clone_is_deep_copy() {
        let cfg = ServerConfig::default();
        let cfg2 = cfg.clone();
        assert_eq!(cfg.sock_path, cfg2.sock_path);
        assert_eq!(cfg.auth_file, cfg2.auth_file);
    }

    #[test]
    fn prepare_socket_creates_dir_and_binds() {
        // 真实 socket bind（不碰宿主：用 tempdir，非 /run）。
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cfg = ServerConfig {
            sock_path: sock.clone(),
            auth_file: dir.path().join("auth"),
            core_dir: None,
            console: false,
        };
        let listener = prepare_socket(&cfg).expect("prepare_socket 应成功");
        // socket 文件已创建。
        assert!(sock.exists(), "socket 文件应被创建");
        // 权限 0666（socket 本身可连）。
        use std::os::unix::fs::MetadataExt;
        let mode = std::fs::metadata(&sock).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o666, "socket 应 chmod 0666");
        // 目录权限 0755。
        let dir_mode = std::fs::metadata(dir.path()).unwrap().mode() & 0o777;
        assert_eq!(dir_mode, 0o755, "socket 目录应 0755");
        // listener 可用（drop 关闭）。
        drop(listener);
    }

    #[test]
    fn prepare_socket_removes_stale_socket() {
        // 旧 socket 存在 → 应删后重 bind（Go :31 os.Remove）。
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("stale.sock");
        // 造一个 stale socket 文件。
        std::fs::write(&sock, b"stale").unwrap();
        let cfg = ServerConfig {
            sock_path: sock.clone(),
            auth_file: dir.path().join("auth"),
            core_dir: None,
            console: false,
        };
        let listener = prepare_socket(&cfg).expect("应清旧 socket 后成功 bind");
        // 内容应被新 socket 替换（非 "stale"）。
        assert!(sock.exists());
        let meta = std::fs::metadata(&sock).unwrap();
        // unix socket 是特殊类型（非普通文件）。
        use std::os::unix::fs::FileTypeExt;
        assert!(meta.file_type().is_socket(), "应是 unix socket 类型");
        drop(listener);
    }

    #[test]
    fn prepare_socket_fails_on_uncreatable_dir() {
        // 目录路径不可创建（如 /proc 下的虚构路径）→ Mkdir 错误。
        let cfg = ServerConfig {
            sock_path: PathBuf::from("/proc/nonexistent_root_xyz/test.sock"),
            auth_file: PathBuf::from("/tmp/auth"),
            core_dir: None,
            console: false,
        };
        let r = prepare_socket(&cfg);
        assert!(r.is_err());
        match r {
            Err(ServerError::Mkdir { .. }) => {}
            other => panic!("expected Mkdir error, got {other:?}"),
        }
    }

    #[test]
    fn set_mode_sets_unix_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("mode_test");
        std::fs::write(&f, b"x").unwrap();
        set_mode(&f, 0o600).unwrap();
        use std::os::unix::fs::MetadataExt;
        let mode = std::fs::metadata(&f).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn ss_lookup_returns_none_when_ss_missing() {
        // ss 可能未装（best-effort，Go 同样容忍）。
        // 仅验证不 panic；结果 None 或 Some 均可（取决于机器）。
        let _ = ss_lookup("99999");
    }

    // ===== L16 AmbientCaps 集/常量（对照 Go helper.go:51-55,441）=====

    #[test]
    fn ambient_caps_match_go_set_and_l16_constants() {
        // 集内容 + 顺序对照 Go AmbientCaps=[NET_ADMIN, NET_RAW, NET_BIND_SERVICE]（:441）。
        let caps = ambient_caps();
        assert_eq!(caps.len(), 3);
        assert_eq!(caps[0], caps::Capability::CAP_NET_ADMIN);
        assert_eq!(caps[1], caps::Capability::CAP_NET_RAW);
        assert_eq!(caps[2], caps::Capability::CAP_NET_BIND_SERVICE);
        // L16 数值常量 == 内核 cap 号 == caps crate 的 index()（Go helper.go:52-54）。
        assert_eq!(CAP_NET_BIND_SERVICE_NUM, 10);
        assert_eq!(CAP_NET_ADMIN_NUM, 12);
        assert_eq!(CAP_NET_RAW_NUM, 13);
        assert_eq!(
            caps::Capability::CAP_NET_BIND_SERVICE.index(),
            CAP_NET_BIND_SERVICE_NUM
        );
        assert_eq!(caps::Capability::CAP_NET_ADMIN.index(), CAP_NET_ADMIN_NUM);
        assert_eq!(caps::Capability::CAP_NET_RAW.index(), CAP_NET_RAW_NUM);
    }

    // ===== terminate 决策（TERM→≤5s→KILL；不发真信号）=====

    #[test]
    fn terminate_child_kills_only_on_timeout() {
        // 期限内退出 → 只 TERM，不 KILL（Go: <-done 分支）。
        let mut termed = false;
        let mut killed = false;
        terminate_child(
            || termed = true,
            || true, /* exited */
            || killed = true,
        );
        assert!(termed, "应先 TERM");
        assert!(!killed, "期限内退出不应 KILL");
    }

    #[test]
    fn terminate_child_escalates_to_kill_on_timeout() {
        // 超时未退 → TERM 后 KILL（Go: <-time.After(5s) 分支）。
        let mut termed = false;
        let mut killed = false;
        terminate_child(
            || termed = true,
            || false, /* timeout */
            || killed = true,
        );
        assert!(termed);
        assert!(killed, "超时应升级 KILL");
    }

    // ===== watchParent 决策（对照 Go watchParent 循环体）=====

    #[test]
    fn watch_parent_step_truth_table() {
        // 非当前 child → 停（Go: if !current { return }）—— 优先于父存活判定。
        assert_eq!(watch_parent_step(false, true), WatchStep::Stop);
        assert_eq!(watch_parent_step(false, false), WatchStep::Stop);
        // 当前 child + 父死 → ParentDead（Go: kill(ppid,0)==ESRCH）。
        assert_eq!(watch_parent_step(true, false), WatchStep::ParentDead);
        // 当前 child + 父活 → 继续。
        assert_eq!(watch_parent_step(true, true), WatchStep::Continue);
    }

    // ===== ChildSlot 退出协调（收割线程 mark_exited 唤醒 terminate 的 wait_exited）=====

    #[test]
    fn child_slot_wait_returns_true_when_marked() {
        let slot = ChildSlot::new(4242);
        let s2 = Arc::clone(&slot);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            s2.mark_exited();
        });
        // 收割线程 20ms 后 mark → wait（宽限 2s）应在期限内返 true。
        assert!(
            slot.wait_exited(Duration::from_secs(2)),
            "mark_exited 后 wait 应返 true（→ terminate 不 KILL）"
        );
    }

    #[test]
    fn child_slot_wait_times_out_when_never_marked() {
        let slot = ChildSlot::new(1);
        // 从不 mark → 短宽限内超时返 false（→ terminate 升级 KILL）。
        assert!(!slot.wait_exited(Duration::from_millis(30)));
    }

    // ===== ReapGroup（对应 Go reapWG / waitReaps）=====

    #[test]
    fn reap_group_wait_returns_after_all_done() {
        let rg = ReapGroup::new();
        rg.add();
        rg.add();
        let r2 = Arc::clone(&rg);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            r2.done();
            r2.done();
        });
        // 两个在途 → done 归零后 wait 立即返回（不吃满 timeout）。
        let t0 = std::time::Instant::now();
        rg.wait(Duration::from_secs(5));
        assert!(t0.elapsed() < Duration::from_secs(4), "归零后应尽快返回");
    }

    #[test]
    fn reap_group_wait_returns_immediately_when_empty() {
        let rg = ReapGroup::new();
        let t0 = std::time::Instant::now();
        rg.wait(Duration::from_secs(5)); // 计数已 0 → 立即返回。
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn forward_fn_points_to_set_forward_prod() {
        // 验证闭包指针 = set_forward_prod（不 panic 即接线正确）。
        let f = forward_fn();
        f(false); // best-effort 写 /proc（非 root 静默失败）
    }

    /// 静态断言：ServerError 实现了 std::error::Error + Display。
    #[test]
    fn server_error_implements_error() {
        fn takes_error<E: std::error::Error>(_e: &E) {}
        let e = ServerError::Mkdir {
            dir: PathBuf::from("/x"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "test"),
        };
        takes_error(&e);
        assert!(e.to_string().contains("/x"));
    }
}
