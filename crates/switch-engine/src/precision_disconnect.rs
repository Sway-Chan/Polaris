//! 精准断连 —— 上游 `ProxyManager.closeOldNodeConnectionsAfterHotSwitch`（L2015-2068）
//! 与 `connectionMatchesSwitchedPairs`（L267-273）、`SwitchedMemberPair`（L255-258）移植。
//!
//! # 维度7 #19 不变式（capability-registry-special-logic.md）
//!
//! 热切换成功后关掉仍指向**旧成员**的存量连接，使其在新成员重建。基于 pair 模型——
//! 对本次实际改变了指向的每个 selector 取 `(selector_tag, 旧成员 tag)` 对，只关 chains
//! 同时含二者的活连接。
//!
//! 为何必须应用侧显式关（实测坐实 v1.14.0-alpha.40/41，见上游 SagerNet/sing-box#4281）：
//! sing-box selector 的 `interrupt_exist_connections` 对 **routed 连接 no-op**——连接经
//! ConnectionHandler 路径直接 dial，不注册进 selector 的 interrupt group，`SelectOutbound`
//! 触发的 `Interrupt()` 遍历空集。故存量连接不会被内核断，须 `CloseConnection` 逐条关。
//!
//! # chains 语义（实测坐实）
//!
//! chains 含所经全部 selector tag + 最终拨号成员 tag，**嵌套不折叠**：
//! - 「跟全局的规则连接」chains=`['节点','proxy-selector','rule-sel-x']` 同时含 proxy-selector
//!   与节点 tag → 全局切换的 pair `('proxy-selector', 旧节点)` 正确命中。
//! - 「规则固定节点」chains=`['节点','rule-sel-x']` 不含 proxy-selector → 全局切换**不误杀**。
//!
//! 据此：
//! - 全局切换：关全局连接 + 跟全局的规则连接，不误杀「规则固定旧节点」的连接。
//! - 规则切换：对称断连该规则自己的旧连接。
//! - direct / 国内 / LAN / Tailscale force-route 段 / 新成员连接均不动。
//!
//! # pair 模型为何「宁可漏关不误杀」
//!
//! 缺旧成员 tag（pair 不可建）或旧==新（指向未变）的 pair **跳过**该 selector 的断连。
//! 误杀（关掉本该保留的新成员 / 固定规则连接）会导致 app 被迫重连产生抖动；漏关（旧连接
//! 留在旧成员）只是该连接继续走旧路径直到自然结束，对单条存量连接可接受。故 pair 缺失时
//! 跳过而非全量关——与 `CloseAllConnections` 的无差别 RST 形成对比（后者是启用代理 flush 的
//! 正交路径，见 scheduleConnectionFlush）。

#![forbid(unsafe_code)]

use std::collections::HashSet;

use polaris_config_engine::builder::hotswitch::HotSwitchPut;

/// 热切换实际改变了指向的某个 selector 的 `(selector_tag, 旧成员 tag)` 对。
///
/// 上游 `SwitchedMemberPair`（L255-258）。一个 pair 描述「这个 selector 原本指向 oldMemberTag，
/// 现在切到别的成员」——据此关掉 chains 同时含二者、仍走旧成员的活连接。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SwitchedMemberPair {
    /// selector tag（proxy-selector 或 rule-sel-<id>）。
    pub selector_tag: String,
    /// 旧成员 tag（切换前该 selector 指向的成员）。
    pub old_member_tag: String,
}

/// 从热切换 plan 的 puts 提取精准断连用的 pair 列表。
///
/// 上游 `closeOldNodeConnectionsAfterHotSwitch` L2021-2023：
/// 只取实际改变了指向的 selector（有 oldMemberTag 且 old != new）的 pair。
/// 缺 oldMemberTag（该 pair 无旧成员信息）或 old==new（指向未变、无需断）的 put 被过滤。
///
/// 返回的 pair 列表可直接喂给 [`connection_matches_switched_pairs`]。
pub fn switched_pairs_from_puts(puts: &[HotSwitchPut]) -> Vec<SwitchedMemberPair> {
    puts.iter()
        .filter_map(|p| {
            // 缺旧成员 tag（pair 不可建）→ None（该 selector 断连跳过，宁可漏关不误杀）。
            let old = p.old_member_tag.as_ref()?;
            // 旧==新 → 指向未变、无需断。
            if old == &p.member_tag {
                return None;
            }
            Some(SwitchedMemberPair {
                selector_tag: p.selector_tag.clone(),
                old_member_tag: old.clone(),
            })
        })
        .collect()
}

/// 精准断连纯谓词：某连接（chain_list）是否属于本次热切换改变了指向的某个 selector 上、
/// 仍指向旧成员的存量连接。
///
/// 上游 `connectionMatchesSwitchedPairs`（L267-273）。chains **同时含** selector_tag 与
/// old_member_tag 即命中（嵌套不折叠——见模块级 chains 语义说明）。
///
/// - `chain_list` 为空或 None → false（无 chain 信息，不断）。
/// - pairs 为空 → false（本次热切无实际改变指向的 selector）。
/// - 任一 pair 的两个 tag 都在 chain_list 中 → true。
///
/// 导出供单测（Polaris 原文亦导出此谓词供测试）。
pub fn connection_matches_switched_pairs(
    chain_list: Option<&[String]>,
    pairs: &[SwitchedMemberPair],
) -> bool {
    let Some(chains) = chain_list else {
        return false;
    };
    if chains.is_empty() || pairs.is_empty() {
        return false;
    }
    // 转 HashSet：chains 通常 2-3 个 tag，pairs 也很少 >2，但用 contains 避免 O(n*m) 退化。
    let chain_set: HashSet<&String> = chains.iter().collect();
    pairs
        .iter()
        .any(|p| chain_set.contains(&p.selector_tag) && chain_set.contains(&p.old_member_tag))
}

/// gRPC 连接快照的最小镜像（精准断连消费的 ConnectionEvents 首帧里每条连接的字段）。
///
/// 上游 `subscribeConnections` 帧里每条 connection 的 `id` + `chainList` + `closedAt`。
/// sing-box 1.14 Connection schema 子集（仅断连决策所需三字段）。
#[derive(Debug, Clone)]
pub struct ConnectionSnapshot {
    /// 连接 id（CloseConnection 参数）。
    pub id: String,
    /// 该连接的 chain（所经 selector tag + 最终拨号成员 tag，嵌套不折叠）。
    pub chains: Vec<String>,
    /// 关闭时间戳（>0 = 已关闭的死连接，历史环幽灵，不处理）。
    pub closed_at: i64,
}

/// 精准断连结果（供上层观测 / 测试断言）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrecisionDisconnectOutcome {
    /// 命中 pair、被关的连接 id 列表（Polaris closeOldNodeConnectionsAfterHotSwitch 累计的 closed）。
    pub closed_ids: Vec<String>,
}

impl PrecisionDisconnectOutcome {
    /// 关闭的连接数（上游 `closed` 计数器，日志用）。
    pub fn closed_count(&self) -> usize {
        self.closed_ids.len()
    }
}

/// 从一批连接快照中筛出该关的连接 id（精准断连核心）。
///
/// 上游 `closeOldNodeConnectionsAfterHotSwitch` 的 subscribeConnections 首帧处理循环（L2050-2060）：
/// - 跳过 `closed_at > 0` 的死连接（重置帧幽灵历史环）。
/// - 命中 [`connection_matches_switched_pairs`] 的活连接 → 加入关闭列表。
///
/// 返回 [`PrecisionDisconnectOutcome`]（关闭的 id 列表）。调用方据此逐条 `CloseConnection`。
///
/// 纯函数：只决定「该关哪些」，不执行关——关连接的 I/O 由 [`crate::executor::ManagementApi`]
/// trait 承担（测试 mock）。
pub fn select_connections_to_close(
    connections: &[ConnectionSnapshot],
    pairs: &[SwitchedMemberPair],
) -> PrecisionDisconnectOutcome {
    let mut closed_ids = Vec::new();
    for c in connections {
        // 死连接（closed_at > 0）跳过——重置帧可能带历史已关连接的幽灵环。
        if c.closed_at > 0 {
            continue;
        }
        if connection_matches_switched_pairs(Some(&c.chains), pairs) {
            closed_ids.push(c.id.clone());
        }
    }
    PrecisionDisconnectOutcome { closed_ids }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;
    use polaris_config_engine::builder::hotswitch::HotSwitchPut;

    fn pair(selector: &str, old: &str) -> SwitchedMemberPair {
        SwitchedMemberPair {
            selector_tag: selector.into(),
            old_member_tag: old.into(),
        }
    }

    fn put(selector: &str, member: &str, old: Option<&str>) -> HotSwitchPut {
        HotSwitchPut {
            selector_tag: selector.into(),
            member_tag: member.into(),
            old_member_tag: old.map(Into::into),
        }
    }

    // =========================================================================
    // switched_pairs_from_puts
    // =========================================================================

    #[test]
    fn pairs_extracted_from_global_and_rule_puts() {
        let puts = vec![
            put("proxy-selector", "tagB", Some("tagA")),
            put("rule-sel-r1", "tagX", Some("tagY")),
        ];
        let pairs = switched_pairs_from_puts(&puts);
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&pair("proxy-selector", "tagA")));
        assert!(pairs.contains(&pair("rule-sel-r1", "tagY")));
    }

    #[test]
    fn pair_skipped_when_old_member_tag_missing() {
        // 缺 oldMemberTag → 跳过（宁可漏关不误杀）。
        let puts = vec![put("proxy-selector", "tagB", None)];
        assert!(switched_pairs_from_puts(&puts).is_empty());
    }

    #[test]
    fn pair_skipped_when_old_equals_new() {
        // 旧==新 → 指向未变、无需断。
        let puts = vec![put("proxy-selector", "tagA", Some("tagA"))];
        assert!(switched_pairs_from_puts(&puts).is_empty());
    }

    #[test]
    fn pairs_empty_when_no_puts() {
        assert!(switched_pairs_from_puts(&[]).is_empty());
    }

    #[test]
    fn pairs_dedup_not_applied_repeats_preserved() {
        // Polaris 原文不做去重（每个 put 一个 pair）；重复 selector+old 保留（极罕见但忠实移植）。
        let puts = vec![
            put("rule-sel-r1", "tagX", Some("tagY")),
            put("rule-sel-r2", "tagX", Some("tagY")),
        ];
        let pairs = switched_pairs_from_puts(&puts);
        assert_eq!(pairs.len(), 2);
    }

    // =========================================================================
    // connection_matches_switched_pairs —— 维度7 #19 chains 语义
    // =========================================================================

    #[test]
    fn global_switch_matches_following_rule_connection() {
        // #19 核心：跟全局的规则连接 chains=['节点A','proxy-selector','rule-sel-x']
        // 同时含 proxy-selector 与节点A tag → 全局切换 pair ('proxy-selector','节点A') 命中。
        let chains = vec![
            "节点A".to_string(),
            "proxy-selector".into(),
            "rule-sel-x".into(),
        ];
        let pairs = vec![pair("proxy-selector", "节点A")];
        assert!(connection_matches_switched_pairs(Some(&chains), &pairs));
    }

    #[test]
    fn global_switch_does_not_kill_rule_fixed_connection() {
        // #19 核心：规则固定节点 chains=['节点A','rule-sel-x'] 不含 proxy-selector
        // → 全局切换不误杀（chains 嵌套不折叠）。
        let chains = vec!["节点A".to_string(), "rule-sel-x".into()];
        let pairs = vec![pair("proxy-selector", "节点A")];
        assert!(!connection_matches_switched_pairs(Some(&chains), &pairs));
    }

    #[test]
    fn rule_switch_matches_its_own_old_connection() {
        // 规则切换：对称断连该规则自己的旧连接。
        let chains = vec!["节点Y".to_string(), "rule-sel-r1".into()];
        let pairs = vec![pair("rule-sel-r1", "节点Y")];
        assert!(connection_matches_switched_pairs(Some(&chains), &pairs));
    }

    #[test]
    fn rule_switch_does_not_match_other_rule_connection() {
        // 规则 r1 切换不误杀 r2 的连接（selector tag 不同）。
        let chains = vec!["节点Z".to_string(), "rule-sel-r2".into()];
        let pairs = vec![pair("rule-sel-r1", "节点Y")];
        assert!(!connection_matches_switched_pairs(Some(&chains), &pairs));
    }

    #[test]
    fn direct_connection_not_matched() {
        // direct 连接（无 selector tag）不受影响。
        let chains = vec!["direct".to_string()];
        let pairs = vec![pair("proxy-selector", "节点A")];
        assert!(!connection_matches_switched_pairs(Some(&chains), &pairs));
    }

    #[test]
    fn new_member_connection_not_matched() {
        // 切到新成员后新建的连接走新成员 tag，不含旧成员 tag → 不被断（保留）。
        let chains = vec!["节点B".to_string(), "proxy-selector".into()];
        let pairs = vec![pair("proxy-selector", "节点A")]; // 旧是节点A
        assert!(!connection_matches_switched_pairs(Some(&chains), &pairs));
    }

    #[test]
    fn empty_chains_not_matched() {
        let pairs = vec![pair("proxy-selector", "节点A")];
        assert!(!connection_matches_switched_pairs(Some(&[]), &pairs));
    }

    #[test]
    fn none_chains_not_matched() {
        let pairs = vec![pair("proxy-selector", "节点A")];
        assert!(!connection_matches_switched_pairs(None, &pairs));
    }

    #[test]
    fn empty_pairs_never_matches() {
        let chains = vec!["节点A".to_string(), "proxy-selector".into()];
        assert!(!connection_matches_switched_pairs(Some(&chains), &[]));
    }

    #[test]
    fn any_pair_match_wins() {
        // 多 pair（global + rules 同时切）：任一命中即关。
        let chains = vec!["节点A".to_string(), "rule-sel-r1".into()];
        let pairs = vec![
            pair("proxy-selector", "节点A"), // 不命中（chains 无 proxy-selector）
            pair("rule-sel-r1", "节点A"),    // 命中
        ];
        assert!(connection_matches_switched_pairs(Some(&chains), &pairs));
    }

    #[test]
    fn both_global_and_rule_pairs_match_following_connection() {
        // kind=Both：跟全局的规则连接同时被 global pair 与 rule pair 命中（去重由上层关连接时处理）。
        let chains = vec![
            "节点A".to_string(),
            "proxy-selector".into(),
            "rule-sel-r1".into(),
        ];
        let pairs = vec![
            pair("proxy-selector", "节点A"),
            pair("rule-sel-r1", "节点A"),
        ];
        assert!(connection_matches_switched_pairs(Some(&chains), &pairs));
    }

    // =========================================================================
    // select_connections_to_close —— 死连接过滤 + 命中筛选
    // =========================================================================

    #[test]
    fn dead_connections_skipped() {
        // closed_at > 0 的死连接（重置帧幽灵历史环）不处理。
        let conns = vec![
            ConnectionSnapshot {
                id: "alive".into(),
                chains: vec!["节点A".into(), "proxy-selector".into()],
                closed_at: 0,
            },
            ConnectionSnapshot {
                id: "dead".into(),
                chains: vec!["节点A".into(), "proxy-selector".into()],
                closed_at: 1234567890,
            },
        ];
        let pairs = vec![pair("proxy-selector", "节点A")];
        let outcome = select_connections_to_close(&conns, &pairs);
        assert_eq!(outcome.closed_ids, vec!["alive".to_string()]);
        assert_eq!(outcome.closed_count(), 1);
    }

    #[test]
    fn only_matching_alive_connections_closed() {
        let conns = vec![
            ConnectionSnapshot {
                id: "old-global".into(),
                chains: vec![
                    "节点A".into(),
                    "proxy-selector".into(),
                    "rule-sel-r1".into(),
                ],
                closed_at: 0,
            },
            ConnectionSnapshot {
                id: "rule-fixed".into(),
                chains: vec!["节点A".into(), "rule-sel-r2".into()], // 不含 proxy-selector
                closed_at: 0,
            },
            ConnectionSnapshot {
                id: "new-member".into(),
                chains: vec!["节点B".into(), "proxy-selector".into()],
                closed_at: 0,
            },
            ConnectionSnapshot {
                id: "direct".into(),
                chains: vec!["direct".into()],
                closed_at: 0,
            },
        ];
        let pairs = vec![pair("proxy-selector", "节点A")];
        let outcome = select_connections_to_close(&conns, &pairs);
        // 只关 old-global（跟全局走旧节点A的连接）。
        assert_eq!(outcome.closed_ids, vec!["old-global".to_string()]);
    }

    #[test]
    fn empty_connections_yields_empty_outcome() {
        let pairs = vec![pair("proxy-selector", "节点A")];
        let outcome = select_connections_to_close(&[], &pairs);
        assert_eq!(outcome.closed_count(), 0);
    }

    #[test]
    fn empty_pairs_yields_empty_outcome() {
        let conns = vec![ConnectionSnapshot {
            id: "x".into(),
            chains: vec!["proxy-selector".into()],
            closed_at: 0,
        }];
        let outcome = select_connections_to_close(&conns, &[]);
        assert_eq!(outcome.closed_count(), 0);
    }

    #[test]
    fn closed_at_zero_or_negative_treated_as_alive() {
        // 上游 `Number(c.closedAt) > 0`：仅 >0 为死连接；0 / 负 / 缺失视为活连接。
        let conns = vec![
            ConnectionSnapshot {
                id: "zero".into(),
                chains: vec!["节点A".into(), "proxy-selector".into()],
                closed_at: 0,
            },
            ConnectionSnapshot {
                id: "negative".into(),
                chains: vec!["节点A".into(), "proxy-selector".into()],
                closed_at: -1,
            },
        ];
        let pairs = vec![pair("proxy-selector", "节点A")];
        let outcome = select_connections_to_close(&conns, &pairs);
        assert_eq!(
            outcome.closed_ids,
            vec!["zero".to_string(), "negative".into()]
        );
    }
}
