//! polaris-net-stack — 网络应用层纯逻辑（订阅解析 + SSRF guard + 安全重定向获取）。
//!
//! 见 `~/docs/polaris/design/polaris-system-design.md` §B.2（crate 边界）。
//! 实现 Polaris 三个源文件的纯逻辑切片（1:1 语义，无真实网络请求）：
//! - [`ssrf`]：SSRF guard（`shared/ssrf-guard.ts`）。
//! - [`safe_redirect`]：逐跳 SSRF 复检的安全重定向 fetch（`main/safe-redirect-fetch.ts`）。
//! - [`clash_parser`]：Clash YAML → ServerConfig[]（`main/services/ClashSubscriptionParser.ts`）。
//! - [`share_link`]：分享链接（vless/vmess/ss/…）→ ServerConfig（`main/services/ProtocolParser.ts`）。
//! - [`xray_import`]：Xray/v2ray JSON outbound → ServerConfig（`main/services/xray-import.ts`）。
//! - [`singbox_import`]：sing-box JSON outbound → ServerConfig（`SubscriptionService.parseSingboxOutbounds`）。
//! - [`subscription`]：订阅 URL 安全校验 + 拉取 + 解析调度（SubscriptionService 纯逻辑切片）。
//! - [`subscription_error`]：订阅错误分类 + i18n key 映射（审计 §C4）。
//!
//! 纯逻辑、零网络副作用：HTTP 客户端用 [`safe_redirect::HttpClient`] trait 抽象、DNS 解析用
//! [`ssrf::DnsLookup`] trait 注入（测试 mock）。ServerConfig / 协议设置 / IP / CIDR 类型复用
//! `polaris-config-engine`（Polaris shared/types.ts + shared/ip.ts 单一真值，避免双真值漂移）。

#![forbid(unsafe_code)]

pub mod clash_parser;
pub mod safe_redirect;
pub mod share_link;
pub mod singbox_import;
pub mod ssrf;
pub mod subscription;
pub mod subscription_error;
pub mod xray_import;
