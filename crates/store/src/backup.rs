//! 选择性备份 / 恢复的类别模型 + 纯函数（零副作用、零 IO）。
//!
//! Polaris 锚点：`shared/backup-categories.ts`(246) + `main/ipc/handlers/backup-handlers.ts` 的纯逻辑部分
//! （`parseBackupContent` / `sanitizeCrossPlatformRules` / `buildBackupInfo`）。
//!
//! 6 类（前 5 类对应「数据备份与恢复」卡的统计维度，第 6 类是通用设置）：
//!   manualNodes     手动节点   —— servers（无 subscriptionId、非 endpoint 协议）
//!   meshNodes       组网节点   —— servers（无 subscriptionId、endpoint 协议，如 Tailscale/WireGuard）
//!   subscriptions   订阅源     —— subscriptions[] + 其展开节点（servers 有 subscriptionId）；两者一体进出
//!   customRules     自定义规则 —— customRules[] + customRuleSets[]（+ ruleResources 同族）
//!   appRules        应用分流   —— appRules[]（+ appRulesSeeded / customAppPresets 同族）
//!   generalSettings 通用设置   —— 其余所有 config 字段，用**排除法**（自动涵盖未来新增设置）
//!
//! 导出 [`pick_categories`]：只抽选中类的字段。
//! 导入 [`merge_categories`]：选中类**整类替换** current，未选类保留；**空跳过**（选了但备份该类为空 →
//! 不用空覆盖、保留 current、记 skipped），防误删。
//!
//! ## 为什么在 `Value` 上做而不是强类型 `UserConfig`
//!
//! `generalSettings` 的定义是**排除法**（「不在 DATA_FIELDS / EXCLUDED_FROM_BACKUP 里的其余所有键」），
//! 其价值正是「自动涵盖未来新增设置」。强类型 struct 需逐字段列举 → 新增字段必漏，把排除法退化成白名单。
//! 且本 crate 的 [`crate::store::ConfigStore`] 全链路（load/sanitize/migrate/save）本就以 `Value` 为载体。
//!
//! ## 单一真值
//!
//! 前端 `ui/src/shared/backup-categories.ts` 只保留 `BackupCategory` 类型 + `BACKUP_CATEGORIES` 有序清单
//! （跨语言边界的枚举对应物，UI 勾选列表 + IPC 类型用）；分类/计数/合并**逻辑只此一份**，前端经
//! `backup:importPick` 拿后端算好的 available/counts，不在渲染端重算 → 结构上不存在漂移面。
//! 顺序不变式由 [`tests::backup_categories_order_matches_frontend`] 锁住。

#![forbid(unsafe_code)]

use serde_json::{Map, Value};

use polaris_config_engine::user_config::dns_constants::is_sentinel_selection;
use polaris_config_engine::user_config::server_config::{is_mesh_protocol, Protocol};

/// 备份类别（上游 `BackupCategory`）。序列化形 = 前端字符串（camelCase）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BackupCategory {
    ManualNodes,
    MeshNodes,
    Subscriptions,
    CustomRules,
    AppRules,
    GeneralSettings,
}

/// 有序类别清单（UI 展示顺序 + 全选基准）。上游 `BACKUP_CATEGORIES`。
///
/// **顺序是契约**：`detect_categories` 按此序返回，前端 `SettingsBackup.tsx` 按同序渲染勾选行。
pub const BACKUP_CATEGORIES: [BackupCategory; 6] = [
    BackupCategory::ManualNodes,
    BackupCategory::MeshNodes,
    BackupCategory::Subscriptions,
    BackupCategory::CustomRules,
    BackupCategory::AppRules,
    BackupCategory::GeneralSettings,
];

impl BackupCategory {
    /// 前端字符串形（IPC 传输值）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ManualNodes => "manualNodes",
            Self::MeshNodes => "meshNodes",
            Self::Subscriptions => "subscriptions",
            Self::CustomRules => "customRules",
            Self::AppRules => "appRules",
            Self::GeneralSettings => "generalSettings",
        }
    }

    /// 从前端字符串解析。未知值 → `None`（调用方按「忽略未知类」处理，不 throw）。
    ///
    /// 刻意不叫 `from_str`/不实现 `FromStr`：那是「可失败的通用解析」语义（返回 `Result`），
    /// 而本函数是 IPC 线格式的窄映射，未知值属**正常输入**（前端版本漂移）非错误 → `Option` 更贴切。
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        BACKUP_CATEGORIES.into_iter().find(|c| c.as_str() == s)
    }
}

impl serde::Serialize for BackupCategory {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

/// 节点类（manualNodes / meshNodes / subscriptions 三选一）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeCategory {
    Manual,
    Mesh,
    Subscription,
}

impl NodeCategory {
    const fn as_backup(self) -> BackupCategory {
        match self {
            Self::Manual => BackupCategory::ManualNodes,
            Self::Mesh => BackupCategory::MeshNodes,
            Self::Subscription => BackupCategory::Subscriptions,
        }
    }
}

/// 不属于 generalSettings 的「数据字段」（随各自类别走，不进通用设置）。generalSettings = config 其余键。
///
/// - `ruleResources` 随自定义规则类（ruleSet 规则按 `res:<id>` 引用它）；`customAppPresets` 随应用分流类
///   （`appRule.appId` 引用它）。
/// - `selectedServerId` 跟节点走、不随通用设置导入；导入节点后若失效，[`merge_categories`] 末尾主动归零
///   （[`crate::validate::validate_config`] 对失效 selectedServerId 是 **Err、非归零**，不兜底会令整份导入失败）。
const DATA_FIELDS: [&str; 9] = [
    "servers",
    "subscriptions",
    "customRules",
    "customRuleSets",
    "ruleResources",
    "appRules",
    "appRulesSeeded",
    "customAppPresets",
    "selectedServerId",
];

/// 敏感 / 临时态字段：既不进任何类别、也不进通用设置（**绝不写入备份文件**）。
///
/// 与诊断脱敏（`polaris_stats_engine::redact`）是两条独立防线，覆盖面刻意不同：备份文件是「跨机搬运」，
/// clashApiSecret / privacyPassword 是本机凭据，不该跨机；诊断报告是「贴公开 issue」，那边打码即可。
/// 曾有第七项 `diagnosticCapture`（临时诊断态）。整条机制已删除，且旧配置里的残留在 `load` 的
/// 迁移链里就被 [`crate::migrate::migrate_diagnostic_capture`] 清掉 ⇒ 走到备份这一层时该键已不存在，
/// 再留一个排除位就是为一个不可能出现的键守门。**旧备份文件里带着它也无妨**：导入侧同样过迁移链。
const EXCLUDED_FROM_BACKUP: [&str; 5] = [
    "clashApiSecret",      // clash_api 明文密钥，不跨机
    "privacyPassword",     // 隐私解锁密码（legacy 明文残留），绝不入备份
    "privacyPasswordHash", // 隐私解锁密码 salted hash，本机凭据，绝不入备份
    "builtinGeoMeta",      // 内置 geo 元数据（随包，无需备份）
    // 托盘「节点·最近」MRU：**本机使用痕迹**，跨机无意义（外机 id 在本机多半解析不出节点，
    // 白占 3 个槽位之一）；且它是「后端权威」字段（前端零写入权，见 `commands/config.rs`
    // 的 `BACKEND_AUTHORITATIVE_KEYS`），不该经备份这条前端全量提交路径被改写。
    "recentServerIds",
];

/// 是否通用设置键（排除法）。上游 `isGeneralKey`。
fn is_general_key(key: &str) -> bool {
    !DATA_FIELDS.contains(&key) && !EXCLUDED_FROM_BACKUP.contains(&key)
}

/// JS 真值语义：`if (s.subscriptionId)` —— null/undefined/"" 皆为假。
fn truthy_str(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(s)) if !s.is_empty())
}

/// 取数组长度（非数组/缺省 → 0）。对齐 TS `config.x?.length ?? 0`。
fn arr_len(config: &Value, key: &str) -> usize {
    config
        .get(key)
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

/// config.servers 视图（非数组/缺省 → 空）。对齐 TS `config.servers ?? []`。
fn servers(config: &Value) -> &[Value] {
    config
        .get("servers")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

/// 单个 server 归到哪个节点类（订阅节点 > 组网节点 > 手动节点）。上游 `classifyServer`。
///
/// 协议解析失败（未知/缺失/非串）→ 非组网（对齐 TS `isMeshProtocol(undefined)` = false），
/// 即归手动节点：**宁可把坏节点算进手动类也不丢**（备份类别是搬运分桶，不是校验门）。
///
/// 判据与前端的组网页签同源（`is_mesh_node` 的节点级口径）：openconnect / openvpn-client 只在用户
/// 声明了 `meshRoutes` 时才算组网。这里直接读 JSON 而不反序列化整个 `ServerConfig` —— 备份搬的是
/// 用户磁盘上的原始 config，其中可能有本仓当前建模不了的字段，整体反序列化失败就会把整个节点丢掉。
fn classify_server(server: &Value) -> NodeCategory {
    if truthy_str(server.get("subscriptionId")) {
        return NodeCategory::Subscription;
    }
    let proto = server
        .get("protocol")
        .and_then(Value::as_str)
        .and_then(|p| serde_json::from_value::<Protocol>(Value::String(p.to_string())).ok());
    let declares_mesh_routes = || {
        server
            .get("meshRoutes")
            .and_then(Value::as_array)
            .is_some_and(|a| {
                a.iter()
                    .any(|c| c.as_str().is_some_and(|s| !s.trim().is_empty()))
            })
    };
    let is_mesh = proto.is_some_and(|p| {
        is_mesh_protocol(p)
            || (matches!(p, Protocol::Openconnect | Protocol::OpenvpnClient)
                && declares_mesh_routes())
    });
    if is_mesh {
        NodeCategory::Mesh
    } else {
        NodeCategory::Manual
    }
}

/// 某 config 是否含通用设置字段（排除法：存在任一非数据键）。上游 `hasGeneralSettings`。
fn has_general_settings(config: &Value) -> bool {
    config
        .as_object()
        .is_some_and(|m| m.keys().any(|k| is_general_key(k)))
}

/// 某类在 config 中的数量。上游 `countCategory`。
///
/// generalSettings 恒计 1 = 整组；订阅源计**订阅源数**（节点随源不单列）。
#[must_use]
pub fn count_category(config: &Value, cat: BackupCategory) -> usize {
    match cat {
        BackupCategory::ManualNodes => count_nodes(config, NodeCategory::Manual),
        BackupCategory::MeshNodes => count_nodes(config, NodeCategory::Mesh),
        BackupCategory::Subscriptions => arr_len(config, "subscriptions"),
        BackupCategory::CustomRules => {
            arr_len(config, "customRules") + arr_len(config, "customRuleSets")
        }
        BackupCategory::AppRules => arr_len(config, "appRules"),
        BackupCategory::GeneralSettings => usize::from(has_general_settings(config)),
    }
}

fn count_nodes(config: &Value, cat: NodeCategory) -> usize {
    servers(config)
        .iter()
        .filter(|s| classify_server(s) == cat)
        .count()
}

/// config / 备份里「有数据」的类（导入时只列这些供勾选）。上游 `detectCategories`。
///
/// subscriptions 特判：**订阅源为空但存在展开节点**（离线备份/订阅已删而节点还在）也算有数据 —— 否则
/// 那些节点会因「该类不可勾选」而永远导不回来。
#[must_use]
pub fn detect_categories(config: &Value) -> Vec<BackupCategory> {
    BACKUP_CATEGORIES
        .into_iter()
        .filter(|&cat| {
            if cat == BackupCategory::Subscriptions {
                return arr_len(config, "subscriptions") > 0
                    || servers(config)
                        .iter()
                        .any(|s| classify_server(s) == NodeCategory::Subscription);
            }
            count_category(config, cat) > 0
        })
        .collect()
}

/// 导出：从完整 config 抽出选中类的字段（其余字段不进备份）。上游 `pickCategories`。
#[must_use]
pub fn pick_categories(config: &Value, selected: &[BackupCategory]) -> Value {
    let sel = |c: BackupCategory| selected.contains(&c);
    let mut out = Map::new();

    // 三个节点类共用 servers[]：任一被选 → 发射 servers，内容 = 被选类的节点并集。
    let picked_node_cats: Vec<NodeCategory> = [
        NodeCategory::Manual,
        NodeCategory::Mesh,
        NodeCategory::Subscription,
    ]
    .into_iter()
    .filter(|c| sel(c.as_backup()))
    .collect();
    if !picked_node_cats.is_empty() {
        let picked: Vec<Value> = servers(config)
            .iter()
            .filter(|s| picked_node_cats.contains(&classify_server(s)))
            .cloned()
            .collect();
        out.insert("servers".into(), Value::Array(picked));
    }

    if sel(BackupCategory::Subscriptions) {
        if let Some(v) = config.get("subscriptions") {
            out.insert("subscriptions".into(), v.clone());
        }
    }
    if sel(BackupCategory::CustomRules) {
        // TS: `out.customRules = config.customRules`（undefined → JSON.stringify 丢键）→ 缺省即不发射。
        if let Some(v) = config.get("customRules") {
            out.insert("customRules".into(), v.clone());
        }
        // TS: `out.customRuleSets = config.customRuleSets ?? []` → 恒发射（缺省填 []）。
        out.insert(
            "customRuleSets".into(),
            config
                .get("customRuleSets")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
        );
        // TS: `if (config.ruleResources)` → 真值才发射。
        if truthy(config.get("ruleResources")) {
            out.insert("ruleResources".into(), config["ruleResources"].clone());
        }
    }
    if sel(BackupCategory::AppRules) {
        if truthy(config.get("appRules")) {
            out.insert("appRules".into(), config["appRules"].clone());
        }
        if let Some(v) = config.get("appRulesSeeded") {
            out.insert("appRulesSeeded".into(), v.clone());
        }
        if truthy(config.get("customAppPresets")) {
            out.insert(
                "customAppPresets".into(),
                config["customAppPresets"].clone(),
            );
        }
    }
    if sel(BackupCategory::GeneralSettings) {
        if let Some(m) = config.as_object() {
            for (k, v) in m {
                if is_general_key(k) {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
    }
    Value::Object(out)
}

/// JS 真值语义（对象/非空数组/非空串/非零数 → 真；null/undefined/false/0/"" → 假）。
/// 用在 TS 写 `if (config.x)` 的位置；`[]` 在 JS 里是**真值**，故空数组 → 真。
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(_) => true, // 对象 / 数组（含 []）在 JS 里恒真
    }
}

/// [`merge_categories`] 的结果。
#[derive(Debug, Clone)]
pub struct MergeOutcome {
    /// 合并后的新 config。
    pub config: Value,
    /// 选了但备份该类为空、被跳过的类（供 UI 提示）。
    pub skipped: Vec<BackupCategory>,
}

/// 导入：把 backup 的选中类合并进 current（**整类替换 + 空跳过**），未选类保留 current。
/// 上游 `mergeCategories`。
///
/// 空跳过是防误删的红线：用户勾了「手动节点」但备份里一个手动节点都没有 → 保留 current 的手动节点、
/// 记 skipped，**绝不用空数组覆盖**。
#[must_use]
pub fn merge_categories(
    current: &Value,
    backup: &Value,
    selected: &[BackupCategory],
) -> MergeOutcome {
    let sel = |c: BackupCategory| selected.contains(&c);
    let mut skipped: Vec<BackupCategory> = Vec::new();
    let mut result = current.clone();
    if !result.is_object() {
        result = Value::Object(Map::new());
    }

    let cur_servers = servers(current).to_vec();
    let bak_servers = servers(backup).to_vec();
    let mut new_servers: Vec<Value> = Vec::new();

    let take = |src: &[Value], cat: NodeCategory| -> Vec<Value> {
        src.iter()
            .filter(|s| classify_server(s) == cat)
            .cloned()
            .collect()
    };

    // manualNodes / meshNodes：各自整类替换 / 空跳过 / 未选保留
    for cat in [NodeCategory::Manual, NodeCategory::Mesh] {
        let cur_nodes = take(&cur_servers, cat);
        let bak_nodes = take(&bak_servers, cat);
        if !sel(cat.as_backup()) {
            new_servers.extend(cur_nodes); // 未选：保留
        } else if !bak_nodes.is_empty() {
            new_servers.extend(bak_nodes); // 替换
        } else {
            new_servers.extend(cur_nodes); // 空跳过：保留 current
            skipped.push(cat.as_backup());
        }
    }

    // subscriptions：订阅源字段 + 订阅节点一体处理（离线也能恢复）
    let cur_sub_nodes = take(&cur_servers, NodeCategory::Subscription);
    let bak_sub_nodes = take(&bak_servers, NodeCategory::Subscription);
    if sel(BackupCategory::Subscriptions) {
        let has_data = arr_len(backup, "subscriptions") > 0 || !bak_sub_nodes.is_empty();
        if has_data {
            new_servers.extend(bak_sub_nodes);
            let subs = backup
                .get("subscriptions")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            set(&mut result, "subscriptions", subs);
        } else {
            new_servers.extend(cur_sub_nodes); // 空跳过：保留
            skipped.push(BackupCategory::Subscriptions);
        }
    } else {
        new_servers.extend(cur_sub_nodes); // 未选：保留订阅节点 + result.subscriptions 已 = current
    }
    set(&mut result, "servers", Value::Array(new_servers));

    // customRules（+ customRuleSets / ruleResources 同族，同进同出）
    if sel(BackupCategory::CustomRules) {
        let has_data = arr_len(backup, "customRules") > 0 || arr_len(backup, "customRuleSets") > 0;
        if has_data {
            set(
                &mut result,
                "customRules",
                arr_or_empty(backup, "customRules"),
            );
            set(
                &mut result,
                "customRuleSets",
                arr_or_empty(backup, "customRuleSets"),
            );
            set(
                &mut result,
                "ruleResources",
                arr_or_empty(backup, "ruleResources"),
            );
        } else {
            skipped.push(BackupCategory::CustomRules);
        }
    }

    // appRules（+ appRulesSeeded / customAppPresets 同族）
    if sel(BackupCategory::AppRules) {
        if arr_len(backup, "appRules") > 0 {
            set(&mut result, "appRules", backup["appRules"].clone());
            if let Some(v) = backup.get("appRulesSeeded") {
                set(&mut result, "appRulesSeeded", v.clone());
            }
            set(
                &mut result,
                "customAppPresets",
                arr_or_empty(backup, "customAppPresets"),
            );
        } else {
            skipped.push(BackupCategory::AppRules);
        }
    }

    // generalSettings：排除法覆盖所有非数据键
    if sel(BackupCategory::GeneralSettings) {
        let mut applied = false;
        if let Some(m) = backup.as_object() {
            let pairs: Vec<(String, Value)> = m
                .iter()
                .filter(|(k, _)| is_general_key(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (k, v) in pairs {
                set(&mut result, &k, v);
                applied = true;
            }
        }
        if !applied {
            skipped.push(BackupCategory::GeneralSettings);
        }
    }

    // 导入节点类后选中节点可能已不在新 servers → 主动归零（validate_config 对失效 selectedServerId 是 Err、
    // 非归零，不兜底会令整份导入失败）。null 与 direct/block 哨兵不动 —— 哨兵压根不是节点 id，
    // 拿它去 servers 里找必然找不到，归零会把用户选的「直连 / 阻断」出口在导入备份时静默改掉。
    let selected_id = result
        .get("selectedServerId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(id) = selected_id {
        let still_exists = servers(&result)
            .iter()
            .any(|s| s.get("id").and_then(Value::as_str) == Some(id.as_str()));
        if !id.is_empty() && !is_sentinel_selection(Some(&id)) && !still_exists {
            set(&mut result, "selectedServerId", Value::Null);
        }
    }

    MergeOutcome {
        config: result,
        skipped,
    }
}

fn set(target: &mut Value, key: &str, value: Value) {
    if let Some(m) = target.as_object_mut() {
        m.insert(key.to_string(), value);
    }
}

fn arr_or_empty(src: &Value, key: &str) -> Value {
    match src.get(key) {
        Some(v @ Value::Array(_)) => v.clone(),
        _ => Value::Array(Vec::new()),
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 备份文件格式 + 跨平台 sanitize + 摘要（backup-handlers.ts 的纯逻辑部分）
// ════════════════════════════════════════════════════════════════════════════

/// 备份文件版本（上游 `BackupFileFormat.version = '1.0'`）。格式不变 → 版本不动（与 上游 备份互通）。
pub const BACKUP_FILE_VERSION: &str = "1.0";

/// 解析结果：备份 config（Partial）+ 导出平台。
#[derive(Debug, Clone)]
pub struct ParsedBackup {
    /// 备份里的 config（选择性导出后可能只含部分类别字段）。
    pub config: Value,
    /// 导出平台（`process.platform` 口径：win32 / darwin / linux）。旧备份无此字段 → None = 视为同平台。
    pub platform: Option<String>,
}

/// 解析备份文件内容。上游 `parseBackupContent`。
///
/// 兼容新格式 `{version, config}` 与**旧版直接导出的裸 UserConfig**（以 `servers` 存在为标志）。
/// 错误码原样对齐 TS（`invalid_json` / `invalid_format`），前端按码出文案。
///
/// # Errors
/// 坏 JSON → `invalid_json`；既非新格式也非旧格式 → `invalid_format`。
pub fn parse_backup_content(raw: &str) -> Result<ParsedBackup, &'static str> {
    let parsed: Value = serde_json::from_str(raw).map_err(|_| "invalid_json")?;
    if truthy(parsed.get("version")) && truthy(parsed.get("config")) {
        return Ok(ParsedBackup {
            config: parsed["config"].clone(),
            platform: parsed
                .get("platform")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    if parsed.get("servers").is_some() {
        // 旧版直接导出的 UserConfig（TS 判据 `parsed.servers !== undefined`）
        return Ok(ParsedBackup {
            config: parsed,
            platform: None,
        });
    }
    Err("invalid_format")
}

/// 一条规则是否含进程匹配条件（processName / processPath）。上游 `ruleHasProcessCondition`。
///
/// 覆盖**首条件镜像**（`rule.type`）与**多条件**（`rule.conditions[].type`）两种承载。
fn rule_has_process_condition(rule: &Value) -> bool {
    let is_proc = |t: Option<&str>| matches!(t, Some("processName" | "processPath"));
    if is_proc(rule.get("type").and_then(Value::as_str)) {
        return true;
    }
    rule.get("conditions")
        .and_then(Value::as_array)
        .is_some_and(|cs| {
            cs.iter()
                .any(|c| is_proc(c.get("type").and_then(Value::as_str)))
        })
}

/// 跨平台 sanitize：进程规则（processName / processPath）平台特定（chrome.exe / Google Chrome / chrome），
/// 跨平台会静默失效 → **禁用而非移除**（`enabled=false`，保留供用户重映射）。上游 `sanitizeCrossPlatformRules`。
///
/// 就地改 `config.customRules`，返回被禁用条数。同平台 / 旧备份（无 platform）→ 0，不动。
pub fn sanitize_cross_platform_rules(
    config: &mut Value,
    backup_platform: Option<&str>,
    current_platform: &str,
) -> usize {
    let Some(bp) = backup_platform else { return 0 };
    if bp == current_platform {
        return 0;
    }
    let Some(rules) = config.get_mut("customRules").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut n = 0;
    for rule in rules.iter_mut() {
        // TS `rule.enabled !== false`：缺省 / true 都算启用中。
        let enabled = rule.get("enabled") != Some(&Value::Bool(false));
        if enabled && rule_has_process_condition(rule) {
            if let Some(m) = rule.as_object_mut() {
                m.insert("enabled".into(), Value::Bool(false));
            }
            n += 1;
        }
    }
    n
}

/// 配置摘要（上游 `BackupInfo`，`ui/src/ipc/api-client.ts` 1:1 镜像）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInfo {
    pub server_count: usize,
    pub manual_server_count: usize,
    pub mesh_server_count: usize,
    pub subscription_count: usize,
    pub rule_count: usize,
    pub rule_set_count: usize,
    pub app_rule_count: usize,
    /// 跨平台导入时被禁用的进程规则数。0 / 同平台 → 不发射（对齐 TS `|| undefined`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cross_platform_disabled_rules: Option<usize>,
}

/// 构建摘要。上游 `buildBackupInfo`。
#[must_use]
pub fn build_backup_info(config: &Value, cross_platform_disabled_rules: usize) -> BackupInfo {
    BackupInfo {
        server_count: servers(config).len(),
        manual_server_count: count_nodes(config, NodeCategory::Manual),
        mesh_server_count: count_nodes(config, NodeCategory::Mesh),
        subscription_count: arr_len(config, "subscriptions"),
        rule_count: arr_len(config, "customRules"),
        rule_set_count: arr_len(config, "customRuleSets"),
        app_rule_count: arr_len(config, "appRules"),
        cross_platform_disabled_rules: (cross_platform_disabled_rules > 0)
            .then_some(cross_platform_disabled_rules),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use BackupCategory as C;

    fn cfg() -> Value {
        json!({
            "selectedServerId": "m1",
            "proxyMode": "rule",
            "servers": [
                { "id": "m1", "name": "手动1", "protocol": "vless" },
                { "id": "m2", "name": "手动2", "protocol": "trojan" },
                { "id": "w1", "name": "WG", "protocol": "wireguard" },
                { "id": "t1", "name": "TS", "protocol": "tailscale" },
                { "id": "s1", "name": "订阅节点", "protocol": "vless", "subscriptionId": "sub1" },
            ],
            "subscriptions": [{ "id": "sub1", "url": "https://a.example/x" }],
            "customRules": [{ "id": "r1", "type": "domain", "values": ["a.com"] }],
            "customRuleSets": [{ "id": "rs1" }],
            "ruleResources": [{ "id": "res1" }],
            "appRules": [{ "id": "a1", "appId": "chrome" }],
            "appRulesSeeded": true,
            "customAppPresets": [{ "id": "p1" }],
            "clashApiSecret": "SUPERSECRET",
            "privacyPassword": "hunter2",
            "privacyPasswordHash": "d34db33f$c0ffee",
        })
    }

    // ── 分类 ──

    #[test]
    fn classify_subscription_beats_endpoint() {
        // 订阅节点 > 组网节点：即便是 wireguard，只要有 subscriptionId 就归订阅类。
        let s = json!({ "protocol": "wireguard", "subscriptionId": "sub1" });
        assert_eq!(classify_server(&s), NodeCategory::Subscription);
    }

    #[test]
    fn classify_empty_subscription_id_is_not_subscription() {
        // JS 真值语义：subscriptionId="" 是假值 → 不归订阅类。
        let s = json!({ "protocol": "vless", "subscriptionId": "" });
        assert_eq!(classify_server(&s), NodeCategory::Manual);
        let s2 = json!({ "protocol": "vless", "subscriptionId": null });
        assert_eq!(classify_server(&s2), NodeCategory::Manual);
    }

    #[test]
    fn classify_unknown_protocol_falls_back_to_manual() {
        // isMeshProtocol(未知) = false → 手动类（宁可分错桶也不丢节点）
        assert_eq!(
            classify_server(&json!({ "protocol": "nosuchproto" })),
            NodeCategory::Manual
        );
        assert_eq!(classify_server(&json!({})), NodeCategory::Manual);
        assert_eq!(
            classify_server(&json!({ "protocol": 42 })),
            NodeCategory::Manual
        );
    }

    #[test]
    fn classify_endpoint_protocols_are_mesh() {
        assert_eq!(
            classify_server(&json!({ "protocol": "wireguard" })),
            NodeCategory::Mesh
        );
        assert_eq!(
            classify_server(&json!({ "protocol": "tailscale" })),
            NodeCategory::Mesh
        );
    }

    // ── countCategory ──

    #[test]
    fn count_category_all_six() {
        let c = cfg();
        assert_eq!(count_category(&c, C::ManualNodes), 2);
        assert_eq!(count_category(&c, C::MeshNodes), 2);
        assert_eq!(
            count_category(&c, C::Subscriptions),
            1,
            "计订阅源数，不计展开节点"
        );
        assert_eq!(
            count_category(&c, C::CustomRules),
            2,
            "rules(1) + ruleSets(1)"
        );
        assert_eq!(count_category(&c, C::AppRules), 1);
        assert_eq!(count_category(&c, C::GeneralSettings), 1, "恒 1 = 整组");
    }

    #[test]
    fn count_general_settings_zero_when_only_data_fields() {
        let c = json!({ "servers": [], "customRules": [] });
        assert_eq!(count_category(&c, C::GeneralSettings), 0);
    }

    #[test]
    fn count_general_settings_zero_when_only_excluded_fields() {
        // 排除字段不算「通用设置」——否则一份只含 clashApiSecret 的 config 会诓出 generalSettings 类
        let c = json!({ "clashApiSecret": "x", "privacyPassword": "y" });
        assert_eq!(count_category(&c, C::GeneralSettings), 0);
    }

    #[test]
    fn count_on_empty_config_is_zero() {
        let c = json!({});
        for cat in BACKUP_CATEGORIES {
            assert_eq!(count_category(&c, cat), 0, "{cat:?}");
        }
    }

    #[test]
    fn count_tolerates_wrong_types() {
        // servers 非数组 / subscriptions 是字符串 → 不 panic，计 0
        let c = json!({ "servers": "oops", "subscriptions": 3 });
        assert_eq!(count_category(&c, C::ManualNodes), 0);
        assert_eq!(count_category(&c, C::Subscriptions), 0);
    }

    // ── detectCategories ──

    #[test]
    fn detect_returns_all_present_in_declared_order() {
        assert_eq!(
            detect_categories(&cfg()),
            vec![
                C::ManualNodes,
                C::MeshNodes,
                C::Subscriptions,
                C::CustomRules,
                C::AppRules,
                C::GeneralSettings
            ]
        );
    }

    #[test]
    fn detect_subscriptions_via_orphan_nodes_only() {
        // 订阅源为空但存在展开节点（离线备份）→ 仍算有数据，否则那些节点永远导不回来
        let c =
            json!({ "servers": [{ "id": "s1", "protocol": "vless", "subscriptionId": "sub1" }] });
        assert_eq!(detect_categories(&c), vec![C::Subscriptions]);
    }

    #[test]
    fn detect_skips_empty_categories() {
        let c = json!({ "servers": [], "subscriptions": [], "customRules": [] });
        assert_eq!(detect_categories(&c), Vec::<C>::new());
    }

    // ── pickCategories ──

    #[test]
    fn pick_only_selected_node_class() {
        let out = pick_categories(&cfg(), &[C::ManualNodes]);
        let ids: Vec<&str> = out["servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["m1", "m2"]);
        assert!(out.get("subscriptions").is_none());
        assert!(out.get("proxyMode").is_none(), "未选通用设置 → 不带");
    }

    #[test]
    fn pick_never_emits_excluded_secrets() {
        // 红线：clashApiSecret / privacyPassword / privacyPasswordHash 绝不入备份文件，哪怕全选
        let out = pick_categories(&cfg(), &BACKUP_CATEGORIES);
        assert!(out.get("clashApiSecret").is_none());
        assert!(out.get("privacyPassword").is_none());
        assert!(out.get("privacyPasswordHash").is_none());
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("SUPERSECRET"));
        assert!(!s.contains("hunter2"));
        assert!(!s.contains("c0ffee"), "salted hash 绝不入备份");
    }

    #[test]
    fn pick_general_settings_uses_exclusion_rule() {
        // 排除法：未来新增的未知设置键自动进 generalSettings
        let mut c = cfg();
        c["someBrandNewSetting2099"] = json!("v");
        let out = pick_categories(&c, &[C::GeneralSettings]);
        assert_eq!(out["someBrandNewSetting2099"], json!("v"));
        assert_eq!(out["proxyMode"], json!("rule"));
        assert!(out.get("servers").is_none());
        assert!(
            out.get("selectedServerId").is_none(),
            "selectedServerId 跟节点走，非通用设置"
        );
    }

    /// 托盘 MRU 是**本机使用痕迹**，绝不入备份：跨机搬运时外机的节点 id 在本机多半解析不出节点，
    /// 却会白占「节点·最近」3 个槽位之一；且它是「后端权威」字段（前端零写入权，见 `commands/config.rs`
    /// 的 `BACKEND_AUTHORITATIVE_KEYS`），不该经备份这条前端全量提交路径被改写。
    ///
    /// 牙：把 `recentServerIds` 从 `EXCLUDED_FROM_BACKUP` 拿掉 → 它落回排除法的 generalSettings
    /// （`is_general_key` 转真）→ 第一个断言转红。
    #[test]
    fn recent_server_ids_never_enters_backup() {
        let mut c = cfg();
        c["recentServerIds"] = json!(["n1", "n2", "n3"]);
        let out = pick_categories(&c, &[C::GeneralSettings]);
        assert!(
            out.get("recentServerIds").is_none(),
            "托盘 MRU 是本机痕迹 + 后端权威字段，绝不入备份"
        );
        // 同一份 config 里的普通通用设置照常入备份（证明上面不是因为整类没导出而空过）。
        assert_eq!(out["proxyMode"], json!("rule"), "普通通用设置照常导出");
        assert!(
            !is_general_key("recentServerIds"),
            "recentServerIds 不属通用设置"
        );
    }

    #[test]
    fn pick_custom_rules_family_together() {
        let out = pick_categories(&cfg(), &[C::CustomRules]);
        assert!(out.get("customRules").is_some());
        assert!(out.get("customRuleSets").is_some());
        assert!(
            out.get("ruleResources").is_some(),
            "ruleResources 随规则类同进同出"
        );
    }

    #[test]
    fn pick_custom_rule_sets_defaults_to_empty_array() {
        // TS: out.customRuleSets = config.customRuleSets ?? [] → 恒发射
        let c = json!({ "customRules": [{ "id": "r1" }] });
        let out = pick_categories(&c, &[C::CustomRules]);
        assert_eq!(out["customRuleSets"], json!([]));
    }

    #[test]
    fn pick_app_rules_family_together() {
        let out = pick_categories(&cfg(), &[C::AppRules]);
        assert!(out.get("appRules").is_some());
        assert_eq!(out["appRulesSeeded"], json!(true));
        assert!(
            out.get("customAppPresets").is_some(),
            "自定义预设随应用分流同进同出"
        );
    }

    #[test]
    fn pick_multi_node_classes_union() {
        let out = pick_categories(&cfg(), &[C::ManualNodes, C::MeshNodes]);
        let ids: Vec<&str> = out["servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["m1", "m2", "w1", "t1"]);
    }

    #[test]
    fn pick_nothing_selected_is_empty() {
        assert_eq!(pick_categories(&cfg(), &[]), json!({}));
    }

    // ── mergeCategories：整类替换 ──

    #[test]
    fn merge_replaces_selected_class_only() {
        let current = cfg();
        let backup = json!({ "servers": [{ "id": "nm1", "protocol": "vmess" }] });
        let out = merge_categories(&current, &backup, &[C::ManualNodes]);
        let ids: Vec<&str> = servers(&out.config)
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["nm1", "w1", "t1", "s1"],
            "手动类被替换，组网/订阅节点保留"
        );
        assert!(out.skipped.is_empty());
    }

    #[test]
    fn merge_empty_class_is_skipped_not_wiped() {
        // 红线：选了但备份该类为空 → 保留 current，记 skipped，绝不空覆盖
        let current = cfg();
        let backup = json!({ "servers": [] });
        let out = merge_categories(&current, &backup, &[C::ManualNodes, C::MeshNodes]);
        let ids: Vec<&str> = servers(&out.config)
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["m1", "m2", "w1", "t1", "s1"], "全保留");
        assert_eq!(out.skipped, vec![C::ManualNodes, C::MeshNodes]);
    }

    #[test]
    fn merge_unselected_class_untouched() {
        let current = cfg();
        let backup = json!({
            "servers": [{ "id": "nm1", "protocol": "vmess" }],
            "customRules": [{ "id": "nr1", "type": "domain" }],
        });
        let out = merge_categories(&current, &backup, &[C::ManualNodes]);
        assert_eq!(
            out.config["customRules"][0]["id"],
            json!("r1"),
            "未选规则类 → current 原样"
        );
    }

    #[test]
    fn merge_subscriptions_source_and_nodes_together() {
        let current = cfg();
        let backup = json!({
            "servers": [{ "id": "ns1", "protocol": "vless", "subscriptionId": "nsub" }],
            "subscriptions": [{ "id": "nsub" }],
        });
        let out = merge_categories(&current, &backup, &[C::Subscriptions]);
        assert_eq!(out.config["subscriptions"][0]["id"], json!("nsub"));
        let sub_ids: Vec<&str> = servers(&out.config)
            .iter()
            .filter(|s| classify_server(s) == NodeCategory::Subscription)
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(sub_ids, vec!["ns1"]);
    }

    #[test]
    fn merge_subscriptions_offline_nodes_only_still_applies() {
        // 备份只有展开节点、无订阅源 → 仍算有数据（离线恢复），subscriptions 置 []
        let current = cfg();
        let backup =
            json!({ "servers": [{ "id": "ns1", "protocol": "vless", "subscriptionId": "nsub" }] });
        let out = merge_categories(&current, &backup, &[C::Subscriptions]);
        assert_eq!(out.config["subscriptions"], json!([]));
        assert!(out.skipped.is_empty());
    }

    #[test]
    fn merge_subscriptions_empty_is_skipped() {
        let current = cfg();
        let backup = json!({ "servers": [], "subscriptions": [] });
        let out = merge_categories(&current, &backup, &[C::Subscriptions]);
        assert_eq!(out.skipped, vec![C::Subscriptions]);
        assert_eq!(
            out.config["subscriptions"][0]["id"],
            json!("sub1"),
            "current 订阅源保留"
        );
    }

    #[test]
    fn merge_custom_rules_family_replaced_together() {
        let current = cfg();
        let backup = json!({ "customRules": [{ "id": "nr1", "type": "domain" }] });
        let out = merge_categories(&current, &backup, &[C::CustomRules]);
        assert_eq!(out.config["customRules"][0]["id"], json!("nr1"));
        assert_eq!(
            out.config["customRuleSets"],
            json!([]),
            "同族整类替换（备份无 → 置空）"
        );
        assert_eq!(
            out.config["ruleResources"],
            json!([]),
            "同族整类替换（备份无 → 置空）"
        );
    }

    #[test]
    fn merge_custom_rules_via_rule_sets_only() {
        // 只有 ruleSets 也算有数据
        let current = cfg();
        let backup = json!({ "customRuleSets": [{ "id": "nrs1" }] });
        let out = merge_categories(&current, &backup, &[C::CustomRules]);
        assert_eq!(out.config["customRules"], json!([]));
        assert_eq!(out.config["customRuleSets"][0]["id"], json!("nrs1"));
        assert!(out.skipped.is_empty());
    }

    #[test]
    fn merge_custom_rules_empty_is_skipped() {
        let current = cfg();
        let backup = json!({ "customRules": [], "customRuleSets": [] });
        let out = merge_categories(&current, &backup, &[C::CustomRules]);
        assert_eq!(out.skipped, vec![C::CustomRules]);
        assert_eq!(
            out.config["customRules"][0]["id"],
            json!("r1"),
            "current 规则保留"
        );
    }

    #[test]
    fn merge_app_rules_family() {
        let current = cfg();
        let backup = json!({ "appRules": [{ "id": "na1" }], "appRulesSeeded": false });
        let out = merge_categories(&current, &backup, &[C::AppRules]);
        assert_eq!(out.config["appRules"][0]["id"], json!("na1"));
        assert_eq!(out.config["appRulesSeeded"], json!(false));
        assert_eq!(out.config["customAppPresets"], json!([]));
    }

    #[test]
    fn merge_app_rules_empty_is_skipped() {
        let current = cfg();
        let out = merge_categories(&current, &json!({ "appRules": [] }), &[C::AppRules]);
        assert_eq!(out.skipped, vec![C::AppRules]);
        assert_eq!(out.config["appRules"][0]["id"], json!("a1"));
    }

    #[test]
    fn merge_general_settings_by_exclusion() {
        let current = cfg();
        let backup = json!({ "proxyMode": "global", "brandNew2099": 7 });
        let out = merge_categories(&current, &backup, &[C::GeneralSettings]);
        assert_eq!(out.config["proxyMode"], json!("global"));
        assert_eq!(
            out.config["brandNew2099"],
            json!(7),
            "未知设置键自动进通用类"
        );
        assert!(out.skipped.is_empty());
    }

    #[test]
    fn merge_general_settings_empty_is_skipped() {
        let current = cfg();
        let backup = json!({ "servers": [{ "id": "x", "protocol": "vless" }] }); // 只有数据键
        let out = merge_categories(&current, &backup, &[C::GeneralSettings]);
        assert_eq!(out.skipped, vec![C::GeneralSettings]);
        assert_eq!(
            out.config["proxyMode"],
            json!("rule"),
            "current 通用设置保留"
        );
    }

    #[test]
    fn merge_general_settings_ignores_excluded_keys_in_backup() {
        // 手工塞了 clashApiSecret 的备份文件不应把本机凭据覆盖掉
        let current = cfg();
        let backup = json!({ "clashApiSecret": "EVIL", "proxyMode": "global" });
        let out = merge_categories(&current, &backup, &[C::GeneralSettings]);
        assert_eq!(
            out.config["clashApiSecret"],
            json!("SUPERSECRET"),
            "current 本机凭据不被备份覆盖"
        );
    }

    // ── mergeCategories：selectedServerId 兜底 ──

    #[test]
    fn merge_clears_dangling_selected_server_id() {
        // 导入手动节点后 m1 不复存在 → 归零（validate_config 对失效引用是 Err、非归零）
        let current = cfg();
        let backup = json!({ "servers": [{ "id": "nm1", "protocol": "vmess" }] });
        let out = merge_categories(&current, &backup, &[C::ManualNodes]);
        assert_eq!(out.config["selectedServerId"], Value::Null);
    }

    #[test]
    fn merge_keeps_still_valid_selected_server_id() {
        let current = cfg();
        let backup = json!({ "servers": [{ "id": "m1", "protocol": "vmess" }] });
        let out = merge_categories(&current, &backup, &[C::ManualNodes]);
        assert_eq!(out.config["selectedServerId"], json!("m1"));
    }

    #[test]
    fn merge_keeps_direct_sentinel() {
        // __direct__ 哨兵不是节点 id，不该被归零
        let mut current = cfg();
        current["selectedServerId"] = json!("__direct__");
        let backup = json!({ "servers": [{ "id": "nm1", "protocol": "vmess" }] });
        let out = merge_categories(&current, &backup, &[C::ManualNodes]);
        assert_eq!(out.config["selectedServerId"], json!("__direct__"));
    }

    #[test]
    fn merge_keeps_null_selected_server_id() {
        let mut current = cfg();
        current["selectedServerId"] = Value::Null;
        let out = merge_categories(
            &current,
            &json!({ "servers": [{ "id": "n", "protocol": "vless" }] }),
            &[C::ManualNodes],
        );
        assert_eq!(out.config["selectedServerId"], Value::Null);
    }

    #[test]
    fn merge_node_order_is_manual_mesh_subscription() {
        // servers[] 重建顺序固定：手动 → 组网 → 订阅（决定性输出，避免导入后节点乱序）
        let current = cfg();
        let out = merge_categories(&current, &json!({}), &[]);
        let ids: Vec<&str> = servers(&out.config)
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["m1", "m2", "w1", "t1", "s1"]);
    }

    #[test]
    fn merge_nothing_selected_is_identity_on_servers() {
        let current = cfg();
        let out = merge_categories(
            &current,
            &json!({ "servers": [{ "id": "x", "protocol": "vless" }] }),
            &[],
        );
        assert_eq!(servers(&out.config).len(), 5);
        assert!(out.skipped.is_empty());
    }

    // ── 往返：pick → merge ──

    #[test]
    fn roundtrip_pick_then_merge_restores_class() {
        let source = cfg();
        let exported = pick_categories(&source, &[C::ManualNodes, C::MeshNodes]);
        let empty = json!({ "servers": [], "selectedServerId": null });
        let out = merge_categories(&empty, &exported, &[C::ManualNodes, C::MeshNodes]);
        let ids: Vec<&str> = servers(&out.config)
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["m1", "m2", "w1", "t1"]);
        assert!(out.skipped.is_empty());
    }

    // ── parseBackupContent ──

    #[test]
    fn parse_new_format() {
        let raw =
            r#"{"version":"1.0","appVersion":"0.1.0","platform":"darwin","config":{"servers":[]}}"#;
        let p = parse_backup_content(raw).unwrap();
        assert_eq!(p.platform.as_deref(), Some("darwin"));
        assert_eq!(p.config["servers"], json!([]));
    }

    #[test]
    fn parse_legacy_bare_user_config() {
        let raw = r#"{"servers":[{"id":"m1","protocol":"vless"}],"proxyMode":"rule"}"#;
        let p = parse_backup_content(raw).unwrap();
        assert_eq!(p.platform, None, "旧备份无 platform → 视为同平台");
        assert_eq!(p.config["servers"][0]["id"], json!("m1"));
    }

    #[test]
    fn parse_rejects_bad_json_and_bad_shape() {
        assert_eq!(
            parse_backup_content("not json").unwrap_err(),
            "invalid_json"
        );
        assert_eq!(
            parse_backup_content(r#"{"hello":"world"}"#).unwrap_err(),
            "invalid_format"
        );
    }

    #[test]
    fn parse_new_format_without_platform() {
        let raw = r#"{"version":"1.0","config":{"servers":[]}}"#;
        assert_eq!(parse_backup_content(raw).unwrap().platform, None);
    }

    // ── 跨平台 sanitize ──

    #[test]
    fn cross_platform_disables_process_rules() {
        let mut c = json!({ "customRules": [
            { "id": "r1", "type": "processName", "values": ["chrome.exe"] },
            { "id": "r2", "type": "domain", "values": ["a.com"] },
            { "id": "r3", "type": "domain", "conditions": [{ "type": "processPath", "values": ["/x"] }] },
        ]});
        let n = sanitize_cross_platform_rules(&mut c, Some("win32"), "darwin");
        assert_eq!(n, 2, "首条件镜像 + 多条件承载都算");
        assert_eq!(c["customRules"][0]["enabled"], json!(false));
        assert!(
            c["customRules"][1].get("enabled").is_none(),
            "非进程规则不动"
        );
        assert_eq!(c["customRules"][2]["enabled"], json!(false));
    }

    #[test]
    fn cross_platform_noop_on_same_platform_or_legacy() {
        let base = json!({ "customRules": [{ "id": "r1", "type": "processName" }] });
        let mut a = base.clone();
        assert_eq!(
            sanitize_cross_platform_rules(&mut a, Some("darwin"), "darwin"),
            0
        );
        assert!(a["customRules"][0].get("enabled").is_none());
        let mut b = base.clone();
        assert_eq!(
            sanitize_cross_platform_rules(&mut b, None, "darwin"),
            0,
            "旧备份无 platform → 不动"
        );
        assert!(b["customRules"][0].get("enabled").is_none());
    }

    #[test]
    fn cross_platform_skips_already_disabled() {
        let mut c =
            json!({ "customRules": [{ "id": "r1", "type": "processName", "enabled": false }] });
        assert_eq!(
            sanitize_cross_platform_rules(&mut c, Some("win32"), "linux"),
            0,
            "已禁用不重复计数"
        );
    }

    #[test]
    fn cross_platform_tolerates_missing_rules() {
        let mut c = json!({});
        assert_eq!(
            sanitize_cross_platform_rules(&mut c, Some("win32"), "linux"),
            0
        );
        let mut c2 = json!({ "customRules": "oops" });
        assert_eq!(
            sanitize_cross_platform_rules(&mut c2, Some("win32"), "linux"),
            0
        );
    }

    // ── BackupInfo ──

    #[test]
    fn build_info_counts() {
        let i = build_backup_info(&cfg(), 0);
        assert_eq!(i.server_count, 5);
        assert_eq!(i.manual_server_count, 2);
        assert_eq!(i.mesh_server_count, 2);
        assert_eq!(i.subscription_count, 1);
        assert_eq!(i.rule_count, 1);
        assert_eq!(i.rule_set_count, 1);
        assert_eq!(i.app_rule_count, 1);
        assert_eq!(i.cross_platform_disabled_rules, None, "0 → 不发射");
    }

    #[test]
    fn build_info_serializes_camel_case_and_omits_zero_cross_platform() {
        let s = serde_json::to_value(build_backup_info(&cfg(), 0)).unwrap();
        assert_eq!(s["serverCount"], json!(5));
        assert_eq!(s["manualServerCount"], json!(2));
        assert_eq!(s["meshServerCount"], json!(2));
        assert_eq!(s["subscriptionCount"], json!(1));
        assert_eq!(s["ruleCount"], json!(1));
        assert_eq!(s["ruleSetCount"], json!(1));
        assert_eq!(s["appRuleCount"], json!(1));
        assert!(s.get("crossPlatformDisabledRules").is_none());
        let s2 = serde_json::to_value(build_backup_info(&cfg(), 3)).unwrap();
        assert_eq!(s2["crossPlatformDisabledRules"], json!(3));
    }

    // ── 契约锁 ──

    #[test]
    fn backup_categories_order_matches_frontend() {
        // 顺序是契约：ui/src/shared/backup-categories.ts 的 BACKUP_CATEGORIES 同序；
        // SettingsBackup.tsx 按此序渲染勾选行、detect_categories 按此序返回。
        let names: Vec<&str> = BACKUP_CATEGORIES.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "manualNodes",
                "meshNodes",
                "subscriptions",
                "customRules",
                "appRules",
                "generalSettings"
            ]
        );
    }

    #[test]
    fn category_str_roundtrip() {
        for cat in BACKUP_CATEGORIES {
            assert_eq!(BackupCategory::from_wire(cat.as_str()), Some(cat));
        }
        assert_eq!(BackupCategory::from_wire("nope"), None);
        assert_eq!(BackupCategory::from_wire("ManualNodes"), None, "大小写敏感");
    }

    #[test]
    fn category_serializes_to_frontend_string() {
        assert_eq!(
            serde_json::to_value(vec![C::ManualNodes, C::GeneralSettings]).unwrap(),
            json!(["manualNodes", "generalSettings"])
        );
    }

    #[test]
    fn data_fields_and_excluded_are_disjoint() {
        for f in DATA_FIELDS {
            assert!(
                !EXCLUDED_FROM_BACKUP.contains(&f),
                "{f} 同时在两表里，语义冲突"
            );
        }
    }
}
