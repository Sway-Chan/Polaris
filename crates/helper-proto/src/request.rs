//! 协议请求（Polaris 行协议请求侧的全部分类）。
//!
//! 请求帧结构（逐平台对照 Go 源 `handle()`/`readLine(r)` 序列）：
//!
//! **macOS**（`helper/helper.go:403-585`）：
//! ```text
//! 行1: <token>
//! 行2: <command>
//! 行3..: <命令特定参数行>（每行 readLine，含空格的路径整行传递）
//! ```
//!
//! **Windows**（`helper-win/helper.go:167-393`）：同 mac（命名管道 + token 行）。
//!
//! **Linux**（`helper-linux/helper.go:333-482`）：
//! ```text
//! （无 token 行 —— SO_PEERCRED 内核背书对端 uid）
//! 行1: <command>
//! 行2..: <命令特定参数行>
//! ```
//!
//! [`Request`] 枚举每个变体的字段严格对应 Go 源某 `case` 分支读取的参数行。序列化见
//! [`Request::write_args`]（写命令后的参数行，不含 token/command 行——那两行由帧层
//! [`crate::codec::Framer`] 按 [`crate::Platform`] 决定是否加 token 行）。

use crate::command;

/// `start` 命令的参数（三平台同构，`helper.go:508-513` 等）。
///
/// 行序（Go `readLine` 顺序）：
/// - mac/win：`cfg` / `log` / `fwd` / `ppid`（行3-6）
/// - linux：`singbox` / `cfg` / `log` / `fwd` / `ppid`（行2-6，多一个核路径行）
///
/// `ppid` 缺失/空 = 不启父死看护（兼容旧客户端，Go `strconv.Atoi("")` → 0）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartParams {
    /// 配置文件路径（必须在 confDir 白名单内，mac/win `cfgAllowed`）。
    pub cfg: String,
    /// 日志文件路径（sing-box 早期 stdout/stderr 重定向到此；可空）。
    pub log: String,
    /// allowLan 转发开关（`"0"` / `"1"`，按字符串下发对齐 Go `fwd == "1"`）。
    pub fwd: bool,
    /// 父 app PID（父死看护用；`None` = 不启看护，对应 Go `ppid <= 0` 分支）。
    pub parent_pid: Option<u32>,
}

/// Linux `start` 多一个核路径行（客户端传的 sing-box 路径，必须 == 锁定的 coreDir/sing-box，
/// `helper-linux/helper.go:401,417-420`）。封装为独立字段以便 mac/win 不带它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxStartParams {
    /// 客户端指定的核路径（helper 校验 == coreBin()，否则 `ERR core-path-denied`）。
    pub singbox_path: String,
    /// 共用 start 参数。
    pub common: StartParams,
}

/// `route-add` / `route-del` 的参数（mac/win，`helper.go:455-456` / `helper-win/helper.go:216-217`）。
///
/// `cidrs` 以逗号分隔下发（Go `strings.Split(cidrsLine, ",")`），每项须过 `net.ParseCIDR` 校验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteParams {
    /// 内核接口名（须过平台 iface 白名单：mac `ifaceAllowed` 允许 polaris-ts/polaris-wg/utunN，
    /// win 允许 polaris-* 前缀）。
    pub iface: String,
    /// CIDR 列表（IPv4 或 IPv6，Go 按 `strings.Contains(c, ":")` 选族）。
    pub cidrs: Vec<String>,
}

/// `install-core` 的参数（mac/linux，`helper.go:583-584` / `helper-linux/helper.go:397-398`）。
///
/// `want_hash` 为 64 字符 hex sha256（Go `len(wantHash) != 64` 校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallCoreParams {
    /// 临时核源目录（app 下载+预检后的用户可写区）。
    pub src_dir: String,
    /// 期望的 sing-box sha256（hex，64 字符）。
    pub want_hash: String,
}

/// 协议请求的全部分类。一个变体对应一个 wire 命令（[`command`] 常量）。
///
/// 序列化纪律：[`Request::write_args`] 只写命令后的参数行。token 行（mac/win）与命令行由
/// 帧层 [`crate::codec::Framer`] 统一加 —— 这样 Request 与鉴权机制解耦，三平台共用同一枚举。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `ping`（无参数行）。
    Ping,
    /// `version`（无参数行）。
    Version,
    /// `status`（无参数行）。
    Status,
    /// `stop [<pid>]`（受管 pid 身份行可选，见 [`stop_pid_matches`]）。
    ///
    /// `pid: Some(p)` = 「只停 p 这个受管核」；`pid: None` = 旧语义「停你当前受管的那个」
    /// （不发身份行，帧与旧客户端逐字节一致）。
    Stop { pid: Option<u32> },
    /// `cleanup`（无参数行）。
    Cleanup,
    /// `freeport <port>`（行3/行2 = 端口字符串）。
    FreePort { port: u16 },
    /// mac/win：`start <cfg> <log> <fwd> <ppid?>`。
    Start(StartParams),
    /// linux：`start <singbox> <cfg> <log> <fwd> <ppid?>`（多核路径行）。
    LinuxStart(LinuxStartParams),
    /// `route-add <iface> <cidrs>`（mac/win）。
    RouteAdd(RouteParams),
    /// `route-del <iface> <cidrs>`（mac/win）。
    RouteDel(RouteParams),
    /// `install-core <srcDir> <wantHash>`（mac/linux）。
    InstallCore(InstallCoreParams),
    /// `default-restore <gateway>`（mac proto v8）。
    DefaultRestore { gateway_ipv4: String },
    /// `flush-dns`（mac proto v9，无参数行）。
    FlushDns,
    /// `iface-metric <iface> <metric>`（win 退役命令，proto v3-v5）。
    IfaceMetric { iface: String, metric: u16 },
    /// `uninstall`（win，无参数行）。
    Uninstall,
}

impl Request {
    /// 本请求对应的 wire 命令名（行2，[`command`] 常量之一）。
    #[must_use]
    pub fn command_name(&self) -> &'static str {
        match self {
            Self::Ping => command::common::PING,
            Self::Version => command::common::VERSION,
            Self::Status => command::common::STATUS,
            Self::Stop { .. } => command::common::STOP,
            Self::Cleanup => command::common::CLEANUP,
            Self::FreePort { .. } => command::common::FREEPORT,
            Self::Start(_) | Self::LinuxStart(_) => command::common::START,
            Self::RouteAdd(_) => command::common::ROUTE_ADD,
            Self::RouteDel(_) => command::common::ROUTE_DEL,
            Self::InstallCore(_) => command::mac::INSTALL_CORE, // linux 同名（command::linux::INSTALL_CORE == "install-core"）
            Self::DefaultRestore { .. } => command::mac::DEFAULT_RESTORE,
            Self::FlushDns => command::mac::FLUSH_DNS,
            Self::IfaceMetric { .. } => command::win::IFACE_METRIC,
            Self::Uninstall => command::win::UNINSTALL,
        }
    }

    /// 把命令后的参数行（行3..）追加到 `out`。每行不含 `\n`（帧层统一加 `\n`）。
    ///
    /// 严格对照 Go 源 `readLine(r)` 顺序：参数行的**顺序**与**是否有尾随空行**都是 wire 兼容约束
    /// （Go 的 `readLine` 对 EOF 返回 `""`，故少发一行 ≠ 协议错，只是该参数取默认值 —— 如 start 的 ppid）。
    pub fn write_args(&self, out: &mut Vec<String>) {
        match self {
            Self::Ping
            | Self::Version
            | Self::Status
            | Self::Cleanup
            | Self::FlushDns
            | Self::Uninstall => {
                // 无参数行
            }
            Self::Stop { pid } => {
                // 受管 pid 身份行（本协议新增，向后兼容两向）：
                // - `None` → **不发这一行**，帧与旧客户端逐字节一致（旧 helper 的 stop 分支本就不读参数行）。
                // - `Some(p)` → 多发一行 `<pid>`。旧 helper 读完 command 就应答、这一行留在缓冲区里随连接关闭
                //   丢弃（每请求一连接 + 写完 shutdown），故新客户端 + 旧 helper **仍能正常停核**，只是退化成
                //   旧的「停当前受管核」语义 —— 绝不会变成「永远停不掉核」。
                if let Some(p) = pid {
                    out.push(p.to_string());
                }
            }
            Self::FreePort { port } => {
                // helper.go:362: port := strings.TrimSpace(readLine(r))
                out.push(port.to_string());
            }
            Self::Start(p) => {
                // mac helper.go:508-513 / win helper-win/helper.go:339-344（无 singbox 行）
                push_start_args(p, out);
            }
            Self::LinuxStart(p) => {
                // linux helper-linux/helper.go:401-405（多 singbox 行）
                out.push(p.singbox_path.clone());
                push_start_args(&p.common, out);
            }
            Self::RouteAdd(rp) | Self::RouteDel(rp) => {
                // helper.go:455-456: iface 行 + cidrs 行（逗号分隔）
                out.push(rp.iface.clone());
                out.push(rp.cidrs.join(","));
            }
            Self::InstallCore(p) => {
                // helper.go:583-584: src 行 + wantHash 行
                out.push(p.src_dir.clone());
                out.push(p.want_hash.clone());
            }
            Self::DefaultRestore { gateway_ipv4 } => {
                // helper.go:485: gw 行（IPv4）
                out.push(gateway_ipv4.clone());
            }
            Self::IfaceMetric { iface, metric } => {
                // helper-win/helper.go:251-252: iface 行 + metric 行
                out.push(iface.clone());
                out.push(metric.to_string());
            }
        }
    }

    /// 便利：返回本请求的全部参数行（命令后的行，不含 token/command 行）。
    #[must_use]
    pub fn args_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.write_args(&mut out);
        out
    }
}

/// 解析 `stop` 的可选身份行（helper 侧解码用）：空/非数字/0 → `None`（旧客户端语义）。
///
/// `0` 归 `None` 而非 `Some(0)`：pid 0 不是合法进程号，`Some(0)` 只会让身份判据恒不匹配 =
/// 停核彻底失效。与 start 的 `ppid` 行同一处置（`filter(|&p| p > 0)`）。
#[must_use]
pub fn parse_stop_pid(line: &str) -> Option<u32> {
    line.trim().parse::<u32>().ok().filter(|&p| p > 0)
}

/// **停核的受管 pid 身份判据**（三平台 helper 的 `stop` 分支共用的唯一真值）。
///
/// `want` = 客户端在 [`Request::Stop`] 里声明的「我要停的那个 pid」，`current` = helper 此刻手里
/// 受管 child 的 pid。返回 `true` 才允许动手杀。
///
/// **为什么必须有**（根因）：客户端的停核腿是异步的 —— 从它发出 `stop` 到 helper 真执行之间，
/// 可能夹进「用户重装 helper / 重新起核」的一整个新会话。此时 helper 手里的受管 pid 已经换成
/// **新核**，而这条老 stop 腿若按「反正要停就杀当前的」执行，杀掉的正是用户刚连上的那个核
/// （表现为「刚连上就被静默断开」，且现象酷似核自己崩了）。客户端侧的世代守卫够不着这一层，
/// 因为杀进程发生在 helper 进程里。
///
/// `want == None` 保留旧语义（停当前受管核）：那是**尚不知道 pid** 的合法场景 —— 起核 IPC 在飞、
/// pid 未回传时的 racing stop 必须能把 daemon 手里的核收走，否则就留下 root 孤儿。
#[must_use]
pub fn stop_pid_matches(want: Option<u32>, current: u32) -> bool {
    match want {
        None => true,
        Some(p) => p == current,
    }
}

/// 把共用 start 参数（cfg/log/fwd/ppid）追加到 out（mac/win/linux start 的共用尾部）。
fn push_start_args(p: &StartParams, out: &mut Vec<String>) {
    out.push(p.cfg.clone());
    out.push(p.log.clone());
    // fwd：Go 源按 "0"/"1" 字符串比对（helper.go:534: `if fwd == "1"`），下发 "0"/"1" 保 wire 一致。
    out.push(if p.fwd {
        "1".to_owned()
    } else {
        "0".to_owned()
    });
    // ppid：None → 不发该行（Go readLine 在 EOF 返回 ""，Atoi("")=0，不启看护 —— 兼容旧客户端）。
    //         Some → 发 pid 的十进制字符串。
    if let Some(pid) = p.parent_pid {
        out.push(pid.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_version_status_no_args() {
        for r in [Request::Ping, Request::Version, Request::Status] {
            assert!(r.args_lines().is_empty(), "{r:?} 不应有参数行");
        }
    }

    // ===== stop 的受管 pid 身份（杀错进程的防线）=====

    /// **变异门（判据本体）**：`stop_pid_matches` 必须只在「want 未声明」或「want == 手里那个」时放行。
    ///
    /// 改成恒 `true`（= 去掉身份判据，退回「反正要停就杀当前的」）→ 第二条断言转红。
    /// 改成 `want.is_some_and(|p| p == current)`（连 `None` 也拒）→ 第一条转红，那会让「起核 IPC
    /// 在飞、pid 未回传」时的 racing stop 停不掉核 = 留 root 孤儿。
    #[test]
    fn stop_pid_matches_only_when_unspecified_or_equal() {
        assert!(
            stop_pid_matches(None, 4242),
            "未声明身份 = 旧语义「停当前受管核」：这是 pid 尚未回传时防孤儿所必需"
        );
        assert!(stop_pid_matches(Some(4242), 4242), "同一个 pid → 放行");
        assert!(
            !stop_pid_matches(Some(4242), 9001),
            "手里的核不是请求所指的那个 = 它属另一个会话 → 绝不动手"
        );
    }

    /// `parse_stop_pid`：空/非数字/0 一律 `None`（0 归 None 而非 Some(0)，否则身份恒不匹配 = 停不掉核）。
    #[test]
    fn parse_stop_pid_rejects_empty_zero_and_garbage() {
        assert_eq!(parse_stop_pid("4242"), Some(4242));
        assert_eq!(parse_stop_pid("  4242  "), Some(4242));
        assert_eq!(parse_stop_pid(""), None);
        assert_eq!(parse_stop_pid("0"), None);
        assert_eq!(parse_stop_pid("abc"), None);
        assert_eq!(parse_stop_pid("-1"), None);
    }

    /// **wire 兼容门（两向）**：`Stop { pid: None }` 的帧必须与旧客户端**逐字节一致**（不多发空行），
    /// `Some` 才多一行 —— 这样旧 helper 收到新客户端的 stop 仍照旧停核，绝不会「永远停不掉核」。
    ///
    /// 变异：把 `None` 写成 `out.push(String::new())`（发空行）→ 首条转红。旧 helper 的 stop 分支
    /// 不读参数行，多出的空行虽被连接关闭丢弃，但会让 wire 形态与已部署实现失配（且 linux 侧
    /// 新 helper 读到空行 = None，等价，但形态漂移无收益）。
    #[test]
    fn stop_omits_identity_line_when_unspecified() {
        assert!(
            Request::Stop { pid: None }.args_lines().is_empty(),
            "不声明身份 → 帧与旧客户端逐字节一致（旧 helper 照常停核）"
        );
        assert_eq!(Request::Stop { pid: Some(4242) }.args_lines(), vec!["4242"]);
    }

    /// 整帧形态（含平台差异）：stop 的身份行紧跟 command 行。
    #[test]
    fn stop_frame_shape_carries_identity_line() {
        use crate::{codec, Platform};
        let framed = String::from_utf8(codec::encode(
            Platform::Mac,
            "TOK",
            &Request::Stop { pid: Some(7) },
        ))
        .unwrap();
        assert_eq!(framed, "TOK\nstop\n7\n");
        let linux = String::from_utf8(codec::encode(
            Platform::Linux,
            "",
            &Request::Stop { pid: None },
        ))
        .unwrap();
        assert_eq!(linux, "stop\n", "旧语义帧不变");
    }

    #[test]
    fn start_writes_cfg_log_fwd_ppid_lines() {
        // 对照 mac helper.go:508-513 的 readLine 顺序
        let r = Request::Start(StartParams {
            cfg: "/tmp/cfg.json".into(),
            log: "/tmp/log.txt".into(),
            fwd: true,
            parent_pid: Some(4242),
        });
        assert_eq!(
            r.args_lines(),
            vec!["/tmp/cfg.json", "/tmp/log.txt", "1", "4242"]
        );
    }

    #[test]
    fn start_without_ppid_omits_line() {
        // 兼容旧客户端：ppid 缺失 = 不启父死看护（Go readLine EOF → "" → Atoi=0）
        let r = Request::Start(StartParams {
            cfg: "/tmp/c.json".into(),
            log: String::new(),
            fwd: false,
            parent_pid: None,
        });
        assert_eq!(r.args_lines(), vec!["/tmp/c.json", "", "0"]);
    }

    #[test]
    fn linux_start_writes_singbox_first() {
        // 对照 linux helper-linux/helper.go:401-405（singbox 行在最前）
        let r = Request::LinuxStart(LinuxStartParams {
            singbox_path: "/usr/local/lib/polaris/core/sing-box".into(),
            common: StartParams {
                cfg: "/tmp/c.json".into(),
                log: String::new(),
                fwd: false,
                parent_pid: None,
            },
        });
        assert_eq!(
            r.args_lines(),
            vec![
                "/usr/local/lib/polaris/core/sing-box",
                "/tmp/c.json",
                "",
                "0",
            ]
        );
    }

    #[test]
    fn route_add_writes_iface_then_cidrs_csv() {
        // 对照 helper.go:455-456
        let r = Request::RouteAdd(RouteParams {
            iface: "polaris-ts".into(),
            cidrs: vec!["10.0.0.0/8".into(), "172.16.0.0/12".into()],
        });
        assert_eq!(
            r.args_lines(),
            vec!["polaris-ts", "10.0.0.0/8,172.16.0.0/12"]
        );
    }

    #[test]
    fn install_core_writes_src_then_hash() {
        // 对照 helper.go:583-584
        let r = Request::InstallCore(InstallCoreParams {
            src_dir: "/tmp/core-staging".into(),
            want_hash: "abc123def4567890abcdef1234567890abcdef1234567890abcdef1234567890".into(),
        });
        let args = r.args_lines();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "/tmp/core-staging");
        assert_eq!(args[1].len(), 64, "want_hash 须 64 字符 hex");
    }

    #[test]
    fn command_name_mapping() {
        // 锁住 wire 命令名 ↔ Request 变体映射
        assert_eq!(Request::Ping.command_name(), "ping");
        assert_eq!(Request::Stop { pid: None }.command_name(), "stop");
        assert_eq!(Request::FreePort { port: 1 }.command_name(), "freeport");
        assert_eq!(
            Request::Start(StartParams {
                cfg: String::new(),
                log: String::new(),
                fwd: false,
                parent_pid: None,
            })
            .command_name(),
            "start"
        );
        assert_eq!(Request::FlushDns.command_name(), "flush-dns");
        assert_eq!(Request::Uninstall.command_name(), "uninstall");
        assert_eq!(
            Request::DefaultRestore {
                gateway_ipv4: "1.2.3.4".into()
            }
            .command_name(),
            "default-restore"
        );
    }
}
