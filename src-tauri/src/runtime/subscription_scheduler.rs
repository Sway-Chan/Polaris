//! 订阅自动更新调度器（上游 `SubscriptionScheduler` 1:1 移植）。
//!
//! 职责：在不打断当前连接的前提下，按需自动刷新订阅节点。
//! - **启动补更**：启动后延迟一段时间（8s，避开启动高峰），对启用自动更新的订阅补一次更新（忽略陈旧
//!   阈值，仅守 10min 地板——开了「启动时更新」就应更新，而非"距上次≥间隔阈值才更"）。
//! - **周期巡检**：每 30 分钟扫一遍，更新到期（陈旧）的订阅。
//! - **退避**：单个订阅失败后指数退避（5min→…→上限 6h），避免对故障源高频重试。
//! - **不打断连接**：默认路径（内容变则经 `perform_subscription_update` 广播 → 汇流点热切换；无变化
//!   跳广播）已在共用核心里处理；选中节点被下架则 reconcile 内 reselect 兜底出口。
//! - **经代理开关**：全局三态策略 `subscriptionProxyPolicy`（follow=按 per-sub / proxy=全强制 / direct=全直连）
//!   作用于各订阅；经代理订阅若代理未运行则只跳过该订阅（冷启动鸡生蛋），挂起待代理就绪（`onProxyStarted`）
//!   补更，直连订阅照常更新。
//!
//! **纯决策 / 计时分离**（§禁真起定时器碰网络）：陈旧/退避/经代理可用性判定收在纯函数 [`select_due`] +
//! [`BackoffTracker`]（`now` 由调用方注入，全单测覆盖）；定时器/事件接线是薄壳，逐个 due 订阅调共用核心
//! [`crate::commands::subscription::perform_subscription_update`]（唯一生产路径，§K7.1）。

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tauri::{AppHandle, Listener, Manager};

use crate::commands::subscription::{perform_subscription_update, resolve_subscription_via_proxy};
use crate::events::channel::{EVENT_PROXY_STARTED, EVENT_SUBSCRIPTION_AUTOUPDATE};
use crate::runtime::AppRuntime;

const TICK_MS: u64 = 30 * 60_000; // 30 分钟巡检
const STARTUP_DELAY_MS: u64 = 8_000; // 启动延迟
const BACKOFF_BASE_MS: u64 = 5 * 60_000; // 退避基数 5 分钟
const BACKOFF_MAX_MS: u64 = 6 * 60 * 60_000; // 退避上限 6 小时
const DEFAULT_INTERVAL_HOURS: u64 = 12;
const STARTUP_MIN_GAP_MS: u64 = 10 * 60_000; // 启动/代理就绪补更免陈旧门时的最小间隔地板

/// 指数退避状态机（按 id）。上游 `BackoffTracker` 1:1。`now` 由调用方注入（确定性、可单测）。
#[derive(Debug)]
pub struct BackoffTracker {
    base_ms: u64,
    max_ms: u64,
    state: HashMap<String, BackoffEntry>,
}

#[derive(Debug, Clone, Copy)]
struct BackoffEntry {
    failures: u32,
    next_eligible_at: u64,
}

impl BackoffTracker {
    #[must_use]
    pub fn new(base_ms: u64, max_ms: u64) -> Self {
        Self {
            base_ms,
            max_ms,
            state: HashMap::new(),
        }
    }

    /// 是否到了可再次尝试的时刻（无记录=可以）。
    #[must_use]
    pub fn is_eligible(&self, id: &str, now: u64) -> bool {
        self.state
            .get(id)
            .is_none_or(|bo| now >= bo.next_eligible_at)
    }

    pub fn record_success(&mut self, id: &str) {
        self.state.remove(id);
    }

    /// 记录失败并推进退避：`min(base * 2^(n-1), max)`。返回 `(failures, delay_ms)`。
    pub fn record_failure(&mut self, id: &str, now: u64) -> (u32, u64) {
        let failures = self.state.get(id).map_or(0, |e| e.failures) + 1;
        // base * 2^(n-1)，饱和防溢出。
        let delay_ms = self
            .base_ms
            .saturating_mul(1u64.checked_shl(failures - 1).unwrap_or(u64::MAX))
            .min(self.max_ms);
        self.state.insert(
            id.to_string(),
            BackoffEntry {
                failures,
                next_eligible_at: now.saturating_add(delay_ms),
            },
        );
        (failures, delay_ms)
    }

    /// 剪除不在活跃集合内的陈旧键（订阅被删后清理，防内存无界增长）。
    pub fn prune(&mut self, active_ids: &std::collections::HashSet<String>) {
        self.state.retain(|id, _| active_ids.contains(id));
    }
}

/// 到期选择结果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DueSelection {
    /// 本轮应更新的订阅 id（声明序）。
    pub due_ids: Vec<String>,
    /// 存在「经代理但代理未起」被跳过的订阅 → 挂起待代理就绪补更。
    pub pending_proxy_catchup: bool,
}

/// 纯决策：从 config + now + 退避状态选出本轮到期订阅。上游 `runDueUpdates` 阶段 1 的选择逻辑。
///
/// - 总开关 `autoUpdateSubscriptionOnStart` 未开 → 空。
/// - `subscriptionUpdateIntervalHours == 0`（UI「仅手动」）→ **周期巡检路径一律空**（见下）。
/// - per-sub `autoUpdate` 关 → 跳过。
/// - 陈旧判断：`ignore_staleness`（启动/代理就绪补更）→ 仅守 10min 地板；否则 `now-last >= interval`。
/// - 退避未到 → 跳过。
/// - 经代理（全局策略 × per-sub）但代理未起 → 跳过 + 置 `pending_proxy_catchup`（直连订阅不受影响）。
#[must_use]
pub fn select_due(
    config: &Value,
    now_ms: u64,
    backoff: &BackoffTracker,
    proxy_running: bool,
    ignore_staleness: bool,
) -> DueSelection {
    let mut out = DueSelection::default();
    if config
        .get("autoUpdateSubscriptionOnStart")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return out; // 总开关未开
    }
    // #18：`interval == 0` 是 UI 下拉的**「仅手动」**档，不是「没填」。旧写法 `.filter(|h| *h > 0)`
    // 把它和缺省一起吞成 12h → 用户选了「仅手动」照样每 12h 自动更新（用户可见的错误行为）。
    //
    // 取舍：**0 只关掉周期巡检腿，不关启动补更腿**。理由——「启动时自动更新订阅」
    // （`autoUpdateSubscriptionOnStart`）与「更新间隔」是两个独立开关，用户把间隔设成「仅手动」
    // 表达的是「别在后台按时钟反复拉」，不是「启动时也别拉」；后者他若不想要，关的是总开关。
    // 启动补更本就免陈旧门（仅守 10min 地板），不受 interval 影响，故此处只挡 `!ignore_staleness`。
    let interval_hours = config
        .get("subscriptionUpdateIntervalHours")
        .and_then(Value::as_u64);
    if interval_hours == Some(0) && !ignore_staleness {
        return out; // 仅手动 → 周期巡检不选任何订阅
    }
    let interval_ms = interval_hours
        .filter(|h| *h > 0)
        .unwrap_or(DEFAULT_INTERVAL_HOURS)
        * 3_600_000;
    let policy = config
        .get("subscriptionProxyPolicy")
        .and_then(Value::as_str);
    let Some(subs) = config.get("subscriptions").and_then(Value::as_array) else {
        return out;
    };

    for sub in subs {
        if sub.get("autoUpdate").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let Some(id) = sub.get("id").and_then(Value::as_str) else {
            continue;
        };
        // 陈旧判断：从未更新（last=0）或超阈值。启动/代理就绪补更仅守 10min 地板。
        let last = sub
            .get("lastUpdated")
            .and_then(Value::as_str)
            .and_then(rfc3339_to_epoch_ms)
            .unwrap_or(0);
        let threshold = if ignore_staleness {
            STARTUP_MIN_GAP_MS
        } else {
            interval_ms
        };
        let stale = last == 0 || now_ms.saturating_sub(last) >= threshold;
        if !stale {
            continue;
        }
        if !backoff.is_eligible(id, now_ms) {
            continue;
        }
        // 经代理但代理未起 → 跳过该订阅、挂起补更（直连订阅照常）。
        let via_proxy = resolve_subscription_via_proxy(
            policy,
            sub.get("updateViaProxy").and_then(Value::as_bool),
        );
        if via_proxy && !proxy_running {
            out.pending_proxy_catchup = true;
            continue;
        }
        out.due_ids.push(id.to_string());
    }
    out
}

/// 解析 `current_iso` 产出的 RFC3339（`YYYY-MM-DDTHH:MM:SS.mmmZ`，UTC）→ epoch 毫秒。
///
/// 只需覆盖本仓 `current_iso`（=`created_at_to_rfc3339`）的输出形态：宽松抽取数字段，缺毫秒/末 `Z` 亦容忍；
/// 无 time 依赖（days_from_civil，Howard Hinnant，与 stats-engine `unix_to_civil` 互逆）。解析失败 → `None`
/// （调用方视作 last=0 → 立即到期，宁可多更一次不漏更）。
#[must_use]
pub fn rfc3339_to_epoch_ms(s: &str) -> Option<u64> {
    // 抽取前 6 个整数段（year month day hour min sec）+ 可选毫秒。
    let mut nums: Vec<u64> = Vec::with_capacity(7);
    let mut cur = String::new();
    let mut millis: u64 = 0;
    let mut seen_dot = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else {
            if !cur.is_empty() {
                if seen_dot {
                    // 毫秒段：取前 3 位（截/补）。
                    let ms3: String = cur.chars().take(3).collect();
                    millis = format!("{ms3:0<3}").parse().unwrap_or(0);
                    cur.clear();
                    break;
                }
                nums.push(cur.parse().ok()?);
                cur.clear();
            }
            if c == '.' {
                seen_dot = true;
            }
            if nums.len() >= 6 && !seen_dot {
                break; // 已够 6 段且非毫秒分隔 → 结束
            }
        }
    }
    if !cur.is_empty() && nums.len() < 6 {
        nums.push(cur.parse().ok()?);
    }
    if nums.len() < 6 {
        return None;
    }
    let (year, month, day, hour, min, sec) =
        (nums[0] as i64, nums[1], nums[2], nums[3], nums[4], nums[5]);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(year, month as u32, day as u32);
    let secs = days * 86_400 + (hour * 3600 + min * 60 + sec) as i64;
    if secs < 0 {
        return None;
    }
    Some((secs as u64) * 1000 + millis)
}

/// civil → 自 1970-01-01 的天数（Howard Hinnant days_from_civil）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + (i64::from(d) - 1); // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// 当前 epoch 毫秒。`pub(crate)` 是为让 `rule_resource_scheduler` 复用同一时钟入口
/// （两调度器的 `now` 语义必须一致，各写一份必然漂移）。
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// 从 config 按 id 取订阅显示名（用于失败日志 + 事件 payload）。缺失 → 空串。
fn sub_name<'a>(config: &'a Value, id: &str) -> &'a str {
    config
        .get("subscriptions")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|s| s.get("id").and_then(Value::as_str) == Some(id))
        })
        .and_then(|s| s.get("name").and_then(Value::as_str))
        .unwrap_or("")
}

/// 由 `perform_subscription_update` 的返回构造 `EVENT_SUBSCRIPTION_AUTOUPDATE` payload（纯函数，可单测）。
///
/// 成功态透传 added/updated/deleted/unchanged 计数；失败态透传后端真实 `error`（缺省兜底文案）。
/// 渲染端只对 `success:false` 弹 toast（对齐 上游 后台更新只入日志、成功静默的 UX）。
fn build_autoupdate_payload(id: &str, name: &str, result: &Value) -> Value {
    let success = result.get("success").and_then(Value::as_bool) == Some(true);
    if success {
        json!({
            "subscriptionId": id,
            "name": name,
            "success": true,
            "addedServers": result.get("addedServers").and_then(Value::as_u64).unwrap_or(0),
            "updatedServers": result.get("updatedServers").and_then(Value::as_u64).unwrap_or(0),
            "deletedServers": result.get("deletedServers").and_then(Value::as_u64).unwrap_or(0),
            "unchanged": result.get("unchanged").and_then(Value::as_bool).unwrap_or(false),
        })
    } else {
        json!({
            "subscriptionId": id,
            "name": name,
            "success": false,
            "error": result.get("error").and_then(Value::as_str).unwrap_or("订阅更新失败"),
        })
    }
}

/// 订阅自动更新调度器（含退避 + 防重入 + 挂起补更）。
pub struct SubscriptionScheduler {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    backoff: BackoffTracker,
    is_running: bool,
    pending_proxy_catchup: bool,
    started: bool,
}

impl Default for SubscriptionScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl SubscriptionScheduler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                backoff: BackoffTracker::new(BACKOFF_BASE_MS, BACKOFF_MAX_MS),
                is_running: false,
                pending_proxy_catchup: false,
                started: false,
            })),
        }
    }

    /// 启动：装 8s 启动补更 + 30min 周期巡检 + `event:proxyStarted` 补更监听。幂等（重复调用 no-op）。
    pub fn start(self: &Arc<Self>, app: AppHandle) {
        {
            let mut inner = self.inner.lock().expect("scheduler lock");
            if inner.started {
                return;
            }
            inner.started = true;
        }

        // 启动补更（忽略陈旧门，仅守 10min 地板）。
        let this = self.clone();
        let app_startup = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(STARTUP_DELAY_MS)).await;
            this.run_due_updates(&app_startup, true).await;
        });

        // 周期巡检。
        let this = self.clone();
        let app_tick = app.clone();
        tauri::async_runtime::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(TICK_MS));
            interval.tick().await; // 立即触发的首 tick 跳过（启动补更已覆盖）
            loop {
                interval.tick().await;
                this.run_due_updates(&app_tick, false).await;
            }
        });

        // 代理就绪 → 补跑挂起（经代理但代理未起而跳过的订阅）。
        let this = self.clone();
        let app_evt = app.clone();
        app.listen(EVENT_PROXY_STARTED, move |_| {
            let this = this.clone();
            let app = app_evt.clone();
            tauri::async_runtime::spawn(async move {
                this.on_proxy_started(&app).await;
            });
        });
    }

    /// 代理就绪补更：仅当有挂起标记且当前空闲。
    async fn on_proxy_started(self: &Arc<Self>, app: &AppHandle) {
        {
            let mut inner = self.inner.lock().expect("scheduler lock");
            // isRunning 时不清 flag：已有更新在途，保留挂起标记待后续触发。
            if !inner.pending_proxy_catchup || inner.is_running {
                return;
            }
            inner.pending_proxy_catchup = false;
        }
        self.run_due_updates(app, true).await;
    }

    /// 一轮到期更新：防重入 → 选到期 → 逐个调共用核心 → 记退避。
    async fn run_due_updates(self: &Arc<Self>, app: &AppHandle, ignore_staleness: bool) {
        // 防重入闸。
        {
            let mut inner = self.inner.lock().expect("scheduler lock");
            if inner.is_running {
                return;
            }
            inner.is_running = true;
        }
        // 无论中途 return/panic 都要清 is_running（用 guard）。
        let _guard = RunningGuard {
            inner: self.inner.clone(),
        };

        let state = app.state::<AppRuntime>();
        let config = match state.config().load_full() {
            Ok(c) => c,
            Err(_) => return,
        };
        let proxy_running = state.proxy().status().running;
        let now = now_ms();

        // 选到期 + 剪退避（读退避在锁内，尽量短持）。
        let selection = {
            let mut inner = self.inner.lock().expect("scheduler lock");
            let active: std::collections::HashSet<String> = config
                .get("subscriptions")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.get("id").and_then(Value::as_str).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            inner.backoff.prune(&active);
            let sel = select_due(
                &config,
                now,
                &inner.backoff,
                proxy_running,
                ignore_staleness,
            );
            inner.pending_proxy_catchup = sel.pending_proxy_catchup;
            sel
        };

        // 逐个更新（不持调度器锁跨 await）。
        for id in selection.due_ids {
            let result = perform_subscription_update(app, state.inner(), &id).await;
            let ok = result.get("success").and_then(Value::as_bool) == Some(true);
            // 失败落日志（对齐 上游 SubScheduler logManager.addLog('warn', ...)：后台失败的用户可见面 =
            // 日志）+ 发事件让渲染端弹 toast（补 上游 无、Polaris 原本静默的缺口——退避已限频，不刷屏）。
            let name = sub_name(&config, &id).to_string();
            if !ok {
                let err = result
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("订阅更新失败");
                log::warn!("[订阅自动更新] 订阅「{name}」更新失败: {err}");
            }
            crate::events::broadcast(
                app,
                EVENT_SUBSCRIPTION_AUTOUPDATE,
                build_autoupdate_payload(&id, &name, &result),
            );
            let mut inner = self.inner.lock().expect("scheduler lock");
            if ok {
                inner.backoff.record_success(&id);
            } else {
                inner.backoff.record_failure(&id, now);
            }
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

    fn base_cfg(subs: Value) -> Value {
        json!({
            "autoUpdateSubscriptionOnStart": true,
            "subscriptionUpdateIntervalHours": 12,
            "subscriptions": subs,
        })
    }

    // ── BackoffTracker ─────────────────────────────────────────────────────────
    #[test]
    fn backoff_exponential_capped_and_reset() {
        let mut b = BackoffTracker::new(BACKOFF_BASE_MS, BACKOFF_MAX_MS);
        assert!(b.is_eligible("s", 0), "无记录 → 可尝试");
        let (f1, d1) = b.record_failure("s", 1000);
        assert_eq!(f1, 1);
        assert_eq!(d1, BACKOFF_BASE_MS, "首败 = base");
        assert!(!b.is_eligible("s", 1000), "退避中不可尝试");
        assert!(
            b.is_eligible("s", 1000 + BACKOFF_BASE_MS),
            "过退避窗 → 可尝试"
        );
        let (f2, d2) = b.record_failure("s", 2000);
        assert_eq!(f2, 2);
        assert_eq!(d2, BACKOFF_BASE_MS * 2, "二败 = base*2");
        // 上限封顶。
        for _ in 0..20 {
            b.record_failure("s", 0);
        }
        let (_, d) = b.record_failure("s", 0);
        assert_eq!(d, BACKOFF_MAX_MS, "封顶 6h");
        // 成功复位。
        b.record_success("s");
        assert!(b.is_eligible("s", 0));
    }

    #[test]
    fn backoff_prune_removes_inactive() {
        let mut b = BackoffTracker::new(BACKOFF_BASE_MS, BACKOFF_MAX_MS);
        b.record_failure("keep", 0);
        b.record_failure("drop", 0);
        b.prune(&std::collections::HashSet::from(["keep".to_string()]));
        assert!(!b.is_eligible("keep", 0), "keep 保留退避");
        assert!(b.is_eligible("drop", 0), "drop 被剪 → 无记录");
    }

    // ── autoupdate 事件 payload ───────────────────────────────────────────────
    #[test]
    fn autoupdate_payload_failure_carries_real_error() {
        // 失败结果（perform_subscription_update 的 update_failure 形态）→ success:false + 透传 error。
        let result = json!({
            "success": false,
            "addedServers": 0,
            "updatedServers": 0,
            "deletedServers": 0,
            "error": "DNS 解析失败",
        });
        let p = build_autoupdate_payload("sub-1", "机场A", &result);
        assert_eq!(p["subscriptionId"], json!("sub-1"));
        assert_eq!(p["name"], json!("机场A"));
        assert_eq!(p["success"], json!(false));
        assert_eq!(
            p["error"],
            json!("DNS 解析失败"),
            "失败态须透传后端真实 error，不用笼统兜底"
        );
        assert!(p.get("addedServers").is_none(), "失败态不带计数字段");
    }

    #[test]
    fn autoupdate_payload_success_carries_counts() {
        let result = json!({
            "success": true,
            "addedServers": 3,
            "updatedServers": 1,
            "deletedServers": 2,
            "unchanged": false,
        });
        let p = build_autoupdate_payload("sub-2", "机场B", &result);
        assert_eq!(p["success"], json!(true));
        assert_eq!(p["addedServers"], json!(3));
        assert_eq!(p["updatedServers"], json!(1));
        assert_eq!(p["deletedServers"], json!(2));
        assert_eq!(p["unchanged"], json!(false));
        assert!(p.get("error").is_none(), "成功态无 error 字段");
    }

    #[test]
    fn autoupdate_payload_failure_falls_back_when_error_missing() {
        // error 字段缺失（异常返回）→ 兜底文案，绝不 panic / 绝不留空。
        let p = build_autoupdate_payload("x", "", &json!({ "success": false }));
        assert_eq!(p["error"], json!("订阅更新失败"));
    }

    #[test]
    fn sub_name_lookup_by_id() {
        let cfg = json!({ "subscriptions": [
            { "id": "a", "name": "Alpha" },
            { "id": "b", "name": "Beta" },
        ]});
        assert_eq!(sub_name(&cfg, "b"), "Beta");
        assert_eq!(sub_name(&cfg, "missing"), "", "缺失订阅 → 空串（不 panic）");
    }

    // ── rfc3339 解析 ──────────────────────────────────────────────────────────
    #[test]
    fn rfc3339_parse_roundtrip_with_stats_engine() {
        // 与 stats-engine created_at_to_rfc3339 互逆（同 civil 算法）。
        let ms: u64 = 1_700_000_000_123;
        let iso = polaris_stats_engine::created_at_to_rfc3339(ms as i64).unwrap();
        assert_eq!(rfc3339_to_epoch_ms(&iso), Some(ms), "iso={iso}");
        // epoch 起点。
        assert_eq!(rfc3339_to_epoch_ms("1970-01-01T00:00:00.000Z"), Some(0));
        // 无毫秒段容忍。
        assert_eq!(rfc3339_to_epoch_ms("1970-01-01T00:00:01Z"), Some(1000));
        // 坏输入 → None。
        assert_eq!(rfc3339_to_epoch_ms("not-a-date"), None);
        assert_eq!(rfc3339_to_epoch_ms(""), None);
    }

    // ── select_due 决策 ───────────────────────────────────────────────────────
    #[test]
    fn select_due_respects_master_switch() {
        let mut cfg = base_cfg(json!([{"id":"a","autoUpdate":true}]));
        cfg["autoUpdateSubscriptionOnStart"] = json!(false);
        let b = BackoffTracker::new(BACKOFF_BASE_MS, BACKOFF_MAX_MS);
        // 打断总开关 → 空（无论陈旧）。
        assert!(
            select_due(&cfg, 0, &b, true, true).due_ids.is_empty(),
            "总开关关 → 不更"
        );
    }

    // 现实 epoch-ms 基准（>1e11，避开 created_at_to_rfc3339 对小值按「秒」的量级分档）。
    const NOW: u64 = 1_700_000_000_000; // 2023-11-14

    #[test]
    fn select_due_stale_and_autoupdate_gate() {
        let cfg = base_cfg(json!([
            {"id":"stale","autoUpdate":true,"lastUpdated":"1970-01-01T00:00:00.000Z"}, // 远超 12h → 陈旧
            {"id":"fresh","autoUpdate":true,"lastUpdated": polaris_stats_engine::created_at_to_rfc3339((NOW - 3_600_000) as i64)}, // 1h 前 → 不陈旧
            {"id":"off","autoUpdate":false,"lastUpdated":"1970-01-01T00:00:00.000Z"} // autoUpdate 关
        ]));
        let b = BackoffTracker::new(BACKOFF_BASE_MS, BACKOFF_MAX_MS);
        let sel = select_due(&cfg, NOW, &b, true, false);
        assert_eq!(
            sel.due_ids,
            vec!["stale".to_string()],
            "仅陈旧且 autoUpdate 开"
        );
    }

    #[test]
    fn select_due_ignore_staleness_uses_min_gap() {
        // 距上次 5min（< 12h interval 但也 < 10min 地板）→ ignore_staleness 下仍不更。
        let recent = polaris_stats_engine::created_at_to_rfc3339((NOW - 5 * 60_000) as i64);
        let cfg = base_cfg(json!([{"id":"a","autoUpdate":true,"lastUpdated": recent}]));
        let b = BackoffTracker::new(BACKOFF_BASE_MS, BACKOFF_MAX_MS);
        assert!(
            select_due(&cfg, NOW, &b, true, true).due_ids.is_empty(),
            "5min < 10min 地板 → 不更"
        );
        // 距上次 15min（> 10min 地板）→ ignore_staleness 下更。
        let older = polaris_stats_engine::created_at_to_rfc3339((NOW - 15 * 60_000) as i64);
        let cfg2 = base_cfg(json!([{"id":"a","autoUpdate":true,"lastUpdated": older}]));
        assert_eq!(
            select_due(&cfg2, NOW, &b, true, true).due_ids,
            vec!["a".to_string()]
        );
    }

    #[test]
    fn select_due_backoff_skips() {
        let cfg = base_cfg(
            json!([{"id":"a","autoUpdate":true,"lastUpdated":"1970-01-01T00:00:00.000Z"}]),
        );
        let mut b = BackoffTracker::new(BACKOFF_BASE_MS, BACKOFF_MAX_MS);
        b.record_failure("a", 1000); // 退避到 1000+base
        assert!(
            select_due(&cfg, 1000, &b, true, false).due_ids.is_empty(),
            "退避中跳过"
        );
        assert_eq!(
            select_due(&cfg, 1000 + BACKOFF_BASE_MS, &b, true, false).due_ids,
            vec!["a".to_string()],
            "退避过 → 更"
        );
    }

    #[test]
    fn select_due_interval_zero_is_manual_only_for_periodic_tick() {
        // #18：UI「仅手动」= interval 0。周期巡检腿一律不选；启动补更腿不受影响（独立开关）。
        let mut cfg = base_cfg(json!([
            {"id":"a","autoUpdate":true,"lastUpdated":"1970-01-01T00:00:00.000Z"}
        ]));
        cfg["subscriptionUpdateIntervalHours"] = json!(0);
        let b = BackoffTracker::new(BACKOFF_BASE_MS, BACKOFF_MAX_MS);
        assert!(
            select_due(&cfg, NOW, &b, true, false).due_ids.is_empty(),
            "interval=0 → 周期巡检不选任何订阅（旧实现会当成 12h 照更）"
        );
        assert_eq!(
            select_due(&cfg, NOW, &b, true, true).due_ids,
            vec!["a".to_string()],
            "interval=0 不影响启动补更腿（仍守 10min 地板）"
        );
    }

    #[test]
    fn select_due_missing_or_bad_interval_still_falls_back_to_default() {
        // 缺字段 / 非数 → 仍回落 12h（存量行为不变，只有显式 0 才是「仅手动」）。
        let mut cfg = base_cfg(json!([
            {"id":"a","autoUpdate":true,"lastUpdated":"1970-01-01T00:00:00.000Z"}
        ]));
        cfg.as_object_mut()
            .unwrap()
            .remove("subscriptionUpdateIntervalHours");
        let b = BackoffTracker::new(BACKOFF_BASE_MS, BACKOFF_MAX_MS);
        assert_eq!(
            select_due(&cfg, NOW, &b, true, false).due_ids,
            vec!["a".to_string()]
        );
        cfg["subscriptionUpdateIntervalHours"] = json!("12");
        assert_eq!(
            select_due(&cfg, NOW, &b, true, false).due_ids,
            vec!["a".to_string()]
        );
    }

    #[test]
    fn select_due_via_proxy_pending_when_proxy_down() {
        // 全局 proxy 策略 + 代理未起 → 跳过 + pending；直连订阅照常。
        let mut cfg = base_cfg(json!([
            {"id":"proxied","autoUpdate":true,"lastUpdated":"1970-01-01T00:00:00.000Z"},
            {"id":"direct","autoUpdate":true,"updateViaProxy":false,"lastUpdated":"1970-01-01T00:00:00.000Z"}
        ]));
        cfg["subscriptionProxyPolicy"] = json!("proxy"); // 全强制经代理
        let b = BackoffTracker::new(BACKOFF_BASE_MS, BACKOFF_MAX_MS);
        let sel = select_due(&cfg, 100 * 3_600_000, &b, false, false); // 代理未起
        assert!(sel.pending_proxy_catchup, "经代理订阅被跳过 → 挂起");
        assert!(
            sel.due_ids.is_empty(),
            "proxy 策略下两订阅都经代理 → 全跳过"
        );
        // 代理起了 → 两个都更。
        let sel2 = select_due(&cfg, 100 * 3_600_000, &b, true, false);
        assert_eq!(sel2.due_ids.len(), 2);
        assert!(!sel2.pending_proxy_catchup);
    }
}
