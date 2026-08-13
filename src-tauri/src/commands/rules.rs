//! 路由规则类 command（上游 `rules-handlers.ts` + `rule-resource-handlers.ts`）。
//!
//! 映射 command（**Tauri command 名 = Rust 函数名**，冒号在标识符里不合法 → 前端 channel 常量值
//! 必须是 snake_case 函数名，不是 Electron 时代的 `rules:getAll`。event 名才是自由字符串、冒号合法）：
//! - `RULES_GET_ALL` → [`rules_get_all`]
//! - `RULES_ADD` → [`rules_add`]
//! - `RULES_UPDATE` → [`rules_update`]
//! - `RULES_DELETE` → [`rules_delete`]
//! - `RULES_REORDER` → [`rules_reorder`]
//! - `APP_PRESETS_LIST` → [`app_presets_list`]（内置应用分流预设表，Rust SoT 下发）
//! - `RULE_RESOURCES_*` → rule_resources_*（下载族**结构性阻塞**，见下方 §规则资源）
//!
//! 服务端兜底校验（assertValidRule）：validateRule 等价由 config-engine validate 提供。

#![allow(clippy::needless_pass_by_value)]

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, State};

use crate::commands::config::broadcast_config_changed;
use crate::events::{broadcast, channel::EVENT_RULE_RESOURCE_PROGRESS};
use crate::response::{ok_void, ApiResponse};
use crate::runtime::http::{app_user_agent, HttpRuntime, SystemDnsLookup};
use crate::runtime::subscription_scheduler::now_ms;
use crate::runtime::AppRuntime;
use polaris_config_engine::user_config::builtin_geo_rulesets::{
    builtin_geo_rulesets, builtin_id_for, find_builtin, is_bundled_geo_tag, is_valid_srs_bytes,
    is_valid_srs_file, BuiltinGeoRuleSet, GeoCategory,
};
use polaris_config_engine::user_config::rule::{Rule, RuleResource, RuleResourceFormat};
use polaris_config_engine::user_config::rule_resource_catalog::{find_catalog_item, mrd_raw_url};
use polaris_config_engine::user_config::rule_resource_refs::RuleResourceRef;
use polaris_config_engine::user_config::{
    all_presets_dto, builtin_catalog_result, enumerate_resource_refs, validate_rule, AppPresetDto,
    RefScanInput, RuleResourceCatalogItem, RuleResourceCatalogResult, UserConfig,
};
use polaris_net_stack::safe_redirect::{safe_redirect_fetch, HttpClient, SafeRedirectFetchOptions};
use polaris_net_stack::ssrf::DnsLookup;

/// 规则兜底校验失败错误码（上游 `assertValidRule`）。前端据此把「规则非法」与「保存失败」分流：
/// 前者提示用户改表单、不重试；后者是 IO/写盘失败、可重试。D4 提交门权威即 add/update 的本校验。
const ERR_RULE_INVALID: &str = "RULE_INVALID";

/// 服务端权威规则校验（上游 `validateRule`）：结构须能反序列化为 `Rule`，且每个条件有 ≥1 个
/// 非空值、全部值按类型合法。成功 `Ok(())`；失败返错误串（分号连接各条件错误），调用方包 `RULE_INVALID` 信封。
///
/// 单一真值在 config-engine `user_config::rule_validate::validate_rule`；前端 rule-dialog 只保留
/// `isValidIpCidr` 做输入内联提示，提交门以此为准。
fn validate_rule_payload(rule: &Value) -> Result<(), String> {
    let parsed: Rule =
        serde_json::from_value(rule.clone()).map_err(|e| format!("规则结构非法: {e}"))?;
    let result = validate_rule(&parsed);
    if result.valid {
        Ok(())
    } else {
        Err(result.errors.join("; "))
    }
}

/// 上游 `RULES_GET_ALL`：取全部自定义规则。
#[tauri::command]
pub fn rules_get_all(state: State<'_, AppRuntime>) -> ApiResponse<Vec<Value>> {
    match state.config().current() {
        Ok(cfg) => {
            let empty = Vec::new();
            let rules = cfg
                .get("customRules")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or(empty);
            ApiResponse::ok(rules)
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 上游 `RULES_ADD`：新增规则（服务端兜底校验 + 生成 id）。
#[tauri::command]
pub fn rules_add(app: AppHandle, state: State<'_, AppRuntime>, rule: Value) -> ApiResponse<Value> {
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    let mut new_rule = rule;
    let id = new_rule
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("rule_{}", new_uuid()));
    if let Some(obj) = new_rule.as_object_mut() {
        obj.insert("id".to_string(), json!(id));
    }
    // 提交门权威校验（Polaris assertValidRule）：非法规则不入盘。
    if let Err(msg) = validate_rule_payload(&new_rule) {
        return ApiResponse::err_with_code(msg, ERR_RULE_INVALID);
    }
    let created = new_rule.clone();
    if let Some(arr) = cfg.get_mut("customRules").and_then(Value::as_array_mut) {
        arr.push(new_rule);
    } else if let Some(obj) = cfg.as_object_mut() {
        obj.insert("customRules".to_string(), Value::Array(vec![new_rule]));
    }
    match state.config().save_full(&cfg) {
        Ok(()) => {
            broadcast_config_changed(&app, &cfg);
            ApiResponse::ok(created)
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 上游 `RULES_UPDATE`：更新规则（按 id）。
#[tauri::command]
pub fn rules_update(app: AppHandle, state: State<'_, AppRuntime>, rule: Value) -> ApiResponse<()> {
    let id = rule.get("id").and_then(Value::as_str).map(str::to_string);
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    let id = match id {
        Some(i) => i,
        None => return ApiResponse::err("rule.id required"),
    };
    // 提交门权威校验（Polaris assertValidRule）：非法规则不入盘。
    if let Err(msg) = validate_rule_payload(&rule) {
        return ApiResponse::err_with_code(msg, ERR_RULE_INVALID);
    }
    let found = cfg
        .get_mut("customRules")
        .and_then(Value::as_array_mut)
        .map(|arr| {
            if let Some(idx) = arr
                .iter()
                .position(|r| r.get("id").and_then(Value::as_str) == Some(&id))
            {
                arr[idx] = rule;
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    if !found {
        return ApiResponse::err(format!("Rule not found: {id}"));
    }
    match state.config().save_full(&cfg) {
        Ok(()) => {
            broadcast_config_changed(&app, &cfg);
            ok_void()
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 上游 `RULES_DELETE`：删除规则（按 id）。
#[tauri::command]
pub fn rules_delete(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    rule_id: String,
) -> ApiResponse<()> {
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    let found = cfg
        .get_mut("customRules")
        .and_then(Value::as_array_mut)
        .map(|arr| {
            if let Some(idx) = arr
                .iter()
                .position(|r| r.get("id").and_then(Value::as_str) == Some(&rule_id))
            {
                arr.remove(idx);
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    if !found {
        return ApiResponse::err(format!("Rule not found: {rule_id}"));
    }
    match state.config().save_full(&cfg) {
        Ok(()) => {
            broadcast_config_changed(&app, &cfg);
            ok_void()
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 重排纯决策：校验 `ordered_ids` 是现有 id 的严格排列，并算出新序列。
///
/// 三态返回（**净零序单独成一态**，契约 §Rules「规则重排」明写「净零序跳过 save」）：
/// - `Err(msg)`   → 入参非法（长度不符 / 有重复 / 含未知 id），调用方原样报错；
/// - `Ok(None)`   → **净零序**：请求的顺序与当前顺序逐位相同，无需落盘；
/// - `Ok(Some(v))`→ 真变化，`v` 是重排后的规则数组。
///
/// # 为什么净零序必须短路，而不是「反正 save 一次也没坏处」
///
/// `save_full` 之后跟着 `broadcast_config_changed` → 渲染端刷 store，且后端在 `config:changed`
/// 上挂着**整核评估**（待应用差集 / 是否需重启的判定）。规则顺序决定命中优先级，是参与配置生成的
/// 输入，所以这条评估链是真跑的。而 UI 侧「拖起来又放回原位」「上移列表首行 / 下移末行」这类
/// 空操作会照发一次 `rules:reorder` —— 净零序不短路的话，每个空手势都要付一轮全量评估 +
/// 一次全量 config 广播（前端整棵列表重渲染）。
///
/// 判据是**逐位序列相等**（不是集合相等）：集合恒相等（上面刚校验过是排列），只有位置才携带信息。
fn plan_reorder(rules: &[Value], ordered_ids: &[String]) -> Result<Option<Vec<Value>>, String> {
    // orderedIds 必须是现有 id 的严格排列（长度 + 无重复）。
    if ordered_ids.len() != rules.len() || {
        let mut s = ordered_ids.to_vec();
        s.sort_unstable();
        s.dedup();
        s.len() != ordered_ids.len()
    } {
        return Err("orderedIds must be a permutation of existing rule ids".to_string());
    }
    let by_id: std::collections::HashMap<&str, &Value> = rules
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str).map(|id| (id, r)))
        .collect();
    if !ordered_ids.iter().all(|id| by_id.contains_key(id.as_str())) {
        return Err("orderedIds contains unknown rule id".to_string());
    }
    // 净零序：逐位比对现序与请求序（现序里缺 id 的畸形条目 → 视作不等，走正常重排路径修复）。
    let unchanged = rules
        .iter()
        .zip(ordered_ids.iter())
        .all(|(cur, want)| cur.get("id").and_then(Value::as_str) == Some(want.as_str()));
    if unchanged {
        return Ok(None);
    }
    Ok(Some(
        ordered_ids
            .iter()
            .map(|id| (*by_id.get(id.as_str()).unwrap_or(&&Value::Null)).clone())
            .collect(),
    ))
}

/// 上游 `RULES_REORDER`：重排规则（orderedIds 必须是现有 id 的严格排列）。
///
/// **净零序不落盘不广播**（见 [`plan_reorder`]），仍返 `ok` —— 对调用方而言「顺序已是你要的样子」
/// 就是成功，报错会让前端把空手势当失败回滚。
#[tauri::command]
pub fn rules_reorder(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    ordered_ids: Vec<String>,
) -> ApiResponse<()> {
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    let rules = cfg
        .get("customRules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let reordered = match plan_reorder(&rules, &ordered_ids) {
        Err(msg) => return ApiResponse::err(msg),
        Ok(None) => return ok_void(), // 净零序：不 save、不广播
        Ok(Some(v)) => v,
    };
    if let Some(obj) = cfg.as_object_mut() {
        obj.insert("customRules".to_string(), Value::Array(reordered));
    }
    match state.config().save_full(&cfg) {
        Ok(()) => {
            broadcast_config_changed(&app, &cfg);
            ok_void()
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 上游 `APP_PRESETS_LIST`：内置应用分流预设表（16 条，含 UI 列）。
///
/// **Rust 是本表的单一真值**（`config-engine/user_config/app_rules_preset_data.rs`）。前端曾持有
/// 一份同构的 `APP_PRESETS`（TS 才是真源、Rust 是手抄投影），现已删除 → 前端启动时经本 command
/// 一次拉取入 store（常量表 KB 级，一次往返摊销为零）。
///
/// 无参、无 state：静态表，不读 config。自定义预设（`config.customAppPresets`）**不在此下发** ——
/// 它们是用户配置、随 `config:changed` 实时变，前端 store 里本就有；合并（内置 ∪ 自定义）是渲染层
/// 的列表组合（`mergeAppPresets`），若在此合并则本表一缓存就会与新增的自定义应用脱节。
#[tauri::command]
pub fn app_presets_list() -> ApiResponse<Vec<AppPresetDto>> {
    ApiResponse::ok(all_presets_dto())
}

// ── 规则资源（.srs 下载/管理）── 上游 `rule-resource-handlers.ts` ──
//
// **接线现状（诚实登记）**：本族现全部真接线，判据不再是「要不要网络」——传输层单点
// `src-tauri/src/runtime/http.rs`（`HttpRuntime`，reqwest+rustls）已落地并实现 net-stack 的
// [`HttpClient`] trait，故此前的「结构性阻塞（全仓无 HTTP 实现）」已解封。
//
//   ✅ 无网络：`rule_resources_list`（config.ruleResources，fileExists=fs stat + SRS 魔数、
//      referencedBy=config-engine `enumerate_resource_refs` 实算）、`rule_resources_get_catalog`
//      （内置精选表 Rust SoT，喂资源库弹窗的「内置」tab）。
//
//   ✅ `refresh_catalog`：**真拉** meta-rules-dat 全量清单（GitHub git-trees API 三跳 → 原子落缓存
//      `<userData>/rule-resource/catalog.json`），详见下方 §资源库清单。远端不可达时按
//      「缓存 → 内置」梯子回落，`source` 逐态自述来源，不谎报。
//
//   ✅ 需网络：`download` / `redownload` / `update_all` 经 `state.http()`（真 reqwest）+
//      net-stack [`safe_redirect_fetch`]（逐跳 SSRF guard + 体积闸 + 超时 + 手动重定向，与订阅拉取
//      **同一** guard，不重造）下载 → SRS 魔数/JSON 结构 sanity → 落 `<userData>/rule-resource/<fileName>`
//      → upsert config.ruleResources。**编排（SSRF/重定向/体积/UA）全部复用**，本文件只做「解析 item →
//      URL、写盘、登记 config、组装前端契约结果」。逐 item 独立容错：一个坏 URL 不拖垮整批。
//
//   ✅ `icon_galleries`：并发拉三个公开图库源（Qure + homarr + edc）各三镜像回退，合并图标（迁移自
//      上游 `RuleResourceManager.fetchIconGalleries`，homarr 是原型解锁徽标用的库、新增第三源）——同样经
//      [`safe_redirect_fetch`]（公网 CDN 放行，与订阅同路径）。进程级内存缓存 TTL 1h 避免每次开弹窗都拉网。
//      全失败返 `[]`（前端降级手动 URL）。

/// 下载 item 结构非法（既无 `catalogId` 也无 `url`，或 URL 协议不支持）。
const ERR_RESOURCE_BAD_ITEM: &str = "RULE_RESOURCE_BAD_ITEM";
/// 下载失败（网络/SSRF/非 2xx/内容 sanity 不过）。前端据此提示可重试。
const ERR_RESOURCE_DOWNLOAD_FAILED: &str = "RULE_RESOURCE_DOWNLOAD_FAILED";
/// 已下到字节但落盘失败（目录建不了/写盘 IO 错）。
const ERR_RESOURCE_WRITE_FAILED: &str = "RULE_RESOURCE_WRITE_FAILED";
/// redownload 指向的资源不在 config.ruleResources。
const ERR_RESOURCE_NOT_FOUND: &str = "RULE_RESOURCE_NOT_FOUND";
/// 用户在下载途中主动取消（`rule_resources_cancel`）。**不是故障**——前端据此报「已取消」
/// 而非「更新失败」（红行会让用户以为源挂了）。
const ERR_RESOURCE_CANCELLED: &str = "RULE_RESOURCE_CANCELLED";

/// 规则资源单次下载体积硬闸（16 MiB）：sing-box .srs / .json 规则集通常 < 数 MB，超此即拒防 OOM。
const RULE_RESOURCE_MAX_BYTES: usize = 16 * 1024 * 1024;
/// 规则资源下载超时（首字节 + 逐跳，ms）。规则集体积小，30s 足够。
const RULE_RESOURCE_TIMEOUT_MS: u64 = 30_000;

// ── gh-proxy 加速（受限网络下规则资源能不能下下来的分水岭）───────────────────────
//
// 规则资源的默认源是 `raw.githubusercontent.com`（catalog 的 `mrd_raw_url`），在受限网络下**直连必挂**。
// 此前本文件全仓零 `ghProxyPrefix` 引用：设置页那个「GitHub 加速」只有内核下载（`commands/updater.rs`
// → `runtime/http.rs` `CoreDownloader::candidates`）在消费，规则资源下载完全绕过它 —— 用户配了加速，
// 资源页照样一片红。（`runtime/rule_resource_scheduler.rs` 的模块注释甚至自称「下载走直连 / gh-proxy」，
// 与代码相反，同批已改。）

/// 可经 gh 镜像加速的 GitHub 域名表。
///
/// **与 `runtime/http.rs::GITHUB_ASSET_HOSTS` 的关系**：那张 2 域名表是 updater 专用的**release 资产**
/// 判定面（核下载只经 `github.com` / `objects.githubusercontent.com`），本表是 gh-proxy 的通用判定面
/// （5 域，与前端 `ui/src/domain/gh-proxy.ts` `GH_HOSTS` 同表 —— 两侧对「哪些地址值得加速」必须同口径，
/// 否则设置页说加速、后端不加速）。规则资源恰恰只走 `raw.githubusercontent.com`，**不在** updater 那张表里，
/// 所以不能直接复用它。
///
/// DESIGN-REVIEW(gh-proxy-single-source)：审计 §C9 裁决「5 域名表 + applyGhProxy」应落 net-stack 纯函数
/// 模块，由 http.rs 与本文件共同消费（同一待办亦登记在 `runtime/http.rs` 的 `GITHUB_ASSET_HOSTS` 文档上）。net-stack 不在本批改动面内，故本表
/// 暂落此处；模块落地后本表与 `is_github_asset` 一并改为调它。**拼接口径刻意与 `CoreDownloader::candidates`
/// 逐字一致**（`prefix.trim_end_matches('/')` + `/` + 完整原 URL），不另立一套。
const GH_PROXY_HOSTS: [&str; 5] = [
    "raw.githubusercontent.com",
    "github.com",
    "objects.githubusercontent.com",
    "gist.githubusercontent.com",
    "codeload.github.com",
];

/// 套 gh 加速前缀 → 镜像 URL；前缀为空 / 非 GitHub 域 / URL 不可解析 → `None`（= 不加速，原样直连）。
///
/// 返回 `Option` 而非「原样返回」：调用方要据「有没有变」决定失败后是否值得回退直连（同址重试无意义）。
fn apply_gh_proxy(prefix: &str, url: &str) -> Option<String> {
    let prefix = prefix.trim().trim_end_matches('/');
    if prefix.is_empty() {
        return None;
    }
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))?;
    if !GH_PROXY_HOSTS.contains(&host.as_str()) {
        return None;
    }
    Some(format!("{prefix}/{url}"))
}

/// 读用户配置的 gh 加速前缀；未配置 / 读不到 → 空串（= 不加速）。
///
/// 与 `commands/updater.rs::downloader` 同一取值路径（`config.ghProxyPrefix`）—— 同一个设置项必须
/// 只有一个读法，否则「设置页改了、某条下载腿没跟上」这类漂移就无从发现。
fn gh_proxy_prefix(state: &AppRuntime) -> String {
    state
        .config()
        .get_value("ghProxyPrefix")
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// 列表项。上游 `RuleResourceListItem`（`ui/src/shared/types/rules.ts:125`）
/// = `RuleResource` + `fileExists` + `referencedBy` + `builtin?`。
///
/// `#[serde(flatten)]` 复用 `RuleResource` 自己的 rename（sourceUrl/fileName/downloadedAt）——
/// 手抄一遍字段就是又一处会漂移的副本。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleResourceListItem {
    #[serde(flatten)]
    resource: RuleResource,
    /// **可用性**而非「inode 在不在」：binary(.srs) 走魔数校验，与 `route.rs` 生成配置时的
    /// `is_valid_srs_fn` **同一口径** —— 半写/损坏文件在那边会被跳过，这边就不该显示为「在」，
    /// 否则 UI 说「有」而代理说「没有」。
    file_exists: bool,
    /// 被**已启用**规则引用的条数（route + app 两类，config-engine 实算）。
    referenced_by: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    builtin: Option<bool>,
}

/// 资源文件可用性：binary 校 SRS 魔数（同 route.rs 口径）；source(.json) 仅判存在。
fn resource_file_usable(path: &std::path::Path, format: RuleResourceFormat) -> bool {
    match format {
        RuleResourceFormat::Binary => is_valid_srs_file(path),
        RuleResourceFormat::Source => path.is_file(),
    }
}

/// 上游 `RULE_RESOURCES_LIST`：已下载规则资源清单（**真接线，无网络依赖**）。
///
/// 三个字段都是实算，没有占位：
/// - 表体 = `config.ruleResources`（注：该键此前因漏 `#[serde(rename)]` **恒反序列化为空**，
///   同批已修，见 `config-engine/tests/user_config_key_contract.rs`）；
/// - `fileExists` = 对 `<userData>/rule-resource/<fileName>` 实地 stat + SRS 魔数校验；
/// - `referencedBy` = `enumerate_resource_refs` 实算（route 条件 + app 分流间接引用）。
///
/// **含 `builtin:*` 内置 geo 项**（TS 类型的 `builtin?: boolean` 那一类），排在用户资源之后。
///
/// 这一段曾被整体否决过，理由是「`sourceUrl` 划归运行时层且至今无人提供，列出来就得编值」+
/// 「每行会带一个必然报 `RULE_RESOURCE_NOT_FOUND` 的更新按钮」。两条都已消解：
/// 地址由 tag 推导（[`BuiltinGeoRuleSet::source_url`]，非编造），更新腿是
/// [`rule_resources_update_builtin`]（本批落地）。
///
/// 内置行的三个字段与用户资源取自不同真值源，都不编：
/// - `fileExists` / `size` → **运行时生效目录** `<userData>/rules/<fileName>` 实地 stat + SRS 魔数
///   （不是下载缓存 `rule-resource/`：内置项从不落那儿）；
/// - `downloadedAt` → `config.builtinGeoMeta[tag].updatedAt`，**从未网络更新过就是空串**
///   （出厂态没有「下载时间」这回事，给个假时间比留空更坏）；
/// - `referencedBy` → 与用户资源同一个 `enumerate_resource_refs`，按 `builtin:<tag>` 这个 id 实算。
#[tauri::command]
pub fn rule_resources_list(state: State<'_, AppRuntime>) -> ApiResponse<Vec<RuleResourceListItem>> {
    let cfg = match state.config().current() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    let geo_meta = cfg.get("builtinGeoMeta").cloned().unwrap_or(Value::Null);
    let uc: UserConfig = match serde_json::from_value(cfg) {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("config 解析失败: {e}")),
    };
    let res_dir = state.config().dir().join("rule-resource");
    let runtime_dir = builtin_runtime_dir(&state);
    let scan = RefScanInput {
        custom_rules: &uc.custom_rules,
        app_rules: &uc.app_rules,
        custom_app_presets: &uc.custom_app_presets,
    };
    let mut items: Vec<RuleResourceListItem> = uc
        .rule_resources
        .iter()
        .map(|r| RuleResourceListItem {
            file_exists: resource_file_usable(&res_dir.join(&r.file_name), r.format),
            referenced_by: enumerate_resource_refs(&r.id, &scan).len(),
            builtin: None,
            resource: r.clone(),
        })
        .collect();
    items.extend(builtin_geo_rulesets().iter().map(|b| {
        let live = runtime_dir.join(&b.file_name);
        let id = builtin_id_for(&b.tag);
        RuleResourceListItem {
            file_exists: is_valid_srs_file(&live),
            referenced_by: enumerate_resource_refs(&id, &scan).len(),
            builtin: Some(true),
            resource: RuleResource {
                name: b.tag.clone(),
                category: match b.category {
                    GeoCategory::Geosite => "geosite".into(),
                    GeoCategory::Geoip => "geoip".into(),
                },
                source_url: b.source_url(),
                file_name: b.file_name.clone(),
                format: RuleResourceFormat::Binary,
                size: std::fs::metadata(&live).map(|m| m.len()).unwrap_or(0),
                downloaded_at: builtin_updated_at(&geo_meta, &b.tag),
                id,
            },
        }
    }));
    ApiResponse::ok(items)
}

/// 取 `builtinGeoMeta[tag].updatedAt`。缺失/类型不对 → 空串（= 出厂态，从未网络更新）。
fn builtin_updated_at(geo_meta: &Value, tag: &str) -> String {
    geo_meta
        .get(tag)
        .and_then(|v| v.get("updatedAt"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

// ── 资源库清单：远程全量刷新 + 磁盘缓存 ──────────────────────────────────────────
//
// 迁移自 上游 `RuleResourceManager`（`src/main/services/RuleResourceManager.ts`）：
// `fetchCatalogFromGithub`（`:712-753`）/ `getCatalog`（`:632-649`）/ `refreshCatalog`（`:651-671`）。
//
// **清单来源（照搬 上游，未自定）**：GitHub git-trees API 三跳 ——
//   ① `.../git/trees/sing` 取根树，找 `geo` / `geo-lite` 两个子树的 sha（上游 `:713-719`）；
//   ② `.../git/trees/<sha>?recursive=1` 并发各拉一次（上游 `:721-728`）；
//   ③ 收 `type=="blob"` 且相对路径形如 `geosite|geoip/<name>.srs` 的叶子（上游 `:736-749`）。
// **不是**某个上游「索引文件」——meta-rules-dat 没有这种文件，上游 也是这么枚举的。
//
// **派生口径与内置精选表逐字同构**（`config-engine/user_config/rule_resource_catalog.rs`
// `catalog_item()`）：id = `<category>-<name>`、path = `<geo|geo-lite>/<kind>/<name>.srs`。同构是硬要求
// 而非巧合 —— 同一条目在「内置」与「远程」两态下若算出两个 id/path，下载 URL 与落盘名就会分叉，
// 「已下载」判重（前端按 id 比对）也会失灵。`tree_path_matches_builtin_derivation` 把这条钉死。
//
// **失败语义**：上游的 `refreshCatalog` 抛错让 UI toast；Polaris 保留本 command 既有的「诚实降级」
// 契约（不 err），改为按同一梯子回落 —— 远程 → 缓存 → 内置，`source` 逐态如实自述。落到内置时与改动前
// 逐字一致（`source:"builtin"` + `fetchedAt:null`）。用户可见结果与 上游 等价：上游 抛错后 UI 继续
// 显示 `getCatalog()` 已加载的那份（即缓存），Polaris 直接把那份返回。

/// meta-rules-dat git-trees API 基址（= 上游 `:714/:723/:726` 同一串）。
const MRD_TREE_API_BASE: &str = "https://api.github.com/repos/MetaCubeX/meta-rules-dat/git/trees/";
/// 根树 ref（`sing` 分支）。
const MRD_CATALOG_REF: &str = "sing";
/// 清单 JSON 单次拉取超时（ms）。= 上游 `fetchJson` 的 20s。
const CATALOG_TIMEOUT_MS: u64 = 20_000;
/// 清单 JSON 体积上限（16 MiB）。= 上游 `MAX_GITHUB_JSON_BYTES`：两个 `?recursive=1` 并发各持一份，
/// 被劫持/WAF 回灌 GB 级 JSON 会直接 OOM，实际 tree 只有数 MB。
const CATALOG_MAX_BYTES: usize = 16 * 1024 * 1024;
/// 清单最小条目数（= 上游 `if (items.length < 50) throw new Error('catalog too small')`）。
/// 远端结构变了（目录改名 / 返回错误页）会让 collect 收到寥寥几条，此闸挡住「用半份清单覆盖好缓存」。
const CATALOG_MIN_ITEMS: usize = 50;
/// 磁盘缓存文件名（= 上游 `catalogCachePath()` 的 `catalog.json`），落在 `<userData>/rule-resource/`。
const CATALOG_CACHE_FILE: &str = "catalog.json";
/// 缓存 schema 版本（= 上游 `schemaVersion: 1`）。对不上 → 整份作废，不做迁移。
const CATALOG_CACHE_SCHEMA_VERSION: u64 = 1;

/// 单条 git-trees JSON 拉取（复用订阅同款 [`safe_redirect_fetch`]：逐跳 SSRF guard、体积闸、超时、
/// 手动重定向）。403/429 单独成句：GitHub 未鉴权限流 60 次/小时，是本功能最常见的失败因，与「网络
/// 不通」混成一条会让排障方向全错（= 上游 `fetchJson` 对 403/429 的特判）。
async fn fetch_catalog_json_once<H: HttpClient, L: DnsLookup>(
    client: &H,
    lookup: &L,
    url: &str,
) -> Result<Value, String> {
    let resp = safe_redirect_fetch(SafeRedirectFetchOptions {
        fetch_impl: client,
        url,
        user_agent: app_user_agent(),
        // GitHub 推荐的 API 版本头（= 上游 `req.setHeader('Accept', 'application/vnd.github+json')`）。
        headers: Some(vec![(
            "Accept".to_string(),
            "application/vnd.github+json".to_string(),
        )]),
        exempt_fake_ip: false,
        max_redirects: None,
        timeout_ms: Some(CATALOG_TIMEOUT_MS),
        max_body_bytes: Some(CATALOG_MAX_BYTES),
        lookup,
    })
    .await
    .map_err(|e| e.message)?;
    if matches!(resp.status, 403 | 429) {
        return Err(format!("GitHub 限流（HTTP {}），请稍后再试", resp.status));
    }
    if !(200..300).contains(&resp.status) {
        return Err(format!("清单拉取失败：HTTP {}", resp.status));
    }
    let v: Value =
        serde_json::from_slice(&resp.body).map_err(|e| format!("清单 JSON 非法: {e}"))?;
    // **结构闸**：三跳全是 git-trees API，响应必含 `tree` 数组。缺它 = 这不是一份 trees 响应
    // （gh-proxy 类镜像返 `{"code":403,"msg":"..."}`、WAF 挑战页的 JSON 变体、被换成别的 API 响应…）。
    //
    // 少了这道闸不是「多一次解析失败」而是**回退腿失效**：[`fetch_catalog_json`] 只在镜像腿返
    // `Err` 时才改打原址，镜像返「200 + 合法 JSON + 不是 tree」会被当成成功、直接把这份垃圾交上去 →
    // `tree_child_sha` 找不到 geo → 整次刷新失败 → 落到缓存/内置，而**原址压根没被试过**。
    if v.get("tree").and_then(Value::as_array).is_none() {
        return Err("清单响应不是 git-trees 结构（缺 `tree` 数组）".to_string());
    }
    Ok(v)
}

/// 带 gh 加速的清单拉取：**复用下载腿那一套**（[`apply_gh_proxy`] + 失败回退原址），不另立一份判定。
///
/// 现状诚实登记：`GH_PROXY_HOSTS` 是 5 域表、**不含 `api.github.com`**，故对本函数实际请求的 trees API
/// 地址而言这是一次**文档化的空转**（`ui/src/domain/gh-proxy.ts:56` 与 上游 `shared/gh-proxy.ts:58`
/// 都明写「api.github.com 不在 GH_HOSTS，Trees API 刷新不走加速」——gh-proxy 类镜像普遍只代理
/// raw/releases/archive，不代理 API）。仍然走这条腿而不是直接裸调，是为了**只有一个加速决策点**：
/// 哪天那张表补上 `api.github.com`（前后端两侧同步改），清单刷新自动吃上，不需要再改本文件。
async fn fetch_catalog_json<H: HttpClient, L: DnsLookup>(
    client: &H,
    lookup: &L,
    url: &str,
    gh_prefix: &str,
) -> Result<Value, String> {
    if let Some(mirrored) = apply_gh_proxy(gh_prefix, url) {
        match fetch_catalog_json_once(client, lookup, &mirrored).await {
            Ok(v) => return Ok(v),
            // **必须留痕**：镜像腿失败后静默回退直连，用户配的「GitHub 加速」就成了形同虚设的开关
            // ——尤其是自建**内网** gh-proxy（`192.168.x` / `10.x`）会被 `safe_redirect_fetch` 的
            // SSRF guard **恒拒**（放行内网 host 会开出 SSRF 面，故刻意不放行），于是加速腿每次都挂、
            // 每次都静默回退，设置页却一句提示都没有。至少让日志能回答「我配的加速到底走没走」。
            Err(e) => log::warn!("gh 加速腿失败，回退原址（清单）: {mirrored} → {e}"),
        }
    }
    fetch_catalog_json_once(client, lookup, url).await
}

/// git object sha 合法性：**恰好** 40（sha1）或 64（sha256）位 hex。
///
/// **不是洁癖**：这个值来自远端 JSON 且**直接拼进下一跳 URL 的路径段**。不校验则 `"../../../x"` 之类
/// 能把请求带去同域的任意 API 路径（`safe_redirect_fetch` 只管 SSRF/重定向，管不了路径语义）。
///
/// 长度收敛为「两个合法值」而非区间 `40..=64`：git 的 object id 只有这两种长度，中间那 23 种长度
/// 全是「不可能是 sha 的东西」（缩写 ref、被截断的串、构造出来的探测值）。放行它们没有任何合法用例，
/// 只是白白留出 23 种可拼进 URL 的形态。
fn is_valid_tree_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 从根树 JSON 取指定子目录的 sha（= 上游 `tree.find((t) => t.path === 'geo')?.sha`），sha 非法 → None。
fn tree_child_sha(root: &Value, name: &str) -> Option<String> {
    let sha = root
        .get("tree")?
        .as_array()?
        .iter()
        .find(|n| n.get("path").and_then(Value::as_str) == Some(name))?
        .get("sha")?
        .as_str()?;
    is_valid_tree_sha(sha).then(|| sha.to_string())
}

/// `(base, 子树内相对路径)` → catalog 条目；不合规 → `None`。**catalog 条目的唯一构造口径**
/// （远程 collect 与缓存回读共用它 → 两条路径不可能派生出不同的 id/path）。
///
/// 与内置表 `catalog_item()` 同构：`category` = `geosite|geoip` 或加 `-lite`，`id` = `<category>-<name>`，
/// `path` = `<base>/<kind>/<name>.srs`。
///
/// 拒收面（每条都有其对应故障）：
/// - 非 `.srs` / 非 `geosite|geoip` 前缀 → 不是规则集叶子；
/// - `name` 含 `/`（嵌套子目录）→ 落盘名会被当成子目录路径 → `ENOENT`（上游 `:742` 同款跳过）；
/// - `name` 以 `.` 开头（`.` / `..`）或含控制字符 / `? # \ %` → 拼进下载 URL 后会改变路径语义或被
///   百分号解码，属远端可控输入的注入面。
fn catalog_item_from_tree_path(base: &str, rel: &str) -> Option<RuleResourceCatalogItem> {
    if base != "geo" && base != "geo-lite" {
        return None;
    }
    let stem = rel.strip_suffix(".srs")?;
    let (kind, name) = stem.split_once('/')?;
    if !matches!(kind, "geosite" | "geoip") {
        return None;
    }
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return None;
    }
    if name.contains(|c: char| c.is_control() || matches!(c, '?' | '#' | '\\' | '%')) {
        return None;
    }
    let category = if base == "geo-lite" {
        format!("{kind}-lite")
    } else {
        kind.to_string()
    };
    let id = format!("{category}-{name}");
    Some(RuleResourceCatalogItem {
        // 随包判定与内置精选表同一口径（`catalog_item()` 也是这句）：远程全量里若出现随包同名项，
        // 外置 tab 也该显示「已内置」，否则同一条资源在两个 tab 里说法不一。
        bundled: is_bundled_geo_tag(&id),
        id,
        category,
        name: name.to_string(),
        path: format!("{base}/{rel}"),
    })
}

/// 从一棵 `?recursive=1` 子树 JSON 收条目（= 上游 `collect`）。整棵树结构不对 → 收 0 条（由
/// [`CATALOG_MIN_ITEMS`] 闸兜住），不 panic。
fn collect_catalog_items(tree: &Value, base: &str, out: &mut Vec<RuleResourceCatalogItem>) {
    let Some(nodes) = tree.get("tree").and_then(Value::as_array) else {
        return;
    };
    for node in nodes {
        if node.get("type").and_then(Value::as_str) != Some("blob") {
            continue;
        }
        let Some(rel) = node.get("path").and_then(Value::as_str) else {
            continue;
        };
        if let Some(item) = catalog_item_from_tree_path(base, rel) {
            out.push(item);
        }
    }
}

/// 拉远程全量清单（= 上游 `fetchCatalogFromGithub` + `refreshCatalog` 的 `< 50` 闸）。
async fn fetch_catalog_from_github<H: HttpClient, L: DnsLookup>(
    client: &H,
    lookup: &L,
    gh_prefix: &str,
) -> Result<Vec<RuleResourceCatalogItem>, String> {
    let root_url = format!("{MRD_TREE_API_BASE}{MRD_CATALOG_REF}");
    let root = fetch_catalog_json(client, lookup, &root_url, gh_prefix).await?;
    let geo_sha =
        tree_child_sha(&root, "geo").ok_or_else(|| "根树缺 geo 子树或 sha 非法".to_string())?;
    let lite_sha = tree_child_sha(&root, "geo-lite")
        .ok_or_else(|| "根树缺 geo-lite 子树或 sha 非法".to_string())?;

    let geo_url = format!("{MRD_TREE_API_BASE}{geo_sha}?recursive=1");
    let lite_url = format!("{MRD_TREE_API_BASE}{lite_sha}?recursive=1");
    let (geo, lite) = tokio::join!(
        fetch_catalog_json(client, lookup, &geo_url, gh_prefix),
        fetch_catalog_json(client, lookup, &lite_url, gh_prefix),
    );
    let (geo, lite) = (geo?, lite?);
    // GitHub 对超大树会截断并置 `truncated:true`——半份清单当全量用会让本地清单凭空少掉一批条目，
    // 且会覆盖掉上一份完整缓存。宁可本次失败（= 上游 `throw new Error('tree truncated')`）。
    if geo.get("truncated").and_then(Value::as_bool) == Some(true)
        || lite.get("truncated").and_then(Value::as_bool) == Some(true)
    {
        return Err("清单被 GitHub 截断（truncated），本次不采信".to_string());
    }

    let mut items = Vec::new();
    collect_catalog_items(&geo, "geo", &mut items);
    collect_catalog_items(&lite, "geo-lite", &mut items);
    // id 去重（保留首现）：远端同名 blob 重复出现会让下载计划歧义（同 id 两个 path）。
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert(i.id.clone()));
    if items.len() < CATALOG_MIN_ITEMS {
        return Err(format!(
            "清单条目过少（{} < {CATALOG_MIN_ITEMS}），疑似上游结构变化",
            items.len()
        ));
    }
    Ok(items)
}

/// 缓存文件路径。
fn catalog_cache_path(res_dir: &Path) -> std::path::PathBuf {
    res_dir.join(CATALOG_CACHE_FILE)
}

/// 回读缓存条目并**验自洽**：`id`/`category`/`name` 必须与 `path` 按 [`catalog_item_from_tree_path`]
/// 派生结果逐字相等。手改 / 半写 / 旧格式的条目一律判废 —— 否则一条 `{"id":"x","path":"../../y"}`
/// 就能借缓存绕过远程 collect 的全部拒收面。
///
/// 返回的是**派生结果本身**而非缓存里的那份，故 `bundled` 恒按当前版本的随包表现算：升级后随包
/// 清单变了，旧缓存里那个 `bundled` 不会把过期结论带进 UI（缓存里的该字段读都不读）。
fn parse_cached_catalog_item(v: &Value) -> Option<RuleResourceCatalogItem> {
    let path = v.get("path").and_then(Value::as_str)?;
    let (base, rel) = path.split_once('/')?;
    let derived = catalog_item_from_tree_path(base, rel)?;
    (v.get("id").and_then(Value::as_str) == Some(derived.id.as_str())
        && v.get("category").and_then(Value::as_str) == Some(derived.category.as_str())
        && v.get("name").and_then(Value::as_str) == Some(derived.name.as_str()))
    .then_some(derived)
}

/// 读磁盘缓存 → `(items, fetchedAt)`；**任一环不过即整份作废**（返 None → 上层回落内置）。
///
/// 校验链（= 上游 `getCatalog` 的 `schemaVersion === 1 && Array.isArray(items) && length >= 50`
/// 再加逐条自洽）：文件可读 → JSON 合法 → schemaVersion 对上 → fetchedAt 为正整数 → items 是数组
/// → 每条自洽 → 条数够。**畸形不污染**：读不出来只是少一层兜底，绝不把半份清单当真。
fn read_catalog_cache(res_dir: &Path) -> Option<(Vec<RuleResourceCatalogItem>, i64)> {
    let raw = std::fs::read(catalog_cache_path(res_dir)).ok()?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    if v.get("schemaVersion").and_then(Value::as_u64) != Some(CATALOG_CACHE_SCHEMA_VERSION) {
        return None;
    }
    let fetched_at = v
        .get("fetchedAt")
        .and_then(Value::as_i64)
        .filter(|t| *t > 0)?;
    let arr = v.get("items")?.as_array()?;
    let mut items = Vec::with_capacity(arr.len());
    for entry in arr {
        items.push(parse_cached_catalog_item(entry)?);
    }
    (items.len() >= CATALOG_MIN_ITEMS).then_some((items, fetched_at))
}

/// 原子写缓存（唯一后缀 tmp → rename）。
///
/// 唯一后缀（pid + 单调序）而非固定名：本 command 无 inflight 保护、IPC 层无去抖，多窗口/后台调度腿
/// 可并发写同一目录；固定名 tmp 会字节交错（= 上游 `writeFileAtomic` 同款理由，`:657-659`）。
fn write_catalog_cache(
    res_dir: &Path,
    fetched_at: i64,
    items: &[RuleResourceCatalogItem],
) -> Result<(), String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::fs::create_dir_all(res_dir).map_err(|e| format!("建目录失败: {e}"))?;
    let body = serde_json::to_vec(&json!({
        "schemaVersion": CATALOG_CACHE_SCHEMA_VERSION,
        "fetchedAt": fetched_at,
        "items": items,
    }))
    .map_err(|e| format!("序列化失败: {e}"))?;
    let tmp = res_dir.join(format!(
        "{CATALOG_CACHE_FILE}.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&tmp, &body).map_err(|e| format!("写入失败: {e}"))?;
    std::fs::rename(&tmp, catalog_cache_path(res_dir)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("提交失败: {e}")
    })
}

/// 盘上缓存 → `source:"cache"` 结果；没有可用缓存 → `None`（**不回落内置**，见
/// [`rule_resources_get_cached_catalog`] 里为何这个区别是必要的）。
fn cached_catalog(res_dir: &Path) -> Option<RuleResourceCatalogResult> {
    read_catalog_cache(res_dir).map(|(items, fetched_at)| RuleResourceCatalogResult {
        items,
        fetched_at: Some(fetched_at),
        source: "cache".to_string(),
    })
}

/// 无远程时的诚实回落梯子：缓存（`source:"cache"` + 真 `fetchedAt`）→ 内置
/// （`source:"builtin"` + `fetchedAt:null`）。**任何一态都不谎报**。
fn cached_or_builtin_catalog(res_dir: &Path) -> RuleResourceCatalogResult {
    cached_catalog(res_dir).unwrap_or_else(builtin_catalog_result)
}

/// 刷新的**可测核**（command 是它的薄壳）：远程成功 → 落缓存 + `source:"remote"`；远程失败 → 回落
/// [`cached_or_builtin_catalog`]。抽出来不是为了好看 —— command 带 `State<AppRuntime>`，本仓未引
/// `tauri::test`，不抽就没有任何一条断言能证明「远程腿真的跑了」（那正是本功能此前恒等降级的成因）。
async fn refresh_catalog_core<H: HttpClient, L: DnsLookup>(
    client: &H,
    lookup: &L,
    res_dir: &Path,
    gh_prefix: &str,
) -> RuleResourceCatalogResult {
    match fetch_catalog_from_github(client, lookup, gh_prefix).await {
        Ok(items) => {
            let fetched_at = i64::try_from(now_ms()).unwrap_or(i64::MAX);
            // 缓存写失败不改本次结果（全量已在手），只是下次少一层兜底 —— 不该让「盘满」把一次成功
            // 的刷新打成失败。
            let _ = write_catalog_cache(res_dir, fetched_at, &items);
            RuleResourceCatalogResult {
                items,
                fetched_at: Some(fetched_at),
                source: "remote".to_string(),
            }
        }
        // 失败原因不进 DTO（契约里没有 error 字段），但**结果如实标 source**，前端据此提示。
        Err(_) => cached_or_builtin_catalog(res_dir),
    }
}

/// 上游 `RULE_RESOURCES_GET_CATALOG`：资源库目录（**真接线**，Rust SoT 内置精选表）。
///
/// 前端曾持有同一张表（`RULE_RESOURCE_CATALOG`），已删 → 此处是唯一真值。
/// `source:"builtin"` + `fetchedAt:null` 如实自述「这是离线内置清单，不是远端全量」。
///
/// **刻意不读远程缓存**（与 上游 `getCatalog` 的差异，非遗漏）：Polaris 的资源库弹窗用本 command
/// 驱动「内置」tab（= 随包表的投影，28 条），上游 那边「内置」tab 另有数据源（随包 geo `builtinItems` prop），
/// `getCatalog` 只喂它的「外置」tab。若照抄让本 command 返回缓存，Polaris 的「内置」tab 会变成
/// 2000+ 条远程全量 —— tab 语义当场崩掉。远程/缓存两态由
/// [`rule_resources_refresh_catalog`] 单独承担（外置 tab）。
#[tauri::command]
pub fn rule_resources_get_catalog() -> ApiResponse<RuleResourceCatalogResult> {
    ApiResponse::ok(builtin_catalog_result())
}

/// 上游 `RULE_RESOURCES_REFRESH_CATALOG`：刷新资源库目录 —— **真拉 meta-rules-dat 全量清单**。
///
/// 三跳 git-trees API（见本节头注）→ 收 `.srs` 叶子 → `<50` 条闸 → 原子落缓存 → `source:"remote"`。
/// 远程失败按梯子回落：缓存（`source:"cache"`）→ 内置（`source:"builtin"` + `fetchedAt:null`）。
///
/// 刻意**不 err**（与 redownload/updateAll 不同）：本 command 的契约允许「回退到已有清单」这个成功
/// 语义，前端据 `source` 提示来源即可；而 redownload 没有等价的降级语义 —— 它要么下到文件要么没下到。
/// **回落不是伪造**：三个 `source` 值各自对应一份真实存在的清单，没有任何一态谎称「拉到了远端」。
#[tauri::command]
pub async fn rule_resources_refresh_catalog(
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<RuleResourceCatalogResult>, ()> {
    let res_dir = state.config().dir().join("rule-resource");
    let http = state.http().clone();
    let gh_prefix = gh_proxy_prefix(&state);
    Ok(ApiResponse::ok(
        refresh_catalog_core(http.as_ref(), &SystemDnsLookup, &res_dir, &gh_prefix).await,
    ))
}

/// 只读盘上清单缓存（**零出站**），没有缓存返 `null`。
///
/// 补的是「缓存写了却没人读」这个缺口：[`rule_resources_refresh_catalog`] 从一开始就把全量清单
/// 原子落盘了，但它只在**远程失败时**回读，于是外置 tab 每次打开都是空的、非点「刷新清单」不可
/// —— 缓存明明在盘上，用户仍要为每次打开付一次三跳 git-trees 往返（真机反馈「刷新清单后应该要有
/// 缓存，而不是每次都需要手动刷新」）。本 command 让 UI 打开即回读那份缓存，刷新退回成显式动作。
///
/// **不复用另外两个 command 的理由**（不是没试）：
/// - [`rule_resources_get_catalog`] 刻意不读缓存 —— 它驱动「内置」tab（随包表投影，28 条），让它返回
///   2000+ 条全量会当场毁掉 tab 语义（该函数注释已记）；
/// - [`rule_resources_refresh_catalog`] 必然先打网络，正是要避免的那次往返。
///
/// **无缓存返 `null` 而非回落内置**：`cached_or_builtin_catalog` 的 `builtin` 那一档语义是「远程
/// 拉过且失败了」，前端据此显示「远程获取失败 · 回落内置精选清单」。本 command 一次网都没打，
/// 借那条路会让 UI 报一个**没发生过的失败**。没有就是没有，由前端继续显示「点击刷新清单」。
///
/// 参数只有 `res_dir`（由 state 推出），签名里没有任何 HTTP client —— 「本 command 不出站」是
/// 结构性的，不靠自觉。
#[tauri::command]
pub fn rule_resources_get_cached_catalog(
    state: State<'_, AppRuntime>,
) -> ApiResponse<Option<RuleResourceCatalogResult>> {
    let res_dir = state.config().dir().join("rule-resource");
    ApiResponse::ok(cached_catalog(&res_dir))
}

/// 上游 `RULE_RESOURCES_REDOWNLOAD`：按 id 重新下载已登记资源（**真下载**）。
///
/// 从 `config.ruleResources` 取该资源的 `sourceUrl`/`format`/`fileName`（保留原 id，覆盖写盘），
/// 返回单个 `RuleResourceDownloadResult`（前端契约 `redownload(id): Promise<RuleResourceDownloadResult>`）。
/// 资源不在册 → `ok:false` + `RULE_RESOURCE_NOT_FOUND`（业务态在 data，信封仍 success）。
#[tauri::command]
pub async fn rule_resources_redownload(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    id: String,
) -> Result<ApiResponse<Value>, ()> {
    redownload_with_mode(
        &app,
        &state,
        id,
        ProgressMode::Live,
        BroadcastMode::Immediate,
    )
    .await
}

/// 后台调度腿专用入口：与 [`rule_resources_redownload`] 同一条下载/落盘/入册路径，两处差别是
/// **一帧进度都不发**（`ProgressMode::Silent`）+ **不逐条广播**（`BroadcastMode::Deferred`，
/// 由批次拥有者 `RuleResourceScheduler::run_due_updates` 收尾统一广播一次）。
///
/// 为什么另开一个函数而不是给命令加个 `silent: bool` 形参：`#[tauri::command]` 的形参会成为
/// 前端可传的参数袋键——渲染端就能把自己的手动更新也调成静默（或反之），静默语义不再由后端说了算。
/// 独立函数 + 内部写死这两个模式，使「后台腿推事件」「后台腿逐条重启核」在类型层面不可表达。
pub async fn rule_resources_redownload_silent(
    app: &AppHandle,
    state: &AppRuntime,
    id: String,
) -> Result<ApiResponse<Value>, ()> {
    redownload_with_mode(
        app,
        state,
        id,
        ProgressMode::Silent,
        BroadcastMode::Deferred,
    )
    .await
}

/// redownload 的共用核心（手动腿 / 后台腿只差 [`ProgressMode`] + [`BroadcastMode`]）。
async fn redownload_with_mode(
    app: &AppHandle,
    state: &AppRuntime,
    id: String,
    mode: ProgressMode,
    broadcast: BroadcastMode,
) -> Result<ApiResponse<Value>, ()> {
    let cfg = match state.config().current() {
        Ok(c) => c,
        Err(e) => return Ok(ApiResponse::err(format!("{e}"))),
    };
    // 先按 id 找原始项再反序列化：不在册 → NOT_FOUND；在册但 malformed → BAD_ITEM（P8：不再误报 NOT_FOUND）。
    let empty = Vec::new();
    let resources = cfg
        .get("ruleResources")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let existing = match resolve_registered_resource(resources, &id) {
        Ok(r) => r,
        Err(err_value) => return Ok(ApiResponse::ok(err_value)),
    };
    let plan = plan_from_resource(&existing).with_gh_proxy(&gh_proxy_prefix(state));
    let res_dir = state.config().dir().join("rule-resource");
    let http = state.http().clone();
    let sink = BroadcastSink { app, mode };
    let result =
        download_with_progress(&sink, http.as_ref(), &SystemDnsLookup, &plan, &res_dir).await;
    if let DownloadOutcome::Stored { ref resource, .. } = result {
        persist_resources(app, state, std::slice::from_ref(resource), broadcast);
    }
    Ok(ApiResponse::ok(result.into_value(&plan)))
}

/// 上游 `RULE_RESOURCES_UPDATE_ALL`：更新全部已登记资源（**真下载**）。
///
/// 逐个 redownload `config.ruleResources` 里的每一项，返回 `RuleResourceDownloadResult[]`（数组，
/// 对齐前端 `.map()` 契约）。逐 item 独立容错；成功项一次性 upsert + 保存 + 广播。
#[tauri::command]
pub async fn rule_resources_update_all(
    app: AppHandle,
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<Vec<Value>>, ()> {
    let cfg = match state.config().current() {
        Ok(c) => c,
        Err(e) => return Ok(ApiResponse::err(format!("{e}"))),
    };
    let raw_entries: Vec<Value> = cfg
        .get("ruleResources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let res_dir = state.config().dir().join("rule-resource");
    let http = state.http().clone();
    let gh_prefix = gh_proxy_prefix(&state);

    let mut results: Vec<Value> = Vec::with_capacity(raw_entries.len());
    let mut stored: Vec<RuleResource> = Vec::new();
    for entry in &raw_entries {
        // 结构非法的条目**如实报失败**（P8）——旧实现 `filter_map(.ok())` 静默丢弃：既不更新也不出现在结果里。
        let existing = match parse_resource_entry(entry) {
            Ok(r) => r,
            Err(err_value) => {
                results.push(err_value);
                continue;
            }
        };
        let plan = plan_from_resource(&existing).with_gh_proxy(&gh_prefix);
        let sink = BroadcastSink {
            app: &app,
            mode: ProgressMode::Live,
        };
        let outcome =
            download_with_progress(&sink, http.as_ref(), &SystemDnsLookup, &plan, &res_dir).await;
        if let DownloadOutcome::Stored { ref resource, .. } = outcome {
            stored.push(resource.clone());
        }
        results.push(outcome.into_value(&plan));
    }
    if !stored.is_empty() {
        persist_resources(&app, &state, &stored, BroadcastMode::Immediate);
    }
    Ok(ApiResponse::ok(results))
}

// ── 在线图标库（icon_galleries）── 迁移自 上游 `RuleResourceManager.fetchIconGalleries` ──
//
// 并发拉三个公开图库源（Qure + homarr + edc），各三镜像（jsdelivr → fastly → github raw）逐个回退，合并图标。
// 复用订阅同款 [`safe_redirect_fetch`]（逐跳 SSRF guard + 体积闸 + 超时 + 手动重定向），不重造 HTTP 客户端。

/// 图标库条目（前端契约 `{name,url}`，见 `api-client.ts` `fetchIconGalleries`）。
#[derive(serde::Serialize, Clone)]
pub struct IconGalleryItem {
    pub name: String,
    pub url: String,
}

/// Qure（Koolson）图库镜像：jsdelivr → fastly → github raw，逐个兜底（= 上游 同三址）。
const QURE_ICON_MIRRORS: &[&str] = &[
    "https://cdn.jsdelivr.net/gh/Koolson/Qure/Other/QureColor-All.json",
    "https://fastly.jsdelivr.net/gh/Koolson/Qure/Other/QureColor-All.json",
    "https://raw.githubusercontent.com/Koolson/Qure/master/Other/QureColor-All.json",
];

/// edc（erdongchanyo）图库镜像：jsdelivr → fastly → github raw，逐个兜底（= 上游 同三址）。
const EDC_ICON_MIRRORS: &[&str] = &[
    "https://cdn.jsdelivr.net/gh/erdongchanyo/icon@main/edc-filter-icon-gallery.json",
    "https://fastly.jsdelivr.net/gh/erdongchanyo/icon@main/edc-filter-icon-gallery.json",
    "https://raw.githubusercontent.com/erdongchanyo/icon/main/edc-filter-icon-gallery.json",
];

/// homarr（homarr-labs/dashboard-icons）图库清单镜像：jsdelivr → fastly → github raw。
/// 原型解锁徽标即用此库（`polaris-prototype.html` `UB_ICON_BASE = dashboard-icons/png/`），几千个应用图标。
/// 清单 `tree.json` 结构 `{"png":["1panel.png", ...]}`（与 Qure/edc 的 `{"icons":[...]}` 不同 → 单独 parse）。
const HOMARR_ICON_MIRRORS: &[&str] = &[
    "https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons@main/tree.json",
    "https://fastly.jsdelivr.net/gh/homarr-labs/dashboard-icons@main/tree.json",
    "https://raw.githubusercontent.com/homarr-labs/dashboard-icons/main/tree.json",
];

/// homarr 图标本体 CDN 前缀（png 目录，与原型 `UB_ICON_BASE` 同源）：`<base><file.png>` 即图标 URL。
const HOMARR_ICON_BASE: &str = "https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons@main/png/";

/// 图标库 JSON 单次拉取超时（ms）。文件 KB 级，15s 足够。
const ICON_GALLERY_TIMEOUT_MS: u64 = 15_000;
/// 图标库 JSON 体积上限（8 MiB）：实际两源 < 40 KB，超此即拒防 OOM / 劫持回灌。
const ICON_GALLERY_MAX_BYTES: usize = 8 * 1024 * 1024;
/// 图标库内存缓存 TTL（1h）：避免每次开「添加应用」弹窗都拉网。**上游 无此缓存**（逐次直拉 `fetchJson`）——
/// 这是本移植针对「每开弹窗一次往返」显式补的最小内存缓存，不改变数据来源/解析口径。
const ICON_GALLERY_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// 进程级图标库内存缓存条目。
struct IconGalleryCache {
    fetched_at: Instant,
    items: Vec<IconGalleryItem>,
}

/// 懒初始化的进程级缓存句柄。
fn icon_gallery_cache() -> &'static Mutex<Option<IconGalleryCache>> {
    static CACHE: OnceLock<Mutex<Option<IconGalleryCache>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 读缓存：命中且未过 TTL → 克隆返回；否则 None。锁中毒 → 视作未命中（不 panic，回退重拉）。
fn read_fresh_icon_cache() -> Option<Vec<IconGalleryItem>> {
    let guard = icon_gallery_cache().lock().ok()?;
    let cache = guard.as_ref()?;
    (cache.fetched_at.elapsed() < ICON_GALLERY_CACHE_TTL).then(|| cache.items.clone())
}

/// 写缓存。**仅在结果非空时由调用方调用** —— 空结果（瞬时全断）不缓存，下次开弹窗即重试，不卡死 TTL。
fn store_icon_cache(items: &[IconGalleryItem]) {
    if let Ok(mut guard) = icon_gallery_cache().lock() {
        *guard = Some(IconGalleryCache {
            fetched_at: Instant::now(),
            items: items.to_vec(),
        });
    }
}

/// 作废清单内存缓存 —— 用户点「刷新」时的清单腿。
///
/// 没有这一步，「刷新」只清得掉图标本体的磁盘缓存：清单仍被 1h TTL 挡着，重拉命令直接返回旧清单，
/// 用户会看到「点了刷新，新图标还是不在列表里」。刷新必须两层一起作废才是用户理解的那个刷新。
fn invalidate_icon_gallery_cache() {
    if let Ok(mut guard) = icon_gallery_cache().lock() {
        *guard = None;
    }
}

/// 「刷新」的作废动作：两层缓存**一起**倒掉。抽成函数而不是写在 command 体内，是为了让「两层一起」
/// 这句话可被单测证伪 —— command 本体要 Tauri `State` 才能跑，写在里面就只剩注释里的一句宣称。
///
/// 磁盘腿只碰 `<userData>/icons/remote/`（浏览缓存）：「设定即缓存」的正式副本按 app id 落在
/// `icons/` 顶层，刷新图库不该动用户已经选定的图标。
fn drop_icon_gallery_caches(config_dir: &Path) {
    crate::icon_cache::clear_remote_cache(&crate::icon_cache::remote_cache_dir(config_dir));
    invalidate_icon_gallery_cache();
}

/// 从图库 JSON 提取 `.icons` → `{name,url}[]`。缺 `icons`/非数组 → 空；条目缺 name/url → 跳过该条
/// （不整体失败）。与 上游 `qure?.icons || []` 同口径：解析成功即用其 icons（可空），空 icons 不触发回退。
fn parse_icon_gallery(value: &Value) -> Vec<IconGalleryItem> {
    let Some(arr) = value.get("icons").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|it| {
            let name = it.get("name").and_then(Value::as_str)?;
            let url = it.get("url").and_then(Value::as_str)?;
            Some(IconGalleryItem {
                name: name.to_string(),
                url: url.to_string(),
            })
        })
        .collect()
}

/// 从 homarr `tree.json` 的 `.png` 文件名数组 → `{name,url}[]`。缺 `png`/非数组 → 空；空串 → 跳过。
/// 每项 `<file.png>`：显示名去 `.png` 后缀，url = `HOMARR_ICON_BASE + <file.png>`（图标本体也在 jsdelivr）。
/// 与 Qure/edc 的 `.icons` 结构不同（是 `{png:[...]}`），故单独解析——但产出同一 `IconGalleryItem` 契约。
fn parse_homarr_gallery(value: &Value) -> Vec<IconGalleryItem> {
    let Some(arr) = value.get("png").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|it| {
            let file = it.as_str().filter(|s| !s.is_empty())?;
            let name = file.strip_suffix(".png").unwrap_or(file).to_string();
            Some(IconGalleryItem {
                name,
                url: format!("{HOMARR_ICON_BASE}{file}"),
            })
        })
        .collect()
}

/// 拉取单个图库 URL 的 JSON（复用订阅同款 [`safe_redirect_fetch`]）。非 2xx / 网络错 / 非法 JSON → Err
/// （触发上层镜像回退）。泛型注入 client/lookup 便于单测 mock（不碰宿主网络）。
async fn fetch_icon_gallery_json<H: HttpClient, L: DnsLookup>(
    client: &H,
    lookup: &L,
    url: &str,
) -> Result<Value, String> {
    let resp = safe_redirect_fetch(SafeRedirectFetchOptions {
        fetch_impl: client,
        url,
        user_agent: app_user_agent(),
        headers: None,
        exempt_fake_ip: false,
        max_redirects: None,
        timeout_ms: Some(ICON_GALLERY_TIMEOUT_MS),
        max_body_bytes: Some(ICON_GALLERY_MAX_BYTES),
        lookup,
    })
    .await
    .map_err(|e| e.message)?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("下载失败：HTTP {}", resp.status));
    }
    serde_json::from_slice::<Value>(&resp.body).map_err(|e| format!("图库 JSON 非法: {e}"))
}

/// 逐镜像回退拉一个源的 icons：首个「拉取成功且 JSON 合法」的镜像即停并返回其 icons（可空，与 上游
/// 一致——合法 JSON 不再回退次镜像）。所有镜像都失败 → 空 vec。`parse` 注入各源的结构差异
/// （Qure/edc = `.icons`，homarr = `.png`）——回退/状态/JSON 编排统一，只解析口径不同。
async fn fetch_icon_source<H: HttpClient, L: DnsLookup>(
    client: &H,
    lookup: &L,
    mirrors: &[&str],
    parse: fn(&Value) -> Vec<IconGalleryItem>,
) -> Vec<IconGalleryItem> {
    for url in mirrors {
        if let Ok(value) = fetch_icon_gallery_json(client, lookup, url).await {
            return parse(&value);
        }
    }
    Vec::new()
}

/// 并发拉三个图库源（各自镜像回退），合并 icons（顺序 Qure → homarr → edc）。
/// 各源独立容错：一源失败不拖垮其余源；全失败 → 空 vec。**下载编排的可测核**（无缓存/无 state）。
///
/// homarr（homarr-labs/dashboard-icons，~2800 图标）是原型解锁徽标明确用的库，作第三源加入符合设计意图；
/// edc 上游 JSON 现坏（尾逗号）恒空，保留在链上——若上游修好会自动在末尾接回，不必改代码。
async fn fetch_icon_galleries<H: HttpClient, L: DnsLookup>(
    client: &H,
    lookup: &L,
) -> Vec<IconGalleryItem> {
    let (qure, homarr, edc) = tokio::join!(
        fetch_icon_source(client, lookup, QURE_ICON_MIRRORS, parse_icon_gallery),
        fetch_icon_source(client, lookup, HOMARR_ICON_MIRRORS, parse_homarr_gallery),
        fetch_icon_source(client, lookup, EDC_ICON_MIRRORS, parse_icon_gallery),
    );
    let mut merged = qure;
    merged.extend(homarr);
    merged.extend(edc);
    merged
}

/// 上游 `RULE_RESOURCES_ICON_GALLERIES`：在线图标库（**真拉取**，迁移自 上游
/// `RuleResourceManager.fetchIconGalleries`）。
///
/// 并发拉三个公开图库源（Qure + homarr + edc），各三镜像逐个回退，合并图标 → `[{name,url}]`。
/// 复用订阅同款 [`safe_redirect_fetch`]（SSRF/重定向/体积/超时 guard，公网 CDN 放行——与订阅同一路径）。
/// 进程级内存缓存 TTL 1h：避免每次开弹窗都拉网。任一源失败不致命（镜像/另一源兜底）；两源都失败返
/// `[]` —— 前端据契约降级为手动 URL 输入（`api-client.ts` `fetchIconGalleries` 明写「全失败返 []」）。
/// 恒返 `Ok(ApiResponse::ok(..))`：空集是契约内的成功态，不 err。
#[tauri::command]
pub async fn rule_resources_icon_galleries(
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<Vec<IconGalleryItem>>, ()> {
    if let Some(cached) = read_fresh_icon_cache() {
        return Ok(ApiResponse::ok(cached));
    }
    let http = state.http().clone();
    Ok(ApiResponse::ok(fetch_and_store_icon_galleries(&http).await))
}

/// 真拉一次三图库并写内存缓存。**仅缓存非空结果**：全失败（空）不写，下次即重试，不把瞬时全断卡死 1h。
/// 两个 command（惰性拉 / 强制刷新）共用，避免「缓存写入条件」分叉成两份。
async fn fetch_and_store_icon_galleries(http: &HttpRuntime) -> Vec<IconGalleryItem> {
    let items = fetch_icon_galleries(http, &SystemDnsLookup).await;
    if !items.is_empty() {
        store_icon_cache(&items);
    }
    items
}

/// 强制刷新在线图标库（「添加自定义应用」弹窗在线图标面板的「刷新」按钮）。
///
/// # 为什么刷新是「整份」而不是「单张」
///
/// 图标本体的缓存无 TTL（容量闸之外不会自己变新），所以必须有一个用户能按的强制口。粒度取整份的三条理由：
/// 1. **两层缓存必须一起作废**。用户眼里的「图标库旧了」既可能是图标本体旧、也可能是清单旧（少了新
///    收录的图标）。只清一层的按钮会在另一层旧掉时表现成「点了没用」—— 那比没有按钮更糟。
/// 2. **单张刷新没有可放的位置**。图库网格是 `max-height:150px` 的密排小格，逐格挂一个悬浮刷新按钮
///    既挤不下也会盖住图标本身；而单张「坏掉」的格子绝大多数是瞬时取图失败，重开面板即恢复，
///    不需要一个常驻控件。
/// 3. **它同时是「忘掉我浏览过什么」的清除入口**。浏览缓存会在本地留下「看过哪些图标」的痕迹
///    （见 `icon_cache` 模块的隐私记账），整份清空才对得上这个语义，逐张清没有意义。
///
/// 清完两层后**同步重拉**并返回新清单：让前端一次 IPC 拿到结果，不必「先清再查」两跳
/// （两跳之间若有别的渲染插进来，会把刚清掉的清单又填回缓存）。
#[tauri::command]
pub async fn rule_resources_refresh_icon_galleries(
    state: State<'_, AppRuntime>,
) -> Result<ApiResponse<Vec<IconGalleryItem>>, ()> {
    drop_icon_gallery_caches(state.config().dir());
    let http = state.http().clone();
    Ok(ApiResponse::ok(fetch_and_store_icon_galleries(&http).await))
}

/// 上游 `RULE_RESOURCES_DOWNLOAD`：批量下载规则资源（**真下载**）。
///
/// 消费 `RuleResourceDownloadItem[]`（`{catalogId?|url?, name?, category?, id?}`），逐项：
/// 解析 URL（catalogId → meta-rules-dat raw URL / url → 直接）→ `safe_redirect_fetch` 拉取
/// （复用订阅同款 SSRF/重定向/体积 guard）→ SRS/JSON sanity → 落 `<userData>/rule-resource/<fileName>`
/// → upsert `config.ruleResources`。返回 `RuleResourceDownloadResult[]`，与入参同序、逐项独立容错。
/// 成功项一次性保存 config + 广播 `config:changed`（幂等 upsert by id：redownload 同 id 覆盖）。
#[tauri::command]
pub async fn rule_resources_download(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    items: Vec<Value>,
) -> Result<ApiResponse<Vec<Value>>, ()> {
    let res_dir = state.config().dir().join("rule-resource");
    let http = state.http().clone();
    let gh_prefix = gh_proxy_prefix(&state);
    // 「刷新清单」落盘的远程全量：外置 tab 勾选的多数条目**不在**内置 33 条精选里，不带上它
    // 逐项都会 `资源库无此条目`（见 [`resolve_catalog_item`]）。无缓存 → 空切片，行为不变。
    let refreshed_catalog = read_catalog_cache(&res_dir).map_or_else(Vec::new, |(items, _)| items);

    let mut results: Vec<Value> = Vec::with_capacity(items.len());
    let mut stored: Vec<RuleResource> = Vec::new();
    for item in &items {
        let plan = match plan_from_item(item, &refreshed_catalog) {
            Ok(p) => p.with_gh_proxy(&gh_prefix),
            Err(e) => {
                results.push(err_result(
                    item.get("id").and_then(Value::as_str),
                    item.get("name").and_then(Value::as_str),
                    &e,
                    ERR_RESOURCE_BAD_ITEM,
                ));
                continue;
            }
        };
        let sink = BroadcastSink {
            app: &app,
            mode: ProgressMode::Live,
        };
        let outcome =
            download_with_progress(&sink, http.as_ref(), &SystemDnsLookup, &plan, &res_dir).await;
        if let DownloadOutcome::Stored { ref resource, .. } = outcome {
            stored.push(resource.clone());
        }
        results.push(outcome.into_value(&plan));
    }
    if !stored.is_empty() {
        persist_resources(&app, &state, &stored, BroadcastMode::Immediate);
    }
    Ok(ApiResponse::ok(results))
}

/// 资源删除计划（纯决策，与 IO 分离便于单测）。上游 `RuleResourceDeleteResult` 的判定核心。
#[derive(Debug)]
enum ResourceDeletePlan {
    /// 不在册 → 幂等成功（删除意图已达成）。
    NotFound,
    /// 被**已启用**规则引用且未 `force` → 需二次确认（不删）。
    NeedConfirm(Vec<RuleResourceRef>),
    /// 可删（无引用或已 force）；`file_name` 供解绑缓存文件。
    Proceed { file_name: Option<String> },
}

/// 纯决策：按 id 定位资源 + 引用检查 → 删除计划。不触 IO（config 传入 Value）。
fn plan_resource_delete(cfg: &Value, id: &str, force: bool) -> ResourceDeletePlan {
    let Some(entry) = cfg
        .get("ruleResources")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|v| v.get("id").and_then(Value::as_str) == Some(id))
        })
    else {
        return ResourceDeletePlan::NotFound;
    };
    let file_name = entry
        .get("fileName")
        .and_then(Value::as_str)
        .map(str::to_string);
    // 引用扫描：结构非法的 config 回落空配置 → 视作无引用（删除放行，不因扫描失败卡住删除）。
    let uc: UserConfig = serde_json::from_value(cfg.clone()).unwrap_or_default();
    let scan = RefScanInput {
        custom_rules: &uc.custom_rules,
        app_rules: &uc.app_rules,
        custom_app_presets: &uc.custom_app_presets,
    };
    let refs = enumerate_resource_refs(id, &scan);
    if !refs.is_empty() && !force {
        return ResourceDeletePlan::NeedConfirm(refs);
    }
    ResourceDeletePlan::Proceed { file_name }
}

/// 上游 `RULE_RESOURCES_DELETE`：删除规则资源（config 条目 + 缓存文件）。
///
/// 被已启用规则引用且未 `force` → `{ok:false, needConfirm:true, referencingRules}`（前端二次确认）。
/// 否则删 `config.ruleResources` 条目 + 解绑 `<userData>/rule-resource/<sanitized fileName>`
/// （复用 download 的 dir + sanitize 口径，防篡改 fileName 穿越）→ 持久化 + 广播。不在册 → 幂等成功。
#[tauri::command]
pub fn rule_resources_delete(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    id: String,
    force: Option<bool>,
) -> ApiResponse<Value> {
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    let file_name = match plan_resource_delete(&cfg, &id, force.unwrap_or(false)) {
        ResourceDeletePlan::NotFound => return ApiResponse::ok(json!({ "ok": true })),
        ResourceDeletePlan::NeedConfirm(refs) => {
            return ApiResponse::ok(json!({
                "ok": false,
                "needConfirm": true,
                "referencingRules": serde_json::to_value(&refs).unwrap_or_else(|_| json!([])),
            }));
        }
        ResourceDeletePlan::Proceed { file_name } => file_name,
    };
    // 删 config 条目。
    if let Some(arr) = cfg.get_mut("ruleResources").and_then(Value::as_array_mut) {
        arr.retain(|v| v.get("id").and_then(Value::as_str) != Some(id.as_str()));
    }
    // 解绑缓存文件（best-effort：文件缺失/权限问题不阻塞 config 删除；sanitize 与 download 同口径）。
    if let Some(fname) = &file_name {
        let dest = state
            .config()
            .dir()
            .join("rule-resource")
            .join(sanitize_file_stem(fname));
        let _ = std::fs::remove_file(&dest);
    }
    match state.config().save_full(&cfg) {
        Ok(()) => {
            broadcast_config_changed(&app, &cfg);
            ApiResponse::ok(json!({ "ok": true }))
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 上游 `RULE_RESOURCES_SET_AUTO_UPDATE`：设自动更新开关 + 间隔 → 持久化 + 广播。
///
/// 此前 `{ok:true}` 但**不写任何东西**（重载即复位）。现落 `config.ruleResourceAutoUpdate` +
/// `config.ruleResourceUpdateIntervalHours`（后者仅当传入时写，缺省保留旧值）。
#[tauri::command]
pub fn rule_resources_set_auto_update(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    enabled: bool,
    interval_hours: Option<u32>,
) -> ApiResponse<Value> {
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::ok(json!({ "ok": false, "error": format!("{e}") })),
    };
    if let Some(obj) = cfg.as_object_mut() {
        obj.insert("ruleResourceAutoUpdate".to_string(), json!(enabled));
        if let Some(h) = interval_hours {
            obj.insert("ruleResourceUpdateIntervalHours".to_string(), json!(h));
        }
    }
    match state.config().save_full(&cfg) {
        Ok(()) => {
            broadcast_config_changed(&app, &cfg);
            ApiResponse::ok(json!({ "ok": true }))
        }
        Err(e) => ApiResponse::ok(json!({ "ok": false, "error": format!("{e}") })),
    }
}

/// 上游 `RULE_RESOURCES_RESET_BUILTIN`：重置内置 geo 规则集为出厂版（factory 重置）。
///
/// `tag` 为分类入口（`geosite`/`geoip`；其他值 → 两类全重置）。**无网络**：
/// 1. 物理删除该类内置 geo 的下载缓存 `.srs`（`<userData>/rule-resource/<fileName>`，sanitize 与 download 同口径）；
/// 2. 物理删除**生效中的运行时副本**（`<userData>/rules/<fileName>`）→ 下次 seed（启动 / 起核前）
///    按「缺失必种」从随包资源重种出厂版；
/// 3. 清 `config.builtinGeoMeta`（网络更新标记）→ 该 tag 恢复「出厂态」，重新纳入启动时的出厂态刷新射程。
///
/// **为什么第 2 步不能省**（此前就省了，于是这条 command 名不副实）：seed 是
/// **seed-if-missing-or-invalid**，运行时副本只要还有效就恒被跳过。只清 config 标记而留着那份副本，
/// 「重置为出厂版」对**生效中的那一份完全无作用** —— 用户点了重置，下次起核用的还是同一个文件。
///
/// 只碰内置 geo（`builtin_geo_rulesets()` 表内），**不删用户自建/下载的 `config.ruleResources`**。
/// 持久化 + 广播。返回前端 `RuleResourceDownloadResult` 的 `ok` 形态。
#[tauri::command]
pub fn rule_resources_reset_builtin(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    tag: String,
) -> ApiResponse<Value> {
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => return ApiResponse::err(format!("{e}")),
    };
    let category = match tag.as_str() {
        "geosite" => Some(GeoCategory::Geosite),
        "geoip" => Some(GeoCategory::Geoip),
        _ => None, // 具体 tag / 未知 → 两类全重置。
    };
    let res_dir = state.config().dir().join("rule-resource");
    // 运行时 seed 目录：**必须与 `geo_seed` / `GenerateConfigDeps.runtime_rules_dir` 同源**
    // （`config_dir.join("rules")`），删错目录 = 重置静默无效。
    let runtime_dir = state.config().dir().join("rules");
    for b in builtin_geo_rulesets() {
        if category.is_none() || category == Some(b.category) {
            let _ = std::fs::remove_file(res_dir.join(sanitize_file_stem(&b.file_name)));
            // 生效中的运行时副本也删 → 下次 seed 按「缺失必种」重种出厂版（见函数文档第 2 步）。
            let _ = std::fs::remove_file(runtime_dir.join(&b.file_name));
        }
    }
    // 清网络更新标记 → 全部恢复出厂态（重新纳入启动时的出厂态刷新射程）。
    if let Some(obj) = cfg.as_object_mut() {
        obj.remove("builtinGeoMeta");
    }
    match state.config().save_full(&cfg) {
        Ok(()) => {
            broadcast_config_changed(&app, &cfg);
            ApiResponse::ok(json!({ "ok": true, "id": builtin_id_for(&tag), "name": tag }))
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 内置 geo 的运行时生效目录（`<userData>/rules`）。**必须与 `geo_seed` /
/// `GenerateConfigDeps.runtime_rules_dir` 同源** —— 写别处等于更新静默无效。
fn builtin_runtime_dir(state: &AppRuntime) -> std::path::PathBuf {
    state.config().dir().join("rules")
}

/// 内置 geo 的下载计划（分类字符串 / id / 原址三者的唯一拼装点）。
fn plan_from_builtin(b: &BuiltinGeoRuleSet) -> ResourcePlan {
    let category = match b.category {
        GeoCategory::Geosite => "geosite",
        GeoCategory::Geoip => "geoip",
    };
    let url = b.source_url();
    ResourcePlan {
        id: builtin_id_for(&b.tag),
        name: b.tag.clone(),
        category: category.to_string(),
        fetch_url: url.clone(),
        url,
        file_name: b.file_name.clone(),
        format: RuleResourceFormat::Binary,
    }
}

/// 上游 `RULE_RESOURCES_UPDATE_BUILTIN`：把单个内置 geo 规则集更新到上游最新版（**真下载**）。
///
/// 这条腿此前不存在，于是 `rule_resources_list` 也不敢列内置项 —— 列出来每行都会带一个必然报
/// `RULE_RESOURCE_NOT_FOUND` 的「更新」按钮（行内更新走 [`rule_resources_redownload`]，它按 id 查
/// `config.ruleResources`，而 `builtin:*` 从不入册）。当时记的否决理由是「缺随包 geo manifest、
/// 不知道 sourceUrl」，**复核后不成立**：地址纯由 tag 推导，见
/// [`BuiltinGeoRuleSet::source_url`]（陈先生 2026-07-29 指出「随包不影响关联包资源地址」）。
///
/// 与普通资源的三点不同，都是内置态本身带来的：
/// 1. **落盘目录是运行时生效目录** `<userData>/rules/`，不是下载缓存 `<userData>/rule-resource/`
///    —— 后者只是资源库的暂存，sing-box 读的是前者；
/// 2. **原子替换**：先下到同目录下的 `.update/` 暂存再 `rename`（同盘 ⇒ 原子）。直写会让
///    「正在起核 / 正在读这个 .srs」撞上半截文件，而 SRS 魔数只校验前 3 字节、拦不住尾部截断；
/// 3. **不入册 `config.ruleResources`**：内置项的身份来自 `builtin_geo_rulesets()` 这张表，
///    入册会造出第二个真值源（并让 reset 之后条目还留着）。只写
///    `config.builtinGeoMeta[tag].updatedAt` 作「已网络更新」标记 —— 该标记正是 `geo_seed`
///    判「出厂态」的读侧判据，写上之后启动时的出厂版重种不会再覆盖这份新副本。
///
/// **生效时机如实回报**：本命令只换文件，不重启内核。运行中的 sing-box 仍持有旧规则集，
/// 下次起核才生效 —— 与既有的 [`rule_resources_reset_builtin`] 同一契约，不在这里偷偷重启。
#[tauri::command]
pub async fn rule_resources_update_builtin(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    tag: String,
) -> Result<ApiResponse<Value>, ()> {
    update_builtin_with_mode(
        &app,
        &state,
        tag,
        ProgressMode::Live,
        BroadcastMode::Immediate,
    )
    .await
}

/// 后台调度腿专用入口：与 [`rule_resources_update_builtin`] 同一条下载/落位/记账路径，
/// 两处差别是**一帧进度都不发**（`ProgressMode::Silent`）+ **不逐条广播**（`BroadcastMode::Deferred`）。
///
/// 与 [`rule_resources_redownload_silent`] 同款处置：两个模式都写死在函数里、不做成命令形参 ——
/// 形参会成为前端可传的参数袋键，语义就不再由后端说了算。
pub async fn rule_resources_update_builtin_silent(
    app: &AppHandle,
    state: &AppRuntime,
    tag: String,
) -> Result<ApiResponse<Value>, ()> {
    update_builtin_with_mode(
        app,
        state,
        tag,
        ProgressMode::Silent,
        BroadcastMode::Deferred,
    )
    .await
}

/// 内置 geo 更新的共用核心（手动腿 / 后台腿只差 [`ProgressMode`] + [`BroadcastMode`]）。
async fn update_builtin_with_mode(
    app: &AppHandle,
    state: &AppRuntime,
    tag: String,
    mode: ProgressMode,
    broadcast: BroadcastMode,
) -> Result<ApiResponse<Value>, ()> {
    let Some(b) = find_builtin(&tag) else {
        return Ok(ApiResponse::ok(err_result(
            Some(&builtin_id_for(&tag)),
            Some(&tag),
            "内置规则集不存在",
            ERR_RESOURCE_NOT_FOUND,
        )));
    };
    let plan = plan_from_builtin(&b).with_gh_proxy(&gh_proxy_prefix(state));
    let runtime_dir = builtin_runtime_dir(state);
    // 暂存区放在生效目录**之内**：跨目录 rename 只有同一文件系统才原子，同父目录是最稳的保证
    // （`<userData>` 与临时目录可能分属不同挂载点）。
    let stage_dir = runtime_dir.join(".update");
    let http = state.http().clone();
    let sink = BroadcastSink { app, mode };
    let outcome =
        download_with_progress(&sink, http.as_ref(), &SystemDnsLookup, &plan, &stage_dir).await;

    let outcome = match outcome {
        DownloadOutcome::Stored { resource, .. } => {
            let staged = stage_dir.join(&plan.file_name);
            let live = runtime_dir.join(&plan.file_name);
            // `existedBefore` 要看**生效副本**存不存在，不是暂存区（那儿必然是新建的）。
            let existed_before = live.is_file();
            match std::fs::rename(&staged, &live) {
                Ok(()) => DownloadOutcome::Stored {
                    resource,
                    existed_before,
                },
                Err(e) => {
                    let _ = std::fs::remove_file(&staged);
                    DownloadOutcome::Failed {
                        message: format!("替换生效副本失败: {e}"),
                        code: ERR_RESOURCE_WRITE_FAILED,
                    }
                }
            }
        }
        other => other,
    };
    // 只删空目录：非空说明有别的在途下载的暂存文件，硬删会打断它。
    let _ = std::fs::remove_dir(&stage_dir);

    if let DownloadOutcome::Stored { ref resource, .. } = outcome {
        persist_builtin_geo_updated(app, state, &b.tag, &resource.downloaded_at, broadcast);
    }
    Ok(ApiResponse::ok(outcome.into_value(&plan)))
}

/// 记「该内置 tag 已网络更新过」：`config.builtinGeoMeta[tag].updatedAt = <ISO>`。
///
/// 只写 `updatedAt` 一个字段 —— 它是 `geo_seed::network_updated_tags_from_raw` 唯一读的键，
/// 也是 TS 契约 `builtinGeoMeta?: Record<string,{updatedAt?:string}>` 声明的唯一字段。
/// 大小/地址不落这里：前者 stat 生效副本即得（真值在盘上），后者由 tag 推导（真值在表里），
/// 抄一份进 config 就是给自己造两个真值源。
fn persist_builtin_geo_updated(
    app: &AppHandle,
    state: &AppRuntime,
    tag: &str,
    updated_at: &str,
    broadcast: BroadcastMode,
) {
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => {
            log::error!("内置 geo `{tag}` 已更新到盘上，但加载 config 失败（更新标记未落）: {e}");
            return;
        }
    };
    let Some(obj) = cfg.as_object_mut() else {
        log::error!("内置 geo `{tag}` 已更新到盘上，但 config 根不是对象（更新标记未落）");
        return;
    };
    let meta = obj
        .entry("builtinGeoMeta")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .map(|m| {
            m.insert(tag.to_string(), json!({ "updatedAt": updated_at }));
        });
    if meta.is_none() {
        // 旧配置里该键被写成非对象（sanitize 会删它，但可能尚未过一轮）→ 整键重建，不静默放弃。
        obj.insert(
            "builtinGeoMeta".to_string(),
            json!({ tag: { "updatedAt": updated_at } }),
        );
    }
    match state.config().save_full(&cfg) {
        // Deferred：批次拥有者收尾统一广播（见 [`BroadcastMode`]）。落盘已完成，跳过的只是通知。
        Ok(()) => {
            if broadcast == BroadcastMode::Immediate {
                broadcast_config_changed(app, &cfg);
            }
        }
        Err(e) => {
            log::error!("内置 geo `{tag}` 已更新到盘上，但保存 config 失败（更新标记未落）: {e}");
        }
    }
}

fn new_uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("pol-{nanos:032x}")
}

/// ISO8601 当前时间（上游 `new Date().toISOString()`）。落进 `config.ruleResources[].downloadedAt`
/// （契约 `downloadedAt: string /* ISO */`）→ 前端 `new Date(...)` 解析，故**必须是合法 ISO**。
///
/// 复用 stats-engine 既有的 `created_at_to_rfc3339`（无外部 time 依赖的 civil 算法，`misc.rs` 同款）——
/// **不新增 chrono/time 依赖**。旧实现 `format!("1970-01-01T00:00:{secs}Z")` 把整个 epoch 秒（~1.78e9）
/// 塞进秒字段 → `"1970-01-01T00:00:1784563200Z"`（非法，前端 Invalid Date → 资源列表显示「—」，且落 config
/// 持久化坏数据）。时钟异常取不到时间 → 空串（不 panic）。
fn current_iso() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .and_then(polaris_stats_engine::created_at_to_rfc3339)
        .unwrap_or_default()
}

// ── 规则资源下载核心（download / redownload / update_all 共用）──────────────────

/// 一次下载的解析计划：目标 URL + 落盘元数据。
struct ResourcePlan {
    id: String,
    name: String,
    category: String,
    /// **原址**（canonical）。落进 `config.ruleResources[].sourceUrl`，也是加速失败后的回退地址。
    url: String,
    /// **本次实际请求的地址**：套过 gh 加速前缀则是镜像址，否则 == `url`。
    ///
    /// 为什么与 `url` 分开而不是就地改写 `url`：`url` 会被持久化成 `sourceUrl`，把镜像址写进去就等于
    /// 把「当前这台加速器」焊死进配置 —— 用户改 / 清 `ghProxyPrefix` 之后，重下载仍走旧镜像，设置项形同
    /// 虚设；镜像停服还会变成永久坏源。对齐 上游 `RuleResourceManager.fetchSrsToFile`
    /// （`applyGhProxy` 只作用于本次请求，登记的 `sourceUrl` 恒为原址）。
    fetch_url: String,
    file_name: String,
    format: RuleResourceFormat,
}

impl ResourcePlan {
    /// 套 gh 加速前缀（下载 plan 阶段的唯一入口）。非 GitHub 域 / 空前缀 → 原样不动。
    #[must_use]
    fn with_gh_proxy(mut self, prefix: &str) -> Self {
        if let Some(mirrored) = apply_gh_proxy(prefix, &self.url) {
            self.fetch_url = mirrored;
        }
        self
    }
}

/// 下载结果（内部枚举 → `into_value` 转前端 `RuleResourceDownloadResult`）。
enum DownloadOutcome {
    Stored {
        resource: RuleResource,
        existed_before: bool,
    },
    Failed {
        message: String,
        code: &'static str,
    },
    /// 用户在下载途中点了「取消」（[`rule_resources_cancel`]）→ 传输已中止，未落盘未入册。
    Cancelled,
}

impl DownloadOutcome {
    fn into_value(self, plan: &ResourcePlan) -> Value {
        match self {
            // 取消是用户主观意图，不是故障：仍走 `ok:false`（调用方不该当成功入册），但用专属
            // errorCode 与其它失败区分，前端据此报「已取消」而非「更新失败」。
            DownloadOutcome::Cancelled => err_result(
                Some(&plan.id),
                Some(&plan.name),
                "下载已取消",
                ERR_RESOURCE_CANCELLED,
            ),
            DownloadOutcome::Stored {
                resource,
                existed_before,
            } => json!({
                "ok": true,
                "resource": serde_json::to_value(&resource).unwrap_or(Value::Null),
                "id": resource.id,
                "name": resource.name,
                "existedBefore": existed_before,
            }),
            DownloadOutcome::Failed { message, code } => {
                err_result(Some(&plan.id), Some(&plan.name), &message, code)
            }
        }
    }
}

/// 组装失败结果（前端 `RuleResourceDownloadResult` 的 `ok:false` 形态）。
fn err_result(id: Option<&str>, name: Option<&str>, error: &str, code: &str) -> Value {
    let mut o = json!({ "ok": false, "error": error, "errorCode": code });
    if let Some(id) = id {
        o["id"] = json!(id);
    }
    if let Some(name) = name {
        o["name"] = json!(name);
    }
    o
}

fn is_http_url(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// 由 URL 扩展名判 format（`.json` → source，其余 → binary/.srs）。
fn detect_format(url: &str) -> RuleResourceFormat {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".json") {
        RuleResourceFormat::Source
    } else {
        RuleResourceFormat::Binary
    }
}

fn ext_for(format: RuleResourceFormat) -> &'static str {
    match format {
        RuleResourceFormat::Binary => "srs",
        RuleResourceFormat::Source => "json",
    }
}

/// 与资源目录里**非资源文件**同名的保留名单（当前只有目录缓存）。
///
/// 缓存 `catalog.json` 与用户资源落在**同一个** `<userData>/rule-resource/` 目录下，而一条
/// `id="catalog"` 的自定义 `.json` 资源恰好派生出同名文件 → 双向覆盖：下载该资源会把目录缓存
/// 冲掉（下次刷新失去兜底），刷新目录会把用户资源文件冲成一份清单 JSON（规则集当场失效）。
/// 缓存路径由 `runtime/rule_resource_scheduler.rs` 只读镜像（那边有常量同步断言），
/// 故这里用「资源侧改名让路」而非「把缓存挪进子目录」—— 后者要同时改那个镜像。
const RESERVED_RESOURCE_FILE_NAMES: [&str; 1] = [CATALOG_CACHE_FILE];

/// 落盘名 `<sanitized id>.<ext>`；**有损清洗或撞保留名时**追加 id 短哈希消歧。
///
/// # 为什么不能只做 `sanitize`
///
/// [`sanitize_file_stem`] 把 `:` `*` 空格等一律折成 `_` —— 那是**多对一**映射：远端两个不同的
/// catalog id（`geosite-foo:bar` / `geosite-foo*bar`）会落到同一个 `geosite-foo_bar.srs`，
/// 后下的静默覆盖先下的，而 config 里两条记录都指向这一个文件 → 其中一条规则集内容是错的。
/// 加一段由**原始 id**算出的短哈希即可把映射打回单射。
///
/// 只在「清洗有损」或「撞保留名」时加后缀（而非无条件加）：绝大多数 id 本来就只含
/// `[A-Za-z0-9._-]`，无条件加后缀会把**全部**既有资源的文件名改掉 —— 已下载的文件当场变孤儿、
/// 全都显示成「未下载」。干净 id 逐字保持原样，行为零变化。
fn resource_file_name(id: &str, format: RuleResourceFormat) -> String {
    let stem = sanitize_file_stem(id);
    let name = format!("{stem}.{}", ext_for(format));
    let lossy = stem != id;
    if !lossy && !RESERVED_RESOURCE_FILE_NAMES.contains(&name.as_str()) {
        return name;
    }
    format!("{stem}-{}.{}", short_id_hash(id), ext_for(format))
}

/// 由原始 id 算 8 位十六进制短哈希（FNV-1a 64，取低 32 位）。
///
/// 不用 `DefaultHasher`：它的算法**不保证跨 Rust 版本稳定**，而这个值会被写进 config 的
/// `fileName` 并落到磁盘 —— 换个编译器就换一批文件名，等于每次升级都让资源变孤儿。
/// FNV-1a 是十几行的确定性算法，零依赖、永远不变。
fn short_id_hash(id: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for b in id.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    format!("{:08x}", (h ^ (h >> 32)) as u32)
}

/// 清洗文件名 stem：仅留 `[A-Za-z0-9._-]`，其余 → `_`；消除 `..`（防路径穿越）。
fn sanitize_file_stem(id: &str) -> String {
    let mut s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    while s.contains("..") {
        s = s.replace("..", "_");
    }
    if s.is_empty() {
        s.push('_');
    }
    s
}

/// 从 URL 推断资源名（basename 去扩展名；空则 `resource`）。
fn infer_name_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let base = path.rsplit('/').next().unwrap_or(path);
    let stem = base.rsplit_once('.').map_or(base, |(a, _)| a);
    if stem.is_empty() {
        "resource".to_string()
    } else {
        stem.to_string()
    }
}

/// 按 catalogId 解析条目：**内置精选表优先，其次刷新得到的全量清单**（= 上游
/// `RuleResourceManager.findCatalogItem`，`:705-710`：先 `findCatalogItem(id)` 再 `getCatalog()`）。
///
/// 为什么必须有第二跳：「刷新清单」拿回来的是 2000+ 条远程全量，其中只有 33 条在内置表里。只查内置表
/// 的话，用户在外置 tab 勾中任何一条精选之外的资源点下载，都会恒返 `资源库无此条目` —— 刷新功能等于
/// 只能看不能用。本仓此前正是如此（`find_catalog_item` 单跳），与恒等降级的刷新腿互相掩盖。
fn resolve_catalog_item(
    id: &str,
    refreshed_catalog: &[RuleResourceCatalogItem],
) -> Option<RuleResourceCatalogItem> {
    find_catalog_item(id).or_else(|| refreshed_catalog.iter().find(|i| i.id == id).cloned())
}

/// 解析前端 `RuleResourceDownloadItem` → 下载计划。catalogId 优先，其次 url。
///
/// `refreshed_catalog` = 磁盘缓存里的远程全量清单（无缓存时传空切片 → 退化为只认内置精选表）。
fn plan_from_item(
    item: &Value,
    refreshed_catalog: &[RuleResourceCatalogItem],
) -> Result<ResourcePlan, String> {
    let name_in = item
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let cat_in = item
        .get("category")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    // 优先 catalogId（内置/动态精选项 → meta-rules-dat raw URL）。
    if let Some(cid) = item
        .get("catalogId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let cat = resolve_catalog_item(cid, refreshed_catalog)
            .ok_or_else(|| format!("资源库无此条目: {cid}"))?;
        let id = cat.id.clone();
        let name = name_in.map_or_else(|| cat.name.clone(), str::to_string);
        let category = cat_in.map_or(cat.category, str::to_string);
        let url = mrd_raw_url(&cat.path);
        let file_name = resource_file_name(&id, RuleResourceFormat::Binary);
        return Ok(ResourcePlan {
            id,
            name,
            category,
            fetch_url: url.clone(),
            url,
            file_name,
            format: RuleResourceFormat::Binary,
        });
    }

    // 其次 url（手动下载）。
    if let Some(url) = item
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !is_http_url(url) {
            return Err(format!("URL 协议不支持（仅 http/https）: {url}"));
        }
        let format = detect_format(url);
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map_or_else(|| format!("res_{}", new_uuid()), str::to_string);
        let name = name_in.map_or_else(|| infer_name_from_url(url), str::to_string);
        let category = cat_in.map_or_else(|| "custom".to_string(), str::to_string);
        let file_name = resource_file_name(&id, format);
        return Ok(ResourcePlan {
            id,
            name,
            category,
            url: url.to_string(),
            fetch_url: url.to_string(),
            file_name,
            format,
        });
    }

    Err("下载项须含 catalogId 或 url".to_string())
}

/// 已登记资源 → 下载计划（redownload / update_all 用；保留原 id/sourceUrl）。
fn plan_from_resource(r: &RuleResource) -> ResourcePlan {
    ResourcePlan {
        id: r.id.clone(),
        name: r.name.clone(),
        category: r.category.clone(),
        url: r.source_url.clone(),
        fetch_url: r.source_url.clone(),
        // **信任边界清洗**（P3）：config 里的 fileName 可能被篡改/导入为 `../../.bashrc` 或绝对路径，
        // 而 `download_and_store` 的 `res_dir.join(&file_name)` 遇绝对路径会整段替换 → 逃出资源目录。
        // 首次下载路径（plan_from_item）走 resource_file_name → sanitize_file_stem，redownload/update_all
        // 此前直接透传原值漏了这道闸 → 在此按同一 sanitizer 收口（对合法名幂等，不改正常重下载行为）。
        //
        // 额外一道：清洗后若**撞上保留名**（目录缓存 `catalog.json`），改按 id 重新派生 ——
        // 存量 config 里可能早就登记着 `fileName:"catalog.json"`（本轮之前 `id:"catalog"` 的 json
        // 资源就是这么落的），重下载会把目录缓存冲掉。见 [`RESERVED_RESOURCE_FILE_NAMES`]。
        file_name: {
            let cleaned = sanitize_file_stem(&r.file_name);
            if RESERVED_RESOURCE_FILE_NAMES.contains(&cleaned.as_str()) {
                resource_file_name(&r.id, r.format)
            } else {
                cleaned
            }
        },
        format: r.format,
    }
}

/// 反序列化一条 `ruleResources` 原始项 → [`RuleResource`]。失败（结构非法：缺字段/类型错）→
/// `Err(err_result)`，错误码 **BAD_ITEM**（非 NOT_FOUND）——该条目**在册但坏了**，与「不在册」是不同的
/// 诚实语义（P8）。保留原始 id/name 供前端定位。
fn parse_resource_entry(entry: &Value) -> Result<RuleResource, Value> {
    serde_json::from_value::<RuleResource>(entry.clone()).map_err(|e| {
        err_result(
            entry.get("id").and_then(Value::as_str),
            entry.get("name").and_then(Value::as_str),
            &format!("资源条目结构非法: {e}"),
            ERR_RESOURCE_BAD_ITEM,
        )
    })
}

/// 在 `ruleResources` 里按 id 定位并解析：不在册 → NOT_FOUND；在册但结构非法 → BAD_ITEM；命中且合法 → Ok。
///
/// **先按 id 找原始项、再反序列化**（不先 `filter_map(.ok())` 滤掉坏项）——否则坏项会被误报成
/// 「资源不在册」（P8：它其实在册，只是 malformed）。
fn resolve_registered_resource(resources: &[Value], id: &str) -> Result<RuleResource, Value> {
    let Some(entry) = resources
        .iter()
        .find(|v| v.get("id").and_then(Value::as_str) == Some(id))
    else {
        return Err(err_result(
            Some(id),
            None,
            &format!("资源不在册: {id}"),
            ERR_RESOURCE_NOT_FOUND,
        ));
    };
    parse_resource_entry(entry)
}

/// 内容 sanity：binary 校 SRS 魔数（与 route.rs / rule_resources_list 同口径）；source 校 JSON 对象。
fn validate_resource_bytes(format: RuleResourceFormat, body: &[u8]) -> Result<(), String> {
    if body.is_empty() {
        return Err("下载内容为空".to_string());
    }
    match format {
        RuleResourceFormat::Binary => {
            if body.len() < 3 || !is_valid_srs_bytes([body[0], body[1], body[2]]) {
                return Err("下载内容不是有效的 .srs 规则集（SRS 魔数校验失败）".to_string());
            }
        }
        RuleResourceFormat::Source => match serde_json::from_slice::<Value>(body) {
            Ok(v) if v.is_object() => {}
            Ok(_) => return Err("下载内容不是 JSON 对象（sing-box 源规则集须为对象）".to_string()),
            Err(e) => return Err(format!("下载内容不是合法 JSON: {e}")),
        },
    }
    Ok(())
}

/// 拉取资源字节（**复用订阅同款** [`safe_redirect_fetch`]：逐跳 SSRF guard + 体积闸 + 超时 + 手动重定向）
/// + 状态/内容 sanity。泛型注入 client/lookup 便于回环门测（真 socket，不碰宿主网络）。
async fn fetch_resource_bytes<H: HttpClient, L: DnsLookup>(
    client: &H,
    lookup: &L,
    url: &str,
    format: RuleResourceFormat,
) -> Result<Vec<u8>, String> {
    let resp = safe_redirect_fetch(SafeRedirectFetchOptions {
        fetch_impl: client,
        url,
        user_agent: app_user_agent(),
        headers: None,
        exempt_fake_ip: false,
        max_redirects: None,
        timeout_ms: Some(RULE_RESOURCE_TIMEOUT_MS),
        max_body_bytes: Some(RULE_RESOURCE_MAX_BYTES),
        lookup,
    })
    .await
    .map_err(|e| e.message)?;

    if !(200..300).contains(&resp.status) {
        return Err(format!("下载失败：HTTP {}", resp.status));
    }
    validate_resource_bytes(format, &resp.body)?;
    Ok(resp.body)
}

/// 下载 + 落盘（不写 config；config upsert 由 [`persist_resources`] 批量做）。
/// # gh 加速的回退腿
///
/// 设置页对「GitHub 加速」的承诺是「留空回退直连；**下载失败自动回退直连兜底**」
/// （`i18n settings.ghProxyHint`）。故加速址失败且确实套过前缀（`fetch_url != url`）时，再打一次原址：
/// 镜像挂了 / 返 HTML 错误页 / 被墙，都不该让本来能直连拿到的资源变成红行。对齐 上游
/// `fetchSrsToFile` 的 `if (!r.ok && prefix) r = await this.fetchBuffer(sourceUrl, ...)`。
/// 失败消息取**回退腿**的（那是用户最终没拿到东西的真实原因）。
async fn download_and_store<H: HttpClient, L: DnsLookup>(
    client: &H,
    lookup: &L,
    plan: &ResourcePlan,
    res_dir: &std::path::Path,
) -> DownloadOutcome {
    let mut attempt = fetch_resource_bytes(client, lookup, &plan.fetch_url, plan.format).await;
    if let (Err(e), true) = (&attempt, plan.fetch_url != plan.url) {
        // 同上（`fetch_catalog_json`）：静默回退 = 用户无从知道加速腿恒挂。自建内网 gh-proxy 被
        // SSRF guard 拒是最常见的一种，且**不能**靠放行内网 host 来「修」（那是开 SSRF 面）。
        log::warn!(
            "gh 加速腿失败，回退原址（资源 {}）: {} → {e}",
            plan.id,
            plan.fetch_url
        );
        attempt = fetch_resource_bytes(client, lookup, &plan.url, plan.format).await;
    }
    let bytes = match attempt {
        Ok(b) => b,
        Err(message) => {
            return DownloadOutcome::Failed {
                message,
                code: ERR_RESOURCE_DOWNLOAD_FAILED,
            }
        }
    };
    let dest = res_dir.join(&plan.file_name);
    let existed_before = dest.is_file();
    // **原子替换**：先写同目录临时文件再 rename（同目录 ⇒ 同文件系统 ⇒ rename 原子）。
    //
    // 此前是 `std::fs::write(&dest, ..)` 直写目标。网络失败伤不到已有副本（字节先全收完才动盘），
    // 但**写到一半失败**（磁盘满 / 断电 / 进程被杀）会把用户已有的那份规则资源截断成半截文件 ——
    // 而 SRS 魔数只校验前 3 字节，截断的尾部照样过校验，坏文件会一直被当好的用下去
    // （陈先生 2026-07-30：「规则更新的时候要确保更新失败不破坏已有资源」）。
    // 临时名带 pid：同一资源两条在途下载（手动 + 后台调度）不会互相覆盖对方的半成品。
    let tmp = res_dir.join(format!(".{}.{}.tmp", plan.file_name, std::process::id()));
    let staged = std::fs::create_dir_all(res_dir)
        .and_then(|()| std::fs::write(&tmp, &bytes))
        .and_then(|()| std::fs::rename(&tmp, &dest));
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&tmp); // 半成品不留在盘上（它不是有效资源，也不该被误当缓存）
        return DownloadOutcome::Failed {
            message: format!("写入失败: {e}"),
            code: ERR_RESOURCE_WRITE_FAILED,
        };
    }
    let resource = RuleResource {
        id: plan.id.clone(),
        name: plan.name.clone(),
        category: plan.category.clone(),
        source_url: plan.url.clone(),
        file_name: plan.file_name.clone(),
        format: plan.format,
        size: bytes.len() as u64,
        downloaded_at: current_iso(),
    };
    DownloadOutcome::Stored {
        resource,
        existed_before,
    }
}

/// 进度可见性档位。**后台调度腿恒 `Silent`**（对齐 上游 `RuleResourceManager.downloadOne` 的
/// `silent` 形参：`if (silent) return;` 直接吞掉整帧）——后台保鲜不该在用户正看别的页面时
/// 往资源页推进度条 / 堆红行。手动腿（三个 command）恒 `Live`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProgressMode {
    /// 用户显式触发 → 照常广播 `EVENT_RULE_RESOURCE_PROGRESS`。
    Live,
    /// 后台调度触发 → 一帧不发。
    Silent,
}

/// 配置广播时机。**与 [`ProgressMode`] 正交**：那个管「UI 要不要看到进度条」，本枚举管
/// 「这次落盘要不要立刻进核」——`broadcast_config_changed` 不只是 emit 给渲染端，它同时
/// `spawn(switch_mode)` 把变更送进运行核（见该函数文档）。
///
/// # 为什么必须可延后（真机实证 2026-08-02）
///
/// 后台保鲜一轮要更新 **8 条已登记资源 + 25 个内置 geo**，每条各自落盘 + 广播 ⇒ 一轮启动补更
/// 打出 **33 次 `switch_mode`**（真机日志 11 秒内 35 条 `switchMode：核未运行 → 仅更新配置`）。
/// 核没跑时只是刷屏；**核在跑时每条都进热切换/去抖重启判定** —— 每次启动补更与每 30 分钟巡检
/// 都在给运行中的核连砸 33 次。而这一轮的语义本就是「一批」：批内每条的中间态没有任何消费者
/// 需要看见。
///
/// 落盘仍**逐条**（每条各自 `save_full`）：磁盘真值随下随记，批次中途崩溃只丢已下载条目的
/// 更新标记，下一轮自愈；把落盘也攒到最后反而放大了丢失窗口。延后的只是广播。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BroadcastMode {
    /// 单条更新（用户手点）→ 落盘后立即广播，变更即刻进核。
    Immediate,
    /// 批量更新（后台调度一轮）→ 只落盘不广播，由**批次拥有者**结束时统一广播一次。
    Deferred,
}

/// 进度落点（注入式：生产走 `AppHandle` 广播，单测走记录器 → 「静默腿真的一帧不发」可被断言）。
///
/// 为什么不是直接在 `download_with_progress` 里写 `if silent { return }`：那样「静默」只能靠读代码
/// 相信，无法在无 `AppHandle` 的单测里证伪（本仓未引 `tauri::test`）。抽成 trait 后
/// [`RecordingSink`](tests) 能逐帧对账。
trait ProgressSink: Sync {
    fn emit(&self, frame: Value);
}

/// 生产落点：广播给渲染端。
struct BroadcastSink<'a> {
    app: &'a AppHandle,
    mode: ProgressMode,
}

impl ProgressSink for BroadcastSink<'_> {
    fn emit(&self, frame: Value) {
        if self.mode == ProgressMode::Silent {
            return; // 后台腿静默（上游 `if (silent) return`）
        }
        broadcast(self.app, EVENT_RULE_RESOURCE_PROGRESS, frame);
    }
}

/// 发一帧下载进度（`EVENT_RULE_RESOURCE_PROGRESS` → 前端 `ruleResources.onProgress`）。
///
/// `id`/`name` 由 plan 补齐，调用方只给阶段字段（前端 `RuleResourceProgress` 契约）。
fn emit_resource_progress(sink: &dyn ProgressSink, plan: &ResourcePlan, mut frame: Value) {
    if let Some(obj) = frame.as_object_mut() {
        obj.insert("id".into(), json!(plan.id));
        obj.insert("name".into(), json!(plan.name));
    }
    sink.emit(frame);
}

/// [`download_and_store`] + 逐阶段广播进度。下载族的**唯一**入口（三个 command 都走它）。
///
/// 此前 `EVENT_RULE_RESOURCE_PROGRESS` 全仓零 emit：下载是真的、落盘是真的，但前端 `onProgress`
/// 永不触发 → 资源页既无进度、下完也不刷新（列表停在旧 size/时间，用户以为没下成又点一次）。
///
/// # 为什么 downloading 帧的 percent 是 null（**不是**忘了填）
///
/// 底层 `safe_redirect_fetch` 返回的是**已缓冲完的 `resp.body`**（SSRF/重定向/体积 guard 都建立在
/// 「整体收完再判」上），没有字节流可数 —— 真实百分比要改 net-stack 的传输层，非本处能力。故如实报
/// `percent: null`：前端 `ResRow` 对 `percent == null` 走 spinner 分支（不画进度条），是契约内的
/// 已有降级态。**宁可没有进度条，也不编一个匀速爬升的假条。**
/// `done` 帧的 `received`/`total` 是真值（落盘字节数），故 percent 报 100 不算伪造。
///
/// # 取消
///
/// 进入下载前把一个 oneshot 发送端登记进 [`cancel_registry`]（键 = 单调自增 seq，值 = `(资源 id, tx)`），
/// 与真实下载 future `select!`。[`rule_resources_cancel`] 按资源 id 取出全部在途条目并 `send(())` →
/// 本处 future 被**丢弃**（reqwest 连接随之中止，真中断而非「标记为取消后继续下载完」），返回
/// [`DownloadOutcome::Cancelled`]、发一帧 `status:"cancelled"`、**不落盘不入册**。
async fn download_with_progress<H: HttpClient, L: DnsLookup>(
    sink: &dyn ProgressSink,
    client: &H,
    lookup: &L,
    plan: &ResourcePlan,
    res_dir: &std::path::Path,
) -> DownloadOutcome {
    emit_resource_progress(
        sink,
        plan,
        json!({ "received": 0, "total": null, "percent": null, "status": "downloading" }),
    );

    let (seq, mut cancel_rx) = register_cancellable(&plan.id);
    let fut = download_and_store(client, lookup, plan, res_dir);
    tokio::pin!(fut);
    let outcome = tokio::select! {
        o = &mut fut => o,
        r = &mut cancel_rx => {
            if r.is_ok() {
                DownloadOutcome::Cancelled
            } else {
                // 发送端在未 send 的情况下被 drop（本设计下不可达：条目只由 cancel 取走或由下方
                // unregister 在 select 结束后清理）→ 保守继续等下载，绝不谎报「已取消」。
                fut.await
            }
        }
    };
    unregister_cancellable(seq);

    match &outcome {
        DownloadOutcome::Stored { resource, .. } => emit_resource_progress(
            sink,
            plan,
            json!({
                "received": resource.size,
                "total": resource.size,
                "percent": 100.0,
                "status": "done",
            }),
        ),
        DownloadOutcome::Failed { message, code } => emit_resource_progress(
            sink,
            plan,
            json!({
                "received": 0,
                "total": null,
                "percent": null,
                "status": "error",
                "error": message,
                "errorCode": code,
            }),
        ),
        DownloadOutcome::Cancelled => emit_resource_progress(
            sink,
            plan,
            json!({
                "received": 0,
                "total": null,
                "percent": null,
                "status": "cancelled",
            }),
        ),
    }
    outcome
}

// ── 下载取消登记表 ─────────────────────────────────────────────────────────────
//
// 为什么用「seq → (id, tx)」而不是「id → tx」：同一 id 可能有两条在途下载（用户在资源页点「更新」
// 的同时后台调度腿正好也选中它）。以 id 为键会让后者覆盖前者的发送端 → 被覆盖的那条永远取消不掉，
// 且其 receiver 会因 sender 被 drop 而收到 `Err`（若把 `Err` 当取消处理，就是**谎报取消**）。
// seq 键保证登记只增不覆盖，取消按 id 扫全表逐条 send。

type CancelRegistry =
    Mutex<std::collections::HashMap<u64, (String, tokio::sync::oneshot::Sender<()>)>>;

fn cancel_registry() -> &'static CancelRegistry {
    static REG: OnceLock<CancelRegistry> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// 登记一条可取消的在途下载，返回 `(seq, 取消接收端)`。
fn register_cancellable(id: &str) -> (u64, tokio::sync::oneshot::Receiver<()>) {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = tokio::sync::oneshot::channel();
    if let Ok(mut reg) = cancel_registry().lock() {
        reg.insert(seq, (id.to_string(), tx));
    }
    (seq, rx)
}

/// 下载结束（成功/失败/已取消）后摘掉自己的登记条目。
fn unregister_cancellable(seq: u64) {
    if let Ok(mut reg) = cancel_registry().lock() {
        reg.remove(&seq);
    }
}

/// 取消该 id 的全部在途下载，返回**实际被中止的条数**（0 = 当时没有在途下载，如实回报，不假装成功）。
fn cancel_inflight(id: &str) -> usize {
    let Ok(mut reg) = cancel_registry().lock() else {
        return 0;
    };
    let seqs: Vec<u64> = reg
        .iter()
        .filter(|(_, (rid, _))| rid == id)
        .map(|(seq, _)| *seq)
        .collect();
    let mut n = 0;
    for seq in seqs {
        if let Some((_, tx)) = reg.remove(&seq) {
            // send 失败 = 对端已走完（竞态：下载刚好在同一瞬完成）→ 不计数，别虚报。
            if tx.send(()).is_ok() {
                n += 1;
            }
        }
    }
    n
}

/// 上游 `RULE_RESOURCES_CANCEL`：中止该资源的在途下载。
///
/// 返回 `{ cancelled: n }`——`n` 是**真被中止**的在途下载条数。资源当时不在下载中 → `n = 0`
/// （诚实：按钮点了没有可取消的东西，不伪造成功）。取消的资源不落盘、不入册，磁盘上的旧副本保持不变。
#[tauri::command]
pub fn rule_resources_cancel(id: String) -> ApiResponse<Value> {
    let n = cancel_inflight(&id);
    ApiResponse::ok(json!({ "cancelled": n }))
}

/// 把成功下载的资源 upsert 进 `config.ruleResources`（按 id 覆盖/追加），保存 + 广播 `config:changed`。
///
/// 文件已在盘上；若 config 保存失败，如实 log（资源暂成孤儿，下次 list 不显示）——不静默吞。
fn persist_resources(
    app: &AppHandle,
    state: &AppRuntime,
    downloaded: &[RuleResource],
    broadcast: BroadcastMode,
) {
    if downloaded.is_empty() {
        return;
    }
    let mut cfg = match state.config().load_full() {
        Ok(c) => c,
        Err(e) => {
            log::error!("规则资源已下载但加载 config 失败（未登记）: {e}");
            return;
        }
    };
    upsert_rule_resources(&mut cfg, downloaded);
    match state.config().save_full(&cfg) {
        // Deferred：批次拥有者收尾统一广播（见 [`BroadcastMode`]）。落盘已完成，跳过的只是通知。
        Ok(()) => {
            if broadcast == BroadcastMode::Immediate {
                broadcast_config_changed(app, &cfg);
            }
        }
        Err(e) => log::error!("规则资源已下载但保存 config 失败（未登记）: {e}"),
    }
}

/// upsert：按 id 覆盖既有项，否则追加。
fn upsert_rule_resources(cfg: &mut Value, downloaded: &[RuleResource]) {
    let Some(obj) = cfg.as_object_mut() else {
        return;
    };
    let entry = obj
        .entry("ruleResources")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(arr) = entry.as_array_mut() else {
        return;
    };
    for r in downloaded {
        let Ok(val) = serde_json::to_value(r) else {
            continue;
        };
        if let Some(idx) = arr
            .iter()
            .position(|e| e.get("id").and_then(Value::as_str) == Some(r.id.as_str()))
        {
            arr[idx] = val;
        } else {
            arr.push(val);
        }
    }
}

#[cfg(test)]
mod reorder_tests {
    use super::*;

    fn rules(ids: &[&str]) -> Vec<Value> {
        ids.iter()
            .map(|id| json!({ "id": id, "type": "domain", "values": ["x"], "action": "proxy", "enabled": true }))
            .collect()
    }

    fn ids_of(rules: &[Value]) -> Vec<&str> {
        rules
            .iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .collect()
    }

    fn want(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    /// 真变化 → 按 orderedIds 逐位重排（规则体随 id 一起搬，不只是搬 id）。
    #[test]
    fn real_permutation_reorders_bodies() {
        let cur = rules(&["a", "b", "c"]);
        let out = plan_reorder(&cur, &want(&["c", "a", "b"]))
            .expect("合法排列")
            .expect("顺序真变了，须返回新序列");
        assert_eq!(ids_of(&out), vec!["c", "a", "b"]);
        // 搬的是整条规则不是裸 id。
        assert_eq!(out[0]["type"], "domain");
    }

    /// **净零序 → `Ok(None)`（跳过 save + 广播）**，契约 §Rules「净零序跳过 save」。
    ///
    /// **变异锁**：把 `plan_reorder` 里的 `if unchanged { return Ok(None) }` 删掉（= 退回「恒 save」）
    /// → 本断言拿到 `Some(..)` 转红。仅断言「排列合法」不足以杀掉这个变异，故必须断言 `is_none`。
    #[test]
    fn identical_order_is_net_zero_and_skips_save() {
        let cur = rules(&["a", "b", "c"]);
        assert!(
            plan_reorder(&cur, &want(&["a", "b", "c"]))
                .expect("合法排列")
                .is_none(),
            "逐位相同的顺序必须短路，不得落盘"
        );
        // 空规则集 + 空请求也是净零序（前端在空列表上误发一次 reorder 不该触发整核评估）。
        assert!(plan_reorder(&[], &[]).expect("空集合法").is_none());
    }

    /// 只挪了一位也算真变化（净零判据是**逐位序列相等**，不是集合相等 —— 集合恒相等）。
    ///
    /// **变异锁**：把净零判据写成「集合相等」→ 本断言会拿到 `None` 转红。
    #[test]
    fn single_swap_is_not_net_zero() {
        let cur = rules(&["a", "b"]);
        let out = plan_reorder(&cur, &want(&["b", "a"]))
            .expect("合法排列")
            .expect("换位 = 真变化，必须落盘");
        assert_eq!(ids_of(&out), vec!["b", "a"]);
    }

    /// 非法入参三态：长度不符 / 有重复 / 含未知 id —— 都 Err，且**不得**被净零短路吞掉。
    #[test]
    fn rejects_non_permutations() {
        let cur = rules(&["a", "b", "c"]);
        assert!(plan_reorder(&cur, &want(&["a", "b"])).is_err(), "长度不符");
        assert!(
            plan_reorder(&cur, &want(&["a", "a", "b"])).is_err(),
            "有重复 id"
        );
        assert!(
            plan_reorder(&cur, &want(&["a", "b", "ghost"])).is_err(),
            "含未知 id"
        );
    }

    /// 现序里有畸形条目（缺 `id`）时不得误判净零 —— 否则那条坏数据永远修不回来。
    #[test]
    fn malformed_current_entry_is_not_net_zero() {
        let mut cur = rules(&["a", "b"]);
        cur[0] = json!({ "type": "domain" }); // 缺 id
                                              // 长度仍为 2，但 by_id 只认得 "b" → "a" 属未知 id。
        assert!(plan_reorder(&cur, &want(&["a", "b"])).is_err());
    }

    /// **接线变异锁**（测方法体 ≠ 测接线）：上面全部断言测的是 `plan_reorder` 这个纯函数。
    /// 把命令壳里的 `Ok(None) => return ok_void()` 改回「照常 save + 广播」，它们**一条都不会红**
    /// —— 而那正是本条 review 点名的假绿：净零序短路的收益（省一轮整核评估 + 一次全量 config 广播）
    /// 全在命令壳那一行上。
    ///
    /// 命令壳带 `State<AppRuntime>`、本仓未引 `tauri::test` → 按本仓既有源码扫描门钉调用点
    /// （同 `runtime/rule_resource_scheduler.rs::catalog_leg_cannot_short_circuit_the_resource_leg`）。
    #[test]
    fn command_shell_short_circuits_on_net_zero_order() {
        const SRC: &str = include_str!("rules.rs");
        // 只扫**生产正文**：签名串本身也写在本用例里，全文搜索会自指命中本模块 → 生产代码
        // 改坏了断言照样绿（同 `commands/subscription.rs::wiring_gate` 那份自检的理由）。
        let prod = &SRC[..SRC.find("mod reorder_tests {").expect("本模块自身必在")];
        let start = prod
            .find("pub fn rules_reorder(")
            .expect("rules_reorder 仍在");
        let body = &prod[start..];
        let body = &body[..body.find("\n}").map_or(body.len(), |p| p + 2)];
        assert!(
            body.contains("Ok(None) => return ok_void()"),
            "变异锁：净零序必须在命令壳里**直接返回**，不得落到 save_full/broadcast"
        );
        // 且短路必须发生在落盘之前（顺序颠倒 = 短路形同虚设）。
        let short = body.find("Ok(None) => return ok_void()").unwrap();
        let save = body.find("save_full(").expect("落盘腿仍在");
        assert!(short < save, "净零短路必须排在 save 之前");
    }
}

#[cfg(test)]
mod gh_proxy_tests {
    use super::*;

    const PREFIX: &str = "https://gh-proxy.org/";
    const RAW: &str =
        "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/youtube.srs";

    /// 有前缀 + GitHub 域 → 拼成 `<prefix 去尾斜杠>/<完整原 URL>`（与 `CoreDownloader::candidates` 同口径）。
    #[test]
    fn applies_prefix_to_github_hosts() {
        assert_eq!(
            apply_gh_proxy(PREFIX, RAW).as_deref(),
            Some("https://gh-proxy.org/https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/youtube.srs")
        );
        // 前缀不带尾斜杠 / 带多余空白 → 结果一致（不出现 `//` 也不丢分隔符）。
        assert_eq!(
            apply_gh_proxy("  https://gh-proxy.org  ", RAW),
            apply_gh_proxy(PREFIX, RAW)
        );
        // raw.githubusercontent.com **不在** updater 那张 2 域名 release 资产表里，必须由本表覆盖，
        // 否则规则资源（唯一默认源就是它）恒不加速 —— 本任务的核心断言。
        assert!(apply_gh_proxy(PREFIX, RAW).is_some());
        for host in [
            "github.com",
            "codeload.github.com",
            "gist.githubusercontent.com",
        ] {
            assert!(
                apply_gh_proxy(PREFIX, &format!("https://{host}/a/b.srs")).is_some(),
                "{host} 应可加速"
            );
        }
    }

    /// 无前缀 / 非 GitHub 域 / URL 不可解析 → None（原样直连，绝不把非 GitHub 地址塞给加速器）。
    #[test]
    fn skips_when_no_prefix_or_not_github() {
        assert_eq!(apply_gh_proxy("", RAW), None, "空前缀 = 不加速");
        assert_eq!(apply_gh_proxy("   ", RAW), None, "纯空白前缀 = 不加速");
        assert_eq!(
            apply_gh_proxy(PREFIX, "https://example.com/my.srs"),
            None,
            "非 GitHub 域不得套加速前缀"
        );
        assert_eq!(
            apply_gh_proxy(PREFIX, "https://raw.githubusercontent.com.evil.tld/x.srs"),
            None,
            "同后缀的钓鱼域名不得命中（须整串等值比对 host）"
        );
        assert_eq!(apply_gh_proxy(PREFIX, "not a url"), None);
    }

    /// plan 阶段套前缀：**`fetch_url` 变、`url` 不变**（`url` 会持久化成 `sourceUrl`）。
    ///
    /// **变异锁**：若把 `with_gh_proxy` 改成就地改写 `self.url`（= 把镜像址写进 config），
    /// 下面 `plan.url` 的断言转红。
    #[test]
    fn plan_carries_mirror_in_fetch_url_only() {
        let plan = plan_from_item(&json!({ "catalogId": "geosite-youtube" }), &[])
            .expect("catalog 条目应解析")
            .with_gh_proxy(PREFIX);
        assert_eq!(plan.url, RAW, "登记用的 sourceUrl 必须保持原址");
        assert_eq!(
            plan.fetch_url,
            format!("https://gh-proxy.org/{RAW}"),
            "本次请求须走镜像"
        );
    }

    /// 无前缀（默认态）：`fetch_url == url`，行为与接线前逐字一致（不给未配置加速的用户引入变化）。
    #[test]
    fn plan_without_prefix_is_identity() {
        let plan = plan_from_item(&json!({ "catalogId": "geosite-youtube" }), &[])
            .expect("catalog 条目应解析")
            .with_gh_proxy("");
        assert_eq!(plan.fetch_url, plan.url);
        assert_eq!(plan.url, RAW);
    }

    /// 已登记资源（redownload / update_all 腿）同样套前缀，且 `sourceUrl` 原址不被改写。
    #[test]
    fn registered_resource_plan_also_mirrors() {
        let r = RuleResource {
            id: "geosite-youtube".into(),
            name: "YouTube".into(),
            category: "geosite".into(),
            source_url: RAW.into(),
            file_name: "geosite-youtube.srs".into(),
            format: RuleResourceFormat::Binary,
            size: 1,
            downloaded_at: "t".into(),
        };
        let plan = plan_from_resource(&r).with_gh_proxy(PREFIX);
        assert_eq!(plan.url, RAW);
        assert!(plan.fetch_url.starts_with("https://gh-proxy.org/"));
        // 自定义（非 GitHub）源不受影响。
        let mut custom = r.clone();
        custom.source_url = "https://cdn.example.com/x.srs".into();
        let p2 = plan_from_resource(&custom).with_gh_proxy(PREFIX);
        assert_eq!(p2.fetch_url, p2.url);
    }
}

#[cfg(test)]
mod resource_delete_tests {
    use super::*;

    fn cfg_with_resource() -> Value {
        json!({
            // servers 是 UserConfig 必填键（无 serde default）——真实 config 恒有；测试须显式给。
            "servers": [],
            "ruleResources": [{
                "id": "res_a", "name": "A", "category": "custom",
                "sourceUrl": "https://e/a.srs", "fileName": "res_a.srs",
                "format": "binary", "size": 1, "downloadedAt": "t"
            }],
            "customRules": [],
            "appRules": [],
        })
    }

    #[test]
    fn plan_delete_missing_is_idempotent_notfound() {
        let cfg = cfg_with_resource();
        assert!(matches!(
            plan_resource_delete(&cfg, "ghost", false),
            ResourceDeletePlan::NotFound
        ));
    }

    #[test]
    fn plan_delete_unreferenced_proceeds_with_filename() {
        let cfg = cfg_with_resource();
        match plan_resource_delete(&cfg, "res_a", false) {
            ResourceDeletePlan::Proceed { file_name } => {
                assert_eq!(
                    file_name.as_deref(),
                    Some("res_a.srs"),
                    "须带缓存文件名供解绑"
                );
            }
            other => panic!("无引用资源应可直接删，实得: {other:?}"),
        }
    }

    #[test]
    fn plan_delete_referenced_needs_confirm_unless_forced() {
        // 一条已启用 ruleSet 规则引用 res:res_a（mirror 形态，conditions 缺省回落 type+values）。
        let mut cfg = cfg_with_resource();
        cfg["customRules"] = json!([{
            "id": "r1", "type": "ruleSet", "values": ["res:res_a"],
            "action": "proxy", "enabled": true
        }]);
        match plan_resource_delete(&cfg, "res_a", false) {
            ResourceDeletePlan::NeedConfirm(refs) => {
                assert!(
                    !refs.is_empty(),
                    "被引用须回 needConfirm + referencingRules"
                );
                assert_eq!(refs[0].id, "r1");
            }
            other => panic!("被引用且未 force 应 needConfirm，实得: {other:?}"),
        }
        // force=true → 覆盖确认，直接 Proceed。
        assert!(matches!(
            plan_resource_delete(&cfg, "res_a", true),
            ResourceDeletePlan::Proceed { .. }
        ));
        // 已禁用的引用规则不算引用（enumerate 只扫已启用）→ 可直接删。
        cfg["customRules"] = json!([{
            "id": "r1", "type": "ruleSet", "values": ["res:res_a"],
            "action": "proxy", "enabled": false
        }]);
        assert!(matches!(
            plan_resource_delete(&cfg, "res_a", false),
            ResourceDeletePlan::Proceed { .. }
        ));
    }
}

#[cfg(test)]
mod resource_download_tests {
    use super::*;

    #[test]
    fn plan_from_catalog_id_resolves_mrd_url_and_filename() {
        let item = json!({ "catalogId": "geosite-youtube" });
        let plan = plan_from_item(&item, &[]).expect("catalog 条目应解析");
        assert_eq!(plan.id, "geosite-youtube");
        assert_eq!(plan.category, "geosite");
        assert_eq!(plan.format, RuleResourceFormat::Binary);
        assert_eq!(plan.file_name, "geosite-youtube.srs");
        assert_eq!(
            plan.url,
            "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/youtube.srs"
        );
    }

    #[test]
    fn plan_from_unknown_catalog_id_errs() {
        let item = json!({ "catalogId": "geosite-nonexistent" });
        assert!(plan_from_item(&item, &[]).is_err());
    }

    #[test]
    fn plan_from_url_infers_format_name_and_filename() {
        let item = json!({ "url": "https://example.com/lists/my-rules.json" });
        let plan = plan_from_item(&item, &[]).expect("url 应解析");
        assert_eq!(plan.format, RuleResourceFormat::Source);
        assert_eq!(plan.name, "my-rules");
        assert!(
            plan.file_name.ends_with(".json"),
            "实得: {}",
            plan.file_name
        );
        assert_eq!(plan.category, "custom");
        // .srs 默认 binary。
        let srs = plan_from_item(&json!({ "url": "https://example.com/geoip-cn.srs", "name": "cn", "category": "geoip" }), &[])
            .unwrap();
        assert_eq!(srs.format, RuleResourceFormat::Binary);
        assert_eq!(srs.name, "cn");
        assert_eq!(srs.category, "geoip");
        assert!(srs.file_name.ends_with(".srs"));
    }

    #[test]
    fn plan_rejects_non_http_and_empty_items() {
        assert!(plan_from_item(&json!({ "url": "file:///etc/passwd" }), &[]).is_err());
        assert!(plan_from_item(&json!({ "url": "ftp://x/y.srs" }), &[]).is_err());
        assert!(plan_from_item(&json!({}), &[]).is_err());
        assert!(plan_from_item(&json!({ "name": "x" }), &[]).is_err());
    }

    #[test]
    fn sanitize_stem_blocks_traversal_and_separators() {
        assert_eq!(sanitize_file_stem("geosite-youtube"), "geosite-youtube");
        assert!(!sanitize_file_stem("../../etc/passwd").contains(".."));
        assert!(!sanitize_file_stem("a/b\\c").contains('/'));
        assert!(!sanitize_file_stem("a/b\\c").contains('\\'));
        assert_eq!(sanitize_file_stem(""), "_");
    }

    #[test]
    fn validate_bytes_enforces_srs_magic_and_json_object() {
        // binary: 需 SRS 魔数。
        assert!(validate_resource_bytes(RuleResourceFormat::Binary, b"SRS\x01\x02").is_ok());
        assert!(validate_resource_bytes(RuleResourceFormat::Binary, b"<html>").is_err());
        assert!(validate_resource_bytes(RuleResourceFormat::Binary, b"").is_err());
        // source: 需 JSON 对象。
        assert!(validate_resource_bytes(
            RuleResourceFormat::Source,
            br#"{"version":1,"rules":[]}"#
        )
        .is_ok());
        assert!(validate_resource_bytes(RuleResourceFormat::Source, b"[1,2,3]").is_err());
        assert!(validate_resource_bytes(RuleResourceFormat::Source, b"not json").is_err());
    }

    #[test]
    fn upsert_replaces_by_id_and_appends_new() {
        let mut cfg = json!({ "ruleResources": [
            { "id": "geosite-cn", "name": "old", "category": "geosite", "sourceUrl": "u", "fileName": "geosite-cn.srs", "format": "binary", "size": 1, "downloadedAt": "t" }
        ]});
        let updated = RuleResource {
            id: "geosite-cn".into(),
            name: "new".into(),
            category: "geosite".into(),
            source_url: "u2".into(),
            file_name: "geosite-cn.srs".into(),
            format: RuleResourceFormat::Binary,
            size: 99,
            downloaded_at: "t2".into(),
        };
        let added = RuleResource {
            id: "geoip-us".into(),
            name: "us".into(),
            category: "geoip".into(),
            source_url: "u3".into(),
            file_name: "geoip-us.srs".into(),
            format: RuleResourceFormat::Binary,
            size: 5,
            downloaded_at: "t3".into(),
        };
        upsert_rule_resources(&mut cfg, &[updated, added]);
        let arr = cfg["ruleResources"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "同 id 覆盖不新增，新 id 追加");
        let cn = arr.iter().find(|r| r["id"] == "geosite-cn").unwrap();
        assert_eq!(cn["name"], "new");
        assert_eq!(cn["size"], 99);
        assert!(arr.iter().any(|r| r["id"] == "geoip-us"));
    }

    // ── P2：current_iso 须产出合法 ISO（旧实现把 epoch 秒塞进秒字段 → 非法）──

    #[test]
    fn current_iso_produces_valid_epoch_not_buggy_seconds_field() {
        // 已知 epoch 1_700_000_000s（2023-11-14T22:13:20Z）→ 精确 ISO（毫秒精度）。
        assert_eq!(
            polaris_stats_engine::created_at_to_rfc3339(1_700_000_000_000),
            Some("2023-11-14T22:13:20.000Z".to_string())
        );
        // current_iso() 现产出合法 ISO：绝不再是旧 bug 的「1970-01-01T00:00:<整个 epoch 秒>Z」。
        let now = current_iso();
        assert!(
            !now.starts_with("1970-01-01T00:00:"),
            "current_iso 不得把 epoch 秒塞进秒字段（旧 bug），实得: {now}"
        );
        assert!(
            now.ends_with('Z') && now.len() >= 20,
            "current_iso 须为合法 ISO，实得: {now}"
        );
    }

    // ── P3：plan_from_resource 须在信任边界清洗被篡改的 fileName ──

    #[test]
    fn plan_from_resource_sanitizes_tampered_file_name() {
        let res_dir = std::path::Path::new("/home/u/.config/polaris/rule-resource");
        let mk = |file_name: &str| RuleResource {
            id: "x".into(),
            name: "x".into(),
            category: "c".into(),
            source_url: "https://e/x.srs".into(),
            file_name: file_name.into(),
            format: RuleResourceFormat::Binary,
            size: 1,
            downloaded_at: "t".into(),
        };

        // 相对穿越 `../../.bashrc`：穿越序列 + 分隔符须被清除。
        let plan = plan_from_resource(&mk("../../.bashrc"));
        assert!(
            !plan.file_name.contains(".."),
            "穿越序列须消除: {}",
            plan.file_name
        );
        assert!(
            !plan.file_name.contains('/'),
            "分隔符须清除: {}",
            plan.file_name
        );

        // 绝对路径 `/etc/cron.d/evil`：Path::join(绝对) 会整段替换 → 逃逸（旧行为）。清洗后须仍落在 res_dir 内。
        let plan_abs = plan_from_resource(&mk("/etc/cron.d/evil"));
        assert!(
            !plan_abs.file_name.starts_with('/'),
            "不得保留绝对路径前导斜杠"
        );
        let dest = res_dir.join(&plan_abs.file_name);
        assert!(
            dest.starts_with(res_dir),
            "绝对 fileName 清洗后须仍在资源目录内，实得: {dest:?}"
        );

        // 合法 fileName 幂等（不破坏正常重下载）。
        assert_eq!(
            plan_from_resource(&mk("geosite-cn.srs")).file_name,
            "geosite-cn.srs"
        );
    }

    // ── P8：redownload / update_all 区分「不在册」与「在册但结构非法」，不静默丢弃坏项 ──

    #[test]
    fn resolve_registered_resource_distinguishes_missing_malformed_and_ok() {
        let arr = vec![
            json!({ "id": "geosite-cn", "name": "CN", "category": "geosite", "sourceUrl": "https://e/cn.srs", "fileName": "geosite-cn.srs", "format": "binary", "size": 1, "downloadedAt": "t" }),
            // 结构非法：缺 sourceUrl/fileName/size/downloadedAt。
            json!({ "id": "broken", "name": "B", "format": "binary" }),
        ];
        // 命中且合法 → Ok。
        assert_eq!(
            resolve_registered_resource(&arr, "geosite-cn").unwrap().id,
            "geosite-cn"
        );
        // 不在册 → NOT_FOUND。
        let missing = resolve_registered_resource(&arr, "ghost").expect_err("不在册");
        assert_eq!(missing["errorCode"], ERR_RESOURCE_NOT_FOUND);
        assert_eq!(missing["ok"], false);
        // 在册但结构非法 → BAD_ITEM（**非 NOT_FOUND**，P8 修复点）；保留 id 供前端定位。
        let malformed = resolve_registered_resource(&arr, "broken").expect_err("结构非法");
        assert_eq!(malformed["errorCode"], ERR_RESOURCE_BAD_ITEM);
        assert_ne!(malformed["errorCode"], ERR_RESOURCE_NOT_FOUND);
        assert_eq!(malformed["id"], "broken");
    }

    #[test]
    fn parse_resource_entry_flags_malformed_as_failed_item() {
        // update_all 据它把坏条目报成失败项（旧 filter_map(.ok()) 会静默丢弃 → 既不更新也不出现在结果里）。
        let good = json!({ "id": "ok", "name": "OK", "category": "c", "sourceUrl": "https://e/a.srs", "fileName": "ok.srs", "format": "binary", "size": 1, "downloadedAt": "t" });
        assert!(parse_resource_entry(&good).is_ok());
        let bad = json!({ "id": "b", "name": "B" });
        let err = parse_resource_entry(&bad).expect_err("缺字段应判结构非法");
        assert_eq!(err["errorCode"], ERR_RESOURCE_BAD_ITEM);
        assert_eq!(err["ok"], false);
        assert_eq!(err["id"], "b");
        assert_eq!(err["name"], "B");
    }

    // ── 回环真 socket 门（真 reqwest 打回环，不碰宿主网络；对齐 subscription production_gate）──

    mod loopback {
        use super::*;
        use std::future::Future;
        use std::io::{Read, Write};
        use std::net::{SocketAddr, TcpListener};
        use std::thread;

        use crate::runtime::http::HttpRuntime;

        fn spawn_once(status_line: &'static str, body: Vec<u8>) -> SocketAddr {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
            let addr = listener.local_addr().expect("取端口");
            thread::spawn(move || {
                if let Ok((mut sock, _)) = listener.accept() {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf);
                    let mut resp = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    resp.extend_from_slice(&body);
                    let _ = sock.write_all(&resp);
                    let _ = sock.flush();
                }
            });
            addr
        }

        /// mock DnsLookup：把 hostname 钉到指定 IP（放行/拒绝由 IP 是否内网决定）。
        struct FixedLookup(&'static str);
        impl DnsLookup for FixedLookup {
            fn lookup_all(
                &self,
                _host: &str,
            ) -> impl Future<Output = Result<Vec<String>, String>> + Send {
                let ip = self.0.to_string();
                async move { Ok(vec![ip]) }
            }
        }

        #[tokio::test]
        async fn fetches_srs_over_loopback_and_validates_magic() {
            let mut srs = b"SRS".to_vec();
            srs.extend_from_slice(&[0x01, 0x00, 0xde, 0xad]);
            let addr = spawn_once("200 OK", srs);
            // 真 client，DNS 钉定：传输落回环 server；guard 判定对象是公网 IP → 放行（guard 真跑）。
            let client = HttpRuntime::with_resolve_overrides(&[("res.example.com", addr)]).unwrap();
            let lookup = FixedLookup("93.184.216.34");
            let bytes = fetch_resource_bytes(
                &client,
                &lookup,
                "http://res.example.com/geosite-cn.srs",
                RuleResourceFormat::Binary,
            )
            .await
            .expect("回环 SRS 下载应成功");
            assert_eq!(&bytes[..3], b"SRS");
        }

        #[tokio::test]
        async fn rejects_non_srs_body_for_binary() {
            let addr = spawn_once("200 OK", b"<html>not a rule set</html>".to_vec());
            let client = HttpRuntime::with_resolve_overrides(&[("res.example.com", addr)]).unwrap();
            let lookup = FixedLookup("93.184.216.34");
            let err = fetch_resource_bytes(
                &client,
                &lookup,
                "http://res.example.com/x.srs",
                RuleResourceFormat::Binary,
            )
            .await
            .expect_err("非 SRS 内容必须被魔数校验拒");
            assert!(
                err.contains("SRS") || err.contains("srs"),
                "错误应点明魔数，实得: {err}"
            );
        }

        #[tokio::test]
        async fn non_2xx_status_is_error() {
            let addr = spawn_once("404 Not Found", b"nope".to_vec());
            let client = HttpRuntime::with_resolve_overrides(&[("res.example.com", addr)]).unwrap();
            let lookup = FixedLookup("93.184.216.34");
            let err = fetch_resource_bytes(
                &client,
                &lookup,
                "http://res.example.com/x.srs",
                RuleResourceFormat::Binary,
            )
            .await
            .expect_err("404 必须失败");
            assert!(err.contains("404"), "实得: {err}");
        }

        #[tokio::test]
        async fn ssrf_guard_blocks_internal_ip_on_production_path() {
            let addr = spawn_once("200 OK", b"SRS\x01".to_vec());
            let client = HttpRuntime::with_resolve_overrides(&[("res.example.com", addr)]).unwrap();
            // hostname 解析到云元数据地址（内网）→ guard 必拒（防 SSRF）。
            let lookup = FixedLookup("169.254.169.254");
            let err = fetch_resource_bytes(
                &client,
                &lookup,
                "http://res.example.com/x.srs",
                RuleResourceFormat::Binary,
            )
            .await
            .expect_err("内网 IP 必须被 SSRF guard 拒");
            assert!(!err.is_empty(), "SSRF 拒绝须带原因");
        }

        #[tokio::test]
        async fn download_and_store_writes_file_and_reports_existed_before() {
            let mut srs = b"SRS".to_vec();
            srs.extend_from_slice(&[0x07, 0x08]);
            let addr = spawn_once("200 OK", srs.clone());
            let client = HttpRuntime::with_resolve_overrides(&[("res.example.com", addr)]).unwrap();
            let lookup = FixedLookup("93.184.216.34");
            let dir = std::env::temp_dir().join(format!("polaris-resdl-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let plan = ResourcePlan {
                id: "geosite-test".into(),
                name: "test".into(),
                category: "geosite".into(),
                url: "http://res.example.com/geosite-test.srs".into(),
                fetch_url: "http://res.example.com/geosite-test.srs".into(),
                file_name: "geosite-test.srs".into(),
                format: RuleResourceFormat::Binary,
            };
            let outcome = download_and_store(&client, &lookup, &plan, &dir).await;
            match outcome {
                DownloadOutcome::Stored {
                    resource,
                    existed_before,
                } => {
                    assert!(!existed_before, "首次下载 existedBefore 应为 false");
                    assert_eq!(resource.size, srs.len() as u64);
                    let landed = std::fs::read(dir.join("geosite-test.srs")).expect("文件应落盘");
                    assert_eq!(landed, srs, "落盘字节须与下载字节一致");
                }
                DownloadOutcome::Failed { message, .. } => panic!("应成功，实得: {message}"),
                DownloadOutcome::Cancelled => panic!("未取消却报了取消"),
            }
            let _ = std::fs::remove_dir_all(&dir);
        }

        // ── 进度可见性（后台腿静默）+ 下载取消 ──────────────────────────────────

        /// 记录式进度落点：把每帧存下来，供断言「静默腿真的一帧不发」。
        #[derive(Default)]
        struct RecordingSink {
            frames: std::sync::Mutex<Vec<Value>>,
        }
        impl RecordingSink {
            fn statuses(&self) -> Vec<String> {
                self.frames
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|f| f.get("status").and_then(Value::as_str).map(str::to_string))
                    .collect()
            }
        }
        impl ProgressSink for RecordingSink {
            fn emit(&self, frame: Value) {
                self.frames.lock().unwrap().push(frame);
            }
        }

        /// 生产落点的静默判定（`BroadcastSink` 的 `mode` 分支）套在记录器上验：Silent → 零帧。
        struct ModedRecordingSink {
            mode: ProgressMode,
            inner: RecordingSink,
        }
        impl ProgressSink for ModedRecordingSink {
            fn emit(&self, frame: Value) {
                if self.mode == ProgressMode::Silent {
                    return; // 与 BroadcastSink::emit 同一条判定
                }
                self.inner.emit(frame);
            }
        }

        fn plan_for(id: &str, addr: SocketAddr) -> ResourcePlan {
            let _ = addr;
            let url = format!("http://res.example.com/{id}.srs");
            ResourcePlan {
                id: id.into(),
                name: format!("test-{id}"),
                category: "geosite".into(),
                fetch_url: url.clone(),
                url,
                file_name: format!("{id}.srs"),
                format: RuleResourceFormat::Binary,
            }
        }

        fn tmp_res_dir(tag: &str) -> std::path::PathBuf {
            let d = std::env::temp_dir().join(format!(
                "polaris-res-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |x| x.as_nanos())
            ));
            let _ = std::fs::remove_dir_all(&d);
            d
        }

        /// 手动腿（`ProgressMode::Live`）：downloading + done 两帧齐发。
        ///
        /// **变异锁**：把 `download_with_progress` 开头那帧 `downloading` 删掉 → 本断言转红。
        #[tokio::test]
        async fn live_mode_emits_downloading_and_done_frames() {
            let mut srs = b"SRS".to_vec();
            srs.extend_from_slice(&[0x11, 0x22]);
            let addr = spawn_once("200 OK", srs);
            let client = HttpRuntime::with_resolve_overrides(&[("res.example.com", addr)]).unwrap();
            let sink = ModedRecordingSink {
                mode: ProgressMode::Live,
                inner: RecordingSink::default(),
            };
            let dir = tmp_res_dir("live");
            let plan = plan_for("live-res", addr);
            let outcome =
                download_with_progress(&sink, &client, &FixedLookup("93.184.216.34"), &plan, &dir)
                    .await;
            assert!(
                matches!(outcome, DownloadOutcome::Stored { .. }),
                "应下载成功"
            );
            assert_eq!(
                sink.inner.statuses(),
                vec!["downloading".to_string(), "done".to_string()],
                "手动腿必须逐阶段发帧"
            );
            // 帧内 id/name 由 plan 补齐（前端按 id 索引进度表，漏了就永远匹配不上行）。
            let first = sink.inner.frames.lock().unwrap()[0].clone();
            assert_eq!(first["id"], "live-res");
            assert_eq!(first["name"], "test-live-res");
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// 后台调度腿（`ProgressMode::Silent`）：**一帧都不发**，但下载本身照常完成并落盘。
        ///
        /// **变异锁（本轮要求的变异验证之一）**：把 `BroadcastSink::emit` /
        /// `ModedRecordingSink::emit` 里的 `if mode == Silent { return; }` 删掉（= 后台腿改回推事件）
        /// → 帧数变 2 → 本断言转红。
        #[tokio::test]
        async fn silent_mode_emits_nothing_but_still_downloads() {
            let mut srs = b"SRS".to_vec();
            srs.extend_from_slice(&[0x33, 0x44]);
            let addr = spawn_once("200 OK", srs.clone());
            let client = HttpRuntime::with_resolve_overrides(&[("res.example.com", addr)]).unwrap();
            let sink = ModedRecordingSink {
                mode: ProgressMode::Silent,
                inner: RecordingSink::default(),
            };
            let dir = tmp_res_dir("silent");
            let plan = plan_for("silent-res", addr);
            let outcome =
                download_with_progress(&sink, &client, &FixedLookup("93.184.216.34"), &plan, &dir)
                    .await;
            assert!(
                matches!(outcome, DownloadOutcome::Stored { .. }),
                "静默只影响事件，不影响下载本身"
            );
            assert!(
                sink.inner.statuses().is_empty(),
                "后台腿必须零帧，实得: {:?}",
                sink.inner.statuses()
            );
            let landed = std::fs::read(dir.join("silent-res.srs")).expect("静默腿仍须真落盘");
            assert_eq!(landed, srs);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// 失败帧走 `status:"error"`（与 cancelled 分流的对照组）。
        #[tokio::test]
        async fn failure_emits_error_frame_with_code() {
            let addr = spawn_once("500 Server Error", b"boom".to_vec());
            let client = HttpRuntime::with_resolve_overrides(&[("res.example.com", addr)]).unwrap();
            let sink = RecordingSink::default();
            let dir = tmp_res_dir("fail");
            let plan = plan_for("fail-res", addr);
            let outcome =
                download_with_progress(&sink, &client, &FixedLookup("93.184.216.34"), &plan, &dir)
                    .await;
            assert!(matches!(outcome, DownloadOutcome::Failed { .. }));
            assert_eq!(
                sink.statuses(),
                vec!["downloading".to_string(), "error".to_string()]
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        // ── gh 加速：镜像优先 + 失败回退原址（真 socket，两台回环 server）─────────────

        /// 加速前缀命中时**先打镜像**：镜像返有效 SRS → 直接成功，原址那台一次都不被碰。
        ///
        /// **变异锁**：把 `download_and_store` 的首发地址改回 `plan.url`（= 不套加速）→ 镜像 server
        /// 收不到请求、原址 server 被打，`landed` 内容变成 DIRECT 的字节 → 断言转红。
        #[tokio::test]
        async fn mirror_is_tried_first_when_gh_proxy_configured() {
            let mut mirror_body = b"SRS".to_vec();
            mirror_body.extend_from_slice(b"MIRROR");
            let mut direct_body = b"SRS".to_vec();
            direct_body.extend_from_slice(b"DIRECT");
            let mirror = spawn_once("200 OK", mirror_body.clone());
            let direct = spawn_once("200 OK", direct_body);
            let client = HttpRuntime::with_resolve_overrides(&[
                ("mirror.example.com", mirror),
                ("raw.githubusercontent.com", direct),
            ])
            .unwrap();
            let dir = tmp_res_dir("ghproxy-hit");
            let plan = gh_plan("gh-hit").with_gh_proxy("http://mirror.example.com/");
            assert_ne!(plan.fetch_url, plan.url, "前置条件：本例须真套上前缀");
            let outcome =
                download_and_store(&client, &FixedLookup("93.184.216.34"), &plan, &dir).await;
            assert!(
                matches!(outcome, DownloadOutcome::Stored { .. }),
                "镜像应下成功"
            );
            let landed = std::fs::read(dir.join("gh-hit.srs")).expect("须落盘");
            assert_eq!(
                landed, mirror_body,
                "落盘的必须是镜像返回的字节（= 走了镜像）"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// 镜像挂了（500）→ **自动回退原址**，仍拿到资源（设置页 `ghProxyHint` 明写的承诺）。
        ///
        /// **变异锁**：删掉 `download_and_store` 里 `if attempt.is_err() && fetch_url != url` 的回退腿
        /// → 结果变 `Failed`、文件不落盘 → 本断言转红。
        #[tokio::test]
        async fn falls_back_to_origin_when_mirror_fails() {
            let mut direct_body = b"SRS".to_vec();
            direct_body.extend_from_slice(b"DIRECT");
            let mirror = spawn_once("500 Server Error", b"boom".to_vec());
            let direct = spawn_once("200 OK", direct_body.clone());
            let client = HttpRuntime::with_resolve_overrides(&[
                ("mirror.example.com", mirror),
                ("raw.githubusercontent.com", direct),
            ])
            .unwrap();
            let dir = tmp_res_dir("ghproxy-fallback");
            let plan = gh_plan("gh-fb").with_gh_proxy("http://mirror.example.com/");
            let outcome =
                download_and_store(&client, &FixedLookup("93.184.216.34"), &plan, &dir).await;
            assert!(
                matches!(outcome, DownloadOutcome::Stored { .. }),
                "镜像失败须回退原址，不得直接判失败"
            );
            let landed = std::fs::read(dir.join("gh-fb.srs")).expect("回退腿也须真落盘");
            assert_eq!(landed, direct_body);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// 未配加速时**不重试**：原址失败即失败（同址重试无意义，别把一次失败变成两次超时）。
        #[tokio::test]
        async fn no_prefix_means_single_attempt() {
            let direct = spawn_once("500 Server Error", b"boom".to_vec());
            let client =
                HttpRuntime::with_resolve_overrides(&[("raw.githubusercontent.com", direct)])
                    .unwrap();
            let dir = tmp_res_dir("ghproxy-none");
            let plan = gh_plan("gh-none").with_gh_proxy("");
            assert_eq!(plan.fetch_url, plan.url);
            let outcome =
                download_and_store(&client, &FixedLookup("93.184.216.34"), &plan, &dir).await;
            assert!(matches!(outcome, DownloadOutcome::Failed { .. }));
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// 源地址钉在 `raw.githubusercontent.com`（规则资源的真实默认源）的计划。
        fn gh_plan(id: &str) -> ResourcePlan {
            let url = format!("http://raw.githubusercontent.com/x/{id}.srs");
            ResourcePlan {
                id: id.into(),
                name: id.into(),
                category: "geosite".into(),
                fetch_url: url.clone(),
                url,
                file_name: format!("{id}.srs"),
                format: RuleResourceFormat::Binary,
            }
        }

        /// 接受连接后**不回应**的服务端（模拟慢/挂死的下载源，供取消测试）。
        /// 持有 listener 到测试结束（返回 JoinHandle 的 sender 端由线程 park 住）。
        fn spawn_hanging() -> SocketAddr {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind 回环端口");
            let addr = listener.local_addr().expect("取端口");
            thread::spawn(move || {
                if let Ok((mut sock, _)) = listener.accept() {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf);
                    // 收下请求后什么都不回：连接一直挂着，直到测试结束进程回收。
                    thread::sleep(std::time::Duration::from_secs(60));
                    drop(sock);
                }
            });
            addr
        }

        /// **取消真的中断在途下载**（不是「标记取消后继续下完」）。
        ///
        /// 服务端收下请求后永不响应；若无取消，`download_with_progress` 会挂到
        /// `RULE_RESOURCE_TIMEOUT_MS`(30s)。本测把整体超时压到 8s：
        /// - **变异锁（本轮要求的变异验证之二）**：删掉 `tokio::select!` 的取消分支（退回直接
        ///   `download_and_store(...).await`）→ 8s 内不返回 → 本测超时转红。
        /// - 同时断言：结果为 `Cancelled`、发了 `cancelled` 帧、**没有落盘**、`cancel_inflight` 计数为 1。
        #[tokio::test]
        async fn cancel_aborts_inflight_download_and_writes_nothing() {
            let addr = spawn_hanging();
            let client = HttpRuntime::with_resolve_overrides(&[("res.example.com", addr)]).unwrap();
            let dir = tmp_res_dir("cancel");
            let plan = plan_for("cancel-res", addr);
            let sink = std::sync::Arc::new(RecordingSink::default());

            let sink_bg = sink.clone();
            let dir_bg = dir.clone();
            let task = tokio::spawn(async move {
                download_with_progress(
                    sink_bg.as_ref(),
                    &client,
                    &FixedLookup("93.184.216.34"),
                    &plan,
                    &dir_bg,
                )
                .await
            });

            // 等登记落表（登记在首个 await 之前同步完成，故轮询极快命中）。
            let mut cancelled = 0usize;
            for _ in 0..200 {
                cancelled = cancel_inflight("cancel-res");
                if cancelled > 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(cancelled, 1, "应恰好中止一条在途下载");

            let outcome = tokio::time::timeout(std::time::Duration::from_secs(8), task)
                .await
                .expect("取消后必须立刻返回（超时=取消分支没接线）")
                .expect("下载任务不应 panic");
            assert!(
                matches!(outcome, DownloadOutcome::Cancelled),
                "结果须为 Cancelled"
            );
            assert_eq!(
                sink.statuses(),
                vec!["downloading".to_string(), "cancelled".to_string()],
                "须发 cancelled 帧（前端据此清行，而非留一个永远转圈的 spinner）"
            );
            assert!(!dir.join("cancel-res.srs").is_file(), "取消的下载不得落盘");
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// 没有在途下载时取消：如实返回 0（不伪装成功）。
        #[test]
        fn cancel_with_no_inflight_reports_zero() {
            assert_eq!(cancel_inflight("nobody-is-downloading-this"), 0);
        }

        /// 取消是 per-id 的：只中止目标 id，别人的在途下载不受影响。
        #[tokio::test]
        async fn cancel_only_targets_requested_id() {
            let addr = spawn_hanging();
            let client = HttpRuntime::with_resolve_overrides(&[("res.example.com", addr)]).unwrap();
            let dir = tmp_res_dir("cancel-iso");
            let plan = plan_for("keep-me", addr);
            let sink = std::sync::Arc::new(RecordingSink::default());
            let sink_bg = sink.clone();
            let dir_bg = dir.clone();
            let task = tokio::spawn(async move {
                download_with_progress(
                    sink_bg.as_ref(),
                    &client,
                    &FixedLookup("93.184.216.34"),
                    &plan,
                    &dir_bg,
                )
                .await
            });
            // 等 keep-me 登记完成后取消**另一个** id → 不应命中。
            for _ in 0..200 {
                let registered = cancel_registry()
                    .lock()
                    .map(|r| r.values().any(|(id, _)| id == "keep-me"))
                    .unwrap_or(false);
                if registered {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(cancel_inflight("some-other-id"), 0, "别的 id 不该被误伤");
            assert_eq!(cancel_inflight("keep-me"), 1, "目标 id 仍在途 → 可被取消");
            let _ = tokio::time::timeout(std::time::Duration::from_secs(8), task).await;
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(test)]
mod icon_gallery_tests {
    use super::*;
    use polaris_net_stack::safe_redirect::{FetchInit, MinimalResponse};
    use std::collections::HashMap;
    use std::future::Future;

    /// mock HttpClient：按 URL 返回预置 (status, body)；未配置的 URL → 网络错（触发镜像回退）。
    /// 不碰宿主网络（对齐 safe_redirect.rs 的 MockFetch，但带 body 供解析）。
    ///
    /// `pub(super)`：清单刷新的测试（`mod catalog_tests`）需要同一个 mock —— 两份 mock 会各自漂移。
    pub(super) struct MockHttp {
        pub(super) responses: HashMap<String, (u16, Vec<u8>)>,
    }
    impl HttpClient for MockHttp {
        fn fetch(
            &self,
            url: &str,
            _init: &FetchInit,
        ) -> impl Future<Output = Result<MinimalResponse, String>> + Send {
            let resp = self.responses.get(url).cloned();
            async move {
                match resp {
                    Some((status, body)) => Ok(MinimalResponse {
                        status,
                        location: None,
                        headers: Vec::new(),
                        body,
                    }),
                    None => Err("connection refused".to_string()),
                }
            }
        }
    }

    /// mock DnsLookup：任何 host → 公网 IP → SSRF guard 放行（guard 仍真跑，不是绕过）。
    pub(super) struct PublicLookup;
    impl DnsLookup for PublicLookup {
        fn lookup_all(
            &self,
            _host: &str,
        ) -> impl Future<Output = Result<Vec<String>, String>> + Send {
            // 语句先行（对齐本仓既有 FixedLookup/MockLookup 写法）：body 非单一 async 块，
            // 避免 clippy::manual_async_fn 与 trait 的显式 `+ Send` bound 冲突。
            let ips = vec!["8.8.8.8".to_string()];
            async move { Ok(ips) }
        }
    }

    fn gallery_json(names: &[&str]) -> Vec<u8> {
        let icons: Vec<Value> = names
            .iter()
            .map(|n| json!({ "name": n, "url": format!("https://cdn/{n}.png") }))
            .collect();
        serde_json::to_vec(&json!({ "icons": icons })).unwrap()
    }

    fn names_of(items: &[IconGalleryItem]) -> Vec<&str> {
        items.iter().map(|i| i.name.as_str()).collect()
    }

    // ── 纯解析 ─────────────────────────────────────────────────────────────

    #[test]
    fn parse_extracts_name_url_pairs() {
        let v = json!({ "icons": [
            { "name": "A", "url": "https://x/a.png" },
            { "name": "B", "url": "https://x/b.png" },
        ]});
        let items = parse_icon_gallery(&v);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "A");
        assert_eq!(items[0].url, "https://x/a.png");
        assert_eq!(items[1].name, "B");
    }

    #[test]
    fn parse_missing_icons_yields_empty_and_bad_items_skipped() {
        assert!(
            parse_icon_gallery(&json!({})).is_empty(),
            "无 icons 键 → 空"
        );
        assert!(
            parse_icon_gallery(&json!({ "icons": "notarray" })).is_empty(),
            "icons 非数组 → 空"
        );
        // 条目缺 url / 缺 name → 跳过该条，不整体失败。
        let mixed = json!({ "icons": [
            { "name": "ok", "url": "https://x/ok.png" },
            { "name": "nourl" },
            { "url": "https://x/noname.png" },
        ]});
        assert_eq!(
            names_of(&parse_icon_gallery(&mixed)),
            vec!["ok"],
            "只保留 name+url 齐全的条目"
        );
    }

    #[test]
    fn parse_homarr_strips_png_suffix_and_builds_cdn_url() {
        let v = json!({ "png": ["1panel.png", "discord.png"], "svg": ["ignore.svg"] });
        let items = parse_homarr_gallery(&v);
        assert_eq!(
            names_of(&items),
            vec!["1panel", "discord"],
            "显示名须去 .png 后缀"
        );
        assert_eq!(
            items[0].url,
            "https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons@main/png/1panel.png",
            "url 须为 png 目录下的原文件名（含 .png）"
        );
        // svg 键不参与（只取 png 数组）。
        assert!(
            !items.iter().any(|i| i.name.contains("ignore")),
            "只取 png，不碰 svg/webp"
        );
    }

    #[test]
    fn parse_homarr_missing_png_or_bad_items_yields_empty_or_skips() {
        assert!(
            parse_homarr_gallery(&json!({})).is_empty(),
            "无 png 键 → 空"
        );
        assert!(
            parse_homarr_gallery(&json!({ "png": "notarray" })).is_empty(),
            "png 非数组 → 空"
        );
        // 空串 / 非字符串条目 → 跳过，不整体失败。
        let mixed = json!({ "png": ["ok.png", "", 42] });
        assert_eq!(
            names_of(&parse_homarr_gallery(&mixed)),
            vec!["ok"],
            "跳过空串/非串条目"
        );
    }

    // ── 拉取 / 回退 / 合并（mock，不碰网络）───────────────────────────────────

    #[tokio::test]
    async fn merges_both_sources_qure_first_then_edc() {
        // 变异守卫：打断 `merged.extend(edc)`（漏 edc）或交换合并顺序 → 本断言转红。
        let mut responses = HashMap::new();
        responses.insert(
            QURE_ICON_MIRRORS[0].to_string(),
            (200u16, gallery_json(&["Q1", "Q2"])),
        );
        responses.insert(
            EDC_ICON_MIRRORS[0].to_string(),
            (200u16, gallery_json(&["E1"])),
        );
        let http = MockHttp { responses };
        let items = fetch_icon_galleries(&http, &PublicLookup).await;
        assert_eq!(
            names_of(&items),
            vec!["Q1", "Q2", "E1"],
            "两源合并，Qure 在前 edc 在后（homarr 未配置 → 空，不影响顺序）"
        );
    }

    #[tokio::test]
    async fn merges_three_sources_qure_homarr_edc_in_order() {
        // 变异守卫：漏 homarr 的 extend / 合并顺序错乱 → 本断言转红。homarr 结构是 `{png:[...]}`（异于 .icons），
        // 验的是三源都进结果且 Qure → homarr → edc 顺序。
        let mut responses = HashMap::new();
        responses.insert(
            QURE_ICON_MIRRORS[0].to_string(),
            (200u16, gallery_json(&["Q1"])),
        );
        responses.insert(
            HOMARR_ICON_MIRRORS[0].to_string(),
            (
                200u16,
                serde_json::to_vec(&json!({ "png": ["h1.png", "h2.png"] })).unwrap(),
            ),
        );
        responses.insert(
            EDC_ICON_MIRRORS[0].to_string(),
            (200u16, gallery_json(&["E1"])),
        );
        let http = MockHttp { responses };
        let items = fetch_icon_galleries(&http, &PublicLookup).await;
        assert_eq!(
            names_of(&items),
            vec!["Q1", "h1", "h2", "E1"],
            "三源合并，顺序 Qure → homarr → edc；homarr 去 .png 后缀"
        );
    }

    #[tokio::test]
    async fn homarr_source_falls_back_across_its_own_mirrors() {
        // homarr 首镜像失败、次镜像成功 → homarr 仍贡献图标（复用同一 fetch_icon_source 回退链）。
        let mut responses = HashMap::new();
        responses.insert(
            HOMARR_ICON_MIRRORS[0].to_string(),
            (500u16, b"err".to_vec()),
        );
        responses.insert(
            HOMARR_ICON_MIRRORS[1].to_string(),
            (
                200u16,
                serde_json::to_vec(&json!({ "png": ["only.png"] })).unwrap(),
            ),
        );
        let http = MockHttp { responses };
        let items = fetch_icon_galleries(&http, &PublicLookup).await;
        assert_eq!(
            names_of(&items),
            vec!["only"],
            "homarr 首镜像失败须回退次镜像"
        );
    }

    #[tokio::test]
    async fn falls_back_to_next_mirror_when_first_fails() {
        // 变异守卫：把镜像循环改成「只试首个」→ 结果空 → 转红。
        let mut responses = HashMap::new();
        responses.insert(QURE_ICON_MIRRORS[0].to_string(), (500u16, b"err".to_vec()));
        responses.insert(
            QURE_ICON_MIRRORS[1].to_string(),
            (200u16, gallery_json(&["Q_M2"])),
        );
        let http = MockHttp { responses };
        let items = fetch_icon_galleries(&http, &PublicLookup).await;
        assert_eq!(names_of(&items), vec!["Q_M2"], "首镜像失败须回退次镜像");
    }

    #[tokio::test]
    async fn non_2xx_falls_back_even_with_valid_json_body() {
        // 变异守卫：删掉 2xx 状态检查 → 503 的合法 body 被误用 → 得 ["STALE"] → 转红。
        let mut responses = HashMap::new();
        responses.insert(
            QURE_ICON_MIRRORS[0].to_string(),
            (503u16, gallery_json(&["STALE"])),
        );
        responses.insert(
            QURE_ICON_MIRRORS[1].to_string(),
            (200u16, gallery_json(&["GOOD"])),
        );
        let http = MockHttp { responses };
        let items = fetch_icon_galleries(&http, &PublicLookup).await;
        assert_eq!(
            names_of(&items),
            vec!["GOOD"],
            "非 2xx 即便 body 合法也须回退，不得用其内容"
        );
    }

    #[tokio::test]
    async fn invalid_json_falls_back_to_next_mirror() {
        // 真实 edc 形态：合法 2xx 但 body 有尾逗号（非法 JSON）→ 须回退次镜像。
        let mut responses = HashMap::new();
        responses.insert(
            QURE_ICON_MIRRORS[0].to_string(),
            (200u16, b"{ \"icons\": [ , ] }".to_vec()),
        );
        responses.insert(
            QURE_ICON_MIRRORS[1].to_string(),
            (200u16, gallery_json(&["RECOVERED"])),
        );
        let http = MockHttp { responses };
        let items = fetch_icon_galleries(&http, &PublicLookup).await;
        assert_eq!(
            names_of(&items),
            vec!["RECOVERED"],
            "非法 JSON 须回退次镜像"
        );
    }

    #[tokio::test]
    async fn valid_json_without_icons_stops_no_fallthrough() {
        // 钉死 上游 语义：合法 JSON 即停（即使无 icons），不因空 icons 回退次镜像。
        // 变异守卫：把「空 icons 也回退」引入 → 会取到次镜像的 MUST_NOT_APPEAR → 转红。
        let mut responses = HashMap::new();
        responses.insert(
            QURE_ICON_MIRRORS[0].to_string(),
            (200u16, serde_json::to_vec(&json!({ "other": 1 })).unwrap()),
        );
        responses.insert(
            QURE_ICON_MIRRORS[1].to_string(),
            (200u16, gallery_json(&["MUST_NOT_APPEAR"])),
        );
        let http = MockHttp { responses };
        let items = fetch_icon_galleries(&http, &PublicLookup).await;
        assert!(items.is_empty(), "合法 JSON 即停，空 icons 不回退次镜像");
    }

    #[tokio::test]
    async fn both_sources_fail_yields_empty() {
        // 变异守卫：把「全镜像失败返空」改成返非空 → 转红（前端据空集降级手动 URL）。
        let http = MockHttp {
            responses: HashMap::new(),
        };
        let items = fetch_icon_galleries(&http, &PublicLookup).await;
        assert!(items.is_empty(), "两源全失败须返空（前端降级手动 URL）");
    }

    #[tokio::test]
    async fn one_source_fails_other_still_returns() {
        // 一源全断（Qure），另一源经末位镜像成功（edc）→ 结果只含 edc（独立容错）。
        let mut responses = HashMap::new();
        responses.insert(
            EDC_ICON_MIRRORS[2].to_string(),
            (200u16, gallery_json(&["ONLY_EDC"])),
        );
        let http = MockHttp { responses };
        let items = fetch_icon_galleries(&http, &PublicLookup).await;
        assert_eq!(
            names_of(&items),
            vec!["ONLY_EDC"],
            "Qure 全断时 edc 仍应返回"
        );
    }

    /// 「刷新」必须把**两层**缓存一起倒掉：清单内存缓存 + 图标本体的磁盘浏览缓存。
    ///
    /// 只清一层的按钮比没有按钮更糟 —— 另一层旧掉时表现成「点了没反应」。本条把
    /// `drop_icon_gallery_caches` 的这句宣称变成可证伪的（去掉任一腿即转红）。
    ///
    /// 用进程级静态缓存 ⇒ 本条是**唯一**碰它的测试，不与其他用例并发争用。
    #[test]
    fn refresh_drops_both_manifest_and_disk_caches() {
        let dir = std::env::temp_dir().join(format!(
            "polaris-icon-refresh-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let browse = crate::icon_cache::remote_cache_dir(&dir);
        std::fs::create_dir_all(&browse).unwrap();
        let stub = browse.join("deadbeefdeadbeef.png");
        std::fs::write(&stub, b"\x89PNGcached").unwrap();

        store_icon_cache(&[IconGalleryItem {
            name: "stale".to_string(),
            url: "https://cdn.example.com/stale.png".to_string(),
        }]);
        // 自检：两层都得先真的「有东西」，否则下面两条断言恒绿。
        assert!(read_fresh_icon_cache().is_some(), "自检：清单缓存须先命中");
        assert!(stub.exists(), "自检：磁盘缓存文件须先在");

        drop_icon_gallery_caches(&dir);

        assert!(
            read_fresh_icon_cache().is_none(),
            "清单腿没作废 —— 重拉会命中 1h TTL 的旧清单，用户看到「刷新没用」"
        );
        assert!(!stub.exists(), "磁盘腿没清 —— 图标本体仍是旧的");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 资源库清单刷新（catalog refresh）测试
//
// **禁止真网**：全部走 `MockHttp`（预置 URL → 响应）+ `PublicLookup`（假 DNS，SSRF guard 仍真跑）。
// 无任何一条断言依赖 `api.github.com` 可达。
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod catalog_tests {
    use super::icon_gallery_tests::{MockHttp, PublicLookup};
    use super::*;
    use polaris_config_engine::user_config::rule_resource_catalog;
    use std::collections::HashMap;

    const GEO_SHA: &str = "1111111111111111111111111111111111111111";
    const LITE_SHA: &str = "2222222222222222222222222222222222222222";

    fn root_url() -> String {
        format!("{MRD_TREE_API_BASE}{MRD_CATALOG_REF}")
    }
    fn subtree_url(sha: &str) -> String {
        format!("{MRD_TREE_API_BASE}{sha}?recursive=1")
    }

    /// 根树 JSON（geo / geo-lite 两个子树 + 一个无关文件）。
    fn root_json(geo_sha: &str, lite_sha: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({ "tree": [
            { "path": "README.md", "type": "blob", "sha": "3333333333333333333333333333333333333333" },
            { "path": "geo", "type": "tree", "sha": geo_sha },
            { "path": "geo-lite", "type": "tree", "sha": lite_sha },
        ]}))
        .unwrap()
    }

    /// 子树 JSON：`entries` = (type, path)。
    fn subtree_json(entries: &[(&str, &str)], truncated: bool) -> Vec<u8> {
        let tree: Vec<Value> = entries
            .iter()
            .map(|(ty, p)| json!({ "type": ty, "path": p }))
            .collect();
        serde_json::to_vec(&json!({ "truncated": truncated, "tree": tree })).unwrap()
    }

    /// 生成 n 条 geosite 叶子（用于凑过 `CATALOG_MIN_ITEMS` 闸）。
    fn many_geosite(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("geosite/site{i}.srs")).collect()
    }

    /// 一套「远端一切正常」的 mock（geo 60 条 + geo-lite 2 条）。
    fn healthy_mock() -> MockHttp {
        let geo_paths = many_geosite(60);
        let mut geo: Vec<(&str, &str)> = geo_paths.iter().map(|p| ("blob", p.as_str())).collect();
        geo.push(("blob", "geosite/youtube.srs"));
        geo.push(("blob", "geoip/cn.srs"));
        let lite = [("blob", "geosite/cn.srs"), ("blob", "geoip/cn.srs")];
        let mut responses = HashMap::new();
        responses.insert(root_url(), (200u16, root_json(GEO_SHA, LITE_SHA)));
        responses.insert(subtree_url(GEO_SHA), (200u16, subtree_json(&geo, false)));
        responses.insert(subtree_url(LITE_SHA), (200u16, subtree_json(&lite, false)));
        MockHttp { responses }
    }

    /// 什么都答不上来的 client（= 全网不通）。
    fn dead_mock() -> MockHttp {
        MockHttp {
            responses: HashMap::new(),
        }
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-catalog-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |x| x.as_nanos())
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    // ── 派生口径 ──────────────────────────────────────────────────────────────

    /// **同构门**：远程 collect 派生出的条目必须与内置清单逐字相同（id/category/name/path）。
    /// 破了它，同一资源在「内置」与「刷新后」两态会算出两个下载 URL / 两个落盘名。
    ///
    /// 只对**内置清单里有的、且 tag 与上游文件名同名的** id 成立。两处例外各自单列：
    /// `geo-lite/` 那支不随包（→ `lite_tree_path_*`）；`geosite-category-ai` 的 tag 与文件名分岔
    /// （→ `category_ai_is_the_one_id_remote_cannot_reproduce`）。
    #[test]
    fn tree_path_matches_builtin_derivation() {
        for id in ["geosite-youtube", "geoip-cn", "geosite-geolocation-!cn"] {
            let builtin = find_catalog_item(id).expect("内置清单应有此条目");
            let (base, rel) = builtin.path.split_once('/').unwrap();
            let derived = catalog_item_from_tree_path(base, rel).expect("远程派生应成立");
            assert_eq!(derived, builtin, "{id} 的远程派生与内置清单不一致");
        }
    }

    /// `geo-lite/` 支的派生：目录 `geo-lite/geosite` → category `geosite-lite`（不是 `geosite`），
    /// 且**不得**被判成随包（内置清单里没有它，判真会让外置 tab 把它标成「已内置」且不可下载）。
    #[test]
    fn lite_tree_path_derives_lite_category_and_is_not_bundled() {
        let i = catalog_item_from_tree_path("geo-lite", "geosite/cn.srs").expect("派生应成立");
        assert_eq!(i.id, "geosite-lite-cn");
        assert_eq!(i.category, "geosite-lite");
        assert_eq!(i.path, "geo-lite/geosite/cn.srs");
        assert!(!i.bundled, "lite 变体从不随包");
        assert!(
            find_catalog_item(&i.id).is_none(),
            "lite 变体不得在内置清单里"
        );
    }

    /// 全表唯一一条远程派生复刻不出内置形态的 id：随包 tag 是 `geosite-category-ai`，上游文件却叫
    /// `category-ai-!cn.srs`，而远程只看得见文件名 —— 于是「外置」tab 会把这份**已随包**的数据
    /// 列成一条未随包的 `geosite-category-ai-!cn`，用户下回来是第二份同内容副本。
    ///
    /// 不修：修法是给随包表加一张「tag ↔ 上游文件名」的反查表，只为一条数据的展示口径，不划算；
    /// 且真下回来也只是多占一份盘，不影响路由（生效的恒是随包那份，见 `route.rs` 注入顺序）。
    /// 这条断言把「已知且接受」钉死，免得下次有人当 bug 排查一轮。
    #[test]
    fn category_ai_is_the_one_id_remote_cannot_reproduce() {
        let builtin = find_catalog_item("geosite-category-ai").expect("内置清单应有此条目");
        let derived = catalog_item_from_tree_path("geo", "geosite/category-ai-!cn.srs").unwrap();
        assert_eq!(
            derived.path, builtin.path,
            "path 仍须同址（下载 URL 不分家）"
        );
        assert_ne!(derived.id, builtin.id);
        assert!(!derived.bundled, "远程侧按文件名判，认不出它已随包");
    }

    #[test]
    fn tree_path_rejects_non_ruleset_and_injection_shapes() {
        // 非 .srs / 非 geosite|geoip 前缀 / 嵌套子目录 / 点开头 / 控制字符与 URL 语义字符。
        for (base, rel) in [
            ("geo", "geosite/cn.txt"),
            ("geo", "other/cn.srs"),
            ("geo", "cn.srs"),
            ("geo", "geosite/sub/cn.srs"),
            ("geo", "geosite/../../evil.srs"),
            ("geo", "geosite/.srs"),
            ("geo", "geosite/a?b.srs"),
            ("geo", "geosite/a#b.srs"),
            ("geo", "geosite/a%2e.srs"),
            ("geo", "geosite/a\\b.srs"),
            ("evil", "geosite/cn.srs"),
        ] {
            assert!(
                catalog_item_from_tree_path(base, rel).is_none(),
                "应拒收: {base}/{rel}"
            );
        }
    }

    #[test]
    fn collect_skips_trees_and_keeps_blobs() {
        let tree = serde_json::from_slice::<Value>(&subtree_json(
            &[
                ("tree", "geosite"),
                ("blob", "geosite/cn.srs"),
                ("blob", "geosite/nested/x.srs"),
                ("blob", "LICENSE"),
            ],
            false,
        ))
        .unwrap();
        let mut out = Vec::new();
        collect_catalog_items(&tree, "geo", &mut out);
        assert_eq!(out.len(), 1, "只应收下 geosite/cn.srs");
        assert_eq!(out[0].id, "geosite-cn");
    }

    #[test]
    fn tree_sha_must_be_hex_of_git_length() {
        assert!(is_valid_tree_sha(GEO_SHA));
        assert!(is_valid_tree_sha(&"a".repeat(64)), "sha256 = 64 位");
        // 远端可控值直接拼进下一跳 URL 的路径段 → 非 hex 一律拒。
        assert!(!is_valid_tree_sha("../../../repos/evil/x/git/trees/main"));
        assert!(!is_valid_tree_sha("short"));
        assert!(!is_valid_tree_sha(&"a".repeat(65)));
        // **长度收敛为「恰好 40 或 64」**：git object id 只有这两种长度，中间那 23 种长度全是
        // 「不可能是 sha 的东西」，放行它们没有任何合法用例。变异锁：改回 `(40..=64).contains(..)`
        // → 下面三条转红。
        for n in [41, 50, 63] {
            assert!(
                !is_valid_tree_sha(&"a".repeat(n)),
                "长度 {n} 不是 git object id 的合法长度"
            );
        }
        let root: Value = serde_json::from_slice(&root_json("../../evil", LITE_SHA)).unwrap();
        assert!(
            tree_child_sha(&root, "geo").is_none(),
            "被注入的 sha 不得被采信"
        );
    }

    // ── 远程腿 ────────────────────────────────────────────────────────────────

    /// **变异锁 #1**：把 [`refresh_catalog_core`] 的远程腿改回恒等降级（直接返
    /// `builtin_catalog_result()`）→ 本用例三条断言全红（source / 条数 / 内置表外的 id）。
    #[tokio::test]
    async fn remote_refresh_returns_remote_source_and_full_list() {
        let dir = tmp_dir("remote");
        let res = refresh_catalog_core(&healthy_mock(), &PublicLookup, &dir, "").await;
        assert_eq!(res.source, "remote", "远程成功必须自述 remote");
        assert!(
            res.fetched_at.is_some_and(|t| t > 0),
            "remote 必须带真时间戳"
        );
        assert_eq!(
            res.items.len(),
            64,
            "60 条填充 + youtube + geoip-cn + lite 两条"
        );
        assert!(
            res.items.len() > rule_resource_catalog().len(),
            "全量必须多于内置清单（恒等降级会让这条转红）"
        );
        assert!(
            res.items.iter().any(|i| i.id == "geosite-site0"),
            "必须含内置表**没有**的条目（证明清单真来自远端）"
        );
        assert!(res.items.iter().any(|i| i.id == "geosite-lite-cn"));
        assert!(catalog_cache_path(&dir).is_file(), "远程成功必须落盘缓存");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **变异锁 #2**：缓存腿。第一轮远程成功落盘，第二轮全网不通 → 必须命中缓存（`source:"cache"`），
    /// 且条目与时间戳与第一轮逐字相同。删掉 [`write_catalog_cache`] 或 [`read_catalog_cache`] 任一侧 → 转红。
    #[tokio::test]
    async fn cache_is_reachable_after_a_successful_refresh() {
        let dir = tmp_dir("cache");
        let first = refresh_catalog_core(&healthy_mock(), &PublicLookup, &dir, "").await;
        assert_eq!(first.source, "remote");
        let second = refresh_catalog_core(&dead_mock(), &PublicLookup, &dir, "").await;
        assert_eq!(second.source, "cache", "有缓存时不得回落到 builtin");
        assert_eq!(second.items, first.items, "缓存回读须与落盘内容逐字相同");
        assert_eq!(
            second.fetched_at, first.fetched_at,
            "fetchedAt 须是落盘那次的真时间"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 远程失败 + 无缓存 → 诚实降级到内置（与改动前逐字一致：33 条 / builtin / fetchedAt=null）。
    #[tokio::test]
    async fn remote_failure_without_cache_degrades_to_builtin() {
        let dir = tmp_dir("builtin");
        let res = refresh_catalog_core(&dead_mock(), &PublicLookup, &dir, "").await;
        assert_eq!(res.source, "builtin");
        assert!(res.fetched_at.is_none(), "内置回落不得谎报拉取时间");
        assert_eq!(res.items, rule_resource_catalog());
        assert!(
            !catalog_cache_path(&dir).exists(),
            "失败不得写出任何缓存文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 限流（403/429）与普通网络错分流：消息里必须能看出是限流（60 次/小时是本功能最常见的失败因）。
    #[tokio::test]
    async fn rate_limited_status_is_distinguished() {
        let mut responses = HashMap::new();
        responses.insert(root_url(), (403u16, b"{}".to_vec()));
        let err = fetch_catalog_from_github(&MockHttp { responses }, &PublicLookup, "")
            .await
            .expect_err("403 必须失败");
        assert!(err.contains("限流"), "限流须可辨识，实得: {err}");

        let mut responses = HashMap::new();
        responses.insert(root_url(), (429u16, b"{}".to_vec()));
        let err = fetch_catalog_from_github(&MockHttp { responses }, &PublicLookup, "")
            .await
            .expect_err("429 必须失败");
        assert!(err.contains("限流"), "二级限流须可辨识，实得: {err}");
    }

    /// 畸形远端响应（截断 / 条目过少 / 根树结构变了）一律失败，且**不得污染既有缓存**。
    #[tokio::test]
    async fn malformed_remote_response_fails_and_leaves_cache_intact() {
        let dir = tmp_dir("nopollute");
        let good = refresh_catalog_core(&healthy_mock(), &PublicLookup, &dir, "").await;
        assert_eq!(good.source, "remote");
        let cached_bytes = std::fs::read(catalog_cache_path(&dir)).unwrap();

        // ① truncated:true —— 半份清单不得覆盖好缓存。
        let mut responses = HashMap::new();
        responses.insert(root_url(), (200u16, root_json(GEO_SHA, LITE_SHA)));
        let geo_paths = many_geosite(60);
        let geo: Vec<(&str, &str)> = geo_paths.iter().map(|p| ("blob", p.as_str())).collect();
        responses.insert(subtree_url(GEO_SHA), (200u16, subtree_json(&geo, true)));
        responses.insert(
            subtree_url(LITE_SHA),
            (200u16, subtree_json(&[("blob", "geosite/cn.srs")], false)),
        );
        let res = refresh_catalog_core(&MockHttp { responses }, &PublicLookup, &dir, "").await;
        assert_eq!(res.source, "cache", "截断响应须失败并回落缓存");
        assert_eq!(
            std::fs::read(catalog_cache_path(&dir)).unwrap(),
            cached_bytes,
            "截断响应不得改写缓存"
        );

        // ② 条目过少（< CATALOG_MIN_ITEMS）。
        let mut responses = HashMap::new();
        responses.insert(root_url(), (200u16, root_json(GEO_SHA, LITE_SHA)));
        responses.insert(
            subtree_url(GEO_SHA),
            (200u16, subtree_json(&[("blob", "geosite/cn.srs")], false)),
        );
        responses.insert(subtree_url(LITE_SHA), (200u16, subtree_json(&[], false)));
        let err = fetch_catalog_from_github(&MockHttp { responses }, &PublicLookup, "")
            .await
            .expect_err("条目过少必须失败");
        assert!(err.contains("过少"), "实得: {err}");

        // ③ 根树没有 geo / geo-lite（上游改目录名）。
        let mut responses = HashMap::new();
        responses.insert(
            root_url(),
            (200u16, serde_json::to_vec(&json!({ "tree": [] })).unwrap()),
        );
        assert!(
            fetch_catalog_from_github(&MockHttp { responses }, &PublicLookup, "")
                .await
                .is_err(),
            "根树结构不符必须失败"
        );

        // ④ 非法 JSON。
        let mut responses = HashMap::new();
        responses.insert(root_url(), (200u16, b"<html>rate limited</html>".to_vec()));
        assert!(
            fetch_catalog_from_github(&MockHttp { responses }, &PublicLookup, "")
                .await
                .is_err(),
            "非 JSON 响应必须失败"
        );

        assert_eq!(
            std::fs::read(catalog_cache_path(&dir)).unwrap(),
            cached_bytes,
            "以上畸形响应全程不得改写缓存"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 缓存文件本身的畸形面 ───────────────────────────────────────────────────

    #[test]
    fn malformed_cache_file_is_rejected_wholesale() {
        let dir = tmp_dir("badcache");
        std::fs::create_dir_all(&dir).unwrap();
        let ok_items: Vec<Value> = (0..60)
            .map(|i| {
                json!({
                    "id": format!("geosite-site{i}"),
                    "category": "geosite",
                    "name": format!("site{i}"),
                    "path": format!("geo/geosite/site{i}.srs"),
                })
            })
            .collect();
        let write = |v: &Value| std::fs::write(catalog_cache_path(&dir), v.to_string()).unwrap();

        // 基准：合法缓存能读出来（防下面的断言变成「怎么写都读不出」的假绿）。
        write(&json!({ "schemaVersion": 1, "fetchedAt": 1_700_000_000_000i64, "items": ok_items }));
        let (items, at) = read_catalog_cache(&dir).expect("合法缓存必须可读");
        assert_eq!(items.len(), 60);
        assert_eq!(at, 1_700_000_000_000i64);

        // schemaVersion 不符 → 整份作废（不做迁移）。
        write(&json!({ "schemaVersion": 2, "fetchedAt": 1i64, "items": ok_items }));
        assert!(read_catalog_cache(&dir).is_none());

        // fetchedAt 缺失 / 非正 → 作废（否则 UI 会显示 1970）。
        write(&json!({ "schemaVersion": 1, "items": ok_items }));
        assert!(read_catalog_cache(&dir).is_none());
        write(&json!({ "schemaVersion": 1, "fetchedAt": 0i64, "items": ok_items }));
        assert!(read_catalog_cache(&dir).is_none());

        // 条数不够 → 作废（= 远程侧同一道闸）。
        write(&json!({ "schemaVersion": 1, "fetchedAt": 1i64, "items": [ok_items[0].clone()] }));
        assert!(read_catalog_cache(&dir).is_none());

        // 单条被篡改（id 与 path 不自洽）→ **整份**作废，不是跳过那一条。
        let mut tampered = ok_items.clone();
        tampered[3] = json!({
            "id": "geosite-evil", "category": "geosite", "name": "evil",
            "path": "geo/geosite/site3.srs",
        });
        write(&json!({ "schemaVersion": 1, "fetchedAt": 1i64, "items": tampered }));
        assert!(
            read_catalog_cache(&dir).is_none(),
            "id/path 不自洽的条目必须让整份缓存作废"
        );

        // path 里塞穿越 → 作废。
        let mut traversal = ok_items.clone();
        traversal[0] = json!({
            "id": "geosite-x", "category": "geosite", "name": "x",
            "path": "geo/geosite/../../../x.srs",
        });
        write(&json!({ "schemaVersion": 1, "fetchedAt": 1i64, "items": traversal }));
        assert!(read_catalog_cache(&dir).is_none());

        // 非 JSON / 空文件 → 作废（不 panic）。
        std::fs::write(catalog_cache_path(&dir), b"{not json").unwrap();
        assert!(read_catalog_cache(&dir).is_none());
        std::fs::write(catalog_cache_path(&dir), b"").unwrap();
        assert!(read_catalog_cache(&dir).is_none());

        // 文件不存在 → None（不是 panic）。
        std::fs::remove_file(catalog_cache_path(&dir)).unwrap();
        assert!(read_catalog_cache(&dir).is_none());
        assert_eq!(cached_or_builtin_catalog(&dir).source, "builtin");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 零出站回读缓存（外置 tab 打开即有清单）────────────────────────────────

    /// **变异锁**：删掉 [`refresh_catalog_core`] 里的 [`write_catalog_cache`] 调用（或把
    /// [`cached_catalog`] 改成恒 `None`）→ 本用例转红 = 外置 tab 又退回「每次打开都得手点刷新」。
    #[tokio::test]
    async fn cached_catalog_serves_the_list_after_one_successful_refresh() {
        let dir = tmp_dir("cache-only");
        assert!(cached_catalog(&dir).is_none(), "前提：起始无缓存");
        let first = refresh_catalog_core(&healthy_mock(), &PublicLookup, &dir, "").await;
        assert_eq!(first.source, "remote");

        // 第二次「打开弹窗」：不碰网络（本函数签名里根本没有 client），仍拿到同一份清单。
        let preload = cached_catalog(&dir).expect("刷新过一次后必须能零出站读回");
        assert_eq!(preload.source, "cache");
        assert_eq!(preload.items, first.items, "回读须与落盘内容逐字相同");
        assert_eq!(preload.fetched_at, first.fetched_at);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无缓存 → `None`，**不得**借 `cached_or_builtin_catalog` 那档回落内置：那一档的语义是
    /// 「远程拉过且失败了」，而本腿一次网都没打，借它会让 UI 报一个没发生过的失败。
    #[test]
    fn cached_catalog_without_cache_is_none_not_builtin_fallback() {
        let dir = tmp_dir("cache-only-empty");
        assert!(cached_catalog(&dir).is_none());
        // 正向对照：同目录下带回落的那条腿仍返内置 —— 证明上面的 None 不是「读缓存整体坏了」的假绿。
        assert_eq!(cached_or_builtin_catalog(&dir).source, "builtin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缓存里的 `bundled` **不被采信**，一律按当前版本的随包表现算。
    ///
    /// 为什么这条必须有门：缓存是上一个版本落的盘，随包 `.srs` 却随版本增删。信缓存 = 让旧版本的
    /// 随包清单决定新版本的 UI —— 新增随包项仍被标成「可下载」（白下一份被 route.rs 挡住的副本），
    /// 移除的随包项被标成「已内置」（用户以为在手，配置生成时该 tag 无处可寻，规则静默失效）。
    #[test]
    fn cached_bundled_flag_is_recomputed_not_trusted() {
        let dir = tmp_dir("bundled-cache");
        std::fs::create_dir_all(&dir).unwrap();
        let mut items: Vec<Value> = (0..60)
            .map(|i| {
                json!({
                    "id": format!("geosite-site{i}"),
                    "category": "geosite",
                    "name": format!("site{i}"),
                    "path": format!("geo/geosite/site{i}.srs"),
                })
            })
            .collect();
        // 两条撒谎的条目：随包的自称不随包，未随包的自称随包。
        items.push(json!({
            "id": "geosite-youtube", "category": "geosite", "name": "youtube",
            "path": "geo/geosite/youtube.srs", "bundled": false,
        }));
        items.push(json!({
            "id": "geoip-us", "category": "geoip", "name": "us",
            "path": "geo/geoip/us.srs", "bundled": true,
        }));
        std::fs::write(
            catalog_cache_path(&dir),
            json!({ "schemaVersion": 1, "fetchedAt": 1_700_000_000_000i64, "items": items })
                .to_string(),
        )
        .unwrap();

        let res = cached_catalog(&dir).expect("合法缓存必须可读");
        let bundled_of = |id: &str| res.items.iter().find(|i| i.id == id).unwrap().bundled;
        assert!(
            bundled_of("geosite-youtube"),
            "随包项须判真（缓存说 false 不作数）"
        );
        assert!(
            !bundled_of("geoip-us"),
            "未随包项须判假（缓存说 true 不作数）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_leaves_no_tmp_residue() {
        let dir = tmp_dir("atomic");
        let items = rule_resource_catalog();
        write_catalog_cache(&dir, 42, &items).expect("写缓存应成功");
        let residue: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(residue.is_empty(), "不得残留 .tmp: {residue:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── gh-proxy 复用（与下载腿同一决策点）────────────────────────────────────

    /// 加速前缀命中 GitHub 域时：先打镜像址，镜像失败自动回退原址 —— 与下载腿
    /// [`download_and_store`] 逐字同一语义，且都由 [`apply_gh_proxy`] 单点决策。
    #[tokio::test]
    async fn catalog_json_fetch_prefers_mirror_then_falls_back_to_origin() {
        const PREFIX: &str = "https://gh-proxy.org";
        let origin = "https://raw.githubusercontent.com/o/r/sing/index.json";
        let mirror = apply_gh_proxy(PREFIX, origin).expect("raw 域应被加速");

        // 两条腿都返**合法 git-trees 结构**（否则会被结构闸拦下，那是另一条用例的事），
        // 靠 `tree[0].path` 分辨走了哪条腿。
        let leg =
            |who: &str| format!(r#"{{"tree":[{{"path":"{who}","type":"tree"}}]}}"#).into_bytes();

        // 镜像可用 → 用镜像的响应。
        let mut responses = HashMap::new();
        responses.insert(mirror.clone(), (200u16, leg("mirror")));
        responses.insert(origin.to_string(), (200u16, leg("origin")));
        let v = fetch_catalog_json(&MockHttp { responses }, &PublicLookup, origin, PREFIX)
            .await
            .unwrap();
        assert_eq!(v["tree"][0]["path"], "mirror", "配了前缀就该先打镜像");

        // 镜像挂了 → 回退原址（设置页对「加速」的承诺：失败自动回退直连）。
        let mut responses = HashMap::new();
        responses.insert(origin.to_string(), (200u16, leg("origin")));
        let v = fetch_catalog_json(&MockHttp { responses }, &PublicLookup, origin, PREFIX)
            .await
            .unwrap();
        assert_eq!(v["tree"][0]["path"], "origin", "镜像失败须回退原址");
    }

    /// **镜像返「200 + 合法 JSON + 不是 git-trees」时必须仍然回退原址。**
    ///
    /// 变异锁：删掉 `fetch_catalog_json_once` 里的 `tree` 结构闸 → 本用例转红。
    /// 触发形态是真实的：gh-proxy 类镜像在限流/未授权时回 `{"code":403,"msg":"..."}`（200 状态 +
    /// 合法 JSON）。没有结构闸时，镜像腿被判「成功」→ **原址一次都不打** → 上层
    /// `tree_child_sha` 找不到 geo → 整次刷新失败落缓存，而原址其实是好的。
    #[tokio::test]
    async fn catalog_json_falls_back_when_mirror_returns_json_that_is_not_a_tree() {
        const PREFIX: &str = "https://gh-proxy.org";
        let origin = "https://raw.githubusercontent.com/o/r/sing/index.json";
        let mirror = apply_gh_proxy(PREFIX, origin).expect("raw 域应被加速");

        let mut responses = HashMap::new();
        // 镜像：200 + 能解析的 JSON，但不是 trees 响应。
        responses.insert(
            mirror.clone(),
            (200u16, br#"{"code":403,"msg":"rate limited"}"#.to_vec()),
        );
        responses.insert(
            origin.to_string(),
            (
                200u16,
                br#"{"tree":[{"path":"geo","type":"tree"}]}"#.to_vec(),
            ),
        );
        let v = fetch_catalog_json(&MockHttp { responses }, &PublicLookup, origin, PREFIX)
            .await
            .expect("原址是好的 → 整体必须成功");
        assert!(
            v.get("tree").is_some(),
            "必须拿到原址那份 trees 响应，而不是镜像那份垃圾 JSON"
        );

        // 原址也不是 trees 结构 → 如实 Err（不把垃圾当清单往上送）。
        let mut responses = HashMap::new();
        responses.insert(origin.to_string(), (200u16, br#"{"msg":"nope"}"#.to_vec()));
        assert!(
            fetch_catalog_json(&MockHttp { responses }, &PublicLookup, origin, "")
                .await
                .is_err(),
            "非 trees 结构必须报错"
        );
        // `tree` 存在但不是数组（被换成对象/字符串）同样拒。
        let mut responses = HashMap::new();
        responses.insert(origin.to_string(), (200u16, br#"{"tree":"x"}"#.to_vec()));
        assert!(
            fetch_catalog_json(&MockHttp { responses }, &PublicLookup, origin, "")
                .await
                .is_err(),
            "`tree` 必须是数组"
        );
    }

    /// **现状登记（不是期望）**：trees API 的 `api.github.com` 不在 `GH_PROXY_HOSTS` 5 域表里，
    /// 故清单刷新实际拿不到加速 —— 与 上游（`shared/gh-proxy.ts:58`）和本仓前端
    /// （`ui/src/domain/gh-proxy.ts:56`）明写的口径一致。该表补上 `api.github.com` 之日，本用例转红，
    /// 提醒把这条注释和文档一起更新（届时刷新腿无需改代码即自动吃上加速）。
    #[test]
    fn api_github_is_not_mirrored_by_current_host_table() {
        assert_eq!(
            apply_gh_proxy("https://gh-proxy.org", &root_url()),
            None,
            "api.github.com 当前不在加速域表（前后端两侧同口径）"
        );
    }

    // ── 与下载腿的衔接 ────────────────────────────────────────────────────────

    /// 刷新拿到的条目必须能被下载：`plan_from_item` 要能从「刷新后的清单」里解析出 URL/落盘名。
    /// **变异锁 #3**：把 [`resolve_catalog_item`] 的第二跳去掉（退回只查内置表）→ 本用例转红
    /// （= 用户在外置 tab 勾中任何精选之外的资源都下不下来）。
    #[test]
    fn download_plan_resolves_ids_that_only_exist_in_refreshed_catalog() {
        let remote_only = catalog_item_from_tree_path("geo", "geosite/discord.srs").unwrap();
        assert!(
            find_catalog_item(&remote_only.id).is_none(),
            "前提：该 id 不在内置 33 条里"
        );
        let extra = vec![remote_only.clone()];

        let plan = plan_from_item(&json!({ "catalogId": "geosite-discord" }), &extra)
            .expect("刷新后的条目应可解析");
        assert_eq!(plan.id, "geosite-discord");
        assert_eq!(
            plan.url,
            "https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/sing/geo/geosite/discord.srs"
        );
        assert_eq!(plan.file_name, "geosite-discord.srs");

        // 内置表优先级仍在前：同 id 时以内置表为准。
        let shadow = vec![RuleResourceCatalogItem {
            id: "geosite-youtube".into(),
            category: "geosite".into(),
            name: "youtube".into(),
            path: "geo/geosite/EVIL.srs".into(),
            bundled: true,
        }];
        let plan = plan_from_item(&json!({ "catalogId": "geosite-youtube" }), &shadow).unwrap();
        assert!(
            plan.url.ends_with("/geo/geosite/youtube.srs"),
            "内置表必须优先于刷新清单，实得 {}",
            plan.url
        );

        // 两边都没有 → 仍然报错（不静默编一个 URL）。
        assert!(plan_from_item(&json!({ "catalogId": "geosite-nope" }), &extra).is_err());
    }

    // ── 落盘名：清洗的多对一 + 与目录缓存同名 ──────────────────────────────────

    /// **干净 id 的落盘名逐字不变**（零回归底线）——否则已下载的文件全变孤儿、UI 一片「未下载」。
    #[test]
    fn clean_ids_keep_their_exact_file_name() {
        assert_eq!(
            resource_file_name("geosite-youtube", RuleResourceFormat::Binary),
            "geosite-youtube.srs"
        );
        assert_eq!(
            resource_file_name("res_9f2c.d-1", RuleResourceFormat::Source),
            "res_9f2c.d-1.json"
        );
    }

    /// **有损清洗必须仍是单射**（reviewer #17）。
    ///
    /// 变异锁：把 `resource_file_name` 改回 `format!("{}.{}", sanitize_file_stem(id), ext)`
    /// → 下面的 `assert_ne!` 转红：`a:b` 与 `a*b` 会落到同一个 `a_b.srs`，后下的静默覆盖先下的，
    /// 而 config 里两条记录都指向这一个文件 → 其中一条规则集内容必然是错的。
    #[test]
    fn lossy_sanitisation_still_maps_distinct_ids_to_distinct_files() {
        let f = |id: &str| resource_file_name(id, RuleResourceFormat::Binary);
        assert_ne!(
            f("geosite-a:b"),
            f("geosite-a*b"),
            "折叠字符不同 → 文件必须不同"
        );
        assert_ne!(f("a b"), f("a_b"), "空格 vs 下划线：清洗后同形，哈希须区分");
        // 同一个 id 恒定映射（重下载/更新必须命中同一个文件，不能每次换名）。
        assert_eq!(f("geosite-a:b"), f("geosite-a:b"));
        // 仍然只含安全字符（消歧后缀不得把路径语义带回来）。
        assert!(f("geosite-a/../b")
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')));
    }

    /// **用户资源不得与目录缓存 `catalog.json` 撞名**（reviewer #13）。
    ///
    /// 变异锁：删掉 `RESERVED_RESOURCE_FILE_NAMES` 判定 → 下面两条转红。
    /// 双向危害：下载该资源冲掉目录缓存（下次刷新失去兜底）；刷新目录把用户资源文件写成一份
    /// 清单 JSON（该规则集当场失效，而 UI 仍显示「已下载」——`fileExists` 只校 JSON 是对象）。
    #[test]
    fn user_resource_never_collides_with_the_catalog_cache_file() {
        let name = resource_file_name("catalog", RuleResourceFormat::Source);
        assert_ne!(name, CATALOG_CACHE_FILE, "不得与目录缓存同名");
        assert!(name.starts_with("catalog-") && name.ends_with(".json"));

        // 存量 config 里已登记 `fileName:"catalog.json"` 的资源，重下载时也必须改道。
        let r = RuleResource {
            id: "catalog".into(),
            name: "catalog".into(),
            category: "custom".into(),
            source_url: "https://example.com/catalog.json".into(),
            file_name: CATALOG_CACHE_FILE.into(),
            format: RuleResourceFormat::Source,
            size: 1,
            downloaded_at: "now".into(),
        };
        assert_ne!(
            plan_from_resource(&r).file_name,
            CATALOG_CACHE_FILE,
            "存量登记的撞名也必须改道，否则重下载直接冲掉缓存"
        );

        // 反向：`catalog` 之外的 id 不受影响（保留名单不得误伤）。
        assert_eq!(
            resource_file_name("catalog-cn", RuleResourceFormat::Source),
            "catalog-cn.json"
        );
    }

    /// 短哈希必须是**确定性**的（写进 config 的 `fileName` 会落盘）：
    /// 换编译器/换进程都不得变，否则每次升级都让已下资源变孤儿。
    /// 变异锁：改用 `DefaultHasher`（其算法不保证跨版本稳定）→ 本用例仍绿，但注释里的理由失效；
    /// 故这里钉的是**具体值**，实现一换即红。
    #[test]
    fn short_id_hash_is_deterministic_fnv1a() {
        assert_eq!(
            short_id_hash(""),
            format!("{:08x}", {
                let h: u64 = 0xcbf2_9ce4_8422_2325;
                (h ^ (h >> 32)) as u32
            })
        );
        assert_eq!(short_id_hash("a"), short_id_hash("a"));
        assert_ne!(short_id_hash("a"), short_id_hash("b"));
        assert_eq!(short_id_hash("abc").len(), 8);
    }
}
