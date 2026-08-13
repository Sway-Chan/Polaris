//! macOS daemon 入口（忠实迁自 上游 Go `helper/helper.go:591-643` 的 `main()`，能力册 M16）。
//!
//! ## Go 源结构（`helper.go:591-643`）
//!
//! ```text
//! func main() {
//!     flag.StringVar(&singboxBin, "singbox", "", ...)          // :592
//!     flag.StringVar(&confDir, "confdir", "", ...)             // :593
//!     flag.StringVar(&supportDir, "support", "/Library/Application Support/上游", ...) // :594
//!     flag.StringVar(&coreDir, "coredir", "", ...)             // :595
//!     flag.Parse()
//!     os.MkdirAll(supportDir, 0o755); os.Chmod(supportDir, 0o755)  // :598-603
//!     sockPath := supportDir/helper.sock; os.Remove(sockPath)      // :604-605
//!     l, _ := net.Listen("unix", sockPath); os.Chmod(sockPath, 0o666) // :606-611
//!     // SIGTERM/SIGINT 收割器（proto v3）：                          // :615-641
//!     signal.Notify(sigCh, SIGTERM, SIGINT)
//!     go func() { <-sigCh; if child!=nil { terminateChild } else { pkill -9 -U 0 -f "<sb> run" }; os.Exit(0) }()
//!     for { conn, _ := l.Accept(); go handle(conn) }              // :636-641
//! }
//! ```
//!
//! ## 落地边界（C6-0 底座 → C6-1 补全）
//!
//! - flag 解析（[`parse_args`]，纯逻辑跨平台可测）、MkdirAll support 0755、socket 0666 建 + accept 循环
//!   （委托 [`super::server::serve`]）、SIGTERM/SIGINT 收割器（[`run`]，mac-gated）**均落地**。
//! - **C6-1**：accept 到的连接经 [`DaemonServices`](super::server::DaemonServices) 真 dispatch（每连接
//!   线程 / 5s read deadline / `command_mu` 单锁纪律）；start 真起锁定核（log 重定向、收割线程、父死看护）；
//!   stop/cleanup 真发信号；watchParent、chownRuntimeDirs、sysctl procStartTime 全接线。
//! - 收割器「有 child → terminateChild」精确分支已补（`DaemonServices::shutdown_reap`）：有 child → 同步
//!   graceful terminate；否则 pkill 兜底（`helper.go:627-631`）。

use crate::platform::macos::handler::MacConfig;

/// macOS Go flag `--support` 默认值（`helper.go:594`，上游→Polaris 品牌改名）。
const DEFAULT_SUPPORT_DIR: &str = "/Library/Application Support/Polaris";

/// 解析 daemon flag → [`MacConfig`]（对照 Go `helper.go:592-596` 的 `flag.StringVar` 四项）。
///
/// 纯逻辑，跨平台可测（本模块以 `cfg(any(target_os="macos", test))` 编入 → Linux CI 覆盖）。
#[must_use]
pub fn parse_args<I: Iterator<Item = String>>(argv: I) -> MacConfig {
    let m = crate::cli::parse_flags(argv, &[]);
    MacConfig::new(
        m.get("singbox").cloned().unwrap_or_default(),
        m.get("confdir").cloned().unwrap_or_default(),
        m.get("support")
            .cloned()
            .unwrap_or_else(|| DEFAULT_SUPPORT_DIR.to_owned()),
        m.get("coredir").cloned().unwrap_or_default(),
    )
}

/// daemon 入口（binary `main()` 经 `cfg(target_os="macos")` 调此）。
///
/// 非 mac 编译（本模块的 `test` 分支在 Linux CI 上）只走占位分支保证类型检查通过；
/// 生产 mac 二进制走 [`run`]。
#[must_use]
pub fn daemon_main<I: Iterator<Item = String>>(argv: I) -> std::process::ExitCode {
    let cfg = parse_args(argv);
    #[cfg(target_os = "macos")]
    {
        run(&cfg)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cfg; // 非 mac：仅编入 test，从不被 binary main 调（其 mac 分支 cfg-out）。
        std::process::ExitCode::SUCCESS
    }
}

/// 真 mac daemon 主体（生产服务 bundle + socket serve + 信号收割器，mac-gated）。
#[cfg(target_os = "macos")]
fn run(cfg: &MacConfig) -> std::process::ExitCode {
    use crate::platform::macos::server::{serve, DaemonServices};

    // 生产服务 bundle（token/runner/config + child 记账 + command_mu）—— accept 循环与信号收割器共享
    // 同一 child 状态（`Arc`），使收割器能在退出前对 child sing-box 走精确 terminateChild 分支。
    let services = DaemonServices::new(cfg.clone());

    // helper.go:615-641：先装 SIGTERM/SIGINT 收割器（阻塞信号 + 起收割线程），再进 accept 循环。
    // 顺序上早于 socket bind 无副作用（信号在 accept 前即被接管；bind 失败则进程随 serve 返回 FAILURE 退出）。
    install_signal_reaper(std::sync::Arc::clone(&services));

    // helper.go:598-611 + :636-642：MkdirAll support 0755 + socket 0666 建 + accept 循环 dispatch
    //（每连接线程 + 5s deadline + command_mu 单锁纪律，C6-1 已接线）。
    match serve(services) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("polaris-helper (macos): serve {}: {e}", cfg.support_dir);
            std::process::ExitCode::FAILURE
        }
    }
}

/// SIGTERM/SIGINT 收割器（`helper.go:615-641`）。
///
/// 阻塞 SIGTERM/SIGINT（后续 spawn 的线程继承此掩码，杜绝默认处置直接杀进程），起一收割线程 `sigwait`。
/// 收到信号后委托 [`DaemonServices::shutdown_reap`]：**有 child → 同步 terminateChild**（TERM→≤5s→KILL，
/// `helper.go:624-625`）**否则 pkill 兜底**（`helper.go:627-631`：`pkill -9 -U 0 -f "<singbox> run"`），
/// 再 `exit(0)`。C6-1 接 production services 后精确分支已补齐（C6-0 曾仅 pkill 兜底）。
///
/// 用 `nix::sys::signal::SigSet::{thread_block, wait}`（sigwait 的 safe wrapper）—— `deny(unsafe_code)`
/// 下不写 unsafe 块（对齐移植纪律：syscall 走 nix crate）。
#[cfg(target_os = "macos")]
fn install_signal_reaper(services: std::sync::Arc<crate::platform::macos::server::DaemonServices>) {
    use nix::sys::signal::{SigSet, Signal};

    let mut set = SigSet::empty();
    set.add(Signal::SIGTERM);
    set.add(Signal::SIGINT);
    // 阻塞信号（当前线程 + 之后 spawn 的线程继承）。失败则放弃收割器（best-effort，对齐 Go 无处理时的旧行为）。
    if set.thread_block().is_err() {
        return;
    }
    std::thread::spawn(move || {
        // helper.go:617-619：<-sigCh 等 SIGTERM/SIGINT。
        let _ = set.wait();
        // helper.go:620-634：摘 child → 有则 graceful terminate 否则 pkill 兜底 → os.Exit(0)。
        services.shutdown_reap();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> std::vec::IntoIter<String> {
        let mut v = vec!["polaris-helper".to_owned()];
        v.extend(args.iter().map(|s| (*s).to_owned()));
        v.into_iter()
    }

    #[test]
    fn parse_args_all_flags() {
        let cfg = parse_args(argv(&[
            "--singbox",
            "/opt/sing-box",
            "--confdir",
            "/etc/polaris/conf",
            "--support",
            "/Library/Application Support/Polaris",
            "--coredir",
            "/usr/local/lib/polaris/core",
        ]));
        assert_eq!(cfg.singbox_bin, "/opt/sing-box");
        assert_eq!(cfg.conf_dir, "/etc/polaris/conf");
        assert_eq!(cfg.support_dir, "/Library/Application Support/Polaris");
        assert_eq!(cfg.core_dir, "/usr/local/lib/polaris/core");
    }

    #[test]
    fn parse_args_support_defaults() {
        // 缺 --support → Go 默认值（品牌改名 上游→Polaris）。
        let cfg = parse_args(argv(&["--singbox", "/opt/sb"]));
        assert_eq!(cfg.support_dir, "/Library/Application Support/Polaris");
        assert_eq!(cfg.singbox_bin, "/opt/sb");
        assert_eq!(cfg.conf_dir, "");
        assert_eq!(cfg.core_dir, "");
    }

    #[test]
    fn parse_args_eq_form() {
        let cfg = parse_args(argv(&["--singbox=/a/b", "--coredir=/c/d"]));
        assert_eq!(cfg.singbox_bin, "/a/b");
        assert_eq!(cfg.core_dir, "/c/d");
    }
}
