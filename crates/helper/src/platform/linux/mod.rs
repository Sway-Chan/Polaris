//! Linux 特权 helper（§D.1 day-1 Rust，移植自 上游 `helper-linux/` Go module）。
//!
//! 职责（system-design §D.3 linux 行）：root systemd unit + 0666 unix socket + **SO_PEERCRED 鉴权**
//!（tokio `peer_cred()` / std `UCred` 一等原生，无需第三方 IPC crate）+ uid 允许列表 + AmbientCaps
//! 拉起 sing-box（登录用户 + CAP_NET_ADMIN/RAW/BIND_SERVICE，**绝不 root 跑核**，`helper.go:432-442`
//! 安全模型差异点）+ pkexec 免打扰 + resolve1 polkit 规则。
//!
//! ## 共用层（不在本模块 —— 见 crate 顶层）
//!
//! - install-core 核心：[`crate::core_install`]（与 mac 逐字同）。本模块 [`core_installer`] 只留
//!   linux 专属的**保守 prune**（仅清 `sing-box` / `lib*` 前缀，防误删同目录 helper 二进制）。
//! - 行协议读写：[`crate::line_io`]（与 mac 共用）。
//! - token：linux **不用** —— 鉴权走 SO_PEERCRED（内核背书，不可伪造），是比 token 行更强的边界，
//!   真平台差异。
//!
//! ## 本模块布局（对照 Go 源文件）
//!
//! - [`auth`]：SO_PEERCRED 凭据取 + 授权 uid 列表 + config 属主校验（移植自 `helper.go:77-133`）。
//! - [`state`]：Handler 进程状态（child/childDone）+ CoreSpawner trait（对应 Go 全局 `child` + `c.Start()`）。
//! - [`ops`]：系统操作 trait 抽象（systemd / TUN / route，§D 特权矩阵可测试边界）。
//! - [`core_installer`]：install-core 的 linux 保守 prune hook（核心走公共层）。
//! - [`freeport`]：按端口找 LISTEN 持有者 + 跨用户防误杀（linux 机制：`ss` 正则 + `/proc` +
//!   `kill(2)` —— 与 mac 的 `lsof`+`ps` 是真差异）。
//! - [`handler`]：命令分发器（移植自 `helper.go:333-482` 的 `handle(conn)`，所有命令逐分支对照）。
//! - [`server`]：socket 准备 + ss_lookup + ServerConfig（移植自 `main.go`）。
//!
//! ## 为何本模块整体 `#[cfg(target_os = "linux")]`
//!
//! 与 mac/win 平台模块不同，本模块**硬依赖 unix-only 的 stdlib 与生态**（`std::os::unix::net::UnixListener`
//! 绑 socket、`tokio::net::UnixStream::peer_cred()`、`nix::sys::signal::kill`），在 Windows 上根本无法
//! 编译，故门控到 `target_os = "linux"`（`tokio` / `nix` 依赖同样是 linux target-specific）。本机即
//! Linux，测试照常跑。详见 [`crate::platform`] 的门控矩阵。
//!
//! ## 移植纪律
//!
//! 1. **Go 源仅 oracle**：逐分支对照 Go 的 socket 循环 + 命令处理，Rust 重写。
//! 2. **复用 helper-proto**：Request/Response/ErrorCode/codec 从 `polaris-helper-proto` import。
//! 3. **deny(unsafe_code)**（C6-0 前曾是 `forbid`，已降为 `deny` 与 win 一致）：纯逻辑 + safe wrapper 的
//!    syscall（SO_PEERCRED 用 tokio peer_cred；kill 用 nix safe wrapper）零 unsafe；C6-2 的
//!    `fork`+`setuid`+AmbientCaps 拉核需 unsafe 时，在 spawner 模块用模块级 `#![allow(unsafe_code)]` +
//!    `// SAFETY:`（`deny` 可覆盖，`forbid` 不能 —— 见 crate 文档 unsafe 政策）。
//! 4. **不碰宿主网络/系统**：所有系统操作（systemctl/ip/route/ss/kill）用 trait 抽象，测试 mock。
//! 5. **不 commit git** / clippy 零警告。
//!
//! ## 关键地雷（C6-2 已落地代码，真降权拉核链为真机门）
//!
//! - **AmbientCaps fork+setuid+execve 链**（[`server::AmbientCapsSpawner`]）：start 拉核 = `Command` +
//!   `pre_exec`（`set_keepcaps` → `setgroups`/`setgid`/`setuid` 降权回对端登录用户 → raise Inheritable/
//!   Ambient CAP_NET_ADMIN/RAW/BIND_SERVICE → execve coreDir/sing-box）。唯一 unsafe = `CommandExt::pre_exec`
//!   （`server.rs` 模块级 `#![allow(unsafe_code)]` + `// SAFETY:`，`deny` 可覆盖）。**真 fork+降权拉核为
//!   真机门**（本机绝不跑）；纯逻辑（caps 集/terminate 决策/watchParent 决策）已单测。
//! - **async UnixStream → 同步 Conn adapter**（[`server::ConnServer::dispatch`]）：accept 到的 tokio
//!   `UnixStream` 先取 SO_PEERCRED（捕获进 [`auth::CapturedPeerCred`]），转 std 阻塞流 + 5s 读超时，
//!   `spawn_blocking` 跑同步 [`handler::handle`]（含 fork 拉核，不占 async worker）。

#![deny(unsafe_code)]

pub mod auth;
pub mod core_installer;
pub mod daemon;
pub mod freeport;
pub mod handler;
pub mod ops;
pub mod server;
pub mod state;

// daemon 入口（binary main 经 cfg 调）：serve 入口即 [`daemon::daemon_main`]（内部建 tokio runtime +
// prepare_socket + accept 循环骨架 + SIGTERM 收割器 + 退出兜底 setForward(false)）。
pub use daemon::daemon_main;

// 便利重导出：让 `platform::linux::HandlerDeps` 等无需钻模块路径。
pub use auth::{
    is_authorized, owned_by, supplementary_groups, AuthError, CapturedPeerCred, PeerCred,
    PeerCredProvider,
};
pub use core_installer::install_core;
pub use freeport::{free_port, parse_ss_pids, FreePortDeps};
pub use handler::{handle, Conn, HandlerDeps, LineConn, READ_TIMEOUT_SECS};
// DNS flush 不在 Linux helper 职责内（无上游 Go 命令 / app 进程非提权侧已有单一真值
// `system-integration::dns_flush`）；判据见 ops.rs「系统 DNS 刷新」段。
pub use ops::{
    set_forward_prod, RouteAction, RouteOps, RouteVerb, SystemdAction, SystemdOps, SystemdResult,
    TokioRoute, TokioSystemd, TokioTun, TunAction, TunOps,
};
pub use server::{
    ambient_caps, forward_fn, prepare_socket, ss_lookup, AmbientCapsSpawner, ConnServer,
    ServerConfig, ServerError, WatchStep, DEFAULT_AUTH_FILE, DEFAULT_CORE_DIR, DEFAULT_SOCK_PATH,
};
pub use state::{CoreHandle, CoreSpawner, HandlerState, SpawnCoreRequest, SpawnError};

/// Linux helper protoVersion（三平台统一 v1，见 `polaris_helper_proto` crate 文档）。
pub const PROTO_VERSION: u32 = polaris_helper_proto::proto_version::CURRENT;
