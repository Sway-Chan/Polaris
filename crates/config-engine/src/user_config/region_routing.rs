//! 地区分流（上游 `shared/region-routing.ts`）。
//!
//! 智能分流的 geo 基线场景单一真值。route/dns 生成共用。
//! `region_routing = None`（存量/新装未改）= 默认中国大陆正向 = 今日智能分流行为 → 零迁移、对拍默认零变。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// 地区 ID（磁盘 JSON 未受编译期约束，手改/旧配置越界值 → 查表取空，fail-safe）。
/// 序列化为小写字符串（Polaris JSON 兼容）。
pub type RegionId = String;

/// 地区分流配置。上游 `RegionRoutingConfig`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionRoutingConfig {
    /// 总开关：true=按地区自动分流（本地直连·海外代理）；false=只用自定义规则。
    pub enabled: bool,
    /// 选用哪套地区 geo。
    pub region: RegionId,
    /// 反向：本地走代理·海外直连（region=cn+reverse=true=回国）。
    pub reverse: bool,
}

impl Default for RegionRoutingConfig {
    fn default() -> Self {
        default_region_routing()
    }
}

/// 默认地区分流配置。上游 `DEFAULT_REGION_ROUTING`。
pub fn default_region_routing() -> RegionRoutingConfig {
    RegionRoutingConfig {
        enabled: true,
        region: "cn".to_string(),
        reverse: false,
    }
}

/// 取生效的地区分流配置；缺省=默认中国大陆正向（=今日行为）。上游 `effectiveRegionRouting`。
pub fn effective_region_routing(
    region_routing: Option<&RegionRoutingConfig>,
) -> RegionRoutingConfig {
    match region_routing {
        Some(rr) => rr.clone(),
        None => default_region_routing(),
    }
}

/// 单个地区的 geo tag 组（本地 / 海外）。上游 `REGION_LOCAL_GEO` / `REGION_FOREIGN_GEO` 表项。
#[derive(Debug, Clone, Default)]
pub struct GeoTagSet {
    pub geosite: Vec<String>,
    pub geoip: Vec<String>,
}

/// 支持的全部地区 id。
///
/// 存在的理由是「随包 geo 必须有消费点」那道门（`tests/builtin_geo_alignment.rs`）要枚举地区分流
/// 用到的全部 tag，而下面两个函数是 `match`、枚举不出来。**新增地区必须同时进这张表**；忘了也不会
/// 漏网 —— 新地区的 geo 必然要随包（否则 fail-closed 剪枝），随包了却没被本表带进消费面，那道门立刻转红。
pub const ALL_REGIONS: &[&str] = &["cn", "ir", "ru"];

/// 各地区「本地」geo rule_set tag（正向→直连 / 反向→代理 的那一组）。
/// tag 与 `BUILTIN_GEO_RULESETS` 对齐。上游 `REGION_LOCAL_GEO`。
pub fn region_local_geo(region: &str) -> Option<GeoTagSet> {
    match region {
        "cn" => Some(GeoTagSet {
            geosite: vec!["geosite-cn".into()],
            geoip: vec!["geoip-cn".into()],
        }),
        "ir" => Some(GeoTagSet {
            geosite: vec!["geosite-category-ir".into()],
            geoip: vec!["geoip-ir".into()],
        }),
        "ru" => Some(GeoTagSet {
            geosite: vec!["geosite-category-ru".into()],
            geoip: vec!["geoip-ru".into()],
        }),
        _ => None,
    }
}

/// 各地区「海外」geo（正向→代理 / 反向→直连 的那一组）。
/// 仅 CN 有成熟的 geolocation-!cn；ir/ru 无对应 !ir/!ru 分类 → 空，靠 final 兜底。
/// 上游 `REGION_FOREIGN_GEO`。
pub fn region_foreign_geo(region: &str) -> Vec<String> {
    match region {
        "cn" => vec!["geosite-geolocation-!cn".into()],
        _ => vec![],
    }
}

/// 智能分流「geo 基线层」用到的内置 geo tag 集合（本地 + 海外，随地区）。
///
/// 仅 `proxy_mode=smart` 且地区分流启用时非空；global/direct 或关地区分流 → 空（基线不生效）。
/// 供规则资源 referencedBy 计数纳入「系统/智能分流对内置默认的隐式引用」。
/// 上游 `smartBaselineGeoTags`。
pub fn smart_baseline_geo_tags(
    proxy_mode: &str,
    region_routing: Option<&RegionRoutingConfig>,
) -> BTreeSet<String> {
    if !proxy_mode.eq_ignore_ascii_case("smart") {
        return BTreeSet::new();
    }
    let rr = effective_region_routing(region_routing);
    if !rr.enabled {
        return BTreeSet::new();
    }
    let local = match region_local_geo(&rr.region) {
        Some(g) => g,
        None => return BTreeSet::new(), // 越界 region fail-safe 返回空集
    };
    let foreign = region_foreign_geo(&rr.region);
    let mut tags = BTreeSet::new();
    for t in &local.geosite {
        tags.insert(t.clone());
    }
    for t in &local.geoip {
        tags.insert(t.clone());
    }
    for t in &foreign {
        tags.insert(t.clone());
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_cn_forward() {
        let rr = default_region_routing();
        assert!(rr.enabled);
        assert_eq!(rr.region, "cn");
        assert!(!rr.reverse);
    }

    #[test]
    fn effective_none_uses_default() {
        let rr = effective_region_routing(None);
        assert_eq!(rr.region, "cn");
        assert!(rr.enabled);
    }

    #[test]
    fn local_geo_cn() {
        let g = region_local_geo("cn").unwrap();
        assert_eq!(g.geosite, vec!["geosite-cn".to_string()]);
        assert_eq!(g.geoip, vec!["geoip-cn".to_string()]);
    }

    #[test]
    fn local_geo_unknown_returns_none() {
        assert!(region_local_geo("us").is_none());
    }

    #[test]
    fn all_regions_resolve() {
        // 表里挂了个 `region_local_geo` 不认的 id → 那道门会把它的 geo 当「无消费点」，
        // 得到一个反向的假红。
        for r in ALL_REGIONS {
            assert!(
                region_local_geo(r).is_some(),
                "ALL_REGIONS 里的 {r} 查不到 geo"
            );
        }
    }

    #[test]
    fn foreign_geo_cn_has_noncn() {
        let g = region_foreign_geo("cn");
        assert_eq!(g, vec!["geosite-geolocation-!cn".to_string()]);
    }

    #[test]
    fn foreign_geo_ir_empty() {
        assert!(region_foreign_geo("ir").is_empty());
    }

    #[test]
    fn smart_baseline_smart_cn_enabled() {
        let rr = default_region_routing();
        let tags = smart_baseline_geo_tags("smart", Some(&rr));
        assert!(tags.contains("geosite-cn"));
        assert!(tags.contains("geoip-cn"));
        assert!(tags.contains("geosite-geolocation-!cn"));
    }

    #[test]
    fn smart_baseline_global_returns_empty() {
        let rr = default_region_routing();
        let tags = smart_baseline_geo_tags("global", Some(&rr));
        assert!(tags.is_empty());
    }

    #[test]
    fn smart_baseline_smart_disabled_returns_empty() {
        let rr = RegionRoutingConfig {
            enabled: false,
            region: "cn".into(),
            reverse: false,
        };
        let tags = smart_baseline_geo_tags("smart", Some(&rr));
        assert!(tags.is_empty());
    }

    #[test]
    fn smart_baseline_smart_unknown_region_returns_empty() {
        let rr = RegionRoutingConfig {
            enabled: true,
            region: "us".into(),
            reverse: false,
        };
        let tags = smart_baseline_geo_tags("smart", Some(&rr));
        assert!(tags.is_empty());
    }
}
