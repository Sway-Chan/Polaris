//! 协议错误码（Polaris 行协议响应行的 `ERR <code> [<detail>]` 全部分类）。
//!
//! 错误响应格式（逐平台对照 Go 源）：
//! - mac `helper/helper.go:406,436,459,...`：`fmt.Fprintln(conn, "ERR <code>")` 或 `fmt.Fprintf(conn, "ERR <code> %v\n", err)`。
//! - win `helper-win/helper.go:170,220,254,...`：同构（镜像 mac）。
//! - linux `helper-linux/helper.go:339,392,...`：`ERR <code>` 或 `ERR <code> (<hint>)`。
//!
//! [`ErrorCode`] 只枚举**确定前缀**（第一个 token）—— 尾部自由文本（如 `ERR start exit status 1`）保留在
//! [`Error::detail`]。解析见 [`Error::parse`]。

use crate::response::parse_first_token;

/// 错误码（wire 第一个 token，穷举自三平台 Go 源所有 `ERR <code>` 调用点）。
///
/// 命名遵循 Polaris 原文（kebab-case），改 = 协议破坏。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(clippy::module_name_repetitions)]
pub enum ErrorCode {
    /// `ERR auth` —— token 不匹配（mac/win，`helper.go:406` / `helper-win/helper.go:170`）。
    Auth,
    /// `ERR peercred` —— 取不到对端凭据（linux，`helper-linux/helper.go:339`）。
    Peercred,
    /// `ERR unauthorized` —— uid 不在授权列表（linux，`helper-linux/helper.go:355`）。
    Unauthorized,
    /// `ERR unknown` —— 未识别的命令（三平台 default 分支，`helper.go:587` 等）。
    Unknown,
    /// `ERR no-config` —— start 的 cfg 行为空（mac/win，`helper.go:526` / `helper-win/helper.go:350`）。
    NoConfig,
    /// `ERR bad-args` —— 参数校验失败（linux start cfg 空 `:412` / install-core wantHash 长度错 `:188`）。
    BadArgs,
    /// `ERR config-path-denied` —— cfg 不在 confDir 白名单内（mac/win，`helper.go:530` / `helper-win/helper.go:354`）。
    ConfigPathDenied,
    /// `ERR log-path-denied` —— start 的 log 路径越出白名单（**Polaris 新增，上游无对应**）。
    ///
    /// 上游三平台的 `log` 参数**零校验**：以特权身份 `O_CREATE|O_APPEND` 打开客户端下发的任意路径
    /// （linux 侧还额外 `fchown` 给对端 uid）⇒ 「root/SYSTEM 在任意位置建文件并把属主给你」。
    /// `cfg` 一直有白名单而 `log` 没有，是移植时被一起继承下来的缺口，非刻意取舍。
    LogPathDenied,
    /// `ERR core-path-denied` —— start 传入的核路径 != 锁定的 coreDir/sing-box（linux，`helper-linux/helper.go:418`）。
    CorePathDenied,
    /// `ERR config-not-owned` —— cfg 不属于对端 uid（linux，`helper-linux/helper.go:426`）。
    ConfigNotOwned,
    /// `ERR core-missing` —— 锁定核二进制不存在（linux，`helper-linux/helper.go:422`）。
    CoreMissing,
    /// `ERR iface-denied` —— route-add/del 的接口名不在白名单（mac/win，`helper.go:459` / `helper-win/helper.go:220`）。
    IfaceDenied,
    /// `ERR bad-gateway` —— default-restore 的网关非合法 IPv4（mac，`helper.go:487`）。
    BadGateway,
    /// `ERR bad-port` —— freeport 的端口非纯数字 / 越界（三平台，`helper.go:364` 等）。
    BadPort,
    /// `ERR bad-metric` —— iface-metric 的 metric 非法（win 退役命令，`helper-win/helper.go:259`）。
    BadMetric,
    /// `ERR coredir-unset` —— install-core 时 coreDir 未配置（mac/linux，`helper.go:135` / `helper-linux/helper.go:184`）。
    CoredirUnset,
    /// `ERR hash-mismatch` —— install-core 的 sha256 校验失败（mac/linux，`helper.go:146` / `helper-linux/helper.go:195`）。
    HashMismatch,
    /// `ERR enum` —— freeport 枚举 LISTEN 持有者失败（win，`helper-win/helper.go:310`）。
    Enum,
    /// `ERR start <err>` —— sing-box 启动失败（三平台，`helper.go:552` 等）。
    Start,
    /// `ERR dscacheutil <err>` —— flush-dns 的 dscacheutil 步失败（mac，`helper.go:499`）。
    Dscacheutil,
    /// `ERR set-metric <err>` —— iface-metric 的 PowerShell 失败（win 退役，`helper-win/helper.go:272`）。
    SetMetric,
    /// 其它 `ERR <token>` —— 未在此枚举的 code（解析到未知前缀时用，保留原 token 在 [`Error::detail`]）。
    ///
    /// 覆盖 `read-singbox`/`readdir`/`mkdir`/`write`/`rename`/`read`（install-core 各步的 OS 错误前缀，
    /// mac `helper.go:142-176` / linux `helper-linux/helper.go:192-228`）等瞬态/OS 错误——它们随 Go 源演进，
    /// 不锁死前缀，统一归 [`ErrorCode::Other`] + detail 保留原文。
    Other,
}

impl ErrorCode {
    /// 解析 wire 第一个 token 到枚举。未知 token 归 [`ErrorCode::Other`]（detail 保留原文，调用方据此诊断）。
    #[must_use]
    pub fn from_wire_token(token: &str) -> Self {
        match token {
            "auth" => Self::Auth,
            "peercred" => Self::Peercred,
            "unauthorized" => Self::Unauthorized,
            "unknown" => Self::Unknown,
            "no-config" => Self::NoConfig,
            "bad-args" => Self::BadArgs,
            "config-path-denied" => Self::ConfigPathDenied,
            "log-path-denied" => Self::LogPathDenied,
            "core-path-denied" => Self::CorePathDenied,
            "config-not-owned" => Self::ConfigNotOwned,
            "core-missing" => Self::CoreMissing,
            "iface-denied" => Self::IfaceDenied,
            "bad-gateway" => Self::BadGateway,
            "bad-port" => Self::BadPort,
            "bad-metric" => Self::BadMetric,
            "coredir-unset" => Self::CoredirUnset,
            "hash-mismatch" => Self::HashMismatch,
            "enum" => Self::Enum,
            "start" => Self::Start,
            "dscacheutil" => Self::Dscacheutil,
            "set-metric" => Self::SetMetric,
            _ => Self::Other,
        }
    }

    /// 序列化回 wire token。[`ErrorCode::Other`] 序列化为 `"unknown"`（最接近的语义兜底——
    /// 调用方构造错误响应时通常已知具体 code，不会序列化 Other）。
    #[must_use]
    pub const fn as_wire_token(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Peercred => "peercred",
            Self::Unauthorized => "unauthorized",
            Self::Unknown | Self::Other => "unknown",
            Self::NoConfig => "no-config",
            Self::BadArgs => "bad-args",
            Self::ConfigPathDenied => "config-path-denied",
            Self::LogPathDenied => "log-path-denied",
            Self::CorePathDenied => "core-path-denied",
            Self::ConfigNotOwned => "config-not-owned",
            Self::CoreMissing => "core-missing",
            Self::IfaceDenied => "iface-denied",
            Self::BadGateway => "bad-gateway",
            Self::BadPort => "bad-port",
            Self::BadMetric => "bad-metric",
            Self::CoredirUnset => "coredir-unset",
            Self::HashMismatch => "hash-mismatch",
            Self::Enum => "enum",
            Self::Start => "start",
            Self::Dscacheutil => "dscacheutil",
            Self::SetMetric => "set-metric",
        }
    }
}

/// 一个协议错误响应（`ERR <code>[ <detail>]`）。
///
/// `detail` 保留 Go 源 `fmt.Fprintf(conn, "ERR <code> %v\n", err)` 的尾部自由文本
/// （OS 错误信息、诊断提示等），用于真机排障。序列化/解析见 [`Error::to_wire_line`] / [`Error::parse`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// 错误分类（wire 第一个 token）。
    pub code: ErrorCode,
    /// 尾部自由文本（可空）。保留 Go 源 `fmt.Fprintf(conn, "ERR <code> %v\n", err)` 的 `%v` 部分。
    pub detail: String,
}

impl Error {
    /// 构造一个无 detail 的错误（对应 Go 源大多数 `fmt.Fprintln(conn, "ERR <code>")` 调用点）。
    #[must_use]
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            detail: String::new(),
        }
    }

    /// 构造一个带 detail 的错误（对应 Go 源 `fmt.Fprintf(conn, "ERR <code> %v\n", err)` 调用点）。
    #[must_use]
    pub fn with_detail(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// 序列化为 wire 行（不含尾部 `\n`，由帧层加）。
    ///
    /// 对应 Go：`fmt.Fprintln(conn, "ERR "+code)` 或 `fmt.Fprintf(conn, "ERR %s %s\n", code, detail.trimmed)`。
    /// detail 经 `trim()` —— 对齐 Go 源 `strings.TrimSpace(string(out))`（mac flush-dns `:499,503` 等）。
    ///
    /// 对 [`ErrorCode::Other`]：detail 保留完整原文（含未知 token，见 [`parse`](Self::parse)），
    /// 故输出 `ERR <detail>` 形态（不重复前缀已知 token）—— 保 round-trip 无损。
    #[must_use]
    pub fn to_wire_line(&self) -> String {
        let trimmed = self.detail.trim();
        if matches!(self.code, ErrorCode::Other) {
            // Other：detail 已含完整原文，直接拼（trimmed 为空时退化为 "ERR unknown"）
            if trimmed.is_empty() {
                return format!("ERR {}", self.code.as_wire_token());
            }
            return format!("ERR {trimmed}");
        }
        if trimmed.is_empty() {
            format!("ERR {}", self.code.as_wire_token())
        } else {
            format!("ERR {} {}", self.code.as_wire_token(), trimmed)
        }
    }

    /// 解析一个 `ERR ...` 响应行（已剥 `\n`）为 [`Error`]。
    ///
    /// 返回 `None` 当且仅当输入不以 `ERR ` 开头（非错误响应，调用方应交给 [`Response::parse`](crate::response::Response::parse)）。
    ///
    /// 对未知 code（[`ErrorCode::Other`]）：detail 保留**完整原文**（含未知 token），便于诊断；
    /// `to_wire_line` 对 Other 会输出 `ERR <detail>`（保 round-trip 无损）。
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let rest = line.strip_prefix("ERR ")?;
        let (first, detail) = parse_first_token(rest);
        let code = ErrorCode::from_wire_token(first);
        // Other：detail 保留完整原文（token + 余部），无损 round-trip；已知 code：detail 仅留尾部自由文本。
        let detail = if matches!(code, ErrorCode::Other) {
            rest.to_owned()
        } else {
            detail.to_owned()
        };
        Some(Self { code, detail })
    }
}

/// [`Error`] 本身满足 [`ResponseKind`](crate::response::ResponseKind) 的语义（它是 Response::Err 的载荷）。
/// 显式 Display 走 wire 行（便于 helper 侧 `write!(conn, "{}", err)` 等价 Go `Fprintln`）。
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_wire_line())
    }
}

impl std::error::Error for Error {}

/// 从任意 [`std::error::Error`] 构造 `ERR unknown <e>` —— helper 侧兜底用（对应 Go `default: ERR unknown`）。
impl From<Box<dyn std::error::Error + Send + Sync>> for Error {
    fn from(e: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self::with_detail(ErrorCode::Unknown, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn err_roundtrip_no_detail() {
        // 对应 Go: fmt.Fprintln(conn, "ERR auth") —— helper.go:406
        let e = Error::new(ErrorCode::Auth);
        assert_eq!(e.to_wire_line(), "ERR auth");
        let parsed = Error::parse("ERR auth").unwrap();
        assert_eq!(parsed, e);
    }

    #[test]
    fn err_roundtrip_with_detail() {
        // 对应 Go: fmt.Fprintf(conn, "ERR start %v\n", err) —— helper.go:552
        let e = Error::with_detail(ErrorCode::Start, "exit status 1");
        assert_eq!(e.to_wire_line(), "ERR start exit status 1");
        let parsed = Error::parse("ERR start exit status 1").unwrap();
        assert_eq!(parsed.code, ErrorCode::Start);
        assert_eq!(parsed.detail, "exit status 1");
    }

    #[test]
    fn err_detail_trimmed_on_serialize() {
        // 对齐 Go strings.TrimSpace(string(out))：尾部空白不应进 wire
        let e = Error::with_detail(ErrorCode::Dscacheutil, "  flusing failed  \n");
        assert_eq!(e.to_wire_line(), "ERR dscacheutil flusing failed");
    }

    #[test]
    fn err_parse_unknown_token_falls_back_to_other() {
        // install-core 的 OS 错误前缀（read-singbox/readdir 等）未锁死进枚举 → Other + detail 保留完整原文
        let parsed = Error::parse("ERR read-singbox open /tmp/x: no such file").unwrap();
        assert_eq!(parsed.code, ErrorCode::Other);
        // Other 的 detail 保留完整原文（含未知 token），保 round-trip 无损
        assert_eq!(parsed.detail, "read-singbox open /tmp/x: no such file");
        // round-trip：to_wire_line 应重建原行
        assert_eq!(
            parsed.to_wire_line(),
            "ERR read-singbox open /tmp/x: no such file"
        );
    }

    #[test]
    fn err_parse_non_err_returns_none() {
        assert!(Error::parse("OK pong uid=0 v9").is_none());
        assert!(Error::parse("").is_none());
    }

    #[test]
    fn all_known_codes_roundtrip_through_wire_token() {
        // 锁住 wire token 不漂移 —— 改名 = 与已部署 helper 断协议
        let known = [
            ErrorCode::Auth,
            ErrorCode::Peercred,
            ErrorCode::Unauthorized,
            ErrorCode::Unknown,
            ErrorCode::NoConfig,
            ErrorCode::BadArgs,
            ErrorCode::ConfigPathDenied,
            ErrorCode::LogPathDenied,
            ErrorCode::CorePathDenied,
            ErrorCode::ConfigNotOwned,
            ErrorCode::CoreMissing,
            ErrorCode::IfaceDenied,
            ErrorCode::BadGateway,
            ErrorCode::BadPort,
            ErrorCode::BadMetric,
            ErrorCode::CoredirUnset,
            ErrorCode::HashMismatch,
            ErrorCode::Enum,
            ErrorCode::Start,
            ErrorCode::Dscacheutil,
            ErrorCode::SetMetric,
        ];
        for c in known {
            let tok = c.as_wire_token();
            assert_eq!(ErrorCode::from_wire_token(tok), c, "token {tok} mismatch");
            // 其它 -> 序列化为 "unknown"（兜底，调用方构造时通常已知具体 code）
            assert_eq!(ErrorCode::from_wire_token("read-singbox"), ErrorCode::Other);
            let _ = tok; // suppress unused in case of empty
        }
    }
}
