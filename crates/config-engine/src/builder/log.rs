//! sing-box 日志配置生成（上游 `singbox-log-builder.ts` 1:1 移植）。
//!
//! 纯函数：读 UserConfig 子集 + privacyMode + 平台 + 日志文件路径（注入，不硬编码）。
//! config 字节等价由 config-snapshot 网验证（含 TUN 三平台 output 文件路径分支）。

#![forbid(unsafe_code)]

use crate::singbox::LogConfig;
use crate::user_config::{LogLevel, ProxyModeType};
// Platform 作单一真值（polaris-helper-proto）：本 builder 不再自定义平台枚举。
// 历史的 Darwin/Win32/Linux 三变体统一映射为 proto 的 Mac/Win/Linux；Other 视同 Linux（Unix 路径日志）。
use polaris_helper_proto::Platform;

/// buildLogConfig 依赖注入：日志文件路径（TUN 模式 output 字段值，由调用方提供——
/// 生产环境 = UserData/singbox.log，对拍 fixture = 固定假路径）。
#[derive(Debug, Clone)]
pub struct LogBuildDeps<'a> {
    pub privacy_mode: bool,
    pub platform: Platform,
    /// TUN 模式下 sing-box 日志文件路径。None = 调用方未提供（TUN 时 output 留空，与异常态一致）。
    pub log_file_path: Option<&'a str>,
}

/// UserConfig 中 buildLogConfig 消费的子集（上游 `singbox-log-builder.ts:15` 入参 config 的投影）。
#[derive(Debug, Clone, Default)]
pub struct LogConfigInput {
    pub log_level: LogLevel,
    pub disable_log_file: bool,
    pub proxy_mode_type: ProxyModeType,
}

/// 生成日志配置（上游 `buildLogConfig` 1:1 移植）。
///
/// 行为契约（对拍锁）：
/// 1. level = effectiveLogLevel(config.logLevel || 'info', privacyMode) —— 隐私模式抬 ≥warn。
/// 2. timestamp 恒 true。
/// 3. disableLogFile → disabled=true，直接返回（不下发 output）。
/// 4. TUN 模式（三平台）→ output = 日志文件路径；manual/systemProxy 模式不写文件（stderr 直喂）。
///
/// # `output` 一旦下发，核的 stderr 就**一行不出**（这条后果必须记住）
///
/// sing-box 的 `log.New` 见到非空 `output` 即把 `logWriter` 换成 `io.Discard` 并把日志写进那个文件
/// （`log/log.go`）。而 TUN 模式恒经提权 helper 起核、app 侧连管道都没有 ⇒ **本仓一度在 TUN 下拿不到
/// 任何实时核日志**，日志页零核行，只有导出诊断时事后 `read_tail` 才看得到。
///
/// 那条缺口现已由**另一条路**补上：核日志实时流走管理 API 的 `SubscribeLog`（不经 stderr、不经文件，
/// 见 `src-tauri/runtime/proxy.rs` 的核日志 relay）。故这里继续写文件是**有意保留**的——它服务的是
/// 「导出诊断包时附上核日志原文」，不再是实时日志的来源。改动这一格前请先确认那条 gRPC 腿仍在。
pub fn build_log_config(input: &LogConfigInput, deps: &LogBuildDeps) -> LogConfig {
    // 1. level（隐私模式从源头不让 sing-box 记录连接明细）。
    let level = input.log_level.effective(deps.privacy_mode);
    let mut cfg = LogConfig {
        level: level_to_string(level),
        timestamp: true,
        output: None,
        disabled: None,
    };

    // 2. 用户关闭日志写盘：整体禁用（隐私/省盘），不再写文件，直接返回。
    if input.disable_log_file {
        cfg.disabled = Some(true);
        return cfg;
    }

    // 3. TUN 模式（三平台）→ output 写文件。
    // Polaris 谓词：isTunMode && (darwin || win32 || linux)。三平台全覆盖 → 实质 = isTunMode，
    // 但保留平台枚举检查忠实移植（防未来平台分支差异）。
    // Platform::Other 视同 Linux（未知平台按 Unix 路径保守写文件，TUN 下 stdout 不可捕获）。
    let writes_log_to_file = input.proxy_mode_type.is_tun()
        && matches!(
            deps.platform,
            Platform::Mac | Platform::Win | Platform::Linux | Platform::Other
        );

    if writes_log_to_file {
        if let Some(path) = deps.log_file_path {
            cfg.output = Some(path.to_string());
        }
    }

    cfg
}

/// LogLevel → sing-box JSON 字符串（serde lowercase rename 的手动镜像，builder 内部产 String）。
fn level_to_string(level: LogLevel) -> String {
    match level {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
        LogLevel::Fatal => "fatal",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps(privacy: bool, platform: Platform, path: Option<&str>) -> LogBuildDeps<'_> {
        LogBuildDeps {
            privacy_mode: privacy,
            platform,
            log_file_path: path,
        }
    }

    #[test]
    fn system_proxy_no_output() {
        let input = LogConfigInput {
            proxy_mode_type: ProxyModeType::SystemProxy,
            ..Default::default()
        };
        let cfg = build_log_config(&input, &deps(false, Platform::Linux, Some("/tmp/sb.log")));
        assert_eq!(cfg.level, "info");
        assert!(cfg.timestamp);
        assert!(cfg.disabled.is_none());
        assert!(cfg.output.is_none(), "systemProxy 不写文件");
    }

    #[test]
    fn tun_linux_writes_output() {
        let input = LogConfigInput {
            proxy_mode_type: ProxyModeType::Tun,
            ..Default::default()
        };
        let cfg = build_log_config(
            &input,
            &deps(false, Platform::Linux, Some("/fake/singbox.log")),
        );
        assert_eq!(cfg.output.as_deref(), Some("/fake/singbox.log"));
    }

    #[test]
    fn tun_mac_writes_output() {
        let input = LogConfigInput {
            proxy_mode_type: ProxyModeType::Tun,
            ..Default::default()
        };
        let cfg = build_log_config(&input, &deps(false, Platform::Mac, Some("/fake/sb.log")));
        assert_eq!(cfg.output.as_deref(), Some("/fake/sb.log"));
    }

    #[test]
    fn privacy_raises_level() {
        let input = LogConfigInput {
            log_level: LogLevel::Debug,
            proxy_mode_type: ProxyModeType::SystemProxy,
            ..Default::default()
        };
        let cfg = build_log_config(&input, &deps(true, Platform::Linux, None));
        assert_eq!(cfg.level, "warn", "隐私模式 debug → warn");
    }

    #[test]
    fn disable_log_file_short_circuits() {
        let input = LogConfigInput {
            disable_log_file: true,
            proxy_mode_type: ProxyModeType::Tun,
            ..Default::default()
        };
        let cfg = build_log_config(&input, &deps(false, Platform::Linux, Some("/fake/sb.log")));
        assert_eq!(cfg.disabled, Some(true));
        assert!(cfg.output.is_none(), "disabled 时不写 output");
    }

    #[test]
    fn manual_mode_no_output() {
        let input = LogConfigInput {
            proxy_mode_type: ProxyModeType::Manual,
            ..Default::default()
        };
        let cfg = build_log_config(&input, &deps(false, Platform::Linux, Some("/fake/sb.log")));
        assert!(cfg.output.is_none(), "manual 模式 stdout 直喂不写文件");
    }
}
