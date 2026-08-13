//! polaris-unlock —— 流媒体/AI 服务解锁检测（ChatGPT/Claude/Gemini/Netflix/Disney+/TikTok/Spotify
//! **已上线**；Grok 已实现但**停飞**待真机标定，见 [`types::ServiceId::PENDING_CALIBRATION`]）。
//!
//! 见 ~/docs/polaris/design/polaris-system-design.md §B.2（crate 边界 + 职责 + Polaris 锚点）。
//! 实现批次见 §H B7；行为不变式见 vault 的能力入口点清单（special-logic 分册）。
//!
//! ## 架构（1:1 移植自 上游 `src/main/services/unlock/`）
//! - [`types`]：检测状态/结果类型 SoT（上游 `shared/unlock-detection.ts`）。
//! - [`http`]：HTTP 传输 trait `UnlockHttp`（取代 Polaris Electron `net.request`）—— electron 依赖隔离的支点。
//! - [`trace`]：Cloudflare cdn-cgi/trace 解析 + IPv4/IPv6 校验（上游 `IpInfoService.parseTrace` + Node `net.isIP`）。
//! - [`endpoints`]：端点 / marker / titleId / apiKey / 正则集中常量（上游 `unlock-endpoints.ts`）。
//! - [`browser`]：与 UA 自洽的 Chrome 请求头集（`Accept`/`Accept-Encoding`/`sec-ch-ua`/`Sec-Fetch-*`）——
//!   补 上游侧由 Chromium `net` 栈隐式自动补齐、Rust 侧必须显式发的那批头。
//! - [`challenge`]：JS 挑战 / CF 1020 防火墙拒绝通用分类器（`cf-mitigated` 主判据 + 强 body marker 辅判据）。
//! - [`checkers`]：逐服务判定逻辑（URL + 响应判定；6 服务 1:1 移植 上游 `checkers.ts`，
//!   tiktok 对齐 1-stream/RegionRestrictionCheck、grok 按本仓设计的弱检测新增 —— 二者 上游 无 oracle）。
//! - [`detector`]：调度 + 结果聚合（上游 `UnlockDetectionService` 核心，剥离 electron 状态机壳）。
//!
//! ## 纯逻辑边界
//! 本 crate 不触碰宿主网络：所有 HTTP 经 [`http::UnlockHttp`] trait，测试 mock 注入。
//! Polaris 中与 Electron/进程状态强耦合的编排策略（session pin/socks5、缓存 TTL/egressIp key、
//! epoch 失效、就绪门退避、settle-retry、IPC 事件广播）属 B7 Tauri 应用层，基于本纯逻辑层重建。

#![forbid(unsafe_code)]

pub mod browser;
pub mod challenge;
pub mod checkers;
pub mod detector;
pub mod endpoints;
pub mod http;
pub mod ipip;
pub mod trace;
pub mod types;

// 顶层再导出最常用符号，便于 `use polaris_unlock::{UnlockDetector, UnlockHttp, ...}`。
pub use browser::{browser_headers, RequestProfile};
pub use challenge::{classify, ChallengeKind};
pub use checkers::{
    check_chatgpt, check_claude, check_disney, check_gemini, check_grok, check_netflix,
    check_spotify, check_tiktok, classify_tiktok, run_checker,
};
pub use detector::{
    detect_all, is_restricted_egress_region, probe_egress, run_checkers_with_progress,
    UnlockDetector,
};
pub use endpoints::{
    ChatgptEndpoints, ClaudeEndpoints, DisneyEndpoints, GeminiEndpoints, GrokEndpoints,
    NetflixEndpoints, SpotifyEndpoints, TiktokEndpoints,
};
pub use http::{HttpMethod, RedirectHop, UnlockHttp, UnlockRequest, UnlockResponse};
pub use ipip::{cc_from_ipip_location, parse_ipip, IpipInfo};
pub use trace::{is_valid_ip, parse_trace, TraceInfo};
pub use types::{
    ServiceId, UnlockBlockedReason, UnlockEgress, UnlockProgress, UnlockResult, UnlockSnapshot,
    UnlockStatus,
};
