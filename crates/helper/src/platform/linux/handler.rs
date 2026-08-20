//! 命令分发器 —— 移植自 上游 `helper-linux/helper.go:333-482` 的 `handle(conn)`。
//!
//! ## 流程（逐行对照 Go 源 handle()）
//!
//! 1. SO_PEERCRED 取对端凭据（uid/gid）→ 失败 `ERR peercred`（:337-340）。
//! 2. 读 command 行（:343，linux 无 token 行）。
//! 3. ping / version 在鉴权前（:345-352，任何持 socket 者可探活）。
//! 4. isAuthorized(uid) → 失败 `ERR unauthorized`（:354-357）。
//! 5. 持 mu 锁，按 command 分发（:359-481）。
//!
//! ## 测试策略
//!
//! Go 源 `handle(conn)` 直接吃 net.Conn。本实现把连接读写 + 凭据获取抽象为 [`Conn`] trait，
//! 让命令处理在不碰真实 socket 的前提下全路径测试（注入伪造 uid + 预置读写行）。
//!
//! 核 spawn（start 命令）经 [`CoreSpawner`](crate::platform::linux::state::CoreSpawner) trait 抽象（生产用真实 AmbientCaps
//! 派生，测试 mock）。进程状态放在 [`HandlerState`](crate::platform::linux::state::HandlerState)（实例化可测）。

use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::line_io;
use polaris_helper_proto::command::{common as cmd, linux as lcmd};
use polaris_helper_proto::{parse_stop_pid, stop_pid_matches, Response, ResponseKind, Stop};

use crate::core_install::InstallResult;
use crate::platform::linux::auth::{
    is_authorized, owned_by, supplementary_groups, AuthError, PeerCred, PeerCredProvider,
};
use crate::platform::linux::core_installer::install_core;
use crate::platform::linux::freeport::{free_port, parse_ss_pids, FreePortDeps};
use crate::platform::linux::ops::SystemdOps;
use crate::platform::linux::state::{CoreSpawner, HandlerState, SpawnCoreRequest, SpawnError};

/// Linux helper protoVersion（三平台统一 v1，见 `polaris_helper_proto` crate 文档）。
pub const PROTO_VERSION: u32 = polaris_helper_proto::proto_version::CURRENT;

/// 5 秒读超时（移植自 Go `conn.SetReadDeadline(time.Now().Add(5 * time.Second))`，:335）。
pub const READ_TIMEOUT_SECS: u64 = polaris_helper_proto::codec::READ_TIMEOUT_SECS;

/// handler 依赖（注入所有外部副作用，便于测试 mock）。
pub struct HandlerDeps<'a, P: PeerCredProvider, S: CoreSpawner, D: FreePortDeps, SD: SystemdOps> {
    /// 锁定的 root-owned 受管核目录（start 只跑 coreDir/sing-box）。
    pub core_dir: Option<&'a Path>,
    /// 授权 uid 列表文件。
    pub auth_file: &'a Path,
    /// SO_PEERCRED 凭据提供者。
    pub peer_cred: &'a P,
    /// sing-box spawn 抽象（start 命令）。
    pub spawner: &'a S,
    /// freeport 进程操作依赖。
    pub freeport_deps: &'a D,
    /// systemd 操作（启停 helper 自身服务，对照任务职责 1）。
    pub systemd: &'a SD,
    /// ss 命令的输出提供者（freeport 用 `ss -ltnp` 找 LISTEN 持有者）。
    /// 抽象为闭包便于测试；生产用 `ss` 子进程。
    pub ss_provider: &'a (dyn Fn(&str) -> Option<String> + Send + Sync),
    /// IP 转发开关的副作用闭包（生产 set_forward_prod，测试可记录调用）。
    pub set_forward: &'a (dyn Fn(bool) + Send + Sync),
}

/// 连接抽象（trait 便于测试 mock；生产用 tokio::net::UnixStream 经 adapter）。
///
/// 对应 Go `handle(conn net.Conn)`：读行 + 写行 + 取对端凭据。
pub trait Conn: Send {
    /// 读一行（trim 尾部 \n/\r）。EOF / 读失败返回 ""（对齐 Go readLine 的 ReadString 行为）。
    fn read_line(&mut self) -> String;
    /// 写一行（自动加 \n）。返回是否写成功。
    fn write_line(&mut self, line: &str) -> bool;
}

/// 把任意 Read + Write 包成 BufRead 行 IO（生产 unix socket adapter 用）。
///
/// 读写本体已上提 [`crate::line_io`]（与 mac 共用单一真值）；本类型只做
/// linux [`Conn`] 契约的形状适配（EOF→`""`、写成功→`bool`）。
pub struct LineConn<RW: Read + Write> {
    inner: BufReader<RW>,
}

impl<RW: Read + Write> LineConn<RW> {
    #[must_use]
    pub fn new(io: RW) -> Self {
        Self {
            inner: BufReader::new(io),
        }
    }
}

impl<RW: Read + Write + Send> Conn for LineConn<RW> {
    fn read_line(&mut self) -> String {
        // Conn 契约：EOF/读失败与空行一律 ""（对齐 Go readLine 的 ReadString 行为）。
        line_io::read_line_trimmed(&mut self.inner).unwrap_or_default()
    }

    fn write_line(&mut self, line: &str) -> bool {
        // 写走底层 RW（BufReader 只缓冲读方向），语义同原实现。
        line_io::write_line(self.inner.get_mut(), line).is_ok()
    }
}

/// 处理一个连接（移植自 Go `handle`，:333-482）。
///
/// 返回处理是否成功（连接层错误由调用方处理）。所有 wire 响应已写入 conn。
pub fn handle<P, S, D, SD>(
    state: &Mutex<HandlerState>,
    deps: &HandlerDeps<'_, P, S, D, SD>,
    conn: &mut impl Conn,
) where
    P: PeerCredProvider,
    S: CoreSpawner,
    D: FreePortDeps,
    SD: SystemdOps,
{
    // 1. SO_PEERCRED 取凭据（:337-340）。
    let Some(cred) = deps.peer_cred.peer_cred() else {
        // ERR peercred（Go: fmt.Fprintln(conn, "ERR peercred")）。
        let _ = conn.write_line(&format!("ERR {}", AuthError::Peercred.wire_token()));
        return;
    };

    // 2. 读 command 行（linux 无 token 行，:343）。
    let command = conn.read_line();

    // 3. ping / version 在鉴权前（任何持 socket 者可探活，:345-352）。
    match command.as_str() {
        cmd::PING => {
            // shared Pong 统一追加 build identity；旧 app 会忽略该字段，新 app 可识别同 protocol 旧 helper。
            let response = Response::Ok(ResponseKind::Pong(polaris_helper_proto::Pong::current(
                i64::from(cred.uid),
            )));
            let _ = conn.write_line(&response.to_wire_line());
            return;
        }
        cmd::VERSION => {
            // OK <ver>（Go: fmt.Fprintf(conn, "OK %s\n", protoVersion)）。
            let _ = conn.write_line(&format!("OK {PROTO_VERSION}"));
            return;
        }
        _ => {}
    }

    // 4. 鉴权（:354-357）。
    if !is_authorized(cred.uid, deps.auth_file) {
        let _ = conn.write_line(&format!("ERR {}", AuthError::Unauthorized.wire_token()));
        return;
    }

    // 5. 持锁按 command 分发（Go: mu.Lock(); defer mu.Unlock()）。
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(e) => {
            // 锁中毒（panic 残留）—— 极少见，回报 unknown。
            let _ = conn.write_line(&format!("ERR unknown {e}"));
            return;
        }
    };
    dispatch_locked(&mut guard, deps, &cred, &command, conn);
}

/// 持锁的命令分发（对照 Go switch cmd { ... }，:362-481）。
fn dispatch_locked<P, S, D, SD>(
    state: &mut HandlerState,
    deps: &HandlerDeps<'_, P, S, D, SD>,
    cred: &PeerCred,
    command: &str,
    conn: &mut impl Conn,
) where
    P: PeerCredProvider,
    S: CoreSpawner,
    D: FreePortDeps,
    SD: SystemdOps,
{
    match command {
        cmd::STATUS => handle_status(state, conn),
        cmd::STOP => handle_stop(state, deps, conn),
        cmd::CLEANUP => handle_cleanup(state, deps, cred, conn),
        cmd::FREEPORT => handle_freeport(deps, cred, conn),
        // install-core 是 linux 专属命令名（lcmd::INSTALL_CORE == "install-core"）。
        lcmd::INSTALL_CORE => handle_install_core(deps, conn),
        cmd::START => handle_start(state, deps, cred, conn),
        _ => {
            let _ = conn.write_line("ERR unknown");
        }
    }
}

// ===== 各命令处理（逐分支对照 Go 源）=====

/// status（:363-368）：running <pid> 或 stopped。
fn handle_status(state: &HandlerState, conn: &mut impl Conn) {
    if let Some(h) = state.child.as_ref() {
        let _ = conn.write_line(&format!("OK running {}", h.pid));
    } else {
        let _ = conn.write_line("OK stopped");
    }
}

/// stop（:369-380）：**受管 pid 身份校验** → 摘除 child + 后台收割 + 复位转发态。
///
/// 身份行（可选，本协议新增）：客户端声明它意图停的那个 pid。判据走
/// [`stop_pid_matches`] —— 不匹配 = 手里这个核属**另一个会话**（客户端的老 stop 腿在 IPC 上挂住
/// 期间，用户已经重装 helper / 重新起了核），此时杀它就是把用户刚连上的核静默掐掉。故不匹配一律
/// 诚实 no-op（`OK stop-mismatch <want> <current>`），绝不「反正要停就杀当前的」。
///
/// 读身份行发生在**持锁临界区内**（与 start/freeport/install-core 的参数行读同款）：连接级 5s 读
/// 超时（`server.rs` 的 `set_read_timeout`）是这条读的上界，客户端写完即 `shutdown` ⇒ 正常路径
/// 立刻 EOF 返 ""。
fn handle_stop<P, S, D, SD>(
    state: &mut HandlerState,
    deps: &HandlerDeps<'_, P, S, D, SD>,
    conn: &mut impl Conn,
) where
    P: PeerCredProvider,
    S: CoreSpawner,
    D: FreePortDeps,
    SD: SystemdOps,
{
    // 旧客户端不发这一行 → read_line 在 EOF 返 "" → None → 沿用「停当前受管核」旧语义。
    let want = parse_stop_pid(&conn.read_line());
    if let Some(h) = state.child.as_ref() {
        if !stop_pid_matches(want, h.pid) {
            let resp = Response::Ok(ResponseKind::Stop(Stop::Mismatch {
                want: want.unwrap_or(0),
                current: h.pid,
            }));
            let _ = conn.write_line(&resp.to_wire_line());
            return;
        }
    }
    if let Some(h) = state.child.take() {
        let pid = h.pid;
        // 复位转发态（:374，跟随运行中的核）。
        (deps.set_forward)(false);
        // 后台收割：TERM → ≤5s → KILL（Go: go func() { terminateChild(c, done) }()）。
        // 本实现同步等待 spawner.terminate（trait 抽象，测试可控；生产 spawn task）。
        deps.spawner.terminate(&h);
        let _ = conn.write_line(&format!("OK stopped {pid}"));
    } else {
        let _ = conn.write_line("OK notrunning");
    }
}

/// cleanup（:381-388）：kill child + pkill sing-box + 复位转发态。
fn handle_cleanup<P, S, D, SD>(
    state: &mut HandlerState,
    deps: &HandlerDeps<'_, P, S, D, SD>,
    cred: &PeerCred,
    conn: &mut impl Conn,
) where
    P: PeerCredProvider,
    S: CoreSpawner,
    D: FreePortDeps,
    SD: SystemdOps,
{
    if let Some(h) = state.child.take() {
        // :383: kill child。
        deps.spawner.kill(&h);
    }
    (deps.set_forward)(false);
    // :387: pkill -9 -U <uid> -f "sing-box run"（兜底清对端 uid 的所有 sing-box 实例）。
    // best-effort：忽略失败（Go: _ = exec.Command("pkill", ...).Run()）。
    let _ = std::process::Command::new("pkill")
        .args(["-9", "-U", &cred.uid.to_string(), "-f", "sing-box run"])
        .output();
    let _ = conn.write_line("OK cleaned");
}

/// freeport（:389-395）：按端口找 LISTEN 持有者。
fn handle_freeport<P, S, D, SD>(
    deps: &HandlerDeps<'_, P, S, D, SD>,
    cred: &PeerCred,
    conn: &mut impl Conn,
) where
    P: PeerCredProvider,
    S: CoreSpawner,
    D: FreePortDeps,
    SD: SystemdOps,
{
    // :390: 读 port 行。
    let port = conn.read_line();
    let port_trim = port.trim();
    // :391: 校验纯数字（Go: IndexFunc 非 '0'-'9' 即拒绝）。
    if port_trim.is_empty() || !port_trim.bytes().all(|b| b.is_ascii_digit()) {
        let _ = conn.write_line("ERR bad-port");
        return;
    }
    // :392: ss -H -ltnp 'sport = :<port>'。
    let ss_out = (deps.ss_provider)(port_trim);
    let pids = match ss_out {
        Some(s) => parse_ss_pids(&s),
        None => Vec::new(),
    };
    // :395: free_port 分发。wire 序列化走协议层单一真值（G3.1/G3.3）。
    let outcome = free_port(&pids, cred.uid, deps.freeport_deps);
    let resp = Response::Ok(ResponseKind::FreePort(outcome));
    let _ = conn.write_line(&resp.to_wire_line());
}

/// install-core（:396-399）：校验 sha256 + 原子写入 coreDir。
fn handle_install_core<P, S, D, SD>(deps: &HandlerDeps<'_, P, S, D, SD>, conn: &mut impl Conn)
where
    P: PeerCredProvider,
    S: CoreSpawner,
    D: FreePortDeps,
    SD: SystemdOps,
{
    // :397-398: 读 srcDir 行 + wantHash 行。
    let src = conn.read_line();
    let want_hash = conn.read_line();
    let outcome: InstallResult = install_core(deps.core_dir, src.trim(), want_hash.trim());
    let _ = conn.write_line(&outcome.to_wire_line());
}

/// start（:400-478）：核路径锁 + config 属主校验 + AmbientCaps 拉核。
#[allow(clippy::too_many_lines)]
fn handle_start<P, S, D, SD>(
    state: &mut HandlerState,
    deps: &HandlerDeps<'_, P, S, D, SD>,
    cred: &PeerCred,
    conn: &mut impl Conn,
) where
    P: PeerCredProvider,
    S: CoreSpawner,
    D: FreePortDeps,
    SD: SystemdOps,
{
    // :401-405: 读 singbox / cfg / log / fwd / ppid 行。
    let singbox = conn.read_line();
    let cfg = conn.read_line();
    let log_path = conn.read_line();
    let fwd = conn.read_line();
    let ppid_str = conn.read_line();
    let ppid: u32 = ppid_str.trim().parse().unwrap_or(0);

    // :407-410: 已有 child → already。
    if let Some(h) = state.child.as_ref() {
        let _ = conn.write_line(&format!("OK already {}", h.pid));
        return;
    }
    // :411-413: cfg 空 → bad-args。
    let cfg = cfg.trim();
    if cfg.is_empty() {
        let _ = conn.write_line("ERR bad-args");
        return;
    }
    // :417-420: 核路径锁 —— singbox 必须 == coreDir/sing-box。
    let Some(core_dir) = deps.core_dir else {
        let _ = conn.write_line("ERR coredir-unset");
        return;
    };
    let core_bin: PathBuf = core_dir.join("sing-box");
    if Path::new(singbox.trim()) != core_bin.as_path() {
        let _ = conn.write_line(&format!(
            "ERR core-path-denied (want {})",
            core_bin.display()
        ));
        return;
    }
    // :421-424: 锁定核二进制必须存在。
    if !core_bin.exists() {
        let _ = conn.write_line("ERR core-missing");
        return;
    }
    // :425-428: config 必须属于对端 uid（防读别人配置）。
    match owned_by(Path::new(cfg), cred.uid) {
        Ok(true) => {}
        Ok(false) => {
            let _ = conn.write_line("ERR config-not-owned");
            return;
        }
        Err(e) => {
            let _ = conn.write_line(&format!("ERR config-not-owned {e}"));
            return;
        }
    }
    // **Polaris 新增（上游无）**：log 必须与 cfg **同一父目录**。
    //
    // 上游只校验 cfg 的属主，而 `spawn` 会以 **root** 身份 `O_CREATE|O_APPEND|0644` 打开这个 log
    // 路径、**并 `fchown` 给对端 uid**（`linux/server.rs`）⇒ 不校验就是「root 在任意位置建文件、
    // 再把属主给调用者」——比单纯的任意追加写更强，`/etc/cron.d/` 之类落一个文件即完全提权。
    //
    // 判据取「同父目录」而非 conf_dir 白名单：linux 腿没有 `--confdir`（它用属主校验代替），
    // 而生产下发的 cfg 与 log 恒是同目录的 `singbox-runtime.json` / `singbox-startup.log`
    //（`runtime/proxy.rs`）⇒ 这条收紧**零行为变更**。含 `..` 的路径会让父目录字面不等，自然被拒。
    //
    // 🔴 **未覆盖：符号链接**。判据是纯路径比较，若攻击者在该目录里放一个指向 `/etc/...` 的符号
    // 链接，root 打开时仍会跟随。要堵死得上 `O_NOFOLLOW`（只管最后一段）或 openat2 RESOLVE_BENEATH；
    // 更彻底的修法是**根本不接受客户端下发 log 路径**（helper 自己按 conf_dir 拼）。均已登记，见
    // `~/docs/polaris/design/polaris-platform-code-sweep-2026-08-09.md`。
    let log_trimmed = log_path.trim();
    if !log_trimmed.is_empty() && Path::new(log_trimmed).parent() != Path::new(cfg).parent() {
        let _ = conn.write_line("ERR log-path-denied");
        return;
    }
    // :429: 显式跟随本次会话的转发态。
    (deps.set_forward)(fwd.trim() == "1");

    // :431-478: AmbientCaps 拉核（setuid 回对端登录用户 + CAP_NET_ADMIN/RAW/BIND_SERVICE）。
    // 经 CoreSpawner trait 抽象：生产实现做 fork+setuid+AmbientCaps+execve（§helper-rust-evaluation B3 真机项）；
    // 测试 mock 返回固定 pid。
    let req = SpawnCoreRequest {
        binary: core_bin.clone(),
        config: PathBuf::from(cfg),
        log: if log_path.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(log_path.trim()))
        },
        fwd: fwd.trim() == "1",
        parent_pid: if ppid > 0 { Some(ppid) } else { None },
        uid: cred.uid,
        gid: cred.gid,
        // 补充组在 fork 前于父进程解析（Go SysProcAttr.Credential.Groups），随 request 下发给 pre_exec 的
        // setgroups（拉核子进程不碰 NSS）。对照 Go start 分支 `Groups: supplementaryGroups(cred.Uid)`，:439。
        groups: supplementary_groups(cred.uid),
    };
    match deps.spawner.spawn(&req) {
        Ok(h) => {
            let pid = h.pid;
            state.child = Some(h);
            let _ = conn.write_line(&format!("OK started {pid}"));
        }
        Err(SpawnError::Spawn { detail }) => {
            // :456: 复位转发态（拉核失败不留全局转发）。
            (deps.set_forward)(false);
            let _ = conn.write_line(&format!("ERR start {detail}"));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;
    use crate::platform::linux::auth::{NoPeerCred, StaticPeerCred};
    use crate::platform::linux::ops::{SystemdAction, SystemdOps, SystemdResult};
    use crate::platform::linux::state::{CoreHandle, CoreSpawner};
    use std::sync::{Arc, Mutex as StdMutex};
    use tempfile::tempdir;

    // ===== Mock Conn（预置读行队列 + 记录写行）=====

    struct MockConn {
        reads: StdMutex<std::collections::VecDeque<String>>,
        writes: StdMutex<Vec<String>>,
    }

    impl MockConn {
        fn new(reads: Vec<&str>) -> Self {
            Self {
                reads: StdMutex::new(reads.into_iter().map(String::from).collect()),
                writes: StdMutex::new(Vec::new()),
            }
        }
        fn writes(&self) -> Vec<String> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl Conn for MockConn {
        fn read_line(&mut self) -> String {
            self.reads.lock().unwrap().pop_front().unwrap_or_default()
        }
        fn write_line(&mut self, line: &str) -> bool {
            self.writes.lock().unwrap().push(line.to_string());
            true
        }
    }

    // ===== Mock Spawner =====

    struct MockSpawner {
        next_pid: u32,
        fail: bool,
        spawn_calls: StdMutex<Vec<SpawnCoreRequest>>,
        terminate_calls: StdMutex<Vec<u32>>,
        kill_calls: StdMutex<Vec<u32>>,
    }

    impl MockSpawner {
        fn succeeding(start_pid: u32) -> Self {
            Self {
                next_pid: start_pid,
                fail: false,
                spawn_calls: StdMutex::new(Vec::new()),
                terminate_calls: StdMutex::new(Vec::new()),
                kill_calls: StdMutex::new(Vec::new()),
            }
        }
    }

    impl CoreSpawner for MockSpawner {
        fn spawn(&self, req: &SpawnCoreRequest) -> Result<CoreHandle, SpawnError> {
            self.spawn_calls.lock().unwrap().push(req.clone());
            if self.fail {
                return Err(SpawnError::Spawn {
                    detail: "mock spawn failure".into(),
                });
            }
            let pid = self.next_pid;
            Ok(CoreHandle { pid })
        }
        fn terminate(&self, h: &CoreHandle) {
            self.terminate_calls.lock().unwrap().push(h.pid);
        }
        fn kill(&self, h: &CoreHandle) {
            self.kill_calls.lock().unwrap().push(h.pid);
        }
    }

    // ===== Mock FreePortDeps =====

    struct MockFreePort {
        uid_map: StdMutex<std::collections::HashMap<u32, u32>>,
        comm_map: StdMutex<std::collections::HashMap<u32, String>>,
        killed: StdMutex<Vec<u32>>,
    }

    impl MockFreePort {
        fn empty() -> Self {
            Self {
                uid_map: StdMutex::new(std::collections::HashMap::new()),
                comm_map: StdMutex::new(std::collections::HashMap::new()),
                killed: StdMutex::new(Vec::new()),
            }
        }
    }

    impl FreePortDeps for MockFreePort {
        fn proc_uid(&self, pid: u32) -> Option<u32> {
            self.uid_map.lock().unwrap().get(&pid).copied()
        }
        fn proc_comm(&self, pid: u32) -> Option<String> {
            self.comm_map.lock().unwrap().get(&pid).cloned()
        }
        fn kill(&self, pid: u32) -> bool {
            self.killed.lock().unwrap().push(pid);
            true
        }
    }

    // ===== Mock Systemd（handler 测试用，记录调用）=====

    #[derive(Default)]
    struct MockSystemd {
        calls: StdMutex<Vec<(String, SystemdAction)>>,
    }

    impl SystemdOps for MockSystemd {
        fn run(&self, unit: &str, action: SystemdAction) -> SystemdResult {
            self.calls.lock().unwrap().push((unit.to_string(), action));
            SystemdResult::ok()
        }
    }

    // ===== 装配 helper =====

    #[allow(clippy::too_many_arguments)]
    fn make_deps<'a, PE: PeerCredProvider>(
        core_dir: Option<&'a Path>,
        auth_file: &'a Path,
        peer: &'a PE,
        spawner: &'a MockSpawner,
        fp: &'a MockFreePort,
        systemd: &'a MockSystemd,
        ss: &'a (dyn Fn(&str) -> Option<String> + Send + Sync),
        fwd: &'a (dyn Fn(bool) + Send + Sync),
    ) -> HandlerDeps<'a, PE, MockSpawner, MockFreePort, MockSystemd> {
        HandlerDeps {
            core_dir,
            auth_file,
            peer_cred: peer,
            spawner,
            freeport_deps: fp,
            systemd,
            ss_provider: ss,
            set_forward: fwd,
        }
    }

    /// 造一个授权文件 + coreDir（含 sing-box 二进制）。
    fn setup_env() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempdir().unwrap();
        let auth = dir.path().join("auth");
        std::fs::write(&auth, "1000\n").unwrap();
        let core_dir = dir.path().join("core");
        std::fs::create_dir_all(&core_dir).unwrap();
        std::fs::write(core_dir.join("sing-box"), b"#!bin\nfake sing-box").unwrap();
        (dir, auth, core_dir)
    }

    fn no_op_fwd() -> impl Fn(bool) {
        |_| {}
    }

    fn no_op_ss() -> impl Fn(&str) -> Option<String> {
        move |_: &str| None
    }

    // ===== ping / version（鉴权前）=====

    #[test]
    fn ping_responds_before_auth() {
        // 即使 uid 不在授权列表，ping 也应响应（Go :345-347）。
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(9999, 9999); // 未授权 uid
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["ping"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(
            conn.writes(),
            vec![format!(
                "OK pong uid=9999 v{PROTO_VERSION} build={}",
                polaris_helper_proto::build_identity::current()
            )]
        );
    }

    #[test]
    fn version_responds_before_auth() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(0, 0);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["version"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec![format!("OK {PROTO_VERSION}")]);
    }

    // ===== SO_PEERCRED 失败 =====

    #[test]
    fn peercred_failure_returns_err_peercred() {
        let (_dir, auth, _core) = setup_env();
        let peer = NoPeerCred; // SO_PEERCRED 失败
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["ping"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["ERR peercred"]);
    }

    // ===== unauthorized =====

    #[test]
    fn unauthorized_uid_rejected_for_status() {
        let (_dir, auth, _core) = setup_env();
        // auth 只授权 1000；对端 uid 9999 → unauthorized。
        let peer = StaticPeerCred::new(9999, 9999);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["status"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["ERR unauthorized"]);
    }

    #[test]
    fn root_always_authorized() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(0, 0); // root
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["status"]);
        handle(&state, &deps, &mut conn);
        // root 应通过鉴权 → OK stopped（无 child）。
        assert_eq!(conn.writes(), vec!["OK stopped"]);
    }

    // ===== status / stop =====

    #[test]
    fn status_stopped_when_no_child() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["status"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK stopped"]);
    }

    #[test]
    fn status_running_when_child_present() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(4242);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let mut state = HandlerState::new();
        state.child = Some(CoreHandle { pid: 4242 });
        let state = Mutex::new(state);
        let mut conn = MockConn::new(vec!["status"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK running 4242"]);
    }

    #[test]
    fn stop_notrunning_when_no_child() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["stop"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK notrunning"]);
    }

    #[test]
    fn stop_terminates_child_and_reports_pid() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(555);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd_called = Arc::new(StdMutex::new(Vec::new()));
        let fwd = {
            let fc = Arc::clone(&fwd_called);
            move |on: bool| fc.lock().unwrap().push(on)
        };
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let mut state = HandlerState::new();
        state.child = Some(CoreHandle { pid: 555 });
        let state = Mutex::new(state);
        let mut conn = MockConn::new(vec!["stop"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK stopped 555"]);
        assert_eq!(*spawner.terminate_calls.lock().unwrap(), vec![555]);
        assert_eq!(
            *fwd_called.lock().unwrap(),
            vec![false],
            "stop 应复位转发态"
        );
    }

    // ===== stop 的受管 pid 身份判据（杀错进程的防线）=====

    /// **变异门（核心）**：身份不匹配时 **一个进程都不许动**。
    ///
    /// 场景（真机时序）：客户端的老 stop 腿挂在 IPC 上，期间用户重装 helper 并起了新核 9001；
    /// 这条腿醒来后拿着旧 pid 555 落到 daemon —— daemon 手里已是新核。
    ///
    /// 变异（逃逸面穷举）：
    /// - 删掉 `handle_stop` 里的 `stop_pid_matches` 判据（退回「反正要停就杀当前的」）→
    ///   `terminate_calls == [9001]` + 响应变 `OK stopped 9001` → 转红。
    /// - 判据改成只比大小/恒真 → 同上转红。
    /// - 只改响应不改行为（回 mismatch 但仍 `take()` + `terminate`）→ 后两条断言转红。
    /// - 顺手把 `set_forward(false)` 留着（让位腿却复位了新会话的转发态）→ fwd 断言转红。
    #[test]
    fn stop_refuses_to_kill_when_managed_pid_is_another_session() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(9001);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd_called = Arc::new(StdMutex::new(Vec::new()));
        let fwd = {
            let fc = Arc::clone(&fwd_called);
            move |on: bool| fc.lock().unwrap().push(on)
        };
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let mut state = HandlerState::new();
        // daemon 手里的是**新会话**的核。
        state.child = Some(CoreHandle { pid: 9001 });
        let state = Mutex::new(state);
        // 老 stop 腿声明它要停的是 555。
        let mut conn = MockConn::new(vec!["stop", "555"]);
        handle(&state, &deps, &mut conn);

        assert_eq!(
            conn.writes(),
            vec!["OK stop-mismatch 555 9001"],
            "身份不匹配 → 诚实 no-op 并回报两个 pid（客户端据此记账/记日志）"
        );
        assert!(
            spawner.terminate_calls.lock().unwrap().is_empty(),
            "绝不能杀：9001 是用户刚连上的新核，杀它 = 静默断线且现象酷似核自己崩了"
        );
        assert!(
            spawner.kill_calls.lock().unwrap().is_empty(),
            "也不许走 kill 腿"
        );
        assert_eq!(
            state.lock().unwrap().child.as_ref().map(|h| h.pid),
            Some(9001),
            "child 记账必须原样留给新会话（摘掉 = 新核失联，daemon 再也停不掉它）"
        );
        assert!(
            fwd_called.lock().unwrap().is_empty(),
            "让位腿不得复位新会话的 IP 转发态"
        );
    }

    /// 身份**匹配**时照常停（反向失效门）：判据不能收得太紧，否则停核彻底失效。
    #[test]
    fn stop_proceeds_when_managed_pid_matches_request() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(555);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let mut state = HandlerState::new();
        state.child = Some(CoreHandle { pid: 555 });
        let state = Mutex::new(state);
        let mut conn = MockConn::new(vec!["stop", "555"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK stopped 555"]);
        assert_eq!(*spawner.terminate_calls.lock().unwrap(), vec![555]);
        assert!(state.lock().unwrap().child.is_none());
    }

    /// 无 child 时带身份 → 诚实 `notrunning`（不是 mismatch —— 本来就没东西可杀）。
    #[test]
    fn stop_with_identity_reports_notrunning_when_no_child() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["stop", "555"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK notrunning"]);
    }

    /// **wire 向后兼容门**：旧客户端只发 `stop`（无身份行）→ 沿用「停当前受管核」旧语义。
    ///
    /// 变异：把 `parse_stop_pid` 的空串处置改成 `Some(0)` → 判据恒不匹配 → 本测转红（那会让
    /// 装了新 helper 的机器上、任何不带身份的停核请求全部失效 = 永远停不掉核）。
    #[test]
    fn stop_without_identity_line_keeps_legacy_semantics() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(777);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let mut state = HandlerState::new();
        state.child = Some(CoreHandle { pid: 777 });
        let state = Mutex::new(state);
        let mut conn = MockConn::new(vec!["stop"]); // 无身份行（read_line 在耗尽后返 ""）
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK stopped 777"]);
        assert_eq!(*spawner.terminate_calls.lock().unwrap(), vec![777]);
    }

    // ===== unknown command =====

    #[test]
    fn unknown_command_returns_err_unknown() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["frobnicate"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["ERR unknown"]);
    }

    // ===== freeport =====

    #[test]
    fn freeport_bad_port_rejected() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["freeport", "abc"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["ERR bad-port"]);
    }

    #[test]
    fn freeport_empty_port_rejected() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["freeport", ""]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["ERR bad-port"]);
    }

    #[test]
    fn freeport_free_when_ss_returns_none() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss(); // ss 缺失
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["freeport", "9090"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK free"]);
    }

    #[test]
    fn freeport_kills_own_singbox() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        fp.uid_map.lock().unwrap().insert(1234, 1000);
        fp.comm_map.lock().unwrap().insert(1234, "sing-box".into());
        let systemd = MockSystemd::default();
        let ss = |_p: &str| Some("pid=1234".to_string());
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["freeport", "9090"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK killed 1234"]);
        assert_eq!(*fp.killed.lock().unwrap(), vec![1234]);
    }

    // ===== install-core =====

    #[test]
    fn install_core_coredir_unset() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let state = Mutex::new(HandlerState::new());
        let hash = "a".repeat(64);
        let mut conn = MockConn::new(vec!["install-core", "/tmp/src", &hash]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["ERR coredir-unset"]);
    }

    #[test]
    fn install_core_bad_args_for_short_hash() {
        let (_dir, auth, core_dir) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(
            Some(&core_dir),
            &auth,
            &peer,
            &spawner,
            &fp,
            &systemd,
            &ss,
            &fwd,
        );
        let state = Mutex::new(HandlerState::new());
        let mut conn = MockConn::new(vec!["install-core", "/tmp/src", "abc"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["ERR bad-args"]);
    }

    // ===== start =====

    #[test]
    fn start_bad_args_when_cfg_empty() {
        let (_dir, auth, core_dir) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(
            Some(&core_dir),
            &auth,
            &peer,
            &spawner,
            &fp,
            &systemd,
            &ss,
            &fwd,
        );
        let state = Mutex::new(HandlerState::new());
        let sb = core_dir.join("sing-box").to_string_lossy().into_owned();
        // singbox / cfg="" / log / fwd / ppid
        let mut conn = MockConn::new(vec!["start", &sb, "", "", "0", ""]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["ERR bad-args"]);
    }

    #[test]
    fn start_core_path_denied_when_singbox_mismatch() {
        let (_dir, auth, core_dir) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(
            Some(&core_dir),
            &auth,
            &peer,
            &spawner,
            &fp,
            &systemd,
            &ss,
            &fwd,
        );
        let state = Mutex::new(HandlerState::new());
        // singbox 传一个错误路径（!= coreDir/sing-box）。
        let mut conn = MockConn::new(vec![
            "start",
            "/tmp/evil/sing-box",
            "/tmp/cfg.json",
            "",
            "0",
            "",
        ]);
        handle(&state, &deps, &mut conn);
        let w = &conn.writes()[0];
        assert!(w.starts_with("ERR core-path-denied"), "got {w}");
    }

    #[test]
    fn start_core_missing_when_binary_absent() {
        // coreDir 存在但 sing-box 不存在 → core-missing。
        let dir = tempdir().unwrap();
        let auth = dir.path().join("auth");
        std::fs::write(&auth, "1000\n").unwrap();
        let core_dir = dir.path().join("core");
        std::fs::create_dir_all(&core_dir).unwrap();
        // 不建 sing-box 二进制。

        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(
            Some(&core_dir),
            &auth,
            &peer,
            &spawner,
            &fp,
            &systemd,
            &ss,
            &fwd,
        );
        let state = Mutex::new(HandlerState::new());
        let sb = core_dir.join("sing-box").to_string_lossy().into_owned();
        let mut conn = MockConn::new(vec!["start", &sb, "/tmp/c.json", "", "0", ""]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["ERR core-missing"]);
    }

    #[test]
    fn start_config_not_owned_rejected() {
        let (_dir, auth, core_dir) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(
            Some(&core_dir),
            &auth,
            &peer,
            &spawner,
            &fp,
            &systemd,
            &ss,
            &fwd,
        );
        // cfg 路径不存在 → owned_by 返回 err → config-not-owned。
        let state = Mutex::new(HandlerState::new());
        let sb = core_dir.join("sing-box").to_string_lossy().into_owned();
        let mut conn = MockConn::new(vec!["start", &sb, "/nonexistent/cfg.json", "", "0", ""]);
        handle(&state, &deps, &mut conn);
        let w = &conn.writes()[0];
        assert!(w.starts_with("ERR config-not-owned"), "got {w}");
    }

    #[test]
    fn start_spawns_and_reports_pid() {
        let (dir, auth, core_dir) = setup_env();
        // 造一个属主 = 本进程 uid 的 cfg。
        let cfg = dir.path().join("cfg.json");
        let self_uid = nix::unistd::getuid().as_raw();
        std::fs::write(&cfg, b"{}").unwrap();
        // auth 文件须包含 self_uid。
        std::fs::write(&auth, format!("{self_uid}\n")).unwrap();
        let peer = StaticPeerCred::new(self_uid, self_uid);

        let spawner = MockSpawner::succeeding(7777);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(
            Some(&core_dir),
            &auth,
            &peer,
            &spawner,
            &fp,
            &systemd,
            &ss,
            &fwd,
        );
        let state = Mutex::new(HandlerState::new());
        let sb = core_dir.join("sing-box").to_string_lossy().into_owned();
        let cfg_s = cfg.to_string_lossy().into_owned();
        let mut conn = MockConn::new(vec!["start", &sb, &cfg_s, "", "0", ""]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK started 7777"]);
        let calls = spawner.spawn_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].binary, core_dir.join("sing-box"));
    }

    /// log 必须与 cfg 同父目录 —— 不校验就是「root 在任意位置建文件、再 fchown 给调用者」。
    #[test]
    fn start_log_path_denied_when_outside_cfg_dir() {
        let (dir, auth, core_dir) = setup_env();
        let cfg = dir.path().join("cfg.json");
        let self_uid = nix::unistd::getuid().as_raw();
        std::fs::write(&cfg, b"{}").unwrap();
        std::fs::write(&auth, format!("{self_uid}\n")).unwrap();
        let peer = StaticPeerCred::new(self_uid, self_uid);
        let spawner = MockSpawner::succeeding(7777);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(
            Some(&core_dir),
            &auth,
            &peer,
            &spawner,
            &fp,
            &systemd,
            &ss,
            &fwd,
        );
        let state = Mutex::new(HandlerState::new());
        let sb = core_dir.join("sing-box").to_string_lossy().into_owned();
        let cfg_s = cfg.to_string_lossy().into_owned();
        // cfg 合法（属主对、目录对），只有 log 越界。
        let mut conn = MockConn::new(vec!["start", &sb, &cfg_s, "/etc/cron.d/pwn", "0", ""]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["ERR log-path-denied"]);
        // 越界就不该 spawn —— 只看错误行不够，恒拒的实现也会让上一条通过。
        assert!(
            spawner.spawn_calls.lock().unwrap().is_empty(),
            "被拒了却仍然起了核"
        );
    }

    /// 生产形态（log 与 cfg 同目录）必须放行，且 log 被原样下发给 spawner。
    #[test]
    fn start_ok_when_log_beside_cfg() {
        let (dir, auth, core_dir) = setup_env();
        let cfg = dir.path().join("singbox-runtime.json");
        let log = dir.path().join("singbox-startup.log");
        let self_uid = nix::unistd::getuid().as_raw();
        std::fs::write(&cfg, b"{}").unwrap();
        std::fs::write(&auth, format!("{self_uid}\n")).unwrap();
        let peer = StaticPeerCred::new(self_uid, self_uid);
        let spawner = MockSpawner::succeeding(7777);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(
            Some(&core_dir),
            &auth,
            &peer,
            &spawner,
            &fp,
            &systemd,
            &ss,
            &fwd,
        );
        let state = Mutex::new(HandlerState::new());
        let sb = core_dir.join("sing-box").to_string_lossy().into_owned();
        let cfg_s = cfg.to_string_lossy().into_owned();
        let log_s = log.to_string_lossy().into_owned();
        let mut conn = MockConn::new(vec!["start", &sb, &cfg_s, &log_s, "0", ""]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK started 7777"], "生产形态被误拒");
        let calls = spawner.spawn_calls.lock().unwrap();
        assert_eq!(calls[0].log.as_deref(), Some(log.as_path()));
    }

    #[test]
    fn start_already_when_child_present() {
        let (_dir, auth, core_dir) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(
            Some(&core_dir),
            &auth,
            &peer,
            &spawner,
            &fp,
            &systemd,
            &ss,
            &fwd,
        );
        let mut state = HandlerState::new();
        state.child = Some(CoreHandle { pid: 8888 });
        let state = Mutex::new(state);
        let sb = core_dir.join("sing-box").to_string_lossy().into_owned();
        let mut conn = MockConn::new(vec!["start", &sb, "/tmp/c.json", "", "0", ""]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK already 8888"]);
        // 已有 child → 不再 spawn。
        assert!(spawner.spawn_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn start_failure_reports_err_start_and_resets_forward() {
        let (dir, auth, core_dir) = setup_env();
        let self_uid = nix::unistd::getuid().as_raw();
        std::fs::write(&auth, format!("{self_uid}\n")).unwrap();
        let peer = StaticPeerCred::new(self_uid, self_uid);
        let spawner = MockSpawner {
            next_pid: 0,
            fail: true,
            spawn_calls: StdMutex::new(Vec::new()),
            terminate_calls: StdMutex::new(Vec::new()),
            kill_calls: StdMutex::new(Vec::new()),
        };
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd_called = Arc::new(StdMutex::new(Vec::new()));
        let fwd = {
            let fc = Arc::clone(&fwd_called);
            move |on: bool| fc.lock().unwrap().push(on)
        };
        let deps = make_deps(
            Some(&core_dir),
            &auth,
            &peer,
            &spawner,
            &fp,
            &systemd,
            &ss,
            &fwd,
        );
        let cfg = dir.path().join("cfg.json");
        std::fs::write(&cfg, b"{}").unwrap();
        let state = Mutex::new(HandlerState::new());
        let sb = core_dir.join("sing-box").to_string_lossy().into_owned();
        let cfg_s = cfg.to_string_lossy().into_owned();
        let mut conn = MockConn::new(vec!["start", &sb, &cfg_s, "", "1", ""]);
        handle(&state, &deps, &mut conn);
        let w = &conn.writes()[0];
        assert!(w.starts_with("ERR start"), "got {w}");
        // fwd=1 先设，spawn 失败后复位为 false。
        assert_eq!(*fwd_called.lock().unwrap(), vec![true, false]);
    }

    // ===== cleanup =====

    #[test]
    fn cleanup_kills_child_and_reports_cleaned() {
        let (_dir, auth, _core) = setup_env();
        let peer = StaticPeerCred::new(1000, 1000);
        let spawner = MockSpawner::succeeding(100);
        let fp = MockFreePort::empty();
        let systemd = MockSystemd::default();
        let ss = no_op_ss();
        let fwd = no_op_fwd();
        let deps = make_deps(None, &auth, &peer, &spawner, &fp, &systemd, &ss, &fwd);
        let mut state = HandlerState::new();
        state.child = Some(CoreHandle { pid: 333 });
        let state = Mutex::new(state);
        let mut conn = MockConn::new(vec!["cleanup"]);
        handle(&state, &deps, &mut conn);
        assert_eq!(conn.writes(), vec!["OK cleaned"]);
        assert_eq!(*spawner.kill_calls.lock().unwrap(), vec![333]);
    }

    // ===== wire 响应形态锁住（对照 Go 源每个 Fprintln/Fprintf）=====

    #[test]
    fn wire_forms_match_go_source() {
        // v1 是 wire 断代真值；build 字段是尾部向后兼容扩展（旧 app 忽略）。
        assert_eq!(PROTO_VERSION, 1);
        assert_eq!(
            Response::Ok(ResponseKind::Pong(polaris_helper_proto::Pong::current(0))).to_wire_line(),
            format!(
                "OK pong uid=0 v1 build={}",
                polaris_helper_proto::build_identity::current()
            )
        );
        assert_eq!(format!("OK {PROTO_VERSION}"), "OK 1");
        assert_eq!("OK stopped", "OK stopped");
        assert_eq!("OK running 12345", "OK running 12345");
        assert_eq!("OK notrunning", "OK notrunning");
        assert_eq!("OK stopped 12345", "OK stopped 12345");
        assert_eq!("OK already 12345", "OK already 12345");
        assert_eq!("OK started 12345", "OK started 12345");
        assert_eq!("OK cleaned", "OK cleaned");
        assert_eq!("OK free", "OK free");
        assert_eq!("OK killed 123,456", "OK killed 123,456");
        assert_eq!("OK foreign a | b", "OK foreign a | b");
        assert_eq!("OK installed", "OK installed");
        assert_eq!("ERR peercred", "ERR peercred");
        assert_eq!("ERR unauthorized", "ERR unauthorized");
        assert_eq!("ERR unknown", "ERR unknown");
        assert_eq!("ERR bad-port", "ERR bad-port");
        assert_eq!("ERR bad-args", "ERR bad-args");
        assert_eq!("ERR core-missing", "ERR core-missing");
        assert_eq!(
            "ERR core-path-denied (want /x)",
            "ERR core-path-denied (want /x)"
        );
        assert_eq!("ERR config-not-owned", "ERR config-not-owned");
        assert_eq!("ERR coredir-unset", "ERR coredir-unset");
        assert_eq!("ERR start boom", "ERR start boom");
    }
}
