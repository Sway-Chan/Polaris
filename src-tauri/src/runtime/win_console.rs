//! Windows 控制台窗口抑制 —— 本 crate 内所有子进程构造的**单一收口**。
//!
//! # 缺陷形状
//!
//! 宿主是 **GUI 子系统**进程（`src-tauri/src/main.rs` 的 `windows_subsystem = "windows"`）⇒ 自身无控制台。
//! 无控制台的父进程创建 **console 子系统**程序时，`CreateProcess` 会为子进程**新分配一个控制台窗口**
//! —— 用户看到黑框。`tasklist` / `taskkill` / `tar` / `sing-box` 全是 console 程序。
//!
//! # 为什么必须逐调用点显式加
//!
//! **std 与 tokio 都没有任何隐含抑制。** `tokio::process::Command::creation_flags` 只是把值透传给
//! `std::os::windows::process::CommandExt::creation_flags`（实测 tokio-1.53.1 `src/process/mod.rs:675-677`），
//! tokio 侧零默认。`core-supervisor/src/spawner.rs` 曾按「tokio 在 Windows 默认不显示控制台窗口」
//! 写过注释 —— **那句是错的**，且因为写成了注释，它让这条缺陷在起核路径上潜伏了整个迁移期。
//!
//! # 严重度不均
//!
//! 最要命的是 [`crate::runtime::proxy::pid_alive`]：挂在崩溃监测上、**1Hz**，而 Windows 的 TUN 常态走
//! helper 腿必经它 ⇒ 不加标志就是代理开着期间**每秒闪一次黑框**。其余（起核 / `check` / 换核解压 /
//! 读版本）是低频，但起核那个窗口在核活着期间**常驻**。
//!
//! # 跨 crate 的另外三份
//!
//! `system-integration`（`exec.rs` 的 `StdCommandRunner`，本 crate 之外唯一的命令缝）、
//! `core-supervisor`（`spawner.rs` 起核 + `config_gate.rs` 的 `check`）、`helper-client`
//! （`manager.rs` 的 `sc`、`privilege.rs` 的提权载体）各自持有等价实现 —— 三者与本 crate 无共同依赖，
//! 且都禁止为一个常量引 `windows-sys`。四份的标志值由
//! `tests/windows_console_suppression.rs` 逐字对齐。

/// `std::process::Command` 挂 `CREATE_NO_WINDOW`（winbase.h `0x0800_0000`）；非 Windows 恒 no-op。
///
/// 返回 `&mut Command` 以便原地包住既有链式构造：
/// `no_console_window(Command::new(x).args(y)).output()`。
pub(crate) fn no_console_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// [`no_console_window`] 的 tokio 版。
///
/// `tokio::process::Command::creation_flags` 是 `#[cfg(windows)]` 的 inherent 方法（无需 import trait），
/// 语义与 std 版完全一致 —— 它就是往 std 里透传。
pub(crate) fn no_console_window_async(
    cmd: &mut tokio::process::Command,
) -> &mut tokio::process::Command {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd
}
