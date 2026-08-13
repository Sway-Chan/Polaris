//! G3 探针：**Windows 服务能否给无 console 的子进程投递 CTRL_BREAK**。
//!
//! # 这个探针要回答什么
//!
//! Windows 上 sing-box 的 `Store.Close()` 永不执行，根因另一半是「没有优雅通道」：
//! Go 侧 `ctrlHandler` 把 `CTRL_C`/`CTRL_BREAK` 翻成 SIGINT，而 `winproc/win.rs::send_ctrl_break`
//! 的注释断言「服务模式无 console → 返回 0，无害」。**那句注释是推测，没人验过。**
//!
//! 决定成败的是一个二值事实：`CREATE_NO_WINDOW` 到底给不给子进程一个**可 attach 的 console**。
//! MSDN 的措辞（"the console handle for the application is not set"）指向「不给」，
//! 实务上又常被当成「给、只是没窗口」。本机（Linux）判不了，靠记忆下结论就是猜。
//!
//! # 三档结论与各自的下一步
//!
//! | 观测 | 结论 | 下一步 |
//! |---|---|---|
//! | ② 直接投递就成功 | `send_ctrl_break` 那句「服务模式 no-op」注释本身是错的 | 直接改 helper |
//! | ② 失败、③ AttachConsole 后成功 | 方案成立 | `reap_sequence` 补一次 AttachConsole，失败回落 `TerminateProcess`（天然 fail-safe） |
//! | 两者都失败 | console 通道整条死 | 只能换机制（去掉 `CREATE_NO_WINDOW` 让 helper 自建隐藏 console，须与黑窗抑制门重新对齐） |
//!
//! # 正向对照（没有它，③ 的失败没有信息量）
//!
//! 同一个探针在**带 console 的普通进程**里跑 ②必须成功。不成功说明探针自己写错了，
//! 服务模式那一侧的任何「失败」都只是在测我的 bug。CI 里这条对照**失败即整个 job 红**。
//!
//! # 顺带回答一处未记录的行为分叉
//!
//! Polaris 比 上游 多传了 `CREATE_NO_WINDOW`（`winproc/win.rs:296` vs 上游
//! `helper-win/winproc.go:26` 只有 `CREATE_NEW_PROCESS_GROUP`）。探针两种 flag 组合都跑，
//! 直接给出「这个多出来的 flag 让 Polaris 更有救还是更没救」。
//!
//! # 安全边界
//!
//! 不建网卡、不改 IP/路由/DNS、不起 TUN、不碰任何用户文件 —— 只在给定的临时目录里
//! 起一个自带的子进程、投一次 console 控制事件。故可在 CI 的 windows runner 上跑。
//!
//! # 用法
//!
//! ```text
//! ctrl-break-probe child   <workdir>   # 被测子进程（探针自己起，不手工调）
//! ctrl-break-probe parent  <workdir>   # 正向对照：在当前（有 console 的）进程里跑
//! ctrl-break-probe service <workdir>   # SCM 入口：由 sc create/start 拉起，在 session 0 无 console
//! ```
//! 结果写 `<workdir>\result-parent.json` / `<workdir>\result-service.json`。

#[cfg(not(windows))]
fn main() {
    eprintln!("ctrl-break-probe 只在 Windows 上有意义");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    win::main()
}

#[cfg(windows)]
mod win {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::System::Console::{
        AllocConsole, AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, GetConsoleProcessList,
        GetConsoleWindow, SetConsoleCtrlHandler, CTRL_BREAK_EVENT,
    };
    use windows_sys::Win32::System::Services::{
        RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
        SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_TABLE_ENTRYW,
    };

    /// `sc create` 时用的服务名。CI 与这里必须一致，否则 SCM 分派器起不来。
    pub const SERVICE_NAME: &str = "PolarisCtrlBreakProbe";

    // winbase.h。std 的 `creation_flags` 收原始位，故这里直接写常量。
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    const SERVICE_WIN32_OWN_PROCESS: u32 = 0x0000_0010;
    const SERVICE_START_PENDING: u32 = 0x0000_0002;
    const SERVICE_RUNNING: u32 = 0x0000_0004;
    const SERVICE_STOPPED: u32 = 0x0000_0001;

    /// 子进程收到的控制事件号；0 = 还没收到。`CTRL_C_EVENT` 是 0，故这里 +1 存。
    static GOT_EVENT: AtomicU32 = AtomicU32::new(0);

    /// 逐步埋点，追加写 `<dir>/trace-<mode>.log`。
    ///
    /// # 为什么非有不可（2026-08-12 首跑的教训）
    ///
    /// 首跑时 parent 腿跑了 3.5 秒、**零 stdout、无结果文件**就退了。workflow 侧的自曝
    /// 只能说出「没跑到写盘那步」，说不出停在哪 —— 而本探针恰恰会**故意破坏自己的
    /// stdout**（`AllocConsole` 会把标准句柄改指到新分配的 console，`FreeConsole` 之后
    /// 那个句柄还会失效），所以 stdout 在这里天生不是可靠的观测通道。
    ///
    /// 埋点一律走**文件**，且每条都立刻落盘：进程随时可能被 console 控制事件带走，
    /// 缓冲在内存里的日志等于没有。
    fn trace(dir: &Path, mode: &str, msg: &str) {
        use std::io::Write;
        let _ = std::fs::create_dir_all(dir);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("trace-{mode}.log")))
        {
            let _ = writeln!(f, "{msg}");
            let _ = f.flush();
        }
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        let mode = args.get(1).map(String::as_str).unwrap_or("");
        let dir = PathBuf::from(args.get(2).map(String::as_str).unwrap_or("."));
        match mode {
            "child" => child(&dir),
            "parent" => run_and_report(&dir, "parent", true),
            "service" => service_entry(),
            other => {
                eprintln!("未知模式 `{other}`；用法见文件头注释");
                std::process::exit(2);
            }
        }
    }

    /// 跑实验并落盘。**panic 也要留下痕迹** —— 否则一次 `expect` 失败与「被信号带走」
    /// 在外面看起来一模一样（都是「没有结果文件」）。
    fn run_and_report(dir: &Path, mode: &str, alloc: bool) {
        trace(dir, mode, &format!("enter alloc={alloc}"));
        let d = dir.to_path_buf();
        let m = mode.to_string();
        let r = std::panic::catch_unwind(move || run_both_flag_combos(&d, &m, alloc));
        match r {
            Ok(report) => {
                write_atomic(&dir.join(format!("result-{mode}.json")), &report);
                trace(dir, mode, "wrote-result");
            }
            Err(e) => {
                // 把 panic 载荷本身写进 trace：stderr 在这条腿上随时可能已经失效
                // （FreeConsole 之后句柄就是废的），只写「见 stderr」等于什么都没说。
                let msg = e
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| e.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<非字符串 panic 载荷>".to_string());
                trace(dir, mode, &format!("PANIC: {msg}"));
            }
        }
    }

    // ── 子进程 ──────────────────────────────────────────────────────────────────

    unsafe extern "system" fn console_ctrl_handler(event: u32) -> BOOL {
        GOT_EVENT.store(event + 1, Ordering::SeqCst);
        1 // TRUE：已处理，别走默认的「直接终止」
    }

    /// 被测子进程：注册 console 控制处理器，收到事件就落一个标记文件再优雅退出。
    ///
    /// 落**文件**而不是打印：父进程在服务模式下无 console，拿不到子进程的 stdout；
    /// 而「有没有走到优雅分支」正是本探针唯一的判据，不能让它依赖一条可能不存在的通道。
    fn child(dir: &Path) {
        // SAFETY: 注册本进程的 console 控制处理器；handler 是 'static fn，无捕获。
        let registered = unsafe { SetConsoleCtrlHandler(Some(console_ctrl_handler), 1) } != 0;
        // SAFETY: 只读查询，无参数。
        let has_console_window = unsafe { !GetConsoleWindow().is_null() };

        write_atomic(
            &dir.join("child-started"),
            &format!(
                "{{\"registered\":{registered},\"console_window\":{has_console_window},\"pid\":{}}}",
                std::process::id()
            ),
        );

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let e = GOT_EVENT.load(Ordering::SeqCst);
            if e != 0 {
                write_atomic(&dir.join("child-graceful"), &format!("{}", e - 1));
                std::process::exit(0);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // 30 秒没等到事件 —— 这本身就是结论（通道不通），不是故障。
        std::process::exit(3);
    }

    // ── 父进程侧的实验 ──────────────────────────────────────────────────────────

    struct Outcome {
        flags_label: &'static str,
        child_pid: u32,
        child_started: String,
        step2_api_ok: bool,
        step2_err: u32,
        step2_graceful: bool,
        step3_ran: bool,
        step3_attach_ok: bool,
        step3_attach_err: u32,
        step3_api_ok: bool,
        step3_err: u32,
        step3_graceful: bool,
    }

    impl Outcome {
        fn to_json(&self) -> String {
            format!(
                "{{\"flags\":\"{}\",\"child_pid\":{},\"child_started\":{},\
                 \"step2\":{{\"api_ok\":{},\"last_error\":{},\"graceful\":{}}},\
                 \"step3\":{{\"ran\":{},\"attach_ok\":{},\"attach_error\":{},\
                 \"api_ok\":{},\"last_error\":{},\"graceful\":{}}}}}",
                self.flags_label,
                self.child_pid,
                if self.child_started.is_empty() {
                    "null".to_string()
                } else {
                    self.child_started.clone()
                },
                self.step2_api_ok,
                self.step2_err,
                self.step2_graceful,
                self.step3_ran,
                self.step3_attach_ok,
                self.step3_attach_err,
                self.step3_api_ok,
                self.step3_err,
                self.step3_graceful,
            )
        }
    }

    /// 两种 flag 组合各跑一遍 —— 这正好回答 Polaris 多传的那个 `CREATE_NO_WINDOW`
    /// 是让情况更好还是更坏（上游 只传 `CREATE_NEW_PROCESS_GROUP`）。
    /// 父进程自己的 console 控制处理器：**吞掉一切事件并返回已处理**。
    ///
    /// 这不是防御性冗余，是首跑那次 3.5 秒静默退出的头号嫌疑：
    /// 原先在步骤 ③ 用的是 `SetConsoleCtrlHandler(NULL, TRUE)`，而 MSDN 对它的定义只有
    /// **「忽略 CTRL+C」** —— `CTRL_BREAK` 不在其中。一旦本进程因为 `AttachConsole` 之类
    /// 的原因被算进了收件范围，走的就是默认处理器 = **直接终止**，且不留任何痕迹。
    /// 换成自己的处理器返回 TRUE，两种事件都吃得下。
    unsafe extern "system" fn parent_ignore_handler(_event: u32) -> BOOL {
        1
    }

    /// 本进程**有没有 console**。
    ///
    /// ⚠️ 不能用 `GetConsoleWindow()` 判 —— 它测的是「有没有 console **窗口**」。
    /// 2026-08-12 第二跑实测：CI 的 pwsh 下 `GetConsoleWindow()` 返回 NULL，于是探针以为没有
    /// console 就去 `AllocConsole()`，结果失败（本来就有），记出一条 `had=false allocated=false`
    /// 的假账；而同一轮 `GenerateConsoleCtrlEvent` 又是成功的 —— 自相矛盾正是这个错判据造成的。
    /// `GetConsoleProcessList` 在无 console 时返回 0，才是「有没有 console」的直接判据。
    fn has_console() -> bool {
        let mut buf = [0u32; 1];
        // SAFETY: 传入长度与缓冲区一致；无 console 时返回 0，缓冲区不足时返回所需长度（>0）。
        unsafe { GetConsoleProcessList(buf.as_mut_ptr(), 1) != 0 }
    }

    fn run_both_flag_combos(dir: &Path, mode: &str, alloc: bool) -> String {
        // 先装自己的处理器，再做任何投递 —— 顺序反了等于裸奔一整个 one_run。
        // SAFETY: handler 是 'static fn，无捕获。
        let guarded = unsafe { SetConsoleCtrlHandler(Some(parent_ignore_handler), 1) } != 0;
        trace(dir, mode, &format!("guard-installed={guarded}"));

        let had_console = has_console();
        // SAFETY: 只读查询。留着只为记账：它与 had_console 的差值本身就是一条信息
        // （服务里两者都 false；CI 的 pwsh 里 console 有、窗口无）。
        let had_console_window = unsafe { !GetConsoleWindow().is_null() };
        // 服务模式下本来就没有 console；正向对照那一侧若恰好也没有（CI 的 shell 不保证给），
        // 就自建一个，否则「对照」测的是另一件事。是否自建如实记进结果。
        //
        // ⚠️ `AllocConsole` 会把本进程的标准句柄改指到新 console ⇒ **此后 stdout 不再流向
        // 外层日志**。这就是首跑「零 stdout」的成因，不是进程没跑。观测一律走 trace 文件。
        let allocated = if had_console || !alloc {
            false
        } else {
            // SAFETY: 无参数；失败时返回 0，后续按 had_console=false 记录。
            unsafe { AllocConsole() != 0 }
        };
        trace(
            dir,
            mode,
            &format!("console had={had_console} window={had_console_window} allocated={allocated}"),
        );

        trace(dir, mode, "run1-begin (GROUP|NO_WINDOW)");
        let a = one_run(
            dir,
            mode,
            "CREATE_NEW_PROCESS_GROUP|CREATE_NO_WINDOW",
            CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
        );
        trace(dir, mode, "run1-end");

        // 上一轮步骤 ③ 结尾 `FreeConsole` 之后本进程可能已经没有 console 了。不补回来的话，
        // 第二轮测的就不是「同样条件下换个 flag」，而是「没有 console 时换个 flag」——
        // 两个变量一起动，结论作废。
        if alloc && !has_console() {
            // SAFETY: 无参数；失败返回 0。
            let re = unsafe { AllocConsole() != 0 };
            trace(dir, mode, &format!("console-restored={re}"));
        }

        trace(dir, mode, "run2-begin (GROUP)");
        let b = one_run(
            dir,
            mode,
            "CREATE_NEW_PROCESS_GROUP",
            CREATE_NEW_PROCESS_GROUP,
        );
        trace(dir, mode, "run2-end");
        format!(
            "{{\"had_console\":{had_console},\"had_console_window\":{had_console_window},\"allocated_console\":{allocated},\
             \"alloc_allowed\":{alloc},\"guard_installed\":{guarded},\"runs\":[{},{}]}}",
            a.to_json(),
            b.to_json()
        )
    }

    fn one_run(dir: &Path, mode: &str, flags_label: &'static str, flags: u32) -> Outcome {
        let started = dir.join("child-started");
        let graceful = dir.join("child-graceful");
        let _ = std::fs::remove_file(&started);
        let _ = std::fs::remove_file(&graceful);

        let exe = std::env::current_exe().expect("拿不到自身路径");
        // 三个标准流一律给 null，**不继承**。
        //
        // 2026-08-12 第二跑就死在这：上一轮步骤 ③ 结尾 `FreeConsole()` 之后，本进程的
        // stdout/stderr 句柄指向已释放的 console ⇒ 失效；`Command` 默认继承它们，
        // `CreateProcessW` 拿到无效句柄直接失败 ⇒ `.expect("起不来子进程")` panic。
        // 子进程的观测本来就走文件，不需要任何一条标准流。
        let mut child = std::process::Command::new(exe)
            .arg("child")
            .arg(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(flags)
            .spawn()
            .expect("起不来子进程");
        let pid = child.id();

        let mut out = Outcome {
            flags_label,
            child_pid: pid,
            child_started: String::new(),
            step2_api_ok: false,
            step2_err: 0,
            step2_graceful: false,
            step3_ran: false,
            step3_attach_ok: false,
            step3_attach_err: 0,
            step3_api_ok: false,
            step3_err: 0,
            step3_graceful: false,
        };

        // 等子进程把处理器注册好再投递 —— 投早了「没收到」只是竞态，不是结论。
        out.child_started = wait_file(&started, Duration::from_secs(10)).unwrap_or_default();
        trace(
            dir,
            mode,
            &format!(
                "  child pid={pid} started={}",
                !out.child_started.is_empty()
            ),
        );

        // ② 直接投递。
        // SAFETY: 目标是 CREATE_NEW_PROCESS_GROUP 起的进程组组长（= child pid）。
        out.step2_api_ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } != 0;
        if !out.step2_api_ok {
            out.step2_err = last_error();
        }
        out.step2_graceful = wait_file(&graceful, Duration::from_secs(3)).is_some();
        trace(
            dir,
            mode,
            &format!(
                "  step2 api_ok={} err={} graceful={}",
                out.step2_api_ok, out.step2_err, out.step2_graceful
            ),
        );

        // ③ FreeConsole → AttachConsole(child) → 重申自身处理器 → 重试。
        if !out.step2_graceful {
            out.step3_ran = true;
            // SAFETY: 先脱离当前 console（服务模式下本来就没有，返回 0 无害）。
            unsafe { FreeConsole() };
            // SAFETY: attach 到子进程的 console（若它有）。
            out.step3_attach_ok = unsafe { AttachConsole(pid) } != 0;
            if !out.step3_attach_ok {
                out.step3_attach_err = last_error();
            }
            // 换了 console 之后重申一次自己的处理器。
            // **不用 `SetConsoleCtrlHandler(NULL, TRUE)`**：它的语义只是「忽略 CTRL+C」，
            // 对 CTRL_BREAK 无效，走默认处理器就是当场终止本进程 —— 首跑那次静默退出的头号嫌疑。
            // SAFETY: handler 是 'static fn，无捕获。
            unsafe { SetConsoleCtrlHandler(Some(parent_ignore_handler), 1) };
            // SAFETY: 同 ②。
            out.step3_api_ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) } != 0;
            if !out.step3_api_ok {
                out.step3_err = last_error();
            }
            out.step3_graceful = wait_file(&graceful, Duration::from_secs(3)).is_some();
            // SAFETY: 还原，别把子进程的 console 一直攥着。
            unsafe { FreeConsole() };
            trace(
                dir,
                mode,
                &format!(
                    "  step3 attach_ok={} attach_err={} api_ok={} err={} graceful={}",
                    out.step3_attach_ok,
                    out.step3_attach_err,
                    out.step3_api_ok,
                    out.step3_err,
                    out.step3_graceful
                ),
            );
        }

        // 不管结论如何，别留孤儿。
        let _ = child.kill();
        let _ = child.wait();
        out
    }

    // ── 服务模式 ────────────────────────────────────────────────────────────────
    //
    // 没有复用 `platform::windows::service::win` 那套：它的 `service_main_entry` 直接绑死
    // helper 的 serve 循环（命名管道 + 请求分发），拿不出一个「跑任意闭包」的入口。
    // 而且探针**恰恰要与 helper 自身的 console 处理相互隔离** —— 复用会让「谁注册了处理器」
    // 变成一个额外变量，正是这个实验最不能有的东西。

    static STATUS_HANDLE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn service_entry() {
        let name_w: &'static [u16] = Box::leak(wide_null(SERVICE_NAME).into_boxed_slice());
        let table: [SERVICE_TABLE_ENTRYW; 2] = [
            SERVICE_TABLE_ENTRYW {
                lpServiceName: name_w.as_ptr() as *mut _,
                lpServiceProc: Some(service_main),
            },
            SERVICE_TABLE_ENTRYW {
                lpServiceName: std::ptr::null_mut(),
                lpServiceProc: None,
            },
        ];
        // SAFETY: 表以 NULL 项终止；阻塞直到 SCM 调完 service_main。
        if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
            // 起不来分派器时把成因落盘：CI 侧「结果文件不存在」必须能区分
            // 「服务没跑」和「跑了但没写」——否则就是一个静默的空转。
            let dir = std::env::args().nth(2).unwrap_or_else(|| ".".into());
            write_atomic(
                &PathBuf::from(dir).join("service-dispatcher-error.txt"),
                &format!(
                    "StartServiceCtrlDispatcherW 失败，GetLastError={}",
                    last_error()
                ),
            );
            std::process::exit(4);
        }
    }

    extern "system" fn service_ctrl_handler(
        _ctrl: u32,
        _ty: u32,
        _data: *mut std::ffi::c_void,
        _ctx: *mut std::ffi::c_void,
    ) -> u32 {
        0 // NO_ERROR：探针不接受 STOP，跑完自己就退
    }

    extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
        let name_w = wide_null(SERVICE_NAME);
        // SAFETY: 注册 SCM 控制处理器；handler 为 'static fn。
        let h = unsafe {
            RegisterServiceCtrlHandlerExW(
                name_w.as_ptr(),
                Some(service_ctrl_handler),
                std::ptr::null(),
            )
        };
        if h.is_null() {
            return;
        }
        STATUS_HANDLE.store(h as usize, Ordering::SeqCst);
        set_status(h, SERVICE_START_PENDING);
        // 先报 RUNNING 再干活：实验本身要十几秒，SCM 等不了那么久。
        set_status(h, SERVICE_RUNNING);

        // service 的命令行参数就是 binPath 里那串，故 args()[2] 仍是 workdir。
        let dir = PathBuf::from(std::env::args().nth(2).unwrap_or_else(|| ".".into()));
        // **先跑 no-alloc**：这一格才是真实 helper 今天的处境（服务从不 AllocConsole）。
        // 放在前面是因为它要求进程处于「从没碰过 console」的原始状态；跑在 alloc 那一遍之后
        // 就已经被污染了。两格的差值直接回答「改 helper 时要不要先 AllocConsole」。
        run_and_report(&dir, "service-noalloc", false);
        run_and_report(&dir, "service", true);

        set_status(h, SERVICE_STOPPED);
    }

    fn set_status(h: SERVICE_STATUS_HANDLE, state: u32) {
        let st = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: 0,
            dwWin32ExitCode: 0,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: 30_000,
        };
        // SAFETY: h 来自 RegisterServiceCtrlHandlerExW，非空已检查。
        unsafe { SetServiceStatus(h, &st) };
    }

    // ── 小工具 ──────────────────────────────────────────────────────────────────

    fn last_error() -> u32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(-1) as u32
    }

    fn wide_null(s: &str) -> Vec<u16> {
        OsString::from(s).encode_wide().chain(Some(0)).collect()
    }

    fn wait_file(p: &Path, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(s) = std::fs::read_to_string(p) {
                return Some(s);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// 先写临时文件再 rename：`wait_file` 在轮询，读到半截内容会让判据变成竞态。
    fn write_atomic(p: &Path, content: &str) {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, content).is_ok() {
            let _ = std::fs::rename(&tmp, p);
        }
    }
}
