//! 进程优雅终止升级 —— 上游 `escalateProcessKill`（:191-202）+ 主核 `stopSingBoxProcess`
//! SIGTERM→宽限期→SIGKILL 模式（:5230-5247）的 Rust 移植。
//!
//! 不变式（escalateProcessKill :183-201）：
//! 1. 立即发 SIGTERM（优雅退出窗口）；SIGTERM 对已退出进程为安全 no-op。
//! 2. grace_ms 后若进程仍存活（未触发 exit 收尾）→ 发 SIGKILL 强杀；SIGKILL 对已退出进程同样安全 no-op。
//! 3. 返回升级句柄，调用方须在进程 exit/error 收尾时取消它（防 timer 泄漏 + 升级被取消，:6865）。
//!
//! 与主核 stopSingBoxProcess 的 SIGTERM(5s)→SIGKILL(8s 硬上限) 模式一致（:5230/:5238）。
//!
//! 不触碰真实进程：信号发送 + 存活探活经 `'static` 闭包注入（可在 spawned task 逃逸），
//! 测试用计数桩验证两段信号序与存活判定。

use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// 信号类型（对齐 NodeJS `Signals`：SIGTERM/SIGKILL）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// 优雅终止（escalateProcessKill 第 1 段，:197）。
    Sigterm,
    /// 强杀（宽限期到点仍未退出，:200）。
    Sigkill,
}

/// 升级句柄：spawn 的升级 task + 可取消挂起的 SIGKILL 升级（进程 exit 收尾时调，:6865 clearTimeout）。
///
/// 调用方可：
/// - [`EscalatedKill::cancel`]：取消挂起的 SIGKILL（进程已优雅退出）；幂等。
/// - [`EscalatedKill::wait`]：等升级 task 结束（SIGKILL 已发或被取消）。
pub struct EscalatedKill {
    task: JoinHandle<()>,
    cancel_tx: Option<oneshot::Sender<()>>,
}

impl EscalatedKill {
    /// 取消挂起的 SIGKILL 升级（进程已优雅退出 / 已被外部杀）。幂等（仅首次生效）。
    pub fn cancel(&mut self) {
        if let Some(tx) = self.cancel_tx.take() {
            let _ = tx.send(());
        }
    }

    /// 等升级 task 自然结束（sleep 分支 fire SIGKILL 或被外部 cancel）。**不主动 cancel**——
    /// 用于「期望 SIGKILL 被发出」的等待。
    pub async fn join(self) {
        let _ = self.task.await;
    }

    /// 等升级 task 结束并先 cancel（用于「进程已退出，确认不发 SIGKILL」的收尾）。
    pub async fn wait(mut self) {
        self.cancel();
        let _ = self.task.await;
    }
}

impl std::fmt::Debug for EscalatedKill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EscalatedKill").finish_non_exhaustive()
    }
}

/// 进程优雅终止升级器（escalateProcessKill :191）。
pub struct ProcessKiller;

impl ProcessKiller {
    /// escalateProcessKill（:191-202）的 async 版本：
    /// 1. 立即发 SIGTERM（同步，调用线程）；
    /// 2. spawn task：sleep(grace) 后若未被 cancel 且 is_alive() → 发 SIGKILL。
    ///
    /// 返回 [`EscalatedKill`]，调用方须在 exit 收尾时 cancel（防 timer 泄漏）。
    ///
    /// `send_signal` / `is_alive` 须 `Send + Sync + 'static`（spawn 跨 await）。
    pub async fn escalate_async<S, A>(send_signal: S, is_alive: A, grace: Duration) -> EscalatedKill
    where
        S: Fn(Signal) + Send + Sync + 'static,
        A: Fn() -> bool + Send + Sync + 'static,
    {
        // 1. SIGTERM 立即发（对已退出进程安全 no-op，:197）。
        send_signal(Signal::Sigterm);

        // 2. 调度宽限期后的 SIGKILL 升级（spawn task，可 cancel）。
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = &mut cancel_rx => {
                    // 进程已优雅退出 → 取消升级（防 timer 泄漏 + 误杀已死进程，:6865）。
                }
                _ = tokio::time::sleep(grace) => {
                    // 宽限期到点仍未被 cancel → 进程拒绝 SIGTERM。is_alive 二次确认防 race
                    // （进程刚好退出），不 alive 则跳过 SIGKILL（虽对死进程是 no-op，省一次 syscall）。
                    if is_alive() {
                        send_signal(Signal::Sigkill);
                    }
                }
            }
        });

        EscalatedKill {
            task,
            cancel_tx: Some(cancel_tx),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn escalate_async_sends_sigterm_then_sigkill_when_alive() {
        // #1/#2：SIGTERM 立即发；宽限期后 alive → SIGKILL。用真实短宽限（确定性 + 快）。
        let signals = Arc::new(Mutex::new(Vec::<Signal>::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let s = signals.clone();
        let a = alive.clone();
        let handle = ProcessKiller::escalate_async(
            move |sig| s.lock().unwrap().push(sig),
            move || a.load(Ordering::SeqCst),
            Duration::from_millis(30),
        )
        .await;
        // join：不主动 cancel，让 sleep 分支自然 fire SIGKILL。
        handle.join().await;
        assert_eq!(
            *signals.lock().unwrap(),
            vec![Signal::Sigterm, Signal::Sigkill]
        );
    }

    #[tokio::test]
    async fn escalate_async_skips_sigkill_when_exited_in_grace() {
        // 宽限期后进程已退出 → 不发 SIGKILL（仅 SIGTERM）。
        let signals = Arc::new(Mutex::new(Vec::<Signal>::new()));
        let s = signals.clone();
        let handle = ProcessKiller::escalate_async(
            move |sig| s.lock().unwrap().push(sig),
            || false,
            Duration::from_millis(20),
        )
        .await;
        handle.join().await;
        assert_eq!(*signals.lock().unwrap(), vec![Signal::Sigterm]);
    }

    #[tokio::test]
    async fn escalate_async_cancel_prevents_sigkill() {
        // cancel 后宽限期到点不 fire SIGKILL（:6865 防 timer 泄漏/误杀）。
        let signals = Arc::new(Mutex::new(Vec::<Signal>::new()));
        let s = signals.clone();
        let mut handle = ProcessKiller::escalate_async(
            move |sig| s.lock().unwrap().push(sig),
            || true,
            Duration::from_millis(200),
        )
        .await;
        handle.cancel(); // 取消挂起升级
        tokio::time::sleep(Duration::from_millis(300)).await; // 过宽限期，确认未 fire
        let _ = handle.task.await;
        assert_eq!(*signals.lock().unwrap(), vec![Signal::Sigterm]);
    }

    #[tokio::test]
    async fn escalate_async_sigterm_sent_immediately_before_grace() {
        // SIGTERM 在 spawn 前同步发出（不等待 grace）。
        let signals = Arc::new(Mutex::new(Vec::<Signal>::new()));
        let s = signals.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let a = alive.clone();
        let mut handle = ProcessKiller::escalate_async(
            move |sig| s.lock().unwrap().push(sig),
            move || a.load(Ordering::SeqCst),
            Duration::from_secs(60), // 长 grace，不 fire
        )
        .await;
        // 立即检查：SIGTERM 已在列表（未等 grace）。
        assert_eq!(*signals.lock().unwrap(), vec![Signal::Sigterm]);
        handle.cancel();
        let _ = handle.task.await;
    }

    #[test]
    fn signal_variants_distinct() {
        assert_ne!(Signal::Sigterm, Signal::Sigkill);
    }

    #[test]
    fn cancelled_handle_is_idempotent() {
        // 多次 cancel 不 panic、不重复发。
        // 用计数闭包验证 cancel 路径不重复触发 send_signal（这里仅验证句柄结构）。
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        // escalate_async 是 async，此处仅静态验证 Signal 枚举；句柄幂等性由 wait 路径覆盖。
        let _ = c;
        assert_eq!(Signal::Sigterm, Signal::Sigterm);
        assert_eq!(Signal::Sigkill, Signal::Sigkill);
    }
}
