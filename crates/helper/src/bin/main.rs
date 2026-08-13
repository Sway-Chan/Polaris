//! Polaris 特权 helper daemon —— 二进制入口（C6-0 底座）。
//!
//! 单 crate + `cfg(target_os)` 出三平台 daemon 二进制（组织原则见 [`polaris_helper::platform`] 模块文档）：
//! 一次交叉编译一个 target，`main()` 只编到该平台那一支，其余平台分支 `cfg`-out。产物即该平台的
//! root LaunchDaemon(mac) / systemd(linux) / SCM 服务(win) 可执行体。
//!
//! 各平台 `daemon_main` 忠实迁自 上游 Go helper 的 `main()`：
//! - macOS：`helper/helper.go:591-643`（M16：flag + MkdirAll support 0755 + socket 0666 + SIGTERM/SIGINT 收割器）。
//! - Linux：`helper-linux/main.go`（L15：flag + prepare_socket + SIGTERM 收割器 + waitReaps + 退出兜底 setForward(false)）。
//! - Windows：`helper-win/main.go`（W19：flag + MkdirAll support + `--console` 前台 or SCM 服务分支）。
//!
//! **C6-0 边界**：本批只接「flag 解析 + main 骨架 + serve 入口调用」，进程能起、能收信号退出。
//! 连接分派（accept 循环 → `process_connection`/`handle`）、提权拉核（mac spawn / linux AmbientCaps /
//! win 已实现）属 C6-1/2/3。

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "macos")]
    {
        polaris_helper::platform::macos::daemon_main(std::env::args())
    }
    #[cfg(target_os = "linux")]
    {
        polaris_helper::platform::linux::daemon_main(std::env::args())
    }
    #[cfg(target_os = "windows")]
    {
        polaris_helper::platform::windows::daemon_main(std::env::args())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        eprintln!("polaris-helper: unsupported target OS");
        std::process::ExitCode::FAILURE
    }
}
