//! 端口 LISTEN 持有者定位（freeport，移植自 上游 `helper/helper.go:358-395`，proto v4）。
//!
//! ## 职责
//!
//! 按端口（root lsof）定位 9090/指定端口的 LISTEN 持有者 —— 是 sing-box 才 kill -9，否则回报占用者
//! 名字（不杀）。彻底摆脱 app 侧 cmdline 匹配（覆盖外部/旧路径 sing-box）。
//!
//! ## Go 源根因（`helper.go:358-360`）
//!
//! app 侧原本靠 cmdline 匹配找 9090 占用者，但「外部 sing-box / 旧 app 路径 / 改过 singboxBin 路径」
//! 都匹配不到 → 残留进程占端口。helper 用 root lsof 按端口定位 + ps comm= 判可执行名，覆盖所有路径。
//!
//! ## 移植纪律
//!
//! Go 源的「lsof → ps comm → kill/记录」三步在本模块拆为：
//! - [`parse_lsof_pids`]：解析 lsof 输出的 pid 列表（纯逻辑，跨平台可测）。
//! - [`classify_holders`]：按 comm 名分类为 sing-box（杀）/ foreign（报）（纯逻辑）。
//! - [`freeport_result_to_response`]：构造 wire 响应。
//! - lsof/ps/kill 执行经 [`CommandRunner`](crate::platform::macos::exec::CommandRunner) 抽象，测试 mock。
//!
//! ## 关键约束（`helper.go:375-376`）
//!
//! 用 `ps -o comm=`（仅可执行名，不含参数）：避免「参数里碰巧含 sing-box」的进程被 root 误杀（M2）。

use crate::platform::macos::exec::{CommandRunner, EXEC_TIMEOUT};
use polaris_helper_proto::response::{FreePort, ResponseKind};
use polaris_helper_proto::Error as ProtoError;
use polaris_helper_proto::ErrorCode;
use polaris_helper_proto::Response;

/// lsof 程序路径（`helper.go:367`：`/usr/sbin/lsof`）。
pub const LSOF_BIN: &str = "/usr/sbin/lsof";
/// ps 程序路径（`helper.go:376`：`/bin/ps`）。
pub const PS_BIN: &str = "/bin/ps";
/// kill 程序路径（`helper.go:379`：`/bin/kill`）。
pub const KILL_BIN: &str = "/bin/kill";

/// 端口字符串里是否全 ASCII 数字（移植自 `helper.go:363`）。
///
/// 对照 Go：`strings.IndexFunc(port, func(c rune) bool { return c < '0' || c > '9' }) >= 0` → 非数字 → ERR bad-port。
#[must_use]
pub fn is_valid_port_str(port: &str) -> bool {
    !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
}

/// 解析 lsof `-ti tcp:<port> -sTCP:LISTEN` 的输出为 pid 列表（移植自 `helper.go:367-368`）。
///
/// Go：`strings.Fields(strings.TrimSpace(string(out)))` —— 按空白分割，每项是一个 pid。
/// 返回解析出的 pid（过滤非数字项，对齐 Go Fields 后隐式信任 lsof 输出格式）。
#[must_use]
pub fn parse_lsof_pids(output: &[u8]) -> Vec<u32> {
    let s = String::from_utf8_lossy(output);
    s.split_whitespace()
        .filter_map(|tok| tok.parse::<u32>().ok())
        .collect()
}

/// 判断 ps comm= 输出是否是 sing-box（移植自 `helper.go:378`）。
///
/// Go：`strings.Contains(comm, "sing-box")`。含子串即视为 sing-box（覆盖各种路径前缀的可执行名）。
#[must_use]
pub fn is_singbox_comm(comm: &str) -> bool {
    comm.contains("sing-box")
}

/// 单个 pid 的分类结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolderKind {
    /// 是 sing-box → kill -9（`helper.go:379`）。
    SingBox,
    /// 非 sing-box → 记名（`helper.go:382-386`）。
    Foreign { name: String },
}

/// 按 comm 分类持有者（纯逻辑）。
#[must_use]
pub fn classify_holder(comm: &str, pid: u32) -> HolderKind {
    let trimmed = comm.trim();
    if is_singbox_comm(trimmed) {
        HolderKind::SingBox
    } else {
        // helper.go:382-385: name = comm（空则 "pid:<pid>"）
        let name = if trimmed.is_empty() {
            format!("pid:{pid}")
        } else {
            trimmed.to_owned()
        };
        HolderKind::Foreign { name }
    }
}

/// 把扫描累计的 killed/foreign 收敛为协议 [`FreePort`]（移植自 `helper.go:389-394`）。
///
/// 原先此处有个 `FreeportOutcome{killed,foreign}` 影子结构 + `From<..> for Response`；
/// 它只是循环累加器 + 一次终态投影，结果形状与 [`FreePort`] 重复 —— 累加器降为两个局部 `Vec`，
/// 终态直接产出协议类型（G3.3）。**优先级语义是 Go 源语义，逐条保留**：
/// 1. 无任何持有者 → `OK free`（`helper.go:370`）。
/// 2. 有 foreign → `OK foreign`（**即使同时有 killed** —— 混合占用回报 foreign 是诚实终态，`:389-391`）。
/// 3. 否则 → `OK killed`（`:393`）。
fn to_free_port(killed: Vec<u32>, foreign: Vec<String>) -> FreePort {
    if killed.is_empty() && foreign.is_empty() {
        return FreePort::Free;
    }
    if !foreign.is_empty() {
        return FreePort::Foreign { names: foreign };
    }
    FreePort::Killed { pids: killed }
}

/// 收敛并包成 wire [`Response`]。
fn free_port_response(killed: Vec<u32>, foreign: Vec<String>) -> Response {
    Response::Ok(ResponseKind::FreePort(to_free_port(killed, foreign)))
}

/// 端口非法时构造 bad-port 错误响应（`helper.go:364`）。
#[must_use]
pub fn bad_port_response() -> Response {
    Response::Err(ProtoError::new(ErrorCode::BadPort))
}

/// 执行完整 freeport 流程（移植自 `helper.go:361-395`）。
///
/// 步骤：
/// 1. 端口校验（非数字 → `ERR bad-port`）。
/// 2. lsof 取 LISTEN 持有者 pid 列表。空 → `OK free`。
/// 3. 逐 pid：ps comm= 取可执行名 → 是 sing-box 则 kill -9（记 killed），否则记 foreign 名。
/// 4. 有 foreign → `OK foreign <names>`；否则 `OK killed <pids>`。
///
/// 不碰 child/childDone（Go 源同理，`helper.go:360`）→ 无需持锁，由 handler 在加锁前调用。
pub fn run_freeport(runner: &dyn CommandRunner, port: &str) -> Response {
    // helper.go:363-366: 端口校验
    if !is_valid_port_str(port) {
        return bad_port_response();
    }
    // helper.go:367-368: lsof -ti tcp:<port> -sTCP:LISTEN
    let lsof_args = [
        "-ti".to_string(),
        format!("tcp:{port}"),
        "-sTCP:LISTEN".to_owned(),
    ];
    let lsof_refs: Vec<&str> = lsof_args.iter().map(String::as_str).collect();
    let pids = match runner.output(EXEC_TIMEOUT, LSOF_BIN, &lsof_refs) {
        Ok(out) => parse_lsof_pids(&out),
        Err(_) => {
            // lsof 失败（无持有者返回非零）→ 视为 free（Go 源 `_ = err` 后 pids 为空 → OK free）
            Vec::new()
        }
    };
    if pids.is_empty() {
        // helper.go:369-371: OK free
        return free_port_response(Vec::new(), Vec::new());
    }

    let mut killed: Vec<u32> = Vec::new();
    let mut foreign: Vec<String> = Vec::new();
    for pid in pids {
        let pid_str = pid.to_string();
        // helper.go:376-377: ps -o comm= -p <pid>
        let comm = match runner.output(EXEC_TIMEOUT, PS_BIN, &["-o", "comm=", "-p", &pid_str]) {
            Ok(out) => String::from_utf8_lossy(&out).to_string(),
            Err(_) => String::new(),
        };
        match classify_holder(&comm, pid) {
            HolderKind::SingBox => {
                // helper.go:379: /bin/kill -9 <pid>
                let _ = runner.run(EXEC_TIMEOUT, KILL_BIN, &["-9", &pid_str]);
                killed.push(pid);
            }
            HolderKind::Foreign { name } => {
                foreign.push(name);
            }
        }
    }
    free_port_response(killed, foreign)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::macos::exec::RunError;
    use std::sync::Mutex;
    use std::time::Duration;

    // ===== 纯逻辑测试 =====

    #[test]
    fn is_valid_port_str_digits_only() {
        // helper.go:363
        assert!(is_valid_port_str("9090"));
        assert!(is_valid_port_str("80"));
        assert!(is_valid_port_str("0"));
        assert!(!is_valid_port_str(""));
        assert!(!is_valid_port_str("9090abc"));
        assert!(!is_valid_port_str("90 90"));
        assert!(!is_valid_port_str("-1"));
        assert!(!is_valid_port_str("9090.0"));
    }

    #[test]
    fn parse_lsof_pids_multiline() {
        // helper.go:367-368: Fields(TrimSpace(out))
        let out = b"123\n456\n789\n";
        assert_eq!(parse_lsof_pids(out), vec![123, 456, 789]);
        // 单行多个（空格分隔）
        assert_eq!(parse_lsof_pids(b"123 456"), vec![123, 456]);
        // 含空白/非数字项过滤
        assert_eq!(parse_lsof_pids(b"  123  \n abc \n456  "), vec![123, 456]);
        // 空
        assert!(parse_lsof_pids(b"").is_empty());
        assert!(parse_lsof_pids(b"   \n").is_empty());
    }

    #[test]
    fn is_singbox_comm_substring_match() {
        // helper.go:378: strings.Contains(comm, "sing-box")
        assert!(is_singbox_comm("sing-box"));
        assert!(is_singbox_comm("/usr/local/bin/sing-box"));
        assert!(is_singbox_comm("/opt/homebrew/bin/sing-box"));
        assert!(!is_singbox_comm("nginx"));
        assert!(!is_singbox_comm("singbox")); // 无连字符
        assert!(!is_singbox_comm(""));
    }

    #[test]
    fn classify_holder_singbox() {
        assert_eq!(
            classify_holder("/usr/local/bin/sing-box", 123),
            HolderKind::SingBox
        );
    }

    #[test]
    fn classify_holder_foreign_uses_comm_name() {
        // helper.go:382-383: name = comm
        assert_eq!(
            classify_holder("nginx", 456),
            HolderKind::Foreign {
                name: "nginx".into()
            }
        );
    }

    #[test]
    fn classify_holder_foreign_empty_comm_uses_pid() {
        // helper.go:384-385: name == "" → "pid:<pid>"
        assert_eq!(
            classify_holder("", 789),
            HolderKind::Foreign {
                name: "pid:789".into()
            }
        );
    }

    #[test]
    fn classify_holder_trims_whitespace() {
        // ps 输出通常带尾随换行
        assert_eq!(classify_holder("sing-box\n", 100), HolderKind::SingBox);
        assert_eq!(
            classify_holder("nginx\n", 200),
            HolderKind::Foreign {
                name: "nginx".into()
            }
        );
    }

    // ===== killed/foreign → FreePort 终态收敛（原 outcome → response）=====

    #[test]
    fn outcome_free_to_response() {
        let r = free_port_response(Vec::new(), Vec::new());
        assert!(matches!(
            r,
            Response::Ok(ResponseKind::FreePort(FreePort::Free))
        ));
    }

    #[test]
    fn outcome_killed_only_to_response() {
        let r = free_port_response(vec![123, 456], Vec::new());
        match r {
            Response::Ok(ResponseKind::FreePort(FreePort::Killed { pids })) => {
                assert_eq!(pids, vec![123, 456]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn outcome_foreign_present_prefers_foreign() {
        // helper.go:389-391: 有 foreign → OK foreign（即使也有 killed）
        let r = free_port_response(vec![123], vec!["nginx".into()]);
        match r {
            Response::Ok(ResponseKind::FreePort(FreePort::Foreign { names })) => {
                assert_eq!(names, vec!["nginx".to_owned()]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bad_port_response_is_err_badport() {
        let r = bad_port_response();
        match r {
            Response::Err(e) => assert_eq!(e.code, ErrorCode::BadPort),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn command_paths_match_go_source() {
        // helper.go:367,376,379
        assert_eq!(LSOF_BIN, "/usr/sbin/lsof");
        assert_eq!(PS_BIN, "/bin/ps");
        assert_eq!(KILL_BIN, "/bin/kill");
    }

    // ===== run_freeport 端到端（mock runner）=====

    /// 可编程 mock：按 (program, key) 返回预设 output。
    struct ProgMock {
        lsof_out: Mutex<Vec<u8>>,
        comm_map: Mutex<std::collections::HashMap<u32, String>>,
        killed: Mutex<Vec<u32>>,
    }

    impl CommandRunner for ProgMock {
        fn run(&self, _t: Duration, p: &str, a: &[&str]) -> Result<(), RunError> {
            // /bin/kill -9 <pid>
            if p == KILL_BIN && a.len() >= 2 && a[0] == "-9" {
                if let Ok(pid) = a[1].parse::<u32>() {
                    self.killed.lock().unwrap().push(pid);
                }
            }
            Ok(())
        }
        fn output(&self, _t: Duration, p: &str, a: &[&str]) -> Result<Vec<u8>, RunError> {
            if p == LSOF_BIN {
                return Ok(self.lsof_out.lock().unwrap().clone());
            }
            if p == PS_BIN {
                // ps -o comm= -p <pid>
                for arg in a {
                    if let Ok(pid) = arg.parse::<u32>() {
                        if let Some(comm) = self.comm_map.lock().unwrap().get(&pid) {
                            return Ok(comm.as_bytes().to_vec());
                        }
                    }
                }
            }
            Ok(Vec::new())
        }
        fn combined(&self, _: Duration, _: &str, _: &[&str]) -> Result<Vec<u8>, RunError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn run_freeport_bad_port() {
        let m = ProgMock {
            lsof_out: Mutex::new(Vec::new()),
            comm_map: Mutex::new(std::collections::HashMap::new()),
            killed: Mutex::new(Vec::new()),
        };
        let r = run_freeport(&m, "abc");
        assert!(matches!(
            r,
            Response::Err(ProtoError {
                code: ErrorCode::BadPort,
                ..
            })
        ));
    }

    #[test]
    fn run_freeport_empty_port() {
        let m = ProgMock {
            lsof_out: Mutex::new(Vec::new()),
            comm_map: Mutex::new(std::collections::HashMap::new()),
            killed: Mutex::new(Vec::new()),
        };
        let r = run_freeport(&m, "");
        assert!(matches!(
            r,
            Response::Err(ProtoError {
                code: ErrorCode::BadPort,
                ..
            })
        ));
    }

    #[test]
    fn run_freeport_no_holder_returns_free() {
        // lsof 输出空 → OK free
        let m = ProgMock {
            lsof_out: Mutex::new(Vec::new()),
            comm_map: Mutex::new(std::collections::HashMap::new()),
            killed: Mutex::new(Vec::new()),
        };
        let r = run_freeport(&m, "9090");
        assert!(matches!(
            r,
            Response::Ok(ResponseKind::FreePort(FreePort::Free))
        ));
    }

    #[test]
    fn run_freeport_kills_singbox_holders() {
        let mut comm = std::collections::HashMap::new();
        comm.insert(123, "/usr/local/bin/sing-box".into());
        comm.insert(456, "/opt/homebrew/bin/sing-box".into());
        let m = ProgMock {
            lsof_out: Mutex::new(b"123\n456\n".to_vec()),
            comm_map: Mutex::new(comm),
            killed: Mutex::new(Vec::new()),
        };
        let r = run_freeport(&m, "9090");
        match r {
            Response::Ok(ResponseKind::FreePort(FreePort::Killed { pids })) => {
                assert_eq!(pids, vec![123, 456]);
            }
            other => panic!("{other:?}"),
        }
        // kill -9 被调用两次
        assert_eq!(*m.killed.lock().unwrap(), vec![123, 456]);
    }

    #[test]
    fn run_freeport_reports_foreign_not_kills() {
        let mut comm = std::collections::HashMap::new();
        comm.insert(123, "nginx".into());
        let m = ProgMock {
            lsof_out: Mutex::new(b"123\n".to_vec()),
            comm_map: Mutex::new(comm),
            killed: Mutex::new(Vec::new()),
        };
        let r = run_freeport(&m, "9090");
        match r {
            Response::Ok(ResponseKind::FreePort(FreePort::Foreign { names })) => {
                assert_eq!(names, vec!["nginx".to_owned()]);
            }
            other => panic!("{other:?}"),
        }
        // foreign 不应被 kill
        assert!(m.killed.lock().unwrap().is_empty());
    }

    #[test]
    fn run_freeport_mixed_prefers_foreign() {
        // 混合：sing-box + nginx。Go helper.go:389 → OK foreign（混合也归 foreign，诚实终态）
        let mut comm = std::collections::HashMap::new();
        comm.insert(123, "sing-box".into());
        comm.insert(456, "nginx".into());
        let m = ProgMock {
            lsof_out: Mutex::new(b"123 456\n".to_vec()),
            comm_map: Mutex::new(comm),
            killed: Mutex::new(Vec::new()),
        };
        let r = run_freeport(&m, "9090");
        assert!(matches!(
            r,
            Response::Ok(ResponseKind::FreePort(FreePort::Foreign { .. }))
        ));
        // sing-box 那个仍被 kill（Go 源对每个 sing-box 都 kill -9，即便最终报 foreign）
        assert_eq!(*m.killed.lock().unwrap(), vec![123]);
    }
}
