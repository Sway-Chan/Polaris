//! 系统接管层错误类型。
//!
//! Polaris 侧系统代理 / DNS 操作以「best-effort + 日志降级」为主（失败兜底回滚、不阻断 TUN 启动），
//! 仅在少数结构性不可降级场景抛错（代理设置失败、marker 损坏等）。本 Error 用于这些显式信号；
//! marker IO 失败仍走「仅告警绝不抛」（与 Polaris writeMarker/clearMarker 同口径）。

#![forbid(unsafe_code)]

use thiserror::Error;

/// 系统接管错误。
#[derive(Debug, Clone, Error)]
pub enum SystemIntegrationError {
    /// 系统代理设置/清除失败（对应 Polaris enableProxy/disableProxy 的 throw 分支）。
    #[error("system proxy error: {0}")]
    Proxy(String),

    /// 系统 DNS 接管失败（非 best-effort 降级路径，仅显式调用方预期）。
    #[error("system dns error: {0}")]
    Dns(String),

    /// 路由出口探测失败（`route -n get` / `ip route get` / `Find-NetRoute` 命令失败或输出无法解析）。
    #[error("system route error: {0}")]
    Route(String),

    /// 不支持的平台。
    #[error("unsupported platform: {0}")]
    UnsupportedPlatform(String),
}

impl SystemIntegrationError {
    /// 构造代理错误。
    pub fn proxy(msg: impl Into<String>) -> Self {
        Self::Proxy(msg.into())
    }

    /// 构造 DNS 错误。
    pub fn dns(msg: impl Into<String>) -> Self {
        Self::Dns(msg.into())
    }

    /// 构造路由错误。
    pub fn route(msg: impl Into<String>) -> Self {
        Self::Route(msg.into())
    }
}
