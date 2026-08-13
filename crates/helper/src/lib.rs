//! # polaris-helper — 三平台特权 helper（单 crate，cfg 隔离平台差异）
//!
//! 本 crate 是 macOS / Windows / Linux 三个特权 helper 的**唯一实现处**。合并前它们是三个独立 crate
//! （`helper-mac` / `helper-win` / `helper-linux`）+ 一个为「让三者共享」而存在的 `helper-common`。
//! **crate 边界本身正是重复的来源** —— 每加一份共享逻辑就要新开一层转发壳（token / install-core 的
//! thin re-export 模块即是），故收敛为单 crate。
//!
//! ## 组织原则（本 crate 的宪法）
//!
//! 1. **默认共用**：逻辑写在 crate 顶层普通模块（[`core_install`] / [`line_io`] / [`token`]），三平台
//!    直接 `crate::xxx` 调用。同 crate 内共享**不需要任何抽象层** —— 就是普通 `pub fn`。
//! 2. **只有真平台差异才隔离**：进 [`platform`] 的 `macos` / `windows` / `linux` 子模块。
//! 3. **判据**：「OS 语义不同」= 真差异（服务管理 launchd/SCM/systemd、freeport 持有者定位机制、
//!    win 裸 HANDLE 整帧读）；「只是当初拆在不同 crate 里」= **假差异**，必须共用。
//!
//! ## 共用层（顶层模块，无 cfg）
//!
//! - [`core_install`]：内核 sha256 校验 + 逐文件原子安装 + 清残留。mac/linux 核心流程逐字同，
//!   平台差异（mac xattr/codesign、linux 保守 lib*.so 清理）由各平台模块在调用前后加 hook。
//! - [`line_io`]：行协议读写原语（trim `\r\n` / 写行 + flush），泛型于 `std::io::{BufRead, Write}`。
//!   mac/linux 共用；**win 走裸 Win32 `HANDLE` 整帧读，是真差异**，不用本模块。
//! - [`token`]：socket 鉴权 token 读写 + **常量时间比对**。mac/win 逐字同 → 单一真值。
//!
//! ## 平台层（[`platform`]，模块声明处 cfg 门控）
//!
//! 见 [`platform`] 模块文档的门控矩阵与理由。
//!
//! ## `unsafe_code` 政策（crate 级，C6-0 确立）
//!
//! **全 crate `deny(unsafe_code)`（非 `forbid`）+ 需 syscall/FFI 的模块局部 `allow(unsafe_code)` + `// SAFETY:`。**
//!
//! - crate 根 `#![deny(unsafe_code)]`：默认禁 unsafe，但 `deny`（非 `forbid`）**可被下游模块用
//!   `#![allow(unsafe_code)]` 局部覆盖** —— 这是提权链（mac sysctl / linux fork+setuid+AmbientCaps）
//!   与 Windows FFI 必需的逃生口。
//! - **提权/FFI 模块局部放开**：Windows FFI（[`platform::windows::winproc`] /
//!   [`platform::windows::service`]）已用模块级 `#![allow(unsafe_code)]`；C6-1（mac `sysctl` procStartTime）
//!   / C6-2（linux `fork`+`setuid`+AmbientCaps 拉核）在各自 syscall 模块**同样用模块级
//!   `#![allow(unsafe_code)]` + 每处 unsafe 块附 `// SAFETY:`**（对齐 win FFI 用法）。
//! - **默认仍是 deny**：未局部放开的模块（共用层 + 各平台纯逻辑）一律禁 unsafe。C6-0 前 mac/linux 平台
//!   模块曾是模块级 `#![forbid(unsafe_code)]`（合并前独立 crate 的遗留），会**硬挡** C6-1/2/3 的 syscall
//!   局部 `allow` —— C6-0 已统一降为 `#![deny(unsafe_code)]`（与 win 一致），本批不写具体 unsafe 块。
//!
//! ## 与其余 helper crate 的边界（**勿合并**，均有据）
//!
//! - [`polaris_helper_proto`]：零依赖协议层，**特权 daemon 侧（本 crate）与非特权 client 侧共用** ——
//!   是真信任边界，且 `[dependencies]` 必须保持为空。
//! - `polaris-helper-client`：非特权侧，刻意维持最小依赖面（连 `rand`/`getrandom` 都不引）。其
//!   `token.rs` 的常量时间比对**独立第 4 份，有据保留**（审计 §G3.5：合并要新增依赖边 + 改安全语义）。
//!   回退提权路径（osascript / UAC / pkexec+polkit）**不另设 crate**：真值源是
//!   [`polaris_helper_client::privilege`]（`Escalation` argv 构造 + `Executor` + 三态归类 + 用户取消判定），
//!   已被 [`polaris_helper_client::HelperManager`] 的 install/uninstall 全流程消费。曾为它预留过一个
//!   `polaris-privilege-broker` 空占位 crate（system-design §B.2「批次 6 待接线」的落点），C6-4 判定
//!   「真逻辑就近留在 helper-client 更优」——迁进去反会新增依赖边、割裂编排——故该 crate 从未承载实现，
//!   2026-07-29 连同本条旧「勿删」陈述一并删除。
//!
//! ## 形态：lib + daemon `[[bin]]`（C6-0 起）
//!
//! 除 lib 外，`[[bin]]` = `polaris-helper`（[`src/bin/main.rs`]）经 `cfg(target_os)` 三分派到各平台
//! [`platform::macos::daemon_main`] / [`platform::linux::daemon_main`] / [`platform::windows::daemon_main`]，
//! 一次交叉编译一个 target 即出该平台的 daemon 可执行体。单 crate + cfg 出三平台二进制的路径见
//! [`platform`] 模块文档末节。
//!
//! C6-0（底座批）只接「flag 解析 + main 骨架 + serve 入口调用」，进程能起、能收信号退出；连接分派
//! （accept → `process_connection`/`handle`）与提权拉核属 C6-1/2/3。

#![deny(unsafe_code)]

// ===== daemon flag 解析：跨平台纯逻辑共用（无 cfg）=====
pub mod cli;

// ===== 共用层：三平台共享的普通模块（无 cfg，无抽象层）=====
pub mod core_install;
pub mod line_io;
pub mod token;

// ===== 平台层：仅真差异 =====
pub mod platform;
