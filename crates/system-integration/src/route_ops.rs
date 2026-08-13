//! 系统路由出口接口探测 + TUN 出口夺取（post-flight）判定。
//!
//! 解决的问题（真机复现 `~/docs/polaris/design/polaris-tun-conflict-detect-design-2026-07-22.md`）：
//! 其他 VPN（如另一个 TUN 客户端）已占用系统默认路由时，Polaris 起核后 sing-box 的 utun 抢不到流量，
//! 但既有就绪门（进程活 + 管理 API 环回口可连）**照样全真** → 假报「已连接」、连接数恒 0、↓0↑0。
//!
//! ## 关键不变式（设计 §4.1 / §4.5）
//!
//! **成功接管 ⟺ 对「本应走代理的公网目的」的出口接口从 baseline 切走。** 直接观测这条比数流量更可靠：
//! 路由表在 utun 起来时即被写（与有无流量无关），故不受「用户恰好空闲、连接数 0」的假阳性影响。
//! macOS 内核给 tun 分配 `utunN` 名字、进程**无法预知自己的 utun 叫什么**（config-engine 不设
//! `interface_name`），故不靠名字识别「我方 utun」，而靠 **baseline 差分**：起核前记出口，起核 +grace
//! 后仍等于 baseline 那个「起核前就存在的接口」→ 判未夺到路由。
//!
//! ## 手段（设计 §4.5 / §4.6，D4/D5）
//!
//! probe 目的固定为公网 IP `1.1.1.1`（[`PROBE_IP`]）——`route get` / `ip route get` 是**路由表读**，
//! **不发包**、非 root、非破坏。命令经 [`CommandRunner`] 下发（mock 可单测），与 `dns_ops` 同缝。
//!
//! - **macOS**：`route -n get 1.1.1.1` → 解析 `interface: utunN`。**绝不用 `route -n get default`**：
//!   sing-box `auto_route` 装的是 `0.0.0.0/1`+`128.0.0.0/1` 两条半程路由、**不动** `0.0.0.0/0`，查 default
//!   会返物理网卡（假阴性）。查具体公网 IP 让 /1 半程按最长前缀命中（设计 §4.5 陷阱行）。
//! - **Linux**：`ip route get 1.1.1.1` → 解析 `dev tunX`。
//! - **Windows**：`Find-NetRoute -RemoteIPAddress 1.1.1.1` → 解析 `InterfaceAlias`（内核代算最优路由，
//!   等价 `route get`；设计 §4.6 「route print 最低 metric」的可解析替身）。首版命令式，PF_ROUTE 编程读延后。
//!
//! ## 零 cfg（与 `dns_ops` / `proxy_ops` 同纪律）
//!
//! 平台差异只是「跑什么命令 / 怎么解析输出」的纯函数，用运行时 [`Platform`] 分派而非 `#[cfg(target_os)]`
//! → Linux CI 100% 编译 + 跑测三平台逻辑。命令构造与输出解析全是纯函数、mock 一注入即可断言。

#![forbid(unsafe_code)]

use crate::error::SystemIntegrationError;
use crate::exec::{Command, CommandRunner};
use polaris_helper_proto::Platform;
use std::time::Duration;

/// post-flight 探测的固定公网目的 IP（设计 D4）。route 查询是路由表读、不发包 → 选公网 IP 让
/// sing-box auto_route 的 /1 半程路由按最长前缀命中，避开 `default`(0/0) 的物理网卡假阴性。
pub const PROBE_IP: &str = "1.1.1.1";

/// 单条路由查询命令硬超时（对齐 `dns_ops::DNS_CMD_TIMEOUT`）。
pub const ROUTE_CMD_TIMEOUT: Duration = Duration::from_secs(5);

/// 平台路由出口探测抽象。mac/linux/win 真实现（命令 + 解析），Other 收敛为「查不到」（`Ok(None)`）。
pub trait SystemRouteOps {
    /// 查目的 IP 的出口接口名。
    ///
    /// - `Ok(Some(iface))`：路由表命中，出口接口 = `iface`（macOS `utunN` / linux `tunX`/`eth0` /
    ///   win `InterfaceAlias`）。
    /// - `Ok(None)`：命令成功但输出里没有可识别的接口（无匹配路由 / 解析不出）——**非错误**，判定层按
    ///   「不可断言」处理，绝不据此硬闸（避免 §4.7 假阳性）。
    /// - `Err`：命令执行失败（spawn/超时/非零退出）。
    fn exit_interface_for(&self, ip: &str) -> Result<Option<String>, SystemIntegrationError>;
}

/// [`SystemRouteOps`] 的生产实现（运行时 [`Platform`] 分派 + [`CommandRunner`] 下发；零 cfg）。
pub struct SystemRouteOpsImpl<R: CommandRunner> {
    runner: R,
    platform: Platform,
    /// Windows `powershell.exe` 绝对路径（规避 PATH 缺 System32；见 `exec::system32`）。
    powershell_exe: String,
}

impl<R: CommandRunner> SystemRouteOpsImpl<R> {
    /// 生产构造：平台取本机。
    pub fn new(runner: R) -> Self {
        Self::with_platform(runner, Platform::current())
    }

    /// 指定平台构造（测试用：Linux 上断言 mac/win 的 argv 与解析）。
    pub fn with_platform(runner: R, platform: Platform) -> Self {
        Self {
            runner,
            platform,
            powershell_exe: crate::exec::system32_from_env(
                "WindowsPowerShell\\v1.0\\powershell.exe",
            ),
        }
    }

    fn run(&self, cmd: &Command) -> Result<crate::exec::CommandOutput, SystemIntegrationError> {
        self.runner
            .run(cmd, ROUTE_CMD_TIMEOUT)
            .map_err(SystemIntegrationError::route)
    }
}

impl<R: CommandRunner> SystemRouteOps for SystemRouteOpsImpl<R> {
    fn exit_interface_for(&self, ip: &str) -> Result<Option<String>, SystemIntegrationError> {
        match self.platform {
            Platform::Mac => {
                // `-n` 抑制反向 DNS（不发包、更快）；查具体公网 IP 而非 default（§4.5 半程路由陷阱）。
                let out = self.run(&Command::new("route", ["-n", "get", ip]))?;
                Ok(parse_mac_route_get_interface(&out.stdout))
            }
            Platform::Linux => {
                let out = self.run(&Command::new("ip", ["route", "get", ip]))?;
                Ok(parse_linux_ip_route_dev(&out.stdout))
            }
            Platform::Win => {
                // Find-NetRoute 让内核代算「到该目的会用哪个接口」（等价 route get），Format-List 收窄输出。
                let cmd = Command::new(
                    &self.powershell_exe,
                    [
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        &format!(
                            "Find-NetRoute -RemoteIPAddress {ip} | Format-List InterfaceAlias"
                        ),
                    ],
                );
                Ok(parse_win_find_netroute_alias(&self.run(&cmd)?.stdout))
            }
            // 未知平台：无对应路由工具 → 查不到（判定层按「不可断言」不闸）。
            Platform::Other => Ok(None),
        }
    }
}

// ── 纯解析函数（跨平台可单测；Linux CI 上断言三平台输出解析）──

/// 解析 macOS `route -n get <ip>` 的 `interface: utunN` 行。无该行 → None。
pub fn parse_mac_route_get_interface(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix("interface:")?;
        let iface = rest.trim();
        (!iface.is_empty()).then(|| iface.to_string())
    })
}

/// 解析 Linux `ip route get <ip>` 的 `dev <iface>` token。无该 token → None。
///
/// 兼容两形态：`1.1.1.1 via <gw> dev eth0 src ...` 与 `1.1.1.1 dev tun0 src ...`。
pub fn parse_linux_ip_route_dev(stdout: &str) -> Option<String> {
    let mut tokens = stdout.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok == "dev" {
            if let Some(dev) = tokens.next() {
                if !dev.is_empty() {
                    return Some(dev.to_string());
                }
            }
        }
    }
    None
}

/// 解析 Windows `Find-NetRoute ... | Format-List InterfaceAlias` 的首个 `InterfaceAlias : <name>` 值。
///
/// Find-NetRoute 返回源地址 + 路由两个对象、二者的 InterfaceAlias 均指向出口接口，取首个即可。
pub fn parse_win_find_netroute_alias(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim() != "InterfaceAlias" {
            return None;
        }
        let alias = value.trim();
        (!alias.is_empty()).then(|| alias.to_string())
    })
}

// ── baseline 差分判定（方向①后验的核心；纯逻辑，与探测手段解耦）──

/// 出口接口是否已从 baseline 切走（成功接管的判据）。
///
/// - `current = Some(c)` 且 `c != baseline` → `true`（含 baseline 不可读的情形：任何可读的新出口都算切走，
///   偏向「不闸」这一安全方向——见 [`ExitCaptureOutcome`]）。
/// - `current = Some(c)` 且 `c == baseline` → `false`（出口未动）。
/// - `current = None`（探测不可读） → `false`（无法断言切走）。
fn exit_changed(baseline: &Option<String>, current: &Option<String>) -> bool {
    match current {
        Some(c) => baseline.as_deref() != Some(c.as_str()),
        None => false,
    }
}

/// post-flight 出口归属判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitCaptureOutcome {
    /// 出口已从 baseline 切走 → 成功接管（macOS 不可命名 → 差分即判据）。
    Captured { interface: Option<String> },
    /// grace 耗尽、出口仍等于（可读的）baseline → **未夺到路由**，判定层据此硬闸。
    NotCaptured { baseline: String, last: String },
    /// baseline 或末次探测不可读 → 无法断言 → **不闸**（caveat，设计 §4.7 避免假阳性）。
    Indeterminate,
}

/// 在 grace 窗口内轮询出口接口，按 baseline 差分判定是否夺到路由（方向①后验）。
///
/// - `baseline`：起核**前**（我方 utun 上线前）的出口接口快照。
/// - `max_polls`：grace 窗口内的探测次数（≥1）。任一次探到出口切走即**立即** `Captured`（早退，接管越快
///   越早点亮）；仅全部轮次都未切走才落终判。
/// - `probe`：读当前出口接口（生产 = [`SystemRouteOps::exit_interface_for`]；测试 = 队列）。探测 `Err`
///   按 `None`（best-effort，绝不据探测失败误判为夺到）。
/// - `sleep_between_polls`：相邻两次探测之间的等待（生产 = `thread::sleep`；测试 = 计数）。**末次探测后
///   不再 sleep**（省一个 grace 间隔的白等）。
///
/// 终判：全程未切走时，若 baseline 与末次探测**都可读且相等** → [`ExitCaptureOutcome::NotCaptured`]；
/// 否则 [`ExitCaptureOutcome::Indeterminate`]（不可读 → 不闸）。
pub fn verify_exit_captured<P, S>(
    baseline: Option<String>,
    max_polls: usize,
    mut probe: P,
    mut sleep_between_polls: S,
) -> ExitCaptureOutcome
where
    P: FnMut() -> Result<Option<String>, SystemIntegrationError>,
    S: FnMut(),
{
    let polls = max_polls.max(1);
    let mut last: Option<String> = None;
    for i in 0..polls {
        let current = probe().unwrap_or(None);
        if exit_changed(&baseline, &current) {
            return ExitCaptureOutcome::Captured { interface: current };
        }
        last = current;
        if i + 1 < polls {
            sleep_between_polls();
        }
    }
    match (baseline, last) {
        // 都可读且相等（出口自始至终没动）→ 未夺到路由，硬闸。
        (Some(b), Some(l)) if b == l => ExitCaptureOutcome::NotCaptured {
            baseline: b,
            last: l,
        },
        // baseline 不可读 / 末次探测不可读 / 二者不等但又没触发早退 → 不可断言，不闸。
        _ => ExitCaptureOutcome::Indeterminate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::exec_tests_helpers::MockRunner;
    use std::cell::Cell;

    // ══════════ 纯解析（真实样例 fixture）══════════

    #[test]
    fn parse_mac_route_get_extracts_utun() {
        // `route -n get 1.1.1.1` 在 sing-box auto_route 已装 /1 半程路由时的真实态：出口 = utunN。
        let sample = "\
   route to: 1.1.1.1
destination: 0.0.0.0
       mask: 128.0.0.0
    gateway: 10.255.0.1
  interface: utun4
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING>
 recvpipe  sendpipe  ssthresh  rtt,msec    rttvar  hopcount      mtu     expire
       0         0         0         0         0         0      1500         0
";
        assert_eq!(
            parse_mac_route_get_interface(sample),
            Some("utun4".to_string())
        );
    }

    #[test]
    fn parse_mac_route_get_physical_iface() {
        // 无 VPN 时出口 = 物理网卡（en0）——差分 baseline 就是这种值。
        let sample = "   route to: 1.1.1.1\n  interface: en0\n      flags: <UP,GATEWAY>\n";
        assert_eq!(
            parse_mac_route_get_interface(sample),
            Some("en0".to_string())
        );
    }

    #[test]
    fn parse_mac_route_get_none_when_no_interface_line() {
        assert_eq!(
            parse_mac_route_get_interface("   route to: 1.1.1.1\n"),
            None
        );
        assert_eq!(parse_mac_route_get_interface(""), None);
    }

    #[test]
    fn parse_linux_ip_route_get_dev_via_gateway() {
        // 物理出口（经网关）：`... via <gw> dev eth0 src ...`。
        let sample = "1.1.1.1 via 192.168.1.1 dev eth0 src 192.168.1.100 uid 1000 \n    cache \n";
        assert_eq!(parse_linux_ip_route_dev(sample), Some("eth0".to_string()));
    }

    #[test]
    fn parse_linux_ip_route_get_dev_tun() {
        // tun 出口（无网关直连 dev）：`1.1.1.1 dev tun0 src ...`。
        let sample = "1.1.1.1 dev tun0 table 2022 src 172.19.0.2 uid 1000 \n    cache \n";
        assert_eq!(parse_linux_ip_route_dev(sample), Some("tun0".to_string()));
    }

    #[test]
    fn parse_linux_ip_route_none_when_no_dev() {
        assert_eq!(
            parse_linux_ip_route_dev("RTNETLINK answers: Network is unreachable\n"),
            None
        );
        assert_eq!(parse_linux_ip_route_dev(""), None);
    }

    #[test]
    fn parse_win_find_netroute_extracts_first_alias() {
        // Find-NetRoute | Format-List InterfaceAlias：源地址对象 + 路由对象各一行，取首个。
        let sample = "\n\nInterfaceAlias : polaris-tun0\n\nInterfaceAlias : polaris-tun0\n\n";
        assert_eq!(
            parse_win_find_netroute_alias(sample),
            Some("polaris-tun0".to_string())
        );
    }

    #[test]
    fn parse_win_find_netroute_physical_alias() {
        let sample = "InterfaceAlias : Ethernet\nInterfaceAlias : Ethernet\n";
        assert_eq!(
            parse_win_find_netroute_alias(sample),
            Some("Ethernet".to_string())
        );
    }

    #[test]
    fn parse_win_find_netroute_none_when_absent() {
        assert_eq!(parse_win_find_netroute_alias("InterfaceIndex : 12\n"), None);
        assert_eq!(parse_win_find_netroute_alias(""), None);
    }

    // ══════════ 生产实现分派（MockRunner 断言 argv）══════════

    fn route_ops_for(platform: Platform, runner: MockRunner) -> SystemRouteOpsImpl<MockRunner> {
        SystemRouteOpsImpl::with_platform(runner, platform)
    }

    #[test]
    fn impl_mac_uses_route_n_get_specific_ip_not_default() {
        let runner = MockRunner::default().with_arg_stdout("get", "  interface: utun7\n");
        let ops = route_ops_for(Platform::Mac, runner);
        assert_eq!(
            ops.exit_interface_for(PROBE_IP).unwrap(),
            Some("utun7".to_string())
        );
        let cmd = &ops.runner.snapshot()[0];
        assert_eq!(cmd.program, "route");
        // **绝不查 default**：必须是具体公网 IP（§4.5 半程路由陷阱）。
        assert_eq!(cmd.args, vec!["-n", "get", "1.1.1.1"]);
        assert!(!ops.runner.ran_arg("default"), "禁止 route -n get default");
    }

    #[test]
    fn impl_linux_uses_ip_route_get() {
        let runner =
            MockRunner::default().with_arg_stdout("route", "1.1.1.1 dev tun0 src 10.0.0.2\n");
        let ops = route_ops_for(Platform::Linux, runner);
        assert_eq!(
            ops.exit_interface_for(PROBE_IP).unwrap(),
            Some("tun0".to_string())
        );
        let cmd = &ops.runner.snapshot()[0];
        assert_eq!(cmd.program, "ip");
        assert_eq!(cmd.args, vec!["route", "get", "1.1.1.1"]);
    }

    #[test]
    fn impl_win_uses_find_netroute() {
        let runner = MockRunner::default()
            .with_arg_stdout("Find-NetRoute", "InterfaceAlias : polaris-tun0\n");
        let ops = route_ops_for(Platform::Win, runner);
        assert_eq!(
            ops.exit_interface_for(PROBE_IP).unwrap(),
            Some("polaris-tun0".to_string())
        );
        let cmd = &ops.runner.snapshot()[0];
        assert!(cmd.program.ends_with("powershell.exe"));
        assert!(cmd
            .args
            .iter()
            .any(|a| a.contains("Find-NetRoute -RemoteIPAddress 1.1.1.1")));
    }

    #[test]
    fn impl_other_platform_returns_none_no_command() {
        let ops = route_ops_for(Platform::Other, MockRunner::default());
        assert_eq!(ops.exit_interface_for(PROBE_IP).unwrap(), None);
        assert!(ops.runner.snapshot().is_empty(), "未知平台不得跑任何命令");
    }

    #[test]
    fn impl_command_failure_propagates_err() {
        let runner = MockRunner {
            fail_programs: vec!["route".into()],
            ..Default::default()
        };
        assert!(route_ops_for(Platform::Mac, runner)
            .exit_interface_for(PROBE_IP)
            .is_err());
    }

    // ══════════ baseline 差分判定（verify_exit_captured）══════════

    /// 队列式 mock 探针：按序返回预置结果；耗尽后返回末个（模拟稳定态）。
    fn queued_probe(
        seq: Vec<Option<String>>,
    ) -> impl FnMut() -> Result<Option<String>, SystemIntegrationError> {
        let mut it = seq.into_iter();
        let mut last: Option<String> = None;
        move || {
            if let Some(next) = it.next() {
                last = next.clone();
                Ok(next)
            } else {
                Ok(last.clone())
            }
        }
    }

    fn s(x: &str) -> Option<String> {
        Some(x.to_string())
    }

    #[test]
    fn verify_captured_when_iface_changes_from_baseline() {
        // baseline=utun3（他方 VPN），起核后切到 utun7（我方）→ 夺到路由 → Captured（且早退）。
        let sleeps = Cell::new(0);
        let outcome = verify_exit_captured(
            s("utun3"),
            8,
            queued_probe(vec![s("utun3"), s("utun7"), s("utun7")]),
            || sleeps.set(sleeps.get() + 1),
        );
        assert_eq!(
            outcome,
            ExitCaptureOutcome::Captured {
                interface: s("utun7")
            }
        );
        // 第 2 次探测即切走 → 只 sleep 了 1 次（第 1 次探测后），随即早退。
        assert_eq!(sleeps.get(), 1, "夺到即早退，不空等剩余 grace");
    }

    #[test]
    fn verify_not_captured_when_iface_stays_baseline_through_grace() {
        // baseline=utun3，grace 全程仍是 utun3（我方 utun 抢不到路由）→ NotCaptured（硬闸）。
        let sleeps = Cell::new(0);
        let outcome = verify_exit_captured(
            s("utun3"),
            4,
            queued_probe(vec![s("utun3")]), // 耗尽后恒返 utun3
            || sleeps.set(sleeps.get() + 1),
        );
        assert_eq!(
            outcome,
            ExitCaptureOutcome::NotCaptured {
                baseline: "utun3".into(),
                last: "utun3".into()
            }
        );
        // grace 超时：4 次探测之间 sleep 3 次（末次探测后不 sleep）。
        assert_eq!(sleeps.get(), 3);
    }

    #[test]
    fn verify_not_captured_for_own_route_install_failure() {
        // 无他方 VPN（baseline=en0 物理网卡），TUN 模式起核后出口仍 en0（我方路由装失败）→ 同样 NotCaptured。
        // 后验断言的是「出口切没切」而非「因谁没切」→ 我方装失败也一网打尽（设计 §4.2）。
        let outcome = verify_exit_captured(s("en0"), 3, queued_probe(vec![s("en0")]), || {});
        assert_eq!(
            outcome,
            ExitCaptureOutcome::NotCaptured {
                baseline: "en0".into(),
                last: "en0".into()
            }
        );
    }

    #[test]
    fn verify_indeterminate_when_baseline_unreadable() {
        // 起核前 baseline 读不到 → 无法差分 → 不闸（避免假阳性拦掉正常起核，§4.7）。
        let outcome = verify_exit_captured(None, 3, queued_probe(vec![s("utun7")]), || {});
        // baseline=None 时任何可读新出口都算切走（偏向不闸的安全方向）。
        assert_eq!(
            outcome,
            ExitCaptureOutcome::Captured {
                interface: s("utun7")
            }
        );
    }

    #[test]
    fn verify_indeterminate_when_probe_unreadable_through_grace() {
        // baseline 可读但 grace 内探测恒不可读（命令一直失败/无接口）→ 不可断言 → 不闸。
        let outcome = verify_exit_captured(
            s("utun3"),
            3,
            || Err(SystemIntegrationError::route("boom")),
            || {},
        );
        assert_eq!(outcome, ExitCaptureOutcome::Indeterminate);
    }

    #[test]
    fn verify_single_poll_grace_still_evaluates() {
        // max_polls=0 被夹到 1：至少探一次，不 sleep。
        let sleeps = Cell::new(0);
        let outcome = verify_exit_captured(s("utun3"), 0, queued_probe(vec![s("utun3")]), || {
            sleeps.set(sleeps.get() + 1)
        });
        assert_eq!(
            outcome,
            ExitCaptureOutcome::NotCaptured {
                baseline: "utun3".into(),
                last: "utun3".into()
            }
        );
        assert_eq!(sleeps.get(), 0, "单次探测无相邻间隔可等");
    }
}
