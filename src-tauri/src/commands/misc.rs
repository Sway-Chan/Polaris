//! 杂项 command（上游 `log-handlers.ts` / `version-handlers.ts` / `backup-handlers.ts` /
//! `diagnostic-handlers.ts` / `autostart-handlers.ts` / `ipinfo-handlers.ts` +
//! singbox-dashboard / shell 相关）。
//!
//! 映射 channel：
//! - `logs:get` / `logs:clear` → [`logs_get`] / [`logs_clear`]
//! - `logs:runtimeLevel` → [`logs_runtime_level`]（读回核在跑的真实日志级别，不是盘上写的那个）
//! - `logs:diagnosticState` / `logs:setDiagnostic` → 会话级 DEBUG（只活到本次应用退出）
//! - `shell:openExternal` → [`shell_open_external`]（tauri-plugin-shell）
//! - `app:openSingboxDashboard` / `app:refreshSingboxDashboard` /
//!   `app:getSingboxDashboardConnection` → singbox_dashboard_*（Polaris helper-handlers 的 dashboard 部分）
//! - `backup:export` / `backup:importPick` / `backup:importApply` / `backup:getInfo` → backup_*
//! - `diagnostic:export` → [`diagnostic_export`]（脱敏 Markdown）
//! - `autoStart:set` / `autoStart:getStatus` → autostart_*（tauri-plugin-autostart）
//! - `ipinfo:get` → [`ipinfo_get`]（出口 IP 信息）

#![allow(clippy::needless_pass_by_value)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_shell::ShellExt;

use polaris_net_stack::safe_redirect::{safe_redirect_fetch, HttpClient, SafeRedirectFetchOptions};
use polaris_stats_engine::redact::collect_node_identifiers;
use polaris_stats_engine::{AppSection, RuntimeSection};
use polaris_store::backup::{
    build_backup_info, count_category, detect_categories, merge_categories, parse_backup_content,
    pick_categories, sanitize_cross_platform_rules, BackupCategory, BACKUP_CATEGORIES,
    BACKUP_FILE_VERSION,
};

use crate::i18n::{key, t};
use crate::response::{ok_void, ApiResponse};
use crate::runtime::http::{app_user_agent, HttpRuntime, SystemDnsLookup};
use crate::runtime::proxy::ProxyStatus;
use crate::runtime::AppRuntime;

/// 每个日志文件最多纳入诊断报告的尾部字节数（足够排障，又不让报告爆大）。上游 `LOG_TAIL_BYTES`。
const LOG_TAIL_BYTES: u64 = 64 * 1024;

/// 连接 / DNS 类错误标记（命中且非 debug 级 → 提示把日志级别切到 DEBUG 复现）。
///
/// 上游 用正则 `TROUBLE_RE`；此处用**小写子串匹配**等价实现 —— 原正则无捕获、无量词、纯 `|` 分支 + `/i`，
/// 子串匹配语义完全等价，且省掉给 src-tauri 新增 `regex` 依赖。
const TROUBLE_MARKERS: [&str; 9] = [
    "servfail",
    "dns",
    "connection refused",
    "timeout",
    "timed out",
    "handshake",
    "authentication failed",
    "no such host",
    "certificate",
];

/// 平台串，**Node `process.platform` 口径**（win32 / darwin / linux）。
///
/// 刻意不用 `std::env::consts::OS`（会给出 windows / macos）：备份文件的 `platform` 字段要与 上游 写出的
/// 备份**互通**（跨平台进程规则 sanitize 靠它比对），词汇表必须同源。
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

/// 读文件尾部最多 `max_bytes` 字节；不存在 / 失败返回占位串（**绝不抛**——诊断导出不该因日志读不到而失败）。
/// 上游 `DiagnosticService.readTail`。
fn read_tail(path: &Path, max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(meta) = std::fs::metadata(path) else {
        return "(无日志文件)".to_string();
    };
    let size = meta.len();
    let start = size.saturating_sub(max_bytes);
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return format!("(读取失败: {e})"),
    };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return "(读取失败: seek)".to_string();
    }
    let mut buf = Vec::new();
    if let Err(e) = f.take(max_bytes).read_to_end(&mut buf) {
        return format!("(读取失败: {e})");
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    // 截断导致首行半截 → 丢弃首个不完整行，保持可读。
    if start > 0 {
        match text.find('\n') {
            Some(i) => text[i + 1..].to_string(),
            None => text,
        }
    } else {
        text
    }
}

/// 当前时刻 ISO 8601（`YYYY-MM-DDTHH:MM:SS.mmmZ`，对齐 JS `new Date().toISOString()`）。
///
/// 复用 stats-engine 既有的 `created_at_to_rfc3339`（无外部 time 依赖的 civil 算法）——
/// 不为一个时间戳新增 `chrono` / `time` 依赖。取不到系统时间（极端时钟异常）→ 空串，不 panic。
fn now_iso8601() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .and_then(polaris_stats_engine::created_at_to_rfc3339)
        .unwrap_or_default()
}

/// 当天日期 `YYYY-MM-DD`（备份 / 报告的默认文件名用）。取 [`now_iso8601`] 的日期段。
fn today_yyyy_mm_dd() -> String {
    let iso = now_iso8601();
    iso.split('T').next().unwrap_or("").to_string()
}

/// 把前端传来的类别串解析成枚举；空 / None → 全选（对齐 上游 `args?.categories?.length ? ... : [...ALL]`）。
/// 未知串**忽略**（不 throw）——前端若因版本差异传来新类，不该让整个导出失败。
fn parse_categories(raw: Option<Vec<String>>) -> Vec<BackupCategory> {
    let picked: Vec<BackupCategory> = raw
        .unwrap_or_default()
        .iter()
        .filter_map(|s| BackupCategory::from_wire(s))
        .collect();
    if picked.is_empty() {
        BACKUP_CATEGORIES.to_vec()
    } else {
        picked
    }
}

// ── 日志 ── 上游 `log-handlers.ts` ──

/// 批量日志事件 coalesce 间隔（ms）。对齐 上游 LogManager ~150ms 合批推送。
const LOG_BATCH_INTERVAL_MS: u64 = 150;

/// UI 不活跃期积压后**单批**最多补推的条数（对齐 上游 `MAX_PENDING_LOG_BATCH`）。
///
/// 超出即丢最旧、保最新（live tail 语义）：渲染端自身缓冲也是 500 行，补推更多只会被它当场切掉，
/// 白白多一次序列化 + 一次 webview 唤醒。**只截 UI 直播流**——落盘与环形缓冲不受影响，
/// 下一次 `logs:get` 水合仍能取到（且截断条数会 warn 出来，不静默）。
const MAX_PENDING_LOG_BATCH: usize = 500;

/// 批量日志推送任务的单次启动闸（首个 logs:get 携 AppHandle 时惰性起，幂等）。
static LOG_BATCH_STARTED: AtomicBool = AtomicBool::new(false);

/// `logging::LogRecord` → 渲染端 `LogEntry`（camelCase 契约：timestamp/level/message/source/_id）。
///
/// `_id` = 环形缓冲的全局单调 seq（[`crate::logging::LogRecord::seq`]）。**必须出境**：渲染端拿它当
/// 列表 key + 去重键 ——
///  - key：环形缓冲滑动（丢最旧）后剩余行的 key 不变；退化成 `timestamp-index` 时首元素一淘汰，
///    后面每一行的 index 全体前移 → React 认定整列换了身份，滚动期全量重渲并打断文本选区。
///  - 去重：本 emitter 是**单例**（`LOG_BATCH_STARTED` 只起一次），第二次进日志页时 `logs:get` 的水合
///    快照会与 emitter 下一 tick 的增量重叠一个 ≤150ms 的窗口 → 同一条日志渲染两遍。有单调 `_id`
///    才能在渲染端按「seq ≤ 已见最大 seq 即丢」精确去重。
fn log_record_to_entry(r: &crate::logging::LogRecord) -> Value {
    json!({
        "_id": r.seq,
        "timestamp": ts_ms_to_iso(r.ts_ms),
        "level": frontend_level(r.level),
        "message": r.message,
        "source": r.target,
    })
}

/// 后端级别标签 → 渲染端 `LogLevel`（'debug'|'info'|'warn'|'error'|'fatal'）：trace 归并入 debug。
fn frontend_level(level: &str) -> &str {
    if level == "trace" {
        "debug"
    } else {
        level
    }
}

/// epoch 毫秒 → ISO 8601（渲染端 `LogEntry.timestamp: string`）。复用 stats-engine 的 civil 算法
/// （不新增 chrono/time）；越界 → 原样毫秒串（不 panic）。
fn ts_ms_to_iso(ts_ms: u128) -> String {
    i64::try_from(ts_ms)
        .ok()
        .and_then(polaris_stats_engine::created_at_to_rfc3339)
        .unwrap_or_else(|| ts_ms.to_string())
}

/// 惰性起批量日志推送任务（首个 logs:get 触发）：每 ~150ms 拉环形缓冲增量 → broadcast
/// `EVENT_LOG_RECEIVED_BATCH`。
///
/// `from_cursor` 由调用方与 `logs:get` 的快照**同一把锁下同时取**（见
/// [`snapshot_with_cursor`](crate::logging::snapshot_with_cursor)）后传入：游标绝不能在本任务内部
/// 才取——spawn 到任务真正开跑之间是一段异步间隙，期间写入的日志 seq 会低于游标而被永久跨过（丢行）。
fn ensure_log_batch_emitter(app: &AppHandle, from_cursor: u64) {
    if LOG_BATCH_STARTED.swap(true, Ordering::SeqCst) {
        return; // 已起。
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut cursor = from_cursor;
        loop {
            tokio::time::sleep(Duration::from_millis(LOG_BATCH_INTERVAL_MS)).await;
            // UI 门控：不活跃时**不拉、不推、游标不动** —— 条目留在环里，下一个活跃 tick 一次性补推。
            if !ui_log_stream_active(&app) {
                continue;
            }
            let (recs, next) = crate::logging::records_from(cursor);
            if recs.is_empty() {
                continue;
            }
            // 游标按**全量**推进（含被截掉的那些）：截断只降 UI 直播流的量，不是「下次再发」——
            // 否则每 tick 都从同一批老条目重发，永远追不上洪流。
            cursor = next;
            let dropped = recs.len().saturating_sub(MAX_PENDING_LOG_BATCH);
            let batch: Vec<Value> = tail_capped(&recs, MAX_PENDING_LOG_BATCH)
                .iter()
                .map(log_record_to_entry)
                .collect();
            crate::events::broadcast(
                &app,
                crate::events::channel::EVENT_LOG_RECEIVED_BATCH,
                batch,
            );
            if dropped > 0 {
                // 自曝截断：不写出来的话，「UI 隐藏期间掉了 N 行直播」与「本来就没这几行」输出无区别。
                // 本条自身也会进环 → 下一批推给 UI，用户在日志页直接看得到。
                log::warn!(
                    "[log-batch] UI 直播流单批截断：丢弃最旧 {dropped} 条（仅 UI 直播，已落盘且仍在缓冲内）"
                );
            }
        }
    });
}

/// 日志直播流的 UI 活跃门控（契约 L81）：主窗口存在且既未隐藏也未最小化时才推。
///
/// # 为什么要门控
///
/// emitter 每 ~150ms 一批，每批都是一次 IPC 序列化 + webview 唤醒。窗口收进托盘 / 最小化时
/// **没有任何消费者**，却仍在按日志速率唤醒 webview —— sing-box 启动期洪流下尤其浪费。
///
/// # 判定为何长这样
///
/// - 窗口**不存在** → 判不活跃：C16 轻量模式会销毁主 webview，那时连渲染端都没有，推给谁都不是。
/// - 平台 API 出错（`is_visible` / `is_minimized` 返 Err）→ **一律判活跃**（fail-open）。判反的代价
///   不对称：误判「活跃」只多推几批；误判「不活跃」会让用户盯着的日志页静默冻住，而那正是日志页
///   存在的意义所在。
/// - 只看可见性，**不看焦点**：切到别的应用但窗口仍在屏幕上时日志必须继续流。
///
/// 注：上游的同名门控还含 `!isDragging`（win32 拖动期 modal loop）。Polaris 侧无拖动态基建，
/// 本函数只做可见性这一半；拖动期降流属另一批。
fn ui_log_stream_active(app: &AppHandle) -> bool {
    let Some(win) = app.get_webview_window("main") else {
        return false; // 主 webview 已销毁（C16 轻量模式）→ 无消费者。
    };
    win.is_visible().unwrap_or(true) && !win.is_minimized().unwrap_or(false)
}

/// 取尾部最多 `cap` 条（丢最旧、保最新 = live tail 语义）。`cap == 0` → 空。
///
/// 抽成纯函数是为了可测：截断策略若写反（取头部）表现为「UI 一直显示几分钟前的旧日志」，
/// 那种错在真机上极难与「日志停了」区分。
fn tail_capped<T>(recs: &[T], cap: usize) -> &[T] {
    if recs.len() > cap {
        &recs[recs.len() - cap..]
    } else {
        recs
    }
}

/// 上游 `LOGS_GET`：取日志缓冲（内存环形缓冲）+ 惰性起批量推送流。
///
/// `limit` = 只取最新 N 条（渲染端 LogsScreen 传 MAX_BUFFER）。
#[tauri::command]
pub fn logs_get(
    app: AppHandle,
    _state: State<'_, AppRuntime>,
    limit: Option<usize>,
) -> ApiResponse<Vec<Value>> {
    // 快照与流式起始游标同锁取 → 水合与增量流首尾相接，不重放、不丢行。
    let (recs, cursor) = crate::logging::snapshot_with_cursor(limit);
    ensure_log_batch_emitter(&app, cursor);
    let entries: Vec<Value> = recs.iter().map(log_record_to_entry).collect();
    ApiResponse::ok(entries)
}

/// 上游 `LOGS_CLEAR`：清日志缓冲 —— **两侧一起清**。
///
/// # 为什么不能只清本地环
///
/// 核自己也留着一份日志环（`SubscribeLog` 的 3000 行历史，`daemon/attached_service.go` 的
/// `defaultAttachedLogMaxLines`）。只清本地的话，核日志 relay 一旦重订阅（断线重连 / 重启后再进
/// 日志页），那份历史又整份回来 —— 用户看到的是「清了又自己长回来」。故本命令在清本地环之后，
/// 再对运行核发一次 `ClearLogs`。
///
/// 核没在跑 / 管理 API 连不上 / 调用失败 → **只 debug 一行，不算失败**：本地环已经清了，那是用户
/// 点这颗按钮的主要诉求；核侧那份历史随核退出本就一起没了。为一个 best-effort 的补充动作把整条
/// 命令判失败，只会让「清空日志」在核未运行时红一个没有意义的错。
#[tauri::command]
pub async fn logs_clear(state: State<'_, AppRuntime>) -> Result<ApiResponse<()>, ()> {
    crate::logging::clear();
    if let Ok((port, secret)) = crate::commands::proxy::management_endpoint(&state) {
        match polaris_singbox_grpc::SingBoxApiClient::connect(
            polaris_singbox_grpc::Endpoint::new("127.0.0.1", port),
            secret,
        )
        .await
        {
            Ok(c) => {
                if let Err(e) = c.clear_logs().await {
                    log::debug!("清空核侧日志环失败（本地已清，不阻断）：{e}");
                }
            }
            Err(e) => log::debug!("清空核侧日志环：管理 API 连接失败（本地已清，不阻断）：{e}"),
        }
    }
    Ok(ok_void())
}

// ── 核在跑的真实日志级别（`logs:runtimeLevel`）─────────────────────────────────
//
// # 它回答的问题，以及为什么 `config.logLevel` 回答不了
//
// 日志页的级别分段控件显示的是**「我写下的值」**（`useEffectiveConfig().logLevel`）。那个值与
// 核实际在跑的级别有两条已实证的分叉，**都不是渲染端能自己补偿的**：
//
//  1. 隐私锁开启时，生成侧走 `LogLevel::effective(privacy)` 把 info/debug 抬到 warn
//     （`config-engine/src/builder/log.rs`）——核跑 warn，而 UI 一直显示 info，零补偿。
//  2. 配置暂存态下改级别命中 staged 分支即 `return`，**零 IPC 写、零磁盘写**——分段控件已经高亮了
//     新级别，核仍按旧级别记录。
//
// 现有工具栏那颗 `i` 的浮窗只是**文案提示**（「sing-box 侧需重启内核后生效」），它说的是一条通则，
// 不是此刻的事实：它既不知道隐私锁把级别抬到了哪里，也不知道你暂存的那次改动有没有落地。
// 本命令把核的值读回来，让那句话变成可核对的事实。
//
// # 为什么读不到时不回落成某个级别
//
// 核未运行时上游 `GetDefaultLogLevel` 必然报错（先 RLock 检查 `serviceStatus.Status ∈
// {STARTING, STARTED}`，否则 `os.ErrInvalid`）。此时若「兜底」成 `config.logLevel`，
// 显示出来的恰恰又是那个「我写下的值」——自证退化成它本要揭穿的那句谎，只是换了个地方说。
// 故一律回 `level: null` + 一个说明为什么读不到的 `reason`。

/// 读不到时的两种理由（`reason` 取值）。UI 据此分别呈现，**不得压成同一句**：
/// 「核没跑」是常态、无需惊动用户；「读不到」是异常，值得让人看见。
const REASON_NOT_RUNNING: &str = "notRunning";
const REASON_UNAVAILABLE: &str = "unavailable";

/// `daemon::LogLevel` → sing-box 配置里那套小写级别名（`panic`/`fatal`/…/`trace`）。
///
/// 走 prost 生成的 `as_str_name()` 再小写，而不是自己手写一张 match 表：手写表会在上游扩枚举时
/// 静默漏项，而 `as_str_name` 由 proto 生成、与 `proto/started_service.proto` 同步演进。
///
/// 注意这**不是** `config-engine::user_config::LogLevel`（五档、严重度升序）；sing-box 侧七档且
/// 序相反，多出的 `panic`/`trace` 本仓生成侧永不写入，但读侧必须能原样说出来。
fn runtime_level_name(level: polaris_singbox_grpc::daemon::LogLevel) -> String {
    level.as_str_name().to_ascii_lowercase()
}

/// 读回核**此刻实际**在用的日志级别（管理 API gRPC `GetDefaultLogLevel`）。
///
/// 恒返成功信封（读不到不是错误，是一种要如实呈现的状态）：
/// - `{ level: "warn", reason: null }` —— 核在跑，这是它真正在用的级别。
/// - `{ level: null, reason: "notRunning" }` —— 核没在跑（我们自己的状态就知道，连都不用连）。
/// - `{ level: null, reason: "unavailable" }` —— 核在跑但读不到（正在启动 / 管理 API 连不上 /
///   核返回了本仓不认识的级号）。
#[tauri::command]
pub async fn logs_runtime_level(state: State<'_, AppRuntime>) -> Result<ApiResponse<Value>, ()> {
    Ok(match read_runtime_log_level(&state).await {
        Ok(level) => ApiResponse::ok(json!({ "level": level, "reason": Value::Null })),
        Err(reason) => ApiResponse::ok(json!({ "level": Value::Null, "reason": reason })),
    })
}

/// 查询会话级诊断模式。状态只在 Rust 进程内，渲染屏卸载/重挂不会误关；应用重启后自然回到 false。
#[tauri::command]
pub fn logs_diagnostic_state() -> ApiResponse<bool> {
    ApiResponse::ok(crate::logging::session_diagnostic_enabled())
}

/// 开关会话级诊断模式：临时把应用 sink + sing-box 实时 relay 抬到至少 DEBUG，不写配置、不重启核。
#[tauri::command]
pub fn logs_set_diagnostic(enabled: bool) -> ApiResponse<bool> {
    ApiResponse::ok(crate::logging::set_session_diagnostic(enabled))
}

/// [`logs_runtime_level`] 的取值本体。**诊断导出复用同一条腿**。
///
/// 抽出来的理由不是省几行：`config.logLevel`（盘上写的）与核实际在跑的级别有两条已实证分叉
/// （隐私锁抬级 / 配置暂存态未落盘，见本节顶部注释）。日志页那条腿已经改成读回真值，而
/// **诊断导出那条腿此前仍在直接报 `config.logLevel`** —— 同一个谎换个地方说，而且说在
/// 一份「用来给别人做根因判断」的报告头部，比在 UI 上说危害更大。
/// 两条腿共用一个取值点，才谈得上不会再次分叉。
async fn read_runtime_log_level(state: &State<'_, AppRuntime>) -> Result<String, &'static str> {
    let (port, secret) =
        crate::commands::proxy::management_endpoint(state).map_err(|_| REASON_NOT_RUNNING)?;
    let client = match polaris_singbox_grpc::SingBoxApiClient::connect(
        polaris_singbox_grpc::Endpoint::new("127.0.0.1", port),
        secret,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            log::debug!("读核日志级别：管理 API 连接失败：{e}");
            return Err(REASON_UNAVAILABLE);
        }
    };
    match client.default_log_level().await {
        Ok(level) => Ok(runtime_level_name(level).to_string()),
        Err(e) => {
            log::debug!("读核日志级别失败（核可能仍在启动）：{e}");
            Err(REASON_UNAVAILABLE)
        }
    }
}

// ── Shell ── 上游 `shell:openExternal` ──

/// 上游 `SHELL_OPEN_EXTERNAL`：用系统默认浏览器打开外链（tauri-plugin-shell）。
///
/// 注：`shell.open` 在 tauri-plugin-shell 2.x 标记 deprecated（推荐 tauri-plugin-opener）；
/// 切换属独立依赖决策，此处暂用 shell 并抑制 deprecation。
#[tauri::command]
#[allow(deprecated)]
pub fn shell_open_external(app: AppHandle, url: String) -> ApiResponse<()> {
    if let Err(e) = app.shell().open(&url, None) {
        return ApiResponse::err(format!("{e}"));
    }
    ok_void()
}

/// 原型 log 工具栏「目录」按钮（`:2065` `data-act="open-log-dir"`）：在系统文件管理器里打开日志目录。
///
/// # 为什么打开的是**配置目录**而不是 `logs/`
///
/// 两份日志不在同一层：应用日志是 `<configDir>/logs/polaris.log`，内核日志是
/// `<configDir>/singbox.log`（`runtime/proxy.rs` 的 `log_file_path`）。只开 `logs/` 会让用户
/// **最常要的那一份**（singbox.log）不在视野里，还得自己往上翻一级。故开二者的共同父目录。
///
/// # 为什么在后端一步做完，而不是「后端返路径 + 前端 openExternal」
///
/// 那样要两次 IPC，且把一个真实文件系统路径交给渲染端只为再传回来。一步做完还让失败只有一个出口：
/// 路径解析与 `shell.open` 任一失败都是同一条 clean error，前端不必分辨「拿到了路径但打不开」。
/// `#[allow(deprecated)]` 同 [`shell_open_external`]：tauri-plugin-shell 2.x 的 `open` 标了 deprecated。
///
/// **属性行后面别加行注释**：`scripts/check-ipc-args.mjs` 的命令表达式按
/// `#[tauri::command]` + 若干属性 + `pub fn` 连续匹配，中间插一行注释会让它认不出这条命令，
/// 报成「前端 invoke 了一个不存在的命令」。
#[tauri::command]
#[allow(deprecated)]
pub fn logs_open_dir(app: AppHandle, state: State<'_, AppRuntime>) -> ApiResponse<()> {
    let dir = state.config().dir().to_path_buf();
    if let Err(e) = app.shell().open(dir.to_string_lossy(), None) {
        return ApiResponse::err(format!("{e}"));
    }
    ok_void()
}

// ── sing-box 官方面板 ── Polaris helper-handlers dashboard 部分 ──

/// sing-box 官方面板内窗口 label（单例：已存在则聚焦复用，对齐 上游 `dashboardWindow`）。
const DASHBOARD_WINDOW_LABEL: &str = "singbox-dashboard";

/// 上游 `OPEN_SINGBOX_DASHBOARD`：打开 sing-box 官方面板（应用内 webview 窗，dashboard #55）。
///
/// 对齐 上游 helper-handlers `OPEN_SINGBOX_DASHBOARD`：开一个内窗口加载核 serve 的运行期
/// `http://127.0.0.1:<clash_api_port>/dashboard/`，并经 `initialization_script`（= Electron preload 等价：
/// document-start 于面板同源执行）在面板 JS 读 localStorage 前预写后端连接——**面板只读 localStorage、不读 URL
/// 参数**（上游 真机 + 面板源码实证），故必须此路径注入而非 URL query。写两个键覆盖各版本面板：
///  - `sing-box-dashboard.servers`：权威 `{servers:[{id,name,url,secret}],activeId}`；
///  - `sing-box-dashboard.server` ：旧版扁平 `{url,secret}` 迁移键。
///
/// 安全（H1）：本窗加载第三方面板代码且 localStorage 内含 clash_api secret → `on_navigation` 锁死导航边界，
/// 仅允许停留在本地 api service 源，跨源 http(s) 一律拦（防 secret 外泄）。代理未运行（端口=0）→ 不开窗。
///
/// 系统右键菜单：本窗与本仓三个前端入口同口径禁掉（第二条 `initialization_script`，见
/// [`DISABLE_CONTEXT_MENU_SCRIPT`]）。`initialization_script` 是 **push 语义**（tauri 2.11.5
/// `webview/mod.rs`：`initialization_scripts.push(..)`），两次调用都会注入，不互相覆盖。
#[tauri::command]
pub fn open_singbox_dashboard(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    locale: Option<String>,
) -> ApiResponse<Value> {
    let info = state.proxy().dashboard_connection();
    if !info.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return ApiResponse::ok(json!({ "ok": false }));
    }
    // 已有面板窗 → 聚焦复用（对齐 上游「已存在则 focus」，不重复开窗）。
    if let Some(win) = app.get_webview_window(DASHBOARD_WINDOW_LABEL) {
        let _ = win.set_focus();
        return ApiResponse::ok(json!({ "ok": true }));
    }
    let url = info
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let api_url = info
        .get("apiUrl")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let secret = info
        .get("secret")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if url.is_empty() || api_url.is_empty() {
        return ApiResponse::ok(json!({ "ok": false }));
    }
    // 面板后端 url 用 host:port（无协议前缀），与面板归一化（去 http:// 前缀 + 去尾斜杠）后的存量格式一致。
    let bare = api_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string();
    let script = build_dashboard_preload_script(
        &bare,
        &secret,
        map_locale_to_dashboard_lang(locale.as_deref()),
    );

    let parsed = match url.parse::<tauri::Url>() {
        Ok(u) => u,
        Err(e) => return ApiResponse::err(format!("面板 URL 非法：{e}")),
    };
    // 仅允许停留在本地 api service 源（`http://127.0.0.1:<port>/…`）；跨源 http(s) 拦下。内部 scheme 放行。
    let allowed_prefix = format!("{api_url}/");
    let win = WebviewWindowBuilder::new(&app, DASHBOARD_WINDOW_LABEL, WebviewUrl::External(parsed))
        .title("sing-box Dashboard")
        .inner_size(1100.0, 760.0)
        .min_inner_size(800.0, 600.0)
        // preload 等价：document-start 于面板同源预写 localStorage（读前已写）。
        .initialization_script(&script)
        // 同一条 preload 通道再挂一次系统右键菜单禁用（面板是第三方产物，改不了它的 JS）。
        .initialization_script(DISABLE_CONTEXT_MENU_SCRIPT)
        .on_navigation(move |u| {
            let scheme = u.scheme();
            if scheme == "http" || scheme == "https" {
                u.as_str().starts_with(&allowed_prefix)
            } else {
                true // tauri:/about:/data: 等内部 scheme 放行
            }
        })
        .build();
    match win {
        Ok(_) => ApiResponse::ok(json!({ "ok": true })),
        Err(e) => ApiResponse::err(format!("建面板窗失败：{e}")),
    }
}

/// Electron/系统 locale → 面板合法语言码（源码实证 `en/zh-Hans/zh-Hant/fa/ru`；前缀匹配处理 zh-CN/zh-TW/fa-IR 等）。
fn map_locale_to_dashboard_lang(locale: Option<&str>) -> &'static str {
    let l = locale.unwrap_or("").to_ascii_lowercase();
    if l.starts_with("zh-hant")
        || l.starts_with("zh-tw")
        || l.starts_with("zh-hk")
        || l.starts_with("zh-mo")
    {
        "zh-Hant"
    } else if l.starts_with("zh") {
        "zh-Hans"
    } else if l.starts_with("fa") {
        "fa"
    } else if l.starts_with("ru") {
        "ru"
    } else {
        "en"
    }
}

/// 构造面板 preload 脚本（注入 localStorage 后端连接）。payload 经 serde_json **双重序列化**嵌为 JS 字符串字面量
/// （secret 含引号/反斜杠也不破——serde 产的字符串本身即合法 JS 字面量），杜绝脚本注入。对齐 上游 `dashboard-preload.ts`。
fn build_dashboard_preload_script(bare_url: &str, secret: &str, lang: &str) -> String {
    // 单一 server（id 固定即可，面板按 activeId 选中）。
    const SERVER_ID: &str = "polaris";
    let servers_val = json!({
        "servers": [{ "id": SERVER_ID, "name": "", "url": bare_url, "secret": secret }],
        "activeId": SERVER_ID,
    });
    let legacy_val = json!({ "url": bare_url, "secret": secret });
    // localStorage 值须为 string → 先 stringify 成 JSON 字符串，再序列化成 JS 字符串字面量嵌入脚本。
    let servers_lit =
        serde_json::to_string(&servers_val.to_string()).unwrap_or_else(|_| "\"\"".into());
    let legacy_lit =
        serde_json::to_string(&legacy_val.to_string()).unwrap_or_else(|_| "\"\"".into());
    let lang_lit = serde_json::to_string(lang).unwrap_or_else(|_| "\"en\"".into());
    format!(
        "(function(){{try{{var ls=window.localStorage;if(!ls)return;\
ls.setItem('sing-box-dashboard.servers',{servers_lit});\
ls.setItem('sing-box-dashboard.server',{legacy_lit});\
if(!ls.getItem('sing-box-dashboard.language')){{ls.setItem('sing-box-dashboard.language',{lang_lit});}}\
}}catch(e){{}}}})();"
    )
}

/// 面板窗的系统右键菜单禁用脚本（document-start 注入）—— 本仓第四个 webview 入口。
///
/// 前三个（主窗 / 托盘浮层 / 更新弹窗）在前端各调一次 `disableNativeContextMenu()`
/// （`ui/src/lib/native-context-menu.ts`）。面板窗**够不到那条腿**：页面是
/// `scripts/fetch-dashboard.mjs` 拉下来的第三方产物、由核 serve，我们改不了它的 JS，
/// 只能经 `initialization_script`（= preload 等价，document-start 同源执行）从外面挂同一条监听。
///
/// 判据与 TS 侧**逐条对齐**（可编辑文本控件放行系统菜单以便粘贴 / 复制，其余一律禁）：
/// input 类型白名单、label→control 解析、disabled 不放行 / readonly 放行、contenteditable 继承。
/// 完整论证在 TS 那份头注，此处不复述。
///
/// ⚠️ 这是**跨语言的第二份实现**（TS 那份跑不到本窗里）。防漂移靠
/// `ui/src/lib/native-context-menu.test.ts` 的 parity 断言：它把两边的类型白名单抠出来比对，
/// 只改一边即转红。
const DISABLE_CONTEXT_MENU_SCRIPT: &str = "(function(){\
var T=['text','search','url','tel','email','password','number'];\
document.addEventListener('contextmenu',function(e){\
var el=e.target;\
if(el&&el.closest){\
var h=el.closest('input, textarea')||(el.closest('label')||{}).control||el;\
var g=(h.tagName||'').toUpperCase();\
if(g==='TEXTAREA'?!h.disabled:g==='INPUT'\
?(!h.disabled&&T.indexOf((h.type||'text').toLowerCase())>=0)\
:h.isContentEditable===true)return;\
}\
e.preventDefault();\
});})();";

/// sing-box 官方面板资源缓存目录名（`<config_dir>/singbox-dashboard`）。核首启时若该目录为空，从
/// `download_url` 拉 zip 解此 + 写 `.etag`；「刷新面板资源」清此目录使核下次启动重拉。对齐 上游
/// `getSingboxDashboardDir()`（`<userData>/singbox-dashboard`，utils/paths.ts）。
const SINGBOX_DASHBOARD_DIR: &str = "singbox-dashboard";

/// 清 sing-box 面板资源缓存目录（best-effort，幂等）。抽出为纯函数便于单测喂临时目录。
///
/// 对齐 上游 `clearSingboxDashboardCache`（helper-handlers.ts）：`fs.rmSync(dir, {recursive, force})`——
/// 删失败 / 目录不存在（ENOENT）均**不致命**（核启动若目录仍非空只是沿用旧资源，下次仍可重试清理），
/// 故忽略错误。
fn clear_singbox_dashboard_cache(dashboard_dir: &Path) {
    let _ = std::fs::remove_dir_all(dashboard_dir);
}

/// 上游 `REFRESH_SINGBOX_DASHBOARD`：清面板资源缓存目录（核下次启动重拉）。
///
/// 对齐 上游 helper-handlers `REFRESH_SINGBOX_DASHBOARD`：清 `<config_dir>/singbox-dashboard` → 核下次
/// 启动（或下次配置变更触发的 `switch_mode` 重启）重拉新 zip。**不在此触发重启**（保「不打断连接」语义）
/// ——UI 提示用户重连 / 下次启动生效。删目录幂等，不存在不报错。
#[tauri::command]
pub fn refresh_singbox_dashboard(state: State<'_, AppRuntime>) -> ApiResponse<Value> {
    clear_singbox_dashboard_cache(&state.config().dir().join(SINGBOX_DASHBOARD_DIR));
    ApiResponse::ok(json!({ "ok": true }))
}

/// 上游 `GET_SINGBOX_DASHBOARD_CONNECTION`：取面板连接信息（URL + secret）。
#[tauri::command]
pub fn get_singbox_dashboard_connection(state: State<'_, AppRuntime>) -> ApiResponse<Value> {
    ApiResponse::ok(state.proxy().dashboard_connection())
}

// ── 数据备份 / 恢复 ── 上游 `backup-handlers.ts` ──

/// 弹「保存文件」框，返回用户选定路径（取消 → None）。
///
/// 用**回调式** API + oneshot，而非 `blocking_save_file` —— 后者禁止在主线程调用（会死锁）；
/// 本 command 是 `async fn`，回调式是官方推荐路径。
async fn ask_save_path(app: &AppHandle, default_name: &str) -> Option<PathBuf> {
    let lang = crate::i18n::app_lang(app);
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(t(lang, key::NATIVE_BACKUP_EXPORT_TITLE))
        .set_file_name(default_name)
        .add_filter(t(lang, key::NATIVE_BACKUP_FILE_TYPE), &["polaris-backup"])
        .add_filter(t(lang, key::NATIVE_ALL_FILES), &["*"])
        .save_file(move |p| {
            let _ = tx.send(p);
        });
    rx.await.ok().flatten().and_then(|p| p.into_path().ok())
}

/// 弹「打开文件」框，返回用户选定路径（取消 → None）。
async fn ask_open_path(app: &AppHandle) -> Option<PathBuf> {
    let lang = crate::i18n::app_lang(app);
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(t(lang, key::NATIVE_BACKUP_IMPORT_TITLE))
        .add_filter(t(lang, key::NATIVE_BACKUP_FILE_TYPE), &["polaris-backup"])
        .add_filter(t(lang, key::NATIVE_JSON_FILE_TYPE), &["json"])
        .add_filter(t(lang, key::NATIVE_ALL_FILES), &["*"])
        .pick_file(move |p| {
            let _ = tx.send(p);
        });
    rx.await.ok().flatten().and_then(|p| p.into_path().ok())
}

/// 上游 `BACKUP_EXPORT`：选择性导出（按 categories）。
///
/// `categories` 缺省 / 空 → 全 6 类。备份文件形与 上游 `BackupFileFormat` 一致（可互相导入）。
/// **clashApiSecret / privacyPassword 恒不入备份**（由 `pick_categories` 的排除表保证，见 store::backup）。
#[tauri::command]
pub async fn backup_export(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    categories: Option<Vec<String>>,
) -> Result<ApiResponse<Value>, ()> {
    let config = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return Ok(ApiResponse::err(format!("{e}"))),
    };
    let selected = parse_categories(categories);
    let picked = pick_categories(&config, &selected);

    let backup = json!({
        "version": BACKUP_FILE_VERSION,
        "appVersion": app.package_info().version.to_string(),
        "platform": node_platform(),
        "exportedAt": now_iso8601(),
        "config": picked,
    });

    let default_name = format!("polaris-backup-{}.polaris-backup", today_yyyy_mm_dd());
    let Some(path) = ask_save_path(&app, &default_name).await else {
        return Ok(ApiResponse::ok(
            json!({ "success": false, "error": "cancelled" }),
        ));
    };

    let body = match serde_json::to_string_pretty(&backup) {
        Ok(s) => s,
        Err(e) => return Ok(ApiResponse::err(format!("{e}"))),
    };
    if let Err(e) = std::fs::write(&path, body) {
        return Ok(ApiResponse::ok(
            json!({ "success": false, "error": format!("{e}") }),
        ));
    }
    Ok(ApiResponse::ok(json!({
        "success": true,
        "filePath": path.to_string_lossy(),
    })))
}

/// 上游 `BACKUP_IMPORT_PICK`：弹文件框 + 解析 → 返回含哪些类 + 各类数量（**不 apply**）。
#[tauri::command]
pub async fn backup_import_pick(app: AppHandle) -> Result<ApiResponse<Value>, ()> {
    let Some(path) = ask_open_path(&app).await else {
        return Ok(ApiResponse::ok(json!({ "canceled": true })));
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(ApiResponse::ok(
            json!({ "canceled": false, "error": "read_failed" }),
        ));
    };
    let parsed = match parse_backup_content(&raw) {
        Ok(p) => p,
        Err(code) => return Ok(ApiResponse::ok(json!({ "canceled": false, "error": code }))),
    };
    let available = detect_categories(&parsed.config);
    let mut counts = serde_json::Map::new();
    for cat in &available {
        counts.insert(
            cat.as_str().to_string(),
            json!(count_category(&parsed.config, *cat)),
        );
    }
    Ok(ApiResponse::ok(json!({
        "canceled": false,
        "filePath": path.to_string_lossy(),
        "available": available,
        "counts": counts,
    })))
}

/// 上游 `BACKUP_IMPORT_APPLY`：按所选类**整类替换 + 空跳过** + 跨平台 sanitize + 保存。
///
/// 失效 `selectedServerId` 已在 `merge_categories` 末尾归零（`validate_config` 对失效引用是 Err、非归零，
/// 不兜底会令整份导入失败）。保存走 `save_full`（内部再跑 sanitize + validate）。
///
/// 存盘成功后必须走 `broadcast_config_changed`：那是本仓配置变更的唯一汇流点（前端 store 对账 +
/// `switch_mode` 热切换/重启判定 + `set_level` 跟随 logLevel）。本命令的落盘腿
/// （[`crate::commands::config::backup_import_save_core`]）不含广播 → 少了这一步，导入的备份只落磁盘、
/// 运行核与前端一无所知（一份含 logLevel/节点变更的备份导入后静默不生效）。
#[tauri::command]
pub async fn backup_import_apply(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    file_path: String,
    categories: Vec<String>,
) -> Result<ApiResponse<Value>, ()> {
    let selected: Vec<BackupCategory> = categories
        .iter()
        .filter_map(|s| BackupCategory::from_wire(s))
        .collect();
    if file_path.is_empty() || selected.is_empty() {
        return Ok(ApiResponse::ok(
            json!({ "success": false, "error": "invalid_args" }),
        ));
    }
    let Ok(raw) = std::fs::read_to_string(&file_path) else {
        return Ok(ApiResponse::ok(
            json!({ "success": false, "error": "read_failed" }),
        ));
    };
    let parsed = match parse_backup_content(&raw) {
        Ok(p) => p,
        Err(code) => return Ok(ApiResponse::ok(json!({ "success": false, "error": code }))),
    };

    let current = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return Ok(ApiResponse::err(format!("{e}"))),
    };
    let mut outcome = merge_categories(&current, &parsed.config, &selected);

    // 仅当导入了自定义规则才需 sanitize（其余类无进程规则）。
    let mut cross_disabled = 0usize;
    if selected.contains(&BackupCategory::CustomRules) {
        cross_disabled = sanitize_cross_platform_rules(
            &mut outcome.config,
            parsed.platform.as_deref(),
            node_platform(),
        );
        if cross_disabled > 0 {
            log::info!(
                "[backup] 跨平台导入（{:?}→{}）：禁用 {cross_disabled} 条进程规则（保留供重映射）",
                parsed.platform,
                node_platform()
            );
        }
    }

    // 落盘前的三条策略 + 保存全部收口在 [`config::backup_import_save_core`]（见该函数文档）：
    // 回填隐私 hash（备份导出侧脱敏，不回填 = 导入即拆锁）、以本机磁盘回正后端权威字段（外机 MRU /
    // geo 元数据不得灌进本机）、全局 UA 变更时作废受影响订阅的条件 GET 验证器（不清 = 换 UA 后恒 304）。
    let mut restored = outcome.config.clone();
    if let Err(e) =
        crate::commands::config::backup_import_save_core(state.config(), &current, &mut restored)
    {
        return Ok(ApiResponse::ok(
            json!({ "success": false, "error": format!("{e}") }),
        ));
    }
    // 恢复后二次 load_full 重走完整迁移链（migrate_all）再广播：备份可能来自旧版本（上游/旧 Polaris），含旧 shape
    // 字段（legacy DomainRule / subscriptionUpdateViaProxy / 未迁移 tunStack 等）。`save_full` 只 sanitize+validate、
    // **不跑迁移链**，直接广播 restored 会让旧 shape 未迁移即入核/下发前端。二次 load_full 触发 migrate_all，
    // 广播迁移后配置。load 异常（刚存的合法配置几乎不可能）→ 回落广播 restored（仍带回填后的私密字段，不裸奔）。
    // 广播**回填后**（restored / 其迁移形）而非 outcome.config：后者 server 私密字段已被导出侧脱敏抹平，入核 = 缺密钥热切换。
    let broadcast_cfg = state.config().load_full().unwrap_or(restored);
    crate::commands::config::broadcast_config_changed(&app, &broadcast_cfg);

    let info = build_backup_info(&outcome.config, cross_disabled);
    let skipped: Vec<&str> = outcome.skipped.iter().map(|c| c.as_str()).collect();
    let mut out = json!({ "success": true, "info": info });
    if !skipped.is_empty() {
        out["skipped"] = json!(skipped);
    }
    Ok(ApiResponse::ok(out))
}

/// 上游 `BACKUP_GET_INFO`：当前配置摘要。
#[tauri::command]
pub fn backup_get_info(state: State<'_, AppRuntime>) -> ApiResponse<Value> {
    match state.config().current() {
        Ok(c) => ApiResponse::ok(json!(build_backup_info(&c, 0))),
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

// ── 诊断报告 ── 上游 `diagnostic-handlers.ts` ──

/// 上游 `DIAGNOSTIC_EXPORT`：导出诊断报告（单 Markdown，**脱敏**）。
///
/// # 红线
///
/// 报告会被贴到公开 issue → **绝不含明文密钥**。脱敏不在本函数做，而是收口在
/// [`polaris_stats_engine::assemble_diagnostic_report`]：它吃**原始** config、内部统一脱敏，
/// 本层拿不到绕过脱敏的入口（见该函数文档 §K7.1）。本层只负责 IO：读配置 / 读日志 / 取版本号。
///
/// # 生成的 sing-box 配置从哪来
///
/// 读 `runtime_config_path()`（`singbox-runtime.json`）——那是**实际下发给内核的那一份**，
/// 比重新生成一次更真（#57 类问题一眼可见 DNS/route 根因；重新生成会掩盖「落盘的和以为的不一致」这类 bug）。
/// 核从未启动 → 文件不存在 → 报告里注明，不阻断导出。
#[tauri::command]
pub async fn diagnostic_export(
    app: AppHandle,
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<Value>, ()> {
    let config = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return Ok(ApiResponse::err(format!("{e}"))),
    };

    // 实际下发给内核的配置（非重新生成）。取不到 → 注明原因，不阻断导出。
    let runtime_cfg_path = state.proxy().runtime_config_path();
    let singbox_config: Value = match std::fs::read_to_string(&runtime_cfg_path) {
        Ok(s) => serde_json::from_str(&s)
            .unwrap_or_else(|e| json!({ "error": format!("运行期配置解析失败: {e}") })),
        Err(_) => json!({ "error": "(核未启动过，无运行期 sing-box 配置)" }),
    };

    let dir = state.config().dir().to_path_buf();
    let app_log_tail = read_tail(&dir.join("logs").join("polaris.log"), LOG_TAIL_BYTES);
    let singbox_log_tail = read_tail(&dir.join("singbox.log"), LOG_TAIL_BYTES);

    let status = state.proxy().status();
    // 报告里这一格必须是**核实际在跑的级别**，不是盘上写的那个。
    //
    // 直接报 `config.logLevel` 会在两种常见情形下说谎：隐私锁开着时生成侧把 info/debug 抬到了
    // warn（`config-engine/src/builder/log.rs`），配置暂存态下改的级别根本没落盘。收报告的人
    // 据「当前级别 info」去判断「为什么日志里没有 DNS 明细」，会一路推到错的地方 ——
    // 上游 issue #347 的诊断包上就实际发生过这件事（头部提示与日志内容对不上）。
    let configured_level = config
        .get("logLevel")
        .and_then(Value::as_str)
        .unwrap_or("info")
        .to_string();
    // 核没跑 / 读不到时**不悄悄回落**成配置值冒充实际值，而是如实标注它的来历。
    let (log_level, level_is_runtime) = match read_runtime_log_level(&state).await {
        Ok(level) => (level, true),
        Err(_) => (configured_level.clone(), false),
    };
    // 提示：当前级别不含连接明细且日志已现连接/DNS 类错误 → 建议把级别拨到 DEBUG 复现。
    //
    // **这句指引在本批被改写**：原文让用户去点「开启诊断采集」，那个按钮连同它背后整条
    // `diagnosticCapture` 机制已删除 —— 核日志现在经 `SubscribeLog` 全级别送来、级别筛在客户端，
    // 把日志页级别拨到 DEBUG 即刻生效，**不需要**改配置也不需要重启内核。
    let lower = app_log_tail.to_lowercase();
    let wants_deeper = log_level != "debug"
        && log_level != "trace"
        && TROUBLE_MARKERS.iter().any(|m| lower.contains(m));
    let hint = wants_deeper.then(|| {
        // 级别的来历要写进这句话本身：读回来的是事实，回落的是「我写下的值」，
        // 后者恰恰可能就是与实际不符的那个 —— 不标来历，这句提示会把读报告的人带偏。
        let origin = if level_is_runtime {
            format!("当前内核实际运行在 {log_level} 级别")
        } else {
            format!("配置中的日志级别为 {log_level}（内核未运行或读不到，未能核对实际级别）")
        };
        format!(
            "{origin}，未含 DNS 解析等连接详情，但日志中已出现连接/DNS 类错误。\
建议到 日志 页把级别切到 DEBUG（即刻生效，无需重启内核），复现问题后再次导出可获得更完整的根因数据。"
        )
    });

    let source = polaris_stats_engine::DiagnosticReportSource {
        generated_at: now_iso8601(),
        app: AppSection {
            polaris_version: app.package_info().version.to_string(),
            core_version: config
                .get("coreVersion")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            os: format!("{} {}", node_platform(), std::env::consts::ARCH),
        },
        runtime: RuntimeSection {
            proxy_mode: config
                .get("proxyMode")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            proxy_mode_type: config
                .get("proxyModeType")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            proxy_running: status.running,
            started_via_helper: Some(status.started_via_helper),
            helper_status: None,
            system_proxy: None,
            effective_dns: None,
            node_domain_resolver: config
                .get("dnsConfig")
                .and_then(|d| d.get("nodeDomainResolver"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            log_level,
            // 两轴计数由 ProxyRuntime 持有并喂数（§O1 喂数缺口已接线）：
            // - 慢起轴 last_start_ready_retries：起核就绪门累计（proxy.rs wait_ready）。
            // - 核崩轴 restart_count：读时从 CrashRecoveryMachine 投影（单一真值，不并行记）。
            counters: state.proxy().diagnostic_counters(),
        },
        user_config: config,
        singbox_config,
        // #57：节点 outbound.server 恒为域名（不烧 IP）→ 无额外预解析 IP 需补脱敏。
        // 若未来引入 resolve-ahead，预解析出的节点 IP 必须从这里传入，否则明文漏进报告。
        extra_addresses: Vec::new(),
        app_log_tail,
        singbox_log_tail,
        hint,
    };
    let markdown = polaris_stats_engine::assemble_diagnostic_report(&source);

    let default_name = format!("polaris-diagnostic-{}.md", today_yyyy_mm_dd());
    let lang = crate::i18n::app_lang(&app);
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(t(lang, key::NATIVE_DIAGNOSTIC_EXPORT_TITLE))
        .set_file_name(&default_name)
        // "Markdown" 是格式名不是文案（五语种同名），刻意不进 locale。
        .add_filter("Markdown", &["md"])
        .add_filter(t(lang, key::NATIVE_ALL_FILES), &["*"])
        .save_file(move |p| {
            let _ = tx.send(p);
        });
    let Some(path) = rx.await.ok().flatten().and_then(|p| p.into_path().ok()) else {
        return Ok(ApiResponse::ok(
            json!({ "success": false, "error": "cancelled" }),
        ));
    };
    if let Err(e) = std::fs::write(&path, markdown) {
        return Ok(ApiResponse::ok(
            json!({ "success": false, "error": format!("{e}") }),
        ));
    }
    Ok(ApiResponse::ok(json!({
        "success": true,
        "filePath": path.to_string_lossy(),
    })))
}

/// 上游 `LOGS_EXPORT`：导出**纯日志**（非诊断包）。
///
/// 与 [`diagnostic_export`] 是**两种产物**（对齐原型 log 工具栏的两个按钮）：
/// - 本命令 = app.log + singbox.log 原文拼接，**不含配置、不含版本号**。
/// - `diagnostic_export` = 脱敏配置 + 版本号 + 运行态 + 日志的完整诊断包。
///
/// # 脱敏边界（重要，勿误当等价物）
///
/// 纯日志导出**只做节点身份打码**（域名/IP/SNI/节点名 → 占位符），因为它压根不含配置块，
/// 没有密钥键可打。这是「给自己看/发给客服」的产物；**要贴公开 issue 请用诊断包**。
/// 报告头已如实声明这一点，不让用户误以为它等同诊断包的脱敏强度。
#[tauri::command]
pub async fn logs_export(
    app: AppHandle,
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<Value>, ()> {
    let dir = state.config().dir().to_path_buf();
    let app_log = read_tail(&dir.join("logs").join("polaris.log"), LOG_TAIL_BYTES);
    let singbox_log = read_tail(&dir.join("singbox.log"), LOG_TAIL_BYTES);

    // 节点身份打码：日志原文含节点域名/IP/节点名 —— 与诊断包共用同一套标识符收集 + 替换，不另写一份。
    let ids = match state.config().load_full() {
        Ok(cfg) => collect_node_identifiers(&cfg, &[]),
        Err(_) => Vec::new(),
    };
    let body = format!(
        "# Polaris 日志导出\n\n\
> 纯日志（不含配置与版本号）。**节点身份已打码**，但本产物不含配置块、未做密钥脱敏。\
要附到公开 issue 请改用「诊断包」导出。\n\n\
生成时间：{}\n\n\
## app.log（近期）\n\n```text\n{}\n```\n\n\
## singbox.log（近期）\n\n```text\n{}\n```\n",
        now_iso8601(),
        if app_log.is_empty() {
            "(空)"
        } else {
            &app_log
        },
        if singbox_log.is_empty() {
            "(空)"
        } else {
            &singbox_log
        },
    );
    let redacted = polaris_stats_engine::redact::redact_identifiers(&body, &ids);

    let default_name = format!("polaris-logs-{}.md", today_yyyy_mm_dd());
    let lang = crate::i18n::app_lang(&app);
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(t(lang, key::NATIVE_LOGS_EXPORT_TITLE))
        .set_file_name(&default_name)
        .add_filter("Markdown", &["md"])
        .add_filter(t(lang, key::NATIVE_ALL_FILES), &["*"])
        .save_file(move |p| {
            let _ = tx.send(p);
        });
    let Some(path) = rx.await.ok().flatten().and_then(|p| p.into_path().ok()) else {
        return Ok(ApiResponse::ok(
            json!({ "success": false, "error": "cancelled" }),
        ));
    };
    if let Err(e) = std::fs::write(&path, redacted) {
        return Ok(ApiResponse::ok(
            json!({ "success": false, "error": format!("{e}") }),
        ));
    }
    Ok(ApiResponse::ok(json!({
        "success": true,
        "filePath": path.to_string_lossy(),
    })))
}

// ── 自启动 ── 上游 `autostart-handlers.ts`（tauri-plugin-autostart）──

/// 上游 `AUTO_START_SET`：设 / 取消开机自启（tauri-plugin-autostart）。
#[tauri::command]
pub fn auto_start_set(
    app: AppHandle,
    _state: State<'_, AppRuntime>,
    enabled: bool,
) -> ApiResponse<()> {
    let autostart = app.state::<tauri_plugin_autostart::AutoLaunchManager>();
    let res = if enabled {
        autostart.enable()
    } else {
        autostart.disable()
    };
    match res {
        Ok(()) => ok_void(),
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 上游 `AUTO_START_GET_STATUS`：自启状态。
#[tauri::command]
pub fn auto_start_get_status(app: AppHandle) -> ApiResponse<bool> {
    let autostart = app.state::<tauri_plugin_autostart::AutoLaunchManager>();
    ApiResponse::ok(autostart.is_enabled().unwrap_or(false))
}

// ── 出口 IP 信息 ── 上游 `ipinfo-handlers.ts` ──

/// 出口 IP 探测超时（ms）。
const IPINFO_TIMEOUT_MS: u64 = 8_000;
/// trace 响应体上限（cdn-cgi/trace 仅数百字节，64 KiB 足够且防滥用）。
const IPINFO_MAX_BODY: usize = 64 * 1024;

// ── 探测重试预算（1:1 移植 上游 `IpInfoService.ts:15-47`）──
//
// 为什么必须有：本模块**纯事件驱动、无轮询**（§10.1），一次失败就是一帧
// `{direct:null, proxy:null, error}` 落地 —— 状态栏 IP 与旗面双双空、`proxy_probed=false`
// 连伴测都不 fire ⇒ 延迟格也空，而**没有任何后续动作会来纠正它**（15s TTL 那条自愈路径全仓
// 无消费方：`ipInfoApi.get` 唯一调用点 `HomeScreen.tsx` 传 force=true，其余全走 peek）。
// 只能等用户再点一次「网络检测」。而失败恰恰高发：起核 +4s 正是 DNS 接管 / FakeIP 刚落的时刻。
//
// ⚠️ 重试**不是**轮询：它收在单次探测腿内、有硬预算封顶，腿结束即止。§12.4 禁的是周期轮询。

/// 单条探测腿的总预算（ms）—— 上游 `TOTAL_PROBE_BUDGET_MS`。
///
/// 上游 在 JS 里靠「循环内逐次查 deadline + 一个赛跑的 `setTimeout`」两道保险，因为 JS 的 promise
/// **不可取消**；Rust 的 [`tokio::time::timeout`] 直接取消整个 future，故这里只留外层一道 ——
/// 行为等价（到点即返回失败），少一半代码。
const IPINFO_PROBE_BUDGET_MS: u64 = 10_000;

/// direct 腿重试 —— 上游 `DIRECT_MAX_PROBE_ATTEMPTS` / `DIRECT_RETRY_DELAY_MS`。
const IPINFO_DIRECT_ATTEMPTS: u32 = 3;
const IPINFO_DIRECT_RETRY_MS: u64 = 1_000;

/// proxy 腿常规重试（停核 / 手点检测 / 启动首探）—— 上游 `MAX_PROBE_ATTEMPTS` / `RETRY_DELAY_MS`。
const IPINFO_PROXY_ATTEMPTS: u32 = 2;
const IPINFO_PROXY_RETRY_MS: u64 = 1_000;

/// proxy 腿**接通后**重试（起核 / 热切，即走选路收敛延迟的那些腿）——
/// 上游 `POST_CONNECT_MAX_PROBE_ATTEMPTS` / `POST_CONNECT_RETRY_DELAY_MS`。
///
/// 间隔比常规腿宽 4 倍：这条腿跑在隧道热身窗口里，失败是「还没好」而非「坏了」，密集重试只是空转。
const IPINFO_PROXY_POST_CONNECT_ATTEMPTS: u32 = 4;
const IPINFO_PROXY_POST_CONNECT_RETRY_MS: u64 = 4_000;
/// 快照缓存 TTL（ms）：非 force 时 TTL 内直接回缓存，不重复 HTTP（避免每次轮询打网）。
const IPINFO_TTL_MS: u128 = 15_000;

/// 事件驱动重探的**选路收敛等待**（对齐 上游 `whenSelectorSettled(4000)`）：核就绪 / 热切完成那一刻
/// selector 的 PUT 才刚落，出口隧道未必已能跑流量 —— 立刻探会打到旧出口或直接失败。停核腿无此问题
/// （出口已确定性消失），故用 0。
pub const IPINFO_SETTLE_DELAY_MS: u64 = 4_000;

/// 最近一次出口 IP 快照缓存（`peek` 零探测读取；TTL 内非 force 复用）。
static IPINFO_CACHE: OnceLock<Mutex<Option<Value>>> = OnceLock::new();

/// 出口 IP 探测的**世代线**：每条会落地的探测腿开工前领一个世代号，落地前再比对。世代已变 ⇒ 后面有
/// 更新的腿，本腿结果已过期，直接退场。「后来者胜」正是这里要的语义。
///
/// **手点「网络检测」腿（[`ipinfo_get`]）与事件驱动排程腿（[`schedule_ipinfo_refresh`]）共用这一条线**
/// ——两条线各管各的等于没管：用户在起核 4s 收敛窗口内点一下检测，就会有两条探测并行，谁先落地纯看
/// 网络抖动，后落地的可能反而是先发起的那条。
static IPINFO_REFRESH_EPOCH: AtomicU64 = AtomicU64::new(0);

/// 领一个新世代，作废所有在飞的旧腿。每条会写缓存 / 广播的探测腿都必须**在开探那一刻**领一次
/// （不是在排程那一刻 —— 理由见 [`schedule_ipinfo_refresh`] 的「按开探顺序发号」一节）。
fn next_ipinfo_epoch() -> u64 {
    IPINFO_REFRESH_EPOCH.fetch_add(1, Ordering::SeqCst) + 1
}

/// 出口 IP 探测的**排程线**：每次「出口世界要变了」的事件（起核 / 停核 / 热切 / TS 就绪 / 启动腿 /
/// 用户手点检测）宣告一次，**在事件那一刻**自增，与 [`IPINFO_REFRESH_EPOCH`] 的开探时刻正交。
///
/// # 为什么一个计数器不够（🔴 2026-07-21 第三轮复审）
///
/// 世代号回答的是「**谁的世界快照最新**」——故必须在**开探**那一刻领（在排程时领会让 4s 收敛窗口内
/// 的手点腿把收敛腿静默作废，见 [`schedule_ipinfo_refresh`] 的 🟠 一节）。但这样一来，「**谁最新**」
/// 这件事在「已排程、尚未开探」的整个收敛窗口里**无人记录**：
///
/// - t=0 热切到 B → L1 置在飞 + 广播置空，睡到 t=4；
/// - t=4.0 L1 醒 → 领世代 1 → 读 status/config（selected=B）→ 开探（走 B 的隧道）；
/// - t=4.1 热切到 C → L2 排程，睡到 t=8.1（**它要到 t=8.1 才领世代 2**）；
/// - t=5.0 L1 探完落地：`IPINFO_REFRESH_EPOCH` 仍是 1 = 自己的号 ⇒ **过闸** ⇒ 广播 B 的出口
///   （用户已在 C），并把 **B 的隧道之外量到的 warm RTT 记进 B 的延迟徽标**。
///
/// 显示错误 4s 后自愈，**延迟徽标的错误却是持久的**（`latencyMap[B]` 保留错值直到下次测 B），而这
/// 正是 [`crate::commands::speedtest::spawn_warm_rtt_probe`] 那道复查存在的全部理由。
///
/// # 两条判据的分工（缺任一条都只对一半）
///
/// | 判据 | 宣告时刻 | 回答 |
/// |---|---|---|
/// | [`IPINFO_REFRESH_EPOCH`] | **开探**（sleep 之后） | 两条已开探的腿，谁的世界快照新 |
/// | `IPINFO_SCHEDULE_SEQ` | **排程 / 事件**（sleep 之前） | 我开探之后，有没有更新的事件宣告过 |
///
/// 腿在**开探那一刻**快照当前值（不是自己自增的那个值 —— 它读 status/config 也在这一刻，两者必须
/// 同一时点），落地时比对「快照 == 当前」：不等 ⇒ 我开探后世界又变了 ⇒ 退场。
static IPINFO_SCHEDULE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 宣告一次「出口世界要变了」（排程 / 手点那一刻调，**不是**开探那一刻）。返回值无消费方：腿要的是
/// 开探时刻的 [`current_ipinfo_schedule_seq`] 快照，不是自己自增出来的号。
fn next_ipinfo_schedule_seq() -> u64 {
    IPINFO_SCHEDULE_SEQ.fetch_add(1, Ordering::SeqCst) + 1
}

/// 开探那一刻的排程线快照（与领世代、读 status/config 同一时点取）。
fn current_ipinfo_schedule_seq() -> u64 {
    IPINFO_SCHEDULE_SEQ.load(Ordering::SeqCst)
}

/// 本腿的「世界」是否仍是当前的：世代未被更新的腿超越 **且** 开探之后无更新事件宣告。
///
/// [`commit_ipinfo_snapshot`] 与**探测腿的下游异步**（当前唯一消费方是出口伴测
/// [`crate::commands::speedtest::spawn_warm_rtt_probe`]：它在探测落地后才 fire，测量期间可能又换了
/// 节点，不复查就会把新节点的 RTT 记到旧节点 id 上）共用这一条判据 —— 两处各写一半必然漂移，而
/// 漂移出来的那一半正是本模块两轮复审各挨了一次的洞。
pub(crate) fn ipinfo_probe_is_current(epoch: u64, seq: u64) -> bool {
    IPINFO_REFRESH_EPOCH.load(Ordering::SeqCst) == epoch
        && IPINFO_SCHEDULE_SEQ.load(Ordering::SeqCst) == seq
}

/// **收敛窗口在飞计数**：延迟腿广播「置空」帧那一刻 +1，该腿跑完（落地 / 被超越退场 / 早退）时 -1。
/// `> 0` ⇒ 至少有一条延迟腿在收敛窗口里，[`peek_ipinfo_snapshot`] 回置空帧。
///
/// # 🔴 为什么是计数而不是 `AtomicBool`（2026-07-21 第三轮复审）
///
/// `AtomicBool` 没有所有权：谁都能清掉谁置的位。两次热切间隔 >4s（完全常规）就够复现——
/// L1（切到 B）t=4 开探、t=5 落地时把位清掉，而 L2（切到 C）t=4.1 才置的位、要到 t=8.1 才开探：
/// 中间 3s 里 `peek` 型消费方（托盘浮层 `TrayMenu.tsx`、主窗水合腿 `App.tsx`）读到的是 **B 的缓存值**，
/// 而用户已在 C。冷启动同样可达（startup 腿清掉起核腿置的位）。
/// 计数后「谁排的位谁归还」，L1 归还只把计数降回 1，窗口在最新那条腿跑完之前不会关。
///
/// # 已知取舍：慢腿会把窗口拖长
///
/// 最新腿已落地、而某条注定要退场的慢腿仍在飞（direct+proxy 两腿串行、各由
/// [`IPINFO_PROBE_BUDGET_MS`] 封顶 ⇒ 最长 20s）时，计数仍 `> 0` ⇒
/// `peek` 继续回置空帧，尽管缓存里已是正确的新值。代价是**多留空几秒**，而反过来（提前关窗）付出的
/// 是**吐旧出口**——本模块的既定纪律是「留空优于用旧出口冒充新出口」，故取前者。要消掉这段窗口得给
/// 在飞腿加取消语义（探测中途放弃），不值。
///
/// # 📌 登记（复审已裁**不计缺陷**，勿据此改代码）
///
/// [`build_ipinfo_snapshot`] 若 panic，排程腿的归还点（体尾那次 `fetch_sub`）走不到 ⇒ 计数永久卡在
/// `> 0` ⇒ `peek` 从此恒回置空帧，且本模块**无轮询**、没有任何后续动作会来纠正。概率极低（该函数及
/// 其调用链无显式 panic 点，网络错误一律走 `Result`），故不为它加 catch_unwind / Drop 守卫。
/// 记在这里是让后来者知道这是**已知取舍**，不是没想到。
///
/// # 为什么不能靠「把 pending 帧也写进 `IPINFO_CACHE`」代替
///
/// `IPINFO_CACHE` 同时喂着两条语义不同的读路径：`peek`（零探测水合）与 [`fresh_cached_snapshot`]
/// （15s TTL 内的非 force 短路）。把双 null 的 pending 帧写进缓存会**毒化后者** —— 收敛窗口后的
/// 15s 内，任何非 force 的 `ipinfo_get` 都会短路拿到双 null，等于把「正在探」固化成「探完了没探到」。
/// 故在飞状态必须是独立标记，缓存里永远只放**真探测结果**。
///
/// # 不置位它会怎样（本标记要修的缺陷）
///
/// `peek` 型消费方（托盘浮层 `TrayMenu.tsx` 每次弹出即 peek 水合、主窗 `App.tsx` 窗口重建水合）
/// **不订阅** `ipInfoUpdated`，只读缓存 ⇒ 起核/热切的 4s 收敛窗口里它们照样吐**上一个出口**的 IP，
/// 而同一时刻订阅方（状态栏）已按 pending 帧置空。同屏两处对「我现在从哪出去」给出互相矛盾的答案，
/// 且错的那个正是「用旧出口冒充新出口」——`pending_ipinfo_snapshot` 存在的全部理由。
static IPINFO_INFLIGHT: AtomicUsize = AtomicUsize::new(0);

/// 落地一次探测结果：[`ipinfo_probe_is_current`] 两条判据同时成立 ⇒ 写缓存、回 `true`（调用方随即
/// 广播 + 伴测）；被更新的腿超越、或开探后又有更新事件宣告 ⇒ 什么都不做、回 `false`。
///
/// **不碰 [`IPINFO_INFLIGHT`]**：那一格是**排程腿**排的，归还权也归它（见该 static 的「谁排的位谁
/// 归还」一节）——在这里清位就是 `AtomicBool` 时代「L1 清掉 L2 的位」那个洞。
///
/// # 为什么闸必须在**探测之后**再查一次
///
/// [`build_ipinfo_snapshot`] 最长跑 `IPINFO_PROBE_BUDGET_MS × 2 = 20s`（direct + proxy 两腿串行，
/// 各含定额重试、各自封顶），而排程间隔只有 [`IPINFO_SETTLE_DELAY_MS`] = 4s ——
/// **探测窗口远大于排程间隔**（移植重试后差距进一步拉大），先发起的慢腿完全可能在后
/// 发起的快腿之后落地。只在探测**前**查闸挡不住这段窗口，旧腿会同时污染 `IPINFO_CACHE` 与广播。
///
/// 三个真实序列（本函数即这三条的共同闸；时刻均为**开探**时刻 = 领号时刻）：
/// - **冷启动**：startup 腿 t=3s 领号开探（此刻核未起，只有 direct），autoconnect 同期起核、其 4s 腿
///   t≈6.5s 领号开探并发布代理出口；startup 腿慢探 t≈11s 落地，把它盖成 `proxy=null` ⇒ 状态栏回退
///   `—`、旗面消失。号按开探顺序发 ⇒ startup(3s) < ready(6.5s)，本闸认得出谁旧。
/// - **停核→1.5s 后起核**：停核腿零延迟、t=0 就领号开探；起核腿 t≈5.5s 才领号（1.5 + 4s 收敛）。
///   停核腿探得慢、后落地，其 `proxy=null` 会覆盖起核腿已发布的新出口。
/// - **连点热切 B→C**：B 腿 t=4s 开探（慢），C 腿 t=9s 开探（快）先发布，B 腿后到 ⇒ 状态栏
///   长期显示 B 的出口 IP 与旗面。
///
/// 🔴 第四条序列**世代号一条也认不出**（第三轮复审）：两次热切间隔 >4s 时，L1 已开探（领了号）而
/// L2 还在睡（尚未领号）—— L1 落地那一刻世代仍是它自己的，过闸。挡它的是 [`IPINFO_SCHEDULE_SEQ`]
/// 那一半判据（L1 开探时快照 seq=1，L2 一排程即 seq=2 ⇒ 不等 ⇒ 退场），详见该 static 的文档。
///
/// # 边界：这是 check-then-act，不是临界区
///
/// 世代比对与写缓存**不在同一把锁下**（`IPINFO_REFRESH_EPOCH` 是 atomic，`IPINFO_CACHE` 是 Mutex）。
/// 理论上存在 TOCTOU：本腿过闸之后、拿到 Mutex 之前，一条更新的腿领了号并抢先写完缓存 ⇒ 本腿仍会覆盖它。
/// **实际不可达**：领号与写缓存之间隔着一整次 `build_ipinfo_snapshot`（网络往返，秒级），而这里的两条
/// 指令间隔是纳秒级。合并成一把锁需要让世代号也进 Mutex，为一个够不到的窗口换掉 atomic 的无锁读，不值。
/// 记在这里是为了让后来者知道这是**已知取舍**，不是没想到。
fn commit_ipinfo_snapshot(epoch: u64, seq: u64, snap: &Value) -> bool {
    if !ipinfo_probe_is_current(epoch, seq) {
        return false;
    }
    if let Ok(mut g) = ipinfo_cache().lock() {
        *g = Some(snap.clone());
    }
    true
}

/// `peek=true` 的零探测读取：**在飞时回置空帧**（与订阅方同一帧），否则回缓存快照。
///
/// 抽成独立函数而非内联在 [`ipinfo_get`] 里：`ipinfo_get` 是 `#[tauri::command]`（要 `AppHandle` +
/// `State`，本仓未引 `tauri::test` ⇒ 单测造不出来），而「收敛窗口内 peek 到底吐什么」正是本轮要钉死的
/// 语义，必须可被直测。
fn peek_ipinfo_snapshot() -> Value {
    if IPINFO_INFLIGHT.load(Ordering::SeqCst) > 0 {
        return pending_ipinfo_snapshot();
    }
    ipinfo_cache()
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(empty_ipinfo_snapshot)
}

fn ipinfo_cache() -> &'static Mutex<Option<Value>> {
    IPINFO_CACHE.get_or_init(|| Mutex::new(None))
}

/// **出口无效直判终态的纯逻辑折叠**（1:1 上游 `IpInfoService.markProxyBlocked`，`IpInfoService.ts:187-197`
/// 的 `{...this.snapshot, proxy:null, updatedAt, loading:false, error:undefined, proxyBlocked}`）。
///
/// 输入 = 当前权威缓存快照（`None` = 从未探过 → 取 [`empty_ipinfo_snapshot`]），输出 = 合并后的新快照。
/// **合并而非重建**：`direct`（本机直连出口 IP + 旗面）与代理出口无效**互不相干** —— 整帧重建会把
/// 状态栏那格已探到的本地出口一并抹成 `—`，用户看到的是「网络全挂」而不是「代理出口无效」。
///
/// `error` 必须**删键**（不是置 null）：`blocked` 与 `error` 是互斥语义（blocked = 已知无效、压根没探；
/// error = 探了但失败）。留着上一轮的 `error` 会让 UI 同时收到两个互斥终态。
fn fold_proxy_blocked(cached: Option<Value>, reason: &str) -> Value {
    let mut snap = cached.unwrap_or_else(empty_ipinfo_snapshot);
    if !snap.is_object() {
        // 缓存被写坏（非 object）→ 从空快照重建，绝不 panic、也不把坏值当基底往下传。
        snap = empty_ipinfo_snapshot();
    }
    if let Some(obj) = snap.as_object_mut() {
        obj.insert("proxy".to_string(), Value::Null);
        obj.insert(
            "updatedAt".to_string(),
            json!(u64::try_from(now_epoch_ms()).unwrap_or(u64::MAX)),
        );
        obj.insert("loading".to_string(), json!(false));
        obj.remove("error");
        obj.insert("proxyBlocked".to_string(), json!(reason));
    }
    snap
}

/// **出口无效直判终态落地**（`ProxyErrorEmitter::mark_exit_blocked` 的唯一实现腿）：把「代理出口已知
/// 无效」同时写进**权威缓存**与广播帧。
///
/// # 为什么必须写缓存（本函数存在的全部理由）
///
/// `EVENT_IP_INFO_UPDATED` 只喂**订阅方**（状态栏）。`peek` 型消费方（托盘浮层每次弹出即 peek、主窗
/// 窗口重建水合）**不订阅**，只读 [`IPINFO_CACHE`] —— 只广播不写缓存 ⇒ 那两处继续吐**上一次探到的
/// 代理出口 IP**，而该出口此刻已被直判无效。同屏两处对「我现在从哪出去」给出互相矛盾的答案，且错的
/// 那个正是「用旧出口冒充一个已知无效的出口」，与 [`pending_ipinfo_snapshot`] 要挡的是同一类失真。
///
/// # 为什么不走 [`commit_ipinfo_snapshot`]
///
/// 那条闸是给**探测腿**用的（领了世代号、可能被更新的腿超越）。本函数不是探测：它是「已知无效」的
/// **直判终态**，没有开探时刻、没有领号，拿别人的号去过闸只会随机被吞。直判终态由调用点（TS 出口
/// 警告跨态）本身保证时序，故此处无条件落地。
///
/// # 🔴 但**必须宣告排程线**（否则在飞的探测腿会把终态盖回去）
///
/// 「出口被直判无效」本身就是一次「出口世界变了」的事件 —— 与起核 / 停核 / 热切同性质，故按本模块
/// 既有的**「排程即宣告」**契约（见 [`IPINFO_SCHEDULE_SEQ`]）在写缓存前自增一次。
///
/// 不宣告的后果（reviewer 复现路径：TS ready 边沿排了 refresh 后 4–24s 内 exit peer 掉线）：
/// 那条腿开探时快照的 `(epoch, seq)` 两个计数器**在整段无人自增** ⇒ 它落地时
/// [`ipinfo_probe_is_current`] 恒过闸 ⇒ 用一个对已知无效出口的探测结果（`proxy:null` + `error`）
/// **覆盖** `proxyBlocked` 终态。而 `reconcile_ts_exit_block` 是边沿触发、同态帧直接早退 ⇒ 终态
/// **不会重落**，错误显示一直挂到下一次真跨态。探测腿的预算最长 20s，收敛窗口 4s，两者都远大于
/// 「掉线到落地」的间隔，所以这不是理论窗口。
///
/// 宣告只作废「**已开探**的腿」；仍在收敛窗口里睡着的腿醒来会快照到这个新号，其结果按本模块既定的
/// 「后开探者胜」语义仍算当前世界的真值 —— 与停核腿宣告后紧跟起核腿的既有形态一致，不另立规矩。
pub(crate) fn mark_ipinfo_proxy_blocked(app: &AppHandle, reason: &str) {
    let snap = commit_proxy_blocked_snapshot(reason);
    crate::events::broadcast(app, crate::events::channel::EVENT_IP_INFO_UPDATED, snap);
}

/// [`mark_ipinfo_proxy_blocked`] 的**无 `AppHandle` 部分**（宣告排程线 + 折叠 + 写权威缓存），返回待广播帧。
///
/// 拆出来的唯一理由是**可测**：`mark_ipinfo_proxy_blocked` 要真 `AppHandle`（本仓未引 `tauri::test`）⇒
/// 行为测试够不着，而「宣告排程线」这条正是本轮修的那一维，只靠源码守卫锁不住落地语义
/// （见 [`tests::stale_probe_leg_must_not_overwrite_newer_leg`] 的段 (g)）。广播留在外面：它是唯一
/// 需要 `AppHandle` 的动作。
fn commit_proxy_blocked_snapshot(reason: &str) -> Value {
    // 🔴 排程即宣告（**写缓存之前**）：见 [`mark_ipinfo_proxy_blocked`] 文档。
    next_ipinfo_schedule_seq();
    let cached = ipinfo_cache().lock().ok().and_then(|g| g.clone());
    let snap = fold_proxy_blocked(cached, reason);
    if let Ok(mut g) = ipinfo_cache().lock() {
        *g = Some(snap.clone());
    }
    snap
}

/// 空快照（无缓存时 peek 的回退：direct/proxy 均 null）。
fn empty_ipinfo_snapshot() -> Value {
    json!({ "direct": Value::Null, "proxy": Value::Null, "updatedAt": 0 })
}

/// 当前 epoch 毫秒（快照 updatedAt 用）。
fn now_epoch_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// TTL 内的缓存快照（非 force 时短路复用）；无缓存 / 过期 → None。
fn fresh_cached_snapshot() -> Option<Value> {
    let snap = ipinfo_cache().lock().ok()?.clone()?;
    let updated = snap.get("updatedAt").and_then(Value::as_u64).unwrap_or(0);
    if now_epoch_ms().saturating_sub(u128::from(updated)) <= IPINFO_TTL_MS {
        Some(snap)
    } else {
        None
    }
}

/// 经指定 HTTP client 拉 Cloudflare `cdn-cgi/trace` → `{ ip, countryCode? }`（渲染端 `IpInfo`）。
///
/// **SSRF**：走 [`safe_redirect_fetch`]（逐跳 `assert_host_allowed`）——URL 为固定常量 `EGRESS_TRACE_URL`
/// （cloudflare.com，解析为公网 IP），既非用户可控、又逐跳复检，绝不可被诱导打 127/169.254 控制面 / 元数据。
async fn fetch_trace_ipinfo<H: HttpClient>(client: &H) -> Result<Value, String> {
    let resp = safe_redirect_fetch(SafeRedirectFetchOptions {
        fetch_impl: client,
        url: polaris_unlock::endpoints::EGRESS_TRACE_URL,
        user_agent: app_user_agent(),
        headers: None,
        exempt_fake_ip: false,
        max_redirects: Some(2),
        timeout_ms: Some(IPINFO_TIMEOUT_MS),
        max_body_bytes: Some(IPINFO_MAX_BODY),
        lookup: &SystemDnsLookup,
    })
    .await
    .map_err(|e| e.message)?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("ipinfo trace HTTP {}", resp.status));
    }
    let body = String::from_utf8_lossy(&resp.body);
    let info = polaris_unlock::parse_trace(&body).ok_or_else(|| "trace 解析失败".to_string())?;
    let mut out = json!({ "ip": info.ip });
    if let Some(cc) = info.country_code {
        out["countryCode"] = json!(cc);
    }
    Ok(out)
}

/// 经指定 HTTP client 拉 `myip.ipip.net/json` → `{ ip, country?, countryCode? }`（渲染端 `IpInfo`）。
///
/// **direct 腿专用**：本地直连出口**只信国内** ipip 端点。旁路由/软路由的透明分流会把国外端点
/// （cloudflare/ip-api/ipify）劫持走代理出口 → 直连出口被误标为境外节点 IP（真机实证）。对齐 上游
/// `IpInfoService.queryDirectChain`（EP_IPIP-only，绝不 fallback 国外端点）；与 [`fetch_trace_ipinfo`]
/// （cloudflare，仅 proxy 腿）互斥。
///
/// **SSRF**：走 [`safe_redirect_fetch`]（逐跳 `assert_host_allowed`）——URL 为固定常量 `DIRECT_IPINFO_URL`
/// （myip.ipip.net，解析为公网 IP），既非用户可控、又逐跳复检，绝不可被诱导打 127/169.254 控制面 / 元数据。
async fn fetch_ipip_ipinfo<H: HttpClient>(client: &H) -> Result<Value, String> {
    let resp = safe_redirect_fetch(SafeRedirectFetchOptions {
        fetch_impl: client,
        url: polaris_unlock::endpoints::DIRECT_IPINFO_URL,
        user_agent: app_user_agent(),
        headers: None,
        exempt_fake_ip: false,
        max_redirects: Some(2),
        timeout_ms: Some(IPINFO_TIMEOUT_MS),
        max_body_bytes: Some(IPINFO_MAX_BODY),
        lookup: &SystemDnsLookup,
    })
    .await
    .map_err(|e| e.message)?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("ipinfo ipip HTTP {}", resp.status));
    }
    let body = String::from_utf8_lossy(&resp.body);
    let info = polaris_unlock::parse_ipip(&body).ok_or_else(|| "ipip 解析失败".to_string())?;
    let mut out = json!({ "ip": info.ip });
    if let Some(c) = info.country {
        out["country"] = json!(c);
    }
    if let Some(cc) = info.country_code {
        out["countryCode"] = json!(cc);
    }
    Ok(out)
}

/// 定额重试一次探测动作 —— 1:1 移植 上游 `IpInfoService.withRetry`（`IpInfoService.ts:257-290`）。
///
/// 语义：至多 `attempts` 次，失败之间隔 `retry_delay_ms`（**定间隔，无指数退避** —— 上游 同），
/// 整体由 [`IPINFO_PROBE_BUDGET_MS`] 封顶。返回**最后一次**的错误（诊断时要看最终态，不是首次抖动）。
///
/// 参数化在「尝试动作」上而非 client 上：`fetch_trace_ipinfo` 经 `safe_redirect_fetch` 走真 DNS，
/// 单测碰不得（禁触碰宿主网络）；收在闭包外则重试逻辑本身可被纯逻辑直测（见 `tests` 三条）。
async fn with_ipinfo_retry<F, Fut>(
    mut attempt: F,
    attempts: u32,
    retry_delay_ms: u64,
) -> Result<Value, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let budget = Duration::from_millis(IPINFO_PROBE_BUDGET_MS);
    tokio::time::timeout(budget, async move {
        let mut last = Err("ipinfo: 未发起任何尝试".to_string());
        for i in 0..attempts {
            last = attempt().await;
            if last.is_ok() {
                return last;
            }
            // 末次失败后不再睡：那一觉纯粹是把预算烧掉，调用方还得多等一个间隔才拿到失败。
            if i + 1 < attempts {
                tokio::time::sleep(Duration::from_millis(retry_delay_ms)).await;
            }
        }
        last
    })
    .await
    .unwrap_or_else(|_| Err(format!("ipinfo: 探测预算 {IPINFO_PROBE_BUDGET_MS}ms 耗尽")))
}

/// 构造出口 IP 快照：direct（经直连传输层单点）+ proxy（经本机混合端口，核未运行 → null）。
///
/// `post_connect` = 本腿是否跑在**隧道热身窗口**里（起核 / 热切，即走选路收敛延迟那些腿）：
/// 决定 proxy 腿吃哪套重试预算。对照 上游 触发点全表（§10.1 / `main/index.ts`）：
///
/// | 触发点 | 上游 | Polaris | |
/// |---|---|---|---|
/// | 启动首探 | `refresh(true)`（`index.ts:1762`）= 常规 | `delay=3s` → 常规 | ✅ |
/// | 起核就绪 | `refreshProxyPostConnect()`（`index.ts:1966`） | `delay=4s` → post-connect | ✅ |
/// | 停核 | `refresh(true)`（`index.ts:2014`）= 常规 | `delay=0` → 常规 | ✅ |
/// | 手点检测 | `refresh(true, true)` = 常规 | [`ipinfo_get`] 传 `false` | ✅ |
/// | 节点热切 | **仅 `accountBased` 走 post-connect**，IP 类节点走常规（`index.ts:1997-2001`） | 一律 post-connect | ⚠️ |
///
/// ⚠️ **登记一处有意偏离**（勿当遗漏改掉）：热切腿 Polaris 不区分 `accountBased`。
/// 上游 `schedule_exit_ip_refresh` 只拿得到 `running`，要区分得把节点类型一路串下来 —— 而代价不对等：
/// 猜宽了只是 IP 类节点失败时多等一个 4s 间隔（仍在 10s 预算内截断，最坏多两次尝试）；
/// 猜窄了则账号制节点在隧道热身期被 2×1s 耗尽 ⇒ 回落到本轮要根治的那个症状（空 IP + 空旗 + 空延迟）。
/// **非对称风险下取宽**。真要区分，前置是给 [`schedule_ipinfo_refresh`] 传节点类型，不是改这里。
async fn build_ipinfo_snapshot(
    direct_http: &HttpRuntime,
    status: &ProxyStatus,
    post_connect: bool,
) -> Value {
    let (direct, direct_err) = match with_ipinfo_retry(
        || fetch_ipip_ipinfo(direct_http),
        IPINFO_DIRECT_ATTEMPTS,
        IPINFO_DIRECT_RETRY_MS,
    )
    .await
    {
        Ok(v) => (v, None),
        Err(e) => (Value::Null, Some(e)),
    };
    let (proxy_attempts, proxy_retry_ms) = if post_connect {
        (
            IPINFO_PROXY_POST_CONNECT_ATTEMPTS,
            IPINFO_PROXY_POST_CONNECT_RETRY_MS,
        )
    } else {
        (IPINFO_PROXY_ATTEMPTS, IPINFO_PROXY_RETRY_MS)
    };
    let proxy = if status.running && status.mixed_port != 0 {
        match HttpRuntime::via_local_proxy(status.mixed_port) {
            Ok(p) => with_ipinfo_retry(|| fetch_trace_ipinfo(&p), proxy_attempts, proxy_retry_ms)
                .await
                .unwrap_or(Value::Null),
            Err(_) => Value::Null,
        }
    } else {
        Value::Null
    };
    let mut snap = json!({
        "direct": direct,
        "proxy": proxy,
        "updatedAt": u64::try_from(now_epoch_ms()).unwrap_or(u64::MAX),
    });
    if let Some(e) = direct_err {
        snap["error"] = json!(e);
    }
    snap
}

/// 上游 `IP_INFO_GET`：出口 IP 信息（本地直连出口 / 代理出口）。
///
/// - `peek=true`：零探测，回最近快照；**收敛窗口在飞时回置空帧**（见 [`peek_ipinfo_snapshot`]）。
/// - 非 force：TTL 内回缓存，不打网。
/// - 探测成功 → 缓存 + 广播 `event:ipInfoUpdated`。
#[tauri::command]
pub async fn ipinfo_get(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    force: Option<bool>,
    visible: Option<bool>,
    peek: Option<bool>,
) -> Result<ApiResponse<Value>, ()> {
    let _ = visible; // 保留入参（真链可见性探测流程）；本层直连+代理双探已覆盖，不额外分流。
    if peek.unwrap_or(false) {
        return Ok(ApiResponse::ok(peek_ipinfo_snapshot()));
    }
    if !force.unwrap_or(false) {
        if let Some(cached) = fresh_cached_snapshot() {
            return Ok(ApiResponse::ok(cached));
        }
    }

    // 手点腿同样宣告排程线 + 领世代：既作废在飞的排程腿，也让自己可被更晚的排程腿作废（共用同一
    // 对判据，否则起核收敛窗口内点检测会出现两条互不作废的并行探测）。手点腿的排程与开探是同一刻
    // ⇒ 先自增再快照，快照到的正是自己刚宣告的那个值。
    next_ipinfo_schedule_seq();
    let epoch = next_ipinfo_epoch();
    let seq = current_ipinfo_schedule_seq();
    // State 借用不跨 await：先取 owned（Arc + status + config），再探测。
    let inputs = ipinfo_probe_inputs(&state);
    // 手点腿吃**常规**重试预算：用户已在等一个即时答复，隧道热身那套 4×4s 会让按钮看起来卡住。
    // 与 上游 一致（手点走 `refresh(true, true)` → 常规腿，非 post-connect）。
    Ok(ApiResponse::ok(
        probe_publish_ipinfo(&app, inputs, epoch, seq, false).await,
    ))
}

/// 一次出口 IP 探测所需的全部 owned 输入（`State` 借用**不得跨 await**，故先摘出来）。
struct IpinfoProbeInputs {
    /// 直连传输层（探 direct 出口）。
    http: std::sync::Arc<HttpRuntime>,
    /// 探测时刻的核状态（决定是否探 proxy 出口 + 伴测门控）。
    status: ProxyStatus,
    /// 用户配置（伴测取 `selectedServerId` / 测速 URL）。
    config: Value,
}

/// 从 `AppRuntime` 摘出 [`IpinfoProbeInputs`]（同步，无 await —— 借用在本函数内即结束）。
fn ipinfo_probe_inputs(state: &AppRuntime) -> IpinfoProbeInputs {
    IpinfoProbeInputs {
        http: state.http().clone(),
        status: state.proxy().status(),
        config: state.config().current().unwrap_or_default(),
    }
}

/// **探测 → 缓存 → 广播 → 出口伴测**：`ipinfo_get` 的 force 腿与事件驱动腿（[`schedule_ipinfo_refresh`]）
/// 共用的唯一实现。抽出来是为了让「用户点网络检测」与「起核 / 热切 / 停核 / 启动自动触发」跑**同一条
/// 编排**——两套逻辑必然漂移（本仓解锁检测已栽过一次：只移植了广播半边）。
///
/// `epoch` / `seq` 由调用方在**开探那一刻**取（[`next_ipinfo_epoch`] + [`current_ipinfo_schedule_seq`]，
/// 与 `inputs` 里的 status/config 快照同一时点）；探测**之后**经 [`commit_ipinfo_snapshot`] 复查，
/// 任一判据变了即原样退场（不写缓存、不广播、不伴测），理由见 `commit_ipinfo_snapshot` 的文档。
///
/// ⚠️ **本函数不得自己领号 / 自增**：那样复查就是拿现场刚取的值跟自己比，恒真 = 没闸，而下游伴测拿到
/// 的也会是个恒真的判据（`spawn_warm_rtt_probe` 的复查随之形同虚设）。由 [`ipinfo_epoch_guard`] 钉住。
async fn probe_publish_ipinfo(
    app: &AppHandle,
    inputs: IpinfoProbeInputs,
    epoch: u64,
    seq: u64,
    post_connect: bool,
) -> Value {
    let IpinfoProbeInputs {
        http,
        status,
        config,
    } = inputs;
    let snap = build_ipinfo_snapshot(http.as_ref(), &status, post_connect).await;

    // 探测期间（含重试，最长 20s）可能已有更新的腿排上并落地 ⇒ 本腿结果作废。返回值仍给直接调用方
    // （`ipinfo_get` 的请求/响应语义），但绝不许污染全局缓存与广播。
    if !commit_ipinfo_snapshot(epoch, seq, &snap) {
        return snap;
    }
    crate::events::broadcast(
        app,
        crate::events::channel::EVENT_IP_INFO_UPDATED,
        snap.clone(),
    );

    // FX-warmttfb 出口伴测：代理出口探测成功那刻（隧道已热）→ fire-and-forget 补测活跃出口 warm RTT + 广播
    // EVENT_SPEED_TEST_RESULT，让切节点后 UI 延迟徽标自动刷新（对齐 上游 IpInfoService.onProxyProbeSuccess，
    // 置于广播之后触发保「IP 先显、延迟后到」）。探测失败（proxy=null）/ 直连 / 核未运行 → 门控内不 fire。
    // **延迟格是本腿的下游**：出口 IP 不自动探 ⇒ 伴测永不跑 ⇒ 延迟恒 `—`。两格同一条链、一次点亮。
    //
    // `epoch` + `seq` 一路传下去：伴测的 `serverId` 取自**开探时刻**的 config 快照，而测量本身是异步的
    // （fire-and-forget，秒级）。中途起停 / 热切会换掉出口，不复查就把新出口的 RTT 记到旧节点 id 上
    // —— 本批把伴测从「点一次才跑」改成「每次起停/热切都跑」后，这条路径的可达性显著上升。
    // 两条判据都必须传：只传世代时，「更新的腿已排程但还在睡（尚未领号）」这一整个 4s 窗口里复查恒真。
    let proxy_probed = snap.get("proxy").is_some_and(|v| !v.is_null());
    crate::commands::speedtest::spawn_warm_rtt_probe(
        app,
        &config,
        proxy_probed,
        status.running,
        status.mixed_port,
        epoch,
        seq,
    );

    snap
}

/// 收敛窗口占位快照：direct/proxy 双 null + `loading:true`。
///
/// 延迟腿在**睡之前**先发它，对齐 上游「started 瞬间清空 → `whenSelectorSettled` 后才真探」。
/// 不发的话，起核 / 热切后的收敛窗口里状态栏会继续显示**上一个出口**的 IP 与旗面 —— 把旧出口冒充成
/// 新出口，比留空更糟（与「不得用入口域名派生出口位置」是同一条纪律）。
///
/// ⚠️ **UI 表现是「置空」（`—`），不是可见的「检测中」文案**：`loading:true` 当前**无任何消费方**
/// （全仓 ipInfo 消费点只有 `StatusBar.tsx` 与 `HomeScreen.tsx`，两处都只读 `ip` / `countryCode`）。
/// 这符合用户已裁的「未探到就留空」，字段保留是为让消费方能区分「正在探」与「探完了但没探到」。
/// 别照着「UI 即刻进检测中」这句旧话去让 StatusBar 消费 `loading` —— 那是在实现一个已被否掉的状态。
fn pending_ipinfo_snapshot() -> Value {
    json!({
        "direct": Value::Null,
        "proxy": Value::Null,
        "updatedAt": u64::try_from(now_epoch_ms()).unwrap_or(u64::MAX),
        "loading": true,
    })
}

/// **出口 IP 自动重探排程**（事件驱动，**无轮询**）——移植 上游 `IpInfoService` 的触发表：
/// 启动 +2s（`runtime::startup_tasks`）· 起核就绪 · 节点热切换 · 停核（后三点经
/// [`ProxyErrorEmitter::schedule_exit_ip_refresh`](crate::runtime::proxy::ProxyErrorEmitter::schedule_exit_ip_refresh)）。
///
/// `delay_ms > 0` ⇒ 先广播 [`pending_ipinfo_snapshot`]（**UI 即刻置空成 `—`**，不是显示可见的
/// 「检测中」文案 —— 见该函数文档）并置 [`IPINFO_INFLIGHT`]，睡满再探。
///
/// # 🟠 按开探顺序发号：[`next_ipinfo_epoch`] 必须在 `sleep` **之后**
///
/// 世代号是「谁更新」的唯一判据，而排程时刻与开探时刻之间隔着整整 [`IPINFO_SETTLE_DELAY_MS`]。
/// 在**排程时**领号 ⇒ 号的顺序是「谁先被排上」，与「谁的结果更新」差一个维度，收敛窗口内会静默丢腿：
///
/// - t=0 起核就绪 → 本函数排程，睡 4s 前先领了号 N；
/// - t≈2 用户手点首页「网络检测」（按钮此刻**可点**：`disabled` 只看 `connected` 与解锁冷却）
///   → `ipinfo_get(force)` 领号 N+1 立刻开探。此刻正是选路未收敛的窗口，它大概率拿回 `proxy=null`；
/// - t=4 收敛腿醒来，睡前领的 N 已经旧了 ⇒ **原地退场、永不开探**。
///
/// 结果：赢的是设计自己判定为不可信的那一次探测，状态栏 `—`、两处旗面消失、`proxy_probed=false` 连伴测
/// 也不跑，而本模块**无轮询**（纯事件驱动）⇒ 没有任何后续动作会来纠正它。
///
/// 改成开探时领号后，上面三条真实序列的先后关系一条不变（`stale_probe_leg_must_not_overwrite_newer_leg`
/// 段 (a)/(b)/(c) 按开探时刻逐条复验），而收敛腿因为号更新而必然胜出。
///
/// 醒后那道旧闸随之删除：领号紧跟在 `sleep` 之后，比对的是零指令之前刚读的同一个值，恒真 = 死代码。
/// 探测**之后**那道闸仍在 [`probe_publish_ipinfo`] 里（挡住探测窗口 20s ≫ 排程间隔 4s 造成的乱序落地）。
///
/// # 🔴 但「开探时领号」只对一半：另一半是 [`IPINFO_SCHEDULE_SEQ`]
///
/// 把发号挪到开探时刻，代价是**排程时刻不再有任何记录**：两次热切间隔 >4s 时，先开探的旧腿落地那一
/// 刻世代仍是它自己的（更新的那条还在睡、尚未领号）⇒ 过闸 ⇒ 广播已切走节点的出口，并把它的 RTT 记进
/// 延迟徽标（持久错值）。故本函数**在 `sleep` 之前**还要自增一次 [`IPINFO_SCHEDULE_SEQ`]，腿在开探时
/// 快照它、落地时比对——「谁最新」由排程线管，「谁的世界快照最新」由世代线管，两条各管一维。
///
/// ⚠️ **必须用 [`tauri::async_runtime::spawn`]，不能用 `tokio::spawn`**（2026-07-21 真机 SIGABRT 血证，
/// 见 `runtime::unlock` 的 `schedule_self_run` 与 `mod spawn_guard`）：本函数的调用链可自**同步** command
/// 路径进入（Tauri 对 `pub fn` command 在主线程直接调、**无 Tokio runtime 上下文**）⇒ 裸 `tokio::spawn`
/// 当场 panic，而 panic 在 Tauri IPC 回调里无处可 catch ⇒ `abort()` ⇒ 整个应用崩溃。
pub fn schedule_ipinfo_refresh(app: &AppHandle, delay_ms: u64) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // 🔴 **排程即宣告**（sleep 之前，无条件）：这一刻起，任何已开探的旧腿都过期了。世代号做不到
        // 这件事——它要到 sleep 之后才领，而「已排程、尚未开探」的整个 4s 窗口里旧腿是无人作废的
        // （见 `IPINFO_SCHEDULE_SEQ` 文档的 t=4.1 序列）。零延迟腿（停核）同样宣告：出口消失也是
        // 一次「世界变了」，在飞的旧腿结果照样作废。
        next_ipinfo_schedule_seq();
        // 只包住「排在飞 + 广播 pending + sleep」——探测之后那道闸在 probe_publish_ipinfo 里，
        // 零延迟腿（停核）跳过本块但**同样**吃到它。
        //
        // 在飞计数只跟着**广播了置空帧**的延迟腿走：零延迟腿不广播 pending，若也计数就会造出
        // 「订阅方（状态栏）仍显示旧出口、peek 方（托盘）却已置空」的同屏矛盾——而消掉这种矛盾
        // 正是这个标记存在的全部理由。
        if delay_ms > 0 {
            // 先排位再广播：排位后 peek 才与订阅方看到同一帧（顺序反了会留一个吐旧出口的窗口）。
            IPINFO_INFLIGHT.fetch_add(1, Ordering::SeqCst);
            crate::events::broadcast(
                &app,
                crate::events::channel::EVENT_IP_INFO_UPDATED,
                pending_ipinfo_snapshot(),
            );
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        // 🟠 **睡满之后**才领号 ⇒ 号按开探顺序发（理由见本函数文档）。挪回 sleep 之前 = 收敛腿被
        // 窗口内的手点腿静默作废。排程线快照与它同刻取：两者合起来才是「我开探时的世界」。
        let epoch = next_ipinfo_epoch();
        let seq = current_ipinfo_schedule_seq();
        // `State` 借用收在本块内，不跨下面的 await（同 `ipinfo_get` 纪律）。
        // setup 前极早期 / 单测：managed state 还没有 ⇒ 静默跳过探测，绝不 panic。
        if let Some(inputs) = app
            .try_state::<AppRuntime>()
            .map(|state| ipinfo_probe_inputs(&state))
        {
            // 走**选路收敛延迟**的腿 = 起核 / 热切 ⇒ proxy 侧吃 post-connect 重试预算（隧道热身窗口）。
            // 判据是「等于收敛延迟」而非「有没有延迟」：启动首探也带延迟（3s，`EXIT_IP_PROBE_DELAY_MS`），
            // 但它不是热身场景，上游 那边同样走常规腿。停核腿（0）同理。
            // 逐触发点对照表 + 一处已登记偏离见 `build_ipinfo_snapshot` 文档。
            let post_connect = delay_ms == IPINFO_SETTLE_DELAY_MS;
            probe_publish_ipinfo(&app, inputs, epoch, seq, post_connect).await;
        }
        // **谁排的位谁归还**，且落地 / 被超越退场 / managed state 缺失三条路径共用这一个归还点。
        // 本函数体内既无 `return` 也无 `?`（spawn 不要求 `Output = ()` ⇒ `?` 同样能早退），两者
        // 由 `ipinfo_epoch_guard` 一并禁掉 —— 故当下不存在绕过这个归还点的路径。
        // 漏还则 peek 永久回置空帧、托盘/水合腿从此再也读不到缓存，而本模块无轮询、无人纠正。
        if delay_ms > 0 {
            IPINFO_INFLIGHT.fetch_sub(1, Ordering::SeqCst);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ══════════════════════════════════════════════════════════════════════════
    // 核在跑的真实日志级别：级别名投影
    // ══════════════════════════════════════════════════════════════════════════

    /// **必须是小写**，不只是「好看」：渲染端拿这个串直接与 `config.logLevel`（恒小写）比对来判
    /// 「核在跑的级别是否与我写下的值分叉」。返回 `WARN` 的话每一次比对都不相等 ⇒ 徽标恒亮分叉告警，
    /// 一个天天喊狼来了的自证等于没有自证。
    ///
    /// **变异锁**：去掉 `to_ascii_lowercase()` → 转红。
    #[test]
    fn runtime_level_name_is_lowercase_matching_config_log_level() {
        use polaris_singbox_grpc::daemon::LogLevel;
        assert_eq!(runtime_level_name(LogLevel::Warn), "warn");
        assert_eq!(runtime_level_name(LogLevel::Info), "info");
        assert_eq!(runtime_level_name(LogLevel::Debug), "debug");
        // sing-box 独有的两档（本仓生成侧永不写入，但读侧必须能原样说出来）。
        assert_eq!(runtime_level_name(LogLevel::Panic), "panic");
        assert_eq!(runtime_level_name(LogLevel::Trace), "trace");
    }

    // ══════════════════════════════════════════════════════════════════════════
    // R2 出口无效直判终态的**载荷折叠**（`fold_proxy_blocked`，1:1 上游
    // `IpInfoService.markProxyBlocked` :187-197）。纯逻辑、不碰任何进程级 static ⇒ 可并行跑，
    // 不受 `stale_probe_leg_must_not_overwrite_newer_leg` 那条「唯一碰 static 的测试」约束。
    // ══════════════════════════════════════════════════════════════════════════

    /// 带 direct + proxy + error 的既有缓存帧（折叠的取材面）。
    fn cached_frame_with_error() -> Value {
        json!({
            "direct": { "ip": "9.9.9.9", "countryCode": "CN" },
            "proxy": { "ip": "1.1.1.1", "countryCode": "HK" },
            "updatedAt": 1,
            "error": "上一轮探测超时",
        })
    }

    /// **代理出口清空、直连出口保留**（上游 `{...this.snapshot, proxy:null}` 的 spread 语义）。
    ///
    /// **变异锁**：把折叠改成「整帧重建」（`empty_ipinfo_snapshot()` 起手，丢掉 `cached`）→ direct
    /// 断言转红。那等于代理出口无效时把状态栏那格已探到的**本机**出口一并抹成 `—`，用户读到的是
    /// 「网络全挂」而不是「代理出口无效」——两者的下一步动作完全不同。
    #[test]
    fn fold_proxy_blocked_clears_proxy_but_keeps_direct() {
        let out = fold_proxy_blocked(Some(cached_frame_with_error()), "ts-exit-device-offline");
        assert!(out["proxy"].is_null(), "已知无效的代理出口必须清空");
        assert_eq!(
            out["direct"]["ip"],
            json!("9.9.9.9"),
            "直连出口与代理出口无效互不相干，不得被一并抹掉"
        );
    }

    /// **`proxyBlocked` 置原因 + `loading:false`**（终态，不是「还在探」）。
    ///
    /// **变异锁**：漏 `loading:false` → 缓存里留着上一帧的 `loading:true` ⇒ peek 型消费方永远读到
    /// 「检测中」，而实际上根本没有任何探测在飞、也永远不会有。
    #[test]
    fn fold_proxy_blocked_marks_terminal_state_with_reason() {
        let out = fold_proxy_blocked(Some(json!({ "loading": true })), "ts-no-exit-device");
        assert_eq!(out["proxyBlocked"], json!("ts-no-exit-device"));
        assert_eq!(out["loading"], json!(false), "直判终态不得留在「检测中」");
    }

    /// **`error` 必须删键，不是置 null**：`blocked`（已知无效、压根没探）与 `error`（探了但失败）是
    /// 互斥语义，同帧并存会让 UI 同时收到两个终态。
    ///
    /// **变异锁**：把 `obj.remove("error")` 改成 `insert("error", Null)` → `get("error")` 变
    /// `Some(Null)` ⇒ `is_none()` 转红（前端 `error !== undefined` 的判据会被 null 骗过）。
    #[test]
    fn fold_proxy_blocked_drops_stale_error_key() {
        let out = fold_proxy_blocked(Some(cached_frame_with_error()), "ts-exit-not-advertised");
        assert!(
            out.get("error").is_none(),
            "blocked 与 error 互斥：上一轮的 error 必须删键而非置 null"
        );
    }

    /// **从未探过（缓存空）也要落成完整终态帧**，而不是 panic / 回半截帧。
    ///
    /// 真实可达：冷启动后用户还没点过「网络检测」，选中的 TS 出口即被直判无效。
    #[test]
    fn fold_proxy_blocked_handles_empty_cache() {
        let out = fold_proxy_blocked(None, "ts-no-exit-device");
        assert!(out["direct"].is_null() && out["proxy"].is_null());
        assert_eq!(out["proxyBlocked"], json!("ts-no-exit-device"));
        assert_eq!(out["loading"], json!(false));
        assert!(out["updatedAt"].as_u64().is_some(), "updatedAt 须为数字");
    }

    /// 缓存被写坏成非 object（防御面）→ 从空快照重建，绝不 panic、也绝不把坏值当基底往下发。
    #[test]
    fn fold_proxy_blocked_recovers_from_non_object_cache() {
        let out = fold_proxy_blocked(Some(json!("garbage")), "ts-no-exit-device");
        assert_eq!(out["proxyBlocked"], json!("ts-no-exit-device"));
        assert!(out["direct"].is_null());
    }

    /// `updatedAt` 必须**刷新**（不能沿用缓存里的旧值）——前端/托盘按它判新旧帧，不刷新 ⇒ 终态帧
    /// 会被当成陈旧帧丢弃。**变异锁**：删掉 `updatedAt` 的 insert → 沿用 `1` → 转红。
    #[test]
    fn fold_proxy_blocked_refreshes_updated_at() {
        let out = fold_proxy_blocked(Some(cached_frame_with_error()), "ts-no-exit-device");
        assert!(
            out["updatedAt"].as_u64().is_some_and(|t| t > 1),
            "updatedAt 必须刷成当前时刻，不得沿用缓存里的旧值"
        );
    }

    /// **「检测中」占位快照的形状**：延迟腿睡之前发的这一帧，必须把 direct/proxy **双双置 null**。
    ///
    /// 只发 `loading:true` 而留着旧 direct/proxy 是最坏解：状态栏会在起核/热切后的 4s 收敛窗口里
    /// 继续显示**上一个出口**的 IP 与旗面，等于用旧出口冒充新出口（与「不得用入口域名派生出口位置」
    /// 是同一条纪律）。
    ///
    /// **变异锁**：任一字段改回沿用旧值 / 漏掉 `loading` → 转红。
    #[test]
    fn pending_snapshot_blanks_both_exits() {
        let snap = pending_ipinfo_snapshot();
        assert!(snap["direct"].is_null(), "收敛窗口内不得留着上一个直连出口");
        assert!(
            snap["proxy"].is_null(),
            "收敛窗口内不得留着上一个代理出口——那正是「旧出口冒充新出口」"
        );
        assert_eq!(
            snap["loading"],
            json!(true),
            "须显式标注「检测中」，与「探完了但没探到」区分开"
        );
        assert!(
            snap["updatedAt"].as_u64().is_some(),
            "updatedAt 须为数字（前端/托盘按它判新旧帧）"
        );
    }

    // ── 探测重试（1:1 移植 上游 `IpInfoService.withRetry`）──
    //
    // 时间用 `start_paused = true`：`sleep` / `timeout` 由 tokio 自动推进虚拟时钟 ⇒ 断言的是**真实
    // 的间隔与预算算术**，且零墙钟耗时、不碰宿主网络。

    /// 调用计数器（每个测试各持一份，互不干扰）。
    type Calls = std::rc::Rc<std::cell::Cell<usize>>;

    /// 造一个「前 `fail_times` 次失败、之后成功」的尝试动作，调用次数记进 `calls`。
    fn flaky_attempt(
        calls: Calls,
        fail_times: usize,
    ) -> impl FnMut() -> std::future::Ready<Result<Value, String>> {
        move || {
            let n = calls.get();
            calls.set(n + 1);
            std::future::ready(if n < fail_times {
                Err(format!("第 {} 次失败", n + 1))
            } else {
                Ok(json!({ "ip": "203.0.113.1" }))
            })
        }
    }

    /// **成功即止**：第一次就成功时不得再试第二次（重试是补救，不是加压）。
    #[tokio::test(start_paused = true)]
    async fn retry_stops_at_first_success() {
        let calls: Calls = Default::default();
        let out = with_ipinfo_retry(
            flaky_attempt(calls.clone(), 0),
            IPINFO_DIRECT_ATTEMPTS,
            IPINFO_DIRECT_RETRY_MS,
        )
        .await;
        assert!(out.is_ok(), "首次成功却回了失败");
        assert_eq!(
            calls.get(),
            1,
            "首次成功后仍继续重试 ⇒ 每次探测都在给出口 IP 端点做无谓加压"
        );
    }

    /// **失败会重试**（本轮要根治的症状）：一次失败不再是终局。
    ///
    /// 变异锁：把重试删成单次（`attempts` 恒 1 / 循环体 `break`）→ 转红。
    #[tokio::test(start_paused = true)]
    async fn retry_recovers_from_transient_failure() {
        let calls: Calls = Default::default();
        let out = with_ipinfo_retry(
            flaky_attempt(calls.clone(), 2),
            IPINFO_DIRECT_ATTEMPTS,
            IPINFO_DIRECT_RETRY_MS,
        )
        .await;
        assert!(
            out.is_ok(),
            "3 次预算内第 3 次成功，却回了失败 ⇒ 起核瞬间的一次抖动就把状态栏 IP/旗面/延迟三格一起打空，\
             而本模块无轮询、只能等用户再点一次「网络检测」"
        );
        assert_eq!(calls.get(), 3, "定额 3 次应恰好用满到成功那次");
    }

    /// **额度用尽回最后一次的错**：诊断要看终态，不是首次抖动。
    #[tokio::test(start_paused = true)]
    async fn retry_exhausts_budget_and_reports_last_error() {
        let calls: Calls = Default::default();
        let out = with_ipinfo_retry(
            flaky_attempt(calls.clone(), usize::MAX),
            IPINFO_PROXY_ATTEMPTS,
            IPINFO_PROXY_RETRY_MS,
        )
        .await;
        assert_eq!(
            out.unwrap_err(),
            "第 2 次失败",
            "应冒泡**最后**一次的错误（回首次错会把「一直没好」误报成「一开始没好」）"
        );
        assert_eq!(
            calls.get(),
            IPINFO_PROXY_ATTEMPTS as usize,
            "常规 proxy 腿定额 2 次"
        );
    }

    /// **总预算封顶**：间隔 × 次数超出 [`IPINFO_PROBE_BUDGET_MS`] 时，到点即止，不跑满次数。
    ///
    /// 这是 post-connect 腿（4×4s = 12s > 10s 预算）的真实形态 —— 上游 同款截断
    /// （`IpInfoService.ts:257-290` 的 deadline 检查 + 赛跑 `setTimeout`）。
    ///
    /// 变异锁：去掉 `tokio::time::timeout` 封顶 → 调用次数变 4、转红。
    #[tokio::test(start_paused = true)]
    async fn retry_is_capped_by_the_total_budget() {
        let calls: Calls = Default::default();
        let started = tokio::time::Instant::now();
        let out = with_ipinfo_retry(
            flaky_attempt(calls.clone(), usize::MAX),
            IPINFO_PROXY_POST_CONNECT_ATTEMPTS,
            IPINFO_PROXY_POST_CONNECT_RETRY_MS,
        )
        .await;
        let elapsed = started.elapsed();
        assert!(out.is_err(), "全程失败却回了成功");
        assert!(
            elapsed <= Duration::from_millis(IPINFO_PROBE_BUDGET_MS),
            "跑过了总预算（{elapsed:?}）⇒ 在飞窗口无界拉长，peek 型消费方（托盘/水合腿）跟着空更久"
        );
        assert!(
            calls.get() < IPINFO_PROXY_POST_CONNECT_ATTEMPTS as usize,
            "4×4s = 12s 超出 10s 预算，必须被截断在第 4 次之前（实际 {} 次）",
            calls.get()
        );
    }

    /// 读当前缓存快照（`commit_ipinfo_snapshot` 的落地结果）。
    fn cached_snapshot() -> Value {
        ipinfo_cache()
            .lock()
            .unwrap()
            .clone()
            .expect("前置：本测试已至少落地过一次快照")
    }

    /// 造一份带代理出口的快照（`cc` = 代理出口地区码）。
    fn snap_with_proxy(ip: &str, cc: &str) -> Value {
        json!({
            "direct": { "ip": "9.9.9.9", "countryCode": "CN" },
            "proxy": { "ip": ip, "countryCode": cc },
            "updatedAt": 1,
        })
    }

    /// 造一份**没有**代理出口的快照（核未起 / 已停 ⇒ proxy=null）。
    fn snap_direct_only() -> Value {
        json!({
            "direct": { "ip": "9.9.9.9", "countryCode": "CN" },
            "proxy": Value::Null,
            "updatedAt": 1,
        })
    }

    /// 一条腿**开探那一刻**取的完整判据（世代 + 排程线快照）—— 与生产代码里 `let epoch = …;
    /// let seq = …;` 那两行同刻同序。测试里所有「开探」都必须走它，否则模型与实现就漂了。
    /// 排程（[`next_ipinfo_schedule_seq`]）则由各段按事件时刻**单独**调，那才是本轮修的那一维。
    fn probe_start() -> (u64, u64) {
        (next_ipinfo_epoch(), current_ipinfo_schedule_seq())
    }

    /// 🔴 **回归**：被更新的腿超越的旧腿，绝不许落地（不写缓存、不广播、不伴测）。
    ///
    /// # 缺陷长相
    ///
    /// 世代闸原先**只在探测「之前」查一次**，而 [`build_ipinfo_snapshot`] 最长跑
    /// `IPINFO_PROBE_BUDGET_MS × 2 = 20s`（direct + proxy 两腿串行，各含定额重试），排程间隔却只有 4s ——
    /// **探测窗口远大于排程间隔**，先发起的慢腿完全可能在后发起的快腿之后落地，同时污染
    /// `IPINFO_CACHE` 与广播。且 `delay_ms == 0` 的停核腿连那一次都不查。
    ///
    /// # 时刻口径：两条时间线，各段按**真实事件顺序**逐个调
    ///
    /// - [`next_ipinfo_schedule_seq`] = 一次**排程 / 事件**（起核就绪、停核、热切、启动腿、手点）；
    /// - [`probe_start`] = 一次**开探**（睡满之后领世代 + 快照排程线，与读 status/config 同刻）。
    ///
    /// 故段内的调用先后 == 真机上的事件/开探先后。段 (a)/(b)/(c)/(d) 的开探先后是
    /// startup t=3s < ready t≈6.5s、stop t=0 < restart t≈5.5s、B t=4s < C t=5s、手点 t≈2 < 收敛 t=4，
    /// 四条的排程都落在两腿开探之前 ⇒ 排程线同值、**由世代定序**，四段原样成立。
    /// 段 (f) 是唯一「排程夹在两次开探之间」的形态 —— 世代闸对它天生失明，只有排程线认得出。
    ///
    /// # 为什么各段挤在同一个 `#[test]` 里
    ///
    /// 各段共用 `IPINFO_REFRESH_EPOCH` / `IPINFO_CACHE` / `IPINFO_INFLIGHT` 三个**进程级 static**；
    /// 拆成多个测试会被 cargo 的并行 runner 交错执行而互相污染（一段领的世代把另一段的腿作废掉、
    /// 一段置的在飞标记让另一段的 peek 吐置空帧），从而变成随机假红。
    ///
    /// ⚠️ 本测试是全仓**唯一**碰这三个 static 的测试，这一点必须保持：将来再加动世代 / 动缓存 /
    /// 动在飞标记的测试，要么并进本函数，要么给它们配一把测试锁 —— 另起一个 `#[test]` 会让两边都变
    /// 成随机红。
    ///
    /// **变异锁**：删掉 [`commit_ipinfo_snapshot`] 里的世代比对 → 段 (a)–(d) 转红；删掉排程线比对
    /// → 段 (f) 转红（段 (a)–(d) **全绿**，这正是第三轮复审逮到的那一半）。
    #[test]
    fn stale_probe_leg_must_not_overwrite_newer_leg() {
        // ── 前提：世代**严格单调递增且互不相等** ──
        // 若两条腿能领到同一个号，「后来者胜」就退化成「两条都算最新」，下面整道闸形同虚设。
        // **变异锁**：把 `next_ipinfo_epoch` 的 `fetch_add(1, …) + 1` 改成 `load(…) + 1` → 此处转红。
        let (e1, e2, e3) = (
            next_ipinfo_epoch(),
            next_ipinfo_epoch(),
            next_ipinfo_epoch(),
        );
        assert!(e1 < e2 && e2 < e3, "世代号必须严格递增：{e1} / {e2} / {e3}");
        // 排程线同理：两次事件领到同一个号 ⇒ 「我开探后世界又变了」这件事无从表达。
        let (s1, s2) = (next_ipinfo_schedule_seq(), next_ipinfo_schedule_seq());
        assert!(s1 < s2, "排程线必须严格递增：{s1} / {s2}");

        // ── 序列 (a) 冷启动：startup 腿 t=3s 开探（慢），起核就绪腿 t≈6.5s 开探（快，先落地）──
        // 两次排程（startup_tasks t≈1、autoconnect 起核就绪 t≈2.5）都发生在两腿开探**之前**
        // ⇒ 两腿快照到同一个排程线值，本序列纯由世代定序。
        next_ipinfo_schedule_seq(); // t≈1 startup_tasks 排程
        next_ipinfo_schedule_seq(); // t≈2.5 起核就绪 → 排程
        let (startup, startup_seq) = probe_start(); // t=3
        let (started, started_seq) = probe_start(); // t≈6.5
        assert_eq!(
            startup_seq, started_seq,
            "两腿都在最后一次排程之后开探 ⇒ 排程线同值，本序列的判据只剩世代"
        );

        let fresh = snap_with_proxy("1.1.1.1", "HK");
        assert!(
            commit_ipinfo_snapshot(started, started_seq, &fresh),
            "最新一腿必须能落地，否则这道闸就成了「谁都别想发布」的死规则"
        );
        // startup 腿 t≈10s 才探完，此刻核还没起 ⇒ 它手里是 proxy=null。
        assert!(
            !commit_ipinfo_snapshot(startup, startup_seq, &snap_direct_only()),
            "冷启动慢腿必须退场：它一落地就把代理出口盖成 null ⇒ 状态栏回退 '—'、旗面消失"
        );
        assert_eq!(
            cached_snapshot()["proxy"]["countryCode"],
            json!("HK"),
            "缓存被旧腿盖回 direct-only ⇒ 序列 (a) 复现"
        );

        // ── 序列 (b) 停核 → 1.5s 后起核：停核腿零延迟、t=0 就开探；起核腿 t≈5.5s 才开探 ──
        next_ipinfo_schedule_seq(); // t=0 停核事件（零延迟腿：排程与开探同刻）
        let (stopped, stopped_seq) = probe_start();
        next_ipinfo_schedule_seq(); // t≈1.5 起核就绪 → 排程（睡 4s）
        let (restarted, restarted_seq) = probe_start(); // t≈5.5 开探
        assert!(commit_ipinfo_snapshot(
            restarted,
            restarted_seq,
            &snap_with_proxy("2.2.2.2", "JP")
        ));
        assert!(
            !commit_ipinfo_snapshot(stopped, stopped_seq, &snap_direct_only()),
            "零延迟停核腿完全跳过睡前那道闸，只能靠探测**之后**这道闸挡住"
        );
        assert_eq!(
            cached_snapshot()["proxy"]["countryCode"],
            json!("JP"),
            "停核腿把刚起的新出口盖成 null ⇒ 序列 (b) 复现"
        );

        // ── 序列 (c) 连点热切 B→C（间隔 <4s）：两次排程都落在两腿开探之前 ⇒ 排程线同值，
        // 由世代定序。B 腿 t=4s 开探（慢），C 腿 t=5s 开探（快）先落地。
        // 间隔 >4s 的那个变体（世代闸认不出）见段 (f)。
        next_ipinfo_schedule_seq(); // t=0 切到 B
        next_ipinfo_schedule_seq(); // t=1 切到 C
        let (node_b, node_b_seq) = probe_start(); // t=4
        let (node_c, node_c_seq) = probe_start(); // t=5
        assert_eq!(
            node_b_seq, node_c_seq,
            "连点（<4s）两腿快照到同一个排程线值"
        );
        assert!(commit_ipinfo_snapshot(
            node_c,
            node_c_seq,
            &snap_with_proxy("3.3.3.3", "SG")
        ));
        assert!(
            !commit_ipinfo_snapshot(node_b, node_b_seq, &snap_with_proxy("4.4.4.4", "HK")),
            "B 腿后到必须退场，否则状态栏长期显示已经切走的 B 的出口 IP 与旗面"
        );
        let cached = cached_snapshot();
        assert_eq!(
            cached["proxy"]["ip"],
            json!("3.3.3.3"),
            "序列 (c) 复现：显示的是切走的那个节点"
        );
        assert_eq!(cached["proxy"]["countryCode"], json!("SG"));

        // ── 🟠 序列 (d) 收敛窗口内手点「网络检测」：**新增的回归段** ──
        //
        // 真机路径：t=0 起核就绪 → 排程 4s 收敛腿；t≈2 用户点首页「网络检测」（按钮此刻可点：
        // `disabled={!connected || unlockCooldown}`，而 `unlockCooldown` 派生自解锁 `lastRunAt`，
        // 重连后通常 >15s ⇒ 不置灰）→ `ipinfoApi.get(true, true)` force 绕过 TTL 立刻开探。
        // 选路尚未收敛 ⇒ 这一次**大概率拿回 `proxy=null`**（这正是那 4s 存在的全部理由）。
        //
        // 旧实现在**排程时**（t=0）就领了号 ⇒ 手点腿（t=2）领到更大的号 ⇒ t=4 收敛腿醒来一比对即
        // 判过期、**原地退场、永不开探**；赢的是设计自己判定为不可信的那次探测，且本模块无轮询，
        // 没有任何后续动作会来纠正 —— 状态栏 `—`、两处旗面消失、`proxy_probed=false` 连伴测都不跑。
        //
        // 改成**开探时**领号后，先后关系颠倒过来：手点腿 t=2 先领，收敛腿 t=4 后领 ⇒ 收敛腿胜。
        next_ipinfo_schedule_seq(); // t=0 起核就绪 → 排程收敛腿（睡 4s）
        next_ipinfo_schedule_seq(); // t≈2 用户手点：force 腿的排程与开探同刻
        let (manual_click, manual_seq) = probe_start(); // t≈2 开探
        assert!(
            commit_ipinfo_snapshot(manual_click, manual_seq, &snap_direct_only()),
            "手点腿此刻是最新的一条，它自己必须能落地（否则用户点了按钮什么都不会发生）"
        );
        let (settled, settled_seq) = probe_start(); // t=4：收敛腿睡满后才领号、才开探
        assert!(
            settled > manual_click,
            "🟠 号必须按**开探**顺序发：排程时领号会让 t=0 排上的收敛腿(号 {settled})\
             反而旧于 t≈2 的手点腿(号 {manual_click})，收敛腿于是永不开探"
        );
        assert_eq!(
            settled_seq, manual_seq,
            "手点腿的排程发生在收敛腿开探**之前** ⇒ 两腿快照到同一个排程线值，本序列仍由世代定序；\
             若排程线在这里把收敛腿判过期，本轮新加的那一半判据就把 round-2 修好的洞又打开了"
        );
        assert!(
            commit_ipinfo_snapshot(settled, settled_seq, &snap_with_proxy("6.6.6.6", "JP")),
            "收敛后那条重探腿必须能落地 —— 它才是唯一能拿到真出口的一次探测"
        );
        assert_eq!(
            cached_snapshot()["proxy"]["ip"],
            json!("6.6.6.6"),
            "序列 (d) 复现：收敛腿被窗口内的手点腿静默作废，状态栏停在 proxy=null（`—` + 无旗面）"
        );

        // ── 🔴 序列 (f) 两次热切间隔 >4s：**世代闸对它天生失明**（第三轮复审的回归段）──
        //
        // 真机路径（间隔 >4s 完全常规）：
        //   t=0   热切到 B → L1 置在飞 + 广播置空，睡到 t=4；
        //   t=4.0 L1 醒 → 领世代 → 读 status/config（selected=B）→ 经 B 的隧道开探；
        //   t=4.1 热切到 C → L2 排程，睡到 t=8.1 —— **它要到 t=8.1 才领世代**；
        //   t=5.0 L1 探完落地：`IPINFO_REFRESH_EPOCH` 仍是 L1 自己的号 ⇒ 过闸。
        //
        // 后果两条、性质不同：广播 B 的出口（状态栏 + 两处旗面显示已切走的节点，~4s 后自愈），
        // 以及 `spawn_warm_rtt_probe` 把**经 C 的隧道量到的 RTT** 写进 B 的延迟徽标 ——
        // 后者**持久**（`latencyMap[B]` 保留错值到下次测 B 为止），而那道复查存在的全部理由
        // 就是「记错比不记更糟」。
        //
        // 根因：一个计数器兼了两件事 ——「谁最新」（该在**排程**时宣告）与「谁的世界快照最新」
        // （该在**开探**时取号）。round-1 用排程时刻做后者、round-2 用开探时刻做前者，两边都只对一半。
        next_ipinfo_schedule_seq(); // t=0 热切到 B → L1 排程
        let (l1, l1_seq) = probe_start(); // t=4.0 L1 领世代 + 快照排程线 → 开探（走 B）
        next_ipinfo_schedule_seq(); // t=4.1 热切到 C → L2 排程（尚未领世代）
        assert_eq!(
            IPINFO_REFRESH_EPOCH.load(Ordering::SeqCst),
            l1,
            "前置：L2 还在睡、尚未领号 ⇒ 世代仍是 L1 自己的 —— 这正是世代闸在本序列里失明的原因，\
             也是为什么本段的红/绿完全取决于排程线那一半判据"
        );
        assert!(
            !commit_ipinfo_snapshot(l1, l1_seq, &snap_with_proxy("8.8.8.8", "HK")),
            "🔴 睡眠中的新腿必须能作废在飞的旧腿：L1 落地会广播已切走的 B 的出口，\
             并把经 C 隧道量到的 RTT 持久写进 B 的延迟徽标"
        );
        assert_eq!(
            cached_snapshot()["proxy"]["ip"],
            json!("6.6.6.6"),
            "序列 (f) 复现：缓存被 B 腿盖掉（peek 型消费方随即吐已切走的节点）"
        );
        let (l2, l2_seq) = probe_start(); // t=8.1 L2 醒 → 领世代开探
        assert!(
            commit_ipinfo_snapshot(l2, l2_seq, &snap_with_proxy("5.5.5.5", "SG")),
            "L2 是最新的一条，它自己必须能落地（否则这道闸又成了「谁都别想发布」）"
        );
        assert_eq!(cached_snapshot()["proxy"]["ip"], json!("5.5.5.5"));

        // ── 🔵 段 (e) 在飞**计数**：谁排的位谁归还，落地一律不清位 ──
        //
        // 缓存里此刻是段 (f) 落地的 5.5.5.5/SG（= 上一个出口）。起核/热切排程腿一排位，peek 型消费方
        // （托盘浮层每次弹出即 peek、主窗窗口重建水合）就必须与订阅方看到同一帧「置空」，否则同屏两处
        // 对「我现在从哪出去」给出互相矛盾的答案，且错的那个是用旧出口冒充新出口。
        //
        // **变异锁**：① 删掉 `peek_ipinfo_snapshot` 的在飞分支（退回「无条件读缓存」）→ 转红；
        // ② 把计数退回 `AtomicBool` 的 `store(true)/store(false)`（L1 归还即清掉 L2 的位）→ 转红；
        // ③ 把归还搬回 `commit_ipinfo_snapshot`（落地即清位）→ 转红。
        assert_eq!(IPINFO_INFLIGHT.load(Ordering::SeqCst), 0, "前置：无腿在飞");
        assert_eq!(
            peek_ipinfo_snapshot()["proxy"]["ip"],
            json!("5.5.5.5"),
            "前置：未在飞时 peek 读缓存（这条同时钉住「别把 peek 改成恒回置空帧」）"
        );

        IPINFO_INFLIGHT.fetch_add(1, Ordering::SeqCst); // L1 排程（切到 B）
        let peeked = peek_ipinfo_snapshot();
        assert!(
            peeked["proxy"].is_null() && peeked["direct"].is_null(),
            "在飞时 peek 仍吐上一个出口 ⇒ 托盘浮层/水合腿把旧出口冒充成新出口"
        );
        assert_eq!(
            peeked["loading"],
            json!(true),
            "在飞帧须与订阅方那一帧同形（含 loading 标记）"
        );

        // 缓存**不得**被 pending 帧污染：非 force 的 TTL 短路读的是同一份缓存，写进去会让收敛窗口后
        // 15s 内的每次 `ipinfo_get` 都短路拿到双 null（把「正在探」固化成「探完了没探到」）。
        // 这正是 reviewer 点名「不能靠把 pending 写进缓存解决」的那条。
        assert_eq!(
            cached_snapshot()["proxy"]["ip"],
            json!("5.5.5.5"),
            "在飞标记绝不许顺手写缓存 —— 那会毒化 fresh_cached_snapshot"
        );

        IPINFO_INFLIGHT.fetch_add(1, Ordering::SeqCst); // L2 排程（切到 C，L1 仍在飞）
                                                        // L1 落地（哪怕它这一次真的过了闸）**不得**清位 —— 位是排程腿自己排的。
        let (landed, landed_seq) = probe_start();
        assert!(commit_ipinfo_snapshot(
            landed,
            landed_seq,
            &snap_with_proxy("7.7.7.7", "SG")
        ));
        assert_eq!(
            IPINFO_INFLIGHT.load(Ordering::SeqCst),
            2,
            "落地不得清位：`AtomicBool` 时代 L1 一落地就把 L2 排的位也清了 —— \
             L2 剩下的 3s 收敛窗口里 peek 型消费方照吐已切走节点的缓存值"
        );
        IPINFO_INFLIGHT.fetch_sub(1, Ordering::SeqCst); // L1 跑完归还自己那一格
        assert!(
            peek_ipinfo_snapshot()["proxy"].is_null(),
            "L2 仍在收敛窗口里 ⇒ peek 必须继续置空，绝不许因为 L1 跑完就提前开窗"
        );

        IPINFO_INFLIGHT.fetch_sub(1, Ordering::SeqCst); // L2 跑完归还
        assert_eq!(
            IPINFO_INFLIGHT.load(Ordering::SeqCst),
            0,
            "全部归还后计数须归零"
        );
        assert_eq!(
            peek_ipinfo_snapshot()["proxy"]["ip"],
            json!("7.7.7.7"),
            "归还后 peek 须回到读缓存，且读到的是最后落地的那个出口"
        );

        // ── 🔴 段 (g) 出口**直判无效终态**：mark 必须宣告排程线，否则在飞探测腿把终态盖回去 ──
        //
        // 真机路径：
        //   t=0    TS 隧道就绪边沿 → 排一次 refresh（收敛 4s）；
        //   t=4    腿醒来领世代 + 快照排程线 → 开探（proxy 侧预算最长 20s）；
        //   t=5    exit peer 掉线 → `reconcile_ts_exit_block` 跨态 → `mark_exit_blocked`
        //          → `mark_ipinfo_proxy_blocked` 直落 `proxyBlocked` 终态；
        //   t=15   探测腿落地。不宣告时**两个计数器在整段无人自增** ⇒ 恒过闸 ⇒ 用一个对已知无效出口
        //          的探测结果（`proxy:null` + `error`）覆盖 `proxyBlocked`。
        //
        // 为什么覆盖了就回不来：`reconcile_ts_exit_block` 是**边沿**触发、同态帧直接早退 ⇒ 终态
        // **不会重落**，用户看到的「检测失败」一直挂到下一次真跨态（本模块无轮询，无人纠正）。
        //
        // **变异锁**：删掉 `commit_proxy_blocked_snapshot` 体首那行 `next_ipinfo_schedule_seq();`
        // → 本段转红（这一段调的就是生产函数本体，不是复刻）。
        next_ipinfo_schedule_seq(); // t=0 TS 隧道就绪 → 排 refresh（睡 4s）
        let (blocked_probe, blocked_probe_seq) = probe_start(); // t=4 腿醒来开探
                                                                // t=5 直判终态落地（= `mark_ipinfo_proxy_blocked` 去掉广播的那一半，见该函数文档）。
        commit_proxy_blocked_snapshot("ts-exit-device-offline");
        assert_eq!(
            cached_snapshot()["proxyBlocked"],
            json!("ts-exit-device-offline"),
            "前置：终态已落进权威缓存（peek 型消费方就是从这里读的）"
        );
        // t=15 在飞腿落地 —— 必须退场。
        assert!(
            !commit_ipinfo_snapshot(blocked_probe, blocked_probe_seq, &snap_direct_only()),
            "🔴 直判终态之后落地的在飞腿必须退场：它探的是一个**已知无效**的出口，结果只可能是 \
             null/error，而覆盖掉 proxyBlocked 之后终态不会重落（reconcile 同态早退）"
        );
        assert_eq!(
            cached_snapshot()["proxyBlocked"],
            json!("ts-exit-device-offline"),
            "段 (g) 复现：状态栏从「出口无效」被改写成「检测失败」，并一直挂到下一次真跨态"
        );
    }

    /// 唯一临时目录（无 `tempfile` 依赖，对齐 icon_cache/updater 测试范式；用完 `remove_dir_all`）。
    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-misc-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A6：清面板缓存目录须真删（变异：`clear_singbox_dashboard_cache` 退回 no-op 桩时此断言转红）。
    #[test]
    fn clear_dashboard_cache_removes_existing_dir() {
        let root = temp_dir("dash-clear");
        let dash = root.join(SINGBOX_DASHBOARD_DIR);
        std::fs::create_dir_all(&dash).unwrap();
        std::fs::write(dash.join("index.html"), b"<html></html>").unwrap();
        std::fs::write(dash.join(".etag"), b"abc").unwrap();
        assert!(dash.exists(), "前置：缓存目录应存在");

        clear_singbox_dashboard_cache(&dash);

        assert!(!dash.exists(), "清缓存后目录须被删除");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A6：目录不存在时清理须幂等、不 panic（best-effort 语义，对齐 上游 `force: true`）。
    #[test]
    fn clear_dashboard_cache_missing_dir_is_noop() {
        let root = temp_dir("dash-missing");
        let dash = root.join(SINGBOX_DASHBOARD_DIR); // 从未创建
        assert!(!dash.exists());
        clear_singbox_dashboard_cache(&dash);
        assert!(!dash.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 面板语言映射：繁体前缀 → zh-Hant；简体/其它 zh → zh-Hans；fa/ru 命中；缺省/未知 → en。
    #[test]
    fn dashboard_lang_maps_by_prefix() {
        assert_eq!(map_locale_to_dashboard_lang(Some("zh-CN")), "zh-Hans");
        assert_eq!(map_locale_to_dashboard_lang(Some("zh-Hans")), "zh-Hans");
        assert_eq!(map_locale_to_dashboard_lang(Some("zh-TW")), "zh-Hant");
        assert_eq!(map_locale_to_dashboard_lang(Some("zh-Hant")), "zh-Hant");
        assert_eq!(map_locale_to_dashboard_lang(Some("fa-IR")), "fa");
        assert_eq!(map_locale_to_dashboard_lang(Some("ru")), "ru");
        assert_eq!(map_locale_to_dashboard_lang(Some("en-US")), "en");
        assert_eq!(map_locale_to_dashboard_lang(None), "en");
    }

    /// preload 脚本：写两个权威键 + 语言键；且**含引号的 secret 经双重序列化后不破坏脚本**（防注入）。
    /// 变异门：把 `serde_json::to_string(&…to_string())` 退成裸拼接 → 含 `"` 的 secret 会截断字面量 →
    /// 解析出的 JSON 不再含完整 secret → 下面 `payload["secret"]` 断言转红。
    #[test]
    fn dashboard_preload_script_injects_keys_and_escapes_secret() {
        let evil_secret = r#"a"b\c'd"#; // 引号 + 反斜杠 + 单引号
        let s = build_dashboard_preload_script("127.0.0.1:9090", evil_secret, "zh-Hans");
        assert!(s.contains("sing-box-dashboard.servers"));
        assert!(s.contains("sing-box-dashboard.server'"), "须写旧版迁移键");
        assert!(s.contains("sing-box-dashboard.language"));

        // 提取 servers setItem 的 JS 字面量 → 反序列化两层 → 校验 secret/url 完整无损。
        let marker = "ls.setItem('sing-box-dashboard.servers',";
        let start = s.find(marker).unwrap() + marker.len();
        let rest = &s[start..];
        let end = rest.find(");").unwrap();
        let js_literal = &rest[..end]; // 形如 "{\"servers\":[…]}"（含外层引号的 JS 字符串字面量）
        let inner_json: String =
            serde_json::from_str(js_literal).expect("外层字面量应为合法 JSON 字符串");
        let payload: Value = serde_json::from_str(&inner_json).expect("内层应为合法 JSON");
        assert_eq!(payload["activeId"], "polaris");
        assert_eq!(payload["servers"][0]["url"], "127.0.0.1:9090");
        assert_eq!(
            payload["servers"][0]["secret"], evil_secret,
            "含引号/反斜杠的 secret 须原样无损（双重序列化防注入）"
        );
    }

    // ── 日志直播流：`_id` 出境 + UI 不活跃期的单批截断 ──

    fn log_rec(seq: u64, msg: &str) -> crate::logging::LogRecord {
        crate::logging::LogRecord {
            seq,
            ts_ms: 1_700_000_000_000,
            level: "info",
            target: "app".into(),
            message: msg.into(),
        }
    }

    /// `_id` 必须随每条日志出境，且**原样**是后端的单调 seq。
    ///
    /// 打断这条（不发 `_id` / 改用 timestamp 派生）→ 渲染端只能退回 `timestamp-index` 作 key：
    /// 环形缓冲一滑动全列换身份（滚动期全量重渲 + 打断选区），且水合与增量流那 ≤150ms 的重叠窗口
    /// 无从去重（同一条日志渲染两遍）。
    #[test]
    fn log_entry_carries_monotonic_id() {
        let a = log_record_to_entry(&log_rec(41, "first"));
        let b = log_record_to_entry(&log_rec(42, "second"));
        assert_eq!(a["_id"], json!(41), "_id 必须原样带出后端 seq");
        assert!(
            a["_id"].as_u64() < b["_id"].as_u64(),
            "_id 必须单调递增——去重键靠「≤ 已见最大值即丢」，非单调即漏行/重放"
        );
        // 其余契约字段不得因加 _id 而漂。
        assert_eq!(b["level"], json!("info"));
        assert_eq!(b["message"], json!("second"));
        assert_eq!(b["source"], json!("app"));
        assert!(b["timestamp"].as_str().is_some_and(|s| s.contains('T')));
    }

    /// trace → debug 的归并不受 `_id` 改动影响（渲染端 `LogLevel` 无 trace 档）。
    #[test]
    fn log_entry_level_still_folds_trace_into_debug() {
        let mut r = log_rec(1, "x");
        r.level = "trace";
        assert_eq!(log_record_to_entry(&r)["level"], json!("debug"));
    }

    /// 单批截断取**尾部**（丢最旧、保最新）。取头部 = UI 永远显示最老的那 500 条，
    /// 真机上与「日志流卡死」几乎不可分辨，故显式钉住方向。
    #[test]
    fn tail_capped_keeps_newest_and_drops_oldest() {
        let v: Vec<u32> = (0..10).collect();
        assert_eq!(tail_capped(&v, 3), &[7, 8, 9], "保最新三条");
        assert_eq!(tail_capped(&v, 10), &v[..], "不超容量 → 原样");
        assert_eq!(tail_capped(&v, 99), &v[..], "cap 超量 → 原样");
        assert!(tail_capped(&v, 0).is_empty(), "cap=0 → 空");
        let empty: [u32; 0] = [];
        assert!(tail_capped(&empty, 5).is_empty(), "空输入不 panic");
    }

    /// 截断上限与渲染端缓冲同量：补推多于渲染端能留的行数只是白费一次序列化 + 一次 webview 唤醒。
    #[test]
    fn pending_batch_cap_matches_renderer_buffer() {
        assert_eq!(
            MAX_PENDING_LOG_BATCH, 500,
            "与 LogsScreen MAX_BUFFER 同量（改一边须同步另一边）"
        );
    }
}

/// 🔴 出口 IP 自动重探的排程腿不得用 `tokio::spawn` —— 2026-07-21 真机 `SIGABRT` 的同款守卫。
///
/// # 为什么必须是源码扫描，而不是行为测试
///
/// [`schedule_ipinfo_refresh`] 的调用链可自**同步 command / 主线程**路径进入（`ProxyErrorEmitter` 的
/// 实现被同步 command 间接触达），而 `tokio::spawn` 要求调用处已在 Tokio runtime 上下文内，否则 panic
/// ⇒ Tauri IPC 回调里无处可 catch ⇒ `abort()` ⇒ 整个应用崩溃。
///
/// **单测结构性抓不到**：`#[tokio::test]` 自带 runtime 上下文，两种 spawn 在测试里行为完全一致、都能过
/// （`runtime::unlock` 那次 14/14 变异全杀 + 5 门全绿照样放进了生产）。唯一能在本层锁住的判据就是
/// 「源码里不许出现那个 API」。
///
/// # ⚠️ 本守卫的逃逸面（已知取舍，别高估它的射程 —— 也别低估）
///
/// 射程**只有 [`schedule_ipinfo_refresh`] 这一个函数体**（`top_level_fn_body` 按列 0 的右花括号封顶），
/// 本文件之外的任何 `tokio::spawn` 一概看不见。
///
/// 但**「整个 `spawn` 挪进 helper fn」并不是逃逸**：正向断言（1564 行的
/// `assert!(body.contains("tauri::async_runtime::spawn"))`）会因为函数体里再也找不到合规 spawn 而**转红**。
/// 2026-07-21 第三轮复审前，这里写的正是「挪进 helper 则守卫不转红」—— 那句话是从**负向**守卫的逃逸面
/// 抄过来的，与本守卫的实际行为相反。
///
/// **真正够得着的逃逸只有一种**：函数体内**保留**这句合规的 `tauri::async_runtime::spawn`（正向断言过），
/// 另外再调一个内部含裸 `tokio::spawn` 的 helper fn（负向断言扫不到 helper 的体）—— 崩溃条件一字未变。
///
/// 接受这个取舍是因为：真正的判据（「调用链有没有 runtime 上下文」）跨函数、跨文件、跨线程，静态扫描
/// 本就够不着；本守卫只承诺钉住**历史上真的出过事的那一处**。要扩射程得换成全仓 lint，不在本批范围。
#[cfg(test)]
mod ipinfo_spawn_guard {
    use crate::commands::guard_scan::top_level_fn_body;

    const SRC: &str = include_str!("misc.rs");

    /// 锚定函数体（签名之后起算 ⇒ 上方那段解释「为什么不能用 `tokio::spawn`」的文档注释天然不在射程内）。
    /// 体内注释仍剥一道，防将来有人在体内写下同名 API 的说明文字而把守卫顶成假红。
    fn scheduler_body() -> String {
        top_level_fn_body(
            SRC,
            "pub fn schedule_ipinfo_refresh(app: &AppHandle, delay_ms: u64) {",
        )
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("//") {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
    }

    #[test]
    fn schedule_ipinfo_refresh_uses_tauri_async_runtime_not_bare_tokio_spawn() {
        let body = scheduler_body();
        assert!(
            body.contains("tauri::async_runtime::spawn"),
            "必须用 tauri::async_runtime::spawn（持全局 runtime handle，任意线程可调）"
        );
        assert!(
            !body.contains("tokio::spawn"),
            "出现裸 tokio::spawn —— 同步 command 路径无 runtime 上下文，真机必 panic→abort"
        );
    }

    /// 守卫的守卫：证明扫到的是真函数体而非空串（空串会让上面的否定断言恒真 = 没门）。
    #[test]
    fn guard_scan_actually_captured_the_scheduler_body() {
        let body = scheduler_body();
        assert!(
            body.contains("next_ipinfo_epoch") && body.contains("probe_publish_ipinfo"),
            "扫到的片段缺少排程腿的标志性内容 ⇒ 锚点漂了，守卫失去判据：{body}"
        );
    }
}

/// 🟠 **两条探测腿必须共用同一条世代线**——手点「网络检测」（[`ipinfo_get`]）与事件驱动排程
/// （[`schedule_ipinfo_refresh`]）都必须经 [`next_ipinfo_epoch`] 领世代、并把它交给
/// [`probe_publish_ipinfo`]。
///
/// # 为什么是源码扫描，而不是行为测试
///
/// [`ipinfo_get`] 是 `#[tauri::command]`，要 `AppHandle` + `State<AppRuntime>` 才能调，单测里造不出来。
/// 而「它有没有领世代」是个纯结构事实：领了就参与「后来者胜」；没领（旧实现）则两条线互不作废——用户
/// 在起核 4s 收敛窗口内点一下检测，两条探测并行打网，谁先落地纯看网络抖动，**后落地的可能反而是先发起
/// 的那条**，状态栏于是显示已经切走的出口。
///
/// 落地顺序本身的行为验证在 `tests::stale_probe_leg_must_not_overwrite_newer_leg`（直接驱动世代闸）；
/// 本守卫只负责钉住「两条腿都接在那条线上」这个接线事实。
///
/// # ⚠️ 逃逸面
///
/// 射程限于被锚定的那几个函数体（`top_level_fn_body` 按列 0 的右花括号封顶）。
///
/// 本模块的断言全是**正向**的（`assert!(body.contains(…))` / `find().expect()`）⇒ **fail-closed**：
/// 把领世代的动作挪进 helper fn，函数体里就找不到 `next_ipinfo_epoch()` 了，守卫**转红**。
/// 2026-07-21 第三轮复审前这里写的是「挪进 helper 则守卫不转红」—— 那是从负向守卫抄来的措辞，
/// 与本模块的实际行为相反；**在这个仓里逃逸面自述是复审者据以判断覆盖的依据，写反会让后人误判射程**。
///
/// **真正够得着的逃逸**：在函数体内用**等价写法**冒充，让正向 `contains` 落空而语义不变 —— 例如把
/// `next_ipinfo_epoch()` 内联成 `IPINFO_REFRESH_EPOCH.fetch_add(1, Ordering::SeqCst) + 1`，或把
/// `probe_publish_ipinfo(` 换成别名调用。这类逃逸静态扫描本就够不着，只能靠落地语义的行为测试
/// （`tests::stale_probe_leg_must_not_overwrite_newer_leg`）兜底。
#[cfg(test)]
mod ipinfo_epoch_guard {
    use crate::commands::guard_scan::top_level_fn_body;

    const SRC: &str = include_str!("misc.rs");

    /// 锚定函数体并剥掉整行注释（防文档/说明文字把守卫顶成假绿）。
    fn fn_body(signature: &str) -> String {
        top_level_fn_body(SRC, signature)
            .lines()
            .map(|l| {
                if l.trim_start().starts_with("//") {
                    ""
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 两条会落地的探测腿（手点 / 排程）。
    const PROBE_LEGS: [&str; 2] = [
        "pub async fn ipinfo_get(",
        "pub fn schedule_ipinfo_refresh(app: &AppHandle, delay_ms: u64) {",
    ];

    /// 排程腿签名（下面两条守卫共用）。
    const SCHEDULER: &str = "pub fn schedule_ipinfo_refresh(app: &AppHandle, delay_ms: u64) {";

    /// 🟡 **手点腿的三行顺序**：`宣告 → 领世代 → 快照排程线`，且宣告**恰一次**。
    ///
    /// # 这半边原先零覆盖（本守卫补的正是这个洞）
    ///
    /// 排程腿有 4 条位置断言钉住它的时点（见 [`scheduler_takes_its_epoch_after_the_settle_sleep`]），
    /// 手点腿却只有 [`both_probe_legs_take_an_epoch_and_pass_it_down`] 那 3 条 `contains` —— **只证
    /// 「三个动作都在」，不证「按什么顺序」**。实测逃逸：把三行改成
    /// `let seq = current(); let epoch = next_ipinfo_epoch(); next_ipinfo_schedule_seq();`
    /// ⇒ `cargo test` 全绿存活。
    ///
    /// # 顺序错了会怎样
    ///
    /// 快照跑到宣告**之前** ⇒ 本腿快照到的 `seq` 比自己随后宣告的值小 ⇒ 落地时
    /// [`commit_ipinfo_snapshot`] 的 `SEQ == seq` 恒假 ⇒ **手点腿永远过不了闸**：不写缓存、不广播、
    /// 不 fire 伴测。而 `HomeScreen.tsx` 明确丢弃 `ipinfo_get` 的返回值（靠广播回写）
    /// ⇒ **「网络检测」按钮完全无反应**，且本模块无轮询、无人纠正。
    ///
    /// 牙：① 三行任意换序 ② 把开探时的 `current_ipinfo_schedule_seq()` 写成再自增一次 —— 均转红。
    #[test]
    fn manual_leg_declares_then_takes_epoch_then_snapshots_the_schedule_line() {
        let body = fn_body(PROBE_LEGS[0]);
        let sched_at = body
            .find("next_ipinfo_schedule_seq()")
            .expect("手点腿必须宣告排程线，否则它作废不了在飞的排程腿（收敛窗口内两条腿互不作废）");
        let epoch_at = body
            .find("next_ipinfo_epoch()")
            .expect("手点腿必须领世代（上一条守卫同判，此处重复取下标）");
        let snap_at = body
            .find("current_ipinfo_schedule_seq()")
            .expect("手点腿必须快照排程线，否则落地时没有比对基准");
        assert!(
            sched_at < epoch_at,
            "宣告晚于领世代 ⇒ 手点腿领号那一刻还没宣告「我最新」，在飞的排程腿不被作废"
        );
        assert!(
            epoch_at < snap_at,
            "快照早于宣告/领号 ⇒ 快照到的是自己宣告**前**的值，落地闸恒假：按钮永不写缓存/不广播/\
             不 fire 伴测，而前端丢弃返回值 ⇒ 「网络检测」完全无反应"
        );
        assert_eq!(
            body.matches("next_ipinfo_schedule_seq()").count(),
            1,
            "排程线：手点腿的排程与开探是同一刻，宣告**一次**、快照**一次**。\
             多自增一次 = 本腿把自己判成更新事件（落地时恒假，同上）"
        );
    }

    /// 🔴 **直判终态腿必须宣告排程线，且宣告在写缓存之前**。
    ///
    /// `mark_ipinfo_proxy_blocked` 不是探测（没有开探时刻、不领世代），但它**是一次「出口世界变了」的
    /// 事件** —— 按本模块的「排程即宣告」契约，事件那一刻必须自增 [`super::IPINFO_SCHEDULE_SEQ`]，
    /// 否则已开探、在飞（预算最长 20s）的探测腿落地时两个计数器都没动过 ⇒ 恒过闸 ⇒ 用 `proxy:null`
    /// 覆盖 `proxyBlocked` 终态，而终态由边沿触发、同态帧早退 ⇒ **不会重落**。
    ///
    /// 位置必须在写缓存**之前**：宣告晚于写缓存，就留出一段「终态已进缓存、旧腿仍算当前」的窗口。
    ///
    /// 牙：① 删掉宣告 ② 把它挪到写缓存之后 ③ 顺手加一次领世代（那是探测腿的语义，直判终态没有开探
    /// 时刻，领了只会让这条线的口径出现第二个真相源）—— 三条均转红。落地语义由
    /// [`super::tests::stale_probe_leg_must_not_overwrite_newer_leg`] 的段 (g) 行为兜底。
    #[test]
    fn mark_blocked_declares_the_schedule_line_before_writing_the_cache() {
        let body = fn_body("fn commit_proxy_blocked_snapshot(reason: &str) -> Value {");
        let declare = body
            .find("next_ipinfo_schedule_seq()")
            .expect("直判终态必须宣告排程线，否则在飞探测腿落地即把 proxyBlocked 盖回 null/error");
        let write = body
            .find("ipinfo_cache()")
            .expect("直判终态必须写权威缓存（peek 型消费方不订阅广播，只读缓存）");
        assert!(
            declare < write,
            "宣告必须在写缓存之前：反过来会留出「终态已进缓存、旧腿仍算当前」的窗口"
        );
        assert_eq!(
            body.matches("next_ipinfo_schedule_seq()").count(),
            1,
            "宣告一次即可（本函数是单点直判终态，不是排程 + 开探两段）"
        );
        assert!(
            !body.contains("next_ipinfo_epoch()"),
            "直判终态不得领世代：世代线的口径是「开探那一刻」，而本函数压根不开探 —— \
             在这里领号会让世代线出现第二个真相源"
        );
    }

    #[test]
    fn both_probe_legs_take_an_epoch_and_pass_it_down() {
        for sig in PROBE_LEGS {
            let body = fn_body(sig);
            assert!(
                body.contains("next_ipinfo_epoch()"),
                "`{sig}` 没领世代 ⇒ 它既不作废在飞的另一条腿、也不被对方作废，两条探测并行乱序落地"
            );
            assert!(
                body.contains("next_ipinfo_schedule_seq()")
                    && body.contains("current_ipinfo_schedule_seq()"),
                "`{sig}` 没接排程线：既不宣告（我最新）也不快照（我开探时的世界），\
                 收敛窗口内「已排程、尚未开探」那 4s 就又回到没人作废旧腿的状态"
            );
            assert!(
                body.contains("probe_publish_ipinfo(")
                    && body.contains("epoch")
                    && body.contains("seq"),
                "`{sig}` 领了判据却没把**两条**都交给 probe_publish_ipinfo ⇒ 探测后那道闸只剩一半"
            );
        }
    }

    /// 🟠 **按开探顺序发号**：排程腿必须先 `sleep` 满收敛延迟、**之后**才 [`next_ipinfo_epoch`]。
    ///
    /// 判据是**文本位置序**（`sleep(` 的下标 < `next_ipinfo_epoch()` 的下标），与 `speedtest.rs` 的
    /// `fallback_leg_captures_generation_before_awaiting_measurement` 同一范式 —— 那条守的也是
    /// 「基准在 await 的哪一侧捕获」这类**接线时点**问题。
    ///
    /// # 为什么行为测试够不着这一条
    ///
    /// [`schedule_ipinfo_refresh`] 要 `AppHandle`（本仓未引 `tauri::test`），单测造不出来；而世代闸本身
    /// 的落地语义已由 `tests::stale_probe_leg_must_not_overwrite_newer_leg` 段 (a)–(d) 直驱验证。两者
    /// 分工：那边证「号大的赢」，这边证「号是在开探那一刻发的」。**缺任一条，缺陷都能整条溜过去**——
    /// 把领号挪回 `sleep` 之前，那边四段照样全绿（它们自己按开探顺序领号），只有这条转红。
    ///
    /// 牙：把 `let epoch = next_ipinfo_epoch();` 挪回 `if delay_ms > 0 {` 之前 → 转红。
    ///
    /// # 本函数还守着另外两组「接线时点」（第三轮复审补）
    ///
    /// - **排程线在 `sleep` 之前宣告、开探时只快照**：世代号既然改到开探时领，「谁最新」这一维就必须
    ///   由 [`IPINFO_SCHEDULE_SEQ`] 在排程时刻接住，否则收敛窗口内无人记录（见该 static 的 t=4.1 序列）。
    /// - **在飞计数的排位/归还成对、且无路径绕过归还**：漏还则 `peek` 永久回置空帧，而本模块无轮询。
    ///   历史上那条绕过路径正是 `try_state` 早退分支里的单独清位 —— 现已收敛到体尾唯一一次归还。
    ///   早退面禁 `return` **与 `?`**（第四轮复审：`spawn` 不要求 `Output = ()`，`?` 可编译且能绕过归还）。
    #[test]
    fn scheduler_takes_its_epoch_after_the_settle_sleep() {
        let body = fn_body(SCHEDULER);
        let sleep_at = body.find("sleep(").expect(
            "排程腿必须真的睡满选路收敛延迟（删掉 sleep = 起核瞬间就探，必打到旧出口/失败）",
        );
        let epoch_at = body
            .find("next_ipinfo_epoch()")
            .expect("排程腿必须领世代（上一条守卫同判，此处重复取下标）");

        // 睡之前必须先「置在飞 + 广播置空帧」：少任一半，收敛窗口内就有消费方仍显示**上一个出口**
        // ——订阅方（状态栏）靠广播帧置空，peek 方（托盘浮层 / 窗口重建水合）靠在飞标记。
        let inflight_at = body.find("IPINFO_INFLIGHT.fetch_add(1").expect(
            "睡前必须排一格在飞，否则 peek 型消费方（托盘/水合腿）在收敛窗口里照吐上一个出口 IP",
        );
        let pending_at = body.find("pending_ipinfo_snapshot()").expect(
            "睡前必须广播置空帧，否则订阅方（状态栏）在收敛窗口里继续显示上一个出口的 IP 与旗面",
        );
        assert!(
            inflight_at < pending_at && pending_at < sleep_at,
            "顺序须是「置在飞 → 广播置空 → 睡」：广播早于置位会留一个「订阅方已置空、peek 仍吐旧值」\
             的窗口；两者晚于 sleep 则整个收敛窗口都在显示旧出口"
        );

        assert!(
            sleep_at < epoch_at,
            "世代号在 sleep **之前**领 ⇒ 号的顺序是「谁先被排上」而非「谁先开探」：4s 收敛窗口内用户\
             手点一次「网络检测」就会领走更大的号，收敛后那条重探腿一醒来即判过期、永不开探，\
             而赢的正是设计自己判定为不可信（proxy 极可能为 null）的那一次"
        );
        assert!(
            !body.contains("IPINFO_REFRESH_EPOCH.load("),
            "醒后闸已随「开探时领号」删除（领号紧跟 sleep 之后，比对恒真 = 死代码）；\
             它若复活，说明有人把领号又挪回了排程时刻"
        );

        // 🔴 **排程线必须在 sleep 之前宣告**（第三轮复审）：世代号在 sleep 之后领 ⇒「已排程、尚未
        // 开探」的整个 4s 窗口里没有任何东西记录「有更新的腿排上了」，在飞的旧腿落地时一比对世代
        // 仍是自己的、过闸 —— 广播已切走节点的出口，并把新隧道量到的 RTT 持久写进旧节点的延迟徽标。
        // 牙：删掉这次自增、或把它挪到 `sleep` / `next_ipinfo_epoch()` 之后 → 转红。
        let sched_at = body.find("next_ipinfo_schedule_seq()").expect(
            "排程腿必须在**排程那一刻**宣告排程线，否则收敛窗口内「谁最新」无人记录（见 IPINFO_SCHEDULE_SEQ）",
        );
        assert!(
            sched_at < sleep_at,
            "排程线宣告晚于 sleep ⇒ 它退化成第二个「开探时刻」计数器，与世代号同维、白加一个 static"
        );
        // 开探那一刻取的必须是**快照**（load），不是再自增一次：自增会让本腿把自己也判成「更新的事件」，
        // 落地时恒真 = 没闸。
        let snap_at = body.find("current_ipinfo_schedule_seq()").expect(
            "开探时必须快照排程线（与领世代、读 status/config 同刻），否则落地时没有比对基准",
        );
        assert!(
            epoch_at < snap_at && body.matches("next_ipinfo_schedule_seq()").count() == 1,
            "排程线：排程时自增**一次**、开探时快照**一次**。多自增一次 = 本腿把自己判成更新事件（恒真）"
        );

        // 🔵 在飞计数：**谁排的位谁归还**，且没有任何路径能绕过归还。
        // 牙：① 删掉 `fetch_sub` ② 把它挪到 `probe_publish_ipinfo` 之前 ③ 在体内加一条 `return`
        // 跳过它 ④ 让两半挂在不同条件下 ⑤ 用 `?` 早退（见下方 `?` 一节）—— 五种逃逸均转红。
        let sub_at = body.find("IPINFO_INFLIGHT.fetch_sub(1").expect(
            "排位了却不归还 ⇒ peek 永久回置空帧、托盘/水合腿从此再也读不到缓存，而本模块无轮询、无人纠正",
        );
        let probe_at = body
            .find("probe_publish_ipinfo(")
            .expect("排程腿必须真的去探（上一条守卫同判，此处重复取下标）");
        assert!(
            probe_at < sub_at,
            "归还早于探测 ⇒ 收敛窗口在探测期间就关了，peek 型消费方立刻吐上一个出口"
        );
        assert_eq!(
            (
                body.matches("IPINFO_INFLIGHT.fetch_add(1").count(),
                body.matches("IPINFO_INFLIGHT.fetch_sub(1").count(),
                body.matches("if delay_ms > 0 {").count(),
            ),
            (1, 1, 2),
            "排位与归还必须各一次、且挂在同一个 `delay_ms > 0` 条件下 —— 两半条件不同即计数会漂"
        );
        // ⚠️ **`return` 不是唯一的早退**（第四轮复审）：`?` 也是，而且它才是真正够得着的那个。
        // [`tauri::async_runtime::spawn`]（tauri-2.11.5 `src/async_runtime.rs:279-284`）的约束只有
        // `F: Future + Send + 'static` + `F::Output: Send + 'static` —— **不要求 `Output = ()`**。
        // 故把 `async move {…}` 改成末尾 `Ok::<(), E>(())` 后就能在体内用 `?`：可编译、可 spawn、
        // 绕过体尾唯一那次 `fetch_sub`，而旧断言只查 `return`、不转红（沙箱实测存活）。
        //
        // 选「加断言」而非「把注释改成『?需人工确认』」的依据：
        // - 前提已实证（上面那份签名 + 最小可编译复现），不是推测；
        // - 本批反复栽在「自述比实际强」上，把守卫降级成一句待办 = 再造一条同型缺陷；
        // - 成本近零：当前函数体内 `?` 出现 **0** 次，且这类早退在本函数里本就不该有
        //   （三条路径必须收敛到同一个归还点）。
        //
        // 禁的是整个 `?` 而非 `"?;"`：`foo()?.bar()` 同样早退，只查 `"?;"` 会漏。
        // 与 `return` 同为**文本**扫描 ⇒ 闭包内的 `return`、字符串字面量里的 `?` 会误伤（假红）——
        // 安全侧，且改法是把那段挪出函数体，不是把守卫改宽。
        assert!(
            !body.contains("return") && !body.contains('?'),
            "函数体内出现 `return` 或 `?` ⇒ 存在绕过归还的路径（`try_state` 早退正是历史上那一条：\
             本腿再也走不到归还点，peek 从此永久置空，而本模块无轮询、无人纠正）。\
             所有分支必须收敛到体尾那一次 fetch_sub"
        );
    }

    /// 🟡 **广播与伴测必须收在世代闸之内**：[`probe_publish_ipinfo`] 里那道
    /// `if !commit_ipinfo_snapshot(…) { return … }` 早退，必须位于 `broadcast(` 与
    /// `spawn_warm_rtt_probe(` **之前**。
    ///
    /// # 这半边原先零覆盖（本守卫补的正是这个洞）
    ///
    /// `tests::stale_probe_leg_must_not_overwrite_newer_leg` 直接驱动 [`commit_ipinfo_snapshot`]，
    /// **不经** [`probe_publish_ipinfo`] ⇒ 只证了「旧腿写不进缓存」，没证「旧腿也不广播、不 fire 伴测」。
    /// 实测逃逸：把那道早退改成 `let _ = commit_ipinfo_snapshot(epoch, &snap);`（保留缓存闸、去掉早退）
    /// ⇒ `cargo test` 全绿，而「旧腿照样广播 + 照样 fire 伴测」原样复活 —— 状态栏会被一条已被判定过期的
    /// 探测结果盖掉，伴测还会把旧出口的 RTT 记进延迟徽标。
    ///
    /// 牙：① 去掉早退（改 `let _ = …`）② 把 `broadcast(` 挪到早退之前 ③ 把 `spawn_warm_rtt_probe(`
    /// 挪到早退之前 —— 三种逃逸均转红。
    #[test]
    fn publish_leg_gates_broadcast_and_warm_probe_behind_the_epoch_check() {
        let body = fn_body("async fn probe_publish_ipinfo(");
        let gate_at = body.find("if !commit_ipinfo_snapshot(").expect(
            "落地前必须**早退式**查闸：写成 `let _ = commit_ipinfo_snapshot(…)` 只挡住缓存，\
             广播与伴测照跑 —— 旧腿仍会盖掉状态栏、仍会把旧出口 RTT 记进延迟徽标",
        );
        let broadcast_at = body
            .find("crate::events::broadcast(")
            .expect("成功腿必须广播 ipInfoUpdated，否则订阅方（状态栏）永远收不到新出口");
        let warm_at = body
            .find("spawn_warm_rtt_probe(")
            .expect("成功腿必须 fire 出口伴测，否则延迟格恒 `—`（它是本腿的下游）");
        assert!(
            gate_at < broadcast_at,
            "广播在世代闸之外 ⇒ 已过期的旧腿照样把自己的快照推给全体消费方"
        );
        assert!(
            gate_at < warm_at,
            "伴测在世代闸之外 ⇒ 已过期的旧腿照样 fire 一次 RTT 测量，把旧出口的延迟记进徽标"
        );

        // 🔵 **判据必须是入参，不得现场取**（第三、四轮复审各逮到一条存活变异）：把传给
        // `spawn_warm_rtt_probe` 的判据换成现场取的值 ⇒ 下游那道复查拿现场值跟现场值比、恒真 = 没闸，
        // 而 `both_probe_legs_take_an_epoch_and_pass_it_down` 与本函数上面几条断言**均不转红**。
        //
        // 「现场取」有**两种**写法，缺一即漏（第四轮复审：断言原先只禁「领 / 宣告」，不禁「现场 load」）：
        // - **领 / 宣告**（自增）：`next_ipinfo_epoch` / `next_ipinfo_schedule_seq` / 裸 `fetch_add`；
        // - **现场 load**（不自增，但同样绕开入参）：`current_ipinfo_schedule_seq()` 遮蔽入参 `seq`，
        //   或直接 `IPINFO_REFRESH_EPOCH.load(…)`。实测逃逸：在 `spawn_warm_rtt_probe` 调用前插一行
        //   `let seq = current_ipinfo_schedule_seq();` ⇒ 全绿存活，而伴测复查的 seq 半边就此退化成恒真
        //   —— 正是「新腿已排程、还在睡」那 4s 窗口里**把新出口 RTT 持久写进旧节点徽标**的那个洞。
        //
        // 牙：体内出现上述任一写法 → 转红。
        assert!(
            !body.contains("next_ipinfo_epoch")
                && !body.contains("next_ipinfo_schedule_seq")
                && !body.contains("fetch_add")
                && !body.contains("current_ipinfo_schedule_seq")
                && !body.contains("IPINFO_REFRESH_EPOCH"),
            "probe_publish_ipinfo 不得自己领世代 / 宣告排程线，**也不得现场 load 任一计数器**：\
             判据必须原样取自开探那一刻的入参，现场取的值让本层的闸与下游伴测的复查双双恒真"
        );
        let warm_call = &body[warm_at..];
        assert!(
            body.contains("commit_ipinfo_snapshot(epoch, seq,")
                && warm_call.contains("epoch,")
                && warm_call.contains("seq,"),
            "两条判据都必须原样传给闸与伴测：只传世代时，「更新的腿已排程但还在睡」那 4s 窗口里复查恒真"
        );
    }

    /// 🔵 **调用点守卫**：出口无效直判终态必须**同时**写权威缓存与广播（本轮修的正是「只广播了一半」）。
    ///
    /// # 为什么必须是源码扫描
    ///
    /// [`super::mark_ipinfo_proxy_blocked`] 要 `AppHandle` 才能调（本仓未引 `tauri::test`）；而载荷折叠
    /// 那一半已由 `fold_proxy_blocked` 的纯逻辑测覆盖。剩下的「折叠结果到底有没有落进 `IPINFO_CACHE`」
    /// 是纯结构事实：把 `*g = Some(snap.clone())` 那两行删掉，**折叠测试一条都不会红** —— 而
    /// `ipinfo:get(peek)` 型消费方（托盘浮层 / 窗口重建水合）**不订阅**事件、只读缓存，于是继续吐
    /// 上一次探到的、此刻已知无效的代理出口 IP。这正是本仓「逻辑在、接线不在」的经典形态。
    ///
    /// 牙：① 删掉缓存写回 ② 删掉 broadcast ③ 把折叠换成就地 `json!` 重建（绕开 `fold_proxy_blocked`）
    /// —— 三条任一均转红。
    #[test]
    fn proxy_blocked_terminal_state_writes_cache_and_broadcasts() {
        // 落地那一半（折叠 + 写缓存）住在 `commit_proxy_blocked_snapshot`（拆出来是为了让「宣告排程线」
        // 那一维能被行为测试直调，见该函数文档）；广播那一半留在需要 `AppHandle` 的 `mark_…` 里。
        let commit = fn_body("fn commit_proxy_blocked_snapshot(reason: &str) -> Value {");
        assert!(
            commit.contains("fold_proxy_blocked(cached, reason)"),
            "载荷必须经 fold_proxy_blocked 折叠（就地重建 json 会绕开 direct 保留 / error 删键两条语义）"
        );
        assert!(
            commit.contains("*g = Some(snap.clone())"),
            "终态必须写进权威缓存：只广播不写缓存 ⇒ peek 型消费方继续吐已知无效的旧代理出口"
        );
        let body = fn_body("pub(crate) fn mark_ipinfo_proxy_blocked(");
        let write = body
            .find("commit_proxy_blocked_snapshot(reason)")
            .expect("终态必须经落地腿写缓存（只广播不写缓存 ⇒ peek 型消费方读陈旧出口）");
        let cast = body
            .find("crate::events::broadcast(")
            .expect("终态必须广播，否则订阅方（状态栏）不会更新");
        assert!(
            write < cast,
            "先写缓存再广播：反过来则广播到达渲染端时缓存仍是旧值，同一时刻两条读路径互相矛盾"
        );
    }

    /// 守卫的守卫：证明三个锚点扫到的是真函数体（空串会让 `contains` 断言恒假、表现为恒红，
    /// 但仍显式钉住正向内容，避免将来有人把断言「修」宽而让守卫静默失牙）。
    #[test]
    fn guard_scan_actually_captured_both_leg_bodies() {
        assert!(
            fn_body(PROBE_LEGS[0]).contains("peek"),
            "ipinfo_get 锚点漂了：扫到的片段没有它标志性的 peek 短路腿"
        );
        assert!(
            fn_body(PROBE_LEGS[1]).contains("async_runtime::spawn"),
            "schedule_ipinfo_refresh 锚点漂了：扫到的片段没有它标志性的 spawn"
        );
        assert!(
            fn_body("async fn probe_publish_ipinfo(").contains("build_ipinfo_snapshot("),
            "probe_publish_ipinfo 锚点漂了：扫到的片段没有它标志性的探测调用"
        );
    }
}
