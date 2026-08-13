//! polaris-core-supervisor — sing-box 进程管理 crate（§B.2 / §H B2）。
//!
//! 职责（Polaris 锚点）：sing-box 进程 spawn/readiness 门/stop+kill/崩溃自愈/端口簿记 + 生命周期单飞（世代 token）。
//! 源真值：`~/Code/polaris/src/main/services/ProxyManager.ts`（无独立 supervisor 文件，进程管理分散其中）
//!   + `core-readiness.ts`（就绪等待 + CoreStartSupersededError）+ `backoff-tracker.ts`。
//!
//! 纯 Rust async（tokio::process::Child）；不触碰宿主网络——所有 spawn/kill/端口探测经 trait 抽象，测试 mock。
//!
//! 行为不变式见 vault 的能力入口点清单（special-logic 分册） §1 维度7 #1-6：
//! - 单飞守卫（lifecycle_depth）：[`lifecycle_gate`]
//! - 就绪让位（CoreStartSupersededError）：[`readiness_gate`]
//! - 崩溃自愈退避 + supersede 补发：[`crash_recovery`]
//! - 端口簿记：[`port_bookkeeping`]
//! - SIGTERM→宽限→SIGKILL 升级：[`process_killer`]
//! - spawn/exit 事件采集：[`spawner`]

#![forbid(unsafe_code)]

/// 起核前的内核闸门：拿即将下发的 config 真跑 `sing-box check`，把内核点名拒收的节点剥掉。
pub mod config_gate;
pub mod crash_recovery;
pub mod lifecycle_gate;
pub mod port_bookkeeping;
pub mod process_killer;
pub mod readiness_gate;
pub mod spawner;
pub mod stale_core;

pub use config_gate::{
    decide_peel, parse_kernel_rejection, run_config_check, run_config_check_within,
    ConfigCheckVerdict, KernelRejection, PeelStep, RejectedArray, INVALID_REASON_KERNEL_REJECTED,
    PEEL_TIME_BUDGET,
};
pub use crash_recovery::{
    classify_child_exit, restart_backoff_ms, AutoRestartOutcome, ChildObservation,
    CrashRecoveryConfig, CrashRecoveryMachine, ExitClassification, FailureOutcome,
    PostStartOutcome, RestartFate,
};
pub use lifecycle_gate::{
    LifecycleGate, LifecycleKind, PendingDrain, PendingSnapshot, StopDiscard,
};
pub use port_bookkeeping::{PortAllocator, PortExclusions, ResolvedPort};
pub use process_killer::{EscalatedKill, ProcessKiller, Signal};
pub use readiness_gate::{
    wait_for_core_ready, CoreReadyDeps, CoreReadyOutcome, CoreStartRetryError,
    CoreStartSupersededError, WaitForCoreReadyOptions,
};
pub use spawner::{SingBoxSpawner, SpawnError, SpawnRequest, SpawnedChild, TokioSpawner};
pub use stale_core::{is_our_core, is_our_core_raw, scan_running_cores, stale_pids, CoreProcess};
