//! polaris-mesh —— Tailscale 登录编排 / 状态流 / 出口对账 + WARP 注册/许可/注销（纯逻辑）。
//!
//! 见 `~/docs/polaris/design/polaris-system-design.md` §B.2（crate 边界 + 职责 + Polaris 锚点）。
//! 实现批次见 §H B8；行为不变式见 vault 的能力入口点清单（special-logic 分册）。
//!
//! ## 架构（1:1 移植自 上游 `src/main/services/`）
//! - [`tailscale_login`]：登录编排（瞬态核 config 生成 + 双写守卫 + 登录状态机；URL 与登录成功
//!   两个信号都取自管理 API STATUS 流）—— 上游 `tailscale-login-core.ts`。
//! - [`tailscale_state`]：状态目录与存在性判定 —— 上游 `tailscale-state.ts`。
//! - [`exit_route`]：出口对账（选中节点 vs TS exit node 状态一致性）—— 上游 `mesh-exit-route-manager.ts`
//!   + `shared/mesh-exit-route.ts`。注：endpoint-routes 的 mesh 路由逻辑已在 config-engine
//!   （`builder/endpoint_routes.rs`），本 crate 只做出口对账。
//! - [`warp`] + [`warp_http`]：WARP 注册（注册 API 调用）/ 许可证管理 / 注销队列 —— 上游 `WarpService.ts`
//!   + `shared/warp.ts`。
//!
//! ## 纯逻辑边界
//! 本 crate **不触碰宿主网络/进程/FS**：
//! - Tailscale 管理 API（STATUS 流）+ WARP 注册 API（CF）+ 平台路由 add/del + utun 查询 + state 目录读
//!   全部经 trait 抽象（[`warp_http::WarpHttp`]、[`exit_route::ExitRouteOp`]、[`tailscale_state::TailscaleStateFs`]），
//!   测试 mock 注入。
//! - 登录编排 / 出口对账是状态机，事件/查询经 trait 注入。
//!
//! ## 维度7 R5：WARP TLS 指纹回归点
//! Cloudflare WAF 校验 TLS 指纹——rustls 的 ClientHello ≠ Node/okhttp → 1020/403。WARP 注册/许可/注销的
//! 网络发送全部抽象到 [`warp_http::WarpHttp`] trait：实现方（应用层）负责用匹配 okhttp/Node TLS 指纹的栈
//! （钉 TLS1.2、HTTP/1.1）发请求，本 crate 只消费响应（status + body），不感知 TLS/HTTP 版本。

// doc_lazy_continuation：中文多行文档注释（对齐 Polaris 源注释风格）非英文列表续行，刻意保留。
#![allow(clippy::doc_lazy_continuation)]
#![forbid(unsafe_code)]

pub mod exit_route;
pub mod tailscale_login;
pub mod tailscale_state;
pub mod warp;
pub mod warp_http;

// 顶层再导出最常用符号，便于 `use polaris_mesh::{...}`。
// Platform 直接从 helper-proto 转 re-export（exit_route 内只 `use`，未 re-export，故在此显式）。
pub use exit_route::{
    mesh_system_supported_on_platform, plan_mesh_exit_route, ExitRouteCancel, ExitRouteLog,
    ExitRouteOp, InstalledRoute, MeshExitRouteManager, MeshExitRoutePlan, NoopExitRouteLog,
};
pub use polaris_helper_proto::Platform;
pub use tailscale_login::{
    advance_login_state, build_tailscale_login_config, login_config_to_json,
    tailscale_endpoint_in_running_core, LoginEvent, LoginState, TailscaleLoginApiService,
    TailscaleLoginConfig,
};
pub use tailscale_state::{state_exists, tailscale_state_dir, TailscaleStateFs};
pub use warp::{
    build_register_body, build_unregister_request, classify_deregister_result,
    enqueue_pending_deregister, parse_register_response, plan_deregister_drain,
    reserved_from_client_id, split_endpoint, DeregisterResult, DrainAction, DrainPlanItem,
    PendingDeregisterEntry, WarpDeviceCreds, WarpDraftMeta, WarpRegisterResult, WarpWireGuardDraft,
    WARP_API_BASE, WARP_API_VERSION, WARP_CLIENT_VERSION, WARP_DEFAULT_ENDPOINT_HOST,
    WARP_DEFAULT_ENDPOINT_PORT, WARP_DEREGISTER_MAX_AGE_MS, WARP_DEREGISTER_MAX_PER_DRAIN,
    WARP_DEREGISTER_MAX_QUEUE, WARP_ENDPOINT_DOMAIN, WARP_USER_AGENT,
};
pub use warp_http::{
    ApplyLicenseResult, NoopWarpLog, RegisterOptions, WarpHttp, WarpHttpMethod, WarpHttpRequest,
    WarpHttpResponse, WarpKeypair, WarpLog, WarpService,
};
