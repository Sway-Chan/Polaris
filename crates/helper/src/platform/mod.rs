//! 平台差异隔离层 —— **只放真差异**，共用逻辑在 crate 顶层（[`crate::core_install`] /
//! [`crate::line_io`] / [`crate::token`]）。
//!
//! ## 已确认的真平台差异（本层存在的全部理由）
//!
//! | 差异 | macOS | Windows | Linux |
//! |---|---|---|---|
//! | 服务管理 | launchd（plist + `launchctl bootstrap`） | SCM 服务（`sc.exe`） | systemd unit |
//! | IPC 端点 | 0666 unix socket | 命名管道（SDDL） | 0666 unix socket |
//! | 鉴权边界 | token 行 | token 行 | **SO_PEERCRED**（内核背书）+ uid 允许列表 |
//! | 帧读取 | 行读（泛型 `std::io`，走 [`crate::line_io`]） | **裸 Win32 `HANDLE` 整帧读** | 行读（走 [`crate::line_io`]） |
//! | freeport 持有者定位 | `lsof` + `ps` + kill 子进程 | `GetExtendedTcpTable` | `ss` 正则 + `/proc` + `kill(2)` |
//! | install-core | 有（+ xattr/codesign hook） | 无（NSIS 安装器负责） | 有（+ 保守 `lib*.so` prune） |
//!
//! ## 门控矩阵（**在模块声明处**，非文件内逐 item）
//!
//! ```text
//! 目标                          macos   windows   linux
//! cargo build --target linux      ✗        ✗        ✓
//! cargo build --target darwin     ✓        ✗        ✗
//! cargo build --target msvc       ✗        ✓        ✗
//! cargo test（本机 Linux 开发）    ✓        ✓        ✓
//! ```
//!
//! **`cargo build --target X` 编译单元里零非本平台代码** —— 平台专用依赖（`nix` / `windows-sys` /
//! `tokio`）在 `Cargo.toml` 里同样是 target-specific，故也零非本平台依赖（`cargo tree` 实证）。
//!
//! ### 为何 macos/windows 的谓词带 `test`
//!
//! mac/win 平台模块里**绝大部分是纯逻辑**（协议分派、argv 构造、token 鉴权、cmd 行拼装、白名单判定），
//! 刻意写成不碰 OS API（系统调用经 trait 抽象），因此在任何 host 上都能编、能测 —— 这是**移植期的
//! 既有设计**（合并前 `helper-win` crate 文档 §5 明写「本机 Linux 可编译/可测」），不是本次新增。
//!
//! 真正的 OS 调用（`nix` syscall / Windows FFI）在**下一层**门控（[`macos::launchd`] 的 install/uninstall、
//! [`windows::winproc`] / [`windows::service`] 整模块 `cfg(windows)`），所以带 `test` 编 mac/win 模块
//! **不会**拉进 `nix` / `windows-sys`。
//!
//! 若把谓词收紧成裸 `target_os`，本机（Linux）`cargo test` 会**静默丢掉 197 / 334 个测试**（mac 140 +
//! win 57），其中包括 SYSTEM 命令注入防护（`is_safe_support_dir_rejects_all_cmd_metacharacters`）与
//! token 鉴权等安全测试。而 mac/win 二进制**无法在 Linux 上执行**，`--target` 跑 clippy 只查 lint、
//! 不跑测试 → 这些测试将**在任何机器上都不再运行**，直到有人手动上真机。审计 §G5 的教训正是
//! 「cfg 门内代码在 Linux 上看不到 → 潜伏」，故不把纯逻辑推进那个盲区。
//!
//! ### 为何 linux 的谓词是裸 `target_os`（不带 `test`）
//!
//! linux 平台模块**硬依赖 unix-only 的 stdlib 与生态**：`std::os::unix::net::UnixListener` 绑 socket、
//! `tokio::net::UnixStream::peer_cred()` 取 SO_PEERCRED、`nix::sys::signal::kill`。它在 Windows 上
//! **根本无法编译**，故不能带 `test`（否则 `clippy --target msvc --all-targets` 会去编它）。
//! 本机就是 Linux，其 97 个测试照常跑，无损失。
//!
//! ## `[[bin]]`（C6-0 已落地）：单 crate + cfg 出平台 daemon 二进制
//!
//! `[[bin]]` = `polaris-helper`（`src/bin/main.rs`），按 `cfg(target_os)` 三分派到各平台 `daemon_main`
//! （各平台 `daemon` 子模块，忠实迁自 上游 Go `main()`）：
//!
//! ```ignore
//! fn main() -> std::process::ExitCode {
//!     #[cfg(target_os = "macos")]
//!     { polaris_helper::platform::macos::daemon_main(std::env::args()) }
//!     #[cfg(target_os = "linux")]
//!     { polaris_helper::platform::linux::daemon_main(std::env::args()) }
//!     #[cfg(target_os = "windows")]
//!     { polaris_helper::platform::windows::daemon_main(std::env::args()) }
//! }
//! ```
//!
//! 每个 target 只编到自己那支（其余模块本就 cfg-out），产物即该平台的 daemon 二进制。**分不出三个
//! 二进制的风险不存在** —— 交叉编译本就一次一个 target，`--target x86_64-apple-darwin` 产出的就是
//! mac daemon。若后续需要同 target 下多 bin（如 mac 的 helper + 卸载器），`[[bin]]` 多段声明 +
//! `required-features` 即可，与 crate 数量无关。

// mac/win：纯逻辑跨平台可测（见上方「为何带 test」），OS 调用在下一层 cfg。
#[cfg(any(target_os = "macos", test))]
pub mod macos;

#[cfg(any(target_os = "windows", test))]
pub mod windows;

// linux：硬依赖 unix stdlib + tokio + nix，非 unix 无法编译（见上方理由）。
#[cfg(target_os = "linux")]
pub mod linux;
