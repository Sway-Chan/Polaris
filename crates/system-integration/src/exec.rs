//! 命令执行缝（本 crate 与宿主 OS 的**唯一**交互点）。
//!
//! ## 为什么是「一条缝」
//!
//! 本 crate 的平台差异只是**构造命令字符串**（`reg.exe` / `networksetup` / `gsettings`），不是平台专属
//! 类型或依赖 —— 故用「运行时 `Platform` 枚举 + 注入 trait」而非 `#[cfg(target_os)]`（审计 §M1 第二形态）。
//! 收益是 **Linux CI 100% 编译 + 跑测三平台逻辑**：命令构造与输出解析全是纯函数，mock 一注入就能断言
//! 三平台的 argv/解析，不必真跑 OS 命令。
//!
//! **接线纪律（勿破坏）**：新增平台行为时，「决定跑什么命令 / 怎么解释输出」必须留在纯函数里，
//! 本模块只负责「把已决定的命令跑掉」。一旦把判定逻辑塞进 [`StdCommandRunner`]，那部分逻辑就退回
//! 「只有真机能测」—— 正是 §G5「cfg 门内代码 Linux 看不到 → 潜伏」的同型陷阱。
//!
//! ## 语义对齐
//!
//! [`CommandRunner::run`] 对齐上游 `execFileAsync` / `execAsync`：**非零退出 / 超时 / spawn 失败 → `Err`**
//! （上游是 reject，调用方 try/catch 降级）。argv 参数化下发、**不经 shell 插值** —— 上游
//! `SystemProxyManager.ts:506,753` 明写把 shell 字符串拼接改成 `execFileAsync` 正是为了杜绝命令注入
//! （bypass 域名 / gsettings host 均可被投毒）。

#![forbid(unsafe_code)]

use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// 一条待执行命令（程序 + argv）。argv 参数化下发，不经 shell 插值（杜绝注入 —— 上游 execFileAsync 口径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
}

impl Command {
    /// 便捷构造。
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// 一次命令执行的产出。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

/// 命令执行器 —— 注入便于 mock；生产用 [`StdCommandRunner`]。
///
/// 契约：非零退出 / 超时 / spawn 失败 → `Err(原因)`（对齐上游 `execFileAsync` reject）。
pub trait CommandRunner {
    fn run(&self, cmd: &Command, timeout: Duration) -> Result<CommandOutput, String>;
}

/// `try_wait` 轮询间隔。std 无 `wait_timeout`，而本 crate **禁止引入新依赖**（`wait-timeout` / `tokio`
/// 都不引）→ 用轮询实现硬超时。10ms 对「瞬时完成的系统命令」足够细，且 CPU 开销可忽略。
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Windows `CREATE_NO_WINDOW`（winbase.h `0x0800_0000`）。
///
/// 宿主进程是 GUI 子系统（`src-tauri/src/main.rs` 的 `windows_subsystem = "windows"`）⇒ 自身**无控制台**。
/// 无控制台的父进程创建 console 子系统程序时，`CreateProcess` 会为子进程**新分配一个控制台窗口**
/// —— 用户看到黑框闪一下。而 `reg.exe` / `netsh` / `tasklist` 这些正是 console 程序。
///
/// **std 与 tokio 都不默认抑制**：`tokio::process::Command::creation_flags` 只是把值透传给
/// `std::os::windows::process::CommandExt::creation_flags`，tokio 侧无任何隐含默认
/// （实测 tokio-1.53.1 `src/process/mod.rs:675`）。故每个 Windows 可达的调用点都得显式加。
///
/// 不为一个常量引 `windows-sys`：本 crate 的硬纪律是零新依赖（见 [`POLL_INTERVAL`]）。
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 生产实现：`std::process::Command` + argv 参数化 + 硬超时（spawn → 轮询 `try_wait` → 超时 kill）。
///
/// **同步**：本 crate 的控制器（`SystemProxyController` / `SystemDnsController`）全是同步 API，
/// 故此处同步执行。调用方若在 async 语境，须自行 `spawn_blocking`（本 crate 不引 tokio）。
///
/// **管道排空**：stdout/stderr 各起一个读取线程。若改成「先轮询后读管道」，子进程输出超过管道缓冲
/// （典型 64KB）就会写阻塞 → 与轮询互等 → 死锁。`scutil --dns` 等命令输出虽小，但不留这个雷。
#[derive(Debug, Clone, Copy, Default)]
pub struct StdCommandRunner;

impl CommandRunner for StdCommandRunner {
    fn run(&self, cmd: &Command, timeout: Duration) -> Result<CommandOutput, String> {
        let mut builder = std::process::Command::new(&cmd.program);
        builder
            .args(&cmd.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Windows：抑制新控制台窗口（见 [`CREATE_NO_WINDOW`]）。本 runner 是本 crate 与 OS 的唯一缝，
        // 故 `reg.exe` / `netsh` / `tasklist` 等全部平台命令在此一处收口。
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            builder.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = builder
            .spawn()
            .map_err(|e| format!("{} 启动失败: {e}", cmd.program))?;

        // 先接管管道并起排空线程（见结构体 doc：防写阻塞死锁）。
        let out_pipe = child.stdout.take();
        let err_pipe = child.stderr.take();
        let out_thread = std::thread::spawn(move || drain(out_pipe));
        let err_thread = std::thread::spawn(move || drain(err_pipe));

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => return Err(format!("{} 等待失败: {e}", cmd.program)),
            }
            if Instant::now() >= deadline {
                // 硬超时：kill + 收割（防僵尸）。kill 后管道关闭 → 排空线程自然退出。
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} 超时（{:?}）", cmd.program, timeout));
            }
            std::thread::sleep(POLL_INTERVAL);
        };

        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();

        if !status.success() {
            // 对齐 execFileAsync：非零退出 = reject。stderr 带进错误便于诊断。
            return Err(format!(
                "{} 退出码 {}: {}",
                cmd.program,
                status
                    .code()
                    .map_or_else(|| "signal".to_string(), |c| c.to_string()),
                stderr.trim()
            ));
        }
        Ok(CommandOutput { stdout, stderr })
    }
}

// ── Windows System32 绝对路径（移植自 上游 `utils/win-system32.ts`）──

/// `%SystemRoot%`（`windir` 兜底，最终回落 `C:\Windows`）。env 注入便于单测。
/// 上游 `getSystemRoot`。
pub fn system_root(env_system_root: Option<&str>, env_windir: Option<&str>) -> String {
    env_system_root
        .filter(|s| !s.is_empty())
        .or(env_windir.filter(|s| !s.is_empty()))
        .unwrap_or("C:\\Windows")
        .to_string()
}

/// 解析 System32 下的二进制为绝对路径（`reg.exe` → `C:\Windows\System32\reg.exe`）。
///
/// **为什么不用裸命令名**：部分设备的进程 PATH 缺失 `C:\Windows\System32`（注册表 Path 值类型从
/// `REG_EXPAND_SZ` 退化为 `REG_SZ` 致 `%SystemRoot%` 不展开，或 PATH 超长被截断）→ 裸 `reg`/`netsh`/
/// `ipconfig` 解析失败报「'reg' 不是内部或外部命令」。**这不是权限问题**，绝对路径可完全绕开。
/// 上游 `system32`。
///
/// 恒生成反斜杠路径，与宿主 OS 无关 —— Linux 上单测亦得 Windows 路径（保持零 cfg 可测性）。
pub fn system32(binary: &str, env_system_root: Option<&str>, env_windir: Option<&str>) -> String {
    let root = system_root(env_system_root, env_windir);
    let root = root.trim_end_matches(['\\', '/']);
    format!("{root}\\System32\\{binary}")
}

/// 从进程环境解析 System32 二进制（生产入口；非 Windows 上取不到 env → 回落默认，无害）。
pub fn system32_from_env(binary: &str) -> String {
    let sr = std::env::var("SystemRoot").ok();
    let wd = std::env::var("windir").ok();
    system32(binary, sr.as_deref(), wd.as_deref())
}

/// 读干一个管道；读失败按空串（best-effort，输出只用于解析/诊断）。
fn drain(pipe: Option<impl Read>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut buf = Vec::new();
    if pipe.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// 测试辅助：跨模块共享的命令执行 mock。
///
/// **这是本 crate「Linux 上跑测三平台」的枢纽** —— 任意 `SystemProxyOpsImpl::with_platform(mock, Platform::Mac)`
/// 都能在 Linux CI 上断言 mac 的 argv 与输出解析，全程不碰宿主网络。
#[cfg(test)]
pub mod exec_tests_helpers {
    use super::{Command, CommandOutput, CommandRunner};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::time::Duration;

    /// 记录调用的 mock（本 crate 全部三平台逻辑测试的注入点 —— 不触碰宿主）。
    #[derive(Default)]
    pub struct MockRunner {
        pub calls: RefCell<Vec<Command>>,
        /// 按调用序返回的 stdout（队列；空则回空串）。
        pub stdouts: RefCell<Vec<String>>,
        /// 按「argv 含此子串」匹配返回 stdout（比队列更稳，不依赖调用序）。
        pub by_arg: RefCell<HashMap<String, String>>,
        /// 这些 program 的调用直接失败。
        pub fail_programs: Vec<String>,
        /// argv 含这些子串的调用直接失败。
        pub fail_args: Vec<String>,
    }

    impl MockRunner {
        /// 按 argv 子串挂 stdout。
        pub fn with_arg_stdout(self, arg_substr: &str, stdout: &str) -> Self {
            self.by_arg
                .borrow_mut()
                .insert(arg_substr.to_string(), stdout.to_string());
            self
        }

        /// 全部调用的 (program, argv) 快照。
        pub fn snapshot(&self) -> Vec<Command> {
            self.calls.borrow().clone()
        }

        /// 是否跑过某条 argv 含该子串的命令。
        pub fn ran_arg(&self, substr: &str) -> bool {
            self.calls
                .borrow()
                .iter()
                .any(|c| c.args.iter().any(|a| a.contains(substr)))
        }

        /// argv 含该子串的调用次数。
        pub fn count_arg(&self, substr: &str) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|c| c.args.iter().any(|a| a.contains(substr)))
                .count()
        }

        /// argv 含**恰好等于**该值的项的调用次数。
        ///
        /// 子串匹配在此处会骗人：`-setwebproxystate` 含 `-setwebproxy` → `count_arg` 把 state 命令
        /// 也算进去。断言「每服务设了几次代理」这类计数必须用精确匹配。
        pub fn count_arg_exact(&self, arg: &str) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|c| c.args.iter().any(|a| a == arg))
                .count()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, cmd: &Command, _timeout: Duration) -> Result<CommandOutput, String> {
            self.calls.borrow_mut().push(cmd.clone());
            if self.fail_programs.iter().any(|p| p == &cmd.program) {
                return Err("mock failure (program)".into());
            }
            if self
                .fail_args
                .iter()
                .any(|f| cmd.args.iter().any(|a| a.contains(f)))
            {
                return Err("mock failure (arg)".into());
            }
            // 优先按 argv 子串匹配。
            for (k, v) in self.by_arg.borrow().iter() {
                if cmd.args.iter().any(|a| a.contains(k)) {
                    return Ok(CommandOutput {
                        stdout: v.clone(),
                        stderr: String::new(),
                    });
                }
            }
            let mut outs = self.stdouts.borrow_mut();
            let stdout = if outs.is_empty() {
                String::new()
            } else {
                outs.remove(0)
            };
            Ok(CommandOutput {
                stdout,
                stderr: String::new(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::exec_tests_helpers::MockRunner;
    use std::cell::RefCell;

    #[test]
    fn command_new_builds_argv() {
        let c = Command::new(
            "gsettings",
            ["set", "org.gnome.system.proxy", "mode", "none"],
        );
        assert_eq!(c.program, "gsettings");
        assert_eq!(c.args.len(), 4);
        assert_eq!(c.args[3], "none");
    }

    #[test]
    fn mock_runner_records_and_returns_queued_stdout() {
        let r = MockRunner {
            stdouts: RefCell::new(vec!["hello".into()]),
            ..Default::default()
        };
        let out = r.run(&Command::new("x", [] as [&str; 0]), Duration::from_secs(1));
        assert_eq!(out.unwrap().stdout, "hello");
        assert_eq!(r.calls.borrow().len(), 1);
    }

    #[test]
    fn mock_runner_fails_listed_program() {
        let r = MockRunner {
            fail_programs: vec!["boom".into()],
            ..Default::default()
        };
        assert!(r
            .run(
                &Command::new("boom", [] as [&str; 0]),
                Duration::from_secs(1)
            )
            .is_err());
    }

    // ── StdCommandRunner：只验「执行器」本身（不碰网络/代理/DNS，仅无害的 true/false/sleep）──
    //
    // 真进程 smoke 按宿主用 `#[cfg]` 选择可执行文件；紧邻的 system32 纯函数仍保持全平台可测。

    // Windows hosted runner 在整仓并行 test 的峰值期，PowerShell 冷启动实测可越过 5s；这里验证的是
    // stdout/stderr/exit-code 契约，不是启动时延。把平台差异收在一个测试常量，避免两条烟测各漂一份。
    #[cfg(unix)]
    const COMMAND_SMOKE_TIMEOUT: Duration = Duration::from_secs(5);
    #[cfg(windows)]
    const COMMAND_SMOKE_TIMEOUT: Duration = Duration::from_secs(15);

    // ── system32（纯函数，Linux 上得 Windows 路径 —— 零 cfg 可测性的样板）──

    #[test]
    fn system_root_prefers_systemroot_then_windir_then_default() {
        assert_eq!(system_root(Some("D:\\Win"), Some("E:\\w")), "D:\\Win");
        assert_eq!(system_root(None, Some("E:\\w")), "E:\\w");
        assert_eq!(system_root(None, None), "C:\\Windows");
        // 空串视同缺失（上游 `env.SystemRoot || env.windir || ...` 的 falsy 语义）。
        assert_eq!(system_root(Some(""), Some("E:\\w")), "E:\\w");
        assert_eq!(system_root(Some(""), Some("")), "C:\\Windows");
    }

    #[test]
    fn system32_builds_backslash_absolute_path() {
        assert_eq!(
            system32("reg.exe", None, None),
            "C:\\Windows\\System32\\reg.exe"
        );
        assert_eq!(
            system32("netsh.exe", Some("D:\\Win"), None),
            "D:\\Win\\System32\\netsh.exe"
        );
        // 尾斜杠不产生双分隔符。
        assert_eq!(
            system32("ipconfig.exe", Some("D:\\Win\\"), None),
            "D:\\Win\\System32\\ipconfig.exe"
        );
    }

    #[test]
    fn std_runner_ok_on_zero_exit() {
        // Windows 用 PowerShell 而非 cmd：`[Console]::Out.Write` 输出字节可精确控制
        // （cmd 的 echo 会带 CRLF 和多余空格，无法与下面的精确相等断言对齐）。
        #[cfg(unix)]
        let cmd = Command::new("/bin/sh", ["-c", "printf out; printf err >&2"]);
        #[cfg(windows)]
        let cmd = Command::new(
            "powershell",
            [
                "-NoProfile",
                "-Command",
                "[Console]::Out.Write('out'); [Console]::Error.Write('err')",
            ],
        );
        let out = StdCommandRunner.run(&cmd, COMMAND_SMOKE_TIMEOUT);
        let out = out.expect("zero exit → Ok");
        assert_eq!(out.stdout, "out");
        assert_eq!(out.stderr, "err");
    }

    #[test]
    fn std_runner_err_on_nonzero_exit_carries_stderr() {
        #[cfg(unix)]
        let cmd = Command::new("/bin/sh", ["-c", "echo boom >&2; exit 3"]);
        #[cfg(windows)]
        let cmd = Command::new(
            "powershell",
            [
                "-NoProfile",
                "-Command",
                "[Console]::Error.Write('boom'); exit 3",
            ],
        );
        let e = StdCommandRunner
            .run(&cmd, COMMAND_SMOKE_TIMEOUT)
            .expect_err("非零退出 → Err（对齐 execFileAsync reject）");
        assert!(e.contains('3'), "错误须带退出码: {e}");
        assert!(e.contains("boom"), "错误须带 stderr: {e}");
    }

    #[test]
    fn std_runner_err_on_missing_program() {
        let e = StdCommandRunner
            .run(
                &Command::new("polaris-no-such-binary-xyz", [] as [&str; 0]),
                Duration::from_secs(5),
            )
            .expect_err("二进制缺失 → Err");
        assert!(e.contains("启动失败"), "{e}");
    }

    /// 硬超时是 [`FlushExec`](crate::dns_flush::FlushExec) 契约的一部分（上游 EXEC_TIMEOUT_MS=3s，
    /// 防挂起命令拖住 fire-and-forget 链）。若 runner 忽略 timeout 参数，本测试转红。
    #[test]
    fn std_runner_kills_on_timeout() {
        #[cfg(unix)]
        let cmd = Command::new("/bin/sh", ["-c", "sleep 30"]);
        #[cfg(windows)]
        let cmd = Command::new(
            "powershell",
            ["-NoProfile", "-Command", "Start-Sleep -Seconds 30"],
        );
        // unix 150ms 够 /bin/sh 进 sleep；Windows 上 PowerShell 冷启动约 200–400ms，
        // 沿用 150ms 会「还没开始 sleep 就超时」→ 测的变成「杀启动中的进程」而非
        // 「杀已在运行的挂起命令」。放宽到 2s（仍 ≪ 被杀命令的 30s，故断言语义不变）。
        #[cfg(unix)]
        let timeout = Duration::from_millis(150);
        #[cfg(windows)]
        let timeout = Duration::from_secs(2);
        let started = Instant::now();
        let e = StdCommandRunner.run(&cmd, timeout).expect_err("超时 → Err");
        assert!(e.contains("超时"), "{e}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "须在超时后即刻返回，实际 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn std_runner_drains_large_output_without_deadlock() {
        #[cfg(unix)]
        let cmd = Command::new("/bin/sh", ["-c", "yes polaris | head -c 300000"]);
        #[cfg(windows)]
        let cmd = Command::new(
            "powershell",
            [
                "-NoProfile",
                "-Command",
                "[Console]::Out.Write('x' * 300000)",
            ],
        );
        // 远超管道缓冲（64KB）：若不起排空线程，此处会与 try_wait 轮询互等 → 超时失败。
        let out = StdCommandRunner
            .run(&cmd, Duration::from_secs(10))
            .expect("大输出须正常收完");
        assert_eq!(out.stdout.len(), 300_000);
    }
}
