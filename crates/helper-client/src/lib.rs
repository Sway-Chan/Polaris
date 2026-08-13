//! polaris-helper-client — 主进程与特权 helper 的通信客户端（§B.2）。
//!
//! Polaris 主进程经 [`HelperManager.ts`](Polaris) 管理 macOS LaunchDaemon（安装/卸载/状态探测），
//! 经 socket（mac/linux）或 named pipe（win）连 helper，发行文本协议请求驱动 sing-box 启停
//!（[`sendCommand`](Polaris)，`HelperManager.ts:433-457`）。helper 不可用时回退平台提权
//!（osascript / UAC / pkexec，[`PlatformPrivilegeService.ts`](Polaris)）。
//!
//! 本 crate 是 Polaris 主进程侧的等价物 —— 三件套：
//!
//! - [`client::HelperClient`]：连接 helper socket/pipe，发 [`Request`](polaris_helper_proto::Request)
//!   收 [`Response`](polaris_helper_proto::Response)。复用 `polaris-helper-proto` 的 codec（零 wire drift）。
//! - [`manager::HelperManager`]：helper 生命周期（安装路径 launchd/systemd/SCM + 状态探测 + 装卸决策）。
//! - [`privilege::PrivilegeEscalation`]：提权回退纯逻辑决策（osascript / UAC / pkexec argv 构造）。
//!
//! ## 移植纪律
//!
//! 1. **socket/pipe trait 抽象**：[`transport::ConnectionStream`] + [`client::Connector`]，测试 mock
//!    （[`transport::MockStream`]），不触碰宿主。
//! 2. **复用 helper-proto codec**：[`polaris_helper_proto::codec::encode`] + [`polaris_helper_proto::Response::parse`]，
//!    与已部署 helper 协议强一致（wire drift 编译期消灭）。
//! 3. **`forbid(unsafe_code)`**：无裸 syscall。
//! 4. **不 commit git**。
//!
//! ## 模块布局
//!
//! - [`transport`]：连接字节流抽象 + mock。
//! - [`token`]：token 行鉴权（读/写/比对 token 文件）。
//! - [`client`]：[`client::HelperClient`]（连接 + send + 重连/超时）。
//! - [`manager`]：[`manager::HelperManager`]（生命周期 + [`manager::SysOps`] trait）。
//! - [`privilege`]：提权回退纯逻辑（argv 构造 + 取消判定）。

#![forbid(unsafe_code)]
#![cfg_attr(not(test), allow(dead_code))]

pub mod client;
pub mod connector;
pub mod manager;
pub mod privilege;
pub mod token;
pub mod transport;

// 顶层便利重导出：让 `polaris_helper_client::HelperClient` 无需钻模块路径（主进程主用类型）。
pub use client::{ClientError, Connector, HelperClient};
#[cfg(windows)]
pub use connector::PipeConnector;
#[cfg(unix)]
pub use connector::UnixConnector;
pub use manager::{
    HelperManager, HelperStatus, InstallParams, InstallPaths, ManagerError, StdSysOps, SysOps,
    READY_POLL_ATTEMPTS, READY_POLL_DELAY,
};
pub use privilege::{
    classify_outcome, is_user_cancelled, osascript_escalation, pkexec_escalation, run_escalation,
    uac_escalation, Escalation, EscalationOutcome, Executor, PrivilegeMethod, StdExecutor,
};
pub use token::{is_authorized, read_token, remove_token, write_token, write_token_content};
pub use transport::{
    ConnectionStream, IoAdapter, MockStream, INSTALL_CORE_TIMEOUT_MS, READ_TIMEOUT,
};
