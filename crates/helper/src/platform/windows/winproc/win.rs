//! Windows FFI 实现（`#[cfg(windows)]`，移植自 `helper-win/winproc.go` 的全部副作用原语）。
//!
//! 本文件实现 [`crate::platform::windows::ops::ProcOps`] + [`crate::platform::windows::ops::NetTableOps`] 的生产版本：直接调 windows-sys FFI
//! 替代 Go 的 `golang.org/x/sys/windows`。每处 unsafe 附 SAFETY 理由。

// 局部放开 crate 级 `#![deny(unsafe_code)]`：windows-sys FFI 调用（OpenProcess/TerminateProcess/
// CreateProcessW/...）必须 unsafe。每处 unsafe 块附 SAFETY 理由。本 allow 仅对 Windows target 生效。
#![allow(unsafe_code)]

use crate::platform::windows::logic::{
    eq_ignore_ascii_case_path, filepath_base, filepath_dir, filter_listen_pids, is_locked_singbox,
    local_port_from_net_order, AF_INET, AF_INET6, MIB_TCP_STATE_LISTEN,
    TCP_TABLE_OWNER_PID_LISTENER,
};
use crate::platform::windows::ops::{NetTableOps, ProcOps};
use crate::platform::windows::selfuninstall::self_uninstall_cmd_line;
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, SetHandleInformation, BOOL, ERROR_INVALID_PARAMETER, FALSE, HANDLE,
    HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, TRUE,
};
use windows_sys::Win32::NetworkManagement::IpHelper::GetExtendedTcpTable;
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Console::{
    AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
    ATTACH_PARENT_PROCESS, CTRL_BREAK_EVENT,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE,
    REG_DWORD, REG_OPTION_NON_VOLATILE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE, STARTF_USESTDHANDLES,
    STARTUPINFOW,
};

/// IPv4 TCP owner PID 行（`winproc.go:259-266` `MIB_TCPROW_OWNER_PID`）。
///
/// 与 Windows API 内存布局逐字段对齐（`#[repr(C)]` 保证无 padding）。
#[repr(C)]
struct MibTcpRowOwnerPid {
    state: u32,
    local_addr: u32,
    local_port: u32, // 网络字节序，仅低 16 位有效
    remote_addr: u32,
    remote_port: u32,
    owning_pid: u32,
}

/// IPv6 TCP owner PID 行（`winproc.go:268-278` `MIB_TCP6ROW_OWNER_PID`）。
#[repr(C)]
struct MibTcp6RowOwnerPid {
    local_addr: [u8; 16],
    local_scope_id: u32,
    local_port: u32,
    remote_addr: [u8; 16],
    remote_scope_id: u32,
    remote_port: u32,
    state: u32,
    owning_pid: u32,
}

/// STILL_ACTIVE（`winproc.go:141`，GetExitCodeProcess 的活跃码 = 259）。
const STILL_ACTIVE_CODE: u32 = 259;

/// Windows FFI 生产实现（对应 Go `winproc.go` 全部原语）。
#[derive(Debug)]
pub struct WinProcOps {
    /// 常驻 Job Object 句柄（`winproc.go:71` `hJob`）。
    /// 惰性创建（ensure_job），所有 child assign 进去 → helper 死则内核连坐杀 child。
    /// Mutex 保证 ensure_job 的「已建则直接返回」幂等性（`winproc.go:75-78`）。
    job: std::sync::Mutex<Option<HANDLE>>,
}

impl Default for WinProcOps {
    fn default() -> Self {
        Self {
            job: std::sync::Mutex::new(None),
        }
    }
}

impl WinProcOps {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

// SAFETY: WinProcOps 的 job HANDLE 是内核对象句柄，跨线程共享安全（Windows 句柄本身线程无关）。
// Mutex 保护句柄的惰性创建，满足 Send + Sync。
unsafe impl Send for WinProcOps {}
unsafe impl Sync for WinProcOps {}

impl WinProcOps {
    /// ensureJob（`winproc.go:75-96`）：惰性创建常驻 job 并设 KILL_ON_JOB_CLOSE。幂等。
    fn ensure_job(&self) -> Option<HANDLE> {
        let mut guard = self.job.lock().unwrap();
        if let Some(h) = *guard {
            return Some(h);
        }
        // SAFETY: CreateJobObjectW(NULL, NULL) 创建匿名 job（无名、默认安全描述符），线程安全。
        // 失败返回 NULL。句柄由本结构持有到进程退出（内核自动关闭 → KILL_ON_JOB_CLOSE 生效）。
        let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if h.is_null() {
            return None;
        }
        // SAFETY: SetInformationJobObject 设置 extended limit。info 在本栈帧存活期间调用有效。
        // JOBOBJECT_EXTENDED_LIMIT_INFORMATION 按 Win32 布局（#[repr(C)] 由 windows-sys 保证）。
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: 同上。失败 → 关句柄返 None（不泄漏）。
        let ok = unsafe {
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            // SAFETY: 关闭未成功的 job 句柄，避免泄漏。
            unsafe { CloseHandle(h) };
            return None;
        }
        *guard = Some(h);
        Some(h)
    }

    /// assignToJob（`winproc.go:105-115`）：把 child pid assign 进常驻 job。best-effort。
    fn assign_to_job(&self, h_job: HANDLE, pid: u32) {
        if h_job.is_null() || pid == 0 {
            return;
        }
        // SAFETY: OpenProcess 取 child 句柄。PID 复用安全：调用点在 Start 成功后立即执行，
        // child 必活、pid 必指向本 child（Go winproc.go:102-103 同款论证）。失败返 NULL，无害。
        let h = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, FALSE, pid) };
        if h.is_null() {
            return;
        }
        // SAFETY: AssignProcessToJobObject 把 h 进程 assign 进 h_job。best-effort：失败无害。随后关句柄。
        unsafe {
            let _ = AssignProcessToJobObject(h_job, h);
            CloseHandle(h);
        }
    }
}

impl ProcOps for WinProcOps {
    fn process_alive(&self, ppid: u32) -> bool {
        // winproc.go:123-143 processAlive。委托无状态自由函数（watchParent 后台线程共用，不捕获 &self）。
        process_alive_raw(ppid)
    }

    fn terminate_pid(&self, pid: u32) -> std::io::Result<()> {
        // winproc.go:146-153 terminatePid。委托无状态自由函数（收割序列/freeport 共用，不捕获 &self）。
        terminate_pid_raw(pid)
    }

    fn process_image_name(&self, pid: u32) -> String {
        // winproc.go:157-169 processImageName：QueryFullProcessImageNameW。失败返回 ""。
        // SAFETY: OpenProcess(QUERY_LIMITED_INFORMATION) 取 pid 句柄。
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
        if h.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 260]; // MAX_PATH
        let mut size = buf.len() as u32;
        // SAFETY: QueryFullProcessImageNameW 写入 buf（wide string）。flags=0 = Win32 路径。
        let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size) };
        // SAFETY: 关句柄。
        unsafe { CloseHandle(h) };
        if ok == 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..size as usize])
    }

    fn kill_all_singbox(&self, singbox_bin: &str) -> usize {
        // winproc.go:184-214 killAllSingbox：CreateToolhelp32Snapshot 枚举进程，按映像全路径 EqualFold
        // 匹配 singbox_bin → TerminateProcess。返回杀掉的实例数。
        if singbox_bin.is_empty() {
            return 0;
        }
        // SAFETY: CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) 取系统进程快照。返回 INVALID_HANDLE_VALUE 失败。
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return 0;
        }
        let mut pe: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut killed = 0usize;
        let target_base = filepath_base(singbox_bin);
        // SAFETY: Process32FirstW/NextW 遍历快照。pe 由上方 zeroed + dwSize 初始化。
        let mut ok = unsafe { Process32FirstW(snap, &mut pe) };
        while ok != 0 {
            let pid = pe.th32ProcessID;
            if pid != 0 && pid != 4 {
                // System Idle / System 跳过（winproc.go:198-200）
                // 先按 ExeFile（basename，UTF-16）粗筛，命中再取全路径精确比对。
                let base = wide_to_string(&pe.szExeFile);
                if eq_ignore_ascii_case_path(&base, target_base) {
                    let img = self.process_image_name(pid);
                    // clippy collapsible_if：两层内 if 合并（&& 短路保 terminate 仅在锁定命中时调用）。
                    if is_locked_singbox(&img, singbox_bin) && terminate_pid_raw(pid).is_ok() {
                        killed += 1;
                    }
                }
            }
            // SAFETY: 同上。
            ok = unsafe { Process32NextW(snap, &mut pe) };
        }
        // SAFETY: 关快照句柄。
        unsafe { CloseHandle(snap) };
        killed
    }

    fn start_singbox(
        &self,
        singbox_bin: &str,
        cfg: &str,
        log_path: &str,
        fwd: bool,
    ) -> std::io::Result<u32> {
        // winproc.go:21-49 startSingbox + winproc.go:414-427 enableIPForwarding。
        if fwd {
            self.enable_ip_forwarding();
        }
        // 构造命令行：singbox_bin run -c cfg（Go: exec.Command(singboxBin, "run", "-c", cfg)）。
        let args = ["run", "-c", cfg];
        let cmdline = build_command_line(singbox_bin, &args);
        // CreateProcessW 需可写 wide lpCommandLine。
        let mut cmdline_w: Vec<u16> = OsString::from(cmdline)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        // W4 修：log 重定向（Go winproc.go:28-35：logPath!="" 时 c.Stdout=c.Stderr=logFile，
        // O_CREATE|O_WRONLY|O_APPEND）。此前 `let _ = log_path` 丢弃 → 启动诊断日志无处可查。
        // 手法：std::fs 开文件拿句柄 → SetHandleInformation 置可继承 → STARTUPINFOW 的 std 句柄 +
        // bInheritHandles=TRUE 传给 child。句柄须活到 CreateProcessW 之后（child 继承独立副本）；
        // 父副本随 File drop（函数末）关闭，避免每次启停泄漏。stdin 指向 NUL（Go os/exec 对 nil Stdin
        // 的等价），避免 child 拿到无效 stdin 句柄。
        // 其余句柄（管道 SA bInheritHandle=FALSE、Job/OpenProcess 默认非继承）→ 不随 bInheritHandles 泄漏。
        let mut log_file: Option<std::fs::File> = None;
        let mut nul_file: Option<std::fs::File> = None;
        let mut inherit_handles = FALSE;
        if !log_path.is_empty() {
            if let Ok(lf) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
            {
                let lh = lf.as_raw_handle() as HANDLE;
                // SAFETY: 置 log 句柄 HANDLE_FLAG_INHERIT（child 经 bInheritHandles=TRUE 继承副本）。
                unsafe { SetHandleInformation(lh, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
                si.dwFlags |= STARTF_USESTDHANDLES;
                si.hStdOutput = lh;
                si.hStdError = lh;
                if let Ok(nf) = std::fs::OpenOptions::new().read(true).open("NUL") {
                    let nh = nf.as_raw_handle() as HANDLE;
                    // SAFETY: 同上，NUL stdin 句柄置可继承。
                    unsafe { SetHandleInformation(nh, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
                    si.hStdInput = nh;
                    nul_file = Some(nf);
                } else {
                    // NUL 打不开（极罕见）：stdin 复用 log 句柄（sing-box run 不读 stdin，无害）。
                    si.hStdInput = lh;
                }
                inherit_handles = TRUE;
                log_file = Some(lf);
            }
        }
        // CWD = 配置文件所在目录（= 用户可写 config 目录）。**不设的后果不是噪音，是写错地方**：
        // helper 是 SCM 服务，进程 CWD 恒为 `C:\Windows\System32`，child 不设就继承它，而 sing-box
        // 对配置里的**相对**路径按 CWD 解析。1.14.0-beta.15 起 tailscale endpoint 的
        // `taildrop_directory` 默认值就是相对的 `Taildrop`，且在 initialize 阶段无条件 `MkdirAll(0700)`
        // ⇒ 目录建在 System32 里，tailnet peer 发来的文件也落在那；helper 跑在 SYSTEM 下，这个 mkdir
        // 还会**成功**，于是没有任何报错。另一条更老的同型：`services[].dashboard` 省略 `path` 时的
        // 联网下载兜底目录 `dashboard`。
        //
        // App 直起（`runtime/proxy.rs`）、Linux helper（`platform/linux/server.rs`）、macOS helper
        // （`platform/macos/server.rs`）三条腿早就设了，本腿是漏的那条 —— 同一根因下做对的三条腿，
        // 正是「这不是有意取舍」的证据。取父目录的方式与 Linux 腿同（配置文件的所在目录），只是不能用
        // `std::path`（见 `logic::filepath_dir`）。取不到父目录（裸文件名）→ 不设，保持旧行为。
        let cwd_w: Option<Vec<u16>> = filepath_dir(cfg).map(|dir| {
            OsString::from(dir)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        });
        let cwd_ptr = cwd_w.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: CreateProcessW 启动 child。CREATE_NEW_PROCESS_GROUP 使 child 可被 CTRL_BREAK 投递
        //（服务模式 no-op，见 send_ctrl_break 注释）。CREATE_NO_WINDOW 避免黑窗。
        // bInheritHandles=inherit_handles（有 log 重定向时 TRUE，让 child 继承 STARTUPINFOW 的 std 句柄；
        // 其余句柄均非可继承 → 不泄漏）。lpApplicationName=NULL → lpCommandLine 首 token 作 exe。
        // lpCurrentDirectory=cwd_ptr（`cwd_w` 须活到本调用之后，故在同作用域持有到函数末）。
        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmdline_w.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                inherit_handles,
                CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
                std::ptr::null(),
                cwd_ptr,
                &si,
                &mut pi,
            )
        };
        // 父副本句柄在此关闭（child 已继承独立副本）；防每次启停泄漏。
        drop(log_file);
        drop(nul_file);
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let pid = pi.dwProcessId;
        // SAFETY: 关闭 CreateProcessW 返回的 thread/process 句柄（我们只用 pid + Job Object 管生命周期，
        // 不直接 Wait —— child 的 reap 由 reap_child 经 OpenProcess(pid) + TerminateProcess 完成）。
        unsafe {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
        }
        // ensureJob + assignToJob（winproc.go:40-47）：best-effort 防孤儿安全网。失败不阻断 start。
        if let Some(h_job) = self.ensure_job() {
            self.assign_to_job(h_job, pid);
        }
        Ok(pid)
    }

    fn reap_child(&self, pid: u32) {
        // W6 修：后台异步收割（Go stop/cleanup/uninstall 的 `go terminateChild(c, done)`）——不阻塞
        // 管道回复（此前同步 sleep(2s) 阻塞 stop 回复）。线程只捕获 pid + 无状态 pid FFI
        //（send_ctrl_break/terminate_pid_raw），不捕获 &self（故无生命周期问题）。
        std::thread::spawn(move || reap_sequence(pid));
    }

    fn reap_child_blocking(&self, pid: u32) {
        // 同步收割（Go reapChildOnExit 的**同步** terminateChild）：服务停止/关机路径须在返回前杀完
        // child，否则异步收割线程随进程退出消失 → 孤儿。
        reap_sequence(pid);
    }

    fn apply_route(&self, iface: &str, cidr: &str, del: bool) {
        // W9 修：真跑 netsh（Go helper.go:222-240）。此前 helper 分派层空壳回 OK、netsh 从未执行。
        // 地址族由 cidr 是否含 ':' 决定（Go 同款）；store=active 非持久（重启自清）。best-effort（Go .Run()）。
        let op = if del { "delete" } else { "add" };
        let fam = if cidr.contains(':') { "ipv6" } else { "ipv4" };
        let _ = std::process::Command::new("netsh")
            .arg("interface")
            .arg(fam)
            .arg(op)
            .arg("route")
            .arg(format!("prefix={cidr}"))
            .arg(format!("interface={iface}"))
            .arg("store=active")
            .status();
    }

    fn spawn_watch_parent(
        &self,
        ppid: u32,
        child_pid: u32,
        is_current: Box<dyn Fn(u32) -> bool + Send>,
        on_parent_dead: Box<dyn Fn(u32) + Send>,
    ) {
        // W15 修：接线父死看护（Go helper.go:131-159 `watchParent` + `go watchParent`）。此前
        // parent_pid 被丢弃、看护零调用 → app 崩溃/taskkill（管道 stop 够不到）后 sing-box 成孤儿
        //（Job 只兜「helper 自己死」，管不到「app≠helper 死、helper 仍活」）。
        // 后台线程只捕获 pid + 闭包 + 无状态 process_alive_raw（不捕获 &self）。
        std::thread::spawn(move || {
            let interval = std::time::Duration::from_secs(1); // Go: time.NewTicker(time.Second)
            loop {
                std::thread::sleep(interval);
                if !is_current(child_pid) {
                    return; // 已被 stop/cleanup/新 start 摘除（Go: child != c）
                }
                // processAlive：仅「确定父已死」返回 false（宁漏勿误）。
                if !process_alive_raw(ppid) {
                    if !is_current(child_pid) {
                        return; // 与 stop 竞态：他人已摘除则由他收割
                    }
                    on_parent_dead(child_pid);
                    return;
                }
            }
        });
    }

    fn spawn_self_uninstall(&self, service_name: &str, support_dir: &str) {
        // winproc.go:233-245 spawnSelfUninstall：CreateProcessW(cmd.exe, CmdLine 原样下发,
        // DETACHED_PROCESS|CREATE_NEW_PROCESS_GROUP)。旁路须比 helper 活得久（不 assignToJob、继承 SYSTEM token）。
        let cmd_line = self_uninstall_cmd_line(service_name, support_dir);
        let cmd_exe = std::env::var_os("ComSpec").unwrap_or_else(|| {
            let root =
                std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from("C:\\Windows"));
            // clippy useless_conversion：root 已是 OsString，无需再 OsString::from。
            let mut p = root;
            p.push("\\System32\\cmd.exe");
            p
        });
        let app_w = wide_null(&cmd_exe);
        let mut cmd_w = wide_null(OsString::from(cmd_line));
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: CreateProcessW(cmd.exe, CmdLine 原样)。DETACHED_PROCESS → 无 console、不随 helper 退出被收。
        // CREATE_NEW_PROCESS_GROUP → 旁路独立进程组。不 assignToJob → 不受 KILL_ON_JOB_CLOSE 连坐。
        // 继承 helper 的 SYSTEM token（子进程默认继承父 token）→ 有权 sc delete / 删 ProgramData。
        // lpApplicationName 用绝对 cmd.exe 路径（不依赖 SYSTEM 服务的 %PATH%）。
        let ok = unsafe {
            CreateProcessW(
                app_w.as_ptr(),
                cmd_w.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                FALSE,
                DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            )
        };
        if ok != 0 {
            // SAFETY: 关句柄（不 Wait —— 旁路须比 helper 活得久，Go: 不 Wait，winproc.go:244）。
            unsafe {
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
            }
        }
        // best-effort：失败只记 log（Go: _ = c.Start()）。
        log::warn!("spawn_self_uninstall completed (ok={ok})");
    }

    fn enable_ip_forwarding(&self) {
        // winproc.go:414-427 enableIPForwarding：IPv4 注册表 IPEnableRouter=1（持久写入）+
        // IPv6 netsh（运行时开关）。best-effort，失败不阻塞 start。
        enable_ip_forwarding_ipv4_registry();
        let _ = std::process::Command::new("netsh")
            .args(["interface", "ipv6", "set", "global", "forwarding=enabled"])
            .spawn();
    }
}

impl NetTableOps for WinProcOps {
    fn list_listen_entries(
        &self,
    ) -> std::io::Result<Vec<crate::platform::windows::logic::ListenEntry>> {
        // winproc.go:302-406 listenPidsForPort：GetExtendedTcpTable 枚举 IPv4 + IPv6 LISTEN 持有者。
        let mut out = Vec::new();
        for &family in &[AF_INET, AF_INET6] {
            let rows = enum_tcp_listener_rows(family)?;
            for (pid, net_port) in rows {
                let port = local_port_from_net_order(net_port);
                out.push(crate::platform::windows::logic::ListenEntry { pid, port });
            }
        }
        Ok(out)
    }

    fn listen_pids_for_port(&self, port: u16) -> std::io::Result<Vec<u32>> {
        let entries = self.list_listen_entries()?;
        Ok(filter_listen_pids(&entries, port))
    }
}

/// sendCtrlBreak（`winproc.go:57-59`）：向指定进程组投递 CTRL_BREAK_EVENT。
///
/// 重要限制：GenerateConsoleCtrlEvent 只路由给与调用者共享 console 的进程。本 helper 作为 session-0 的
/// LocalSystem 服务无 console ⇒ 服务模式下这一发**必然失败**（实测 `GetLastError=6`
/// `ERROR_INVALID_HANDLE`）。仅 --console dev 模式（有 console）下直接有效。
///
/// ⚠️ 这里此前的注释写的是「服务模式无 console → 返回 0，**无害**」。前半对，后半错，
/// 而且是最贵的那一半：它把「这条路走不通」写成了「这件事做不到」，于是没人再去找第二条路，
/// Windows 上 sing-box 的 `Store.Close()` 永不执行就此成为既定现实。
/// 第二条路见 [`send_ctrl_break_via_child_console`]（2026-08-12 G3 探针实测成立）。
fn send_ctrl_break(pid: u32) -> std::io::Result<()> {
    // SAFETY: GenerateConsoleCtrlEvent 投递控制信号。pid 为 child 进程组组长。
    let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// 本进程的 console 控制处理器：吞掉事件并报告「已处理」。
///
/// 装它是为了在 [`send_ctrl_break_via_child_console`] 借用子进程 console 的那个窗口里，
/// **不被自己发出的 CTRL_BREAK 带走**。返回 TRUE = 已处理，不走默认的「终止本进程」。
unsafe extern "system" fn ignore_ctrl_handler(_event: u32) -> BOOL {
    1
}

/// 无 console 时的第二条路：**借用子进程自己的 console** 再投一次 CTRL_BREAK。
///
/// # 依据（2026-08-12 实测，非推断）
///
/// G3 探针（`crates/helper/probes/ctrl_break_probe.rs`，run `31591517111`）在**真实 Windows 服务**
/// （session 0、从没碰过 console）里逐格测过：
/// - 直接 `GenerateConsoleCtrlEvent` → `api_ok=false`、`err=6`；
/// - `FreeConsole` → `AttachConsole(child)` → 重投 → 子进程**走到优雅退出分支**（`attach_ok=true`）。
///
/// 两个常见误读一并否掉：
/// - MSDN 说 `CREATE_NO_WINDOW` 下 "the console handle for the application is not set" ——
///   实测 `AttachConsole(child)` **成功**，子进程确实有一个可 attach 的 console，只是不属于父进程那个；
/// - **不需要**先给服务 `AllocConsole`：上面那组数据就是在从没碰过 console 的进程上取得的。
///
/// # 两处必须按这个顺序
///
/// 1. `SetConsoleCtrlHandler` 必须传**自己的 handler**，不能用 `SetConsoleCtrlHandler(NULL, TRUE)` ——
///    后者的语义只是「忽略 CTRL+C」，**对 CTRL_BREAK 无效**，不装的话这一发可能把 helper 自己带走
///    （探针首跑就是这样静默退出的）。
/// 2. 收尾要 `FreeConsole` 再**摘掉** handler：一直挂着会让 --console dev 模式下的 Ctrl+C 也失效。
///    摘除排在 `FreeConsole` 之后 —— 先脱离 console 就已经不在收件范围里了。
fn send_ctrl_break_via_child_console(pid: u32) -> std::io::Result<()> {
    // 服务模式下本来就没有 console（返回 0，无害）；dev --console 模式下必须先脱离，
    // 否则 AttachConsole 会失败——一个进程同一时刻只能连一个 console。
    // SAFETY: 无参数。
    unsafe { FreeConsole() };

    // SAFETY: attach 到子进程的 console。
    if unsafe { AttachConsole(pid) } == 0 {
        let e = std::io::Error::last_os_error();
        // 尽力还原 dev 模式下刚被丢掉的那个 console（服务里没有可还原的，失败无害）。
        // SAFETY: 常量 ATTACH_PARENT_PROCESS。
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
        return Err(e);
    }

    // SAFETY: handler 是 'static fn，无捕获。
    unsafe { SetConsoleCtrlHandler(Some(ignore_ctrl_handler), 1) };
    // SAFETY: 同 send_ctrl_break，此刻已与子进程共享 console。
    let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    let err = if ok == 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };

    // SAFETY: 别把子进程的 console 一直攥着。
    unsafe { FreeConsole() };
    // SAFETY: 摘掉自己的 handler（第二个参数 FALSE = remove），否则 dev 模式的 Ctrl+C 会永久失效。
    unsafe { SetConsoleCtrlHandler(Some(ignore_ctrl_handler), 0) };
    // SAFETY: 还原 dev 模式的 console。
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };

    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// 宽限期内轮询子进程是否已退出。返回 `true` = **确定已死**。
///
/// [`process_alive_raw`] 的口径是「宁漏勿误」（不确定按存活），故本函数返回 false 只表示
/// 「没能确认它死了」，不表示它还活着 —— 调用方据此走硬杀兜底，方向是安全的。
fn wait_child_exit(pid: u32, grace: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        if !process_alive_raw(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    !process_alive_raw(pid)
}

/// 收割序列（`helper.go:111-123` `terminateChild`）：优雅信号 CTRL_BREAK → 宽限 2s → 硬杀。
///
/// **无状态**（仅 pid，无 `&self`），供异步 [`WinProcOps::reap_child`] 与同步
/// [`WinProcOps::reap_child_blocking`] 共用（W6：命令路径异步不阻塞、退出路径同步防孤儿）。
fn reap_sequence(pid: u32) {
    // ① 直接投递。dev --console 模式下本就有效；服务模式下必然失败（err=6），失败即走 ②。
    if send_ctrl_break(pid).is_err() {
        // ② 借子进程自己的 console 再投一次。**这一步才是服务模式下唯一能走通的优雅通道**
        //    （2026-08-12 G3 探针实测，见该函数文档）。任一步失败都只是回到 ③ 硬杀，
        //    即「不比改动前更差」——天然 fail-safe，故失败只吞不报。
        let _ = send_ctrl_break_via_child_console(pid);
    }

    // ③ 宽限期内轮询而不是死等满 2s（helper.go:120 的宽限值不变）。
    //    改动前这里恒睡满 2 秒是**合理的**：那时优雅信号从来不生效，早醒也没有意义。
    //    现在它能生效了，轮询把「停核」从恒定 2 秒缩到实际退出耗时。
    let exited = wait_child_exit(pid, std::time::Duration::from_millis(2000));

    // ④ 硬杀兜底。**确认已死就不发这一刀** —— 不是为了省一次系统调用，而是躲开
    //    「pid 已被回收并复用给别的进程」这一格：那种情况下这一刀会砍到无关进程。
    //    `process_alive_raw` 宁漏勿误，故 `exited==false` 时照旧硬杀，方向安全。
    if !exited {
        let _ = terminate_pid_raw(pid);
    }
}

/// 硬杀指定 pid（`winproc.go:146-153` `terminatePid`）。**无状态**自由函数（收割序列 / trait `terminate_pid`
/// / kill_all_singbox 共用）。
fn terminate_pid_raw(pid: u32) -> std::io::Result<()> {
    // SAFETY: OpenProcess(TERMINATE) 取 pid 句柄。pid 来自 freeport/cleanup 的 LISTEN 持有者枚举或收割。
    let h = unsafe { OpenProcess(PROCESS_TERMINATE, FALSE, pid) };
    if h.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: TerminateProcess(h, 1) 硬杀。exit code=1（Go 同款）。
    let ok = unsafe { TerminateProcess(h, 1) };
    // SAFETY: 关句柄。
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// 判 ppid 是否存活（`winproc.go:123-143` `processAlive`）。**无状态**自由函数（watchParent 后台线程 /
/// trait `process_alive` 共用，不捕获 `&self`）。
///
/// 宁漏勿误（对齐 macOS `kill(ppid,0)`）：仅「确定已死」（`ERROR_INVALID_PARAMETER` = pid 不存在）返回
/// false；其余不确定（ACCESS_DENIED / 查询失败）按存活返回 true。
fn process_alive_raw(ppid: u32) -> bool {
    if ppid == 0 {
        return false;
    }
    let access = SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
    // SAFETY: OpenProcess 取父进程句柄。pid 来自客户端（已被 token 鉴权为可信调用方）。
    let h = unsafe { OpenProcess(access, FALSE, ppid) };
    if h.is_null() {
        // ERROR_INVALID_PARAMETER（87，pid 已不存在）→ 确定已死；其余不确定 → 按存活。
        return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
    }
    let mut code: u32 = 0;
    // SAFETY: GetExitCodeProcess 写入 code。h 由上方 OpenProcess 返回（非 NULL）。
    let ok = unsafe { GetExitCodeProcess(h, &mut code) };
    // SAFETY: 关句柄。
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return true; // 查询失败 → 不确定 → 按存活
    }
    code == STILL_ACTIVE_CODE
}

/// 枚举指定 family（IPv4/IPv6）的 LISTEN 持有者（pid + 网络序 port）。
///
/// 两步法：首次调 GetExtendedTcpTable 取所需缓冲大小（返回 ERROR_INSUFFICIENT_BUFFER），
/// 再次调用填充。空表（size=4，仅 dwNumEntries=0 头）防越界（winproc.go:345-353）。
fn enum_tcp_listener_rows(family: u32) -> std::io::Result<Vec<(u32, u32)>> {
    let mut size: u32 = 0;
    // SAFETY: 首次调用取大小（pTcpTable=NULL）。返回 ERROR_INSUFFICIENT_BUFFER 预期。
    let rc = unsafe {
        GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            1, // sorted=TRUE（BOOL i32）
            family,
            TCP_TABLE_OWNER_PID_LISTENER as i32,
            0,
        )
    };
    // ERROR_INSUFFICIENT_BUFFER = 122
    if rc != 0 && rc != 122 {
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; size as usize];
    // SAFETY: 第二次调用填充 buf（按 size 分配足够）。
    let rc = unsafe {
        GetExtendedTcpTable(
            buf.as_mut_ptr() as *mut _,
            &mut size,
            1,
            family,
            TCP_TABLE_OWNER_PID_LISTENER as i32,
            0,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    // 解析：首 4 字节 = dwNumEntries（行数），其后是行数组。空表防越界。
    if buf.len() < 4 {
        return Ok(Vec::new());
    }
    let n = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(n as usize);
    if family == AF_INET {
        let row_size = std::mem::size_of::<MibTcpRowOwnerPid>();
        for i in 0..n as usize {
            let off = 4 + i * row_size;
            if off + row_size > buf.len() {
                break; // 防越界（GetExtendedTcpTable 保证表完整，但保守）
            }
            // SAFETY: off 已校验在 buf 范围内；#[repr(C)] 保证布局。
            let row: &MibTcpRowOwnerPid =
                unsafe { &*(buf.as_ptr().add(off) as *const MibTcpRowOwnerPid) };
            if row.state == MIB_TCP_STATE_LISTEN {
                out.push((row.owning_pid, row.local_port));
            }
        }
    } else {
        let row_size = std::mem::size_of::<MibTcp6RowOwnerPid>();
        for i in 0..n as usize {
            let off = 4 + i * row_size;
            if off + row_size > buf.len() {
                break;
            }
            // SAFETY: 同 IPv4。
            let row: &MibTcp6RowOwnerPid =
                unsafe { &*(buf.as_ptr().add(off) as *const MibTcp6RowOwnerPid) };
            if row.state == MIB_TCP_STATE_LISTEN {
                out.push((row.owning_pid, row.local_port));
            }
        }
    }
    Ok(out)
}

/// enableIPForwarding IPv4 注册表写入（`winproc.go:417-424`）。
///
/// HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\IPEnableRouter=1（DWORD）。
/// 持久写入（stop/卸载不还原 —— 已知限制 M3，镜像 Go 现状）。
fn enable_ip_forwarding_ipv4_registry() {
    let subkey: Vec<u16> = OsString::from("SYSTEM\\CurrentControlSet\\Services\\Tcpip\\Parameters")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let value_name: Vec<u16> = OsString::from("IPEnableRouter")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: RegCreateKeyExW 打开/创建注册表键。KEY_SET_VALUE 权限。SYSTEM 有权。
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return;
    }
    let data: u32 = 1;
    // SAFETY: RegSetValueExW 写 IPEnableRouter=1（DWORD）。hkey 由上方 RegCreateKeyExW 返回。
    let _ = unsafe {
        RegSetValueExW(
            hkey,
            value_name.as_ptr(),
            0,
            REG_DWORD,
            &data as *const _ as *const u8,
            std::mem::size_of::<u32>() as u32,
        )
    };
    // SAFETY: 关键句柄。
    unsafe { RegCloseKey(hkey) };
}

/// 宽字符串辅助：OsStr → null 终止 UTF-16（供 windows-sys API）。
fn wide_null(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// PROCESSENTRY32W.szExeFile（固定 260 wide）→ String。
fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// 构造 CreateProcessW 的 lpCommandLine（argv[0]=exe + args，空格分隔，含空格路径用引号）。
fn build_command_line(exe: &str, args: &[&str]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(args.len() + 1);
    parts.push(quote_if_needed(exe));
    for a in args {
        parts.push(quote_if_needed(a));
    }
    parts.join(" ")
}

/// token 含空格或 tab → 加双引号（CreateProcessW 命令行解析规则）。
fn quote_if_needed(s: &str) -> String {
    if s.contains(' ') || s.contains('\t') {
        format!("\"{s}\"")
    } else {
        s.to_owned()
    }
}

// 父死看护（watchParent，`helper.go:131-159`）现由 [`WinProcOps::spawn_watch_parent`] 承载（W15 接线：
// helper 分派层在 start 成功后调 trait 方法起后台线程；此前的 zero-call 自由函数版已删）。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_command_line_quotes_spaced_paths() {
        let cl = build_command_line(
            r"C:\Program Files\Polaris\sing-box.exe",
            &["run", "-c", r"C:\my cfg\c.json"],
        );
        assert!(cl.contains(r#""C:\Program Files\Polaris\sing-box.exe""#));
        assert!(cl.contains(r#""C:\my cfg\c.json""#));
        assert!(cl.contains(" run -c "));
    }

    #[test]
    fn build_command_line_no_quotes_for_simple_paths() {
        let cl = build_command_line("/usr/bin/sing-box", &["run", "-c", "/tmp/c.json"]);
        assert_eq!(cl, "/usr/bin/sing-box run -c /tmp/c.json");
    }

    #[test]
    fn wide_to_string_truncates_at_null() {
        let buf = [b'A'.into(), b'B'.into(), 0u16, b'C'.into()];
        assert_eq!(wide_to_string(&buf), "AB");
    }
}
