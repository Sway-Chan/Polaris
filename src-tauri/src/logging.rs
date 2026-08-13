//! 最小日志 sink（stderr + 文件）。
//!
//! ## 为什么需要这个文件（发现的缺口，非本批原定范围）
//!
//! `Cargo.toml:17` 只声明了 `log = "0.4"` —— 那是**门面（facade）**，不是实现。全仓 `log::set_logger` /
//! `env_logger::init` / `tauri-plugin-log` **零命中**，即当前 30 处 `log::info!/warn!/error!` **全部是静默
//! no-op，输出去向为空**。这正是审计结论「白屏时无一行日志可排查」比预想更深的一层根因：不只是缺
//! renderer→主进程的转发，而是**主进程侧的日志本身就没有落点**。
//!
//! 故若不补 sink，「console 错误转发」这条不变式即便接线完成也仍然产出零日志 —— 等于宣称了一个 no-op
//! 能力。本模块用来把它变成真的。
//!
//! ## 为什么不用现成的
//!
//! 本批纪律：禁止引入任何新依赖。`env_logger` / `tauri-plugin-log` 都是新依赖，故走简约阶梯的
//! 「stdlib + 已装依赖」档：`log` crate 自带 `set_logger`，配 `std::fs` / `std::io` 手写 ~40 行
//! `log::Log` 实现即可。
//!
//! 注：刻意用 `set_logger(&'static dyn Log)` + `OnceLock` 而非更顺手的 `set_boxed_logger` —— 后者
//! 被 `log` 的 `std` feature 门控，而本仓 `Cargo.toml:17` 是裸 `log = "0.4"`（default-features 为空，
//! 不含 `std`）。走 `set_logger` 就无需改 Cargo.toml 的 feature 面，改动面更小、也不与并发批次抢文件。
//!
//! ## 边界（如实登记，勿当完整 LogManager）
//!
//! - 时间戳是 **Unix epoch 毫秒**（可读日期需 `chrono`/`time` = 新依赖）。排障够用（可排序、可换算），
//!   但不如 上游 app.log 的人类可读格式。
//! - `logs:get` / `logs:clear` / `diagnostic:export` 三个 command **仍是 stub**（`misc.rs:28/35/111`），
//!   本模块只保证日志**落盘**，没接前端日志页与诊断导出。完整 LogManager 属另一批。
//! - 单文件 + 满则轮转一次（`.1`），无按日切分、无压缩。

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Metadata, Record};

/// sink 单例：`log::set_logger` 要求 `&'static dyn Log`，故存 static。
static LOGGER: OnceLock<PolarisLogger> = OnceLock::new();

/// 日志文件大小上限：超过即轮转一次到 `.1`（防无界增长撑爆用户磁盘）。
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

// ── 内存环形缓冲（logs:get 水合 + EVENT_LOG_RECEIVED_BATCH 流式推送用）─────────────
//
// 上游 `LogManager` 的最小面：主进程侧每条 log 落盘之余，也进一个有界环形缓冲，供
// 日志页 `logs:get` 一次性水合 + 批量事件流增量推送（`misc.rs` 侧 ~150ms coalesce）。
// 与落盘解耦（独立 static，不进 `PolarisLogger`）：`snapshot` / `records_from` / `clear`
// 是自由函数，command 层无需持有 logger 引用。

/// 环形缓冲容量上限（条）。超出即从头丢弃最旧条目（有界，防无限增长）。
const LOG_RING_CAP: usize = 2000;

/// 结构化日志条目（渲染端 `LogEntry` 的后端镜像；`timestamp`/`level` 的最终成形在 `misc.rs`）。
#[derive(Clone, Debug)]
pub struct LogRecord {
    /// 单调递增序号（批量流用游标：只推 `seq >= cursor` 的新条目，不重放整环）。
    pub seq: u64,
    /// Unix epoch 毫秒（与落盘行同源）。
    pub ts_ms: u128,
    /// 级别标签（`error`/`warn`/`info`/`debug`/`trace`）。
    pub level: &'static str,
    /// 来源 target（渲染端 `LogEntry.source`）。
    pub target: String,
    /// 正文（渲染端 `LogEntry.message`）。
    pub message: String,
}

/// 环形缓冲单例（首用即建）。
static LOG_RING: OnceLock<Mutex<VecDeque<LogRecord>>> = OnceLock::new();
/// 全局单调序号发号器（下一个待分配 seq）。
static LOG_SEQ: AtomicU64 = AtomicU64::new(0);

fn ring() -> &'static Mutex<VecDeque<LogRecord>> {
    LOG_RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(LOG_RING_CAP)))
}

/// `record.target()` → 渲染端 `LogEntry.source` 词汇（只有 `sing-box` | `app` 两个值）。
///
/// 日志页的来源过滤器按字面量 `'sing-box'` / `'app'` 判等。但 `log::info!` 不显式指定 `target:` 时，
/// `log` 默认取**调用处的 Rust 模块路径**（`polaris::runtime::proxy` 之类）——全仓 126 处裸调用无一
/// 等于 `"app"`，故「应用」筛选恒空。唯一显式打 target 的是 `proxy.rs::pipe_to_log`（sing-box 子进程行）。
///
/// 故在此按「不是 sing-box 就是应用自身」归一，而不是去改 126 处调用点加 `target: "app"`——后者要靠每个
/// 新调用点自觉，漏一处又是一条查不到的日志；此处归一是**闭合**的（无论谁怎么写都落进两态之一）。
///
/// 只影响环形缓冲（UI 面）：落盘行仍写完整模块路径，那是排障时定位代码位置的唯一线索，不能抹掉。
fn ui_source(target: &str) -> &str {
    if target == SING_BOX_TARGET {
        SING_BOX_TARGET
    } else {
        "app"
    }
}

/// sing-box 子进程日志行的 target（`proxy.rs::pipe_to_log` 显式打，[`ui_source`] 据此分流）。
pub const SING_BOX_TARGET: &str = "sing-box";

/// `log::Level` → 稳定小写标签（渲染端 `LogLevel` 词汇；`trace` 的 `debug` 归并由 `misc.rs` 做）。
fn level_tag(l: Level) -> &'static str {
    match l {
        Level::Error => "error",
        Level::Warn => "warn",
        Level::Info => "info",
        Level::Debug => "debug",
        Level::Trace => "trace",
    }
}

// 纯逻辑（不碰全局；单测直接喂本地 VecDeque，规避共享 static 的测试互扰）。

/// 有界追加：满则先丢最旧，再入队（环形语义核心）。
fn bounded_push(r: &mut VecDeque<LogRecord>, cap: usize, rec: LogRecord) {
    if r.len() >= cap {
        r.pop_front();
    }
    r.push_back(rec);
}

/// 取最新 N 条快照（`limit=None` → 全部）。
fn collect_snapshot(r: &VecDeque<LogRecord>, limit: Option<usize>) -> Vec<LogRecord> {
    match limit {
        Some(n) if n < r.len() => r.iter().skip(r.len() - n).cloned().collect(),
        _ => r.iter().cloned().collect(),
    }
}

/// 取 `seq >= from_seq` 的增量 + 下一游标（最后一条 seq+1；无则原样返回 `from_seq`）。
fn collect_from(r: &VecDeque<LogRecord>, from_seq: u64) -> (Vec<LogRecord>, u64) {
    let recs: Vec<LogRecord> = r.iter().filter(|e| e.seq >= from_seq).cloned().collect();
    let next = recs.last().map_or(from_seq, |e| e.seq + 1);
    (recs, next)
}

/// 追加一条到环形缓冲（满则丢最旧）。锁毒化 → 静默跳过（日志缓冲绝不反噬主流程）。
///
/// **seq 必须在 ring 锁内分配**：发号与入环若非原子，两个并发 logger 线程可以先各自取到 seq=N/N+1，
/// 再以相反顺序入环 → 环内 seq 乱序。而 [`collect_from`] 的游标 = **末条** seq+1，一旦乱序，那条
/// seq 较小的插队条目会被下一批重新取到（重放），或被游标跨过（丢行）。锁内分配令「seq 递增」与
/// 「入环次序」同一把锁下成对发生 → 环内 seq 恒单调，游标语义才成立。
fn push_ring(ts_ms: u128, level: Level, target: &str, message: String) {
    if let Ok(mut r) = ring().lock() {
        let seq = LOG_SEQ.fetch_add(1, Ordering::Relaxed);
        bounded_push(
            &mut r,
            LOG_RING_CAP,
            LogRecord {
                seq,
                ts_ms,
                level: level_tag(level),
                target: ui_source(target).to_string(),
                message,
            },
        );
    }
}

/// 取环形缓冲快照 + 流式起始游标（`logs:get` 水合）。`limit` = 只取最新 N 条（None = 全部）。
///
/// 二者**必须在同一把 ring 锁下取**，否则水合与流式之间有缝：`logs:get` 先 snapshot、批量流稍后才
/// 取游标 → 期间新写入的条目 seq 低于游标即被跨过（**丢行**）；反之游标先取则被重复下发（**重放**）。
/// 锁内取：游标 = 此刻发号器位置（seq 已改为 ring 锁内分配，故锁内 load 恒等于「下一条待分配」，
/// 且环内不可能已有 seq ≥ 它的条目）→ 快照与增量流恰好首尾相接，不重不漏。
#[must_use]
pub fn snapshot_with_cursor(limit: Option<usize>) -> (Vec<LogRecord>, u64) {
    let Ok(r) = ring().lock() else {
        return (Vec::new(), LOG_SEQ.load(Ordering::Relaxed));
    };
    (collect_snapshot(&r, limit), LOG_SEQ.load(Ordering::Relaxed))
}

/// 取 `seq >= from_seq` 的新条目 + 下一游标（批量事件流增量拉取）。
///
/// 返回 `(recs, next_cursor)`；`next_cursor` = 最后一条 seq+1（无新条目则原样返回 `from_seq`）。
#[must_use]
pub fn records_from(from_seq: u64) -> (Vec<LogRecord>, u64) {
    let Ok(r) = ring().lock() else {
        return (Vec::new(), from_seq);
    };
    collect_from(&r, from_seq)
}

/// 清空环形缓冲（`logs:clear`）。发号器不复位（seq 全局单调，避免游标错位）。
pub fn clear() {
    if let Ok(mut r) = ring().lock() {
        r.clear();
    }
}

struct PolarisLogger {
    /// None = 文件不可用（只写 stderr）。日志绝不能因为落盘失败而影响主流程。
    file: Mutex<Option<File>>,
}

impl log::Log for PolarisLogger {
    /// 判级取**全局** [`log::max_level`] 而非自身快照字段：级别要能随 `config.logLevel` 在运行期改
    /// （[`set_level`]），持一份构造期的副本会让 `set_max_level` 只改宏侧的前置门、改不动这里，
    /// 出现「宏放行了但 sink 自己按旧级别丢弃」的半生效。单一真值源 = `log::max_level()`。
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let line = format!(
            "[{ts}] {:<5} {} — {}",
            record.level(),
            record.target(),
            record.args()
        );
        // stderr：开发态 / 终端启动时直接可见。
        eprintln!("{line}");
        // 文件：打包后用户唯一能捞到的痕迹。写失败一律吞（日志失败不得反噬主流程）。
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
            }
        }
        // 内存环形缓冲：供日志页 logs:get 水合 + EVENT_LOG_RECEIVED_BATCH 流式推送。
        push_ring(
            ts,
            record.level(),
            record.target(),
            record.args().to_string(),
        );
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.flush();
            }
        }
    }
}

/// 解析级别字面量。同时服务 `POLARIS_LOG` 与 `config.logLevel` 两个来源（词汇不同但取值重叠）。
///
/// `fatal` 是渲染端 `LogLevel` 才有的档（原型日志页 seg 的最高档），`log` crate 无对应 —— 映到
/// `Error`（最接近的「只留最严重」语义）。无法识别 → `None`，由调用方决定回退，不静默当 Info。
fn parse_level(s: &str) -> Option<LevelFilter> {
    match s {
        "trace" => Some(LevelFilter::Trace),
        "debug" => Some(LevelFilter::Debug),
        "info" => Some(LevelFilter::Info),
        "warn" => Some(LevelFilter::Warn),
        // fatal 无 log crate 对应档，归并到 Error（渲染端仍按自己的 fatal 标签过滤显示）。
        "error" | "fatal" => Some(LevelFilter::Error),
        "off" => Some(LevelFilter::Off),
        _ => None,
    }
}

/// `POLARIS_LOG` 是否在启动时被采纳为级别（由 [`init`] 经 [`startup_level`] 置一次）。
///
/// [`set_level`] 据此让位：见该函数文档。
static ENV_LEVEL_OVERRIDE: AtomicBool = AtomicBool::new(false);

/// 启动级别：`POLARIS_LOG` 环境变量 > `config.logLevel` > Info。
///
/// 环境变量优先是因为它是**排障者的临时超驰**——已经在用它抓日志时，不该被用户配置里存的级别顶掉。
/// 读 config 原文本而非走 `ConfigManager`：本函数在 setup 最早处跑，此刻 store 尚未装配。
///
/// 采纳 env 时置 [`ENV_LEVEL_OVERRIDE`]：该优先级此前只在**启动这一刻**成立，运行期 [`set_level`]
/// 会无条件把它顶掉（见该函数文档）。
fn startup_level(config_dir: &Path) -> LevelFilter {
    resolve_startup_level(std::env::var("POLARIS_LOG").ok().as_deref(), config_dir)
}

/// [`startup_level`] 的本体，env 取值由调用方传入（单测无需改进程环境即可覆盖 env 超驰这一路）。
///
/// 采纳 env 时**武装** [`ENV_LEVEL_OVERRIDE`]：那是 [`set_level`] 让位的唯一依据 —— 漏了这一步，
/// 声明的优先级就只在启动那一刻成立，第一次配置写就把级别顶回去了（P2-5 的本体）。
fn resolve_startup_level(env: Option<&str>, config_dir: &Path) -> LevelFilter {
    if let Some(l) = env_override_level(env) {
        ENV_LEVEL_OVERRIDE.store(true, Ordering::Relaxed);
        return l;
    }
    level_from_config_file(config_dir).unwrap_or(LevelFilter::Info)
}

/// env 超驰判定（纯逻辑，便于单测不碰进程环境）：`Some(l)` = `POLARIS_LOG` 给了可识别级别 → 超驰生效；
/// `None` = 未设或值无法识别 → 无超驰，走 config/默认（无法识别的 env 值不该冻住配置侧的级别控制）。
fn env_override_level(env: Option<&str>) -> Option<LevelFilter> {
    env.and_then(parse_level)
}

/// 从 `<config_dir>/config.json` 读 `logLevel`。任何 IO/解析失败 → None（回退默认，绝不 panic）。
fn level_from_config_file(config_dir: &Path) -> Option<LevelFilter> {
    let raw = std::fs::read_to_string(config_dir.join("config.json")).ok()?;
    let cfg: serde_json::Value = serde_json::from_str(&raw).ok()?;
    parse_level(cfg.get("logLevel")?.as_str()?)
}

/// 运行期改日志级别（`config.logLevel` 变更时调用）。无法识别的值 → no-op + warn，不悄悄降级。
///
/// # 它的射程比「应用侧」大（本批起）
///
/// 本 sink 立即生效自不必说；**核日志现在也归它管**：`runtime/proxy.rs` 的核日志 relay 订阅管理 API
/// 的 `SubscribeLog`，那条流恒是全级别，转发时走的就是 `log::log!` ⇒ 由这里设的 `max_level` 筛。
/// 于是「把级别拨到 debug 立刻看到核的 debug 行」不再需要改核配置、更不需要重启核。
///
/// **仍然管不到的那一格**：核写进它自己那份日志文件（`singbox.log`）时用的级别，是起核时注入进生成
/// 配置的，改配置不追溯已在跑的核 —— UI 就这一格如实标注「需重启内核生效」，本函数不假装管得到。
///
/// # 为什么 env 超驰在这里也要让位
///
/// [`startup_level`] 声明的优先级是 `POLARIS_LOG` > `config.logLevel`，但它此前只在启动那一刻成立：
/// 本函数挂在 `broadcast_config_changed`（配置写的唯一汇流点）上，而 config 恒带 `logLevel` 字段 →
/// `POLARIS_LOG=debug` 起 app 后，**任意**一次配置写都会把级别拉回 config 里存的值，抓 debug 的排障
/// 会话就此静默降级。故 env 一旦生效即在此让位——超驰的定义就是「压过配置」，只压启动那一次不叫超驰。
pub fn set_level(level: &str) {
    if ENV_LEVEL_OVERRIDE.load(Ordering::Relaxed) {
        log::debug!("POLARIS_LOG 环境超驰生效中，忽略 config.logLevel=`{level}` 的级别变更");
        return;
    }
    let Some(filter) = parse_level(level) else {
        log::warn!("未知日志级别 `{level}`，级别未变更");
        return;
    };
    if log::max_level() == filter {
        return; // 幂等：config 保存常带全量字段，级别没变时不刷屏。
    }
    log::set_max_level(filter);
    log::info!("应用日志级别已切到 {filter}（sing-box 侧需重启内核生效）");
}

/// 打开日志文件（append）；超限先轮转一次。任何 IO 失败 → None（退化成只写 stderr，绝不 panic）。
fn open_log_file(log_dir: &Path) -> Option<File> {
    std::fs::create_dir_all(log_dir).ok()?;
    let path = log_dir.join("polaris.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, log_dir.join("polaris.log.1"));
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

/// 装 sink。**须在 setup 最早处调用一次**；重复调用（`set_boxed_logger` 返回 Err）静默忽略。
///
/// 日志落 `<config_dir>/logs/polaris.log`。
pub fn init(config_dir: &Path) {
    let level = startup_level(config_dir);
    let logger = LOGGER.get_or_init(|| PolarisLogger {
        file: Mutex::new(open_log_file(&config_dir.join("logs"))),
    });
    let file_available = logger.file.lock().is_ok_and(|f| f.is_some());
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
        log::info!("日志 sink 已装：level={level}, file={file_available}");
        if !file_available {
            log::warn!("日志文件不可用（目录不可写？）→ 仅 stderr");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_log_file_creates_dir_and_file() {
        let dir = std::env::temp_dir().join(format!(
            "polaris-log-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        assert!(open_log_file(&dir).is_some());
        assert!(dir.join("polaris.log").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 落盘失败不得 panic —— 日志是排障工具，绝不能自己变成故障源。
    /// 夹具：父路径是**普通文件**而非目录 → `create_dir_all` 在三平台都必失败。
    /// panic-safety 是平台无关逻辑，故不 cfg 门任何平台。
    #[test]
    fn open_log_file_on_unwritable_path_is_none_not_panic() {
        let dir = std::env::temp_dir().join(format!(
            "polaris-log-unwritable-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let log_dir = blocker.join("logs"); // 父是文件 → create_dir_all 必失败
        assert!(open_log_file(&log_dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn level_defaults_to_info() {
        // 未设 POLARIS_LOG 且无 config.json（测试进程默认）时为 Info。
        if std::env::var("POLARIS_LOG").is_err() {
            assert_eq!(
                startup_level(Path::new("/proc/polaris-nope")),
                LevelFilter::Info
            );
        }
    }

    /// 级别字面量解析：覆盖两个来源的全部词汇；fatal 归 Error；未知 → None（不静默当 Info）。
    #[test]
    fn parse_level_maps_all_frontend_levels() {
        assert_eq!(parse_level("debug"), Some(LevelFilter::Debug));
        assert_eq!(parse_level("info"), Some(LevelFilter::Info));
        assert_eq!(parse_level("warn"), Some(LevelFilter::Warn));
        assert_eq!(parse_level("error"), Some(LevelFilter::Error));
        assert_eq!(
            parse_level("fatal"),
            Some(LevelFilter::Error),
            "fatal 无对应档 → 归 Error"
        );
        assert_eq!(parse_level("trace"), Some(LevelFilter::Trace));
        assert_eq!(parse_level("off"), Some(LevelFilter::Off));
        assert_eq!(parse_level("bogus"), None, "未知值必须 None，由调用方回退");
    }

    /// config.logLevel 在启动时被读到（否则用户存的 debug 要等到手动再点一次才生效）。
    #[test]
    fn startup_level_reads_config_log_level() {
        let dir = std::env::temp_dir().join(format!(
            "polaris-loglevel-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), r#"{"logLevel":"debug"}"#).unwrap();
        assert_eq!(level_from_config_file(&dir), Some(LevelFilter::Debug));
        // 损坏的 config 不得让日志系统炸/卡住 → None 回退。
        std::fs::write(dir.join("config.json"), "{not json").unwrap();
        assert_eq!(level_from_config_file(&dir), None);
        // 无 logLevel 字段 → None（回退默认，非报错）。
        std::fs::write(dir.join("config.json"), r#"{"other":1}"#).unwrap();
        assert_eq!(level_from_config_file(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// UI 来源归一：sing-box 保留，其余（含裸调用的 Rust 模块路径）一律 app。
    /// 打断归一（直接返回 target）→ 本测转红，即「应用」筛选恒空的那个 bug。
    #[test]
    fn ui_source_normalizes_module_paths_to_app() {
        assert_eq!(ui_source("sing-box"), "sing-box");
        assert_eq!(ui_source("polaris::runtime::proxy"), "app");
        assert_eq!(ui_source("polaris"), "app");
        assert_eq!(ui_source("app"), "app");
        assert_eq!(ui_source(""), "app");
    }

    fn rec(seq: u64) -> LogRecord {
        LogRecord {
            seq,
            ts_ms: u128::from(seq),
            level: "info",
            target: "t".into(),
            message: format!("m{seq}"),
        }
    }

    /// 有界追加：超容量丢最旧、保最新（环形不变式）。打断 pop_front → 本测转红。
    #[test]
    fn bounded_push_evicts_oldest_over_cap() {
        let mut r = VecDeque::new();
        for i in 0..5 {
            bounded_push(&mut r, 3, rec(i));
        }
        let seqs: Vec<u64> = r.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2, 3, 4], "只保留最新 cap 条");
    }

    /// 快照 limit：只取最新 N；None/超量 → 全部。
    #[test]
    fn snapshot_takes_latest_n() {
        let mut r = VecDeque::new();
        for i in 0..5 {
            bounded_push(&mut r, 100, rec(i));
        }
        let last2: Vec<u64> = collect_snapshot(&r, Some(2))
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(last2, vec![3, 4]);
        assert_eq!(collect_snapshot(&r, None).len(), 5);
        assert_eq!(collect_snapshot(&r, Some(99)).len(), 5, "limit 超量 → 全部");
    }

    // ── BUG-P2-5：POLARIS_LOG 环境超驰不得被运行期 set_level 击穿 ──

    /// env 超驰判定：可识别的 `POLARIS_LOG` → 超驰生效；未设 / 无法识别 → 无超驰（不冻住配置侧控制）。
    #[test]
    fn env_override_level_only_on_recognized_value() {
        assert_eq!(env_override_level(Some("debug")), Some(LevelFilter::Debug));
        assert_eq!(env_override_level(Some("off")), Some(LevelFilter::Off));
        assert_eq!(env_override_level(None), None, "未设 → 无超驰");
        assert_eq!(
            env_override_level(Some("bogus")),
            None,
            "无法识别的 env 值不得冻住配置侧的级别控制"
        );
    }

    /// 全局级别态（`log::max_level` + `ENV_LEVEL_OVERRIDE`）是进程级单例 → 触碰它的测试必须串行，
    /// 否则并行跑时互相打架。
    static LEVEL_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 启动时采纳 `POLARIS_LOG` → **必须武装超驰 flag**，否则 [`set_level`] 的让位形同虚设
    /// （env 优先级只在启动那一刻成立，第一次配置写就被顶回去）。
    ///
    /// 这条锁的是「两半接线」：`set_level` 的让位闸门（另一条测试）+ 本处的武装，缺任一半 bug 就整个回来。
    #[test]
    fn resolve_startup_level_arms_override_flag_on_env() {
        let _g = LEVEL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = ENV_LEVEL_OVERRIDE.load(Ordering::Relaxed);

        // 无 env → 不武装（config/默认路径不得冻住配置侧的级别控制）。
        ENV_LEVEL_OVERRIDE.store(false, Ordering::Relaxed);
        assert_eq!(
            resolve_startup_level(None, Path::new("/proc/polaris-nope")),
            LevelFilter::Info
        );
        assert!(
            !ENV_LEVEL_OVERRIDE.load(Ordering::Relaxed),
            "无 env → 不得武装超驰"
        );

        // 无法识别的 env → 同样不武装。
        assert_eq!(
            resolve_startup_level(Some("bogus"), Path::new("/proc/polaris-nope")),
            LevelFilter::Info
        );
        assert!(
            !ENV_LEVEL_OVERRIDE.load(Ordering::Relaxed),
            "无法识别的 env 值不得武装超驰（否则配置侧被永久冻住）"
        );

        // 可识别的 env → 采纳该级别 **且** 武装超驰。
        assert_eq!(
            resolve_startup_level(Some("debug"), Path::new("/proc/polaris-nope")),
            LevelFilter::Debug
        );
        assert!(
            ENV_LEVEL_OVERRIDE.load(Ordering::Relaxed),
            "采纳 POLARIS_LOG 后必须武装超驰，否则第一次配置写就把 debug 顶回 info"
        );

        ENV_LEVEL_OVERRIDE.store(saved, Ordering::Relaxed);
    }

    /// 生产路径直测：env 超驰生效时，`set_level`（挂在配置写的唯一汇流点上）必须 no-op。
    ///
    /// 打断 `set_level` 的 `ENV_LEVEL_OVERRIDE` 早返 → 本测转红 —— 那正是
    /// 「`POLARIS_LOG=debug` 起 app → 任意配置写把级别拉回 info → 排障会话静默降级」的 bug。
    #[test]
    fn set_level_is_noop_while_env_override_active() {
        let _g = LEVEL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_flag = ENV_LEVEL_OVERRIDE.load(Ordering::Relaxed);
        let saved_level = log::max_level();

        // 模拟 POLARIS_LOG=debug 已在启动时被采纳。
        ENV_LEVEL_OVERRIDE.store(true, Ordering::Relaxed);
        log::set_max_level(LevelFilter::Debug);

        // 配置广播携带 logLevel=info（config 恒带该字段）→ 必须被忽略。
        set_level("info");
        assert_eq!(
            log::max_level(),
            LevelFilter::Debug,
            "env 超驰生效期间 config.logLevel 不得顶掉级别（否则排障抓的 debug 静默降级）"
        );

        ENV_LEVEL_OVERRIDE.store(saved_flag, Ordering::Relaxed);
        log::set_max_level(saved_level);
    }

    /// 无 env 超驰时 `set_level` 照常生效（让位只在超驰生效时发生，不得把配置侧控制一并废掉）。
    #[test]
    fn set_level_applies_without_env_override() {
        let _g = LEVEL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_flag = ENV_LEVEL_OVERRIDE.load(Ordering::Relaxed);
        let saved_level = log::max_level();

        ENV_LEVEL_OVERRIDE.store(false, Ordering::Relaxed);
        log::set_max_level(LevelFilter::Info);

        set_level("warn");
        assert_eq!(
            log::max_level(),
            LevelFilter::Warn,
            "无 env 超驰 → 配置侧照常生效"
        );

        set_level("bogus");
        assert_eq!(
            log::max_level(),
            LevelFilter::Warn,
            "无法识别的值 → 级别不变"
        );

        ENV_LEVEL_OVERRIDE.store(saved_flag, Ordering::Relaxed);
        log::set_max_level(saved_level);
    }

    // ── BUG-P3-7：环内 seq 单调 + 水合/流式衔接不重不漏 ──

    /// seq 在 ring 锁内分配 → 环内 seq 恒单调递增（`collect_from` 的「末条 seq+1」游标语义前提）。
    ///
    /// 高并发下把发号与入环打散即可能乱序：本测起 8 线程 × 200 条抢同一把锁。**注意**：这是概率性
    /// 捕获（乱序需真实交错），不是确定性判定——它对正确实现恒绿（无 flake 风险），对错误实现大概率转红。
    #[test]
    fn ring_seqs_stay_monotonic_under_concurrent_push() {
        let _g = LEVEL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let threads: Vec<_> = (0..8)
            .map(|t| {
                std::thread::spawn(move || {
                    for i in 0..200 {
                        push_ring(0, Level::Info, "t", format!("{t}-{i}"));
                    }
                })
            })
            .collect();
        for h in threads {
            h.join().unwrap();
        }
        let (recs, _) = snapshot_with_cursor(None);
        let seqs: Vec<u64> = recs.iter().map(|r| r.seq).collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "环内 seq 必须单调递增（乱序 → collect_from 的游标会重放/跨过条目）"
        );
        clear();
    }

    /// 水合与流式首尾相接：`snapshot_with_cursor` 的游标之后拉增量 → 只得到快照之后写入的条目，
    /// 快照里的一条都不重放。
    ///
    /// 打断 `snapshot_with_cursor`（改成先取游标再取快照，或分两把锁取）→ 本测转红。
    #[test]
    fn snapshot_and_cursor_hand_off_without_replay_or_gap() {
        let _g = LEVEL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        for i in 0..3 {
            push_ring(0, Level::Info, "t", format!("hydrate-{i}"));
        }
        let (snap, cursor) = snapshot_with_cursor(None);
        assert_eq!(snap.len(), 3, "水合拿到已写入的 3 条");
        assert!(
            snap.iter().all(|r| r.seq < cursor),
            "游标必须严格大于快照内全部 seq（否则已水合的条目会被流式重放）"
        );

        // 快照之后新写入的条目 → 流式恰好接上。
        for i in 0..2 {
            push_ring(0, Level::Info, "t", format!("stream-{i}"));
        }
        let (batch, next) = records_from(cursor);
        let msgs: Vec<&str> = batch.iter().map(|r| r.message.as_str()).collect();
        assert_eq!(
            msgs,
            vec!["stream-0", "stream-1"],
            "只推快照之后的增量，不重放水合过的"
        );
        assert!(records_from(next).0.is_empty(), "游标推进后无重放");
        clear();
    }

    /// 增量拉取：只取 seq>=from，游标推进到末尾+1；无新条目游标不动（防重放/漏推）。
    #[test]
    fn collect_from_streams_incrementally() {
        let mut r = VecDeque::new();
        for i in 10..15 {
            bounded_push(&mut r, 100, rec(i));
        }
        let (batch1, cur1) = collect_from(&r, 12);
        assert_eq!(
            batch1.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![12, 13, 14]
        );
        assert_eq!(cur1, 15, "游标 = 末条 seq+1");
        let (batch2, cur2) = collect_from(&r, cur1);
        assert!(batch2.is_empty(), "无新增 → 空批");
        assert_eq!(cur2, cur1, "无新增 → 游标不动（不重放）");
    }
}
