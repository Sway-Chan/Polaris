//! 进程启动时间 + 存活探测（移植自 上游 `helper/proc_starttime_darwin.go` + `helper.go:289-356`）。
//!
//! ## Go 源根因（`helper.go:289-310,316-356`）
//!
//! 父死看护（watchParent）托管 child sing-box 期间，每秒探测父 app 进程。父消失（GUI 崩溃/kill -9/强退）
//! → 收割 child。但单一 `kill(ppid, 0)` 判存活有**假阴性窗口**：父 kill -9 后 PID 在 tick 间隙被别的
//! 进程复用 → `kill(ppid,0)` 返回 nil（误判存活）→ root sing-box 漏成孤儿。
//!
//! Go 源用**进程启动时间**作进程唯一身份补这个窗口：启动时快照父进程启动时间（`ppidStart`），每轮除
//! `kill(ppid,0)` 外再比对当前启动时间，不符 = 原父已死（PID 被复用）。
//!
//! ## sysctl 取启动时间（`proc_starttime_darwin.go`）
//!
//! Go 用 `sysctl kern.proc.pid` 取 `kinfo_proc.p_un.__p_starttime`（`struct timeval`，微秒精度），
//! 相比 `ps -o lstart=`（秒级）把「同秒 PID 复用」假阴性窗口从 1s 收到 1µs。darwin MIB 是
//! `{CTL_KERN=1, KERN_PROC=14, KERN_PROC_PID=1, pid}`，64 位上 `tv_sec` 在偏移 0（int64）、
//! `tv_usec` 在偏移 8（int32）。
//!
//! ## 移植纪律（forbid(unsafe_code)）
//!
//! Go 用裸 `syscall.SYS___SYSCTL`（unsafe）。Rust 侧：
//! - **启动时间格式化**（`format_start_time`）是纯函数，跨平台可测。
//! - **sysctl 取值**：nix 0.31 不直接暴露 sysctl MIB 调用。本模块 mac 分支用 `libc::sysctl` 经
//!   `nix::unistd::Pid` 配套，但**核心安全语义**（取不到 → 空串 → 保守不判死）跨平台一致。
//!   非 mac（Linux CI）返回 None，watchParent 退化为仅 kill(0) 探测（对齐 Go `helper.go:335`：
//!   `cur==""` 不据此判死）。
//!
//! 地雷（B3 真机复验）：sysctl 的 kinfo_proc 布局因 macOS 版本而异（p_un union 偏移），需在
//! 13/14/15 上各跑一次确认 `tv_sec`/`tv_usec` 偏移正确。本实现用常量偏移（64 位 darwin ABI 稳定）。

// M14：sysctl kern.proc.pid 取启动时间需 FFI（`nix::libc::sysctl`）。模块级放开 unsafe（crate 是 `deny`
// 非 `forbid`，可覆盖），仿 win `winproc`/`service` 用法；每处 unsafe 块附 `// SAFETY:`。字节解析
// （`parse_kinfo_starttime`）是纯逻辑、跨平台可测，不含 unsafe。
#![allow(unsafe_code)]

use std::time::Duration;

/// darwin sysctl MIB 常量（`proc_starttime_darwin.go:20`：`{CTL_KERN=1, KERN_PROC=14, KERN_PROC_PID=1, pid}`）。
///
/// 这些是 darwin ABI 稳定常量（MIB 编号），不随 SDK 变。
#[cfg(target_os = "macos")]
mod sysctl_const {
    /// CTL_KERN.
    pub const CTL_KERN: i32 = 1;
    /// KERN_PROC.
    pub const KERN_PROC: i32 = 14;
    /// KERN_PROC_PID.
    pub const KERN_PROC_PID: i32 = 1;
}

/// kinfo_proc 在 64 位 darwin 上 __p_starttime 的字段偏移（`proc_starttime_darwin.go:36-37`）。
///
/// `tv_sec` 在偏移 0（int64）、`tv_usec` 在偏移 8（int32）。arm64/amd64 同布局（darwin ABI 稳定）。
/// 未 cfg-gate —— [`parse_kinfo_starttime`]（纯解析）跨平台可测时引用。
const KINFO_STARTTIME_SEC_OFFSET: usize = 0;
const KINFO_STARTTIME_USEC_OFFSET: usize = 8;
/// kinfo_proc 在 64 位 darwin 为 648 字节（`proc_starttime_darwin.go:22`，参考值）。
#[cfg(target_os = "macos")]
const KINFO_PROC_SIZE_64: usize = 648;
/// sysctl 输出缓冲长度（`proc_starttime_darwin.go:22`：给足 1024，内核经 n 回写实际长度）。
#[cfg(target_os = "macos")]
const SYSCTL_BUF_LEN: usize = 1024;

/// 把 (sec, usec) 格式化为启动时间身份串（移植自 `proc_starttime_darwin.go:43`）。
///
/// 对照 Go：`fmt.Sprintf("%d.%06d", sec, usec)`。微秒精度（6 位补零）。
///
/// 校验（`proc_starttime_darwin.go:39-42`）：sec<=0 或 usec ∉ [0,999999] 返回 None（保守不判死，
/// 不产出 "123.-00001" 畸形串）。
#[must_use]
pub fn format_start_time(sec: i64, usec: i32) -> Option<String> {
    if sec <= 0 || !(0..=999_999).contains(&usec) {
        return None;
    }
    Some(format!("{sec}.{usec:06}"))
}

/// 从 sysctl 回写的 kinfo_proc 缓冲解析 `__p_starttime`（纯逻辑，移植 `proc_starttime_darwin.go:33-37`）。
///
/// `n` = 内核经 `oldlenp` 回写的实际字节数。对照 Go：
/// ```text
/// if errno != 0 || n < 16 { return "" }
/// sec  := int64(binary.LittleEndian.Uint64(kp[0:8]))
/// usec := int32(binary.LittleEndian.Uint32(kp[8:12]))
/// ```
/// - `n < 16`（内核未填充 timeval）→ None（Go 返回 ""）。
/// - 缓冲不足 12 字节 → None（防越界，Go 缓冲恒 ≥1024）。
/// - 小端读（arm64/amd64 darwin 同为 LE，等价 `binary.LittleEndian`）。
///
/// 返回原始 `(sec, usec)`；范围校验（sec>0、usec∈[0,999999]）由 [`format_start_time`] 负责（对齐 Go
/// `proc_starttime_darwin.go:39-42` 在 Sprintf 前的校验）。
#[must_use]
pub fn parse_kinfo_starttime(buf: &[u8], n: usize) -> Option<(i64, i32)> {
    // proc_starttime_darwin.go:33: n < 16 → 取不到
    if n < 16 || buf.len() < KINFO_STARTTIME_USEC_OFFSET + 4 {
        return None;
    }
    let sec_bytes: [u8; 8] = buf[KINFO_STARTTIME_SEC_OFFSET..KINFO_STARTTIME_SEC_OFFSET + 8]
        .try_into()
        .ok()?;
    let usec_bytes: [u8; 4] = buf[KINFO_STARTTIME_USEC_OFFSET..KINFO_STARTTIME_USEC_OFFSET + 4]
        .try_into()
        .ok()?;
    Some((
        i64::from_le_bytes(sec_bytes),
        i32::from_le_bytes(usec_bytes),
    ))
}

/// 进程存活探测结果（移植自 `helper.go:336-343`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliveProbe {
    /// 进程存活且启动时间匹配快照（`kill(pid,0)==nil && start==snapshot`）。
    Alive,
    /// PID 无进程（`kill(pid,0)==ESRCH`，`helper.go:337`）。
    Dead,
    /// PID 存在但启动时间变了（PID 被复用，`helper.go:340-342`）。
    PidReused,
    /// 取不到启动时间（`cur==""`，保守维持存活，不误杀真父，`helper.go:335` 注释）。
    Unknown,
}

/// 综合判定父进程死活（移植自 `helper.go:336-343` 的 dead 判定）。
///
/// 对照 Go：
/// ```text
/// dead := false
/// if err := syscall.Kill(ppid, 0); err == syscall.ESRCH {
///     dead = true                                    // PID 无进程
/// } else if err == nil && ppidStart != "" {
///     if cur := procStartTime(ppid); cur != "" && cur != ppidStart {
///         dead = true                                // PID 复用
///     }
/// }
/// ```
///
/// 本函数把「kill(0) 结果」与「启动时间比对」组合成 [`AliveProbe`]：
/// - `exists=false` → [`AliveProbe::Dead`]（ESRCH）。
/// - `exists=true` 且 `snapshot.is_none()` → [`AliveProbe::Unknown`]（未记录快照，对齐 Go ppidStart==""）。
/// - `exists=true` 且 `current.is_none()` → [`AliveProbe::Unknown`]（取不到当前，对齐 Go cur==""）。
/// - `exists=true` 且 `current==snapshot` → [`AliveProbe::Alive`]。
/// - `exists=true` 且 `current!=snapshot` → [`AliveProbe::PidReused`]。
#[must_use]
pub fn classify_alive(exists: bool, snapshot: Option<&str>, current: Option<&str>) -> AliveProbe {
    if !exists {
        return AliveProbe::Dead;
    }
    match (snapshot, current) {
        (Some(snap), Some(cur)) if snap == cur => AliveProbe::Alive,
        (Some(_), Some(_)) => AliveProbe::PidReused,
        // 任一为 None → 保守不判死（对齐 Go: 取不到不据此判）
        _ => AliveProbe::Unknown,
    }
}

/// 探测进程是否存在（`kill(pid, 0)` 语义，mac-gated）。
///
/// 对照 Go `helper.go:337`：`syscall.Kill(ppid, 0)`。ESRCH → 不存在（false），nil → 存在（true），
/// 其它错误（EPERM 等）按存活处理（Go 源注释「root 不应见 EPERM，宁漏勿误」）。
///
/// mac 上走 nix::sys::signal::kill；非 mac 返回 true（生产永不跑非 mac，测试用 [`AliveChecker`] trait mock）。
#[must_use]
pub fn kill_zero_exists(pid: u32) -> bool {
    kill_zero_exists_inner(pid)
}

#[cfg(target_os = "macos")]
fn kill_zero_exists_inner(pid: u32) -> bool {
    // kill(pid, None) 只探活不发信号（Go `syscall.Kill(pid, 0)`）—— 不需要 Signal 类型。
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid as i32), None) {
        // kill(pid, None) 等价 kill(pid, 0)：0 = 存活
        Ok(()) => true,
        Err(nix::Error::ESRCH) => false,
        Err(_) => true, // EPERM 等：按存活（Go 源「宁漏勿误」）
    }
}

#[cfg(not(target_os = "macos"))]
fn kill_zero_exists_inner(_pid: u32) -> bool {
    // 非 mac（Linux CI）：生产永不跑此路径。
    // 注：理论上 Linux 也能用 libc::kill(pid,0)，但 helper-mac 是 mac crate，非 mac 仅编译跳过。
    // 这里返回 true（保守存活）—— watchParent 在非 mac 上本就是编译跳过的死代码。
    true
}

/// 经 sysctl 取进程启动时间（mac-gated，移植自 `proc_starttime_darwin.go:19-44`）。
///
/// mac 上调 sysctl kern.proc.pid，解析 kinfo_proc 的 __p_starttime。
/// 非 mac / 取不到 → None（对齐 Go 返回 ""，保守不判死）。
///
/// 地雷：kinfo_proc 布局需 B3 在 13/14/15 各验证一次偏移。
#[must_use]
pub fn proc_start_time(pid: u32) -> Option<String> {
    proc_start_time_inner(pid)
}

#[cfg(target_os = "macos")]
fn proc_start_time_inner(pid: u32) -> Option<String> {
    // helper.go:301-310 procStartTime：sysctl（微秒精度）优先，机制不可用则 ps lstart 兜底（秒级）。
    // 机制经 start_time_via_sysctl 一次性锁定、终身不切换（helper.go:292 startTimeViaSysctl OnceValue）——
    // 若 sysctl 快照后某次 tick 转 ps，两种格式必不等 → watchParent 误判「父已死」错杀存活父的 root sing-box。
    if start_time_via_sysctl() {
        sysctl_start_time(pid)
    } else {
        proc_start_time_via_ps(pid)
    }
}

/// 机制一次性锁定（移植 `helper.go:292` 的 `startTimeViaSysctl = sync.OnceValue`）。
///
/// 启动时对自身 PID 探测一次：sysctl 能取到 → 终身用 sysctl；否则终身用 ps。绝不中途切换（防格式漂移
/// 导致 watchParent 假阳误杀）。锁定后 sysctl 偶发失败只返 None（跳过本轮比对，保守安全）。
#[cfg(target_os = "macos")]
fn start_time_via_sysctl() -> bool {
    use std::sync::OnceLock;
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| sysctl_start_time(std::process::id()).is_some())
}

/// 经 `sysctl kern.proc.pid` 取启动时间（微秒精度，移植 `proc_starttime_darwin.go:19-44`）。
///
/// MIB = `{CTL_KERN=1, KERN_PROC=14, KERN_PROC_PID=1, pid}`（darwin ABI 稳定常量，等价 Go
/// `syscall.Syscall6(SYS___SYSCTL, &mib[0], 4, &kp[0], &n, 0, 0)`）。取不到（errno≠0 / n<16 /
/// 范围校验失败）→ None（调用方保守不判死）。
#[cfg(target_os = "macos")]
fn sysctl_start_time(pid: u32) -> Option<String> {
    use nix::libc;
    // proc_starttime_darwin.go:20: mib := [4]int32{1, 14, 1, pid}
    let mut mib: [libc::c_int; 4] = [
        sysctl_const::CTL_KERN,
        sysctl_const::KERN_PROC,
        sysctl_const::KERN_PROC_PID,
        pid as libc::c_int,
    ];
    // proc_starttime_darwin.go:22: kinfo_proc 在 64 位 darwin 为 648 字节（KINFO_PROC_SIZE_64），
    // 给足 SYSCTL_BUF_LEN=1024 缓冲（编译期恒 ≥ 648，内核经 n 回写实际长度）。
    const _: () = assert!(SYSCTL_BUF_LEN >= KINFO_PROC_SIZE_64);
    let mut buf = [0u8; SYSCTL_BUF_LEN];
    let mut n: libc::size_t = buf.len();
    // SAFETY: `mib` 是 4 元素有效数组，`mib.len()`=4 为其真实长度；`buf`/`n` 是 1024 字节有效可写缓冲
    // 及其长度（内核经 `oldlenp` 回写实际字节数，恒 ≤ 缓冲长）；`newp`=null、`newlen`=0 表示只读查询
    // （不写内核态）。返回值与 `n` 之后即被读走，指针不逃逸本调用。等价 Go 的 SYS___SYSCTL 裸 syscall。
    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &mut n,
            std::ptr::null_mut(),
            0,
        )
    };
    // proc_starttime_darwin.go:33: errno != 0 → ""
    if ret != 0 {
        return None;
    }
    // proc_starttime_darwin.go:33-42: n<16 → ""；解析 (sec,usec) → format 校验范围
    let (sec, usec) = parse_kinfo_starttime(&buf, n)?;
    format_start_time(sec, usec)
}

/// 经 `ps -o lstart=` 取启动时间（移植自 `helper.go:301-309` 的 ps 兜底路径）。
///
/// Go `helper.go:305`：`execOutput(execTimeout, "/bin/ps", "-o", "lstart=", "-p", strconv.Itoa(pid))`。
/// 这是 sysctl 不可用时的 fallback，秒级精度（比 sysctl 微秒级差，但取不到时保守不判死，安全性不变）。
///
/// 抽象为接受 [`crate::platform::macos::exec::CommandRunner`]，便于测试 mock。
#[must_use]
pub fn proc_start_time_via_ps_with_runner(
    runner: &dyn crate::platform::macos::exec::CommandRunner,
    pid: u32,
) -> Option<String> {
    let pid_str = pid.to_string();
    match runner.output(
        crate::platform::macos::exec::EXEC_TIMEOUT,
        "/bin/ps",
        &["-o", "lstart=", "-p", &pid_str],
    ) {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out).trim().to_owned();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        Err(_) => None,
    }
}

#[cfg(target_os = "macos")]
fn proc_start_time_via_ps(pid: u32) -> Option<String> {
    let runner = crate::platform::macos::exec::SystemRunner::new();
    proc_start_time_via_ps_with_runner(&runner, pid)
}

#[cfg(not(target_os = "macos"))]
fn proc_start_time_inner(_pid: u32) -> Option<String> {
    // 非 mac：生产永不跑。返回 None（保守不判死，watchParent 退化为仅 kill(0) 探测）。
    None
}

/// 父死看护的 tick 间隔（移植自 `helper.go:317`：`time.NewTicker(time.Second)`）。
pub const WATCH_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// TERM→KILL 升级的宽限窗口（移植自 `helper.go:284`：`time.After(5 * time.Second)`）。
pub const TERMINATE_GRACE: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    // ===== format_start_time（移植自 proc_starttime_darwin.go:39-43）=====

    #[test]
    fn format_start_time_valid() {
        // proc_starttime_darwin.go:43: fmt.Sprintf("%d.%06d", sec, usec)
        assert_eq!(
            format_start_time(1_700_000_000, 1234),
            Some("1700000000.001234".into())
        );
        assert_eq!(
            format_start_time(1_700_000_000, 0),
            Some("1700000000.000000".into())
        );
        assert_eq!(
            format_start_time(1_700_000_000, 999_999),
            Some("1700000000.999999".into())
        );
    }

    #[test]
    fn format_start_time_rejects_invalid() {
        // proc_starttime_darwin.go:39-42: sec<=0 / usec<0 / usec>999999 → 返回空（这里 None）
        assert_eq!(format_start_time(0, 0), None);
        assert_eq!(format_start_time(-1, 0), None);
        assert_eq!(format_start_time(100, -1), None);
        assert_eq!(format_start_time(100, 1_000_000), None);
    }

    // ===== parse_kinfo_starttime（移植 proc_starttime_darwin.go:33-37，纯解析跨平台可测）=====

    fn kinfo_buf(sec: i64, usec: i32) -> Vec<u8> {
        // tv_sec @0 (int64 LE), tv_usec @8 (int32 LE)，其余填充到 648 字节（模拟内核回写）
        let mut b = vec![0u8; 648];
        b[0..8].copy_from_slice(&sec.to_le_bytes());
        b[8..12].copy_from_slice(&usec.to_le_bytes());
        b
    }

    #[test]
    fn parse_kinfo_starttime_reads_le_fields() {
        // proc_starttime_darwin.go:36-37: sec @0, usec @8 小端
        let buf = kinfo_buf(1_700_000_000, 123_456);
        assert_eq!(
            parse_kinfo_starttime(&buf, 648),
            Some((1_700_000_000, 123_456))
        );
        // 结合 format：得微秒身份串
        let (s, u) = parse_kinfo_starttime(&buf, 648).unwrap();
        assert_eq!(
            format_start_time(s, u).as_deref(),
            Some("1700000000.123456")
        );
    }

    #[test]
    fn parse_kinfo_starttime_rejects_short_n() {
        // proc_starttime_darwin.go:33: n < 16 → None
        let buf = kinfo_buf(1, 1);
        assert_eq!(parse_kinfo_starttime(&buf, 15), None);
        assert_eq!(parse_kinfo_starttime(&buf, 0), None);
        // n>=16 边界可解析
        assert!(parse_kinfo_starttime(&buf, 16).is_some());
    }

    #[test]
    fn parse_kinfo_starttime_rejects_short_buf() {
        // 缓冲不足 12 字节 → None（防越界；生产缓冲恒 1024）
        let short = vec![0u8; 8];
        assert_eq!(parse_kinfo_starttime(&short, 648), None);
    }

    // ===== classify_alive（移植自 helper.go:336-343）=====

    #[test]
    fn classify_alive_dead_when_not_exists() {
        // helper.go:337: kill(pid,0)==ESRCH → dead
        assert_eq!(classify_alive(false, None, None), AliveProbe::Dead);
        assert_eq!(
            classify_alive(false, Some("x"), Some("x")),
            AliveProbe::Dead
        );
    }

    #[test]
    fn classify_alive_when_start_matches() {
        // helper.go:339-340: kill==nil && cur==ppidStart → 存活
        assert_eq!(
            classify_alive(true, Some("100.000001"), Some("100.000001")),
            AliveProbe::Alive
        );
    }

    #[test]
    fn classify_pid_reused_when_start_differs() {
        // helper.go:340-342: kill==nil 但 cur != ppidStart → PID 复用 → dead
        assert_eq!(
            classify_alive(true, Some("100.000001"), Some("200.000002")),
            AliveProbe::PidReused
        );
    }

    #[test]
    fn classify_unknown_when_snapshot_missing() {
        // helper.go:335: ppidStart=="" → 不据此判死（保守存活）
        assert_eq!(
            classify_alive(true, None, Some("100.000001")),
            AliveProbe::Unknown
        );
    }

    #[test]
    fn classify_unknown_when_current_missing() {
        // helper.go:335: cur==""（ps 失败/超时）→ 不据此判死
        assert_eq!(
            classify_alive(true, Some("100.000001"), None),
            AliveProbe::Unknown
        );
    }

    // ===== proc_start_time_via_ps_with_runner =====

    struct PsMock {
        retval: Mutex<Option<Vec<u8>>>,
    }

    impl crate::platform::macos::exec::CommandRunner for PsMock {
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
            self.retval.lock().unwrap().clone().ok_or(
                crate::platform::macos::exec::RunError::NonZero {
                    code: 1,
                    output: std::process::Output {
                        status: std::process::ExitStatus::default(),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    },
                },
            )
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

    #[test]
    fn proc_start_time_via_ps_parses_output() {
        // helper.go:305-309: ps -o lstart= -p <pid> 取启动时间字符串
        let m = PsMock {
            retval: Mutex::new(Some(b"Mon Jan  1 10:00:00 2024\n".to_vec())),
        };
        let t = proc_start_time_via_ps_with_runner(&m, 1234);
        assert_eq!(t.as_deref(), Some("Mon Jan  1 10:00:00 2024"));
    }

    #[test]
    fn proc_start_time_via_ps_empty_returns_none() {
        // helper.go:308: strings.TrimSpace(out) 为空 → 返回 ""
        let m = PsMock {
            retval: Mutex::new(Some(b"   \n".to_vec())),
        };
        assert!(proc_start_time_via_ps_with_runner(&m, 1234).is_none());
    }

    #[test]
    fn proc_start_time_via_ps_error_returns_none() {
        // helper.go:305: err != nil → 返回 ""（保守不判死）
        let m = PsMock {
            retval: Mutex::new(None),
        };
        assert!(proc_start_time_via_ps_with_runner(&m, 1234).is_none());
    }

    // ===== 常量锁定 =====

    #[test]
    fn watch_tick_and_grace_match_go_source() {
        // helper.go:284: time.After(5 * time.Second)
        // helper.go:317: time.NewTicker(time.Second)
        assert_eq!(WATCH_TICK_INTERVAL, Duration::from_secs(1));
        assert_eq!(TERMINATE_GRACE, Duration::from_secs(5));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn kill_zero_exists_for_self() {
        // 自身进程必然存在
        let pid = std::process::id();
        assert!(kill_zero_exists(pid));
        // PID 0 不存在（init 在 mac 上是 1，0 是 kernel_task 但 kill(0,0) 语义特殊）
        // 用一个大不存在的 PID 测：不保证 ESRCH（可能被复用），跳过断言方向
        let _ = kill_zero_exists(999_999);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_sysctl_consts_match_darwin_abi() {
        // proc_starttime_darwin.go:20: mib := [4]int32{1, 14, 1, pid}
        assert_eq!(sysctl_const::CTL_KERN, 1);
        assert_eq!(sysctl_const::KERN_PROC, 14);
        assert_eq!(sysctl_const::KERN_PROC_PID, 1);
        // proc_starttime_darwin.go:36-37: tv_sec @0, tv_usec @8
        assert_eq!(KINFO_STARTTIME_SEC_OFFSET, 0);
        assert_eq!(KINFO_STARTTIME_USEC_OFFSET, 8);
        // proc_starttime_darwin.go:22: kinfo_proc 648 bytes
        assert_eq!(KINFO_PROC_SIZE_64, 648);
    }
}
