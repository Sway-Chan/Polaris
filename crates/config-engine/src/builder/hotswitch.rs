//! 热切换判定纯逻辑（上游 `ProxyManager.ts` planHotSwitch / planRuleHotSwitch /
//! canSkipRestartForAddedUnreferenced / isServerDirty / winTunBlocksHotSwitch /
//! resolveGlobalExitTag 1:1 移植）。
//!
//! 所有运行态（currentConfig / currentIdToTagMap / bootstrapFallbackEngaged /
//! currentRuleTargetMap / runningServersFingerprint）经 [`HotSwitchDeps`] 注入 ——
//! 原始 ProxyManager 读 `this.*`，此处全抽到参数，无 I/O、无实例态、可单测。
//!
//! 前提：norm(old) === norm(new)（由调用方经 [`super::orchestration::config_generation_norm`]
//! 保证，本模块内部亦复检一次）。

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use crate::builder::endpoint_routes::{
    endpoint_forced_route_cidrs, mesh_always_routes_subnets, referenced_server_ids,
};
use crate::builder::orchestration::{config_generation_norm, server_fingerprint};
use crate::builder::route::mesh_selected_exit_falls_back_to_direct;
use crate::user_config::app_config::UserConfig;
use crate::user_config::dns_constants::{
    is_block_selection, is_direct_selection, DIRECT_TAG, PROXY_SELECTOR_TAG,
};
use crate::user_config::server_config::{is_mesh_node, ServerConfig};
use crate::user_config::tun_stack::{resolve_tun_stack, ConcreteTunStack};

// ============================================================================
// 类型
// ============================================================================

/// planHotSwitch 注入的运行态依赖（上游 `this.*` 态的纯化镜像）。
#[derive(Debug, Clone, Default)]
pub struct HotSwitchDeps {
    /// id → outbound tag（启动时映射；结构等价 ⇒ 不变，热切可复用）。
    /// 上游 `this.currentIdToTagMap`。None = 未注入（规则目标无法解析 → 规则热切返 None）。
    pub current_id_to_tag_map: Option<BTreeMap<String, String>>,
    /// id → serverFingerprint 运行核快照（启动时 serverFingerprint）。
    /// 上游 `this.runningServersFingerprint`。None = 核未起（dirty 判定返 false）。
    pub running_servers_fingerprint: Option<BTreeMap<String, String>>,
    /// ruleKey → RuleTargetEntry（生成时 rule-sel 元数据）。
    /// 上游 `this.currentRuleTargetMap`。None = 启动无 rule-sel（规则热切返空 Vec）。
    pub current_rule_target_map: Option<BTreeMap<String, RuleTargetEntry>>,
    /// 登录期出口让位态：proxy-selector 实际指 direct（非 config 选中节点 tag）。
    /// 上游 `this.bootstrapFallbackEngaged`。
    pub bootstrap_fallback_engaged: bool,
    /// 平台标识（process.platform 字面：win32/darwin/linux）。winTunBlocksHotSwitch 用。
    pub platform: String,
}

/// currentRuleTargetMap 的条目：生成时 rule-sel 的 selectorTag + memberTag(default)。
/// 上游 `{ selectorTag, memberTag }`。memberTag 仅展示用（planRuleHotSwitch 从 oldTarget 现算旧 tag）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleTargetEntry {
    /// 该规则对应的 rule-sel selector tag（如 "rule-sel-r1"）。
    pub selector_tag: String,
    /// 生成时的 default memberTag（启动时 targetServerId 解析；planRuleHotSwitch 不依赖，仅兼容结构）。
    pub member_tag: String,
}

/// 热切换规划结果四态分类。上游 `kind: 'none'|'global'|'rules'|'both'`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HotSwitchKind {
    /// 无热切换可行（结构变更/目标不在 selector/Win gvisor guard）→ 退回去抖重启或正常分流。
    #[default]
    None,
    /// 仅 selectedServerId 变 → PUT proxy-selector。
    Global,
    /// 仅规则 targetServerId 变 → PUT 各 rule-sel-<id>。
    Rules,
    /// 全局 + 规则同时变 → PUT proxy-selector + 各 rule-sel。
    Both,
}

/// 一条 selector PUT 操作（热切换下发项）。上游 `{ selectorTag, memberTag, oldMemberTag? }`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotSwitchPut {
    /// 目标 selector tag（proxy-selector 或 rule-sel-<id>）。
    pub selector_tag: String,
    /// 新成员 tag（PUT 目标）。
    pub member_tag: String,
    /// 旧成员 tag（精准断连 pair 用；缺失→该 pair 断连跳过）。Polaris oldMemberTag（可选）。
    pub old_member_tag: Option<String>,
}

/// planHotSwitch 规划结果。上游 `planHotSwitch` 返回值。
#[derive(Debug, Clone, Default)]
pub struct HotSwitchPlan {
    /// 四态分类。
    pub kind: HotSwitchKind,
    /// 需 PUT 的 selector 列表。
    pub puts: Vec<HotSwitchPut>,
    /// §2 P2-B review F1：kind=None 但规则目标无法热切（目标 dirty/不在 selector）→ 必须重启
    /// （防 no-op/canSkip 腿吞掉 targetServerId 出 norm 的规则变更、静默不生效）。仅此路径置 true。
    pub must_restart: bool,
}

impl HotSwitchPlan {
    /// 构造 kind=None 的空规划（norm 前提失败/结构变更/Win guard 等正常退回路径）。
    fn none() -> Self {
        HotSwitchPlan {
            kind: HotSwitchKind::None,
            puts: Vec::new(),
            must_restart: false,
        }
    }

    /// 构造 kind=None + mustRestart=true 的强制重启规划（规则目标 dirty/不在 selector）。
    fn none_must_restart() -> Self {
        HotSwitchPlan {
            kind: HotSwitchKind::None,
            puts: Vec::new(),
            must_restart: true,
        }
    }
}

// ============================================================================
// planHotSwitch（Polaris L1923-2000）
// ============================================================================

/// 热切换规划：判 `new` 相对 `old` 能否走 clash_api 热切换，并产出需 PUT 的 selector 列表。
///
/// - norm(old) === norm(new) 是前提（targetServerId/selectedServerId 已移出 norm → 改这俩值不翻转 norm）。
/// - kind=Global：仅 selectedServerId 变 → PUT proxy-selector。
/// - kind=Rules：仅规则 targetServerId 变 → PUT 各 rule-sel-<id>。
/// - kind=Both：全局 + 规则同时变 → PUT proxy-selector + 各 rule-sel。
/// - kind=None：无热切换可行 → `must_restart` 区分「正常退回（false）」与「规则目标 dirty/不在 selector 须强制重启（true）」。
///
/// 上游 `ProxyManager.planHotSwitch`（this.currentConfig/this.currentIdToTagMap/
/// this.bootstrapFallbackEngaged 经 `deps` 注入；old 替代 this.currentConfig）。
pub fn plan_hot_switch(old: &UserConfig, new: &UserConfig, deps: &HotSwitchDeps) -> HotSwitchPlan {
    // norm 前提：结构等价（targetServerId/selectedServerId 均已移出 norm → 仅这俩值变不翻转 norm）。
    if config_generation_norm(old, None) != config_generation_norm(new, None) {
        return HotSwitchPlan::none();
    }
    // Windows TUN guard：非 system 栈（gvisor/mixed）保守退回重启（实测零环路仅 system）。
    if win_tun_blocks_hot_switch(new, &deps.platform) {
        return HotSwitchPlan::none();
    }
    // 进出阻断一律整核重启（2026-08-13）。
    //
    // 阻断改由**规则级** `action:"reject"` 表达（见 `builder::route` 末尾那段），不再是
    // 「selector 的 default 指向 block 出站」。热切换能表达的只有「PUT 一个 selector 的 default」，
    // 表达不了「整份 route 规则从全 reject 变回正常路由」⇒ 两个方向都必须重下发。
    //
    // 这是那次迁移**唯一**的行为代价，如实记在这里：切出阻断此前是热切（毫秒、不断流），
    // 现在会断掉阻断期间仍然活着的直连连接（LAN/NAS/本地 SSH）。换到的是「阻断态不再每拦一条
    // 连接就打一行 ERROR 把核日志历史挤掉」。
    if is_block_selection(old.selected_server_id.as_deref())
        != is_block_selection(new.selected_server_id.as_deref())
    {
        return HotSwitchPlan::none();
    }

    let mut puts: Vec<HotSwitchPut> = Vec::new();
    let mut global_changed = false;

    // 全局节点变化（含切到/切出 direct 哨兵：memberTag=direct，direct 恒是 proxy-selector 成员→可热切不重启）。
    // Polaris: old.selectedServerId !== newConfig.selectedServerId && newConfig.selectedServerId
    if old.selected_server_id != new.selected_server_id && new.selected_server_id.is_some() {
        let new_id = new.selected_server_id.as_deref().unwrap();
        let to_direct = is_direct_selection(Some(new_id));
        // 目标节点必须已存在于运行中的 selector（= 启动时 old.servers），否则 PUT 指向不存在的成员；
        // direct 豁免（恒为成员）。
        if !to_direct && !old.servers.iter().any(|s| s.id == new_id) {
            return HotSwitchPlan::none();
        }
        // §2 P2-B dirty 闸门：目标节点已编辑未生效（config 参数 ≠ 运行核快照）→ 热切到它会 PUT 到
        // 运行核里的旧参数成员、流量走旧参数且不自愈 → 退回重启。direct 哨兵无参数、不受影响。
        if !to_direct && is_server_dirty(new_id, new, deps) {
            return HotSwitchPlan::none();
        }
        // 选中节点 route 投影 guard（ICMP 已恒走 direct 静态、不再依赖选中节点，但其【其它】route 投影
        // 仍随之变，而 norm 已剔除 selectedServerId → 这些差异 PUT 不重生成，必须退回重启）：
        //   (1) 全隧道兜底：meshSelectedExitFallsBackToDirect 翻转 → final/smart-geo 的 userExitTag
        //       (proxy-selector↔direct) 翻转（补 ICMP 改静态后漏的 off-mesh↔普通代理 同款翻转）。
        //   (2) force-route engaged：alwaysRouteSubnets=false 的 endpoint 仅被选中时发其内网段；
        //       切到/离开它 → 段规则增删（保守：任一端是此类节点即重启）。
        let old_sel = old
            .servers
            .iter()
            .find(|s| Some(s.id.as_str()) == old.selected_server_id.as_deref());
        let new_sel = new
            .servers
            .iter()
            .find(|s| Some(s.id.as_str()) == new.selected_server_id.as_deref());
        let sel_only_forces_subnets = |s: Option<&ServerConfig>| -> bool {
            match s {
                Some(srv) => {
                    is_mesh_node(srv)
                        && !mesh_always_routes_subnets(srv)
                        && !endpoint_forced_route_cidrs(srv).is_empty()
                }
                None => false,
            }
        };
        if mesh_selected_exit_falls_back_to_direct(old)
            != mesh_selected_exit_falls_back_to_direct(new)
            || sel_only_forces_subnets(old_sel)
            || sel_only_forces_subnets(new_sel)
        {
            return HotSwitchPlan::none();
        }
        // 目标 tag：选中节点 id → tag（经 idToTagMap）。direct 哨兵 → 'direct'。解析不到 → 退回重启。
        let target_tag =
            match resolve_global_exit_tag(Some(new_id), deps.current_id_to_tag_map.as_ref()) {
                Some(t) => t,
                None => return HotSwitchPlan::none(),
            };
        // 旧全局出口 tag（供精准断连 pair）：登录期出口让位态下 proxy-selector 实际指 direct
        // （非 config 选中节点 tag），否则解析旧选中节点 tag。缺失（解析不到）→ 该 pair 断连跳过（宁可漏关不误杀）。
        let old_global_tag = if deps.bootstrap_fallback_engaged {
            Some(DIRECT_TAG.to_string())
        } else {
            resolve_global_exit_tag(
                old.selected_server_id.as_deref(),
                deps.current_id_to_tag_map.as_ref(),
            )
        };
        puts.push(HotSwitchPut {
            selector_tag: PROXY_SELECTOR_TAG.to_string(),
            member_tag: target_tag,
            old_member_tag: old_global_tag,
        });
        global_changed = true;
    }

    // 规则目标变化：diff customRules + appRules 的 targetServerId，对每条变化的规则从
    // currentRuleTargetMap 查 selectorTag、从 newConfig 的 idToTagMap（结构等价不变）解析新 memberTag。
    let rule_puts = match plan_rule_hot_switch(old, new, deps) {
        RuleHotSwitchResult::Puts(p) => p,
        // 任一规则目标节点不在 selector（新节点未入核）或 dirty（已编辑未生效）→ 无法热切 → 必须重启
        // （mustRestart 防被 no-op/canSkip 腿吞：targetServerId 出 norm，no-op 看不到规则目标变更）。
        RuleHotSwitchResult::CannotHotSwitch => return HotSwitchPlan::none_must_restart(),
    };
    puts.extend(rule_puts.iter().cloned());

    if puts.is_empty() {
        return HotSwitchPlan::none();
    }
    let kind = if global_changed && !rule_puts.is_empty() {
        HotSwitchKind::Both
    } else if global_changed {
        HotSwitchKind::Global
    } else {
        HotSwitchKind::Rules
    };
    HotSwitchPlan {
        kind,
        puts,
        must_restart: false,
    }
}

// ============================================================================
// planRuleHotSwitch（Polaris L2075-2132）
// ============================================================================

/// planRuleHotSwitch 的结果（Polaris 返回 `puts[] | null` 的三态）。
enum RuleHotSwitchResult {
    /// 可热切的 PUT 列表（含空 Vec = 无规则目标变化）。
    Puts(Vec<HotSwitchPut>),
    /// 任一目标节点不在运行 selector（新节点未入核）或 dirty（已编辑未生效）→ 无法热切。
    /// 上游 `return null`。
    CannotHotSwitch,
}

/// 规则 targetServerId diff → rule-sel PUT 列表。
///
/// 返回 [`RuleHotSwitchResult::CannotHotSwitch`] 表示任一目标节点不在运行中 selector（应整体退回重启）。
/// currentRuleTargetMap 的 memberTag 是【生成时】的 default（启动时 targetServerId 解析）；此处按 newConfig
/// 重解析新 targetServerId → 新 memberTag（currentIdToTagMap 结构等价不变，可复用）。
///
/// 上游 `ProxyManager.planRuleHotSwitch`（this.currentRuleTargetMap/this.currentIdToTagMap 经 deps 注入）。
fn plan_rule_hot_switch(
    old: &UserConfig,
    new: &UserConfig,
    deps: &HotSwitchDeps,
) -> RuleHotSwitchResult {
    let map = match &deps.current_rule_target_map {
        None => return RuleHotSwitchResult::Puts(Vec::new()), // 启动时无 rule-sel → 无规则热切换
        Some(m) => m,
    };
    let id_to_tag = match &deps.current_id_to_tag_map {
        None => return RuleHotSwitchResult::CannotHotSwitch, // 无法解析节点 tag → 退回重启
        Some(m) => m,
    };

    let mut puts: Vec<HotSwitchPut> = Vec::new();

    // visit 单条规则的 target 变化。返回 true=可继续，false=该规则目标无法热切（调用方返 CannotHotSwitch）。
    // Polaris 内层 `visit(ruleKey, oldTarget, newTarget): boolean`。
    let visit = |rule_key: &str,
                 old_target: Option<&str>,
                 new_target: Option<&str>,
                 puts: &mut Vec<HotSwitchPut>|
     -> bool {
        if old_target == new_target.map(|s| s.to_string()).as_deref() {
            return true; // 未变
        }
        let entry = match map.get(rule_key) {
            None => return true, // currentRuleTargetMap 无此条（启动时该规则未生成 rule-sel，如被 gate 剔除）→ 跳过
            Some(e) => e,
        };
        // 新目标有→解析节点 tag；无（节点切回默认/跟全局）→ proxy-selector（rule-sel 嵌套 default）。
        let member_tag: Option<String> = match new_target {
            Some(t) => id_to_tag.get(t).cloned(),
            None => Some(PROXY_SELECTOR_TAG.to_string()),
        };
        let member_tag = match member_tag {
            Some(t) => t,
            None => return false, // 新目标节点不在 selector → 退回重启
        };
        // §2 P2-B dirty 闸门：规则新目标节点已编辑未生效（config 参数 ≠ 运行核快照）→ 退回重启
        // （防规则热切到旧参数成员）。上游 `newTarget && this.isServerDirty(newTarget, newConfig)`。
        if let Some(t) = new_target {
            if is_server_dirty(t, new, deps) {
                return false;
            }
        }
        // 旧成员 tag（供精准断连 pair）：旧目标节点 tag，无旧目标（跟全局）→ proxy-selector。
        // 必须从 oldTarget 现算，禁用 currentRuleTargetMap.memberTag（那是生成时 default，首次规则热切后即陈旧）。
        let old_member_tag: String = match old_target {
            Some(t) => id_to_tag
                .get(t)
                .cloned()
                .unwrap_or_else(|| PROXY_SELECTOR_TAG.to_string()),
            None => PROXY_SELECTOR_TAG.to_string(),
        };
        puts.push(HotSwitchPut {
            selector_tag: entry.selector_tag.clone(),
            member_tag, // 上游 `memberTag || 'proxy-selector'`（此处已非空，等价）
            old_member_tag: Some(old_member_tag),
        });
        true
    };

    // customRules：按 id 配对（结构等价 ⇒ 顺序与 id 集合一致）。仅 enabled。
    let old_rules_by_id: BTreeMap<&str, &_> = old
        .custom_rules
        .iter()
        .filter(|r| r.enabled)
        .map(|r| (r.id.as_str(), r))
        .collect();
    for r in &new.custom_rules {
        if !r.enabled {
            continue;
        }
        let old_r = match old_rules_by_id.get(r.id.as_str()) {
            None => continue,
            Some(o) => *o,
        };
        let rule_key = format!("custom:{}", r.id);
        if !visit(
            &rule_key,
            old_r.target_server_id.as_deref(),
            r.target_server_id.as_deref(),
            &mut puts,
        ) {
            return RuleHotSwitchResult::CannotHotSwitch;
        }
    }

    // appRules：按 appId 配对（无 enabled 过滤——Polaris appRules 全量投影，但 visit 仍按 appId 配对）。
    // 注：Polaris planRuleHotSwitch 的 appRules 分支 `filter((a) => a.enabled)`，与 customRules 一致。
    let old_apps_by_app_id: BTreeMap<&str, &_> = old
        .app_rules
        .iter()
        .filter(|a| a.enabled)
        .map(|a| (a.app_id.as_str(), a))
        .collect();
    for a in &new.app_rules {
        if !a.enabled {
            continue;
        }
        let old_a = match old_apps_by_app_id.get(a.app_id.as_str()) {
            None => continue,
            Some(o) => *o,
        };
        let rule_key = format!("app:{}", a.app_id);
        if !visit(
            &rule_key,
            old_a.target_server_id.as_deref(),
            a.target_server_id.as_deref(),
            &mut puts,
        ) {
            return RuleHotSwitchResult::CannotHotSwitch;
        }
    }

    RuleHotSwitchResult::Puts(puts)
}

// ============================================================================
// canSkipRestartForAddedUnreferenced（Polaris L2327-2350）
// ============================================================================

/// 判 `next` 相对 `old` 是否「仅变更了不影响活流量的节点」——是则免整核重启（defer）。
/// 覆盖：新增未引用节点（P2-A，订阅刷新）+ 编辑/删除未引用节点（P2-B）。
///
/// 非对称安全模型：未引用节点（非选中/非规则目标/非 endpoint/不在 detour 链）仅作 selector 惰性成员、
/// 不承载流量，增/改/删对运行核行为无影响 → 可 defer；被引用节点改/删 → 运行核残留陈旧条目或用旧参数承载流量 → 必须重启。
///
/// 全部满足才放行（缺一即重启）：
/// ① selectedServerId 未变；② 非 servers 生成字段全等（混入 DNS/mode 变更即重启=正交守卫）；
/// ③ 所有被引用（refOld∪refNext）旧节点原样保留（无删除、无改动）；④ 新增节点全部未被引用。
///
/// 上游 `ProxyManager.canSkipRestartForAddedUnreferenced`。
pub fn can_skip_restart_for_added_unreferenced(
    old: &UserConfig,
    next: &UserConfig,
    running_servers_fingerprint: &BTreeMap<String, String>,
) -> bool {
    can_skip_restart_for_added_unreferenced_impl(old, next, Some(running_servers_fingerprint))
}

/// canSkipRestart 的内部实现：running_servers_fingerprint 参数化以便单测注入 None（核未起）。
/// 公开签名恒传 Some（调用方保证核起）；Polaris 走 this.runningServersFingerprint（私有同源）。
fn can_skip_restart_for_added_unreferenced_impl(
    old: &UserConfig,
    next: &UserConfig,
    _running_servers_fingerprint: Option<&BTreeMap<String, String>>,
) -> bool {
    // ① selectedServerId 未变。
    if old.selected_server_id != next.selected_server_id {
        return false;
    }
    // ② 非 servers 字段逐字节一致（servers 投影到空 → 仅比对路由/规则/DNS/端口/模式 等非节点字段；
    //   混入 DNS/mode 变更即重启=正交守卫，节点 defer 绝不吞其它字段的重启）。
    let empty: BTreeSet<String> = BTreeSet::new();
    if config_generation_norm(old, Some(&empty)) != config_generation_norm(next, Some(&empty)) {
        return false;
    }
    let old_by_id: BTreeMap<&str, &ServerConfig> =
        old.servers.iter().map(|s| (s.id.as_str(), s)).collect();
    let new_by_id: BTreeMap<&str, &ServerConfig> =
        next.servers.iter().map(|s| (s.id.as_str(), s)).collect();
    let ref_old = referenced_server_ids(old);
    let ref_next = referenced_server_ids(next);
    // ③ P2-B：被引用（旧或新）的旧节点必须原样保留——删/改被引用节点影响活流量→重启；
    //   未引用旧节点的删/改放行（defer）。
    for s in &old.servers {
        if !ref_old.contains(&s.id) && !ref_next.contains(&s.id) {
            continue; // 未引用 → 删/改放行
        }
        let n = match new_by_id.get(s.id.as_str()) {
            None => return false, // 被引用节点被删 → 重启
            Some(n) => *n,
        };
        if server_fingerprint(n) != server_fingerprint(s) {
            return false; // 被引用节点参数变 → 重启
        }
    }
    // ④ 新增节点全部未被引用。
    for s in &next.servers {
        if !old_by_id.contains_key(s.id.as_str()) && ref_next.contains(&s.id) {
            return false; // 新增且被引用 → 重启
        }
    }
    true
}

// ============================================================================
// isServerDirty（Polaris L2308-2315）
// ============================================================================

/// 节点是否 dirty：config 里的参数 ≠ 运行核快照（= 已编辑未生效）。
///
/// planHotSwitch 禁热切到 dirty 节点——否则 PUT 到运行核里的旧参数成员、流量走旧参数且不自愈
/// （§2 P2-B 唯一新增风险的堵法）。不在快照（新增未入核）返 false——那由 planHotSwitch
/// 「目标不在运行 selector」既有判据挡（退回重启）。
///
/// 上游 `ProxyManager.isServerDirty`（this.runningServersFingerprint 经 deps 注入）。
pub fn is_server_dirty(id: &str, config: &UserConfig, deps: &HotSwitchDeps) -> bool {
    let snap = match &deps.running_servers_fingerprint {
        None => return false, // 核未起 → 非 dirty
        Some(s) => s,
    };
    let fp = match snap.get(id) {
        None => return false, // 不在快照（新增未入核）→ false（由「目标不在 selector」判据挡）
        Some(f) => f,
    };
    let s = match config.servers.iter().find(|x| x.id == id) {
        Some(s) => s,
        None => return false,
    };
    fp != &server_fingerprint(s)
}

// ============================================================================
// winTunBlocksHotSwitch（Polaris L1909-1913）
// ============================================================================

/// Windows TUN 热切换 guard：system 栈放行（实测零环路），非 system（gvisor/mixed）未实测保守退回重启。
///
/// 全局节点热切换与规则目标热切换共用（rule-sel selector 切换同理可能触发 Wintun 回捕）。
/// 必须用 resolve_tun_stack 把 'auto' 解析成具体栈再判，不能按裸 'auto'!=='system' 字面比较。
///
/// # ⚠ 与 Windows 新默认栈的耦合（2026-08-05，未决）
///
/// Windows 的 auto 已解析为 **gvisor**（`tun_stack::platform_default_stack`），故本 guard 现在
/// **默认拦住所有 Windows TUN 用户** ⇒ 每次切节点退回重启核。guard 的原始依据（「非 system 未实测」）
/// 已被 vault 的 win-tun MTU 基准记录 §0.7 的三栈 21/21 实测推翻，但同文档钉了翻转前置门：
/// 需用**本体**（含 FakeIP / DNS 劫持 / 规则集 / helper 路径）在 Windows 上跑一遍 gvisor 换节点回归。
/// 门未过，故此处保持原样，由 `win_tun_win32_auto_now_blocks_pending_gvisor_regression` 锚住。
///
/// 上游 `ProxyManager.winTunBlocksHotSwitch`（process.platform 经 deps.platform 注入）。
pub fn win_tun_blocks_hot_switch(config: &UserConfig, platform: &str) -> bool {
    // 非 win32 或 非 tun 模式 → false（放行）。
    if !platform.eq_ignore_ascii_case("win32") || !config.proxy_mode_type.is_tun() {
        return false;
    }
    // win32 + tun：resolveTunStack('auto'→system(Win))，非 system 一律拦。
    let user_stack = config.tun_config.as_ref().map(|c| c.stack);
    resolve_tun_stack(user_stack, platform) != ConcreteTunStack::System
}

// ============================================================================
// resolveGlobalExitTag（Polaris shared/direct-selection.ts L22-28）
// ============================================================================

/// 全局出口的 proxy-selector 成员 tag（单一真值，收口「selectedServerId → memberTag」）：
/// 直连哨兵 → 'direct'，否则查 idToTagMap 得节点 tag。未知节点返回 None，由调用方按场景兜底
/// （planHotSwitch → 退回重启）。
///
/// 上游 `resolveGlobalExitTag`（shared/direct-selection.ts）。
pub fn resolve_global_exit_tag(
    selected_server_id: Option<&str>,
    id_to_tag_map: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    if is_direct_selection(selected_server_id) {
        return Some(DIRECT_TAG.to_string());
    }
    // 阻断**没有对应的成员 tag**（2026-08-13 起它由规则级 reject 表达，不再是一个出站）⇒ 返回 None。
    // 调用方两处的退化方向都正确：目标侧 None ⇒ 整核重启；旧出口侧 None ⇒ 跳过那一对精准断连
    // （宁可漏关不误杀），而进出阻断本来就走重启，不依赖精准断连。
    if is_block_selection(selected_server_id) {
        return None;
    }
    let id = selected_server_id?;
    let map = id_to_tag_map?;
    map.get(id).cloned()
}

// ============================================================================
// 测试（移植 Polaris proxy-manager-{platform,norm,p2a}-hotswitch.test.ts 的纯逻辑子集）
// ============================================================================

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::user_config::app_config::UserConfig;
    use crate::user_config::proxy_mode::{ProxyMode, ProxyModeType};
    use crate::user_config::rule::{Rule, RuleAction, RuleType};
    use crate::user_config::server_config::{Protocol, ServerConfig, WireGuardSettings};
    use crate::user_config::tun_config::TunModeConfig;
    use crate::user_config::tun_stack::TunStack;

    const NODE_A: &str = "node-a";
    const NODE_B: &str = "node-b";

    fn ss(id: &str, addr: &str) -> ServerConfig {
        ServerConfig {
            id: id.into(),
            name: id.into(),
            protocol: Protocol::Shadowsocks,
            address: addr.into(),
            port: 8388,
            ..Default::default()
        }
    }

    fn ss_extra(id: &str, addr: &str, port: u16) -> ServerConfig {
        ServerConfig {
            port,
            ..ss(id, addr)
        }
    }

    fn wg(id: &str, allow_internet: Option<bool>, always_route: Option<bool>) -> ServerConfig {
        ServerConfig {
            id: id.into(),
            name: id.into(),
            protocol: Protocol::Wireguard,
            wireguard_settings: Some(Box::new(WireGuardSettings {
                allow_internet,
                always_route_subnets: always_route,
                allowed_ips: vec!["10.9.0.0/24".into()],
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn ext_rule(id: &str, target: Option<&str>) -> Rule {
        Rule {
            id: id.into(),
            type_field: RuleType::DomainSuffix,
            values: vec!["example.com".into()],
            conditions: None,
            combine_mode: None,
            action: RuleAction::Proxy,
            enabled: true,
            bypass_fakeip: None,
            target_server_id: target.map(String::from),
            remarks: None,
            tls_spoof: None,
            tls_spoof_method: None,
        }
    }

    /// 基础 smart config：两节点(A/B) + selectedServerId=A + systemProxy（非 tun，winTun guard 不拦）。
    fn base_config() -> UserConfig {
        UserConfig {
            servers: vec![ss(NODE_A, "1.1.1.1"), ss(NODE_B, "2.2.2.2")],
            selected_server_id: Some(NODE_A.into()),
            proxy_mode: ProxyMode::Smart,
            proxy_mode_type: ProxyModeType::SystemProxy,
            custom_rules: vec![],
            app_rules: vec![],
            ..Default::default()
        }
    }

    fn deps_with_tags() -> HotSwitchDeps {
        let mut map = BTreeMap::new();
        map.insert(NODE_A.into(), "tagA".into());
        map.insert(NODE_B.into(), "tagB".into());
        HotSwitchDeps {
            current_id_to_tag_map: Some(map),
            platform: "linux".into(),
            ..Default::default()
        }
    }

    // === resolveGlobalExitTag ===

    #[test]
    fn resolve_global_exit_tag_direct_sentinel() {
        assert_eq!(
            resolve_global_exit_tag(Some("__direct__"), None),
            Some("direct".into())
        );
    }

    /// 阻断哨兵 → block tag（不依赖 idToTagMap，同 direct）。
    ///
    /// 变异锁：删掉 `is_block_selection` 那条早返回 → 落到 map 查询 → None → 转红。
    #[test]
    fn resolve_global_exit_tag_block_sentinel() {
        // 阻断不再是一个出站 ⇒ 没有成员 tag 可解析。返回 None 是**正确的退化**：
        // 目标侧 None ⇒ 整核重启；旧出口侧 None ⇒ 跳过精准断连（而进出阻断本就走重启）。
        assert_eq!(
            resolve_global_exit_tag(Some("__block__"), None),
            None,
            "阻断已改由规则级 reject 表达；若这里又能解析出 tag，说明 block 出站被复活了"
        );
    }

    /// 【切入阻断退回重启】block 尚未进运行核的 selector ⇒ planHotSwitch 必须给空计划（= 整核重启），
    /// 绝不能 PUT 到一个不存在的成员（核返 NotFound → executor 判 Failed → 静默退回重启，
    /// 用户看到「切换成功」而热切永久失效）。
    ///
    /// 变异锁：给 hotswitch 的成员校验加一条 block 豁免（仿 `to_direct`）→ 本用例转红。
    #[test]
    fn switch_into_block_falls_back_to_restart() {
        let old = base_config();
        let mut new = old.clone();
        new.selected_server_id = Some("__block__".into());
        let plan = plan_hot_switch(&old, &new, &deps_with_tags());
        assert!(
            plan.puts.is_empty(),
            "切入阻断必须退回重启，不得 PUT 到非成员：{:?}",
            plan.puts
        );
    }

    /// 【切出阻断可热切】运行核的 selector 是带 block 成员生成的、目标节点也在其中 ⇒ 可热切，
    /// 且 old_member_tag 须解析成 block（供精准断连那一对）。
    #[test]
    fn switch_out_of_block_falls_back_to_restart() {
        // 【行为变更 2026-08-13，如实钉住】此前切出阻断是**热切**（block 是 selector 成员）。
        // 阻断改由规则级 reject 表达之后，进出阻断都动 route 规则集，而热切换只能 PUT 一个
        // selector 的 default ⇒ 表达不了 ⇒ 两个方向都必须整核重启。
        //
        // 这是那次迁移唯一的行为代价：会断掉阻断期间仍活着的直连连接。换到的是「阻断态不再
        // 每拦一条连接打一行 ERROR 把核日志历史挤掉」。
        let mut old = base_config();
        old.selected_server_id = Some("__block__".into());
        let mut new = old.clone();
        new.selected_server_id = Some(NODE_A.into());
        let plan = plan_hot_switch(&old, &new, &deps_with_tags());
        assert!(
            plan.puts.is_empty(),
            "切出阻断必须退回整核重启（规则集变了，PUT selector default 表达不了）：{:?}",
            plan.puts
        );
    }

    #[test]
    fn resolve_global_exit_tag_node_via_map() {
        let mut map = BTreeMap::new();
        map.insert("n1".into(), "tagN1".into());
        assert_eq!(
            resolve_global_exit_tag(Some("n1"), Some(&map)),
            Some("tagN1".into())
        );
    }

    #[test]
    fn resolve_global_exit_tag_unknown_node_none() {
        let map = BTreeMap::new();
        assert_eq!(resolve_global_exit_tag(Some("ghost"), Some(&map)), None);
    }

    #[test]
    fn resolve_global_exit_tag_none_when_no_id() {
        assert_eq!(resolve_global_exit_tag(None, None), None);
    }

    // === winTunBlocksHotSwitch（平台 × 模式 × stack 矩阵）===

    #[test]
    fn win_tun_win32_gvisor_blocks() {
        let mut cfg = base_config();
        cfg.proxy_mode_type = ProxyModeType::Tun;
        cfg.tun_config = Some(TunModeConfig {
            stack: TunStack::Gvisor,
            ..Default::default()
        });
        assert!(win_tun_blocks_hot_switch(&cfg, "win32"));
    }

    #[test]
    fn win_tun_win32_system_passes() {
        let mut cfg = base_config();
        cfg.proxy_mode_type = ProxyModeType::Tun;
        cfg.tun_config = Some(TunModeConfig {
            stack: TunStack::System,
            ..Default::default()
        });
        assert!(!win_tun_blocks_hot_switch(&cfg, "win32"));
    }

    /// 🔴 **本条记录一个已知的功能耦合，不是在为它背书**（2026-08-05）。
    ///
    /// Windows 的 auto 默认栈已从 system 改为 gvisor（实测依据见 `tun_stack::platform_default_stack`），
    /// 而本 guard 拦一切非 system 栈 ⇒ **Windows TUN 用户默认落进「禁热切换、每次切节点重启核」**。
    ///
    /// guard 的原始依据是「非 system **未实测**」（不是实测有环）。该依据现已被
    /// vault `design/networking/` 下的 win-tun MTU 基准记录 §0.7 推翻：三栈各 21/21 成功、
    /// 3 次切换、切后 0 失败、selector 终态正确。**但那轮是裸 sing-box 最小配置**，缺 FakeIP / DNS 劫持 /
    /// route 规则集 / auto_detect_interface 交互 / helper 提权路径，故同文档写死了翻转前置条件：
    /// 「必须用本体在 207 上跑一遍 gvisor 下的换节点回归」。那道门尚未过，故此处**不擅自翻转**。
    ///
    /// 本用例因此断言的是当前真实行为（拦），并把它标成待决口——回归跑完放开 guard 时，
    /// 这条会转红，那正是提醒同步改它的锚点。
    #[test]
    fn win_tun_win32_auto_now_blocks_pending_gvisor_regression() {
        // guard 仍必须 resolve_tun_stack 后再判（不能按裸 'auto' 字面比较）——这一半没变。
        let mut cfg = base_config();
        cfg.proxy_mode_type = ProxyModeType::Tun;
        cfg.tun_config = Some(TunModeConfig {
            stack: TunStack::Auto,
            ..Default::default()
        });
        assert!(win_tun_blocks_hot_switch(&cfg, "win32"));
    }

    #[test]
    fn win_tun_win32_mixed_blocks() {
        let mut cfg = base_config();
        cfg.proxy_mode_type = ProxyModeType::Tun;
        cfg.tun_config = Some(TunModeConfig {
            stack: TunStack::Mixed,
            ..Default::default()
        });
        assert!(win_tun_blocks_hot_switch(&cfg, "win32"));
    }

    #[test]
    fn win_tun_non_tun_mode_passes() {
        let cfg = base_config(); // systemProxy
        assert!(!win_tun_blocks_hot_switch(&cfg, "win32"));
    }

    #[test]
    fn win_tun_darwin_never_blocks() {
        let mut cfg = base_config();
        cfg.proxy_mode_type = ProxyModeType::Tun;
        cfg.tun_config = Some(TunModeConfig {
            stack: TunStack::Gvisor,
            ..Default::default()
        });
        assert!(!win_tun_blocks_hot_switch(&cfg, "darwin"));
    }

    #[test]
    fn win_tun_linux_never_blocks() {
        let mut cfg = base_config();
        cfg.proxy_mode_type = ProxyModeType::Tun;
        cfg.tun_config = Some(TunModeConfig {
            stack: TunStack::Gvisor,
            ..Default::default()
        });
        assert!(!win_tun_blocks_hot_switch(&cfg, "linux"));
    }

    // === planHotSwitch 全局节点切换 ===

    #[test]
    fn plan_global_switch_a_to_b() {
        let old = base_config();
        let mut new_cfg = base_config();
        new_cfg.selected_server_id = Some(NODE_B.into());
        let plan = plan_hot_switch(&old, &new_cfg, &deps_with_tags());
        assert_eq!(plan.kind, HotSwitchKind::Global);
        assert_eq!(
            plan.puts,
            vec![HotSwitchPut {
                selector_tag: "proxy-selector".into(),
                member_tag: "tagB".into(),
                old_member_tag: Some("tagA".into()),
            }]
        );
        assert!(!plan.must_restart);
    }

    #[test]
    fn plan_global_switch_to_direct() {
        let old = base_config();
        let mut new_cfg = base_config();
        new_cfg.selected_server_id = Some("__direct__".into());
        let plan = plan_hot_switch(&old, &new_cfg, &deps_with_tags());
        assert_eq!(plan.kind, HotSwitchKind::Global);
        assert_eq!(plan.puts[0].member_tag, "direct");
        assert!(!plan.must_restart);
    }

    #[test]
    fn plan_global_switch_no_norm_change_is_none() {
        // norm 不等（结构变）→ none。
        let old = base_config();
        let mut new_cfg = base_config();
        new_cfg.proxy_mode = ProxyMode::Global; // norm 翻转
        new_cfg.selected_server_id = Some(NODE_B.into());
        let plan = plan_hot_switch(&old, &new_cfg, &deps_with_tags());
        assert_eq!(plan.kind, HotSwitchKind::None);
        assert!(plan.puts.is_empty());
        assert!(!plan.must_restart);
    }

    #[test]
    fn plan_global_switch_target_added_node_flips_norm_none() {
        // 目标节点不在 old.servers（新增未入核）→ 但加节点本身翻转 norm（servers 集合变）→
        //   norm 前提失败先于「目标不在 selector」闸门 → none（非 mustRestart）。
        // 注：纯 config 视角下「目标不在运行 selector」无法与「norm 翻转」分离——新增节点必改 servers 集合。
        //   该闸门在 Polaris 运行态才有独立意义（currentIdToTagMap 与 servers 集合解耦）。unknown_tag 用例覆盖 idToTagMap 缺失路径。
        let old = base_config();
        let mut new_cfg = base_config();
        new_cfg.servers.push(ss("ghost", "9.9.9.9"));
        new_cfg.selected_server_id = Some("ghost".into());
        let mut deps = deps_with_tags();
        deps.current_id_to_tag_map
            .as_mut()
            .unwrap()
            .insert("ghost".into(), "tagGhost".into());
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::None);
        assert!(plan.puts.is_empty());
        assert!(!plan.must_restart);
    }

    #[test]
    fn plan_global_switch_unknown_target_tag_none() {
        // 目标 tag 解析不到（idToTagMap 无此 id）→ none。
        let old = base_config();
        let mut new_cfg = base_config();
        new_cfg.selected_server_id = Some(NODE_B.into());
        // idToTagMap 无 B → resolve 返 None → none。
        let mut map = BTreeMap::new();
        map.insert(NODE_A.into(), "tagA".into());
        let deps = HotSwitchDeps {
            current_id_to_tag_map: Some(map),
            platform: "linux".into(),
            ..Default::default()
        };
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::None);
        assert!(plan.puts.is_empty());
    }

    #[test]
    fn plan_global_switch_bootstrap_fallback_old_tag_is_direct() {
        let old = base_config();
        let mut new_cfg = base_config();
        new_cfg.selected_server_id = Some(NODE_B.into());
        let mut deps = deps_with_tags();
        deps.bootstrap_fallback_engaged = true; // 旧全局 tag = direct
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::Global);
        assert_eq!(plan.puts[0].old_member_tag.as_deref(), Some("direct"));
    }

    #[test]
    fn plan_no_change_is_none() {
        // old===new（selectedServerId 同）+ 无规则变化 → none。
        let old = base_config();
        let new_cfg = base_config();
        let plan = plan_hot_switch(&old, &new_cfg, &deps_with_tags());
        assert_eq!(plan.kind, HotSwitchKind::None);
        assert!(plan.puts.is_empty());
        assert!(!plan.must_restart);
    }

    // === planHotSwitch Win TUN 端到端 ===

    #[test]
    fn plan_win_tun_gvisor_blocks_global_switch() {
        let mut old = base_config();
        old.proxy_mode_type = ProxyModeType::Tun;
        old.tun_config = Some(TunModeConfig {
            stack: TunStack::Gvisor,
            ..Default::default()
        });
        let mut new_cfg = old.clone();
        new_cfg.selected_server_id = Some(NODE_B.into());
        let mut deps = deps_with_tags();
        deps.platform = "win32".into();
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::None);
        assert!(plan.puts.is_empty());
    }

    #[test]
    fn plan_win_tun_system_allows_global_switch() {
        let mut old = base_config();
        old.proxy_mode_type = ProxyModeType::Tun;
        old.tun_config = Some(TunModeConfig {
            stack: TunStack::System,
            ..Default::default()
        });
        let mut new_cfg = old.clone();
        new_cfg.selected_server_id = Some(NODE_B.into());
        let mut deps = deps_with_tags();
        deps.platform = "win32".into();
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::Global);
    }

    // === planHotSwitch route 投影 guard（mesh 退回 direct 翻转 / force-route engaged）===

    #[test]
    fn plan_full_tunnel_to_off_mesh_endpoint_none() {
        // 全隧道 endpoint → off-mesh endpoint：fallsBackToDirect 翻转 → none。
        let list = vec![
            wg("wg-full", Some(true), None),
            wg("wg-offmesh", Some(false), None),
        ];
        let mut old = base_config();
        old.servers = list.clone();
        old.selected_server_id = Some("wg-full".into());
        let mut new_cfg = old.clone();
        new_cfg.selected_server_id = Some("wg-offmesh".into());
        let mut deps = HotSwitchDeps {
            platform: "linux".into(),
            ..Default::default()
        };
        let mut map = BTreeMap::new();
        map.insert("wg-full".into(), "tag-wg-full".into());
        map.insert("wg-offmesh".into(), "tag-wg-offmesh".into());
        deps.current_id_to_tag_map = Some(map);
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::None);
    }

    #[test]
    fn plan_full_tunnel_to_another_full_tunnel_global() {
        let list = vec![
            wg("wg-full", Some(true), None),
            wg("wg-full-2", Some(true), None),
        ];
        let mut old = base_config();
        old.servers = list.clone();
        old.selected_server_id = Some("wg-full".into());
        let mut new_cfg = old.clone();
        new_cfg.selected_server_id = Some("wg-full-2".into());
        let mut deps = HotSwitchDeps {
            platform: "linux".into(),
            ..Default::default()
        };
        let mut map = BTreeMap::new();
        map.insert("wg-full".into(), "tag-wg-full".into());
        map.insert("wg-full-2".into(), "tag-wg-full-2".into());
        deps.current_id_to_tag_map = Some(map);
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::Global);
        assert!(plan.puts.iter().any(|p| p.member_tag == "tag-wg-full-2"));
    }

    #[test]
    fn plan_switch_to_force_route_only_endpoint_none() {
        // 切到 alwaysRouteSubnets=false 的 endpoint（force-route 段随选中翻转）→ none。
        let list = vec![
            wg("wg-full", Some(true), None),
            wg("wg-onlysub", Some(true), Some(false)),
        ];
        let mut old = base_config();
        old.servers = list.clone();
        old.selected_server_id = Some("wg-full".into());
        let mut new_cfg = old.clone();
        new_cfg.selected_server_id = Some("wg-onlysub".into());
        let mut deps = HotSwitchDeps {
            platform: "linux".into(),
            ..Default::default()
        };
        let mut map = BTreeMap::new();
        map.insert("wg-full".into(), "t1".into());
        map.insert("wg-onlysub".into(), "t2".into());
        deps.current_id_to_tag_map = Some(map);
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::None);
    }

    // === planHotSwitch dirty 闸门 ===

    #[test]
    fn plan_switch_to_dirty_node_none() {
        // 场景（对齐 Polaris p2a "§2 dirty 闸门"）：编辑步骤已提交 → currentConfig(old) 的 Z 已是 5.5.5.5；
        //   运行核快照(snap)仍是 9.9.9.9（编辑未生效）→ Z dirty。本步把选中 A→Z。
        // old/new servers 同（均 Z=5.5.5.5）→ norm 等价（selectedServerId 已出 norm）→ 进全局切换分支 →
        //   dirty 闸门：目标 Z dirty → none（退回重启，防热切到运行核旧参数成员）。
        let mut old = base_config();
        old.servers = vec![ss("A", "1.1.1.1"), ss("Z", "5.5.5.5")];
        old.selected_server_id = Some("A".into());
        let mut new_cfg = old.clone();
        new_cfg.selected_server_id = Some("Z".into()); // 仅切选中，servers 不变
        let mut deps = HotSwitchDeps {
            platform: "linux".into(),
            ..Default::default()
        };
        let mut map = BTreeMap::new();
        map.insert("A".into(), "tagA".into());
        map.insert("Z".into(), "tagZ".into());
        deps.current_id_to_tag_map = Some(map);
        // 快照起于旧参数 Z(9.9.9.9) → config Z(5.5.5.5) dirty。
        let mut snap = BTreeMap::new();
        snap.insert("A".into(), server_fingerprint(&ss("A", "1.1.1.1")));
        snap.insert("Z".into(), server_fingerprint(&ss("Z", "9.9.9.9")));
        deps.running_servers_fingerprint = Some(snap);
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::None);
        assert!(!plan.must_restart); // 全局 dirty 闸门走正常 none（非 mustRestart）
    }

    #[test]
    fn plan_rule_target_to_dirty_node_must_restart() {
        // §2 F1：规则目标改到 dirty 节点 → mustRestart（防被 no-op/canSkip 吞）。
        // 场景（对齐 Polaris p2a F1）：编辑步骤已提交 → currentConfig(old) 的 Z 已是 5.5.5.5；
        //   运行核快照(snap)仍是 9.9.9.9（编辑未生效）→ Z dirty。本步把 r1 目标 A→Z。
        // old 与 new 的 servers 同（均 Z=5.5.5.5），仅 customRules.targetServerId 变（出 norm）→ norm 等价 →
        //   进 planRuleHotSwitch → 新目标 Z dirty → null → mustRestart（防 no-op/canSkip 腿吞静默不生效）。
        let r1_a = ext_rule("r1", Some("A"));
        let mut old = base_config();
        old.servers = vec![ss("A", "1.1.1.1"), ss("Z", "5.5.5.5")];
        old.selected_server_id = Some("A".into());
        old.custom_rules = vec![r1_a];
        let r1_z = ext_rule("r1", Some("Z"));
        let mut new_cfg = old.clone();
        new_cfg.custom_rules = vec![r1_z]; // 仅规则目标 A→Z，servers 不变
        let mut deps = HotSwitchDeps {
            platform: "linux".into(),
            ..Default::default()
        };
        let mut map = BTreeMap::new();
        map.insert("A".into(), "tagA".into());
        map.insert("Z".into(), "tagZ".into());
        deps.current_id_to_tag_map = Some(map);
        // 快照仍是旧参数 Z(9.9.9.9) → config Z(5.5.5.5) dirty。
        let mut snap = BTreeMap::new();
        snap.insert("A".into(), server_fingerprint(&ss("A", "1.1.1.1")));
        snap.insert("Z".into(), server_fingerprint(&ss("Z", "9.9.9.9")));
        deps.running_servers_fingerprint = Some(snap);
        let mut rtm = BTreeMap::new();
        rtm.insert(
            "custom:r1".into(),
            RuleTargetEntry {
                selector_tag: "rule-sel-r1".into(),
                member_tag: "tagA".into(),
            },
        );
        deps.current_rule_target_map = Some(rtm);
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::None);
        assert!(plan.must_restart);
    }

    // === planRuleHotSwitch（经 plan_hot_switch 端到端 + 直接断言行为）===

    fn setup_rule_deps() -> (HotSwitchDeps, ()) {
        let mut map = BTreeMap::new();
        map.insert(NODE_A.into(), "tagA".into());
        map.insert(NODE_B.into(), "tagB".into());
        let mut rtm = BTreeMap::new();
        rtm.insert(
            "custom:r1".into(),
            RuleTargetEntry {
                selector_tag: "rule-sel-r1".into(),
                member_tag: "stub".into(),
            },
        );
        (
            HotSwitchDeps {
                current_id_to_tag_map: Some(map),
                current_rule_target_map: Some(rtm),
                platform: "linux".into(),
                ..Default::default()
            },
            (),
        )
    }

    #[test]
    fn plan_rule_switch_a_to_b() {
        let (deps, _) = setup_rule_deps();
        let mut old = base_config();
        old.custom_rules = vec![ext_rule("r1", Some(NODE_A))];
        let mut new_cfg = old.clone();
        new_cfg.custom_rules = vec![ext_rule("r1", Some(NODE_B))];
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::Rules);
        assert_eq!(
            plan.puts,
            vec![HotSwitchPut {
                selector_tag: "rule-sel-r1".into(),
                member_tag: "tagB".into(),
                old_member_tag: Some("tagA".into()),
            }]
        );
    }

    #[test]
    fn plan_rule_switch_node_to_default() {
        let (deps, _) = setup_rule_deps();
        let mut old = base_config();
        old.custom_rules = vec![ext_rule("r1", Some(NODE_A))];
        let mut new_cfg = old.clone();
        new_cfg.custom_rules = vec![ext_rule("r1", None)];
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::Rules);
        assert_eq!(plan.puts[0].member_tag, "proxy-selector");
        assert_eq!(plan.puts[0].old_member_tag.as_deref(), Some("tagA"));
    }

    #[test]
    fn plan_rule_switch_default_to_node() {
        let (deps, _) = setup_rule_deps();
        let mut old = base_config();
        old.custom_rules = vec![ext_rule("r1", None)];
        let mut new_cfg = old.clone();
        new_cfg.custom_rules = vec![ext_rule("r1", Some(NODE_B))];
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::Rules);
        assert_eq!(plan.puts[0].member_tag, "tagB");
        assert_eq!(
            plan.puts[0].old_member_tag.as_deref(),
            Some("proxy-selector")
        );
    }

    #[test]
    fn plan_rule_target_unknown_node_must_restart() {
        let (deps, _) = setup_rule_deps();
        let mut old = base_config();
        old.servers.push(ss("ghost-src", "4.4.4.4")); // 保持 servers 集合一致让 norm 等价
        old.custom_rules = vec![ext_rule("r1", Some(NODE_A))];
        let mut new_cfg = old.clone();
        new_cfg.custom_rules = vec![ext_rule("r1", Some("ghost"))]; // ghost 不在 idToTagMap
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::None);
        assert!(plan.must_restart);
    }

    #[test]
    fn plan_rule_no_map_entry_skipped() {
        // currentRuleTargetMap 无 custom:r1 → 跳过（非 null/mustRestart）→ 无规则 puts。
        let (deps, _) = setup_rule_deps();
        let mut deps = deps;
        // 换成只有 r2 的 map
        deps.current_rule_target_map = Some(
            [(
                ("custom:r2".to_string()),
                RuleTargetEntry {
                    selector_tag: "rule-sel-r2".into(),
                    member_tag: "m".into(),
                },
            )]
            .into_iter()
            .collect(),
        );
        let mut old = base_config();
        old.custom_rules = vec![ext_rule("r1", Some(NODE_A))];
        let mut new_cfg = old.clone();
        new_cfg.custom_rules = vec![ext_rule("r1", Some(NODE_B))];
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::None); // 无 puts
        assert!(!plan.must_restart);
    }

    #[test]
    fn plan_rule_no_id_to_tag_map_must_restart() {
        // currentIdToTagMap 未注入但 currentRuleTargetMap 有条目 → null → mustRestart。
        let mut rtm = BTreeMap::new();
        rtm.insert(
            "custom:r1".into(),
            RuleTargetEntry {
                selector_tag: "rule-sel-r1".into(),
                member_tag: "m".into(),
            },
        );
        let deps = HotSwitchDeps {
            current_id_to_tag_map: None,
            current_rule_target_map: Some(rtm),
            platform: "linux".into(),
            ..Default::default()
        };
        let mut old = base_config();
        old.custom_rules = vec![ext_rule("r1", Some(NODE_A))];
        let mut new_cfg = old.clone();
        new_cfg.custom_rules = vec![ext_rule("r1", Some(NODE_B))];
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::None);
        assert!(plan.must_restart);
    }

    #[test]
    fn plan_rule_no_target_map_empty_puts() {
        // currentRuleTargetMap=None（启动无 rule-sel）→ 返空 Vec（非 null）→ 无规则 puts、非 mustRestart。
        let mut deps = deps_with_tags();
        deps.current_rule_target_map = None;
        let mut old = base_config();
        old.custom_rules = vec![ext_rule("r1", Some(NODE_A))];
        let mut new_cfg = old.clone();
        new_cfg.custom_rules = vec![ext_rule("r1", Some(NODE_B))];
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::None);
        assert!(!plan.must_restart);
    }

    #[test]
    fn plan_rule_disabled_rule_skipped() {
        let (deps, _) = setup_rule_deps();
        let mut old = base_config();
        let mut r2 = ext_rule("r2", Some(NODE_A));
        r2.enabled = false;
        old.custom_rules = vec![ext_rule("r1", Some(NODE_A)), r2.clone()];
        let mut new_cfg = old.clone();
        new_cfg.custom_rules[0].target_server_id = Some(NODE_B.into()); // r1 变
        new_cfg.custom_rules[1].target_server_id = Some(NODE_B.into()); // r2 禁用，不参与
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::Rules);
        assert_eq!(plan.puts.len(), 1);
        assert_eq!(plan.puts[0].selector_tag, "rule-sel-r1");
    }

    #[test]
    fn plan_both_global_and_rule_change() {
        // 全局 + 规则同时变 → kind=Both。
        let (deps, _) = setup_rule_deps();
        let mut old = base_config();
        old.custom_rules = vec![ext_rule("r1", Some(NODE_A))];
        let mut new_cfg = old.clone();
        new_cfg.selected_server_id = Some(NODE_B.into());
        new_cfg.custom_rules = vec![ext_rule("r1", Some(NODE_B))];
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::Both);
        assert_eq!(plan.puts.len(), 2);
    }

    #[test]
    fn plan_rule_app_rule_switch() {
        // appRules 换节点 → PUT rule-sel-<appId>。
        use crate::user_config::rule::AppRule;
        let mut map = BTreeMap::new();
        map.insert(NODE_A.into(), "tagA".into());
        map.insert(NODE_B.into(), "tagB".into());
        let mut rtm = BTreeMap::new();
        rtm.insert(
            "app:app1".into(),
            RuleTargetEntry {
                selector_tag: "rule-sel-app1".into(),
                member_tag: "stub".into(),
            },
        );
        let deps = HotSwitchDeps {
            current_id_to_tag_map: Some(map),
            current_rule_target_map: Some(rtm),
            platform: "linux".into(),
            ..Default::default()
        };
        let mut old = base_config();
        old.app_rules = vec![AppRule {
            app_id: "app1".into(),
            action: RuleAction::Proxy,
            enabled: true,
            target_server_id: Some(NODE_A.into()),
        }];
        let mut new_cfg = old.clone();
        new_cfg.app_rules = vec![AppRule {
            app_id: "app1".into(),
            action: RuleAction::Proxy,
            enabled: true,
            target_server_id: Some(NODE_B.into()),
        }];
        let plan = plan_hot_switch(&old, &new_cfg, &deps);
        assert_eq!(plan.kind, HotSwitchKind::Rules);
        assert_eq!(
            plan.puts,
            vec![HotSwitchPut {
                selector_tag: "rule-sel-app1".into(),
                member_tag: "tagB".into(),
                old_member_tag: Some("tagA".into()),
            }]
        );
    }

    // === isServerDirty ===

    #[test]
    fn is_server_dirty_no_snapshot_false() {
        let cfg = base_config();
        let deps = HotSwitchDeps::default(); // 无快照
        assert!(!is_server_dirty(NODE_A, &cfg, &deps));
    }

    #[test]
    fn is_server_dirty_not_in_snapshot_false() {
        let cfg = base_config();
        let mut deps = HotSwitchDeps::default();
        deps.running_servers_fingerprint = Some(BTreeMap::new()); // 空 → A 不在
        assert!(!is_server_dirty(NODE_A, &cfg, &deps));
    }

    #[test]
    fn is_server_dirty_params_changed_true() {
        let cfg = base_config(); // A=1.1.1.1
        let mut deps = HotSwitchDeps::default();
        let mut snap = BTreeMap::new();
        snap.insert(NODE_A.into(), server_fingerprint(&ss(NODE_A, "8.8.8.8"))); // 快照是旧地址
        deps.running_servers_fingerprint = Some(snap);
        assert!(is_server_dirty(NODE_A, &cfg, &deps)); // 1.1.1.1 ≠ 8.8.8.8
    }

    #[test]
    fn is_server_dirty_same_params_false() {
        let cfg = base_config();
        let mut deps = HotSwitchDeps::default();
        let mut snap = BTreeMap::new();
        snap.insert(NODE_A.into(), server_fingerprint(&ss(NODE_A, "1.1.1.1")));
        deps.running_servers_fingerprint = Some(snap);
        assert!(!is_server_dirty(NODE_A, &cfg, &deps));
    }

    // === canSkipRestartForAddedUnreferenced（四步守卫）===

    fn snap_of(servers: &[ServerConfig]) -> BTreeMap<String, String> {
        servers
            .iter()
            .map(|s| (s.id.clone(), server_fingerprint(s)))
            .collect()
    }

    #[test]
    fn can_skip_add_unreferenced_node_true() {
        let a = base_config(); // A 选中, B
        let mut b = base_config();
        b.servers.push(ss("Z", "9.9.9.9")); // 新增未引用 Z
        let snap = snap_of(&a.servers);
        assert!(can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    /// 新增一个 **openconnect / openvpn-client** 节点 ⇒ **不得** defer，必须重启。
    ///
    /// 它们落 `endpoints[]`，无论有没有被选中/被规则指向都自成一条出网路径（内核起来就在跑），
    /// 不是「只挂在 selector 上的惰性成员」。承流播种此前只认 WG/TS，这两个协议漏在外面 ——
    /// 后果不是「少一次重启」而是**静默失效**：走 defer 腿不重启，核继续用旧配置，用户以为加上了。
    ///
    /// 变异对照：把 `endpoint_routes.rs` 的播种判据改回 `is_mesh_protocol` ⇒ 本条转红。
    #[test]
    fn adding_an_endpoint_leg_vpn_client_forces_restart() {
        use crate::user_config::protocol_settings::OpenconnectSettings;
        use crate::user_config::server_config::Protocol;
        for proto in [Protocol::Openconnect, Protocol::OpenvpnClient] {
            let a = base_config();
            let mut b = base_config();
            b.servers.push(ServerConfig {
                id: "vpn".into(),
                name: "VPN".into(),
                protocol: proto,
                openconnect_settings: Some(Box::new(OpenconnectSettings {
                    server: Some("vpn.example.com:443".into()),
                    ..Default::default()
                })),
                ..Default::default()
            });
            let snap = snap_of(&a.servers);
            assert!(
                !can_skip_restart_for_added_unreferenced(&a, &b, &snap),
                "{proto:?} 新增被判成「未引用可 defer」—— 它是 endpoint 腿，核起来就在承流"
            );
        }
    }

    #[test]
    fn can_skip_delete_unreferenced_node_true() {
        // P2-B：删未引用节点 → defer。
        let mut a = base_config();
        a.servers.push(ss("Z", "9.9.9.9"));
        let b = base_config(); // 删 Z
        let snap = snap_of(&a.servers);
        assert!(can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    #[test]
    fn can_skip_edit_unreferenced_node_true() {
        // P2-B：改未引用节点 address → defer（dirty 闸门防热切到旧参数）。
        let mut a = base_config();
        a.servers.push(ss("Z", "9.9.9.9"));
        let mut b = base_config();
        b.servers.push(ss("Z", "5.5.5.5")); // Z 地址变
        let snap = snap_of(&a.servers);
        assert!(can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    #[test]
    fn can_skip_edit_rule_targeted_node_false() {
        // 删/改被规则指向的节点 → 重启（被引用，改/删影响活流量）。
        let mut a = base_config();
        a.servers.push(ss("Z", "9.9.9.9"));
        a.custom_rules = vec![ext_rule("r1", Some("Z"))];
        let mut b = a.clone();
        b.servers = vec![
            ss(NODE_A, "1.1.1.1"),
            ss(NODE_B, "2.2.2.2"),
            ss("Z", "5.5.5.5"),
        ]; // Z 地址变
        let snap = snap_of(&a.servers);
        assert!(!can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    #[test]
    fn can_skip_edit_selected_node_false() {
        // 改选中节点参数 → 重启（选中∈旧节点、须不变）。
        let a = base_config(); // A=1.1.1.1 选中
        let mut b = base_config();
        b.servers = vec![ss(NODE_A, "8.8.8.8"), ss(NODE_B, "2.2.2.2")];
        let snap = snap_of(&a.servers);
        assert!(!can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    #[test]
    fn can_skip_add_endpoint_node_false() {
        // 新增 endpoint 节点 → 重启（endpoint 被引用：可 force-route 子网）。
        let a = base_config();
        let mut b = base_config();
        b.servers.push(wg("wg1", None, None));
        let snap = snap_of(&a.servers);
        assert!(!can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    #[test]
    fn can_skip_change_selected_server_id_false() {
        // ① selectedServerId 变 → 重启。
        let a = base_config();
        let mut b = base_config();
        b.selected_server_id = Some(NODE_B.into());
        let snap = snap_of(&a.servers);
        assert!(!can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    #[test]
    fn can_skip_add_rule_false() {
        // ② 非 servers 字段变（加规则）→ 重启（正交守卫）。
        let a = base_config();
        let mut b = base_config();
        b.custom_rules = vec![ext_rule("r1", None)];
        let snap = snap_of(&a.servers);
        assert!(!can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    #[test]
    fn can_skip_add_node_with_detour_to_old_unreferenced_true() {
        // 新增节点的 detour 指向某旧节点（链未触达选中）→ 仍可免重启（新节点整体未被引用）。
        let a = base_config();
        let mut b = base_config();
        let mut z = ss("Z", "9.9.9.9");
        z.detour = Some(NODE_B.into());
        b.servers.push(z);
        let snap = snap_of(&a.servers);
        assert!(can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    #[test]
    fn can_skip_port_only_change_on_unreferenced_true() {
        // 改未引用节点任一参数（同址端口）→ defer（P2-B）。
        let mut a = base_config();
        a.servers.push(ss_extra("Z", "9.9.9.9", 8388));
        let mut b = base_config();
        b.servers.push(ss_extra("Z", "9.9.9.9", 9999)); // 端口变
        let snap = snap_of(&a.servers);
        assert!(can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    #[test]
    fn can_skip_added_node_also_rule_targeted_false() {
        // 新增节点同时被规则指向（被引用）→ 重启（②规则变 与 ④Z被引用 双重拦截）。
        let a = base_config();
        let mut b = base_config();
        b.servers.push(ss("Z", "9.9.9.9"));
        b.custom_rules = vec![ext_rule("r1", Some("Z"))];
        let snap = snap_of(&a.servers);
        assert!(!can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    // === selector default 兜底态：defer 腿不得放行任何节点编辑 ===

    /// 未选节点态的两节点基线（刚导入订阅、还没选出口）。
    fn no_selection_config() -> UserConfig {
        UserConfig {
            selected_server_id: None,
            ..base_config()
        }
    }

    /// 【缺陷复现 · 首节点】`selectedServerId=None` ⇒ `build_outbounds`（outbounds.rs:262-271）把
    /// proxy-selector 的 default 落到 `node_tags.first()`，该节点承载**全部**代理流量。
    /// 它若不在 `referenced_server_ids` 里，改它的 address 会被本函数第③步判「未引用 → 放行」
    /// → 走 defer 腿不重启 → 核继续用**旧地址**出网，且无任何提示（热切腿有 `is_server_dirty`
    /// 闸门，defer 腿没有）。本用例红 = 这条静默失效回来了。
    #[test]
    fn can_skip_edit_first_node_without_selection_false() {
        let a = no_selection_config();
        let mut b = a.clone();
        b.servers[0] = ss(NODE_A, "8.8.8.8");
        let snap = snap_of(&a.servers);
        assert!(!can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    /// 【缺陷复现 · 非首节点】兜底命中的是「生成期**第一个成功发射**的节点」，而生成期跳过了谁
    /// 取决于运行期能力（naive 缺 cronet / WG 不可路由 / custom-endpoint 解析失败）——UserConfig
    /// 静态算不出 ⇒ 未选节点态下**任何**节点都可能是 live default，改任何一个都必须重启。
    /// 本用例红 = 判据退化成「只保护 servers[0]」，前面的节点一被跳过就又漏。
    #[test]
    fn can_skip_edit_non_first_node_without_selection_false() {
        let a = no_selection_config();
        let mut b = a.clone();
        b.servers[1] = ss(NODE_B, "8.8.8.8");
        let snap = snap_of(&a.servers);
        assert!(!can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    /// 【同型第二处 · prune 后重算 default】`prune_detour_dead_references` 剔掉 detour 死引用的
    /// outbound 后，经 `pruned_selector_default`（outbound_helpers.rs:147）把 proxy-selector 的
    /// default 重算成 `remaining.first()`（outbounds.rs:568-578）——又一个「不在任何播种里」的节点。
    ///
    /// 此处 NODE_A 的 detour 指向 naive 节点：缺 libcronet 时 naive 不发射 → NODE_A 成死引用被剔
    /// → default 由 NODE_B 接棒。该重算只可能发生在「default ≠ 选中节点 tag」时（default == 选中
    /// tag 会走 outbounds.rs:558 的 Err 腿而非静默重算）⇒ 与兜底态同一状态，故同一道闸覆盖。
    /// 本用例红 = 接棒者漏出引用集，改它照样静默不重启。
    #[test]
    fn can_skip_edit_reelected_default_after_prune_false() {
        let mut a = no_selection_config();
        a.servers[0].detour = Some("naive-1".into());
        a.servers.push(ServerConfig {
            protocol: Protocol::Naive,
            ..ss("naive-1", "9.9.9.9")
        });
        let mut b = a.clone();
        b.servers[1] = ss(NODE_B, "8.8.8.8"); // 改接棒者
        let snap = snap_of(&a.servers);
        assert!(!can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }

    /// 【新增节点在兜底态也不得放行】未选节点时新增一个节点：它可能排在现有节点之前、
    /// 或前面的节点被跳过而由它接棒成 live default ⇒ 不能按「新增即未引用」放行。
    #[test]
    fn can_skip_add_node_without_selection_false() {
        let a = no_selection_config();
        let mut b = a.clone();
        b.servers.push(ss("Z", "9.9.9.9"));
        let snap = snap_of(&a.servers);
        assert!(!can_skip_restart_for_added_unreferenced(&a, &b, &snap));
    }
}
