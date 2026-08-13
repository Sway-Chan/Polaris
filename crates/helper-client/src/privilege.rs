//! [`PrivilegeEscalation`] —— 提权回退的纯逻辑决策。
//!
//! ## 职责（移植自 上游 `PlatformPrivilegeService.ts`）
//!
//! helper 不可用（未装 / 未就绪 / 装卸过渡期）时，回退到平台提权机制跑一次性 root 脚本：
//! - **macOS**：`osascript` `do shell script "..." with administrator privileges`（弹一次密码框）。
//!   （`PlatformPrivilegeService.ts:592`、`HelperManager.ts:888-906`）
//! - **Windows**：UAC PowerShell `Start-Process -Verb RunAs`（弹一次 UAC 框）。
//!   （`PlatformPrivilegeService.ts:482,710`）
//! - **Linux**：`pkexec /bin/bash <script>`（弹一次 polkit 密码框）。
//!   （`PlatformPrivilegeService.ts:162`）
//!
//! ### Linux：deb 包必须声明 pkexec 依赖（2026-08-05 补）
//!
//! `pkexec` 不是基础系统组件，精简发行版 / server 镜像上可能没有 —— 缺了它这条提权腿直接失败，
//! 用户看到的是「提权助手安装失败」而不是「缺依赖」。`tauri.conf.json` 的
//! `bundle.linux.deb.recommends` 已加 `"pkexec | policykit-1"`。
//!
//! [用 `recommends` 不用 `depends`：没有 pkexec 时应用仍可用（系统代理 / 手动代理模式照常），
//! 只是 TUN 与 helper 装不了；`depends` 会让这类用户**装都装不上**，比降级更糟。apt 默认安装
//! Recommends，正常桌面用户拿到的效果与 depends 一致。与 上游 `electron-builder.json` 的
//! `deb.recommends: policykit-1` 同强度]
//!
//! [写成 `pkexec | policykit-1` 而非单一包名：Debian 12 / Ubuntu 23.04 起 `policykit-1` 已拆成
//! `polkitd` + `pkexec`，只写 `policykit-1` 在新发行版上会指向一个可能不存在的过渡包；只写
//! `pkexec` 又在 Ubuntu 22.04（本仓的构建基线）上不存在。`|` 是 Debian 控制字段的择一依赖语法，
//! Tauri 的 deb bundler 只做字符串拼接，原样透传]
//!
//! ## 纯逻辑决策
//!
//! 本模块**只决定用哪种机制 + 构造命令 argv**，**不执行**（执行需 spawn / 权限，属宿主操作）。
//! 决策结果 [`Escalation`] 是 argv + 用户取消判定 —— 上层（Tauri 主进程 / 测试）拿 argv 自行 spawn，
//! 或注入 [`Executor`](crate::privilege) trait mock。这样本模块零宿主依赖、可在 Linux 上测 macOS 决策。
//!
//! ## 移植纪律
//!
//! 1. 纯逻辑：决策 + argv 构造，无 spawn。
//! 2. `forbid(unsafe_code)`。
//! 3. 错误码区分用户取消（126/127/-128）vs 脚本失败，对齐 上游 `runPkexecScript` / `runRootScript` 语义。

use crate::ClientError;
use std::process::{Command, ExitStatus};

/// 三平台提权机制（对应 Polaris 的三套 spawn 路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegeMethod {
    /// macOS osascript（`HelperManager.ts:888` 的 `spawn('/usr/bin/osascript', [...])`）。
    Osascript,
    /// Windows UAC PowerShell（`PlatformPrivilegeService.ts:710` 的 RunAs taskkill）。
    Uac,
    /// Linux pkexec（`PlatformPrivilegeService.ts:162` 的 `spawn('/usr/bin/pkexec', ['/bin/bash', scriptPath])`）。
    Pkexec,
}

/// 一次提权决策：用什么机制 + 跑什么脚本。
///
/// 不含执行 —— 仅 argv，上层据此 spawn。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escalation {
    /// 机制（决定 argv 头部 + 取消码判定）。
    pub method: PrivilegeMethod,
    /// 完整 argv（argv[0] = 解释器路径，argv[1..] = 脚本/参数）。
    pub argv: Vec<String>,
}

/// 提权执行结果（区分用户取消 / 脚本失败 / 成功，对齐 Polaris 三套 runXxxScript 语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationOutcome {
    /// 成功（退出码 0）。
    Success,
    /// 用户取消授权（macOS osascript -128 / Linux pkexec 126-127 / Windows UAC 拒绝）。
    Cancelled,
    /// 脚本执行失败（非取消的非零退出）。
    Failed { stderr: String, code: i32 },
}

impl PrivilegeMethod {
    /// 当前平台的默认机制（运行期 target 决定）。
    #[must_use]
    pub fn for_current_platform() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Osascript
        }
        #[cfg(target_os = "windows")]
        {
            Self::Uac
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            Self::Pkexec
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
        {
            Self::Pkexec // unreachable on supported platforms
        }
    }
}

/// 构造 macOS osascript 提权 argv（移植自 `HelperManager.ts:888`）。
///
/// 对应 Polaris：
/// ```ignore
/// spawn('/usr/bin/osascript', ['-e',
///   `do shell script "/bin/bash '<script>'" with administrator privileges`])
/// ```
///
/// 返回的 argv 直接喂 `Command::new(argv[0]).args(&argv[1..])`。
#[must_use]
pub fn osascript_escalation(script_path: &str) -> Escalation {
    // script_path 已是私有 0700 目录 + 随机名（TOCTOU 加固，HelperManager.ts:874-884）
    // 这里对路径做 shell 单引号转义（防含空格/撇号家目录击穿引号）+ AppleScript 转义
    let escaped = applescript_escape(&shell_quote(script_path));
    let argv = vec![
        "/usr/bin/osascript".to_owned(),
        "-e".to_owned(),
        format!("do shell script \"/bin/bash {escaped}\" with administrator privileges"),
    ];
    Escalation {
        method: PrivilegeMethod::Osascript,
        argv,
    }
}

/// 构造 Linux pkexec 提权 argv（移植自 `PlatformPrivilegeService.ts:162`）。
///
/// 对应 Polaris：`spawn('/usr/bin/pkexec', ['/bin/bash', scriptPath])`。
#[must_use]
pub fn pkexec_escalation(script_path: &str) -> Escalation {
    Escalation {
        method: PrivilegeMethod::Pkexec,
        argv: vec![
            "/usr/bin/pkexec".to_owned(),
            "/bin/bash".to_owned(),
            script_path.to_owned(),
        ],
    }
}

/// 构造 Windows UAC PowerShell 提权 argv（移植自 `PlatformPrivilegeService.ts:482` 的 Start-Process -Verb RunAs）。
///
/// 对应 Polaris：`Start-Process` 经 `powershell -Command` 以 `-Verb RunAs` 触发 UAC。
#[must_use]
pub fn uac_escalation(script_path: &str) -> Escalation {
    // PowerShell -File 执行脚本（含空格路径用单引号包裹）
    //
    // # 🔴 `-ExecutionPolicy Bypass` 不可省
    //
    // `powershell.exe -File <x>.ps1` 受执行策略约束，而 **Restricted 是 Windows 客户端 SKU 的出厂
    // 默认**（Server 2012 R2+ 才是 RemoteSigned）⇒ 不带这个 flag，脚本在**未改过策略的机器上一行
    // 都跑不了**。外层的 `-Command` 不受策略约束，所以只有内层需要。
    //
    // 同仓另一条同手法的腿一直带着它：`src-tauri/nsis-hooks.nsh:134`（卸载钩子，外层内层都带），
    // 被移植的上游 上游 `src/main/services/WindowsServiceHelper.ts` 同样带 —— 本处属移植时丢失，
    // 不是刻意取舍。
    //
    // # 退出码：`-PassThru` 回传，读不到时保持旧行为
    //
    // `Start-Process -Wait` 本身不透传子进程退出码，外层 `-Command` 在 cmdlet 正常收尾时恒退 0
    // ⇒ 脚本内部 `throw`（Copy-Item 退避耗尽 / New-Service 1072 重试耗尽 / sc delete 失败）
    // 全部被谎报成 `EscalationOutcome::Success`，而卸载腿据此 `clear_token()`，把 app 侧 token
    // 删掉、SYSTEM 服务还在跑 ⇒ 之后恒鉴权失败且「修复」也修不回来。
    //
    // `$p.HasExited` 守卫的用意：`-Verb RunAs` 是跨提权会话起进程，个别环境下 `-PassThru` 拿到的
    // 对象读不出 `ExitCode`。读得到就精确回传；读不到就**退回原来的 0**，只增精度不引回归 ——
    // 反过来「读不到就判失败」会把装成功的机器也报成失败。
    let argv = vec![
        // 仍走 PATH 解析（`nsis-hooks.nsh:134` 那侧用的是 `$SYSDIR\WindowsPowerShell\v1.0\powershell.exe`
        // 绝对路径）。这里不跟进的理由：本函数是纯函数、有 argv 逐字单测，读 `%SystemRoot%` 会把
        // 环境依赖带进判据；而 PATH 劫持的前提是攻击者已能改本用户的 PATH，那时提权面本就更大。
        // 记在这里是为了让下一个人知道**这是取舍不是遗漏**。
        "powershell.exe".to_owned(),
        "-NoProfile".to_owned(),
        "-Command".to_owned(),
        format!(
            "$p = Start-Process -FilePath 'powershell.exe' \
             -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{path}' \
             -Verb RunAs -Wait -PassThru; \
             if ($p -and $p.HasExited) {{ exit $p.ExitCode }} else {{ exit 0 }}",
            path = script_path.replace('\'', "''")
        ),
    ];
    Escalation {
        method: PrivilegeMethod::Uac,
        argv,
    }
}

/// 判定退出码是否为「用户取消授权」（对齐 Polaris 三套 runXxxScript 的取消判定）。
///
/// - macOS osascript：退出码 -128 / stderr 含 "User canceled"（`HelperManager.ts:903-905`）
/// - Linux pkexec：退出码 126（取消）/ 127（无认证代理）（`PlatformPrivilegeService.ts:171-173`）
/// - Windows UAC：脚本返回非零退出码（取消 UAC 致进程仍活，`PlatformPrivilegeService.ts:733-744`）
#[must_use]
pub fn is_user_cancelled(method: PrivilegeMethod, code: i32, stderr: &str) -> bool {
    match method {
        PrivilegeMethod::Osascript => {
            // HelperManager.ts:903: /-128|User canceled/i.test(stderr)
            code == -128
                || stderr.contains("-128")
                || stderr.to_lowercase().contains("user canceled")
        }
        PrivilegeMethod::Pkexec => {
            // PlatformPrivilegeService.ts:171: code === 126 (取消) || 127 (无认证代理)
            code == 126 || code == 127
        }
        PrivilegeMethod::Uac => {
            // UAC 取消导致 Start-Process 抛错；简化：stderr 含 'canceled'/'拒绝' 类标记
            // 真实 UAC 取消在 Windows 上以 PowerShell 异常形式返回，这里保守判定
            code != 0
                && (stderr.to_lowercase().contains("cancel")
                    || stderr.contains("拒绝")
                    || stderr.contains("已取消"))
        }
    }
}

/// 把退出码 + stderr 归类为 [`EscalationOutcome`]。
#[must_use]
pub fn classify_outcome(
    method: PrivilegeMethod,
    status: ExitStatus,
    stderr: &str,
) -> EscalationOutcome {
    let code = status.code().unwrap_or(-1);
    if status.success() {
        return EscalationOutcome::Success;
    }
    if is_user_cancelled(method, code, stderr) {
        return EscalationOutcome::Cancelled;
    }
    EscalationOutcome::Failed {
        stderr: stderr.trim().to_owned(),
        code,
    }
}

// ===== 内部转义工具（移植自 Polaris shq + osaShellArg）=====

/// shell 单引号转义（移植自 上游 `shq`：把路径包进单引号，内部单引号用 '\'' 关闭再开）。
///
/// `pub(crate)`：install 脚本生成（[`manager`](crate::manager) 的 mac/linux `build_*_install_script`）
/// 复用同一 shell 转义逻辑 —— 单一真值，与 osascript 提权 argv 的转义同源（Polaris 全侧共用 `shq`）。
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// AppleScript 字符串转义（移植自 `HelperManager.ts:854-857` 的 `osaShellArg`）。
///
/// shq 后再做：反斜杠翻倍 + 双引号转义（嵌入 AppleScript 双引号字符串）。
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ===== Executor trait（可选：把 spawn 也抽象，便于测试）=====

/// 提权脚本执行器 trait —— 抽象 `Command::spawn` + 等待 + 捕获 stderr。
///
/// 生产实现 [`StdExecutor`] 真正 spawn；测试实现可 mock 退出码 + stderr。
pub trait Executor: Send {
    /// 执行一个提权 argv，返回 stderr（去首尾空白）+ 退出码。
    fn execute(&self, argv: &[String]) -> Result<(String, i32), ClientError>;
}

/// 生产执行器：真正 spawn argv + 捕获 stderr。
pub struct StdExecutor;

impl Executor for StdExecutor {
    fn execute(&self, argv: &[String]) -> Result<(String, i32), ClientError> {
        if argv.is_empty() {
            return Err(ClientError::Connect("empty argv".into()));
        }
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        // Windows：提权载体是 `powershell -Command Start-Process -Verb RunAs`，powershell 本身是
        // console 程序 ⇒ 宿主（GUI 子系统，无控制台）起它会新分配一个控制台窗口。UAC 对话框由系统在
        // 安全桌面弹出，与这里无关，抑制窗口不影响提权可见性。
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let output = cmd
            .output()
            .map_err(|e| ClientError::Connect(format!("spawn 失败: {e}")))?;
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        Ok((stderr, output.status.code().unwrap_or(-1)))
    }
}

/// 跑一次提权脚本并归类结果。
///
/// 便利函数：决策（`escalation`）+ 执行（`executor`）+ 归类（[`classify_outcome`]）一站式。
pub fn run_escalation(
    escalation: &Escalation,
    executor: &dyn Executor,
) -> Result<EscalationOutcome, ClientError> {
    let (stderr, code) = executor.execute(&escalation.argv)?;
    let method = escalation.method;
    if code == 0 {
        Ok(EscalationOutcome::Success)
    } else if is_user_cancelled(method, code, &stderr) {
        Ok(EscalationOutcome::Cancelled)
    } else {
        Ok(EscalationOutcome::Failed {
            stderr: stderr.trim().to_owned(),
            code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 测试用 executor：按预设返回 (stderr, code)。
    type MockResults = Arc<Mutex<Vec<(String, i32)>>>;

    struct MockExecutor {
        results: MockResults,
    }

    impl Executor for MockExecutor {
        fn execute(&self, _argv: &[String]) -> Result<(String, i32), ClientError> {
            let mut guard = self.results.lock().unwrap();
            if guard.is_empty() {
                return Ok((String::new(), 0));
            }
            Ok(guard.remove(0))
        }
    }

    fn executor_with(results: Vec<(String, i32)>) -> (MockExecutor, MockResults) {
        let results: MockResults = Arc::new(Mutex::new(results));
        let e = MockExecutor {
            results: results.clone(),
        };
        (e, results)
    }

    #[test]
    fn osascript_escalation_argv_structure() {
        // 对照 HelperManager.ts:888-892: osascript -e 'do shell script "/bin/bash <path>" with admin'
        let esc = osascript_escalation("/tmp/install.sh");
        assert_eq!(esc.method, PrivilegeMethod::Osascript);
        assert_eq!(esc.argv[0], "/usr/bin/osascript");
        assert_eq!(esc.argv[1], "-e");
        assert!(esc.argv[2].contains("do shell script"));
        assert!(esc.argv[2].contains("with administrator privileges"));
        assert!(esc.argv[2].contains("/tmp/install.sh"));
    }

    #[test]
    fn pkexec_escalation_argv_structure() {
        // 对照 PlatformPrivilegeService.ts:162: pkexec /bin/bash <path>
        let esc = pkexec_escalation("/tmp/setcap.sh");
        assert_eq!(esc.method, PrivilegeMethod::Pkexec);
        assert_eq!(
            esc.argv,
            vec!["/usr/bin/pkexec", "/bin/bash", "/tmp/setcap.sh"]
        );
    }

    #[test]
    fn uac_escalation_argv_structure() {
        // 对照 PlatformPrivilegeService.ts:482: Start-Process -Verb RunAs
        let esc = uac_escalation(r"C:\temp\install.ps1");
        assert_eq!(esc.method, PrivilegeMethod::Uac);
        assert_eq!(esc.argv[0], "powershell.exe");
        assert!(esc.argv.iter().any(|a| a.contains("Start-Process")));
        assert!(esc.argv.iter().any(|a| a.contains("-Verb RunAs")));
        assert!(esc.argv.iter().any(|a| a.contains(r"C:\temp\install.ps1")));
    }

    #[test]
    fn osascript_path_with_spaces_quoted() {
        // 含空格家目录不应击穿引号（Polaris osaShellArg 两层转义，HelperManager.ts:854）
        let esc = osascript_escalation("/Users/some user/App Data/install.sh");
        // argv[2] 是 AppleScript 字符串，路径应被 shell 单引号包裹
        assert!(esc.argv[2].contains("'/Users/some user/App Data/install.sh'"));
    }

    #[test]
    fn osascript_path_with_apostrophe_escaped() {
        // 含撇号家目录：shell_quote 用 '\'' 关闭再开单引号（防 shell 注入，Polaris shq）
        // 直接测 shell_quote（安全关键层）—— applescript_escape 后反斜杠会翻倍，故不查 argv
        let quoted = shell_quote("/Users/o'brien/install.sh");
        // shq 输出: '/Users/o'\''brien/install.sh'（撇号用 '\'' 转义）
        assert!(quoted.contains("'\\''"), "撇号须被 '\\'' 转义: {quoted}");
        assert!(quoted.starts_with('\'') && quoted.ends_with('\''));
    }

    #[test]
    fn is_cancelled_osascript_minus_128() {
        // HelperManager.ts:903: code === -128 或 stderr 含 -128 / "User canceled"
        assert!(is_user_cancelled(PrivilegeMethod::Osascript, -128, ""));
        assert!(is_user_cancelled(
            PrivilegeMethod::Osascript,
            1,
            "User canceled"
        ));
        assert!(is_user_cancelled(
            PrivilegeMethod::Osascript,
            1,
            "some -128 error"
        ));
        assert!(!is_user_cancelled(
            PrivilegeMethod::Osascript,
            1,
            "mkdir failed"
        ));
    }

    #[test]
    fn is_cancelled_pkexec_126_127() {
        // PlatformPrivilegeService.ts:171: 126 = 取消, 127 = 无认证代理
        assert!(is_user_cancelled(PrivilegeMethod::Pkexec, 126, ""));
        assert!(is_user_cancelled(PrivilegeMethod::Pkexec, 127, ""));
        // 3 = 缺 setcap（非取消）
        assert!(!is_user_cancelled(PrivilegeMethod::Pkexec, 3, ""));
    }

    #[test]
    fn classify_success() {
        let status = exit_status_for(0);
        assert_eq!(
            classify_outcome(PrivilegeMethod::Pkexec, status, ""),
            EscalationOutcome::Success
        );
    }

    #[test]
    fn classify_cancelled() {
        let status = exit_status_for(126);
        assert_eq!(
            classify_outcome(PrivilegeMethod::Pkexec, status, ""),
            EscalationOutcome::Cancelled
        );
    }

    #[test]
    fn classify_failed() {
        let status = exit_status_for(1);
        assert_eq!(
            classify_outcome(PrivilegeMethod::Osascript, status, "mkdir: error"),
            EscalationOutcome::Failed {
                stderr: "mkdir: error".to_owned(),
                code: 1
            }
        );
    }

    #[test]
    fn run_escalation_success_via_executor() {
        let esc = pkexec_escalation("/tmp/x.sh");
        let (exec, _) = executor_with(vec![("".to_owned(), 0)]);
        let outcome = run_escalation(&esc, &exec).unwrap();
        assert_eq!(outcome, EscalationOutcome::Success);
    }

    #[test]
    fn run_escalation_cancelled_via_executor() {
        let esc = pkexec_escalation("/tmp/x.sh");
        let (exec, _) = executor_with(vec![("canceled".to_owned(), 126)]);
        let outcome = run_escalation(&esc, &exec).unwrap();
        assert_eq!(outcome, EscalationOutcome::Cancelled);
    }

    #[test]
    fn run_escalation_failed_via_executor() {
        let esc = pkexec_escalation("/tmp/x.sh");
        let (exec, _) = executor_with(vec![("setcap: error".to_owned(), 3)]);
        let outcome = run_escalation(&esc, &exec).unwrap();
        assert_eq!(
            outcome,
            EscalationOutcome::Failed {
                stderr: "setcap: error".to_owned(),
                code: 3
            }
        );
    }

    #[test]
    fn shell_quote_basic() {
        assert_eq!(shell_quote("simple"), "'simple'");
        assert_eq!(shell_quote("with space"), "'with space'");
    }

    #[test]
    fn shell_quote_apostrophe() {
        // shq: o'brien -> 'o'\''brien'
        assert_eq!(shell_quote("o'brien"), "'o'\\''brien'");
    }

    #[test]
    fn applescript_escape_doubles_backslash_and_quote() {
        assert_eq!(applescript_escape(r"a\b"), r"a\\b");
        assert_eq!(applescript_escape(r#"a"b"#), r#"a\"b"#);
    }

    #[test]
    fn privilege_method_current_platform_is_one_of_three() {
        let m = PrivilegeMethod::for_current_platform();
        assert!(matches!(
            m,
            PrivilegeMethod::Osascript | PrivilegeMethod::Uac | PrivilegeMethod::Pkexec
        ));
    }

    /// 构造一个测试用 ExitStatus（正常退出码 N，非信号）。
    ///
    /// unix 的 wait status 编码：高 8 位 = 正常退出码，低 7 位 = 信号（0 = 正常退出）。
    /// `from_raw(code << 8)` 让 `.code()` 正确返回 `Some(code)`（避免 from_raw(126) 被当信号 → None）。
    fn exit_status_for(code: i32) -> ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(code << 8)
        }
        #[cfg(not(unix))]
        {
            // 非 unix（Windows CI 腿）：用 `cmd /c exit N` 构造真 ExitStatus。
            //
            // **必须原样透传 code**：早先这里写 `if code == 0 { "0" } else { "1" }`，把一切非零
            // 压成 1 —— 而 126 恰是 pkexec「用户取消」的判据码（`is_user_cancelled`），于是
            // `classify_cancelled` 在 Windows 上拿到的是 `Failed { code: 1 }`，断言必红。
            // `cmd /c exit N` 对任意 0..=255 保真，无需降级。
            let exit_arg = code.to_string();
            Command::new("cmd")
                .args(["/c", "exit", &exit_arg])
                .status()
                .unwrap()
        }
    }

    // 非 unix 占位 trait 已移除（from_exit_code 在 unix 上正确工作，本 crate 测试在 linux 跑）。

    /// Windows UAC 提权 argv 的三条硬约束。本函数此前**零测试覆盖** —— 改坏了不会红。
    ///
    /// 判据落在**可执行形态**（ArgumentList 里的实参、退出码回传语句）上，不是「注释里提没提过」。
    #[test]
    fn uac_argv_bypasses_execution_policy_and_returns_the_exit_code() {
        let esc = uac_escalation(r"C:\Users\u\AppData\Roaming\x\ab-polaris-helper-install.ps1");
        assert_eq!(esc.method, PrivilegeMethod::Uac);
        let cmd = &esc.argv[3];

        // ① 执行策略：Restricted 是 Windows **客户端 SKU 的出厂默认**，不带 Bypass 则 `-File` 一行都跑不了。
        assert!(
            cmd.contains("'-ExecutionPolicy','Bypass'"),
            "内层 ArgumentList 丢了 -ExecutionPolicy Bypass：出厂默认策略的机器上脚本不会执行\n{cmd}"
        );
        // 顺序也要对：必须在 '-File' 之前（PowerShell 的 flag 要在 -File 之前给，-File 之后的都算脚本参数）。
        let ep = cmd.find("'-ExecutionPolicy'").expect("上一条已断言存在");
        let file = cmd.find("'-File'").expect("-File 不见了");
        assert!(
            ep < file,
            "-ExecutionPolicy 必须排在 -File 之前，否则会被当成脚本参数"
        );

        // ② 退出码回传：Start-Process -Wait 本身不透传子进程退出码。
        assert!(
            cmd.contains("-PassThru"),
            "没有 -PassThru 就拿不到子进程对象"
        );
        assert!(
            cmd.contains("exit $p.ExitCode"),
            "拿到了 -PassThru 却不 exit 回去，等于没拿：脚本内 throw 仍会被谎报成功"
        );
        // 读不到 ExitCode 时保持旧的 0，不引入「装成功也报失败」的回归。
        assert!(cmd.contains("$p.HasExited"), "缺少 HasExited 守卫");
        assert!(cmd.contains("else { exit 0 }"), "缺少读不到时的回落");

        // ③ 路径仍被单引号包裹。
        assert!(cmd.contains(
            "'-File','C:\\Users\\u\\AppData\\Roaming\\x\\ab-polaris-helper-install.ps1'"
        ));
    }

    /// 路径里的单引号不得击穿 PowerShell 字符串（`''` 是 PS 单引号串里的转义写法）。
    #[test]
    fn uac_argv_escapes_single_quotes_in_the_script_path() {
        let esc = uac_escalation(r"C:\Users\o'brien\x.ps1");
        let cmd = &esc.argv[3];
        assert!(
            cmd.contains(r"'C:\Users\o''brien\x.ps1'"),
            "单引号未按 PS 规则加倍\n{cmd}"
        );
        // 反向：不得留下奇数个引号把命令拆断。
        assert_eq!(
            cmd.matches('\'').count() % 2,
            0,
            "引号总数为奇 → 命令被击穿"
        );
    }
}
