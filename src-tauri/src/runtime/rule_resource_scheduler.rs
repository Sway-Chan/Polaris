//! 规则资源自动更新调度器（上游 `src/main/services/RuleResourceScheduler.ts` 1:1 移植）。
//!
//! **为什么需要它**：sing-box 不会自动重下本地 `rule_set`（`res:` 引用的 `.srs` 是 `type:local`，
//! 无 `update_interval`），本地副本会一直陈旧下去。故由 Polaris 侧周期重下载保鲜。
//! - **启动补更**：启动后 12s（错开 `SubscriptionScheduler` 的 8s 高峰）扫一次陈旧资源。
//! - **周期巡检**：每 30 分钟一轮。
//! - **资源库目录（catalog）同轮刷新**：每轮先按同一道开关 / 同一个间隔节流刷一次外置清单，
//!   失败静默（见 [`RuleResourceScheduler::refresh_catalog_if_due`]）。**不新起定时器** ——
//!   它挂在既有的 12s / 30min 两条腿上，故不参与启动错峰预算。
//! - **退避**：单资源失败后指数退避（10min→…→上限 6h），不对故障源高频重试。
//! - **静默**：失败仅日志 + 退避，**不发 toast**（后台保鲜不该抢用户注意力，对齐 上游 `silent:true`）。
//! - **无冷启动鸡生蛋**：下载走直连 / gh-proxy（`commands::rules::apply_gh_proxy`，套用户配置的
//!   `ghProxyPrefix`，加速失败自动回退原址），不依赖代理是否运行 → 不需要订阅调度器那套
//!   `pending_proxy_catchup` 挂起机制。本条曾**与代码相反**（当时 `commands/rules.rs` 全仓零
//!   `ghProxyPrefix` 引用，只有直连），同批接线后本注释才成立。
//!
//! **纯决策 / 计时分离**（与 `subscription_scheduler` 同骨架）：陈旧 / 文件缺失 / 退避判定全收在纯函数
//! [`select_due_resources`]（`now` 与「文件在不在」由调用方注入，全单测覆盖，纯函数**不碰真实文件系统**）；
//! 定时器 + 真下载是薄壳，逐个 due 资源调既有命令 [`crate::commands::rule_resources_redownload`]。
//!
//! 退避状态机与 RFC3339 解析直接复用 `subscription_scheduler` 的 [`BackoffTracker`] /
//! [`rfc3339_to_epoch_ms`]——两调度器的退避 / 时间口径必须一致，各写一份必然漂移。

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Manager};

use polaris_config_engine::user_config::builtin_geo_rulesets::{
    builtin_geo_rulesets, builtin_id_for,
};

use crate::runtime::subscription_scheduler::{now_ms, rfc3339_to_epoch_ms, BackoffTracker};
use crate::runtime::AppRuntime;

const TICK_MS: u64 = 30 * 60_000; // 30 分钟巡检（= 上游 TICK_MS）
const STARTUP_DELAY_MS: u64 = 12_000; // 启动延迟，错开订阅调度器的 8s
const BACKOFF_BASE_MS: u64 = 10 * 60_000; // 退避基数 10 分钟
const BACKOFF_MAX_MS: u64 = 6 * 60 * 60_000; // 退避上限 6 小时
const DEFAULT_INTERVAL_HOURS: u64 = 12;

/// 资源库目录缓存文件名 —— **只读镜像** `commands::rules` 的私有常量 `CATALOG_CACHE_FILE`
/// （同在 `<userData>/rule-resource/`）。
///
/// 这里只 peek `fetchedAt` 做节流，**写入仍只在 `commands/rules.rs` 一处**（本调度器不碰缓存落盘）。
/// 为什么不改调 `rule_resources_get_catalog` 拿 `fetchedAt`：那个 command 刻意恒返内置精选表
/// （`fetchedAt: null`，理由见其文档「内置 tab 语义」），拿它节流等于每轮都判到期 —— 恰是本节流
/// 要避免的每 30min 白打 GitHub。两处文件名漂移由单测 `catalog_cache_file_name_mirrors_rules_rs` 守。
const CATALOG_CACHE_FILE: &str = "catalog.json";

/// 纯决策：本轮自动更新的间隔（ms）；`None` = **整条自动更新腿本轮都不跑**。
///
/// 两条执行腿（资源重下载 / 资源库目录刷新）共用这一道门与这一个间隔 —— 对齐 上游
/// `RuleResourceScheduler.ts:103`（总开关早退在两条腿之前）+ `:108/:116`（两处同一个 `intervalMs`）。
/// 各写一份必然漂移出「关了总开关目录还在刷」这类分叉。
///
/// - **总开关**：`ruleResourceAutoUpdate === false` 才停；**缺省（老配置 undefined）视为开启**
///   （逐字对齐 上游 `if (config.ruleResourceAutoUpdate === false) return`）。
/// - **间隔**：`ruleResourceUpdateIntervalHours`，`> 0` 才用，缺省 / 非数 → 回落 12h。
/// - **`interval == 0` → `None`**（#18 的 0 语义）：0 是 UI 下拉的「仅手动」档。本调度器的两条腿
///   都动网，故「仅手动」就是彻底不自动动网——包括文件缺失的强制补更、以及目录刷新；缺文件的
///   资源仍可在资源页手动「重新下载」补回，目录也仍可手点「刷新」。**这是与 上游的刻意分叉**
///   （上游的 `intervalMs()` 把 0 折成 12h，因为它那边 0 没有「仅手动」语义）。
#[must_use]
fn auto_update_interval_ms(config: &Value) -> Option<u64> {
    if config
        .get("ruleResourceAutoUpdate")
        .and_then(Value::as_bool)
        == Some(false)
    {
        return None; // 仅显式关闭才停（老配置 undefined → 开）
    }
    let interval_hours = config
        .get("ruleResourceUpdateIntervalHours")
        .and_then(Value::as_u64);
    if interval_hours == Some(0) {
        return None; // 「仅手动」
    }
    Some(
        interval_hours
            .filter(|h| *h > 0)
            .unwrap_or(DEFAULT_INTERVAL_HOURS)
            * 3_600_000,
    )
}

/// 纯决策：本轮该不该刷新**资源库目录**（catalog，外置全量清单）。
///
/// 对齐 上游 `RuleResourceScheduler.ts:110-120`：节流基准取「上次**成功**拉取（缓存里的
/// `fetchedAt`）与上次**尝试**（进程内 `last_catalog_refresh_attempt`）的较晚者」。
/// 两者缺一不可：
///  - 只看 `fetchedAt`：远程一直拉不到时它恒为 0（Polaris 侧是「无缓存」），每 tick 都判到期 →
///    离线 / 被限流时每 30min 白打一次 GitHub 三跳。
///  - 只看进程内 `last_attempt`：它随进程重启清零 → 每次开应用都必刷一次，缓存等于白存。
///
/// `cached_fetched_at_ms` / `last_attempt_ms` 取 0 表示「没有该记录」（`0.max(0) = 0` →
/// `now - 0 >= interval` 恒真 → 首次立即刷，与 上游的 `?? 0` 同义）。
#[must_use]
pub fn catalog_refresh_due(
    config: &Value,
    now_ms: u64,
    cached_fetched_at_ms: u64,
    last_attempt_ms: u64,
) -> bool {
    let Some(interval_ms) = auto_update_interval_ms(config) else {
        return false;
    };
    let last = cached_fetched_at_ms.max(last_attempt_ms);
    now_ms.saturating_sub(last) >= interval_ms
}

/// peek 磁盘 catalog 缓存的 `fetchedAt`（epoch ms）——**只读**，任何一环不过即 0（= 没这条记录，
/// 由 [`catalog_refresh_due`] 判成立即到期）。不复刻 `commands::rules` 那套逐条自洽校验：本函数
/// 只为节流取一个时间戳，缓存内容合不合法由那边的读取腿把关（它才是消费者）。
fn cached_catalog_fetched_at(res_dir: &Path) -> u64 {
    std::fs::read(res_dir.join(CATALOG_CACHE_FILE))
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|v| v.get("fetchedAt").and_then(Value::as_u64))
        .unwrap_or(0)
}

/// 纯决策：从 config + now + 退避状态 + 「文件在不在」选出本轮该重下载的资源 id（声明序）。
///
/// - **总开关 / 间隔 / 「仅手动」** → 见 [`auto_update_interval_ms`]（与目录刷新腿共用）。
/// - **陈旧判据**（对齐 上游）：从未记录 `downloadedAt` / 距上次 ≥ interval / **磁盘文件缺失**。
///   文件缺失是**强制**补更（即便时间上不陈旧）——备份恢复或手删后下一轮自动补回，否则被引用的
///   `rule_set` 文件不在会让内核起不来。
/// - 退避未到 → 跳过。
///
/// `file_exists` 收的是资源的 `fileName`（相对规则资源目录），由薄壳注入真实目录拼接。
#[must_use]
pub fn select_due_resources(
    config: &Value,
    now_ms: u64,
    backoff: &BackoffTracker,
    file_exists: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(interval_ms) = auto_update_interval_ms(config) else {
        return out;
    };
    let Some(resources) = config.get("ruleResources").and_then(Value::as_array) else {
        return out;
    };

    for res in resources {
        let Some(id) = res.get("id").and_then(Value::as_str) else {
            continue;
        };
        let last = res
            .get("downloadedAt")
            .and_then(Value::as_str)
            .and_then(rfc3339_to_epoch_ms)
            .unwrap_or(0);
        // fileName 缺失 = 条目结构损坏，无从判断文件在不在 → 按「缺失」处理，让重下载去如实报错
        // （命令层对损坏条目返 BAD_ITEM），总比静默跳过永远不修好。
        let missing = !res
            .get("fileName")
            .and_then(Value::as_str)
            .is_some_and(file_exists);
        if !missing && last != 0 && now_ms.saturating_sub(last) < interval_ms {
            continue; // 文件在 + 有记录 + 未超间隔 → 不陈旧
        }
        if !backoff.is_eligible(id, now_ms) {
            continue;
        }
        out.push(id.to_string());
    }
    out
}

/// 规则资源自动更新调度器（含退避 + 防重入）。
pub struct RuleResourceScheduler {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    backoff: BackoffTracker,
    is_running: bool,
    started: bool,
    /// 资源库目录刷新的**上次尝试**时刻（epoch ms，0 = 本进程内尚未尝试过）。
    /// 与磁盘 `fetchedAt` 的分工见 [`catalog_refresh_due`]：失败也算一次尝试，间隔内不重试。
    last_catalog_refresh_attempt: u64,
}

impl Default for RuleResourceScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl RuleResourceScheduler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                backoff: BackoffTracker::new(BACKOFF_BASE_MS, BACKOFF_MAX_MS),
                is_running: false,
                started: false,
                last_catalog_refresh_attempt: 0,
            })),
        }
    }

    /// 启动：装 12s 启动补更 + 30min 周期巡检。幂等（重复调用 no-op）。
    pub fn start(self: &Arc<Self>, app: AppHandle) {
        {
            let mut inner = self.inner.lock().expect("rule-res scheduler lock");
            if inner.started {
                return;
            }
            inner.started = true;
        }

        let this = self.clone();
        let app_startup = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(STARTUP_DELAY_MS)).await;
            this.run_due_updates(&app_startup, "启动补更").await;
        });

        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(TICK_MS));
            interval.tick().await; // 立即触发的首 tick 跳过（启动补更已覆盖）
            loop {
                interval.tick().await;
                this.run_due_updates(&app, "周期更新").await;
            }
        });
    }

    /// 一轮到期更新：防重入 → 选到期 → 逐个调既有 redownload 命令 → 记退避 + 汇总日志。
    async fn run_due_updates(self: &Arc<Self>, app: &AppHandle, reason: &str) {
        {
            let mut inner = self.inner.lock().expect("rule-res scheduler lock");
            if inner.is_running {
                return;
            }
            inner.is_running = true;
        }
        // 中途 return / panic 都要清 is_running。
        let _guard = RunningGuard {
            inner: self.inner.clone(),
        };

        let (config, res_dir) = {
            let state = app.state::<AppRuntime>();
            let Ok(config) = state.config().load_full() else {
                return;
            };
            // 与 `commands::rules::rule_resources_redownload` 同一落盘目录（单一真值源在那儿）。
            (config, state.config().dir().join("rule-resource"))
        };
        let now = now_ms();

        // 资源库目录（catalog）刷新腿 —— **独立语句、返回 `()`**：这是「失败不打断资源重下载腿」的
        // 编译期保证（= 上游 那圈 try/catch 的作用），把它的结果拿去 `?` / `return` 就退回原状。
        self.refresh_catalog_if_due(app, &config, now, &res_dir, reason)
            .await;

        let due = {
            let mut inner = self.inner.lock().expect("rule-res scheduler lock");
            let active: HashSet<String> = config
                .get("ruleResources")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            inner.backoff.prune(&active); // 资源被删后剪退避键，防内存无界增长
            select_due_resources(&config, now, &inner.backoff, &|f| res_dir.join(f).exists())
        };
        if due.is_empty() {
            return;
        }

        let (mut ok_count, mut failures) = (0usize, Vec::new());
        for id in due {
            // 走既有下载核心而非直接调下载函数：命令层已收口「条目解析 / 落盘 / persist + 广播」，
            // 绕过它就得在这里复刻一份必然漂移的副本。
            //
            // **静默腿**（对齐 上游 `RuleResourceScheduler` 的 `updateMany(ids, { silent: true })`）：
            // 走 `rule_resources_redownload_silent` 而非 command 本体——后台保鲜在用户毫不知情时
            // 往资源页推 `EVENT_RULE_RESOURCE_PROGRESS`，表现为「没点更新，行却自己转起圈/变红」。
            // 静默由函数内部写死（无 bool 形参可传错，见该函数文档）。
            let state = app.state::<AppRuntime>();
            let resp =
                crate::commands::rules::rule_resources_redownload_silent(app, &state, id.clone())
                    .await;
            let data = resp.ok().and_then(|r| r.data).unwrap_or(Value::Null);
            let ok = data.get("ok").and_then(Value::as_bool) == Some(true);
            let mut inner = self.inner.lock().expect("rule-res scheduler lock");
            if ok {
                ok_count += 1;
                inner.backoff.record_success(&id);
            } else {
                // 失败明细带 errorCode（对齐 上游 formatRuleUpdateSummary）：只记「失败 N」时
                // 排查无从区分 timeout / http 4xx / invalid_content。
                let code = data
                    .get("errorCode")
                    .and_then(Value::as_str)
                    .or_else(|| data.get("error").and_then(Value::as_str))
                    .unwrap_or("unknown");
                failures.push(format!("{id}: {code}"));
                inner.backoff.record_failure(&id, now);
            }
        }
        // ── 随包内置 geo 也纳入自动更新射程 ──
        //
        // 此前只遍历 `config.ruleResources`，而内置 geo **从不入册** ⇒ 随包那 28 个 `.srs`
        // 一旦出厂就永不更新，用户看到的分流数据可能落后好几个月
        // （陈先生 2026-07-30：「本地 srs 应该跟随更新」）。
        // 它们的更新腿是本批之前才补上的 `rule_resources_update_builtin`，缺腿才是当初漏掉的真因。
        //
        // 与已登记资源共用同一个总开关和同一个间隔（`auto_update_interval_ms`）——
        // 「我关了自动更新」必须对两类都成立。退避表也共用，键用 `builtin:<tag>`（= 它的资源 id，
        // 与前端列表里那一行同名），故不会与已登记资源的 id 撞键。
        let builtin_ok = self.run_builtin_geo(app, &config, now, reason).await;

        // ── 整批收尾：广播一次（而不是批内每条各广播一次）──
        //
        // `broadcast_config_changed` 不只是 emit 给渲染端，它同时 `spawn(switch_mode)` 把变更送进
        // **运行中的核**。批内逐条广播时，一轮启动补更 = 8 条已登记 + 25 个内置 geo = 33 次
        // `switch_mode`（真机 2026-08-02：11 秒内 35 条 `switchMode：核未运行 → 仅更新配置`）。
        // 核未跑时只是刷屏，核在跑时是**连砸 33 次热切/去抖重启判定** —— 而这一轮语义上就是一批，
        // 批内中间态没有任何消费者需要看见。故两条静默腿传 `BroadcastMode::Deferred`（只落盘），
        // 收口在此处广播一次。
        //
        // **一条都没成功就不广播**：配置没变，广播等于凭空给核一次无谓的 switch_mode 判定。
        if ok_count > 0 || builtin_ok > 0 {
            match app.state::<AppRuntime>().config().current() {
                Ok(latest) => crate::commands::config::broadcast_config_changed(app, &latest),
                Err(e) => log::warn!("[{reason}] 整批更新已落盘，但读回配置广播失败: {e}"),
            }
        }

        if !failures.is_empty() {
            log::warn!(
                "[{reason}] 规则资源自动更新：成功 {ok_count}，失败 {}（{}）",
                failures.len(),
                failures.join("；")
            );
        } else {
            log::info!("[{reason}] 规则资源自动更新：成功 {ok_count}，失败 0");
        }
    }

    /// 内置 geo 的自动更新腿（与已登记资源同开关、同间隔、同退避表）。
    ///
    /// 「到期」判据取 `config.builtinGeoMeta[tag].updatedAt`：
    /// - 缺失（= 出厂态，从未联网更新过）→ **立即到期**，让随包数据尽快追上上游；
    /// - 有值 → 距今超过间隔才到期。
    ///
    /// 落位安全性由 `rule_resources_update_builtin` 自己保证（下到 `.update/` 暂存 + 原子 rename），
    /// 故一次失败绝不会破坏正在生效的那份副本。
    ///
    /// 返回**本轮成功更新的条数** —— 调用方据此决定收尾要不要广播一次配置变更
    /// （见 `run_due_updates` 的整批收尾段；一条都没成功就不该白给核一次 `switch_mode`）。
    async fn run_builtin_geo(
        &self,
        app: &tauri::AppHandle,
        config: &Value,
        now: u64,
        reason: &str,
    ) -> usize {
        let Some(interval_ms) = auto_update_interval_ms(config) else {
            return 0; // 总开关关 / 间隔为「仅手动」→ 整条腿不跑（与已登记资源同口径）
        };
        let meta = config.get("builtinGeoMeta").cloned().unwrap_or(Value::Null);
        let due: Vec<String> = {
            let inner = self.inner.lock().expect("rule-res scheduler lock");
            builtin_geo_rulesets()
                .into_iter()
                .filter(|b| {
                    if !inner.backoff.is_eligible(&builtin_id_for(&b.tag), now) {
                        return false;
                    }
                    let updated_at = meta
                        .get(&b.tag)
                        .and_then(|v| v.get("updatedAt"))
                        .and_then(Value::as_str);
                    match updated_at.and_then(rfc3339_to_epoch_ms) {
                        // 从未联网更新过 → 立即到期。
                        None => true,
                        Some(t) => now.saturating_sub(t) >= interval_ms,
                    }
                })
                .map(|b| b.tag)
                .collect()
        };
        if due.is_empty() {
            return 0;
        }
        let (mut ok_count, mut failures) = (0usize, Vec::new());
        for tag in due {
            let state = app.state::<AppRuntime>();
            let resp = crate::commands::rules::rule_resources_update_builtin_silent(
                app,
                &state,
                tag.clone(),
            )
            .await;
            let data = resp.ok().and_then(|r| r.data).unwrap_or(Value::Null);
            let ok = data.get("ok").and_then(Value::as_bool) == Some(true);
            let key = builtin_id_for(&tag);
            let mut inner = self.inner.lock().expect("rule-res scheduler lock");
            if ok {
                ok_count += 1;
                inner.backoff.record_success(&key);
            } else {
                let code = data
                    .get("errorCode")
                    .and_then(Value::as_str)
                    .or_else(|| data.get("error").and_then(Value::as_str))
                    .unwrap_or("unknown");
                failures.push(format!("{tag}: {code}"));
                inner.backoff.record_failure(&key, now);
            }
        }
        if failures.is_empty() {
            log::info!("[{reason}] 内置 geo 自动更新：成功 {ok_count}，失败 0");
        } else {
            log::warn!(
                "[{reason}] 内置 geo 自动更新：成功 {ok_count}，失败 {}（{}）",
                failures.len(),
                failures.join("；")
            );
        }
        ok_count
    }

    /// 资源库目录（catalog）随自动更新一并刷新 —— 移植 上游
    /// `src/main/services/RuleResourceScheduler.ts:110-123`。
    ///
    /// **为什么必须有这条腿**：资源页「外置」tab 的全量清单只有手点「刷新」才会更新，缺了它用户
    /// 拿到的永远是首次刷新（或从未刷新 → 33 条内置精选）那一份，新上游资源永不出现。
    ///
    /// 三条语义逐条对齐 上游：
    ///  1. **绑同一个总开关 + 同一个间隔**（[`auto_update_interval_ms`]）——关掉自动更新就该连目录
    ///     一起停，否则「我关了自动更新」这句话是假的。
    ///  2. **按间隔节流**（[`catalog_refresh_due`]）：**先记尝试再发请求**，故失败同样消耗本轮配额，
    ///     离线 / 被限流时不会每 30min 重打（上游 `:117` 的 `lastCatalogRefreshAttempt = now` 亦在
    ///     `await refreshCatalog()` 之前）。
    ///  3. **失败静默**：`rule_resources_refresh_catalog` 契约上不 Err（远程失败按 缓存→内置 梯子
    ///     降级并如实标 `source`），故这里靠 `source` 分辨真假刷新：只有 `remote` 才是真拉到了，
    ///     其余落 debug 日志、**不发 toast / 不发事件**（后台保鲜不抢用户注意力，同重下载的静默腿）。
    ///
    /// 返回 `()` 是刻意的：调用方无从短路 —— 目录刷新失败绝不能拖累 `.srs` 重下载腿。
    ///
    /// # 读盘在锁外
    ///
    /// [`cached_catalog_fetched_at`] 是**同步**的读盘 + JSON parse。原实现把它写在
    /// `catalog_refresh_due(...)` 的实参位置上，于是整个 read + parse 都发生在持 `inner` 互斥锁
    /// 期间、且是在 async fn 里 —— 目录缓存文件几百 KB 且落在用户配置目录（可能是网络盘 / 正被
    /// 备份软件锁住），那段时间里 `run_due_updates` 的防重入判定、退避记账全部排队等它。
    /// 锁内只留「判定 + 记 attempt」这两步纯内存操作。顺序由单测
    /// `catalog_refresh_reads_disk_before_taking_the_lock` 锁死。
    async fn refresh_catalog_if_due(
        self: &Arc<Self>,
        app: &AppHandle,
        config: &Value,
        now: u64,
        res_dir: &Path,
        reason: &str,
    ) {
        // 总开关关 / 「仅手动」→ 连盘都不必读（与 `catalog_refresh_due` 的第一道判据同一个函数，
        // 故这条早退与它**恒等价**，只是把读盘省掉）。
        if auto_update_interval_ms(config).is_none() {
            return;
        }
        // ── 锁外读盘（见方法文档「读盘在锁外」）。
        let cached_fetched_at = cached_catalog_fetched_at(res_dir);
        {
            let mut inner = self.inner.lock().expect("rule-res scheduler lock");
            if !catalog_refresh_due(
                config,
                now,
                cached_fetched_at,
                inner.last_catalog_refresh_attempt,
            ) {
                return;
            }
            // 先记尝试：下面这次请求无论成败都消耗本轮配额（见方法文档第 2 条）。
            inner.last_catalog_refresh_attempt = now;
        }

        // 复用刷新命令本体（`refresh_catalog_core` 的薄壳）：远程三跳 → `<50` 条闸 → 原子落缓存 →
        // 失败按 缓存→内置 梯子降级，整条口径只此一份。绕过它自己拼一份必然与手动刷新腿漂移。
        let state = app.state::<AppRuntime>();
        let source = crate::commands::rules::rule_resources_refresh_catalog(state)
            .await
            .ok()
            .and_then(|r| r.data)
            .map_or_else(|| "unknown".to_string(), |c| c.source);
        if source == "remote" {
            log::info!("[{reason}] 资源库目录已刷新");
        } else {
            // 拉不到远端不是错误态（清单仍可用，只是不是最新）→ debug，不打扰。
            log::debug!("[{reason}] 资源库目录刷新未拉到远端，沿用 {source} 清单");
        }
    }
}

/// 清 is_running 的 RAII 守卫（中途 return/panic 均复位）。
struct RunningGuard {
    inner: Arc<Mutex<Inner>>,
}
impl Drop for RunningGuard {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.is_running = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_700_000_000_000; // 2023-11-14
    const HOUR: u64 = 3_600_000;

    /// 全部文件都在（默认注入）。
    fn all_present(_: &str) -> bool {
        true
    }
    /// 全部文件都缺。
    fn all_missing(_: &str) -> bool {
        false
    }

    fn iso(ms: u64) -> String {
        polaris_stats_engine::created_at_to_rfc3339(ms as i64).unwrap()
    }

    fn cfg_with(resources: Value) -> Value {
        json!({ "ruleResources": resources })
    }

    fn fresh(id: &str) -> Value {
        json!({ "id": id, "fileName": format!("{id}.srs"), "downloadedAt": iso(NOW - HOUR) })
    }
    fn stale(id: &str) -> Value {
        json!({ "id": id, "fileName": format!("{id}.srs"), "downloadedAt": iso(NOW - 24 * HOUR) })
    }

    fn tracker() -> BackoffTracker {
        BackoffTracker::new(BACKOFF_BASE_MS, BACKOFF_MAX_MS)
    }

    #[test]
    fn master_switch_only_stops_when_explicitly_false() {
        let mut cfg = cfg_with(json!([stale("a")]));
        cfg["ruleResourceAutoUpdate"] = json!(false);
        assert!(
            select_due_resources(&cfg, NOW, &tracker(), &all_present).is_empty(),
            "显式 false → 停"
        );
        // 缺省（老配置 undefined）→ 照跑。这是与本仓 UI `!!config.ruleResourceAutoUpdate` 的已知
        // 不一致点，代码按 上游 语义（缺省=开）。
        let cfg_default = cfg_with(json!([stale("a")]));
        assert_eq!(
            select_due_resources(&cfg_default, NOW, &tracker(), &all_present),
            vec!["a".to_string()]
        );
        // 显式 true 同样跑。
        let mut cfg_on = cfg_with(json!([stale("a")]));
        cfg_on["ruleResourceAutoUpdate"] = json!(true);
        assert_eq!(
            select_due_resources(&cfg_on, NOW, &tracker(), &all_present),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn stale_and_fresh_are_split() {
        let cfg = cfg_with(json!([stale("old"), fresh("new")]));
        assert_eq!(
            select_due_resources(&cfg, NOW, &tracker(), &all_present),
            vec!["old".to_string()],
            "仅超间隔的进入本轮"
        );
    }

    #[test]
    fn never_downloaded_is_always_due() {
        // 无 downloadedAt / 空串 → 从未记录 → 立即到期。
        let cfg = cfg_with(json!([
            { "id": "a", "fileName": "a.srs" },
            { "id": "b", "fileName": "b.srs", "downloadedAt": "" },
        ]));
        assert_eq!(
            select_due_resources(&cfg, NOW, &tracker(), &all_present),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn missing_file_forces_update_even_when_fresh() {
        // 1h 前刚下过（远未到 12h），但磁盘文件不在 → 强制补更。
        let cfg = cfg_with(json!([fresh("a")]));
        assert!(
            select_due_resources(&cfg, NOW, &tracker(), &all_present).is_empty(),
            "文件在 + 新鲜 → 不更"
        );
        assert_eq!(
            select_due_resources(&cfg, NOW, &tracker(), &all_missing),
            vec!["a".to_string()],
            "文件缺失 → 即便新鲜也补更"
        );
    }

    #[test]
    fn missing_filename_field_treated_as_missing_file() {
        // fileName 缺失 = 条目损坏 → 按文件缺失处理（进入本轮，由命令层如实报 BAD_ITEM）。
        let cfg = cfg_with(json!([{ "id": "a", "downloadedAt": iso(NOW - HOUR) }]));
        assert_eq!(
            select_due_resources(&cfg, NOW, &tracker(), &all_present),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn backoff_skips_then_recovers() {
        let cfg = cfg_with(json!([stale("a")]));
        let mut b = tracker();
        b.record_failure("a", NOW);
        assert!(
            select_due_resources(&cfg, NOW, &b, &all_present).is_empty(),
            "退避中跳过"
        );
        assert_eq!(
            select_due_resources(&cfg, NOW + BACKOFF_BASE_MS, &b, &all_present),
            vec!["a".to_string()],
            "退避过期 → 恢复"
        );
        // 退避对「文件缺失」同样生效：故障源不因缺文件被高频重试。
        assert!(
            select_due_resources(&cfg, NOW, &b, &all_missing).is_empty(),
            "退避中即便文件缺失也跳过"
        );
    }

    #[test]
    fn interval_falls_back_to_default_on_illegal_values() {
        // 13h 前下过：默认 12h 下算陈旧；若非法值被当成别的数就会判错。
        let res = json!([{ "id": "a", "fileName": "a.srs", "downloadedAt": iso(NOW - 13 * HOUR) }]);
        for bad in [json!(null), json!("12"), json!(-1), json!(1.5)] {
            let mut cfg = cfg_with(res.clone());
            cfg["ruleResourceUpdateIntervalHours"] = bad.clone();
            assert_eq!(
                select_due_resources(&cfg, NOW, &tracker(), &all_present),
                vec!["a".to_string()],
                "非法值 {bad} 应回落 12h"
            );
        }
        // 合法自定义间隔被尊重：24h 间隔下 13h 前的资源不陈旧。
        let mut cfg = cfg_with(res);
        cfg["ruleResourceUpdateIntervalHours"] = json!(24);
        assert!(select_due_resources(&cfg, NOW, &tracker(), &all_present).is_empty());
    }

    #[test]
    fn interval_zero_is_manual_only() {
        // #18 的 0 语义：本调度器只有一条腿 → 0 = 彻底不自动跑（含文件缺失的强制补更）。
        let mut cfg = cfg_with(json!([stale("a")]));
        cfg["ruleResourceUpdateIntervalHours"] = json!(0);
        assert!(select_due_resources(&cfg, NOW, &tracker(), &all_present).is_empty());
        assert!(
            select_due_resources(&cfg, NOW, &tracker(), &all_missing).is_empty(),
            "仅手动优先于文件缺失补更（用户显式要求别动网）"
        );
    }

    #[test]
    fn missing_or_bad_resources_array_is_empty() {
        assert!(select_due_resources(&json!({}), NOW, &tracker(), &all_present).is_empty());
        assert!(select_due_resources(
            &json!({"ruleResources": "x"}),
            NOW,
            &tracker(),
            &all_present
        )
        .is_empty());
        // 无 id 的条目跳过（无退避键可记，也无法调 redownload）。
        let cfg = cfg_with(json!([{ "fileName": "a.srs" }]));
        assert!(select_due_resources(&cfg, NOW, &tracker(), &all_missing).is_empty());
    }

    /* ── 资源库目录（catalog）刷新腿 ─────────────────────────────────────────────────── */

    /// 空配置（总开关缺省=开、间隔缺省 12h）。
    fn cfg_empty() -> Value {
        json!({})
    }

    #[test]
    fn catalog_refresh_due_when_never_fetched() {
        // 从未拉过（缓存无 fetchedAt）+ 本进程未尝试过 → 立即到期（对齐 上游的 `?? 0`）。
        assert!(catalog_refresh_due(&cfg_empty(), NOW, 0, 0));
    }

    #[test]
    fn catalog_refresh_throttled_by_cached_fetched_at() {
        // **节流的第一条腿**：上次成功拉取在间隔内 → 跳过；跨过间隔 → 到期。
        // 删掉 `cached_fetched_at_ms` 这一项（或整个节流）→ 本用例第一条断言转红。
        assert!(
            !catalog_refresh_due(&cfg_empty(), NOW, NOW - 11 * HOUR, 0),
            "11h < 12h 间隔 → 不刷（每 30min 一 tick，不节流就是每 30min 白打一次 GitHub）"
        );
        assert!(catalog_refresh_due(&cfg_empty(), NOW, NOW - 13 * HOUR, 0));
        // 边界：恰好到点即刷（`>=`，与 上游 同）。
        assert!(catalog_refresh_due(&cfg_empty(), NOW, NOW - 12 * HOUR, 0));
    }

    #[test]
    fn catalog_refresh_throttled_by_last_attempt_even_when_never_fetched() {
        // **节流的第二条腿**：远程一直拉不到 ⇒ 缓存 fetchedAt 恒 0，只靠它会每 tick 重拉。
        // 「上次尝试」把失败也算一次配额 → 离线/限流下不再高频重打。
        assert!(
            !catalog_refresh_due(&cfg_empty(), NOW, 0, NOW - HOUR),
            "1h 前刚尝试过（虽然失败了）→ 本轮跳过"
        );
        assert!(
            catalog_refresh_due(&cfg_empty(), NOW, 0, NOW - 13 * HOUR),
            "尝试也过期 → 重试"
        );
    }

    #[test]
    fn catalog_refresh_takes_the_later_of_the_two_marks() {
        // 取较晚者：任一条在间隔内就该跳过（取较早者会让另一条形同虚设）。
        assert!(!catalog_refresh_due(
            &cfg_empty(),
            NOW,
            NOW - 20 * HOUR,
            NOW - HOUR
        ));
        assert!(!catalog_refresh_due(
            &cfg_empty(),
            NOW,
            NOW - HOUR,
            NOW - 20 * HOUR
        ));
    }

    #[test]
    fn catalog_refresh_shares_the_master_switch_and_interval() {
        // 总开关显式 false → 目录也不刷（否则「我关了自动更新」是假话）。
        let mut off = cfg_empty();
        off["ruleResourceAutoUpdate"] = json!(false);
        assert!(!catalog_refresh_due(&off, NOW, 0, 0));
        // 「仅手动」(0) → 彻底不自动动网，目录同样不刷（与 select_due_resources 同一道门）。
        let mut manual = cfg_empty();
        manual["ruleResourceUpdateIntervalHours"] = json!(0);
        assert!(!catalog_refresh_due(&manual, NOW, 0, 0));
        // 自定义间隔被尊重：24h 下 13h 前拉过的不到期（12h 下则到期，见上一用例）。
        let mut long = cfg_empty();
        long["ruleResourceUpdateIntervalHours"] = json!(24);
        assert!(!catalog_refresh_due(&long, NOW, NOW - 13 * HOUR, 0));
        assert!(catalog_refresh_due(&long, NOW, NOW - 25 * HOUR, 0));
    }

    #[test]
    fn cached_catalog_fetched_at_reads_zero_on_any_defect() {
        // 目录不存在 / 文件不是 JSON / 无 fetchedAt / fetchedAt 非正整数 → 一律 0（=立即到期）。
        // 这是「宁可多刷一次，也不因缓存损坏永久停刷」的取向。
        let dir = std::env::temp_dir().join(format!("polaris-cat-{}", now_ms()));
        assert_eq!(cached_catalog_fetched_at(&dir), 0, "目录不存在");
        std::fs::create_dir_all(&dir).unwrap();
        for (body, why) in [
            ("not json", "非 JSON"),
            ("{}", "无 fetchedAt"),
            (r#"{"fetchedAt":"x"}"#, "fetchedAt 非数"),
            (r#"{"fetchedAt":-1}"#, "fetchedAt 负数"),
        ] {
            std::fs::write(dir.join(CATALOG_CACHE_FILE), body).unwrap();
            assert_eq!(cached_catalog_fetched_at(&dir), 0, "{why}");
        }
        std::fs::write(
            dir.join(CATALOG_CACHE_FILE),
            r#"{"fetchedAt":1700000000000}"#,
        )
        .unwrap();
        assert_eq!(cached_catalog_fetched_at(&dir), 1_700_000_000_000);
        std::fs::remove_dir_all(&dir).ok();
    }

    /* ── 接线守卫（变异锁）：纯函数全对、但没人调它 = 用户拿不到刷新 ───────────────── */

    const SRC: &str = include_str!("rule_resource_scheduler.rs");

    /// 取具名函数体（从签名起到下一个同缩进的 `\n    }` 为止，够本文件用）。
    fn fn_body(sig: &str) -> &'static str {
        let start = SRC.find(sig).unwrap_or_else(|| panic!("找不到 {sig}"));
        let rest = &SRC[start..];
        let end = rest.find("\n    }").unwrap_or(rest.len());
        &rest[..end]
    }

    #[test]
    fn scheduler_actually_wires_the_catalog_refresh_leg() {
        // 变异锁：删掉这条腿（或只留纯函数不调）→ 转红。
        let body = fn_body("async fn run_due_updates(");
        assert!(
            body.contains("self.refresh_catalog_if_due("),
            "每轮更新必须带上资源库目录刷新（上游 RuleResourceScheduler.ts:110-123）"
        );
        let leg = fn_body("async fn refresh_catalog_if_due(");
        assert!(
            leg.contains("catalog_refresh_due("),
            "变异锁：删掉节流判定 = 每 30min 白打一次 GitHub 三跳"
        );
        assert!(
            leg.contains("last_catalog_refresh_attempt = now"),
            "变异锁：不记『上次尝试』则失败态每 tick 重打（fetchedAt 在未成功时恒 0）"
        );
        assert!(
            leg.contains("rule_resources_refresh_catalog("),
            "必须复用刷新命令本体，不得另拼一套下载/落缓存"
        );
    }

    /// 🔴 **一轮保鲜只准给核一次配置变更**（真机实证 2026-08-02）。
    ///
    /// 逐条广播时一轮启动补更打出 33 次 `broadcast_config_changed` ⇒ 33 次 `switch_mode`
    /// （日志实测 11 秒内 35 条）。核未跑时只是刷屏，**核在跑时是连砸 33 次热切/去抖重启判定**。
    ///
    /// 守两件事，缺一即回归：
    /// ① 两条后台静默腿必须传 `BroadcastMode::Deferred`（改回 `Immediate` 或删掉参数 → 转红）；
    /// ② 批次收尾必须有且只有一处广播，且**门控在「本轮真有成功」上**（去掉 `ok_count`/`builtin_ok`
    ///    门 → 转红：一条都没更新还广播，等于凭空给核一次无谓的 `switch_mode`）。
    #[test]
    fn one_refresh_round_broadcasts_exactly_once() {
        let rules = include_str!("../commands/rules.rs");
        for silent_fn in [
            "pub async fn rule_resources_redownload_silent(",
            "pub async fn rule_resources_update_builtin_silent(",
        ] {
            let at = rules
                .find(silent_fn)
                .unwrap_or_else(|| panic!("找不到 {silent_fn}"));
            let body = &rules[at..at + 400];
            assert!(
                body.contains("BroadcastMode::Deferred"),
                "{silent_fn} 是后台批量腿，必须延后广播，否则一轮 33 次 switch_mode"
            );
        }

        let body = fn_body("async fn run_due_updates(");
        assert_eq!(
            body.matches("broadcast_config_changed(").count(),
            1,
            "整批只准广播一次（多于一次 = 风暴回归；零次 = 变更永远进不了运行中的核）"
        );
        let at = body
            .find("broadcast_config_changed(")
            .expect("上一条断言已保证存在");
        let head = &body[..at];
        assert!(
            head.contains("if ok_count > 0 || builtin_ok > 0"),
            "收尾广播必须门控在『本轮真有成功』上，否则空轮也白给核一次 switch_mode"
        );
    }

    #[test]
    fn catalog_leg_cannot_short_circuit_the_resource_leg() {
        // 变异锁：目录刷新失败**不得**打断 `.srs` 重下载腿。守的是形态——它必须是一条独立语句
        // （结果不被消费、不被 `?` 传播），这正是 上游 那圈 try/catch 的等价物。
        let body = fn_body("async fn run_due_updates(");
        let at = body
            .find("self.refresh_catalog_if_due(")
            .expect("上一个用例已保证存在");
        let head = &body[..at];
        let line_start = head.rfind('\n').map_or(0, |p| p + 1);
        assert!(
            head[line_start..].trim().is_empty(),
            "目录刷新腿的返回值不得被消费（`let x = ...` / `if ...` 都意味着它能左右后续流程）"
        );
        let stmt_end = body[at..].find(';').expect("语句必有分号");
        assert!(
            !body[at..at + stmt_end].contains('?'),
            "目录刷新腿不得用 `?` 传播——那会让一次目录刷新失败吞掉整轮资源更新"
        );
        // 且必须排在资源选取之前（同 上游的顺序：目录先刷新，随后按新目录做资源判定）。
        let sel = body.find("select_due_resources(").expect("资源腿仍在");
        assert!(at < sel, "目录刷新应在资源选取之前");
    }

    /// 🟡 **调用点守卫：目录缓存的读盘 + JSON parse 必须发生在拿 `inner` 锁之前。**
    ///
    /// 原实现把 [`cached_catalog_fetched_at`]（同步 `read` + `serde_json` parse）写在
    /// `catalog_refresh_due(...)` 的实参位置上 ⇒ 整个读盘都在持锁期间、且在 async fn 里。
    /// 缓存文件几百 KB 且落在用户配置目录（可能是网络盘 / 正被备份软件锁住），
    /// 那段时间 `run_due_updates` 的防重入判定与退避记账全部排队等它。
    ///
    /// **变异探针**：把 `cached_catalog_fetched_at(res_dir)` 挪回 `catalog_refresh_due` 的实参位
    /// （即挪到 `self.inner.lock()` 之后）⇒ 本条转红。
    #[test]
    fn catalog_refresh_reads_disk_before_taking_the_lock() {
        use crate::runtime::core_update_scheduler::method_scan::method_body;
        let src = include_str!("rule_resource_scheduler.rs");
        let body = method_body(src, "    async fn refresh_catalog_if_due(");
        let read_at = body
            .find("cached_catalog_fetched_at(res_dir)")
            .expect("锚点消失：守卫已失去判据");
        let lock_at = body
            .find("self.inner.lock()")
            .expect("锚点消失：守卫已失去判据");
        assert!(
            read_at < lock_at,
            "读盘 + JSON parse 必须在锁外完成（实得 read@{read_at} / lock@{lock_at}）—— \
             锁内只留判定与记 attempt 两步纯内存操作"
        );
        // 锁内不得再出现任何读盘。
        let in_lock = &body[lock_at..];
        assert!(
            !in_lock.contains("cached_catalog_fetched_at("),
            "持锁期间又读了一次盘"
        );
    }

    #[test]
    fn catalog_cache_file_name_mirrors_rules_rs() {
        // 本文件只读 peek `fetchedAt`，文件名是 `commands/rules.rs` 那份写入口的镜像。
        // 那边改名而这边没跟 → 节流基准恒 0 → 每轮重打远端。此断言让重命名当场转红。
        let rules = include_str!("../commands/rules.rs");
        assert!(
            rules.contains(&format!(
                r#"CATALOG_CACHE_FILE: &str = "{CATALOG_CACHE_FILE}""#
            )),
            "commands/rules.rs 的 catalog 缓存文件名已变，本文件的只读镜像常量必须同步"
        );
    }
}
