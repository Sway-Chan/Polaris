//! 「内置清单 / 随包物料 / 消费点」三方恒等的门。
//!
//! 裁定（2026-07-30）：**既然叫内置，就该全随包；随包的每一条都得真有人用**。三个集合互为充要，
//! 任意一方漂了这里都转红。三条腿各自的守法：
//!
//! - **内置清单 ≡ 随包表**：`rule_resource_catalog()` 已改成随包表的投影，恒等由结构保证
//!   （`rule_resource_catalog.rs::catalog_is_exactly_the_bundled_table`），本文件不再重复。
//! - **随包表 ≡ 盘上 .srs**：`src-tauri/src/runtime/geo_seed.rs` 的出厂副本门 + `build.rs` 的
//!   `EXPECTED_SRS_COUNT`，各自在自己的 crate 里守。
//! - **随包表 ≡ 消费面**：本文件。允许具名例外（`BUNDLED_FOR_USER_RULES_ONLY`）——
//!   价值在用户面、判据看不见的 tag 走这条，且自带保质期门。
//!
//! 消费面 = 代码里**确定**引用某 geo tag 的三处，没有第四处：
//!   ① 地区分流 geo 基线（`region_routing`，随所选地区进 route/dns）；
//!   ② 内置应用分流预设（`app_rules_preset_data`，用户开哪个应用就引哪几个 tag）；
//!   ③ `route.rs` 私有/本地域名直连那条固定腿（`PRIVATE_DOMAIN_DIRECT_TAG`）。
//!
//! 「用户可以在自定义规则里手写任意 geosite/geoip 标签」**不算消费点**：那条路对全世界的 tag 都成立，
//! 拿它当判据等于判据恒真，这道门就没有牙了。

use polaris_config_engine::user_config::app_rules_preset::all_presets_dto;
use polaris_config_engine::user_config::builtin_geo_rulesets::{
    builtin_geo_rulesets, PRIVATE_DOMAIN_DIRECT_TAG,
};
use polaris_config_engine::user_config::region_routing::{
    region_foreign_geo, region_local_geo, ALL_REGIONS,
};
use std::collections::BTreeSet;

/// **已裁定保留**（陈先生，2026-07-30）：随包、无代码消费点，但**有用户面使用场景** ——
/// 用户在自定义规则里引私网 IP 段（`geoip-private` 是 `geosite-private` 的 IP 侧对偶）。
///
/// 这条为什么必须是具名例外、而不是把它接进 `consumed_geo_tags()`：上面第 17 行的判据
/// 「用户可以手写任意 tag 不算消费点」是这道门的牙，一旦为它开口子，判据就恒真、门就废了。
/// 所以它的价值在用户面而判据只看代码面 ⇒ 只能人工具名放行，一条一条来，且被下面那道
/// 保质期门盯着。
///
/// 事实备查（2026-07-30 核实，与裁定同向，不是裁定依据）：
/// - `fixtures/config-snapshot.json` 里 37 个场景有 **36** 个含它，唯一没有的是
///   `update-in 端口注入（direct）`—— 直连模式压根不注入 rule_set（`route.rs` 那道门），
///   不是它被区别对待。所以保留它也顺带维持了金样对拍的严格 1:1。
/// - 金样锁的是「Rust ≡ 上游 TS」（见 `golden_config_snapshot.rs` 开头），导出器在 上游仓、
///   跑的是 上游的 TS 实现，而 上游的 `APP_GEOIP_TAGS` 里仍有 `'private'`。
///   即：**「删掉它再重导金样」是条死路**，重导会逐字复现，删除数为 0。别再往那条路上走。
const BUNDLED_FOR_USER_RULES_ONLY: &[&str] = &["geoip-private"];

/// 代码里确定引用的全部 geo tag。
fn consumed_geo_tags() -> BTreeSet<String> {
    let mut tags = BTreeSet::new();
    // ① 地区分流基线
    for region in ALL_REGIONS {
        if let Some(local) = region_local_geo(region) {
            tags.extend(local.geosite);
            tags.extend(local.geoip);
        }
        tags.extend(region_foreign_geo(region));
    }
    // ② 内置应用分流预设（表里存裸名，加 kind 前缀还原成 tag）
    for p in all_presets_dto() {
        tags.extend(p.geosite_tags.iter().map(|t| format!("geosite-{t}")));
        tags.extend(p.geoip_tags.iter().map(|t| format!("geoip-{t}")));
    }
    // ③ route.rs 固定腿
    tags.insert(PRIVATE_DOMAIN_DIRECT_TAG.to_string());
    tags
}

fn bundled_tags() -> BTreeSet<String> {
    builtin_geo_rulesets().into_iter().map(|b| b.tag).collect()
}

/// 随包的每一条都得有人用。破法：往随包表加个没人引用的 tag（顺手补上 .srs 骗过播种门）→ 这里红。
#[test]
fn every_bundled_tag_has_a_consumer() {
    let consumed = consumed_geo_tags();
    let orphans: Vec<String> = bundled_tags()
        .into_iter()
        .filter(|t| !consumed.contains(t) && !BUNDLED_FOR_USER_RULES_ONLY.contains(&t.as_str()))
        .collect();
    assert!(
        orphans.is_empty(),
        "随包但无消费点（白占安装包体积，且在「内置」tab 里冒充有用条目）：{orphans:?}"
    );
}

/// 用到的每一条都得随包。破法：给某个预设加个没随包的 geo tag → 这里红（不加这条门的后果是
/// 生成配置时该 tag 被 fail-closed 剪枝，规则静默失效，只在真机上才看得出来）。
#[test]
fn every_consumed_tag_is_bundled() {
    let bundled = bundled_tags();
    let missing: Vec<String> = consumed_geo_tags()
        .into_iter()
        .filter(|t| !bundled.contains(t))
        .collect();
    assert!(
        missing.is_empty(),
        "代码引用了未随包的 geo tag（运行时会被 fail-closed 剪枝，规则静默失效）：{missing:?}"
    );
}

/// 例外清单本身要有保质期：里面的 tag 必须**真的**既随包又无代码消费点。
/// 哪天它被接上了消费点（或被删掉），这条会催着把例外一起撤了 —— 不撤就是永久后门。
#[test]
fn user_rules_only_list_stays_honest() {
    let bundled = bundled_tags();
    let consumed = consumed_geo_tags();
    for t in BUNDLED_FOR_USER_RULES_ONLY {
        assert!(
            bundled.contains(*t),
            "{t} 已不在随包表里，例外清单该删掉这一条"
        );
        assert!(
            !consumed.contains(*t),
            "{t} 已经有消费点了，例外清单该删掉这一条"
        );
    }
}
