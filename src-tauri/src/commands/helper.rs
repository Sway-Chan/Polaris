//! helper 类 command（上游 `helper-handlers.ts` 的 helper 部分）。
//!
//! 映射 channel：
//! - `helper:getStatus` → [`helper_get_status`]
//! - `helper:install` → [`helper_install`]（弹一次提权框）
//! - `helper:uninstall` → [`helper_uninstall`]
//!
//! 真实 install/uninstall 经 helper-client HelperManager（SysOps 跑 install 脚本 + 提权），
//! 属系统交互批次；本层提供状态查询 + 命令入口。

#![allow(clippy::needless_pass_by_value)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri::State;

use crate::response::ApiResponse;
use crate::runtime::helper::{
    decide_uninstall_preflight, uninstall_preflight_stop, HelperActionResult, HelperStatusSnapshot,
    UninstallPreflight,
};
use crate::runtime::uninstall::{stop_core_outcome, StepOutcome};
use crate::runtime::AppRuntime;

/// 卸载期间的停核复查节拍。
///
/// 提权框可挂几分钟，而「用户点连接 → root 核起来」是秒级动作，故复查必须密于分钟级；
/// 500ms 一次的代价只是一次进程内 `proxy.status()` 读（不碰 socket、不碰系统）。
const UNINSTALL_RECHECK_INTERVAL: Duration = Duration::from_millis(500);

/// 卸载收尾时**等看门狗自然退出**的预算上限。
///
/// 取值依据是「看门狗最后一拍最坏要花多久」，不是拍脑袋：那一拍最重的动作是一次
/// `ProxyRuntime::stop()` —— 杀核 `SIGTERM → 5s 宽限 → SIGKILL` + 收割，叠加还原系统 DNS、
/// 清系统代理各一次 exec。20s 给足这条链，且封顶了 IPC 应答的最坏等待。
///
/// **超预算也绝不 abort**（见 [`join_watchdog_cooperatively`]）：这个数只决定「命令还等不等」，
/// 不决定「看门狗死不死」。
const WATCHDOG_JOIN_BUDGET: Duration = Duration::from_secs(20);

/// 上游 `HELPER_GET_STATUS`：helper 安装/就绪/版本状态（真探测 HelperManager.compute_status）。
#[tauri::command]
pub fn helper_get_status(
    state: State<'_, AppRuntime>,
    _force: Option<bool>,
) -> ApiResponse<HelperStatusSnapshot> {
    ApiResponse::ok(state.helper().status())
}

/// 上游 `HELPER_INSTALL`：安装 helper（弹一次提权框）。
///
/// 提权三态（成功/用户取消/失败）在 [`HelperActionResult`] 内表达——外层恒 `ok`（IPC 层不失败，
/// 用户取消是正常流程，前端读 `r.status`/`r.error` 展示）。**提权本身是真机门**。
///
/// **提权框（可 30s+）在 `spawn_blocking` 线程等**，不占 tokio worker、不冻 UI ——
/// 与 [`helper_uninstall`] 同口径（两条腿都是「同步 + 弹框 + 分钟级阻塞」，不该一条 async 一条 sync）。
#[tauri::command]
pub async fn helper_install(
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<HelperActionResult>, ()> {
    let helper = state.helper.clone();
    Ok(
        match tokio::task::spawn_blocking(move || helper.install()).await {
            Ok(r) => ApiResponse::ok(r),
            Err(e) => ApiResponse::err(format!("helper 安装任务异常终止: {e}")),
        },
    )
}

/// 上游 `HELPER_UNINSTALL`：卸载 helper（**先零提权停核**，再弹一次提权框卸载）。
///
/// # 停核腿（契约 `polaris-上游-capability-contract.md:93`「卸载前零提权停核」）
///
/// 代理正经 helper 运行时，先用**仍在的** helper 停掉它的 root/SYSTEM 受管核，再卸载。
/// 顺序不可换：卸载会连 daemon 带 socket 一起删掉，之后那个 root 核就成了用户态杀不动的孤儿
/// （TUN 还占着 → 全网断），只能落 forceKill 裸弹一次无引导的提权框。
///
/// 判定 + 停失败语义（**继续卸载**，与 `update_install` 的停代理腿刻意相反）收在纯函数
/// [`uninstall_preflight_stop`] —— 那里有真值表与理由；本命令只做注入。
///
/// # 一次前置停核**不够**：整段卸载期都要看着
///
/// 前置腿的判据是「进 `uninstall()` 之前」的一张快照，而 `uninstall()` 会弹提权框并同步等到用户
/// 处理（分钟级）。这段时间 helper 完整活着，用户点一下「连接」就能把 root 受管核起起来 ——
/// 卸载一完成它就是孤儿核 + 断网，正是这条腿要防的形态。故整段卸载期挂一条
/// [`uninstall_stop_watchdog`]，见到「经 helper 起来的核」就再停一次。
///
/// # `uninstall()` 走 `spawn_blocking`
///
/// 它内部同步 spawn 提权框并等其退出（分钟级）。在 async 命令里直调会把一个 tokio worker 占死
/// 整段等待期 —— 而本批同时把 stats 的三条 poller 从「每拍阻塞式回读窗口」改成读缓存，
/// 正是因为主循环被这类原生模态占住时不该再连累 worker。
#[tauri::command]
pub async fn helper_uninstall(
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<HelperActionResult>, ()> {
    let helper = state.helper.clone();
    // 停核腿的结果在这条腿上**故意忽略**：helper 单卸载停不掉也继续卸（真值表见
    // `uninstall_preflight_stop` 文档）。完全卸载腿读同一个值并中止 —— 政策不同，机制同一份。
    let outcome = with_uninstall_core_guard(&state, |_stop| async move {
        tokio::task::spawn_blocking(move || helper.uninstall()).await
    })
    .await;

    Ok(match outcome {
        Ok(r) => ApiResponse::ok(r),
        Err(e) => ApiResponse::err(format!("helper 卸载任务异常终止: {e}")),
    })
}

/// 卸载类命令共用的外壳：**零提权前置停核 → 全程挂停核看门狗 → 跑 `body` → 协作式收停**。
///
/// # 为什么抽出来（而不是让完全卸载再抄一份）
///
/// 这段编排的每一行都有血债：前置停核的时机（[`uninstall_preflight_stop`]）、看门狗必须覆盖整段
/// 提权框窗口（见 [`helper_uninstall`] 文档「一次前置停核不够」）、收尾**绝不能 abort**
/// （三条后果见 [`join_watchdog_cooperatively`]）。完全卸载面对的是同一个提权框、同一个窗口，
/// 抄第二份必然漂移，而漂移的代价是孤儿 root 核 + 断网。故两条命令共用这一份。
///
/// `body` 收到停核腿的 [`StepOutcome`]，**自行决定政策**：
/// - [`helper_uninstall`] 忽略它（停不掉也继续卸）；
/// - `app_uninstall_all` 把它当作 fail-fast 的第一步（停不掉就一项都不删）。
///
/// 看门狗在 `body` 全程挂着 —— 对完全卸载来说这不只覆盖 helper 的提权框，还覆盖后面删配置、
/// 删应用本体那段时间。
pub(crate) async fn with_uninstall_core_guard<F, Fut, T>(state: &AppRuntime, body: F) -> T
where
    F: FnOnce(StepOutcome) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let status = state.proxy().status();
    let preflight = decide_uninstall_preflight(status.running, status.started_via_helper);
    let proxy = state.proxy.clone();
    let stopped =
        uninstall_preflight_stop(status.running, status.started_via_helper, || async move {
            proxy.stop().await
        })
        .await;
    let stop_outcome = stop_core_outcome(preflight, stopped.error());

    // ── 卸载期间的持续停核看门狗（见 [`helper_uninstall`] 文档「一次前置停核不够」）。
    let done = Arc::new(AtomicBool::new(false));
    let mut watchdog = {
        let done = done.clone();
        let status_proxy = state.proxy.clone();
        let stop_proxy = state.proxy.clone();
        tauri::async_runtime::spawn(async move {
            uninstall_stop_watchdog(
                done,
                UNINSTALL_RECHECK_INTERVAL,
                move || {
                    let s = status_proxy.status();
                    (s.running, s.started_via_helper)
                },
                move || {
                    let p = stop_proxy.clone();
                    async move { p.stop().await }
                },
            )
            .await
        })
    };

    let out = body(stop_outcome).await;

    // 协作式收停（**不能 abort**，理由见 [`join_watchdog_cooperatively`]）。
    if !join_watchdog_cooperatively(&done, &mut watchdog, WATCHDOG_JOIN_BUDGET).await {
        log::warn!(
            "卸载收尾时看门狗仍在停核中（已超 {WATCHDOG_JOIN_BUDGET:?}）：不打断它，\
             让它把这一次停核走完后自退（`done` 已置位 → 不会再发起新的停核）"
        );
    }
    out
}

/// 协作式收停看门狗：置位 `done` → **等它自己退出**，最多等 `budget`。
///
/// 返回 `true` = 看门狗已自然退出；`false` = 超预算（调用方放手，**绝不 abort**）。
///
/// # 为什么这里必须是协作式取消，而不是 `JoinHandle::abort()`（本函数存在的全部理由）
///
/// 原本这里是 `done.store(true); watchdog.abort();`。`abort()` 让任务在**当前 await 点**被整体 drop
/// —— 若那一刻看门狗正落在 `proxy.stop().await` 里，被 drop 的就是**在飞的停核 future**。
/// 触发窗口窄（uninstall 刚返回 + 看门狗恰在停核中），但后果不是「停核慢一点」，是下面三条：
///
/// 1. **`LifecycleGate` 深度永久泄漏**（最重）。`ProxyRuntime::stop_inner` 的形态是
///    `gate.begin(); … 6 个 await …; finish_lifecycle(Stop)`，中间**没有任何 RAII guard**
///    （`runtime/proxy.rs` 里的 `ReconcileGuard`/`InflightGuard`/`TsExitRecoverGuard` 都不管这个门），
///    而 `LifecycleGate`（`crates/core-supervisor/src/lifecycle_gate.rs`）是裸引用计数：
///    `begin()` 加一、`end()` 减一。future 在中途被 drop ⇒ `end()` 永不执行 ⇒ depth 恒 >0 ⇒
///    此后**本进程内每一次** `switch_mode` / 去抖重启都只置 pending 不执行
///    （`runtime/proxy.rs` 自陈：「depth 长期 >0 ⇒ 此期间 switch_mode / 去抖重启只置 pending 不执行」）。
///    切节点、改模式从此静默失效，直到重启应用。
/// 2. **核变孤儿 + pid 记账错乱**。`kill_core` 先把 `Child` 句柄 `take()` 出锁再 `await`；中途 drop ⇒
///    句柄未 `wait()` 就没了（不收割），且 `self.pid` 不被清 —— 而那个字段正是 stale-core 清扫的
///    「受管 pid 排除表」，留个死 pid 等于给同号新进程发免死金牌（该风险 `kill_core` 里已有成文记录）。
/// 3. **系统代理留在死端口上**。`stop()` = `stop_inner().await` + `clear_system_proxy().await`；
///    在前半段被取消 ⇒ 第二段根本不跑 ⇒ OS 代理仍指向刚被杀的本地口 = 用户全网断连，需手动改回。
///    而这正是本命令那条停核腿要防的形态本身。
///
/// 即：`proxy.stop()` **不是 cancel-safe** 的，所以这条腿不能靠 abort 收场。
///
/// # 为什么超预算也不 abort（而不是「先等一会儿再强杀」）
///
/// 置位 `done` 之后，看门狗**结构上已不可能再发起新的停核**：循环体是
/// `while !done { sleep; if done { break } … stop().await }` —— 在飞的那次 stop 一返回就回到
/// `while !done` 并退出。所以「残任务」只有一个有界的尾巴，不需要强杀；而强杀恰好会命中上面三条。
/// `budget` 因此只是**命令还等不等**的上限（防 IPC 应答被一次挂死的停核无限期拖住），
/// 超时后把句柄一丢让它自己收尾即可。
///
/// # 那条尾巴晚落地时的**换代毒性**（由停核腿自己收口，不在本层）
///
/// 「有界」说的是它不会再发起**新的**停核，不代表它落地时还当权：超预算意味着在飞的那次
/// `proxy.stop()` 已经挂了 >`WATCHDOG_JOIN_BUDGET`（macOS `networksetup` exec 卡死 /
/// `spawn_blocking` 饥饿），而命令这时已经返回 —— 用户完全可能重装 helper 并起一个新核。残 stop
/// 随后醒来，其拆除段每一步（清 sidecar 注入态 / 抹 running 态 / 还原系统 DNS / 清系统代理）都会
/// 落在**新会话**上。
///
/// 这条不在本层堵：本层没有「谁当权」的判据（`proxy.stop()` 是个不透明 future）。收口在
/// `runtime::proxy::ProxyRuntime::stop_inner` 的换代守卫 —— 它在拆除段的**每个 await 之后**比对
/// 自己 bump 出来的世代，一旦发现被更新的 start/stop 接管就整段让位（`gate.begin()/end()` 仍配对，
/// 不会重演上面第 1 条的 depth 泄漏）。故本层「把句柄一丢」是安全的。
async fn join_watchdog_cooperatively<F>(done: &AtomicBool, handle: &mut F, budget: Duration) -> bool
where
    F: std::future::Future + Unpin,
{
    done.store(true, Ordering::SeqCst);
    tokio::time::timeout(budget, handle).await.is_ok()
}

/// 卸载期间的持续停核看门狗（`status` / `stop` 注入 → 可单测，不碰真代理）。
///
/// 每 `interval` 复查一次代理态，判据复用**同一个** [`decide_uninstall_preflight`]
/// （各写一份必然与前置腿漂移：那边只停「经 helper 起的核」，这边也只该停那种 ——
/// app 自己直起的核不归 daemon 管，卸载不会让它变孤儿，停它等于无故断用户的网）。
///
/// `done` 置位即退出。返回本次卸载期间**真正发起过几次**停核（供单测断言，生产忽略）。
async fn uninstall_stop_watchdog<S, F, Fut>(
    done: Arc<AtomicBool>,
    interval: Duration,
    status: S,
    stop: F,
) -> usize
where
    S: Fn() -> (bool, bool),
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let mut stops = 0usize;
    while !done.load(Ordering::SeqCst) {
        tokio::time::sleep(interval).await;
        if done.load(Ordering::SeqCst) {
            break; // 卸载已结束 → 这一拍不再插手（避免停掉用户卸载完之后新起的核）
        }
        let (running, started_via_helper) = status();
        if decide_uninstall_preflight(running, started_via_helper)
            != UninstallPreflight::StopCoreFirst
        {
            continue;
        }
        log::warn!(
            "卸载 helper 期间检测到受管内核又被起了起来 → 立即再停一次 \
             （放着不管：卸载完成后它就是用户态杀不动的 root 孤儿核 + TUN 占着 = 断网）"
        );
        stops += 1;
        if let Err(e) = stop().await {
            log::warn!("卸载期间停核失败（{e}）：卸载后可能残留 root 受管核，需手动确认");
        }
    }
    stops
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个可翻转的代理态源 + 计数型 stop。
    fn probe(
        running: Arc<AtomicBool>,
        via_helper: bool,
    ) -> impl Fn() -> (bool, bool) + Clone + Send + 'static {
        move || (running.load(Ordering::SeqCst), via_helper)
    }

    /// 🟡 **变异锁：卸载期间核被重新起起来 → 看门狗必须再停一次。**
    ///
    /// 复现的正是提权框挂着的那几分钟：前置停核已跑过（快照那一刻核是停的），用户随后点了连接。
    /// **变异探针**：把 `helper_uninstall` 里的看门狗删掉 / 让本函数只查一次 ⇒ 本条转红。
    #[tokio::test(start_paused = true)]
    async fn watchdog_restops_core_started_during_the_elevation_dialog() {
        let done = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(false)); // 前置停核之后：核是停的
        let stops = Arc::new(AtomicBool::new(false));

        // 提权框挂着期间：1 拍后用户把核起了起来，5 拍后卸载才结束。
        {
            let (running, done) = (running.clone(), done.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(500)).await;
                running.store(true, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(2500)).await;
                done.store(true, Ordering::SeqCst);
            });
        }

        let stopped = stops.clone();
        let running_for_stop = running.clone();
        let n = uninstall_stop_watchdog(
            done,
            Duration::from_millis(500),
            probe(running.clone(), true),
            move || {
                let (stopped, running) = (stopped.clone(), running_for_stop.clone());
                async move {
                    stopped.store(true, Ordering::SeqCst);
                    running.store(false, Ordering::SeqCst); // 停成功
                    Ok(())
                }
            },
        )
        .await;

        assert!(
            stops.load(Ordering::SeqCst),
            "卸载期间起来的受管核必须被再停一次 —— 否则卸载完成后是杀不动的 root 孤儿核 + 断网"
        );
        assert_eq!(n, 1, "核只起了一次 → 只该停一次（不该逐拍空转发 stop）");
    }

    /// 核一直是停的 → 一次 stop 都不发（看门狗不制造噪音）。
    #[tokio::test(start_paused = true)]
    async fn watchdog_is_silent_when_core_stays_down() {
        let done = Arc::new(AtomicBool::new(false));
        {
            let done = done.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(3000)).await;
                done.store(true, Ordering::SeqCst);
            });
        }
        let n = uninstall_stop_watchdog(
            done,
            Duration::from_millis(500),
            probe(Arc::new(AtomicBool::new(false)), true),
            || async { Ok(()) },
        )
        .await;
        assert_eq!(n, 0);
    }

    /// **app 自己直起的核不停**：它不归 daemon 管，卸载不会让它变孤儿，停它等于无故断网。
    /// 判据与前置腿共用 [`decide_uninstall_preflight`]；把它换成只看 `running` ⇒ 本条转红。
    #[tokio::test(start_paused = true)]
    async fn watchdog_leaves_app_started_core_alone() {
        let done = Arc::new(AtomicBool::new(false));
        {
            let done = done.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(3000)).await;
                done.store(true, Ordering::SeqCst);
            });
        }
        let n = uninstall_stop_watchdog(
            done,
            Duration::from_millis(500),
            probe(Arc::new(AtomicBool::new(true)), false), // 在跑，但**不经 helper**
            || async { Ok(()) },
        )
        .await;
        assert_eq!(n, 0, "非 helper 起的核不该被卸载腿停掉");
    }

    /// 停核失败不得让看门狗退出（提权框还挂着，下一拍仍要继续看）。
    #[tokio::test(start_paused = true)]
    async fn watchdog_keeps_watching_after_a_failed_stop() {
        let done = Arc::new(AtomicBool::new(false));
        {
            let done = done.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(2200)).await;
                done.store(true, Ordering::SeqCst);
            });
        }
        let n = uninstall_stop_watchdog(
            done,
            Duration::from_millis(500),
            probe(Arc::new(AtomicBool::new(true)), true), // 恒在跑（停不掉）
            || async { Err("stop failed".to_string()) },
        )
        .await;
        assert!(n >= 3, "停失败后必须继续每拍重试，实得 {n} 次");
    }

    /// 🟡 **变异锁：卸载收尾不得打断在飞的停核。**
    ///
    /// 复现 LOW-3 那个窄窗口：`uninstall()` 刚返回的那一刻，看门狗正落在 `stop().await` 中途。
    /// 协作式收停必须**等它把这次停核走完**（`proxy.stop()` 不是 cancel-safe，三条后果见
    /// [`join_watchdog_cooperatively`] 文档）。
    ///
    /// **变异探针**：把 `join_watchdog_cooperatively(...)` 换回 `watchdog.abort()` ⇒
    /// `stop_finished` 恒 false ⇒ 本条转红。
    #[tokio::test(start_paused = true)]
    async fn cooperative_join_lets_an_inflight_stop_finish() {
        let done = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true)); // 核在跑 → 看门狗第一拍就会去停
        let stop_finished = Arc::new(AtomicBool::new(false));

        let mut watchdog = {
            let (done, running, finished) = (done.clone(), running.clone(), stop_finished.clone());
            tokio::spawn(async move {
                uninstall_stop_watchdog(
                    done,
                    Duration::from_millis(500),
                    probe(running.clone(), true),
                    move || {
                        let (running, finished) = (running.clone(), finished.clone());
                        async move {
                            // 一次真停核的量级：SIGTERM → 宽限 → SIGKILL + 收割。
                            tokio::time::sleep(Duration::from_secs(6)).await;
                            running.store(false, Ordering::SeqCst);
                            finished.store(true, Ordering::SeqCst); // 只有跑完整条 future 才置位
                            Ok(())
                        }
                    },
                )
                .await
            })
        };

        // 让看门狗真的进到 stop().await 里（第一拍 500ms 到点 → 发起停核，停核要 6s）。
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            !stop_finished.load(Ordering::SeqCst),
            "前提：此刻停核确实还在飞（否则本用例没复现那个窗口）"
        );

        // 卸载返回 → 协作式收停。
        let exited = join_watchdog_cooperatively(&done, &mut watchdog, WATCHDOG_JOIN_BUDGET).await;

        assert!(exited, "看门狗必须在预算内自然退出");
        assert!(
            stop_finished.load(Ordering::SeqCst),
            "在飞的停核必须被走完 —— abort 会把它整体 drop：\
             LifecycleGate 深度永久泄漏（此后 switch_mode/去抖重启全成空转）、\
             核句柄不收割、系统代理留在死端口上"
        );
        assert!(
            done.load(Ordering::SeqCst),
            "收停必须置位 `done` —— 少了它，看门狗会在卸载完成后继续停用户新起的核"
        );
    }

    /// 停核挂死时**命令不得被无限期拖住**：超预算返回 false（调用方放手，不 abort）。
    ///
    /// 变异探针：把 `tokio::time::timeout(budget, handle)` 换成裸 `handle.await` ⇒ 本条永远跑不完。
    #[tokio::test(start_paused = true)]
    async fn cooperative_join_is_bounded_when_stop_hangs() {
        let done = Arc::new(AtomicBool::new(false));
        let mut watchdog = {
            let done = done.clone();
            tokio::spawn(async move {
                uninstall_stop_watchdog(
                    done,
                    Duration::from_millis(500),
                    probe(Arc::new(AtomicBool::new(true)), true),
                    || async {
                        // 挂死的停核（helper IPC 卡住等）。
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                        Ok(())
                    },
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_secs(1)).await;

        let t0 = tokio::time::Instant::now();
        let exited =
            join_watchdog_cooperatively(&done, &mut watchdog, Duration::from_secs(2)).await;
        assert!(!exited, "挂死的停核 → 超预算返回 false");
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "等待必须有界（实等 {:?}）",
            t0.elapsed()
        );
        assert!(
            done.load(Ordering::SeqCst),
            "done 必须已置位（看门狗不会再发起新的停核）"
        );
        watchdog.abort(); // 测试收尾清理，非生产语义
    }

    /// 预算必须**盖得住一次真停核**（SIGTERM→5s 宽限→SIGKILL + DNS 还原 + 清系统代理）。
    /// 调小到停核量级以下 ⇒ 每次卸载都走超时腿 ⇒ 协作式收停名存实亡。
    #[test]
    fn join_budget_covers_a_worst_case_stop() {
        assert!(
            WATCHDOG_JOIN_BUDGET >= Duration::from_secs(10),
            "预算 {WATCHDOG_JOIN_BUDGET:?} 盖不住 SIGTERM→5s 宽限→SIGKILL 再加两次系统 exec"
        );
    }

    /// 🟡 **调用点守卫**：`helper_uninstall` 必须在 `uninstall()` 之前经过前置停核，
    /// 且整段卸载期挂着看门狗、`uninstall()` 本身跑在 `spawn_blocking` 上。
    ///
    /// 这几条不变式没法用普通单测覆盖（命令持 `State<'_, AppRuntime>`，单测构造不出 Tauri 运行时），
    /// 故按本层既有做法用源码扫描锁调用点。语义（何时停 / 停失败怎么办）由 `runtime::helper` 的
    /// 真值表 + 上面那组注入式单测覆盖；这里只锁「腿还在不在、顺序对不对」。
    ///
    /// **变异探针**：删掉 `uninstall_preflight_stop(...)` / 删掉看门狗 spawn / 把看门狗挪到
    /// `body(` 之后 / 把收尾换回裸 abort ⇒ 逐条转红。
    ///
    /// 取材面已从 `helper_uninstall` 挪到 [`with_uninstall_core_guard`]（这段编排现由两条卸载命令
    /// 共用），断言一条没减；「两个调用方都还接着这层壳」由
    /// [`both_uninstall_commands_go_through_the_core_guard`] 单独钉死。
    #[test]
    fn uninstall_core_guard_wires_preflight_watchdog_and_cooperative_join() {
        let src = include_str!("helper.rs");
        let body = crate::commands::guard_scan::top_level_fn_body(
            src,
            "pub(crate) async fn with_uninstall_core_guard<",
        );
        let stop_at = body
            .find("uninstall_preflight_stop")
            .expect("卸载前置停核腿被删了 —— TUN 跑着时卸 helper 会留无人管的 root 核 + 断网");
        let watchdog_at = body
            .find("uninstall_stop_watchdog(")
            .expect("卸载期看门狗被删了 —— 提权框挂着的几分钟里用户起的核会变成 root 孤儿核");
        let body_at = body
            .find("body(stop_outcome)")
            .expect("锚点消失：守卫已失去判据");
        assert!(
            stop_at < body_at,
            "停核必须在真卸载动作**之前** —— 卸载会连 daemon 带 socket 一起删掉，之后停不了核"
        );
        assert!(
            watchdog_at < body_at,
            "看门狗必须在真卸载动作**之前**挂上 —— 挂在后面就完全错过了提权框那段窗口"
        );
        // LOW-3：收尾必须走协作式收停，且**不得**再出现裸 abort（`proxy.stop()` 非 cancel-safe）。
        assert!(
            body.contains("join_watchdog_cooperatively(&done, &mut watchdog"),
            "变异锁：收尾腿绕过了协作式收停"
        );
        assert!(
            !body.contains("watchdog.abort()"),
            "abort 会 drop 在飞的 `proxy.stop()`：LifecycleGate 深度永久泄漏 + 核不收割 + \
             系统代理留在死端口上（三条后果见 join_watchdog_cooperatively 文档）"
        );
    }

    /// 🟡 **调用点守卫：两条卸载命令都必须经过 [`with_uninstall_core_guard`]，且提权调用都在
    /// `spawn_blocking` 里。**
    ///
    /// 上一条守的是「壳里那几行还在不在」，这条守的是「还有没有人绕开这层壳」——
    /// 少了它，把编排抽成公共函数反而制造了一个新的逃逸面（谁都可以直调 `helper.uninstall()`）。
    ///
    /// **变异探针**：把任一命令里的 `with_uninstall_core_guard(` 拆掉改回直调 /
    /// 把 `spawn_blocking` 去掉 ⇒ 逐条转红。
    #[test]
    fn both_uninstall_commands_go_through_the_core_guard() {
        let helper_src = include_str!("helper.rs");
        let hb = crate::commands::guard_scan::top_level_fn_body(
            helper_src,
            "pub async fn helper_uninstall(",
        );
        let guard_at = hb
            .find("with_uninstall_core_guard(")
            .expect("helper_uninstall 绕开了停核外壳 —— TUN 跑着时卸 helper 会留 root 孤儿核");
        let uninstall_at = hb.find(".uninstall()").expect("锚点消失：守卫已失去判据");
        assert!(guard_at < uninstall_at, "外壳必须包住 uninstall() 调用");
        assert!(
            hb.contains("spawn_blocking"),
            "uninstall() 又被直调了 —— 提权框会把一个 tokio worker 占死分钟级"
        );

        // 完全卸载腿（`commands/updater.rs`）同样不许绕开。
        let updater_src = include_str!("updater.rs");
        let ub = crate::commands::guard_scan::top_level_fn_body(
            updater_src,
            "pub async fn app_uninstall_all(",
        );
        assert!(
            ub.contains("with_uninstall_core_guard("),
            "完全卸载绕开了停核外壳 —— 它后面还要删配置和应用本体，留下的孤儿 root 核将无人能停"
        );
        assert!(
            ub.contains("spawn_blocking"),
            "完全卸载整条链（含提权框 + 两次 remove_dir_all）必须在 spawn_blocking 上跑"
        );
    }

    /// 复查节拍必须**远密于**提权框的量级，否则「看着」只是个说法。
    #[test]
    fn recheck_interval_is_far_below_the_dialog_timescale() {
        assert!(
            UNINSTALL_RECHECK_INTERVAL <= Duration::from_secs(1),
            "复查节拍 {UNINSTALL_RECHECK_INTERVAL:?} 太粗 —— 用户点连接到卸载完成只有几秒"
        );
    }
}
