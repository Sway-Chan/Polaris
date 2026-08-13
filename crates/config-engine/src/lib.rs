//! polaris-config-engine — sing-box 配置生成纯函数库（§B.2 / §H B1）。
//!
//! 职责：从 UserConfig 生成 sing-box JSON config（六 builder + 编排 generate_sing_box_config
//! + canonical 序列化/norm 投影 diff + planHotSwitch + 未引用节点 defer 差集）。
//!
//! 纯逻辑、无 I/O 副作用 → B1 gate = 金样对拍（Polaris TS 输出 vs Rust 输出逐字节 diff = 0）。
//!
//! 见 ~/docs/polaris/design/polaris-system-design.md §B.2（crate 边界）+ §H B1（验收门）；
//! 行为不变式见 polaris-polaris-capability-registry-special-logic.md（维度7 淬火 70 条）。
//!
//! 模块：
//! - [`singbox`]：sing-box JSON schema 的 Rust 类型镜像（1:1 Polaris singbox-config-types.ts，零逻辑）。
//! - [`user_config`]：用户配置输入类型（Polaris shared/types.ts UserConfig/ServerConfig 子集）。
//! - [`builder`]：六 builder + 编排（B1 增量移植中，log 已落地）。
//! - [`warp`]：WARP 节点身份判定（`system:true` 否决的判据源，前端 `domain/warp.ts` 的对应物）。

#![forbid(unsafe_code)]

pub mod builder;
pub mod singbox;
pub mod user_config;
pub mod warp;
