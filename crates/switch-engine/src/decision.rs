//! 三腿决策 —— 上游 `ProxyManager.switchMode`（L1753-1897）消费 `planHotSwitch` 产物的纯逻辑分流。
//!
//! config-engine 的 [`plan_hot_switch`](polaris_config_engine::builder::hotswitch::plan_hot_switch)
//! 已产出 [`HotSwitchPlan`]（kind/puts/must_restart）；本模块只做**运行时调度层**的下一步决策：
//! 那条 plan 该落到「热切换执行 / 重启 / defer / no-op」哪条腿，外加配置层的 norm 等价与
//! 「仅新增未引用节点」放行（switchMode 的 no-op / canSkip 两腿前提）。
//!
//! Polaris 原始 switchMode 是巨型方法：判 core-swap / lifecycle busy / 核未起 → planHotSwitch →
//! 热切腿（PUT 成功即记 currentConfig + 精准断连 + syncCustomRuleFiles + emit unlock/IP 事件）→
//! no-op 腿（norm 等价 + selectedServerId 未变）→ canSkip 腿（仅新增未引用节点 + 非 restartOnNodeChange）
//! → 最终去抖重启腿。其中涉及的大量宿主状态（currentConfig / process handle / event emit /
//! customRuleFilesDegraded / meshExitRoute / loginFallback）由上层 actor 持有，本 crate 不耦合。
//!
//! 本模块只暴露**可单测的决策谓词**：给定 plan + 配置层等价信息，返回该走哪条腿。
//! 执行（PUT / 重启调度 / 断连）由 [`crate::executor`]、[`crate::debounced_restart`]、
//! [`crate::precision_disconnect`] 各自承担。
//!
//! 行为不变式（capability-registry-special-logic.md 维度7 #19 + §2 F1/P2-A）：
//! 1. `must_restart=true`（kind=None 但规则目标 dirty/不在 selector）**绝不**落 NoOp / Defer
//!    腿——否则 targetServerId 出 norm 会被静默吞掉（review F1）。见 `None` + `must_restart` → Restart。
//! 2. plan.kind 有 puts → 必须走热切腿（执行 PUT；失败由 executor 退回去抖重启兜底）。
//! 3. norm 等价 + selectedServerId 未变 + 无 must_restart → NoOp（生成无关变更，零重启）。
//! 4. 仅新增未引用节点 + 非 restartOnNodeChange + 无 must_restart → Defer（免整核重启）。
//! 5. 其余 → Restart（去抖合并，断流仅一次）。
//! 6. `defer_restart=true`（暂存层「保存」腿，spec §2.5 Q4）**只降级第 5 条**（结构性变更的去抖重启
//!    → Defer）。`must_restart` 腿与热切腿都不受它影响 —— 前者一降级就回归 上游 F1 那条静默吞，
//!    后者本就零重启、降级无意义。

#![forbid(unsafe_code)]

use polaris_config_engine::builder::hotswitch::{HotSwitchKind, HotSwitchPlan};

/// `ProxyManager.switchMode` 的三腿（+ NoOp / Defer）决策结果。
///
/// 对应 Polaris switchMode 的四个提前返回分支：
/// - [`SwitchDecision::HotSwitch`]：`plan.kind != None`（L1786-1841）→ executor 执行 PUT。
/// - [`SwitchDecision::NoOp`]：norm 等价 + selectedServerId 未变 + 无 must_restart（L1846-1863）。
/// - [`SwitchDecision::Defer`]：仅新增未引用节点 + 非 restartOnNodeChange + 无 must_restart（L1871-1887）。
/// - [`SwitchDecision::Restart`]：最终去抖重启腿（L1889-1896）。
///
/// 注意：core-swap 门控 / lifecycle busy 暂存 / 核未起早退（L1754-1767）属上层 actor 生命周期编排，
/// 不在此决策——那些是「switchMode 是否被调用」的前置闸，而非「plan 已算出后走哪条腿」。
///
/// 不派生 `PartialEq`：`HotSwitch` 变体内嵌的 [`HotSwitchPlan`] 含 `Vec` 且 config-engine 未为其
/// 派生 `PartialEq`（plan 是 planHotSwitch 的产物，比较其字段而非整结构更清晰）。测试用 `matches!`
/// 断言变体，需要时 unwrap 取 plan 字段比对。
#[derive(Debug, Clone)]
pub enum SwitchDecision {
    /// 热切换腿：执行 `plan.puts` 的 selector PUT（clash_api SelectOutbound），不重启 sing-box。
    /// 附带 plan 以便 executor 直接消费（精准断连 pair 也在 puts 内）。
    HotSwitch(HotSwitchPlan),
    /// no-op 腿：生成无关变更（仅 mainSessionViaProxy/订阅元数据/语言等归一化字段，
    /// selectedServerId 未变、无规则 targetServerId 变化）→ 仅更新 currentConfig 引用，零热切零重启。
    NoOp,
    /// defer 腿：本次变更只触及**未被引用**的节点（订阅刷新新增、改址、删除都算）→ 免整核重启，
    /// 下次被选中 / 重启时自然生效。`restart_on_node_change` 关闭时才放行。
    ///
    /// 「未引用节点的改/删也放行」是有意的（`hotswitch.rs` 第 ③ 步）：它们不承载活流量，
    /// 运行核里那条陈旧条目没人用。判据的边界因此落在**「有没有被引用」**，不在「是不是新增」。
    /// 引用面由 `referenced_server_ids` 单点定义，含 selector default 兜底可能命中的节点。
    Defer,
    /// 重启腿：模式/端口/TUN/规则结构/选中或被引用节点集合/interrupt 开关等需重生成配置的项
    /// → 去抖重启应用（窗口内连改多条只重启一次）。包含 §2 F1 的 `must_restart` 强制重启路径。
    Restart,
}

/// 三腿决策的配置层等价输入（上游 `this.currentConfig` 与 `newConfig` 的对比结果，预先算好注入）。
///
/// config-engine 持有 UserConfig 类型与 norm / 未引用差集的纯函数；本 crate 不重复定义，
/// 只消费其布尔结论（避免 switch-engine 耦合 UserConfig 全量 schema）。
#[derive(Debug, Clone, Default)]
pub struct DecisionInput {
    /// norm(old) === norm(new)（config_generation_norm 等价）。switchMode 的 no-op / canSkip 前提。
    pub norm_equal: bool,
    /// old.selectedServerId === new.selectedServerId。no-op 腿要求节点未变（planHotSwitch=none
    /// 已排除规则 targetServerId 变化，但显式复核 selectedServerId 防 plan.kind=None 且节点变）。
    pub selected_server_id_equal: bool,
    /// `can_skip_restart_for_added_unreferenced` 的结果。defer 腿前提。
    ///
    /// ⚠️ **字段名是历史遗留的窄化说法，别照字面理解**：该谓词放行的不止「新增」——
    /// `hotswitch.rs` 的第 ③ 步对「新旧两侧都未被引用」的节点直接放行，即**未引用节点的改与删
    /// 同样落 defer**（P2-B 早已落地）。真正的判据是「本次变更是否只触及未被引用的节点」。
    /// 被引用节点的改/删、以及新增即被引用，都会让它为 false。
    ///
    /// 未按语义改名是因为它同时是 Polaris/上游侧同名概念的对拍锚点；改名要两侧同批。
    pub only_added_unreferenced: bool,
    /// `restart_on_node_change` 开关（switchMode 用它决定节点变更 defer 还是即刻重启）。
    /// true = 节点变更即刻重启（auto-apply 语义）→ 不落 defer 腿。
    pub restart_on_node_change: bool,
    /// 暂存层「保存」腿的**不主动重启**标志（spec §2.5 Q4 选 A / §3.6 P4）。
    ///
    /// 语义：本次落盘由用户点「保存」触发，用户的意图是「先存下来，什么时候断流我自己决定」。
    /// 于是**第 4 腿（结构性变更 → 去抖重启）降级为 Defer**：仍 commit 新配置、仍进待应用差集，
    /// 但不排程重启。用户随后点「立即应用」才断流一次。
    ///
    /// # 射程严格限于第 4 腿（这是本字段唯一的风险面）
    ///
    /// - **不降级 `must_restart` 腿**：`must_restart` 的含义是「不重启就**静默不生效**」
    ///   （规则目标 dirty / 目标不在 selector）。它一旦被降级，变更既没进核也没有任何 UI 说得清
    ///   ——正是 上游 F1 那条 Medium-High 回归（见本模块头注不变式 1）。
    /// - **不降级热切腿**：热切本就零重启、落盘那刻已入核，「保存不重启」对它无事可做。
    /// - **不改 NoOp / 既有 Defer 腿**：那两腿本来就不重启，降无可降。
    ///
    /// # 与 `restart_on_node_change` 的优先级
    ///
    /// `restart_on_node_change=true` + 节点变更时，第 3 腿被挡住 → 落到第 4 腿 → 本标志把它降 Defer。
    /// 这是有意的：`restartOnNodeChange` 是**用户设的默认策略**，而「保存」是用户**此刻的显式动作**，
    /// 显式动作压默认策略。降级后变更进待应用差集、条上可见、可一键应用，不是静默吞。
    ///
    /// 缺省 `false` = 今天行为逐字节不变（`Default` 派生给出 false，所有既有构造点无需改动语义）。
    pub defer_restart: bool,
}

/// 决策：给定 plan + 配置层等价输入，返回该走哪条腿。
///
/// 纯函数，无 I/O、无实例态——可单测。对应 上游 `switchMode` L1785-1896 的四个分支判定
/// （planHotSwitch 后的 if/else 链）。Polaris 把「算 plan」与「判腿」耦合在一个方法里，
/// 这里拆开：plan 由 config-engine 算，腿由本函数判。
///
/// 分支顺序（与 Polaris 严格对齐，顺序即不变式）：
/// 1. `plan.kind != None` → [`SwitchDecision::HotSwitch`]（L1786）。注意 kind=None 但 must_restart
///    不走此腿（无 puts 可执行）。
/// 2. `!plan.must_restart && norm_equal && selected_server_id_equal` → [`SwitchDecision::NoOp`]（L1846）。
///    §2 F1：must_restart 必须先判（防 targetServerId 出 norm 被误吞）。
/// 3. `!plan.must_restart && !restart_on_node_change && only_added_unreferenced` →
///    [`SwitchDecision::Defer`]（L1871）。
/// 4. 其余 → [`SwitchDecision::Restart`]（L1889）；**`defer_restart=true` 时这一腿改返
///    [`SwitchDecision::Defer`]**（落盘 + 进差集，重启时机交给用户）。前三腿不受该标志影响。
pub fn decide(plan: &HotSwitchPlan, input: &DecisionInput) -> SwitchDecision {
    // 腿 1：热切腿——plan 有 puts（kind=global/rules/both）→ 执行 selector PUT。
    if plan.kind != HotSwitchKind::None {
        return SwitchDecision::HotSwitch(plan.clone());
    }

    // 到此 kind=None。§2 F1：must_restart 必须在 no-op/defer 之前判——
    // kind=None + must_restart=true（规则目标 dirty/不在 selector）绝不落 no-op/defer（否则静默吞）。
    if plan.must_restart {
        return SwitchDecision::Restart;
    }

    // 腿 2：no-op——生成无关变更 + 节点未变。
    // norm 等价 ⇔ 生成的 sing-box 配置等价；selectedServerId 未变 → 无热切、无重启。
    if input.norm_equal && input.selected_server_id_equal {
        return SwitchDecision::NoOp;
    }

    // 腿 3：defer——本次只触及未引用节点 + 非节点变更即时重启开关。
    // 安全模型：未被引用 ⇒ 不承载活流量 ⇒ 运行核里那条陈旧条目没人用，增/改/删都可延后
    //（判据是「有没有被引用」，不是「是不是新增」——见 only_added_unreferenced 的字段注释）。
    // restart_on_node_change=true 时节点变更即刻重启 → 不落 defer（auto-apply 语义）。
    if !input.restart_on_node_change && input.only_added_unreferenced {
        return SwitchDecision::Defer;
    }

    // 腿 4：去抖重启（结构性变更 / 模式 / 端口 / TUN / 规则结构 / interrupt 开关 等）。
    // 「保存不重启」只在这里生效：到达本行意味着已排除 must_restart（上面早退）与热切/NoOp/Defer 三腿，
    // 剩下的正是「需要重启才生效、但重启时机可以由用户定」的那一类。
    if input.defer_restart {
        return SwitchDecision::Defer;
    }
    SwitchDecision::Restart
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]

    use super::*;
    use polaris_config_engine::builder::hotswitch::{HotSwitchKind, HotSwitchPut};

    fn empty_plan() -> HotSwitchPlan {
        HotSwitchPlan::default()
    }

    fn plan_with_global_put() -> HotSwitchPlan {
        HotSwitchPlan {
            kind: HotSwitchKind::Global,
            puts: vec![HotSwitchPut {
                selector_tag: "proxy-selector".into(),
                member_tag: "tagB".into(),
                old_member_tag: Some("tagA".into()),
            }],
            must_restart: false,
        }
    }

    fn plan_none_must_restart() -> HotSwitchPlan {
        HotSwitchPlan {
            kind: HotSwitchKind::None,
            puts: Vec::new(),
            must_restart: true,
        }
    }

    #[test]
    fn hotswitch_leg_when_plan_has_puts() {
        // 腿 1：kind=Global → HotSwitch（即使 norm 不等也走此腿：planHotSwitch 已保证 kind!=None 时 norm 相等）。
        let plan = plan_with_global_put();
        let input = DecisionInput {
            norm_equal: true,
            selected_server_id_equal: false,
            ..DecisionInput::default()
        };
        assert!(matches!(
            decide(&plan, &input),
            SwitchDecision::HotSwitch(_)
        ));
    }

    #[test]
    fn hotswitch_leg_rules_kind() {
        let plan = HotSwitchPlan {
            kind: HotSwitchKind::Rules,
            puts: vec![HotSwitchPut {
                selector_tag: "rule-sel-r1".into(),
                member_tag: "tagX".into(),
                old_member_tag: Some("tagY".into()),
            }],
            must_restart: false,
        };
        let input = DecisionInput::default();
        assert!(matches!(
            decide(&plan, &input),
            SwitchDecision::HotSwitch(_)
        ));
    }

    #[test]
    fn hotswitch_leg_both_kind() {
        let plan = HotSwitchPlan {
            kind: HotSwitchKind::Both,
            puts: vec![
                HotSwitchPut {
                    selector_tag: "proxy-selector".into(),
                    member_tag: "tagB".into(),
                    old_member_tag: Some("tagA".into()),
                },
                HotSwitchPut {
                    selector_tag: "rule-sel-r1".into(),
                    member_tag: "tagX".into(),
                    old_member_tag: Some("tagY".into()),
                },
            ],
            must_restart: false,
        };
        assert!(matches!(
            decide(&plan, &DecisionInput::default()),
            SwitchDecision::HotSwitch(_)
        ));
    }

    #[test]
    fn must_restart_overrides_noop_and_defer() {
        // §2 F1：kind=None + must_restart=true → Restart，绝不落 no-op/defer（即使 norm 等价 / 仅新增）。
        let plan = plan_none_must_restart();

        let input_noop = DecisionInput {
            norm_equal: true,
            selected_server_id_equal: true,
            ..DecisionInput::default()
        };
        assert!(matches!(
            decide(&plan, &input_noop),
            SwitchDecision::Restart
        ));

        let input_defer = DecisionInput {
            only_added_unreferenced: true,
            restart_on_node_change: false,
            ..DecisionInput::default()
        };
        assert!(matches!(
            decide(&plan, &input_defer),
            SwitchDecision::Restart
        ));
    }

    #[test]
    fn noop_leg_when_norm_equal_and_selected_unchanged() {
        // 腿 2：norm 等价 + selectedServerId 未变 + 无 must_restart → NoOp。
        let plan = empty_plan();
        let input = DecisionInput {
            norm_equal: true,
            selected_server_id_equal: true,
            ..DecisionInput::default()
        };
        assert!(matches!(decide(&plan, &input), SwitchDecision::NoOp));
    }

    #[test]
    fn noop_not_taken_when_selected_server_id_changed() {
        // selectedServerId 变了但 norm 等价（selectedServerId 出 norm）+ kind=None：
        // 说明切到一个不在运行 selector 的节点（planHotSwitch 退回 None）→ 必须重启。
        let plan = empty_plan();
        let input = DecisionInput {
            norm_equal: true,
            selected_server_id_equal: false,
            ..DecisionInput::default()
        };
        assert!(matches!(decide(&plan, &input), SwitchDecision::Restart));
    }

    #[test]
    fn defer_leg_when_only_added_unreferenced_and_switch_off() {
        // 腿 3：仅新增未引用节点 + restart_on_node_change 关闭 → Defer。
        let plan = empty_plan();
        let input = DecisionInput {
            norm_equal: false,
            selected_server_id_equal: true,
            only_added_unreferenced: true,
            restart_on_node_change: false,
            defer_restart: false,
        };
        assert!(matches!(decide(&plan, &input), SwitchDecision::Defer));
    }

    #[test]
    fn defer_not_taken_when_restart_on_node_change_on() {
        // restart_on_node_change=true → 节点变更即刻重启，不落 defer（auto-apply 语义）。
        let plan = empty_plan();
        let input = DecisionInput {
            only_added_unreferenced: true,
            restart_on_node_change: true,
            ..DecisionInput::default()
        };
        assert!(matches!(decide(&plan, &input), SwitchDecision::Restart));
    }

    #[test]
    fn restart_leg_for_structural_changes() {
        // 腿 4：norm 不等（结构性变更）+ 非仅新增未引用 → Restart。
        let plan = empty_plan();
        let input = DecisionInput {
            norm_equal: false,
            selected_server_id_equal: true,
            only_added_unreferenced: false,
            restart_on_node_change: false,
            defer_restart: false,
        };
        assert!(matches!(decide(&plan, &input), SwitchDecision::Restart));
    }

    #[test]
    fn restart_leg_default_for_empty_input() {
        // 默认输入（全 false / norm 不等）→ Restart。
        let plan = empty_plan();
        assert!(matches!(
            decide(&plan, &DecisionInput::default()),
            SwitchDecision::Restart
        ));
    }

    #[test]
    fn hotswitch_leg_precedence_over_must_restart() {
        // kind!=None 时走热切腿（planHotSwitch 保证 must_restart 仅在 kind=None 时置 true）。
        // 即便 must_restart 误置，只要 kind!=None 仍走热切（plan.puts 是真值）。
        let plan = HotSwitchPlan {
            kind: HotSwitchKind::Global,
            puts: vec![HotSwitchPut {
                selector_tag: "proxy-selector".into(),
                member_tag: "tagB".into(),
                old_member_tag: None,
            }],
            must_restart: true, // 异常组合，但 kind!=None 优先
        };
        assert!(matches!(
            decide(&plan, &DecisionInput::default()),
            SwitchDecision::HotSwitch(_)
        ));
    }

    // ── defer_restart（暂存层「保存」腿，spec §2.5 Q4）─────────────────────────────────
    //
    // 这五条一起钉死「只降级第 4 腿」这一条射程。任意一条缺失都能让实现悄悄扩大射程：
    // 把 `if input.defer_restart` 挪到 `must_restart` 早退之前 → 第二条转红；
    // 挪到函数最前 → 第三/四条转红；漏加这一腿 → 第一条转红。

    #[test]
    fn defer_restart_downgrades_structural_restart_leg() {
        // 唯一被降级的腿：结构性变更（norm 不等、非仅新增未引用、无 must_restart）。
        let plan = empty_plan();
        let input = DecisionInput {
            norm_equal: false,
            selected_server_id_equal: true,
            only_added_unreferenced: false,
            restart_on_node_change: false,
            defer_restart: true,
        };
        assert!(
            matches!(decide(&plan, &input), SwitchDecision::Defer),
            "「保存」腿的结构性变更必须落 Defer（落盘 + 进差集、不排程重启）"
        );
    }

    #[test]
    fn defer_restart_never_downgrades_must_restart() {
        // 不变式 1 的延伸：must_restart 代表「不重启就静默不生效」，绝不可被本标志降级
        //（降了即回归 上游 F1：规则目标出 norm 被静默吞）。
        let plan = plan_none_must_restart();
        let input = DecisionInput {
            defer_restart: true,
            ..DecisionInput::default()
        };
        assert!(
            matches!(decide(&plan, &input), SwitchDecision::Restart),
            "must_restart 腿必须无视 defer_restart 仍走 Restart"
        );
    }

    #[test]
    fn defer_restart_leaves_hotswitch_leg_intact() {
        // 热切腿零重启、落盘即入核 → 「不主动重启」对它无事可做，不得改变分流。
        let plan = plan_with_global_put();
        let input = DecisionInput {
            defer_restart: true,
            ..DecisionInput::default()
        };
        assert!(matches!(
            decide(&plan, &input),
            SwitchDecision::HotSwitch(_)
        ));
    }

    #[test]
    fn defer_restart_leaves_noop_leg_intact() {
        // NoOp 腿本就不重启；若被本标志改写成 Defer，生成无关变更会凭空进「待应用」差集 → 噪音条。
        let plan = empty_plan();
        let input = DecisionInput {
            norm_equal: true,
            selected_server_id_equal: true,
            defer_restart: true,
            ..DecisionInput::default()
        };
        assert!(matches!(decide(&plan, &input), SwitchDecision::NoOp));
    }

    #[test]
    fn defer_restart_outranks_restart_on_node_change() {
        // 显式动作（点「保存」）压默认策略（restartOnNodeChange=true 的 auto-apply）。
        // 降级后变更仍进待应用差集、条上可见，不是静默吞。
        let plan = empty_plan();
        let input = DecisionInput {
            norm_equal: false,
            selected_server_id_equal: false,
            only_added_unreferenced: true,
            restart_on_node_change: true,
            defer_restart: true,
        };
        assert!(matches!(decide(&plan, &input), SwitchDecision::Defer));
    }
}
