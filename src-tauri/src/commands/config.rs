//! 配置类 command（上游 `config-handlers.ts` + `privacy-handlers.ts`）。
//!
//! 映射 channel：
//! - `config:get` → [`config_get`]
//! - `config:save` → [`config_save`]（落盘 + 广播 event:configChanged）
//! - `config:updateMode` → [`config_update_mode`]
//! - `config:getValue` → [`config_get_value`]
//! - `config:setValue` → [`config_set_value`]
//! - `config:getPrivacyMode` / `config:setPrivacyMode` → [`config_get_privacy_mode`] / [`config_set_privacy_mode`]
//! - `privacy:setPassword` / `privacy:unlock` / `privacy:hasPassword` → [`privacy_set_password`] /
//!   [`privacy_unlock`] / [`privacy_has_password`]（scrypt 独立文件 privacy-lock.json + 存量 SHA-256 平滑迁移）
//!
//! F29：config_get 绝不下发隐私密码（privacyPassword 字段剥除）——对齐 Polaris。

#![allow(clippy::needless_pass_by_value)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use polaris_store::fs::StdFs;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::subscription::{
    invalidate_validators_on_global_ua_change, SUBSCRIPTION_USER_AGENT_KEY,
};
use crate::events::channel::{
    EVENT_CONFIG_CHANGED, EVENT_ENTER_PRIVACY_MODE, EVENT_EXIT_PRIVACY_MODE,
};
use crate::response::{ok_void, ApiResponse};
use crate::runtime::config::ConfigManager;
use crate::runtime::proxy::StagedClassification;
use crate::runtime::unlock::{
    selected_exit_changed, BroadcastSink, UnlockEventSink, UnlockRuntime,
};
use crate::runtime::AppRuntime;
use polaris_config_engine::builder::orchestration::stable_stringify;
use serde::Serialize;

/// 上游 `CONFIG_GET`：加载完整 UserConfig（剥除 privacyPassword）。
///
/// F1：`bypassLANList` 缺省时在此边界补成生效默认（27 条 `DEFAULT_BYPASS_LAN`），使 UI
/// 的旁路 / route_exclude 编辑器永远编辑真实清单 —— 否则首个按键会把前端 3 条兜底当用户清单
/// 持久化，静默丢弃 24 条真实默认。语义镜像 `effective_bypass_lan`，对 builder 透明。
#[tauri::command]
pub fn config_get(state: State<'_, AppRuntime>) -> ApiResponse<Value> {
    // 启动期一次性配置维护（对齐 上游 `loadConfig` 内联步骤，Polaris 运行时 `load_full` 未接线 →
    // 收口在前端首个配置入口）：清孤儿 tmp + 回填 clashApiSecret + F29 旧明文密码无损迁移为哈希。best-effort。
    run_startup_maintenance_once(state.config());
    match state.config().load_full() {
        Ok(mut cfg) => {
            apply_frontend_view(&mut cfg);
            ApiResponse::ok(cfg)
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// 磁盘配置 → **渲染端看到的那一份**的投影。`config_get` 的下发形，也是 [`config_version`] 的定义域。
///
/// # 为什么必须抽出来（而不是让 `config_get` 内联这两步）
///
/// 乐观并发的版本号两侧各算（spec §3.7）：前端对 `config:get` 拿到的 config 算 FNV 短 hash，
/// 后端对磁盘现值算。两边算的若不是**同一份文档**，版本恒不等 ⇒ 每一次带 `base_version` 的保存
/// 都返 conflict，功能整体失效。而 `config_get` 恰好不是原样下发：
///
///  - `strip_privacy_secrets`：设过隐私密码的机器上，磁盘有 `privacyPasswordHash`、前端没有；
///  - `ensure_bypass_lan_list`：磁盘缺 `bypassLANList` 时前端拿到的是补齐后的 27 条默认。
///
/// 两条都足以让「hash 磁盘」与「hash 前端那份」系统性分叉。故版本的定义域**只能**是本投影。
fn apply_frontend_view(cfg: &mut Value) {
    // F29：绝不下发隐私密码（历史残留明文 `privacyPassword` + salted hash `privacyPasswordHash`）。
    strip_privacy_secrets(cfg);
    // F1：补齐 bypassLANList，防编辑器首个按键坍塌默认。
    polaris_config_engine::user_config::system_proxy_bypass::ensure_bypass_lan_list(cfg);
}

/// 配置的**内容版本**（spec §2.3.3）：渲染端投影经 `stable_stringify` 后取 FNV-1a 32 位短 hash。
///
/// 不用 mtime（同秒两次写可能相等），不用自增计数（进程重启即失忆）。
///
/// # 与前端 `configBaseVersion` 的逐字节等价（`ui/src/lib/staged-config.ts`）
///
/// 两侧各算、不走 IPC 往返，故实现必须逐位对齐，三处易错点：
///
///  1. **哈希单元是 UTF-16 code unit**，不是 UTF-8 字节 —— JS 侧是 `text.charCodeAt(i)`。
///     故此处走 `encode_utf16()`；写成 `bytes()` 会在任何非 ASCII 字符串（节点名、备注）上分叉。
///  2. **乘法回绕**：JS 侧 `Math.imul` 是 32 位有符号回绕乘 ⇒ 此处 `wrapping_mul`。
///  3. **序列化必须同源**：`stable_stringify` 键序无关、数组保序，与前端 `stableStringify` 同规。
///
/// 由 `ui/src/contracts/config-version.fixture.json` 的双侧固定 fixture 锁住（值一致性，非表一致性）。
///
/// # 已知边界（不在本轮射程）
///
/// serde_json 把「JSON 字面量带小数点的整数」（`5000.0`）序列化回 `5000.0`，而 JS `JSON.stringify`
/// 输出 `5000` ⇒ 该形态下两侧分叉。config 里唯一的浮点字段是 `dnsConfig.dnsTimeoutMs`，其写入路径
/// （前端提交 / `sanitize_dns_config` 取整成 i64）都产出整数字面量，故只有**手改 config.json 写成
/// `5000.0`** 才够得着。后果是保存恒返 conflict（不丢数据、不误写），不是静默错值。
fn config_version(cfg: &Value) -> String {
    let mut view = cfg.clone();
    apply_frontend_view(&mut view);
    config_content_hash(&view)
}

/// 版本函数的**纯哈希那一半**（与前端 `configBaseVersion` 逐位对齐的就是这个函数）。
///
/// 与 [`config_version`] 拆开是为了让跨语言 fixture 锁只锁哈希、不受渲染端投影的干扰：
/// 投影是「哪一份文档」的问题，哈希是「怎么算」的问题，两者各有各的门，混在一起会让
/// fixture 被迫写成「已投影形」，而那个前提读者无从校验。
fn config_content_hash(cfg: &Value) -> String {
    let text = stable_stringify(cfg);
    let mut hash: u32 = 0x811c_9dc5;
    for unit in text.encode_utf16() {
        hash ^= u32::from(unit);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

/// `config:save` 的结果（spec §2.3.3）。
///
/// **conflict 不是错误**：它不走 `ApiResponse::err`，因为「磁盘在你编辑期间被别人改了」是一个
/// 正常结局 —— 前端据此走合并腿（Q8-b），而不是弹一个报错。走 err 会让它和「落盘 IO 失败」
/// 挤在同一条通道里，前端只能靠 message 文本区分，那是最脆的一种分派。
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum SaveOutcome {
    /// 已落盘。`version` = 落盘后的新版本号（前端据此把 staged 的锚点刷到新值）。
    Saved { version: String },
    /// 磁盘现值 ≠ `base_version` ⇒ **一个字节都没写**。`diskVersion` 供前端定位它该基于哪一版重放。
    #[serde(rename_all = "camelCase")]
    Conflict { disk_version: String },
}

/// 上游 `CONFIG_SAVE`：保存 UserConfig + 广播 event:configChanged。
///
/// `deferRestart`（可选，缺省 `false`）= 暂存层「保存」腿的**不主动重启**标志（spec §2.5 Q4）。
/// 前端不传 ⇒ 与今天逐字节相同；传 `true` ⇒ 结构性变更只落盘 + 进待应用差集，重启时机交给
/// 「立即应用」。射程只到 switch-engine 第 4 腿，`must_restart` 腿不受影响
/// （因果在 `DecisionInput::defer_restart`）。
///
/// `baseVersion`（可选，缺省 = **不校验**）= 乐观并发的基准版本（spec §2.5 Q8-b）。既有十余个
/// 调用点不传该参数 ⇒ 行为与今天逐字节相同。传了且与磁盘现值不符 ⇒ 返 `conflict` 且**一个字节都不写**。
#[tauri::command]
pub fn config_save(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    config: Value,
    defer_restart: Option<bool>,
    base_version: Option<String>,
) -> ApiResponse<SaveOutcome> {
    // 本地转 mut 而非参数上写 `mut config`：check-ipc-args.mjs 的 Rust 形参解析不 strip `mut`，会把
    // 参数名误读成 `mut config` 从而要求前端多传该键（运行期 Tauri 其实 strip 了、无害，但 CI 门会红）。
    let mut config = config;
    // A7（R21）：落盘前快照旧选中出口，供保存后比对（全量保存 / 备份恢复可换出口而不走 server_switch）。
    let old_selected = current_selected_server_id(state.config());
    match config_save_core(state.config(), &mut config, base_version.as_deref()) {
        // 冲突腿**不广播、不入核**：磁盘没变，广播出去只会让所有窗口把一份从未落盘的配置当现值。
        Ok(outcome @ SaveOutcome::Conflict { .. }) => ApiResponse::ok(outcome),
        Ok(outcome) => {
            broadcast_config_changed_with(&app, &config, defer_restart.unwrap_or(false));
            invalidate_unlock_on_exit_change(
                state.unlock(),
                &BroadcastSink::new(&app),
                state.proxy().status().running,
                old_selected.as_deref(),
                config.get("selectedServerId").and_then(Value::as_str),
            );
            ApiResponse::ok(outcome)
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// `CONFIG_CLASSIFY_STAGED`（spec §2.3.4）：候选配置**若现在落盘**会走哪条腿。
///
/// **只读、零副作用**：不落盘、不碰核、不 emit。用于暂存层在保存**之前**逐条标注
/// 「保存即生效 / 需重启生效」（FR-9），从而解释「5 项待保存 → 保存 → 2 项待应用」这个转移。
///
/// 判定本体在 [`ProxyRuntime::classify_staged`]，与真正的 `switch_mode` 共用同一个
/// [`classify_switch`](crate::runtime::proxy::ProxyRuntime) —— 预告与实际在构造上不可能分歧。
#[tauri::command]
pub fn config_classify_staged(
    state: State<'_, AppRuntime>,
    config: Value,
) -> ApiResponse<StagedClassification> {
    ApiResponse::ok(state.proxy().classify_staged(&config))
}

/// `config_save` 的可测核心（剥掉 `AppHandle`/`State`，单测能直接调）。
///
/// **抽出来是为了让测试走生产路径**：若测试自己调 `preserve_server_owned_secrets` 再 `save_full`，
/// 那么删掉生产代码里的回填调用测试照样绿 = 假绿（本仓刚因同类假绿漏掉一个隐私锁失效的洞）。
/// 让二者共用本函数后，回填与落盘的**顺序与配对**才真被测试锁住。
/// # 乐观并发校验为什么钉在**最顶端**（R6）
///
/// 下面三条策略都以「磁盘现值」为输入、并就地改写 `incoming`。校验若排在它们之后：
///  - `incoming` 已被回填/覆盖过 —— 冲突腿本该「一个字节都没动」，实际却交还了一份被改过的入参；
///  - 校验基准与「用户提交的到底是什么」之间多出一层后端自己刚加的东西，判据不再是纯粹的
///    「磁盘变没变」。
///
/// 由 `optimistic_conflict_touches_nothing` 钉住（把这段挪到三条策略之后即转红）。
fn config_save_core(
    config: &ConfigManager,
    incoming: &mut Value,
    base_version: Option<&str>,
) -> Result<SaveOutcome, polaris_store::StoreError> {
    if let Some(base) = base_version {
        let disk_version = config_version(&config.current()?);
        if base != disk_version {
            return Ok(SaveOutcome::Conflict { disk_version });
        }
    }
    // 前端提交的全量 config 恒不含隐私 hash（`config_get` 是全量快照的唯一出口，由
    // strip_privacy_secrets strip 掉了；configChanged 已无载荷，不构成全量快照出口）→
    // 不回填就等于每次保存都拆锁。（单键出口 `config_get_value` 另经 `is_privacy_key` 挡，
    // 但它不产生全量快照，不是本行回填逻辑依赖的对象。）
    preserve_server_owned_secrets(config, incoming);
    // 后端权威字段以磁盘为准（前端快照对这些键恒可能陈旧，见 enforce_ 文档）。
    enforce_backend_authoritative_fields(config, incoming);
    // 全局订阅 UA 变更 → 作废受影响订阅的条件 GET 验证器（见 [`invalidate_stale_subscription_validators`]）。
    // 必须排在 `save_full` **之前**：落盘后再清等于没清（这一版验证器已经进磁盘了）。
    invalidate_stale_subscription_validators(config, incoming);
    config.save_full(incoming)?;
    // 落盘后的版本取 `current()`（`save_full` 刚把缓存刷成这一份）—— 前端拿它当新锚点，
    // 与下一次 `config:get` 的版本同源。
    Ok(SaveOutcome::Saved {
        version: config_version(&config.current()?),
    })
}

// ── 全局订阅 UA 变更 → 条件 GET 验证器作废（config 写入侧的那一半）────────────────────
//
// per-sub UA 那一级由 `commands/subscription.rs` 的 `subscription_update` 收口；全局
// `subscriptionUserAgent` 只能经**本文件的两个写命令**改（`config:save` 全量提交 / `config:setValue`
// 单键写），故两处各挂一次。判据本体（含「带 per-sub 覆盖的订阅不该被牵连」这条射程限制）收在
// `subscription.rs::invalidate_validators_on_global_ua_change` —— UA 的归一与优先级语义只有一份。

/// 全量保存腿：拿盘上旧配置与入参比全局 UA，变了就清受影响订阅的 `etag`/`lastModified`。
///
/// 读不到当前配置（首启无文件等）→ 无旧值可比，跳过（保守：判不准不误清，与同文件
/// [`preserve_server_owned_secrets`] / [`enforce_backend_authoritative_fields`] 同款取向）。
fn invalidate_stale_subscription_validators(config: &ConfigManager, incoming: &mut Value) {
    let Ok(current) = config.current() else {
        return;
    };
    log_invalidated_validators(invalidate_validators_on_global_ua_change(
        &current, incoming,
    ));
}

/// 备份导入腿的**落盘前收口**（三条策略 + 保存），与 [`config_save_core`] 是同一条流水线的第三个入口。
///
/// # 为什么抽出来（理由与 [`config_save_core`] 逐字相同）
///
/// `backup_import_apply` 持 `State<'_, AppRuntime>` + `AppHandle`，单测构造不出 Tauri 运行时 ⇒ 若测试
/// 自己按顺序调那三个函数再 `save_full`，「命令里少挂一条」对测试是**恒绿**的（本仓已因同类假绿漏过
/// 隐私锁与后端权威字段两次）。收口成一个函数后，三条策略的**存在、顺序与落盘配对**才真被测试锁住。
///
/// # 三条策略各自守什么
///
/// 1. [`preserve_server_owned_secrets`]：备份文件不含隐私 hash（导出侧脱敏）→ 不回填 = 导入即拆锁；
/// 2. [`enforce_backend_authoritative_fields`]：外机的托盘 MRU / geo 元数据不得覆盖本机真值；
/// 3. [`invalidate_validators_on_global_ua_change`]：**这条腿此前缺失**。`subscriptionUserAgent` 按排除法
///    属 generalSettings 类（既不在 `DATA_FIELDS` 也不在 `EXCLUDED_FROM_BACKUP`，见 `store::backup`）⇒
///    勾了「通用设置」的导入就能改全局 UA，而本机订阅的 `etag`/`lastModified` 原样留着 ⇒ 机场按 UA 下发
///    变体时**恒 304**、新格式永远拿不到（与 `config:save` / `config:setValue` 两腿是同一个洞的第三条腿）。
///
/// `current` = 本机磁盘现值。命令层为 `merge_categories` 已读过一次，此处**复用同一份快照**而不再
/// `config.current()`：UA 比对的基准必须与 merge 的基准是同一张快照，否则两者之间被别的写腿改过 UA 时，
/// 会按一个从未参与 merge 的旧值判「没变」而漏清。
pub(crate) fn backup_import_save_core(
    config: &ConfigManager,
    current: &Value,
    restored: &mut Value,
) -> Result<(), polaris_store::StoreError> {
    preserve_server_owned_secrets(config, restored);
    enforce_backend_authoritative_fields(config, restored);
    // 必须排在 `save_full` **之前**：落盘后再清等于没清（这一版验证器已经进磁盘了）。
    log_invalidated_validators(invalidate_validators_on_global_ua_change(current, restored));
    config.save_full(restored)
}

/// 作废条数的统一日志（三条写腿共用；0 条不出声，避免每次保存都刷一行）。
fn log_invalidated_validators(n: usize) {
    if n > 0 {
        log::info!(
            "全局订阅 UA 变更 → 已作废 {n} 条订阅的条件 GET 验证器（下次更新走全量 GET，\
             不再因机场按 UA 下发变体而恒 304）"
        );
    }
}

/// [`ConfigManager::set_value`] 的**订阅 UA 感知**包装。
///
/// # 为什么包在命令层而不是改 `ConfigManager::set_value`
///
/// 那是 `runtime/config.rs` 里与业务语义无关的通用顶层键写入器（任何键都走它）。把「订阅验证器」
/// 这种领域知识塞进去，等于让配置运行时依赖订阅模块的语义。命令层是既持 `ConfigManager`、
/// 又允许知道订阅语义的那一层，故收口在此。
///
/// 非 [`SUBSCRIPTION_USER_AGENT_KEY`] → **逐字**走原路径（零行为变化）。是该键 → 复刻 `set_value`
/// 的「current → 插键 → `save_full`」三步，在落盘前插入验证器作废（顺序同 [`config_save_core`]）。
fn set_value_with_ua_invalidation(
    config: &ConfigManager,
    key: &str,
    value: Value,
) -> Result<Value, polaris_store::StoreError> {
    if key != SUBSCRIPTION_USER_AGENT_KEY {
        return config.set_value(key, value);
    }
    let old = config.current()?;
    let mut cfg = old.clone();
    if let Some(obj) = cfg.as_object_mut() {
        obj.insert(key.to_string(), value);
    }
    log_invalidated_validators(invalidate_validators_on_global_ua_change(&old, &mut cfg));
    config.save_full(&cfg)?;
    Ok(cfg)
}

/// 上游 `CONFIG_UPDATE_MODE`：更新 proxyMode 字段。
#[tauri::command]
pub fn config_update_mode(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    mode: Value,
) -> ApiResponse<()> {
    match state.config().set_value("proxyMode", mode) {
        Ok(cfg) => {
            broadcast_config_changed(&app, &cfg);
            ok_void()
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

/// [`config_get_value`] 可测核心（剥掉 `State<'_, AppRuntime>`）：隐私密钥
/// （[`is_privacy_key`]：legacy 明文 `privacyPassword` / legacy hash `privacyPasswordHash`，与
/// `config_get`/`strip_privacy_secrets` 剥的是同一份真值）单键读也不放行——命中即当「键不存在」
/// 处理，短路返回 `Null`，不额外暴露「这个键存在但被挡下」这个信号，与全量快照出口剥除整键（读出来
/// 就是没有这键）的效果对齐；其余键照常走 [`ConfigManager::get_value`] 直读。
///
/// 拆成 `_core`（同 [`config_save_core`]/`unlock_core` 的理由）不只是为了绕开「单测构造不出 Tauri
/// 运行时」——`ConfigManager` 本身不需要 Tauri，可以直接 `ConfigManager::new` 拿一份真实例，让
/// `config_get_value_core_blocks_privacy_keys_even_when_present_on_disk` 端到端地证明「磁盘上真有
/// 这个哈希、读接口却真的拿不到」，而不是靠源码扫描推断。
fn config_get_value_core(
    config: &ConfigManager,
    key: &str,
) -> Result<Value, polaris_store::StoreError> {
    if is_privacy_key(key) {
        return Ok(Value::Null);
    }
    config.get_value(key)
}

/// 上游 `CONFIG_GET_VALUE`：取单键（currentConfig 投影）。见 [`config_get_value_core`]。
#[tauri::command]
pub fn config_get_value(state: State<'_, AppRuntime>, key: String) -> ApiResponse<Value> {
    ApiResponse::from_result(config_get_value_core(state.config(), &key))
}

/// 上游 `CONFIG_SET_VALUE`：置单键 + 广播 event:configChanged。
#[tauri::command]
pub fn config_set_value(
    app: AppHandle,
    state: State<'_, AppRuntime>,
    key: String,
    value: Value,
) -> ApiResponse<()> {
    // A7（R21）：置键前快照旧选中出口（直接置 `selectedServerId` 键 = 换出口，不走 server_switch）。
    let old_selected = current_selected_server_id(state.config());
    // 单键写腿同样能改全局订阅 UA → 经 [`set_value_with_ua_invalidation`] 落盘（其余键零行为变化）。
    match set_value_with_ua_invalidation(state.config(), &key, value) {
        Ok(cfg) => {
            broadcast_config_changed(&app, &cfg);
            invalidate_unlock_on_exit_change(
                state.unlock(),
                &BroadcastSink::new(&app),
                state.proxy().status().running,
                old_selected.as_deref(),
                cfg.get("selectedServerId").and_then(Value::as_str),
            );
            ok_void()
        }
        Err(e) => ApiResponse::err(format!("{e}")),
    }
}

// ── A7（R21）：换出口后作废旧解锁探测缓存 ────────────────────────────────────────
//
// server_switch（β 已接，`commands/server.rs`）之外，config 写路径也能改选中出口（`selectedServerId`）
// 而不走 server_switch：`config_save`（前端全量保存 / 备份恢复整份覆盖）与 `config_set_value`（直接置
// `selectedServerId` 键）。换出口后旧出口的解锁角标最长陈旧 30min（缓存 `FRESH_TTL_MS`）。此处按与
// server_switch **同款判准**（出口 identity 变 = 失效）在这两条命令层腿补接线——命令层持 `State<AppRuntime>`
// （可达 `unlock()`/`proxy()`）+ `AppHandle`（建 `BroadcastSink`），是能触达失效契约的正确层
// （`ProxyRuntime` 内部不持 unlock/AppHandle，故失效不在 `runtime/proxy.rs` 内接）。
//
// **守卫「同 id 不失效」**：出口未变（含改无关 config 键）→ 不失效，避免每次设置写都白刷解锁探测。
//
// 判准谓词 `selected_exit_changed` 收敛到 `runtime::unlock`（四写腿共用单一真值源），此处 use 引入。

/// 读当前选中出口 id（落盘前快照，用于出口变判定）。读不到（首启无文件等）→ None（保守：判不准不误失效）。
fn current_selected_server_id(config: &ConfigManager) -> Option<String> {
    config.current().ok().and_then(|c| {
        c.get("selectedServerId")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

/// A7（R21）失效决策**可测核心**（剥掉 `AppHandle`/`State`）：仅当出口 identity 真变时经注入的
/// `UnlockEventSink` 调 `UnlockRuntime::invalidate`。单测注 `UnlockRuntime` + 记录型 sink 摆 old/new
/// 即可断言「变→失效一次 / 不变→零失效」，无需 Tauri 运行时。
///
/// `exit_blocked=false`：切换瞬间尚未探新出口，交前端按 `running` 复位「检测中」并重跑（对齐 invalidate
/// 契约，同 server_switch）。
fn invalidate_unlock_on_exit_change<S: UnlockEventSink>(
    unlock: &UnlockRuntime,
    sink: &S,
    running: bool,
    old_selected: Option<&str>,
    new_selected: Option<&str>,
) {
    if selected_exit_changed(old_selected, new_selected) {
        unlock.invalidate(sink, running, false);
    }
}

// ── 隐私锁状态机（F29；FX-privacy-kdf 升级 scrypt + 独立文件）────────────────────────
//
// 此前三占位是**安全洞**：set_password 空转、unlock 恒 true、has_password 恒 false ——
// 任何人无需密码即可退出隐私模式。现落真状态机（对齐 上游 `main/utils/privacy-lock.ts`）：
//   - **存储（新真值源）**：scrypt 哈希（memory-hard 慢哈希）存**独立文件** `<userData>/privacy-lock.json`
//     （0600，仅属主读写），落盘结构 `{algo,salt,hash,params}`（见 `store::privacy_lock`）。独立文件**永不进
//     config 对象** → 天然免疫「前端全量保存把 config 里的密钥静默抹除」这类洞，也无需在 10+ 个 configChanged
//     广播点脱敏（上游 选独立文件的原始理由）。scrypt 参数逐字对齐 上游 交互档（N=2^14/r=8/p=1/keyLen=32，
//     salt 16B CSPRNG）。
//   - **为何从 salted SHA-256 升级**：早期把 salted SHA-256（**快**哈希，GPU 每秒几十亿次暴力）存进 config.json
//     `privacyPasswordHash`。SHA-256 无 KDF 慢化 → 离线撞库成本极低。scrypt memory-hard 单次 ~50-100ms，
//     抬高暴力成本数个量级。
//   - **存量迁移（不锁死老用户）**：读侧优先 scrypt 文件；文件不存在时回退 config.json 里的 legacy salted-SHA256
//     `privacyPasswordHash`（旧版本存量），**验过即透明升级**到 scrypt 文件并抹掉旧键（见 [`unlock_core`]）；
//     `set` 新密码亦直接落 scrypt 文件 + 抹旧键。旧密码**验败绝不删旧键**（防锁死）。写文件在前、抹旧键在后，
//     任一步失败都不会出现「两者皆无」的锁死窗口。SHA-256 是单向 → 无法在启动期无明文批量转 scrypt，故迁移只能
//     在「拿得到明文」的 unlock/set 时刻惰性做。
//   - **legacy 键防护（过渡期）**：legacy `privacyPasswordHash`（未迁移态）+ 历史明文 `privacyPassword` 仍由
//     [`strip_privacy_secrets`] 在 `config_get`（全量快照的唯一出口）剥除（绝不下发前端；`configChanged`
//     已无载荷，`strip_privacy_secrets` 在那条广播路径上服务的是入核的那份 `cfg`，不是发给前端的）；单键
//     出口 `config_get_value` 另经 [`is_privacy_key`] 短路挡下同一份键。backup / 诊断脱敏亦排除（见
//     store::backup / stats_engine::redact）。scrypt 独立文件本就不在 config 里，无从经这些出口泄漏。
//   - **校验**：scrypt 与 legacy SHA-256 均**常量时间比较**，仅匹配返 true。
//   - 隐私模式开关：进程内状态（随重启复位，对齐前端 app-store）；enter/exit 状态变更时
//     emit `EVENT_ENTER/EXIT_PRIVACY_MODE`。

/// 隐私模式当前状态（进程内；重启复位——对齐前端 app-store 的 `privacyMode: false` 初值）。
static PRIVACY_MODE: AtomicBool = AtomicBool::new(false);

/// 历史遗留明文密码键（旧版本残留）。由 `store::migrate` 每次 load 清空 + 本层在 `config_get`
/// （全量快照的唯一出口；`configChanged` 已无载荷，不构成全量快照出口）与单键出口
/// `config_get_value`（经 [`is_privacy_key`]）两处剥除。
const PRIVACY_PASSWORD_KEY: &str = "privacyPassword";

/// **legacy** 隐私密码 salted-SHA256 存储键（FX-privacy-kdf 之前的旧真值源）。新真值源已迁至独立
/// `privacy-lock.json`（scrypt）；此键仅为**存量未迁移用户**保留读取/校验 + 迁移完成后清除。
/// `config_get`（全量快照的唯一出口）与 `config_get_value`（单键出口，经 [`is_privacy_key`]）均
/// 剥除此键 → 绝不下发前端；`broadcast_config_changed` 里的剥除服务的是入核那份 `cfg`，
/// `configChanged` 广播本身已无载荷，不构成前端出口。
const PRIVACY_PASSWORD_HASH_KEY: &str = "privacyPasswordHash";

/// 隐私锁独立文件路径（`<userData>/privacy-lock.json`，与 config.json 同目录）。scrypt 新真值源。
fn privacy_lock_path(config: &ConfigManager) -> PathBuf {
    polaris_store::privacy_lock::lock_path(config.dir())
}

/// legacy 隐私键的**单一真值源**：[`strip_privacy_secrets`]（全量出口剥除）与 [`is_privacy_key`]
/// （单键出口 [`config_get_value`] 短路）共用同一份列表，而不是「一边 `remove` 两句、一边 `||`
/// 两句」各写各的——那种写法只是没抄常量的**名字**，抄了常量的**用法**，后人往 `strip_privacy_secrets`
/// 加第三个键、忘了同步 `is_privacy_key`，两边都还是「合法 Rust」，编译期与既有测试（各自只覆盖
/// 已知两个键）都发现不了分叉。列表是唯一的，分叉在这个共用点上写不出来。
const PRIVACY_KEYS: [&str; 2] = [PRIVACY_PASSWORD_KEY, PRIVACY_PASSWORD_HASH_KEY];

/// 剥除绝不下发前端的隐私密钥键：legacy 明文 `privacyPassword` + legacy salted-SHA256 `privacyPasswordHash`。
/// `config_get`（读出口）与 `broadcast_config_changed`（写广播出口）共用同一份 —— 防任一处漏剥。
/// （scrypt 新真值源在独立文件，本就不在 config 里，无需在此剥除。）
fn strip_privacy_secrets(cfg: &mut Value) {
    if let Some(obj) = cfg.as_object_mut() {
        for key in PRIVACY_KEYS {
            obj.remove(key);
        }
    }
}

/// `key` 是否命中 legacy 隐私键——与 [`strip_privacy_secrets`] 剥的是同一份 [`PRIVACY_KEYS`]，
/// 供单键出口 [`config_get_value`] 复用。
fn is_privacy_key(key: &str) -> bool {
    PRIVACY_KEYS.contains(&key)
}

/// 回填「服务端独占」的隐私密钥，供**前端来的全量保存**用。
///
/// # 为什么必须有
///
/// `config_get`（全量快照的唯一出口）经 [`strip_privacy_secrets`]（hash 绝不下发；`configChanged`
/// 已无载荷，不构成出口），故前端 store
/// 里的 config **恒无** `privacyPasswordHash`。用户改任意设置走 `saveConfig({...config, ...})` 全量提交
/// → `save_full` 全量覆盖 → 磁盘与缓存里的 hash 被静默抹除 → `has_password` 恒 false、`unlock` 任意
/// 密码放行（`unlock_core`：hash 为空 = 未设密码 = 自由解锁）。即：**设了隐私密码后，第一次改任何
/// 设置就等于把锁拆了**，且用户无感。
///
/// # 为什么不做在 `save_full`（唯一汇流点）里
///
/// `set_password_core` **清除密码用的就是「键缺失」**（`obj.remove(HASH_KEY)`）。若在汇流点无条件回填，
/// 清除密码会永久失效（每次都把旧 hash 填回来）。故只作用于「前端全量提交」的两个入口
/// （`config_save` / `backup_import_apply`）；后端自己读 `current()` 改键的路径（server/rules/
/// subscription/set_value…）本就带着 hash，不受影响。
///
/// 语义：**入参显式带该键 → 尊重入参**（专线写入 / 清除）；入参缺该键 → 从当前配置回填。
pub(crate) fn preserve_server_owned_secrets(config: &ConfigManager, incoming: &mut Value) {
    // 读不到当前配置（首启无文件等）→ 无可回填，原样保存（不猜、不阻断保存）。
    let Ok(current) = config.current() else {
        return;
    };
    let Some(obj) = incoming.as_object_mut() else {
        return;
    };
    for key in [PRIVACY_PASSWORD_KEY, PRIVACY_PASSWORD_HASH_KEY] {
        if obj.contains_key(key) {
            continue;
        }
        if let Some(v) = current.get(key) {
            obj.insert(key.to_string(), v.clone());
        }
    }
}

// ── 后端权威字段（前端零写入权）在全量保存边界的强制回正 ────────────────────────────
//
// # 与 `preserve_server_owned_secrets` 是两条不同策略，不能合并
//
// 隐私密钥在 `config_get`（全量快照的唯一出口）被 `strip_privacy_secrets` 剥除 ⇒ 前端快照里
// **根本没有该键**，故「键缺失即回填、键在即尊重入参」够用（且必须尊重入参——清密码用的就是键缺失）。
//
// 本组字段**照常下发前端**（`TrayMenu` 要读 `recentServerIds` 渲染「节点·最近」）⇒ 前端快照里
// **键在、值陈旧**，回填策略永不触发，必须无条件以磁盘为准。
//
// # 坑长什么样（用户 2026-07-21 真机报「托盘最近节点只剩 1 条」）
//
// 后端 `server_switch` 写 `recentServerIds`（`commands/server.rs` 的 `push_recent_server_id`：
// unshift + 去重 + `truncate(3)`）后经 `broadcast_config_changed` 广播；前端保鲜总线
// （`ui/src/App.tsx` 的 `api.config.onChanged(() => void loadConfig(true))`）本应把新值拉回，但
// `ui/src/store/app-store.ts` 的乐观写腿（`switchServer` / `saveConfig`）在 mutation 后调
// `invalidateLoadConfig()`，而其代际守卫（`if (myGeneration !== loadConfigGeneration) return`）
// **无法区分**「mutation 之前发起的陈旧 load（该丢）」与「mutation 自己的新鲜回声（该留）」，一律丢弃
// ⇒ store 留陈旧值 ⇒ 6 个全量保存入口（HomeScreen / LogsScreen / RulesScreen / TrayMenu /
// settings·useConfig）任意一个把后端刚写的历史整份抹回。
//
// # 为什么修在这个边界，而不是修前端时序
//
// 前端修是**时序修**（只缩小竞争窗口），仍依赖广播不丢、监听已挂载、回声与乐观写的先后；
// 本边界修把窗口从「**整个前端快照的生命周期**」（load → 用户操作 → 提交，秒级至分钟级，且期间
// 任意一次后端写都会被抹回）收窄到「本命令内 `current()` 读 → `save_full` 写」的**微秒级**，
// 6 个写入口零改动，且射程被死死限制在「前端本就无权写」的键上——白名单外一切**保持整份覆盖**语义
// （与 上游 `ConfigManager.saveConfig` 逐字一致：`JSON.stringify(入参)` 直接落盘，无 merge）。
//
// # 残余竞态：如实记账，**不宣称已消除**
//
// 本函数**不是**无锁安全的：`enforce_backend_authoritative_fields` 读 `config.current()`、随后
// `config_save_core` 再 `save_full`，两步之间不持锁。并发的 `server_switch`（`commands/server.rs`，
// 对同一缓存同样是 read-modify-write）若正落在这一读一写之间，其写入的 MRU 仍会丢。
//
// 为什么**不**在此加 mutex：加在本函数上是**假安全**。全仓的配置写路径清一色是
// `load_full()/current()` → 改 → `save_full()` 的读改写对（`server_add_core` / `server_add_bulk_core` /
// `server_update` / `backup_import_apply` / `misc.rs` 的两处 / 本函数…）。只锁本函数，`server_switch`
// 的那一对照样在锁外自由交错 —— 窗口一寸未减，却多出一份「这里有锁 ⇒ 这里安全」的错误暗示。
// 真正关掉它需要一把覆盖**每一对**读改写的全仓配置写锁（`ConfigManager` 层面，而非命令层面），
// 那是独立的跨切面改动，射程远超本项，且需连同 `ConfigManager` 内部 `RwLock` 的层次一并设计以免死锁。
// 在此之前，本段按实际情况描述：**窗口大幅收窄，但未消除**。
//
// # 为什么不做深合并（本仓刻意不引入 merge）
//
// 深合并会同时废掉「清空数组」（传 `[]`）与「删键」两种删除表达，且射程覆盖**全部**字段——用户删掉
// 最后一条规则会发现删不掉。上游 全仓 save 路径零 merge 正是这个原因；它对唯一需要保护的字段
// （隐私密码）的解法是**把字段搬出 config 对象**（独立 `privacy-lock.json`），而非在 save 路径加保护。
// 字段级所有权划分把「以磁盘为准」的射程压到前端根本不写的键上，删除困境自然不存在。

/// 「后端权威」配置字段：**前端零写入权**（UI 只读或全仓零引用），真值只由后端写路径产生。
///
/// # 判准是「前端零写入权」，不是「后端写过」
///
/// `clashApiSecret` 后端也写（[`backfill_secret_and_privacy`] 回填），但前端**有**写入权——
/// 设置·网络页有「重新生成」按钮（`ui/src/components/screens/settings/SettingsNetwork.tsx` 的
/// `update({ clashApiSecret: generateSecret() })`）⇒ **不得**收录，否则该按钮会被静默废掉
/// （点了没反应，比现在的 bug 更隐蔽）。收录前必须逐字段实证「ui/ 全仓零写入」，宁缺勿滥。
///
/// `appRulesSeeded` 同样**不收**：它在 `polaris_store::backup` 的 `DATA_FIELDS` 里，随 appRules 类
/// 被备份导入合法写入 ⇒ 所有权有争议，不满足「零写入权」。
const BACKEND_AUTHORITATIVE_KEYS: [&str; 2] = [
    // 托盘「节点·最近」MRU。只由 `server_switch` 写；ui 全仓仅 TrayMenu 读。
    "recentServerIds",
    // 内置 geo 元数据（随包）。只由 geo seed 写；ui 全仓零读零写。
    "builtinGeoMeta",
    // 曾有第三项 `diagnosticCapture`（诊断采集态）。整条机制已删除（核日志改由 `SubscribeLog` 全级别
    // 送达、级别筛在客户端，不再需要「临时把核提级到 debug」的会话），故该键不再是任何人的权威字段。
    // 旧配置里的残留由 `polaris_store::migrate::migrate_diagnostic_capture` 还原级别后清除。
];

/// 以磁盘当前值**强制回正**入参里的后端权威字段（[`BACKEND_AUTHORITATIVE_KEYS`]）。
///
/// 语义是**镜像磁盘**，两条腿缺一不可：
/// - 磁盘**有**该键 → 覆盖入参（挡掉前端陈旧值）
/// - 磁盘**无**该键 → 从入参**删除**（否则前端携带的陈旧值会把后端刚删掉的键复活）
///
/// 第二条腿不是可选的：只做「有则覆盖」就只实现了「镜像磁盘」的一半 —— 后端一旦**删掉**某个权威键，
/// 任一全量保存都会用前端携带的陈旧值把它复活，而字段所有者对此毫无察觉。
/// （这条腿此前的血证是 `diagnosticCapture` 的「结束采集 = 删该键」；那套机制已随本批删除，
/// 语义本身不变 —— 删除权归字段所有者，缺了这条腿就等于所有者删不掉自己的键。）
///
/// 这也是本组字段的**删除表达**：删除权归字段所有者（后端），前端既无写入权也无需表达删除。
/// 白名单外的键一律不受影响，删除仍靠整份覆盖天然表达（传 `[]` 清空数组 / 缺键删除），与 上游 同构。
pub(crate) fn enforce_backend_authoritative_fields(config: &ConfigManager, incoming: &mut Value) {
    // 读不到当前配置（首启无文件等）→ 无权威值可依，原样保存（不猜、不阻断保存；
    // 与 `preserve_server_owned_secrets` 同款保守取向）。
    let Ok(current) = config.current() else {
        return;
    };
    let Some(obj) = incoming.as_object_mut() else {
        return;
    };
    for key in BACKEND_AUTHORITATIVE_KEYS {
        match current.get(key) {
            Some(v) => {
                obj.insert(key.to_string(), v.clone());
            }
            None => {
                obj.remove(key);
            }
        }
    }
}

// ── 启动期配置维护（上游 loadConfig 内联步骤的 Polaris 收口点）────────────────────
//
// 上游 在 `loadConfig` 里做了三件启动维护：sweepStaleTmpFiles / 回填 clashApiSecret / F29 旧明文密码
// 迁移为哈希。Polaris 的 `store::ConfigStore::load` 是纯逻辑（load 成功路径绝不写盘，仅返回 migration_delta
// 供调用方决策），故这三件需 FS 写 + crypto 的维护收口在**前端首个配置入口** `config_get`，一次性执行。

/// 进程内一次性守卫：启动维护只跑一次（对齐 上游 `tmpSwept` / 首次 loadConfig 语义）。
static STARTUP_MAINTENANCE_DONE: AtomicBool = AtomicBool::new(false);

/// 启动期一次性维护：清孤儿 tmp + 回填 clashApiSecret + F29 明文密码无损迁移。全 best-effort，绝不阻断。
fn run_startup_maintenance_once(config: &ConfigManager) {
    if STARTUP_MAINTENANCE_DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    // ① 清扫原子写遗留的孤儿 tmp（进程 write(tmp) 成功后 rename 前被硬杀/断电留下；随机名不会被下次写覆盖自愈）。
    sweep_stale_tmp_files(config.path());
    // ② 回填 clashApiSecret（本地管理 API/dashboard 出厂鉴权）+ F29 旧明文密码无损迁移为 salted hash。
    if let Err(e) = backfill_secret_and_privacy(config) {
        log::warn!("启动配置维护（clashApiSecret / 隐私哈希回填）失败（不阻断）: {e}");
    }
}

/// 清扫孤儿 tmp（`<config>.<12hex>.tmp` 且 mtime>60s）。上游 `sweepStaleTmpFiles`。
///
/// 决策纯逻辑收在 `store::fs::should_sweep_stale_tmp`（名匹配 + 龄期>60s，变异可验）；本函数只做 FS 遍历/删除。
/// mtime 守卫防误删并发 saveConfig 的在途 tmp。best-effort：任何 FS 失败忽略。
fn sweep_stale_tmp_files(config_path: &Path) {
    let (Some(dir), Some(base_name)) = (
        config_path.parent(),
        config_path.file_name().and_then(|n| n.to_str()),
    ) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let age_secs = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .map_or(0, |d| d.as_secs());
        if polaris_store::fs::should_sweep_stale_tmp(base_name, name, age_secs) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 生成本地管理 API 的 secret（CSPRNG 16 字节 → 32 位小写 hex）。上游 `randomBytes(16).toString('hex')`。
/// 复用 `gen_salt` 同源的 ring CSPRNG（rustls 既有依赖），OS 熵源失败 → Err（绝不产弱/空密钥）。
///
/// 两个消费者、同一形状：持久化的 `clashApiSecret`（本文件 [`backfill_secret_and_privacy`]）与
/// Tailscale 瞬态登录核那条一次性管理 API 的 secret（`runtime::tailscale_login_core`）。
pub(crate) fn generate_local_api_secret() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut bytes)
        .map_err(|_| "系统随机源不可用，无法生成本地管理 API secret".to_string())?;
    Ok(hex_encode(&bytes))
}

/// 回填 clashApiSecret（缺失/空 → 随机生成）+ F29 旧明文密码无损迁移（明文 → salted hash）。
///
/// # clashApiSecret（HIGH 安全）
/// 本地管理 API（含默认开的 sing-box dashboard）出厂无鉴权（`proxy.rs` 读侧：空 secret = 免认证）。
/// 新装/存量随机回填 + **持久化**（供 external_ui/外部客户端跨会话复用，故必须落盘稳定，不能每次 load 重生成）。
///
/// # F29 无损迁移（隐私锁 → scrypt 独立文件）
/// `store::migrate::migrate_privacy_password_clear` 每次 load 把旧明文 `privacyPassword` 抹成 ""（防外泄），
/// 但**丢了密码** → 隐私锁静默失效。此处在明文被抹前直读**盘上**明文（in-memory load 已清空，盘上 load 不落盘
/// 仍留），算 **scrypt** 哈希存进独立 `privacy-lock.json`（0600），并触发 config save_full 抹掉盘上残留明文。
/// 仅当盘上有明文 **且** 既无 scrypt 文件 **又** 无 legacy SHA-256 键时执行（不覆盖用户已设的新密码）。
///
/// clashApiSecret 与「明文迁移触发的明文抹除」合并为**一次** save_full 落盘。幂等：secret 已在 /
/// 无旧明文 / 已有 scrypt 文件或 legacy 键 → 不写。
fn backfill_secret_and_privacy(config: &ConfigManager) -> Result<(), String> {
    let path = config.path();
    // 盘上旧明文密码：in-memory load 经 migrate 抹成 ""，故直读盘取明文（load 成功不落盘 → 盘上此刻仍留明文）。
    let disk_plain: Option<String> = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get(PRIVACY_PASSWORD_KEY)
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|s| !s.is_empty());

    // 直接取 LoadResult 判「是否真从盘加载成功」：损坏回落（error 且非新装）**绝不 save_full**——
    // 否则会用默认配置覆盖损坏原文件 = 破坏 `store::ConfigStore::load` 的「不覆盖损坏磁盘」保护（数据丢失）。
    let loaded = polaris_store::ConfigStore::load(&polaris_store::StdFs, path);
    if loaded.error.is_some() && !loaded.was_missing {
        return Ok(()); // 损坏配置：只备份（load 已做），绝不回填覆盖
    }
    let mut cfg = loaded.config;
    // 已有隐私密码 = scrypt 文件存在 **或** legacy SHA-256 键存在（任一都不得被旧明文覆盖）。
    let has_scrypt_file = polaris_store::privacy_lock::has(&StdFs, &privacy_lock_path(config));
    let has_legacy = config_has_password(&cfg);
    let Some(obj) = cfg.as_object_mut() else {
        return Ok(());
    };
    let mut changed = false;

    // clashApiSecret 回填（缺失/空 → 随机生成）。
    let has_secret = obj
        .get("clashApiSecret")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if !has_secret {
        obj.insert(
            "clashApiSecret".to_string(),
            json!(generate_local_api_secret()?),
        );
        changed = true;
    }

    // F29 无损迁移：盘上有旧明文 && 既无 scrypt 文件又无 legacy 键 → 用明文算 **scrypt** 存独立文件。
    // changed=true 触发下方 save_full → 用 migrate 已抹空明文的 cfg 覆盖盘上 config.json（scrub 残留明文）。
    if let Some(plain) = disk_plain {
        if !has_scrypt_file && !has_legacy {
            let salt = gen_salt()?;
            let hash = polaris_store::privacy_lock::hash_password(&plain, &salt)
                .map_err(|e| format!("{e}"))?;
            polaris_store::privacy_lock::write(&StdFs, &privacy_lock_path(config), &hash)
                .map_err(|e| format!("{e}"))?;
            changed = true;
        }
    }

    if changed {
        // save_full 内部再跑 sanitize+validate + 原子写 + 刷缓存（含撞口避让等 validate 归一）。
        config.save_full(&cfg).map_err(|e| format!("{e}"))?;
    }
    Ok(())
}

/// 生成 16 字节盐（ring CSPRNG，经 rustls 既有依赖的 `crypto::ring` provider 暴露——与
/// `runtime::mesh::generate_warp_seed` 同源，无新依赖）。OS 熵源失败 → Err（绝不产弱/零盐）。
fn gen_salt() -> Result<[u8; 16], String> {
    let mut salt = [0u8; 16];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut salt)
        .map_err(|_| "系统随机源不可用，无法生成密码盐".to_string())?;
    Ok(salt)
}

/// 字节 → 小写 hex。
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        s.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
    }
    s
}

/// hex → 字节。非偶长度/非法字符 → None。
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// **legacy** salted SHA-256：`sha256(salt || password)` → hex。复用 `polaris_helper::core_install::sha256_hex`。
/// 新密码已改用 scrypt 独立文件（见 `store::privacy_lock`）；本函数仅供**存量 SHA-256 用户的解锁校验**
/// （经 [`verify_password`] → [`unlock_core`] legacy 分支）复算比对，production 不再用它**创建**新哈希。
fn hash_password(salt: &[u8], password: &str) -> String {
    let mut data = salt.to_vec();
    data.extend_from_slice(password.as_bytes());
    polaris_helper::core_install::sha256_hex(&data)
}

/// 常量时间比较（等长逐字节 XOR 累加，无早退时序泄漏）。长度不等直接 false（hash 恒等长，不泄信息）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// **legacy** 校验：明文是否匹配存储的 `salt_hex$hash_hex`（salted SHA-256）。格式非法 → 不匹配（fail-closed）。
/// 仅存量未迁移用户走此路径；验过后 [`unlock_core`] 会把其升级为 scrypt 文件。
fn verify_password(stored: &str, password: &str) -> bool {
    let Some((salt_hex, hash_hex)) = stored.split_once('$') else {
        return false;
    };
    let Some(salt) = hex_decode(salt_hex) else {
        return false;
    };
    let expected = hash_password(&salt, password);
    constant_time_eq(expected.as_bytes(), hash_hex.as_bytes())
}

/// 清除 config.json 里的 legacy salted-SHA256 `privacyPasswordHash` 键（scrypt 文件已成新真值源）。
///
/// 快路径：当前配置无该键（绝大多数新用户 / 已迁移用户）→ 空操作，不触盘。有该键 → load_full 拿全量 →
/// 移除 → save_full 落盘。留着旧键 = 双真值源 + 多一处泄漏面，故迁移完成即抹。
fn clear_legacy_hash_key(config: &ConfigManager) -> Result<(), polaris_store::StoreError> {
    let cur = config.current()?;
    if cur.get(PRIVACY_PASSWORD_HASH_KEY).is_none() {
        return Ok(());
    }
    let mut cfg = config.load_full()?;
    if let Some(obj) = cfg.as_object_mut() {
        obj.remove(PRIVACY_PASSWORD_HASH_KEY);
    }
    config.save_full(&cfg)
}

/// config 是否已设隐私密码（`privacyPasswordHash` 存有非空 salted hash）。纯函数，便于单测。
fn config_has_password(cfg: &Value) -> bool {
    cfg.get(PRIVACY_PASSWORD_HASH_KEY)
        .and_then(Value::as_str)
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// `set_password_core` 失败原因：区分「锁屏门控拒绝」（`privacy_set_password` 需转 `err_with_code`
/// 供前端按 code 识别）与其它失败（config 读写 / CSPRNG 出错等，原始 message 透传）。
#[derive(Debug)]
enum SetPasswordError {
    /// 隐私模式（锁屏）中：契约 L141「锁屏禁改/清密码」，无条件拒绝——不读写存储、不生成新盐。
    Locked,
    /// 非锁屏态下的其它失败。
    Other(String),
}

/// `privacy:setPassword` 核心（注入 `ConfigManager` + 显式 `locked` 态，便于真实 ConfigStore 驱动测试）。
///
/// 非空 → 新盐算 **scrypt** 存独立 `privacy-lock.json`（0600）+ 抹掉 config.json 里的 legacy SHA-256 键；
/// 空串 → 删 scrypt 文件 + 抹 legacy 键（任何人可解锁）。**绝不存明文**。每次 set 都新生成盐（salt 唯一）。
///
/// # 为什么 `locked` 是显式参数而非直接读 `PRIVACY_MODE`
///
/// 同文件其余 `_core` 函数一律不碰进程内 static（纯函数、状态由调用方传入），本函数照此惯例；
/// 且 `cargo test` 默认多线程并行跑，若在此直接读写共享的 `PRIVACY_MODE` static，跑锁屏门控用例时
/// 会与同时跑的其它 `set_password_core` 正常流程用例互相脏读、产生 flaky（该 static 是进程唯一实例，
/// 无法每测试隔离一份）。真实调用方（`privacy_set_password`）在其**唯一**调用处显式读一次 `PRIVACY_MODE`
/// 传入，语义等价，且单测可完全绕开全局态直接摆 `locked: true/false`。
///
/// # 门控做什么
///
/// `locked=true` → 无条件拒绝改 / 清密码（契约 L141），且在**碰存储之前**就返回——这正是此前的洞：
/// 锁屏状态下传空串会走到清密码路径 = 未验证密码即解锁。对「改」与「清」一视同仁
/// （`password` 是否为空不影响门控判定）。
fn set_password_core(
    config: &ConfigManager,
    password: &str,
    locked: bool,
) -> Result<(), SetPasswordError> {
    if locked {
        return Err(SetPasswordError::Locked);
    }
    let path = privacy_lock_path(config);
    if password.is_empty() {
        // 清除：删 scrypt 文件（不存在视为成功）。
        polaris_store::privacy_lock::remove(&StdFs, &path)
            .map_err(|e| SetPasswordError::Other(format!("{e}")))?;
    } else {
        // 新盐 → scrypt 哈希 → 写独立文件（0600）。文件写成功后才抹 legacy 键，避免中途失败致锁死。
        let salt = gen_salt().map_err(SetPasswordError::Other)?;
        let hash = polaris_store::privacy_lock::hash_password(password, &salt)
            .map_err(|e| SetPasswordError::Other(format!("{e}")))?;
        polaris_store::privacy_lock::write(&StdFs, &path, &hash)
            .map_err(|e| SetPasswordError::Other(format!("{e}")))?;
    }
    // 抹掉 config.json 里的 legacy SHA-256 键（若存量用户此前设过）——scrypt 文件已成唯一真值源。
    clear_legacy_hash_key(config).map_err(|e| SetPasswordError::Other(format!("{e}")))
}

/// `privacy:unlock` 核心：已设密码 → 仅匹配返 true；未设 → 自由解锁（true）。
///
/// 读侧优先级：**scrypt 独立文件**（新真值源）> config.json legacy SHA-256（存量未迁移）> 未设密码。
/// legacy 分支验过后**透明升级**到 scrypt 文件（拿得到明文的唯一时机）；升级 best-effort，失败不阻断解锁
/// （下次再升）。旧密码**验败绝不删旧键 / 不建文件**（防把老用户锁在外）。
fn unlock_core(config: &ConfigManager, password: &str) -> Result<bool, String> {
    let path = privacy_lock_path(config);
    // ① scrypt 文件存在 → 唯一判据（忽略残留 legacy 键）。
    if let Some(h) = polaris_store::privacy_lock::read(&StdFs, &path) {
        return Ok(polaris_store::privacy_lock::verify(password, &h));
    }
    // ② 无 scrypt 文件 → 回退 legacy SHA-256。
    let cfg = config.current().map_err(|e| format!("{e}"))?;
    let stored = cfg
        .get(PRIVACY_PASSWORD_HASH_KEY)
        .and_then(Value::as_str)
        .unwrap_or("");
    if stored.is_empty() {
        return Ok(true); // 未设密码 → 自由解锁。
    }
    if !verify_password(stored, password) {
        return Ok(false); // 旧格式密码错——不升级、不删旧键（防锁死）。
    }
    // 旧格式验过：升级到 scrypt 文件（写文件在前、抹旧键在后）。best-effort，失败仅记日志、仍放行解锁。
    if let Err(e) = upgrade_legacy_to_scrypt(config, &path, password) {
        log::warn!("隐私锁 legacy SHA-256 → scrypt 升级失败（不阻断解锁，下次再升）: {e}");
    }
    Ok(true)
}

/// 把验过的 legacy SHA-256 密码升级为 scrypt 独立文件：写文件 → 抹 legacy 键。
/// **顺序关键**：先写 scrypt 文件、后抹旧键，任一步失败都不会出现「两者皆无」的锁死窗口。
fn upgrade_legacy_to_scrypt(
    config: &ConfigManager,
    path: &Path,
    password: &str,
) -> Result<(), String> {
    let salt = gen_salt()?;
    let hash =
        polaris_store::privacy_lock::hash_password(password, &salt).map_err(|e| format!("{e}"))?;
    polaris_store::privacy_lock::write(&StdFs, path, &hash).map_err(|e| format!("{e}"))?;
    clear_legacy_hash_key(config).map_err(|e| format!("{e}"))
}

/// `privacy:hasPassword` 核心：scrypt 文件存在 **或** config.json 里有非空 legacy SHA-256 键。
fn has_password_core(config: &ConfigManager) -> Result<bool, String> {
    if polaris_store::privacy_lock::has(&StdFs, &privacy_lock_path(config)) {
        return Ok(true);
    }
    let cfg = config.current().map_err(|e| format!("{e}"))?;
    Ok(config_has_password(&cfg))
}

/// 上游 `CONFIG_GET_PRIVACY_MODE`：隐私模式开关状态（进程内状态机实值）。
#[tauri::command]
pub fn config_get_privacy_mode(_state: State<'_, AppRuntime>) -> ApiResponse<bool> {
    ApiResponse::ok(PRIVACY_MODE.load(Ordering::Relaxed))
}

/// 上游 `CONFIG_SET_PRIVACY_MODE`：切换隐私模式（进/出）+ 状态变更时 emit enter/exit 事件。
///
/// 密码闸在 unlock 侧（前端退出前先 `privacy_unlock` 验证）；本 command 落状态转移 + 广播事件，
/// 供 UI（Logs/Connections 脱敏）与 log builder（隐私模式抬日志级别）联动。
#[tauri::command]
pub fn config_set_privacy_mode(
    app: AppHandle,
    _state: State<'_, AppRuntime>,
    value: bool,
) -> ApiResponse<()> {
    let prev = PRIVACY_MODE.swap(value, Ordering::Relaxed);
    if prev != value {
        let evt = if value {
            EVENT_ENTER_PRIVACY_MODE
        } else {
            EVENT_EXIT_PRIVACY_MODE
        };
        let _ = app.emit(evt, ());
    }
    ok_void()
}

/// 上游 `PRIVACY_HAS_PASSWORD`：是否设置了隐私密码（scrypt 文件存在或存量 legacy 键非空）。
#[tauri::command]
pub fn privacy_has_password(state: State<'_, AppRuntime>) -> ApiResponse<bool> {
    match has_password_core(state.config()) {
        Ok(has) => ApiResponse::ok(has),
        Err(e) => ApiResponse::err(e),
    }
}

/// 上游 `PRIVACY_SET_PASSWORD`：设置 / 改 / 清隐私密码。
///
/// 非空 → 新盐算 **scrypt** 存独立 `privacy-lock.json`（0600）+ 抹 legacy 键；空串 → 删文件 + 抹 legacy 键。
/// **绝不存明文**。不广播 `config:changed`：密码变更不影响代理配置生成 → 无需热切换（且 scrypt 哈希本就
/// 不入 config → 不经 configChanged 出口）。返回 `{success:true}`（前端契约）。
///
/// 锁屏门控（契约 L141）：`PRIVACY_MODE` 为 true（隐私模式/锁屏中）时无条件拒绝——改密码、清密码皆算，
/// 返 `err_with_code(_, "PRIVACY_LOCKED")` 供前端区分。当前隐私遮罩 UI 尚未接线，此路径暂无 UI 触发
/// （latent），但契约明确点名「不得简化」，故后端闸先落好，UI 落地时直接受益。
#[tauri::command]
pub fn privacy_set_password(state: State<'_, AppRuntime>, password: String) -> ApiResponse<Value> {
    let locked = PRIVACY_MODE.load(Ordering::Relaxed);
    match set_password_core(state.config(), &password, locked) {
        Ok(()) => ApiResponse::ok(json!({ "success": true })),
        Err(SetPasswordError::Locked) => ApiResponse::err_with_code(
            "锁屏状态下禁止修改或清除隐私密码，请先解锁",
            "PRIVACY_LOCKED",
        ),
        Err(SetPasswordError::Other(e)) => ApiResponse::err(e),
    }
}

/// 解锁失败弱限速时长（契约 L141「解锁失败 sleep(300) 弱限速」）：抑制单进程高速暴力猜密码
/// （无限速时单进程每秒可猜上万次）。契约给定值，不额外加码。
const UNLOCK_FAIL_DELAY_MS: u64 = 300;

/// 解锁限速：`ok=false`（密码错）才延时 [`UNLOCK_FAIL_DELAY_MS`]；`ok=true`（密码对 / 未设密码自由解锁）
/// 不延时，不拖累正常解锁手感。
///
/// 抽成独立 async helper（不接触 `ConfigManager`/`State`）只为让 300ms 限速本身可单测（`State<'_, AppRuntime>`
/// 无法在 `#[tokio::test]` 里构造，同文件其余 `_core` 拆分也是同一动机）。
async fn apply_unlock_rate_limit(ok: bool) {
    if !ok {
        tokio::time::sleep(std::time::Duration::from_millis(UNLOCK_FAIL_DELAY_MS)).await;
    }
}

/// 上游 `PRIVACY_UNLOCK`：解锁（验证密码，常量时间比较）。返回 `{ok:bool}`（前端契约）。
///
/// 已设密码 → 仅哈希匹配返 `true`（scrypt 文件优先，legacy SHA-256 回退+透明升级）；未设密码 → 自由解锁
/// （`true`，对齐「留空则任何人可解锁」）。
///
/// 契约 L141：解锁失败经 [`apply_unlock_rate_limit`] 弱限速 300ms。`async fn` + `tokio::time::sleep`——
/// **绝不 `std::thread::sleep`**：本 command 跑在 tauri 的 tokio executor 上，`std::thread::sleep`
/// 会硬阻塞该 worker 线程、冻结同线程上其余并发 IPC；`tokio::time::sleep` 只让出当前 task，executor
/// 照常调度其余任务（对齐仓内既有用法，如 `runtime/stats.rs`）。`unlock_core` 本身是纯同步计算（无 IO），
/// 限速前先同步跑完拿到 `ok`，`state` 借用不跨随后的 `.await`（本仓 async command 惯例，
/// 见 `proxy.rs::system_proxy_disable`）。
#[tauri::command]
pub async fn privacy_unlock(
    state: State<'_, AppRuntime>,
    password: String,
) -> Result<ApiResponse<Value>, ()> {
    // tauri 硬性要求：async command 若带引用型入参（`State<'_, _>`），返回值必须是 `Result`
    // （否则宏展开报 `AsyncCommandMustReturnResult` / `'static` 借用期不够）——同 `system_proxy_disable`。
    let result = unlock_core(state.config(), &password);
    Ok(match result {
        Ok(ok) => {
            apply_unlock_rate_limit(ok).await;
            ApiResponse::ok(json!({ "ok": ok }))
        }
        Err(e) => ApiResponse::err(e),
    })
}

/// 广播 event:configChanged（上游 `ipcEventEmitter.sendToAll('event:configChanged', { newValue })`）
/// **并把变更送进运行核**（上游 `config-change-handler.ts:77` 的 `proxyManager.switchMode(latest)`）。
///
/// # 为什么接线在这里
///
/// 这是本仓所有配置写命令（`config:save` / `config:setValue` / `server:switch` / `rules:*` /
/// `subscription:*` 共 10+ 处）的**唯一汇流点** —— 与 Polaris 把 switchMode 挂在 CONFIG_CHANGED
/// 单一监听器上同构。接在此处 = 每条配置变更路径自动获得热切换判定，无需逐个命令改造，
/// 也不会漏掉将来新增的写命令（§K7.1：门要开在唯一的生产路径上）。
///
/// 此前本函数只 emit 给 UI，**运行核对配置变更一无所知** —— 切节点只改磁盘、核继续跑旧节点，
/// 唯一入核手段是用户手点「应用」触发的全量重启。
///
/// `switch_mode` 是 async 且含 gRPC I/O（最长 ~2s deadline），而本函数被同步 command 调用 →
/// `spawn` 到 tokio 后台，不阻塞 IPC 返回（对齐 Polaris 的 `void switchMode(...)` 即发即忘）。
pub(crate) fn broadcast_config_changed(app: &AppHandle, new_value: &Value) {
    broadcast_config_changed_with(app, new_value, false);
}

/// [`broadcast_config_changed`] 带「保存不重启」标志的形态（暂存层「保存」腿，spec §2.5 Q4）。
///
/// 只有 `config:save` 会传 `true`（且仅当前端显式传了 `deferRestart`）。其余十余个配置写命令
/// （`server:switch` / `rules:*` / `subscription:*` / `config:setValue` …）一律走无参形态 =
/// 今天行为逐字节不变 —— 那些是「用户点了某个具体动作」，不是「用户点了保存」，不该被降级。
pub(crate) fn broadcast_config_changed_with(
    app: &AppHandle,
    new_value: &Value,
    defer_restart: bool,
) {
    // F29 defense-in-depth：隐私密码（legacy 明文 + salted hash）绝不经**任何**前端可见路径下发。
    // 本事件已不带载荷（见下），故这份剥离服务的是**入核**那一份 —— `cfg` 一路 move 进
    // `switch_mode_with`；剥在源头，将来谁把它接回某条前端可见路径也带不出 hash。
    // （隐私密码不参与代理配置生成，剥除对热切换无影响。）
    let mut cfg = new_value.clone();
    strip_privacy_secrets(&mut cfg);
    // **无载荷信号**。四个消费方一个都不读 payload，收到即各自重拉：`App.tsx` → `loadConfig(true)`、
    // `TrayMenu.tsx` → `hydrate()`、`settings/useConfig.ts` → `load(true)`（该处还专门注明「payload 的
    // newValue 不能直接用」——它经脱敏、且没走 `config_get` 那侧的 bypassLANList 补齐，与其契约不同源）、
    // `main.rs` 的 `listen_any` → `reconcile_tray`（回调签名 `|_|` 直接丢弃）。
    //
    // 而 `cfg` 在这行之后仍要用（logLevel / uiTheme / move 进 `switch_mode_with`）⇒ 载荷里写 `cfg`
    // 只能借用 ⇒ `json!` 展开成 `to_value(&cfg)`，在上面那次 clone 之外**再深拷贝一整棵配置树**，
    // 外加整份 JSON 序列化、按 webview 拼注入脚本、`NSString` 构造与 Rust 侧监听各自一份 —— 全白做。
    let _ = app.emit(EVENT_CONFIG_CHANGED, json!({}));
    // 应用侧日志级别跟随 config.logLevel —— 同 switch_mode 的道理接在**唯一**的配置变更路径上：
    // 此前 `log::set_max_level` 只在 sink 装配时设一次，日志页选 DEBUG 对应用侧毫无效果（核侧另算，
    // 级别在生成配置时注入，须经下方 switch_mode 重启才生效，UI 已如实标注）。
    if let Some(level) = cfg.get("logLevel").and_then(Value::as_str) {
        crate::logging::set_level(level);
    }
    // 原生窗口外观跟随 `uiTheme` —— 与建窗处（`main.rs::create_main_window`）同一判据，接在这条
    // **唯一**的配置变更路径上，理由同上面的 logLevel：只在建窗时设一次的话，用户在设置里改主题后
    // vibrancy/Mica 的明暗会一直停在启动那一刻的值（症状：浅色主题配深色 vibrancy，侧栏发黑）。
    // `system` 传 None = 交回系统跟随，与建窗处逐分支同构。
    {
        let native_theme = match cfg.get("uiTheme").and_then(Value::as_str).map(str::trim) {
            Some("dark") => Some(tauri::Theme::Dark),
            Some("light") => Some(tauri::Theme::Light),
            _ => None,
        };
        for label in ["main", crate::tray::TRAY_LABEL] {
            if let Some(win) = app.get_webview_window(label) {
                let _ = win.set_theme(native_theme);
            }
        }
    }
    // try_state：测试/早期启动期可能尚未 manage(AppRuntime) → 取不到就只广播不入核，不 panic。
    if let Some(state) = app.try_state::<AppRuntime>() {
        let proxy = state.proxy.clone();
        tauri::async_runtime::spawn(async move {
            proxy.switch_mode_with(cfg, defer_restart).await;
        });
    }
}

/// P0-1 **无载荷守卫**：`event:configChanged` 是纯信号 —— 发射点不带配置内容，四个消费方一个都不读。
///
/// # 为什么只能是结构守卫
///
/// 发射点要 `AppHandle`（本仓未引 `tauri::test`），四个消费方里三个在渲染端 —— 没有任何一条行为
/// 断言能同时站在两侧。而这条不变式破掉时的症状是**纯性能回退**：`cfg` 在 emit 之后仍被使用
/// （logLevel / uiTheme / move 进 `switch_mode_with`）⇒ 载荷里写 `cfg` 只能借用 ⇒ `json!` 展开成
/// `to_value(&cfg)`，在既有 clone 之外再深拷一整棵配置树，外加整份 JSON 序列化、按 webview 拼注入
/// 脚本、`NSString` 构造各一份。行为面**完全看不出来**，只能锁结构。
///
/// # 射程为什么是五个点（发射点 + 四个消费方），缺一不可
///
/// 少了发射点 = 载荷可以悄悄加回来；少了任一消费方 = 有人开始读 `{}` 里不存在的字段，拿到
/// `undefined` 后走出一条静默错路。`newValue` 恰恰是「看着能用、其实不能用」的那类字段：它经
/// `strip_privacy_secrets` 脱敏、也没走 `config_get` 那侧的 bypassLANList 补齐（见 `useConfig.ts`）。
#[cfg(test)]
mod config_changed_payload_tests {
    use crate::commands::guard_scan::{
        strip_block_comments, strip_line_comments, top_level_fn_body,
    };

    /// 三个渲染端消费点（仓内相对路径 → 源码）。
    ///
    /// 用 `include_str!` 而不是运行期读盘：文件被挪走 = **编译失败**，而不是守卫静默扫了个空串
    /// 然后断言恒真。仓内已有同款先例（本文件的 `config-version.fixture.json`）。
    ///
    /// # 跨语言耦合是刻意的
    ///
    /// 这三份前端源码被直接嵌进 Rust 测试判据：`App.tsx` / `TrayMenu.tsx` / `useConfig.ts` 任一个
    /// 多挂或删掉一个 `.onChanged(` 都会让 `cargo test -p polaris` 转红（见下面
    /// `every_consumer_discards_the_payload` 的数量断言）。只改前端的人未必会想到去跑 Rust 测试——
    /// 灯下记账：
    ///
    /// CI 覆盖面（`.github/workflows/ci.yml` 实测）：`pull_request` 触发**无路径过滤**，纯改这三个
    /// 文件的 PR 仍会跑 `cargo test --workspace`，本测试正常拦截。只有**绕过 PR 直接 push 到
    /// main**、且改动只命中 `on.push.paths-ignore` 里的 `ui/**`/`**.md`/`docs/**` 时，整条 Rust 链
    /// （含本测试）才会被跳过——那是 push 主干的调试期额度优化，不针对本测试。结论：这道门在
    /// 「PR 流程」下始终执行；只在「绕过 PR 的直接 push」这一条路径上失效。
    const TS_CONSUMERS: [(&str, &str); 3] = [
        ("ui/src/App.tsx", include_str!("../../../ui/src/App.tsx")),
        (
            "ui/src/tray/TrayMenu.tsx",
            include_str!("../../../ui/src/tray/TrayMenu.tsx"),
        ),
        (
            "ui/src/components/screens/settings/useConfig.ts",
            include_str!("../../../ui/src/components/screens/settings/useConfig.ts"),
        ),
    ];

    /// 发射点：`app.emit(EVENT_CONFIG_CHANGED, …)` 的实参必须是空对象字面量 `json!({})`。
    ///
    /// 判据是**对实参的正向等值断言**，不是负向枚举——旧版判据是「实参里不出现 `cfg`/`newValue`
    /// 这两个今天恰好在用的标识符」，换个变量名（`broadcast_config_changed_with` 的形参本身就叫
    /// `new_value`）或直接把载荷内容写成字面量，两条禁词一条都不命中，守卫全绿而配置树已在路上。
    /// 判据按配对括号取实参，不要求 emit 与其实参写在同一行（rustfmt 拆行不影响本判据）。
    ///
    /// 扫**全部** `app.emit(` 调用点，只对事件名匹配 `EVENT_CONFIG_CHANGED` 的逐一断言载荷、且
    /// 数量必须恰为 1——而不是只看函数体里第一个 `app.emit(`：只看第一个会两头出错：本函数如果
    /// 先发别的事件（如隐私模式跃迁）再发 configChanged，事件名断言会误红；反过来，如果
    /// configChanged 之后又插入第二个带载荷的 `app.emit(EVENT_CONFIG_CHANGED, …)`，第一个合规、
    /// 第二个违规，只看第一个会让第二个静默漏检。数量断言与消费方那侧（`sites == 1`）同规：多插
    /// 一个**合规**的重复 emit 同样要停下来裁定——重复广播 = 三个前端消费方各多跑一次全量
    /// `config_get`，正是本批要防的白付出。
    ///
    /// 事件名不匹配时不再直接跳过不留痕迹：扫到的全部事件名收进 `seen_events`，0 命中时打进失败
    /// 消息——有人把 `EVENT_CONFIG_CHANGED` 改写成全路径或换了个本地别名，emit 明明还在原地，
    /// 消息也不会说成「发射点没了」这种指错方向的话。
    ///
    /// 牙：把载荷改回 `json!({ "config": new_value })`（或任何非空内容，哪怕换个变量名）→ 转红；
    /// 在合规 emit 之后再插一个**同样合规**的 `app.emit(EVENT_CONFIG_CHANGED, json!({}))` → 数量
    /// 断言转红；把 `EVENT_CONFIG_CHANGED` 换成一个不存在的名字 → 转红且消息里能看到扫到的事件名
    /// 不含它。
    #[test]
    fn emit_site_carries_no_config_content() {
        let body = top_level_fn_body(
            include_str!("config.rs"),
            "pub(crate) fn broadcast_config_changed_with(",
        );
        // 切点自检①：扫到的确实是那个生产函数体。
        assert!(
            body.contains("strip_privacy_secrets(&mut cfg)"),
            "扫到的不是 broadcast_config_changed_with 的函数体 —— 守卫已失去判据"
        );
        // 切点自检②：判据词在本文件的测试代码里也各有一份，切片若漏封顶就会被自己喂饱 ——
        // 那正是「源码级判据被自己污染」的形态。
        assert!(
            !body.contains("config_changed_payload_tests"),
            "切片切进了本测试模块，判据会被自己写的字面量喂饱"
        );

        let mut config_changed_emits = 0usize;
        // 扫到的每个 emit 的事件名，仅用于失败诊断——事件名对不上时把它打进消息，不能只说
        // 「发射点没了」（那会把排查方向指反：emit 明明在原地，只是名字变了）。
        let mut seen_events: Vec<&str> = Vec::new();
        for (call_at, _) in body.match_indices("app.emit(") {
            let args_at = call_at + "app.emit(".len();
            // 按配对括号取到本次调用的实参列表（而非要求「事件名 + 逗号」紧跟在 `app.emit(`
            // 后面同一行）。
            let mut depth = 1i32;
            let mut close = None;
            for (k, ch) in body[args_at..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(k);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let close = close.expect("app.emit(...) 括号未配对 —— 发射调用格式已变，需要更新守卫");
            let args = body[args_at..args_at + close].trim();
            let event = args.split_once(',').map_or(args, |(event, _)| event.trim());
            seen_events.push(event);
            if event != "EVENT_CONFIG_CHANGED" {
                continue; // 别的事件，不归本守卫管。
            }
            config_changed_emits += 1;
            if let Some((_, payload)) = args.split_once(',') {
                let payload = payload.trim().trim_end_matches(',').trim();
                assert_eq!(
                    payload, "json!({})",
                    "configChanged 的发射载荷不是空对象字面量（实参：`{payload}`）——\
                     要发载荷必须用剥过隐私的那一份（`strip_privacy_secrets` 之后），且必须\
                     同步改本断言"
                );
            } // 单参数 emit（无逗号）：天然无载荷可言，直接过。
        }
        // 与消费方那侧（`sites == 1`）同规：增减都要停下来人工裁定，不止拦删除。多插一个**合规**
        // 的 `app.emit(EVENT_CONFIG_CHANGED, json!({}))` 一样是重复广播——三个前端消费方各多跑一次
        // 全量 `config_get`、托盘多一次 reconcile，正是本批要防的那类白付出。
        assert_eq!(
            config_changed_emits, 1,
            "configChanged 的发射点数不是 1（实为 {config_changed_emits}）。本函数体内扫到的全部 \
             emit 事件名：{seen_events:?}"
        );
    }

    /// 四个消费方（三个渲染端 + Rust 侧托盘汇流）必须全部丢弃 payload。
    ///
    /// 判据是「形参表为空」，不是字面 `() =>` 前缀匹配。TS 可赋值性规则是「source **必需**形参数
    /// ≤ target 形参数」，rest 形参在这条规则下视作「零个必需形参」——`(...a: unknown[]) => void`、
    /// `async (...a: unknown[]) => void`，以及先具名再传入的
    /// `const h = (...a: unknown[]) => {…}; onChanged(h)`，**全部**能合法赋给
    /// `onChanged(listener: () => void)`（签名见 `ui/src/ipc/api-client.ts`）——类型层完全挡不住
    /// rest 参数，这正是本结构守卫存在的理由；「非箭头字面量就退回类型层」这个论证只在「箭头函数
    /// 只有裸 `(...) =>` 一种写法」时成立，`async` 前缀与具名传参都会绕开它。
    ///
    /// 故判定前先剥可选的 `async ` 前缀，落到真正的形参括号上再比较是否为 `()`；剥完仍不是 `(`
    /// 开头（裸标识符、`function` 表达式、或其它未识别形态，如无括号的单参箭头 `x => …`）
    /// **不静默放过**——源码扫描判不出那类实参的形参表，直接 panic 要求人工裁定。
    ///
    /// `function` 表达式**故意**没有像 `async` 那样被剥前缀特殊处理，即便它形参表可以是空
    /// `()`——因为 `function () { … }` 会绑定 `arguments`，`arguments[0]` 照样能读到完整 payload；
    /// 箭头函数不绑定 `arguments`，才是「形参表空 ⇒ 读不到 payload」这条判据成立的前提。把
    /// `function` 也纳入「形参表为空即放行」会在这条新腿上开一个箭头函数没有的洞，故与裸标识符
    /// 归同一类：源码扫描判不全，一律 panic 要求人工裁定，不假定它已被类型层挡住。
    ///
    /// 牙：`onChanged(() => …)` 改成 `onChanged((...args: unknown[]) => …)`（或加 `async`）→
    /// 转红；改成 `onChanged(onCfg)`（具名回调）或 `onChanged(function () { … })` → panic 要求
    /// 人工裁定。
    #[test]
    fn every_consumer_discards_the_payload() {
        const CALL: &str = ".onChanged(";
        for (path, src) in TS_CONSUMERS {
            // 先剥块注释（含 JSDoc）再剥整行注释：注释里出现调用形态（如 `useConfig.ts` 头部 JSDoc
            // 提到的 `` `configApi.onChanged` ``）会喂饱/顶红判据（与 Rust 侧剥行注释同一理由）。
            let src = strip_line_comments(&strip_block_comments(src));
            // **自曝**：`strip_block_comments` 找不到闭合就不清空、原样保留——那份「不作为」必须
            // 自己被看见，不能只在剩余文本恰好含 `.onChanged(` 时才被数量断言间接带出来（那是零
            // 信号的巧合绿）。扫一遍剥完的文本，任何一行 trim 后仍以 `/*`/`{/*` 开头，说明这正是
            // 一次未闭合起笔被原样吐了回来。
            for (n, line) in src.lines().enumerate() {
                let t = line.trim_start();
                assert!(
                    !t.starts_with("/*") && !t.starts_with("{/*"),
                    "{path}:{} 有一个块注释起笔从未找到闭合 `*/`，strip_block_comments 按 doc 原样\
                     保留了它——这段残留文本没有被清空扫描过，可能藏着一次伪造/丢失的 `.onChanged(` \
                     订阅，需要人工核实",
                    n + 1
                );
            }
            let mut sites = 0usize;
            for (i, _) in src.match_indices(CALL) {
                sites += 1;
                let rest = &src[i + CALL.len()..];
                let rest = rest.trim_start();
                // 剥 `async `：`async (...) => …` 与 `(...) => …` 的形参表位置相同。`function`
                // 前缀不剥——理由见上面 doc 的 `arguments` 那段。
                let param_scan_at = rest.strip_prefix("async").map_or(rest, str::trim_start);
                match param_scan_at.strip_prefix('(') {
                    Some(after_open) => {
                        // 形参表 = 首个 `(` 到与之配对的 `)`（含首尾括号）。
                        let mut depth = 1i32;
                        let mut close = None;
                        for (k, ch) in after_open.char_indices() {
                            match ch {
                                '(' => depth += 1,
                                ')' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        close = Some(k);
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        let close = close.unwrap_or_else(|| {
                            panic!(
                                "{path} 的 `.onChanged(` 实参括号未配对（实处：`{}`）",
                                rest.chars().take(60).collect::<String>()
                            )
                        });
                        let params = &param_scan_at[..close + 2];
                        assert_eq!(
                            params, "()",
                            "{path} 的 configChanged 订阅读了 payload —— 事件已是无载荷信号，读到的\
                             只会是 `{{}}`。形参表：`{params}`"
                        );
                    }
                    None => panic!(
                        "{path} 的 `.onChanged(` 实参不是箭头函数字面量（实处：`{}`）——具名回调 / \
                         `function` 表达式源码扫描判不出（`function` 还会绑定 `arguments`，形参表\
                         为空也可能读到 payload），需要人工核实该回调是否读了 payload，再决定是否\
                         扩展本判据",
                        rest.chars().take(60).collect::<String>()
                    ),
                }
            }
            // 数量断言：订阅点增减必须停下来显式裁定，不许守卫自适应放行（多了 = 新消费方没过判据；
            // 少了 = 这一腿已删，判据表该同步改）。射程记账：本判据只抗块注释伪造（见
            // `strip_block_comments`），不抗**行尾**注释（`foo(); // 见 api.onChanged(cb)` 照数）、
            // 也不抗字符串/模板字面量/JSX 文本里出现 `.onChanged(` 这串字面量——这两类都不做词法
            // 分析，真被这么写就会被静默算作一次「订阅还在」。
            assert_eq!(sites, 1, "{path} 的 configChanged 订阅点数变了");
        }

        // Rust 侧第四腿：`TRAY_SYNC_EVENTS` 含 `EVENT_CONFIG_CHANGED`（订阅面由 `main.rs` 自己的
        // `tray_icon_events_are_the_proxy_lifecycle_channels` 钉住），本条只钉**回调丢弃 payload**。
        let main_body = top_level_fn_body(include_str!("../main.rs"), "fn main() {");
        assert!(
            main_body.contains("wire_tray_icon_sync("),
            "扫到的不是 main() 的函数体 —— 守卫已失去判据"
        );
        // 回调现有两项工作（同步 warm 偏好 + reconcile tray），不能再把整条闭包钉成单表达式；
        // 真正的契约只有形参必须是 `_`，这样闭包体结构扩展也不会误红，同时 payload 仍结构性不可读。
        assert!(
            main_body.contains("handle.listen_any(ev, move |_| {"),
            "托盘汇流的事件回调不再以 `_` 丢弃 payload —— configChanged 已无载荷，读它只会拿到空对象"
        );
    }

    /// **预防性自检**：块注释（含 JSDoc）里若提到调用形态 `.onChanged(cb)` 不得被计入。
    ///
    /// 今天的收益是 0：`useConfig.ts` 头部 JSDoc 提到的是 `` `configApi.onChanged` ``（**没有**左
    /// 括号），不含判据串 `.onChanged(`，就算没有 `strip_block_comments` 也数不进来——本用例钉的是
    /// 「JSDoc 一旦被后人改写成带括号的调用形态」这类将来态，不是复现今天已经存在的漏洞。少了这条
    /// 剥离、且真出现这种改写时：注释能伪造一次订阅、真订阅被删也仍全绿（`sites == 1` 是三腿
    /// 「订阅还在」唯一的钉子）。
    ///
    /// 变异锁：把 `strip_block_comments(src)` 换成裸 `src` → 本用例转红（`sites` 变 2）。
    #[test]
    fn block_comment_mentioning_on_changed_is_not_counted() {
        let src = "/**\n * see `configApi.onChanged(cb)` for details\n */\n\
                   const off = api.onChanged(() => void load());\n";
        let src = strip_line_comments(&strip_block_comments(src));
        let sites = src.match_indices(".onChanged(").count();
        assert_eq!(
            sites, 1,
            "块注释里的 `.onChanged(` 被计入了 —— TS 取材器漏剥块注释，注释能伪造一次订阅"
        );
    }
}

#[cfg(test)]
mod privacy_tests {
    use super::*;

    #[test]
    fn salted_hash_verifies_correct_rejects_wrong_and_empty() {
        let salt = gen_salt().expect("CSPRNG 应可用");
        let stored = format!("{}${}", hex_encode(&salt), hash_password(&salt, "s3cret"));
        assert!(verify_password(&stored, "s3cret"), "正确密码必须验过");
        assert!(!verify_password(&stored, "wrong"), "错误密码必须验败");
        assert!(!verify_password(&stored, ""), "已设密码时空密码不得验过");
    }

    #[test]
    fn hash_is_salted_and_never_plaintext() {
        // 不同盐同密码 → 不同 hash（盐生效，防彩虹表）；hash 不含明文；SHA-256 = 64 hex；同盐同密码稳定。
        let h1 = hash_password(&[1u8; 16], "password");
        let h2 = hash_password(&[2u8; 16], "password");
        assert_ne!(h1, h2, "不同盐必产不同 hash");
        assert!(!h1.contains("password"), "存储绝不含明文");
        assert_eq!(h1.len(), 64, "SHA-256 → 64 hex");
        assert_eq!(
            hash_password(&[1u8; 16], "password"),
            h1,
            "同盐同密码须可复算"
        );
    }

    #[test]
    fn verify_rejects_malformed_stored_fail_closed() {
        assert!(!verify_password("no-separator", "x"));
        assert!(
            !verify_password("zz$deadbeef", "x"),
            "非法盐 hex → fail-closed"
        );
        assert!(!verify_password("$", "x"));
    }

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn has_password_reflects_stored_hash_none_means_false() {
        // no-password → has=false（含缺键与空串）。has 读的是 privacyPasswordHash（非 legacy 明文键）。
        assert!(!config_has_password(&json!({})), "无密码 → has=false");
        assert!(
            !config_has_password(&json!({ "privacyPasswordHash": "" })),
            "空串 → has=false"
        );
        // legacy 明文键即便非空也不算「已设密码」——只认 hash 键。
        assert!(
            !config_has_password(&json!({ "privacyPassword": "legacy-plaintext" })),
            "legacy 明文键不参与 has 判定"
        );
        // set → has=true。
        let salt = gen_salt().unwrap();
        let stored = format!("{}${}", hex_encode(&salt), hash_password(&salt, "pw"));
        assert!(
            config_has_password(&json!({ "privacyPasswordHash": stored })),
            "已设密码 → has=true"
        );
    }

    #[test]
    fn set_has_unlock_flow() {
        // set：写 salted hash 到 privacyPasswordHash（模拟 privacy_set_password 的存储侧）。
        let salt = gen_salt().unwrap();
        let stored = format!(
            "{}${}",
            hex_encode(&salt),
            hash_password(&salt, "correct-horse")
        );
        let cfg = json!({ "privacyPasswordHash": stored });
        // has → true。
        assert!(config_has_password(&cfg));
        let got = cfg
            .get(PRIVACY_PASSWORD_HASH_KEY)
            .and_then(Value::as_str)
            .unwrap();
        // unlock(correct) → true；unlock(wrong) → false（模拟 privacy_unlock 的校验侧）。
        assert!(verify_password(got, "correct-horse"), "正确密码解锁");
        assert!(!verify_password(got, "nope"), "错误密码不解锁");
    }

    #[test]
    fn strip_privacy_secrets_removes_both_legacy_and_hash_keeps_rest() {
        // `config_get`（全量快照的唯一出口）与 `broadcast_config_changed`（入核那份 cfg，非前端
        // 出口）共用的剥离：明文 + hash 都不下发，其余键保留。
        let mut cfg = json!({
            "privacyPassword": "legacy-plaintext",
            "privacyPasswordHash": "aabb$deadbeef",
            "proxyMode": "global",
            "mixedPort": 7890,
        });
        strip_privacy_secrets(&mut cfg);
        assert!(cfg.get("privacyPassword").is_none(), "legacy 明文键剥除");
        assert!(
            cfg.get("privacyPasswordHash").is_none(),
            "salted hash 键剥除"
        );
        assert_eq!(cfg["proxyMode"], json!("global"), "非敏感键保留");
        assert_eq!(cfg["mixedPort"], json!(7890));
    }

    /// [`is_privacy_key`] 与 [`strip_privacy_secrets`] 判的是同一份键，不多不少。
    #[test]
    fn is_privacy_key_matches_exactly_the_two_legacy_keys() {
        assert!(is_privacy_key(PRIVACY_PASSWORD_KEY));
        assert!(is_privacy_key(PRIVACY_PASSWORD_HASH_KEY));
        assert!(!is_privacy_key("proxyMode"));
        assert!(!is_privacy_key("mixedPort"));
        assert!(!is_privacy_key(""));
    }

    /// **调用点守卫**：`config_get_value` 持 `State<'_, AppRuntime>`，单测构造不出 Tauri 运行时
    /// ⇒ 用源码扫描锁「命令确实委托给可测核心」（同 `backup_import_routes_through_the_shared_save_core`
    /// 的理由）；核心本身（[`config_get_value_core`]）不持 State，行为面由下面
    /// `config_get_value_core_blocks_privacy_keys_even_when_present_on_disk` 端到端覆盖。
    ///
    /// 不盖住它的后果：单键出口 `configApi.getValue('privacyPasswordHash')` 会把 legacy hash 原样
    /// 交给渲染端——`config_get`/`broadcast_config_changed` 的剥离都拦不住它，因为它们剥的是**另一条**
    /// 路径（全量快照），`config_get_value` 走的是 `ConfigManager::get_value` 直读，从未经过
    /// `strip_privacy_secrets`。
    ///
    /// 牙：把 `config_get_value` 改回直接调 `state.config().get_value(&key)`（绕开
    /// `config_get_value_core`）→ 转红。
    #[test]
    fn config_get_value_delegates_to_the_testable_core() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            include_str!("config.rs"),
            "pub fn config_get_value(",
        );
        assert!(
            body.contains("config_get_value_core(state.config(), &key)"),
            "config_get_value 不再委托 config_get_value_core —— 单键读的隐私键短路可能被绕过"
        );
    }

    /// **顺序守卫**：[`config_get_value_core`] 里的隐私键判定必须排在真正读配置**之前**短路返回，
    /// 不是读完了再事后补救。
    ///
    /// 牙：把 `is_privacy_key(key)` 判定挪到 `config.get_value(key)` 之后 → 转红。
    #[test]
    fn config_get_value_core_checks_privacy_keys_before_touching_config_manager() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            include_str!("config.rs"),
            "fn config_get_value_core(",
        );
        let guard_at = body
            .find("is_privacy_key(key)")
            .expect("扫到的不是 config_get_value_core 的函数体 —— 守卫已失去判据");
        let read_at = body
            .find("config.get_value(key)")
            .expect("真正的读配置调用没了 —— 守卫已失去判据");
        assert!(
            guard_at < read_at,
            "隐私键判定必须排在读配置之前短路返回，不是读完了再事后补救"
        );
    }

    /// **端到端实测**（不是源码扫描推断）：磁盘上真有隐私哈希时，单键读依然拿不到它。
    ///
    /// 先用底层 `ConfigManager::get_value` 直读做反证——证明磁盘上确实存了这个哈希、直读确实能
    /// 读出真值，排除「碰巧键不存在所以是 Null」这个混淆；再证明经 `config_get_value_core` 读同一个
    /// 键拿到的是 `Null`。
    ///
    /// 牙：删掉 [`config_get_value_core`] 里的 `is_privacy_key` 短路 → 第二组断言转红（会读到真哈希
    /// 而不是 `Null`）。
    #[test]
    fn config_get_value_core_blocks_privacy_keys_even_when_present_on_disk() {
        let dir = std::env::temp_dir().join(format!(
            "polaris-config-get-value-privacy-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mgr = ConfigManager::new(dir);
        // 首启默认配置本身已合法（含 `tunConfig` 等 validate 必需字段），在它上面叠两个隐私键，
        // 不必手搭一份完整合法配置。
        let mut cfg = mgr.current().expect("首启应给默认配置");
        cfg["privacyPassword"] = json!("legacy-plaintext");
        cfg["privacyPasswordHash"] = json!("aabb$deadbeef");
        mgr.save_full(&cfg).expect("save_full 应成功");

        // 反证：底层直读确实能拿到磁盘上真实存在的隐私键，下面的 Null 不是「键不存在」的巧合。
        assert_eq!(
            mgr.get_value(PRIVACY_PASSWORD_HASH_KEY).unwrap(),
            json!("aabb$deadbeef"),
            "ConfigManager::get_value 本身必须能读到真哈希，否则下面的测试无意义"
        );

        assert_eq!(
            config_get_value_core(&mgr, PRIVACY_PASSWORD_HASH_KEY).unwrap(),
            Value::Null,
            "getValue('privacyPasswordHash') 必须拿不到值，即便磁盘上真有这个哈希"
        );
        assert_eq!(
            config_get_value_core(&mgr, PRIVACY_PASSWORD_KEY).unwrap(),
            Value::Null,
            "legacy 明文键同样必须拦住"
        );
        assert_eq!(
            config_get_value_core(&mgr, "proxyMode").unwrap(),
            cfg["proxyMode"],
            "非隐私键必须不受影响，堵洞不能堵过头"
        );
    }

    /// 契约 L141「解锁失败 sleep(300) 弱限速」：只在失败路径限速，成功/未设密码自由解锁不拖手感。
    ///
    /// 打断 `apply_unlock_rate_limit` 里的 `tokio::time::sleep` 调用（或整段 if 分支）→ 第二个
    /// 断言（失败须 ≥300ms）转红。
    #[tokio::test]
    async fn rate_limit_delays_only_on_failure() {
        let t_ok = std::time::Instant::now();
        apply_unlock_rate_limit(true).await;
        assert!(
            t_ok.elapsed() < std::time::Duration::from_millis(100),
            "密码正确 / 未设密码自由解锁：不得限速"
        );

        let t_fail = std::time::Instant::now();
        apply_unlock_rate_limit(false).await;
        assert!(
            t_fail.elapsed() >= std::time::Duration::from_millis(UNLOCK_FAIL_DELAY_MS),
            "解锁失败必须弱限速 ≥{UNLOCK_FAIL_DELAY_MS}ms（契约 L141）"
        );
    }
}

// ── 后端权威字段闭环（用户报「托盘最近节点只剩 1 条」的回归门）────────────────────
//
// 全部经**生产路径** `config_save_core` 驱动（而非测试自己调 `enforce_backend_authoritative_fields`
// 再 `save_full`）—— 后者会让「删掉生产代码里的 enforce 调用后测试照样绿」成为可能 = 假绿，
// 同 `config_save_core` 文档里记的那条纪律。
#[cfg(test)]
mod backend_authoritative_tests {
    use super::*;
    use crate::runtime::config::ConfigManager;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-backend-auth-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// **用户报的那个 bug 的直接回归**：后端写了 MRU 历史后，前端携**陈旧** `recentServerIds`
    /// 的全量保存不得把它抹回去。
    ///
    /// 牙：删掉 `config_save_core` 里的 `enforce_backend_authoritative_fields` 调用 → 落盘变成
    /// 前端那份 `["stale"]` → 第一个断言转红。把 enforce 改成「仅当入参缺键才回填」（即退化成
    /// `preserve_server_owned_secrets` 那套策略）→ 同样转红，因为前端快照**带着**该键。
    #[test]
    fn stale_frontend_snapshot_cannot_wipe_backend_written_mru() {
        let dir = temp_dir("mru");
        let mgr = ConfigManager::new(dir.clone());

        // T0：前端拿到快照（此刻 MRU 还是旧值）——`config_get` 下发的是**完整** config。
        let mut as_frontend_sees = mgr.load_full().unwrap();
        as_frontend_sees["recentServerIds"] = json!(["stale"]);
        as_frontend_sees["logLevel"] = json!("debug");

        // T1：后端写 MRU（等价 server_switch 连切三个节点），前端快照就此过期。
        let mut cfg = mgr.load_full().unwrap();
        cfg["recentServerIds"] = json!(["n3", "n2", "n1"]);
        mgr.save_full(&cfg).unwrap();

        // T2：前端此刻才提交那份陈旧快照（改任意设置都会走到这条路）。
        config_save_core(&mgr, &mut as_frontend_sees, None).expect("save 应成功");

        // 从磁盘重 load 核实：后端历史完好，前端的无关改动照常生效。
        let on_disk = ConfigManager::new(dir.clone()).load_full().unwrap();
        assert_eq!(
            on_disk["recentServerIds"],
            json!(["n3", "n2", "n1"]),
            "后端权威字段不得被前端陈旧快照抹回"
        );
        assert_eq!(
            on_disk["logLevel"],
            json!("debug"),
            "前端权威字段的改动照常落盘"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 删除表达（后端权威侧）：磁盘**无**该键 ⇒ 落盘也必须无该键，前端携带的陈旧值不得复活它。
    ///
    /// 这是白名单的另一半语义：删除权归字段所有者（后端），前端既无写入权也无需表达删除。缺了
    /// `None` 腿，所有者就**删不掉自己的键** —— 任一全量保存都会把它从前端的陈旧快照里复活回来，
    /// 而所有者对此毫无察觉。
    ///
    /// 夹具用 `recentServerIds`（本批之前用的是 `diagnosticCapture`，那套机制已删除）。断言的是
    /// [`enforce_backend_authoritative_fields`] 的镜像契约本身，与具体是哪个键无关，故照旧走**生产
    /// 保存路径**打，而不是直接调那个函数。
    ///
    /// 牙：删掉 `enforce_backend_authoritative_fields` 里的 `None => { obj.remove(key); }` 腿 → 转红。
    #[test]
    fn backend_deleted_key_is_not_resurrected_by_stale_snapshot() {
        let dir = temp_dir("authoritative-delete");
        let mgr = ConfigManager::new(dir.clone());

        // 后端写入权威键（形态同 `server_switch` 落 MRU）。
        let mut cfg = mgr.load_full().unwrap();
        cfg["recentServerIds"] = json!(["srv-a", "srv-b"]);
        mgr.save_full(&cfg).unwrap();

        // 前端快照停在这一刻（完整 config，带着该键）。
        let mut as_frontend_sees = mgr.load_full().unwrap();
        as_frontend_sees["logLevel"] = json!("warn");
        assert!(
            as_frontend_sees.get("recentServerIds").is_some(),
            "前提：前端快照确实带着该权威键"
        );

        // 后端删掉该键（所有者行使删除权）。
        let mut cfg = mgr.load_full().unwrap();
        cfg.as_object_mut().unwrap().remove("recentServerIds");
        mgr.save_full(&cfg).unwrap();

        // 前端此刻才提交陈旧快照（LogsScreen 改日志级别的真实形态）。
        config_save_core(&mgr, &mut as_frontend_sees, None).expect("save 应成功");

        let on_disk = ConfigManager::new(dir.clone()).load_full().unwrap();
        assert!(
            on_disk.get("recentServerIds").is_none(),
            "后端已删的键不得被前端陈旧快照复活（否则字段所有者永远删不掉自己的键）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **删除语义未被破坏**（前端权威侧）：白名单外的键仍是整份覆盖，用户清空数组 / 删键必须真落盘。
    ///
    /// 这条守的是「引入 merge 会让删除变不可能」那个设计陷阱：本批**没有**引入 merge，
    /// 故 上游的两种删除表达（传 `[]` 清空、缺键删除）必须原样有效。
    ///
    /// 牙：把 `config_save_core` 改成对**全部**键做「磁盘有值就覆盖」的深合并 → 三个断言全红。
    #[test]
    fn frontend_owned_fields_keep_full_overwrite_delete_semantics() {
        let dir = temp_dir("delete");
        let mgr = ConfigManager::new(dir.clone());

        // 起点：磁盘上有一条自定义规则、一个非空旁路清单、一个自定义应用预设。
        let mut cfg = mgr.load_full().unwrap();
        cfg["customRules"] = json!([{ "id": "r1", "enabled": true }]);
        cfg["fakeIpFilterList"] = json!(["a.example", "b.example"]);
        cfg["customAppPresets"] = json!([{ "id": "p1", "name": "P" }]);
        mgr.save_full(&cfg).unwrap();

        // 前端：删掉最后一条规则（传空数组）、清空旁路清单（传空数组）、
        // 删掉 customAppPresets 键本身（不传该键 —— 上游的 `x: undefined` 等价形）。
        let mut submitted = mgr.load_full().unwrap();
        submitted["customRules"] = json!([]);
        submitted["fakeIpFilterList"] = json!([]);
        submitted
            .as_object_mut()
            .unwrap()
            .remove("customAppPresets");
        config_save_core(&mgr, &mut submitted, None).expect("save 应成功");

        let on_disk = ConfigManager::new(dir.clone()).load_full().unwrap();
        assert_eq!(
            on_disk["customRules"],
            json!([]),
            "删最后一条规则必须真删掉（merge 语义下这里会被还原成 [r1]）"
        );
        assert_eq!(
            on_disk["fakeIpFilterList"],
            json!([]),
            "清空列表必须真清空（merge 语义下会被还原）"
        );
        assert!(
            on_disk.get("customAppPresets").is_none(),
            "缺键删除不得被磁盘旧值还原（merge 语义下会被还原成 [p1]）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 白名单**边界**：`clashApiSecret` 后端也写，但前端有「重新生成」按钮（SettingsNetwork）
    /// ⇒ 绝不能进白名单，否则该按钮静默失效（点了没反应）。
    ///
    /// 牙：把 `clashApiSecret` 加进 `BACKEND_AUTHORITATIVE_KEYS` → 本测转红。
    /// 这条是防「未来有人按『后端写过就算后端权威』的错判准扩白名单」的守卫。
    #[test]
    fn frontend_writable_secret_is_not_locked_by_whitelist() {
        let dir = temp_dir("secret");
        let mgr = ConfigManager::new(dir.clone());

        let mut cfg = mgr.load_full().unwrap();
        cfg["clashApiSecret"] = json!("old-secret");
        mgr.save_full(&cfg).unwrap();

        // 前端点「重新生成」→ 全量提交新 secret。
        let mut submitted = mgr.load_full().unwrap();
        submitted["clashApiSecret"] = json!("regenerated-secret");
        config_save_core(&mgr, &mut submitted, None).expect("save 应成功");

        let on_disk = ConfigManager::new(dir.clone()).load_full().unwrap();
        assert_eq!(
            on_disk["clashApiSecret"],
            json!("regenerated-secret"),
            "前端有写入权的字段不得被白名单锁住"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 白名单**非空**守卫：防「把 `BACKEND_AUTHORITATIVE_KEYS` 清空」这种假绿变异——
    /// 清空后上面几条生产路径测试里，只有正向断言会红，此处显式锁住成员构成。
    #[test]
    fn whitelist_membership_is_pinned() {
        assert!(
            BACKEND_AUTHORITATIVE_KEYS.contains(&"recentServerIds"),
            "托盘 MRU 必须在白名单内（用户报的那个 bug）"
        );
        assert!(
            BACKEND_AUTHORITATIVE_KEYS.contains(&"builtinGeoMeta"),
            "随包 geo 元数据必须在白名单内（ui 全仓零读零写）"
        );
        assert!(
            !BACKEND_AUTHORITATIVE_KEYS.contains(&"diagnosticCapture"),
            "诊断采集机制已整体删除，该键不得再作为任何人的权威字段留在白名单里"
        );
        assert!(
            !BACKEND_AUTHORITATIVE_KEYS.contains(&"clashApiSecret"),
            "clashApiSecret 前端可写，绝不得进白名单"
        );
        assert!(
            !BACKEND_AUTHORITATIVE_KEYS.contains(&"servers"),
            "servers 前端有写入权（备份导入 / 全量保存），绝不得进白名单"
        );
    }

    /// **调用点守卫**（射程补齐）：`config_save` 这条腿由上面的生产路径测试盖住了，但**备份导入**
    /// （`backup_import_apply`）是前端全量提交的**第二个**入口，它持 `State<'_, AppRuntime>` +
    /// `AppHandle`，单测构造不出 Tauri 运行时 ⇒ 改用源码扫描锁调用点。
    ///
    /// 不盖住它的后果：导入一份**存量旧备份**（那些文件里还带着导出机的 `recentServerIds`）会把外机的
    /// MRU 装进本机。与 `preserve_server_owned_secrets` 在同一处、同一理由。
    ///
    /// 牙：把 `misc.rs` 里 `backup_import_apply` **函数体内**的
    /// `backup_import_save_core(...)` 换回裸 `save_full(&restored)` → 转红。
    ///
    /// # 切片必须封顶（本守卫此前的洞）
    ///
    /// 原实现切的是 `&src[s..]` —— 从签名一路到 **EOF**，而非该函数的右花括号。于是「删掉本函数里的调用、
    /// 再在这个 1000+ 行文件的**任意后续位置**加一个」就能让守卫照样绿，牙只在今天这个文件布局下存在。
    /// 现按列 0 的 `\n}\n` 封顶到函数自己的作用域，见 [`top_level_fn_body`]。
    #[test]
    fn backup_import_routes_through_the_shared_save_core() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            include_str!("misc.rs"),
            "pub async fn backup_import_apply(",
        );
        assert!(
            body.contains("backup_import_save_core(state.config(), &current, &mut restored)"),
            "备份导入必须经共用落盘腿（三条策略 + save_full 的顺序与配对由它单一收口）"
        );
        assert!(
            !body.contains("save_full("),
            "第二条落盘路径 = 迟早只挂一条策略：备份导入不得再直接 save_full"
        );
    }

    /// 🔴 **落盘腿本身的三条策略 + 顺序**：全部必须排在 `save_full` **之前**。
    ///
    /// 行为面由本模块的生产路径测试覆盖（`backup_import_*` 三条 + 上面的 enforce/preserve 用例）；
    /// 本守卫只钉「三条都还在、且都在落盘之前」这个纯结构事实 —— 顺序反了（落盘后再清/再回填）
    /// 语义上等于没做，而行为测试对「先落盘再改内存副本」这种写法**恰好也是绿的**（磁盘上是坏值，
    /// 但测试若读的是返回的内存值就看不出来）。
    ///
    /// 牙：删掉三条策略任一 / 把任一挪到 `config.save_full(restored)` 之后 → 逐条转红。
    #[test]
    fn backup_import_save_core_runs_all_three_policies_before_the_write() {
        let body = crate::commands::guard_scan::top_level_fn_body(
            include_str!("config.rs"),
            "pub(crate) fn backup_import_save_core(",
        );
        let write = body
            .find("config.save_full(restored)")
            .expect("落盘腿被删了 —— 导入不再落盘");
        for needle in [
            "preserve_server_owned_secrets(config, restored)",
            "enforce_backend_authoritative_fields(config, restored)",
            "invalidate_validators_on_global_ua_change(current, restored)",
        ] {
            let at = body
                .find(needle)
                .unwrap_or_else(|| panic!("备份导入落盘腿少了一条策略: {needle}"));
            assert!(
                at < write,
                "`{needle}` 必须排在落盘之前，落盘后再做等于没做"
            );
        }
    }

    /// 首启无配置文件（`current()` 读不到）→ enforce 必须是空操作，不阻断保存、不凭空造键。
    #[test]
    fn missing_current_config_is_a_noop() {
        let dir = temp_dir("first-run");
        let mgr = ConfigManager::new(dir.clone());
        let mut incoming = json!({ "recentServerIds": ["a"], "logLevel": "info" });
        // 不先 load_full：走「缓存未暖 → current() 自行 load 默认配置」的首启路径。
        enforce_backend_authoritative_fields(&mgr, &mut incoming);
        // 默认配置无 recentServerIds → 按镜像语义该键被删（而非保留前端值），且不 panic。
        assert!(incoming.get("recentServerIds").is_none());
        assert_eq!(incoming["logLevel"], json!("info"), "非白名单键不受影响");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ── 真实 ConfigStore 驱动的隐私密码闭环（HIGH 安全回归）──────────────────────────
//
// 核心不变式：hash 存进 `privacyPasswordHash` → 经 store 的 sanitize/migrate/validate/save + 重 load
// 后**存活**（migrate 只清 legacy 明文 `privacyPassword`，不碰 hash 键）。若回归到把 hash 存进
// `privacyPassword`，reload 后 hash 被 migrate 抹空 → has=false + 任意密码免验通过，下列测试转红。
#[cfg(test)]
mod privacy_store_tests {
    use super::*;
    use crate::runtime::config::ConfigManager;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-privacy-store-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 生产路径回归：前端全量 `saveConfig` 覆盖 config.json 后，隐私密码（scrypt 独立文件）必须仍在。
    ///
    /// 独立文件与 config.json 物理分离 → 前端全量保存**永远碰不到**它（架构级消除「设完密码改任意设置就拆锁」的洞）。
    #[test]
    fn frontend_full_save_without_hash_preserves_password() {
        let dir = temp_dir("full-save");
        let mgr = ConfigManager::new(dir.clone());
        set_password_core(&mgr, "correct horse", false).expect("set 应成功");

        // 模拟前端：拿 config_get 的产物（已 strip hash）→ 改一个无关键 → 全量提交。
        let mut as_frontend_sees = mgr.load_full().expect("load 应成功");
        strip_privacy_secrets(&mut as_frontend_sees);
        assert!(
            as_frontend_sees.get(PRIVACY_PASSWORD_HASH_KEY).is_none(),
            "前提：前端拿到的 config 不含 hash"
        );
        as_frontend_sees["logLevel"] = json!("debug");

        config_save_core(&mgr, &mut as_frontend_sees, None).expect("save 应成功");

        let mgr2 = ConfigManager::new(dir.clone());
        assert!(
            has_password_core(&mgr2).unwrap(),
            "全量保存后密码仍在（独立文件未受影响）"
        );
        assert!(
            unlock_core(&mgr2, "correct horse").unwrap(),
            "正确密码仍可解锁"
        );
        assert!(!unlock_core(&mgr2, "whatever").unwrap(), "任意密码不得放行");
        assert_eq!(
            mgr2.current().unwrap()["logLevel"],
            json!("debug"),
            "无关键的改动照常生效"
        );
    }

    /// 回填**不得**堵死清除密码：清除走的是「键缺失」（`obj.remove`），若回填无条件生效则永远清不掉。
    ///
    /// 打断 `preserve_server_owned_secrets` 的「入参显式带该键 → 尊重入参」分支 → 本测转红。
    #[test]
    fn clearing_password_still_works_through_its_own_path() {
        let dir = temp_dir("clear");
        let mgr = ConfigManager::new(dir.clone());
        set_password_core(&mgr, "temp pass", false).expect("set 应成功");
        assert!(has_password_core(&mgr).unwrap(), "前提：已设密码");

        set_password_core(&mgr, "", false).expect("清除应成功");

        let mgr2 = ConfigManager::new(dir.clone());
        assert!(
            !has_password_core(&mgr2).unwrap(),
            "密码已清除（回填没把它填回来）"
        );
        assert!(
            unlock_core(&mgr2, "anything").unwrap(),
            "未设密码 → 自由解锁"
        );
    }

    /// 入参显式带 hash（专线写入）→ 尊重入参，不被当前值顶掉。
    #[test]
    fn explicit_hash_in_payload_wins_over_backfill() {
        let dir = temp_dir("explicit");
        let mgr = ConfigManager::new(dir.clone());
        set_password_core(&mgr, "old", false).expect("set 应成功");

        let mut incoming = mgr.current().unwrap();
        incoming[PRIVACY_PASSWORD_HASH_KEY] = json!("aabb$newhash");
        preserve_server_owned_secrets(&mgr, &mut incoming);
        assert_eq!(
            incoming[PRIVACY_PASSWORD_HASH_KEY],
            json!("aabb$newhash"),
            "显式入参不被回填覆盖"
        );
    }

    #[test]
    fn hash_survives_reload_and_gates_unlock() {
        let dir = temp_dir("survive");
        {
            let mgr = ConfigManager::new(dir.clone());
            set_password_core(&mgr, "correct horse", false).expect("set 应成功");
        }
        // 新建 ConfigManager 从磁盘重 load（每次 load 都跑 migrate_privacy_password_clear）。
        let mgr2 = ConfigManager::new(dir.clone());
        assert!(
            has_password_core(&mgr2).unwrap(),
            "reload 后 has=true（hash 未被 migrate 清空）"
        );
        assert!(unlock_core(&mgr2, "correct horse").unwrap(), "正确密码解锁");
        assert!(!unlock_core(&mgr2, "wrong").unwrap(), "错误密码不解锁");
        assert!(
            !unlock_core(&mgr2, "").unwrap(),
            "已设密码时空密码不得免验通过（原 bug 的核心）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_plaintext_wiped_but_scrypt_file_untouched_on_reload() {
        let dir = temp_dir("legacy");
        {
            let mgr = ConfigManager::new(dir.clone());
            set_password_core(&mgr, "pw", false).unwrap(); // scrypt 文件
        }
        // 往磁盘 config 手动塞 legacy 明文 privacyPassword（模拟旧版残留）。
        let cfg_path = dir.join("config.json");
        let mut on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        on_disk
            .as_object_mut()
            .unwrap()
            .insert("privacyPassword".into(), json!("LEAK_PLAINTEXT"));
        std::fs::write(&cfg_path, serde_json::to_string(&on_disk).unwrap()).unwrap();
        // reload：migrate 清明文；scrypt 文件（独立于 config）不受影响。
        let mgr2 = ConfigManager::new(dir.clone());
        let cfg = mgr2.load_full().unwrap();
        assert_eq!(
            cfg["privacyPassword"],
            json!(""),
            "legacy 明文被 migrate 清空"
        );
        assert!(
            has_password_core(&mgr2).unwrap(),
            "scrypt 文件未受 config 迁移影响 → has=true"
        );
        assert!(unlock_core(&mgr2, "pw").unwrap(), "scrypt 文件仍可校验解锁");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_password_removes_hash_and_reopens_free_unlock() {
        let dir = temp_dir("clear");
        let mgr = ConfigManager::new(dir.clone());
        set_password_core(&mgr, "pw", false).unwrap();
        assert!(has_password_core(&mgr).unwrap());
        // 空串清除。
        set_password_core(&mgr, "", false).unwrap();
        let mgr2 = ConfigManager::new(dir.clone());
        assert!(!has_password_core(&mgr2).unwrap(), "清除后 has=false");
        assert!(
            unlock_core(&mgr2, "anything").unwrap(),
            "未设密码 → 自由解锁"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scrypt_hash_never_enters_config_object() {
        // 架构级隔离：scrypt 哈希存独立 privacy-lock.json，**从不进 config 对象** → 前端出口天然无从泄漏
        // （比「进 config 再逐出口剥」强一档）。set 后 config 缓存里恒无 hash 键；文件里则有。
        let dir = temp_dir("noleak");
        let mgr = ConfigManager::new(dir.clone());
        set_password_core(&mgr, "topsecret", false).unwrap();
        let full = mgr.current().unwrap();
        assert!(
            full.get(PRIVACY_PASSWORD_HASH_KEY).is_none(),
            "scrypt hash 从不进 config 对象（独立文件存储）"
        );
        // config 默认模板带 `privacyPassword: ""`（空），但绝不含非空明文（migrate 恒清空）。
        let plain = full
            .get(PRIVACY_PASSWORD_KEY)
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(plain.is_empty(), "config 里无非空明文");
        // 独立文件确实持有哈希（可验），但它不在任何前端可见的 config 出口里。
        let path = privacy_lock_path(&mgr);
        let h = polaris_store::privacy_lock::read(&StdFs, &path).expect("文件持有 scrypt 哈希");
        assert!(polaris_store::privacy_lock::verify("topsecret", &h));
        // strip 仍兜底剥 legacy 键（存量未迁移用户过渡期）。
        let mut with_legacy = json!({ "privacyPassword": "x", "privacyPasswordHash": "aa$bb" });
        strip_privacy_secrets(&mut with_legacy);
        assert!(
            with_legacy.get(PRIVACY_PASSWORD_HASH_KEY).is_none(),
            "legacy hash 键剥除"
        );
        assert!(
            with_legacy.get(PRIVACY_PASSWORD_KEY).is_none(),
            "legacy 明文键剥除"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 存量迁移（不锁死老用户）：config.json 里的 legacy salted-SHA256（无 scrypt 文件）→ has=true、
    /// 正确密码可解锁，且解锁后**透明升级**为 scrypt 文件 + 抹掉 legacy 键。
    ///
    /// 变异门「迁移丢旧 hash」：若升级路径在写 scrypt 文件前就删 legacy 键、或验败也删键 → 老用户被锁死，
    /// 下方「升级后仍可解锁」或姊妹测试 `legacy_sha256_wrong_password_no_upgrade_no_lockout` 转红。
    #[test]
    fn legacy_sha256_unlock_upgrades_to_scrypt_file() {
        let dir = temp_dir("legacy-upgrade");
        // 手工种一个 legacy salted-SHA256 config（模拟旧版本存量用户，无 privacy-lock.json）。
        let salt = gen_salt().unwrap();
        let legacy = format!("{}${}", hex_encode(&salt), hash_password(&salt, "old-pass"));
        {
            let mgr = ConfigManager::new(dir.clone());
            let mut cfg = mgr.load_full().unwrap();
            cfg.as_object_mut()
                .unwrap()
                .insert(PRIVACY_PASSWORD_HASH_KEY.into(), json!(legacy));
            mgr.save_full(&cfg).unwrap();
        }
        let path = polaris_store::privacy_lock::lock_path(&dir);
        assert!(!path.exists(), "前提：存量态无 scrypt 文件");

        let mgr = ConfigManager::new(dir.clone());
        assert!(has_password_core(&mgr).unwrap(), "legacy 键 → has=true");
        // 错密码：不解锁、不升级、不删旧键（防锁死）。
        assert!(!unlock_core(&mgr, "wrong").unwrap(), "错误密码不解锁");
        assert!(!path.exists(), "错密码不得建 scrypt 文件");
        // 正确密码：解锁 + 透明升级。
        assert!(
            unlock_core(&mgr, "old-pass").unwrap(),
            "正确 legacy 密码解锁"
        );
        assert!(path.exists(), "解锁后升级为 scrypt 文件");
        // 升级后：新 ConfigManager（冷缓存）仍可解锁；legacy 键已从 config 抹除；走的是 scrypt 文件。
        let mgr2 = ConfigManager::new(dir.clone());
        assert!(
            mgr2.load_full()
                .unwrap()
                .get(PRIVACY_PASSWORD_HASH_KEY)
                .is_none(),
            "升级后 legacy 键已抹除（单一真值源）"
        );
        assert!(
            has_password_core(&mgr2).unwrap(),
            "升级后 has=true（来自文件）"
        );
        assert!(
            unlock_core(&mgr2, "old-pass").unwrap(),
            "升级后正确密码仍解锁"
        );
        assert!(
            !unlock_core(&mgr2, "old-pass-wrong").unwrap(),
            "升级后错误密码不解锁"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 存量迁移安全边：legacy 密码**验败**绝不删旧键 / 不建文件 → 老用户不被锁死，正确密码仍能解锁。
    #[test]
    fn legacy_sha256_wrong_password_no_upgrade_no_lockout() {
        let dir = temp_dir("legacy-nolockout");
        let salt = gen_salt().unwrap();
        let legacy = format!("{}${}", hex_encode(&salt), hash_password(&salt, "keep-me"));
        {
            let mgr = ConfigManager::new(dir.clone());
            let mut cfg = mgr.load_full().unwrap();
            cfg.as_object_mut()
                .unwrap()
                .insert(PRIVACY_PASSWORD_HASH_KEY.into(), json!(legacy));
            mgr.save_full(&cfg).unwrap();
        }
        let path = polaris_store::privacy_lock::lock_path(&dir);
        let mgr = ConfigManager::new(dir.clone());
        // 连续错密码若误删旧键则会把用户锁死——此处验证多次错误后正确密码依旧解锁。
        assert!(!unlock_core(&mgr, "nope1").unwrap());
        assert!(!unlock_core(&mgr, "nope2").unwrap());
        assert!(!path.exists(), "错密码全程不建文件");
        assert!(
            has_password_core(&mgr).unwrap(),
            "旧键未被删 → 仍算已设密码"
        );
        assert!(
            unlock_core(&mgr, "keep-me").unwrap(),
            "正确密码始终能解锁（未被锁死）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 每次 set 新生成盐（salt 唯一，防同密码撞相同哈希）。变异门「删 salt / 盐恒定」→ 两次哈希相同、转红。
    #[test]
    fn salt_unique_per_set() {
        let dir = temp_dir("salt-uniq");
        let mgr = ConfigManager::new(dir.clone());
        let path = privacy_lock_path(&mgr);
        set_password_core(&mgr, "same-pw", false).unwrap();
        let h1 = polaris_store::privacy_lock::read(&StdFs, &path).unwrap();
        set_password_core(&mgr, "same-pw", false).unwrap();
        let h2 = polaris_store::privacy_lock::read(&StdFs, &path).unwrap();
        assert_ne!(h1.salt, h2.salt, "两次 set 同密码须用不同盐");
        assert_ne!(h1.hash, h2.hash, "不同盐 → 不同哈希");
        // 两者都能验过（盐随各自哈希一起存）。
        assert!(polaris_store::privacy_lock::verify("same-pw", &h2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 契约 L141「锁屏禁改/清密码」回归：锁屏态下改密码、清密码都必须被拒，且**不得动存储一个字节**——
    /// 这正是此前的洞：锁屏状态下传空串本会走到 `obj.remove(HASH_KEY)` 直接清密码 = 免验解锁。
    ///
    /// 打断 `set_password_core` 顶部的 `if locked { return Err(...) }` → 本测两处 `expect_err` 转红
    /// （改密码会把 "before-lock" 覆盖掉、清密码会让 has_password 变 false）。
    #[test]
    fn locked_rejects_set_and_clear_without_touching_storage() {
        let dir = temp_dir("locked-gate");
        let mgr = ConfigManager::new(dir.clone());
        set_password_core(&mgr, "before-lock", false).expect("解锁态应可正常设密码");
        assert!(has_password_core(&mgr).unwrap(), "前提：已设密码");

        // 锁屏态：改密码被拒——旧密码原样有效。
        let err =
            set_password_core(&mgr, "attempt-change", true).expect_err("锁屏态必须拒绝改密码");
        assert!(matches!(err, SetPasswordError::Locked));
        assert!(
            unlock_core(&mgr, "before-lock").unwrap(),
            "锁屏态改密码被拒后，旧密码必须原样可解锁"
        );

        // 锁屏态：清密码（空串）同样被拒——密码必须仍在。
        let err2 = set_password_core(&mgr, "", true).expect_err("锁屏态必须拒绝清密码");
        assert!(matches!(err2, SetPasswordError::Locked));
        assert!(
            has_password_core(&mgr).unwrap(),
            "锁屏态清密码请求被拒后，密码必须仍在"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ── A7（R21）换出口 → 解锁缓存失效：谓词 + 决策核心 + 老/新提取链变异门 ─────────────────
#[cfg(test)]
mod unlock_invalidate_tests {
    use super::*;
    use crate::runtime::http::HttpRuntime;
    use crate::runtime::unlock::UnlockRuntime;
    use polaris_unlock::{UnlockResult, UnlockSnapshot};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// 记录型 sink：本腿只触发 invalidated（progress/updated 由检测轮触发，与失效接线无关）。
    #[derive(Default)]
    struct CountingSink {
        invalidated: Mutex<Vec<(bool, bool)>>,
    }
    impl UnlockEventSink for CountingSink {
        fn progress(&self, _service_id: &str, _result: &UnlockResult) {}
        fn updated(&self, _snapshot: &UnlockSnapshot) {}
        fn invalidated(&self, running: bool, exit_blocked: bool) {
            self.invalidated
                .lock()
                .unwrap()
                .push((running, exit_blocked));
        }
    }

    /// 建 UnlockRuntime（http client 仅供构造；`invalidate` 不碰 http，不触网）。
    fn runtime() -> UnlockRuntime {
        UnlockRuntime::new(Arc::new(HttpRuntime::new().expect("建 http client")))
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-unlock-inval-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 纯谓词：出口 identity 变判准（两侧 Option）。打断（恒 true / 恒 false）→ 对应断言转红。
    #[test]
    fn selected_exit_changed_only_on_identity_change() {
        assert!(selected_exit_changed(Some("a"), Some("b")), "换节点 → 变");
        assert!(
            !selected_exit_changed(Some("a"), Some("a")),
            "重选同一节点 → 不变（防白刷）"
        );
        assert!(
            selected_exit_changed(None, Some("a")),
            "首次选中（旧 None）→ 变"
        );
        assert!(
            selected_exit_changed(Some("a"), None),
            "清除选中（新 None）→ 变"
        );
        assert!(!selected_exit_changed(None, None), "始终无选中 → 不变");
    }

    /// 决策核心 · 出口变 → 失效一次 + 递增 epoch + 带 (running, exitBlocked=false)。
    /// 打断 `invalidate_unlock_on_exit_change` 的 `unlock.invalidate(...)` 调用 → 本测转红（零失效 + epoch 不动）。
    #[test]
    fn invalidate_fires_once_on_exit_change() {
        let rt = runtime();
        let sink = CountingSink::default();
        let e0 = rt.epoch();
        invalidate_unlock_on_exit_change(&rt, &sink, true, Some("a"), Some("b"));
        assert_eq!(
            sink.invalidated.lock().unwrap().as_slice(),
            &[(true, false)],
            "出口变 → 失效一次，带 running=true / exitBlocked=false"
        );
        assert_eq!(rt.epoch(), e0 + 1, "失效必须递增 epoch（作废在飞轮）");
    }

    /// 决策核心 · 出口未变 → 零失效 + epoch 不动（守卫白刷探测）。
    /// 打断谓词为恒 true → 本测转红（无关 config 写触发白刷）。
    #[test]
    fn invalidate_skips_when_exit_unchanged() {
        let rt = runtime();
        let sink = CountingSink::default();
        let e0 = rt.epoch();
        invalidate_unlock_on_exit_change(&rt, &sink, true, Some("a"), Some("a"));
        assert!(
            sink.invalidated.lock().unwrap().is_empty(),
            "同出口 → 不失效"
        );
        assert_eq!(rt.epoch(), e0, "不失效则 epoch 不动");
    }

    /// 决策核心 · running 透传（false → 前端复位 idle 而非「检测中」）。
    /// 打断 running 硬编码为 true → 本测转红。
    #[test]
    fn invalidate_propagates_running_state() {
        let rt = runtime();
        let sink = CountingSink::default();
        invalidate_unlock_on_exit_change(&rt, &sink, false, Some("a"), Some("b"));
        assert_eq!(
            sink.invalidated.lock().unwrap().as_slice(),
            &[(false, false)],
            "running=false 须透传"
        );
    }

    /// 老/新提取链（去 Tauri）：`current_selected_server_id` 读旧 + `set_value` 后取新 + 谓词判定——
    /// 覆盖命令层「捕获旧 → 保存 → 提取新 → 决策」的提取逻辑（唯 sink 侧需 Tauri，已由上面注入测覆盖）。
    /// 换出口 → 判变；改无关键 → 判不变（守卫白刷在提取链上同样成立）。
    #[test]
    fn extraction_chain_detects_change_and_guards_unrelated_write() {
        let dir = temp_dir("extract");
        let mgr = ConfigManager::new(dir.clone());
        // 建含 node-a/node-b 的合法配置（selectedServerId 存在性校验要求节点真在册，否则 save 校验 Err）。
        let mut cfg = mgr.load_full().unwrap();
        cfg.as_object_mut().unwrap().insert(
            "servers".into(),
            json!([
                { "id": "node-a", "name": "A", "protocol": "trojan", "address": "1.2.3.4", "port": 443, "password": "pw" },
                { "id": "node-b", "name": "B", "protocol": "trojan", "address": "5.6.7.8", "port": 443, "password": "pw" },
            ]),
        );
        cfg.as_object_mut()
            .unwrap()
            .insert("selectedServerId".into(), json!("node-a"));
        mgr.save_full(&cfg).unwrap();
        assert_eq!(
            current_selected_server_id(&mgr).as_deref(),
            Some("node-a"),
            "读回刚置的选中出口"
        );

        // 换选中出口：捕获旧 → set_value 新 → 提取新 → 判「变」。
        let old = current_selected_server_id(&mgr);
        let new_cfg = mgr.set_value("selectedServerId", json!("node-b")).unwrap();
        let new_sel = new_cfg.get("selectedServerId").and_then(Value::as_str);
        assert!(
            selected_exit_changed(old.as_deref(), new_sel),
            "换出口 → 失效"
        );

        // 无关键写（改 mixedPort，不动选中）：判「不变」（守卫白刷）。
        let old2 = current_selected_server_id(&mgr);
        let cfg2 = mgr.set_value("mixedPort", json!(7890)).unwrap();
        let new2 = cfg2.get("selectedServerId").and_then(Value::as_str);
        assert!(
            !selected_exit_changed(old2.as_deref(), new2),
            "无关键写不改出口 → 不失效"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ── 启动期配置维护（clashApiSecret 回填 / F29 无损迁移 / tmp 清扫）变异门 ─────────────
#[cfg(test)]
mod startup_maintenance_tests {
    use super::*;
    use crate::runtime::config::ConfigManager;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-maint-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 有效配置模板（能过 validate → load 成功走 loaded_from_disk）。
    fn valid_config() -> Value {
        json!({
            "proxyMode": "global",
            "proxyModeType": "systemProxy",
            "logLevel": "info",
            "mixedPort": 7890,
            "controlPort": 9090,
            "tunConfig": {"mtu": 1350, "stack": "auto", "autoRoute": true, "strictRoute": true}
        })
    }

    #[test]
    fn clash_secret_is_32_lowercase_hex_and_unique() {
        // 该修（HIGH）：CSPRNG 16B → 32 位小写 hex（对齐 上游 randomBytes(16).toString('hex')）。
        let a = generate_local_api_secret().unwrap();
        assert_eq!(a.len(), 32, "16 字节 → 32 hex");
        assert!(a
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        let b = generate_local_api_secret().unwrap();
        assert_ne!(a, b, "连续生成不得相同（CSPRNG）");
    }

    #[test]
    fn backfill_generates_and_persists_secret_when_missing() {
        // 缺 clashApiSecret → 回填随机值并**落盘持久化**（跨会话稳定，供外部客户端复用）。
        let dir = temp_dir("secret-gen");
        std::fs::write(dir.join("config.json"), valid_config().to_string()).unwrap();
        let mgr = ConfigManager::new(dir.clone());
        backfill_secret_and_privacy(&mgr).unwrap();
        // 落盘（新 ConfigManager 从盘重载）后 secret 存在且非空。
        let mgr2 = ConfigManager::new(dir.clone());
        let secret = mgr2.load_full().unwrap()["clashApiSecret"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert_eq!(secret.len(), 32, "回填的 secret 须落盘持久化");
        // 幂等：二次维护不改已有 secret（稳定，不每次 load 重生成）。
        backfill_secret_and_privacy(&mgr2).unwrap();
        let mgr3 = ConfigManager::new(dir.clone());
        assert_eq!(
            mgr3.load_full().unwrap()["clashApiSecret"]
                .as_str()
                .unwrap(),
            secret,
            "已有 secret 须稳定不变"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backfill_preserves_existing_secret() {
        // 已有 secret → 不覆盖（幂等门；打断「!has_secret」判定即转红）。
        let dir = temp_dir("secret-keep");
        let mut cfg = valid_config();
        cfg["clashApiSecret"] = json!("deadbeefdeadbeefdeadbeefdeadbeef");
        std::fs::write(dir.join("config.json"), cfg.to_string()).unwrap();
        let mgr = ConfigManager::new(dir.clone());
        backfill_secret_and_privacy(&mgr).unwrap();
        let mgr2 = ConfigManager::new(dir.clone());
        assert_eq!(
            mgr2.load_full().unwrap()["clashApiSecret"]
                .as_str()
                .unwrap(),
            "deadbeefdeadbeefdeadbeefdeadbeef",
            "已有 secret 不得被覆盖"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backfill_does_not_overwrite_corrupt_config() {
        // 数据保护红线：损坏配置 load 回落默认，但维护**绝不** save 默认覆盖损坏原文件。
        let dir = temp_dir("corrupt-guard");
        let corrupt = "{ not valid json at all";
        std::fs::write(dir.join("config.json"), corrupt).unwrap();
        let mgr = ConfigManager::new(dir.clone());
        backfill_secret_and_privacy(&mgr).unwrap();
        // 原损坏文件须原样保留（未被默认+secret 覆盖）。
        assert_eq!(
            std::fs::read_to_string(dir.join("config.json")).unwrap(),
            corrupt,
            "损坏配置绝不被维护覆盖"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn f29_legacy_plaintext_migrated_to_scrypt_file_losslessly() {
        // 旧明文 privacyPassword 无损迁移为 scrypt 独立文件——密码不丢，锁不失效，且盘上明文被 scrub。
        let dir = temp_dir("f29");
        let mut cfg = valid_config();
        cfg["privacyPassword"] = json!("legacy-secret-42");
        std::fs::write(dir.join("config.json"), cfg.to_string()).unwrap();
        let mgr = ConfigManager::new(dir.clone());
        backfill_secret_and_privacy(&mgr).unwrap();
        // scrypt 文件已落；盘上明文已被清空（save_full 用 migrate 抹空的 cfg 覆盖）。
        let path = polaris_store::privacy_lock::lock_path(&dir);
        assert!(path.exists(), "须已落 scrypt 独立文件");
        let mgr2 = ConfigManager::new(dir.clone());
        let reloaded = mgr2.load_full().unwrap();
        assert_eq!(
            reloaded["privacyPassword"],
            json!(""),
            "盘上明文须被清空（无残留）"
        );
        assert!(
            reloaded.get(PRIVACY_PASSWORD_HASH_KEY).is_none(),
            "迁移落文件、不再写 config 里的 SHA-256 键"
        );
        assert!(
            unlock_core(&mgr2, "legacy-secret-42").unwrap(),
            "旧明文须能解锁（无损迁移）"
        );
        assert!(!unlock_core(&mgr2, "wrong").unwrap(), "错误密码不得解锁");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn f29_skips_when_scrypt_file_already_present() {
        // 已有 scrypt 文件（用户已设新密码）→ 旧明文不得覆盖（`!has_scrypt_file && !has_legacy` 门；打断即转红）。
        let dir = temp_dir("f29-skip");
        let mgr = ConfigManager::new(dir.clone());
        set_password_core(&mgr, "real-password", false).unwrap(); // scrypt 文件
                                                                  // 手动往盘塞 legacy 明文（模拟旧残留 + 新密码并存）。
        let cfg_path = dir.join("config.json");
        let mut on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        on_disk
            .as_object_mut()
            .unwrap()
            .insert("privacyPassword".into(), json!("stale-plaintext"));
        std::fs::write(&cfg_path, on_disk.to_string()).unwrap();
        backfill_secret_and_privacy(&mgr).unwrap();
        // 既有密码仍有效，旧明文未顶掉它。
        let mgr2 = ConfigManager::new(dir.clone());
        assert!(
            unlock_core(&mgr2, "real-password").unwrap(),
            "既有密码不被旧明文顶替"
        );
        assert!(
            !unlock_core(&mgr2, "stale-plaintext").unwrap(),
            "旧明文不得成为新密码"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ── LOW-1 回归门：全局订阅 UA 变更必须作废条件 GET 验证器 ────────────────────────────
//
// 全部经**生产路径**驱动（`config_save_core` / `set_value_with_ua_invalidation`），而非测试自己调
// `invalidate_validators_on_global_ua_change` 再 `save_full` —— 后者会让「删掉生产代码里的那行调用
// 测试照样绿」成为可能 = 假绿，同 `config_save_core` 文档里记的那条纪律。
//
// 判据本体的射程/归一语义由 `commands/subscription.rs::ua_tests` 的纯函数用例覆盖；这里只锁
// **两条 config 写腿有没有真接上**。
#[cfg(test)]
mod subscription_ua_invalidation_tests {
    use super::*;
    use crate::runtime::config::ConfigManager;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-ua-invalidate-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 盘上先有一条已拉取过（带验证器）的订阅 + 一条自带 per-sub UA 的订阅。
    fn seed(mgr: &ConfigManager, global_ua: &str) {
        let mut cfg = mgr.load_full().unwrap();
        cfg["subscriptionUserAgent"] = json!(global_ua);
        cfg["subscriptions"] = json!([
            {
                "id": "s-global",
                "name": "跟随全局",
                "url": "https://example.invalid/a",
                "etag": "W/\"v1\"",
                "lastModified": "Mon, 01 Jan 2024 00:00:00 GMT",
            },
            {
                "id": "s-own",
                "name": "自带 UA",
                "url": "https://example.invalid/b",
                "userAgent": "mihomo/1.18",
                "etag": "W/\"v2\"",
                "lastModified": "Tue, 02 Jan 2024 00:00:00 GMT",
            },
        ]);
        mgr.save_full(&cfg).unwrap();
    }

    fn sub<'a>(cfg: &'a Value, id: &str) -> &'a Value {
        cfg["subscriptions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == json!(id))
            .unwrap()
    }

    /// **全量保存腿**（设置页改全局 UA 后 `saveConfig({...config})` 的真实形态）。
    ///
    /// 牙：删掉 `config_save_core` 里的 `invalidate_stale_subscription_validators(...)` 调用 →
    /// 前两条断言转红（= 改了全局 UA 仍带旧 ETag 请求，机场按 UA 下发变体时恒 304，新格式永远拿不到）。
    #[test]
    fn full_save_with_new_global_ua_drops_validators_of_affected_subs() {
        let dir = temp_dir("save");
        let mgr = ConfigManager::new(dir.clone());
        seed(&mgr, "clash-verge/1.0");

        // 前端提交全量 config，只把全局 UA 改了。
        let mut submitted = mgr.load_full().unwrap();
        submitted["subscriptionUserAgent"] = json!("sing-box/1.9");
        config_save_core(&mgr, &mut submitted, None).expect("save 应成功");

        let on_disk = ConfigManager::new(dir.clone()).load_full().unwrap();
        assert!(
            sub(&on_disk, "s-global").get("etag").is_none(),
            "全局 UA 变了 → 跟随全局的订阅 etag 必须作废"
        );
        assert!(
            sub(&on_disk, "s-global").get("lastModified").is_none(),
            "lastModified 同样必须作废（两者任一残留都足以让服务端回 304）"
        );
        assert_eq!(
            sub(&on_disk, "s-own")["etag"],
            json!("W/\"v2\""),
            "per-sub 覆盖的订阅生效 UA 没变 → 验证器不得白扔"
        );
        assert_eq!(
            on_disk["subscriptionUserAgent"],
            json!("sing-box/1.9"),
            "UA 本身照常落盘"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无关键的全量保存**绝不**碰验证器（否则每改一次设置就把下次订阅更新变成全量下载）。
    ///
    /// 牙：把作废判据改成「无条件清」/「只要有 subscriptions 就清」→ 本条转红。
    #[test]
    fn full_save_without_ua_change_keeps_validators() {
        let dir = temp_dir("noop");
        let mgr = ConfigManager::new(dir.clone());
        seed(&mgr, "clash-verge/1.0");

        let mut submitted = mgr.load_full().unwrap();
        submitted["logLevel"] = json!("debug");
        config_save_core(&mgr, &mut submitted, None).expect("save 应成功");

        let on_disk = ConfigManager::new(dir.clone()).load_full().unwrap();
        assert_eq!(sub(&on_disk, "s-global")["etag"], json!("W/\"v1\""));
        assert_eq!(
            sub(&on_disk, "s-global")["lastModified"],
            json!("Mon, 01 Jan 2024 00:00:00 GMT")
        );
        assert_eq!(sub(&on_disk, "s-own")["etag"], json!("W/\"v2\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **备份导入腿**（`backup:importApply` 勾「通用设置」）：第三条能改全局 UA 的写路径。
    ///
    /// # 为什么这条腿必须独立存在
    ///
    /// `subscriptionUserAgent` 按排除法属 generalSettings 类（既不在 `DATA_FIELDS` 也不在
    /// `EXCLUDED_FROM_BACKUP`，见 `polaris_store::backup`）⇒ 勾了通用设置的导入就能把全局 UA 换掉，
    /// 而**不勾订阅类**时本机订阅的 `etag`/`lastModified` 原样留着 ⇒ 换 UA 后恒 304、新格式永远拿不到。
    /// 上面两条用例（`config:save` / `config:setValue`）对这条腿是**恒绿**的。
    ///
    /// 驱动方式与命令层逐字同形：`merge_categories(current, backup, [GeneralSettings])`
    /// → [`backup_import_save_core`]。
    ///
    /// 牙：删掉 `backup_import_save_core` 里的 `invalidate_validators_on_global_ua_change(...)`
    /// → 前两条断言转红。
    #[test]
    fn backup_import_of_general_settings_drops_validators_of_affected_subs() {
        let dir = temp_dir("backup-import");
        let mgr = ConfigManager::new(dir.clone());
        seed(&mgr, "clash-verge/1.0");
        let current = mgr.load_full().unwrap();

        // 外机备份：只有通用设置被勾，且它带着**不同**的全局 UA。
        let mut backup = current.clone();
        backup["subscriptionUserAgent"] = json!("sing-box/1.9");
        let outcome = polaris_store::backup::merge_categories(
            &current,
            &backup,
            &[polaris_store::backup::BackupCategory::GeneralSettings],
        );
        let mut restored = outcome.config;
        backup_import_save_core(&mgr, &current, &mut restored).expect("导入落盘应成功");

        let on_disk = ConfigManager::new(dir.clone()).load_full().unwrap();
        assert_eq!(
            on_disk["subscriptionUserAgent"],
            json!("sing-box/1.9"),
            "备份里的全局 UA 照常落盘（前提：这条腿确实能改 UA）"
        );
        assert!(
            sub(&on_disk, "s-global").get("etag").is_none()
                && sub(&on_disk, "s-global").get("lastModified").is_none(),
            "导入换了全局 UA → 跟随全局的订阅验证器必须作废，否则换 UA 后恒 304"
        );
        assert_eq!(
            sub(&on_disk, "s-own")["etag"],
            json!("W/\"v2\""),
            "per-sub 覆盖的订阅生效 UA 没变 → 验证器不得白扔（射程限制）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 备份里的全局 UA 与本机**相同** → 一条验证器都不许扔（否则每次导入都把下次订阅更新变成全量下载）。
    ///
    /// 牙：把作废判据改成「导入即无条件清」→ 本条转红。
    #[test]
    fn backup_import_with_same_ua_keeps_validators() {
        let dir = temp_dir("backup-import-noop");
        let mgr = ConfigManager::new(dir.clone());
        seed(&mgr, "clash-verge/1.0");
        let current = mgr.load_full().unwrap();

        let mut backup = current.clone();
        backup["logLevel"] = json!("debug"); // 通用设置有变化，但 UA 没变
        let outcome = polaris_store::backup::merge_categories(
            &current,
            &backup,
            &[polaris_store::backup::BackupCategory::GeneralSettings],
        );
        let mut restored = outcome.config;
        backup_import_save_core(&mgr, &current, &mut restored).expect("导入落盘应成功");

        let on_disk = ConfigManager::new(dir.clone()).load_full().unwrap();
        assert_eq!(on_disk["logLevel"], json!("debug"), "通用设置照常导入");
        assert_eq!(sub(&on_disk, "s-global")["etag"], json!("W/\"v1\""));
        assert_eq!(
            sub(&on_disk, "s-global")["lastModified"],
            json!("Mon, 01 Jan 2024 00:00:00 GMT")
        );
        assert_eq!(sub(&on_disk, "s-own")["etag"], json!("W/\"v2\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **单键写腿**（`config:setValue("subscriptionUserAgent", …)`）：同一条不变式。
    ///
    /// 牙：把 `config_set_value` 里的 `set_value_with_ua_invalidation` 改回
    /// `state.config().set_value(...)` → 本条转红（上面两条全量保存的用例**不会**红，
    /// 这正是本条必须独立存在的理由）。
    #[test]
    fn set_value_leg_drops_validators_too() {
        let dir = temp_dir("setvalue");
        let mgr = ConfigManager::new(dir.clone());
        seed(&mgr, "clash-verge/1.0");

        let returned = set_value_with_ua_invalidation(
            &mgr,
            SUBSCRIPTION_USER_AGENT_KEY,
            json!("sing-box/1.9"),
        )
        .expect("置键应成功");
        assert_eq!(
            returned["subscriptionUserAgent"],
            json!("sing-box/1.9"),
            "返回值须是置键后的新配置（`set_value` 的既有契约，广播要用它）"
        );

        let on_disk = ConfigManager::new(dir.clone()).load_full().unwrap();
        assert!(sub(&on_disk, "s-global").get("etag").is_none());
        assert!(sub(&on_disk, "s-global").get("lastModified").is_none());
        assert_eq!(sub(&on_disk, "s-own")["etag"], json!("W/\"v2\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非 UA 键走单键写腿时**逐字等价于原路径**：只改目标键，验证器一动不动。
    #[test]
    fn set_value_of_unrelated_key_is_byte_for_byte_the_old_path() {
        let dir = temp_dir("setvalue-other");
        let mgr = ConfigManager::new(dir.clone());
        seed(&mgr, "clash-verge/1.0");

        set_value_with_ua_invalidation(&mgr, "logLevel", json!("debug")).expect("置键应成功");

        let on_disk = ConfigManager::new(dir.clone()).load_full().unwrap();
        assert_eq!(on_disk["logLevel"], json!("debug"));
        assert_eq!(sub(&on_disk, "s-global")["etag"], json!("W/\"v1\""));
        assert_eq!(sub(&on_disk, "s-own")["etag"], json!("W/\"v2\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🟡 **接线守卫**：`config_set_value` 命令壳持 `State<AppRuntime>`、单测直调不了，
    /// 故按本仓既有做法用源码扫描锁住「它走的是 UA 感知包装、不是裸 `set_value`」。
    ///
    /// 变异探针：把那一行改回 `state.config().set_value(&key, value)` ⇒ 本条转红。
    #[test]
    fn set_value_command_routes_through_the_ua_aware_wrapper() {
        let src = include_str!("config.rs");
        let body = crate::commands::guard_scan::top_level_fn_body(src, "pub fn config_set_value(");
        assert!(
            body.contains("set_value_with_ua_invalidation(state.config(), &key, value)"),
            "变异锁：单键写腿绕过了 UA 感知包装 → 经 setValue 改全局 UA 后验证器不清，恒 304"
        );
        assert!(
            !body.contains("state.config().set_value("),
            "裸 set_value 不得再出现在本命令里（双路径 = 迟早只改一条）"
        );
    }

    /// 🟡 **顺序守卫**：作废必须排在 `save_full` **之前** —— 落盘后再清等于没清。
    #[test]
    fn invalidation_happens_before_the_write() {
        let src = include_str!("config.rs");
        let body = crate::commands::guard_scan::top_level_fn_body(src, "fn config_save_core(");
        let at = body
            .find("invalidate_stale_subscription_validators(config, incoming)")
            .expect("变异锁：全量保存腿的验证器作废被删了");
        let write = body.find("config.save_full(incoming)").expect("落盘腿仍在");
        assert!(at < write, "作废必须排在落盘之前");
    }
}

// ── P5：乐观并发 + 内容版本（spec §2.5 Q8-b / §3.7 / R6）────────────────────────────
#[cfg(test)]
mod optimistic_concurrency_tests {
    use super::*;
    use crate::runtime::config::ConfigManager;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-optimistic-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 缺省 `base_version` = **不校验** = 今天行为（回滚腿：既有十余个调用点零改动）。
    ///
    /// 牙：把 `if let Some(base)` 改成无条件校验（拿 `""` 当基准之类）→ 本条转红，
    /// 而那正是「P5 上线即打断所有既有保存路径」的形态。
    #[test]
    fn absent_base_version_skips_the_check_entirely() {
        let dir = temp_dir("absent");
        let mgr = ConfigManager::new(dir.clone());
        let mut submitted = mgr.load_full().unwrap();
        submitted["logLevel"] = json!("debug");

        let outcome = config_save_core(&mgr, &mut submitted, None).expect("save 应成功");
        assert!(
            matches!(outcome, SaveOutcome::Saved { .. }),
            "不传 base_version 必须直通落盘"
        );
        assert_eq!(
            ConfigManager::new(dir.clone()).load_full().unwrap()["logLevel"],
            json!("debug")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 基准相符 ⇒ 落盘，且返回的 `version` 就是**落盘后**磁盘现值的版本（前端拿它当新锚点）。
    ///
    /// 牙：让 `Saved.version` 返回入参 `base_version`（或落盘前的版本）→ 第二个断言转红
    /// （前端会把陈旧版本当锚点，下一次保存必然自判冲突）。
    #[test]
    fn matching_base_version_saves_and_returns_the_post_write_version() {
        let dir = temp_dir("match");
        let mgr = ConfigManager::new(dir.clone());
        // 新装那一次 `load_full` 的返回值与它刚落盘的默认配置并不同源（`was_missing` 腿落的是
        // sanitize 后的形，缓存里放的是 sanitize 前的）—— 先跑一次把文件建出来，此后缓存与盘同源。
        let _ = mgr.load_full().unwrap();

        // 模拟 `config:get`：同一次 load 既是前端拿到的 config，也是后端缓存的现值。
        let mut submitted = mgr.load_full().unwrap();
        let before = config_version(&submitted);
        submitted["logLevel"] = json!("debug");
        let outcome = config_save_core(&mgr, &mut submitted, Some(&before)).expect("save 应成功");

        let after = config_version(&ConfigManager::new(dir.clone()).load_full().unwrap());
        match outcome {
            SaveOutcome::Saved { version } => {
                assert_ne!(version, before, "落盘改了内容，版本必须随之变化");
                assert_eq!(version, after, "返回的版本须与落盘后磁盘现值同源");
            }
            SaveOutcome::Conflict { .. } => panic!("基准相符不该判冲突"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **R6 的守门人**：基准不符 ⇒ 冲突，且「一个字节都没动」—— 既没写盘，也没跑过任何一条
    /// 落盘前策略（`incoming` 原样交还）。
    ///
    /// 牙（本条同时是 R6 的顺序变异对照）：把乐观并发校验从 `config_save_core` 顶端挪到
    /// `preserve_server_owned_secrets` / `enforce_backend_authoritative_fields` /
    /// `invalidate_stale_subscription_validators` 三条之后 → `incoming` 会被回填出
    /// `privacyPasswordHash` 与后端权威的 `recentServerIds` → 后两个断言转红。
    /// 把冲突腿改成「照样落盘」→ 第一个断言转红（T2-2：冲突绝不写盘）。
    #[test]
    fn optimistic_conflict_touches_nothing() {
        let dir = temp_dir("conflict");
        let mgr = ConfigManager::new(dir.clone());

        // 磁盘：带隐私 hash（`preserve_` 的输入）+ 后端权威 MRU（`enforce_` 的输入）。
        let mut cfg = mgr.load_full().unwrap();
        cfg["privacyPasswordHash"] = json!("aabb$deadbeef");
        cfg["recentServerIds"] = json!(["n3", "n2", "n1"]);
        cfg["logLevel"] = json!("info");
        mgr.save_full(&cfg).unwrap();

        // 前端提交：与磁盘不同源的陈旧基准（模拟「暂存期间别人改了盘」）。
        let mut submitted = mgr.load_full().unwrap();
        strip_privacy_secrets(&mut submitted);
        submitted["recentServerIds"] = json!(["stale"]);
        submitted["logLevel"] = json!("debug");
        let before = submitted.clone();

        let outcome = config_save_core(&mgr, &mut submitted, Some("00000000")).expect("不应报错");
        match outcome {
            SaveOutcome::Conflict { disk_version } => {
                assert_eq!(
                    disk_version,
                    config_version(&mgr.current().unwrap()),
                    "回传的 diskVersion 须是磁盘现值的版本"
                );
            }
            SaveOutcome::Saved { .. } => panic!("基准不符必须判冲突"),
        }

        assert_eq!(
            ConfigManager::new(dir.clone()).load_full().unwrap()["logLevel"],
            json!("info"),
            "冲突腿绝不写盘"
        );
        assert!(
            submitted.get("privacyPasswordHash").is_none(),
            "冲突腿不得跑过 preserve_server_owned_secrets（校验必须在三条策略之前）"
        );
        assert_eq!(
            submitted["recentServerIds"],
            json!(["stale"]),
            "冲突腿不得跑过 enforce_backend_authoritative_fields（校验必须在三条策略之前）"
        );
        assert_eq!(
            submitted, before,
            "冲突腿交还的入参必须逐字节等于传进来的那份"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 版本的定义域是**渲染端投影**，不是磁盘原样。
    ///
    /// 前端对 `config:get` 的产物算版本、后端对磁盘算，两边若不是同一份文档则版本恒不等 ⇒
    /// 每一次带 `base_version` 的保存都返 conflict、功能整体失效。
    ///
    /// 牙：把 `config_version` 里的 `apply_frontend_view` 删掉 → 两个断言分别转红
    /// （设过隐私密码的机器 / `bypassLANList` 缺省的机器上，前端与后端各算各的）。
    #[test]
    fn config_version_is_computed_over_the_frontend_view() {
        let base = json!({ "mixedPort": 7890, "bypassLANList": ["192.168.0.0/16"] });

        let mut with_secret = base.clone();
        with_secret["privacyPasswordHash"] = json!("aabb$deadbeef");
        assert_eq!(
            config_version(&with_secret),
            config_version(&base),
            "隐私 hash 不下发给前端 ⇒ 不得参与版本"
        );

        let missing_bypass = json!({ "mixedPort": 7890 });
        let mut filled = missing_bypass.clone();
        polaris_config_engine::user_config::system_proxy_bypass::ensure_bypass_lan_list(
            &mut filled,
        );
        assert_eq!(
            config_version(&missing_bypass),
            config_version(&filled),
            "bypassLANList 由 config_get 补齐 ⇒ 补前补后必须同版本"
        );
    }

    /// **跨语言值锁**：同一组 fixture，Rust `config_content_hash` 与前端 `configBaseVersion`
    /// 必须算出同一个短 hash（fixture 里写死的 `expected` 是双侧共同真值）。
    ///
    /// 前端那一半在 `ui/src/contracts/config-version.test.ts`，读的是同一个文件。
    ///
    /// 牙：把 `encode_utf16()` 换成 `bytes()` → `nonAscii` 用例转红；把 `wrapping_mul` 换成
    /// 饱和/普通乘 → 全部转红；把 `stable_stringify` 换成 `to_string` → `nestedKeysShuffled` 转红。
    #[test]
    fn config_version_matches_the_shared_cross_language_fixture() {
        #[derive(serde::Deserialize)]
        struct Case {
            name: String,
            expected: String,
            config: Value,
        }
        #[derive(serde::Deserialize)]
        struct Fixture {
            cases: Vec<Case>,
        }

        let raw = include_str!("../../../ui/src/contracts/config-version.fixture.json");
        let fixture: Fixture = serde_json::from_str(raw).expect("fixture 解析失败");
        assert!(
            fixture.cases.len() >= 8,
            "自曝：fixture 读空/读少了，恒绿的空断言比没有这道门更危险"
        );
        for case in &fixture.cases {
            assert_eq!(
                config_content_hash(&case.config),
                case.expected,
                "fixture `{}` 的版本与前端不一致",
                case.name
            );
        }
    }
}
