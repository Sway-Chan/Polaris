//! 系统能力类 command（上游 `system-handlers.ts` + 系统代理/DNS 设置）。
//!
//! 映射 channel：
//! - `system:listProcesses` → [`system_list_processes`]（路由规则的进程快速选择器）
//!
//! 进程枚举按平台取**可执行真实路径**（picker 选出的 name/path 会成为 processName/processPath
//! 路由规则值，必须与 sing-box 的进程匹配口径一致，否则据其建的规则永不命中 → 静默路由失效）：
//! - Linux：读 `/proc/<pid>/exe`（readlink 得完整可执行路径——无 comm 的 15 字符截断、含空格无碍），
//!   basename → name；exe 不可读（内核线程/权限）时回退 `/proc/<pid>/comm` 或 cmdline argv[0]；
//! - macOS/BSD：`ps -axo comm=`（**单列**：每行即完整 comm/路径，无第二列可误切；`=` 抑制 `COMM` 表头），
//!   basename → name；
//! - Windows：`tasklist /fo csv /nh`（CSV 首字段带引号 = 映像名；tasklist 不给路径 → `None`）。
//!
//! 解析层是**纯函数**（captured 样本驱动，跨平台可单测）；平台差异只在「读哪个源 / 怎么解析」的运行时
//! [`Platform`] 分派里。**绝不对合并了多列的整行做空白切分**——否则 `Google Chrome Helper` /
//! `Web Content` / `tmux: server` 之类含空格的名字会被切碎（`name="Google"` / `path="Chrome.app/…"`）。

#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;
use std::time::Duration;

use tauri::State;

use polaris_helper_proto::Platform;
use polaris_system_integration::{Command, CommandRunner, StdCommandRunner};

use crate::response::ApiResponse;
use crate::runtime::AppRuntime;

/// 进程枚举命令的硬超时（本机 `ps`/`tasklist` 均在毫秒级返回；5s 仅作挂起兜底）。
const PROCESS_LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// 进程信息（上游 `SystemProcessInfo`，`ui/src/contracts/types/rules.ts:81`）。
///
/// 按进程名聚合：`count` = 同名进程数，`path` = 首个带可执行路径的实例（`tasklist` 不给路径 → `None`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemProcessInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub count: u32,
}

/// macOS/BSD 进程枚举命令：`ps -axo comm=`（**单列** comm，`=` 抑制 `COMM` 表头）。
fn macos_ps_command() -> Command {
    Command::new("ps", ["-axo", "comm="])
}

/// Windows 进程枚举命令：`tasklist /fo csv /nh`（CSV 无表头，首字段 = 映像名；不提供路径）。
fn windows_tasklist_command() -> Command {
    Command::new("tasklist", ["/fo", "csv", "/nh"])
}

/// 取路径 basename（去目录段）。用作聚合键的进程名（`comm`/exe 在 macOS/Linux 常是可执行全路径）。
fn basename(s: &str) -> String {
    s.rsplit(['/', '\\']).next().unwrap_or(s).to_string()
}

/// 解析 `ps -axo comm=` 单列输出：每行 = 一个进程的 comm（macOS 常为完整可执行路径，可含空格）。
fn parse_comm_column_output(stdout: &str) -> Vec<SystemProcessInfo> {
    aggregate(stdout.lines().filter_map(parse_comm_line))
}

/// 单行 comm → (name, path?)。**整行**即名字/路径——绝不空白切分；name = basename，含分隔符则整行即 path。
fn parse_comm_line(line: &str) -> Option<(String, Option<String>)> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let name = basename(line);
    if name.is_empty() {
        return None;
    }
    let path = (line.contains('/') || line.contains('\\')).then(|| line.to_string());
    Some((name, path))
}

/// 从 `/proc/<pid>` 三来源解析单个 Linux 进程的 (name, path?)。
///
/// 优先级：`exe`（readlink 真实完整路径——无 15 字符 comm 截断、含空格无碍）→ `comm`（exe 不可读时的
/// 名字回退，无路径）→ cmdline argv[0]（comm 也缺时）。全空 → `None`（跳过）。
fn resolve_linux_process(
    exe: Option<&str>,
    comm: Option<&str>,
    cmdline_argv0: Option<&str>,
) -> Option<(String, Option<String>)> {
    if let Some(exe) = exe.map(str::trim).filter(|s| !s.is_empty()) {
        // exe readlink 可能带 " (deleted)" 后缀（二进制已被删/替换）——剥离再取 basename。
        let clean = exe.strip_suffix(" (deleted)").unwrap_or(exe);
        let name = basename(clean);
        if !name.is_empty() {
            return Some((name, Some(clean.to_string())));
        }
    }
    if let Some(comm) = comm.map(str::trim).filter(|s| !s.is_empty()) {
        return Some((comm.to_string(), None));
    }
    if let Some(argv0) = cmdline_argv0.map(str::trim).filter(|s| !s.is_empty()) {
        let name = basename(argv0);
        if !name.is_empty() {
            let path = argv0.contains('/').then(|| argv0.to_string());
            return Some((name, path));
        }
    }
    None
}

/// `/proc/<pid>/cmdline` 的 argv[0]（NUL 分隔；空 → `None`，如内核线程）。
fn read_cmdline_argv0(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    let argv0 = raw.split(|&b| b == 0).next()?;
    if argv0.is_empty() {
        return None;
    }
    String::from_utf8(argv0.to_vec()).ok()
}

/// 枚举 Linux 进程：遍历 `/proc/<pid>`，每 pid 读 exe/comm/cmdline → [`resolve_linux_process`] → 聚合。
fn enumerate_linux_processes() -> Result<Vec<SystemProcessInfo>, String> {
    let entries = std::fs::read_dir("/proc").map_err(|e| format!("读取 /proc 失败: {e}"))?;
    let rows = entries.flatten().filter_map(|entry| {
        let file_name = entry.file_name();
        let pid = file_name.to_str()?;
        // 只认纯数字 pid 目录（跳过 /proc/self、/proc/meminfo 等非进程项）。
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let dir = entry.path();
        let exe = std::fs::read_link(dir.join("exe"))
            .ok()
            .and_then(|p| p.to_str().map(str::to_string));
        let comm = std::fs::read_to_string(dir.join("comm")).ok();
        let argv0 = read_cmdline_argv0(&dir.join("cmdline"));
        resolve_linux_process(exe.as_deref(), comm.as_deref(), argv0.as_deref())
    });
    Ok(aggregate(rows))
}

/// 取 CSV 一行的首字段（tasklist `/fo csv` 各字段带双引号；映像名内无引号）。
fn csv_first_field(line: &str) -> String {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix('"') {
        return rest.split('"').next().unwrap_or(rest).to_string();
    }
    line.split(',').next().unwrap_or("").trim().to_string()
}

fn parse_tasklist_output(stdout: &str) -> Vec<SystemProcessInfo> {
    aggregate(stdout.lines().filter_map(|line| {
        let name = csv_first_field(line);
        // tasklist 不提供路径 → path 恒 None。
        (!name.is_empty()).then_some((name, None))
    }))
}

/// 按 name 聚合行 → `count` + 首个可用 `path`。输出按 count 降序、同数按 name 升序（确定性，利 UI/测试）。
fn aggregate(rows: impl IntoIterator<Item = (String, Option<String>)>) -> Vec<SystemProcessInfo> {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, (u32, Option<String>)> = HashMap::new();
    for (name, path) in rows {
        if name.is_empty() {
            continue;
        }
        let entry = map.entry(name.clone()).or_insert_with(|| {
            order.push(name.clone());
            (0, None)
        });
        entry.0 = entry.0.saturating_add(1);
        if entry.1.is_none() {
            entry.1 = path;
        }
    }
    let mut out: Vec<SystemProcessInfo> = order
        .into_iter()
        .map(|name| {
            let (count, path) = map.remove(&name).unwrap_or((0, None));
            SystemProcessInfo { name, path, count }
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    out
}

/// 按平台阻塞枚举进程（读 `/proc` 或跑 `ps`/`tasklist`）。在 [`system_list_processes`] 的
/// `spawn_blocking` 里调——含同步 IO / 子进程等待，不得在 async executor 线程直跑。
fn list_processes_blocking() -> Result<Vec<SystemProcessInfo>, String> {
    match Platform::current() {
        Platform::Linux => enumerate_linux_processes(),
        Platform::Win => {
            let out = StdCommandRunner.run(&windows_tasklist_command(), PROCESS_LIST_TIMEOUT)?;
            Ok(parse_tasklist_output(&out.stdout))
        }
        // macOS / BSD / Other 均有 POSIX `ps`，`-axo comm=` 单列输出。
        Platform::Mac | Platform::Other => {
            let out = StdCommandRunner.run(&macos_ps_command(), PROCESS_LIST_TIMEOUT)?;
            Ok(parse_comm_column_output(&out.stdout))
        }
    }
}

/// 上游 `SYSTEM_LIST_PROCESSES`：枚举系统进程（路由规则的进程快速选择器用）。
///
/// **`async fn` + `spawn_blocking`**：枚举要读 `/proc` 或跑 `ps`/`tasklist`（0.5–2s，5s 超时），
/// Tauri 同步 command 跑在**主线程**上会冻 UI（与 `http.rs` CoreDownloader 同纪律）→ 卸到 blocking
/// 线程池。失败（`/proc` 不可读 / 命令缺失 / 超时 / 非零退出）→ 显式 `err`，而非静默返空数组：picker
/// 「拉不到进程」与「一个都没有」语义不同，前端据此可提示重试。
#[tauri::command]
pub async fn system_list_processes(
    _state: State<'_, AppRuntime>,
) -> Result<ApiResponse<Vec<SystemProcessInfo>>, ()> {
    match tokio::task::spawn_blocking(list_processes_blocking).await {
        Ok(Ok(procs)) => Ok(ApiResponse::ok(procs)),
        Ok(Err(e)) => {
            log::warn!("枚举系统进程失败: {e}");
            Ok(ApiResponse::err(format!("枚举系统进程失败: {e}")))
        }
        Err(e) => {
            log::warn!("进程枚举任务失败: {e}");
            Ok(ApiResponse::err("进程枚举任务失败"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_argv_is_selected_per_platform() {
        assert_eq!(macos_ps_command(), Command::new("ps", ["-axo", "comm="]));
        assert_eq!(
            windows_tasklist_command(),
            Command::new("tasklist", ["/fo", "csv", "/nh"])
        );
    }

    // ── macOS/BSD：`ps -axo comm=` 单列 —— 含空格的进程名/路径不得被切碎（P1/P7）──

    #[test]
    fn comm_column_preserves_names_and_paths_with_spaces() {
        // captured `ps -axo comm=` 样本：含空格的全路径（Google Chrome Helper）、目录段含空格的
        // 绝对 macOS 路径、无路径含空格名（tmux: server）、同名多实例、无分隔符名（login）。
        // `=` 已抑制 COMM 表头 → 样本无表头行（P7）。
        let sample = "\
/Applications/Google Chrome.app/Contents/MacOS/Google Chrome Helper
/Applications/Google Chrome.app/Contents/MacOS/Google Chrome Helper
/Applications/Visual Studio Code.app/Contents/MacOS/Electron
tmux: server
login
";
        let got = parse_comm_column_output(sample);

        // 含空格全路径 → name = 完整 basename「Google Chrome Helper」，path = 整行全路径（不被空白切碎）。
        let helper = got
            .iter()
            .find(|p| p.name == "Google Chrome Helper")
            .expect("Google Chrome Helper 应保留完整 name");
        assert_eq!(helper.count, 2);
        assert_eq!(
            helper.path.as_deref(),
            Some("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome Helper")
        );

        // 目录段含空格的绝对路径 → basename 正确、path 完整。
        let electron = got.iter().find(|p| p.name == "Electron").expect("Electron");
        assert_eq!(
            electron.path.as_deref(),
            Some("/Applications/Visual Studio Code.app/Contents/MacOS/Electron")
        );

        // `tmux: server`（含空格、无分隔符）→ name 整体保留，不得切成 "tmux:"，无路径。
        let tmux = got
            .iter()
            .find(|p| p.name == "tmux: server")
            .expect("tmux: server 应整体保留");
        assert_eq!(tmux.path, None);

        // 确定性排序：count 降序 → Google Chrome Helper（2）居首。
        assert_eq!(got[0].name, "Google Chrome Helper");
    }

    // ── Linux：/proc 解析 —— exe 真实路径优先，无 15 字符 comm 截断、含空格 comm 不误切（P1/P4）──

    #[test]
    fn linux_resolve_prefers_exe_realpath_over_misleading_or_truncated_comm() {
        // Firefox 内容进程：comm 被改写成含空格的 "Web Content"（旧空白切分 → "Web"）；
        // exe 指向真实二进制 → name = "firefox"（sing-box 据此匹配），path = 全路径。
        let (name, path) = resolve_linux_process(
            Some("/usr/lib/firefox/firefox"),
            Some("Web Content"),
            Some("/usr/lib/firefox/firefox"),
        )
        .expect("exe 可读时应据 exe 解析");
        assert_eq!(name, "firefox");
        assert_eq!(path.as_deref(), Some("/usr/lib/firefox/firefox"));

        // comm 15 字符截断：chrome_crashpad_handler → comm 只剩 "chrome_crashpad"；exe 给出完整名。
        let (name, path) = resolve_linux_process(
            Some("/opt/google/chrome/chrome_crashpad_handler"),
            Some("chrome_crashpad"),
            None,
        )
        .expect("exe 可读");
        assert_eq!(
            name, "chrome_crashpad_handler",
            "exe basename 不得被 15 字符 comm 截断"
        );
        assert_eq!(
            path.as_deref(),
            Some("/opt/google/chrome/chrome_crashpad_handler")
        );

        // exe readlink 带 " (deleted)" 后缀（二进制已被替换）→ 剥离后取 basename。
        let (name, path) =
            resolve_linux_process(Some("/usr/bin/nginx (deleted)"), Some("nginx"), None).unwrap();
        assert_eq!(name, "nginx");
        assert_eq!(path.as_deref(), Some("/usr/bin/nginx"));
    }

    #[test]
    fn linux_resolve_falls_back_to_comm_then_cmdline() {
        // 内核线程 / exe 不可读：无 exe，comm 存在 → name = comm（含斜杠原样保留），无路径。
        let (name, path) = resolve_linux_process(None, Some("kworker/0:1"), None).unwrap();
        assert_eq!(name, "kworker/0:1");
        assert_eq!(path, None);

        // exe、comm 都缺 → cmdline argv[0] 的 basename + 路径。
        let (name, path) = resolve_linux_process(None, None, Some("/usr/sbin/sshd")).unwrap();
        assert_eq!(name, "sshd");
        assert_eq!(path.as_deref(), Some("/usr/sbin/sshd"));

        // 全空 → None（跳过）。
        assert!(resolve_linux_process(None, None, None).is_none());
        assert!(resolve_linux_process(Some("  "), Some(""), Some("  ")).is_none());
    }

    #[test]
    fn cmdline_argv0_takes_first_nul_delimited_token() {
        let dir = std::env::temp_dir().join(format!("polaris-cmdline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let f = dir.join("cmdline");
        // 真实 /proc/<pid>/cmdline：NUL 分隔 argv。argv0 = 完整路径，后续为参数。
        std::fs::write(&f, b"/usr/lib/firefox/firefox\0-contentproc\0-childID\0").unwrap();
        assert_eq!(
            read_cmdline_argv0(&f).as_deref(),
            Some("/usr/lib/firefox/firefox")
        );
        // 空 cmdline（内核线程）→ None。
        std::fs::write(&f, b"").unwrap();
        assert_eq!(read_cmdline_argv0(&f), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_enumerate_returns_current_process() {
        // enumerate_linux_processes 是 Linux 专属（读 /proc）；非 Linux 编译期缺席，不做运行期假绿。
        let procs = enumerate_linux_processes().expect("读 /proc 应成功");
        assert!(!procs.is_empty(), "应至少枚举到本进程");
        assert!(procs.iter().all(|p| !p.name.is_empty()), "name 不得为空串");
    }

    #[test]
    fn tasklist_csv_aggregates_and_has_no_path() {
        // captured `tasklist /fo csv /nh` 样本（CSV 无表头，内存字段含逗号——只取首字段不受影响）。
        let sample = "\
\"chrome.exe\",\"1234\",\"Console\",\"1\",\"123,456 K\"\r
\"chrome.exe\",\"1240\",\"Console\",\"1\",\"90,000 K\"\r
\"sing-box.exe\",\"555\",\"Services\",\"0\",\"20,000 K\"\r
";
        let got = parse_tasklist_output(sample);
        let chrome = got
            .iter()
            .find(|p| p.name == "chrome.exe")
            .expect("chrome.exe");
        assert_eq!(chrome.count, 2);
        assert_eq!(chrome.path, None, "tasklist 不提供路径");
        assert!(got.iter().any(|p| p.name == "sing-box.exe" && p.count == 1));
    }

    #[test]
    fn empty_output_yields_empty_list() {
        assert!(parse_comm_column_output("").is_empty());
        assert!(parse_comm_column_output("\n   \n").is_empty());
        assert!(parse_tasklist_output("").is_empty());
    }
}
