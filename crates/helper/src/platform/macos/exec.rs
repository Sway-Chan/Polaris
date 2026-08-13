//! 带超时的系统命令执行（移植自 上游 `helper/helper.go:97-125` 的三个 exec 包装）。
//!
//! ## Go 源根因（`helper.go:97-104`）
//!
//! handle 持全局 mu 同步跑这些命令，任一命令无限挂起（stale NFS 下 lsof、异常 route 等）会持锁不放 →
//! 并发 ping/status/start/stop 全阻塞在 mu.Lock() → 整个 helper 冻死。Go 源用 `CommandContext+超时`
//! 把「无限挂起」降级为「有界失败」：超时即 kill 子进程、返回非 nil error（各调用方沿用原有「忽略 / 上报」
//! 错误路径不变）。
//!
//! ## 移植纪律
//!
//! Go 的三个包装（`execRun`/`execOutput`/`execCombined`）在 Rust 侧抽象为 [`CommandRunner`] trait ——
//! 三个方法对应三个 Go 函数，签名逐一对齐。生产 [`SystemRunner`] 用 `std::process::Command` 实现
//! （超时经子线程 spawn + 主线程 channel recv_timeout，超时则 kill 子进程），不引入 tokio
//! （helper 是同步常驻守护进程，Go 源也是同步 `exec.CommandContext`）；测试注入 mock **完全不触碰宿主**。
//!
//! ## 超时常量（`helper.go:101-104`）
//!
//! | 常量 | Go 值 | 用途 |
//! |------|-------|------|
//! | [`EXEC_TIMEOUT`] | 8s | 常规系统命令（route/lsof/ps/sysctl/dscacheutil/killall/pkill/kill/xattr） |
//! | [`CODESIGN_TIMEOUT`] | 30s | codesign --deep 对大二进制签名较慢，给更长窗口 |

use std::process::Output;
use std::sync::mpsc;
use std::time::Duration;

/// 常规系统命令超时（`helper.go:102`：8s）。
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(8);

/// codesign --deep 超时（`helper.go:103`：30s）。
pub const CODESIGN_TIMEOUT: Duration = Duration::from_secs(30);

/// 系统命令执行抽象（对应 Go `execRun`/`execOutput`/`execCombined` 三个包装）。
///
/// trait 化后生产用 [`SystemRunner`]（真跑命令），测试注入 mock（不触碰宿主）——
/// 满足「系统操作 trait 抽象、测试 mock」的移植纪律。
pub trait CommandRunner: Send + Sync {
    /// 带超时执行并等待完成（等价 Go `execRun`，`helper.go:107-111`）。丢弃输出，仅看成功/失败。
    ///
    /// 超时 → kill 子进程 → 返回 Err。调用方通常忽略错误（Go 源多数 `_ = execRun(...)`）。
    fn run(&self, timeout: Duration, program: &str, args: &[&str]) -> Result<(), RunError>;

    /// 带超时执行并捕获 stdout（等价 Go `execOutput`，`helper.go:114-118`）。
    ///
    /// 超时或非零退出码 → Err。成功返回 stdout 字节。
    fn output(&self, timeout: Duration, program: &str, args: &[&str]) -> Result<Vec<u8>, RunError>;

    /// 带超时执行并捕获合并输出（stdout+stderr，等价 Go `execCombined`，`helper.go:121-125`）。
    ///
    /// 用于 flush-dns 的 dscacheutil/killall —— Go 源要 stderr 文本作诊断 detail（`helper.go:499,503`）。
    fn combined(
        &self,
        timeout: Duration,
        program: &str,
        args: &[&str],
    ) -> Result<Vec<u8>, RunError>;
}

/// 命令执行错误（超时 / 启动失败 / 非零退出码）。
#[derive(Debug)]
pub enum RunError {
    /// 命令不存在 / 无执行权限 / spawn 失败。
    Spawn(String),
    /// 超过 timeout 仍未完成，已 kill。
    Timeout,
    /// 进程退出码非零（携带 stdout/stderr 供诊断，对齐 Go `*exec.ExitError` 携带 Stderr）。
    NonZero {
        /// 退出码（对齐 Go `ExitError.ExitCode()`）。
        code: i32,
        /// 完整输出（stdout + stderr 分离，便于 flush-dns 取 stderr 作 detail）。
        output: Output,
    },
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(msg) => write!(f, "spawn failed: {msg}"),
            Self::Timeout => write!(f, "timeout"),
            // 对齐 Go `%v` 格式化 *exec.ExitError：显示 "exit status N"
            Self::NonZero { code, .. } => write!(f, "exit status {code}"),
        }
    }
}

impl std::error::Error for RunError {}

/// 生产实现：用 `std::process::Command` 真跑命令，超时经子线程 + channel recv_timeout。
///
/// 不引入 tokio —— helper 是同步常驻守护进程。超时实现：spawn 子进程后，在子线程里
/// `wait_with_output()`，主线程用 `recv_timeout(timeout)` 等待；超时则 `kill()` 子进程并回收。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl SystemRunner {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// 内部：spawn + 带超时等待，返回完整 Output（含 status/stdout/stderr）。
    fn run_with_timeout(
        &self,
        timeout: Duration,
        program: &str,
        args: &[&str],
    ) -> Result<Output, RunError> {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = cmd.spawn().map_err(|e| RunError::Spawn(e.to_string()))?;
        let child_id = child.id();

        // 子线程：阻塞 wait_with_output（回收子进程资源 + 捕获管道）
        let (tx, rx) = mpsc::channel::<Result<Output, String>>();
        let join = std::thread::spawn(move || {
            let result = child.wait_with_output().map_err(|e| e.to_string());
            let _ = tx.send(result);
        });

        // 主线程：带超时等待结果
        match rx.recv_timeout(timeout) {
            Ok(Ok(output)) => {
                if output.status.success() {
                    Ok(output)
                } else {
                    Err(RunError::NonZero {
                        code: output.status.code().unwrap_or(-1),
                        output,
                    })
                }
            }
            Ok(Err(e)) => Err(RunError::Spawn(e)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 超时：kill 子进程（对齐 Go CommandContext ctx cancel → SIGKILL）
                // child_id 在此 std 版本为 u32（非 Option）—— child 已被 move 进子线程，
                // 故按 pid 直接发 SIGKILL（不依赖 Child handle，等同 Go 的 cmd.Process.Kill()）。
                kill_process(child_id);
                // 等 join 回收子线程（避免泄漏）；wait_with_output 线程会因 child 被 kill 而返回
                let _ = join.join();
                Err(RunError::Timeout)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // 子线程 panic 后 channel 断开 —— 兜底 kill
                kill_process(child_id);
                let _ = join.join();
                Err(RunError::Spawn("wait thread panicked".into()))
            }
        }
    }
}

impl CommandRunner for SystemRunner {
    fn run(&self, timeout: Duration, program: &str, args: &[&str]) -> Result<(), RunError> {
        let _ = self.run_with_timeout(timeout, program, args)?;
        Ok(())
    }

    fn output(&self, timeout: Duration, program: &str, args: &[&str]) -> Result<Vec<u8>, RunError> {
        let output = self.run_with_timeout(timeout, program, args)?;
        Ok(output.stdout)
    }

    fn combined(
        &self,
        timeout: Duration,
        program: &str,
        args: &[&str],
    ) -> Result<Vec<u8>, RunError> {
        // Go CombinedOutput 把 stdout+stderr 合并到单字节流（按写入顺序交错）。
        // std::process 无法原生合并 —— 近似：返回 stdout + stderr 拼接（顺序可能略有差异，
        // 但 flush-dns 只用其作诊断 detail 文本，等价于 Go `strings.TrimSpace(string(out))`）。
        let output = self.run_with_timeout(timeout, program, args)?;
        let mut combined = output.stdout;
        combined.extend_from_slice(&output.stderr);
        Ok(combined)
    }
}

/// 跨平台 kill 一个进程（对齐 Go `cmd.Process.Kill()` = SIGKILL）。
///
/// **forbid(unsafe_code)** 下走 nix crate（mac）。非 mac 测试环境无生产路径调用此函数。
fn kill_process(pid: u32) {
    let _ = kill_process_inner(pid);
}

#[cfg(target_os = "macos")]
fn kill_process_inner(pid: u32) -> Result<(), nix::Error> {
    use nix::sys::signal::{kill, Signal};
    kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGKILL)
}

#[cfg(not(target_os = "macos"))]
fn kill_process_inner(_pid: u32) -> Result<(), ()> {
    // 非 mac（Linux 测试环境）：helper-mac 在非 mac 上只编译、不跑生产路径。
    // 此处仅作占位 —— SystemRunner 的超时 kill 在 mac 上走 nix 分支生效。
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// MockRunner：记录所有调用 + 按预设规则返回结果。测试完全不触碰宿主命令。
    #[derive(Default)]
    struct MockRunner {
        calls: Mutex<Vec<Call>>,
        output_bytes: Vec<u8>,
        fail_combined: bool,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Call {
        program: String,
        args: Vec<String>,
    }

    impl MockRunner {
        fn calls_snapshot(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, _t: Duration, program: &str, args: &[&str]) -> Result<(), RunError> {
            self.calls.lock().unwrap().push(Call {
                program: program.into(),
                args: args.iter().map(|s| (*s).into()).collect(),
            });
            Ok(())
        }

        fn output(&self, _t: Duration, program: &str, args: &[&str]) -> Result<Vec<u8>, RunError> {
            self.calls.lock().unwrap().push(Call {
                program: program.into(),
                args: args.iter().map(|s| (*s).into()).collect(),
            });
            Ok(self.output_bytes.clone())
        }

        fn combined(
            &self,
            _t: Duration,
            program: &str,
            args: &[&str],
        ) -> Result<Vec<u8>, RunError> {
            self.calls.lock().unwrap().push(Call {
                program: program.into(),
                args: args.iter().map(|s| (*s).into()).collect(),
            });
            if self.fail_combined {
                Err(RunError::NonZero {
                    code: 1,
                    output: std::process::Output {
                        status: make_exit_status(1),
                        stdout: Vec::new(),
                        stderr: b"err".to_vec(),
                    },
                })
            } else {
                Ok(self.output_bytes.clone())
            }
        }
    }

    fn make_exit_status(code: i32) -> std::process::ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code)
        }
        #[cfg(not(unix))]
        {
            let _ = code;
            std::process::ExitStatus::default()
        }
    }

    #[test]
    fn mock_runner_records_calls() {
        let m = MockRunner::default();
        m.run(
            EXEC_TIMEOUT,
            "/sbin/route",
            &["-n", "add", "default", "1.2.3.4"],
        )
        .unwrap();
        let calls = m.calls_snapshot();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "/sbin/route");
        assert_eq!(calls[0].args, vec!["-n", "add", "default", "1.2.3.4"]);
    }

    #[test]
    fn mock_runner_output_returns_preset_bytes() {
        let m = MockRunner {
            output_bytes: b"192.168.1.1".to_vec(),
            ..Default::default()
        };
        let out = m.output(EXEC_TIMEOUT, "/bin/ps", &[]).unwrap();
        assert_eq!(out, b"192.168.1.1");
    }

    #[test]
    fn mock_runner_combined_failure_returns_nonzero() {
        let m = MockRunner {
            fail_combined: true,
            ..Default::default()
        };
        let err = m.combined(EXEC_TIMEOUT, "/usr/bin/dscacheutil", &["-flushcache"]);
        assert!(matches!(err, Err(RunError::NonZero { code: 1, .. })));
    }

    #[test]
    fn timeouts_match_go_source() {
        // helper.go:102-103
        assert_eq!(EXEC_TIMEOUT, Duration::from_secs(8));
        assert_eq!(CODESIGN_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn run_error_display_matches_go_exit_status_format() {
        // Go *exec.ExitError 的 Error() 返回 "exit status N" —— 我们 Display 对齐
        let e = RunError::NonZero {
            code: 1,
            output: std::process::Output {
                status: make_exit_status(1),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        };
        assert_eq!(e.to_string(), "exit status 1");
        assert_eq!(RunError::Timeout.to_string(), "timeout");
        assert_eq!(
            RunError::Spawn("no such file".into()).to_string(),
            "spawn failed: no such file"
        );
    }

    // ===== SystemRunner 真跑命令（按平台选程序；spawn 失败即红，不容忍）=====

    #[test]
    fn system_runner_run_echo() {
        // 跨平台验证 SystemRunner 真能 spawn + wait。
        #[cfg(unix)]
        let (program, args): (&str, &[&str]) = ("/bin/echo", &["hello"]);
        #[cfg(windows)]
        let (program, args): (&str, &[&str]) = ("cmd", &["/c", "echo hello"]);
        let runner = SystemRunner::new();
        runner
            .run(EXEC_TIMEOUT, program, args)
            .expect("echo 须 spawn 成功且零退出");
    }

    #[test]
    fn system_runner_output_echo() {
        #[cfg(unix)]
        let (program, args): (&str, &[&str]) = ("/bin/echo", &["world"]);
        #[cfg(windows)]
        let (program, args): (&str, &[&str]) = ("cmd", &["/c", "echo world"]);
        let runner = SystemRunner::new();
        let out = runner
            .output(EXEC_TIMEOUT, program, args)
            .expect("echo 须 spawn 成功且零退出");
        assert!(String::from_utf8_lossy(&out).contains("world"));
    }

    #[test]
    fn system_runner_nonzero_exit() {
        // unix：/usr/bin/false 退出码 1；windows：cmd /c exit 1。
        #[cfg(unix)]
        let (program, args): (&str, &[&str]) = ("/usr/bin/false", &[]);
        #[cfg(windows)]
        let (program, args): (&str, &[&str]) = ("cmd", &["/c", "exit 1"]);
        let runner = SystemRunner::new();
        let result = runner.run(EXEC_TIMEOUT, program, args);
        match result {
            Err(RunError::NonZero { .. }) => {}
            other => panic!("expected NonZero, got {other:?}"),
        }
    }
}
