//! SCM 服务 + 命名管道监听占位（`#[cfg(windows)]` 真实现见下方）。

#![cfg_attr(windows, allow(unsafe_code))]

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::*;
