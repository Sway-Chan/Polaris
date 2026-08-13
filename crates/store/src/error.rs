//! 配置存储错误类型。
//!
//! 遵循 sanitize-don't-throw 纪律（维度7 #7）：坏字段跳过、不崩、绝不清空用户配置。
//! 本 Error 仅用于「结构性不可 sanitize」的回退信号（交上层 catch 备份不覆盖），
//! 与 validateConfig 中极少数 throw 同口径（subscriptions/servers 非数组、proxyMode 非法等）。

#![forbid(unsafe_code)]

use thiserror::Error;

/// 配置存储错误。对应 Polaris ConfigManager.validateConfig 中会 throw 的结构性错误。
#[derive(Debug, Clone, Error)]
pub enum StoreError {
    /// JSON 解析失败（坏 JSON）。对应 TS loadConfig catch 的 SyntaxError 分支。
    /// 存 String（serde_json::Error 未实现 Clone；LoadResult 需要 Clone 以便日志记录）。
    #[error("config JSON parse failed: {0}")]
    Parse(String),

    /// 结构性校验失败（subscriptions/servers 非数组、proxyMode/proxyModeType/logLevel 非法等）。
    /// 对应 validateConfig 中的 throw——交 loadConfig catch（已备份不覆盖）。
    #[error("config validation failed: {0}")]
    Validation(String),

    /// 文件系统错误（读/写/创建目录）。运行时层填充，纯逻辑测试用 mock。
    #[error("io error: {0}")]
    Io(String),
}

impl StoreError {
    /// 构造一条校验错误（语义化便捷构造）。
    pub fn validation(msg: impl Into<String>) -> Self {
        StoreError::Validation(msg.into())
    }

    /// 从 serde_json::Error 构造 Parse 错误（便捷转换）。
    pub fn from_parse(e: serde_json::Error) -> Self {
        StoreError::Parse(e.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Parse(e.to_string())
    }
}
