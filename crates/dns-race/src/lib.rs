//! polaris-dns-race —— C11「节点域名解析多源竞速」sidecar（issue #147 移植）。
//!
//! ## 这个 crate 解决什么
//! 节点 outbound 的 `server` 恒填**域名**（不预解析烧 IP），由内核运行期解析出多 A 后 DialSerial
//! 逐 IP 重试。可这一步解析在境内极易被投毒/污染 —— 单上游一旦被抢答，整个节点就连不上。
//! 本 crate 起一个只监听 `127.0.0.1:<动态口>` 的 UDP DNS server，作为内核解析节点域名的**唯一上游**，
//! 内部把同一个 query 齐射给多个上游竞速，并剔除 GFW decoy 应答。
//!
//! ## 与 sing-box config 的接线（三方约定）
//! 1. 本 crate [`NodeDnsRaceServer::start`] 拿到端口 →
//! 2. src-tauri 把端口喂给 `GenerateConfigDeps::race_server_port`（同时把 [`ResolvedUpstreams::direct_ips`]
//!    喂 `race_upstream_ips`、[`ResolvedUpstreams::direct_ports`] 喂 `race_upstream_ports`，让 route
//!    直连放行这些上游的 IP **与端口**，防 TUN 下回环 —— 两轴缺一，规则就匹配不上，见 `direct_ports`）→
//! 3. config-engine 仅在 `race_server_port > 0` 时生成 `dns-node-race` server 并让节点域名指它；
//!    `=0` 时 `with_race_off` 强制回落单上游（`nodeResolverSingle`）路径。
//!
//! 即：**端口必须先于 config 生成拿到**，且「起不来」与「关掉」在生成侧是同一种状态（port=0），
//! 不存在「config 引用了一个不存在的 server」的中间态（那会让内核 FATAL）。
//!
//! ## 模块
//! - [`wire`]：极小 DNS wire 编解码 + 三态分类（纯函数、零 I/O、逐字节可测）。
//! - [`decoy`]：GFW 投毒 decoy IP 段匹配（纯函数）。段表可由调用方注入（[`DecoySet`]），默认内置。
//! - [`upstream`]：上游模型 + Tier 分桶/去重/上限 + [`upstream::plan_upstreams`] 起不起 sidecar 的唯一决策点（纯函数）。
//! - [`race`]：四态竞速转发核心（上游查询经 trait 注入 ⟹ 单测零网络）。
//! - [`query`]：生产上游查询（UDP / system 自持；**DoH 经 [`query::DohPost`] 注入**，因为整个
//!   workspace 只有 `src-tauri/runtime/http.rs` 持有真实 HTTP/TLS 客户端）。
//! - [`server`]：UDP 薄壳（绑口 / 收发 / watchdog），不含任何解析判定。
//!
//! ## 测试边界（硬约束）
//! 本 crate 的测试**绝不触碰宿主网络**：所有上游经 mock 注入；确需真 socket 的（UDP 上游硬化、
//! sidecar 收发）一律只绑 `127.0.0.1:0`，用完即关。不建 netns/veth、不改路由/iptables/系统 DNS、
//! 不对公网发任何真实 DNS 查询。

#![forbid(unsafe_code)]

pub mod decoy;
pub mod query;
pub mod race;
pub mod server;
/// sidecar 会话内噪音型事件的计数（按条 `debug`、收尾一条 `INFO` 汇总）。
pub mod stats;
pub mod upstream;
pub mod wire;

pub use decoy::{is_decoy_ip, DecoySet, ParsedDecoySet};
pub use query::{DefaultUpstreamQuery, DohPost, PER_UPSTREAM_TIMEOUT};
pub use race::{race_forward, UpstreamQuery, DEFAULT_RACE_BUDGET};
pub use server::{NodeDnsRaceServer, OnRaceServerDead, MAX_INFLIGHT_QUERIES};
pub use upstream::{
    plan_upstreams, resolve_upstreams, ResolveUpstream, ResolvedUpstreams, UpstreamKind,
    DEFAULT_POOL_IDS, DEFAULT_SINGLE_ID, MAX_TIER1_UPSTREAMS,
};
pub use wire::{DnsQuestion, DnsResponseClass};
