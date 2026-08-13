//! 换核后的**稳定性观察窗**判据（纯逻辑）—— 上游 `CoreUpdateService` 那套
//! `armPendingValidation` / `startStabilityWatch` / `autoRollbackIfPendingUpdate` 的判定侧。
//!
//! # 为什么需要它（移植缺口，不是新功能）
//!
//! Polaris 的换核验证闩此前**只有同步那一半**：`swap_core_with_restart` 里
//! `proxy.start(cfg).await` 返 `Err` 才回滚，返 `Ok` 即宣告终局。而 上游的验证是四件联动：
//!
//! | 部件 | 上游 | 移植前的 Polaris |
//! |---|---|---|
//! | 待验证闩 | `armPendingValidation`（`CoreUpdateService.ts:521`） | 无 |
//! | **抑制崩溃自愈重启** | `setAutoRestartSuppressed(true)`（同上 `:527`） | 字段在、判据在、**无人置起** |
//! | 30s 稳定观察 | `startStabilityWatch()`（`:1597`） | 无 |
//! | 事后自动回滚 | `autoRollbackIfPendingUpdate()`（`:1623`） | 无 |
//!
//! 缺口的用户可见后果：新核**起得来**但在真实流量下几十秒后崩 ⇒ Polaris 的崩溃自愈
//! （`CrashRecoveryMachine`，退避重启 3 次后放弃）会先把这个信号消化掉，最后停在 `error` 终态，
//! 而那份 `.bak` 备份原封不动躺在盘上、没有任何路径去用它 —— 用户得自己发现、自己去设置页手动回滚。
//! 自愈非但没帮忙，还**主动掩盖了首次失败信号**：这正是 上游 要 `setAutoRestartSuppressed(true)` 的原因。
//!
//! # 本模块的边界
//!
//! 只放**判据与常量**；「置抑制位 → 轮询 → 回滚」的编排在 `commands::updater::arm_core_validation`
//! （回滚要复用 `core_rollback` 的整条停/起核编排，那些搭档都在 commands 层）。
//! 这样判据可在无 Tauri、无进程、无计时的条件下穷举单测。

use std::time::Duration;

/// 稳定观察窗长度（上游 `STABILITY_DWELL_MS = 30000`，`CoreUpdateService.ts:104`，逐字对齐）。
///
/// 挺过这段时间没崩 ⇒ 判定新核稳定：删旧备份、撤抑制位。
pub const STABILITY_DWELL: Duration = Duration::from_secs(30);

/// 观察窗内的轮询间隔。
///
/// 用轮询而不是订阅事件：`ProxyRuntime` 对外只有 `status()` 快照，没有状态变更广播通道
/// （`Notify` 那个是世代变更专用，语义不同）。为这一处新造一条广播通道，代价大于
/// 「30s 窗口里查 60 次一个 `Mutex` 保护的结构体」。窗口有 30s，500ms 的观测延迟无关紧要。
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 判「这次故障是否应当触发回滚到旧核」。
///
/// # 判据是**白名单**，不是黑名单
///
/// 上游 在 `index.ts:1930` 用的是黑名单：排除 `TUN_INIT_PERSISTENT`（TUN 地址冲突是**环境问题**
/// ——本机另一张网卡占了同一地址——与核版本无关；若恰好紧跟一次换核发生，回滚会把健康的新核换掉、
/// 再以旧核重启一轮，冲突照旧存在，等于白白回滚还把环境冲突误归因成「新核坏了」，见其 issue #324）。
///
/// 本仓改用白名单，理由是错误码表的形状不同：Polaris 的码表里**多数码是环境/权限轴**
/// （`HELPER_NOT_INSTALLED` / `HELPER_GATE_ABORTED` / `ROOT_ORPHAN_BLOCKED` /
/// `TUN_ROUTE_NOT_CAPTURED`），黑名单要逐条列全、且**将来新增一个环境码就会静默变成误回滚**。
/// 白名单反过来：只认「核自己没起来 / 起来了又挂了」这三条，新增码默认**不**触发回滚，
/// 方向是「宁可不回滚」，与 `github.rs` 取消跨架构回落是同一条纪律。
///
/// # 两道门缺一不可
///
/// `running == false` 这道门单独就滤掉了全部**非致命**码（`SYSTEM_PROXY_FAILED` /
/// `EXIT_MISMATCH` 走 `set_nonfatal_error`，核仍在跑 ⇒ `running` 保持 true）。
/// 只看码不看 `running`，会在「核好好跑着但系统代理设置失败」时把核回滚掉。
#[must_use]
pub fn failure_warrants_rollback(running: bool, error_code: Option<&str>) -> bool {
    if running {
        return false;
    }
    matches!(
        error_code,
        // 起核腿失败（就绪门判定核已死 / 就绪超时）。
        Some("STARTUP_FAILED")
        // 核意外退出且无可用配置重启。
        | Some("PROCESS_EXITED")
        // 崩溃自愈放弃 —— 抑制窗口内这就是「第一次崩溃即上报」那条腿的出口。
        | Some("AUTO_RESTART_FAILED")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dwell_matches_upstream_constant() {
        // 上游 CoreUpdateService.ts:104 `STABILITY_DWELL_MS = 30000`。
        assert_eq!(STABILITY_DWELL.as_millis(), 30_000);
    }

    #[test]
    fn core_failures_warrant_rollback() {
        for code in ["STARTUP_FAILED", "PROCESS_EXITED", "AUTO_RESTART_FAILED"] {
            assert!(
                failure_warrants_rollback(false, Some(code)),
                "{code} 表示核自己没跑起来 / 跑挂了 —— 换核窗口内必须回滚"
            );
        }
    }

    #[test]
    fn environment_failures_do_not_warrant_rollback() {
        // 这些是「环境/权限」轴：回滚换不掉它们，只会把健康的新核白白换走
        // （上游 issue #324 的同类失败面）。
        for code in [
            "HELPER_NOT_INSTALLED",
            "HELPER_GATE_ABORTED",
            "ROOT_ORPHAN_BLOCKED",
            "TUN_ROUTE_NOT_CAPTURED",
        ] {
            assert!(
                !failure_warrants_rollback(false, Some(code)),
                "{code} 是环境问题，与核版本无关，回滚解决不了"
            );
        }
    }

    #[test]
    fn unknown_future_codes_default_to_no_rollback() {
        // 白名单的**全部价值**在这一条：将来新增任何码，默认不回滚。
        assert!(!failure_warrants_rollback(false, Some("SOME_FUTURE_CODE")));
        assert!(!failure_warrants_rollback(false, None));
    }

    #[test]
    fn running_core_never_rolls_back_even_with_error_code() {
        // 非致命错误（set_nonfatal_error）保留 running=true。只看码不看 running 会在
        // 「核好好跑着、只是系统代理没设上」时把核回滚掉。
        assert!(!failure_warrants_rollback(
            true,
            Some("SYSTEM_PROXY_FAILED")
        ));
        assert!(!failure_warrants_rollback(true, Some("EXIT_MISMATCH")));
        // 连白名单里的码也一样：running 为真就不是「核挂了」。
        assert!(!failure_warrants_rollback(true, Some("PROCESS_EXITED")));
    }
}
