//! 节点域名解析竞速转发核心 —— 上游 `shared/node-dns-race.ts` 1:1 移植。
//!
//! 上游查询经 [`UpstreamQuery`] 注入 ⟹ **本模块单测零网络**（mock 上游即可穷举四态）。
//!
//! ## 四态竞速
//! - **HIT 抢跑**：任一上游返回 NOERROR + 含 qtype 记录 → 立即取该上游【完整响应 wire】透传
//!   （回填内核 query id），其余 in-flight 取消；
//! - **POISONED（first-clean-wins）**：HIT 但答案 IP ∈ GFW decoy 段 → 弃之、**不抢跑**、按 FAIL 递减
//!   （等干净上游胜出）；全 settle 只剩 POISONED → 当 FAIL 走 SERVFAIL（fail-safe：宁可失败重试，
//!   也不把用户连到投毒 IP）；
//! - **EMPTY 不抢跑**：空解析（NODATA/NXDOMAIN）不立即用 —— 等本层全部 settle 才下「空」结论
//!   （否则一个快的 NXDOMAIN 会盖掉慢的真答案）；
//! - **FAIL ≠ EMPTY**：上游故障（SERVFAIL/超时/畸形/TC）不算答案；全 FAIL → SERVFAIL。
//!
//! ## Tier 分层
//! 先 Tier1（加密 DoH）抢跑；Tier1 全无 HIT 才查 Tier2（明文/system 兜底，**不**与 Tier1 抢跑）。
//! 整体受 `total_budget` 硬约束。
//!
//! ## 取消语义（TS AbortController → Rust）
//! TS 侧靠 `AbortController` 显式取消其余上游；Rust 侧 future 天然「drop 即取消」——
//! 抢跑时直接 `return`，[`FuturesUnordered`] 随栈销毁把未完成的上游查询一并析构。
//! 预算到点同理。故不需要、也**不应该**再造一层 abort 标志（那会是第二个真值）。

#![forbid(unsafe_code)]

use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::time::Instant;

use crate::decoy::DecoySet;
use crate::upstream::{ResolveUpstream, ResolvedUpstreams};
use crate::wire::{
    build_servfail, classify_dns_response, decode_dns_question, extract_answer_ip_bytes,
    set_dns_message_id, DnsResponseClass,
};

/// 竞速总预算（上游 `DEFAULT_RACE_BUDGET_MS`）。超时即用已有 EMPTY 收口，无 EMPTY 则 SERVFAIL。
pub const DEFAULT_RACE_BUDGET: Duration = Duration::from_millis(2000);

/// 单上游查询注入面：发 query wire → 响应 wire。
///
/// `Err` ⟺ FAIL（超时 / 网络错 / 拒绝 / 不支持的上游形态）。实现方**必须**自带单上游超时，
/// 否则一个永不返回的上游会把本层拖到总预算耗尽（见 [`crate::query::DefaultUpstreamQuery`]）。
#[async_trait]
pub trait UpstreamQuery: Send + Sync {
    async fn query(&self, upstream: &ResolveUpstream, query: &[u8]) -> Result<Vec<u8>, String>;
}

/// HIT 响应的答案 IP 是否含 GFW decoy 段（→ 判 POISONED，弃之）。上游 `isPoisonedResponse`。
///
/// `decoys` 由调用方注入而非直读内置常量：段表要能跟 geo 资源同节奏更新（见 [`DecoySet`] 模块文档），
/// 而读文件/挑路径不属于本 crate —— 注入是唯一能同时保住「可更新」与「纯函数、零 I/O」的形态。
fn is_poisoned_response(resp: &[u8], decoys: &DecoySet) -> bool {
    extract_answer_ip_bytes(resp)
        .iter()
        .any(|ip| decoys.contains(ip))
}

/// 单层竞速结果。`hit` 有值 ⟺ 本层抢跑成功。
#[derive(Debug, Default)]
struct TierResult {
    hit: Option<Vec<u8>>,
    /// 本层见到的**第一个** EMPTY 整包（保留原始 NXDOMAIN/NODATA 语义透传给内核）。
    empty: Option<Vec<u8>>,
}

/// 单层竞速：并发查一组上游。HIT 抢跑（first-clean-wins）；无 HIT 则等全部 settle。
/// 预算到点 → 用已收集到的 EMPTY 收口（未完成的上游随 `FuturesUnordered` drop 取消）。
async fn race_tier(
    query: &[u8],
    qtype: u16,
    upstreams: &[ResolveUpstream],
    fetch: &dyn UpstreamQuery,
    deadline: Instant,
    decoys: &DecoySet,
) -> TierResult {
    if upstreams.is_empty() {
        return TierResult::default();
    }
    let mut inflight: FuturesUnordered<_> = upstreams
        .iter()
        .map(|up| async move { fetch.query(up, query).await })
        .collect();
    let mut empty: Option<Vec<u8>> = None;
    let timer = tokio::time::sleep_until(deadline);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            settled = inflight.next() => match settled {
                None => return TierResult { hit: None, empty }, // 本层全部 settle，无 HIT
                Some(Err(_)) => {}                              // FAIL：不抢跑，继续等其余
                Some(Ok(resp)) => match classify_dns_response(&resp, qtype) {
                    DnsResponseClass::Hit => {
                        // first-clean-wins：HIT 但答案含 GFW decoy → POISONED，弃之、按 FAIL 递减。
                        // 删掉这一段 = 投毒应答会抢跑（它总是最快的那个）→ 用户被连到伪造 IP。
                        if is_poisoned_response(&resp, decoys) {
                            // 按条 `debug` + 计数，会话结束由停 sidecar 腿汇总一条 INFO：
                            // 这是**防护生效**的标志而非异常，按条 WARN 会把真异常淹掉（见 `stats` 模块）。
                            crate::stats::record_poisoned_dropped();
                            log::debug!(
                                "dns-race: 上游 {} 返回 decoy 答案，判 POISONED 丢弃（first-clean-wins）",
                                upstream_label(upstreams, &resp)
                            );
                            continue;
                        }
                        return TierResult { hit: Some(resp), empty };
                    }
                    DnsResponseClass::Empty => {
                        if empty.is_none() {
                            empty = Some(resp); // 记下但**不**抢跑
                        }
                    }
                    DnsResponseClass::Fail => {}
                },
            },
            () = &mut timer => return TierResult { hit: None, empty },
        }
    }
}

/// POISONED 日志里的上游标识。`FuturesUnordered` 不保序、拿不回是哪个上游返回的，故只报本层规模
/// （漂移信号看的是**计数**不是归属）。单独抽出来是为了让 `race_tier` 主流程不被字符串拼装淹没。
fn upstream_label(upstreams: &[ResolveUpstream], resp: &[u8]) -> String {
    format!("(本层 {} 个之一，应答 {}B)", upstreams.len(), resp.len())
}

/// 竞速转发主入口：内核 query wire → 四态竞速（Tier1 抢跑 → Tier2 兜底）→ 响应 wire（回填内核 id）。
///
/// **绝不返回 Err**（fail-open 第一层）：畸形 query / 全上游 FAIL / 预算耗尽一律回 SERVFAIL ——
/// 挂着不回会让内核那条 Lookup 一直等到它自己的超时，比明确失败更糟。
/// HIT / EMPTY 透传命中上游的【完整响应】（多 A / TTL / CNAME 全保留，供内核 DialSerial 逐 IP 重试）。
pub async fn race_forward(
    query: &[u8],
    upstreams: &ResolvedUpstreams,
    fetch: &dyn UpstreamQuery,
    total_budget: Duration,
    decoys: &DecoySet,
) -> Vec<u8> {
    let Some(q) = decode_dns_question(query) else {
        return build_servfail(query);
    };
    let deadline = Instant::now() + total_budget;

    // 阶段 1：Tier1 抢跑。
    let r1 = race_tier(query, q.qtype, &upstreams.tier1, fetch, deadline, decoys).await;
    if let Some(hit) = r1.hit {
        return set_dns_message_id(&hit, q.id);
    }
    let mut empty = r1.empty;

    // 阶段 2：Tier1 无 HIT 且预算未尽 → Tier2 兜底（不与 Tier1 抢跑）。
    if Instant::now() < deadline && !upstreams.tier2.is_empty() {
        let r2 = race_tier(query, q.qtype, &upstreams.tier2, fetch, deadline, decoys).await;
        if let Some(hit) = r2.hit {
            return set_dns_message_id(&hit, q.id);
        }
        if empty.is_none() {
            empty = r2.empty;
        }
    }

    // 阶段 3：有 EMPTY → 如实空（NODATA/NXDOMAIN 透传，回填 id）；全 FAIL → SERVFAIL。
    match empty {
        Some(e) => set_dns_message_id(&e, q.id),
        None => build_servfail(query),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::{resolve_upstreams, UpstreamKind};
    use crate::wire::{
        build_answer_response, encode_dns_query, AnswerRecord, DnsResponseClass, TYPE_A,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 单个上游的脚本化行为。**零网络**：只是延时后吐一段预置 wire（或报错）。
    #[derive(Clone)]
    enum Script {
        /// 延时后返回给定响应。
        Reply(Duration, Vec<u8>),
        /// 延时后 FAIL。
        Fail(Duration),
        /// 永不返回（模拟挂死上游；由预算兜）。
        Hang,
    }

    /// mock 上游查询：按上游 id 派发脚本，并记录被真正查过的上游（验「Tier2 不与 Tier1 抢跑」）。
    struct MockQuery {
        scripts: HashMap<String, Script>,
        calls: Arc<std::sync::Mutex<Vec<String>>>,
        concurrent_peak: Arc<AtomicUsize>,
        live: Arc<AtomicUsize>,
    }

    impl MockQuery {
        fn new(scripts: &[(&str, Script)]) -> Self {
            Self {
                scripts: scripts
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), v.clone()))
                    .collect(),
                calls: Arc::new(std::sync::Mutex::new(Vec::new())),
                concurrent_peak: Arc::new(AtomicUsize::new(0)),
                live: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn queried(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl UpstreamQuery for MockQuery {
        async fn query(&self, up: &ResolveUpstream, _q: &[u8]) -> Result<Vec<u8>, String> {
            self.calls.lock().unwrap().push(up.id.clone());
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.concurrent_peak.fetch_max(live, Ordering::SeqCst);
            let script = self
                .scripts
                .get(&up.id)
                .cloned()
                .unwrap_or(Script::Fail(Duration::from_millis(1)));
            let out = match script {
                Script::Reply(d, wire) => {
                    tokio::time::sleep(d).await;
                    Ok(wire)
                }
                Script::Fail(d) => {
                    tokio::time::sleep(d).await;
                    Err("mock fail".into())
                }
                Script::Hang => {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            };
            self.live.fetch_sub(1, Ordering::SeqCst);
            out
        }
    }

    fn q_wire() -> Vec<u8> {
        encode_dns_query("node.example.com", TYPE_A, 0x4242)
    }

    fn a_reply(q: &[u8], ips: &[[u8; 4]]) -> Vec<u8> {
        let answers: Vec<AnswerRecord> = ips
            .iter()
            .map(|ip| AnswerRecord {
                rtype: TYPE_A,
                rdata: ip.to_vec(),
            })
            .collect();
        // 用不同的 message id 造响应，验「透传前必须回填内核 id」。
        let mut r = build_answer_response(q, &answers);
        r[0] = 0xff;
        r[1] = 0xff;
        r
    }

    fn ups(pool: &[&str]) -> ResolvedUpstreams {
        resolve_upstreams(
            &pool.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
            &[],
        )
    }

    #[tokio::test]
    async fn hit_wins_and_message_id_is_rewritten() {
        let q = q_wire();
        let mock = MockQuery::new(&[
            (
                "ali",
                Script::Reply(Duration::from_millis(5), a_reply(&q, &[[1, 2, 3, 4]])),
            ),
            ("dnspod", Script::Fail(Duration::from_millis(1))),
        ]);
        let out = race_forward(
            &q,
            &ups(&["ali", "dnspod"]),
            &mock,
            DEFAULT_RACE_BUDGET,
            &DecoySet::builtin(),
        )
        .await;
        assert_eq!(classify_dns_response(&out, TYPE_A), DnsResponseClass::Hit);
        assert_eq!(&out[..2], &q[..2], "响应 id 必须回填成内核 query 的 id");
        assert_eq!(
            &out[12..],
            &a_reply(&q, &[[1, 2, 3, 4]])[12..],
            "整包透传，不重编码"
        );
    }

    /// 【不变式：first-clean-wins 剔 decoy】
    /// 变异验证：删掉 `race_tier` 里的 `is_poisoned_response` 分支（让 POISONED 走正常 HIT 抢跑）
    /// → 本测试拿到 31.13.95.169 而非 93.184.216.34 → 转红。
    #[tokio::test]
    async fn poisoned_hit_is_discarded_and_clean_slow_upstream_wins() {
        let q = q_wire();
        let mock = MockQuery::new(&[
            // 投毒应答**更快**（GFW 抢答的真实形态）。
            (
                "ali",
                Script::Reply(Duration::from_millis(1), a_reply(&q, &[[31, 13, 95, 169]])),
            ),
            // 干净答案慢 30ms。
            (
                "dnspod",
                Script::Reply(
                    Duration::from_millis(30),
                    a_reply(&q, &[[93, 184, 216, 34]]),
                ),
            ),
        ]);
        let out = race_forward(
            &q,
            &ups(&["ali", "dnspod"]),
            &mock,
            DEFAULT_RACE_BUDGET,
            &DecoySet::builtin(),
        )
        .await;
        assert_eq!(
            extract_answer_ip_bytes(&out),
            vec![vec![93, 184, 216, 34]],
            "decoy 抢答必须被弃，干净上游胜出"
        );
    }

    /// 全上游都投毒 → 当 FAIL 处理 → SERVFAIL（fail-safe：宁可失败重试，也不连 decoy）。
    #[tokio::test]
    async fn all_poisoned_degrades_to_servfail_not_decoy() {
        let q = q_wire();
        let mock = MockQuery::new(&[
            (
                "ali",
                Script::Reply(Duration::from_millis(1), a_reply(&q, &[[31, 13, 95, 169]])),
            ),
            (
                "dnspod",
                Script::Reply(Duration::from_millis(2), a_reply(&q, &[[157, 240, 17, 35]])),
            ),
        ]);
        let out = race_forward(
            &q,
            &ups(&["ali", "dnspod"]),
            &mock,
            DEFAULT_RACE_BUDGET,
            &DecoySet::builtin(),
        )
        .await;
        assert_eq!(classify_dns_response(&out, TYPE_A), DnsResponseClass::Fail);
        assert_eq!(out[3] & 0x0f, 2, "RCODE=SERVFAIL");
    }

    /// 【不变式：fail-open】全上游 FAIL / 上游挂死 / 畸形 query —— 一律有回包，绝不挂着不回。
    /// 变异验证：把阶段 3 的 `None => build_servfail(query)` 改成返回空 `Vec` 或让函数 hang
    /// → 本测试转红（收不到合法 SERVFAIL / 超时）。
    #[tokio::test]
    async fn all_fail_returns_servfail_with_echoed_id() {
        let q = q_wire();
        let mock = MockQuery::new(&[
            ("ali", Script::Fail(Duration::from_millis(1))),
            ("dnspod", Script::Fail(Duration::from_millis(2))),
        ]);
        let out = race_forward(
            &q,
            &ups(&["ali", "dnspod"]),
            &mock,
            DEFAULT_RACE_BUDGET,
            &DecoySet::builtin(),
        )
        .await;
        assert!(out.len() >= 12);
        assert_eq!(&out[..2], &q[..2], "SERVFAIL 也要回声 id，否则内核直接丢弃");
        assert_eq!(out[3] & 0x0f, 2, "RCODE=SERVFAIL");
        assert_eq!(out[2] & 0x80, 0x80, "QR=1");
    }

    #[tokio::test]
    async fn hung_upstreams_are_cut_by_total_budget() {
        let q = q_wire();
        let mock = MockQuery::new(&[("ali", Script::Hang), ("dnspod", Script::Hang)]);
        let t0 = std::time::Instant::now();
        let out = race_forward(
            &q,
            &ups(&["ali", "dnspod"]),
            &mock,
            Duration::from_millis(60),
            &DecoySet::builtin(),
        )
        .await;
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "必须被预算切断，不等到天荒地老"
        );
        assert_eq!(out[3] & 0x0f, 2, "预算耗尽且无 EMPTY → SERVFAIL");
    }

    #[tokio::test]
    async fn malformed_query_returns_servfail_without_touching_upstreams() {
        let mock = MockQuery::new(&[]);
        let out = race_forward(
            &[0u8; 5],
            &ups(&["ali"]),
            &mock,
            DEFAULT_RACE_BUDGET,
            &DecoySet::builtin(),
        )
        .await;
        assert!(out.len() >= 12);
        assert!(mock.queried().is_empty(), "畸形 query 不该打上游");
    }

    #[tokio::test]
    async fn empty_does_not_preempt_and_is_only_used_after_all_settle() {
        let q = q_wire();
        // ali 秒回 NODATA，dnspod 30ms 后回真答案 —— EMPTY 不得抢跑。
        let mock = MockQuery::new(&[
            (
                "ali",
                Script::Reply(Duration::from_millis(1), a_reply(&q, &[])),
            ),
            (
                "dnspod",
                Script::Reply(Duration::from_millis(30), a_reply(&q, &[[5, 6, 7, 8]])),
            ),
        ]);
        let out = race_forward(
            &q,
            &ups(&["ali", "dnspod"]),
            &mock,
            DEFAULT_RACE_BUDGET,
            &DecoySet::builtin(),
        )
        .await;
        assert_eq!(extract_answer_ip_bytes(&out), vec![vec![5, 6, 7, 8]]);
    }

    #[tokio::test]
    async fn empty_is_passed_through_when_every_upstream_says_empty() {
        let q = q_wire();
        let mock = MockQuery::new(&[
            (
                "ali",
                Script::Reply(Duration::from_millis(1), a_reply(&q, &[])),
            ),
            (
                "dnspod",
                Script::Reply(Duration::from_millis(2), a_reply(&q, &[])),
            ),
        ]);
        let out = race_forward(
            &q,
            &ups(&["ali", "dnspod"]),
            &mock,
            DEFAULT_RACE_BUDGET,
            &DecoySet::builtin(),
        )
        .await;
        assert_eq!(classify_dns_response(&out, TYPE_A), DnsResponseClass::Empty);
        assert_eq!(&out[..2], &q[..2]);
        assert_ne!(out[3] & 0x0f, 2, "空解析不得伪装成 SERVFAIL");
    }

    #[tokio::test]
    async fn tier2_is_not_queried_when_tier1_hits() {
        let q = q_wire();
        let mock = MockQuery::new(&[
            (
                "ali",
                Script::Reply(Duration::from_millis(2), a_reply(&q, &[[1, 1, 1, 1]])),
            ),
            (
                "system",
                Script::Reply(Duration::from_millis(1), a_reply(&q, &[[2, 2, 2, 2]])),
            ),
        ]);
        let u = ups(&["ali", "system"]);
        assert_eq!(u.tier2[0].kind, UpstreamKind::System);
        let out = race_forward(&q, &u, &mock, DEFAULT_RACE_BUDGET, &DecoySet::builtin()).await;
        assert_eq!(extract_answer_ip_bytes(&out), vec![vec![1, 1, 1, 1]]);
        assert_eq!(mock.queried(), vec!["ali"], "Tier1 命中 → Tier2 一次都不打");
    }

    #[tokio::test]
    async fn tier2_backs_up_when_tier1_all_fail() {
        let q = q_wire();
        let mock = MockQuery::new(&[
            ("ali", Script::Fail(Duration::from_millis(1))),
            ("dnspod", Script::Fail(Duration::from_millis(1))),
            (
                "system",
                Script::Reply(Duration::from_millis(2), a_reply(&q, &[[3, 3, 3, 3]])),
            ),
        ]);
        let out = race_forward(
            &q,
            &ups(&["ali", "dnspod", "system"]),
            &mock,
            DEFAULT_RACE_BUDGET,
            &DecoySet::builtin(),
        )
        .await;
        assert_eq!(extract_answer_ip_bytes(&out), vec![vec![3, 3, 3, 3]]);
    }

    #[tokio::test]
    async fn tier1_upstreams_run_concurrently_not_serially() {
        let q = q_wire();
        let mock = MockQuery::new(&[
            ("ali", Script::Fail(Duration::from_millis(40))),
            (
                "dnspod",
                Script::Reply(Duration::from_millis(5), a_reply(&q, &[[4, 4, 4, 4]])),
            ),
        ]);
        let t0 = std::time::Instant::now();
        let out = race_forward(
            &q,
            &ups(&["ali", "dnspod"]),
            &mock,
            DEFAULT_RACE_BUDGET,
            &DecoySet::builtin(),
        )
        .await;
        assert_eq!(extract_answer_ip_bytes(&out), vec![vec![4, 4, 4, 4]]);
        assert!(t0.elapsed() < Duration::from_millis(40), "串行就会 ≥40ms");
        assert_eq!(mock.concurrent_peak.load(Ordering::SeqCst), 2, "同层齐射");
    }
}
