//! 用户配置输入类型（上游 `shared/types.ts` UserConfig/ServerConfig 子集）。
//!
//! 增量定义：仅落地当前 builder 所需字段，随 builder 移植扩展（不全量预定义 80+ 字段，YAGNI）。
//! 字段 Optionality 与 TS 侧一致；serde rename 对齐 Polaris JSON 键名（对拍约束）。

pub mod app_config;
pub mod app_rules_preset;
pub mod builtin_geo_rulesets;
pub mod cidr;
pub mod collections;
pub mod control_url;
pub mod dns_config;
pub mod dns_constants;
pub mod dns_spec;
pub mod fakeip_filter;
pub mod ip;
pub mod log_level;
pub mod neighbor;
pub mod normalize;
pub mod own_lan;
pub mod protocol_settings;
pub mod proxy_mode;
pub mod proxy_ports;
pub mod region_routing;
pub mod rule;
pub mod rule_resource_catalog;
pub mod rule_resource_refs;
pub mod rule_validate;
pub mod rules;
pub mod server_config;
pub mod system_proxy_bypass;
pub mod tls_spoof;
pub mod tun_config;
pub mod tun_stack;

pub use app_config::UserConfig;

pub use app_rules_preset::{all_presets_dto, get_app_preset_dto, AppPresetDto};
pub use rule_resource_catalog::{
    builtin_catalog_result, find_catalog_item, rule_resource_catalog, RuleResourceCatalogItem,
    RuleResourceCatalogResult,
};
pub use rule_resource_refs::{
    enumerate_resource_refs, geo_tag_of, is_resource_referenced, RefScanInput, RuleResourceRef,
};

pub use control_url::{reject_token, tailscale_control_url_reject, ControlUrlReject};
pub use ip::{is_ip_literal, is_ipv4, is_ipv4_literal, is_ipv6_literal};
pub use log_level::LogLevel;
pub use neighbor::{
    is_source_device_match_supported, is_tun_mac_filter_supported, is_valid_mac_address,
    is_valid_neighbor_domain, is_valid_source_hostname, normalize_neighbor_domain,
    TunMacFilterMode,
};
pub use normalize::{de_opt_token, normalize_token};
pub use proxy_mode::{ProxyMode, ProxyModeType};
pub use proxy_ports::{
    control_api_port, local_proxy_port, PortConfig, DEFAULT_CONTROL_PORT, DEFAULT_MIXED_PORT,
};
pub use region_routing::{
    default_region_routing, effective_region_routing, region_foreign_geo, region_local_geo,
    smart_baseline_geo_tags, GeoTagSet, RegionId, RegionRoutingConfig,
};
pub use rule::{
    AppRule, CombineMode, CustomAppPreset, Rule, RuleAction, RuleCondition, RuleResource,
    RuleResourceFormat, RuleType,
};
pub use rule_validate::{
    is_known_rule_type, is_valid_ip_cidr, validate_rule, validate_rule_value, RuleValidation,
    RULE_TYPE_IDS,
};
pub use rules::{is_valid_port_value, parse_port_values, rule_conditions, rule_ip_cidrs};
pub use tls_spoof::{validate_tls_spoof_default, TLS_SPOOF_METHODS};
pub use tun_stack::{resolve_tun_stack, ConcreteTunStack, TunStack};
