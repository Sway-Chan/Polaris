//! 日志级别（上游 `shared/types.ts:135 LogLevel`）+ effectiveLogLevel（隐私模式抬 ≥warn）。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// sing-box / app 日志级别。顺序 = 严重度递增（effectiveLogLevel 依赖此顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    #[default]
    Info,
    Warn,
    Error,
    Fatal,
}

impl LogLevel {
    /// 严重度序号（debug=0 … fatal=4），对齐 Polaris ORDER 数组 indexOf。
    fn order(self) -> usize {
        match self {
            LogLevel::Debug => 0,
            LogLevel::Info => 1,
            LogLevel::Warn => 2,
            LogLevel::Error => 3,
            LogLevel::Fatal => 4,
        }
    }

    /// 隐私模式下有效日志级别：抬到至少 warn（info/debug 才记连接明细，warn+ 不记）。
    /// 上游 `shared/log-level.ts effectiveLogLevel` 1:1 移植。非隐私原样返回。
    pub fn effective(self, privacy: bool) -> Self {
        if !privacy {
            return self;
        }
        let cur = self.order();
        let warn = LogLevel::Warn.order();
        if cur < warn {
            LogLevel::Warn
        } else {
            self
        }
    }

    /// 默认级别（config.logLevel 缺省时）。const fn 供常量上下文用。
    pub const fn default_level() -> Self {
        LogLevel::Info
    }
}

#[cfg(test)]
mod tests {
    use super::LogLevel;

    #[test]
    fn effective_privacy_raises_below_warn() {
        assert_eq!(LogLevel::Debug.effective(true), LogLevel::Warn);
        assert_eq!(LogLevel::Info.effective(true), LogLevel::Warn);
    }

    #[test]
    fn effective_privacy_keeps_warn_and_above() {
        assert_eq!(LogLevel::Warn.effective(true), LogLevel::Warn);
        assert_eq!(LogLevel::Error.effective(true), LogLevel::Error);
        assert_eq!(LogLevel::Fatal.effective(true), LogLevel::Fatal);
    }

    #[test]
    fn effective_no_privacy_passthrough() {
        for lvl in [
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
            LogLevel::Fatal,
        ] {
            assert_eq!(lvl.effective(false), lvl);
        }
    }
}
