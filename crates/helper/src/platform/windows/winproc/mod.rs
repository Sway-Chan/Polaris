//! Windows FFI 生产实现（移植自 `helper-win/winproc.go`）。
//!
//! 本模块仅在 `cfg(windows)` 下编译（生产 Windows target）。顶层 `#![forbid(unsafe_code)]` 在本模块局部
//! 放开（`#![allow(unsafe_code)]`）—— 所有 Windows API 调用都是 FFI，必须 unsafe。每处 unsafe 块附 SAFETY
//! 理由。实现 [`crate::platform::windows::ops::ProcOps`] + [`crate::platform::windows::ops::NetTableOps`]，作为 [`crate::platform::windows::helper::WinHelper`] 的生产注入。
//!
//! 地雷（system-design §C #8，必真机复验）：
//! - session-0 无 console → [`send_ctrl_break`] 是 no-op → [`WinProcOps::reap_child`] 走 TerminateProcess 硬杀。
//! - Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）兜底防孤儿进程（helper 崩溃时内核连坐杀 child）。

#![cfg_attr(windows, allow(unsafe_code))]

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::*;
