//! 协议响应（Polaris 行协议响应行 `OK <payload>` / `ERR <code>[ <detail>]` 的全部载荷分类）。
//!
//! 响应一律单行（`\n` 结尾由帧层加），首 token 决定分类：
//! - `OK` 开头 → 成功，载荷格式由命令决定（见 [`Response`] 各变体）。
//! - `ERR` 开头 → 失败，见 [`crate::error::Error`]。
//!
//! 逐平台对照（Go 源每个 `fmt.Fprintln(conn, "OK ...")` / `fmt.Fprintf(conn, "OK ...\n", ...)` 调用点）：
//! - mac `helper/helper.go:423,425,428,430,441,443,449,480,491,506,522,579,585,597`
//! - win `helper-win/helper.go:180,182,185,187,197,199,211,241,275,289,314,333,336,390`
//! - linux `helper-linux/helper.go:347,350,365,367,377,379,388,478`
//!
//! [`Response::parse`] 是宽容的：未知 `OK <token>` 归 [`Response::OkRaw`]（保留原文），不丢消息。

use crate::error::Error;

/// 拆出字符串首个空白分隔的 token + 余部（已 trim 首尾空白）。
///
/// Go 源用 `strings.Fields` / `strings.SplitN(line, " ", 2)` 拆响应载荷；本函数是等价原语，供
/// [`Response::parse`] / [`Error::parse`](crate::error::Error::parse) 共用。空串或全空白 → `("", "")`。
#[must_use]
pub(crate) fn parse_first_token(s: &str) -> (&str, &str) {
    let trimmed = s.trim();
    match trimmed.find(char::is_whitespace) {
        Some(i) => (&trimmed[..i], trimmed[i..].trim_start()),
        None => (trimmed, ""),
    }
}

/// `ping` 响应载荷 `OK pong uid=<n> v<ver>`（三平台，`helper.go:423` / `helper-win/helper.go:180` /
/// `helper-linux/helper.go:347`）。Windows 的 uid 恒为 0（`helper-win/helper.go:179` 注释）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pong {
    /// helper 进程的 uid（mac/linux 真实值；win 固定 0）。
    pub uid: i64,
    /// helper 报告的 protoVersion。
    pub proto_version: u32,
}

/// `status` 响应载荷（三平台，`helper.go:427-430` 等）：running <pid> 或 stopped。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// `OK running <pid>` —— child sing-box 在跑。
    Running { pid: u32 },
    /// `OK stopped` —— 无 child。
    Stopped,
}

/// `stop` 响应载荷（三平台，`helper.go:440-442` 等）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// `OK stopped <pid>` —— 摘除并后台收割了一个在跑的 child。
    Stopped { pid: u32 },
    /// `OK notrunning` —— 本来就没 child（幂等）。
    NotRunning,
    /// `OK stop-mismatch <want> <current>` —— **诚实 no-op**：请求要停 `want`，但 helper 手里的受管
    /// 核是 `current`（另一个会话的）。不杀，原样回报两个 pid 供客户端记账/日志。
    ///
    /// 判据见 [`stop_pid_matches`](crate::request::stop_pid_matches)。旧客户端不发身份行 ⇒
    /// `want` 恒 `None` ⇒ 永不产生本响应，故新 helper + 旧客户端不受影响。
    Mismatch { want: u32, current: u32 },
}

/// `start` 响应载荷（三平台，`helper.go:522,579` 等）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// `OK started <pid>` —— 新起了 child sing-box。
    Started { pid: u32 },
    /// `OK already <pid>` —— 已有 child 在跑，复用（不重启）。
    Already { pid: u32 },
}

/// `freeport` 响应载荷（三平台，`helper.go:370-394` 等）。`foreign` 的 names 由 ` | ` 分隔（Go `strings.Join(foreign, " | ")`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreePort {
    /// `OK free` —— 端口无 LISTEN 持有者。
    Free,
    /// `OK killed <pid>,<pid>,...` —— 杀掉的 sing-box 实例 pid 列表（Go `strings.Join(killed, ",")`）。
    Killed { pids: Vec<u32> },
    /// `OK foreign <name> | <name>` —— 端口被非 sing-box 进程占用，未杀、回报占用者名（混合占用亦归此）。
    Foreign { names: Vec<String> },
}

/// `flush-dns` 响应载荷（mac 专属，`helper/helper.go:498-506`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushDns {
    /// `OK flushed` —— dscacheutil + HUP mDNSResponder 均成功。
    Flushed,
    /// `OK flushed-partial killall-hup <err> <out>` —— dscacheutil 成功但 HUP mDNSResponder 失败
    ///（用户级同样无权 HUP，app 不降级）。tail 保留 Go `:503` 的 `killall-hup <err> <out>` 诊断文本。
    FlushedPartial { tail: String },
}

/// 协议成功响应的全部载荷分类（一个请求一个响应，类型由请求命令决定）。
///
/// 设计原则：每个变体对应一组 Go `case` 分支的 `OK ...` 输出；未识别的 `OK <token>` 归
/// [`ResponseKind::OkRaw`]（保留原文）——保证解析永不丢消息，便于协议演进期的诊断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseKind {
    /// `OK pong uid=<n> v<ver>`（ping）。
    Pong(Pong),
    /// `OK <ver>`（version）。
    Version { proto_version: u32 },
    /// `OK running <pid>` / `OK stopped`（status）。
    Status(Status),
    /// `OK stopped <pid>` / `OK notrunning`（stop）。
    Stop(Stop),
    /// `OK started <pid>` / `OK already <pid>`（start）。
    Start(Start),
    /// `OK cleaned`（cleanup）。
    Cleaned,
    /// `OK route`（route-add / route-del）。
    Route,
    /// `OK free` / `OK killed <pids>` / `OK foreign <names>`（freeport）。
    FreePort(FreePort),
    /// `OK installed`（install-core，mac/linux）。
    Installed,
    /// `OK default-restore`（default-restore，mac）。
    DefaultRestored,
    /// `OK flushed` / `OK flushed-partial ...`（flush-dns，mac）。
    FlushDns(FlushDns),
    /// `OK iface-metric`（iface-metric，win 退役命令）。
    IfaceMetric,
    /// `OK uninstalling`（uninstall，win）。
    Uninstalling,
    /// 未识别的 `OK <token> <rest>` —— 保留原文，永不丢消息（协议演进期诊断兜底）。
    OkRaw { token: String, rest: String },
}

/// 一个完整响应（成功或失败）。
///
/// 对应 Go 源每个 `fmt.Fprintln(conn, "...")` 调用点产生的单行。`Err` 变体承载 [`Error`]，
/// 与 [`ResponseKind`] 各成功变体互补。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// 成功响应（载荷分类见 [`ResponseKind`]）。
    Ok(ResponseKind),
    /// 失败响应（错误码 + detail）。
    Err(Error),
}

impl Response {
    /// 解析一个响应行（已剥 `\n`）为 [`Response`]。
    ///
    /// - `OK ...` → [`Response::Ok`]，载荷按首 token 分派。
    /// - `ERR ...` → [`Response::Err`]（见 [`Error::parse`](crate::error::Error::parse)）。
    /// - 其它 → [`Response::Ok`](`OkRaw`)，token 为原文（永不丢消息，便于诊断畸形/未来协议响应）。
    #[must_use]
    pub fn parse(line: &str) -> Self {
        if let Some(err) = Error::parse(line) {
            return Self::Err(err);
        }
        let rest = match line.strip_prefix("OK") {
            // 严格对照 Go：所有响应形如 "OK ..." 或 "OK\n"（无 payload 的极少，如未识别命令的兜底）。
            // strip_prefix("OK") 后若紧跟空格 → 取余部；若 "OK" 恰是整行 → rest 为空（归 OkRaw token="")。
            Some(r) => r.trim_start(),
            None => {
                // 既非 OK 也非 ERR —— 视作未知成功响应，保留原文兜底（不丢消息）。
                let (token, rest) = parse_first_token(line);
                return Self::Ok(ResponseKind::OkRaw {
                    token: token.to_owned(),
                    rest: rest.to_owned(),
                });
            }
        };
        Self::Ok(parse_ok(rest))
    }

    /// 序列化为 wire 行（不含尾部 `\n`，由帧层加）—— [`Response::parse`] 的反方向。
    ///
    /// 对应 Go 源每个 `fmt.Fprintln(conn, "OK ...")` / `fmt.Fprintf(conn, "OK ...\n", ...)` 调用点
    /// （逐平台调用点清单见本模块顶部注释）。**三平台 helper 共用本函数**：读/写两个方向在同一文件里
    /// 成对定义，协议演进时不会只改一侧（原先 mac/win 各持一份副本、linux 内联 `format!`，
    /// 是漏改的温床）。
    ///
    /// 与 [`parse`](Self::parse) 满足 round-trip：`parse(r.to_wire_line()) == r`
    /// （`OkRaw` 的空 `rest` 不产尾空格，即为此）。
    #[must_use]
    pub fn to_wire_line(&self) -> String {
        match self {
            Self::Ok(kind) => ok_kind_to_wire(kind),
            Self::Err(e) => e.to_wire_line(),
        }
    }
}

/// 把 [`ResponseKind`] 序列化为 `OK ...` wire 行（[`parse_ok`] 的反方向）。
fn ok_kind_to_wire(kind: &ResponseKind) -> String {
    match kind {
        ResponseKind::Pong(p) => format!("OK pong uid={} v{}", p.uid, p.proto_version),
        ResponseKind::Version { proto_version } => format!("OK {proto_version}"),
        ResponseKind::Status(Status::Running { pid }) => format!("OK running {pid}"),
        ResponseKind::Status(Status::Stopped) => "OK stopped".to_owned(),
        ResponseKind::Stop(Stop::Stopped { pid }) => format!("OK stopped {pid}"),
        ResponseKind::Stop(Stop::NotRunning) => "OK notrunning".to_owned(),
        ResponseKind::Stop(Stop::Mismatch { want, current }) => {
            format!("OK stop-mismatch {want} {current}")
        }
        ResponseKind::Start(Start::Started { pid }) => format!("OK started {pid}"),
        ResponseKind::Start(Start::Already { pid }) => format!("OK already {pid}"),
        ResponseKind::Cleaned => "OK cleaned".to_owned(),
        ResponseKind::Route => "OK route".to_owned(),
        ResponseKind::FreePort(fp) => free_port_to_wire(fp),
        ResponseKind::Installed => "OK installed".to_owned(),
        ResponseKind::DefaultRestored => "OK default-restore".to_owned(),
        ResponseKind::FlushDns(f) => flush_dns_to_wire(f),
        ResponseKind::IfaceMetric => "OK iface-metric".to_owned(),
        ResponseKind::Uninstalling => "OK uninstalling".to_owned(),
        // 空 rest 不产尾空格 —— 保 round-trip（`OK <token> ` 会被 parse 回 rest=""，但原文多一空格）。
        ResponseKind::OkRaw { token, rest } => {
            if rest.is_empty() {
                format!("OK {token}")
            } else {
                format!("OK {token} {rest}")
            }
        }
    }
}

/// `freeport` 载荷 → wire（`OK free` / `OK killed <pids>` / `OK foreign <names>`）。
///
/// 分隔符逐字对齐 Go：killed 用 `strings.Join(killed, ",")`，foreign 用 `strings.Join(foreign, " | ")`。
fn free_port_to_wire(fp: &FreePort) -> String {
    match fp {
        FreePort::Free => "OK free".to_owned(),
        FreePort::Killed { pids } => {
            let joined: Vec<String> = pids.iter().map(ToString::to_string).collect();
            format!("OK killed {}", joined.join(","))
        }
        FreePort::Foreign { names } => format!("OK foreign {}", names.join(" | ")),
    }
}

/// `flush-dns` 载荷 → wire（mac 专属，`helper/helper.go:498-506`）。
fn flush_dns_to_wire(f: &FlushDns) -> String {
    match f {
        FlushDns::Flushed => "OK flushed".to_owned(),
        FlushDns::FlushedPartial { tail } => format!("OK flushed-partial {tail}"),
    }
}

/// 分派 `OK <rest>` 的载荷（rest 已剥 "OK " 前缀，可能为空）。
fn parse_ok(rest: &str) -> ResponseKind {
    let (token, tail) = parse_first_token(rest);
    match token {
        "" => ResponseKind::OkRaw {
            token: String::new(),
            rest: String::new(),
        },
        "pong" => ResponseKind::Pong(parse_pong(tail)),
        "version" => ResponseKind::Version { proto_version: 0 }, // version 响应是 "OK <ver>"，ver 是首 token
        // 注意：version 响应 `OK 9` 中 "9" 是首 token，非 "version" —— 上方 "version" 分支永远不命中，
        // 真实 version 响应走下方纯数字兜底。这里保留分支以防未来协议加 "OK version <ver>" 形态。
        "running" => ResponseKind::Status(Status::Running {
            pid: parse_pid(tail),
        }),
        "stopped" => {
            // `OK stopped` 或 `OK stopped <pid>`（后者是 stop 响应，前者是 status 响应）
            if tail.trim().is_empty() {
                ResponseKind::Status(Status::Stopped)
            } else {
                ResponseKind::Stop(Stop::Stopped {
                    pid: parse_pid(tail),
                })
            }
        }
        "notrunning" => ResponseKind::Stop(Stop::NotRunning),
        "stop-mismatch" => {
            // `<want> <current>`：解析失败归 0（不丢消息，客户端据此诊断；与 parse_pid 同处置）。
            let (want, rest) = parse_first_token(tail);
            ResponseKind::Stop(Stop::Mismatch {
                want: want.parse().unwrap_or(0),
                current: parse_pid(rest),
            })
        }
        "started" => ResponseKind::Start(Start::Started {
            pid: parse_pid(tail),
        }),
        "already" => ResponseKind::Start(Start::Already {
            pid: parse_pid(tail),
        }),
        "cleaned" => ResponseKind::Cleaned,
        "route" => ResponseKind::Route,
        "free" => ResponseKind::FreePort(FreePort::Free),
        "killed" => ResponseKind::FreePort(FreePort::Killed {
            pids: parse_pid_list(tail),
        }),
        "foreign" => ResponseKind::FreePort(FreePort::Foreign {
            names: parse_foreign_names(tail),
        }),
        "installed" => ResponseKind::Installed,
        "default-restore" => ResponseKind::DefaultRestored,
        "flushed" => ResponseKind::FlushDns(FlushDns::Flushed),
        "flushed-partial" => ResponseKind::FlushDns(FlushDns::FlushedPartial {
            tail: tail.to_owned(),
        }),
        "iface-metric" => ResponseKind::IfaceMetric,
        "uninstalling" => ResponseKind::Uninstalling,
        _ => {
            // version 响应 `OK 9`：首 token 是纯数字 → 当作 version 报告
            if token.chars().all(|c| c.is_ascii_digit()) {
                ResponseKind::Version {
                    proto_version: token.parse().unwrap_or(0),
                }
            } else {
                ResponseKind::OkRaw {
                    token: token.to_owned(),
                    rest: tail.to_owned(),
                }
            }
        }
    }
}

/// 解析 `uid=<n> v<ver>` → [`Pong`]。
///
/// 对照 Go wire 形态：`fmt.Fprintf(conn, "OK pong uid=%d v%s\n", os.Getuid(), protoVersion)`
///（`helper.go:423` / `helper-linux/helper.go:347` / `helper-win/helper.go:180`）。注意 `%s` 直接接 protoVersion
/// 常量值（如 "9"），故 wire 形态是 `uid=0 v9`（`v` 后无 `=`，ver 是独立 token）。
fn parse_pong(tail: &str) -> Pong {
    let mut uid: i64 = 0;
    let mut ver: u32 = 0;
    for field in tail.split_whitespace() {
        if let Some(v) = field.strip_prefix("uid=") {
            uid = v.parse().unwrap_or(0);
        } else if let Some(v) = field.strip_prefix('v') {
            // `v9` 形态：v 后直接跟版本号数字（无 `=`）
            ver = v.parse().unwrap_or(0);
        }
    }
    Pong {
        uid,
        proto_version: ver,
    }
}

/// 解析 `<pid>` → u32（失败归 0；调用方据此诊断，不丢响应）。
fn parse_pid(tail: &str) -> u32 {
    let (tok, _) = parse_first_token(tail);
    tok.parse().unwrap_or(0)
}

/// 解析 `pid,pid,pid` → Vec<u32>（Go `strings.Join(killed, ",")` 的逆）。
fn parse_pid_list(tail: &str) -> Vec<u32> {
    tail.split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

/// 解析 `name | name | name` → Vec<String>（Go `strings.Join(foreign, " | ")` 的逆）。
fn parse_foreign_names(tail: &str) -> Vec<String> {
    tail.split('|')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn parse_pong_mac_linux() {
        // helper.go:423: fmt.Fprintf(conn, "OK pong uid=%d v%s\n", os.Getuid(), protoVersion)
        let r = Response::parse("OK pong uid=0 v9");
        let Response::Ok(ResponseKind::Pong(p)) = r else {
            panic!("{r:?}");
        };
        assert_eq!(
            p,
            Pong {
                uid: 0,
                proto_version: 9
            }
        );
    }

    #[test]
    fn parse_pong_win_uid_zero() {
        // helper-win/helper.go:180: 固定发 uid=0（os.Getuid() 在 Windows 返回 -1 会破坏客户端正则）
        let r = Response::parse("OK pong uid=0 v5");
        let Response::Ok(ResponseKind::Pong(p)) = r else {
            panic!("{r:?}");
        };
        assert_eq!(p.uid, 0);
        assert_eq!(p.proto_version, 5);
    }

    #[test]
    fn parse_version_response() {
        // helper.go:425: fmt.Fprintf(conn, "OK %s\n", protoVersion) —— 注意首 token 是版本号本身
        for (line, want) in [("OK 9", 9u32), ("OK 5", 5), ("OK 1", 1)] {
            let r = Response::parse(line);
            let Response::Ok(ResponseKind::Version { proto_version }) = r else {
                panic!("{r:?}");
            };
            assert_eq!(proto_version, want);
        }
    }

    #[test]
    fn parse_status_running_stopped() {
        // helper.go:427-430
        let r = Response::parse("OK running 12345");
        let Response::Ok(ResponseKind::Status(Status::Running { pid })) = r else {
            panic!("{r:?}");
        };
        assert_eq!(pid, 12345);

        let r = Response::parse("OK stopped");
        let Response::Ok(ResponseKind::Status(Status::Stopped)) = r else {
            panic!("{r:?}");
        };
    }

    #[test]
    fn parse_stop_stopped_notrunning() {
        // helper.go:440-442
        let r = Response::parse("OK stopped 12345"); // stop 的 stopped 带 pid
        let Response::Ok(ResponseKind::Stop(Stop::Stopped { pid })) = r else {
            panic!("{r:?}");
        };
        assert_eq!(pid, 12345);

        let r = Response::parse("OK notrunning");
        let Response::Ok(ResponseKind::Stop(Stop::NotRunning)) = r else {
            panic!("{r:?}");
        };
    }

    /// `stop-mismatch` 的解析 + 序列化 round-trip（诚实 no-op 的 wire 形态）。
    ///
    /// 两个 pid 都必须原样带回来：客户端的日志/记账靠它们才说得清「谁被拒了、daemon 手里的是谁」。
    #[test]
    fn stop_mismatch_round_trips_with_both_pids() {
        let r = Response::parse("OK stop-mismatch 4242 9001");
        let Response::Ok(ResponseKind::Stop(Stop::Mismatch { want, current })) = r else {
            panic!("{r:?}");
        };
        assert_eq!((want, current), (4242, 9001));
        assert_eq!(
            Response::Ok(ResponseKind::Stop(Stop::Mismatch {
                want: 4242,
                current: 9001
            }))
            .to_wire_line(),
            "OK stop-mismatch 4242 9001"
        );
    }

    #[test]
    fn parse_start_started_already() {
        // helper.go:522,579
        let r = Response::parse("OK started 12345");
        let Response::Ok(ResponseKind::Start(Start::Started { pid })) = r else {
            panic!("{r:?}");
        };
        assert_eq!(pid, 12345);

        let r = Response::parse("OK already 12345");
        let Response::Ok(ResponseKind::Start(Start::Already { pid })) = r else {
            panic!("{r:?}");
        };
        assert_eq!(pid, 12345);
    }

    #[test]
    fn parse_freeport_variants() {
        // helper.go:370,391,393
        let Response::Ok(ResponseKind::FreePort(FreePort::Free)) = Response::parse("OK free")
        else {
            panic!();
        };

        let r = Response::parse("OK killed 123,456");
        let Response::Ok(ResponseKind::FreePort(FreePort::Killed { pids })) = r else {
            panic!("{r:?}");
        };
        assert_eq!(pids, vec![123, 456]);

        let r = Response::parse("OK foreign nginx | pid:789");
        let Response::Ok(ResponseKind::FreePort(FreePort::Foreign { names })) = r else {
            panic!("{r:?}");
        };
        assert_eq!(names, vec!["nginx".to_owned(), "pid:789".to_owned()]);
    }

    #[test]
    fn parse_flush_dns_mac() {
        // helper/helper.go:503,506
        let Response::Ok(ResponseKind::FlushDns(FlushDns::Flushed)) = Response::parse("OK flushed")
        else {
            panic!();
        };

        let r = Response::parse("OK flushed-partial killall-hup exit status 1 ");
        let Response::Ok(ResponseKind::FlushDns(FlushDns::FlushedPartial { tail })) = r else {
            panic!("{r:?}");
        };
        assert!(tail.contains("killall-hup"));
    }

    #[test]
    fn parse_simple_ok_tokens() {
        assert_eq!(
            Response::parse("OK cleaned"),
            Response::Ok(ResponseKind::Cleaned)
        );
        assert_eq!(
            Response::parse("OK route"),
            Response::Ok(ResponseKind::Route)
        );
        assert_eq!(
            Response::parse("OK installed"),
            Response::Ok(ResponseKind::Installed)
        );
        assert_eq!(
            Response::parse("OK default-restore"),
            Response::Ok(ResponseKind::DefaultRestored)
        );
        assert_eq!(
            Response::parse("OK iface-metric"),
            Response::Ok(ResponseKind::IfaceMetric)
        );
        assert_eq!(
            Response::parse("OK uninstalling"),
            Response::Ok(ResponseKind::Uninstalling)
        );
    }

    #[test]
    fn parse_err_routes_to_err_variant() {
        let r = Response::parse("ERR auth");
        let Response::Err(e) = r else {
            panic!("{r:?}");
        };
        assert_eq!(e.code, ErrorCode::Auth);
    }

    #[test]
    fn parse_unknown_ok_token_kept_as_raw() {
        // 协议演进兜底：未来加 "OK new-capability X" 不应丢消息
        let r = Response::parse("OK new-capability payload here");
        let Response::Ok(ResponseKind::OkRaw { token, rest }) = r else {
            panic!("{r:?}");
        };
        assert_eq!(token, "new-capability");
        assert_eq!(rest, "payload here");
    }

    #[test]
    fn parse_empty_line_no_panic() {
        // 畸形输入兜底：绝不 panic（freeport 任何持 token 用户可触发）
        let _ = Response::parse("");
        let _ = Response::parse("OK");
        let _ = Response::parse("garbage no prefix");
    }

    // ===== to_wire_line（写方向，G3.1 上提自 mac handler.rs / win service/win.rs）=====

    /// 逐字锁定 wire 输出 —— 对照 Go 源各 `fmt.Fprintln(conn, "OK ...")` 调用点。
    /// 这些字符串是三平台 helper 与 app 的协议契约，改动即破网。
    #[test]
    fn to_wire_line_matches_go_source_literals() {
        let cases: &[(Response, &str)] = &[
            (
                Response::Ok(ResponseKind::Pong(Pong {
                    uid: 501,
                    proto_version: 9,
                })),
                "OK pong uid=501 v9",
            ),
            (
                Response::Ok(ResponseKind::Version { proto_version: 9 }),
                "OK 9",
            ),
            (
                Response::Ok(ResponseKind::Status(Status::Running { pid: 123 })),
                "OK running 123",
            ),
            (
                Response::Ok(ResponseKind::Status(Status::Stopped)),
                "OK stopped",
            ),
            (
                Response::Ok(ResponseKind::Stop(Stop::Stopped { pid: 456 })),
                "OK stopped 456",
            ),
            (
                Response::Ok(ResponseKind::Stop(Stop::NotRunning)),
                "OK notrunning",
            ),
            (
                Response::Ok(ResponseKind::Start(Start::Started { pid: 7 })),
                "OK started 7",
            ),
            (
                Response::Ok(ResponseKind::Start(Start::Already { pid: 8 })),
                "OK already 8",
            ),
            (Response::Ok(ResponseKind::Cleaned), "OK cleaned"),
            (Response::Ok(ResponseKind::Route), "OK route"),
            (
                Response::Ok(ResponseKind::FreePort(FreePort::Free)),
                "OK free",
            ),
            (
                Response::Ok(ResponseKind::FreePort(FreePort::Killed {
                    pids: vec![1, 22, 333],
                })),
                "OK killed 1,22,333",
            ),
            (
                Response::Ok(ResponseKind::FreePort(FreePort::Foreign {
                    names: vec!["nginx".into(), "pid:42".into()],
                })),
                "OK foreign nginx | pid:42",
            ),
            (Response::Ok(ResponseKind::Installed), "OK installed"),
            (
                Response::Ok(ResponseKind::DefaultRestored),
                "OK default-restore",
            ),
            (
                Response::Ok(ResponseKind::FlushDns(FlushDns::Flushed)),
                "OK flushed",
            ),
            (
                Response::Ok(ResponseKind::FlushDns(FlushDns::FlushedPartial {
                    tail: "killall-hup err out".into(),
                })),
                "OK flushed-partial killall-hup err out",
            ),
            (Response::Ok(ResponseKind::IfaceMetric), "OK iface-metric"),
            (Response::Ok(ResponseKind::Uninstalling), "OK uninstalling"),
            (
                Response::Err(Error::new(ErrorCode::BadPort)),
                "ERR bad-port",
            ),
        ];
        for (resp, want) in cases {
            assert_eq!(&resp.to_wire_line(), want, "wire mismatch for {resp:?}");
        }
    }

    /// 写→读 round-trip：`parse(to_wire_line(r)) == r`，含 OkRaw 空 rest（不产尾空格）。
    #[test]
    fn to_wire_line_round_trips_through_parse() {
        let cases = vec![
            Response::Ok(ResponseKind::Pong(Pong {
                uid: 0,
                proto_version: 3,
            })),
            Response::Ok(ResponseKind::Status(Status::Running { pid: 999 })),
            Response::Ok(ResponseKind::Status(Status::Stopped)),
            Response::Ok(ResponseKind::Stop(Stop::Stopped { pid: 1 })),
            Response::Ok(ResponseKind::Stop(Stop::NotRunning)),
            Response::Ok(ResponseKind::Stop(Stop::Mismatch {
                want: 4242,
                current: 9001,
            })),
            Response::Ok(ResponseKind::Start(Start::Started { pid: 2 })),
            Response::Ok(ResponseKind::Start(Start::Already { pid: 3 })),
            Response::Ok(ResponseKind::Cleaned),
            Response::Ok(ResponseKind::Route),
            Response::Ok(ResponseKind::FreePort(FreePort::Free)),
            Response::Ok(ResponseKind::FreePort(FreePort::Killed {
                pids: vec![5, 6],
            })),
            Response::Ok(ResponseKind::FreePort(FreePort::Foreign {
                names: vec!["a".into(), "b".into()],
            })),
            Response::Ok(ResponseKind::Installed),
            Response::Ok(ResponseKind::DefaultRestored),
            Response::Ok(ResponseKind::FlushDns(FlushDns::Flushed)),
            Response::Ok(ResponseKind::IfaceMetric),
            Response::Ok(ResponseKind::Uninstalling),
            // OkRaw 空 rest：`OK foo`（无尾空格）→ parse 回 rest=""
            Response::Ok(ResponseKind::OkRaw {
                token: "foo".into(),
                rest: String::new(),
            }),
            Response::Ok(ResponseKind::OkRaw {
                token: "new-capability".into(),
                rest: "payload here".into(),
            }),
        ];
        for r in cases {
            assert_eq!(Response::parse(&r.to_wire_line()), r, "round-trip failed");
        }
    }
}
