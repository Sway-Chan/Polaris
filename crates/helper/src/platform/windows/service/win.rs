//! Windows SCM 服务 + 命名管道监听（移植自 `helper-win/service.go` + `main.go`）。
//!
//! 实现两件事：
//! 1. **命名管道监听**（`service.go:37-44` `listen` + `service.go:48-57` `serve`）：创建 SDDL 防护的命名管道
//!    （`\\.\pipe\polaris-helper`，SDDL=`D:(A;;FA;;;SY)(A;;GRGW;;;IU)`），accept 循环，每连接一个线程跑//!    [`crate::platform::windows::helper::WinHelper::handle`]。
//! 2. **SCM 服务托管**（`service.go:64-93` `Execute` + `main.go:39-42`）：注册服务控制处理器，
//!    STOP/SHUTDOWN → reapChildOnExit → 关 listener → 报 Stopped 退出。

// 局部放开 crate 级 `#![deny(unsafe_code)]`：SCM/命名管道/SDDL 的 windows-sys FFI 调用
// （CreateNamedPipeW/ConnectNamedPipe/ConvertStringSecurityDescriptorToSecurityDescriptorW/
// StartServiceCtrlDispatcherW/RegisterServiceCtrlHandlerExW/SetServiceStatus/...）必须 unsafe。
// 每处 unsafe 块附 SAFETY 理由。本 allow 仅对 Windows target 生效。
#![allow(unsafe_code)]

use crate::platform::windows::helper::{HandleOutcome, WinHelper};
use crate::platform::windows::logic;
use crate::platform::windows::winproc::WinProcOps;
use crate::platform::windows::{DEFAULT_SUPPORT_DIR, PIPE_NAME, PIPE_SDDL, SERVICE_NAME};
use crate::token::FileTokenStore;
use polaris_helper_proto::codec::MAX_FRAME_BYTES;
use polaris_helper_proto::command;
use polaris_helper_proto::Request;
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED,
    SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows_sys::Win32::System::IO::CancelIoEx;

/// 命名管道实例数（accept 循环预创建的并行实例）。镜像 go-winio 默认行为。
const PIPE_INSTANCES: u32 = 4;

/// FILE_FLAG_FIRST_PIPE_INSTANCE（首实例独占，防重复创建）。windows-sys 常量值。
const FILE_FLAG_FIRST_PIPE_INSTANCE: FILE_FLAGS_AND_ATTRIBUTES = 0x0008_0000;

/// SDDL_REVISION_1（ConvertStringSecurityDescriptor 的 revision 参数）。
const SDDL_REVISION_1: u32 = 1;

/// ERROR_PIPE_CONNECTED（ConnectNamedPipe 时客户端已先连上）。
const ERROR_PIPE_CONNECTED: u32 = 230;

/// ERROR_BROKEN_PIPE（ReadFile 时客户端关闭，正常 EOF）。
const ERROR_BROKEN_PIPE: u32 = 109;

/// 全局停止标志（SCM STOP/SHUTDOWN 时置 true，accept 循环据此退出）。
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// SCM 状态句柄（`service_main_entry` 注册后写入，`ctrl_handler` 读它上报 STOP_PENDING）。
///
/// 存 `usize` 而非句柄类型：`static` 要求 `Sync`，而 `SERVICE_STATUS_HANDLE` 是裸指针别名。
/// 0 = 尚未注册。
static STATUS_HANDLE: AtomicUsize = AtomicUsize::new(0);

/// 服务运行配置（main 解析 flags 后构造）。
pub struct ServiceConfig {
    pub singbox_bin: String,
    pub conf_dir: String,
    pub support_dir: String,
    /// core_dir：Windows 接受并忽略（镜像 macOS flag 形态，无 install-core）。
    pub core_dir: String,
}

impl ServiceConfig {
    #[must_use]
    pub fn default_with(singbox_bin: impl Into<String>, conf_dir: impl Into<String>) -> Self {
        Self {
            singbox_bin: singbox_bin.into(),
            conf_dir: conf_dir.into(),
            support_dir: DEFAULT_SUPPORT_DIR.to_owned(),
            core_dir: String::new(),
        }
    }
}

/// 以 SCM 服务身份运行（`main.go:39-42` `runService`）。
pub fn run_service(cfg: ServiceConfig) -> std::io::Result<()> {
    let cfg: &'static ServiceConfig = Box::leak(Box::new(cfg));
    set_global_config(cfg);
    start_service_ctrl_dispatcher()
}

/// --console dev 模式：前台跑 listener 无 SCM（`main.go:47-66` `runConsole`）。
pub fn run_console(cfg: ServiceConfig) -> std::io::Result<()> {
    let helper = build_helper(&cfg);
    let helper = Arc::new(helper);
    let helper_for_stop = helper.clone();
    // Ctrl+C → reapChildOnExit + 设 STOP_REQUESTED → serve 退出。
    let _ = ctrl_c_handler(move || {
        helper_for_stop.reap_child_on_exit();
        STOP_REQUESTED.store(true, Ordering::SeqCst);
    });
    serve(helper)
}

/// 构造生产 WinHelper（FileTokenStore + WinProcOps）。
fn build_helper(cfg: &ServiceConfig) -> WinHelper<FileTokenStore, WinProcOps, WinProcOps> {
    let token = FileTokenStore::new(&cfg.support_dir);
    let proc = WinProcOps::new();
    let net = WinProcOps::new();
    WinHelper::new(
        token,
        proc,
        net,
        &cfg.singbox_bin,
        &cfg.conf_dir,
        SERVICE_NAME,
        &cfg.support_dir,
    )
}

/// HANDLE（`*mut c_void`）的 Send 包装，用于把管道句柄移入 spawned 线程。
///
/// SAFETY: 命名管道句柄在本设计中仅被单一连接线程独占使用（创建 → ConnectNamedPipe → 读写 →
/// CloseHandle 全在 `handle_connection` 内），无跨线程共享，故可安全 Send。
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}
impl SendHandle {
    fn raw(self) -> HANDLE {
        self.0
    }
}

/// serve：accept 循环，每连接一个线程（`service.go:48-57`）。
fn serve<T, P, N>(helper: Arc<WinHelper<T, P, N>>) -> std::io::Result<()>
where
    T: crate::token::TokenStore + 'static,
    P: crate::platform::windows::ops::ProcOps + 'static,
    N: crate::platform::windows::ops::NetTableOps + 'static,
{
    let pipe_name_w = wide_null(OsString::from(PIPE_NAME));
    let sddl_w = wide_null(OsString::from(PIPE_SDDL));
    // 只有**第一个**实例带 FILE_FLAG_FIRST_PIPE_INSTANCE（防他人抢占同名管道）；后续实例不带，
    // 否则「工作线程还持有上一个实例」期间回头重建必然 ERROR_ACCESS_DENIED，
    // 整个连接处理期间没有监听实例。见 `logic::pipe_open_mode`。
    let mut first = true;
    while !STOP_REQUESTED.load(Ordering::SeqCst) {
        // 创建一个管道实例（阻塞等连接）。镜像 Go winio.ListenPipe。
        let h = match create_pipe_instance(&pipe_name_w, &sddl_w, first) {
            Ok(h) => h,
            Err(e) => {
                // 创建失败：短暂 sleep 防忙等，重试（除非已 STOP_REQUESTED）。
                if STOP_REQUESTED.load(Ordering::SeqCst) {
                    return Ok(());
                }
                log::error!("create_pipe_instance failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
        };
        // 建成了一个实例 ⇒ 从此管道名已存在，后续再带首实例 flag 必被拒。
        first = false;
        // ConnectNamedPipe 阻塞等客户端连接。
        if connect_pipe(h) {
            let helper = helper.clone();
            // HANDLE = *mut c_void 非 Send；先包成 SendHandle 再 move 入 spawned 线程（句柄在
            // handle_connection 内仅本线程独占使用，CloseHandle 也在内部，无并发访问）。
            let sh = SendHandle(h);
            std::thread::spawn(move || {
                handle_connection(sh.raw(), &helper);
            });
        } else {
            // SAFETY: 关未连接的实例，下一轮重建。
            unsafe { CloseHandle(h) };
        }
    }
    Ok(())
}

/// 钉住 [`logic::pipe_open_mode`] 里硬编码的位值 == `windows-sys` 常量。
///
/// 那两个位值必须硬编码（`logic` 模块在 Linux 上也编译，好让纯逻辑有门可跑；而 `windows-sys`
/// 只在 Windows target 的依赖图里）。这条编译期断言是它们之间唯一的对账 —— 任一侧改了就编不过，
/// 且 CI 的 `cargo check --target x86_64-pc-windows-msvc` 会把它跑到。
const _: () = {
    assert!(logic::pipe_open_mode(true) == PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE);
    assert!(logic::pipe_open_mode(false) == PIPE_ACCESS_DUPLEX);
};

/// 创建一个命名管道实例（SDDL 防护，字节流，PIPE_WAIT 阻塞）。
///
/// `first` 决定要不要带 `FILE_FLAG_FIRST_PIPE_INSTANCE` —— 见 [`logic::pipe_open_mode`]，
/// 带错了会让**连接处理期间没有监听实例**。
fn create_pipe_instance(name_w: &[u16], sddl_w: &[u16], first: bool) -> std::io::Result<HANDLE> {
    let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
    sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
    sa.bInheritHandle = FALSE;
    // SAFETY: ConvertStringSecurityDescriptorToSecurityDescriptorW 解析 SDDL → SD。
    // sa.lpSecurityDescriptor 指向新 SD（由 LocalFree 释放；此处为简化随进程生命周期，生产可加 LocalFree）。
    let mut sd: *mut SECURITY_DESCRIPTOR = std::ptr::null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_w.as_ptr(),
            SDDL_REVISION_1,
            &mut sd as *mut *mut SECURITY_DESCRIPTOR as *mut _,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 {
        sa.lpSecurityDescriptor = sd as *mut _;
    }
    // SAFETY: CreateNamedPipeW 创建管道实例。**W1 修：PIPE_ACCESS_DUPLEX**（双向）——此前
    // PIPE_ACCESS_INBOUND 只给服务端只读句柄，WriteFile 响应据 Win32 契约回 ERROR_ACCESS_DENIED →
    // 请求/响应回路断（client 收不到响应）。DUPLEX 让服务端可读请求 + 写响应（对齐 Go winio 默认双向）。
    // FILE_FLAG_FIRST_PIPE_INSTANCE **只给首个实例**（见 `logic::pipe_open_mode`）。
    // PIPE_TYPE_BYTE | PIPE_READMODE_BYTE（字节流）。
    // PIPE_WAIT 阻塞（ConnectNamedPipe 阻塞等连接）。PIPE_REJECT_REMOTE_CLIENTS 拒绝远程客户端。
    let h = unsafe {
        CreateNamedPipeW(
            name_w.as_ptr(),
            logic::pipe_open_mode(first),
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_INSTANCES,
            4096,
            4096,
            0,
            &sa,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(h)
}

/// ConnectNamedPipe：等客户端连接。返回 true = 已连接，false = 中断/失败。
fn connect_pipe(h: HANDLE) -> bool {
    // SAFETY: ConnectNamedPipe 阻塞等连接（PIPE_WAIT，lpOverlapped=NULL）。返回 0 = 失败。
    let ok = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
    if ok != 0 {
        return true;
    }
    // ERROR_PIPE_CONNECTED（230）= 客户端在 CreateNamedPipe 与 ConnectNamedPipe 之间已连上 → 视为成功。
    let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    err == ERROR_PIPE_CONNECTED
}

/// 处理一个已连接的管道实例（对应 Go `handle(conn)` 的 IO 部分）。
fn handle_connection<T, P, N>(h: HANDLE, helper: &WinHelper<T, P, N>)
where
    T: crate::token::TokenStore,
    // 'static：WinHelper 的 impl 因父死看护闭包捕获 Arc<P> 进 'static 线程而要求 P: 'static（W15）。
    P: crate::platform::windows::ops::ProcOps + 'static,
    N: crate::platform::windows::ops::NetTableOps,
{
    // 读一个请求帧（≤ MAX_FRAME_BYTES，防超长帧耗尽内存）。
    // W1 修：5s 读超时（镜像 Go `conn.SetReadDeadline(5s)`）——guard 在读结束时 drop（停看护 + join，
    // 句柄仍开），随后才 cleanup 关句柄。超时（客户端连上不发数据）→ CancelIoEx 中断 → read 返 Err → 关连接。
    let mut buf = vec![0u8; MAX_FRAME_BYTES];
    let read_result = {
        let _timeout = ReadTimeoutGuard::arm(h, std::time::Duration::from_secs(5));
        read_frame(h, &mut buf)
    };
    let n = match read_result {
        Ok(n) if n > 0 => n,
        _ => {
            cleanup_pipe(h);
            return;
        }
    };
    let raw = String::from_utf8_lossy(&buf[..n]);
    let lines: Vec<&str> = raw.lines().collect();
    // 帧结构（Platform::Win）：行1=token，行2=command，行3..=args。
    let (token_line, cmd_line, arg_lines) = match lines.as_slice() {
        [tok, cmd, args @ ..] => (*tok, *cmd, args),
        _ => {
            write_response(h, b"ERR unknown\n");
            cleanup_pipe(h);
            return;
        }
    };
    let req = match parse_request(cmd_line, arg_lines) {
        Some(r) => r,
        None => {
            write_response(h, b"ERR unknown\n");
            cleanup_pipe(h);
            return;
        }
    };
    let outcome = helper.handle(token_line, req);
    let response_line = match outcome {
        HandleOutcome::AuthFailed => "ERR auth\n".to_owned(),
        HandleOutcome::Respond(resp) => format!("{}\n", resp.to_wire_line()),
        HandleOutcome::UninstallAndExit(resp) => {
            let line = format!("{}\n", resp.to_wire_line());
            write_response(h, line.as_bytes());
            cleanup_pipe(h);
            // 800ms 后 os.Exit（Go helper.go:291-294）。
            std::thread::sleep(std::time::Duration::from_millis(800));
            std::process::exit(0);
        }
    };
    write_response(h, response_line.as_bytes());
    cleanup_pipe(h);
}

/// 读一个请求帧（**单次 ReadFile**，对应 mod.rs「一次 ReadFile 取整个请求帧再切行」的设计）。
///
/// **W1 关键**：不可 loop-until-EOF —— 命名管道无半关，duplex 下 client 写完请求后**保持连接**等读响应
/// （不会关写端使服务端读到 EOF）。若循环读到 EOF 会永久阻塞在第二次 ReadFile（由 5s 超时兜成失败），
/// 响应永远发不出。故按帧一次读回：client 把整帧一次 WriteFile 发来（字节模式 ReadFile 一到即返回全帧），
/// 服务端读回后切行 → 处理 → 写响应（同一 duplex 句柄）。
///
/// DESIGN-REVIEW(win-pipe-single-frame-read)：假设 client 把整帧单次 WriteFile 发送（本协议帧 < 1KB，
/// 远小于 4KB 管道缓冲），单次 ReadFile 取全帧。分帧/跨多次 WriteFile 的 client 不支持——duplex 请求/响应
/// 往返（含此假设）= 真机门（C6-4 生产 PipeConnector 须一次写全帧）。
///
/// 返回读到的字节数（0 = 客户端未发数据即关连接，视为无效）。
fn read_frame(h: HANDLE, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut got: u32 = 0;
    // SAFETY: ReadFile 从管道读一帧。buf 是本函数拥有的缓冲。lpOverlapped=NULL → 同步阻塞
    //（由 ReadTimeoutGuard 的 CancelIoEx 兜 5s 超时 → 返 ERROR_OPERATION_ABORTED）。
    let ok = unsafe {
        ReadFile(
            h,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        if err == ERROR_BROKEN_PIPE {
            return Ok(0); // 客户端未发数据即关（正常 EOF）→ 无效帧
        }
        // 其余（含超时 ERROR_OPERATION_ABORTED）→ 失败 → 上层关连接。
        return Err(std::io::Error::last_os_error());
    }
    Ok(got as usize)
}

/// 读超时看护（W1：镜像 Go `conn.SetReadDeadline(5s)`）。
///
/// 命名管道同步阻塞 ReadFile 无原生 deadline —— 另起看护线程，超时后 `CancelIoEx` 中断在途 ReadFile
///（返回 `ERROR_OPERATION_ABORTED` → [`read_frame`] 视为失败 → 关连接）。防无 token 进程连上不发数据
/// 耗尽句柄/线程（Go helper.go:163-165 同款动机）。
///
/// `drop` 时置 `done` + `unpark` + `join` 看护线程 —— 保证看护线程在句柄被 `CloseHandle` **前**退出，
/// 绝不对已关/复用句柄调 `CancelIoEx`。
struct ReadTimeoutGuard {
    done: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ReadTimeoutGuard {
    fn arm(h: HANDLE, timeout: std::time::Duration) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let done_t = done.clone();
        // HANDLE 非 Send：包 SendHandle 移入看护线程（句柄仅用于 CancelIoEx，读线程与看护线程对同一
        // 句柄的并发访问是 CancelIoEx 的设计用途——取消另一线程的在途同步 IO）。
        let sh = SendHandle(h);
        let join = std::thread::spawn(move || {
            // sh.raw() 消费整个 SendHandle（Send）→ 拿回 HANDLE；避免 edition-2021 disjoint 捕获直接抓
            // sh.0（`*mut c_void` 非 Send）导致闭包不 Send。
            let h = sh.raw();
            let deadline = std::time::Instant::now() + timeout;
            loop {
                if done_t.load(Ordering::SeqCst) {
                    return; // 读已完成（guard 已 drop）
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                std::thread::park_timeout(deadline - now);
            }
            if done_t.load(Ordering::SeqCst) {
                return;
            }
            // 超时且读未完成：中断在途同步 ReadFile。
            // SAFETY: CancelIoEx(h, NULL) 取消本句柄全部在途 IO。h 未关（guard drop 前不 CloseHandle）。
            unsafe { CancelIoEx(h, std::ptr::null()) };
        });
        Self {
            done,
            join: Some(join),
        }
    }
}

impl Drop for ReadTimeoutGuard {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            j.thread().unpark(); // 唤醒看护线程立即退出（不等 deadline）
            let _ = j.join(); // 等看护退出 → 之后 cleanup 才可安全 CloseHandle
        }
    }
}

/// 写响应到管道。
fn write_response(h: HANDLE, data: &[u8]) {
    let mut written: u32 = 0;
    // SAFETY: WriteFile 写响应。lpOverlapped=NULL → 同步阻塞。命名管道服务端在
    // DisconnectNamedPipe 前必须 FlushFileBuffers：同步 WriteFile 只保证字节进内核缓冲，若随即
    // disconnect，尚未被 client 读走的响应会被丢弃，client 稳定收到 ERROR_PIPE_NOT_CONNECTED(233)。
    // FlushFileBuffers 会等 client 取走这条很短的单行响应；生产 client 写完即同步读，协议本就要求
    // 一请求一连接。真机 A/B：同包 app/helper、token/pipe 均一致时，缺 flush 的 ping 仍 0 bytes/233。
    unsafe {
        let ok = WriteFile(
            h,
            data.as_ptr(),
            data.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        );
        if ok != 0 && written == data.len() as u32 {
            let _ = FlushFileBuffers(h);
        }
    }
}

/// 断开并关闭管道实例。
fn cleanup_pipe(h: HANDLE) {
    // SAFETY: DisconnectNamedPipe + CloseHandle。
    unsafe {
        let _ = DisconnectNamedPipe(h);
        CloseHandle(h);
    }
}

/// 解析 command + arg lines 为 Request（对应 Go handle() 各 case 的 readLine 序列）。
///
/// clippy needless_lifetimes：显式 `<'a>` 可省（Request 为 owned，不借 args 生命周期）→ 用省略式。
fn parse_request(cmd: &str, args: &[&str]) -> Option<Request> {
    let mut iter = args.iter();
    let mut next_line = || iter.next().copied().unwrap_or("");
    Some(match cmd {
        command::common::PING => Request::Ping,
        command::common::VERSION => Request::Version,
        command::common::STATUS => Request::Status,
        // stop 的受管 pid 身份行可选：旧客户端不发 → next_line() 返 "" → None（旧语义）。
        command::common::STOP => Request::Stop {
            pid: polaris_helper_proto::parse_stop_pid(next_line()),
        },
        command::common::CLEANUP => Request::Cleanup,
        command::common::FREEPORT => {
            let port = crate::platform::windows::logic::parse_port(next_line())?;
            Request::FreePort { port }
        }
        command::common::START => Request::Start(polaris_helper_proto::StartParams {
            cfg: next_line().to_owned(),
            log: next_line().to_owned(),
            fwd: next_line() == "1",
            parent_pid: next_line().parse::<u32>().ok(),
        }),
        command::common::ROUTE_ADD => Request::RouteAdd(parse_route_params(&mut next_line)),
        command::common::ROUTE_DEL => Request::RouteDel(parse_route_params(&mut next_line)),
        command::win::UNINSTALL => Request::Uninstall,
        command::win::IFACE_METRIC => {
            let iface = next_line().to_owned();
            let metric: u16 = next_line().parse().ok()?;
            Request::IfaceMetric { iface, metric }
        }
        _ => return None, // ERR unknown
    })
}

/// 解析 route-add/route-del 的 iface + cidrs 两行。
fn parse_route_params<'a>(next: &mut impl FnMut() -> &'a str) -> polaris_helper_proto::RouteParams {
    let iface = next().to_owned();
    let cidrs_line = next();
    let cidrs = if cidrs_line.is_empty() {
        Vec::new()
    } else {
        cidrs_line.split(',').map(|s| s.trim().to_owned()).collect()
    };
    polaris_helper_proto::RouteParams { iface, cidrs }
}

// wire 写方向已上提 helper-proto：见 [`polaris_helper_proto::Response::to_wire_line`]
//（与 `Response::parse` 成对，三平台共用）。原 win 私有副本（`wire_response`/`wire_ok`，38 行）已删。
//
// 合并后 win 侧两个「兜底」分支的输出有变，二者均为 **win 不产的 mac/linux 专属响应**（原注释自述
// 「Windows helper 不产以下响应，兜底原文」），故对真实 win 流量无影响，且新输出更正确：
// - `FlushDns(FlushedPartial{tail})`：原恒输出 `OK flushed`（丢 tail，与 mac 不一致）→ 现 `OK flushed-partial <tail>`。
// - `OkRaw{rest:""}`：原输出 `OK <token> `（尾空格，parse 回来不等原值）→ 现 `OK <token>`（round-trip 无损）。

// ===== SCM 服务控制 =====

/// 全局 ServiceConfig（main → service_main_entry 经 OnceLock 传参）。
static GLOBAL_CFG: std::sync::OnceLock<&'static ServiceConfig> = std::sync::OnceLock::new();

pub(crate) fn set_global_config(cfg: &'static ServiceConfig) {
    let _ = GLOBAL_CFG.set(cfg);
}

fn global_config() -> Option<&'static ServiceConfig> {
    GLOBAL_CFG.get().copied()
}

/// 启动 SCM 服务控制分派器（`service.go:96-98` `svc.Run`）。
fn start_service_ctrl_dispatcher() -> std::io::Result<()> {
    // name_w 必须是 'static 或与 table 同生命周期：用 Box::leak 让它活到进程结束。
    let name_w: &'static [u16] =
        Box::leak(wide_null(OsString::from(SERVICE_NAME)).into_boxed_slice());
    let table: [SERVICE_TABLE_ENTRYW; 2] = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name_w.as_ptr() as *mut _,
            lpServiceProc: Some(service_main_entry),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: StartServiceCtrlDispatcherW 阻塞，SCM 调用 service_main_entry。表以 NULL 终止。
    let ok = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// SCM 调用入口（`service.go:64` `Execute`）。extern "system" 约定（windows-sys 要求）。
extern "system" fn service_main_entry(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let cfg = match global_config() {
        Some(c) => c,
        None => return,
    };
    let helper = build_helper(cfg);
    let helper = Arc::new(helper);
    let status_handle = register_ctrl_handler();
    if status_handle.is_null() {
        return;
    }
    // 交给 ctrl_handler 用于上报 STOP_PENDING（它拿不到这个局部变量）。
    STATUS_HANDLE.store(status_handle as usize, Ordering::SeqCst);
    // StartPending → Running。
    let _ = set_status(status_handle, SERVICE_START_PENDING, 0);
    let _ = set_status(
        status_handle,
        SERVICE_RUNNING,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
    );
    // serve（阻塞）。STOP/SHUTDOWN 时 ctrl_handler 设 STOP_REQUESTED → serve 退出。
    let _ = serve(helper.clone());
    // reapChildOnExit（service.go:84-85：先收割 child 再退出，杜绝孤儿）。
    helper.reap_child_on_exit();
    let _ = set_status(status_handle, SERVICE_STOPPED, 0);
}

/// 注册 SCM 控制处理器。
fn register_ctrl_handler() -> SERVICE_STATUS_HANDLE {
    let name_w = wide_null(OsString::from(SERVICE_NAME));
    // SAFETY: RegisterServiceCtrlHandlerExW 注册 ctrl_handler。返回状态句柄（NULL 失败）。
    unsafe { RegisterServiceCtrlHandlerExW(name_w.as_ptr(), Some(ctrl_handler), std::ptr::null()) }
}

/// SCM 控制处理器（`service.go:79-92`）。STOP/SHUTDOWN → 设 STOP_REQUESTED → serve 退出后 reap。
extern "system" fn ctrl_handler(
    ctrl: u32,
    _evt_type: u32,
    _evt_data: *mut std::ffi::c_void,
    _ctx: *mut std::ffi::c_void,
) -> u32 {
    match ctrl {
        SERVICE_CONTROL_STOP | windows_sys::Win32::System::Services::SERVICE_CONTROL_SHUTDOWN => {
            STOP_REQUESTED.store(true, Ordering::SeqCst);
            // 诚实上报中间态：此前从不上报 STOP_PENDING，SCM 记录仍是 RUNNING，`sc stop` 直接
            // 返回成功而进程还在跑 —— 比「卡在 STOP_PENDING」更静默。
            let h = STATUS_HANDLE.load(Ordering::SeqCst);
            if h != 0 {
                let _ = set_status(h as SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, 0);
            }
            // 🔴 **只置标志是不够的**：accept 循环此刻阻塞在 `ConnectNamedPipe(h, NULL)`
            //（同步、无 overlapped），而 `STOP_REQUESTED` 只在**拿到/放弃一个连接之后**才被求值
            // ⇒ 没有下一个客户端连上来就永远回不到判定点。
            wake_accept_loop();
            1 // NO_ERROR
        }
        _ => 0, // 未声明接受的控制码 → 忽略
    }
}

/// 唤醒阻塞在 `ConnectNamedPipe` 上的 accept 循环 —— 对自己的管道做一次连接即可。
///
/// # 为什么是自连接而不是 `CancelIoEx`
///
/// 监听句柄是 `serve` 的**函数局部变量**，`ctrl_handler` 在结构上够不到它；要用 `CancelIoEx`
/// 就得把句柄存进 static 并处理「已被工作线程 move 走」的置空竞态。自连接不碰句柄所有权，
/// 且被唤醒的那一帧会走进 `handle_connection` 后立刻读到 EOF（本函数连上就关）自然收尾。
///
/// 成功与否都不重要：目的只是让 `ConnectNamedPipe` 返回一次。管道已不存在（服务正在收尾）
/// 时 `CreateFileW` 失败，那说明监听本来就没了，同样无需唤醒。
///
/// # 不修会怎样
///
/// 应用内卸载腿 `clear_token()` 先删了 app 侧 token，其后的状态查询因 token 为空**早返不 ping**
/// ⇒ 没有任何客户端来解阻塞，服务留着运行中的 SYSTEM 进程 + 删不掉的 `polaris-helper.exe`
/// 到重启（NSIS 卸载钩子同理且更彻底：app 已退出）。安装/修复腿则会被自身的状态探测解阻塞，
/// 表现为「第一次莫名失败、第二次成功」。
fn wake_accept_loop() {
    let name_w = wide_null(OsString::from(PIPE_NAME));
    // SAFETY: 对本进程自己的管道名发起一次连接。所有指针参数为 NULL（无 SA / 无模板句柄），
    // 返回 INVALID_HANDLE_VALUE 表示失败（此处失败即无需唤醒）。
    let h = unsafe {
        CreateFileW(
            name_w.as_ptr(),
            0, // 不需要任何访问权限：连上即达目的，不读不写
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h != INVALID_HANDLE_VALUE {
        // SAFETY: 关掉这条只用于唤醒的连接；服务端那帧随即读到 EOF 收尾。
        unsafe { CloseHandle(h) };
    }
}

/// 报服务状态。
fn set_status(handle: SERVICE_STATUS_HANDLE, state: u32, accepts: u32) -> std::io::Result<()> {
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    status.dwServiceType = SERVICE_WIN32_OWN_PROCESS;
    status.dwCurrentState = state;
    status.dwControlsAccepted = accepts;
    status.dwWin32ExitCode = 0;
    status.dwWaitHint = 30000; // 30s
                               // SAFETY: SetServiceStatus 报状态。
    let ok = unsafe { SetServiceStatus(handle, &status) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ===== Ctrl+C 处理器（--console 模式）=====

/// 注册 Ctrl+C 处理器（SetConsoleCtrlHandler）。handler 在 Ctrl+C 时被回调。
fn ctrl_c_handler<F>(handler: F) -> std::io::Result<()>
where
    F: Fn() + Send + 'static,
{
    use std::sync::Mutex;
    static HANDLER: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);
    *HANDLER.lock().unwrap() = Some(Box::new(handler));
    // SAFETY: SetConsoleCtrlHandler 回调签名 = `unsafe extern "system" fn(u32) -> BOOL`。
    // 体内只取 Mutex 锁定的闭包调用，不做其他 unsafe 操作；返回 TRUE(1) 表示已处理。
    unsafe extern "system" fn raw_handler(_ctrl: u32) -> windows_sys::Win32::Foundation::BOOL {
        if let Some(h) = HANDLER.lock().unwrap().as_ref() {
            h();
        }
        windows_sys::Win32::Foundation::TRUE
    }
    // SAFETY: SetConsoleCtrlHandler 注册 raw_handler（PHANDLER_ROUTINE = Option<unsafe extern "system" fn>)。
    let ok =
        unsafe { SetConsoleCtrlHandler(Some(raw_handler), windows_sys::Win32::Foundation::TRUE) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ===== 辅助 =====

/// 宽字符串 → null 终止 UTF-16。
fn wide_null(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_ping_status() {
        assert!(matches!(
            parse_request("ping", &[][..]),
            Some(Request::Ping)
        ));
        assert!(matches!(
            parse_request("status", &[][..]),
            Some(Request::Status)
        ));
    }

    /// stop 的受管 pid 身份行可选：有则解出，无（旧客户端）则 `None`。
    ///
    /// 变异：把 `STOP` 分支退回不读身份行的 `Request::Stop { pid: None }` → 首条转红
    /// （身份判据从此永远拿不到 want = 形同虚设）。
    #[test]
    fn parse_request_stop_reads_optional_pid() {
        assert_eq!(
            parse_request("stop", &["4242"]).unwrap(),
            Request::Stop { pid: Some(4242) }
        );
        assert_eq!(
            parse_request("stop", &[][..]).unwrap(),
            Request::Stop { pid: None },
            "旧客户端不发身份行 → None → 沿用「停当前受管核」"
        );
    }

    #[test]
    fn parse_request_freeport() {
        let r = parse_request("freeport", &["9090"]).unwrap();
        assert_eq!(r, Request::FreePort { port: 9090 });
    }

    #[test]
    fn parse_request_freeport_bad_port_returns_none() {
        assert!(parse_request("freeport", &["abc"]).is_none());
    }

    #[test]
    fn parse_request_start_full() {
        let r = parse_request("start", &["/c/cfg.json", "/l/log.txt", "1", "4242"]).unwrap();
        let Request::Start(p) = r else { panic!() };
        assert_eq!(p.cfg, "/c/cfg.json");
        assert_eq!(p.log, "/l/log.txt");
        assert!(p.fwd);
        assert_eq!(p.parent_pid, Some(4242));
    }

    #[test]
    fn parse_request_start_without_ppid() {
        let r = parse_request("start", &["/c/cfg.json", "", "0"]).unwrap();
        let Request::Start(p) = r else { panic!() };
        assert_eq!(p.parent_pid, None);
    }

    #[test]
    fn parse_request_route_add() {
        let r = parse_request("route-add", &["polaris-tun0", "10.0.0.0/8,172.16.0.0/12"]).unwrap();
        let Request::RouteAdd(rp) = r else { panic!() };
        assert_eq!(rp.iface, "polaris-tun0");
        assert_eq!(rp.cidrs, vec!["10.0.0.0/8", "172.16.0.0/12"]);
    }

    #[test]
    fn parse_request_uninstall_iface_metric() {
        assert!(matches!(
            parse_request("uninstall", &[][..]),
            Some(Request::Uninstall)
        ));
        let r = parse_request("iface-metric", &["polaris-tun0", "999"]).unwrap();
        let Request::IfaceMetric { iface, metric } = r else {
            panic!()
        };
        assert_eq!(iface, "polaris-tun0");
        assert_eq!(metric, 999);
    }

    #[test]
    fn parse_request_unknown_returns_none() {
        assert!(parse_request("bogus", &[][..]).is_none());
    }

    /// win 侧 wire 契约（序列化本体已上提 helper-proto，见 `Response::to_wire_line`；
    /// 本测试保留为 win 视角的回归断言 —— win 的 uid 恒 0，`helper-win/helper.go:179`）。
    #[test]
    fn wire_ok_matches_go_format() {
        use polaris_helper_proto::{Pong, Response, ResponseKind};
        let wire = |k| Response::Ok(k).to_wire_line();
        assert_eq!(
            wire(ResponseKind::Pong(Pong {
                uid: 0,
                proto_version: 5,
                build_identity: None,
            })),
            "OK pong uid=0 v5"
        );
        assert_eq!(wire(ResponseKind::Version { proto_version: 5 }), "OK 5");
        assert_eq!(wire(ResponseKind::Cleaned), "OK cleaned");
        assert_eq!(wire(ResponseKind::Uninstalling), "OK uninstalling");
    }
}
