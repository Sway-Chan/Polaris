//! 崩溃自愈状态机 —— 上游 `attemptAutoRestart` / `shouldAutoRestart` / `handleProcessExit` 的
//! 纯逻辑移植（维度7 #5/#6，源 `ProxyManager.ts:5888-6064`）。
//!
//! 本模块只做「崩溃 → 是否重启 / 退避多久 / 是否让位 / 是否补发」的决策状态机，不直接 spawn/await——
//! 上层 actor 拿决策后执行真正的退避 sleep + start()，再把结果反馈回来。这样退避/世代/supersede 补发
//! 均可零进程确定性单测（维度7 要求）。
//!
//! 不变式（capability-registry-special-logic.md §1 #5/#6）：
//! 5. `is_restarting` 幂等去重（双触发只跑一次）；退避第 1 次 2s / 第 2 次 5s / 第 3 次 15s；
//!    退避前后均须检 `auto_restarted_aborted` 与 `superseded`；start 后若让位须直接退场不报「成功」。
//! 6. supersede 让位时用 `crash_while_superseded`（仅真崩溃置位）判是否补发一次；
//!    补发前 schedGen + refs 双守卫（被接管/已起来则不补）。
//!
//! 锚点行号：shouldAutoRestart:5888 / attemptAutoRestart:5942 / backoff:5968 /
//!           supersede let-go:5995 / crashWhileSuperseded 补发:5998-6009 / handleProcessExit:7653-7665。

use std::time::Duration;

/// 上游 `ProxyManager.MAX_RESTART_COUNT = 3`（:550）。
pub const MAX_RESTART_COUNT: u32 = 3;
/// 上游 `ProxyManager.RESTART_COOLDOWN = 60000ms`（:551，1 分钟内最多重启 3 次）。
pub const RESTART_COOLDOWN: Duration = Duration::from_secs(60);

/// 退避表（attemptAutoRestart :5968）。第 1 次 2s、第 2 次 5s、第 3 次（及之后）15s。
///
/// 固定 2s 会纯刷屏（端口/helper 等外部资源需时间释放）。Polaris 注释原文：
/// "退避：第 1 次 2s、第 2 次 5s、第 3 次 15s——端口/helper 等外部资源需时间释放，固定 2s 纯刷屏。"
pub fn restart_backoff_ms(restart_count: u32) -> u64 {
    match restart_count {
        1 => 2_000,
        2 => 5_000,
        _ => 15_000,
    }
}

/// 等价 Duration 形式。
pub fn restart_backoff(restart_count: u32) -> Duration {
    Duration::from_millis(restart_backoff_ms(restart_count))
}

/// 崩溃自愈配置（默认对齐 Polaris 常量，可注入用于测试）。
#[derive(Debug, Clone, Copy)]
pub struct CrashRecoveryConfig {
    pub max_restart_count: u32,
    pub cooldown: Duration,
    /// 是否允许自动重启（autoRestartEnabled，shouldAutoRestart 前置）。
    pub auto_restart_enabled: bool,
    /// 核心更新待验证窗口内抑制自动重启（autoRestartSuppressed，:5893）。
    pub auto_restart_suppressed: bool,
}

impl Default for CrashRecoveryConfig {
    fn default() -> Self {
        Self {
            max_restart_count: MAX_RESTART_COUNT,
            cooldown: RESTART_COOLDOWN,
            auto_restart_enabled: true,
            auto_restart_suppressed: false,
        }
    }
}

/// 一次崩溃决策的结局（attemptAutoRestart 的各分支出口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoRestartOutcome {
    /// 不重启（shouldAutoRestart 拒绝：auto-restart 关 / 抑制窗口 / 达上限 / 冷却后计数仍超）。
    /// 上层走 error 上报 + 终态 cleanup。
    GiveUp,
    /// isRestarting 幂等去重命中：已有重启在途，本次触发静默吞掉（:5944）。
    Dedup,
    /// 用户已主动停止 → finalizeUserAbortedRestart 终态收尾（M-3，:5988/5954）。
    /// `crash_while_superseded` 已被复位（用户已停不需补发）。
    AbortedByUser,
    /// 开始一次重启：先退避 `backoff`，退避后若仍有效则 start。退避前后由调用方再调
    /// [`CrashRecoveryMachine::post_backoff`] 决定是否真起核。
    Attempt {
        /// 第几次（1-based，与 MAX_RESTART_COUNT 比较）。
        attempt: u32,
        backoff: Duration,
        /// 进入本次 attempt 时快照的世代（supersede 比对基线，:5960）。
        generation: u64,
    },
}

/// 退避后判定的结局（attemptAutoRestart await sleep 后的分支，:5987-6014）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartFate {
    /// 退避期间用户主动停止（autoRestartAborted）→ 终态收尾（M-3）。
    AbortedByUser,
    /// 退避期间被更新的 start/stop 接管（generation 变化，M-2′）→ 让位。
    /// `replay` = Some 表示接管会话在退避期内崩溃（crashWhileSuperseded），需补发一次 attemptAutoRestart（M-2′-G1）。
    Superseded { replay: bool },
    /// 未被接管、未被停 → 真正执行 start()。start 后调用方须再调
    /// [`CrashRecoveryMachine::post_start`] 判 `lastStartSuperseded`（让位则不报成功，:6021）。
    Start,
}

/// 崩溃自愈状态机。状态经 `&mut self` 同步操作（与 Polaris 主线程同步语义对齐）。
#[derive(Debug)]
pub struct CrashRecoveryMachine {
    cfg: CrashRecoveryConfig,
    /// restartCount（:548）。冷却后复位。
    restart_count: u32,
    /// lastRestartTime（epoch ms）。0=从未。
    last_restart_ms: u64,
    /// isRestarting（幂等去重，:5944）。true=有重启腿在途。
    is_restarting: bool,
    /// 在途腿的世代快照（restartingGen，:5963）。
    restarting_gen: u64,
    /// autoRestartAborted（用户主动停止，:358）。
    auto_restart_aborted: bool,
    /// crashWhileSuperseded（M-2′-G1 补发标记，:7658-7659）。仅 handle_crash 真崩溃置位。
    crash_while_superseded: bool,
}

impl Default for CrashRecoveryMachine {
    fn default() -> Self {
        Self::new(CrashRecoveryConfig::default())
    }
}

impl CrashRecoveryMachine {
    pub fn new(cfg: CrashRecoveryConfig) -> Self {
        Self {
            cfg,
            restart_count: 0,
            last_restart_ms: 0,
            is_restarting: false,
            restarting_gen: 0,
            auto_restart_aborted: false,
            crash_while_superseded: false,
        }
    }

    /// 当前 restart_count（诊断用，:548）。
    pub fn restart_count(&self) -> u32 {
        self.restart_count
    }

    /// 是否有重启腿在途（isRestarting）。
    pub fn is_restarting(&self) -> bool {
        self.is_restarting
    }

    /// 在途重启腿的世代快照（restartingGen，:5963）；**无在途腿返 `None`**。
    ///
    /// 供 [`Self::handle_crash`] 的 `restarting_gen_of_inflight` 实参：崩溃自愈的 `run_crash_recovery`
    /// 读回本值再喂 `handle_crash`，判「在途腿是否已被接管」（世代不等）置 `crash_while_superseded`
    /// → 让位腿醒来补发（M-2′-G1）。上游侧 `restartingGen` 是 ProxyManager 字段（:5879 置 myGen /
    /// :5967 清 -1），`handleProcessExit`（:7517）读 `this.restartingGen`；本机把它归到状态机内，
    /// 以本 getter 暴露给上层 wiring（此前上层硬编码 `None` → 新代核崩溃永不补发 → 无人接管）。
    pub fn restarting_gen(&self) -> Option<u64> {
        if self.is_restarting {
            Some(self.restarting_gen)
        } else {
            None
        }
    }

    /// 用户主动停止标记是否已置（autoRestartAborted）。
    pub fn auto_restart_aborted(&self) -> bool {
        self.auto_restart_aborted
    }

    /// 用户主动停止（stop() 入口置位，M-3）。令退避中的 attempt 放弃重启。
    pub fn mark_user_aborted(&mut self) {
        self.auto_restart_aborted = true;
    }

    /// 新一次 start() 接管时复位 abort 标记（Polaris start() 入口 reset，使接管 start 不被遗留 abort 拦下）。
    pub fn reset_user_aborted(&mut self) {
        self.auto_restart_aborted = false;
    }

    /// 读 `autoRestartSuppressed`（换核验证窗口）。
    #[must_use]
    pub fn auto_restart_suppressed(&self) -> bool {
        self.cfg.auto_restart_suppressed
    }

    /// 置 `autoRestartSuppressed`（上游 `setAutoRestartSuppressed`，:5893 的**写侧**）。
    ///
    /// 🔴 **这是本字段唯一的生产写入口**（2026-08-05 补）。在此之前 `auto_restart_suppressed` 只有
    /// 默认值 `false`、[`should_auto_restart`](Self::should_auto_restart) 里的判据、以及一条直接构造
    /// config 的单测 —— 也就是「机制移植了、接线没接」：全仓没有任何生产代码能把它置 true，
    /// 那条单测却照样绿（它绕过了写入口，直接造了个 `auto_restart_suppressed: true` 的 config）。
    ///
    /// 语义：窗口内核**意外退出不自动重启**，让首次失败立刻上报，而不是在坏核上退避空转 3 次 ——
    /// 空转会把「新核有问题」这个信号淹掉，正是换核验证最需要的那一条信息。
    pub fn set_auto_restart_suppressed(&mut self, suppressed: bool) {
        self.cfg.auto_restart_suppressed = suppressed;
    }

    /// shouldAutoRestart（:5888）。`now_ms` 为当前 epoch ms。
    ///
    /// - auto_restart 关 / 抑制窗口 → false。
    /// - 距上次重启超冷却 → 复位 restart_count=0。
    /// - restart_count >= max → false。
    pub fn should_auto_restart(&mut self, now_ms: u64) -> bool {
        if !self.cfg.auto_restart_enabled || self.cfg.auto_restart_suppressed {
            return false;
        }
        // 冷却后复位（:5900）。
        if now_ms.saturating_sub(self.last_restart_ms) > self.cfg.cooldown.as_millis() as u64 {
            self.restart_count = 0;
        }
        self.restart_count < self.cfg.max_restart_count
    }

    /// handleProcessExit（:7653）的崩溃分支决策入口：进程异常退出时调此。
    ///
    /// `now_ms` 当前 epoch ms；`current_generation` 当前生命周期世代；`restarting_gen_of_inflight`
    /// 是若已有在途重启腿时的世代快照（None=无在途腿）——用于判 crashWhileSuperseded（:7658）。
    ///
    /// 返回 [`AutoRestartOutcome`] 指导上层动作。关键：若已有在途腿但它已被接管（世代不等），
    /// 置 `crash_while_superseded` 让那条腿醒来补发（M-2′-G1）。
    pub fn handle_crash(
        &mut self,
        now_ms: u64,
        current_generation: u64,
        restarting_gen_of_inflight: Option<u64>,
    ) -> AutoRestartOutcome {
        // 若已有在途重启腿但它已被接管 → 标记崩溃，让那条腿醒来补发（:7658-7659）。
        // 此处本崩溃信号会被 attempt_crash 的 is_restarting dedup 吞掉（无害），真正恢复由让位腿消费。
        if self.is_restarting {
            if let Some(inflight_gen) = restarting_gen_of_inflight {
                if inflight_gen != current_generation {
                    // 在途腿已被接管 + 本次崩溃来自接管会话 → 标记补发。
                    self.crash_while_superseded = true;
                }
            }
        }

        self.attempt_crash(now_ms, current_generation)
    }

    /// attemptAutoRestart 入口（:5942）。
    ///
    /// 幂等去重 + 用户停止 + 退避计算。返回 [`AutoRestartOutcome`]。调用方据此：
    /// - GiveUp → error 上报 + 终态。
    /// - Dedup / AbortedByUser → 静默。
    /// - Attempt { backoff, generation } → 执行退避 sleep，然后调 [`Self::post_backoff`]。
    pub fn attempt_crash(&mut self, now_ms: u64, current_generation: u64) -> AutoRestartOutcome {
        // 幂等去重（:5944）：双触发只跑一次。
        if self.is_restarting {
            return AutoRestartOutcome::Dedup;
        }
        // 用户已停（M-3）：finalizeUserAbortedRestart 终态收尾。
        if self.auto_restart_aborted {
            self.crash_while_superseded = false; // 用户已停不需补发（:6071）。
            return AutoRestartOutcome::AbortedByUser;
        }
        if !self.should_auto_restart(now_ms) {
            return AutoRestartOutcome::GiveUp;
        }

        // 进入一次 attempt：置位 + 计数 + 记世代（:5960-5965）。
        self.is_restarting = true;
        self.restarting_gen = current_generation;
        self.restart_count += 1;
        self.last_restart_ms = now_ms;

        AutoRestartOutcome::Attempt {
            attempt: self.restart_count,
            backoff: restart_backoff(self.restart_count),
            generation: current_generation,
        }
    }

    /// 退避后（attemptAutoRestart await sleep 后，:5987）判定真正是否起核。
    ///
    /// `my_gen` = 进入本次 attempt 时快照的世代（Attempt.generation）。
    /// `current_generation` = 退避后当前世代。
    pub fn post_backoff(&mut self, my_gen: u64, current_generation: u64) -> RestartFate {
        // 退避期间用户已停 → 放弃（M-3，:5988）。复位 is_restarting 由 finalize 完成。
        if self.auto_restart_aborted {
            self.finalize_inflight();
            self.crash_while_superseded = false;
            return RestartFate::AbortedByUser;
        }
        // 退避期间被接管 → 让位（M-2′，:5995）。
        if current_generation != my_gen {
            // 补发判定（M-2′-G1，:5998）：仅真崩溃（crash_while_superseded）才补发。
            let replay = self.crash_while_superseded;
            self.crash_while_superseded = false;
            self.finalize_inflight();
            return RestartFate::Superseded { replay };
        }
        // 未被接管 → 真正 start（:6017）。
        RestartFate::Start
    }

    /// start() 完成后判定：是否因就绪等待期被接管而让位（lastStartSuperseded，:6021）。
    ///
    /// 返回 true = 让位（不报「自动重启成功」、不发陈旧 started 事件）。
    /// 无论是否让位，本次 in-flight 腿都结束（finalize is_restarting）。
    pub fn post_start(&mut self, last_start_superseded: bool) -> PostStartOutcome {
        self.finalize_inflight();
        if last_start_superseded {
            PostStartOutcome::Superseded
        } else {
            PostStartOutcome::Restarted
        }
    }

    /// start 失败后（attemptAutoRestart catch，:6034）。
    ///
    /// - 不可恢复错误 / 已达上限 → GiveUp（:6045）。
    /// - 否则 → Retry（:6047，自循环调度下一次 attempt，退避在下一次 attempt 内按计数算）。
    pub fn post_start_failure(&mut self, unrecoverable: bool) -> FailureOutcome {
        self.finalize_inflight();
        if unrecoverable || self.restart_count >= self.cfg.max_restart_count {
            FailureOutcome::GiveUp
        } else {
            FailureOutcome::Retry
        }
    }

    /// 退避中 start 失败的调度判定：补发的 attempt_crash 须在 is_restarting 已复位后 fire
    /// （:6054-6062）。返回 true=可补发一次 attempt_crash。
    ///
    /// 调用方典型用法（retryScheduled 分支，:6057）：
    /// ```ignore
    /// let sched_gen = current_generation;
    /// // ... macrotask 缝隙 ...
    /// if machine.generation_changed_since(sched_gen, current_generation) { return; }
    /// machine.attempt_crash(now_ms, current_generation);
    /// ```
    pub fn can_retry(&self) -> bool {
        !self.is_restarting && self.restart_count < self.cfg.max_restart_count
    }

    /// 补发 attemptAutoRestart 前的 schedGen + refs 双守卫（M-2′-G1，:6004-6008）。
    ///
    /// `sched_gen` = 调度补发时快照的世代；`current_generation` = fire 时当前世代；
    /// `core_running` = refs 非空（已起来）。任一守卫失败 → 不补发（返回 false）。
    pub fn should_replay_crash(
        &self,
        sched_gen: u64,
        current_generation: u64,
        core_running: bool,
    ) -> bool {
        if current_generation != sched_gen {
            return false; // 已被更新的 start/stop 接管。
        }
        if core_running {
            return false; // 期间已起来。
        }
        true
    }

    /// 复位 in-flight 腿状态（finally 块，:6050-6051）。
    fn finalize_inflight(&mut self) {
        self.is_restarting = false;
        self.restarting_gen = 0;
    }

    /// 显式结束生命周期（stop 终态）：复位所有在途状态，丢弃 crash_while_superseded。
    pub fn finalize_for_stop(&mut self) {
        self.finalize_inflight();
        self.auto_restart_aborted = false;
        self.crash_while_superseded = false;
    }
}

/// start 完成后的结局（post_start）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostStartOutcome {
    /// 真正重启成功。
    Restarted,
    /// 就绪等待期被接管 → 让位，不报成功（:6021）。
    Superseded,
}

/// start 失败后的结局（post_start_failure）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureOutcome {
    /// 不可恢复 / 达上限 → 终态 giveUpAutoRestart。
    GiveUp,
    /// 可重试 → 自循环调度下一次 attempt。
    Retry,
}

/// 后台崩溃监测任务对「运行中的核」子进程句柄的一次观察结果。
///
/// tokio `Child::wait()` 是单持有者的（`&mut self`），而主动停止路径（`kill_core`）已经持有并
/// `wait()` 那个句柄——故崩溃监测不能也去 `wait()`，只能**轮询** `try_wait` 观察句柄状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildObservation {
    /// `try_wait()==Ok(None)`：进程仍在跑。
    Alive,
    /// `try_wait()==Ok(Some(_))` 或探活出错：进程已退出（本任务收割）。
    Exited,
    /// 句柄已被取走（`kill_core` 的 `take()`）→ 主动停止/重启正在进行。
    Absent,
}

/// 崩溃监测对一次观察的分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitClassification {
    /// 核仍健康 → 继续监测。
    KeepWatching,
    /// 主动 stop/restart 已接管（世代已变 / 句柄被取走）→ 本监测任务退场，**不**触发自愈。
    Retire,
    /// 核**意外**退出（世代未变 + 句柄仍在 + 进程已退）→ 触发崩溃自愈。
    Crash,
}

/// 区分「核意外崩溃」与「主动 stop/restart 杀核」—— **本任务最容易出的 bug 的唯一判据**。
///
/// 判据完全建立在 [`LifecycleGate`](crate::LifecycleGate) 的**世代**上（`stop`/`restart` 入口
/// 必先 `bump_generation()`，再 `kill_core` 取句柄）：
/// - **世代已变** ⟹ 有更新的 start/stop/restart 接管 ⟹ 这次退出是「主动杀核 / 让位」，**绝非崩溃** → `Retire`。
///   （主动停止路径的 SIGTERM/SIGKILL 恰好落在此分支——世代在杀核之前就已 +1。）
/// - 世代未变但**句柄已被取走** ⟹ `kill_core` 正在跑（防御性冗余，正常情况下取句柄前世代已变）→ `Retire`。
/// - 世代未变 + 句柄仍在 + 进程仍活 → `KeepWatching`。
/// - 世代未变 + 句柄仍在 + **进程已退** ⟹ 无任何生命周期操作在飞、核却死了 → `Crash`。
///
/// **变异防线**：去掉「世代已变 → Retire」这一守卫（即把主动 stop 也当崩溃）→
/// [`intentional_stop_bumped_generation_is_retire`](self) 立即转红。
pub fn classify_child_exit(
    my_generation: u64,
    current_generation: u64,
    observation: ChildObservation,
) -> ExitClassification {
    if current_generation != my_generation {
        // 更新的 start/stop/restart 接管（含主动停止：其入口先 bump 世代再杀核）→ 不是崩溃。
        return ExitClassification::Retire;
    }
    match observation {
        ChildObservation::Absent => ExitClassification::Retire,
        ChildObservation::Alive => ExitClassification::KeepWatching,
        ChildObservation::Exited => ExitClassification::Crash,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;

    const NOW: u64 = 1_000_000; // 任意 epoch ms 基线。

    #[test]
    fn backoff_table_matches_polaris_constants() {
        // #5：第 1 次 2s / 第 2 次 5s / 第 3 次 15s（:5968）。
        assert_eq!(restart_backoff_ms(1), 2_000);
        assert_eq!(restart_backoff_ms(2), 5_000);
        assert_eq!(restart_backoff_ms(3), 15_000);
        // 第 4 次（若 max 调高）仍 15s（capped）。
        assert_eq!(restart_backoff_ms(4), 15_000);
        // 无效 0 → 兜底 15s（match _）。
        assert_eq!(restart_backoff_ms(0), 15_000);
    }

    #[test]
    fn dedup_second_crash_during_inflight_returns_dedup() {
        // #5：进程 exit 事件与健康检查双触发 → 只跑一次。
        let mut m = CrashRecoveryMachine::default();
        let r1 = m.attempt_crash(NOW, 1);
        let AutoRestartOutcome::Attempt { attempt, .. } = r1 else {
            panic!("expected Attempt, got {r1:?}")
        };
        assert_eq!(attempt, 1);
        assert!(m.is_restarting());

        // 第二次触发：dedup。
        let r2 = m.attempt_crash(NOW, 1);
        assert_eq!(r2, AutoRestartOutcome::Dedup);
        assert!(m.is_restarting()); // 仍在途
    }

    #[test]
    fn aborted_by_user_short_circuits_before_attempt() {
        // M-3：用户已停 → 终态，不起核。auto_restart_aborted 保持置位直到新 start 复位
        // （Polaris finalizeUserAbortedRestart 不清该标志；由下次 start() 入口 reset）。
        let mut m = CrashRecoveryMachine::default();
        m.mark_user_aborted();
        assert!(m.auto_restart_aborted());
        let r = m.attempt_crash(NOW, 1);
        assert_eq!(r, AutoRestartOutcome::AbortedByUser);
        assert!(!m.is_restarting());
        // 标志保持（仅 reset_user_aborted / 新 start 复位）。
        assert!(m.auto_restart_aborted());

        // 后续崩溃仍走 AbortedByUser（不重启）。
        let r2 = m.attempt_crash(NOW, 1);
        assert_eq!(r2, AutoRestartOutcome::AbortedByUser);

        // 新 start 复位后恢复自动重启。
        m.reset_user_aborted();
        assert!(!m.auto_restart_aborted());
        let r3 = m.attempt_crash(NOW, 1);
        assert!(matches!(r3, AutoRestartOutcome::Attempt { .. }));
    }

    #[test]
    fn give_up_when_disabled() {
        let cfg = CrashRecoveryConfig {
            auto_restart_enabled: false,
            ..CrashRecoveryConfig::default()
        };
        let mut m = CrashRecoveryMachine::new(cfg);
        let r = m.attempt_crash(NOW, 1);
        assert_eq!(r, AutoRestartOutcome::GiveUp);
    }

    #[test]
    fn give_up_when_suppressed_during_core_update_window() {
        // :5893 核心更新待验证窗口内禁止自动重启。
        let cfg = CrashRecoveryConfig {
            auto_restart_suppressed: true,
            ..CrashRecoveryConfig::default()
        };
        let mut m = CrashRecoveryMachine::new(cfg);
        let r = m.attempt_crash(NOW, 1);
        assert_eq!(r, AutoRestartOutcome::GiveUp);
    }

    /// ⚠️ 上面那条测试**绕过了写入口**（直接造 `auto_restart_suppressed: true` 的 config），
    /// 所以在「全仓没有任何生产代码能置起这个位」的整段时期里它照样绿 —— 它验的是判据，
    /// 不是「这条腿真的能被打开」。本条走 [`CrashRecoveryMachine::set_auto_restart_suppressed`]
    /// 这个真实写入口，且验来回两个方向。
    #[test]
    fn suppression_toggles_through_the_real_setter() {
        // 每次崩溃后必须把在途重启腿收尾（post_backoff + post_start），否则 `is_restarting`
        // 仍为真，下一次 attempt_crash 会先命中幂等去重返回 Dedup、**根本到不了**
        // should_auto_restart —— 抑制位是否生效就无从断言。
        fn crash_and_settle(m: &mut CrashRecoveryMachine) -> AutoRestartOutcome {
            let outcome = m.attempt_crash(NOW, 1);
            if let AutoRestartOutcome::Attempt { generation, .. } = outcome {
                assert!(matches!(m.post_backoff(generation, 1), RestartFate::Start));
                let _ = m.post_start(false);
            }
            outcome
        }

        let mut m = CrashRecoveryMachine::default();
        assert!(!m.auto_restart_suppressed(), "默认不抑制");
        assert!(
            matches!(crash_and_settle(&mut m), AutoRestartOutcome::Attempt { .. }),
            "未抑制时崩溃应走自愈重启"
        );

        // 置起 → 下一次崩溃必须 GiveUp（不再退避空转 3 次把首次失败信号淹掉）。
        m.set_auto_restart_suppressed(true);
        assert!(m.auto_restart_suppressed());
        assert_eq!(crash_and_settle(&mut m), AutoRestartOutcome::GiveUp);

        // 撤下 → 自愈能力必须完整回来（回滚回去的老核要照常受崩溃保护）。
        m.set_auto_restart_suppressed(false);
        assert!(!m.auto_restart_suppressed());
        assert!(
            matches!(crash_and_settle(&mut m), AutoRestartOutcome::Attempt { .. }),
            "撤下抑制后自愈必须恢复，否则回滚回去的老核将失去崩溃保护"
        );
    }

    #[test]
    fn max_restart_count_enforced_across_attempts() {
        // #5：达 MAX_RESTART_COUNT=3 后 GiveUp。
        let mut m = CrashRecoveryMachine::default();
        let gen = 1u64;
        for expected_attempt in 1..=3 {
            let r = m.attempt_crash(NOW, gen);
            let AutoRestartOutcome::Attempt { attempt, .. } = r else {
                panic!("attempt {expected_attempt}: expected Attempt, got {r:?}")
            };
            assert_eq!(attempt, expected_attempt);
            // 模拟 start 失败 → Retry（除最后一次）。
            let fate = m.post_start_failure(false);
            if expected_attempt < 3 {
                assert_eq!(fate, FailureOutcome::Retry);
            } else {
                assert_eq!(fate, FailureOutcome::GiveUp); // 达上限
            }
        }
        // 第 4 次崩溃 → GiveUp（计数已到 3）。
        let r = m.attempt_crash(NOW, gen);
        assert_eq!(r, AutoRestartOutcome::GiveUp);
    }

    #[test]
    fn cooldown_resets_restart_count() {
        // #5：距上次重启超 cooldown(60s) → 复位 restart_count。
        let mut m = CrashRecoveryMachine::default();
        m.attempt_crash(NOW, 1);
        m.post_start_failure(false); // Retry → is_restarting 复位
        assert_eq!(m.restart_count(), 1);

        // 60s+ 后 → should_auto_restart 复位计数。
        let later = NOW + 61_000;
        assert!(m.should_auto_restart(later));
        assert_eq!(m.restart_count(), 0); // 已复位
    }

    #[test]
    fn post_backoff_aborted_by_user_after_grace() {
        // M-3：退避期间用户停止 → 放弃。
        let mut m = CrashRecoveryMachine::default();
        let r = m.attempt_crash(NOW, 5);
        let AutoRestartOutcome::Attempt { generation, .. } = r else {
            panic!("expected Attempt")
        };
        // 退避期间用户停。
        m.mark_user_aborted();
        let fate = m.post_backoff(generation, 5);
        assert_eq!(fate, RestartFate::AbortedByUser);
        assert!(!m.is_restarting());
    }

    #[test]
    fn post_backoff_superseded_no_replay_when_clean_takeover() {
        // M-2′：退避期间被 start 接管（generation 变化）→ 让位，无崩溃则不补发。
        let mut m = CrashRecoveryMachine::default();
        let r = m.attempt_crash(NOW, 5);
        let AutoRestartOutcome::Attempt { generation, .. } = r else {
            panic!("expected Attempt")
        };
        // 接管：generation 已变（用户手动 start）。
        let fate = m.post_backoff(generation, 6);
        assert_eq!(fate, RestartFate::Superseded { replay: false });
        assert!(!m.is_restarting());
    }

    #[test]
    fn post_backoff_superseded_replays_when_takeover_session_crashed() {
        // #6 M-2′-G1：接管 start 在退避期内崩溃 → crash_while_superseded 置位 → 补发。
        let mut m = CrashRecoveryMachine::default();
        // 第一条腿：attempt 起核（gen=5）。
        let r = m.attempt_crash(NOW, 5);
        let AutoRestartOutcome::Attempt {
            generation: my_gen, ..
        } = r
        else {
            panic!("expected Attempt")
        };
        assert_eq!(my_gen, 5);

        // 退避期内：接管 start 发生（generation 变 6），且接管会话崩溃。
        // handle_crash 检测到在途腿（gen=5）已被接管（current=6）→ 置 crash_while_superseded。
        m.handle_crash(NOW + 100, 6, Some(my_gen));
        assert!(m.is_restarting()); // 第一条腿仍在退避中（dedup 吞掉本次崩溃）

        // 第一条腿退避完 → post_backoff 判 supersede + replay=true。
        let fate = m.post_backoff(my_gen, 6);
        assert_eq!(fate, RestartFate::Superseded { replay: true });
        assert!(!m.is_restarting());
    }

    #[test]
    fn replay_double_guard_generation_and_core_running() {
        // #6：补发前 schedGen + refs 双守卫（:6004-6008）。
        let m = CrashRecoveryMachine::default();
        let sched_gen = 6u64;
        // 世代未变 + 核未起来 → 可补发。
        assert!(m.should_replay_crash(sched_gen, 6, false));
        // 世代已变（又被接管）→ 不补。
        assert!(!m.should_replay_crash(sched_gen, 7, false));
        // 核已起来 → 不补。
        assert!(!m.should_replay_crash(sched_gen, 6, true));
    }

    #[test]
    fn post_start_superseded_does_not_report_success() {
        // #5：start 后 lastStartSuperseded=true → 直接退场，不报成功（:6021）。
        let mut m = CrashRecoveryMachine::default();
        m.attempt_crash(NOW, 1);
        let out = m.post_start(true);
        assert_eq!(out, PostStartOutcome::Superseded);
        assert!(!m.is_restarting());
    }

    #[test]
    fn post_start_restarted_reports_success() {
        let mut m = CrashRecoveryMachine::default();
        m.attempt_crash(NOW, 1);
        let out = m.post_start(false);
        assert_eq!(out, PostStartOutcome::Restarted);
        assert!(!m.is_restarting());
    }

    #[test]
    fn post_start_failure_unrecoverable_gives_up_immediately() {
        // :6043：不可恢复错误 → 立即终态（即使未达上限）。
        let mut m = CrashRecoveryMachine::default();
        m.attempt_crash(NOW, 1);
        let fate = m.post_start_failure(true);
        assert_eq!(fate, FailureOutcome::GiveUp);
    }

    #[test]
    fn can_retry_only_when_not_inflight_and_under_max() {
        let mut m = CrashRecoveryMachine::default();
        assert!(m.can_retry());
        m.attempt_crash(NOW, 1);
        assert!(!m.can_retry()); // in-flight
        m.post_start_failure(false);
        assert!(m.can_retry());
    }

    #[test]
    fn restarting_gen_getter_exposes_inflight_generation() {
        // M-2′-G1 补发依赖本 getter 返回真实在途世代（此前无 getter → 上层硬编码 None）。
        let mut m = CrashRecoveryMachine::default();
        // 无在途腿 → None（供 handle_crash 判「无接管、不补发」）。
        assert_eq!(m.restarting_gen(), None);
        // 进入 attempt（gen=5）→ 暴露 Some(5)。
        let _ = m.attempt_crash(NOW, 5);
        assert!(m.is_restarting());
        assert_eq!(m.restarting_gen(), Some(5));
        // in-flight 结束（post_start_failure Retry → finalize_inflight）→ 复位 None。
        let _ = m.post_start_failure(false);
        assert!(!m.is_restarting());
        assert_eq!(m.restarting_gen(), None);
    }

    #[test]
    fn handle_crash_with_no_inflight_does_not_set_replay_flag() {
        // 无在途腿时崩溃 → 不触 crash_while_superseded（语义：仅「接管会话死」才补发）。
        let mut m = CrashRecoveryMachine::default();
        let r = m.handle_crash(NOW, 5, None);
        // 无在途腿 → 直接进 attempt（非 dedup）。
        assert!(matches!(r, AutoRestartOutcome::Attempt { .. }));
    }

    #[test]
    fn handle_crash_inflight_same_generation_does_not_set_replay() {
        // 在途腿与崩溃同代（未接管）→ 不置 crash_while_superseded（正常重入，dedup 吞掉）。
        let mut m = CrashRecoveryMachine::default();
        m.attempt_crash(NOW, 5); // 在途腿 gen=5
        let r = m.handle_crash(NOW + 50, 5, Some(5)); // 同代崩溃
        assert_eq!(r, AutoRestartOutcome::Dedup);
    }

    #[test]
    fn finalize_for_stop_clears_all_inflight_state() {
        let mut m = CrashRecoveryMachine::default();
        m.attempt_crash(NOW, 1);
        m.mark_user_aborted();
        m.finalize_for_stop();
        assert!(!m.is_restarting());
        assert!(!m.auto_restart_aborted());
    }

    #[test]
    fn reset_user_aborted_allows_takeover_start() {
        // 接管 start 复位 abort（:5993 注释：用户手动 start 会 reset autoRestartAborted 绕过 abort 检查）。
        let mut m = CrashRecoveryMachine::default();
        m.mark_user_aborted();
        assert!(m.auto_restart_aborted());
        m.reset_user_aborted();
        let r = m.attempt_crash(NOW, 1);
        assert!(matches!(r, AutoRestartOutcome::Attempt { .. }));
    }

    // ── classify_child_exit：主动 stop vs 意外崩溃的判据（本任务最易出 bug 处的门）──

    #[test]
    fn crash_when_generation_unchanged_and_child_exited() {
        // 世代未变 + 句柄仍在 + 进程已退 = 无生命周期操作在飞却死了 → 崩溃。
        assert_eq!(
            classify_child_exit(5, 5, ChildObservation::Exited),
            ExitClassification::Crash
        );
    }

    #[test]
    fn intentional_stop_bumped_generation_is_retire() {
        // **变异门**：stop/restart 入口先 bump 世代（5→6）再杀核；即便观察到进程已退，
        // 世代已变 ⟹ 主动杀核 ⟹ Retire，绝不能判 Crash（否则主动 stop 会触发自愈、与重启打架）。
        assert_eq!(
            classify_child_exit(5, 6, ChildObservation::Exited),
            ExitClassification::Retire
        );
        // 让位（就绪期被接管等）同理。
        assert_eq!(
            classify_child_exit(5, 6, ChildObservation::Alive),
            ExitClassification::Retire
        );
    }

    #[test]
    fn child_handle_taken_is_retire_even_same_generation() {
        // 防御性冗余：句柄被 kill_core 取走 → 主动停止在跑 → Retire（不判崩溃）。
        assert_eq!(
            classify_child_exit(5, 5, ChildObservation::Absent),
            ExitClassification::Retire
        );
    }

    #[test]
    fn healthy_core_keeps_watching() {
        assert_eq!(
            classify_child_exit(5, 5, ChildObservation::Alive),
            ExitClassification::KeepWatching
        );
    }

    #[test]
    fn full_crash_loop_converges_to_giveup_within_max() {
        // 端到端：3 次崩溃均失败 → GiveUp，不无限循环（:6040 limio 修复）。
        let mut m = CrashRecoveryMachine::default();
        for i in 0..5 {
            // 试图 attempt，但第 4 次起应 GiveUp。
            let r = m.attempt_crash(NOW + i * 10, 1);
            if i < 3 {
                assert!(
                    matches!(r, AutoRestartOutcome::Attempt { .. }),
                    "iter {i}: expected Attempt, got {r:?}"
                );
                m.post_start_failure(false);
            } else {
                assert_eq!(r, AutoRestartOutcome::GiveUp, "iter {i}: expected GiveUp");
            }
        }
    }
}
