//! 正常退出标记（spec §2.5 Q1-b 清除时机 ④「主进程退出前清 staged」的后端半边）。
//!
//! # 为什么要有它
//!
//! Q1-b 给 staged 列了四个清除时机，④ 是「主进程退出前」。它守的是 NFR-3：不清则「关掉 App
//! 再打开，两天前的半截编辑还在」，而且会参与下一次保存的整份合成 —— 那不是「不丢编辑」，是埋雷。
//! 同时 NFR-1 不可破：`window_health` 的 Reload 自愈腿（renderer 活着但 DOM 空 → 重载一次）与
//! C16 轻量模式（`tray_enter_lightweight` **销毁**主窗 webview，随后按需重建）都是**用户无感**的
//! 真实路径，在它们身上丢编辑 = App 自己吃掉用户的工作。
//!
//! ⇒ 判据必须区分**正常退出**与**重载 / webview 重建 / 强杀**，而这个区分只有主进程知道。
//!
//! # 为什么是「留标记」而不是「退出时通知 webview 清」
//!
//! 退出那一刻 webview 正在拆，清除指令可能永远送不到（竞态），且强杀根本没有那一刻。
//! 改成主进程**只写一个持久标记**、渲染端**下次启动**据此决定清不清：
//!
//! | 上次进程怎么结束的 | 标记 | 下次启动 |
//! |---|---|---|
//! | 正常退出（托盘「退出」/ ⌘Q / 末窗关闭 / `app.exit`） | 有 | 清 staged |
//! | **`app:restart`（U-7 第三类重启）** | **无** | **恢复 staged** |
//! | 强杀 / 断电 / 崩溃 | 无 | 恢复 staged |
//! | 重载 / 轻量模式销毁重建（**进程没退**） | 无 | 恢复 staged |
//!
//! 方向是保守的，与 Q1-b 原文一致：宁可多恢复一次让用户看见「N 项待保存」，也不静默吞掉。
//!
//! # 标记的语义是「用户主动结束了这次使用」，**不是**「进程退出过」
//!
//! 两者的差别就是 `app:restart`。Q1-b 的「退出即清」守的是**「用户对它已无记忆」**
//! （原文：不清则「关掉 App 再打开，两天前的半截编辑还在」）。`app:restart` 是用户几秒内就回来、
//! 心智完全连续的一次重启 —— 尤其它的主要用途是 U-7 那类「改了 `hardwareAcceleration`，重启生效」。
//! 在那条路径上清掉 staged 就是「App 自己吃掉了用户的工作」，正是「为什么重载不清（NFR-1）」那段要防的。
//!
//! 故进程是不是真的退了**不是判据**：`app:restart` 与真退出在进程层面完全一样（都置 `QuitState`、
//! 都走 `ExitRequested`），差别只有「用户还记不记得那批编辑」，而那只有发起方知道 —— 所以由
//! `app_restart` 显式置一个 `RestartState` 告诉退出腿，**不是**从「`QuitState` 是谁置的」反推
//! （那是把两个语义压进一个布尔）。
//!
//! # 为什么是文件而不是配置里的一个键
//!
//! 塞进 `UserConfig` 会让它进 `config_generation_norm` 投影 → 影响热切/重启判定 —— 一个与内核配置
//! 毫不相干的键去动「第四类重启」的判据，是纯粹的副作用。独立标记文件与 `system-proxy.marker.json`
//! / `system-dns.marker.json` 同一范式（同一个目录、同样「存在即为真」、同样跨进程），零新依赖。
//!
//! 载荷是**空文件**：要表达的就是一个 bit，写内容只会引出「内容坏了算什么」这种没有答案的问题。

use std::path::{Path, PathBuf};

/// 标记文件名。与 `system-proxy.marker.json` / `portable.marker` 同目录同范式。
pub const CLEAN_EXIT_MARKER_FILENAME: &str = "clean-exit.marker";

fn marker_path(dir: &Path) -> PathBuf {
    dir.join(CLEAN_EXIT_MARKER_FILENAME)
}

/// 退出腿落标记（best-effort）。写不进去（目录只读 / 磁盘满）⇒ 下次启动当强杀处理 = 恢复 staged，
/// 方向安全，故失败只记日志、绝不阻断退出。
pub fn mark(dir: &Path) {
    if let Err(e) = std::fs::write(marker_path(dir), b"") {
        log::warn!("写正常退出标记失败（下次启动会保守恢复暂存）：{e}");
    }
}

/// 退出腿的落标记腿：`restarting`（本次退出是 `app:restart` 发起的）为真 ⇒ **跳过**。
///
/// 判定与执行合在一处而不是让调用方 `if !restarting { mark(dir) }`：这个 `if` 就是本模块顶注释
/// 那条语义（「用户主动结束了这次使用」而非「进程退出过」）的全部实现，摆在调用方等于把它挪进一个
/// 单测够不着的位置 —— 下面 `restart_leg_does_not_leave_a_marker` 钉的正是它。
pub fn mark_unless_restarting(dir: &Path, restarting: bool) {
    if restarting {
        log::info!(
            "app:restart 发起的退出：不落正常退出标记（用户几秒内就回来，暂存的编辑要留着）"
        );
        return;
    }
    mark(dir);
}

/// **读即清**：返回「上次是正常退出」，并把标记消费掉。
///
/// 用 `remove_file` 的返回值当读取结果 —— 一次系统调用同时完成「读」与「清」，中间没有任何窗口能让
/// 两次启动读到同一个标记。分成 `exists()` + `remove_file()` 两步则不是原子的，且多一条「存在但删不掉」
/// 的分支要处理（那条分支下第二次启动会重复判成「上次正常退出」）。
///
/// 删不掉（权限 / 并发）⇒ 返 `false` = 保守恢复，而不是「清了但没清掉」。
pub fn take(dir: &Path) -> bool {
    std::fs::remove_file(marker_path(dir)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独占临时目录；`Drop` 里清理（无 `tempfile` dev-dep，对齐本仓 updater/uninstall 测试范式）。
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "polaris-clean-exit-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&p).expect("建临时目录");
            Self(p)
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 没写过标记 ⇒ 上次不是正常退出（强杀 / 崩溃 / 首次启动）。
    /// 牙：把 `take` 改成恒 `true` → 本条转红。
    #[test]
    fn absent_marker_means_not_a_clean_exit() {
        let d = TmpDir::new("absent");
        assert!(!take(&d.0));
    }

    /// 写了 ⇒ 读到真。牙：删掉 `mark` 里的 `fs::write` → 本条转红。
    #[test]
    fn marked_exit_is_seen_as_clean() {
        let d = TmpDir::new("marked");
        mark(&d.0);
        assert!(d.0.join(CLEAN_EXIT_MARKER_FILENAME).exists());
        assert!(take(&d.0));
    }

    /// **读后即清**：连续两次启动不会重复判成「上次正常退出」。
    /// 这条是 ④ 的正确性核心 —— 不清的话，正常退出一次之后**每次**启动都会清掉 staged，
    /// 强杀那条恢复腿就永远走不到了。
    /// 牙：把 `take` 改成 `marker_path(dir).exists()`（只读不清）→ 第二次断言转红。
    #[test]
    fn take_consumes_the_marker_so_the_next_start_sees_nothing() {
        let d = TmpDir::new("consume");
        mark(&d.0);
        assert!(take(&d.0), "第一次启动：上次正常退出");
        assert!(!take(&d.0), "第二次启动：标记已被消费，按非正常退出处理");
        assert!(!d.0.join(CLEAN_EXIT_MARKER_FILENAME).exists());
    }

    /// 重复 `mark` 幂等（退出腿理论上只跑一次，但 `app:restart` + 末窗关闭这类叠加路径不该炸）。
    #[test]
    fn mark_is_idempotent() {
        let d = TmpDir::new("idempotent");
        mark(&d.0);
        mark(&d.0);
        assert!(take(&d.0));
        assert!(!take(&d.0));
    }

    /// **`app:restart` 不落标记** ⇒ 重启回来暂存还在。用户几秒内就回来、心智连续，
    /// 在这条路径上清掉 staged = App 自己吃掉用户的工作（NFR-1）。
    ///
    /// 牙：删掉 `mark_unless_restarting` 里的 `if restarting { return; }` → 本条转红。
    ///
    /// ⚠️ 本条钉的是**判定腿**（bit 为真就不落）。「那个 bit 真的由 `app_restart` 置上、
    /// 且真的传到了这里」是**接线**，本条证不了 —— 那一半由
    /// `main.rs::restart_leg_is_wired_to_skip_the_clean_exit_marker` 的源码守卫盯着。
    #[test]
    fn restart_leg_does_not_leave_a_marker() {
        let d = TmpDir::new("restart");
        mark_unless_restarting(&d.0, true);
        assert!(!d.0.join(CLEAN_EXIT_MARKER_FILENAME).exists());
        assert!(
            !take(&d.0),
            "app:restart 后再启动必须按「没正常退出」处理 ⇒ 恢复暂存"
        );
    }

    /// 反向对照：同一个函数、bit 为假 ⇒ 照落。没有这条，上一条把 `mark_unless_restarting`
    /// 改成空函数也全绿（「跳过」变成「永远跳过」）。
    #[test]
    fn real_exit_leg_still_leaves_a_marker() {
        let d = TmpDir::new("realexit");
        mark_unless_restarting(&d.0, false);
        assert!(take(&d.0));
    }

    /// 目录不存在 ⇒ 写失败但不 panic、不留标记（下次启动保守恢复）。
    /// 牙：把 `mark` 里的 `if let Err` 改成 `.expect(...)` → 本条 panic 转红。
    #[test]
    fn mark_on_missing_dir_degrades_to_no_marker() {
        let d = TmpDir::new("missing");
        let gone = d.0.join("nope");
        mark(&gone);
        assert!(!take(&gone));
    }
}
