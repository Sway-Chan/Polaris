//! Windows 特权 helper 本体（§D 特权矩阵 win 行，移植自 上游 `helper-win/` Go module）。
//!
//! ## 职责（system-design §D.3 + 维度7）
//!
//! Polaris Windows helper 是一个以 LocalSystem 身份运行的 SCM 服务，监听 token 鉴权的命名管道，按行协议驱动
//! sing-box 启停。装一次（安装期一次 UAC）后，普通用户 app 经命名管道零提权启停 sing-box —— 切节点/停止/
//! 退出/崩溃回收均免再次 UAC。本模块是该 helper 的 Rust 重写（day-1 全 Rust 收益：与 core-supervisor /
//! helper-proto 共享类型，消灭跨语言 wire drift）。
//!
//! 源真值（Go 源仅 oracle，Rust 重写不编译 Go）：
//! - `~/Code/polaris/helper-win/helper.go` —— 协议骨架 + 并发纪律（mu/child/watchParent/terminateChild）
//! - `~/Code/polaris/helper-win/main.go` —— 入口（--console dev / SCM 服务）
//! - `~/Code/polaris/helper-win/service.go` —— SCM 托管 + 命名管道（SDDL）+ serve 循环
//! - `~/Code/polaris/helper-win/selfuninstall.go` —— 零 UAC 自毁卸载旁路命令行（SYSTEM cmd 删自身）
//! - `~/Code/polaris/helper-win/winproc.go` —— 进程枚举/kill/Job Object/freeport/IP 转发/spawn 旁路
//!
//! ## 共用层（不在本模块 —— 见 crate 顶层）
//!
//! - token 文件读取 + **常量时间比对**：[`crate::token`]（与 mac 逐字同 → 单一真值。原
//!   `helper-win/src/token.rs` 是 thin re-export 壳，单 crate 后已删；调用点直接用
//!   [`crate::token::is_authed_constant_time`]）。
//! - **不用** [`crate::line_io`]：win 走裸 Win32 `HANDLE` 的**整帧读**（命名管道，一次 `ReadFile` 取整个
//!   请求帧再切行），不经 `std::io::BufRead` —— 是**真平台差异**，不强行归一。
//! - **无 install-core**：macOS 专属内核持久化，Windows 由 app 侧 NSIS 安装器处理。
//!
//! ## 移植纪律
//!
//! 1. **Go 源仅 oracle**：逐行对照，Rust 重写（语义对齐，不照搬语法）。
//! 2. **复用 helper-proto**：[`polaris_helper_proto`] 的 Request/Response/codec 单一真值，本模块不重复定义 wire 形态。
//! 3. **Windows API**：用 `windows-sys` crate（非手写 syscall），全部 `#[cfg(windows)]` gate —— 该依赖在
//!    `Cargo.toml` 里是 `[target.'cfg(windows)'.dependencies]`，编非 Windows target 时**不参与解析**。
//! 4. **`#![deny(unsafe_code)]`**：本模块 deny（非 forbid —— forbid 无法被子模块 allow 覆盖）。
//!    Windows FFI 的 unsafe 集中在 [`winproc`] / [`service`] 模块，用模块级 `#![allow(unsafe_code)]` 局部放开
//!    + 文档化每处 unsafe 的理由；FFI 之外全部 safe（跨平台核心逻辑 + 测试 mock 无 unsafe）。
//! 5. **本机 Linux 可编译/可测**：Windows 特定代码 cfg skip；跨平台逻辑（自卸载命令行拼装、iface 白名单判定、
//!    token 鉴权、协议分派、wintun 轮询骨架、winproc 逻辑经 trait mock）在 Linux 上有单元测试。
//!    故本模块声明处的 cfg 谓词带 `test`（见 [`crate::platform`] 门控矩阵）。
//! 6. **不触碰宿主**：系统操作（进程枚举/kill、TCP 表、IP 转发注册表、SCM 服务、命名管道）经 trait 抽象
//!    （[`ops`]）+ 生产实现（[`winproc`]），测试用 mock 实现。
//!
//! ## 本模块布局（真 win 差异）
//!
//! - [`selfuninstall`]：零 UAC 自毁卸载旁路命令行拼装（`helper-win/selfuninstall.go`，跨平台纯字符串逻辑）。
//! - [`logic`]：协议层纯逻辑（iface/cfg/port 白名单、filepath basename、TCP 端口字节序解析、sing-box 镜像匹配）。
//! - [`ops`]：系统操作 trait（[`ops::ProcessOps`] / [`ops::NetTableOps`] / [`ops::IpForwardingOps`]）+ mock。
//! - [`wintun`]：wintun 适配器释放探测（维度7 #30，trait 抽象的有界轮询）。
//! - [`helper`]：协议分派核心（Go `handle()` 的 switch 分支，经 trait 抽象，跨平台可测）。
//! - [`winproc`]：Windows FFI 生产实现（`#[cfg(windows)]`，`helper-win/winproc.go` 对应）。
//! - [`service`]：SCM 服务 + 命名管道监听（`#[cfg(windows)]`，`helper-win/service.go` + `main.go` 对应）。
//!
//! ## 地雷（system-design §C / 维度7，B3 真机必验）
//!
//! - **session-0 无 console**：服务模式 `GenerateConsoleCtrlEvent(CTRL_BREAK)` 是 no-op（只投给共享 console 的
//!   进程），停核走 `TerminateProcess` 硬杀 → wintun 适配器/路由残留竞态（§C #8）。Job Object
//!   （`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）兜底防孤儿进程（不防 wintun 残留）。
//! - **自毁卸载难度守恒**：cmd.exe 引号必须手工拼（`SysProcAttr.CmdLine` 等价物）—— `CreateProcessW` 的
//!   `lpCommandLine` 原样下发，绕开 argv 转义，否则 rmdir 路径被 `\"` 转义成非法路径（`selfuninstall.go:10-18`）。
//! - **零 UAC 自毁残留**：维度7 要求卸载后系统零残留 —— 旁路停删 SCM 服务 + 删 supportDir（含 helper.exe 自身 +
//!   helper.token），失败由 app 侧 NSIS 卸载钩子兜底（纵深）。

#![deny(unsafe_code)]

pub mod daemon;
pub mod helper;
pub mod logic;
/// 本机网络接口只读枚举（**免提权**，app 进程直调；住在本 crate 纯为复用已有的 `windows-sys` FFI —— 理由见模块文档）。
pub mod netinfo;
pub mod ops;
pub mod selfuninstall;
pub mod wintun;

#[cfg(windows)]
pub mod service;
#[cfg(windows)]
pub mod winproc;

// 便利重导出：让 `platform::windows::WinHelper` 等无需钻模块路径。
pub use helper::{HandleOutcome, WinHelper};
pub use selfuninstall::{is_safe_support_dir, self_uninstall_cmd_line};

// daemon 入口（binary main 经 cfg 调）：serve 入口即 [`daemon::daemon_main`]（内部 MkdirAll support +
// console/SCM 分支，接现有 service::run_console / run_service）。
pub use daemon::{daemon_main, parse_args as parse_daemon_args, WinArgs};

/// Windows helper protoVersion（三平台统一 v1，见 `polaris_helper_proto` crate 文档）。
///
/// **不移植** 上游的 win=5：那个 5 是 Go helper 的功能加法计数（v2 加 route-add/del、v3/v4 换
/// iface-metric 实现、v5 PowerShell 全路径），用途是让新 client 认出机器上装着的旧 helper。
/// Polaris 的 Rust helper 是首发，**上来就带齐全部命令**，没有中间代需要被区分 ⇒ v1。
/// 无 install-core（macOS 专属内核持久化，Windows 由 app 侧 NSIS 安装器处理）。
pub const PROTO_VERSION: u32 = polaris_helper_proto::proto_version::CURRENT;

/// 本 helper 的协议谱系平台标识（命名管道 + token 行；无 install-core）。
///
/// 用于 [`polaris_helper_proto::codec`] 帧编解码时决定是否在头部加 token 行
/// （[`polaris_helper_proto::Platform::has_token_line`]）。
pub const PLATFORM: polaris_helper_proto::Platform = polaris_helper_proto::Platform::Win;

/// Windows SCM 服务名（`helper-win/main.go:15`，`const serviceName = "PolarisHelper"`）。
///
/// 安装期固定常量，sc.exe stop/delete 与自毁旁路均引用本名。
pub const SERVICE_NAME: &str = "PolarisHelper";

/// 命名管道名（`helper-win/service.go:16`，`const pipeName = \\.\pipe\polaris-helper`）。
///
/// 镜像 macOS 的 helper.sock 命名谱系（polaris-helper）。Windows 命名管道路径形如 `\\.\pipe\<name>`。
pub const PIPE_NAME: &str = r"\\.\pipe\polaris-helper";

/// 命名管道 SDDL（纵深防御；token 仍是主鉴权边界，`helper-win/service.go:34`）。
///
/// `D:(A;;FA;;;SY)(A;;GRGW;;;IU)`：
/// - `D:` DACL 起始。
/// - `(A;;FA;;;SY)` Allow / FILE_ALL_ACCESS / SYSTEM —— 服务本体（LocalSystem）完全控制管道。
/// - `(A;;GRGW;;;IU)` Allow / GENERIC_READ+GENERIC_WRITE / Interactive Users —— 桌面登录用户可连接读写。
///
/// 选 IU（交互登录用户）而非 BU/AU：Polaris GUI 由当前桌面登录用户运行，IU 精确覆盖「本机交互会话」，
/// 不授予服务账户/远程会话/网络登录。GENERIC_READ|WRITE 足以连接管道 + ReadFile/WriteFile，
/// 不授予 FILE_ALL_ACCESS（无需改 ACL/删管道）。
pub const PIPE_SDDL: &str = "D:(A;;FA;;;SY)(A;;GRGW;;;IU)";

/// Windows 默认 supportDir（`helper-win/main.go:21`，`--support` 默认值）。
///
/// `helper.token` 落此目录（SYSTEM 私有，ACL 由安装期设定）。
pub const DEFAULT_SUPPORT_DIR: &str = r"C:\ProgramData\Polaris";

/// 安全占位路径（`selfuninstall.go:26`）—— 恶意 supportDir 命中 cmd 元字符时改用此路径（rmdir 落空，不注入）。
pub const SAFE_PLACEHOLDER_DIR: &str = r"C:\Polaris\safe-placeholder-nonexistent";

#[cfg(test)]
mod tests {
    use super::*;

    // 服务名 / 管道名 / SDDL 是已部署 helper 的硬约束（改 = 断连接）；protoVersion 则**不再**锁
    // 上游的 5 —— 那是别人 Go module 的演进计数，Polaris 首发即 v1（见 helper-proto crate 文档）。
    #[test]
    fn proto_version_is_unified_current() {
        assert_eq!(PROTO_VERSION, polaris_helper_proto::proto_version::CURRENT);
        assert_eq!(PROTO_VERSION, 1);
    }

    #[test]
    fn platform_is_win_with_token_line() {
        // helper-win/helper.go:167-168: 命名管道 + token 行（tok := readLine(r)）
        assert_eq!(PLATFORM, polaris_helper_proto::Platform::Win);
        assert!(PLATFORM.has_token_line());
    }

    #[test]
    fn service_name_pipe_name_sddl_match_go_source() {
        // main.go:15 / service.go:16,34
        assert_eq!(SERVICE_NAME, "PolarisHelper");
        assert_eq!(PIPE_NAME, r"\\.\pipe\polaris-helper");
        assert_eq!(PIPE_SDDL, "D:(A;;FA;;;SY)(A;;GRGW;;;IU)");
    }
}
