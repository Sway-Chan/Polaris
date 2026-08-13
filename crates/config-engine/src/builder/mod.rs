//! sing-box config builder 模块（Polaris 六 builder 1:1 移植）。
//!
//! 增量移植中（按 B1 计划）。每个 builder 纯函数 + 依赖注入（路径/实例态经参数传入，
//! 不硬编码 → 金样对拍可注入固定假路径）。

pub mod custom_rule_files;
pub mod custom_rules;
pub mod dns;
pub mod endpoint_routes;
pub mod endpoints;
pub mod generate;
pub mod helpers;
pub mod hotswitch;
pub mod inbounds;
pub mod log;
pub mod orchestration;
pub mod outbound;
pub mod outbound_helpers;
pub mod outbounds;
pub mod route;
pub mod tun_route_exclude;

pub use custom_rule_files::{
    build_custom_rule_files, cond_matcher_fields, custom_rule_file_base,
    is_custom_rule_orphan_file, is_ext_type, plan_custom_rule, RulePlan,
};
pub use custom_rules::{apply_rule_action, build_custom_rules, CustomRulesDeps, CustomRulesResult};
pub use generate::{
    generate_sing_box_config, generate_sing_box_config_with_report, GenerateConfigDeps,
    GenerateOutcome, InvalidNode,
};
pub use helpers::{
    apply_rule_set_prune, build_id_to_tag_map, effective_app_rules, effective_custom_rules,
    get_required_geo_categories, host_to_exclude_cidr, is_ipv4_host, is_ipv6_host,
    is_probe_pool_inbound_tag, probe_pool_inbound_tag, strip_host_brackets,
    DOMESTIC_BANK_AND_STOCK_DOMAINS, PROBE_POOL_INBOUND_TAG_PREFIX, RESERVED_OUTBOUND_TAGS,
};
pub use log::{build_log_config, LogBuildDeps, LogConfigInput};
// Platform 作单一真值（polaris-helper-proto）：log.rs 仅 `use`，未 re-export，故在此显式转 re-export。
pub use polaris_helper_proto::Platform;
