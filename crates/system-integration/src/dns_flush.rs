//! OS 级 DNS 缓存刷新命令构造（三平台，best-effort）。
//!
//! 1:1 移植自 上游 `os-dns-flush.ts`。模块只构造命令 + 编排降级；真实 exec 经 trait 注入（不触碰宿主）。
//!
//! 不变量（对齐 Polaris）：永不抛——刷缓存是增益项，绝不阻塞代理生命周期；每个命令 3s 硬超时。

#![forbid(unsafe_code)]

use crate::exec::{Command, CommandRunner};
use polaris_helper_proto::Platform;
use std::time::Duration;

/// 单个外部命令硬超时（上游 `EXEC_TIMEOUT_MS`）。
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(3);

/// 一条刷缓存命令。
///
/// 单一真值在 [`crate::exec::Command`]（此前本模块的 `FlushCommand` 与 `proxy_ops::Command` 是两份
/// 逐字相同的 `{program, args}` —— 假差异，已合并）。别名保留移植血缘可读性。
pub type FlushCommand = Command;

/// 命令执行器（注入便于 mock；真实实现带超时）。
/// 失败返回 Err（调用方降级为告警，不抛）。
pub trait FlushExec {
    fn exec(&self, cmd: &FlushCommand, timeout: Duration) -> Result<(), String>;
}

/// [`FlushExec`] 的生产实现：委托 [`CommandRunner`]（硬超时在其中落实）。
///
/// **不是多余的一层**：`FlushExec` 是 flush 的**语义**缝（契约=「失败返 Err，调用方降级为告警」），
/// `CommandRunner` 是**执行**缝。此 impl 让任意 runner（含生产 `StdCommandRunner`）直接当 flush 执行器用，
/// 同时保留 `FlushExec` 的独立 mock 面。
impl<R: CommandRunner> FlushExec for R {
    fn exec(&self, cmd: &FlushCommand, timeout: Duration) -> Result<(), String> {
        self.run(cmd, timeout).map(|_| ())
    }
}

/// macOS 用户级降级命令：`dscacheutil -flushcache`。
/// 上游 `flushOsDnsCache` darwin 降级腿。
pub fn mac_user_flush_command() -> FlushCommand {
    Command::new("/usr/bin/dscacheutil", ["-flushcache"])
}

/// Windows 命令：`ipconfig /flushdns`。
///
/// 用 System32 绝对路径（上游 `WindowsSystemProxy` 的 `ipconfigExe = system32('ipconfig.exe')` 同因）：
/// 部分设备 PATH 缺 `C:\Windows\System32` → 裸 `ipconfig` 报「不是内部或外部命令」。见 [`crate::exec::system32`]。
pub fn windows_flush_command() -> FlushCommand {
    Command::new(
        crate::exec::system32_from_env("ipconfig.exe"),
        ["/flushdns"],
    )
}

/// Linux 命令：`resolvectl flush-caches`。
///
/// **无回退，与上游一致**（`os-dns-flush.ts:82`）。曾有一份 `helper/platform/linux/ops.rs::TokioDns`
/// 带 `resolvconf -u` 回退与本函数分叉；2026-07-16 调和时判定其**无上游、不可达、语义非刷缓存**并删除
/// （判据见该文件「系统 DNS 刷新」段）。**已知缺口**：非 systemd 且跑 nscd/dnsmasq 的机器不刷 —— 上游同样如此。
pub fn linux_flush_command() -> FlushCommand {
    Command::new("resolvectl", ["flush-caches"])
}

/// helper flush 结果（macOS root helper 通道）。上游 `helperFlushDns` 返回。
#[derive(Debug, Clone, Default)]
pub struct HelperFlushResult {
    pub ok: bool,
    pub partial: Option<String>,
    pub error: Option<String>,
}

/// helper flush 通道（mac root helper；缺省 None = 不可用走用户级降级）。
pub type HelperFlushFn<'a> = Option<&'a dyn Fn() -> HelperFlushResult>;

/// 刷 OS DNS 缓存。best-effort、永不抛（失败仅 on_warn）。
///
/// - mac：helper 可用且 ok → 用 helper；否则降级 `dscacheutil -flushcache`。
/// - win：`ipconfig /flushdns`。
/// - linux：`resolvectl flush-caches`。
/// - 其它：no-op。
///
/// 上游 `flushOsDnsCache`。
pub fn flush_os_dns_cache<E: FlushExec>(
    platform: Platform,
    exec: &E,
    helper_flush: HelperFlushFn,
    on_warn: &mut dyn FnMut(&str),
) {
    match platform {
        Platform::Mac => {
            if let Some(helper) = helper_flush {
                let r = helper();
                if r.ok {
                    if r.partial.is_some() {
                        on_warn(&format!(
                            "已刷新系统 DNS 缓存（helper root，partial：{}）",
                            r.partial.unwrap_or_default()
                        ));
                    }
                    // ok（无论 partial）→ 不降级。
                    return;
                }
                on_warn(&format!(
                    "helper flush-dns 不可用（{}），降级用户级 dscacheutil",
                    r.error.unwrap_or_else(|| "未知".into())
                ));
            }
            // 用户级降级。
            if let Err(e) = exec.exec(&mac_user_flush_command(), EXEC_TIMEOUT) {
                on_warn(&format!("刷新系统 DNS 缓存失败（忽略）: {e}"));
            }
        }
        Platform::Win => {
            if let Err(e) = exec.exec(&windows_flush_command(), EXEC_TIMEOUT) {
                on_warn(&format!("刷新系统 DNS 缓存失败（忽略）: {e}"));
            }
        }
        Platform::Linux => {
            if let Err(e) = exec.exec(&linux_flush_command(), EXEC_TIMEOUT) {
                on_warn(&format!("刷新系统 DNS 缓存失败（忽略）: {e}"));
            }
        }
        Platform::Other => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct MockExec {
        calls: RefCell<Vec<FlushCommand>>,
        fail: bool,
    }
    impl FlushExec for MockExec {
        fn exec(&self, cmd: &FlushCommand, _timeout: Duration) -> Result<(), String> {
            self.calls.borrow_mut().push(cmd.clone());
            if self.fail {
                Err("exec failed".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn mac_user_flush_command_shape() {
        let c = mac_user_flush_command();
        assert_eq!(c.program, "/usr/bin/dscacheutil");
        assert_eq!(c.args, vec!["-flushcache".to_string()]);
    }

    #[test]
    fn windows_flush_command_shape() {
        let c = windows_flush_command();
        // System32 绝对路径而非裸 `ipconfig`：部分设备 PATH 缺 System32 → 裸命令报「不是内部或外部
        // 命令」（上游 `ipconfigExe = system32('ipconfig.exe')` 同因）。本机非 Windows 时 env 无
        // SystemRoot → 回落 C:\Windows，故断言以 System32 路径结尾。
        assert!(
            c.program.ends_with("\\System32\\ipconfig.exe"),
            "须用 System32 绝对路径，实际 {}",
            c.program
        );
        assert_eq!(c.args, vec!["/flushdns".to_string()]);
    }

    #[test]
    fn linux_flush_command_shape() {
        let c = linux_flush_command();
        assert_eq!(c.program, "resolvectl");
        assert_eq!(c.args, vec!["flush-caches".to_string()]);
    }

    #[test]
    fn mac_uses_helper_when_ok() {
        let exec = MockExec::default();
        let mut warned = String::new();
        flush_os_dns_cache(
            Platform::Mac,
            &exec,
            Some(&|| HelperFlushResult {
                ok: true,
                partial: None,
                error: None,
            }),
            &mut |m| warned = m.into(),
        );
        // helper ok → 不走 exec。
        assert!(exec.calls.borrow().is_empty());
        assert!(warned.is_empty());
    }

    #[test]
    fn mac_partial_warns_no_degrade() {
        let exec = MockExec::default();
        let mut warned = String::new();
        flush_os_dns_cache(
            Platform::Mac,
            &exec,
            Some(&|| HelperFlushResult {
                ok: true,
                partial: Some("HUP mDNSResponder failed".into()),
                error: None,
            }),
            &mut |m| warned = m.into(),
        );
        // partial → 不降级（不 exec），仅 warn。
        assert!(exec.calls.borrow().is_empty());
        assert!(warned.contains("partial"));
    }

    #[test]
    fn mac_helper_unavailable_degrades_to_user_level() {
        let exec = MockExec::default();
        let mut warned = String::new();
        flush_os_dns_cache(
            Platform::Mac,
            &exec,
            Some(&|| HelperFlushResult {
                ok: false,
                partial: None,
                error: Some("ERR unknown".into()),
            }),
            &mut |m| warned = m.into(),
        );
        // helper 不可用 → 降级 dscacheutil。
        let calls = exec.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "/usr/bin/dscacheutil");
        assert!(warned.contains("降级"));
    }

    #[test]
    fn mac_no_helper_degrades_directly() {
        let exec = MockExec::default();
        let warned = String::new();
        flush_os_dns_cache(Platform::Mac, &exec, None, &mut |_m| {});
        let calls = exec.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "/usr/bin/dscacheutil");
        assert!(warned.is_empty());
    }

    #[test]
    fn mac_exec_failure_warns_not_throws() {
        let exec = MockExec {
            fail: true,
            ..Default::default()
        };
        let mut warned = String::new();
        flush_os_dns_cache(Platform::Mac, &exec, None, &mut |m| warned = m.into());
        assert!(warned.contains("失败（忽略）"));
    }

    #[test]
    fn windows_runs_ipconfig_flushdns() {
        let exec = MockExec::default();
        flush_os_dns_cache(Platform::Win, &exec, None, &mut |_| {});
        let calls = exec.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].program.ends_with("ipconfig.exe"), "{:?}", calls[0]);
        assert_eq!(calls[0].args, vec!["/flushdns".to_string()]);
    }

    #[test]
    fn linux_runs_resolvectl() {
        let exec = MockExec::default();
        flush_os_dns_cache(Platform::Linux, &exec, None, &mut |_| {});
        let calls = exec.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "resolvectl");
    }

    #[test]
    fn other_platform_noop() {
        let exec = MockExec::default();
        flush_os_dns_cache(Platform::Other, &exec, None, &mut |_| {});
        assert!(exec.calls.borrow().is_empty());
    }

    #[test]
    fn current_platform_matches_target() {
        // current() 由编译 target 决定，按 target 断言（对齐 helper-proto
        // platform_current_matches_compile_target），三平台 CI 均成立。
        let cur = Platform::current();
        if cfg!(target_os = "macos") {
            assert_eq!(cur, Platform::Mac);
        } else if cfg!(target_os = "windows") {
            assert_eq!(cur, Platform::Win);
        } else if cfg!(target_os = "linux") {
            assert_eq!(cur, Platform::Linux);
        } else {
            assert_eq!(cur, Platform::Other);
        }
    }
}
