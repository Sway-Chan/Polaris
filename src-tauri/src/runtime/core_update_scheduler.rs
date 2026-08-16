//! 内核自动更新调度器（上游 `CoreUpdateScheduler` + `CoreUpdateService.runAutoUpdateCycle` 移植）。
//!
//! **补的是整条断链**：`crates/updater` 的纯决策 [`UpdateScheduler`] 与 staged 生产端
//! [`CoreStagedUpdater::stage`] 此前在生产侧零调用（唯一 caller 是 `lib.rs` 的 re-export 与
//! `runtime/http.rs` 的 `#[cfg(test)]` 段），`EVENT_CORE_AUTO_UPDATE_STATUS` 全仓零 emit，
//! `core_update_get_auto_status` 的 `autoUpdateCore` 硬返 `null` —— 消费端（`core_update_apply_staged`
//! 的五态、前端订阅）早就齐了，缺的只有生产端。本模块就是那个生产端。
//!
//! # 三条腿 + 两道闸
//!
//! | 腿 | 时机 | 干什么 |
//! |---|---|---|
//! | 启动 | T+30s（[`ScheduleConfig::startup_delay`]） | 跑一轮 `cycle_if_due` |
//! | 巡检 | 每 6h（[`ScheduleConfig::tick_interval`]） | 同上 |
//! | 代理停止 | `event:proxyStopped` 后 5s 双查 | 落位 staged（唯一的换核时机） |
//!
//! 两道闸：`autoUpdateCore` 总开关（[`auto_update_core_enabled`]，**缺省关**）+ 24h due
//! （[`ScheduleConfig::check_interval`]，收在 [`UpdateScheduler::should_check`]）。
//!
//! # 硬不变量（逐条对齐 上游）
//!
//! 1. **绝不主动断流**：自动路径只在 `proxy.status().running == false` 时才落位换核。运行中一律
//!    保持 staged、返回 deferred。「停代理→换核→重启」只有用户点「立即应用」
//!    （`core_update_apply_staged`）才允许。
//!    **判定的权威点在 `commands::updater::swap_core_with_restart`**（与读 `was_running` 同一处、
//!    与 `proxy.stop()` 之间无 await）；[`CoreUpdateScheduler::apply_staged_auto`] 里那道同款
//!    gating 只是省一次白跑 —— 两处之间隔着读几十 MB 暂存核的时间，用户在窗口里点连接会让
//!    「先判后停」变成一次无同意的断流（TOCTOU）。
//! 2. **跨带绝不自动**：自动路径恒强制同 `major.minor`，**不受** `restrictCoreUpdateToCompatibleMinor`
//!    影响（那个开关只作用于半手动的 `core_update_run`）。跨带只发一次提示事件，不下载。
//!    判定沿用既有的 [`same_major_minor`]（经 `core_update_check` 已算好的 `crossBand` 字段），
//!    不另写一套；`CoreStagedUpdater` 的 `restrict_band=true` 是同一条闸的第二道。
//! 3. **fork 核绝不被覆盖**：活核是第三方 fork → 零网络早退，且作废任何暂存的官方核。
//! 4. **失败轮不刷 `lastCheckAt`**：检查失败保留旧值让 6h tick 下轮重试，而非把 24h due 整个推迟。
//!
//! # 纯决策 / 副作用分离（与 `subscription_scheduler` 同纪律）
//!
//! 判定全收在纯函数：[`auto_update_core_enabled`] / [`decide_cycle`] / [`build_auto_status_payload`]
//! / [`UpdateScheduler::should_check`]（在 `crates/updater`）。定时器 / HTTP / FS / 事件是薄壳，
//! 逐个动作调**既有**命令（`core_update_check` / `core_update_apply_staged`）与既有适配器
//! （`CoreDownloader` / `extract_core_bytes`），不复制第二份编排。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use polaris_updater::core_build::CoreBuildKind;
use polaris_updater::scheduler::{ScheduleConfig, SystemClock, UpdateScheduler};
use polaris_updater::staged::{
    ApplyOutcome, CoreStagedUpdater, StagedConfig, StagedInfo, StagedStateStore, StagedUpdateError,
};
use polaris_updater::traits::{DownloadError, StdFs, UpdateDownloader};
use polaris_updater::verify::verify_bytes;
use polaris_updater::VersionManifestEntry;
use serde_json::{json, Value};
use tauri::{AppHandle, Listener, Manager};

use crate::events::channel::{EVENT_CORE_AUTO_UPDATE_STATUS, EVENT_PROXY_STOPPED};
use crate::runtime::updater::{StagedRecord, UpdateStateFile, UpdaterRuntime};
use crate::runtime::{core_paths, AppRuntime};

// ── 纯决策 ────────────────────────────────────────────────────────────────────

/// 纯决策：内核自动更新总开关。
///
/// **`=== true` 才开（缺省 = 关）**，逐字对齐 上游 `CoreUpdateScheduler.cycleIfDue`
/// 的 `if (config.autoUpdateCore !== true) return` 与 `core-management-card.tsx` 的
/// `checked={config?.autoUpdateCore === true}`。
///
/// # 为什么方向与「自动检查更新」相反
///
/// `autoCheckUpdate` 缺省为开（[`crate::runtime::startup_tasks::should_auto_check_update`]）——
/// 它只是**只读**地问一句 GitHub，误开无害。本开关会**替换正在被 root/SYSTEM 起核使用的二进制**，
/// 误开的代价是用户从未同意的静默换核。故失败安全的方向在这里是「关」。
#[must_use]
pub fn auto_update_core_enabled(config: &Value) -> bool {
    config.get("autoUpdateCore").and_then(Value::as_bool) == Some(true)
}

/// 一轮周期该干什么（[`decide_cycle`] 的产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleAction {
    /// 本轮无事可做（无更新 / 无适配资产 / 缺下载地址）。
    Idle,
    /// 跨带：**不下载**，只按需记一次提示并发事件。`notify=false` = 该版本已提示过。
    CrossBand { latest: String, notify: bool },
    /// 已有同版本 staged → 直接试落位，不重复下载（= 上游 `staged-same-version`）。
    ApplyStaged,
    /// 带内且确有更新 → 下载 + 暂存。
    Download {
        latest: String,
        url: String,
        sha256: Option<String>,
    },
}

/// 一轮周期的完整决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleDecision {
    /// 清除残留的跨带提示（= 上游 M3：用户手动升级追上 / 已同带后，旧提示不该常驻 UI）。
    pub clear_cross_band_notice: bool,
    pub action: CycleAction,
}

/// 纯决策：由 `core_update_check` 的返回 + 当前 staged + 已提示过的跨带版本，定出本轮动作。
///
/// 入参 `check` 是 [`crate::commands::core_update_check`] 的 `data`
/// （`{hasUpdate, currentVersion, latestVersion, downloadUrl, sha256, crossBand}`）。
///
/// 判定顺序逐字对齐 上游 `runAutoUpdateCycle:730-838`：
/// M3 清提示 → 无 latest 即止 → **跨带闸**（只提示不下载）→ 同版本 staged 直落位 →
/// `hasUpdate && downloadUrl` 才下载。
///
/// **跨带用的是 `check.crossBand`**（`core_update_check` 里由 [`same_major_minor`] 算出）——
/// 本函数刻意不重算，两处各算一次必然漂移。
///
/// [`same_major_minor`]: polaris_updater::version::same_major_minor
#[must_use]
pub fn decide_cycle(
    check: &Value,
    staged_version: Option<&str>,
    cross_band_notified: Option<&str>,
) -> CycleDecision {
    let latest = check
        .get("latestVersion")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let current = check
        .get("currentVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let cross_band = check.get("crossBand").and_then(Value::as_bool) == Some(true);

    // M3：当前核已追上（>=）或已同带 → 清掉残留跨带提示。
    let clear_cross_band_notice = cross_band_notified.is_some_and(|notified| {
        !current.is_empty()
            && (polaris_updater::version::compare_semver(current, notified).is_ok_and(|o| o >= 0)
                || polaris_updater::version::same_major_minor(current, notified) == Some(true))
    });

    if latest.is_empty() {
        return CycleDecision {
            clear_cross_band_notice,
            action: CycleAction::Idle,
        };
    }

    // ── 跨带硬闸：绝不下载、绝不落位，每个版本只提示一次。
    if cross_band {
        let notify = check.get("hasUpdate").and_then(Value::as_bool) == Some(true)
            && cross_band_notified != Some(latest.as_str());
        return CycleDecision {
            clear_cross_band_notice,
            action: CycleAction::CrossBand { latest, notify },
        };
    }

    // ── 带内：已有同版本 staged → 直接试落位（避免重复下载）。
    if staged_version == Some(latest.as_str()) {
        return CycleDecision {
            clear_cross_band_notice,
            action: CycleAction::ApplyStaged,
        };
    }

    // 带内但 hasUpdate=false（已是最新）/ 缺下载地址 → 无可下。
    if check.get("hasUpdate").and_then(Value::as_bool) != Some(true) {
        return CycleDecision {
            clear_cross_band_notice,
            action: CycleAction::Idle,
        };
    }
    let Some(url) = check
        .get("downloadUrl")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return CycleDecision {
            clear_cross_band_notice,
            action: CycleAction::Idle,
        };
    };

    CycleDecision {
        clear_cross_band_notice,
        action: CycleAction::Download {
            latest,
            url: url.to_string(),
            sha256: check
                .get("sha256")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        },
    }
}

/// 纯函数：构造 `EVENT_CORE_AUTO_UPDATE_STATUS` 的 payload。
///
/// 形状逐字对齐前端 `coreUpdateApi.onAutoStatusChanged`：
/// `{ lastCheckAt, staged: {version, stagedAt} | null, crossBandLatest }`。
///
/// **刻意不含 `autoUpdateEnabled`**（= 上游 M3）：该真值由 `core_update_get_auto_status` 快照
/// 提供，事件里再发一份占位会在渲染端覆盖掉快照拉到的真值。
///
/// `cross_band_override`：`None` = 用持久态里的 `crossBandNotifiedVersion`；`Some(v)` = 本次显式
/// 覆盖（`Some(None)` 表示「刚清掉」，用于让 UI 立刻撤下旧提示）。
#[must_use]
pub fn build_auto_status_payload(
    state: &UpdateStateFile,
    cross_band_override: Option<Option<&str>>,
) -> Value {
    let cross_band_latest = match cross_band_override {
        Some(v) => v.map(str::to_string),
        None => state.cross_band_notified_version.clone(),
    };
    json!({
        "lastCheckAt": state.last_check_at,
        "staged": state.staged.as_ref().map(|s| json!({
            "version": s.version,
            "stagedAt": s.staged_at,
        })),
        "crossBandLatest": cross_band_latest,
    })
}

// ── staged 生产端的两个宿主适配器 ─────────────────────────────────────────────

/// 把「已下载并校验完、解归档后的裸核字节」交给 [`CoreStagedUpdater`] 的进程内 downloader。
///
/// # 为什么不直接把 GitHub 资产 URL 喂给 `CoreStagedUpdater`
///
/// 官方 sing-box 资产是 `.tar.gz`/`.zip`，而 `stage` 会把 `download()` 返回的字节**原样**写成
/// `sing-box` —— 直接喂 URL 会暂存一个归档文件冒充内核。解归档需要跑 OS 的 `tar`
/// （见 `core_swap` 模块文档），属 runtime 行为、**不在纯逻辑 crate 边界内**。
///
/// 故分工是：真下载（含重定向 / 完整性 / 镜像回退）+ **对归档字节**的 sha256 强校验 + 解归档
/// 都在薄壳里用既有 [`CoreDownloader`](crate::runtime::http::CoreDownloader) /
/// [`extract_core_bytes`](crate::commands::extract_core_bytes) 做完，`stage` 只负责它真正擅长的那段：
/// 版本闸 + 带闸 + 原子落盘 + 簿记。本适配器就是这条交接线。
///
/// 对应地，交给 `stage` 的 entry `sha256` 恒 `None` —— GitHub 给的是**归档**的摘要，拿它去校验
/// **解压产物**必然不符。校验没被跳过，只是发生在解归档之前（见 `run_download_and_stage`）。
struct PreparedCoreBytes(Vec<u8>);

/// 交给 [`CoreStagedUpdater`] 的占位 URL：字节已在手，这个串只用于在 map 里对上号，从不出网。
const PREPARED_BYTES_URL: &str = "polaris-internal://prepared-core-bytes";

impl UpdateDownloader for PreparedCoreBytes {
    fn download(&self, url: &str) -> Result<Vec<u8>, DownloadError> {
        if url != PREPARED_BYTES_URL {
            // 结构性不可达（entry.url 由本模块构造）。真走到这里说明有人把真 URL 塞了进来 ——
            // 必须响亮失败，绝不把归档字节当裸核暂存。
            return Err(DownloadError::Other(format!(
                "PreparedCoreBytes 只服务 {PREPARED_BYTES_URL}，收到 {url}"
            )));
        }
        Ok(self.0.clone())
    }
}

/// 生产 staged 簿记：读写 `update-state.json` 的 `staged` 字段（经 [`UpdaterRuntime`] 的原子写）。
///
/// # 只服务生产端（`stage`），不服务落位端
///
/// 落位在本仓走 `core_update_apply_staged` 命令 —— 那里有停/起核编排、备份、验证闩与自动回滚、
/// `pendingChangeNotice`，是**唯一**的换核落位路径（`swap_core_with_restart`）。
/// 故 [`CoreStagedUpdater::apply`] / [`CoreStagedUpdater::try_apply_staged`] 在本仓**没有调用点**，
/// 它们要的 `record_applied` / `last_applied_version` 两个方法也随之无生产语义可映射。
/// 下面对这两个方法的实现如实反映这一点（不假装记了账）。
struct UpdaterStagedStore<'a> {
    updater: &'a UpdaterRuntime,
}

impl StagedStateStore for UpdaterStagedStore<'_> {
    fn load_staged(&self) -> Option<StagedInfo> {
        self.updater.state().staged.map(|r| StagedInfo {
            version: r.version,
            dir: std::path::PathBuf::from(r.dir),
            staged_at: r.staged_at,
        })
    }

    fn save_staged(&self, info: &StagedInfo) -> Result<(), StagedUpdateError> {
        let record = StagedRecord {
            version: info.version.clone(),
            dir: info.dir.to_string_lossy().into_owned(),
            staged_at: info.staged_at.clone(),
        };
        self.updater
            .mutate_state(|s| s.staged = Some(record))
            .map_err(StagedUpdateError::StateStore)
    }

    fn clear_staged(&self) -> Result<(), StagedUpdateError> {
        self.updater
            .mutate_state(|s| s.staged = None)
            .map_err(StagedUpdateError::StateStore)
    }

    /// 最近一次落位的版本 = `pendingChangeNotice.currentVersion`（由 `swap_core_with_restart` 写）。
    /// 通知被 ack 清除后返 `None` —— 如实反映「本仓不单独持久化 last-applied」，不编造。
    fn last_applied_version(&self) -> Option<String> {
        self.updater
            .state()
            .pending_change_notice
            .map(|n| n.current_version)
    }

    /// **本仓无调用点**（见结构体文档）：落位记账由 `swap_core_with_restart` 一次写完
    /// `pendingChangeNotice{previous, current}` —— 它同时知道 previous，本方法不知道，
    /// 在这里补写只能编一个假的 previous（banner 会显示错误的「从 X 升到 Y」）。
    ///
    /// 故这里**不写**，只留响亮日志：若将来有人把 `apply`/`try_apply_staged` 接进生产路径，
    /// 日志会立刻暴露「记账没跟上」，而不是静默丢失。返回 `Ok` 是 trait 契约要求
    /// （此方法在 `apply` 里是**换核已完成之后**才调的，报错会把成功的换核判成失败）。
    fn record_applied(&self, version: &str) -> Result<(), StagedUpdateError> {
        log::error!(
            "UpdaterStagedStore::record_applied({version}) 被调用 —— 本仓的落位记账应走 \
             swap_core_with_restart（core_update_apply_staged）；请检查是否误接了 CoreStagedUpdater::apply"
        );
        Ok(())
    }
}

// ── 调度器薄壳 ────────────────────────────────────────────────────────────────

/// 内核自动更新调度器（真定时器 + 真 I/O 的薄壳；决策全在纯函数里）。
pub struct CoreUpdateScheduler {
    /// 纯决策核（启动闸 / 24h due 闸 / 防重入 / `lastCheckAt`）。
    inner: Mutex<UpdateScheduler<SystemClock>>,
    /// 全局互斥闸（= 上游 `isUpdating`，H1/L1）：覆盖「检查→下载→暂存」与「落位」两段。
    /// 二者并发会同写 `core-staged/` 与现役核目录 → 备份取自半替换核心、回滚失真。
    busy: AtomicBool,
}

impl Default for CoreUpdateScheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// `busy` 闸的 RAII 释放（中途 return / panic 均复位）。
struct BusyGuard<'a>(&'a AtomicBool);
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// `UpdateScheduler::running` 闩的 RAII 释放（与 [`BusyGuard`] 同纪律）。
///
/// # 没有它会怎样
///
/// `run_cycle` 链上任何一处 panic（本模块此前的 `.expect("core update scheduler lock")`、
/// 或它调用的命令层 panic）都会跳过 `cycle_if_due` 末尾那句 `mark_done` ⇒ `running` 恒 true ⇒
/// [`UpdateScheduler::should_check`] 永远返 false ⇒ **内核自动更新静默死亡到进程重启为止**，
/// 且没有任何一条日志会说这件事（`BusyGuard` 只复位 `busy`，管不着 `running`）。
///
/// 故：正常路径调 [`Self::finish`]（按本轮成败刷 `last_check`），异常路径由 `Drop` 兜底
/// —— 并且**响亮记一条 error**，让「自动更新怎么不跑了」有据可查。
struct RunGuard<'a> {
    inner: &'a Mutex<UpdateScheduler<SystemClock>>,
    /// 已由 [`Self::finish`] 正常收尾 → `Drop` 不再动它。
    finished: bool,
}

impl<'a> RunGuard<'a> {
    const fn new(inner: &'a Mutex<UpdateScheduler<SystemClock>>) -> Self {
        Self {
            inner,
            finished: false,
        }
    }

    /// 正常收尾：按本轮**检查**是否成功刷 `last_check`，释放 running 闩，返回刷新后的 `last_check`。
    fn finish(mut self, checked_ok: bool) -> u64 {
        self.finished = true;
        let mut s = lock_scheduler(self.inner);
        // 仅成功检查刷新 last_check：失败轮保留旧值让 6h tick 下轮重试，
        // 而不是把整 24h due 推迟（上游 明写的取舍）。
        s.mark_done(checked_ok);
        s.last_check_ms()
    }
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        log::error!(
            "内核自动更新：一轮周期异常终止（panic 或任务被取消），已强制释放 running 闩 —— \
             不释放则 should_check 永假，自动更新会静默死亡到下次重启"
        );
        lock_scheduler(self.inner).mark_done(false);
    }
}

/// 取调度器锁；**锁毒化不 panic**。
///
/// 毒化恰是本模块要防的静默死亡成因之一：`.expect()` 会让此后每一次 `cycle_if_due` 都 panic，
/// 而 panic 又在 `RunGuard::drop` 里 —— 那是 double panic ⇒ abort。故一律取回内部值继续用，
/// 并响亮记一条 error（状态可能不自洽，但比整条腿死掉好，且日志能对上账）。
fn lock_scheduler(
    inner: &Mutex<UpdateScheduler<SystemClock>>,
) -> std::sync::MutexGuard<'_, UpdateScheduler<SystemClock>> {
    inner.lock().unwrap_or_else(|e| {
        log::error!("内核自动更新调度器锁已毒化（此前有一轮 panic）：取回内部状态继续，不放弃调度");
        e.into_inner()
    })
}

impl CoreUpdateScheduler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(UpdateScheduler::new(ScheduleConfig::default(), SystemClock)),
            busy: AtomicBool::new(false),
        }
    }

    /// 装三条腿：T+30s 启动检查、6h 巡检、`event:proxyStopped` 后 5s 落位。幂等（重复调 no-op）。
    ///
    /// **错峰**：30s 刻意避开 `startup_tasks` 的 2s/3s/5s/6s/7s 与订阅 8s、规则资源 12s
    /// （= 上游 常量注释「避开 2s autoConnect / 5s App 更新检查 / 8s 订阅补更高峰」）。
    pub fn start(self: &Arc<Self>, app: AppHandle) {
        let (startup_delay, tick_interval) = {
            let mut s = lock_scheduler(&self.inner);
            if s.is_started() {
                return;
            }
            // 跨重启恢复 lastCheckAt：否则每次开 App 都算「从未检查」→ 24h due 闸形同虚设。
            if let Some(rt) = app.try_state::<AppRuntime>() {
                if let Some(last) = rt.updater().state().last_check_at {
                    s.restore_last_check(last);
                }
            }
            s.start();
            (s.config().startup_delay, s.config().tick_interval)
        };

        // 启动腿。
        let this = self.clone();
        let app_startup = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(startup_delay).await;
            this.cycle_if_due(&app_startup).await;
        });

        // 6h 巡检腿。
        let this = self.clone();
        let app_tick = app.clone();
        tauri::async_runtime::spawn(async move {
            let mut ticker = tokio::time::interval(tick_interval);
            ticker.tick().await; // 立即触发的首 tick 跳过（启动腿已覆盖）
            loop {
                ticker.tick().await;
                this.cycle_if_due(&app_tick).await;
            }
        });

        // 代理停止 → 延 5s 双查后落位（**唯一**不断流的换核窗口）。
        let this = self.clone();
        let app_evt = app.clone();
        app.listen(EVENT_PROXY_STOPPED, move |_| {
            let this = this.clone();
            let app = app_evt.clone();
            tauri::async_runtime::spawn(async move {
                this.on_proxy_stopped(&app).await;
            });
        });
    }

    /// 代理停止后的落位腿：延迟 + 双查（规避 `stop→start` 重启窗口内误判「已停止」）。
    async fn on_proxy_stopped(self: &Arc<Self>, app: &AppHandle) {
        let delay = {
            let s = lock_scheduler(&self.inner);
            if !s.is_started() {
                return;
            }
            s.config().stopped_apply_delay
        };
        tokio::time::sleep(delay).await;

        let Some(state) = app.try_state::<AppRuntime>() else {
            return;
        };
        let proxy_running = state.proxy().status().running;
        {
            let s = lock_scheduler(&self.inner);
            if !s.should_apply_on_proxy_stopped(proxy_running) {
                log::debug!("代理在 5s 内又起来了（重启窗口）→ 本次不落位 staged 内核");
                return;
            }
        }
        self.apply_staged_auto(app, "proxy-stopped").await;
    }

    /// 一轮周期：时间闸（纯决策）→ 跑 cycle → 按结果刷新 `lastCheckAt`。
    ///
    /// `running` 闩由 [`RunGuard`] 持有：`run_cycle` 链上的 panic / 任务取消都会走它的 `Drop`，
    /// **不存在**「闩没释放 → 自动更新静默死亡」那条路径（守卫见单测
    /// `run_guard_releases_running_even_on_panic` + `cycle_if_due_holds_a_run_guard_across_the_cycle`）。
    async fn cycle_if_due(self: &Arc<Self>, app: &AppHandle) {
        {
            let mut s = lock_scheduler(&self.inner);
            if !s.should_check() {
                return;
            }
            s.mark_running();
        }
        let guard = RunGuard::new(&self.inner);
        let checked_ok = self.run_cycle(app).await;
        let last_check_ms = guard.finish(checked_ok);
        if checked_ok {
            if let Some(state) = app.try_state::<AppRuntime>() {
                let _ = state
                    .updater()
                    .mutate_state(|st| st.last_check_at = Some(last_check_ms));
                emit_auto_status(app, state.updater(), None);
            }
        }
    }

    /// 真正跑一轮（守门 → 检查 → 决策 → 动作）。返回「本轮**检查**是否成功」（决定刷不刷 `lastCheckAt`）。
    async fn run_cycle(self: &Arc<Self>, app: &AppHandle) -> bool {
        if self.busy.swap(true, Ordering::SeqCst) {
            return false; // 已有一轮在飞（下载/落位）→ 本轮放弃，不叠加
        }
        let _guard = BusyGuard(&self.busy);

        let Some(state) = app.try_state::<AppRuntime>() else {
            return false;
        };
        let Ok(config) = state.config().load_full() else {
            log::warn!("内核自动更新：读取配置失败，跳过本轮");
            return false;
        };
        // ── 闸 1：总开关（缺省关）。
        if !auto_update_core_enabled(&config) {
            return false;
        }
        // ── 闸 2：fork 硬闸（零网络）。第三方核绝不被官方核覆盖，并作废任何暂存的官方核。
        if state.updater().core_build_kind() == CoreBuildKind::Fork {
            if state.updater().state().staged.is_some() {
                log::info!("当前为第三方内核，作废暂存的官方内核（不覆盖用户内核）");
                discard_staged(state.updater());
                emit_auto_status(app, state.updater(), None);
            }
            return false;
        }

        // ── 检查（复用既有命令：fork 闸 / 资产选择 / 版本比较 / 跨带标注全在里面）。
        let checked = match crate::commands::core_update_check(state.clone()).await {
            Ok(r) => r,
            Err(()) => return false,
        };
        let Some(data) = checked.data.filter(|_| checked.success) else {
            log::warn!(
                "内核自动更新：检查失败（{}），保留 lastCheckAt 待下轮重试",
                checked.error.as_deref().unwrap_or("未知错误")
            );
            return false;
        };

        let st = state.updater().state();
        let decision = decide_cycle(
            &data,
            st.staged.as_ref().map(|s| s.version.as_str()),
            st.cross_band_notified_version.as_deref(),
        );

        if decision.clear_cross_band_notice {
            log::info!("当前内核已追上跨带提示版本，清除跨带提示");
            let _ = state
                .updater()
                .mutate_state(|s| s.cross_band_notified_version = None);
            emit_auto_status(app, state.updater(), Some(None));
        }

        match decision.action {
            CycleAction::Idle => {}
            CycleAction::CrossBand { latest, notify } => {
                if notify {
                    log::info!("检测到跨版本带新版 {latest}，**不自动更新**，已提示用户手动处理");
                    let v = latest.clone();
                    let _ = state
                        .updater()
                        .mutate_state(|s| s.cross_band_notified_version = Some(v));
                    emit_auto_status(app, state.updater(), Some(Some(&latest)));
                }
            }
            CycleAction::ApplyStaged => {
                // 提前放闸：落位腿要自己重新取（Drop 即 store(false)，不必再手写一遍）。
                drop(_guard);
                self.apply_staged_auto(app, "staged-same-version").await;
            }
            CycleAction::Download {
                latest,
                url,
                sha256,
            } => {
                match run_download_and_stage(&state, &latest, &url, sha256.as_deref()).await {
                    Ok(outcome) => {
                        log::info!("内核 {latest} 暂存结果：{outcome:?}");
                        emit_auto_status(app, state.updater(), None);
                        // 下载后立即试落位：代理未运行→直接换；运行中→保持 staged 待安全窗口。
                        if outcome == ApplyOutcome::Applied {
                            drop(_guard); // 同上：放闸交给落位腿
                            self.apply_staged_auto(app, "post-download").await;
                        }
                    }
                    Err(e) => log::warn!("内核自动更新：下载/暂存 {latest} 失败：{e}"),
                }
            }
        }
        true
    }

    /// 自动落位 staged（**唯一**会真换核的地方；三个触发点共用）。
    ///
    /// 三道闸，逐条对齐 上游 `tryApplyStaged`：
    /// 1. **M4**：`autoUpdateCore` 关 → 保持 staged 不落位（用户撤回同意后，已暂存的核不该自动生效；
    ///    重开开关或点「立即应用」仍可落）。
    /// 2. **不断流硬不变量**：代理运行中 → 绝不落位，保持 staged。
    /// 3. fork 核 → 不覆盖（由 `run_cycle` 的闸 2 与命令层的 fork 闸共同守）。
    ///
    /// 过闸后调**既有**消费端 `core_update_apply_staged_auto` —— 它已实现五态
    /// （applied/discarded/deferred/failed/noop）+ 停起核编排 + 备份 + 验证闩 + 自动回滚 +
    /// `pendingChangeNotice`。
    ///
    /// # 闸 2 在这里只是省一次白跑，**不是**不变量的守卫
    ///
    /// 从这里判 `running == false` 到命令层真的 `proxy.stop()` 之间隔着「读簿记 → 版本闸 →
    /// 读几十 MB 的暂存核 → sha 复核」几十~几百 ms（post-download 触发点更是落在下载完成的任意
    /// 时刻）。用户在这个窗口里点连接，旧实现会照常 stop→swap→restart —— **在用户从未同意的
    /// 情况下断流一次**。故走 `_auto` 入口：真守卫在 `swap_core_with_restart` 里、与读
    /// `was_running` 同一处，中间不再有 await；那一层若发现代理已起，返 `deferred` 并保留 staged。
    async fn apply_staged_auto(self: &Arc<Self>, app: &AppHandle, trigger: &str) {
        if self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let _guard = BusyGuard(&self.busy);

        let Some(state) = app.try_state::<AppRuntime>() else {
            return;
        };
        if state.updater().state().staged.is_none() {
            return; // 无 staged → noop（不惊动命令层）
        }
        let Ok(config) = state.config().load_full() else {
            return; // 读配置失败 → 失败安全：视作未开启，不自动落位
        };
        if !auto_update_core_enabled(&config) {
            log::info!("内核自动更新已关闭，已暂存内核保持不落位（{trigger}）");
            return;
        }
        if state.proxy().status().running {
            log::info!("代理运行中，已暂存内核暂不落位，待停止后生效（{trigger}）");
            return;
        }

        let resp = crate::commands::core_update_apply_staged_auto(app, &state).await;
        let result = resp
            .data
            .as_ref()
            .and_then(|d| d.get("result"))
            .and_then(Value::as_str)
            .unwrap_or(if resp.success { "applied" } else { "failed" });
        log::info!("staged 内核落位（{trigger}）：{result}");
        emit_auto_status(app, state.updater(), None);
    }
}

/// 下载 → sha256 强校验（对**归档**）→ 解归档 → 经 [`CoreStagedUpdater::stage`] 暂存
/// → 记裸核 sha256（旁挂文件，供落位前复核）。
///
/// # 跨带的第二道闸防的是接线漂移，**不是**判据被改坏
///
/// 版本闸 + 带闸（`restrict_band=true`，跨带绝不暂存）由 `stage` 内部把守 —— 这是跨带闸的
/// 第二道（第一道在 [`decide_cycle`]）。两道闸**用的是同一个谓词**
/// （[`same_major_minor`]：`decide_cycle` 消费的 `check.crossBand` 由它算，`stage` 也直接调它），
/// 只是调用点不同。故它拦得住的是「某一处的接线被删/被绕过」这类漂移，
/// **拦不住**谓词本身被改坏（那时两道一起失效）。谓词的牙齿在 `polaris_updater::version` 的单测。
///
/// [`same_major_minor`]: polaris_updater::version::same_major_minor
async fn run_download_and_stage(
    state: &tauri::State<'_, AppRuntime>,
    latest: &str,
    url: &str,
    expected_sha: Option<&str>,
) -> Result<ApplyOutcome, String> {
    let base = core_paths::base_dir().ok_or("内核可写目录未初始化")?;
    let asset_name = url.rsplit('/').next().unwrap_or("core-asset").to_string();

    // ── 真下载（重定向 / 完整性 / 停滞看门狗 / 镜像回退全在既有适配器里）。
    // 内核腿整包入内存（解归档要用）⇒ 闸就是内存闸，逐字沿用形参化之前的 16 MiB。
    let dl = crate::commands::updater_downloader(state, crate::runtime::http::MAX_DOWNLOAD_BYTES);
    let url_owned = url.to_string();
    let bytes = tokio::task::spawn_blocking(move || dl.download(&url_owned))
        .await
        .map_err(|e| format!("下载任务异常终止: {e}"))?
        .map_err(|e| format!("下载内核失败: {e}"))?;

    // ── sha256 强校验：GitHub asset 的 digest 是**归档**的摘要，故必须在解归档**之前**校验。
    if let Some(sha) = expected_sha.filter(|s| !s.is_empty()) {
        verify_bytes(&bytes, sha)
            .map_err(|e| format!("内核校验失败（可能被截断或篡改），已拒绝暂存: {e}"))?;
    }

    // ── 归档 → 裸核字节（走 OS 自带 tar，不引新依赖）。
    let core_bytes = crate::commands::extract_core_bytes(base, &asset_name, &bytes)?;
    // 裸核自算摘要：GitHub 只给**归档**的 digest，落位时手上是解压产物，没有任何现成基准可对。
    // 暂存与落位之间可以隔好几天（代理一直在跑），位腐 / 被本机其它进程改写都拦不住 ——
    // 起核验证闩只能发现「起不来」，发现不了「起得来但行为坏」。
    let core_sha = polaris_updater::verify::sha256_hex(&core_bytes);

    // ── 暂存（版本闸 + 带闸 + 原子写 + 簿记全在 stage 里）。
    let source = PreparedCoreBytes(core_bytes);
    let fs = StdFs;
    let store = UpdaterStagedStore {
        updater: state.updater(),
    };
    // dest_dir 是「落位目标」；`stage` 不写它（只写 staged_dir），但填真值以免将来误读。
    let cfg = StagedConfig::new(core_paths::core_update_dir_in(base));
    let entry = VersionManifestEntry {
        version: latest.to_string(),
        url: PREPARED_BYTES_URL.to_string(),
        // 归档摘要已在上面核过；拿它校验解压产物必然不符（见 PreparedCoreBytes 文档）。
        sha256: None,
        prerelease: false,
        notes: String::new(),
    };
    let current = state.updater().read_core_version();
    let staged_dir = core_paths::staged_dir_in(base);
    let staged_at = current_iso();

    let updater = CoreStagedUpdater::new(&source, &fs, &store, cfg);
    let outcome = updater
        .stage(
            &entry,
            &current,
            core_paths::core_filename(),
            &staged_dir,
            &staged_at,
        )
        .map_err(|e| format!("暂存内核失败: {e}"))?;

    // 摘要旁挂在暂存目录里（`stage` 每次都重建该目录 ⇒ 摘要与核同生共死，不会错配）。
    // **只在真暂存成功后写**：Discarded / Deferred 时目录里根本没有本次的核。
    if outcome == ApplyOutcome::Applied {
        let sha_path = crate::commands::staged_core_sha_path(&staged_dir);
        if let Err(e) = std::fs::write(&sha_path, core_sha.as_bytes()) {
            // 写不下摘要不该让一次成功的暂存回滚（落位端把「无记录」当放行）；但必须留声。
            log::warn!(
                "暂存内核摘要写入失败（{} : {e}）：本次落位将跳过完整性复核",
                sha_path.display()
            );
        }
    }
    Ok(outcome)
}

/// 作废 staged：清簿记 + 删暂存目录（= 上游 `clearStaged`）。
fn discard_staged(updater: &UpdaterRuntime) {
    if let Some(staged) = updater.state().staged {
        let _ = std::fs::remove_dir_all(&staged.dir);
    }
    let _ = updater.mutate_state(|s| s.staged = None);
}

/// 广播 `EVENT_CORE_AUTO_UPDATE_STATUS`（载荷由纯函数 [`build_auto_status_payload`] 构造）。
fn emit_auto_status(
    app: &AppHandle,
    updater: &UpdaterRuntime,
    cross_band_override: Option<Option<&str>>,
) {
    let payload = build_auto_status_payload(&updater.state(), cross_band_override);
    crate::events::broadcast(app, EVENT_CORE_AUTO_UPDATE_STATUS, payload);
}

/// 当前时刻的 RFC3339（`stagedAt`）。与 `commands::*` 的 `current_iso` 同一算法
/// （stats-engine 的 civil 换算，无 chrono/time 依赖）；时钟异常 → 空串，不 panic。
fn current_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .and_then(polaris_stats_engine::created_at_to_rfc3339)
        .unwrap_or_default()
}

/// 方法体源码切片工具（**测试专用**，跨 runtime 模块共用）。
///
/// # 为什么不能用 `commands::guard_scan::top_level_fn_body`
///
/// 那个只认**列 0** 的 `\n}\n` 作结束锚 —— 对 `impl` 块里的方法不成立：切片会一路吃到整个 `impl`
/// 结束，把后续所有方法都囊括进来。于是「把被守的调用从这个方法删掉、在同一个 impl 的下一个方法里
/// 加一句」就能骗过顺序守卫（正是本轮 review 点名的「文档声称有牙、实际没有」的形态）。
///
/// 本模块按花括号配对精确截到该方法**自己**的闭合括号，并跳过字符串字面量与 `//` 行注释
/// （否则 `format!("{x}")`、注释里的括号都会算进深度）。
///
/// 放在本文件而非 `commands.rs`：本轮改动面不含 `commands.rs`。
/// `runtime/rule_resource_scheduler.rs` 的同类守卫从这里 `use`。
///
/// **已知边界**：不处理块注释 `/* */` 与裸字符字面量 `'{'`（本仓被守的几个函数体里都没有）。
#[cfg(test)]
pub(crate) mod method_scan {
    /// 取方法体源码（从签名锚点起，到与签名后第一个 `{` 配对的 `}` 止，含两端）。
    ///
    /// 锚点缺失 / 括号不配对一律 panic —— 守卫**失去判据时必须转红**，而不是静默退化成
    /// 「扫了个空串、断言恒真」。
    pub(crate) fn method_body<'a>(src: &'a str, signature: &str) -> &'a str {
        let start = src
            .find(signature)
            .unwrap_or_else(|| panic!("锚点消失，守卫已失去判据: {signature}"));
        let rest = &src[start..];
        let open = rest
            .find('{')
            .unwrap_or_else(|| panic!("{signature} 之后找不到左花括号（守卫已失去判据）"));
        // 逐字节扫：UTF-8 续字节恒 >= 0x80，绝不会与下面这几个 ASCII 记号相等，故按字节比较安全。
        let bytes = rest.as_bytes();
        let (mut depth, mut i) = (0usize, open);
        let (mut in_str, mut in_line_comment) = (false, false);
        while i < bytes.len() {
            let c = bytes[i];
            if in_line_comment {
                if c == b'\n' {
                    in_line_comment = false;
                }
            } else if in_str {
                match c {
                    b'\\' => i += 1, // 跳过被转义的那个字节
                    b'"' => in_str = false,
                    _ => {}
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    b'/' if bytes.get(i + 1) == Some(&b'/') => {
                        in_line_comment = true;
                        i += 1;
                    }
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            return &rest[..=i]; // `}` 是单字节 ASCII ⇒ i+1 必是 char 边界
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        panic!("{signature} 的花括号不配对（守卫已失去判据）")
    }

    /// **守卫的守卫**：证明 [`method_body`] 真的截在方法自己的括号上。
    ///
    /// 没有这条，「我按括号配对封了顶」只是一句注释。
    #[test]
    fn method_body_stops_at_the_methods_own_brace() {
        let src = "impl X {\n    fn target(&self) {\n        if a { inside(); }\n    }\n\n    fn later(&self) {\n        outside();\n    }\n}\n";
        let body = method_body(src, "fn target(");
        assert!(body.contains("inside()"), "必须包含被守方法自己的函数体");
        assert!(
            !body.contains("outside()"),
            "**封顶失效**：切到了同一个 impl 的后续方法 → 守卫可被「删这里、加那里」骗过"
        );

        // 字符串里的花括号（`format!("{x}")` 之类）不得计入深度。
        let with_fmt = "impl X {\n    fn target(&self) {\n        log(\"a {b} c }\");\n        tail();\n    }\n\n    fn later(&self) {\n        outside();\n    }\n}\n";
        let body = method_body(with_fmt, "fn target(");
        assert!(
            body.contains("tail()"),
            "字符串字面量里的右花括号被误当作方法结束"
        );
        assert!(!body.contains("outside()"));

        // 行注释里的花括号同理。
        let with_comment = "impl X {\n    fn target(&self) {\n        // } 这不是结束\n        tail();\n    }\n\n    fn later(&self) {\n        outside();\n    }\n}\n";
        let body = method_body(with_comment, "fn target(");
        assert!(body.contains("tail()"));
        assert!(!body.contains("outside()"));
    }

    /// 锚点消失必须 panic（转红），而不是返回空切片让断言恒真。
    #[test]
    #[should_panic(expected = "锚点消失")]
    fn missing_anchor_panics_instead_of_silently_passing() {
        method_body("fn other() {\n}\n", "fn nonexistent(");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::core_update_scheduler::method_scan::method_body;
    use serde_json::json;
    use std::time::Duration;

    /// 本文件源码（调用点守卫共用）。
    const SRC: &str = include_str!("core_update_scheduler.rs");

    // ── 总开关（变异锁：任何把它改成 `!= Some(false)` 的变异都会转红）─────────────
    #[test]
    fn auto_update_core_defaults_to_off_and_needs_explicit_true() {
        assert!(
            !auto_update_core_enabled(&json!({})),
            "缺字段必须视为关 —— 静默换核不能是默认行为"
        );
        assert!(!auto_update_core_enabled(
            &json!({ "autoUpdateCore": false })
        ));
        assert!(
            !auto_update_core_enabled(&json!({ "autoUpdateCore": "true" })),
            "非 bool 一律关（不做字符串 truthy 推断）"
        );
        assert!(!auto_update_core_enabled(&json!({ "autoUpdateCore": 1 })));
        assert!(auto_update_core_enabled(&json!({ "autoUpdateCore": true })));
    }

    fn check(
        has_update: bool,
        current: &str,
        latest: &str,
        cross_band: bool,
        url: &str,
    ) -> serde_json::Value {
        json!({
            "hasUpdate": has_update,
            "currentVersion": current,
            "latestVersion": latest,
            "downloadUrl": url,
            "sha256": "a".repeat(64),
            "crossBand": cross_band,
        })
    }

    // ── decide_cycle 真值表 ──────────────────────────────────────────────────
    #[test]
    fn in_band_update_downloads() {
        let d = decide_cycle(
            &check(true, "1.13.13", "1.13.14", false, "https://x/core.tar.gz"),
            None,
            None,
        );
        assert_eq!(
            d.action,
            CycleAction::Download {
                latest: "1.13.14".into(),
                url: "https://x/core.tar.gz".into(),
                sha256: Some("a".repeat(64)),
            }
        );
        assert!(!d.clear_cross_band_notice);
    }

    /// 🟡 **变异锁：跨带必须 stage 都不 stage —— 连下载都不许发生。**
    ///
    /// 把 `if cross_band` 分支删掉 ⇒ 本条转红（会变成 Download）。
    #[test]
    fn cross_band_never_downloads_only_notifies_once() {
        let c = check(true, "1.13.13", "1.14.0", true, "https://x/core.tar.gz");
        // 首次：提示。
        let d = decide_cycle(&c, None, None);
        assert_eq!(
            d.action,
            CycleAction::CrossBand {
                latest: "1.14.0".into(),
                notify: true
            },
            "跨带只能提示，绝不下载/暂存"
        );
        // 已提示过同一版本 → 不重复提示（仍不下载）。
        let d2 = decide_cycle(&c, None, Some("1.14.0"));
        assert_eq!(
            d2.action,
            CycleAction::CrossBand {
                latest: "1.14.0".into(),
                notify: false
            }
        );
    }

    #[test]
    fn cross_band_with_staged_same_version_still_does_not_apply() {
        // 跨带闸优先于「同版本 staged 直落位」：跨带的 staged 绝不能被自动落位。
        let c = check(true, "1.13.13", "1.14.0", true, "https://x/core.tar.gz");
        let d = decide_cycle(&c, Some("1.14.0"), None);
        assert!(
            matches!(d.action, CycleAction::CrossBand { .. }),
            "跨带时即便有同版本 staged 也不得走落位分支，实得 {:?}",
            d.action
        );
    }

    #[test]
    fn staged_same_version_applies_without_redownload() {
        let d = decide_cycle(
            &check(true, "1.13.13", "1.13.14", false, "https://x/core.tar.gz"),
            Some("1.13.14"),
            None,
        );
        assert_eq!(d.action, CycleAction::ApplyStaged);
    }

    #[test]
    fn no_update_or_missing_url_is_idle() {
        // 无 latest。
        assert_eq!(
            decide_cycle(&json!({ "hasUpdate": false }), None, None).action,
            CycleAction::Idle
        );
        // 带内但已是最新。
        assert_eq!(
            decide_cycle(
                &check(false, "1.13.14", "1.13.14", false, "https://x/c"),
                None,
                None
            )
            .action,
            CycleAction::Idle
        );
        // 有更新但缺下载地址（后端契约破损）→ Idle，**不**编一个空 URL 去下载。
        assert_eq!(
            decide_cycle(&check(true, "1.13.13", "1.13.14", false, ""), None, None).action,
            CycleAction::Idle
        );
    }

    #[test]
    fn stale_cross_band_notice_is_cleared_once_caught_up() {
        // 当前核已升到 1.14.0（≥ 提示的 1.14.0）→ 清提示。
        let d = decide_cycle(
            &check(false, "1.14.0", "1.14.0", false, ""),
            None,
            Some("1.14.0"),
        );
        assert!(d.clear_cross_band_notice, "已追上 → 清跨带提示");
        // 同带（1.14.3 vs 提示的 1.14.0）也算消化。
        let d2 = decide_cycle(
            &check(false, "1.14.3", "1.14.3", false, ""),
            None,
            Some("1.14.0"),
        );
        assert!(d2.clear_cross_band_notice);
        // 仍落后 → 不清。
        let d3 = decide_cycle(
            &check(true, "1.13.13", "1.14.0", true, "https://x/c"),
            None,
            Some("1.14.0"),
        );
        assert!(!d3.clear_cross_band_notice);
    }

    // ── 事件 payload ─────────────────────────────────────────────────────────
    #[test]
    fn auto_status_payload_matches_frontend_contract() {
        let mut st = UpdateStateFile::default();
        assert_eq!(
            build_auto_status_payload(&st, None),
            json!({ "lastCheckAt": null, "staged": null, "crossBandLatest": null })
        );
        st.last_check_at = Some(1_700_000_000_000);
        st.staged = Some(StagedRecord {
            version: "1.13.14".into(),
            dir: "/tmp/core-staged".into(),
            staged_at: "2026-07-28T00:00:00.000Z".into(),
        });
        st.cross_band_notified_version = Some("1.14.0".into());
        let p = build_auto_status_payload(&st, None);
        assert_eq!(p["lastCheckAt"], json!(1_700_000_000_000u64));
        assert_eq!(p["staged"]["version"], json!("1.13.14"));
        assert_eq!(p["staged"]["stagedAt"], json!("2026-07-28T00:00:00.000Z"));
        assert_eq!(p["crossBandLatest"], json!("1.14.0"));
        // payload **不得**含 autoUpdateEnabled（= 上游 M3：占位会覆盖渲染端从快照拿到的真值）。
        assert!(p.get("autoUpdateEnabled").is_none());
        // staged 只投 version/stagedAt（dir 是本地路径，无需外泄给渲染端）。
        assert!(p["staged"].get("dir").is_none());
    }

    #[test]
    fn auto_status_payload_override_clears_or_sets_cross_band() {
        let st = UpdateStateFile {
            cross_band_notified_version: Some("1.14.0".into()),
            ..UpdateStateFile::default()
        };
        assert_eq!(
            build_auto_status_payload(&st, Some(None))["crossBandLatest"],
            json!(null),
            "显式清除须立刻反映到事件里（否则 UI 旧提示常驻）"
        );
        assert_eq!(
            build_auto_status_payload(&st, Some(Some("1.15.0")))["crossBandLatest"],
            json!("1.15.0")
        );
    }

    // ── staged 生产端适配器 ──────────────────────────────────────────────────
    #[test]
    fn prepared_bytes_source_refuses_foreign_urls() {
        // 结构性防线：这个 downloader 只交付「已解归档的裸核字节」，绝不能被当成通用下载器
        // 去「下载」一个真 URL（那会把归档字节原样暂存成 sing-box）。
        let src = PreparedCoreBytes(b"raw-core".to_vec());
        assert_eq!(src.download(PREPARED_BYTES_URL).unwrap(), b"raw-core");
        let err = src
            .download("https://github.com/SagerNet/sing-box/releases/download/v1/x.tar.gz")
            .unwrap_err();
        assert!(matches!(err, DownloadError::Other(_)));
    }

    /// 🟡 **调用点守卫：总开关必须早退在 `core_update_check` 之前。**
    ///
    /// 前身 `disabled_switch_short_circuits_before_any_network` **名不副实**：它只断言了两个纯函数
    /// （`auto_update_core_enabled(false) == false` 与 `decide_cycle` 不看总开关），
    /// 而「闸 1 早退于任何网络请求之前」这条**接线顺序**零锁 —— 把 enabled 判定挪到
    /// `core_update_check` 之后（= 关着开关也照打 GitHub）时，那个测试全绿。
    ///
    /// 现在按本仓 guard_scan 模式锁 `run_cycle` 体内的实际出现顺序。
    /// **变异探针**：把 `if !auto_update_core_enabled(&config) { return false; }` 挪到
    /// `core_update_check` 调用之后（或整段删掉）⇒ 本条转红。
    #[test]
    fn disabled_switch_short_circuits_before_any_network() {
        let body = method_body(SRC, "async fn run_cycle(");
        let gate_at = body
            .find("auto_update_core_enabled(&config)")
            .expect("总开关闸被删了 —— 关着开关也会打 GitHub 并静默换核");
        let network_at = body
            .find("core_update_check(")
            .expect("锚点消失：守卫已失去判据（run_cycle 不再调 core_update_check？）");
        assert!(
            gate_at < network_at,
            "总开关判定必须出现在 core_update_check **之前** —— 否则「关掉自动更新」这句话是假的：\
             每 6h tick 照样向 GitHub 发一次请求"
        );
        // fork 硬闸同理：零网络早退（第三方核绝不被官方核覆盖）。
        let fork_at = body
            .find("CoreBuildKind::Fork")
            .expect("fork 硬闸被删了 —— 官方 release 会覆盖用户明确选择的特性分支核");
        assert!(fork_at < network_at, "fork 闸也必须前置于网络请求");

        // 判定本体（纯函数）的真值另有 `auto_update_core_defaults_to_off_and_needs_explicit_true` 锁；
        // 这里顺带确认职责分离：决策层自己不看总开关。
        let d = decide_cycle(
            &check(true, "1.13.13", "1.13.14", false, "https://x/c"),
            None,
            None,
        );
        assert!(
            matches!(d.action, CycleAction::Download { .. }),
            "决策层本身不看总开关（职责分离）；总开关的牙齿在 run_cycle 的早退"
        );
    }

    /// 🟡 **变异锁：`running` 闩在 panic 后必须已释放。**
    ///
    /// 不释放 ⇒ [`UpdateScheduler::should_check`] 永假 ⇒ 内核自动更新静默死亡到进程重启，
    /// 且零自曝。**变异探针**：把 [`RunGuard`] 的 `Drop` 实现删掉 / 让它在 `finished` 为 false 时
    /// 也直接 return ⇒ 本条转红。
    #[test]
    fn run_guard_releases_running_even_on_panic() {
        let cfg = ScheduleConfig {
            startup_delay: Duration::ZERO, // 让 should_check 只受 running 影响
            ..ScheduleConfig::default()
        };
        let inner = Mutex::new(UpdateScheduler::new(cfg, SystemClock));
        {
            let mut s = inner.lock().unwrap();
            s.start();
            s.mark_running();
            assert!(!s.should_check(), "前提：running 置位时 should_check 为假");
        }

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = RunGuard::new(&inner);
            panic!("run_cycle 链上的 panic");
        }));
        assert!(panicked.is_err(), "前提：闭包确实 panic 了");

        assert!(
            inner.lock().unwrap().should_check(),
            "panic 后 running 闩仍占着 —— 自动更新会静默死亡到下次重启"
        );
    }

    /// 正常收尾：`finish(true)` 刷 `last_check` 并释放闩；`finish(false)` 只释放不刷
    /// （失败轮保留旧值让 6h tick 下轮重试，而不是把整 24h due 推迟）。
    #[test]
    fn run_guard_finish_refreshes_last_check_only_on_success() {
        let cfg = ScheduleConfig {
            startup_delay: Duration::ZERO,
            ..ScheduleConfig::default()
        };
        let inner = Mutex::new(UpdateScheduler::new(cfg, SystemClock));
        inner.lock().unwrap().start();

        // 失败轮：last_check 不动（仍是 0 = 从未检查）。
        inner.lock().unwrap().mark_running();
        assert_eq!(RunGuard::new(&inner).finish(false), 0);
        assert!(inner.lock().unwrap().should_check(), "失败轮也必须释放闩");

        // 成功轮：last_check 被刷成「刚刚」→ 24h due 闸随即生效。
        inner.lock().unwrap().mark_running();
        let last = RunGuard::new(&inner).finish(true);
        assert!(last > 0, "成功检查必须刷新 lastCheckAt，实得 {last}");
        assert!(
            !inner.lock().unwrap().should_check(),
            "刚成功检查过 → 24h due 闸必须挡住下一轮"
        );
    }

    /// 🟡 **调用点守卫：`cycle_if_due` 必须把 `RunGuard` 持在 `run_cycle` 两侧。**
    ///
    /// **变异探针**：删掉 `RunGuard::new(` / 把它挪到 `self.run_cycle(` 之后 / 在方法体里
    /// 直接手写 `mark_done`（绕过 guard）⇒ 逐条转红。
    #[test]
    fn cycle_if_due_holds_a_run_guard_across_the_cycle() {
        let body = method_body(SRC, "async fn cycle_if_due(");
        let guard_at = body
            .find("RunGuard::new(")
            .expect("running 闩的 RAII 守卫被删了 —— panic 一次自动更新就静默死亡");
        let cycle_at = body
            .find("self.run_cycle(")
            .expect("锚点消失：守卫已失去判据");
        assert!(
            guard_at < cycle_at,
            "RunGuard 必须在 run_cycle **之前**建立 —— 建在后面就覆盖不到 run_cycle 的 panic"
        );
        assert!(
            !body.contains("mark_done("),
            "mark_done 必须只在 RunGuard 内调用 —— 方法体里手写一份就又有了「panic 时跳过」的路径"
        );
    }

    /// 🟡 **调用点守卫：自动落位必须走「绝不断流」的那个入口。**
    ///
    /// 走 `core_update_apply_staged`（手动入口）⇒ 命令层的 `swap_core_with_restart` 会在
    /// `was_running == true` 时照常 stop→swap→restart，把「先判后停」之间几百 ms 的 TOCTOU
    /// 变成一次用户从未同意的断流。**变异探针**：把 `_auto` 后缀去掉 ⇒ 本条转红。
    #[test]
    fn apply_staged_auto_uses_the_no_interrupt_entry_point() {
        let body = method_body(SRC, "async fn apply_staged_auto(");
        assert!(
            body.contains("core_update_apply_staged_auto("),
            "自动落位腿必须调 `core_update_apply_staged_auto`（SwapInterrupt::Forbidden），\
             不能调用户手动入口 —— 那条会在代理运行中停核"
        );
    }

    /// 🟡 **调用点守卫：暂存成功后必须记下裸核 sha256（供落位前复核）。**
    ///
    /// **变异探针**：删掉 `staged_core_sha_path` 那段写入 ⇒ 本条转红；
    /// 把它挪到 `stage(...)` 之前 ⇒ 也转红（`stage` 会 `remove_dir_all` 暂存目录，先写必被删掉）。
    #[test]
    fn stage_records_the_bare_core_digest_after_staging() {
        let body =
            crate::commands::guard_scan::top_level_fn_body(SRC, "async fn run_download_and_stage(");
        let stage_at = body.find(".stage(").expect("锚点消失：守卫已失去判据");
        let sha_at = body
            .find("staged_core_sha_path(")
            .expect("暂存核摘要没记 —— 落位时没有任何基准可对，位腐/篡改的核会被原样换入");
        assert!(
            stage_at < sha_at,
            "摘要必须写在 stage() **之后** —— stage 会先 remove_dir_all 暂存目录，写在前面必被删掉"
        );
        assert!(
            body.contains("ApplyOutcome::Applied"),
            "只有真暂存成功（Applied）才该写摘要：Discarded/Deferred 时目录里没有本次的核"
        );
    }

    #[test]
    fn scheduler_constants_match_upstream() {
        // 上游 CoreUpdateScheduler.ts:26-29 的四个常量（本调度器直接取 ScheduleConfig::default）。
        let c = ScheduleConfig::default();
        assert_eq!(c.startup_delay, Duration::from_secs(30));
        assert_eq!(c.tick_interval, Duration::from_secs(6 * 60 * 60));
        assert_eq!(c.check_interval, Duration::from_secs(24 * 60 * 60));
        assert_eq!(c.stopped_apply_delay, Duration::from_secs(5));
        // 错峰：30s 启动延迟不得与其它任何启动腿撞点（2/3/5/6/7s + 订阅 8s + 规则资源 12s）。
        for other in [2u64, 3, 5, 6, 7, 8, 12] {
            assert_ne!(
                c.startup_delay.as_secs(),
                other,
                "内核更新启动腿与其它启动腿撞在 {other}s"
            );
        }
    }

    #[test]
    fn busy_guard_releases_on_drop() {
        let flag = AtomicBool::new(false);
        {
            assert!(!flag.swap(true, Ordering::SeqCst));
            let _g = BusyGuard(&flag);
            assert!(flag.load(Ordering::SeqCst));
        }
        assert!(!flag.load(Ordering::SeqCst), "guard drop 必须复位 busy 闸");
    }

    #[test]
    fn current_iso_is_parseable_rfc3339() {
        let iso = current_iso();
        assert!(!iso.is_empty(), "stagedAt 不得为空串");
        // 与订阅调度器的解析器互逆（同一 civil 算法）。
        assert!(
            crate::runtime::subscription_scheduler::rfc3339_to_epoch_ms(&iso).is_some(),
            "stagedAt 必须是可解析的 RFC3339，实得 {iso}"
        );
    }
}
