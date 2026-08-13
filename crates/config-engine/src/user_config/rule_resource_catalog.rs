//! 规则资源库（catalog）内置精选清单 —— **Rust 是单一真值（SoT）**。
//!
//! 前端曾持有同一张表（`ui/src/shared/rule-resource-catalog.ts` 的 `RULE_RESOURCE_CATALOG`），
//! 现经 `rule_resources_get_catalog` command 下发（常量表 → Rust SoT + 一次 invoke 拉取，
//! 判据见 ~/docs/polaris/design/polaris-dialog-layer-and-governance.md §3.1 Q3）。
//!
//! **本表不再是独立表，而是 `builtin_geo_rulesets()` 的投影**。
//!
//! 此前两张表各自维护、只有交集（33 条精选 vs 28 条随包，重合 19 条），于是「内置」tab 里同时躺着
//! 两类东西：随包出厂的（勾上、不用下）和只是列在这儿的（点了要联网下）。同一个词「内置」指两件事，
//! 用户看不出区别 —— 而列而不随包的那 14 条无一被代码引用（既不在地区分流基线，也不在内置应用预设），
//! 选中它们只会走到 `route.rs` 的 fail-closed 剪枝：规则静默失效。
//!
//! 裁定（2026-07-30）：**内置 = 随包 = 有消费点**，三者恒等。恒等由「投影」而非「一条对拍测试」
//! 保证 —— 测试只能在漂了之后转红，投影让它压根漂不出来。
//!
//! 全量可下载清单仍在「外置」tab（`rule_resources_refresh_catalog` 拉 meta-rules-dat 全量），
//! 想要 `geoip-jp` / `geosite-apple` 这类未随包资源的用户走那条路，不受本次收敛影响。

#![forbid(unsafe_code)]

use super::builtin_geo_rulesets::{builtin_geo_rulesets, BuiltinGeoRuleSet, GeoCategory};
use serde::Serialize;

/// meta-rules-dat `sing` 分支 raw 基址。上游 `MRD_RAW_BASE`。
///
/// 注：`builtin_geo_rulesets::mrd_geo_raw_base()` 是**另一个**基址（多一段 `/geo`），供随包 geo 更新用；
/// 本表的 `path` 已含 `geo/` 前缀，故基址到 `sing/` 为止。二者不可互换。
pub const MRD_RAW_BASE: &str = "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/";

/// 仓库内路径（如 `geo/geosite/youtube.srs`）→ raw 下载 URL。上游 `mrdRawUrl`。
pub fn mrd_raw_url(path: &str) -> String {
    format!("{MRD_RAW_BASE}{path}")
}

/// 资源库条目。上游 `RuleResourceCatalogItem`（`ui/src/shared/types/rules.ts`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleResourceCatalogItem {
    pub id: String,
    /// `geosite` / `geoip` / `geosite-lite` / `geoip-lite` / `custom`。
    pub category: String,
    pub name: String,
    /// meta-rules-dat 仓库内路径。
    pub path: String,
    /// 该条目**已随包出厂**（`builtin_geo_rulesets` 表内同名 tag），资源库里应显示为「已内置」而非可下载。
    ///
    /// 字段保留而非删除，尽管本表投影出来的条目恒为 `true`：真正需要它的是**外置 tab** ——
    /// 远端全量清单里也会出现随包同名项，那边必须逐条现算才知道该不该标「已内置」
    /// （`commands/rules.rs` 的 `catalog_item_from_tree_path`）。
    ///
    /// 恒由 Rust 现算、**不从缓存回读**（见 `commands/rules.rs` 的 `parse_cached_catalog_item`）：
    /// 随包内容随版本走，缓存却是上个版本落的盘，信缓存等于让旧版本的随包清单决定新版本的 UI。
    pub bundled: bool,
}

/// catalog 拉取结果。上游 `RuleResourceCatalogResult`。
///
/// `source` **自述来源**（`remote` / `cache` / `builtin`）—— 这是本 DTO 最要紧的字段：
/// 无网络时返回内置精选表并标 `source:"builtin"` 是**诚实降级**（前端可据此提示「离线清单」），
/// 与「谎称拉到了远端全量」是两回事。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleResourceCatalogResult {
    pub items: Vec<RuleResourceCatalogItem>,
    /// 远端拉取时间戳（ms）；内置回退为 `None` → TS `fetchedAt: number | null`。
    pub fetched_at: Option<i64>,
    pub source: String,
}

/// 单条随包 geo → 资源库条目。
///
/// `name` 取自 **tag**、`path` 取自 **file_name**，两者只在 `geosite-category-ai` 这一条上分岔
/// （tag 是 `geosite-category-ai`，上游文件却叫 `category-ai-!cn.srs`）。分岔必须保留：
/// - `name` 若跟着文件名走 → 应用分流弹窗的标签池会出现 `category-ai-!cn`，用它建的预设生成
///   `geosite-category-ai-!cn`，不在随包表内 → fail-closed 剪枝，规则静默失效；
/// - `path` 若跟着 tag 走 → 下载 URL 指向上游不存在的 `geo/geosite/category-ai.srs` → 404。
fn catalog_item_of(b: BuiltinGeoRuleSet) -> RuleResourceCatalogItem {
    let kind = match b.category {
        GeoCategory::Geosite => "geosite",
        GeoCategory::Geoip => "geoip",
    };
    let prefix = format!("{kind}-");
    let name = b.tag.strip_prefix(&prefix).unwrap_or(&b.tag).to_string();
    let stem = b.file_name.strip_suffix(".srs").unwrap_or(&b.file_name);
    let bare = stem.strip_prefix(&prefix).unwrap_or(stem);
    RuleResourceCatalogItem {
        path: format!("geo/{kind}/{bare}.srs"),
        // 投影自随包表 ⇒ 恒真，无须再查一次 `is_bundled_geo_tag`。
        bundled: true,
        id: b.tag.clone(),
        category: kind.to_string(),
        name,
    }
}

/// 内置清单 = 随包清单的投影。
pub fn rule_resource_catalog() -> Vec<RuleResourceCatalogItem> {
    builtin_geo_rulesets()
        .into_iter()
        .map(catalog_item_of)
        .collect()
}

/// 按 id 查条目。上游 `findCatalogItem`。
pub fn find_catalog_item(id: &str) -> Option<RuleResourceCatalogItem> {
    rule_resource_catalog().into_iter().find(|i| i.id == id)
}

/// 内置回退的 catalog 结果（`source:"builtin"`, `fetchedAt:null`）。
pub fn builtin_catalog_result() -> RuleResourceCatalogResult {
    RuleResourceCatalogResult {
        items: rule_resource_catalog(),
        fetched_at: None,
        source: "builtin".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内置清单 ≡ 随包清单（逐条同序）。投影让它恒成立，这条只是把恒等写成可执行的断言：
    /// 谁把 `rule_resource_catalog()` 改回独立表，第一个撞上的就是它。
    #[test]
    fn catalog_is_exactly_the_bundled_table() {
        let ids: Vec<String> = rule_resource_catalog().into_iter().map(|i| i.id).collect();
        let tags: Vec<String> = builtin_geo_rulesets().into_iter().map(|b| b.tag).collect();
        assert_eq!(ids, tags);
    }

    #[test]
    fn ids_are_unique() {
        let items = rule_resource_catalog();
        let mut ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "catalog id 重复");
    }

    #[test]
    fn geosite_path_and_id() {
        let i = find_catalog_item("geosite-youtube").unwrap();
        assert_eq!(i.path, "geo/geosite/youtube.srs");
        assert_eq!(i.category, "geosite");
        assert_eq!(i.name, "youtube");
    }

    /// `geosite-category-ai` 是全表唯一 tag 与上游文件名不同的一条：id/name 跟 tag，path 跟文件名。
    /// 任一侧跟错，要么下载 404，要么建出的预设引用一个不随包的 tag 被静默剪掉。
    #[test]
    fn category_ai_keeps_tag_side_and_file_side_apart() {
        let i = find_catalog_item("geosite-category-ai").unwrap();
        assert_eq!(i.name, "category-ai");
        assert_eq!(i.path, "geo/geosite/category-ai-!cn.srs");
    }

    /// 精简版（`geo-lite/`）不再进内置清单：它们从不随包，且没有任何代码引用
    /// （地区分流基线走 `geosite-cn`，不走 `geosite-lite-cn`）。仍可从「外置」tab 下载。
    #[test]
    fn lite_variants_are_not_builtin() {
        assert!(find_catalog_item("geosite-lite-cn").is_none());
        assert!(find_catalog_item("geoip-lite-cn").is_none());
    }

    #[test]
    fn negated_name_preserved_in_path() {
        // 'geolocation-!cn' 含 '!'，不得被转义/清洗（下载 URL 逐字节要对）。
        let i = find_catalog_item("geosite-geolocation-!cn").unwrap();
        assert_eq!(i.path, "geo/geosite/geolocation-!cn.srs");
        assert_eq!(
            mrd_raw_url(&i.path),
            "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/geolocation-!cn.srs"
        );
    }

    #[test]
    fn builtin_result_self_describes_source() {
        let r = builtin_catalog_result();
        assert_eq!(r.source, "builtin");
        assert!(r.fetched_at.is_none(), "内置回退不得谎报拉取时间");
        assert_eq!(r.items.len(), builtin_geo_rulesets().len());
    }

    /// 「内置」tab 里不得出现要联网才拿得到的条目 —— 那正是本次收敛消灭的形态：
    /// 用户看到「内置」，点下去却要下载，下不下来就静默剪枝。
    #[test]
    fn every_builtin_item_is_bundled() {
        for i in rule_resource_catalog() {
            assert!(i.bundled, "{} 列在内置清单却不随包", i.id);
        }
        // 未随包的资源一律不在内置清单（要它们请走「外置」tab）。
        for id in ["geoip-us", "geosite-apple", "geosite-category-ads-all"] {
            assert!(
                find_catalog_item(id).is_none(),
                "{id} 未随包，不得进内置清单"
            );
        }
    }

    /// 内置条目的下载地址必须与随包更新腿的 `source_url()` 同址 —— 不同址就意味着同一份数据
    /// 在「资源库下载」与「内置更新」两条腿会取到两个来源。
    /// CN 三件套除外：它们的更新腿走 SagerNet release 资产，本就不是 MRD raw（见随包表注释）。
    #[test]
    fn catalog_url_matches_builtin_source_url() {
        use super::super::builtin_geo_rulesets::mrd_geo_raw_base;
        for b in builtin_geo_rulesets() {
            let url = b.source_url();
            if !url.starts_with(mrd_geo_raw_base()) {
                continue;
            }
            let item = find_catalog_item(&b.tag).unwrap();
            assert_eq!(mrd_raw_url(&item.path), url, "{} 两腿地址不一致", b.tag);
        }
    }

    #[test]
    fn find_unknown_returns_none() {
        assert!(find_catalog_item("geosite-nonexistent").is_none());
    }

    // 「前端不得再有第二份 catalog 表」的门在 `tests/frontend_sot_guard.rs`（与预设表的同类门同处）。
}
