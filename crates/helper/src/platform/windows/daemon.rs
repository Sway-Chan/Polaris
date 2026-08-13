//! Windows daemon 入口（忠实迁自 上游 Go `helper-win/main.go` 的 `main()`，能力册 W19）。
//!
//! ## Go 源结构（`helper-win/main.go`）
//!
//! ```text
//! func main() {
//!     flag.StringVar(&singboxBin, "singbox", "", ...)
//!     flag.StringVar(&confDir, "confdir", "", ...)
//!     flag.StringVar(&supportDir, "support", `C:\ProgramData\上游`, ...)
//!     flag.StringVar(&coreDir, "coredir", "", "accepted and ignored on Windows")  // 接受并忽略
//!     flag.BoolVar(&console, "console", false, ...)
//!     flag.Parse()
//!     os.MkdirAll(supportDir, 0o755)                 // helper.token 落此目录（ACL 由安装期设定）
//!     if console { runConsole(); return }            // 前台 dev/test（Ctrl+C 收割 child）
//!     if err := runService(serviceName); err != nil { ...; os.Exit(1) }  // SCM 服务
//! }
//! ```
//!
//! ## C6-0 落地边界
//!
//! - flag 解析（[`parse_args`]，纯逻辑跨平台可测）、MkdirAll support、`--console`/SCM 分支（[`run`]，
//!   win-gated）**均落地**，直接接现有 [`run_console`](super::service::run_console) /
//!   [`run_service`](super::service::run_service)（二者已建 production `WinHelper` + serve + Ctrl+C/SCM 收割）。
//! - Windows daemon 侧本就最全（能力册 W2..W18 多为真实现）；本批只补「缺 main 入口」这一环（W19 🟡）。
//! - `--coredir` 接受并忽略（镜像 macOS flag 形态，Windows 无 install-core）。

/// Windows daemon flag 解析结果（纯数据，跨平台可测；不引 `windows-sys`）。
///
/// 独立于 [`ServiceConfig`](super::service::ServiceConfig)（后者 `cfg(windows)`，含 FFI 侧类型），
/// 便于在 Linux CI 上单测 flag 解析（本模块以 `cfg(any(target_os="windows", test))` 编入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinArgs {
    /// 锁定的 sing-box 二进制路径（`--singbox`）。
    pub singbox_bin: String,
    /// 允许的配置文件目录（`--confdir`）。
    pub conf_dir: String,
    /// support 目录（`--support`，helper.token 落此）。
    pub support_dir: String,
    /// core 目录（`--coredir`，接受并忽略）。
    pub core_dir: String,
    /// 前台 console 模式（`--console`；否则 SCM 服务）。
    pub console: bool,
}

/// 解析 daemon flag → [`WinArgs`]（对照 Go `main.go` 的 `flag.StringVar`/`flag.BoolVar` 五项）。
#[must_use]
pub fn parse_args<I: Iterator<Item = String>>(argv: I) -> WinArgs {
    let m = crate::cli::parse_flags(argv, &["console"]);
    WinArgs {
        singbox_bin: m.get("singbox").cloned().unwrap_or_default(),
        conf_dir: m.get("confdir").cloned().unwrap_or_default(),
        support_dir: m
            .get("support")
            .cloned()
            .unwrap_or_else(|| super::DEFAULT_SUPPORT_DIR.to_owned()),
        core_dir: m.get("coredir").cloned().unwrap_or_default(),
        console: m.contains_key("console"),
    }
}

/// daemon 入口（binary `main()` 经 `cfg(target_os="windows")` 调此）。
///
/// 非 windows 编译（本模块的 `test` 分支在 Linux CI 上）只走占位分支保证类型检查通过；
/// 生产 Windows 二进制走 [`run`]。
#[must_use]
pub fn daemon_main<I: Iterator<Item = String>>(argv: I) -> std::process::ExitCode {
    let args = parse_args(argv);
    #[cfg(target_os = "windows")]
    {
        run(args)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = args; // 非 windows：仅编入 test，从不被 binary main 调（其 win 分支 cfg-out）。
        std::process::ExitCode::SUCCESS
    }
}

/// 真 Windows daemon 主体（MkdirAll support + console/SCM 分支，win-gated）。
#[cfg(target_os = "windows")]
fn run(args: WinArgs) -> std::process::ExitCode {
    use super::service::{run_console, run_service, ServiceConfig};

    // main.go: MkdirAll(supportDir, 0755)（helper.token 落此；ACL 由安装期设定，此处仅建目录）。
    if let Err(e) = std::fs::create_dir_all(&args.support_dir) {
        eprintln!(
            "polaris-helper (windows): mkdir support {}: {e}",
            args.support_dir
        );
        return std::process::ExitCode::FAILURE;
    }

    let cfg = ServiceConfig {
        singbox_bin: args.singbox_bin,
        conf_dir: args.conf_dir,
        support_dir: args.support_dir,
        core_dir: args.core_dir,
    };

    // main.go: console → 前台 runConsole；否则 SCM runService。
    let result = if args.console {
        run_console(cfg)
    } else {
        run_service(cfg)
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // main.go: `service run failed: %v (use --console to run in foreground)`。
            eprintln!("polaris-helper (windows): service run failed: {e} (use --console to run in foreground)");
            std::process::ExitCode::FAILURE
        }
    }
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
        let a = parse_args(argv(&[
            "--singbox",
            r"C:\sb.exe",
            "--confdir",
            r"C:\conf",
            "--support",
            r"C:\ProgramData\Polaris",
            "--coredir",
            r"C:\core",
            "--console",
        ]));
        assert_eq!(a.singbox_bin, r"C:\sb.exe");
        assert_eq!(a.conf_dir, r"C:\conf");
        assert_eq!(a.support_dir, r"C:\ProgramData\Polaris");
        assert_eq!(a.core_dir, r"C:\core");
        assert!(a.console);
    }

    #[test]
    fn parse_args_support_defaults_and_service_mode() {
        // 缺 --support → Go 默认（品牌改名 上游→Polaris）；缺 --console → SCM 服务模式。
        let a = parse_args(argv(&["--singbox", r"C:\sb.exe"]));
        assert_eq!(a.support_dir, super::super::DEFAULT_SUPPORT_DIR);
        assert!(!a.console);
        assert_eq!(a.conf_dir, "");
        assert_eq!(a.core_dir, "");
    }

    #[test]
    fn parse_args_console_bool_does_not_consume() {
        let a = parse_args(argv(&["--console", "--singbox", r"C:\sb.exe"]));
        assert!(a.console);
        assert_eq!(a.singbox_bin, r"C:\sb.exe");
    }
}
