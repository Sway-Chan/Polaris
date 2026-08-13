//! polaris-switch-engine — 节点切换编排（热切换 / 重启 / defer 调度决策）。
//!
//! 消费 `polaris-config-engine` 的 `planHotSwitch` 决策（[`HotSwitchPlan`]），编排实际执行：
//! 把那条 plan 落到「热切换 PUT / 去抖重启 / defer / no-op」哪条腿，并执行热切换的 selector PUT
//! 与精准断连、去抖重启调度。
//!
//! 见 `~/docs/polaris/design/polaris-system-design.md` §B.2（crate 边界 + 职责 + Polaris 锚点）。
//! 行为不变式见 `vault 的能力入口点清单（special-logic 分册）` 维度7（淬火 70 条）。
//!
//! # 模块
//! - [`decision`]：三腿决策（NoOp/HotSwitch/Restart/Defer）——消费 [`HotSwitchPlan`] 纯逻辑分流。
//! - [`executor`]：热切换执行（selector PUT 循环 + 精准断连触发），gRPC 经 [`executor::ManagementApi`] trait 抽象。
//! - [`precision_disconnect`]：精准断连（维度7 #19 pair 模型 + chains 命中谓词）。
//! - [`debounced_restart`]：去抖重启调度（tokio timer + 复用 core-supervisor 的 [`LifecycleGate`]）。
//!
//! # Polaris 锚点
//! 源 `~/Code/polaris/src/main/services/ProxyManager.ts`：
//! - `switchMode`（L1753-1897）：消费 planHotSwitch 的三腿分发。
//! - `scheduleDebouncedRestart`（L1573-1600）：去抖重启调度 + issue #176 单飞。
//! - `closeOldNodeConnectionsAfterHotSwitch`（L2015-2068）：精准断连（维度7 #19）。
//! - `hotSwitchSelector`（L2451-2470）：单 selector PUT。
//! - `connectionMatchesSwitchedPairs`（L267-273）+ `SwitchedMemberPair`（L255-258）：pair 模型谓词。
//!
//! # 移植纪律
//! 1. gRPC / 进程操作经 trait 抽象（[`executor::ManagementApi`]），测试用 mock 注入。
//! 2. 维度7 #19 不变式可测（[`precision_disconnect::connection_matches_switched_pairs`] 纯谓词）。
//! 3. 不耦合 UserConfig 全量 schema / sing-box 进程实例——只消费 config-engine 产物 + 布尔等价输入。
//! 4. `#![forbid(unsafe_code)]` 全 crate，零 unsafe。
//!
//! [`HotSwitchPlan`]: polaris_config_engine::builder::hotswitch::HotSwitchPlan
//! [`LifecycleGate`]: polaris_core_supervisor::lifecycle_gate::LifecycleGate

#![forbid(unsafe_code)]

pub mod debounced_restart;
pub mod decision;
pub mod executor;
pub mod precision_disconnect;

pub use debounced_restart::{
    DebouncedHandle, DebouncedOutcome, DebouncedRestart, RESTART_DEBOUNCE,
};
pub use decision::{decide, DecisionInput, SwitchDecision};
pub use executor::{HotSwitchOutcome, ManagementApi, ManagementError, SwitchExecutor};
pub use precision_disconnect::{
    connection_matches_switched_pairs, select_connections_to_close, switched_pairs_from_puts,
    ConnectionSnapshot, PrecisionDisconnectOutcome, SwitchedMemberPair,
};
