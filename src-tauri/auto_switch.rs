//! **C3 自动换节点决策层**（上游 `AutoSwitchService` 的纯逻辑镜像）。
//!
//! # 职责边界（与崩溃恢复解耦——1:1 移植 上游 头注）
//! 只负责「当前节点不可达」时换到更优节点。**进程崩溃由 [`ProxyRuntime`](crate::runtime::proxy) 的
//! 崩溃监测「原地重启同节点」兜底，绝不触发换节点**——崩溃多为瞬时/配置问题，换节点既不对症又会丢失
//! 用户选中节点（上游 AutoSwitchService.ts:4-6）。故本层**不消费** `spawn_crash_monitor`，只消费
//! 「应用层连通性」这个独立信号。
//!
//! # 为什么把决策抽成纯状态机
//! 「别过度触发（一次瞬断不该切）也别欠触发」+「重试阈值/冷却/熔断」是本任务的核心正确性，而它们
//! 全是**与网络 I/O 无关的时序决策**。抽成纯 [`AutoSwitchMachine`] + 纯选择函数 → 触发判定 / 冷却 /
//! 熔断 / 下一节点选择全部可用真值表单测 + 变异验证锁死，**无需真起核、不碰宿主网络**（网络探测 I/O
//! 留在 `proxy.rs` 驱动层，真机门）。范式对齐同仓 `CrashRecoveryMachine`（决策在 crate、I/O 在 runtime）。
//!
//! # 常量（逐一对齐 上游 AutoSwitchService.ts:29-40）
//! 阈值/冷却/熔断窗口全部照搬，偏离即语义漂移。

use serde_json::Value;

/// 心跳检测间隔（上游 `HEARTBEAT_INTERVAL_MS`，:29）。
pub const HEARTBEAT_INTERVAL_MS: u64 = 30_000;
/// 连续失败触发换节点的阈值（上游 `MAX_CONSECUTIVE_FAILURES`，:30）。
/// **别过度触发的核心**：单次瞬断只累加计数，连续 3 次才切。
pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// 单次候选节点 TCP ping 超时（上游 `PING_TIMEOUT_MS`，:31）。
pub const PING_TIMEOUT_MS: u64 = 4_000;
/// 换节点冷却窗口（上游 `SWITCH_COOLDOWN_MS`，:32）。防频繁切换。
pub const SWITCH_COOLDOWN_MS: u64 = 60_000;
/// 应用层连通性探测超时（上游 `CONNECTIVITY_TIMEOUT_MS`，:33）。
pub const CONNECTIVITY_TIMEOUT_MS: u64 = 5_000;
/// 熔断阈值：连续自动切换达此数仍未恢复 → 暂停（上游 `MAX_AUTO_SWITCHES`，:34）。
pub const MAX_AUTO_SWITCHES: u32 = 3;
/// 熔断冷却：触发后暂停切换的时长，10 分钟后放行一次重试（上游 `BREAKER_COOLDOWN_MS`，:35）。
pub const BREAKER_COOLDOWN_MS: u64 = 10 * 60_000;
/// 经代理请求的连通性探测端点（返回 204）：海外可达即证明代理链通；多个互为兜底
/// （上游 `CONNECTIVITY_URLS`，:37-40）。
pub const CONNECTIVITY_URLS: [&str; 2] = [
    "http://cp.cloudflare.com/generate_204",
    "http://www.gstatic.com/generate_204",
];

/// 一次心跳连通性检测喂入决策机后的结论（上游 `runHeartbeat` 的分支）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatOutcome {
    /// 连通且此前无失败 → 稳态，无动作。
    Stable,
    /// 连通但此前有连续失败 → 复位计数（连通性恢复正常）。`prior` = 复位前的失败次数（供日志）。
    Recovered { prior: u32 },
    /// 未连通但未达阈值 → 累加失败计数，暂不切。`failures` = 累加后的连续失败次数。
    Failing { failures: u32 },
    /// 连续失败达阈值 → 触发换节点（失败计数已在内部复位，对齐 上游 :142）。
    Trigger,
}

/// 换节点前的闸门评估结果（上游 `triggerSwitch` 前半：isSwitching / 熔断 / 冷却）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchGate {
    /// 放行——可执行换节点。
    Proceed,
    /// 已有换节点在飞 → 跳过（上游 isSwitching 守卫）。
    InFlight,
    /// 熔断中（连续切换未恢复）→ 暂停。`remaining_ms` = 距放行剩余时间。
    Breaker { remaining_ms: u64 },
    /// 冷却中 → 暂停。`remaining_ms` = 距可再触发剩余时间。
    Cooldown { remaining_ms: u64 },
}

/// 自动换节点决策状态机（上游 `AutoSwitchService` 的时序态，纯逻辑无 I/O）。
///
/// 每个运行核世代一个实例（随核就绪 `enable`、随核停/接管退场丢弃）——对齐 上游 单例但按世代重置。
#[derive(Debug)]
pub struct AutoSwitchMachine {
    enabled: bool,
    /// 连续连通性失败次数（上游 `consecutiveFailures`）。
    consecutive_failures: u32,
    /// 换节点在飞标志（上游 `isSwitching`）：同一时刻只允许一个换节点操作。
    is_switching: bool,
    /// 上次换节点时刻 ms（上游 `lastSwitchTime`），冷却基准。
    last_switch_time: u64,
    /// 连续自动切换次数（上游 `consecutiveSwitches`），熔断计数。
    consecutive_switches: u32,
    /// 熔断触发时刻 ms（上游 `breakerTrippedAt`）。
    breaker_tripped_at: u64,
}

impl Default for AutoSwitchMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoSwitchMachine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: false,
            consecutive_failures: 0,
            is_switching: false,
            last_switch_time: 0,
            consecutive_switches: 0,
            breaker_tripped_at: 0,
        }
    }

    /// 启用（上游 `enable`，:63-71）：复位失败/熔断计数。幂等（已启用则 no-op）。
    pub fn enable(&mut self) {
        if self.enabled {
            return;
        }
        self.enabled = true;
        self.consecutive_failures = 0;
        self.consecutive_switches = 0;
        self.breaker_tripped_at = 0;
    }

    /// 禁用（上游 `disable`，:73-78）。幂等。
    pub fn disable(&mut self) {
        self.enabled = false;
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn is_switching(&self) -> bool {
        self.is_switching
    }

    /// 核未运行时只复位失败计数、**不动熔断计数**（上游 `runHeartbeat` 的 `!running` 分支，:107-110）。
    pub fn reset_failures_only(&mut self) {
        self.consecutive_failures = 0;
    }

    /// 喂一次心跳连通性结果 → 决策（上游 `runHeartbeat` 的 alive/失败分支，:122-145）。
    ///
    /// - `alive=true`：复位连续失败 **且** 复位熔断计数（恢复联通即视为已稳定，上游 :130-132）。
    /// - `alive=false`：累加失败；达 [`MAX_CONSECUTIVE_FAILURES`] → 复位失败计数并返 [`Trigger`]
    ///   （上游 :141-143：先 `consecutiveFailures = 0` 再 `triggerSwitch`）。
    ///
    /// [`Trigger`]: HeartbeatOutcome::Trigger
    pub fn on_heartbeat(&mut self, alive: bool) -> HeartbeatOutcome {
        if alive {
            let prior = self.consecutive_failures;
            self.consecutive_failures = 0;
            // 恢复联通即视为已稳定，复位熔断计数（上游 :132）。
            self.consecutive_switches = 0;
            if prior > 0 {
                HeartbeatOutcome::Recovered { prior }
            } else {
                HeartbeatOutcome::Stable
            }
        } else {
            self.consecutive_failures += 1;
            if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                self.consecutive_failures = 0;
                HeartbeatOutcome::Trigger
            } else {
                HeartbeatOutcome::Failing {
                    failures: self.consecutive_failures,
                }
            }
        }
    }

    /// 换节点前闸门（上游 `triggerSwitch` :151-178）：**顺序即语义**——
    /// 1. 在飞 → [`InFlight`]（上游 :151-154）。
    /// 2. 熔断：连续切换达 [`MAX_AUTO_SWITCHES`] 且仍在 [`BREAKER_COOLDOWN_MS`] 内 → [`Breaker`]；
    ///    冷却结束 → 复位 `consecutive_switches` 放行一次重试（上游 :157-170）。
    /// 3. 冷却：距上次换节点 < [`SWITCH_COOLDOWN_MS`] → [`Cooldown`]（上游 :173-178）。
    /// 4. 否则 [`Proceed`]。
    ///
    /// **有副作用**（熔断冷却结束的复位），故 `&mut self`——与 上游 在 triggerSwitch 内联复位同构。
    ///
    /// [`InFlight`]: SwitchGate::InFlight
    /// [`Breaker`]: SwitchGate::Breaker
    /// [`Cooldown`]: SwitchGate::Cooldown
    /// [`Proceed`]: SwitchGate::Proceed
    pub fn evaluate_switch(&mut self, now: u64) -> SwitchGate {
        if self.is_switching {
            return SwitchGate::InFlight;
        }
        // 熔断检查（先于冷却，对齐 上游 顺序）。
        if self.consecutive_switches >= MAX_AUTO_SWITCHES {
            let since_trip = now.saturating_sub(self.breaker_tripped_at);
            if since_trip < BREAKER_COOLDOWN_MS {
                return SwitchGate::Breaker {
                    remaining_ms: BREAKER_COOLDOWN_MS - since_trip,
                };
            }
            // 冷却结束，复位熔断，放行一次重试（上游 :169）。
            self.consecutive_switches = 0;
        }
        // 冷却检查。
        let since_last = now.saturating_sub(self.last_switch_time);
        if since_last < SWITCH_COOLDOWN_MS {
            return SwitchGate::Cooldown {
                remaining_ms: SWITCH_COOLDOWN_MS - since_last,
            };
        }
        SwitchGate::Proceed
    }

    /// 闸门放行后进入换节点在飞态（上游 `triggerSwitch` :180-181：置 `isSwitching` + `lastSwitchTime`）。
    /// **无论成功与否都提前置 `lastSwitchTime`** → 失败/无候选也进入冷却，防在节点间空转（上游 同构）。
    pub fn begin_switch(&mut self, now: u64) {
        self.is_switching = true;
        self.last_switch_time = now;
    }

    /// 换节点**真正执行了一次切换**后记账（上游 `triggerSwitch` :233-236）：
    /// `consecutive_switches++`，达 [`MAX_AUTO_SWITCHES`] → 记熔断触发时刻。
    ///
    /// **只在真发生切换时调**（候选空 / 全不可达的早退不调，对齐 上游 那两个 `return` 不增计数）。
    pub fn record_switch_success(&mut self, now: u64) {
        self.consecutive_switches += 1;
        if self.consecutive_switches >= MAX_AUTO_SWITCHES {
            self.breaker_tripped_at = now;
        }
    }

    /// 换节点结束，退出在飞态（上游 `triggerSwitch` finally :257-259：`isSwitching = false`）。
    /// 成功/失败/早退都必须调（对齐 finally 语义）。
    pub fn end_switch(&mut self) {
        self.is_switching = false;
    }
}

/// 候选节点及其测得延迟（上游 `{ server, latency }`）。`latency_ms=None` = 不可达。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateLatency {
    pub id: String,
    pub name: String,
    pub latency_ms: Option<u32>,
}

/// 从原始配置抽「非当前选中」的候选节点（上游 `candidates = servers.filter(s => s.id !== currentId)`，:188）。
///
/// 纯函数：仅读 `servers` 数组 + 排除 `current_id`。缺 `servers` / 非数组 → 空。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateNode {
    pub id: String,
    pub name: String,
    pub address: String,
    pub port: u16,
}

#[must_use]
pub fn extract_candidates(config: &Value, current_id: Option<&str>) -> Vec<CandidateNode> {
    let Some(servers) = config.get("servers").and_then(Value::as_array) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|s| {
            let id = s.get("id").and_then(Value::as_str)?;
            if Some(id) == current_id {
                return None;
            }
            let name = s.get("name").and_then(Value::as_str).unwrap_or(id);
            let address = s
                .get("address")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let port = s.get("port").and_then(Value::as_u64).unwrap_or(0) as u16;
            Some(CandidateNode {
                id: id.to_string(),
                name: name.to_string(),
                address: address.to_string(),
                port,
            })
        })
        .collect()
}

/// 选最优候选（上游 :208-218：过滤不可达 → 按延迟升序 → 取 `available[0]`）。
///
/// 纯函数。入参**已排除当前节点**（由 [`extract_candidates`] 保证）。全不可达 → `None`（上游 :213-216）。
/// 延迟并列取**首个**（`min_by_key` 稳定返回首元 = 上游 稳定排序取 `[0]`，保候选原序优先）。
#[must_use]
pub fn select_best_candidate(candidates: &[CandidateLatency]) -> Option<&CandidateLatency> {
    candidates
        .iter()
        .filter(|c| c.latency_ms.is_some())
        .min_by_key(|c| c.latency_ms.unwrap_or(u32::MAX))
}

/// 前端 `autoNodeSwitched` 事件 payload（上游 :243-247 `{ reason, newServerName, latency }`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoNodeSwitchedPayload {
    /// 触发原因（如「连通性检测」）。
    pub reason: String,
    /// 切到的目标节点显示名。
    pub new_server_name: String,
    /// 目标节点测得延迟（ms）。
    pub latency: u32,
}

/// 换节点方案：新配置（`selectedServerId` 改到最优候选）+ 待发事件 payload（上游 :226 + :243-247）。
///
/// 不 `derive(Eq)`：`new_config` 是 [`serde_json::Value`]（含 f64 → 只 `PartialEq` 不 `Eq`）。
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchPlan {
    /// `{...config, selectedServerId: best.id}`——喂给 `switch_mode` 的新配置。
    pub new_config: Value,
    /// 切换成功后 emit 的事件 payload。
    pub payload: AutoNodeSwitchedPayload,
}

/// 由当前原始配置 + 选中的最优候选 + reason → 换节点方案（纯函数，上游 :226 + :243-247）。
///
/// `best.latency_ms` 必为 `Some`（[`select_best_candidate`] 已过滤不可达）；理论不可达的 `None` → 返 `None`
/// 防御。原始配置非对象 → 返 `None`（无从写 `selectedServerId`）。
#[must_use]
pub fn plan_switch(current_config: &Value, best: &CandidateLatency, reason: &str) -> Option<SwitchPlan> {
    let latency = best.latency_ms?;
    let mut new_config = current_config.clone();
    new_config
        .as_object_mut()?
        .insert("selectedServerId".to_string(), Value::String(best.id.clone()));
    Some(SwitchPlan {
        new_config,
        payload: AutoNodeSwitchedPayload {
            reason: reason.to_string(),
            new_server_name: best.name.clone(),
            latency,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── on_heartbeat：触发阈值真值表（别过度触发 / 别欠触发）──

    #[test]
    fn heartbeat_alive_when_no_prior_failures_is_stable() {
        let mut m = AutoSwitchMachine::new();
        m.enable();
        assert_eq!(m.on_heartbeat(true), HeartbeatOutcome::Stable);
    }

    #[test]
    fn heartbeat_two_failures_do_not_trigger() {
        // 单次/两次瞬断不该切——别过度触发。
        let mut m = AutoSwitchMachine::new();
        m.enable();
        assert_eq!(m.on_heartbeat(false), HeartbeatOutcome::Failing { failures: 1 });
        assert_eq!(m.on_heartbeat(false), HeartbeatOutcome::Failing { failures: 2 });
    }

    #[test]
    fn heartbeat_third_consecutive_failure_triggers() {
        // 恰好第 3 次连续失败触发——变异：阈值 >= 改 > 会漏这次触发。
        let mut m = AutoSwitchMachine::new();
        m.enable();
        m.on_heartbeat(false);
        m.on_heartbeat(false);
        assert_eq!(m.on_heartbeat(false), HeartbeatOutcome::Trigger);
    }

    #[test]
    fn heartbeat_trigger_resets_failure_count() {
        // 触发后失败计数复位（上游 :142）——下一次失败重新从 1 计。
        let mut m = AutoSwitchMachine::new();
        m.enable();
        m.on_heartbeat(false);
        m.on_heartbeat(false);
        assert_eq!(m.on_heartbeat(false), HeartbeatOutcome::Trigger);
        assert_eq!(m.on_heartbeat(false), HeartbeatOutcome::Failing { failures: 1 });
    }

    #[test]
    fn heartbeat_alive_resets_failure_streak() {
        // 中途恢复联通 → 失败连击清零（别欠触发的对偶：也别把不连续的失败攒成触发）。
        let mut m = AutoSwitchMachine::new();
        m.enable();
        m.on_heartbeat(false);
        m.on_heartbeat(false);
        assert_eq!(m.on_heartbeat(true), HeartbeatOutcome::Recovered { prior: 2 });
        // 复位后重新从 1 计，不会因之前 2 次就触发。
        assert_eq!(m.on_heartbeat(false), HeartbeatOutcome::Failing { failures: 1 });
    }

    // ── evaluate_switch：冷却 / 熔断 / 在飞 真值表 ──

    #[test]
    fn gate_in_flight_blocks() {
        let mut m = AutoSwitchMachine::new();
        m.enable();
        m.begin_switch(1_000_000);
        assert_eq!(m.evaluate_switch(2_000_000), SwitchGate::InFlight);
    }

    #[test]
    fn gate_proceeds_when_clear() {
        let mut m = AutoSwitchMachine::new();
        m.enable();
        // last_switch_time=0，now 远超冷却 → 放行（首次切换）。
        assert_eq!(m.evaluate_switch(10_000_000), SwitchGate::Proceed);
    }

    #[test]
    fn gate_cooldown_blocks_within_window() {
        // 距上次换节点 30s < 60s 冷却 → 拦。变异：删冷却检查会误放行。
        let mut m = AutoSwitchMachine::new();
        m.enable();
        m.begin_switch(1_000_000);
        m.end_switch();
        match m.evaluate_switch(1_000_000 + 30_000) {
            SwitchGate::Cooldown { remaining_ms } => assert_eq!(remaining_ms, 30_000),
            other => panic!("期望 Cooldown，实际 {other:?}"),
        }
    }

    #[test]
    fn gate_proceeds_after_cooldown_window() {
        let mut m = AutoSwitchMachine::new();
        m.enable();
        m.begin_switch(1_000_000);
        m.end_switch();
        // 距上次 60s+ → 冷却结束，放行。
        assert_eq!(
            m.evaluate_switch(1_000_000 + 60_001),
            SwitchGate::Proceed
        );
    }

    #[test]
    fn gate_breaker_trips_after_max_switches() {
        // 连续切换达上限 + 未过熔断冷却 → 熔断拦。变异：删熔断检查会在整体网络故障时空转。
        let mut m = AutoSwitchMachine::new();
        m.enable();
        // 模拟 3 次成功切换记账。
        m.record_switch_success(1_000_000);
        m.record_switch_success(1_000_000);
        m.record_switch_success(1_000_000); // 第 3 次 → breaker_tripped_at=1_000_000
        // 冷却窗内（+5min < 10min）且非在飞、且冷却已过（last_switch_time=0）→ 仍应被熔断拦。
        match m.evaluate_switch(1_000_000 + 5 * 60_000) {
            SwitchGate::Breaker { remaining_ms } => assert_eq!(remaining_ms, 5 * 60_000),
            other => panic!("期望 Breaker，实际 {other:?}"),
        }
    }

    #[test]
    fn gate_breaker_resets_and_proceeds_after_cooldown() {
        let mut m = AutoSwitchMachine::new();
        m.enable();
        m.record_switch_success(1_000_000);
        m.record_switch_success(1_000_000);
        m.record_switch_success(1_000_000);
        // 熔断冷却过后（+10min+1）→ 复位熔断 + 放行。
        let now = 1_000_000 + BREAKER_COOLDOWN_MS + 1;
        assert_eq!(m.evaluate_switch(now), SwitchGate::Proceed);
    }

    #[test]
    fn recovered_heartbeat_clears_breaker_count() {
        // 恢复联通 → 熔断计数清零（上游 :132）：随后连续失败触发时不再被残留计数熔断。
        let mut m = AutoSwitchMachine::new();
        m.enable();
        m.record_switch_success(1_000_000);
        m.record_switch_success(1_000_000);
        m.record_switch_success(1_000_000);
        m.on_heartbeat(true); // 恢复 → consecutive_switches 清零
        // 冷却也已过（用远后的 now），闸门应放行（熔断计数已清）。
        assert_eq!(m.evaluate_switch(20_000_000), SwitchGate::Proceed);
    }

    #[test]
    fn record_success_only_trips_breaker_at_threshold() {
        // 变异：把 record 的 >= 改成别的会让熔断时刻记错 → 第 3 次才置 breaker_tripped_at。
        let mut m = AutoSwitchMachine::new();
        m.enable();
        m.record_switch_success(500);
        m.record_switch_success(600);
        // 前两次：consecutive_switches<3 → 未熔断，冷却过后放行。
        assert_eq!(m.evaluate_switch(10_000_000), SwitchGate::Proceed);
    }

    // ── enable/disable 复位 ──

    #[test]
    fn enable_resets_counters() {
        let mut m = AutoSwitchMachine::new();
        m.enable();
        m.on_heartbeat(false);
        m.record_switch_success(1_000_000);
        m.disable();
        m.enable(); // 重新启用 → 复位
        assert_eq!(m.on_heartbeat(false), HeartbeatOutcome::Failing { failures: 1 });
    }

    #[test]
    fn enable_is_idempotent_no_reset_on_second_call() {
        // 幂等：已启用再 enable 不复位（否则轮询驱动的重复 enable 会抹掉进行中的失败连击 → 永不触发）。
        let mut m = AutoSwitchMachine::new();
        m.enable();
        m.on_heartbeat(false);
        m.on_heartbeat(false);
        m.enable(); // 已启用 → no-op，不复位
        assert_eq!(m.on_heartbeat(false), HeartbeatOutcome::Trigger);
    }

    #[test]
    fn reset_failures_only_keeps_breaker_count() {
        // 核未运行分支：只清失败、不清熔断（上游 :107-110）。
        let mut m = AutoSwitchMachine::new();
        m.enable();
        m.record_switch_success(1_000_000);
        m.record_switch_success(1_000_000);
        m.record_switch_success(1_000_000);
        m.on_heartbeat(false);
        m.reset_failures_only();
        // 失败清零：下一失败从 1 计。
        assert_eq!(m.on_heartbeat(false), HeartbeatOutcome::Failing { failures: 1 });
        // 熔断计数未清：仍被熔断拦。
        assert!(matches!(
            m.evaluate_switch(1_000_000 + 60_001),
            SwitchGate::Breaker { .. }
        ));
    }

    // ── extract_candidates ──

    #[test]
    fn extract_candidates_excludes_current() {
        let cfg = json!({
            "selectedServerId": "a",
            "servers": [
                { "id": "a", "name": "A", "address": "1.1.1.1", "port": 443 },
                { "id": "b", "name": "B", "address": "2.2.2.2", "port": 8443 },
            ]
        });
        let cands = extract_candidates(&cfg, Some("a"));
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].id, "b");
        assert_eq!(cands[0].name, "B");
        assert_eq!(cands[0].address, "2.2.2.2");
        assert_eq!(cands[0].port, 8443);
    }

    #[test]
    fn extract_candidates_missing_servers_is_empty() {
        assert!(extract_candidates(&json!({}), Some("a")).is_empty());
    }

    #[test]
    fn extract_candidates_name_falls_back_to_id() {
        let cfg = json!({ "servers": [ { "id": "x", "address": "h", "port": 1 } ] });
        let cands = extract_candidates(&cfg, None);
        assert_eq!(cands[0].name, "x");
    }

    // ── select_best_candidate：下一节点选择决策 ──

    fn cand(id: &str, lat: Option<u32>) -> CandidateLatency {
        CandidateLatency {
            id: id.to_string(),
            name: format!("name-{id}"),
            latency_ms: lat,
        }
    }

    #[test]
    fn select_picks_lowest_latency() {
        let list = vec![cand("a", Some(120)), cand("b", Some(40)), cand("c", Some(80))];
        assert_eq!(select_best_candidate(&list).unwrap().id, "b");
    }

    #[test]
    fn select_skips_unreachable() {
        // 变异：不过滤 None 会把不可达当最优 → 切到死节点。
        let list = vec![cand("a", None), cand("b", Some(200))];
        assert_eq!(select_best_candidate(&list).unwrap().id, "b");
    }

    #[test]
    fn select_none_when_all_unreachable() {
        let list = vec![cand("a", None), cand("b", None)];
        assert!(select_best_candidate(&list).is_none());
    }

    #[test]
    fn select_empty_is_none() {
        assert!(select_best_candidate(&[]).is_none());
    }

    #[test]
    fn select_ties_take_first() {
        let list = vec![cand("a", Some(50)), cand("b", Some(50))];
        assert_eq!(select_best_candidate(&list).unwrap().id, "a");
    }

    // ── plan_switch：新配置 + emit payload ──

    #[test]
    fn plan_sets_selected_server_and_payload() {
        // 变异：把 selectedServerId 写成别的 id / payload 用错 name/latency 都被此测抓。
        let cfg = json!({ "selectedServerId": "old", "servers": [], "proxyMode": "tun" });
        let best = cand("new-id", Some(42));
        let plan = plan_switch(&cfg, &best, "连通性检测").unwrap();
        assert_eq!(
            plan.new_config.get("selectedServerId").and_then(Value::as_str),
            Some("new-id")
        );
        // 其余字段保留。
        assert_eq!(
            plan.new_config.get("proxyMode").and_then(Value::as_str),
            Some("tun")
        );
        assert_eq!(plan.payload.reason, "连通性检测");
        assert_eq!(plan.payload.new_server_name, "name-new-id");
        assert_eq!(plan.payload.latency, 42);
    }

    #[test]
    fn plan_none_when_candidate_unreachable() {
        let cfg = json!({ "selectedServerId": "old" });
        assert!(plan_switch(&cfg, &cand("x", None), "r").is_none());
    }

    #[test]
    fn plan_none_when_config_not_object() {
        assert!(plan_switch(&json!("not-an-object"), &cand("x", Some(1)), "r").is_none());
    }

    #[test]
    fn payload_serializes_camel_case() {
        let p = AutoNodeSwitchedPayload {
            reason: "连通性检测".to_string(),
            new_server_name: "东京-01".to_string(),
            latency: 88,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v.get("newServerName").and_then(Value::as_str), Some("东京-01"));
        assert_eq!(v.get("reason").and_then(Value::as_str), Some("连通性检测"));
        assert_eq!(v.get("latency").and_then(Value::as_u64), Some(88));
    }
}
